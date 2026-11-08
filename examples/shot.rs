//! Ferramenta de desenvolvimento: desenha as telas com dados de exemplo e salva em PNG,
//! sem abrir janela de verdade. Uso: cargo run --example shot -- <pasta-de-saida>

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, WindowAdapter};
use slint::{Color, ComponentHandle, Image, ModelRc, PlatformError, Rgb8Pixel, Rgba8Pixel, SharedPixelBuffer, VecModel};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

slint::include_modules!();

struct HeadlessPlatform {
    window: Rc<MinimalSoftwareWindow>,
    started: Instant,
}

impl Platform for HeadlessPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.window.clone())
    }
    fn duration_since_start(&self) -> Duration {
        self.started.elapsed()
    }
}

fn gradient(seed: u8) -> Image {
    let (w, h) = (96u32, 72u32);
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(w, h);
    for (i, pixel) in buffer.make_mut_slice().iter_mut().enumerate() {
        let (x, y) = ((i as u32 % w) as u8, (i as u32 / w) as u8);
        *pixel = Rgba8Pixel::new(x.wrapping_mul(2).wrapping_add(seed), y.wrapping_mul(3), 140u8.wrapping_add(seed), 255);
    }
    Image::from_rgba8(buffer)
}

fn model<T: Clone + 'static>(items: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(items))
}

fn file(name: &str, kind: &str, ext: &str, color: Color, thumb: Option<u8>, online: bool) -> FileItem {
    FileItem {
        name: name.into(),
        path: format!("H:\\Biblioteca\\Videos e fotos\\{name}").into(),
        size: "1.4 GB".into(),
        kind: kind.into(),
        source: "H:\\".into(),
        online,
        ext: ext.into(),
        color,
        date: "12/03/2021".into(),
        thumb: thumb.map(gradient).unwrap_or_default(),
        has_thumb: thumb.is_some(),
    }
}

fn main() {
    let out = PathBuf::from(std::env::args().nth(1).expect("pasta de saída"));
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(HeadlessPlatform { window: window.clone(), started: Instant::now() })).unwrap();
    let (width, height) = (1280u32, 800u32);
    window.set_size(slint::PhysicalSize::new(width, height));

    let ui = MainWindow::new().unwrap();
    ui.show().unwrap();

    let pink = Color::from_rgb_u8(0xFF, 0x8A, 0xB5);
    let purple = Color::from_rgb_u8(0xB3, 0x88, 0xFF);
    let orange = Color::from_rgb_u8(0xFF, 0xAB, 0x70);
    let gray = Color::from_rgb_u8(0x9A, 0x9A, 0x9A);
    let files: Vec<FileItem> = (0..16)
        .map(|i| match i % 4 {
            0 => file(&format!("Dexter - 1x0{i} Piloto.mkv"), "Vídeos", "MKV", purple, Some(i as u8 * 9), true),
            1 => file(&format!("IMG_40{i}.jpg"), "Fotos", "JPG", pink, Some(i as u8 * 13), false),
            2 => file(&format!("anyburn_setup_{i}.exe"), "Instaladores", "EXE", orange, None, true),
            _ => file(&format!("Manual {i}.pdf"), "Documentos", "PDF", gray, None, false),
        })
        .collect();

    ui.set_results(model(files.clone()));
    ui.set_library_count("48.213 itens · mostrando 300".into());
    ui.set_library_has_next(true);
    ui.set_library_page(2);
    ui.set_source_names(model(vec!["Todas as fontes".into(), "H:\\".into(), "J:\\".into()]));
    ui.set_selected(files[0].clone());
    ui.set_selected_index(0);
    ui.set_vault_unlocked(true);
    ui.set_vault_results(model(files.clone()));
    ui.set_vault_count("12 itens".into());
    ui.set_status("".into());

    let games: Vec<GameItem> = ["Baldur's Gate 3", "Resident Evil 4", "The Witcher 3", "Hades", "Stardew Valley", "Portal 2", "Hollow Knight", "Cyberpunk 2077", "Alan Wake", "A Bird Story"]
        .iter()
        .enumerate()
        .map(|(i, name)| GameItem {
            name: (*name).into(),
            info: format!("AppID 10869{i} · {} GB", 20 + i).into(),
            library: "K:\\SteamLibrary".into(),
            color: Color::from_rgb_u8(0x1B, 0x9D, 0xE2),
            initials: name.chars().take(2).collect::<String>().to_uppercase().into(),
            thumb: gradient(i as u8 * 20),
            has_thumb: i % 3 != 2,
        })
        .collect();
    ui.set_games(model(games));

    let dupes: Vec<DupItem> = (0..7)
        .map(|i| DupItem {
            name: format!("Filme {i} 1080p.mkv").into(),
            size_bytes: "2000000000".into(),
            info: "3 cópias · 1.9 GB cada · 3.8 GB a mais".into(),
            color: purple,
            ext: "MKV".into(),
            thumb: gradient(i as u8 * 25),
            has_thumb: i % 2 == 0,
        })
        .collect();
    ui.set_dupes(model(dupes));
    ui.set_dupe_summary("120 grupos (os maiores primeiro) · até 85.0 GB para liberar".into());
    ui.set_dupe_selected(0);
    ui.set_dupe_copies(model(vec![
        DupCopy { id: 1, path: "H:\\Biblioteca\\Filmes\\Filme 0 1080p.mkv".into(), info: "H:\\ · modificado há 400 dias".into(), online: true, checked: false },
        DupCopy { id: 2, path: "J:\\Backup\\Filme 0 1080p.mkv".into(), info: "J:\\ · modificado há 200 dias".into(), online: true, checked: true },
        DupCopy { id: 3, path: "K:\\Videos\\Filme 0 1080p.mkv".into(), info: "K:\\ · modificado há 90 dias".into(), online: false, checked: false },
    ]));

    ui.set_stat_files("48.213".into());
    ui.set_stat_bytes("11.4 TB".into());
    ui.set_stat_offline("3 de 5".into());
    ui.set_stat_dupes("1.206".into());
    ui.set_stat_thumbs("Miniaturas guardadas no banco: 2.480 (7.3 MB)".into());

    let shots: [(&str, i32, Option<(bool, i32)>); 7] = [
        ("inicio", 0, None),
        ("biblioteca-lista", 1, Some((false, 1))),
        ("biblioteca-miniaturas", 1, Some((true, 1))),
        ("jogos", 2, None),
        ("duplicados", 4, None),
        ("cofre-miniaturas", 5, Some((true, 5))),
        ("configuracoes", 6, None),
    ];
    for (name, page, grid) in shots {
        ui.set_page(page);
        if let Some((grid, which)) = grid {
            if which == 1 {
                ui.set_library_grid(grid);
            } else {
                ui.set_vault_grid(grid);
            }
        }
        slint::platform::update_timers_and_animations();
        let mut buffer = SharedPixelBuffer::<Rgb8Pixel>::new(width, height);
        window.request_redraw();
        window.draw_if_needed(|renderer| {
            renderer.render(buffer.make_mut_slice(), width as usize);
        });
        // A primeira passada calcula o layout; a segunda desenha com ele pronto.
        slint::platform::update_timers_and_animations();
        window.request_redraw();
        window.draw_if_needed(|renderer| {
            renderer.render(buffer.make_mut_slice(), width as usize);
        });
        let image = image::RgbImage::from_raw(width, height, buffer.as_bytes().to_vec()).unwrap();
        image.save(out.join(format!("{name}.png"))).unwrap();
        println!("salvo {name}.png");
    }
}
