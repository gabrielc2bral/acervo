//! Configurações do usuário, guardadas na tabela `settings` do banco principal.
//! Nunca guardam senha: o VeraCrypt pede a senha na janela dele.

use crate::db::Db;
use anyhow::Result;
use std::path::PathBuf;

/// Como lidar com a leitura rápida do NTFS, que exige administrador.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminMode {
    /// Pede permissão do Windows (UAC) a cada varredura rápida.
    Prompt,
    /// Usa o serviço do Windows, instalado uma única vez: sem perguntar de novo.
    Service,
    /// Nunca usa administrador: varre pelas pastas (mais lento).
    Never,
}

impl AdminMode {
    pub fn as_str(self) -> &'static str {
        match self {
            AdminMode::Prompt => "prompt",
            AdminMode::Service => "service",
            AdminMode::Never => "never",
        }
    }

    pub fn parse(text: &str) -> AdminMode {
        match text {
            "service" => AdminMode::Service,
            "never" => AdminMode::Never,
            _ => AdminMode::Prompt,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub threads: usize,
    pub skip_hidden: bool,
    pub autolock_minutes: u32,
    /// Gerar miniaturas sozinho depois de cada varredura.
    pub auto_thumbs: bool,
    pub admin_mode: AdminMode,
    pub vault_container: Option<PathBuf>,
    pub vault_letter: char,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            threads: 2,
            skip_hidden: true,
            auto_thumbs: true,
            admin_mode: AdminMode::Prompt,
            autolock_minutes: 5,
            vault_container: None,
            vault_letter: 'V',
        }
    }
}

impl Settings {
    pub fn load(db: &Db) -> Result<Settings> {
        let defaults = Settings::default();
        Ok(Settings {
            threads: read(db, "threads")?.unwrap_or(defaults.threads).clamp(1, 16),
            skip_hidden: read(db, "skip_hidden")?.unwrap_or(defaults.skip_hidden),
            auto_thumbs: read(db, "auto_thumbs")?.unwrap_or(defaults.auto_thumbs),
            admin_mode: db
                .get_setting("admin_mode")?
                .map(|text| AdminMode::parse(&text))
                .unwrap_or(defaults.admin_mode),
            autolock_minutes: read(db, "autolock_minutes")?
                .unwrap_or(defaults.autolock_minutes)
                .clamp(1, 120),
            vault_container: db.get_setting("vault_container")?.map(PathBuf::from),
            vault_letter: db
                .get_setting("vault_letter")?
                .and_then(|text| text.chars().next())
                .filter(char::is_ascii_alphabetic)
                .map(|letter| letter.to_ascii_uppercase())
                .unwrap_or(defaults.vault_letter),
        })
    }

    pub fn scan_options(&self) -> crate::scan::ScanOptions {
        crate::scan::ScanOptions {
            threads: self.threads,
            skip_hidden: self.skip_hidden,
        }
    }

    pub fn save(&self, db: &Db) -> Result<()> {
        db.set_setting("threads", &self.threads.to_string())?;
        db.set_setting("skip_hidden", &self.skip_hidden.to_string())?;
        db.set_setting("auto_thumbs", &self.auto_thumbs.to_string())?;
        db.set_setting("admin_mode", self.admin_mode.as_str())?;
        db.set_setting("autolock_minutes", &self.autolock_minutes.to_string())?;
        db.set_setting("vault_letter", &self.vault_letter.to_string())?;
        if let Some(container) = &self.vault_container {
            db.set_setting("vault_container", &container.to_string_lossy())?;
        }
        Ok(())
    }
}

fn read<T: std::str::FromStr>(db: &Db, key: &str) -> Result<Option<T>> {
    Ok(db.get_setting(key)?.and_then(|text| text.parse().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_nothing_saved() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(Settings::load(&db).unwrap(), Settings::default());
    }

    #[test]
    fn roundtrip_and_clamp() {
        let db = Db::open_in_memory().unwrap();
        let settings = Settings {
            threads: 4,
            skip_hidden: false,
            auto_thumbs: false,
            admin_mode: AdminMode::Service,
            autolock_minutes: 10,
            vault_container: Some(PathBuf::from("E:\\cofre.hc")),
            vault_letter: 'X',
        };
        settings.save(&db).unwrap();
        assert_eq!(Settings::load(&db).unwrap(), settings);

        db.set_setting("threads", "999").unwrap();
        assert_eq!(Settings::load(&db).unwrap().threads, 16);
    }
}
