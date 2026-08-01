//! Compositing: rounded corners, soft shadow, padding, styled backdrop.

use image::RgbaImage;
use rayon::prelude::*;

use crate::style::{Backdrop, Rgb, Style};

/// Product default for automatic matte framing. Editors may override it
/// within their 4–18% range, while hand-tuned assets can opt out entirely.
pub const DEFAULT_PAD_FACTOR: f32 = 0.08;

/// Antialiased coverage for a rounded-rect mask at pixel (x, y).
fn rounded_rect_coverage(x: f32, y: f32, w: f32, h: f32, radius: f32) -> f32 {
    // Signed distance to a rounded rectangle centered in [0,w]x[0,h].
    let (cx, cy) = (w / 2.0, h / 2.0);
    let (qx, qy) = ((x - cx).abs() - (cx - radius), (y - cy).abs() - (cy - radius));
    let dist = if qx > 0.0 && qy > 0.0 {
        (qx * qx + qy * qy).sqrt() - radius
    } else {
        qx.max(qy) - radius
    };
    (0.5 - dist).clamp(0.0, 1.0)
}

/// One horizontal box-blur pass, every row independent so they run in parallel.
fn box_blur_rows(src: &[f32], dst: &mut [f32], w: usize, radius: usize) {
    let norm = 1.0 / (2 * radius + 1) as f32;
    dst.par_chunks_mut(w)
        .zip(src.par_chunks(w))
        .for_each(|(out, row)| {
            // Edge pixels are replicated, matching a clamped sample.
            let mut acc: f32 =
                row[0] * radius as f32 + row[..=(radius.min(w - 1))].iter().sum::<f32>();
            for x in 0..w {
                out[x] = acc * norm;
                acc += row[(x + radius + 1).min(w - 1)] - row[x.saturating_sub(radius)];
            }
        });
}

/// `src` is `w` wide and `h` tall; `dst` comes back `h` wide and `w` tall.
fn transpose(src: &[f32], dst: &mut [f32], w: usize, h: usize) {
    dst.par_chunks_mut(h).enumerate().for_each(|(x, column)| {
        for y in 0..h {
            column[y] = src[y * w + x];
        }
    });
}

fn blur_axis(mut src: Vec<f32>, w: usize, radius: usize) -> Vec<f32> {
    let mut dst = vec![0f32; src.len()];
    for _ in 0..3 {
        box_blur_rows(&src, &mut dst, w, radius);
        std::mem::swap(&mut src, &mut dst);
    }
    src
}

/// Separable box blur on a single channel, repeated 3x to approximate gaussian.
///
/// The two axes are independent linear operators, so they commute: running all
/// three horizontal passes before all three vertical ones gives the same result
/// as alternating. That matters because it lets the vertical passes run as row
/// passes over a transposed buffer, which is both cache-friendly and
/// parallelisable. This blur dominates matte composition, and composition runs
/// on every mouse move while the padding slider is dragged.
fn blur_alpha(buf: &mut [f32], w: usize, h: usize, radius: usize) {
    if radius == 0 || w == 0 || h == 0 {
        return;
    }
    let horizontal = blur_axis(buf.to_vec(), w, radius);
    let mut transposed = vec![0f32; buf.len()];
    transpose(&horizontal, &mut transposed, w, h);
    let vertical = blur_axis(transposed, h, radius);
    transpose(&vertical, buf, h, w);
}

