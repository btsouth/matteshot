//! Content-derived styling: extract a dominant hue from the capture and
//! build a family of backdrops around it. Deterministic per capture —
//! variety comes from the subject, not randomness.

use image::RgbaImage;

#[derive(Clone, Copy, Debug)]
pub struct Rgb(pub f32, pub f32, pub f32);

/// A soft radial color blob: center (fractions of canvas), radius (fraction
/// of max canvas dimension), color.
#[derive(Clone, Copy)]
pub struct Blob {
    pub cx: f32,
    pub cy: f32,
    pub r: f32,
    pub color: Rgb,
}

#[derive(Clone)]
pub enum Backdrop {
    /// Diagonal two-stop gradient, subtly darkened toward the bottom.
    Linear { c1: Rgb, c2: Rgb },
    /// Dark base with soft color blobs (mesh-gradient look).
    Aurora { base: Rgb, blobs: Vec<Blob> },
    /// No matte at all: the raw capture (plus annotations), untouched.
    Plain,
}

#[derive(Clone)]
pub struct Style {
    pub name: &'static str,
    pub backdrop: Backdrop,
}

fn hsl(h: f32, s: f32, l: f32) -> Rgb {
    let h = h.rem_euclid(360.0) / 60.0;
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - (h % 2.0 - 1.0).abs());
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    Rgb(r + m, g + m, b + m)
}

fn rgb_to_hsl(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if max == min {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = if l > 0.5 { d / (2.0 - max - min) } else { d / (max + min) };
    let h = if max == r {
        60.0 * (((g - b) / d) % 6.0)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    (h.rem_euclid(360.0), s, l)
}

/// Dominant saturated hue via a weighted histogram, or None when the capture
/// is essentially grayscale (most dev UIs).
fn dominant_hue(img: &RgbaImage) -> Option<f32> {
    let thumb = image::imageops::thumbnail(img, 64, 64);
    const BINS: usize = 24;
    let mut weight = [0f32; BINS];
    let mut hue_sum = [0f32; BINS];

    for p in thumb.pixels() {
        if p[3] < 200 {
            continue;
        }
        let (h, s, l) = rgb_to_hsl(
            p[0] as f32 / 255.0,
            p[1] as f32 / 255.0,
            p[2] as f32 / 255.0,
        );
        // Only saturated mid-tones vote; whites/blacks/grays carry no hue info.
        if s > 0.25 && (0.15..0.85).contains(&l) {
            let bin = ((h / 360.0 * BINS as f32) as usize).min(BINS - 1);
            let w = s * (1.0 - (l - 0.5).abs());
            weight[bin] += w;
            hue_sum[bin] += h * w;
        }
    }

    let (best, &best_w) = weight
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .unwrap();
    (best_w > 6.0).then(|| hue_sum[best] / best_w)
}

/// The six style families offered in the picker, tuned around the capture's
/// dominant hue (indigo when the capture is grayscale).
pub fn variants(img: &RgbaImage) -> Vec<Style> {
    let h = dominant_hue(img).unwrap_or(232.0);
    vec![
        Style {
            name: "Adaptive",
            backdrop: Backdrop::Linear {
                c1: hsl(h - 14.0, 0.58, 0.66),
                c2: hsl(h + 32.0, 0.52, 0.40),
            },
        },
        Style {
            name: "Deep",
            backdrop: Backdrop::Linear {
                c1: hsl(h + 10.0, 0.45, 0.30),
                c2: hsl(h - 30.0, 0.55, 0.12),
            },
        },
        Style {
            name: "Aurora",
            backdrop: Backdrop::Aurora {
                base: hsl(h, 0.35, 0.15),
                blobs: vec![
                    Blob { cx: 0.12, cy: 0.15, r: 0.60, color: hsl(h + 40.0, 0.65, 0.46) },
                    Blob { cx: 0.88, cy: 0.22, r: 0.52, color: hsl(h - 55.0, 0.60, 0.40) },
                    Blob { cx: 0.50, cy: 0.98, r: 0.55, color: hsl(h + 165.0, 0.36, 0.26) },
                ],
            },
        },
        Style {
            name: "Slate",
            backdrop: Backdrop::Linear {
                c1: hsl(222.0, 0.12, 0.24),
                c2: hsl(222.0, 0.14, 0.09),
            },
        },
        Style {
            name: "Paper",
            backdrop: Backdrop::Linear {
                c1: hsl(40.0, 0.28, 0.96),
                c2: hsl(222.0, 0.16, 0.86),
            },
        },
        Style {
            name: "Pop",
            backdrop: Backdrop::Linear {
                c1: hsl(h + 150.0, 0.62, 0.58),
                c2: hsl(h + 200.0, 0.68, 0.34),
            },
        },
        Style { name: "None", backdrop: Backdrop::Plain },
    ]
}
