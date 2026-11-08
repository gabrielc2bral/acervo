//! Miniaturas pequenas guardadas dentro do próprio SQLite (tabela `thumbs`).
//!
//! De onde vem cada miniatura (da mais rápida para a mais lenta):
//! 1. Do SQLite do Acervo: instantâneo e funciona com o HD desconectado.
//! 2. Do cache de miniaturas do Windows (o mesmo do Explorer), sem gerar nada: `Mode::WindowsCache`.
//!    É o que as telas usam ao listar, então a lista nunca espera uma extração.
//! 3. Gerada pelo Windows (vídeo, PDF, ícone de programa) ou pelo Acervo (fotos): `Mode::WindowsFull`,
//!    só no "Gerar miniaturas" e depois de varrer.
//! Tudo que vem de 2 e 3 é copiado para o SQLite (JPEG/PNG de 2 a 4 KB).
//!
//! No Cofre (`Mode::DecodeOnly`) o Windows não é consultado, porque ele guardaria cópias das
//! miniaturas fora do volume VeraCrypt: só fotos, decodificadas pelo Acervo, no banco do Cofre.

use crate::db::Db;
use image::{DynamicImage, ImageEncoder, RgbaImage};
use std::path::Path;

/// Lado maior da miniatura guardada. Pequeno de propósito: cada uma ocupa 2 a 4 KB
/// (10 mil miniaturas ≈ 30 MB), e ainda dá para reconhecer o conteúdo.
pub const STORED_SIDE: u32 = 128;
/// Não decodifica fotos maiores que isso (evita travar com arquivos enormes).
const MAX_IMAGE_BYTES: u64 = 150 * 1024 * 1024;

pub struct Thumb {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Só pega o que o Windows já tem no cache dele. Rápido; não gera nada.
    WindowsCache,
    /// Pede ao Windows para gerar (e, para fotos, decodifica se ele não conseguir).
    WindowsFull,
    /// Só fotos, decodificadas pelo Acervo, sem consultar o Windows (Cofre).
    DecodeOnly,
}

/// Chave estável do caminho (FNV-1a de 64 bits, sem diferenciar maiúsculas).
pub fn key_for(text: &str) -> i64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in text.to_lowercase().bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash as i64
}

fn is_photo_ext(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| matches!(ext.to_lowercase().as_str(), "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp"))
        .unwrap_or(false)
}

/// Tipos para os quais vale pedir miniatura ao Windows.
fn shell_kinds(kind: &str) -> bool {
    matches!(kind, "photo" | "video" | "document" | "program" | "installer")
}

/// Só vale tentar arquivos de formatos que podem ter miniatura de verdade (foto, vídeo, PDF e
/// Office) ou ícone próprio (exe/msi). Todo o resto (dll, zip, txt, .bin...) é pulado na hora,
/// sem consultar o disco nem o Windows. É isso que deixa a geração em lote rápida.
pub fn can_have_thumbnail(path: &Path, kind: &str) -> bool {
    if !shell_kinds(kind) {
        return false;
    }
    let Some(ext) = path.extension().and_then(|ext| ext.to_str()) else {
        return false;
    };
    let ext = ext.to_lowercase();
    match kind {
        "photo" => matches!(
            ext.as_str(),
            "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" | "tif" | "tiff" | "heic" | "heif" | "avif" | "ico"
        ),
        "video" => matches!(
            ext.as_str(),
            "mp4" | "mkv" | "avi" | "mov" | "wmv" | "webm" | "m4v" | "flv" | "mpg" | "mpeg" | "ts" | "m2ts" | "3gp"
        ),
        "document" => matches!(ext.as_str(), "pdf" | "docx" | "xlsx" | "pptx"),
        "program" | "installer" => matches!(ext.as_str(), "exe" | "msi"),
        _ => false,
    }
}

/// O que pedir ao Windows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fetch {
    /// Só o que já está no cache do Windows (rápido, não gera nada).
    CacheOnly,
    /// Só miniatura de verdade do conteúdo: se o Windows não souber gerar, falha (sem ícone genérico).
    ThumbnailOnly,
    /// Miniatura, ou o ícone do arquivo (programas e instaladores).
    AnyIcon,
}

