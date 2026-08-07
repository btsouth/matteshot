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

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f32, b: f32, eps: f32) -> bool {
        (a - b).abs() <= eps
    }

    #[test]
    fn hsl_and_rgb_to_hsl_round_trip_for_saturated_colors() {
        for (h, s, l) in [
            (0.0, 1.0, 0.5),
            (120.0, 0.6, 0.4),
            (240.0, 0.8, 0.3),
            (300.0, 0.5, 0.7),
            (37.0, 0.9, 0.55),
        ] {
            let Rgb(r, g, b) = hsl(h, s, l);
            let (h2, s2, l2) = rgb_to_hsl(r, g, b);
            assert!(approx(h, h2, 0.5), "hue: {h} vs {h2}");
            assert!(approx(s, s2, 0.01), "sat: {s} vs {s2}");
            assert!(approx(l, l2, 0.01), "light: {l} vs {l2}");
        }
    }

    #[test]
    fn hsl_produces_the_expected_primary_colors() {
        let Rgb(r, g, b) = hsl(0.0, 1.0, 0.5);
        assert!(approx(r, 1.0, 0.01) && approx(g, 0.0, 0.01) && approx(b, 0.0, 0.01), "red");
        let Rgb(r, g, b) = hsl(120.0, 1.0, 0.5);
        assert!(approx(r, 0.0, 0.01) && approx(g, 1.0, 0.01) && approx(b, 0.0, 0.01), "green");
        let Rgb(r, g, b) = hsl(240.0, 1.0, 0.5);
        assert!(approx(r, 0.0, 0.01) && approx(g, 0.0, 0.01) && approx(b, 1.0, 0.01), "blue");
    }

    #[test]
    fn zero_saturation_is_gray_regardless_of_hue() {
        for h in [0.0, 90.0, 217.0, 359.0] {
            let Rgb(r, g, b) = hsl(h, 0.0, 0.5);
            assert!(
                approx(r, 0.5, 0.001) && approx(g, 0.5, 0.001) && approx(b, 0.5, 0.001),
                "for hue {h}"
            );
        }
    }

    #[test]
    fn a_uniformly_gray_capture_has_no_dominant_hue() {
        let img = RgbaImage::from_pixel(64, 64, image::Rgba([128, 128, 128, 255]));
        assert_eq!(dominant_hue(&img), None);
    }

    #[test]
    fn a_saturated_capture_reports_its_hue() {
        for (color, expected_hue) in [
            (image::Rgba([255u8, 0, 0, 255]), 0.0f32),
            (image::Rgba([0, 255, 0, 255]), 120.0),
            (image::Rgba([0, 0, 255, 255]), 240.0),
        ] {
            let img = RgbaImage::from_pixel(64, 64, color);
            let hue = dominant_hue(&img).expect("a saturated capture must report a hue");
            assert!((hue - expected_hue).abs() < 1.0, "for {color:?}: got {hue}");
        }
    }

    #[test]
    fn near_transparent_pixels_do_not_vote() {
        // Fully transparent red must not be mistaken for a red-hued capture.
        let img = RgbaImage::from_pixel(64, 64, image::Rgba([255, 0, 0, 10]));
        assert_eq!(dominant_hue(&img), None);
    }

    #[test]
    fn variants_always_returns_the_same_seven_named_styles_in_order() {
        let img = RgbaImage::from_pixel(20, 20, image::Rgba([10, 200, 90, 255]));
        let names: Vec<&str> = variants(&img).iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["Adaptive", "Deep", "Aurora", "Slate", "Paper", "Pop", "None"]);
    }

    #[test]
    fn the_none_style_is_a_plain_backdrop() {
        let img = RgbaImage::from_pixel(20, 20, image::Rgba([10, 200, 90, 255]));
        assert!(matches!(variants(&img).last().unwrap().backdrop, Backdrop::Plain));
    }

    #[test]
    fn variants_are_deterministic_for_the_same_capture() {
        let img = RgbaImage::from_pixel(20, 20, image::Rgba([200, 60, 40, 255]));
        let (a, b) = (variants(&img), variants(&img));
        for (sa, sb) in a.iter().zip(&b) {
            match (&sa.backdrop, &sb.backdrop) {
                (Backdrop::Linear { c1: a1, c2: a2 }, Backdrop::Linear { c1: b1, c2: b2 }) => {
                    assert_eq!((a1.0, a1.1, a1.2), (b1.0, b1.1, b1.2), "{}", sa.name);
                    assert_eq!((a2.0, a2.1, a2.2), (b2.0, b2.1, b2.2), "{}", sa.name);
                }
                (Backdrop::Plain, Backdrop::Plain) => {}
                (Backdrop::Aurora { base: ab, blobs: abl }, Backdrop::Aurora { base: bb, blobs: bbl }) => {
                    assert_eq!((ab.0, ab.1, ab.2), (bb.0, bb.1, bb.2), "{}", sa.name);
                    assert_eq!(abl.len(), bbl.len(), "{}", sa.name);
                }
                _ => panic!("{} changed backdrop kind between calls", sa.name),
            }
        }
    }

    #[test]
    fn a_grayscale_capture_still_produces_seven_finite_styles() {
        // No dominant hue: falls back to the indigo default rather than NaN.
        let img = RgbaImage::from_pixel(20, 20, image::Rgba([128, 128, 128, 255]));
        for style in variants(&img) {
            if let Backdrop::Linear { c1, c2 } = &style.backdrop {
                assert!(c1.0.is_finite() && c1.1.is_finite() && c1.2.is_finite(), "{}", style.name);
                assert!(c2.0.is_finite() && c2.1.is_finite() && c2.2.is_finite(), "{}", style.name);
            }
        }
    }
}
