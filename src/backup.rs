//! Backup e restauração do catálogo, para guardar antes de formatar o PC.
//!
//! O arquivo `.acervo-backup` leva TUDO do catálogo principal: fontes, arquivos, miniaturas e
//! configurações. Ele é uma cópia consistente do SQLite (`VACUUM INTO`, que pode ser feita com o
//! app aberto e o serviço rodando) comprimida em gzip, com um cabeçalho de texto.
//! O catálogo do Cofre fica dentro do volume VeraCrypt e não entra aqui: guarde o volume à parte.
//!
//! Restaurar troca o conteúdo do catálogo atual pelo do backup (e antes guarda uma cópia do atual
//! em `antes-de-restaurar.acervo-backup`, na mesma pasta do catálogo).

use crate::db::Db;
use crate::version::{SCHEMA_VERSION, VERSION};
use anyhow::{bail, Context, Result};
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

const MAGIC: &str = "ACERVO-BACKUP 1";
/// Configurações que pertencem a esta máquina/sessão e não vão no backup nem voltam dele.
const MACHINE_SETTINGS_SQL: &str =
    "key = 'service_heartbeat' OR key LIKE 'usn:%' OR key LIKE 'usn_%' OR key LIKE 'thumbs_purged%'";

/// Extensão sugerida para os arquivos de backup.
pub const EXTENSION: &str = "acervo-backup";

#[derive(Debug)]
pub struct Summary {
    pub files: i64,
    pub sources: i64,
    pub thumbnails: i64,
}

fn sql_quote(path: &Path) -> String {
    path.to_string_lossy().replace('\'', "''")
}

fn temporary_beside(path: &Path, label: &str) -> PathBuf {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    path.with_file_name(format!("{name}.{label}.tmp"))
}

/// Cria o backup do catálogo `db_path` em `destination`. `report` recebe frases de andamento.
pub fn create(db_path: &Path, destination: &Path, report: &dyn Fn(&str)) -> Result<Summary> {
    let db = Db::open(db_path)?;
    let temporary_db = temporary_beside(destination, "catalogo");
    let _ = std::fs::remove_file(&temporary_db);

    report("Copiando o catálogo...");
    db.conn
        .execute_batch(&format!("VACUUM INTO '{}'", sql_quote(&temporary_db)))
        .context("não foi possível copiar o catálogo")?;

    // Tira da cópia o que só vale para esta máquina.
    let copy = Db::open(&temporary_db)?;
    copy.conn.execute_batch(&format!(
        "DELETE FROM changes; DELETE FROM service_jobs; DELETE FROM settings WHERE {MACHINE_SETTINGS_SQL};"
    ))?;
    let summary = Summary {
        files: copy.conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))?,
        sources: copy.conn.query_row("SELECT COUNT(*) FROM sources", [], |r| r.get(0))?,
        thumbnails: copy.conn.query_row("SELECT COUNT(*) FROM thumbs", [], |r| r.get(0))?,
    };
    // Fecha a cópia (e leva o WAL para dentro do arquivo) antes de comprimir.
    copy.conn
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode = DELETE;")?;
    drop(copy);

    report("Comprimindo o backup...");
    let temporary_backup = temporary_beside(destination, "backup");
    let result = (|| -> Result<()> {
        let mut out = BufWriter::new(File::create(&temporary_backup)?);
        let created = crate::scan::unix_seconds(Ok(std::time::SystemTime::now())).unwrap_or(0);
        write!(
            out,
            "{MAGIC}\nversion={VERSION}\nschema={SCHEMA_VERSION}\ncreated={created}\nfiles={}\nsources={}\nthumbnails={}\n\n",
            summary.files, summary.sources, summary.thumbnails
        )?;
        let mut encoder = GzEncoder::new(out, Compression::fast());
        std::io::copy(&mut BufReader::new(File::open(&temporary_db)?), &mut encoder)?;
        encoder.finish()?.flush()?;
        std::fs::rename(&temporary_backup, destination)?;
        Ok(())
    })();
    let _ = std::fs::remove_file(&temporary_db);
    let _ = std::fs::remove_file(temporary_db.with_extension("tmp-wal"));
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary_backup);
    }
    result?;
    Ok(summary)
}