fn decode_stored(bytes: &[u8]) -> Option<Thumb> {
    // Registro vazio = já tentamos e não há miniatura.
    let rgba = image::load_from_memory(bytes).ok()?.to_rgba8();
    Some(Thumb {
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw(),
    })
}

/// JPEG qualidade 65 (fotos, vídeos, capas); PNG quando há transparência (ícones).
fn encode_small(rgba: &RgbaImage) -> Option<Vec<u8>> {
    let has_alpha = rgba.pixels().any(|pixel| pixel.0[3] < 250);
    let mut out = Vec::new();
    if has_alpha {
        image::codecs::png::PngEncoder::new(&mut out)
            .write_image(rgba.as_raw(), rgba.width(), rgba.height(), image::ExtendedColorType::Rgba8)
            .ok()?;
    } else {
        let rgb = DynamicImage::ImageRgba8(rgba.clone()).to_rgb8();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 65)
            .write_image(rgb.as_raw(), rgb.width(), rgb.height(), image::ExtendedColorType::Rgb8)
            .ok()?;
    }
    Some(out)
}

/// Lê uma imagem do disco e reduz para caber em `side` x `side`.
pub fn shrink_image_file(path: &Path, side: u32) -> Option<RgbaImage> {
    if std::fs::metadata(path).ok()?.len() > MAX_IMAGE_BYTES {
        return None;
    }
    let image = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?;
    Some(image.thumbnail(side, side).to_rgba8())
}

fn shrink(image: RgbaImage) -> RgbaImage {
    DynamicImage::ImageRgba8(image)
        .thumbnail(STORED_SIDE, STORED_SIDE)
        .to_rgba8()
}

fn generate(path: &Path, kind: &str, mode: Mode) -> Option<RgbaImage> {
    match mode {
        Mode::WindowsCache => {
            if !can_have_thumbnail(path, kind) {
                return None;
            }
            shell::thumbnail(path, STORED_SIDE, Fetch::CacheOnly).map(shrink)
        }
        Mode::WindowsFull => {
            if !can_have_thumbnail(path, kind) {
                return None;
            }
            let fetch = if matches!(kind, "program" | "installer") {
                Fetch::AnyIcon
            } else {
                Fetch::ThumbnailOnly
            };
            shell::thumbnail(path, STORED_SIDE, fetch)
                .map(shrink)
                .or_else(|| is_photo_ext(path).then(|| shrink_image_file(path, STORED_SIDE)).flatten())
        }
        Mode::DecodeOnly => is_photo_ext(path)
            .then(|| shrink_image_file(path, STORED_SIDE))
            .flatten(),
    }
}

fn store_and_return(db: &Db, key: i64, image: RgbaImage) -> Option<Thumb> {
    let bytes = encode_small(&image)?;
    let _ = db.put_thumb(key, &bytes);
    Some(Thumb {
        width: image.width(),
        height: image.height(),
        rgba: image.into_raw(),
    })
}

/// Miniatura de `path`: do banco se já existir; senão gera conforme `mode` (se o arquivo estiver
/// acessível), guarda no banco e devolve. `None` = sem miniatura (a tela mostra um quadrado
/// colorido com a extensão).
pub fn thumb_for(db: &Db, path: &Path, kind: &str, mode: Mode) -> Option<Thumb> {
    let key = key_for(&path.to_string_lossy());
    if let Ok(Some(bytes)) = db.get_thumb(key) {
        return decode_stored(&bytes);
    }
    if !path.exists() {
        return None; // HD desconectado: tenta de novo quando voltar
    }
    match generate(path, kind, mode) {
        Some(image) => store_and_return(db, key, image),
        None => {
            // Só "WindowsCache" pode achar depois (o Explorer pode gerar mais tarde);
            // nos outros modos já tentamos tudo, então marca para não tentar de novo.
            if mode != Mode::WindowsCache {
                let _ = db.put_thumb(key, &[]);
            }
            None
        }
    }
}

