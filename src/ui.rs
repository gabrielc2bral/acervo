//! Janela principal (Slint). Toda a lógica fica em `acervo::*`; aqui só liga a tela ao núcleo.
//! Trabalho pesado (banco, varredura, miniaturas, VeraCrypt) roda em outra thread para a janela
//! não travar.

use acervo::{
    db::{Db, FileRow, SearchQuery, Sort},
    dedupe, fast,
    index::{Index, NewEntry},
    scan::{self, Outcome, Progress, ScanOptions},
    service,
    settings::{AdminMode, Settings},
    steam,
    thumbs::{self, Mode},
    vault,
};
use anyhow::Result;
use slint::{Color, ComponentHandle, Model, ModelRc, Timer, TimerMode, VecModel, Weak};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

slint::include_modules!();

const PAGE_SIZE: usize = 100;
const VAULT_LIMIT: usize = 500;
const THUMB_THREADS: usize = 3;
const ALL_SOURCES: &str = "Todas as fontes";
const MB: i64 = 1024 * 1024;

/// Contadores que invalidam buscas e miniaturas antigas quando chega uma busca nova.
#[derive(Clone, Default)]
struct Generations {
    library: Arc<AtomicU64>,
    vault: Arc<AtomicU64>,
    games: Arc<AtomicU64>,
    dupes: Arc<AtomicU64>,
}

/// Índice de nomes em memória para a busca da Biblioteca.
/// - Ao abrir o app, carrega o índice salvo em disco (se for do mesmo catálogo); senão monta do zero.
/// - Depois só aplica o que mudou (arquivo criado/apagado), lendo o registro `changes` do catálogo.
/// - Só é refeito do zero depois de uma varredura, de remover uma fonte ou de muitas lápides.
#[derive(Clone, Default)]
struct IndexHandle {
    slot: Arc<RwLock<Option<Index>>>,
    building: Arc<AtomicBool>,
    dirty: Arc<AtomicBool>,
    /// Última mudança do registro já aplicada ao índice.
    last_seq: Arc<AtomicI64>,
    polling: Arc<AtomicBool>,
    /// Desde quando há mudanças aplicadas que ainda não foram salvas em disco.
    unsaved_since: Arc<Mutex<Option<Instant>>>,
}

impl IndexHandle {
    fn search(&self, query: &SearchQuery) -> Option<acervo::index::Found> {
        self.slot.read().ok()?.as_ref().map(|index| index.search(query))
    }

    fn file(db_path: &Path) -> PathBuf {
        db_path.with_file_name("index.bin")
    }

    /// Refaz do zero, em segundo plano.
    fn rebuild(&self, db_path: PathBuf) {
        self.load_or_build(db_path, false);
    }

    /// Carrega o índice salvo (`use_saved`) ou monta do zero, em segundo plano. Pedidos que chegam
    /// durante o trabalho viram uma única nova rodada.
    fn load_or_build(&self, db_path: PathBuf, use_saved: bool) {
        if self.building.swap(true, Ordering::SeqCst) {
            self.dirty.store(true, Ordering::SeqCst);
            return;
        }
        let handle = self.clone();
        std::thread::spawn(move || {
            let mut use_saved = use_saved;
            loop {
                handle.dirty.store(false, Ordering::SeqCst);
                if let Ok(db) = Db::open(&db_path) {
                    let seq = db.last_change_seq().unwrap_or(0);
                    let stamp = db.catalog_stamp().unwrap_or((0, 0, 0));
                    let file = Self::file(&db_path);
                    let saved = if use_saved { Index::load(&file, stamp) } else { None };
                    let from_disk = saved.is_some();
                    let index = match saved {
                        Some(index) => Ok(index),
                        None => Index::build(&db),
                    };
                    if let Ok(index) = index {
                        if !from_disk {
                            let _ = index.save(&file, stamp);
                        }
                        if let Ok(mut slot) = handle.slot.write() {
                            *slot = Some(index);
                        }
                        handle.last_seq.store(seq, Ordering::SeqCst);
                    }
                }
                use_saved = false;
                if !handle.dirty.load(Ordering::SeqCst) {
                    break;
                }
            }
            handle.building.store(false, Ordering::SeqCst);
        });
    }

    /// Lê as mudanças novas do catálogo e aplica no índice, em segundo plano. `on_changed(n)` é
    /// chamado se algo mudou. Várias chamadas seguidas não se acumulam.
    fn poll(&self, db_path: &Path, on_changed: impl FnOnce(usize) + Send + 'static) {
        if self.polling.swap(true, Ordering::SeqCst) {
            return;
        }
        let handle = self.clone();
        let db_path = db_path.to_path_buf();
        std::thread::spawn(move || {
            let changed = handle.apply_changes(&db_path).unwrap_or(0);
            if changed == 0 {
                handle.save_if_idle(&db_path);
            }
            handle.polling.store(false, Ordering::SeqCst);
            if changed > 0 {
                on_changed(changed);
            }
        });
    }

    fn apply_changes(&self, db_path: &Path) -> Result<usize> {
        if self.slot.read().map_or(true, |slot| slot.is_none()) {
            return Ok(0); // o índice ainda está sendo carregado
        }
        let db = Db::open(db_path)?;
        let changes = db.changes_since(self.last_seq.load(Ordering::SeqCst), 5000)?;
        let Some(last) = changes.last().map(|change| change.0) else {
            return Ok(0);
        };
        let removed: Vec<i64> = changes.iter().filter(|c| c.1 == "del").map(|c| c.2).collect();
        let added_ids: Vec<i64> = changes.iter().filter(|c| c.1 == "add").map(|c| c.2).collect();
        let added: Vec<NewEntry> = db
            .rows_by_ids(&added_ids)?
            .into_iter()
            .map(|row| NewEntry {
                id: row.id,
                name: row.name,
                size: row.size,
                mtime: row.mtime,
                source: row.source_id,
                kind: row.kind,
            })
            .collect();

        let needs_rebuild = match self.slot.write() {
            Ok(mut slot) => slot.as_mut().is_some_and(|index| index.apply(&removed, added)),
            Err(_) => false,
        };
        self.last_seq.store(last, Ordering::SeqCst);
        if let Ok(mut since) = self.unsaved_since.lock() {
            since.get_or_insert_with(Instant::now);
        }
        if needs_rebuild {
            self.rebuild(db_path.to_path_buf()); // lápides demais: refaz limpo
        }
        Ok(changes.len())
    }

    /// Salva o índice em disco um minuto depois da última mudança (para abrir rápido na próxima vez).
    fn save_if_idle(&self, db_path: &Path) {
        let due = self
            .unsaved_since
            .lock()
            .ok()
            .and_then(|since| *since)
            .is_some_and(|since| since.elapsed() > Duration::from_secs(60));
        if !due {
            return;
        }
        if let (Ok(db), Ok(slot)) = (Db::open(db_path), self.slot.read()) {
            if let (Some(index), Ok(stamp)) = (slot.as_ref(), db.catalog_stamp()) {
                if index.save(&Self::file(db_path), stamp).is_ok() {
                    if let Ok(mut since) = self.unsaved_since.lock() {
                        *since = None;
                    }
                }
            }
        }
    }
}

struct App {
    index: IndexHandle,
    db_path: PathBuf,
    settings: RefCell<Settings>,
    cancel: Arc<AtomicBool>,
    generations: Generations,
    /// Página atual da Biblioteca (começa em 0).
    library_page: Cell<usize>,
    last_activity: Cell<Instant>,
}

impl App {
    /// Marca que o usuário mexeu no app (para o bloqueio automático do Cofre).
    fn touch(&self) {
        self.last_activity.set(Instant::now());
    }

    fn vault_catalog(&self) -> Option<PathBuf> {
        vault::catalog_path(self.settings.borrow().vault_letter).ok()
    }
}

