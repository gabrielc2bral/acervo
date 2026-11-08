//! Apagar cópias duplicadas com segurança.
//! Antes de apagar cada cópia, confere que o arquivo ainda existe, tem o tamanho do catálogo
//! e tem o mesmo conteúdo da cópia que fica (amostras do começo, do meio e do fim).

use crate::db::Db;
use anyhow::Result;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const SAMPLE: u64 = 1024 * 1024;

#[derive(Debug, Default)]
pub struct Report {
    pub deleted: usize,
    pub freed_bytes: u64,
    /// Uma frase por cópia que NÃO foi apagada, com o motivo.
    pub skipped: Vec<String>,
}

fn read_sample(file: &mut File, offset: u64, len: u64) -> std::io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(offset))?;
    let mut buffer = vec![0u8; len as usize];
    file.read_exact(&mut buffer)?;
    Ok(buffer)
}

/// Mesmo tamanho e mesmas amostras (começo, meio e fim). Para arquivos até 3 MB compara tudo.
pub fn same_content(a: &Path, b: &Path, size: u64) -> std::io::Result<bool> {
    if std::fs::metadata(a)?.len() != size || std::fs::metadata(b)?.len() != size {
        return Ok(false);
    }
    let (mut file_a, mut file_b) = (File::open(a)?, File::open(b)?);
    let len = SAMPLE.min(size);
    let offsets = [0, (size / 2).saturating_sub(len / 2), size.saturating_sub(len)];
    for offset in offsets {
        if read_sample(&mut file_a, offset, len)? != read_sample(&mut file_b, offset, len)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Apaga as cópias de `targets` (id no catálogo, caminho), mantendo `keep`.
/// `remove` faz o apagar de verdade (no app, manda para a Lixeira do Windows).
pub fn remove_duplicates(
    db: &Db,
    keep: &Path,
    size: u64,
    targets: &[(i64, PathBuf)],
    remove: &dyn Fn(&Path) -> Result<()>,
) -> Result<Report> {
    let mut report = Report::default();
    let mut removed_ids = Vec::new();

    for (id, path) in targets {
        if path == keep {
            report.skipped.push(format!("{}: é a cópia que fica", path.display()));
            continue;
        }
        if !path.exists() {
            report.skipped.push(format!("{}: não encontrado (HD desconectado?)", path.display()));
            continue;
        }
        match same_content(keep, path, size) {
            Ok(true) => {}
            Ok(false) => {
                report.skipped.push(format!("{}: o conteúdo é diferente da cópia que fica", path.display()));
                continue;
            }
            Err(error) => {
                report.skipped.push(format!("{}: não deu para comparar ({error})", path.display()));
                continue;
            }
        }
        match remove(path) {
            Ok(()) => {
                report.deleted += 1;
                report.freed_bytes += size;
                removed_ids.push(*id);
            }
            Err(error) => report.skipped.push(format!("{}: {error}", path.display())),
        }
    }

    db.delete_files(&removed_ids)?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn db_with_files(paths: &[&Path]) -> (Db, Vec<i64>) {
        let db = Db::open_in_memory().unwrap();
        let source = db.add_source("t", "t").unwrap();
        let mut ids = Vec::new();
        for path in paths {
            db.conn
                .execute(
                    "INSERT INTO files (source_id, dir, name, size, mtime, kind) VALUES (?1, ?2, ?3, 4, 0, 'other')",
                    rusqlite::params![
                        source,
                        path.parent().unwrap().to_string_lossy(),
                        path.file_name().unwrap().to_string_lossy()
                    ],
                )
                .unwrap();
            ids.push(db.conn.last_insert_rowid());
        }
        (db, ids)
    }

    #[test]
    fn deletes_identical_copy_and_keeps_the_other() {
        let dir = tempfile::tempdir().unwrap();
        let keep = dir.path().join("a.bin");
        let copy = dir.path().join("b.bin");
        fs::write(&keep, b"1234").unwrap();
        fs::write(&copy, b"1234").unwrap();
        let (db, ids) = db_with_files(&[&keep, &copy]);

        let report = remove_duplicates(&db, &keep, 4, &[(ids[1], copy.clone())], &|p| {
            fs::remove_file(p).map_err(Into::into)
        })
        .unwrap();

        assert_eq!(report.deleted, 1);
        assert_eq!(report.freed_bytes, 4);
        assert!(keep.exists() && !copy.exists());
        assert_eq!(db.search("b.bin", None, 5).unwrap().len(), 0);
        assert_eq!(db.search("a.bin", None, 5).unwrap().len(), 1);
    }

    #[test]
    fn refuses_to_delete_when_content_differs() {
        let dir = tempfile::tempdir().unwrap();
        let keep = dir.path().join("a.bin");
        let other = dir.path().join("b.bin");
        fs::write(&keep, b"1234").unwrap();
        fs::write(&other, b"1235").unwrap();
        let (db, ids) = db_with_files(&[&keep, &other]);

        let report = remove_duplicates(&db, &keep, 4, &[(ids[1], other.clone())], &|p| {
            fs::remove_file(p).map_err(Into::into)
        })
        .unwrap();

        assert_eq!(report.deleted, 0);
        assert_eq!(report.skipped.len(), 1);
        assert!(other.exists());
    }

    #[test]
    fn never_deletes_the_kept_copy() {
        let dir = tempfile::tempdir().unwrap();
        let keep = dir.path().join("a.bin");
        fs::write(&keep, b"1234").unwrap();
        let (db, ids) = db_with_files(&[&keep]);

        let report = remove_duplicates(&db, &keep, 4, &[(ids[0], keep.clone())], &|p| {
            fs::remove_file(p).map_err(Into::into)
        })
        .unwrap();

        assert_eq!(report.deleted, 0);
        assert!(keep.exists());
    }

    #[test]
    fn missing_file_is_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let keep = dir.path().join("a.bin");
        fs::write(&keep, b"1234").unwrap();
        let ghost = dir.path().join("sumiu.bin");
        let (db, ids) = db_with_files(&[&keep, &ghost]);

        let report = remove_duplicates(&db, &keep, 4, &[(ids[1], ghost)], &|p| {
            fs::remove_file(p).map_err(Into::into)
        })
        .unwrap();

        assert_eq!(report.deleted, 0);
        assert_eq!(report.skipped.len(), 1);
    }
}
