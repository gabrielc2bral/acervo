//! Linha de comando do Acervo: scan <pasta-ou-hd> | search <texto> | stats | steam

use acervo::{
    db::Db,
    scan::{self, Outcome},
    settings::Settings,
    steam,
};
use anyhow::{bail, Result};
use std::path::Path;
use std::sync::atomic::AtomicBool;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["scan", path] => scan_path(Path::new(path)),
        ["search", text] => search(text),
        ["--version"] | ["version"] => {
            println!("{}", acervo::version::describe());
            Ok(())
        }
        // Teste real do sistema de atualização: finge ser a versão <atual> e olha o GitHub.
        // Com "baixar", também baixa o instalador e confere o SHA-256 (sem instalar nada).
        ["update-check", current] => update_check(current, false),
        ["update-check", current, "baixar"] => update_check(current, true),
        ["service-info"] => service_info(),
        ["stats"] => stats(),
        ["thumbs-report"] => thumbs_report(),
        ["thumbs-generate"] => thumbs_generate(),
        ["thumbs-same"] => thumbs_same(),
        ["index-bench"] => index_bench(),
        ["thumbs-bench", kind, count] => thumbs_bench(kind, count.parse()?, 4),
        ["thumbs-bench", kind, count, threads] => thumbs_bench(kind, count.parse()?, threads.parse()?),
        ["thumbs-purge"] => {
            println!("{} miniaturas genéricas apagadas", Db::open(&Db::default_path()?)?.purge_generic_thumbs()?);
            Ok(())
        }
        ["list", sort, offset, text] => list_page(sort, offset.parse()?, text),
        ["thumb", file, out] => thumb(Path::new(file), Path::new(out), acervo::thumbs::Mode::WindowsFull),
        ["thumb-cache", file, out] => thumb(Path::new(file), Path::new(out), acervo::thumbs::Mode::WindowsCache),
        ["reclassify"] => reclassify(),
        ["steam"] => list_steam(),
        _ => bail!("uso: acervo-cli scan <pasta-ou-hd> | search <texto> | stats | steam"),
    }
}

fn scan_path(path: &Path) -> Result<()> {
    let mut db = Db::open(&Db::default_path()?)?;
    let options = Settings::load(&db)?.scan_options();
    let name = path.to_string_lossy().into_owned();
    let source_id = db.add_source(&name, &name)?;
    let cancel = AtomicBool::new(false);
    let mut report = |p: &scan::Progress| {
        eprintln!("{} arquivos, {:.1} GB", p.files, p.bytes as f64 / 1e9);
    };
    let outcome = match scan::scan_source(&mut db, source_id, path, &options, &cancel, &mut report)? {
        Outcome::NeedsAdmin => {
            eprintln!("Varredura rápida precisa de administrador; usando o modo normal.");
            scan::scan_walk(&mut db, source_id, path, &options, &cancel, &mut report)?
        }
        other => other,
    };
    match outcome {
        Outcome::Done(p) => println!("Pronto: {} arquivos catalogados.", p.files),
        _ => println!("Cancelado."),
    }
    Ok(())
}

fn search(text: &str) -> Result<()> {
    let db = Db::open(&Db::default_path()?)?;
    for row in db.search(text, None, 50)? {
        println!("[{}] {} ({} bytes) — {}", row.kind, row.path, row.size, row.source_name);
    }
    Ok(())
}

