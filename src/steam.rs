//! Lê as bibliotecas da Steam direto dos arquivos dela (sem login e sem internet).

use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub struct SteamGame {
    pub app_id: u64,
    pub name: String,
    pub size_bytes: u64,
    pub library: PathBuf,
}

/// Separa uma linha VDF/ACF no formato `"chave"  "valor"`.
fn parse_pair(line: &str) -> Option<(String, String)> {
    let mut parts = line.split('"');
    parts.next()?; // antes da primeira aspa
    let key = parts.next()?;
    parts.next()?; // espaço entre as duas strings
    let value = parts.next()?;
    Some((key.to_string(), value.replace("\\\\", "\\")))
}

/// Caminhos das bibliotecas listadas em `libraryfolders.vdf`.
pub fn parse_library_folders(vdf: &str) -> Vec<PathBuf> {
    vdf.lines()
        .filter_map(parse_pair)
        .filter(|(key, _)| key == "path")
        .map(|(_, value)| PathBuf::from(value))
        .collect()
}

/// Lê um `appmanifest_*.acf`. Devolve `None` se faltar appid ou nome.
pub fn parse_app_manifest(acf: &str, library: &Path) -> Option<SteamGame> {
    let mut app_id = None;
    let mut name = None;
    let mut size = 0u64;
    for (key, value) in acf.lines().filter_map(parse_pair) {
        match key.as_str() {
            "appid" => app_id = value.parse().ok(),
            "name" => name = Some(value),
            "SizeOnDisk" => size = value.parse().unwrap_or(0),
            _ => {}
        }
    }
    Some(SteamGame {
        app_id: app_id?,
        name: name?,
        size_bytes: size,
        library: library.to_path_buf(),
    })
}

#[cfg(windows)]
fn steam_root() -> Option<PathBuf> {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};
    let key = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey("Software\\Valve\\Steam")
        .ok()?;
    let path: String = key.get_value("SteamPath").ok()?;
    Some(PathBuf::from(path))
}

#[cfg(not(windows))]
fn steam_root() -> Option<PathBuf> {
    None
}

/// Capa do jogo que a própria Steam já guardou no PC (sem internet). Prefere a capa vertical.
pub fn cover_path(app_id: u64) -> Option<PathBuf> {
    let cache = steam_root()?.join("appcache").join("librarycache");
    [
        cache.join(app_id.to_string()).join("library_600x900.jpg"),
        cache.join(format!("{app_id}_library_600x900.jpg")),
        cache.join(app_id.to_string()).join("header.jpg"),
        cache.join(format!("{app_id}_header.jpg")),
    ]
    .into_iter()
    .find(|path| path.is_file())
}

/// Todos os jogos instalados em todas as bibliotecas da Steam.
pub fn installed_games() -> Vec<SteamGame> {
    let Some(root) = steam_root() else {
        return Vec::new();
    };
    let vdf_path = root.join("steamapps").join("libraryfolders.vdf");
    let mut libraries = std::fs::read_to_string(&vdf_path)
        .map(|text| parse_library_folders(&text))
        .unwrap_or_default();
    if !libraries.contains(&root) {
        libraries.push(root);
    }

    let mut games = Vec::new();
    for library in libraries {
        let Ok(entries) = std::fs::read_dir(library.join("steamapps")) else {
            continue; // biblioteca em HD desconectado
        };
        for entry in entries.flatten() {
            let file_name = entry.file_name().to_string_lossy().into_owned();
            if !(file_name.starts_with("appmanifest_") && file_name.ends_with(".acf")) {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(entry.path()) {
                games.extend(parse_app_manifest(&text, &library));
            }
        }
    }
    games.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    games
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_library_paths() {
        let vdf = r#""libraryfolders"
{
    "0"
    {
        "path"		"C:\\Program Files (x86)\\Steam"
    }
    "1"
    {
        "path"		"D:\\SteamLibrary"
    }
}"#;
        let libs = parse_library_folders(vdf);
        assert_eq!(libs.len(), 2);
        assert_eq!(libs[1], PathBuf::from("D:\\SteamLibrary"));
    }

    #[test]
    fn parses_app_manifest() {
        let acf = r#""AppState"
{
    "appid"		"1086940"
    "name"		"Baldur's Gate 3"
    "SizeOnDisk"		"152000000000"
}"#;
        let game = parse_app_manifest(acf, Path::new("D:\\SteamLibrary")).unwrap();
        assert_eq!(game.app_id, 1086940);
        assert_eq!(game.name, "Baldur's Gate 3");
        assert_eq!(game.size_bytes, 152_000_000_000);
    }

    #[test]
    fn manifest_without_name_is_rejected() {
        assert!(parse_app_manifest("\"appid\"\t\"1\"", Path::new("D:\\")).is_none());
    }
}
