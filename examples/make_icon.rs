//! Gera `assets/acervo.ico` a partir de `assets/acervo.svg`:
//!   cargo run --example make_icon
//! O .ico leva várias resoluções em PNG (16 a 256), que o Windows aceita desde o Vista.

use image::imageops::FilterType;
use image::{ImageEncoder, RgbaImage};
use std::path::Path;

fn main() {
    let svg = Path::new("assets/acervo.svg");
    // O Slint desenha o SVG (256 x 256); a gente só reduz para os outros tamanhos.
    let loaded = slint::Image::load_from_path(svg).expect("não consegui abrir o SVG");
    let pixels = loaded.to_rgba8().expect("o SVG não virou imagem");
    let big = RgbaImage::from_raw(pixels.width(), pixels.height(), pixels.as_bytes().to_vec()).expect("imagem inválida");

    let sizes = [16u32, 24, 32, 48, 64, 128, 256];
    let mut entries: Vec<(u32, Vec<u8>)> = Vec::new();
    for size in sizes {
        // Pixel art: reduz sem suavizar (cada quadradinho continua nítido).
        let small = image::imageops::resize(&big, size, size, FilterType::Nearest);
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(small.as_raw(), size, size, image::ExtendedColorType::Rgba8)
            .expect("falha ao gerar PNG");
        entries.push((size, png));
    }

    let mut ico: Vec<u8> = Vec::new();
    ico.extend_from_slice(&[0, 0, 1, 0]); // reservado, tipo 1 = ícone
    ico.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * entries.len() as u32;
    for (size, png) in &entries {
        let dimension = if *size >= 256 { 0u8 } else { *size as u8 }; // 0 significa 256
        ico.extend_from_slice(&[dimension, dimension, 0, 0]);
        ico.extend_from_slice(&1u16.to_le_bytes()); // planos
        ico.extend_from_slice(&32u16.to_le_bytes()); // bits por pixel
        ico.extend_from_slice(&(png.len() as u32).to_le_bytes());
        ico.extend_from_slice(&offset.to_le_bytes());
        offset += png.len() as u32;
    }
    for (_, png) in &entries {
        ico.extend_from_slice(png);
    }
    std::fs::write("assets/acervo.ico", &ico).expect("não consegui gravar o .ico");
    std::fs::write("assets/acervo-256.png", &entries.last().unwrap().1).expect("não consegui gravar o PNG");
    println!("assets/acervo.ico gerado ({} bytes, {} tamanhos)", ico.len(), entries.len());
}
