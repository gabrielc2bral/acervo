//! Atualização pelo GitHub Releases.
//!
//! O Acervo olha o último release do repositório. Se a versão for mais nova que a instalada, a
//! janela avisa. Nada é baixado nem instalado sem o usuário clicar em "Atualizar agora". Ao clicar:
//! 1. baixa o `Acervo-Setup-X.Y.Z.exe` do release;
//! 2. confere o SHA-256 contra o `Acervo-Setup-X.Y.Z.sha256.txt` do mesmo release (sem esse arquivo
//!    ou com o hash diferente, o instalador é descartado);
//! 3. abre o instalador no modo de atualização, que mantém a pasta, os atalhos e as configurações.
//!
//! Só aceita endereços https dos servidores do GitHub. A verificação automática pode ser desligada
//! em Configurações.

use anyhow::{bail, Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Repositório onde os releases são publicados.
pub const REPO: &str = "gabrielc2bral/acervo";

const ALLOWED_HOSTS: [&str; 4] = [
    "api.github.com",
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// Sem o "v": `1.2.0`.
    pub version: String,
    pub notes: String,
    /// Página do release no GitHub (para "Ver novidades").
    pub page_url: String,
    pub installer_name: String,
    pub installer_url: String,
    pub installer_size: u64,
    pub sha_url: String,
}

/// `1.2.0` ou `v1.2.0` (sufixos como `-beta` são ignorados) viram (1, 2, 0).
pub fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let core = text.trim().trim_start_matches('v').split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let version = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    parts.next().is_none().then_some(version)
}

/// `candidate` é uma versão válida e mais nova que `current`.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(candidate), Some(current)) => candidate > current,
        _ => false,
    }
}

/// Só https e só os servidores do GitHub (sem usuário@ no endereço).
pub fn is_allowed_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else { return false };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return false;
    }
    let host = authority.split(':').next().unwrap_or("");
    ALLOWED_HOSTS.contains(&host)
}

/// Interpreta a resposta de `releases/latest`. `Ok(None)` = nada de novo (ou rascunho/pré-lançamento).
pub fn parse_release(release: &Value, current: &str) -> Result<Option<Release>> {
    if release["draft"].as_bool() == Some(true) || release["prerelease"].as_bool() == Some(true) {
        return Ok(None);
    }
    let tag = release["tag_name"].as_str().context("o release não tem tag_name")?;
    if !is_newer(tag, current) {
        return Ok(None);
    }
    let version = tag.trim_start_matches('v').to_string();
    let installer_name = format!("Acervo-Setup-{version}.exe");
    let sha_name = format!("Acervo-Setup-{version}.sha256.txt");

    let assets = release["assets"].as_array().context("o release não tem arquivos")?;
    let find = |name: &str| -> Option<(&str, u64)> {
        assets.iter().find(|asset| asset["name"].as_str() == Some(name)).and_then(|asset| {
            Some((asset["browser_download_url"].as_str()?, asset["size"].as_u64().unwrap_or(0)))
        })
    };
    let (installer_url, installer_size) =
        find(&installer_name).with_context(|| format!("o release {version} não tem o instalador {installer_name}"))?;
    let (sha_url, _) =
        find(&sha_name).with_context(|| format!("o release {version} não tem o arquivo de SHA-256 {sha_name}"))?;
    for url in [installer_url, sha_url] {
        if !is_allowed_url(url) {
            bail!("endereço de download não permitido: {url}");
        }
    }
    Ok(Some(Release {
        version,
        notes: release["body"].as_str().unwrap_or("").trim().to_string(),
        page_url: release["html_url"].as_str().unwrap_or("").to_string(),
        installer_name,
        installer_url: installer_url.to_string(),
        installer_size,
        sha_url: sha_url.to_string(),
    }))
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(30))
        .user_agent(&format!("Acervo/{}", crate::version::VERSION))
        .build()
}

/// Pergunta ao GitHub qual é o último release. `Ok(None)` = você já está na última versão
/// (ou o repositório ainda não tem releases).
pub fn check_latest(repo: &str, current: &str) -> Result<Option<Release>> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    match agent().get(&url).set("Accept", "application/vnd.github+json").call() {
        Ok(response) => {
            let json: Value = response.into_json().context("resposta do GitHub inválida")?;
            parse_release(&json, current)
        }
        Err(ureq::Error::Status(404, _)) => Ok(None),
        Err(ureq::Error::Status(403, _)) | Err(ureq::Error::Status(429, _)) => {
            bail!("O GitHub limitou as consultas por enquanto. Tente de novo daqui a pouco.")
        }
        Err(error) => bail!("Não foi possível falar com o GitHub: {error}"),
    }
}

