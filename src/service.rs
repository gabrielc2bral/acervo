//! Parte do Acervo que roda como administrador, dentro do serviço do Windows.
//!
//! O app comum não precisa de administrador: ele só deixa tarefas no `service.db` (ver `link`) e lê
//! o andamento de volta. O serviço:
//! 1. faz a varredura rápida da MFT dos HDs NTFS (a que exige administrador);
//! 2. acompanha o diário USN de cada HD, que registra tudo que foi criado, apagado ou renomeado,
//!    inclusive enquanto o Acervo estava fechado, e ajusta o catálogo só nesses arquivos.
//!
//! Tudo que acontece vai para o registro `C:\ProgramData\Acervo\service.log`.
//! Instalar o serviço pede a permissão do Windows uma única vez.

use crate::db::Db;
use crate::link::{Job, ServiceLink};
use crate::log;
use crate::scan::{self, Outcome, Progress};
use crate::settings::Settings;
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub use crate::link::now_secs;

pub const SERVICE_NAME: &str = "AcervoService";

/// Permissões do serviço: o sistema e os administradores controlam tudo; quem está logado no PC pode
/// ver o estado e **iniciar/parar** (é o que o ícone da bandeja faz), mas não reconfigurar nem apagar.
pub const SERVICE_SDDL: &str = "D:(A;;CCLCSWRPWPDTLOCRRC;;;SY)(A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;BA)(A;;CCLCSWRPWPLOCRRC;;;IU)(A;;CCLCSWLOCRRC;;;SU)";

/// O serviço está dando sinal de vida agora? (Ele escreve a cada segundo, de uma thread só para isso.)
pub fn is_alive_at(catalog_path: &Path) -> bool {
    ServiceLink::open(catalog_path).map(|link| link.is_alive()).unwrap_or(false)
}

/// Estado do serviço no Windows, pelo `sc query`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Installed {
    No,
    Stopped,
    Running,
}

pub fn installed_state() -> Installed {
    let Ok(output) = std::process::Command::new("sc").args(["query", SERVICE_NAME]).output() else {
        return Installed::No;
    };
    let text = String::from_utf8_lossy(&output.stdout).to_uppercase();
    if !output.status.success() {
        Installed::No
    } else if text.contains("RUNNING") {
        Installed::Running
    } else {
        Installed::Stopped
    }
}

/// Pede uma varredura rápida ao serviço e devolve o id da tarefa.
pub fn request_scan(catalog_path: &Path, source_id: i64) -> Result<i64> {
    ServiceLink::open(catalog_path)?.enqueue_job("scan", source_id)
}

fn position_key(source_id: i64) -> String {
    format!("usn:{source_id}")
}

/// O serviço roda sempre; para não pesar no PC, trabalha com prioridade abaixo do normal.
#[cfg(windows)]
fn lower_priority() {
    use windows::Win32::System::Threading::{GetCurrentProcess, SetPriorityClass, BELOW_NORMAL_PRIORITY_CLASS};
    // SAFETY: só muda a prioridade do próprio processo.
    unsafe {
        let _ = SetPriorityClass(GetCurrentProcess(), BELOW_NORMAL_PRIORITY_CLASS);
    }
}

#[cfg(not(windows))]
fn lower_priority() {}

/// Laço do serviço. Roda até `stop` ficar verdadeiro.
pub fn run_loop(db_path: &Path, stop: &AtomicBool) {
    log::write(&format!("serviço iniciado (catálogo: {})", db_path.display()));
    lower_priority();

    // Sinal de vida numa thread própria: continua batendo durante uma varredura longa.
    let heartbeat_stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let Ok(link) = ServiceLink::open(db_path) else {
                log::write("não foi possível abrir o service.db para o sinal de vida");
                return;
            };
            while !heartbeat_stop.load(Ordering::Relaxed) {
                let _ = link.beat();
                for _ in 0..10 {
                    if heartbeat_stop.load(Ordering::Relaxed) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            let _ = link.clear_beat();
        });

        work_loop(db_path, stop);
        heartbeat_stop.store(true, Ordering::Relaxed);
    });
    log::write("serviço parado");
}