fn backdrop_at(backdrop: &Backdrop, x: usize, y: usize, cw: usize, ch: usize) -> Rgb {
    match backdrop {
        // Handled before composition ever runs.
        Backdrop::Plain => Rgb(0.0, 0.0, 0.0),
        Backdrop::Linear { c1, c2 } => {
            let t = (x as f32 / cw as f32 + y as f32 / ch as f32) / 2.0;
            let shade = 1.0 - 0.08 * (y as f32 / ch as f32);
            Rgb(
                (c1.0 + (c2.0 - c1.0) * t) * shade,
                (c1.1 + (c2.1 - c1.1) * t) * shade,
                (c1.2 + (c2.2 - c1.2) * t) * shade,
            )
        }
        Backdrop::Aurora { base, blobs } => {
            let max_dim = cw.max(ch) as f32;
            let (px, py) = (x as f32, y as f32);
            let (mut r, mut g, mut b) = (base.0, base.1, base.2);
            for blob in blobs {
                let dx = px - blob.cx * cw as f32;
                let dy = py - blob.cy * ch as f32;
                let radius = blob.r * max_dim;
                let d2 = (dx * dx + dy * dy) / (radius * radius);
                let w = (-d2 * 2.2).exp();
                r += (blob.color.0 - base.0) * w;
                g += (blob.color.1 - base.1) * w;
                b += (blob.color.2 - base.2) * w;
            }
            Rgb(r.clamp(0.0, 1.0), g.clamp(0.0, 1.0), b.clamp(0.0, 1.0))
        }
    }
}

#[derive(Clone, Copy)]
pub struct ComposeOpts {
    /// Scales the clamp bounds on padding/corners/shadow so a downscaled
    /// preview composes proportionally to the full-size render.
    pub metric_scale: f32,
    /// Padding as a fraction of the content's geometric mean dimension.
    pub pad_factor: f32,
    /// Target canvas aspect ratio (w/h); the matte extends on one axis.
    pub aspect: Option<f32>,
}

impl Default for ComposeOpts {
    fn default() -> Self {
        ComposeOpts { metric_scale: 1.0, pad_factor: DEFAULT_PAD_FACTOR, aspect: None }
    }
}

pub fn compose_scaled(window: &RgbaImage, style: &Style, metric_scale: f32) -> RgbaImage {
    compose_with(window, style, &ComposeOpts { metric_scale, ..Default::default() })
}

/// Export-path composite: applies supersampling before framing. Large
/// captures skip upscaling — it only bloats files viewers have to shrink.
pub fn export(
    raw: &RgbaImage,
    style: &Style,
    pad_factor: f32,
    aspect: Option<f32>,
    export_scale: u32,
) -> RgbaImage {
    if is_plain(style) {
        // "Just the screenshot": native pixels, untouched.
        return raw.clone();
    }
    let scale = if raw.width().max(raw.height()) >= 1600 { 1 } else { export_scale.clamp(1, 4) };
    let opts = ComposeOpts { metric_scale: scale as f32, pad_factor, aspect };
    if scale > 1 {
        let up = image::imageops::resize(
            raw,
            raw.width() * scale,
            raw.height() * scale,
            image::imageops::FilterType::Lanczos3,
        );
        compose_with(&up, style, &opts)
    } else {
        compose_with(raw, style, &opts)
    }
}

/// Content placement inside the composite, exposed so interactive UIs can
/// map composite coordinates back to content coordinates.
pub struct Layout {
    pub pad_x: usize,
    pub pad_y: usize,
    pub pad: usize,
}

pub fn layout(w: usize, h: usize, opts: &ComposeOpts) -> Layout {
    let metric_scale = opts.metric_scale;
    // Geometric mean, not max: extreme aspect ratios (full-width strips)
    // must not get padding sized to their long edge.
    let basis = ((w * h) as f32).sqrt();
    let pad = (basis * opts.pad_factor)
        .clamp(24.0 * metric_scale, 400.0 * metric_scale)
        .max(4.0) as usize;

    // Aspect targeting: extend padding on whichever axis falls short.
    let (mut pad_x, mut pad_y) = (pad, pad);
    if let Some(a) = opts.aspect {
        let (cw, ch) = ((w + pad * 2) as f32, (h + pad * 2) as f32);
        if cw / ch < a {
            pad_x += (((ch * a) - cw) / 2.0).max(0.0) as usize;
        } else {
            pad_y += (((cw / a) - ch) / 2.0).max(0.0) as usize;
        }
    }
    Layout { pad_x, pad_y, pad }
}