pub fn run() -> Result<()> {
    let db_path = Db::default_path()?;
    let settings = Settings::load(&Db::open(&db_path)?)?;
    let app = Rc::new(App {
        index: IndexHandle::default(),
        db_path: db_path.clone(),
        settings: RefCell::new(settings),
        cancel: Arc::new(AtomicBool::new(false)),
        generations: Generations::default(),
        library_page: Cell::new(0),
        last_activity: Cell::new(Instant::now()),
    });

    let ui = MainWindow::new()?;
    let weak = ui.as_weak();
    show_settings(&ui, &app.settings.borrow());
    ui.set_app_version(acervo::version::describe().into());
    ui.set_veracrypt_found(vault::veracrypt_exe().is_some());
    refresh_sources(&ui, &db_path)?;
    refresh_drives(&ui);
    // Abre rápido: usa o índice salvo em disco quando o catálogo não mudou desde que foi salvo.
    app.index.load_or_build(db_path.clone(), true);
    load_games(&weak, &app, 0);
    refresh_service_status(&weak, &db_path);
    // Catálogos gravados por versões antigas ganham as regras novas sem precisar varrer de novo.
    let reclassify_db = db_path.clone();
    background(
        &weak,
        move || {
            let mut db = Db::open(&reclassify_db)?;
            scan::reclassify(&mut db)?;
            // Uma vez só: versões antigas guardavam o ícone genérico do Windows como se fosse
            // miniatura. Apaga essas repetidas; as de verdade voltam sozinhas ao gerar de novo.
            if db.get_setting("thumbs_purged_generic")?.is_none() {
                let purged = db.purge_generic_thumbs()?;
                db.set_setting("thumbs_purged_generic", "1")?;
                return Ok(purged);
            }
            Ok::<usize, anyhow::Error>(0)
        },
        {
            let db_path = db_path.clone();
            let (cancel, threads, auto) = {
                let settings = app.settings.borrow();
                (app.cancel.clone(), thumb_threads(), settings.auto_thumbs)
            };
            move |ui, purged: Result<usize>| {
                refresh_stats(&ui.as_weak(), &db_path);
                let purged = purged.unwrap_or(0);
                if purged > 0 && auto {
                    let prefix = format!("Limpei {} miniaturas genéricas antigas.", thousands(purged as i64));
                    start_thumb_batch(&ui.as_weak(), db_path, Vec::new(), cancel, threads, prefix);
                }
            }
        },
    );

    ui.on_page_changed({
        let (weak, app) = (weak.clone(), app.clone());
        move |page| {
            app.touch();
            let Some(ui) = weak.upgrade() else { return };
            match page {
                0 => {
                    let _ = refresh_sources(&ui, &app.db_path);
                    refresh_stats(&weak, &app.db_path);
                }
                1 => {
                    let _ = refresh_sources(&ui, &app.db_path);
                    app.library_page.set(0);
                    run_library_search(&weak, &app, ui.get_library_query().to_string(), ui.get_kind_filter().to_string());
                }
                2 => load_games(&weak, &app, ui.get_games_tab()),
                3 => {
                    let _ = refresh_sources(&ui, &app.db_path);
                    refresh_drives(&ui);
                }
                4 => load_duplicates(&weak, app.db_path.clone(), app.generations.dupes.clone()),
                6 => refresh_service_status(&weak, &app.db_path),
                _ => {}
            }
        }
    });

    // O catálogo se atualiza sozinho quando algo é apagado/criado/renomeado nas fontes conectadas.
    // (O vigia grava as mudanças no catálogo; quem avisa a tela é a leitura periódica abaixo, que
    // também pega o que o serviço do Windows e o diário USN gravam.)
    let _watcher_stop = acervo::watch::spawn(db_path.clone(), |_changed| {});
    let index_poll = Timer::default();
    index_poll.start(TimerMode::Repeated, Duration::from_millis(1500), {
        let (weak, app) = (weak.clone(), app.clone());
        move || {
            let weak = weak.clone();
            app.index.poll(&app.db_path, move |_changed| {
                let _ = weak.upgrade_in_event_loop(|ui| ui.set_catalog_version(ui.get_catalog_version() + 1));
            });
        }
    });

    ui.on_catalog_changed({
        let (weak, app) = (weak.clone(), app.clone());
        move |page| {
            let Some(ui) = weak.upgrade() else { return };
            if ui.get_scanning() {
                return; // durante varredura a tela atualiza no fim
            }
            // Atualiza a tela que está aberta, sem sair da página em que o usuário está.
            match page {
                0 => {
                    let _ = refresh_sources(&ui, &app.db_path);
                    refresh_stats(&weak, &app.db_path);
                }
                1 => run_library_search(&weak, &app, ui.get_library_query().to_string(), ui.get_kind_filter().to_string()),
                4 => load_duplicates(&weak, app.db_path.clone(), app.generations.dupes.clone()),
                _ => {}
            }
        }
    });

    ui.on_add_folder({
        let (weak, app) = (weak.clone(), app.clone());
        move || {
            app.touch();
            let Some(folder) = rfd::FileDialog::new().pick_folder() else { return };
            add_and_scan(&weak, &app, &folder);
        }
    });

    ui.on_add_drive({
        let (weak, app) = (weak.clone(), app.clone());
        move |drive| {
            app.touch();
            if !confirm(
                "Adicionar HD",
                format!(
                    "Varrer o HD inteiro {drive}?\n\nPode demorar e usa o disco por um tempo. O Acervo só lê os nomes e tamanhos; nada é alterado nem apagado. Arquivos do sistema são ignorados."
                ),
            ) {
                return;
            }
            add_and_scan(&weak, &app, Path::new(drive.as_str()));
        }
    });

    ui.on_scan_source({
        let (weak, app) = (weak.clone(), app.clone());
        move |id| {
            app.touch();
            let Some(ui) = weak.upgrade() else { return };
            let name = ui
                .get_sources()
                .iter()
                .find(|source| source.id == id)
                .map(|source| source.name.to_string())
                .unwrap_or_default();
            if !confirm(
                "Varrer de novo",
                format!(
                    "Varrer \"{name}\" de novo?\n\nO catálogo dessa fonte é substituído pelo resultado da nova varredura (arquivos novos entram, os que sumiram saem). Nada é alterado no disco.\n\nSe você cancelar no meio, o catálogo anterior é mantido."
                ),
            ) {
                return;
            }
            start_scan(&weak, &app, app.db_path.clone(), id as i64, false);
        }
    });

    ui.on_remove_source({
        let (weak, app) = (weak.clone(), app.clone());
        move |id| {
            let Some(ui) = weak.upgrade() else { return };
            let name = ui
                .get_sources()
                .iter()
                .find(|source| source.id == id)
                .map(|source| source.name.to_string())
                .unwrap_or_default();
            let files = Db::open(&app.db_path)
                .and_then(|db| db.source_file_count(id as i64))
                .unwrap_or(0);
            let confirmed = confirm(
                "Remover fonte do catálogo",
                format!(
                    "Remover \"{name}\" do Acervo?\n\n{} arquivos catalogados (e as miniaturas deles) saem do catálogo. Para voltar, é preciso varrer de novo.\n\nOs arquivos no disco NÃO são apagados.",
                    thousands(files)
                ),
            );
            if !confirmed {
                return;
            }
            match Db::open(&app.db_path).and_then(|db| db.remove_source(id as i64)) {
                Ok(()) => {
                    let _ = refresh_sources(&ui, &app.db_path);
                    refresh_stats(&weak, &app.db_path);
                    app.index.rebuild(app.db_path.clone());
                }
                Err(error) => ui.set_status(format!("Erro ao remover: {error}").into()),
            }
        }
    });

    ui.on_cancel_scan({
        let app = app.clone();
        move || {
            if confirm(
                "Cancelar",
                "Cancelar a operação em andamento?\n\nNa varredura, o catálogo anterior é mantido. Nas miniaturas, o que já foi gerado fica guardado.".to_string(),
            ) {
                app.cancel.store(true, Ordering::Relaxed);
            }
        }
    });

    // Espera o usuário parar de digitar (250 ms) antes de buscar, em vez de buscar a cada letra.
    let search_debounce = Rc::new(Timer::default());
    ui.on_search({
        let (weak, app) = (weak.clone(), app.clone());
        move |text, kind| {
            app.library_page.set(0); // filtro novo: volta para a primeira página
            let (weak, app) = (weak.clone(), app.clone());
            let (text, kind) = (text.to_string(), kind.to_string());
            search_debounce.start(TimerMode::SingleShot, Duration::from_millis(250), move || {
                run_library_search(&weak, &app, text.clone(), kind.clone())
            });
        }
    });

    ui.on_library_page_prev({
        let (weak, app) = (weak.clone(), app.clone());
        move || {
            let Some(ui) = weak.upgrade() else { return };
            app.library_page.set(app.library_page.get().saturating_sub(1));
            run_library_search(&weak, &app, ui.get_library_query().to_string(), ui.get_kind_filter().to_string());
        }
    });

    ui.on_library_page_next({
        let (weak, app) = (weak.clone(), app.clone());
        move || {
            let Some(ui) = weak.upgrade() else { return };
            app.library_page.set(app.library_page.get() + 1);
            run_library_search(&weak, &app, ui.get_library_query().to_string(), ui.get_kind_filter().to_string());
        }
    });

    ui.on_generate_thumbs({
        let (weak, app) = (weak.clone(), app.clone());
        move || {
            app.touch();
            if !confirm(
                "Gerar miniaturas",
                "Gerar as miniaturas que faltam dos arquivos dos HDs conectados?\n\nRoda em segundo plano e pode levar alguns minutos. Só tenta foto, vídeo e programa que o Windows consegue mostrar. Nada é alterado nos arquivos.".to_string(),
            ) {
                return;
            }
            // Miniatura é leve para o PC: usa pelo menos 4 threads, mesmo com a varredura em 2.
            let threads = thumb_threads();
            start_thumb_batch(&weak, app.db_path.clone(), Vec::new(), app.cancel.clone(), threads, String::new());
        }
    });

    ui.on_select_file({
        let weak = weak.clone();
        move |index| {
            let Some(ui) = weak.upgrade() else { return };
            if let Some(item) = ui.get_results().row_data(index as usize) {
                ui.set_selected(item);
                ui.set_selected_index(index);
            }
        }
    });

    ui.on_reveal(|path| reveal_in_explorer(path.as_str()));

    let games_debounce = Rc::new(Timer::default());
    ui.on_games_tab_changed({
        let (weak, app) = (weak.clone(), app.clone());
        move |tab| {
            app.touch();
            load_games(&weak, &app, tab);
        }
    });
    ui.on_games_filter_changed({
        let (weak, app) = (weak.clone(), app.clone());
        move || {
            let (weak, app) = (weak.clone(), app.clone());
            games_debounce.start(TimerMode::SingleShot, Duration::from_millis(250), move || {
                if let Some(ui) = weak.upgrade() {
                    load_games(&weak, &app, ui.get_games_tab());
                }
            });
        }
    });

    let dupes_debounce = Rc::new(Timer::default());
    ui.on_dupes_filter_changed({
        let (weak, app) = (weak.clone(), app.clone());
        move || {
            let (weak, app) = (weak.clone(), app.clone());
            dupes_debounce.start(TimerMode::SingleShot, Duration::from_millis(250), move || {
                load_duplicates(&weak, app.db_path.clone(), app.generations.dupes.clone())
            });
        }
    });

    ui.on_settings_changed({
        let (weak, app) = (weak.clone(), app.clone());
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let updated = Settings {
                threads: ui.get_set_threads().round() as usize,
                skip_hidden: ui.get_set_skip_hidden(),
                auto_thumbs: ui.get_set_auto_thumbs(),
                admin_mode: admin_mode_from_label(ui.get_set_admin_mode().as_str()),
                autolock_minutes: ui.get_set_autolock().round() as u32,
                vault_letter: ui.get_set_letter().chars().next().unwrap_or('V'),
                vault_container: app.settings.borrow().vault_container.clone(),
            };
            match Db::open(&app.db_path).and_then(|db| updated.save(&db)) {
                Ok(()) => *app.settings.borrow_mut() = updated,
                Err(error) => ui.set_status(format!("Erro ao salvar: {error}").into()),
            }
        }
    });

    ui.on_install_service({
        let (weak, app) = (weak.clone(), app.clone());
        move || run_service_command(&weak, &app, true)
    });
    ui.on_uninstall_service({
        let (weak, app) = (weak.clone(), app.clone());
        move || run_service_command(&weak, &app, false)
    });

    wire_vault(&ui, &app);
    wire_duplicates(&ui, &app);

    // Bloqueio automático do Cofre depois de um tempo parado.
    let autolock = Timer::default();
    autolock.start(TimerMode::Repeated, Duration::from_secs(20), {
        let (weak, app) = (weak.clone(), app.clone());
        move || {
            let Some(ui) = weak.upgrade() else { return };
            if !ui.get_vault_unlocked() || ui.get_vault_busy() || ui.get_scanning() {
                return;
            }
            let limit = Duration::from_secs(app.settings.borrow().autolock_minutes as u64 * 60);
            if app.last_activity.get().elapsed() >= limit {
                lock_vault(&weak, &app); // bloqueio automático: sem pergunta
            }
        }
    });

    // Ao fechar com o Cofre aberto, desmonta (sem forçar).
    ui.window().on_close_requested({
        let (weak, app) = (weak.clone(), app.clone());
        move || {
            let busy = weak
                .upgrade()
                .is_some_and(|ui| ui.get_scanning() || ui.get_vault_busy());
            if busy
                && !confirm(
                    "Fechar o Acervo",
                    "Há uma varredura ou o Cofre está em andamento.\n\nFechar agora cancela a operação (o catálogo anterior é mantido). Fechar mesmo assim?".to_string(),
                )
            {
                return slint::CloseRequestResponse::KeepWindowShown;
            }
            app.cancel.store(true, Ordering::Relaxed);
            if weak.upgrade().is_some_and(|ui| ui.get_vault_unlocked()) {
                let _ = vault::dismount(app.settings.borrow().vault_letter);
            }
            slint::CloseRequestResponse::HideWindow
        }
    });

    ui.run()?;
    Ok(())
}

