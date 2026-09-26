//! App icon generator. The icon IS a matte shot: a clean near-white card
//! composed onto the Aurora matte by the product's own pipeline, so the brand
//! mark and the product output are literally the same thing.
//! `matteshot --icon [dir]` regenerates the .ico and preview PNGs.

use anyhow::Result;
use image::RgbaImage;

/// The "screenshot": a near-white card with a faint diagonal sheen so large
/// sizes don't read as flat white.
fn card(w: u32, h: u32) -> RgbaImage {
    let mut img = RgbaImage::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let t = (x as f32 / w as f32 + y as f32 / h as f32) / 2.0;
            let v = 251.0 - t * 14.0;
            img.put_pixel(
                x,
                y,
                image::Rgba([v as u8, (v + 1.0) as u8, 255.0_f32.min(v + 4.0) as u8, 255]),
            );
        }
    }
    img
}

/// Rounded-rect alpha mask over the whole canvas (Win11-style outer corners).
fn round_outer(img: &mut RgbaImage, radius: f32) {
    let (w, h) = (img.width() as f32, img.height() as f32);
    for y in 0..img.height() {
        for x in 0..img.width() {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            let qx = (px - w / 2.0).abs() - (w / 2.0 - radius);
            let qy = (py - h / 2.0).abs() - (h / 2.0 - radius);
            let d =
                (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - radius;
            let a = (0.5 - d).clamp(0.0, 1.0);
            let p = img.get_pixel_mut(x, y);
            p[3] = (p[3] as f32 * a) as u8;
        }
    }
}

pub fn generate(outdir: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(outdir)?;

    // Hand-tuned aurora in the brand's indigo family — violet, teal and
    // magenta blobs (the derived variant's third blob goes olive on white).
    use crate::style::{Backdrop, Blob, Rgb, Style};
    let content = card(640, 420);
    let aurora = Style {
        name: "Aurora",
        backdrop: Backdrop::Aurora {
            base: Rgb(0.10, 0.08, 0.24),
            blobs: vec![
                Blob {
                    cx: 0.10,
                    cy: 0.12,
                    r: 0.62,
                    color: Rgb(0.44, 0.32, 0.92),
                },
                Blob {
                    cx: 0.90,
                    cy: 0.18,
                    r: 0.55,
                    color: Rgb(0.10, 0.62, 0.58),
                },
                Blob {
                    cx: 0.55,
                    cy: 1.00,
                    r: 0.60,
                    color: Rgb(0.66, 0.22, 0.48),
                },
            ],
        },
    };
    let big = crate::compose::export(&content, &aurora, 0.17, Some(1.0), 2, 0);

    let sizes: [u32; 7] = [256, 128, 64, 48, 32, 24, 16];
    let mut pngs: Vec<(u32, Vec<u8>)> = Vec::new();
    for &s in &sizes {
        let mut im = image::imageops::resize(&big, s, s, image::imageops::FilterType::Lanczos3);
        round_outer(&mut im, s as f32 * 0.20);
        if s == 256 {
            im.save(outdir.join("icon-256.png"))?;
        }
        let mut buf = Vec::new();
        im.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)?;
        pngs.push((s, buf));
    }

    // .ico container: ICONDIR + ICONDIRENTRYs + PNG blobs (PNG entries are
    // valid since Vista).
    let mut ico: Vec<u8> = vec![0, 0, 1, 0];
    ico.extend_from_slice(&(pngs.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * pngs.len();
    for (s, buf) in &pngs {
        let dim = if *s >= 256 { 0u8 } else { *s as u8 };
        ico.extend_from_slice(&[dim, dim, 0, 0]);
        ico.extend_from_slice(&1u16.to_le_bytes());
        ico.extend_from_slice(&32u16.to_le_bytes());
        ico.extend_from_slice(&(buf.len() as u32).to_le_bytes());
        ico.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += buf.len();
    }
    for (_, buf) in &pngs {
        ico.extend_from_slice(buf);
    }
    let ico_path = outdir.join("matteshot.ico");
    std::fs::write(&ico_path, ico)?;
    eprintln!(
        "icon: {} + icon-256.png ({} sizes)",
        ico_path.display(),
        sizes.len()
    );
    Ok(())
}