/// The "None" matte: no backdrop, no padding, no rounding.
pub fn is_plain(style: &Style) -> bool {
    matches!(style.backdrop, crate::style::Backdrop::Plain)
}

pub fn compose_with(window: &RgbaImage, style: &Style, opts: &ComposeOpts) -> RgbaImage {
    if is_plain(style) {
        return window.clone();
    }
    let mut canvas = compose_base(
        window.width() as usize,
        window.height() as usize,
        style,
        opts,
    );
    blend_content(&mut canvas, window, opts);
    canvas
}

/// Backdrop + shadow for content of the given size — everything except the
/// content itself. Cacheable: interactive UIs reuse it across annotation
/// edits, which only need `blend_content` on a clone.
pub fn compose_base(w: usize, h: usize, style: &Style, opts: &ComposeOpts) -> RgbaImage {
    let metric_scale = opts.metric_scale;
    let max_dim = w.max(h) as f32;

    let Layout { pad_x, pad_y, pad } = layout(w, h, opts);
    let corner = (max_dim * 0.012).clamp(10.0 * metric_scale, 28.0 * metric_scale).max(2.0);
    let shadow_blur = (pad as f32 * 0.38).min(120.0 * metric_scale) as usize;
    let shadow_offset_y = (pad as f32 * 0.16) as usize;
    let shadow_strength = 0.42;

    let (cw, ch) = (w + pad_x * 2, h + pad_y * 2);

    // 1) Backdrop. Rows are independent, and Aurora's per-pixel blob maths is
    // the most expensive backdrop we ship.
    let mut canvas = RgbaImage::new(cw as u32, ch as u32);
    canvas
        .as_mut()
        .par_chunks_mut(cw * 4)
        .enumerate()
        .for_each(|(y, row)| {
            for x in 0..cw {
                let c = backdrop_at(&style.backdrop, x, y, cw, ch);
                let pixel = &mut row[x * 4..x * 4 + 4];
                pixel[0] = (c.0 * 255.0) as u8;
                pixel[1] = (c.1 * 255.0) as u8;
                pixel[2] = (c.2 * 255.0) as u8;
                pixel[3] = 255;
            }
        });

    // 2) Shadow: blurred rounded-rect silhouette, offset downward.
    let mut shadow = vec![0f32; cw * ch];
    shadow
        .par_chunks_mut(cw)
        .skip(pad_y + shadow_offset_y)
        .take(h)
        .enumerate()
        .for_each(|(y, row)| {
            for x in 0..w {
                row[pad_x + x] = rounded_rect_coverage(
                    x as f32 + 0.5,
                    y as f32 + 0.5,
                    w as f32,
                    h as f32,
                    corner,
                );
            }
        });
    blur_alpha(&mut shadow, cw, ch, shadow_blur);
    canvas
        .as_mut()
        .par_chunks_mut(cw * 4)
        .zip(shadow.par_chunks(cw))
        .for_each(|(row, coverage)| {
            for x in 0..cw {
                let a = coverage[x] * shadow_strength;
                if a > 0.002 {
                    // Colour channels only; alpha stays put.
                    for channel in &mut row[x * 4..x * 4 + 3] {
                        *channel = (*channel as f32 * (1.0 - a)) as u8;
                    }
                }
            }
        });

    canvas
}