/// O hash de `file_name` num arquivo `.sha256.txt` (formato "hash  nome", ou só o hash).
pub fn parse_sha_file(text: &str, file_name: &str) -> Option<String> {
    let is_hash = |token: &str| token.len() == 64 && token.chars().all(|c| c.is_ascii_hexdigit());
    for line in text.lines() {
        let mut tokens = line.split_whitespace();
        let (Some(hash), name) = (tokens.next(), tokens.next()) else { continue };
        if is_hash(hash) && name.map_or(true, |name| name.trim_start_matches('*') == file_name) {
            return Some(hash.to_lowercase());
        }
    }
    None
}

/// Copia `reader` para `target` calculando o SHA-256 no caminho. Só deixa o arquivo final se o
/// hash for o esperado. `progress(bytes_lidos, total)` é chamado de tempos em tempos.
pub fn copy_verified(
    reader: &mut dyn Read,
    target: &Path,
    expected_sha256: &str,
    total: u64,
    progress: &dyn Fn(u64, u64),
    cancel: &AtomicBool,
) -> Result<()> {
    let partial = target.with_extension("part");
    let result = (|| -> Result<()> {
        let mut out = std::io::BufWriter::new(std::fs::File::create(&partial)?);
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; 64 * 1024];
        let mut done = 0u64;
        loop {
            if cancel.load(Ordering::Relaxed) {
                bail!("Download cancelado.");
            }
            let read = reader.read(&mut buffer).context("o download foi interrompido")?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            out.write_all(&buffer[..read])?;
            done += read as u64;
            progress(done, total);
        }
        out.flush()?;
        let actual: String = hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect();
        if actual != expected_sha256.to_lowercase() {
            bail!("O SHA-256 do instalador não confere com o do release. O arquivo foi descartado por segurança.");
        }
        Ok(())
    })();
    match result {
        Ok(()) => {
            std::fs::rename(&partial, target)?;
            Ok(())
        }
        Err(error) => {
            let _ = std::fs::remove_file(&partial);
            Err(error)
        }
    }
}