fn work_loop(db_path: &Path, stop: &AtomicBool) {
    let Ok(link) = ServiceLink::open(db_path) else {
        log::write("não foi possível abrir o service.db: o serviço não vai processar tarefas");
        return;
    };
    if let Ok(count) = link.fail_interrupted_jobs() {
        if count > 0 {
            log::write(&format!("{count} tarefa(s) interrompida(s) por uma parada anterior foram marcadas como falha"));
        }
    }

    // O catálogo pode demorar a ficar acessível (perfil do usuário ainda carregando no boot):
    // tenta de novo em vez de desistir e deixar o serviço parado.
    let db = loop {
        match Db::open(db_path) {
            Ok(db) => break db,
            Err(error) => {
                log::write(&format!("não foi possível abrir o catálogo ({error}); tentando de novo em 5 s"));
                for _ in 0..50 {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    };

    let mut followers = follow::Followers::default();
    let mut last_sync: Option<Instant> = None;
    let mut last_trim = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        if let Ok(Some(job)) = link.next_queued_job() {
            run_job(&link, db_path, job, stop);
        }
        if last_sync.is_none_or(|at| at.elapsed() > Duration::from_secs(20)) {
            followers.sync(&db);
            last_sync = Some(Instant::now());
        }
        followers.poll(&db);

        if last_trim.elapsed() > Duration::from_secs(600) {
            let _ = db.trim_changes(200_000);
            let _ = link.clean_jobs();
            last_trim = Instant::now();
        }
        for _ in 0..10 {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn run_job(link: &ServiceLink, db_path: &Path, job: Job, stop: &AtomicBool) {
    let finish = |state: &str, message: &str| {
        log::write(&format!("tarefa #{} ({}): {state} - {message}", job.id, job.kind));
        let _ = link.set_job(job.id, state, message);
    };
    if job.kind != "scan" {
        return finish("failed", "tarefa desconhecida");
    }
    let Ok(mut scan_db) = Db::open(db_path) else {
        return finish("failed", "não foi possível abrir o catálogo");
    };
    let Some(source) = scan_db
        .sources()
        .ok()
        .and_then(|sources| sources.into_iter().find(|source| source.id == job.source_id))
    else {
        return finish("failed", "fonte não encontrada");
    };
    let root = PathBuf::from(&source.path);
    let options = Settings::load(&scan_db).map(|s| s.scan_options()).unwrap_or_default();
    log::write(&format!("tarefa #{}: varrendo {}", job.id, root.display()));
    let _ = link.set_job(job.id, "running", "Iniciando a varredura rápida...");

    // A posição do diário é anotada ANTES de varrer: o que mudar durante a varredura será
    // aplicado depois (aplicar uma mudança duas vezes não faz mal).
    let start_position = follow::current_position(&root);

    let cancel = AtomicBool::new(false);
    let mut last_report = Instant::now();
    let mut report = |progress: &Progress| {
        if stop.load(Ordering::Relaxed) {
            cancel.store(true, Ordering::Relaxed); // o serviço está parando
        }
        if last_report.elapsed() < Duration::from_millis(700) {
            return;
        }
        last_report = Instant::now();
        let _ = link.set_job_progress(job.id, &format!("{} arquivos lidos pelo serviço", progress.files));
        if link.job(job.id).ok().flatten().is_some_and(|current| current.state == "cancel") {
            cancel.store(true, Ordering::Relaxed);
        }
    };

    match scan::scan_source(&mut scan_db, job.source_id, &root, &options, &cancel, &mut report) {
        Ok(Outcome::Done(progress)) => {
            if let Some(position) = start_position {
                let _ = scan_db.set_setting(&position_key(job.source_id), &follow::encode(position));
            }
            finish("done", &format!("Pronto: {} arquivos catalogados pelo serviço.", progress.files));
        }
        Ok(Outcome::Cancelled) => finish("cancelled", "Cancelado."),
        Ok(Outcome::NeedsAdmin) => finish("failed", "O serviço não tem permissão de administrador."),
        Err(error) => finish("failed", &format!("Erro: {error}")),
    }
}

/// Diário USN do NTFS: só existe no Windows.
#[cfg(windows)]
mod follow {
    use super::position_key;
    use crate::db::Db;
    use crate::fast::{drive_letter_of_root, strip_device_prefix};
    use crate::kind::{classify, has_hidden_or_system_attr};
    use crate::log;
    use crate::scan::unix_seconds;
    use crate::settings::Settings;
    use crate::watch::{attributes, in_system_dir};
    use ntfs_reader::{Journal, JournalOptions, JournalPosition, NextUsn, Reason, UsnRecord, Volume};
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    pub fn encode(position: JournalPosition) -> String {
        format!("{}:{}", position.journal_id, position.usn)
    }

    fn decode(text: &str) -> Option<JournalPosition> {
        let (journal_id, usn) = text.split_once(':')?;
        Some(JournalPosition { journal_id: journal_id.parse().ok()?, usn: usn.parse().ok()? })
    }

    fn open_volume(root: &Path) -> Option<Volume> {
        let letter = drive_letter_of_root(root)?;
        Volume::new(format!(r"\\.\{letter}:")).ok()
    }

    /// Onde o diário está agora (o fim). `None` se não for um HD NTFS inteiro ou sem permissão.
    pub fn current_position(root: &Path) -> Option<JournalPosition> {
        let journal = Journal::new(open_volume(root)?, JournalOptions::default()).ok()?;
        Some(journal.position())
    }

    struct Follower {
        source_id: i64,
        root: PathBuf,
        journal: Journal,
        /// Pastas renomeadas: número no NTFS -> caminho antigo, entre o registro do nome antigo e o do novo.
        old_dir_names: HashMap<i64, String>,
    }

    #[derive(Default)]
    pub struct Followers {
        active: HashMap<i64, Follower>,
    }

    impl Followers {
        /// Começa a acompanhar os HDs que já têm posição salva (feita por uma varredura rápida)
        /// e para de acompanhar os que sumiram. O diário guarda o que aconteceu enquanto o
        /// serviço (ou o PC) estava desligado, então ao voltar a posição salva é retomada.
        pub fn sync(&mut self, db: &Db) {
            let Ok(sources) = db.sources() else { return };
            self.active.retain(|id, follower| {
                follower.root.exists() && sources.iter().any(|source| source.id == *id)
            });
            for source in sources {
                let root = PathBuf::from(&source.path);
                if self.active.contains_key(&source.id) || !root.exists() {
                    continue;
                }
                let Some(saved) = db
                    .get_setting(&position_key(source.id))
                    .ok()
                    .flatten()
                    .and_then(|text| decode(&text))
                else {
                    continue;
                };
                let Some(volume) = open_volume(&root) else { continue };
                let options = JournalOptions { next_usn: NextUsn::Custom(saved), ..JournalOptions::default() };
                match Journal::new(volume, options) {
                    Ok(journal) => {
                        log::write(&format!("acompanhando o diário USN de {}", root.display()));
                        self.active.insert(source.id, Follower { source_id: source.id, root, journal, old_dir_names: HashMap::new() });
                    }
                    Err(error) => {
                        // Diário recriado ou posição perdida: não dá para saber o que mudou.
                        log::write(&format!("diário USN de {} perdido ({error}): vale varrer de novo", root.display()));
                        let _ = db.set_setting(&position_key(source.id), "");
                        let _ = db.set_setting(&format!("usn_lost:{}", source.id), "1");
                    }
                }
            }
        }

        pub fn poll(&mut self, db: &Db) {
            let skip_hidden = Settings::load(db).map(|s| s.skip_hidden).unwrap_or(true);
            for follower in self.active.values_mut() {
                for _ in 0..50 {
                    let Ok(page) = follower.journal.read() else { break };
                    // Uma página inteira numa transação só: bem mais rápido e sem brigar com
                    // quem está lendo o catálogo.
                    if let Ok(tx) = db.conn.unchecked_transaction() {
                        for record in &page.records {
                            if let Err(error) = apply_record(db, follower, record, skip_hidden) {
                                log::write(&format!("erro ao aplicar mudança do diário USN: {error}"));
                            }
                        }
                        let _ = db.set_setting(&position_key(follower.source_id), &encode(follower.journal.position()));
                        let _ = tx.commit();
                    }
                    if page.caught_up {
                        break;
                    }
                }
            }
        }
    }

    /// Só os registros de fechamento (`CLOSE`) trazem o resultado final de uma operação.
    fn apply_record(db: &Db, follower: &mut Follower, record: &UsnRecord, skip_hidden: bool) -> anyhow::Result<()> {
        let frn = record.file_id.as_reference().map(|reference| (reference & 0x0000_FFFF_FFFF_FFFF) as i64);
        let is_directory = record.file_attributes & 0x10 != 0;

        // O registro com o nome ANTIGO da pasta vem antes do fechamento: guarda o caminho antigo
        // para, no fechamento, mover tudo que está dentro dela.
        if is_directory && record.reason.contains(Reason::RENAME_OLD_NAME) {
            if let (Some(frn), Some(old)) = (frn, follower.journal.resolve_path(record)) {
                let old = old.to_string_lossy().into_owned();
                follower.old_dir_names.insert(frn, strip_device_prefix(&old).to_string());
            }
            return Ok(());
        }
        if !record.reason.contains(Reason::CLOSE) {
            return Ok(());
        }

        // Apagado: sai do catálogo pelo número do arquivo, sem precisar do caminho. (Cada arquivo
        // de uma pasta apagada tem o próprio registro de exclusão.)
        if record.reason.contains(Reason::FILE_DELETE) {
            if let (Some(frn), false) = (frn, is_directory) {
                db.remove_by_frn(follower.source_id, frn)?;
            }
            return Ok(());
        }

        let changed = Reason::FILE_CREATE
            | Reason::RENAME_NEW_NAME
            | Reason::DATA_OVERWRITE
            | Reason::DATA_EXTEND
            | Reason::DATA_TRUNCATION
            | Reason::BASIC_INFO_CHANGE;
        if !record.reason.intersects(changed) {
            return Ok(());
        }
        if is_directory {
            // Pasta renomeada/movida: os arquivos dentro dela mudam de caminho sem registro próprio.
            // Troca o começo do caminho no catálogo, sem varrer nada.
            if record.reason.contains(Reason::RENAME_NEW_NAME) {
                let old = frn.and_then(|frn| follower.old_dir_names.remove(&frn));
                let new = follower.journal.resolve_path(record).map(|path| path.to_string_lossy().into_owned());
                match (old, new) {
                    (Some(old), Some(new)) => {
                        let moved = db.move_dir(follower.source_id, &old, strip_device_prefix(&new))?;
                        log::write(&format!("pasta movida: {old} -> {} ({moved} arquivos)", strip_device_prefix(&new)));
                    }
                    // Sem o caminho antigo não dá para saber o que mudou de lugar: marca para conferir.
                    _ => db.set_setting(&format!("usn_dir_moved:{}", follower.source_id), "1")?,
                }
            }
            return Ok(());
        }

        let Some(path) = follower.journal.resolve_path(record) else { return Ok(()) };
        let text = path.to_string_lossy().into_owned();
        let text = strip_device_prefix(&text);
        let path = PathBuf::from(text);
        if in_system_dir(&follower.root, &path) {
            return Ok(());
        }
        let Ok(meta) = std::fs::metadata(&path) else { return Ok(()) };
        if !meta.is_file() || (skip_hidden && has_hidden_or_system_attr(attributes(&meta))) {
            return Ok(());
        }
        let (dir, name) = Db::split_path(text);
        let kind = if dir.to_lowercase().contains("\\steamapps\\") {
            "steam"
        } else {
            classify(&name).as_str()
        };
        if let Some(frn) = frn {
            db.remove_by_frn(follower.source_id, frn)?; // renomeado: tira o nome antigo
        }
        db.upsert_file_frn(
            follower.source_id,
            &dir,
            &name,
            meta.len() as i64,
            unix_seconds(meta.modified()),
            kind,
            frn,
        )?;
        Ok(())
    }
}

#[cfg(not(windows))]
mod follow {
    use crate::db::Db;
    use std::path::Path;

    #[derive(Clone, Copy)]
    pub struct Position;

    pub fn encode(_position: Position) -> String {
        String::new()
    }

    pub fn current_position(_root: &Path) -> Option<Position> {
        None
    }

    #[derive(Default)]
    pub struct Followers;

    impl Followers {
        pub fn sync(&mut self, _db: &Db) {}
        pub fn poll(&mut self, _db: &Db) {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn service_runs_a_queued_scan_job_for_a_plain_folder_and_keeps_beating() {
        // Pasta comum (não é raiz de HD NTFS): o serviço varre pelo caminho normal e conclui a tarefa.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("pasta");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.txt"), b"1").unwrap();
        let db_path = dir.path().join("catalogo.db");
        let db = Db::open(&db_path).unwrap();
        let source = db.add_source("pasta", &root.to_string_lossy()).unwrap();
        let link = ServiceLink::open(&db_path).unwrap();
        let job_id = link.enqueue_job("scan", source).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread_path = db_path.clone();
        let handle = std::thread::spawn(move || run_loop(&thread_path, &thread_stop));

        let started = Instant::now();
        let mut state = String::new();
        while started.elapsed() < Duration::from_secs(20) {
            state = link.job(job_id).unwrap().unwrap().state;
            if state == "done" || state == "failed" {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        assert!(link.is_alive(), "o serviço devia estar dando sinal de vida");
        stop.store(true, Ordering::Relaxed);
        handle.join().unwrap();

        assert_eq!(state, "done");
        assert_eq!(db.search("a.txt", None, 5).unwrap().len(), 1);
        assert!(!link.is_alive(), "ao parar, o sinal de vida é zerado");
    }

    #[test]
    fn heartbeat_keeps_beating_while_the_catalog_is_locked_by_a_long_write() {
        // O defeito antigo: uma varredura longa segurava o banco e o serviço parecia morto.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("catalogo.db");
        let writer = Db::open(&db_path).unwrap();
        writer.conn.execute_batch("BEGIN IMMEDIATE").unwrap(); // trava a escrita do catálogo

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread_path = db_path.clone();
        let handle = std::thread::spawn(move || run_loop(&thread_path, &thread_stop));

        let link = ServiceLink::open(&db_path).unwrap();
        let started = Instant::now();
        while !link.is_alive() && started.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(link.is_alive(), "o sinal de vida não pode depender do catálogo");
        std::thread::sleep(Duration::from_millis(2500));
        assert!(link.is_alive());
        // e uma tarefa nova continua sendo aceita pelo app durante a varredura
        link.enqueue_job("scan", 99).unwrap();

        stop.store(true, Ordering::Relaxed);
        handle.join().unwrap();
        writer.conn.execute_batch("ROLLBACK").unwrap();
    }

    #[test]
    fn sddl_lets_logged_on_users_start_and_stop_but_not_reconfigure() {
        // IU = usuário logado: RP (iniciar) e WP (parar), sem DC (alterar configuração) nem DT/SD (apagar)
        let interactive = SERVICE_SDDL.split(";;;IU").next().unwrap().rsplit('(').next().unwrap();
        assert!(interactive.contains("RP") && interactive.contains("WP"), "{interactive}");
        assert!(!interactive.contains("DC") && !interactive.contains("SD"), "{interactive}");
        assert!(SERVICE_SDDL.starts_with("D:"));
    }
}
