//! Classificação por extensão e nome, e regras de "arquivo de sistema" que nunca entram no catálogo.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Video,
    Photo,
    Music,
    Document,
    Program,
    Installer,
    Archive,
    Other,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Video => "video",
            Kind::Photo => "photo",
            Kind::Music => "music",
            Kind::Document => "document",
            Kind::Program => "program",
            Kind::Installer => "installer",
            Kind::Archive => "archive",
            Kind::Other => "other",
        }
    }
}

/// Classifica um arquivo pelo nome. `name` é o nome com extensão.
pub fn classify(name: &str) -> Kind {
    let lower = name.to_lowercase();
    let ext = lower.rsplit_once('.').map(|(_, e)| e).unwrap_or("");

    match ext {
        "mp4" | "mkv" | "avi" | "mov" | "wmv" | "webm" | "m4v" | "flv" => Kind::Video,
        "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" | "heic" | "tiff" | "raw" | "psd" => {
            Kind::Photo
        }
        "mp3" | "flac" | "wav" | "ogg" | "m4a" | "aac" | "wma" | "opus" => Kind::Music,
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "epub" | "mobi"
        | "odt" | "csv" | "md" => Kind::Document,
        "zip" | "rar" | "7z" | "tar" | "gz" | "iso" | "bin" | "img" | "cab" => Kind::Archive,
        "exe" | "msi" => {
            if is_installer_name(&lower) {
                Kind::Installer
            } else {
                Kind::Program
            }
        }
        _ => Kind::Other,
    }
}

fn is_installer_name(lower_name: &str) -> bool {
    ["setup", "install", "instalador", "installer"]
        .iter()
        .any(|word| lower_name.contains(word))
}

/// Nomes de pasta que pertencem ao sistema operacional e são ignorados em qualquer profundidade.
const SYSTEM_DIRS: &[&str] = &[
    "windows",
    "program files",
    "program files (x86)",
    "programdata",
    "appdata",
    "$recycle.bin",
    "system volume information",
    "$windows.~bt",
    "$windows.~ws",
    "recovery",
    "msocache",
    "config.msi",
];

pub fn is_system_dir(dir_name: &str) -> bool {
    let lower = dir_name.to_lowercase();
    SYSTEM_DIRS.contains(&lower.as_str())
}

/// Atributos do Windows: oculto (0x2) ou de sistema (0x4).
pub fn has_hidden_or_system_attr(attributes: u32) -> bool {
    attributes & 0x2 != 0 || attributes & 0x4 != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_by_extension() {
        assert_eq!(classify("Filme.MKV"), Kind::Video);
        assert_eq!(classify("foto.jpg"), Kind::Photo);
        assert_eq!(classify("faixa.flac"), Kind::Music);
        assert_eq!(classify("livro.epub"), Kind::Document);
        assert_eq!(classify("backup.7z"), Kind::Archive);
        assert_eq!(classify("sem_extensao"), Kind::Other);
    }

    #[test]
    fn separates_installers_from_programs() {
        assert_eq!(classify("Setup_Photoshop.exe"), Kind::Installer);
        assert_eq!(classify("gta_installer.exe"), Kind::Installer);
        assert_eq!(classify("notepad.exe"), Kind::Program);
    }

    #[test]
    fn detects_system_dirs_case_insensitive() {
        assert!(is_system_dir("Windows"));
        assert!(is_system_dir("$Recycle.Bin"));
        assert!(is_system_dir("PROGRAM FILES (X86)"));
        assert!(!is_system_dir("Jogos"));
    }

    #[test]
    fn detects_hidden_and_system_attributes() {
        assert!(has_hidden_or_system_attr(0x2));
        assert!(has_hidden_or_system_attr(0x4 | 0x20));
        assert!(!has_hidden_or_system_attr(0x20));
    }
}