/// Lê o cabeçalho de um backup (e confere se é mesmo um backup do Acervo).
fn read_header(reader: &mut impl BufRead) -> Result<std::collections::HashMap<String, String>> {
    let mut first = String::new();
    reader.read_line(&mut first)?;
    if first.trim_end() != MAGIC {
        bail!("Este arquivo não é um backup do Acervo (ou é de um formato que este programa não conhece).");
    }
    let mut fields = std::collections::HashMap::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            bail!("backup incompleto");
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((key, value)) = line.split_once('=') {
            fields.insert(key.to_string(), value.to_string());
        }
    }
    Ok(fields)
}

/// O que há no backup, sem restaurar nada (para mostrar antes de confirmar).
pub fn describe(backup: &Path) -> Result<String> {
    let mut reader = BufReader::new(File::open(backup)?);
    let header = read_header(&mut reader)?;
    let get = |key: &str| header.get(key).cloned().unwrap_or_default();
    Ok(format!(
        "Acervo {} · {} arquivos em {} fontes · {} miniaturas",
        get("version"),
        get("files"),
        get("sources"),
        get("thumbnails")
    ))
}

/// Restaura `backup` no catálogo `db_path`. Antes, guarda uma cópia do catálogo atual.
pub fn restore(db_path: &Path, backup: &Path, report: &dyn Fn(&str)) -> Result<Summary> {
    let mut reader = BufReader::new(File::open(backup).context("não foi possível abrir o backup")?);
    let header = read_header(&mut reader)?;
    let backup_schema: i64 = header.get("schema").and_then(|s| s.parse().ok()).unwrap_or(0);
    if backup_schema > SCHEMA_VERSION {
        bail!(
            "Este backup é de uma versão mais nova do Acervo (formato v{backup_schema}; este programa entende até v{SCHEMA_VERSION}). Atualize o Acervo antes de restaurar."
        );
    }

    report("Descompactando o backup...");
    let temporary_db = temporary_beside(db_path, "restauracao");
    let _ = std::fs::remove_file(&temporary_db);
    let unpacked = (|| -> Result<()> {
        let mut out = BufWriter::new(File::create(&temporary_db)?);
        std::io::copy(&mut GzDecoder::new(reader), &mut out).context("o backup está corrompido")?;
        out.flush()?;
        Ok(())
    })();
    if let Err(error) = unpacked {
        let _ = std::fs::remove_file(&temporary_db);
        return Err(error);
    }

    let result = (|| -> Result<Summary> {
        // Abrir pelo Db já atualiza o formato de um backup mais antigo.
        let check = Db::open(&temporary_db)?;
        let verdict: String = check.conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
        if verdict != "ok" {
            bail!("o backup está corrompido ({verdict})");
        }
        drop(check);

        report("Guardando uma cópia do catálogo atual...");
        let safety = db_path.with_file_name(format!("antes-de-restaurar.{EXTENSION}"));
        create(db_path, &safety, &|_| {}).context("não foi possível guardar a cópia de segurança")?;

        report("Restaurando os dados...");
        let db = Db::open(db_path)?;
        db.conn
            .execute(&format!("ATTACH DATABASE '{}' AS bk", sql_quote(&temporary_db)), [])?;
        let copied = (|| -> Result<Summary> {
            let tx = db.conn.unchecked_transaction()?;
            tx.execute_batch(
                "DELETE FROM main.thumbs;
                 DELETE FROM main.files;
                 DELETE FROM main.sources;
                 INSERT INTO main.sources (id, name, path, last_scan) SELECT id, name, path, last_scan FROM bk.sources;
                 INSERT INTO main.files (id, source_id, dir, name, size, mtime, kind, frn)
                     SELECT id, source_id, dir, name, size, mtime, kind, frn FROM bk.files;
                 INSERT INTO main.thumbs (key, data) SELECT key, data FROM bk.thumbs;
                 DELETE FROM main.changes;",
            )?;
            tx.execute_batch(&format!(
                "INSERT OR REPLACE INTO main.settings (key, value)
                     SELECT key, value FROM bk.settings WHERE NOT ({MACHINE_SETTINGS_SQL});"
            ))?;
            let summary = Summary {
                files: tx.query_row("SELECT COUNT(*) FROM main.files", [], |r| r.get(0))?,
                sources: tx.query_row("SELECT COUNT(*) FROM main.sources", [], |r| r.get(0))?,
                thumbnails: tx.query_row("SELECT COUNT(*) FROM main.thumbs", [], |r| r.get(0))?,
            };
            tx.commit()?;
            Ok(summary)
        })();
        let _ = db.conn.execute("DETACH DATABASE bk", []);
        copied
    })();
    let _ = std::fs::remove_file(&temporary_db);
    let _ = std::fs::remove_file(temporary_db.with_extension("tmp-wal"));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog(dir: &Path) -> (PathBuf, Db) {
        let path = dir.join("acervo.db");
        let db = Db::open(&path).unwrap();
        let source = db.add_source("D:", "D:\\").unwrap();
        for name in ["a.mkv", "b.jpg", "c.pdf"] {
            db.upsert_file(source, "D:\\x", name, 100, Some(5), "other").unwrap();
        }
        db.put_thumb(7, &[1, 2, 3]).unwrap();
        db.set_setting("admin_mode", "service").unwrap();
        db.set_setting("service_heartbeat", "123").unwrap();
        db.set_setting("usn:1", "5:9").unwrap();
        (path, db)
    }

    #[test]
    fn backup_then_restore_brings_everything_back() {
        let dir = tempfile::tempdir().unwrap();
        let (path, db) = catalog(dir.path());
        let backup = dir.path().join("copia.acervo-backup");

        let summary = create(&path, &backup, &|_| {}).unwrap();
        assert_eq!((summary.files, summary.sources, summary.thumbnails), (3, 1, 1));
        assert!(describe(&backup).unwrap().contains("3 arquivos"));

        // "formatou": o catálogo ficou vazio e as configurações mudaram
        db.conn.execute_batch("DELETE FROM files; DELETE FROM sources; DELETE FROM thumbs;").unwrap();
        db.set_setting("admin_mode", "never").unwrap();
        assert_eq!(db.search("", None, 10).unwrap().len(), 0);

        let restored = restore(&path, &backup, &|_| {}).unwrap();
        assert_eq!((restored.files, restored.sources, restored.thumbnails), (3, 1, 1));
        assert_eq!(db.search("", None, 10).unwrap().len(), 3);
        assert_eq!(db.get_thumb(7).unwrap(), Some(vec![1, 2, 3]));
        assert_eq!(db.get_setting("admin_mode").unwrap().as_deref(), Some("service"));
        // o que é da máquina não vai e não volta
        assert_eq!(db.get_setting("usn:1").unwrap().as_deref(), Some("5:9"));
        // uma cópia de segurança do que havia antes de restaurar
        assert!(dir.path().join("antes-de-restaurar.acervo-backup").is_file());
        // e nenhum arquivo temporário sobrou
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn backup_does_not_carry_machine_settings() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _db) = catalog(dir.path());
        let backup = dir.path().join("copia.acervo-backup");
        create(&path, &backup, &|_| {}).unwrap();

        let other = tempfile::tempdir().unwrap();
        let (other_path, other_db) = {
            let path = other.path().join("acervo.db");
            let db = Db::open(&path).unwrap();
            (path, db)
        };
        restore(&other_path, &backup, &|_| {}).unwrap();
        assert_eq!(other_db.get_setting("admin_mode").unwrap().as_deref(), Some("service"));
        assert_eq!(other_db.get_setting("service_heartbeat").unwrap(), None);
        assert_eq!(other_db.get_setting("usn:1").unwrap(), None);
    }

    #[test]
    fn refuses_files_that_are_not_backups_or_are_from_a_newer_version() {
        let dir = tempfile::tempdir().unwrap();
        let (path, db) = catalog(dir.path());

        let not_backup = dir.path().join("qualquer.acervo-backup");
        std::fs::write(&not_backup, b"isto nao e um backup").unwrap();
        assert!(restore(&path, &not_backup, &|_| {}).is_err());

        let newer = dir.path().join("novo.acervo-backup");
        std::fs::write(&newer, format!("{MAGIC}\nversion=9.9.9\nschema={}\n\n", SCHEMA_VERSION + 1)).unwrap();
        let error = restore(&path, &newer, &|_| {}).unwrap_err().to_string();
        assert!(error.contains("mais nova"), "{error}");

        let corrupt = dir.path().join("quebrado.acervo-backup");
        std::fs::write(&corrupt, format!("{MAGIC}\nschema={SCHEMA_VERSION}\n\nnao-e-gzip")).unwrap();
        assert!(restore(&path, &corrupt, &|_| {}).is_err());

        // nada do catálogo atual foi mexido pelas tentativas que falharam
        assert_eq!(db.search("", None, 10).unwrap().len(), 3);
    }
}
