//! Cofre: o catálogo fica dentro de um volume VeraCrypt.
//!
//! O Acervo nunca recebe, guarda nem repassa a senha. Ele só chama o VeraCrypt,
//! que mostra a janela de senha dele. Sem `/p` na linha de comando, a senha
//! não aparece na lista de processos.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

pub fn veracrypt_exe() -> Option<PathBuf> {
    ["ProgramFiles", "ProgramFiles(x86)"]
        .iter()
        .filter_map(|var| std::env::var_os(var))
        .map(|dir| PathBuf::from(dir).join("VeraCrypt").join("VeraCrypt.exe"))
        .find(|path| path.is_file())
}

fn drive_root(letter: char) -> Result<String> {
    if !letter.is_ascii_alphabetic() {
        bail!("letra de unidade inválida: {letter}");
    }
    Ok(format!("{}:\\", letter.to_ascii_uppercase()))
}

/// A unidade existe (o volume está montado, ou outro disco usa essa letra).
pub fn drive_exists(letter: char) -> bool {
    drive_root(letter)
        .map(|root| Path::new(&root).exists())
        .unwrap_or(false)
}

/// Caminho do catálogo do Cofre dentro do volume montado.
pub fn catalog_path(letter: char) -> Result<PathBuf> {
    Ok(PathBuf::from(drive_root(letter)?)
        .join(".acervo")
        .join("catalogo.db"))
}

/// Pede ao VeraCrypt para montar o volume e espera a unidade aparecer.
/// A senha é digitada na janela do VeraCrypt.
pub fn mount(container: &Path, letter: char, timeout: Duration) -> Result<()> {
    let exe = veracrypt_exe().context("VeraCrypt não encontrado")?;
    let root = drive_root(letter)?;
    if Path::new(&root).exists() {
        bail!("a unidade {root} já está em uso");
    }

    Command::new(exe)
        .arg("/q")
        .arg("/v")
        .arg(container)
        .arg("/l")
        .arg(letter.to_string())
        .spawn()
        .context("não foi possível abrir o VeraCrypt")?;

    let started = Instant::now();
    while started.elapsed() < timeout {
        if Path::new(&root).exists() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    bail!("o volume não foi montado a tempo")
}

/// Desmonta sem forçar. Se algum programa estiver usando o volume, o VeraCrypt recusa
/// e nada é perdido. O banco do Cofre precisa estar fechado antes de chamar isto.
pub fn dismount(letter: char) -> Result<()> {
    let exe = veracrypt_exe().context("VeraCrypt não encontrado")?;
    let status = Command::new(exe)
        .arg("/q")
        .arg("/d")
        .arg(letter.to_string())
        .status()
        .context("não foi possível abrir o VeraCrypt")?;
    if !status.success() {
        bail!("o VeraCrypt não conseguiu desmontar (talvez o volume esteja em uso)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_lives_inside_the_volume() {
        assert_eq!(
            catalog_path('x').unwrap(),
            PathBuf::from("X:\\.acervo\\catalogo.db")
        );
    }

    #[test]
    fn rejects_invalid_letters() {
        assert!(drive_root('1').is_err());
        assert!(drive_root(':').is_err());
    }
}