/// Janela de confirmação (Sim/Não) para ações que mudam ou apagam coisas.
fn confirm(title: &str, description: String) -> bool {
    rfd::MessageDialog::new()
        .set_title(title)
        .set_description(description)
        .set_level(rfd::MessageLevel::Warning)
        .set_buttons(rfd::MessageButtons::YesNo)
        .show()
        == rfd::MessageDialogResult::Yes
}

/// Abre o Explorer com o arquivo selecionado. O Explorer exige `/select,"caminho"` com aspas só
/// no caminho; se o Rust pusesse aspas no argumento inteiro, ele abriria em Documentos.
#[cfg(windows)]
fn reveal_in_explorer(path: &str) {
    use std::os::windows::process::CommandExt;
    let _ = std::process::Command::new("explorer")
        .raw_arg(format!("/select,\"{path}\""))
        .spawn();
}

#[cfg(not(windows))]
fn reveal_in_explorer(_path: &str) {}

// ---------------------------------------------------------------- Miniaturas

#[derive(Clone, Copy)]
enum Target {
    Library,
    Vault,
    Games,
    Dupes,
}

struct ThumbJob {
    index: usize,
    kind: String,
    path: PathBuf,
    /// Capa pronta (chave, arquivo de imagem), como as da Steam.
    cover: Option<(String, PathBuf)>,
}

fn to_image(thumb: &thumbs::Thumb) -> slint::Image {
    let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
        &thumb.rgba,
        thumb.width,
        thumb.height,
    );
    slint::Image::from_rgba8(buffer)
}

fn update_row<T: Clone + 'static>(model: ModelRc<T>, index: usize, change: impl FnOnce(&mut T)) {
    if let Some(mut row) = model.row_data(index) {
        change(&mut row);
        model.set_row_data(index, row);
    }
}

fn apply_thumb(ui: &MainWindow, target: Target, index: usize, image: slint::Image) {
    match target {
        Target::Library => update_row(ui.get_results(), index, |row| {
            row.thumb = image;
            row.has_thumb = true;
        }),
        Target::Vault => update_row(ui.get_vault_results(), index, |row| {
            row.thumb = image;
            row.has_thumb = true;
        }),
        Target::Games => update_row(ui.get_games(), index, |row| {
            row.thumb = image;
            row.has_thumb = true;
        }),
        Target::Dupes => update_row(ui.get_dupes(), index, |row| {
            row.thumb = image;
            row.has_thumb = true;
        }),
    }
}

/// Cache em memória das miniaturas já decodificadas (só na thread da janela).
/// Trocar de tela não busca nem decodifica de novo o que já foi visto: as miniaturas aparecem na
/// hora. O cache só descarta algo quando enche (as mais antigas saem primeiro) ou quando a
/// miniatura realmente muda. O Cofre não passa por aqui: ao bloquear, nada dele fica na memória.
const THUMB_CACHE_LIMIT: usize = 2500;

#[derive(Default)]
struct ThumbCache {
    images: HashMap<i64, (slint::Image, u64)>,
    /// Arquivos que já sabemos que não têm miniatura (evita perguntar ao Windows toda hora).
    absent: std::collections::HashSet<i64>,
    tick: u64,
}

impl ThumbCache {
    fn get(&mut self, key: i64) -> Option<slint::Image> {
        self.tick += 1;
        let tick = self.tick;
        self.images.get_mut(&key).map(|(image, used)| {
            *used = tick;
            image.clone()
        })
    }

    fn put(&mut self, key: i64, image: slint::Image) {
        self.tick += 1;
        self.absent.remove(&key);
        self.images.insert(key, (image, self.tick));
        if self.images.len() > THUMB_CACHE_LIMIT {
            // Enche: descarta os 10% menos usados recentemente.
            let mut by_age: Vec<(u64, i64)> = self.images.iter().map(|(key, (_, used))| (*used, *key)).collect();
            by_age.sort_unstable();
            for (_, key) in by_age.into_iter().take(THUMB_CACHE_LIMIT / 10) {
                self.images.remove(&key);
            }
        }
    }
}

thread_local! {
    static THUMB_CACHE: RefCell<ThumbCache> = RefCell::new(ThumbCache::default());
}

/// Chave do banco/cache de uma miniatura (a mesma de `thumbs::key_for`).
fn job_key(job: &ThumbJob) -> i64 {
    match &job.cover {
        Some((key, _)) => thumbs::key_for(key),
        None => thumbs::key_for(&job.path.to_string_lossy()),
    }
}

/// Esquece quem "não tem miniatura": depois de gerar miniaturas novas, vale perguntar de novo.
fn forget_absent_thumbs() {
    THUMB_CACHE.with(|cache| cache.borrow_mut().absent.clear());
}

/// Coloca as miniaturas na tela. As que já estão no cache entram na hora; só o que falta é
/// buscado (SQLite, cache do Windows) em segundo plano, e cada uma aparece quando fica pronta.
/// As miniaturas ficam no SQLite de `db_path` (no Cofre, dentro do volume VeraCrypt).
fn spawn_thumbs(
    weak: &Weak<MainWindow>,
    target: Target,
    generation: Arc<AtomicU64>,
    my_generation: u64,
    db_path: PathBuf,
    mode: Mode,
    jobs: Vec<ThumbJob>,
) {
    let use_cache = !matches!(target, Target::Vault);
    let mut missing: Vec<(i64, ThumbJob)> = Vec::new();
    for job in jobs {
        let key = job_key(&job);
        if use_cache {
            let cached = THUMB_CACHE.with(|cache| {
                let mut cache = cache.borrow_mut();
                if cache.absent.contains(&key) {
                    return Some(None); // já sabemos que não tem
                }
                cache.get(key).map(Some)
            });
            match cached {
                Some(Some(image)) => {
                    if let Some(ui) = weak.upgrade() {
                        apply_thumb(&ui, target, job.index, image);
                    }
                    continue;
                }
                Some(None) => continue,
                None => {}
            }
        }
        missing.push((key, job));
    }

    let mut lanes: Vec<Vec<(i64, ThumbJob)>> = (0..THUMB_THREADS).map(|_| Vec::new()).collect();
    for (position, item) in missing.into_iter().enumerate() {
        lanes[position % THUMB_THREADS].push(item);
    }
    for lane in lanes.into_iter().filter(|lane| !lane.is_empty()) {
        let (weak, generation, db_path) = (weak.clone(), generation.clone(), db_path.clone());
        std::thread::spawn(move || {
            let Ok(db) = Db::open(&db_path) else { return };
            for (key, job) in lane {
                if generation.load(Ordering::SeqCst) != my_generation {
                    return; // chegou uma busca nova: essas miniaturas não interessam mais
                }
                let thumb = match &job.cover {
                    Some((cover_key, file)) => thumbs::thumb_from_image(&db, cover_key, file),
                    None => thumbs::thumb_for(&db, &job.path, &job.kind, mode),
                };
                let generation = generation.clone();
                let _ = weak.upgrade_in_event_loop(move |ui| match thumb {
                    Some(thumb) => {
                        let image = to_image(&thumb);
                        if use_cache {
                            THUMB_CACHE.with(|cache| cache.borrow_mut().put(key, image.clone()));
                        }
                        if generation.load(Ordering::SeqCst) == my_generation {
                            apply_thumb(&ui, target, job.index, image);
                        }
                    }
                    None if use_cache => THUMB_CACHE.with(|cache| {
                        cache.borrow_mut().absent.insert(key);
                    }),
                    None => {}
                });
            }
        });
    }
}

