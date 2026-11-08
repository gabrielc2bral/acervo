//! Mantém o catálogo atualizado sozinho: observa as pastas das fontes conectadas e, quando você
//! apaga, move, cria ou renomeia algo (no Explorer ou em qualquer programa), ajusta só aquele
//! arquivo no SQLite. Não precisa de administrador e não varre nada de novo.
//!
//! O que mudou com o Acervo fechado não passa por aqui: a Biblioteca confere os itens que mostra
//! (ver `Db::delete_files` no `ui.rs`) e uma nova varredura acerta o resto.

use crate::db::Db;
use crate::kind::{classify, has_hidden_or_system_attr, is_system_dir};
use crate::scan::unix_seconds;
use crate::settings::Settings;
use anyhow::Result;
use notify::{event::ModifyKind, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

const SYNC_SOURCES_EVERY: Duration = Duration::from_secs(15);
/// Espera o barulho acabar (ex.: copiando uma pasta grande) antes de gravar.
const QUIET_BEFORE_APPLY: Duration = Duration::from_millis(700);
const MAX_PENDING: usize = 3000;

#[cfg(windows)]
pub(crate) fn attributes(meta: &std::fs::Metadata) -> u32 {
    use std::os::windows::fs::MetadataExt;
    meta.file_attributes()
}

#[cfg(not(windows))]
pub(crate) fn attributes(_meta: &std::fs::Metadata) -> u32 {
    0
}

/// Começa a observar em uma thread própria. `on_change(n)` avisa quantos itens mudaram no catálogo.
/// Devolve a chave para parar (`stop.store(true)`).
pub fn spawn(db_path: PathBuf, on_change: impl Fn(usize) + Send + 'static) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = stop.clone();
    std::thread::spawn(move || run(&db_path, &stop_flag, &on_change));
    stop
}

fn run(db_path: &Path, stop: &AtomicBool, on_change: &dyn Fn(usize)) {
    let (sender, receiver) = mpsc::channel::<notify::Result<Event>>();
    let Ok(mut watcher) = RecommendedWatcher::new(sender, notify::Config::default()) else {
        return;
    };
    let mut watched: HashMap<PathBuf, i64> = HashMap::new();
    // caminho -> mudou a estrutura (criou/removeu/renomeou)? Só isso vale para pastas.
    let mut pending: HashMap<PathBuf, bool> = HashMap::new();
    let mut last_sync: Option<Instant> = None;
    let mut last_event = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        if last_sync.is_none_or(|at| at.elapsed() > SYNC_SOURCES_EVERY) {
            sync_sources(db_path, &mut watcher, &mut watched);
            last_sync = Some(Instant::now());
        }

        match receiver.recv_timeout(Duration::from_millis(300)) {
            Ok(Ok(event)) => {
                let structural = match event.kind {
                    EventKind::Access(_) => continue,
                    EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_)) => true,
                    _ => false,
                };
                for path in event.paths {
                    let entry = pending.entry(path).or_insert(false);
                    *entry |= structural;
                }
                last_event = Instant::now();
            }
            Ok(Err(_)) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        if !pending.is_empty() && (last_event.elapsed() >= QUIET_BEFORE_APPLY || pending.len() >= MAX_PENDING) {
            let batch = std::mem::take(&mut pending);
            if let Ok(changed) = apply(db_path, &watched, &batch) {
                if changed > 0 {
                    on_change(changed);
                }
            }
        }
    }
}

/// Passa a observar fontes novas conectadas e para de observar as que saíram ou foram removidas.
fn sync_sources(db_path: &Path, watcher: &mut RecommendedWatcher, watched: &mut HashMap<PathBuf, i64>) {
    let Ok(db) = Db::open(db_path) else { return };
    let Ok(sources) = db.sources() else { return };
    let wanted: HashMap<PathBuf, i64> = sources
        .into_iter()
        .map(|source| (PathBuf::from(source.path), source.id))
        .filter(|(path, _)| path.exists())
        .collect();

    let gone: Vec<PathBuf> = watched.keys().filter(|path| !wanted.contains_key(*path)).cloned().collect();
    for path in gone {
        let _ = watcher.unwatch(&path);
        watched.remove(&path);
    }
    for (path, id) in wanted {
        if !watched.contains_key(&path) && watcher.watch(&path, RecursiveMode::Recursive).is_ok() {
            watched.insert(path, id);
        }
    }
}

/// A fonte que contém `path` (a de caminho mais longo, para pastas dentro de HDs).
fn source_of<'a>(watched: &'a HashMap<PathBuf, i64>, path: &Path) -> Option<(&'a PathBuf, i64)> {
    let lower = path.to_string_lossy().to_lowercase();
    watched
        .iter()
        .filter(|(root, _)| lower.starts_with(&root.to_string_lossy().to_lowercase()))
        .max_by_key(|(root, _)| root.as_os_str().len())
        .map(|(root, id)| (root, *id))
}

/// Alguma pasta do sistema (Windows, AppData...) no caminho, contando só DEPOIS da raiz da fonte.
/// (Se a própria fonte estiver dentro de uma pasta dessas, ela continua valendo, como na varredura.)
pub(crate) fn in_system_dir(root: &Path, path: &Path) -> bool {
    let root_len = root.as_os_str().len();
    let text = path.as_os_str().to_string_lossy();
    let relative = text.get(root_len..).unwrap_or("");
    relative
        .split(['\\', '/'])
        .any(|part| !part.is_empty() && is_system_dir(part))
}

