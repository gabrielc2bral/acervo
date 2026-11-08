//! Varredura de uma pasta ou HD. Em HD NTFS inteiro usa a leitura rápida da MFT (`fast`);
//! no resto, percorre as pastas em poucas threads. Pula arquivos e pastas do sistema.

use crate::db::Db;
use crate::fast::{self, FastOutcome};
use crate::kind::{classify, has_hidden_or_system_attr, is_system_dir};
use anyhow::Result;
use jwalk::{Parallelism, WalkDir};
use rusqlite::{params, Transaction};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Default, Clone, Copy)]
pub struct Progress {
    pub files: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct ScanOptions {
    pub threads: usize,
    pub skip_hidden: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions {
            threads: 2,
            skip_hidden: true,
        }
    }
}

#[derive(Debug)]
pub enum Outcome {
    Done(Progress),
    /// O usuário cancelou. O catálogo antigo continua intacto.
    Cancelled,
    /// HD NTFS inteiro, mas a leitura rápida exige administrador.
    NeedsAdmin,
}

#[cfg(windows)]
fn attributes(meta: &std::fs::Metadata) -> u32 {
    use std::os::windows::fs::MetadataExt;
    meta.file_attributes()
}

#[cfg(not(windows))]
fn attributes(_meta: &std::fs::Metadata) -> u32 {
    0
}

pub(crate) fn unix_seconds(time: std::io::Result<SystemTime>) -> Option<i64> {
    let since_epoch = time.ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some(since_epoch.as_secs() as i64)
}

/// Escolhe o jeito mais rápido possível para `root`.
pub fn scan_source(
    db: &mut Db,
    source_id: i64,
    root: &Path,
    opts: &ScanOptions,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(&Progress),
) -> Result<Outcome> {
    match fast::try_scan(db, source_id, root, opts, cancel, on_progress)? {
        FastOutcome::Done(Some(progress)) => Ok(Outcome::Done(progress)),
        FastOutcome::Done(None) => Ok(Outcome::Cancelled),
        FastOutcome::NeedsAdmin => Ok(Outcome::NeedsAdmin),
        FastOutcome::NotApplicable => scan_walk(db, source_id, root, opts, cancel, on_progress),
    }
}

/// Percorre as pastas. Funciona em qualquer disco e sem administrador.
pub fn scan_walk(
    db: &mut Db,
    source_id: i64,
    root: &Path,
    opts: &ScanOptions,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(&Progress),
) -> Result<Outcome> {
    let walker = WalkDir::new(root)
        .skip_hidden(false)
        .parallelism(Parallelism::RayonNewPool(opts.threads.max(1)))
        .process_read_dir(|_depth, _path, _state, children| {
            children.retain(|entry| match entry {
                Ok(e) if e.file_type().is_dir() => {
                    !is_system_dir(&e.file_name().to_string_lossy())
                }
                _ => true,
            });
        });

    // Os arquivos lidos vão para um banco temporário. O catálogo de verdade só é tocado no fim,
    // numa troca curta: durante a leitura (que pode levar minutos) ninguém fica esperando o banco.
    let stage = Stage::new()?;
    let mut progress = Progress::default();
    for entry in walker {
        if cancel.load(Ordering::Relaxed) {
            return Ok(Outcome::Cancelled); // nada foi gravado: o catálogo antigo continua intacto
        }
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if opts.skip_hidden && has_hidden_or_system_attr(attributes(&meta)) {
            continue;
        }

        let name = entry.file_name().to_string_lossy().into_owned();
        let dir = entry.parent_path().to_string_lossy().into_owned();
        let size = meta.len();
        stage.insert(source_id, &dir, &name, size as i64, unix_seconds(meta.modified()), classify(&name).as_str(), None)?;

        progress.files += 1;
        progress.bytes += size;
        if progress.files % 2000 == 0 {
            on_progress(&progress);
        }
    }

    stage.commit_into(db, source_id)?;
    on_progress(&progress);
    Ok(Outcome::Done(progress))
}

/// Arquivos lidos na varredura, num banco temporário, até o momento de trocar no catálogo.
pub(crate) struct Stage {
    // `conn` vem antes de `file`: o arquivo só é apagado depois de a conexão fechar (no Windows
    // não dá para apagar um arquivo aberto).
    conn: rusqlite::Connection,
    file: StageFile,
}

struct StageFile(std::path::PathBuf);

impl Drop for StageFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