/// Content layer with our own rounded corners (WGC alpha varies by OS
/// build, so we mask unconditionally for a consistent result).
pub fn blend_content(canvas: &mut RgbaImage, window: &RgbaImage, opts: &ComposeOpts) {
    let metric_scale = opts.metric_scale;
    let (w, h) = (window.width() as usize, window.height() as usize);
    let max_dim = w.max(h) as f32;
    let Layout { pad_x, pad_y, .. } = layout(w, h, opts);
    let corner = (max_dim * 0.012).clamp(10.0 * metric_scale, 28.0 * metric_scale).max(2.0);

    let canvas_w = canvas.width() as usize;
    let src = window.as_raw();
    canvas
        .as_mut()
        .par_chunks_mut(canvas_w * 4)
        .skip(pad_y)
        .take(h)
        .enumerate()
        .for_each(|(y, row)| {
            let src_row = &src[y * w * 4..(y + 1) * w * 4];
            let dst_row = &mut row[pad_x * 4..(pad_x + w) * 4];
            for x in 0..w {
                let s = &src_row[x * 4..x * 4 + 4];
                let d = &mut dst_row[x * 4..x * 4 + 4];
                let corner_pixel = (x < corner as usize || x + corner as usize >= w)
                    && (y < corner as usize || y + corner as usize >= h);
                if s[3] == 255 && !corner_pixel {
                    d[..3].copy_from_slice(&s[..3]);
                    continue;
                }
                let cov = if corner_pixel {
                    rounded_rect_coverage(
                        x as f32 + 0.5,
                        y as f32 + 0.5,
                        w as f32,
                        h as f32,
                        corner,
                    )
                } else {
                    1.0
                };
                let a = cov * (s[3] as f32 / 255.0);
                if a > 0.0 {
                    for c in 0..3 {
                        d[c] = (s[c] as f32 * a + d[c] as f32 * (1.0 - a)) as u8;
                    }
                }
            }
        });
}