/// Baixa o instalador do release para `dir` e confere o SHA-256. Devolve o caminho do instalador.
pub fn download(release: &Release, dir: &Path, progress: &dyn Fn(u64, u64), cancel: &AtomicBool) -> Result<PathBuf> {
    for url in [&release.installer_url, &release.sha_url] {
        if !is_allowed_url(url) {
            bail!("endereço de download não permitido: {url}");
        }
    }
    std::fs::create_dir_all(dir)?;
    let client = agent();

    let sha_text = client
        .get(&release.sha_url)
        .call()
        .context("não foi possível baixar o arquivo de SHA-256")?
        .into_string()?;
    let expected = parse_sha_file(&sha_text, &release.installer_name)
        .context("o arquivo de SHA-256 do release não tem o hash do instalador")?;

    let response = client
        .get(&release.installer_url)
        .call()
        .context("não foi possível baixar o instalador")?;
    let total = response
        .header("Content-Length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(release.installer_size);
    let target = dir.join(&release.installer_name);
    copy_verified(&mut response.into_reader(), &target, &expected, total, progress, cancel)?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn release_json(tag: &str) -> Value {
        let v = tag.trim_start_matches('v');
        json!({
            "tag_name": tag,
            "html_url": format!("https://github.com/gabrielc2bral/acervo/releases/tag/{tag}"),
            "body": "  novidades  ",
            "draft": false,
            "prerelease": false,
            "assets": [
                { "name": format!("Acervo-Setup-{v}.exe"), "size": 1234,
                  "browser_download_url": format!("https://github.com/gabrielc2bral/acervo/releases/download/{tag}/Acervo-Setup-{v}.exe") },
                { "name": format!("Acervo-Setup-{v}.sha256.txt"), "size": 90,
                  "browser_download_url": format!("https://github.com/gabrielc2bral/acervo/releases/download/{tag}/Acervo-Setup-{v}.sha256.txt") }
            ]
        })
    }

    #[test]
    fn compares_versions_numerically_not_as_text() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.10.0-beta.1"), Some((1, 10, 0)));
        assert_eq!(parse_version("1.2"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
        assert_eq!(parse_version("abc"), None);
        assert!(is_newer("1.10.0", "1.9.0"));
        assert!(is_newer("v2.0.0", "1.99.99"));
        assert!(!is_newer("1.1.0", "1.1.0"));
        assert!(!is_newer("1.0.9", "1.1.0"));
        assert!(!is_newer("lixo", "1.1.0"));
    }

    #[test]
    fn only_https_github_hosts_are_allowed() {
        assert!(is_allowed_url("https://github.com/a/b/releases/download/v1/x.exe"));
        assert!(is_allowed_url("https://objects.githubusercontent.com/abc?x=1"));
        assert!(!is_allowed_url("http://github.com/a/b/x.exe"));
        assert!(!is_allowed_url("https://github.com.evil.com/x.exe"));
        assert!(!is_allowed_url("https://evil.com/github.com/x.exe"));
        assert!(!is_allowed_url("https://github.com@evil.com/x.exe"));
        assert!(!is_allowed_url("ftp://github.com/x"));
        assert!(!is_allowed_url("github.com/x"));
    }

    #[test]
    fn finds_the_new_release_with_installer_and_hash() {
        let release = parse_release(&release_json("v1.2.0"), "1.1.0").unwrap().unwrap();
        assert_eq!(release.version, "1.2.0");
        assert_eq!(release.installer_name, "Acervo-Setup-1.2.0.exe");
        assert_eq!(release.notes, "novidades");
        assert!(release.installer_url.ends_with("Acervo-Setup-1.2.0.exe"));
        assert!(release.sha_url.ends_with("Acervo-Setup-1.2.0.sha256.txt"));
        assert_eq!(release.installer_size, 1234);
    }

    #[test]
    fn nothing_to_do_when_up_to_date_or_draft_or_prerelease() {
        assert!(parse_release(&release_json("v1.1.0"), "1.1.0").unwrap().is_none());
        assert!(parse_release(&release_json("v1.0.0"), "1.1.0").unwrap().is_none());
        let mut draft = release_json("v9.0.0");
        draft["draft"] = json!(true);
        assert!(parse_release(&draft, "1.1.0").unwrap().is_none());
        let mut pre = release_json("v9.0.0");
        pre["prerelease"] = json!(true);
        assert!(parse_release(&pre, "1.1.0").unwrap().is_none());
    }

    #[test]
    fn a_release_without_installer_hash_or_with_foreign_urls_is_an_error() {
        let mut no_sha = release_json("v1.2.0");
        no_sha["assets"].as_array_mut().unwrap().pop();
        assert!(parse_release(&no_sha, "1.1.0").is_err());

        let mut foreign = release_json("v1.2.0");
        foreign["assets"][0]["browser_download_url"] = json!("https://evil.example.com/Acervo-Setup-1.2.0.exe");
        assert!(parse_release(&foreign, "1.1.0").is_err());

        let mut empty = release_json("v1.2.0");
        empty["assets"] = json!([]);
        assert!(parse_release(&empty, "1.1.0").is_err());
    }

    #[test]
    fn reads_the_hash_for_the_right_file() {
        let hash = "a".repeat(64);
        let other = "b".repeat(64);
        let text = format!("{other}  Outro.exe\n{hash}  Acervo-Setup-1.2.0.exe\n");
        assert_eq!(parse_sha_file(&text, "Acervo-Setup-1.2.0.exe"), Some(hash.clone()));
        assert_eq!(parse_sha_file(&format!("{hash}\n"), "qualquer.exe"), Some(hash.clone()));
        assert_eq!(parse_sha_file(&format!("{hash} *Acervo-Setup-1.2.0.exe"), "Acervo-Setup-1.2.0.exe"), Some(hash));
        assert_eq!(parse_sha_file("lixo", "x.exe"), None);
        assert_eq!(parse_sha_file(&format!("{other}  Outro.exe"), "Acervo-Setup-1.2.0.exe"), None);
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn keeps_the_download_only_when_the_hash_matches() {
        let dir = tempfile::tempdir().unwrap();
        let content = vec![7u8; 300_000];
        let target = dir.path().join("Acervo-Setup-1.2.0.exe");
        let progress_calls = std::cell::Cell::new(0u32);

        copy_verified(
            &mut &content[..],
            &target,
            &sha256_hex(&content),
            content.len() as u64,
            &|_, _| progress_calls.set(progress_calls.get() + 1),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), content);
        assert!(progress_calls.get() > 0);
        assert!(!dir.path().join("Acervo-Setup-1.2.0.part").exists());

        // hash errado: o arquivo não fica, nem o parcial
        let bad = dir.path().join("Acervo-Setup-9.9.9.exe");
        let error = copy_verified(&mut &content[..], &bad, &"0".repeat(64), 0, &|_, _| {}, &AtomicBool::new(false))
            .unwrap_err()
            .to_string();
        assert!(error.contains("SHA-256"), "{error}");
        assert!(!bad.exists() && !dir.path().join("Acervo-Setup-9.9.9.part").exists());
    }

    #[test]
    fn download_can_be_cancelled_and_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let content = vec![1u8; 100_000];
        let target = dir.path().join("x.exe");
        let error = copy_verified(&mut &content[..], &target, &sha256_hex(&content), 0, &|_, _| {}, &AtomicBool::new(true))
            .unwrap_err()
            .to_string();
        assert!(error.contains("cancelado"), "{error}");
        assert!(!target.exists() && !dir.path().join("x.part").exists());
    }
}