/// Miniatura a partir de uma imagem qualquer (ex.: capa da Steam), guardada com chave própria.
pub fn thumb_from_image(db: &Db, key_text: &str, image_path: &Path) -> Option<Thumb> {
    let key = key_for(key_text);
    if let Ok(Some(bytes)) = db.get_thumb(key) {
        return decode_stored(&bytes);
    }
    let image = shrink_image_file(image_path, STORED_SIDE)?;
    store_and_return(db, key, image)
}

/// Tipos que ganham miniatura em lote.
pub const BATCH_KINDS: &[&str] = &["photo", "video", "installer", "program"];

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BatchReport {
    pub generated: usize,
    pub already_stored: usize,
    pub without_thumbnail: usize,
}

/// Gera as miniaturas que faltam dos arquivos já catalogados (só das fontes conectadas).
/// `source_ids` vazio = todas as fontes conectadas. Para quando `cancel` for ligado.
/// `on_progress(feitos, total)` é chamado de tempos em tempos.
pub fn generate_missing(
    db_path: &Path,
    source_ids: &[i64],
    kinds: &[&str],
    mode: Mode,
    threads: usize,
    cancel: &std::sync::atomic::AtomicBool,
    on_progress: &(dyn Fn(usize, usize) + Sync),
) -> anyhow::Result<BatchReport> {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let candidates = {
        let db = Db::open(db_path)?;
        let online: Vec<i64> = db
            .sources()?
            .into_iter()
            .filter(|source| {
                (source_ids.is_empty() || source_ids.contains(&source.id))
                    && Path::new(&source.path).exists()
            })
            .map(|source| source.id)
            .collect();
        // Só arquivos que podem ter miniatura: o resto nem entra na fila.
        let mut files = db.files_for_thumbs(&online, kinds)?;
        files.retain(|(path, kind)| mode == Mode::DecodeOnly || can_have_thumbnail(path, kind));
        files
    };
    let total = candidates.len();
    let (next, done) = (AtomicUsize::new(0), AtomicUsize::new(0));
    let (generated, already, without) = (AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0));

    std::thread::scope(|scope| {
        for _ in 0..threads.max(1) {
            scope.spawn(|| {
                let Ok(db) = Db::open(db_path) else { return };
                while !cancel.load(Ordering::Relaxed) {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some((path, kind)) = candidates.get(index) else { break };
                    if db.has_thumb(key_for(&path.to_string_lossy())).unwrap_or(false) {
                        already.fetch_add(1, Ordering::Relaxed);
                    } else if thumb_for(&db, path, kind, mode).is_some() {
                        generated.fetch_add(1, Ordering::Relaxed);
                    } else {
                        without.fetch_add(1, Ordering::Relaxed);
                    }
                    let finished = done.fetch_add(1, Ordering::Relaxed) + 1;
                    if finished % 20 == 0 || finished == total {
                        on_progress(finished, total);
                    }
                }
            });
        }
    });

    Ok(BatchReport {
        generated: generated.load(Ordering::Relaxed),
        already_stored: already.load(Ordering::Relaxed),
        without_thumbnail: without.load(Ordering::Relaxed),
    })
}

#[cfg(windows)]
mod shell {
    use image::RgbaImage;
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::SIZE;
    use windows::Win32::Graphics::Gdi::{
        DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER,
        BI_RGB, DIB_RGB_COLORS, HGDIOBJ,
    };
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::{
        IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_BIGGERSIZEOK, SIIGBF_INCACHEONLY,
        SIIGBF_THUMBNAILONLY,
    };

    /// Liga o COM uma vez por thread (e desliga quando a thread termina), em vez de a cada arquivo.
    struct ComGuard(bool);