/// Blend a tightly packed BGRA frame directly into an RGBA matte. Video
/// export uses this to avoid allocating and channel-swapping a second
/// full-resolution image for every decoded frame.
pub fn blend_bgra_content(
    canvas: &mut RgbaImage,
    bgra: &[u8],
    w: usize,
    h: usize,
    opts: &ComposeOpts,
) {
    debug_assert!(bgra.len() >= w * h * 4);
    let metric_scale = opts.metric_scale;
    let max_dim = w.max(h) as f32;
    let Layout { pad_x, pad_y, .. } = layout(w, h, opts);
    let corner = (max_dim * 0.012).clamp(10.0 * metric_scale, 28.0 * metric_scale).max(2.0);
    let canvas_w = canvas.width() as usize;
    canvas
        .as_mut()
        .par_chunks_mut(canvas_w * 4)
        .skip(pad_y)
        .take(h)
        .enumerate()
        .for_each(|(y, row)| {
            let src_row = &bgra[y * w * 4..(y + 1) * w * 4];
            let dst_row = &mut row[pad_x * 4..(pad_x + w) * 4];
            for x in 0..w {
                let s = &src_row[x * 4..x * 4 + 4];
                let d = &mut dst_row[x * 4..x * 4 + 4];
                let corner_pixel = (x < corner as usize || x + corner as usize >= w)
                    && (y < corner as usize || y + corner as usize >= h);
                if s[3] == 255 && !corner_pixel {
                    d[0] = s[2];
                    d[1] = s[1];
                    d[2] = s[0];
                    continue;
                }
                let cov = if corner_pixel {
                    rounded_rect_coverage(
                        x as f32 + 0.5,
                        y as f32 + 0.5,
                        w as f32,
                        h as f32,
                        corner,
                    )
                } else {
                    1.0
                };
                let a = cov * (s[3] as f32 / 255.0);
                if a > 0.0 {
                    d[0] = (s[2] as f32 * a + d[0] as f32 * (1.0 - a)) as u8;
                    d[1] = (s[1] as f32 * a + d[1] as f32 * (1.0 - a)) as u8;
                    d[2] = (s[0] as f32 * a + d[2] as f32 * (1.0 - a)) as u8;
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::{compose_with, ComposeOpts};
    use crate::style::{Backdrop, Rgb, Style};
    use image::{Rgba, RgbaImage};

    fn style() -> Style {
        Style {
            name: "Test",
            backdrop: Backdrop::Linear {
                c1: Rgb(0.2, 0.3, 0.6),
                c2: Rgb(0.4, 0.2, 0.5),
            },
        }
    }

    #[test]
    fn forced_aspect_extends_the_matte_without_distorting_content() {
        let raw = RgbaImage::from_pixel(400, 225, Rgba([24, 32, 48, 255]));
        let output = compose_with(
            &raw,
            &style(),
            &ComposeOpts {
                aspect: Some(1.0),
                ..Default::default()
            },
        );
        assert!((output.width() as i64 - output.height() as i64).abs() <= 1);
        assert!(output.width() > raw.width());
    }

    #[test]
    fn padding_control_changes_canvas_size_monotonically() {
        let raw = RgbaImage::from_pixel(800, 450, Rgba([24, 32, 48, 255]));
        let tight = compose_with(
            &raw,
            &style(),
            &ComposeOpts {
                pad_factor: 0.04,
                ..Default::default()
            },
        );
        let roomy = compose_with(
            &raw,
            &style(),
            &ComposeOpts {
                pad_factor: 0.18,
                ..Default::default()
            },
        );
        let default = compose_with(&raw, &style(), &ComposeOpts::default());
        assert!(default.width() > tight.width());
        assert!(default.width() < roomy.width());
        assert!(roomy.width() > tight.width());
        assert!(roomy.height() > tight.height());
    }
}

#[cfg(test)]
mod blur_tests {
    use super::*;

    /// The original alternating implementation, kept as the reference the fast
    /// path has to agree with.
    fn blur_alpha_reference(buf: &mut [f32], w: usize, h: usize, radius: usize) {
        if radius == 0 {
            return;
        }
        let mut tmp = vec![0f32; buf.len()];
        let norm = 1.0 / (2 * radius + 1) as f32;
        for _ in 0..3 {
            for y in 0..h {
                let row = &buf[y * w..(y + 1) * w];
                let mut acc: f32 =
                    row[0] * radius as f32 + row[..=(radius.min(w - 1))].iter().sum::<f32>();
                for x in 0..w {
                    tmp[y * w + x] = acc * norm;
                    acc += row[(x + radius + 1).min(w - 1)] - row[x.saturating_sub(radius)];
                }
            }
            for x in 0..w {
                let col = |yy: usize| tmp[yy * w + x];
                let mut acc: f32 =
                    col(0) * radius as f32 + (0..=radius.min(h - 1)).map(col).sum::<f32>();
                for y in 0..h {
                    buf[y * w + x] = acc * norm;
                    acc += col((y + radius + 1).min(h - 1)) - col(y.saturating_sub(radius));
                }
            }
        }
    }

    fn sample(w: usize, h: usize) -> Vec<f32> {
        // Deterministic, asymmetric, and not separable, so an axis mix-up shows.
        (0..w * h)
            .map(|i| {
                let (x, y) = (i % w, i / w);
                if x * 3 % 7 == 0 || y % 5 == 0 {
                    ((x * 13 + y * 29) % 97) as f32 / 97.0
                } else {
                    0.0
                }
            })
            .collect()
    }

    #[test]
    fn parallel_blur_matches_the_alternating_reference() {
        // Non-square, and a radius that exceeds one dimension, to cover the
        // edge clamping on both axes.
        for (w, h, radius) in [(64, 40, 7), (40, 64, 19), (33, 17, 25), (9, 9, 0)] {
            let source = sample(w, h);
            let mut fast = source.clone();
            let mut reference = source.clone();
            blur_alpha(&mut fast, w, h, radius);
            blur_alpha_reference(&mut reference, w, h, radius);
            for (index, (a, b)) in fast.iter().zip(reference.iter()).enumerate() {
                assert!(
                    (a - b).abs() < 1e-4,
                    "{w}x{h} r{radius} differs at {index}: {a} vs {b}"
                );
            }
        }
    }

    #[test]
    fn transpose_round_trips() {
        let (w, h) = (7, 5);
        let source = sample(w, h);
        let mut once = vec![0f32; source.len()];
        let mut back = vec![0f32; source.len()];
        transpose(&source, &mut once, w, h);
        assert_eq!(once[2 * 5 + 3], source[3 * 7 + 2]);
        transpose(&once, &mut back, h, w);
        assert_eq!(back, source);
    }
}