impl Stage {
    pub(crate) fn new() -> Result<Stage> {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "acervo-varredura-{}-{}.db",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&path);
        let conn = rusqlite::Connection::open(&path)?;
        let file = StageFile(path);
        // Arquivo descartável: sem diário e sem esperar o disco gravar.
        conn.execute_batch(
            "PRAGMA journal_mode = OFF;
             PRAGMA synchronous = OFF;
             CREATE TABLE files (source_id INTEGER, dir TEXT, name TEXT, size INTEGER, mtime INTEGER, kind TEXT, frn INTEGER);
             BEGIN;",
        )?;
        Ok(Stage { conn, file })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn insert(
        &self,
        source_id: i64,
        dir: &str,
        name: &str,
        size: i64,
        mtime: Option<i64>,
        kind: &str,
        frn: Option<i64>,
    ) -> Result<()> {
        self.conn
            .prepare_cached("INSERT INTO files VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)")?
            .execute(params![source_id, dir, name, size, mtime, kind, frn])?;
        Ok(())
    }

    /// Troca as linhas da fonte no catálogo pelas lidas, numa transação só (a única parte em que
    /// o catálogo fica travado para gravação, e dura segundos).
    pub(crate) fn commit_into(self, db: &mut Db, source_id: i64) -> Result<()> {
        self.conn.execute_batch("COMMIT")?;
        let path = self.file.0.to_string_lossy().replace('\'', "''");
        db.conn.execute_batch(&format!("ATTACH DATABASE '{path}' AS stage"))?;
        let result = (|| -> Result<()> {
            let tx = db.conn.transaction()?;
            tx.execute("DELETE FROM main.files WHERE source_id = ?1", params![source_id])?;
            tx.execute(
                "INSERT INTO main.files (source_id, dir, name, size, mtime, kind, frn)
                 SELECT source_id, dir, name, size, mtime, kind, frn FROM stage.files",
                [],
            )?;
            finish_scan(&tx, source_id)?;
            tx.commit()?;
            Ok(())
        })();
        let _ = db.conn.execute_batch("DETACH DATABASE stage");
        result
    }
}

/// Passos finais comuns às duas varreduras, dentro da mesma transação.
pub(crate) fn finish_scan(tx: &Transaction, source_id: i64) -> Result<()> {
    mark_repacks(tx, source_id)?;
    let now = unix_seconds(Ok(SystemTime::now()));
    tx.execute(
        "UPDATE sources SET last_scan = ?1 WHERE id = ?2",
        params![now, source_id],
    )?;
    Ok(())
}

/// Reclassifica de novo o catálogo já gravado, sem varrer os HDs (que podem estar offline).
pub fn reclassify(db: &mut Db) -> Result<()> {
    let ids: Vec<i64> = db.sources()?.into_iter().map(|source| source.id).collect();
    let tx = db.conn.transaction()?;
    for id in ids {
        mark_repacks(&tx, id)?;
    }
    tx.commit()?;
    Ok(())
}