fn jobs_for_rows(rows: &[(FileRow, bool)], only_photos: bool) -> Vec<ThumbJob> {
    rows.iter()
        .enumerate()
        .filter(|(_, (row, _))| {
            // Só arquivos que podem ter miniatura; o resto fica com o quadrado colorido.
            (!only_photos || row.kind == "photo")
                && thumbs::can_have_thumbnail(Path::new(&row.name), &row.kind)
        })
        .map(|(index, (row, _))| ThumbJob {
            index,
            kind: row.kind.clone(),
            path: PathBuf::from(&row.path),
            cover: None,
        })
        .collect()
}

// ---------------------------------------------------------------- Biblioteca e Cofre

fn sort_from_label(label: &str) -> Sort {
    match label {
        "Maiores" => Sort::Largest,
        "Menores" => Sort::Smallest,
        "Mais recentes" => Sort::Newest,
        "Mais antigos" => Sort::Oldest,
        _ => Sort::Name,
    }
}

fn min_size_from_label(label: &str) -> i64 {
    match label {
        "1 MB+" => MB,
        "10 MB+" => 10 * MB,
        "100 MB+" => 100 * MB,
        "1 GB+" => 1024 * MB,
        "10 GB+" => 10 * 1024 * MB,
        _ => 0,
    }
}

fn kind_filter(kind: &str) -> Option<String> {
    (!kind.is_empty()).then(|| kind.to_string())
}

/// Busca da Biblioteca com os filtros que estão na tela.
fn run_library_search(weak: &Weak<MainWindow>, app: &Rc<App>, text: String, kind: String) {
    app.touch();
    let Some(ui) = weak.upgrade() else { return };
    let source = ui.get_library_source().to_string();
    let query = SearchQuery {
        text: text.trim().to_string(),
        kind: kind_filter(&kind),
        source_ids: Vec::new(),
        min_size: min_size_from_label(ui.get_library_size().as_str()),
        sort: sort_from_label(ui.get_library_sort().as_str()),
        // Uma linha a mais só para saber se existe próxima página.
        limit: PAGE_SIZE + 1,
        offset: app.library_page.get() * PAGE_SIZE,
    };
    ui.set_library_page(app.library_page.get() as i32);
    // Ao listar, só usa o cache de miniaturas do Windows: nunca espera o Windows gerar uma.
    run_listing(
        weak,
        app.db_path.clone(),
        query,
        (source != ALL_SOURCES).then_some(source),
        ui.get_library_online_only(),
        Target::Library,
        app.generations.library.clone(),
        Mode::WindowsCache,
        Some(app.index.clone()),
    );
}

/// Busca dentro do Cofre (catálogo no volume VeraCrypt).
fn run_vault_search(weak: &Weak<MainWindow>, catalog: PathBuf, generation: Arc<AtomicU64>) {
    let Some(ui) = weak.upgrade() else { return };
    let query = SearchQuery {
        text: ui.get_vault_query().trim().to_string(),
        kind: kind_filter(ui.get_vault_kind().as_str()),
        source_ids: Vec::new(),
        min_size: 0,
        sort: sort_from_label(ui.get_vault_sort().as_str()),
        limit: VAULT_LIMIT,
        offset: 0,
    };
    // No Cofre o Windows não é consultado para miniaturas (ele guardaria cópias fora do volume).
    run_listing(weak, catalog, query, None, false, Target::Vault, generation, Mode::DecodeOnly, None);
}

#[allow(clippy::too_many_arguments)]
fn run_listing(
    weak: &Weak<MainWindow>,
    db_path: PathBuf,
    query: SearchQuery,
    source_name: Option<String>,
    online_only: bool,
    target: Target,
    generation: Arc<AtomicU64>,
    mode: Mode,
    index: Option<IndexHandle>,
) {
    let my_generation = generation.fetch_add(1, Ordering::SeqCst) + 1;
    let latest = generation.clone();
    let thumbs_db = db_path.clone();
    let count_db = db_path.clone();
    let page_size = query.limit.saturating_sub(usize::from(matches!(target, Target::Library)));
    background(
        weak,
        move || -> Result<(Vec<(FileRow, bool)>, SearchQuery, Option<usize>)> {
            let db = Db::open(&db_path)?;
            let mut query = query;

            // Fonte escolhida e/ou "só online" viram uma lista de ids de fonte.
            let sources = db.sources()?;
            let mut allowed: Option<Vec<i64>> = source_name.as_ref().map(|name| {
                sources.iter().filter(|s| &s.name == name).map(|s| s.id).collect()
            });
            if online_only {
                let online: Vec<i64> = sources
                    .iter()
                    .filter(|s| Path::new(&s.path).exists())
                    .map(|s| s.id)
                    .collect();
                allowed = Some(match allowed {
                    Some(chosen) => chosen.into_iter().filter(|id| online.contains(id)).collect(),
                    None => online,
                });
            }
            if let Some(ids) = allowed {
                query.source_ids = if ids.is_empty() { vec![-1] } else { ids };
            }

            // Com o índice em memória a busca leva poucos milissegundos e já traz o total;
            // sem ele (ainda carregando, ou no Cofre) cai no SQLite.
            let (rows, total) = match index.as_ref().and_then(|handle| handle.search(&query)) {
                Some(found) => (db.rows_by_ids(&found.ids)?, Some(found.total)),
                None => (db.search_query(&query)?, None),
            };
            // Confere cada fonte uma vez só: conferir arquivo por arquivo acorda HDs parados.
            let mut source_online: HashMap<String, bool> = HashMap::new();
            let mut gone: Vec<i64> = Vec::new();
            let mut found = Vec::new();
            for row in rows {
                let online = *source_online
                    .entry(row.source_path.clone())
                    .or_insert_with(|| Path::new(&row.source_path).exists());
                // Só os itens desta página (até 100) são conferidos no disco, e só os de HD
                // conectado: se o arquivo não existe mais (apagado fora do app), sai do catálogo.
                if online && !Path::new(&row.path).exists() {
                    gone.push(row.id);
                } else {
                    found.push((row, online));
                }
            }
            if !gone.is_empty() {
                // O registro de mudanças avisa o índice em memória, sem reconstruir.
                db.delete_files(&gone)?;
            }
            Ok((found, query, total.map(|total| total.saturating_sub(gone.len()))))
        },
        move |ui: &MainWindow, result: Result<(Vec<(FileRow, bool)>, SearchQuery, Option<usize>)>| {
            if latest.load(Ordering::SeqCst) != my_generation {
                return; // uma busca mais nova já foi pedida
            }
            let (mut rows, query, total) = match result {
                Ok(found) => found,
                Err(error) => {
                    ui.set_status(format!("Erro na busca: {error}").into());
                    return;
                }
            };
            let has_next = rows.len() > page_size;
            rows.truncate(page_size);
            let jobs = jobs_for_rows(&rows, matches!(target, Target::Vault));
            match target {
                Target::Vault => ui.set_vault_results(file_model(rows)),
                _ => {
                    ui.set_results(file_model(rows));
                    ui.set_library_has_next(has_next);
                    ui.set_selected_index(-1);
                }
            }
            match total {
                // O índice em memória já sabe o total.
                Some(total) => {
                    let text = format!("{} itens", thousands(total as i64));
                    match target {
                        Target::Vault => ui.set_vault_count(text.into()),
                        _ => ui.set_library_count(text.into()),
                    }
                }
                // Pelo SQLite o total é uma consulta à parte: chega depois, sem atrasar a lista.
                None => spawn_count(&ui.as_weak(), target, latest.clone(), my_generation, count_db, query),
            }
            spawn_thumbs(&ui.as_weak(), target, latest, my_generation, thumbs_db, mode, jobs);
        },
    );
}

/// Conta quantos arquivos passam nos filtros (consulta mais lenta que uma página).
fn spawn_count(
    weak: &Weak<MainWindow>,
    target: Target,
    generation: Arc<AtomicU64>,
    my_generation: u64,
    db_path: PathBuf,
    query: SearchQuery,
) {
    background(
        weak,
        move || Db::open(&db_path).and_then(|db| db.count_query(&query)),
        move |ui, total| {
            if generation.load(Ordering::SeqCst) != my_generation {
                return;
            }
            let Ok(total) = total else { return };
            let text = format!("{} itens", thousands(total));
            match target {
                Target::Vault => ui.set_vault_count(text.into()),
                _ => ui.set_library_count(text.into()),
            }
        },
    );
}

fn file_model(rows: Vec<(FileRow, bool)>) -> ModelRc<FileItem> {
    let items: Vec<FileItem> = rows
        .into_iter()
        .map(|(row, online)| {
            let (label, color) = kind_style(&row.kind);
            FileItem {
                ext: ext_label(&row.name).into(),
                date: date_text(row.mtime).into(),
                kind: label.into(),
                color,
                size: human_size(row.size as u64).into(),
                name: row.name.into(),
                path: row.path.into(),
                source: row.source_name.into(),
                online,
                thumb: slint::Image::default(),
                has_thumb: false,
            }
        })
        .collect();
    ModelRc::new(VecModel::from(items))
}

// ---------------------------------------------------------------- Cofre