    impl ComGuard {
        fn new() -> Self {
            ComGuard(unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok() })
        }
    }

    impl Drop for ComGuard {
        fn drop(&mut self) {
            if self.0 {
                unsafe { CoUninitialize() };
            }
        }
    }

    thread_local! {
        static COM: ComGuard = ComGuard::new();
    }

    /// Miniatura (ou ícone) do Windows, conforme `fetch`.
    pub fn thumbnail(path: &Path, side: u32, fetch: super::Fetch) -> Option<RgbaImage> {
        COM.with(|_| ());
        unsafe { read_thumbnail(path, side, fetch) }
    }

    unsafe fn read_thumbnail(path: &Path, side: u32, fetch: super::Fetch) -> Option<RgbaImage> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let factory: IShellItemImageFactory =
            SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None).ok()?;
        let flags = match fetch {
            super::Fetch::CacheOnly => SIIGBF_BIGGERSIZEOK | SIIGBF_INCACHEONLY,
            super::Fetch::ThumbnailOnly => SIIGBF_BIGGERSIZEOK | SIIGBF_THUMBNAILONLY,
            super::Fetch::AnyIcon => SIIGBF_BIGGERSIZEOK,
        };
        let bitmap = factory
            .GetImage(SIZE { cx: side as i32, cy: side as i32 }, flags)
            .ok()?;

        let mut info = BITMAP::default();
        let read = GetObjectW(
            HGDIOBJ(bitmap.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut info as *mut BITMAP as *mut c_void),
        );
        if read == 0 || info.bmWidth <= 0 || info.bmHeight <= 0 {
            let _ = DeleteObject(HGDIOBJ(bitmap.0));
            return None;
        }
        let (width, height) = (info.bmWidth as u32, info.bmHeight as u32);

        let mut header = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32), // de cima para baixo
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut pixels = vec![0u8; (width * height * 4) as usize];
        let hdc = GetDC(None);
        let lines = GetDIBits(
            hdc,
            bitmap,
            0,
            height,
            Some(pixels.as_mut_ptr() as *mut c_void),
            &mut header,
            DIB_RGB_COLORS,
        );
        ReleaseDC(None, hdc);
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
        if lines == 0 {
            return None;
        }

        // BGRA -> RGBA. Bitmaps sem canal alfa vêm com alfa 0: nesse caso fica opaco.
        let all_transparent = pixels.chunks_exact(4).all(|px| px[3] == 0);
        for px in pixels.chunks_exact_mut(4) {
            px.swap(0, 2);
            if all_transparent {
                px[3] = 255;
            }
        }
        RgbaImage::from_raw(width, height, pixels)
    }
}

#[cfg(not(windows))]
mod shell {
    use image::RgbaImage;
    use std::path::Path;