/// Mede a velocidade real: gera até `count` miniaturas que faltam de um tipo, em 4 threads,
/// e diz quantas o Windows conseguiu e quanto tempo levou por arquivo.
fn thumbs_bench(kind: &str, count: usize, threads: usize) -> Result<()> {
    use acervo::thumbs::{can_have_thumbnail, key_for, thumb_for, Mode};
    use std::sync::atomic::{AtomicUsize, Ordering};
    let db = Db::open(&Db::default_path()?)?;
    let online: Vec<i64> = db
        .sources()?
        .into_iter()
        .filter(|source| Path::new(&source.path).exists())
        .map(|source| source.id)
        .collect();
    let mut todo: Vec<(std::path::PathBuf, String)> = db
        .files_for_thumbs(&online, &[kind])?
        .into_iter()
        .filter(|(path, kind)| can_have_thumbnail(path, kind))
        .filter(|(path, _)| !db.has_thumb(key_for(&path.to_string_lossy())).unwrap_or(true))
        .collect();
    todo.truncate(count);
    let (made, none) = (AtomicUsize::new(0), AtomicUsize::new(0));
    let next = AtomicUsize::new(0);
    let started = std::time::Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let Ok(db) = Db::open(&Db::default_path().unwrap()) else { return };
                while let Some((path, kind)) = todo.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let counter = if thumb_for(&db, path, kind, Mode::WindowsFull).is_some() { &made } else { &none };
                    counter.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
    });
    let elapsed = started.elapsed();
    let total = todo.len().max(1);
    println!(
        "{kind}: {} arquivos em {:.1?} ({:.1} ms por arquivo, {threads} threads) -> {} com miniatura, {} sem",
        todo.len(),
        elapsed,
        elapsed.as_millis() as f64 / total as f64,
        made.load(Ordering::Relaxed),
        none.load(Ordering::Relaxed)
    );
    Ok(())
}

/// Diagnóstico do serviço do Windows: estado no Windows, sinal de vida, tarefas e posições do USN.
fn service_info() -> Result<()> {
    let db_path = Db::default_path()?;
    let db = Db::open(&db_path)?;
    let link = acervo::link::ServiceLink::open(&db_path)?;
    println!("estado no Windows: {:?}", acervo::service::installed_state());
    match link.seconds_since_beat() {
        None => println!("sinal de vida: nunca (ou o serviço parou normalmente)"),
        Some(seconds) => println!("sinal de vida: há {seconds} s ({})", if link.is_alive() { "ativo" } else { "parado" }),
    }
    println!("modo de administrador: {:?}", db.get_setting("admin_mode")?);
    let jobs = link.recent_jobs(10)?;
    println!("tarefas do serviço (últimas {}):", jobs.len());
    for job in jobs {
        println!("  #{} {} fonte {}: {} - {}", job.id, job.kind, job.source_id, job.state, job.progress);
    }
    println!("registro ({}), últimas linhas:", acervo::log::path().display());
    println!("{}", acervo::log::tail(15));
    let mut stmt = db.conn.prepare("SELECT key, value FROM settings WHERE key LIKE 'usn%' ORDER BY key")?;
    let positions: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;
    println!("diário USN ({} entradas):", positions.len());
    for (key, value) in positions {
        println!("  {key} = {value}");
    }
    Ok(())
}

fn update_check(current: &str, download: bool) -> Result<()> {
    use acervo::update;
    let Some(release) = update::check_latest(update::REPO, current)? else {
        println!("Nenhuma versão mais nova que {current}.");
        return Ok(());
    };
    println!("Versão nova: {} ({} bytes)  {}", release.version, release.installer_size, release.page_url);
    println!("Instalador:  {}", release.installer_url);
    if download {
        let dir = std::env::temp_dir().join("Acervo-teste-atualizacao");
        let started = std::time::Instant::now();
        let last = std::sync::atomic::AtomicU64::new(0);
        let path = update::download(
            &release,
            &dir,
            &|done, total| {
                let step = done / (8 * 1024 * 1024);
                if step != last.swap(step, std::sync::atomic::Ordering::Relaxed) {
                    eprintln!("  {} de {} MB", done / 1_048_576, total / 1_048_576);
                }
            },
            &AtomicBool::new(false),
        )?;
        println!(
            "Baixado e conferido (SHA-256 ok) em {:.1?}: {} ({} bytes)",
            started.elapsed(),
            path.display(),
            std::fs::metadata(&path)?.len()
        );
    }
    Ok(())
}