/// Desmonta o Cofre (sem forçar). Usado pelo botão (depois da confirmação) e pelo bloqueio automático.
fn lock_vault(weak: &Weak<MainWindow>, app: &Rc<App>) {
    let Some(ui) = weak.upgrade() else { return };
    let letter = app.settings.borrow().vault_letter;
    // Invalida buscas e miniaturas em andamento antes de desmontar.
    app.generations.vault.fetch_add(1, Ordering::SeqCst);
    ui.set_vault_busy(true);
    background(
        weak,
        move || vault::dismount(letter).map_err(|error| error.to_string()),
        |ui, result| {
            ui.set_vault_busy(false);
            match result {
                Ok(()) => {
                    ui.set_vault_unlocked(false);
                    ui.set_vault_results(ModelRc::new(VecModel::default()));
                    ui.set_vault_query("".into());
                    ui.set_vault_status("Cofre bloqueado.".into());
                }
                Err(message) => ui.set_status(message.into()),
            }
        },
    );
}

fn wire_vault(ui: &MainWindow, app: &Rc<App>) {
    let weak = ui.as_weak();

    ui.on_vault_pick_container({
        let (weak, app) = (weak.clone(), app.clone());
        move || {
            app.touch();
            let Some(ui) = weak.upgrade() else { return };
            let Some(file) = rfd::FileDialog::new().pick_file() else { return };
            if app.settings.borrow().vault_container.is_some()
                && !confirm(
                    "Trocar o volume do Cofre",
                    format!(
                        "Usar \"{}\" como volume do Cofre?\n\nO catálogo do Cofre atual continua guardado dentro do volume antigo, mas deixa de aparecer aqui.",
                        file.display()
                    ),
                )
            {
                return;
            }
            let mut settings = app.settings.borrow().clone();
            settings.vault_container = Some(file);
            match Db::open(&app.db_path).and_then(|db| settings.save(&db)) {
                Ok(()) => {
                    show_settings(&ui, &settings);
                    *app.settings.borrow_mut() = settings;
                }
                Err(error) => ui.set_vault_status(format!("Erro ao salvar: {error}").into()),
            }
        }
    });

    ui.on_vault_unlock({
        let (weak, app) = (weak.clone(), app.clone());
        move || {
            app.touch();
            let Some(ui) = weak.upgrade() else { return };
            let settings = app.settings.borrow().clone();
            let Some(container) = settings.vault_container.clone() else { return };
            ui.set_vault_busy(true);
            ui.set_vault_status("Digite a senha na janela do VeraCrypt...".into());
            let catalog = app.vault_catalog();
            let generation = app.generations.vault.clone();
            background(
                &weak,
                move || {
                    vault::mount(&container, settings.vault_letter, Duration::from_secs(180))
                        .map_err(|error| error.to_string())
                },
                move |ui, result| {
                    ui.set_vault_busy(false);
                    match result {
                        Ok(()) => {
                            ui.set_vault_unlocked(true);
                            ui.set_vault_status("".into());
                            ui.set_status("".into());
                            if let Some(catalog) = catalog {
                                run_vault_search(&ui.as_weak(), catalog, generation);
                            }
                        }
                        Err(message) => ui.set_vault_status(message.into()),
                    }
                },
            );
        }
    });

    ui.on_vault_lock({
        let (weak, app) = (weak.clone(), app.clone());
        move || {
            if confirm(
                "Bloquear o Cofre",
                "Bloquear o Cofre agora?\n\nO volume VeraCrypt é desmontado e os itens somem da tela. Para ver de novo é preciso digitar a senha no VeraCrypt.".to_string(),
            ) {
                lock_vault(&weak, &app);
            }
        }
    });

    ui.on_vault_search({
        let (weak, app) = (weak.clone(), app.clone());
        move |_text| {
            app.touch();
            if let Some(catalog) = app.vault_catalog() {
                run_vault_search(&weak, catalog, app.generations.vault.clone());
            }
        }
    });

    ui.on_vault_add_folder({
        let (weak, app) = (weak.clone(), app.clone());
        move || {
            app.touch();
            let Some(catalog) = app.vault_catalog() else { return };
            let Some(folder) = rfd::FileDialog::new().pick_folder() else { return };
            let name = folder.to_string_lossy().into_owned();
            match Db::open(&catalog).and_then(|db| db.add_source(&name, &name)) {
                Ok(id) => start_scan(&weak, &app, catalog, id, true),
                Err(error) => {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_status(format!("Erro no Cofre: {error}").into());
                    }
                }
            }
        }
    });
}

// ---------------------------------------------------------------- Varredura

fn add_and_scan(weak: &Weak<MainWindow>, app: &Rc<App>, path: &Path) {
    let name = path.to_string_lossy().into_owned();
    let db_path = app.db_path.clone();
    match Db::open(&db_path).and_then(|db| db.add_source(&name, &name)) {
        Ok(id) => start_scan(weak, app, db_path, id, false),
        Err(error) => {
            if let Some(ui) = weak.upgrade() {
                ui.set_status(format!("Erro: {error}").into());
            }
        }
    }
}

/// Varre uma fonte em outra thread. `db_path` é o banco principal ou o catálogo do Cofre.
fn start_scan(weak: &Weak<MainWindow>, app: &Rc<App>, db_path: PathBuf, source_id: i64, is_vault: bool) {
    let Some(ui) = weak.upgrade() else { return };
    if ui.get_scanning() {
        return;
    }
    ui.set_scanning(true);
    ui.set_status("Preparando a varredura...".into());
    app.cancel.store(false, Ordering::Relaxed);

    let cancel = app.cancel.clone();
    let settings = app.settings.borrow().clone();
    let (auto_thumbs, threads) = (settings.auto_thumbs, thumb_threads());
    let main_db = app.db_path.clone();
    let catalog = db_path.clone();
    let vault_generation = app.generations.vault.clone();
    let progress_weak = weak.clone();
    let batch_cancel = app.cancel.clone();
    let index = app.index.clone();
    background(
        weak,
        move || {
            run_scan(&progress_weak, &db_path, source_id, &settings, &cancel)
                .unwrap_or_else(|error| format!("Erro na varredura: {error}"))
        },
        move |ui, message| {
            ui.set_scanning(false);
            ui.set_status(message.clone().into());
            if is_vault {
                run_vault_search(&ui.as_weak(), catalog, vault_generation);
            } else {
                let _ = refresh_sources(ui, &main_db);
                refresh_stats(&ui.as_weak(), &main_db);
                index.rebuild(main_db.clone()); // a busca em memória passa a enxergar o que foi varrido
                // Os arquivos novos já ganham miniatura (só os desta fonte).
                if auto_thumbs && message.starts_with("Pronto") {
                    start_thumb_batch(&ui.as_weak(), main_db, vec![source_id], batch_cancel, threads, message);
                }
            }
        },
    );
}

/// Threads para gerar miniaturas: usa os núcleos do PC (de 4 a 12). Gerar miniatura é leve e
/// espera muito o disco, então mais threads deixam bem mais rápido; a varredura continua com poucas.
fn thumb_threads() -> usize {
    std::thread::available_parallelism()
        .map(|cores| cores.get())
        .unwrap_or(4)
        .clamp(4, 12)
}

/// Gera em segundo plano as miniaturas que faltam (fotos, vídeos, instaladores e programas),
/// usando o Windows para extrair. `source_ids` vazio = todas as fontes conectadas.
/// `prefix` é a mensagem anterior (ex.: do fim de uma varredura), repetida no final.
fn start_thumb_batch(
    weak: &Weak<MainWindow>,
    db_path: PathBuf,
    source_ids: Vec<i64>,
    cancel: Arc<AtomicBool>,
    threads: usize,
    prefix: String,
) {
    let Some(ui) = weak.upgrade() else { return };
    if ui.get_scanning() {
        return;
    }
    ui.set_scanning(true);
    ui.set_status("Gerando miniaturas...".into());
    cancel.store(false, Ordering::Relaxed);

    let progress_weak = weak.clone();
    let stats_db = db_path.clone();
    background(
        weak,
        move || {
            let report_weak = progress_weak.clone();
            let progress = move |done: usize, total: usize| {
                set_status_later(
                    &report_weak,
                    format!(
                        "Gerando miniaturas... {} de {}",
                        thousands(done as i64),
                        thousands(total as i64)
                    ),
                );
            };
            thumbs::generate_missing(
                &db_path,
                &source_ids,
                thumbs::BATCH_KINDS,
                Mode::WindowsFull,
                threads,
                &cancel,
                &progress,
            )
        },
        move |ui: &MainWindow, report: Result<thumbs::BatchReport>| {
            forget_absent_thumbs(); // as miniaturas novas passam a valer nas telas
            ui.set_scanning(false);
            let summary = match report {
                Ok(report) => format!(
                    "Miniaturas: {} novas, {} já existiam, {} sem miniatura.",
                    thousands(report.generated as i64),
                    thousands(report.already_stored as i64),
                    thousands(report.without_thumbnail as i64)
                ),
                Err(error) => format!("Erro ao gerar miniaturas: {error}"),
            };
            ui.set_status(if prefix.is_empty() { summary } else { format!("{prefix} {summary}") }.into());
            refresh_stats(&ui.as_weak(), &stats_db);
        },
    );
}

fn set_status_later(weak: &Weak<MainWindow>, text: String) {
    let _ = weak.upgrade_in_event_loop(move |ui| ui.set_status(text.into()));
}

fn done_message(progress: &Progress) -> String {
    format!(
        "Pronto: {} arquivos catalogados ({}).",
        thousands(progress.files as i64),
        human_size(progress.bytes)
    )
}

