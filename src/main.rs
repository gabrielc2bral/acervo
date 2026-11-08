//! Janela do Acervo. Sem console: é o que o usuário abre.
//! `scan-id` é usado pela própria janela para varrer como administrador.
//! Para a linha de comando, use `acervo-cli`.

#![cfg_attr(windows, windows_subsystem = "windows")]

mod ui;

use acervo::{
    db::Db,
    scan::{self, Outcome},
    settings::Settings,
};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] => ui::run(),
        ["scan-id", id, db_path] => scan_by_id(id.parse()?, Path::new(db_path)),
        _ => bail!("argumentos desconhecidos"),
    }
}

/// Roda elevado: só a leitura rápida. Se faltar permissão, sai com erro e a janela usa o modo normal.
fn scan_by_id(source_id: i64, db_path: &Path) -> Result<()> {
    let mut db = Db::open(db_path)?;
    let root = db
        .sources()?
        .into_iter()
        .find(|source| source.id == source_id)
        .map(|source| PathBuf::from(source.path))
        .context("fonte não encontrada")?;
    let options = Settings::load(&Db::open(&Db::default_path()?)?)?.scan_options();
    let cancel = AtomicBool::new(false);
    match scan::scan_source(&mut db, source_id, &root, &options, &cancel, &mut |_| {})? {
        Outcome::Done(_) => Ok(()),
        Outcome::Cancelled => bail!("cancelado"),
        Outcome::NeedsAdmin => bail!("sem permissão de administrador"),
    }
}