/// Grava no catálogo o que mudou. Devolve quantos caminhos alteraram o banco.
fn apply(db_path: &Path, watched: &HashMap<PathBuf, i64>, batch: &HashMap<PathBuf, bool>) -> Result<usize> {
    let db = Db::open(db_path)?;
    let skip_hidden = Settings::load(&db)?.skip_hidden;
    let tx = db.conn.unchecked_transaction()?;
    let mut changed = 0;

    for (path, structural) in batch {
        let Some((root, source_id)) = source_of(watched, path) else { continue };
        if in_system_dir(root, path) {
            continue;
        }
        let text = path.to_string_lossy();
        match std::fs::metadata(path) {
            Ok(meta) if meta.is_file() => {
                if skip_hidden && has_hidden_or_system_attr(attributes(&meta)) {
                    continue;
                }
                let (dir, name) = Db::split_path(&text);
                let kind = if dir.to_lowercase().contains("\\steamapps\\") {
                    "steam"
                } else {
                    classify(&name).as_str()
                };
                db.upsert_file(source_id, &dir, &name, meta.len() as i64, unix_seconds(meta.modified()), kind)?;
                changed += 1;
            }
            Ok(meta) if meta.is_dir() => {
                // Pasta nova ou renomeada: reindexa o conteúdo. Mudança "dentro" da pasta
                // (cada arquivo copiado) já chega como evento do próprio arquivo.
                if *structural {
                    db.remove_path(source_id, &text)?;
                    changed += 1 + index_dir(&db, source_id, path, skip_hidden)?;
                }
            }
            Ok(_) => {}
            Err(_) => {
                // Sumiu: apagado, movido ou renomeado.
                if db.remove_path(source_id, &text)? > 0 {
                    changed += 1;
                }
            }
        }
    }
    tx.commit()?;
    Ok(changed)
}

/// Indexa os arquivos de uma pasta (usado quando uma pasta aparece de uma vez).
fn index_dir(db: &Db, source_id: i64, dir: &Path, skip_hidden: bool) -> Result<usize> {
    let walker = jwalk::WalkDir::new(dir)
        .skip_hidden(false)
        .parallelism(jwalk::Parallelism::Serial)
        .process_read_dir(|_depth, _path, _state, children| {
            children.retain(|entry| match entry {
                Ok(e) if e.file_type().is_dir() => !is_system_dir(&e.file_name().to_string_lossy()),
                _ => true,
            });
        });
    let mut count = 0;
    for entry in walker.into_iter().flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if skip_hidden && has_hidden_or_system_attr(attributes(&meta)) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let parent = entry.parent_path().to_string_lossy().into_owned();
        let kind = if parent.to_lowercase().contains("\\steamapps\\") {
            "steam"
        } else {
            classify(&name).as_str()
        };
        db.upsert_file(source_id, &parent, &name, meta.len() as i64, unix_seconds(meta.modified()), kind)?;
        count += 1;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{scan_walk, ScanOptions};
    use std::fs;
    use std::sync::atomic::AtomicUsize;

    fn wait_until(mut condition: impl FnMut() -> bool) -> bool {
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(15) {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        false
    }

    #[test]
    fn splits_paths_like_the_scan_stores_them() {
        assert_eq!(Db::split_path("D:\\Jogos\\a.exe"), ("D:\\Jogos".to_string(), "a.exe".to_string()));
        assert_eq!(Db::split_path("D:\\a.exe"), ("D:\\".to_string(), "a.exe".to_string()));
    }

    #[test]
    fn removing_a_folder_removes_all_of_its_contents() {
        let db = Db::open_in_memory().unwrap();
        let s = db.add_source("D:", "D:\\").unwrap();
        db.upsert_file(s, "D:\\Filmes", "a.mkv", 1, None, "video").unwrap();
        db.upsert_file(s, "D:\\Filmes\\Serie\\T1", "b.mkv", 1, None, "video").unwrap();
        db.upsert_file(s, "D:\\Filmes2", "c.mkv", 1, None, "video").unwrap(); // nome parecido: não pode sair
        db.remove_path(s, "D:\\Filmes").unwrap();
        let left = db.search("", None, 10).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].name, "c.mkv");
    }

    #[test]
    fn catalog_follows_creates_and_deletes_made_outside_the_app() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("hd");
        fs::create_dir_all(root.join("fotos")).unwrap();
        fs::write(root.join("fotos").join("velha.jpg"), b"1234").unwrap();

        let db_path = dir.path().join("catalogo.db");
        let mut db = Db::open(&db_path).unwrap();
        let id = db.add_source("hd", &root.to_string_lossy()).unwrap();
        scan_walk(&mut db, id, &root, &ScanOptions::default(), &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert_eq!(db.search("velha", None, 10).unwrap().len(), 1);

        let notified = Arc::new(AtomicUsize::new(0));
        let counter = notified.clone();
        let stop = spawn(db_path.clone(), move |n| {
            counter.fetch_add(n, Ordering::Relaxed);
        });
        std::thread::sleep(Duration::from_secs(1)); // tempo de começar a observar

        // apagar fora do app
        fs::remove_file(root.join("fotos").join("velha.jpg")).unwrap();
        assert!(wait_until(|| db.search("velha", None, 10).unwrap().is_empty()), "arquivo apagado continuou no catálogo");

        // criar fora do app
        fs::write(root.join("fotos").join("nova.png"), b"123456").unwrap();
        assert!(wait_until(|| db.search("nova", None, 10).unwrap().len() == 1), "arquivo novo não entrou no catálogo");

        // pasta inteira apagada
        fs::remove_dir_all(root.join("fotos")).unwrap();
        assert!(wait_until(|| db.search("", None, 10).unwrap().is_empty()), "pasta apagada continuou no catálogo");

        stop.store(true, Ordering::Relaxed);
        assert!(notified.load(Ordering::Relaxed) > 0);
    }
}
