//! Versões do Acervo (seguem o versionamento semântico: MAJOR.MINOR.PATCH).
//!
//! - **Programa** (`VERSION`): vem do `Cargo.toml`, uma única fonte para o app, o serviço, o
//!   instalador e o "Aplicativos e recursos" do Windows.
//! - **Catálogo** (`SCHEMA_VERSION`): número do formato do banco SQLite, guardado dentro do próprio
//!   banco (`PRAGMA user_version`). Um catálogo de versão mais nova que o programa não é aberto,
//!   para um programa antigo nunca estragar dados que ele não entende.
//! - **Índice salvo** (`index::FORMAT`): formato do arquivo `index.bin`; se mudar, ele é refeito.
//!
//! Quando subir `SCHEMA_VERSION`, acrescente o passo em `db::migrate` e anote no CHANGELOG.md.

/// Versão do programa, a mesma do `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Versão atual do formato do catálogo.
///  1: fontes, arquivos, configurações, miniaturas
///  2: número do arquivo no NTFS (`frn`), registro de mudanças, tarefas do serviço
pub const SCHEMA_VERSION: i64 = 2;

/// Texto para mostrar ao usuário.
pub fn describe() -> String {
    format!("Acervo {VERSION} (catálogo v{SCHEMA_VERSION})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_semver() {
        let parts: Vec<&str> = VERSION.split('.').collect();
        assert_eq!(parts.len(), 3, "esperava MAJOR.MINOR.PATCH, veio {VERSION}");
        assert!(parts.iter().all(|part| part.parse::<u32>().is_ok()));
    }

    #[test]
    fn describes_both_versions() {
        assert!(describe().contains(VERSION));
        assert!(describe().contains(&format!("v{SCHEMA_VERSION}")));
    }
}
