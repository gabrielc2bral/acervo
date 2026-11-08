//! Varredura rápida: lê a tabela de arquivos (MFT) do NTFS em vez de percorrer pasta por pasta.
//! Só vale para um HD inteiro (ex.: `D:\`) e exige administrador para abrir o volume.

use crate::db::Db;
use crate::scan::{Progress, ScanOptions};
use anyhow::Result;
use std::path::Path;
use std::sync::atomic::AtomicBool;

pub enum FastOutcome {
    /// `None` = cancelado.
    Done(Option<Progress>),
    NeedsAdmin,
    /// Não é a raiz de um HD NTFS (pasta, exFAT...): use a varredura por pastas.
    NotApplicable,
}

/// "D:\" ou "D:" viram `Some('D')`. Qualquer subpasta vira `None`.
pub fn drive_letter_of_root(path: &Path) -> Option<char> {
    let text = path.to_str()?;
    let mut chars = text.chars();
    let letter = chars.next().filter(char::is_ascii_alphabetic)?;
    if chars.next() != Some(':') {
        return None;
    }
    match chars.as_str() {
        "" | "\\" | "/" => Some(letter.to_ascii_uppercase()),
        _ => None,
    }
}

/// `\\.\D:\pasta\arquivo` vira `D:\pasta\arquivo`.
pub fn strip_device_prefix(path: &str) -> &str {
    path.strip_prefix(r"\\.\")
        .or_else(|| path.strip_prefix(r"\\?\"))
        .unwrap_or(path)
}

/// Reabre o próprio Acervo como administrador (o Windows mostra o pedido de permissão)
/// só para varrer essa fonte com a leitura rápida. Devolve `false` se o usuário recusou
/// ou se a varredura falhou.
pub fn run_elevated_scan(db_path: &Path, source_id: i64) -> Result<bool> {
    let exe = std::env::current_exe()?;
    run_elevated(
        &exe,
        &["scan-id".to_string(), source_id.to_string(), db_path.to_string_lossy().into_owned()],
    )
}

/// Roda `exe` com `args` como administrador (o Windows mostra o pedido de permissão) e espera
/// terminar. Devolve `false` se o usuário recusou ou se o programa saiu com erro.
pub fn run_elevated(exe: &Path, args: &[String]) -> Result<bool> {
    let quote = |text: &str| text.replace('\'', "''");
    // Argumentos com espaço precisam de aspas dentro do argumento, para o programa receber inteiro.
    let list: Vec<String> = args
        .iter()
        .map(|arg| {
            if arg.contains(' ') {
                format!("'\"{}\"'", quote(arg))
            } else {
                format!("'{}'", quote(arg))
            }
        })
        .collect();
    let script = format!(
        "$p = Start-Process -FilePath '{}' -ArgumentList {} -Verb RunAs -Wait -PassThru -WindowStyle Hidden -ErrorAction Stop; exit $p.ExitCode",
        quote(&exe.to_string_lossy()),
        list.join(","),
    );
    let status = std::process::Command::new("powershell")
        .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &script])
        .status()?;
    Ok(status.success())
}

#[cfg(not(windows))]
pub fn try_scan(
    _db: &mut Db,
    _source_id: i64,
    _root: &Path,
    _opts: &ScanOptions,
    _cancel: &AtomicBool,
    _on_progress: &mut dyn FnMut(&Progress),
) -> Result<FastOutcome> {
    Ok(FastOutcome::NotApplicable)
}

#[cfg(windows)]
pub fn try_scan(
    db: &mut Db,
    source_id: i64,
    root: &Path,
    opts: &ScanOptions,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(&Progress),
) -> Result<FastOutcome> {
    use crate::kind::{classify, has_hidden_or_system_attr, is_system_dir};
    use crate::scan::Stage;
    use ntfs_reader::{DefaultPathCache, FileInfo, MftScan, NtfsReaderError, Volume};
    use std::path::PathBuf;
    use std::sync::atomic::Ordering;

    let Some(letter) = drive_letter_of_root(root) else {
        return Ok(FastOutcome::NotApplicable);
    };
    let volume = match Volume::new(format!(r"\\.\{letter}:")) {
        Ok(volume) => volume,
        Err(NtfsReaderError::AccessDenied) => return Ok(FastOutcome::NeedsAdmin),
        Err(_) => return Ok(FastOutcome::NotApplicable), // não é NTFS
    };
    let mut scan = match MftScan::new(volume) {
        Ok(scan) => scan,
        Err(_) => return Ok(FastOutcome::NotApplicable),
    };

    // A leitura da MFT (minutos num HD grande) vai para um banco temporário; o catálogo só é
    // travado na troca final, que dura segundos.
    let stage = Stage::new()?;
    let mut progress = Progress::default();
    let mut cache = DefaultPathCache::new();
    while let Some(chunk) = scan.next_chunk()? {
        if cancel.load(Ordering::Relaxed) {
            return Ok(FastOutcome::Done(None));
        }
        for file in chunk.files() {
            let info = FileInfo::with_cache(&file, &mut cache);
            if info.is_directory || info.is_deleted {
                continue;
            }
            if opts.skip_hidden && has_hidden_or_system_attr(info.file_attributes) {
                continue;
            }
            let Some(full_path) = info.path.as_ref() else {
                continue;
            };
            let full_path = PathBuf::from(strip_device_prefix(&full_path.to_string_lossy()));
            let in_system_dir = full_path
                .components()
                .any(|part| is_system_dir(&part.as_os_str().to_string_lossy()));
            if in_system_dir {
                continue;
            }
            let dir = full_path
                .parent()
                .map(|parent| parent.to_string_lossy().into_owned())
                .unwrap_or_default();

            stage.insert(
                source_id,
                &dir,
                &info.name,
                info.size as i64,
                info.modified.map(|time| time.unix_timestamp()),
                classify(&info.name).as_str(),
                Some(file.number() as i64),
            )?;
            progress.files += 1;
            progress.bytes += info.size;
        }
        on_progress(&progress);
    }

    stage.commit_into(db, source_id)?;
    on_progress(&progress);
    Ok(FastOutcome::Done(Some(progress)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_drive_roots_only() {
        assert_eq!(drive_letter_of_root(Path::new("D:\\")), Some('D'));
        assert_eq!(drive_letter_of_root(Path::new("e:")), Some('E'));
        assert_eq!(drive_letter_of_root(Path::new("D:\\Jogos")), None);
        assert_eq!(drive_letter_of_root(Path::new("\\\\servidor\\x")), None);
    }

    #[test]
    fn strips_device_prefix() {
        assert_eq!(strip_device_prefix(r"\\.\D:\a\b.txt"), r"D:\a\b.txt");
        assert_eq!(strip_device_prefix(r"D:\a"), r"D:\a");
    }
}