/// Pede a varredura rápida ao serviço do Windows (que já é administrador) e acompanha o andamento.
fn run_via_service(weak: &Weak<MainWindow>, db_path: &Path, source_id: i64, cancel: &AtomicBool) -> Result<String> {
    let db = Db::open(db_path)?;
    let job_id = service::request_scan(&db, source_id)?;
    let mut silent_since: Option<Instant> = None;
    loop {
        std::thread::sleep(Duration::from_millis(500));
        if cancel.load(Ordering::Relaxed) {
            db.set_job(job_id, "cancel", "")?;
        }
        let Some(job) = db.job(job_id)? else {
            anyhow::bail!("a tarefa do serviço sumiu");
        };
        match job.state.as_str() {
            "done" => return Ok(job.progress),
            "failed" => anyhow::bail!("{}", job.progress),
            "cancelled" => return Ok("Varredura cancelada.".to_string()),
            _ => set_status_later(weak, format!("Serviço do Windows: {}", job.progress)),
        }
        // Se o serviço parar de dar sinal de vida no meio, não fica esperando para sempre.
        if service::is_alive(&db) {
            silent_since = None;
        } else if silent_since.get_or_insert_with(Instant::now).elapsed() > Duration::from_secs(20) {
            anyhow::bail!("o serviço do Acervo parou de responder");
        }
    }
}

/// Tenta a leitura rápida da MFT; se precisar de administrador, usa o serviço ou pede ao Windows.
/// Se o usuário recusar, usa a varredura normal por pastas.
fn run_scan(
    weak: &Weak<MainWindow>,
    db_path: &Path,
    source_id: i64,
    settings: &Settings,
    cancel: &AtomicBool,
) -> Result<String> {
    let mut db = Db::open(db_path)?;
    let root = db
        .sources()?
        .into_iter()
        .find(|source| source.id == source_id)
        .map(|source| PathBuf::from(source.path))
        .ok_or_else(|| anyhow::anyhow!("fonte não encontrada"))?;
    let opts = ScanOptions {
        threads: settings.threads,
        skip_hidden: settings.skip_hidden,
    };
    let mut report = |progress: &Progress| {
        set_status_later(
            weak,
            format!(
                "Varrendo... {} arquivos, {}",
                thousands(progress.files as i64),
                human_size(progress.bytes)
            ),
        );
    };

    match scan::scan_source(&mut db, source_id, &root, &opts, cancel, &mut report)? {
        Outcome::Done(progress) => Ok(done_message(&progress)),
        Outcome::Cancelled => Ok("Varredura cancelada.".to_string()),
        Outcome::NeedsAdmin => {
            drop(db);
            let mode = settings.admin_mode;
            if mode == AdminMode::Service {
                // Serviço do Windows instalado e ativo: ele faz a varredura rápida, sem perguntar nada.
                let alive = Db::open(db_path).map(|db| service::is_alive(&db)).unwrap_or(false);
                if alive {
                    return run_via_service(weak, db_path, source_id, cancel);
                }
                set_status_later(weak, "O serviço do Acervo não está ativo; pedindo permissão ao Windows...".to_string());
            }
            if mode != AdminMode::Never {
                set_status_later(
                    weak,
                    "Varredura rápida: aceite o pedido de administrador do Windows...".to_string(),
                );
                if fast::run_elevated_scan(db_path, source_id).unwrap_or(false) {
                    return Ok("Pronto (varredura rápida como administrador).".to_string());
                }
            }
            set_status_later(weak, "Sem administrador: usando a varredura normal...".to_string());
            let mut db = Db::open(db_path)?;
            match scan::scan_walk(&mut db, source_id, &root, &opts, cancel, &mut report)? {
                Outcome::Done(progress) => Ok(done_message(&progress)),
                _ => Ok("Varredura cancelada.".to_string()),
            }
        }
    }
}

// ---------------------------------------------------------------- Dados das telas

/// Roda `work` em outra thread e entrega o resultado para a janela na thread da interface.
fn background<T: Send + 'static>(
    weak: &Weak<MainWindow>,
    work: impl FnOnce() -> T + Send + 'static,
    apply: impl FnOnce(&MainWindow, T) + Send + 'static,
) {
    let weak = weak.clone();
    std::thread::spawn(move || {
        let value = work();
        let _ = weak.upgrade_in_event_loop(move |ui| apply(&ui, value));
    });
}

fn refresh_sources(ui: &MainWindow, db_path: &Path) -> Result<()> {
    let db = Db::open(db_path)?;
    let sources = db.sources()?;
    let mut names: Vec<slint::SharedString> = vec![ALL_SOURCES.into()];
    names.extend(sources.iter().map(|source| slint::SharedString::from(source.name.as_str())));
    ui.set_source_names(ModelRc::new(VecModel::from(names)));

    let items: Vec<SourceItem> = sources
        .into_iter()
        .map(|source| SourceItem {
            id: source.id as i32,
            online: Path::new(&source.path).exists(),
            info: describe_last_scan(source.last_scan).into(),
            name: source.name.into(),
            thumbs: "miniaturas: calculando...".into(),
        })
        .collect();
    ui.set_sources(ModelRc::new(VecModel::from(items)));
    Ok(())
}

/// Unidades A:..Z: que existem agora, para "Adicionar HD".
fn refresh_drives(ui: &MainWindow) {
    let drives: Vec<slint::SharedString> = ('A'..='Z')
        .map(|letter| format!("{letter}:\\"))
        .filter(|root| Path::new(root).exists())
        .map(Into::into)
        .collect();
    ui.set_drives(ModelRc::new(VecModel::from(drives)));
}

struct StatsData {
    files: i64,
    bytes: i64,
    sources: usize,
    offline: usize,
    duplicate_groups: i64,
    kinds: Vec<(String, i64, i64)>,
    thumbs: (i64, i64),
    /// (id da fonte, quantidade, bytes) de miniaturas por fonte.
    thumbs_by_source: Vec<(i64, i64, i64)>,
}

fn refresh_stats(weak: &Weak<MainWindow>, db_path: &Path) {
    let db_path = db_path.to_path_buf();
    background(
        weak,
        move || -> Result<StatsData> {
            let db = Db::open(&db_path)?;
            let totals = db.totals()?;
            let sources = db.sources()?;
            Ok(StatsData {
                files: totals.files,
                bytes: totals.bytes,
                offline: sources.iter().filter(|s| !Path::new(&s.path).exists()).count(),
                sources: sources.len(),
                duplicate_groups: db.duplicate_group_count()?,
                kinds: db.stats_by_kind()?,
                thumbs: db.thumb_stats()?,
                thumbs_by_source: db.thumb_usage_by_source()?,
            })
        },
        |ui, data| {
            let Ok(data) = data else { return };
            // Espaço das miniaturas em cada fonte (as listas já estão na tela, só atualiza o texto).
            let sources = ui.get_sources();
            for index in 0..sources.row_count() {
                let Some(mut row) = sources.row_data(index) else { continue };
                let text = match data.thumbs_by_source.iter().find(|usage| usage.0 == row.id as i64) {
                    Some((_, count, bytes)) => format!(
                        "miniaturas: {} ({} itens)",
                        human_size(*bytes as u64),
                        thousands(*count)
                    ),
                    None => "miniaturas: nenhuma".to_string(),
                };
                row.thumbs = text.into();
                sources.set_row_data(index, row);
            }
            ui.set_stat_files(thousands(data.files).into());
            ui.set_stat_bytes(human_size(data.bytes as u64).into());
            ui.set_stat_offline(format!("{} de {}", data.offline, data.sources).into());
            ui.set_stat_dupes(thousands(data.duplicate_groups).into());
            ui.set_stat_thumbs(
                format!(
                    "Miniaturas guardadas no banco: {} ({})",
                    thousands(data.thumbs.0),
                    human_size(data.thumbs.1 as u64)
                )
                .into(),
            );
            let total = data.bytes.max(1) as f32;
            let items: Vec<TypeStat> = data
                .kinds
                .into_iter()
                .map(|(kind, count, bytes)| {
                    let (label, color) = kind_style(&kind);
                    TypeStat {
                        label: label.into(),
                        size: human_size(bytes as u64).into(),
                        count: format!("{} itens", thousands(count)).into(),
                        fraction: bytes as f32 / total,
                        color,
                    }
                })
                .collect();
            ui.set_type_stats(ModelRc::new(VecModel::from(items)));
        },
    );
}

const ADMIN_PROMPT: &str = "Perguntar a cada vez (permissão do Windows)";
const ADMIN_SERVICE: &str = "Usar o serviço do Windows (sem perguntar)";
const ADMIN_NEVER: &str = "Nunca usar administrador";

fn admin_label(mode: AdminMode) -> &'static str {
    match mode {
        AdminMode::Prompt => ADMIN_PROMPT,
        AdminMode::Service => ADMIN_SERVICE,
        AdminMode::Never => ADMIN_NEVER,
    }
}

fn admin_mode_from_label(label: &str) -> AdminMode {
    match label {
        ADMIN_SERVICE => AdminMode::Service,
        ADMIN_NEVER => AdminMode::Never,
        _ => AdminMode::Prompt,
    }
}

/// Mostra se o serviço está instalado e ativo (consulta em segundo plano).
fn refresh_service_status(weak: &Weak<MainWindow>, db_path: &Path) {
    let db_path = db_path.to_path_buf();
    background(
        weak,
        move || {
            let alive = Db::open(&db_path).map(|db| service::is_alive(&db)).unwrap_or(false);
            match (service::installed_state(), alive) {
                (service::Installed::No, _) => "Serviço do Windows: não instalado".to_string(),
                (service::Installed::Running, true) => "Serviço do Windows: instalado e ativo".to_string(),
                (service::Installed::Running, false) => "Serviço do Windows: iniciando...".to_string(),
                (service::Installed::Stopped, _) => "Serviço do Windows: instalado, mas parado".to_string(),
            }
        },
        |ui, text| ui.set_service_status(text.into()),
    );
}

