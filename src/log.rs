//! Registro em arquivo do serviço e do instalador de serviço: `C:\ProgramData\Acervo\service.log`.
//! Roda sem janela e sem console, então é o único jeito de saber o que aconteceu quando algo falha.
//! A pasta do ProgramData é gravável pelo serviço (sistema) e pelo administrador, e legível por todos.

use std::io::Write;
use std::path::PathBuf;

/// Passou disso, o arquivo antigo vira `service.log.antigo` e começa outro.
const MAX_BYTES: u64 = 1_000_000;

pub fn path() -> PathBuf {
    if !cfg!(windows) {
        return std::env::temp_dir().join("Acervo").join("service.log"); // sem serviço no Linux
    }
    let base = std::env::var_os("ProgramData").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    base.join("Acervo").join("service.log")
}

fn timestamp() -> String {
    let now = crate::link::now_secs();
    let (days, seconds) = (now.div_euclid(86_400), now.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        seconds / 3600,
        seconds % 3600 / 60,
        seconds % 60
    )
}

/// Acrescenta uma linha ao registro. Nunca falha: se não der para gravar, simplesmente não grava.
pub fn write(message: &str) {
    let path = path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::metadata(&path).map(|meta| meta.len() > MAX_BYTES).unwrap_or(false) {
        let _ = std::fs::rename(&path, path.with_extension("log.antigo"));
    }
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "{} [{}] {message}", timestamp(), std::process::id());
    }
}

/// As últimas `lines` linhas do registro (para mostrar ao usuário).
pub fn tail(lines: usize) -> String {
    let text = std::fs::read_to_string(path()).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_has_the_expected_shape() {
        let text = timestamp();
        assert!(text.ends_with(" UTC"), "{text}");
        assert_eq!(text.len(), "2026-11-07 22:06:54 UTC".len(), "{text}");
        assert!(text.starts_with("20"), "{text}");
    }
}