/// Uma pasta é de repack quando:
/// - tem arquivos `fg-01.bin`, `setup-fitgirl-01.bin` ou os atalhos "FitGirl releases on ...", ou
/// - o nome da própria pasta diz "repack" (ex.: `Hades [FitGirl Repack]`).
/// Tudo que está nessa pasta passa a ser do tipo "repack".
fn mark_repacks(tx: &Transaction, source_id: i64) -> Result<()> {
    // Arquivos dentro de bibliotecas da Steam ficam separados (tipo "steam"), nunca viram repack.
    tx.execute(
        "UPDATE files SET kind = 'steam'
         WHERE source_id = ?1 AND kind != 'steam' AND dir LIKE '%\\steamapps\\%'",
        params![source_id],
    )?;

    tx.execute(
        "UPDATE files SET kind = 'repack'
         WHERE source_id = ?1 AND kind NOT IN ('repack', 'steam')
           AND dir IN (SELECT dir FROM files
                       WHERE source_id = ?1
                         AND (name LIKE 'fg-%.bin'
                              OR name LIKE 'setup-fitgirl-%.bin'
                              OR name LIKE 'fitgirl releases on%'))",
        params![source_id],
    )?;

    let candidate_dirs: Vec<String> = {
        let mut stmt = tx.prepare(
            "SELECT DISTINCT dir FROM files
             WHERE source_id = ?1 AND kind NOT IN ('repack', 'steam') AND dir LIKE '%repack%'",
        )?;
        let rows = stmt.query_map(params![source_id], |row| row.get(0))?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    for dir in candidate_dirs {
        if folder_name_says_repack(&dir) {
            tx.execute(
                "UPDATE files SET kind = 'repack' WHERE source_id = ?1 AND dir = ?2",
                params![source_id, dir],
            )?;
        }
    }
    Ok(())
}

/// A palavra "repack" inteira no nome da pasta. "texturepacks" não conta.
fn folder_name_says_repack(dir: &str) -> bool {
    let Some(name) = Path::new(dir).file_name() else {
        return false;
    };
    name.to_string_lossy()
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| word == "repack" || word == "repacks")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn build_tree(root: &Path) {
        fs::create_dir_all(root.join("Filmes")).unwrap();
        fs::create_dir_all(root.join("Windows").join("System32")).unwrap();
        fs::create_dir_all(root.join("Jogo Repack")).unwrap();
        fs::write(root.join("Filmes").join("a.mkv"), b"1234").unwrap();
        fs::write(root.join("Windows").join("System32").join("x.dll"), b"1").unwrap();
        fs::write(root.join("Jogo Repack").join("fg-01.bin"), b"12").unwrap();
        fs::write(root.join("Jogo Repack").join("setup.exe"), b"1").unwrap();
    }

    fn run(db: &mut Db, id: i64, root: &Path, cancel: &AtomicBool) -> Outcome {
        scan_walk(db, id, root, &ScanOptions::default(), cancel, &mut |_| {}).unwrap()
    }

    #[test]
    fn scans_skips_system_and_marks_repacks() {
        let dir = tempfile::tempdir().unwrap();
        build_tree(dir.path());
        let mut db = Db::open_in_memory().unwrap();
        let id = db.add_source("teste", &dir.path().to_string_lossy()).unwrap();

        let Outcome::Done(progress) = run(&mut db, id, dir.path(), &AtomicBool::new(false)) else {
            panic!("a varredura devia terminar");
        };

        assert_eq!(progress.files, 3); // x.dll em Windows\ ficou de fora
        assert!(db.search("x.dll", None, 10).unwrap().is_empty());
        assert_eq!(db.search("setup.exe", None, 10).unwrap()[0].kind, "repack");
        assert_eq!(db.search("a.mkv", None, 10).unwrap()[0].kind, "video");
    }

    #[test]
    fn recognizes_other_repack_layouts_and_reclassifies_old_catalogs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("Jogo A [FitGirl Repack]")).unwrap();
        fs::create_dir_all(root.join("Jogo B")).unwrap();
        fs::create_dir_all(root.join("Jogo C")).unwrap();
        fs::create_dir_all(root.join("Outros").join("repack-ferramentas-antigas")).unwrap();
        // só o nome da pasta diz repack
        fs::write(root.join("Jogo A [FitGirl Repack]").join("setup.exe"), b"1").unwrap();
        // formato antigo dos binários
        fs::write(root.join("Jogo B").join("setup-fitgirl-01.bin"), b"1").unwrap();
        fs::write(root.join("Jogo B").join("setup.exe"), b"1").unwrap();
        // pasta comum com um programa qualquer
        fs::write(root.join("Jogo C").join("setup.exe"), b"1").unwrap();
        fs::write(root.join("Outros").join("repack-ferramentas-antigas").join("x.txt"), b"1").unwrap();

        let mut db = Db::open_in_memory().unwrap();
        let id = db.add_source("teste", &root.to_string_lossy()).unwrap();
        run(&mut db, id, root, &AtomicBool::new(false));
        // simula catálogo gravado por uma versão antiga: tudo sem tipo repack
        db.conn.execute("UPDATE files SET kind = 'other'", []).unwrap();
        reclassify(&mut db).unwrap();

        let repacks = db.repack_folders().unwrap();
        let names: Vec<&str> = repacks.iter().map(|folder| folder.dir.as_str()).collect();
        assert_eq!(repacks.len(), 3, "{names:?}");
        assert!(names.iter().any(|dir| dir.ends_with("Jogo A [FitGirl Repack]")));
        assert!(names.iter().any(|dir| dir.ends_with("Jogo B")));
        assert!(!names.iter().any(|dir| dir.ends_with("Jogo C")));
    }

    #[test]
    fn steam_files_are_separate_and_texturepacks_is_not_a_repack() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let textures = root.join("SteamLibrary").join("steamapps").join("common").join("Jogo").join("texturepacks");
        fs::create_dir_all(&textures).unwrap();
        fs::write(textures.join("a.png"), b"1").unwrap();
        fs::write(root.join("normal.png"), b"1").unwrap();

        let mut db = Db::open_in_memory().unwrap();
        let id = db.add_source("teste", &root.to_string_lossy()).unwrap();
        run(&mut db, id, root, &AtomicBool::new(false));

        assert!(db.repack_folders().unwrap().is_empty());
        assert_eq!(db.search("a.png", Some("steam"), 10).unwrap().len(), 1);
        // a busca sem filtro não mostra arquivos da Steam
        assert!(db.search("a.png", None, 10).unwrap().is_empty());
        assert_eq!(db.search("normal.png", None, 10).unwrap().len(), 1);
    }

    #[test]
    fn cancel_keeps_previous_catalog() {
        let dir = tempfile::tempdir().unwrap();
        build_tree(dir.path());
        let mut db = Db::open_in_memory().unwrap();
        let id = db.add_source("teste", &dir.path().to_string_lossy()).unwrap();
        run(&mut db, id, dir.path(), &AtomicBool::new(false));

        let outcome = run(&mut db, id, dir.path(), &AtomicBool::new(true));

        assert!(matches!(outcome, Outcome::Cancelled));
        assert_eq!(db.search("a.mkv", None, 10).unwrap().len(), 1);
    }
}