/// Instala ou remove o serviço. O Windows pede permissão de administrador uma vez, na hora.
fn run_service_command(weak: &Weak<MainWindow>, app: &Rc<App>, install: bool) {
    let Some(ui) = weak.upgrade() else { return };
    let (title, question) = if install {
        (
            "Instalar o serviço do Acervo",
            "Instalar o serviço do Acervo no Windows?\n\nO Windows vai pedir permissão de administrador UMA vez. Depois disso o Acervo faz a varredura rápida dos HDs NTFS e acompanha as mudanças (arquivos criados/apagados) sem perguntar de novo, inclusive com o app fechado.\n\nO serviço só lê nomes e tamanhos de arquivos e grava no seu catálogo. Você pode remover quando quiser.",
        )
    } else {
        (
            "Remover o serviço do Acervo",
            "Remover o serviço do Acervo do Windows?\n\nO Windows vai pedir permissão de administrador. O catálogo continua como está; a varredura rápida volta a pedir permissão a cada uso (ou usa o modo normal).",
        )
    };
    if !confirm(title, question.to_string()) {
        return;
    }
    let exe = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|dir| dir.join("acervo-service.exe")))
        .filter(|path| path.is_file());
    let Some(exe) = exe else {
        ui.set_service_status("Não achei o acervo-service.exe ao lado do acervo.exe.".into());
        return;
    };
    ui.set_service_status("Aguardando a permissão do Windows...".into());
    let db_path = app.db_path.clone();
    let status_db = db_path.clone();
    background(
        weak,
        move || {
            let args = if install {
                vec!["install".to_string(), "--db".to_string(), db_path.to_string_lossy().into_owned()]
            } else {
                vec!["uninstall".to_string()]
            };
            fast::run_elevated(&exe, &args).unwrap_or(false)
        },
        move |ui, ok| {
            if !ok {
                ui.set_service_status("Não foi concluído (permissão recusada ou erro).".into());
                return;
            }
            // O serviço leva uns segundos para começar a dar sinal de vida.
            refresh_service_status(&ui.as_weak(), &status_db);
        },
    );
}

fn show_settings(ui: &MainWindow, settings: &Settings) {
    ui.set_set_admin_mode(admin_label(settings.admin_mode).into());
    ui.set_set_threads(settings.threads as f32);
    ui.set_set_skip_hidden(settings.skip_hidden);
    ui.set_set_auto_thumbs(settings.auto_thumbs);
    ui.set_set_autolock(settings.autolock_minutes as f32);
    ui.set_set_letter(settings.vault_letter.to_string().into());
    ui.set_vault_configured(settings.vault_container.is_some());
    ui.set_vault_container(
        settings
            .vault_container
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default()
            .into(),
    );
}

// ---------------------------------------------------------------- Jogos

struct GameData {
    name: String,
    info: String,
    library: String,
    size: u64,
    kind: &'static str,
    job: Option<(String, PathBuf, Option<(String, PathBuf)>)>,
}

fn initials(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric())
        .take(2)
        .collect::<String>()
        .to_uppercase()
}

/// tab 0 = Steam, 1 = instaladores, 2 = repacks. Aplica a busca e a ordem da tela.
fn load_games(weak: &Weak<MainWindow>, app: &Rc<App>, tab: i32) {
    let Some(ui) = weak.upgrade() else { return };
    let query = ui.get_games_query().trim().to_lowercase();
    let by_size = ui.get_games_sort() == "Maiores";
    let db_path = app.db_path.clone();
    let generation = app.generations.games.clone();
    let my_generation = generation.fetch_add(1, Ordering::SeqCst) + 1;
    let latest = generation.clone();
    let thumbs_db = db_path.clone();

    background(
        weak,
        move || -> Result<Vec<GameData>> {
            let mut games: Vec<GameData> = match tab {
                0 => steam::installed_games()
                    .into_iter()
                    .map(|game| GameData {
                        info: format!("AppID {} · {}", game.app_id, human_size(game.size_bytes)),
                        library: game.library.to_string_lossy().into_owned(),
                        size: game.size_bytes,
                        kind: "steam",
                        job: Some((
                            String::new(),
                            PathBuf::new(),
                            steam::cover_path(game.app_id)
                                .map(|cover| (format!("steam:{}", game.app_id), cover)),
                        )),
                        name: game.name,
                    })
                    .collect(),
                1 => Db::open(&db_path)?
                    .search_query(&SearchQuery {
                        kind: Some("installer".to_string()),
                        limit: 1000,
                        ..SearchQuery::default()
                    })?
                    .into_iter()
                    .map(|row| GameData {
                        info: format!("{} · {}", human_size(row.size as u64), row.source_name),
                        library: Path::new(&row.path)
                            .parent()
                            .map(|dir| dir.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                        size: row.size as u64,
                        kind: "installer",
                        job: Some(("installer".to_string(), PathBuf::from(&row.path), None)),
                        name: row.name,
                    })
                    .collect(),
                _ => Db::open(&db_path)?
                    .repack_folders()?
                    .into_iter()
                    .map(|folder| GameData {
                        name: Path::new(&folder.dir)
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_else(|| folder.dir.clone()),
                        info: format!(
                            "{} arquivos · {} · {}",
                            folder.files,
                            human_size(folder.bytes as u64),
                            folder.source_name
                        ),
                        size: folder.bytes as u64,
                        kind: "repack",
                        job: None,
                        library: folder.dir,
                    })
                    .collect(),
            };
            if !query.is_empty() {
                games.retain(|game| game.name.to_lowercase().contains(&query));
            }
            if by_size {
                games.sort_by(|a, b| b.size.cmp(&a.size));
            } else {
                games.sort_by_cached_key(|game| game.name.to_lowercase());
            }
            Ok(games)
        },
        move |ui: &MainWindow, games: Result<Vec<GameData>>| {
            if latest.load(Ordering::SeqCst) != my_generation {
                return;
            }
            let games = games.unwrap_or_default();
            let mut jobs = Vec::new();
            let items: Vec<GameItem> = games
                .into_iter()
                .enumerate()
                .map(|(index, game)| {
                    if let Some((kind, path, cover)) = game.job {
                        jobs.push(ThumbJob { index, kind, path, cover });
                    }
                    GameItem {
                        initials: initials(&game.name).into(),
                        color: kind_style(game.kind).1,
                        name: game.name.into(),
                        info: game.info.into(),
                        library: game.library.into(),
                        thumb: slint::Image::default(),
                        has_thumb: false,
                    }
                })
                .collect();
            ui.set_games(ModelRc::new(VecModel::from(items)));
            spawn_thumbs(&ui.as_weak(), Target::Games, latest, my_generation, thumbs_db, Mode::WindowsCache, jobs);
        },
    );
}

// ---------------------------------------------------------------- Duplicados

fn load_duplicates(weak: &Weak<MainWindow>, db_path: PathBuf, generation: Arc<AtomicU64>) {
    let Some(ui) = weak.upgrade() else { return };
    let text = ui.get_dupe_query().trim().to_string();
    let kind = kind_filter(ui.get_dupe_kind().as_str());
    let min_size = min_size_from_label(ui.get_dupe_min().as_str());
    let my_generation = generation.fetch_add(1, Ordering::SeqCst) + 1;
    let latest = generation.clone();
    let thumbs_db = db_path.clone();

    background(
        weak,
        move || Db::open(&db_path).and_then(|db| db.duplicates_filtered(&text, kind.as_deref(), min_size, 200)),
        move |ui, groups| {
            if latest.load(Ordering::SeqCst) != my_generation {
                return;
            }
            let groups = groups.unwrap_or_default();
            let recoverable: i64 = groups.iter().map(|g| g.size * (g.copies - 1)).sum();
            ui.set_dupe_summary(
                format!(
                    "{} grupos (os maiores primeiro) · até {} para liberar",
                    groups.len(),
                    human_size(recoverable as u64)
                )
                .into(),
            );
            let jobs: Vec<ThumbJob> = groups
                .iter()
                .enumerate()
                .filter(|(_, group)| thumbs::can_have_thumbnail(Path::new(&group.name), &group.kind))
                .map(|(index, group)| ThumbJob {
                    index,
                    kind: group.kind.clone(),
                    path: PathBuf::from(&group.first_path),
                    cover: None,
                })
                .collect();
            let items: Vec<DupItem> = groups
                .into_iter()
                .map(|group| DupItem {
                    info: format!(
                        "{} cópias · {} cada · {} a mais",
                        group.copies,
                        human_size(group.size as u64),
                        human_size((group.size * (group.copies - 1)) as u64)
                    )
                    .into(),
                    color: kind_style(&group.kind).1,
                    ext: ext_label(&group.name).into(),
                    size_bytes: group.size.to_string().into(),
                    name: group.name.into(),
                    thumb: slint::Image::default(),
                    has_thumb: false,
                })
                .collect();
            ui.set_dupe_selected(-1);
            ui.set_dupe_copies(ModelRc::new(VecModel::default()));
            ui.set_dupes(ModelRc::new(VecModel::from(items)));
            spawn_thumbs(&ui.as_weak(), Target::Dupes, latest, my_generation, thumbs_db, Mode::WindowsCache, jobs);
        },
    );
}

fn age_text(mtime: Option<i64>) -> String {
    let Some(timestamp) = mtime else {
        return "data desconhecida".to_string();
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(timestamp);
    format!("modificado há {} dias", (now - timestamp).max(0) / 86_400)
}

fn copy_rows(ui: &MainWindow) -> Vec<DupCopy> {
    let model = ui.get_dupe_copies();
    (0..model.row_count()).filter_map(|i| model.row_data(i)).collect()
}

/// Seleciona um grupo e carrega as cópias dele.
fn show_duplicate_copies(weak: &Weak<MainWindow>, db_path: &Path, index: i32) {
    let Some(ui) = weak.upgrade() else { return };
    let Some(group) = ui.get_dupes().row_data(index as usize) else { return };
    ui.set_dupe_selected(index);
    ui.set_dupe_copies(ModelRc::new(VecModel::default()));
    let size: i64 = group.size_bytes.parse().unwrap_or(0);
    let name = group.name.to_string();
    let db_path = db_path.to_path_buf();
    background(
        weak,
        move || -> Result<Vec<DupCopy>> {
            let db = Db::open(&db_path)?;
            let mut drive_online: HashMap<String, bool> = HashMap::new();
            let mut gone: Vec<i64> = Vec::new();
            let mut copies = Vec::new();
            for copy in db.duplicate_copies(&name, size)? {
                let drive = copy
                    .path
                    .split_once('\\')
                    .map(|(drive, _)| format!("{drive}\\"))
                    .unwrap_or_default();
                let drive_is_online = *drive_online
                    .entry(drive.clone())
                    .or_insert_with(|| Path::new(&drive).exists());
                let exists = drive_is_online && Path::new(&copy.path).exists();
                if drive_is_online && !exists {
                    gone.push(copy.id); // apagada fora do app: sai do catálogo
                    continue;
                }
                copies.push(DupCopy {
                    id: copy.id as i32,
                    info: format!("{} · {}", copy.source_name, age_text(copy.mtime)).into(),
                    path: copy.path.into(),
                    online: exists,
                    checked: false,
                });
            }
            if !gone.is_empty() {
                db.delete_files(&gone)?;
            }
            Ok(copies)
        },
        move |ui, copies| {
            // Só mostra se o usuário não trocou de grupo no meio da leitura.
            if ui.get_dupe_selected() == index {
                ui.set_dupe_copies(ModelRc::new(VecModel::from(copies.unwrap_or_default())));
            }
        },
    );
}

fn wire_duplicates(ui: &MainWindow, app: &Rc<App>) {
    let weak = ui.as_weak();

    ui.on_select_dupe({
        let (weak, app) = (weak.clone(), app.clone());
        move |index| {
            app.touch();
            show_duplicate_copies(&weak, &app.db_path, index);
        }
    });

    ui.on_toggle_copy({
        let weak = weak.clone();
        move |index, checked| {
            let Some(ui) = weak.upgrade() else { return };
            let model = ui.get_dupe_copies();
            if let Some(mut row) = model.row_data(index as usize) {
                row.checked = checked && row.online;
                model.set_row_data(index as usize, row);
            }
        }
    });

    ui.on_mark_all_but_one({
        let weak = weak.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let model = ui.get_dupe_copies();
            // Fica a primeira cópia conectada (a mais antiga); as outras conectadas são marcadas.
            let Some(keep) = copy_rows(&ui).iter().position(|row| row.online) else {
                ui.set_status("Nenhuma cópia está conectada: plugue um dos HDs.".into());
                return;
            };
            for index in 0..model.row_count() {
                if let Some(mut row) = model.row_data(index) {
                    row.checked = index != keep && row.online;
                    model.set_row_data(index, row);
                }
            }
        }
    });

    ui.on_clear_marks({
        let weak = weak.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let model = ui.get_dupe_copies();
            for index in 0..model.row_count() {
                if let Some(mut row) = model.row_data(index) {
                    row.checked = false;
                    model.set_row_data(index, row);
                }
            }
        }
    });

    ui.on_delete_marked({
        let (weak, app) = (weak.clone(), app.clone());
        move || {
            app.touch();
            let Some(ui) = weak.upgrade() else { return };
            let Some(group) = ui.get_dupes().row_data(ui.get_dupe_selected().max(0) as usize) else {
                return;
            };
            let size: i64 = group.size_bytes.parse().unwrap_or(0);
            let rows = copy_rows(&ui);
            let Some(keep) = rows.iter().find(|row| !row.checked && row.online) else {
                ui.set_status("Deixe pelo menos uma cópia sem marca, com o HD conectado.".into());
                return;
            };
            let keep_path = PathBuf::from(keep.path.as_str());
            let targets: Vec<(i64, PathBuf)> = rows
                .iter()
                .filter(|row| row.checked && row.online)
                .map(|row| (row.id as i64, PathBuf::from(row.path.as_str())))
                .collect();
            if targets.is_empty() {
                ui.set_status("Marque pelo menos uma cópia para apagar.".into());
                return;
            }

            let list: Vec<String> = targets.iter().take(6).map(|(_, path)| format!("• {}", path.display())).collect();
            let more = targets.len().saturating_sub(6);
            if !confirm(
                "Apagar duplicados",
                format!(
                    "Apagar {} cópia(s) de \"{}\" ({} no total)?\n\n{}{}\n\nFica: {}\n\nVão para a Lixeira do Windows. Em HDs externos sem Lixeira, ou para arquivos maiores que ela, o Windows pode apagar de vez.",
                    targets.len(),
                    group.name,
                    human_size(size as u64 * targets.len() as u64),
                    list.join("\n"),
                    if more > 0 { format!("\n… e mais {more}") } else { String::new() },
                    keep_path.display()
                ),
            ) {
                return;
            }

            ui.set_status("Conferindo e apagando...".into());
            let db_path = app.db_path.clone();
            background(
                &weak,
                move || -> Result<dedupe::Report> {
                    let db = Db::open(&db_path)?;
                    dedupe::remove_duplicates(&db, &keep_path, size as u64, &targets, &|path| {
                        trash::delete(path).map_err(|error| anyhow::anyhow!(error.to_string()))
                    })
                },
                {
                    let db_path = app.db_path.clone();
                    let generation = app.generations.dupes.clone();
                    move |ui: &MainWindow, report: Result<dedupe::Report>| {
                        let message = match report {
                            Ok(report) => {
                                let mut text = format!(
                                    "{} cópia(s) apagada(s), {} liberados.",
                                    report.deleted,
                                    human_size(report.freed_bytes)
                                );
                                if !report.skipped.is_empty() {
                                    text.push_str(&format!(" Não apagadas: {}", report.skipped.join("; ")));
                                }
                                text
                            }
                            Err(error) => format!("Erro ao apagar: {error}"),
                        };
                        ui.set_status(message.into());
                        refresh_stats(&ui.as_weak(), &db_path);
                        load_duplicates(&ui.as_weak(), db_path, generation);
                    }
                },
            );
        }
    });
}

