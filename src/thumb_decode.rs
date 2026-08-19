//! History thumbnail decode: never materialize a full-resolution RGBA buffer.
//!
//! Picker already downscales in-memory captures to `PREVIEW_MAX` (480) in
//! `src/main.rs`. History open/reload used to `image::open` + `to_rgba8` every
//! indexed PNG at native size before shrinking to ~176×118, so a few 40k-px
//! scroll captures could spike multi-GB RAM and freeze the tray (SBS-913).

use std::fs::File;
use std::io::{BufRead, BufReader, Seek};
use std::path::Path;

use image::{Rgba, RgbaImage};

/// Long-edge budget for a history thumbnail decode. Equal to picker
/// `PREVIEW_MAX` in `src/main.rs` (480). History cells are smaller (~176×118,
/// more with DPI); this is the buffer we materialize, not the on-screen size.
pub const THUMB_DECODE_MAX: u32 = 480;

/// Decode `path` so the returned image's long edge is at most `max_edge`.
///
/// A PNG taller or wider than the budget is box-downsampled while the decoder
/// still only holds one scanline. Formats we cannot downsample that way are
/// opened at full resolution only when they already fit the budget; a huge
/// interlaced PNG or GIF is skipped (`None`) rather than decoded at native size.
/// Corrupt or unreadable files are also `None` — same as the old `image::open`
/// failure path, so History just leaves that cell out of the grid.
pub fn decode_for_thumb(path: &Path, max_edge: u32) -> Option<RgbaImage> {
    let max_edge = max_edge.max(1);
    decode_png_capped(path, max_edge).or_else(|| decode_small_generic(path, max_edge))
}

fn decode_png_capped(path: &Path, max_edge: u32) -> Option<RgbaImage> {
    let file = File::open(path).ok()?;
    let mut decoder = png::Decoder::new(BufReader::new(file));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    let info = reader.info();
    let (width, height) = (info.width, info.height);
    if width == 0 || height == 0 {
        return None;
    }
    let interlaced = info.interlaced;
    if interlaced {
        // Adam7 rows are not sequential scanlines. Reconstructing them is a
        // full-frame allocation — the failure this ticket removes. A small
        // interlaced file can still go through `image` because it already fits.
        if width.max(height) > max_edge {
            return None;
        }
        drop(reader);
        return image::open(path).ok().map(|img| img.to_rgba8());
    }
    downsample_png_rows(&mut reader, width, height, max_edge)
}

fn downsample_png_rows<R: BufRead + Seek>(
    reader: &mut png::Reader<R>,
    width: u32,
    height: u32,
    max_edge: u32,
) -> Option<RgbaImage> {
    let factor = long_edge_factor(width, height, max_edge);
    let out_w = width.div_ceil(factor);
    let out_h = height.div_ceil(factor);
    if out_w == 0 || out_h == 0 {
        return None;
    }

    let (color, depth) = reader.output_color_type();
    if depth != png::BitDepth::Eight {
        return None;
    }
    let channels = match color {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Indexed => return None,
    };
    let row_bytes = width as usize * channels;

    let mut out = RgbaImage::new(out_w, out_h);
    let mut acc = Acc::new(out_w as usize);
    let mut current_oy = 0u32;

    for y in 0..height {
        let row = reader.next_row().ok()??;
        let data = row.data();
        if data.len() < row_bytes {
            return None;
        }
        let oy = y / factor;
        if oy != current_oy {
            acc.flush_into(&mut out, current_oy);
            acc.reset();
            current_oy = oy;
        }
        acc.add_row(&data[..row_bytes], width, channels, factor);
    }
    acc.flush_into(&mut out, current_oy);
    Some(out)
}

/// Integer factor so `max(width, height) / factor` fits in `max_edge`.
fn long_edge_factor(width: u32, height: u32, max_edge: u32) -> u32 {
    width.max(height).max(1).div_ceil(max_edge.max(1)).max(1)
}

/// Open a non-PNG (or a small interlaced PNG already handled above) only when
/// the header says it already fits the budget. A huge GIF still has no
/// scanline downsample path here; refusing it is the fail-closed choice.
fn decode_small_generic(path: &Path, max_edge: u32) -> Option<RgbaImage> {
    let reader = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?;
    let (width, height) = reader.into_dimensions().ok()?;
    if width == 0 || height == 0 || width.max(height) > max_edge {
        return None;
    }
    image::open(path).ok().map(|img| img.to_rgba8())
}

struct Acc {
    r: Vec<u64>,
    g: Vec<u64>,
    b: Vec<u64>,
    a: Vec<u64>,
    n: Vec<u64>,
}

impl Acc {
    fn new(out_w: usize) -> Self {
        Self {
            r: vec![0; out_w],
            g: vec![0; out_w],
            b: vec![0; out_w],
            a: vec![0; out_w],
            n: vec![0; out_w],
        }
    }

    fn reset(&mut self) {
        for slot in [
            &mut self.r,
            &mut self.g,
            &mut self.b,
            &mut self.a,
            &mut self.n,
        ] {
            slot.fill(0);
        }
    }