/// Compara a busca em memória com a busca no SQLite, no catálogo real.
fn index_bench() -> Result<()> {
    use acervo::db::{SearchQuery, Sort};
    use acervo::index::Index;
    let db = Db::open(&Db::default_path()?)?;
    let started = std::time::Instant::now();
    let index = Index::build(&db)?;
    println!("índice: {} arquivos carregados em {:.0?}", index.len(), started.elapsed());
    for (text, sort) in [("", Sort::Name), ("the", Sort::Name), ("dexter", Sort::Name), ("mkv", Sort::Largest), ("zzzqx", Sort::Name), ("a", Sort::Newest)] {
        let query = SearchQuery { text: text.to_string(), sort, limit: 100, ..SearchQuery::default() };
        let t = std::time::Instant::now();
        let found = index.search(&query);
        let memory = t.elapsed();
        let t = std::time::Instant::now();
        let rows = db.search_query(&query)?;
        let sql = t.elapsed();
        println!(
            "{:>8} {:?}: memória {:>7.2?} ({} no total) | SQLite {:>7.2?} ({} linhas)",
            format!("\"{text}\""),
            sort,
            memory,
            found.total,
            sql,
            rows.len()
        );
    }
    Ok(())
}

/// Diagnóstico: miniaturas idênticas (ícone genérico repetido) e que arquivos são.
fn thumbs_same() -> Result<()> {
    let db = Db::open(&Db::default_path()?)?;
    let mut stmt = db.conn.prepare(
        "SELECT COUNT(*) AS n, LENGTH(data), MIN(key) FROM thumbs WHERE LENGTH(data) > 0
         GROUP BY data HAVING n > 5 ORDER BY n DESC LIMIT 8",
    )?;
    let groups: Vec<(i64, i64, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<std::result::Result<_, _>>()?;
    for (count, bytes, key) in groups {
        println!("{count} miniaturas idênticas ({bytes} bytes cada), por exemplo:");
        let mut samples = db.conn.prepare(
            "SELECT f.kind, f.dir || '\\' || f.name FROM files f
             JOIN thumbs t ON t.key = acervo_key(RTRIM(f.dir, '\\') || '\\' || f.name)
             WHERE t.data = (SELECT data FROM thumbs WHERE key = ?1) LIMIT 4",
        )?;
        let rows = samples.query_map([key], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (kind, path) = row?;
            println!("   [{kind}] {path}");
        }
    }
    Ok(())
}

/// Gera as miniaturas que faltam (o mesmo do botão "Gerar miniaturas" da janela).
fn thumbs_generate() -> Result<()> {
    use acervo::thumbs::{generate_missing, Mode, BATCH_KINDS};
    let started = std::time::Instant::now();
    let cancel = AtomicBool::new(false);
    let report = generate_missing(
        &Db::default_path()?,
        &[],
        BATCH_KINDS,
        Mode::WindowsFull,
        4,
        &cancel,
        &|done, total| {
            if done % 500 == 0 || done == total {
                eprintln!("{done} de {total} ({:.0?})", started.elapsed());
            }
        },
    )?;
    println!(
        "novas: {}, já existiam: {}, sem miniatura: {}, tempo: {:.0?}",
        report.generated, report.already_stored, report.without_thumbnail, started.elapsed()
    );
    Ok(())
}

/// Diagnóstico: por tipo, quantas miniaturas existem, quantas estão marcadas "sem miniatura"
/// e quantas faltam (amostra de até 400 arquivos conectados por tipo).
fn thumbs_report() -> Result<()> {
    use acervo::thumbs::{key_for, Mode};
    let db = Db::open(&Db::default_path()?)?;
    let online: Vec<i64> = db
        .sources()?
        .into_iter()
        .filter(|source| Path::new(&source.path).exists())
        .map(|source| source.id)
        .collect();
    println!("fontes conectadas: {}", online.len());
    let (count, bytes) = db.thumb_stats()?;
    println!("miniaturas no banco: {count} ({:.1} MB)", bytes as f64 / 1e6);
    for kind in ["photo", "video", "installer", "program"] {
        let files = db.files_for_thumbs(&online, &[kind])?;
        let (mut with, mut marked, mut missing) = (0, 0, 0);
        let mut marked_samples = Vec::new();
        for (path, _) in files.iter().step_by((files.len() / 400).max(1)) {
            match db.get_thumb(key_for(&path.to_string_lossy()))? {
                Some(data) if !data.is_empty() => with += 1,
                Some(_) => {
                    marked += 1;
                    if marked_samples.len() < 3 {
                        marked_samples.push(path.clone());
                    }
                }
                None => missing += 1,
            }
        }
        println!("{kind}: {} arquivos; amostra -> com miniatura {with}, marcados sem miniatura {marked}, faltando {missing}", files.len());
        for path in marked_samples {
            let scratch = Db::open_in_memory()?;
            let retry = acervo::thumbs::thumb_for(&scratch, &path, kind, Mode::WindowsFull).is_some();
            println!("   marcado: {} -> agora {}", path.display(), if retry { "CONSEGUE gerar" } else { "continua sem" });
        }
    }
    Ok(())
}

/// Teste manual de velocidade: uma página da Biblioteca (100 itens) e o tempo que levou.
fn list_page(sort: &str, offset: usize, text: &str) -> Result<()> {
    use acervo::db::{SearchQuery, Sort};
    let db = Db::open(&Db::default_path()?)?;
    let sort = match sort {
        "size" => Sort::Largest,
        "new" => Sort::Newest,
        _ => Sort::Name,
    };
    let query = SearchQuery {
        text: if text == "-" { String::new() } else { text.to_string() },
        sort,
        limit: 100,
        offset,
        ..SearchQuery::default()
    };
    let started = std::time::Instant::now();
    let rows = db.search_query(&query)?;
    let page_time = started.elapsed();
    let started = std::time::Instant::now();
    let total = db.count_query(&query)?;
    println!(
        "pagina: {} linhas em {:?} | contagem: {} em {:?}",
        rows.len(),
        page_time,
        total,
        started.elapsed()
    );
    Ok(())
}

/// Teste manual: gera a miniatura de um arquivo (como o app faz) e salva em PNG.
fn thumb(file: &Path, out: &Path, mode: acervo::thumbs::Mode) -> Result<()> {
    let db = Db::open_in_memory()?;
    let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let kind = acervo::kind::classify(&name).as_str();
    let started = std::time::Instant::now();
    match acervo::thumbs::thumb_for(&db, file, kind, mode) {
        Some(thumb) => {
            let (_, stored) = db.thumb_stats()?;
            image_save(&thumb, out)?;
            println!(
                "{kind}: {}x{} em {:?}, {} bytes guardados no banco",
                thumb.width,
                thumb.height,
                started.elapsed(),
                stored
            );
        }
        None => println!("{kind}: sem miniatura"),
    }
    Ok(())
}

fn image_save(thumb: &acervo::thumbs::Thumb, out: &Path) -> Result<()> {
    let image = image::RgbaImage::from_raw(thumb.width, thumb.height, thumb.rgba.clone())
        .ok_or_else(|| anyhow::anyhow!("imagem inválida"))?;
    image.save(out)?;
    Ok(())
}

/// Aplica as regras de classificação atuais ao catálogo já gravado (não varre os HDs).
fn reclassify() -> Result<()> {
    let mut db = Db::open(&Db::default_path()?)?;
    scan::reclassify(&mut db)?;
    for folder in db.repack_folders()? {
        println!("{} ({} arquivos) — {}", folder.dir, folder.files, folder.source_name);
    }
    Ok(())
}

fn stats() -> Result<()> {
    let db = Db::open(&Db::default_path()?)?;
    for (kind, count, bytes) in db.stats_by_kind()? {
        println!("{kind:<10} {count:>10} arquivos  {:>8.1} GB", bytes as f64 / 1e9);
    }
    Ok(())
}

fn list_steam() -> Result<()> {
    for game in steam::installed_games() {
        println!(
            "{} (AppID {}) — {:.1} GB — {}",
            game.name,
            game.app_id,
            game.size_bytes as f64 / 1e9,
            game.library.display()
        );
    }
    Ok(())
}