// ---------------------------------------------------------------- Formatação

/// Nome em português e cor de cada tipo de arquivo.
fn kind_style(kind: &str) -> (&'static str, Color) {
    match kind {
        "video" => ("Vídeos", Color::from_rgb_u8(0xB3, 0x88, 0xFF)),
        "photo" => ("Fotos", Color::from_rgb_u8(0xFF, 0x8A, 0xB5)),
        "music" => ("Músicas", Color::from_rgb_u8(0xFF, 0xD8, 0x66)),
        "document" => ("Documentos", Color::from_rgb_u8(0xA5, 0xD6, 0xA7)),
        "program" => ("Programas", Color::from_rgb_u8(0xFF, 0xAB, 0x70)),
        "installer" => ("Instaladores", Color::from_rgb_u8(0xF4, 0xD3, 0x5E)),
        "steam" => ("Steam", Color::from_rgb_u8(0x1B, 0x9D, 0xE2)),
        "repack" => ("Repacks", Color::from_rgb_u8(0x60, 0xCD, 0xFF)),
        "archive" => ("Compactados", Color::from_rgb_u8(0x9A, 0xA0, 0xA6)),
        _ => ("Outros", Color::from_rgb_u8(0x9A, 0x9A, 0x9A)),
    }
}

/// Extensão em maiúsculas (até 4 letras) para o quadrado sem miniatura.
fn ext_label(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, ext)| ext.chars().take(4).collect::<String>().to_uppercase())
        .unwrap_or_default()
}

/// dd/mm/aaaa a partir de segundos Unix (algoritmo de dias civis, sem biblioteca de datas).
fn date_text(mtime: Option<i64>) -> String {
    let Some(timestamp) = mtime else {
        return "sem data".to_string();
    };
    let z = timestamp.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{day:02}/{month:02}/{year}")
}

fn thousands(value: i64) -> String {
    let digits = value.abs().to_string();
    let mut grouped = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            grouped.push('.');
        }
        grouped.push(digit);
    }
    if value < 0 {
        format!("-{grouped}")
    } else {
        grouped
    }
}

fn describe_last_scan(last_scan: Option<i64>) -> String {
    let Some(timestamp) = last_scan else {
        return "nunca varrido".to_string();
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(timestamp);
    match (now - timestamp).max(0) / 86_400 {
        0 => "varrido hoje".to_string(),
        1 => "varrido ontem".to_string(),
        days => format!("varrido há {days} dias"),
    }
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_sizes() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(5 * 1024 * 1024 * 1024), "5.0 GB");
    }

    #[test]
    fn groups_thousands() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(48213), "48.213");
        assert_eq!(thousands(1_234_567), "1.234.567");
    }

    #[test]
    fn formats_dates() {
        assert_eq!(date_text(Some(0)), "01/01/1970");
        assert_eq!(date_text(Some(1_709_164_800)), "29/02/2024");
        assert_eq!(date_text(None), "sem data");
    }

    #[test]
    fn labels_for_filters() {
        assert_eq!(sort_from_label("Maiores"), Sort::Largest);
        assert_eq!(sort_from_label("qualquer outra"), Sort::Name);
        assert_eq!(min_size_from_label("1 GB+"), 1024 * MB);
        assert_eq!(min_size_from_label("Qualquer tamanho"), 0);
        assert_eq!(ext_label("filme.mkv"), "MKV");
        assert_eq!(ext_label("semextensao"), "");
    }

    #[test]
    fn describes_never_scanned_source() {
        assert_eq!(describe_last_scan(None), "nunca varrido");
    }
}