    pub fn thumbnail(_path: &Path, _side: u32, _fetch: super::Fetch) -> Option<RgbaImage> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn photo(path: &Path) {
        let mut image = RgbaImage::new(640, 480);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            *pixel = image::Rgba([(x % 255) as u8, (y % 255) as u8, 120, 255]);
        }
        image.save(path).unwrap();
    }

    #[test]
    fn only_files_that_can_have_a_thumbnail_are_tried() {
        let yes = |name: &str, kind: &str| can_have_thumbnail(Path::new(name), kind);
        assert!(yes("F:\\a.JPG", "photo"));
        assert!(yes("F:\\a.mkv", "video"));
        assert!(yes("F:\\setup.exe", "installer"));
        assert!(yes("F:\\manual.pdf", "document"));
        assert!(!yes("F:\\a.dll", "other"));
        assert!(!yes("F:\\jogo.bin", "archive"));
        assert!(!yes("F:\\notas.txt", "document"));
        assert!(!yes("F:\\semextensao", "photo"));
        assert!(!yes("F:\\a.mp3", "music"));
    }

    #[test]
    fn key_ignores_case_and_differs_by_path() {
        assert_eq!(key_for("H:\\Fotos\\A.JPG"), key_for("h:\\fotos\\a.jpg"));
        assert_ne!(key_for("H:\\a.jpg"), key_for("H:\\b.jpg"));
    }

    #[test]
    fn photo_thumbnail_is_small_stored_in_sqlite_and_survives_file_removal() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("foto.png");
        photo(&file);
        let db = Db::open_in_memory().unwrap();

        let first = thumb_for(&db, &file, "photo", Mode::DecodeOnly).unwrap();
        assert!(first.width <= STORED_SIDE && first.height <= STORED_SIDE);
        assert_eq!(first.rgba.len(), (first.width * first.height * 4) as usize);

        let (count, bytes) = db.thumb_stats().unwrap();
        assert_eq!(count, 1);
        assert!(bytes < 8_000, "miniatura grande demais: {bytes} bytes");

        // HD "desconectado": o arquivo some, a miniatura continua no banco.
        std::fs::remove_file(&file).unwrap();
        let offline = thumb_for(&db, &file, "photo", Mode::DecodeOnly).unwrap();
        assert_eq!((offline.width, offline.height), (first.width, first.height));
    }

    #[test]
    fn no_thumbnail_for_missing_file_or_non_photo_without_shell() {
        let db = Db::open_in_memory().unwrap();
        assert!(thumb_for(&db, Path::new("Z:\\nao\\existe.jpg"), "photo", Mode::DecodeOnly).is_none());
        let dir = tempfile::tempdir().unwrap();
        let video = dir.path().join("a.mkv");
        std::fs::write(&video, b"x").unwrap();
        assert!(thumb_for(&db, &video, "video", Mode::DecodeOnly).is_none());
    }

    #[test]
    fn cache_only_mode_never_stores_a_negative_mark_but_full_modes_do() {
        let dir = tempfile::tempdir().unwrap();
        let video = dir.path().join("a.mkv");
        std::fs::write(&video, b"x").unwrap(); // não é vídeo de verdade: o Windows não tem miniatura
        let db = Db::open_in_memory().unwrap();

        assert!(thumb_for(&db, &video, "video", Mode::WindowsCache).is_none());
        assert!(!db.has_thumb(key_for(&video.to_string_lossy())).unwrap());

        assert!(thumb_for(&db, &video, "video", Mode::DecodeOnly).is_none());
        assert!(db.has_thumb(key_for(&video.to_string_lossy())).unwrap());
    }

    #[test]
    fn batch_generates_missing_thumbnails_for_already_cataloged_files_once() {
        let dir = tempfile::tempdir().unwrap();
        let photos = dir.path().join("fotos");
        std::fs::create_dir_all(&photos).unwrap();
        for name in ["a.png", "b.png", "c.png"] {
            photo(&photos.join(name));
        }
        std::fs::write(photos.join("notas.txt"), b"x").unwrap();

        // Catálogo "antigo": os arquivos já estão catalogados, sem nenhuma miniatura.
        let db_path = dir.path().join("catalogo.db");
        let mut db = Db::open(&db_path).unwrap();
        let id = db.add_source("fotos", &photos.to_string_lossy()).unwrap();
        crate::scan::scan_walk(
            &mut db,
            id,
            &photos,
            &crate::scan::ScanOptions::default(),
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(db.thumb_stats().unwrap().0, 0);
        drop(db);

        let cancel = AtomicBool::new(false);
        let first = generate_missing(&db_path, &[], BATCH_KINDS, Mode::DecodeOnly, 2, &cancel, &|_, _| {}).unwrap();
        assert_eq!(first, BatchReport { generated: 3, already_stored: 0, without_thumbnail: 0 });

        let second = generate_missing(&db_path, &[], BATCH_KINDS, Mode::DecodeOnly, 2, &cancel, &|_, _| {}).unwrap();
        assert_eq!(second, BatchReport { generated: 0, already_stored: 3, without_thumbnail: 0 });
    }

    #[test]
    fn batch_skips_offline_sources() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("catalogo.db");
        let db = Db::open(&db_path).unwrap();
        db.add_source("hd-desconectado", "Z:\\nao-existe").unwrap();
        drop(db);
        let report = generate_missing(&db_path, &[], BATCH_KINDS, Mode::DecodeOnly, 1, &AtomicBool::new(false), &|_, _| {}).unwrap();
        assert_eq!(report, BatchReport::default());
    }
}