    fn add_row(&mut self, data: &[u8], width: u32, channels: usize, factor: u32) {
        for x in 0..width {
            let i = x as usize * channels;
            let (r, g, b, a) = match channels {
                1 => (data[i], data[i], data[i], 255),
                2 => (data[i], data[i], data[i], data[i + 1]),
                3 => (data[i], data[i + 1], data[i + 2], 255),
                4 => (data[i], data[i + 1], data[i + 2], data[i + 3]),
                _ => return,
            };
            let ox = (x / factor) as usize;
            self.r[ox] += u64::from(r);
            self.g[ox] += u64::from(g);
            self.b[ox] += u64::from(b);
            self.a[ox] += u64::from(a);
            self.n[ox] += 1;
        }
    }

    fn flush_into(&self, out: &mut RgbaImage, oy: u32) {
        if oy >= out.height() {
            return;
        }
        for ox in 0..out.width() {
            let i = ox as usize;
            let n = self.n[i].max(1);
            out.put_pixel(
                ox,
                oy,
                Rgba([
                    (self.r[i] / n) as u8,
                    (self.g[i] / n) as u8,
                    (self.b[i] / n) as u8,
                    (self.a[i] / n) as u8,
                ]),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_dir(name: &str) -> std::path::PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "matteshot-thumb-{}-{}-{}",
            name,
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_png(path: &Path, width: u32, height: u32, pixel: impl Fn(u32, u32) -> [u8; 4]) {
        let mut img = RgbaImage::new(width, height);
        for y in 0..height {
            for x in 0..width {
                img.put_pixel(x, y, Rgba(pixel(x, y)));
            }
        }
        img.save(path).unwrap();
    }

    /// Pins SBS-913: History used to materialize the native RGBA buffer.
    /// Without the cap this assertion fails because the decoded image is
    /// 80×4000, not a 480-edge downsample.
    #[test]
    fn a_tall_scroll_png_is_decoded_at_the_thumb_budget_not_full_res() {
        let dir = temp_dir("scroll");
        let path = dir.join("scroll.png");
        write_png(&path, 80, 4000, |_x, y| {
            if y < 2000 {
                [255, 32, 64, 255]
            } else {
                [0, 32, 64, 255]
            }
        });

        let decoded = decode_for_thumb(&path, THUMB_DECODE_MAX).expect("scroll png should decode");
        assert!(
            decoded.width().max(decoded.height()) <= THUMB_DECODE_MAX,
            "decoded {}×{} still at full resolution",
            decoded.width(),
            decoded.height()
        );
        assert!(
            decoded.height() < 4000,
            "a 4000-px source must not be returned at native height"
        );
        let top = decoded.get_pixel(decoded.width() / 2, 0).0;
        let bot = decoded
            .get_pixel(decoded.width() / 2, decoded.height() - 1)
            .0;
        assert!(
            top[0] > 200,
            "top half of the scroll should stay light, got {top:?}"
        );
        assert!(
            bot[0] < 50,
            "bottom half of the scroll should stay dark, got {bot:?}"
        );
    }

    #[test]
    fn a_small_png_is_not_upscaled_by_the_decode_cap() {
        let dir = temp_dir("small");
        let path = dir.join("small.png");
        write_png(&path, 32, 24, |x, y| [x as u8, y as u8, 90, 255]);
        let decoded = decode_for_thumb(&path, THUMB_DECODE_MAX).unwrap();
        assert_eq!(decoded.dimensions(), (32, 24));
        assert_eq!(decoded.get_pixel(4, 7).0, [4, 7, 90, 255]);
    }

    #[test]
    fn a_solid_color_survives_box_downsampling() {
        let dir = temp_dir("solid");
        let path = dir.join("solid.png");
        write_png(&path, 64, 2000, |_x, _y| [10, 20, 30, 255]);
        let decoded = decode_for_thumb(&path, THUMB_DECODE_MAX).unwrap();
        assert!(decoded.width().max(decoded.height()) <= THUMB_DECODE_MAX);
        let mid = decoded
            .get_pixel(decoded.width() / 2, decoded.height() / 2)
            .0;
        assert_eq!(mid, [10, 20, 30, 255]);
    }

    #[test]
    fn a_corrupt_file_yields_no_thumb() {
        let dir = temp_dir("corrupt");
        let path = dir.join("nope.png");
        std::fs::write(&path, b"not a png").unwrap();
        assert!(decode_for_thumb(&path, THUMB_DECODE_MAX).is_none());
    }

    #[test]
    fn a_missing_file_yields_no_thumb() {
        let dir = temp_dir("missing");
        assert!(decode_for_thumb(&dir.join("gone.png"), THUMB_DECODE_MAX).is_none());
    }

    #[test]
    fn long_edge_factor_fits_the_budget() {
        assert_eq!(long_edge_factor(80, 4000, 480), 9);
        assert_eq!(long_edge_factor(176, 118, 480), 1);
        assert_eq!(long_edge_factor(480, 480, 480), 1);
        let w = 1920u32;
        let h = 40_000u32;
        let f = long_edge_factor(w, h, 480);
        assert!(w.div_ceil(f).max(h.div_ceil(f)) <= 480);
    }
}
