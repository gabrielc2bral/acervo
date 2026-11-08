//! Parte do Acervo que roda como administrador, dentro do serviço do Windows.
//!
//! O app comum não precisa de administrador: ele só deixa tarefas na tabela `service_jobs` do
//! catálogo e lê o andamento de volta. O serviço:
//! 1. faz a varredura rápida da MFT dos HDs NTFS (a que exige administrador);
//! 2. acompanha o diário USN de cada HD, que registra tudo que foi criado, apagado ou renomeado,
//!    inclusive enquanto o Acervo estava fechado, e ajusta o catálogo só nesses arquivos.
//!
//! Instalar o serviço pede a permissão do Windows uma única vez.

use crate::db::{Db, Job};
use crate::scan::{self, Outcome, Progress};
use crate::settings::Settings;
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const SERVICE_NAME: &str = "AcervoService";
const HEARTBEAT_KEY: &str = "service_heartbeat";
/// O serviço é considerado ativo se deu sinal de vida nos últimos segundos.
const ALIVE_WINDOW_SECS: i64 = 10;

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// O serviço está rodando agora? (Ele escreve um sinal de vida no catálogo a cada segundo.)
pub fn is_alive(db: &Db) -> bool {
    db.get_setting(HEARTBEAT_KEY)
        .ok()
        .flatten()
        .and_then(|text| text.parse::<i64>().ok())
        .is_some_and(|beat| now_secs() - beat <= ALIVE_WINDOW_SECS)
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

/// Pede uma tarefa ao serviço e devolve o id dela.
pub fn request_scan(db: &Db, source_id: i64) -> Result<i64> {
    db.enqueue_job("scan", source_id)
}

fn position_key(source_id: i64) -> String {
    format!("usn:{source_id}")
}

/// Laço do serviço. Roda até `stop` ficar verdadeiro.
pub fn run_loop(db_path: &Path, stop: &AtomicBool) {
    let Ok(db) = Db::open(db_path) else { return };
    let mut followers = follow::Followers::default();
    let mut last_sync: Option<Instant> = None;
    let mut last_trim = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        let _ = db.set_setting(HEARTBEAT_KEY, &now_secs().to_string());

        if let Ok(Some(job)) = db.next_queued_job() {
            run_job(&db, db_path, job);
        }
        if last_sync.is_none_or(|at| at.elapsed() > Duration::from_secs(20)) {
            followers.sync(&db);
            last_sync = Some(Instant::now());
        }
        followers.poll(&db);

        if last_trim.elapsed() > Duration::from_secs(600) {
            let _ = db.trim_changes(200_000);
            let _ = db.clean_jobs();
            last_trim = Instant::now();
        }
        for _ in 0..10 {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    let _ = db.set_setting(HEARTBEAT_KEY, "0");
}

fn run_job(db: &Db, db_path: &Path, job: Job) {
    let finish = |state: &str, message: &str| {
        let _ = db.set_job(job.id, state, message);
    };
    if job.kind != "scan" {
        return finish("failed", "tarefa desconhecida");
    }
    let Ok(sources) = db.sources() else {
        return finish("failed", "não foi possível ler as fontes");
    };
    let Some(source) = sources.into_iter().find(|source| source.id == job.source_id) else {
        return finish("failed", "fonte não encontrada");
    };
    let root = PathBuf::from(&source.path);
    let Ok(mut scan_db) = Db::open(db_path) else {
        return finish("failed", "não foi possível abrir o catálogo");
    };
    let options = Settings::load(db).map(|s| s.scan_options()).unwrap_or_default();
    let _ = db.set_job(job.id, "running", "Iniciando a varredura rápida...");

    // A posição do diário é anotada ANTES de varrer: o que mudar durante a varredura será
    // aplicado depois (aplicar uma mudança duas vezes não faz mal).
    let start_position = follow::current_position(&root);

    let cancel = AtomicBool::new(false);
    let mut last_report = Instant::now();
    let mut report = |progress: &Progress| {
        if last_report.elapsed() < Duration::from_millis(700) {
            return;
        }
        last_report = Instant::now();
        let _ = db.set_job_progress(
            job.id,
            &format!("{} arquivos lidos pelo serviço", progress.files),
        );
        if db.job(job.id).ok().flatten().is_some_and(|current| current.state == "cancel") {
            cancel.store(true, Ordering::Relaxed);
        }
    };

    match scan::scan_source(&mut scan_db, job.source_id, &root, &options, &cancel, &mut report) {
        Ok(Outcome::Done(progress)) => {
            if let Some(position) = start_position {
                let _ = db.set_setting(&position_key(job.source_id), &follow::encode(position));
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
                        self.active.insert(source.id, Follower { source_id: source.id, root, journal });
                    }
                    Err(_) => {
                        // Diário recriado ou posição perdida: não dá para saber o que mudou.
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
                    for record in &page.records {
                        let _ = apply_record(db, follower, record, skip_hidden);
                    }
                    let _ = db.set_setting(&position_key(follower.source_id), &encode(follower.journal.position()));
                    if page.caught_up {
                        break;
                    }
                }
            }
        }
    }

    /// Só os registros de fechamento (`CLOSE`) trazem o resultado final de uma operação.
    fn apply_record(db: &Db, follower: &Follower, record: &UsnRecord, skip_hidden: bool) -> anyhow::Result<()> {
        if !record.reason.contains(Reason::CLOSE) {
            return Ok(());
        }
        let frn = record.file_id.as_reference().map(|reference| (reference & 0x0000_FFFF_FFFF_FFFF) as i64);
        let is_directory = record.file_attributes & 0x10 != 0;

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
            // Marca a fonte; a conferência ao listar tira os caminhos antigos e uma nova
            // varredura traz os novos.
            if record.reason.contains(Reason::RENAME_NEW_NAME) {
                db.set_setting(&format!("usn_dir_moved:{}", follower.source_id), "1")?;
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

    #[test]
    fn alive_only_with_a_fresh_heartbeat() {
        let db = Db::open_in_memory().unwrap();
        assert!(!is_alive(&db));
        db.set_setting(HEARTBEAT_KEY, &now_secs().to_string()).unwrap();
        assert!(is_alive(&db));
        db.set_setting(HEARTBEAT_KEY, &(now_secs() - 60).to_string()).unwrap();
        assert!(!is_alive(&db));
        db.set_setting(HEARTBEAT_KEY, "0").unwrap();
        assert!(!is_alive(&db));
    }

    #[test]
    fn service_runs_a_queued_scan_job_for_a_plain_folder() {
        // Pasta comum (não é raiz de HD NTFS): o serviço varre pelo caminho normal e conclui a tarefa.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("pasta");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.txt"), b"1").unwrap();
        let db_path = dir.path().join("catalogo.db");
        let db = Db::open(&db_path).unwrap();
        let source = db.add_source("pasta", &root.to_string_lossy()).unwrap();
        let job_id = request_scan(&db, source).unwrap();

        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread_path = db_path.clone();
        let handle = std::thread::spawn(move || run_loop(&thread_path, &thread_stop));

        let started = Instant::now();
        let mut state = String::new();
        while started.elapsed() < Duration::from_secs(15) {
            state = db.job(job_id).unwrap().unwrap().state;
            if state == "done" || state == "failed" {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        assert!(is_alive(&db), "o serviço devia estar dando sinal de vida");
        stop.store(true, Ordering::Relaxed);
        handle.join().unwrap();

        assert_eq!(state, "done");
        assert_eq!(db.search("a.txt", None, 5).unwrap().len(), 1);
        assert!(!is_alive(&db), "ao parar, o sinal de vida é zerado");
    }

    #[test]
    fn job_queue_goes_in_order() {
        let db = Db::open_in_memory().unwrap();
        let first = db.enqueue_job("scan", 1).unwrap();
        let second = db.enqueue_job("scan", 2).unwrap();
        assert_eq!(db.next_queued_job().unwrap().unwrap().id, first);
        db.set_job(first, "done", "ok").unwrap();
        assert_eq!(db.next_queued_job().unwrap().unwrap().id, second);
    }
}
