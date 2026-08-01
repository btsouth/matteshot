//! Annotation rendering: arrows, freehand strokes, boxes, text, and
//! pixelate-redaction, drawn on
//! the content layer in raw-image coordinates so they survive framing
//! changes and export at any scale.

use image::{Rgba, RgbaImage};
use windows::core::w;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, DrawTextW,
    GdiFlush, SelectObject, SetBkMode, SetTextColor, ANTIALIASED_QUALITY, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DEFAULT_CHARSET, DIB_RGB_COLORS, DT_CALCRECT, DT_LEFT,
    FF_DONTCARE, TRANSPARENT,
};

/// Annotation palette: red, amber, blue, white.
pub const COLORS: [[u8; 3]; 4] = [
    [239, 68, 68],
    [251, 191, 36],
    [96, 165, 250],
    [245, 245, 245],
];

#[derive(Clone)]
pub enum Shape {
    Arrow { from: (f32, f32), to: (f32, f32) },
    Line { from: (f32, f32), to: (f32, f32) },
    Freehand { points: Vec<(f32, f32)> },
    Rect { a: (f32, f32), b: (f32, f32) },
    Ellipse { a: (f32, f32), b: (f32, f32) },
    /// Translucent marker fill.
    Highlight { a: (f32, f32), b: (f32, f32) },
    Text { pos: (f32, f32), text: String },
    Blur { a: (f32, f32), b: (f32, f32) },
    /// Numbered step badge.
    Counter { pos: (f32, f32), n: u32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextStyle {
    Shadow,
    Box,
}

#[derive(Clone)]
pub struct Annotation {
    pub shape: Shape,
    pub color: usize,
    /// Stroke/text size multiplier (S/M/L in the editor).
    pub size: f32,
    pub text_style: TextStyle,
    pub text_box_opacity: f32,
}

fn blend(img: &mut RgbaImage, x: i32, y: i32, color: [u8; 3], a: f32) {
    if x < 0 || y < 0 || x >= img.width() as i32 || y >= img.height() as i32 || a <= 0.0 {
        return;
    }
    let p = img.get_pixel_mut(x as u32, y as u32);
    for c in 0..3 {
        p[c] = (color[c] as f32 * a + p[c] as f32 * (1.0 - a)) as u8;
    }
}

/// Thick antialiased line. Per scanline, only a tight conservative band
/// around the capsule is evaluated — iterating the full bounding box makes
/// long diagonal strokes quadratically expensive.
fn line(img: &mut RgbaImage, from: (f32, f32), to: (f32, f32), stroke: f32, color: [u8; 3]) {
    let r = stroke / 2.0;
    let rr = r + 0.75;
    let (x0, y0, x1, y1) = (from.0, from.1, to.0, to.1);
    let (dx, dy) = (x1 - x0, y1 - y0);
    let len2 = (dx * dx + dy * dy).max(1e-6);
    let len = len2.sqrt();
    let min_y = (y0.min(y1) - rr).floor() as i32;
    let max_y = (y0.max(y1) + rr).ceil() as i32;
    for y in min_y..=max_y {
        let py = y as f32 + 0.5;
        let (mut lo, mut hi) = (f32::MAX, f32::MIN);
        // Endpoint caps.
        for (ex, ey) in [(x0, y0), (x1, y1)] {
            let d = (py - ey).abs();
            if d <= rr {
                let w = (rr * rr - d * d).max(0.0).sqrt();
                lo = lo.min(ex - w);
                hi = hi.max(ex + w);
            }
        }
        // Body band.
        if dy.abs() > 1e-3 {
            let t = (py - y0) / dy;
            if (-0.05..=1.05).contains(&t) {
                let xc = x0 + t * dx;
                let w = rr * len / dy.abs();
                lo = lo.min(xc - w);
                hi = hi.max(xc + w);
            }
        } else if (py - y0).abs() <= rr {
            lo = lo.min(x0.min(x1) - rr);
            hi = hi.max(x0.max(x1) + rr);
        }
        if lo > hi {
            continue;
        }
        // Clamp the band to the true horizontal extent.
        lo = lo.max(x0.min(x1) - rr);
        hi = hi.min(x0.max(x1) + rr);
        for x in lo.floor() as i32..=hi.ceil() as i32 {
            let px = x as f32 + 0.5;
            let t = (((px - x0) * dx + (py - y0) * dy) / len2).clamp(0.0, 1.0);
            let (cx, cy) = (x0 + t * dx, y0 + t * dy);
            let d = ((px - cx).powi(2) + (py - cy).powi(2)).sqrt();
            blend(img, x, y, color, (r - d + 0.5).clamp(0.0, 1.0));
        }
    }
}

/// Filled triangle via barycentric coverage (for arrowheads).
fn triangle(img: &mut RgbaImage, p: [(f32, f32); 3], color: [u8; 3]) {
    let min_x = p.iter().map(|q| q.0).fold(f32::MAX, f32::min).floor() as i32;
    let max_x = p.iter().map(|q| q.0).fold(f32::MIN, f32::max).ceil() as i32;
    let min_y = p.iter().map(|q| q.1).fold(f32::MAX, f32::min).floor() as i32;
    let max_y = p.iter().map(|q| q.1).fold(f32::MIN, f32::max).ceil() as i32;
    let area = (p[1].0 - p[0].0) * (p[2].1 - p[0].1) - (p[2].0 - p[0].0) * (p[1].1 - p[0].1);
    if area.abs() < 1e-6 {
        return;
    }
    // 2x2 supersampling for soft edges.
    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let mut cov = 0.0;
            for (ox, oy) in [(0.25, 0.25), (0.75, 0.25), (0.25, 0.75), (0.75, 0.75)] {
                let (px, py) = (x as f32 + ox, y as f32 + oy);
                let w0 = ((p[1].0 - px) * (p[2].1 - py) - (p[2].0 - px) * (p[1].1 - py)) / area;
                let w1 = ((p[2].0 - px) * (p[0].1 - py) - (p[0].0 - px) * (p[2].1 - py)) / area;
                let w2 = 1.0 - w0 - w1;
                if w0 >= 0.0 && w1 >= 0.0 && w2 >= 0.0 {
                    cov += 0.25;
                }
            }
            blend(img, x, y, color, cov);
        }
    }
}

fn pixelate(img: &mut RgbaImage, a: (f32, f32), b: (f32, f32), block: u32) {
    let (w, h) = (img.width() as i32, img.height() as i32);
    let x0 = (a.0.min(b.0) as i32).clamp(0, w);
    let x1 = (a.0.max(b.0) as i32).clamp(0, w);
    let y0 = (a.1.min(b.1) as i32).clamp(0, h);
    let y1 = (a.1.max(b.1) as i32).clamp(0, h);
    let block = block.max(4) as i32;
    let mut by = y0;
    while by < y1 {
        let mut bx = x0;
        let bh = (by + block).min(y1);
        while bx < x1 {
            let bw = (bx + block).min(x1);
            let (mut r, mut g, mut b_, mut n) = (0u64, 0u64, 0u64, 0u64);
            for y in by..bh {
                for x in bx..bw {
                    let p = img.get_pixel(x as u32, y as u32);
                    r += p[0] as u64;
                    g += p[1] as u64;
                    b_ += p[2] as u64;
                    n += 1;
                }
            }
            if let (Some(r), Some(g), Some(b)) =
                (r.checked_div(n), g.checked_div(n), b_.checked_div(n))
            {
                let avg = Rgba([r as u8, g as u8, b as u8, 255]);
                for y in by..bh {
                    for x in bx..bw {
                        img.put_pixel(x as u32, y as u32, avg);
                    }
                }
            }
            bx += block;
        }
        by += block;
    }
}

/// Rasterize text via GDI (Segoe UI, grayscale AA) into an alpha mask.
fn raster_text(text: &str, px_height: i32) -> Option<(Vec<u8>, i32, i32)> {
    unsafe {
        let hdc = CreateCompatibleDC(None);
        let font = CreateFontW(
            -px_height,
            0,
            0,
            0,
            600,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            0,
            0,
            ANTIALIASED_QUALITY.0 as u32,
            FF_DONTCARE.0 as u32,
            w!("Segoe UI"),
        );
        let old_font = SelectObject(hdc, font);

        let mut wtext: Vec<u16> = text.encode_utf16().collect();
        let mut rc = windows::Win32::Foundation::RECT::default();
        DrawTextW(hdc, &mut wtext, &mut rc, DT_CALCRECT | DT_LEFT);
        let (w, h) = ((rc.right - rc.left).max(1) + 4, (rc.bottom - rc.top).max(1) + 2);

        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let Ok(bmp) = CreateDIBSection(hdc, &info, DIB_RGB_COLORS, &mut bits, None, 0) else {
            SelectObject(hdc, old_font);
            let _ = DeleteObject(font);
            let _ = DeleteDC(hdc);
            return None;
        };
        let old_bmp = SelectObject(hdc, bmp);
        // Black background is already zeroed; draw white text.
        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, windows::Win32::Foundation::COLORREF(0x00FFFFFF));
        let mut draw_rc = windows::Win32::Foundation::RECT { left: 2, top: 1, right: w, bottom: h };
        DrawTextW(hdc, &mut wtext, &mut draw_rc, DT_LEFT);
        let _ = GdiFlush();

        let src = std::slice::from_raw_parts(bits as *const u8, (w * h * 4) as usize);
        // Any channel works — white on black, grayscale AA.
        let alpha: Vec<u8> = src.chunks_exact(4).map(|p| p[2]).collect();

        SelectObject(hdc, old_bmp);
        SelectObject(hdc, old_font);
        let _ = DeleteObject(bmp);
        let _ = DeleteObject(font);
        let _ = DeleteDC(hdc);
        Some((alpha, w, h))
    }
}

/// Render annotations onto `img`. Coordinates in the annotations are in
/// raw-capture space; `scale` maps them into `img`'s space and `offset`
/// shifts them (e.g. onto a composited canvas at the content's position).
pub fn render(
    img: &mut RgbaImage,
    anns: &[Annotation],
    scale: f32,
    offset: (f32, f32),
    caret: Option<usize>,
) {
    render_with_metric(img, anns, scale, scale, offset, caret);
}

/// Return the exact pixel box produced by the caption rasterizer. Selection,
/// hit testing, and rendering all use this measurement so the editor outline
/// cannot drift away from the text it represents.
pub(crate) fn caption_text_size(
    text: &str,
    size: f32,
    metric_scale: f32,
) -> Option<(i32, i32)> {
    if text.is_empty() {
        return None;
    }
    let px_h = (24.0 * metric_scale * size).max(12.0) as i32;
    raster_text(text, px_h).map(|(_, width, height)| (width, height))
}

fn rounded_plate(
    img: &mut RgbaImage,
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
    radius: f32,
    opacity: f32,
) {
    let width = (right - left).max(1) as f32;
    let height = (bottom - top).max(1) as f32;
    let radius = radius.min(width / 2.0).min(height / 2.0).max(1.0);
    for y in top..bottom {
        for x in left..right {
            let px = x as f32 + 0.5 - left as f32;
            let py = y as f32 + 0.5 - top as f32;
            let qx = (px - width / 2.0).abs() - (width / 2.0 - radius);
            let qy = (py - height / 2.0).abs() - (height / 2.0 - radius);
            let distance = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt()
                + qx.max(qy).min(0.0)
                - radius;
            let coverage = (0.5 - distance).clamp(0.0, 1.0);
            blend(
                img,
                x,
                y,
                [12, 14, 20],
                coverage * opacity.clamp(0.0, 1.0),
            );
        }
    }
}

/// Draw a video caption at an already-expanded destination-pixel position.
/// `boxed` adds a translucent rounded plate for busy footage; shadow mode is
/// deliberately stronger than screenshot text so captions remain readable.
pub struct CaptionOptions {
    pub color_index: usize,
    pub size: f32,
    pub metric_scale: f32,
    pub offset: (f32, f32),
    pub boxed: bool,
    pub box_opacity: f32,
}

pub fn render_caption(
    img: &mut RgbaImage,
    pos: (f32, f32),
    text: &str,
    options: CaptionOptions,
) {
    if text.is_empty() {
        return;
    }
    let px_h = (24.0 * options.metric_scale * options.size).max(12.0) as i32;
    let Some((alpha, tw, th)) = raster_text(text, px_h) else {
        return;
    };
    let color = COLORS[options.color_index.min(COLORS.len() - 1)];
    let ox = (pos.0 + options.offset.0) as i32;
    let oy = (pos.1 + options.offset.1) as i32;
    if options.boxed {
        // A compact plate reads like modern UI instead of a large subtitle
        // banner. Opacity is user-controlled by the video editor.
        let pad_x = (px_h as f32 * 0.30).round() as i32;
        let pad_y = (px_h as f32 * 0.16).round() as i32;
        rounded_plate(
            img,
            ox - pad_x,
            oy - pad_y,
            ox + tw + pad_x,
            oy + th + pad_y,
            px_h as f32 * 0.24,
            options.box_opacity,
        );
    }
    let shadow = if options.boxed {
        (options.metric_scale * options.size)
            .round()
            .clamp(1.0, 3.0) as i32
    } else {
        (options.metric_scale * options.size * 2.0)
            .round()
            .clamp(1.0, 5.0) as i32
    };
    let shadow_offsets: &[(i32, i32)] = if options.boxed {
        &[(0, shadow), (shadow / 2, shadow)]
    } else {
        &[
            (-shadow, 0),
            (shadow, 0),
            (0, -shadow),
            (0, shadow),
            (shadow, shadow),
        ]
    };
    let shadow_alpha = if options.boxed { 0.46 } else { 0.72 };
    for y in 0..th {
        for x in 0..tw {
            let a = alpha[(y * tw + x) as usize] as f32 / 255.0;
            if a > 0.0 {
                for (nx, ny) in shadow_offsets {
                    blend(
                        img,
                        ox + x + *nx,
                        oy + y + *ny,
                        [8, 9, 13],
                        a * shadow_alpha,
                    );
                }
            }
        }
    }
    for y in 0..th {
        for x in 0..tw {
            let a = alpha[(y * tw + x) as usize] as f32 / 255.0;
            blend(img, ox + x, oy + y, color, a);
        }
    }
}

/// Render when coordinates are already in destination pixels but strokes and
/// text still need to scale for preview/output resolution. Video annotations
/// use this because their normalized coordinates are expanded before render.
pub fn render_with_metric(
    img: &mut RgbaImage,
    anns: &[Annotation],
    coordinate_scale: f32,
    metric_scale: f32,
    offset: (f32, f32),
    caret: Option<usize>,
) {
    for (i, ann) in anns.iter().enumerate() {
        let stroke = (5.0 * metric_scale * ann.size).max(2.0);
        let color = COLORS[ann.color.min(COLORS.len() - 1)];
        let s = |p: (f32, f32)| {
            (
                p.0 * coordinate_scale + offset.0,
                p.1 * coordinate_scale + offset.1,
            )
        };
        match &ann.shape {
            Shape::Arrow { from, to } => {
                let (f, t) = (s(*from), s(*to));
                let (dx, dy) = (t.0 - f.0, t.1 - f.1);
                let len = (dx * dx + dy * dy).sqrt().max(1e-3);
                let (ux, uy) = (dx / len, dy / len);
                let head = (stroke * 3.4)
                    .min(len * 0.5)
                    .max(14.0 * metric_scale * ann.size);
                // Shorten the shaft so it doesn't poke out of the head.
                let shaft_end = (t.0 - ux * head * 0.7, t.1 - uy * head * 0.7);
                line(img, f, shaft_end, stroke, color);
                let (px, py) = (-uy, ux);
                triangle(
                    img,
                    [
                        t,
                        (t.0 - ux * head + px * head * 0.5, t.1 - uy * head + py * head * 0.5),
                        (t.0 - ux * head - px * head * 0.5, t.1 - uy * head - py * head * 0.5),
                    ],
                    color,
                );
            }
            Shape::Line { from, to } => {
                line(img, s(*from), s(*to), stroke, color);
            }
            Shape::Freehand { points } => {
                for segment in points.windows(2) {
                    line(img, s(segment[0]), s(segment[1]), stroke, color);
                }
            }
            Shape::Rect { a, b } => {
                let (pa, pb) = (s(*a), s(*b));
                let (x0, y0) = (pa.0.min(pb.0), pa.1.min(pb.1));
                let (x1, y1) = (pa.0.max(pb.0), pa.1.max(pb.1));
                line(img, (x0, y0), (x1, y0), stroke, color);
                line(img, (x1, y0), (x1, y1), stroke, color);
                line(img, (x1, y1), (x0, y1), stroke, color);
                line(img, (x0, y1), (x0, y0), stroke, color);
            }
            Shape::Ellipse { a, b } => {
                let (pa, pb) = (s(*a), s(*b));
                let (cx, cy) = ((pa.0 + pb.0) / 2.0, (pa.1 + pb.1) / 2.0);
                let (rx, ry) = ((pa.0 - pb.0).abs() / 2.0, (pa.1 - pb.1).abs() / 2.0);
                let perim = std::f32::consts::PI * (3.0 * (rx + ry))
                    - ((3.0 * rx + ry) * (rx + 3.0 * ry)).sqrt();
                let steps = ((perim / 4.0) as usize).clamp(48, 720);
                let mut prev = (cx + rx, cy);
                for k in 1..=steps {
                    let t = k as f32 / steps as f32 * std::f32::consts::TAU;
                    let pt = (cx + rx * t.cos(), cy + ry * t.sin());
                    line(img, prev, pt, stroke, color);
                    prev = pt;
                }
            }
            Shape::Highlight { a, b } => {
                let (pa, pb) = (s(*a), s(*b));
                let (x0, y0) = (pa.0.min(pb.0) as i32, pa.1.min(pb.1) as i32);
                let (x1, y1) = (pa.0.max(pb.0) as i32, pa.1.max(pb.1) as i32);
                for y in y0..y1 {
                    for x in x0..x1 {
                        blend(img, x, y, color, 0.35);
                    }
                }
            }
            Shape::Counter { pos, n } => {
                let c = s(*pos);
                let r = (14.0 * metric_scale * ann.size).max(9.0);
                // Filled badge with AA edge.
                let (min_x, max_x) = ((c.0 - r - 1.0) as i32, (c.0 + r + 1.0) as i32);
                let (min_y, max_y) = ((c.1 - r - 1.0) as i32, (c.1 + r + 1.0) as i32);
                for y in min_y..=max_y {
                    for x in min_x..=max_x {
                        let d = ((x as f32 + 0.5 - c.0).powi(2)
                            + (y as f32 + 0.5 - c.1).powi(2))
                        .sqrt();
                        blend(img, x, y, color, (r - d + 0.5).clamp(0.0, 1.0));
                    }
                }
                let num = n.to_string();
                let px_h = (r * 1.15) as i32;
                if let Some((alpha, tw, th)) = raster_text(&num, px_h) {
                    let (ox, oy) = ((c.0 - tw as f32 / 2.0) as i32, (c.1 - th as f32 / 2.0) as i32);
                    // White on colored badge (dark text on the white swatch).
                    let text_color =
                        if ann.color == 3 { [30u8, 28, 26] } else { [255u8, 255, 255] };
                    for y in 0..th {
                        for x in 0..tw {
                            let a = alpha[(y * tw + x) as usize] as f32 / 255.0;
                            blend(img, ox + x, oy + y, text_color, a);
                        }
                    }
                }
            }
            Shape::Blur { a, b } => {
                pixelate(img, s(*a), s(*b), (14.0 * metric_scale) as u32);
            }
            Shape::Text { pos, text } => {
                let shown = if caret == Some(i) {
                    format!("{text}\u{2502}")
                } else if text.is_empty() {
                    continue;
                } else {
                    text.clone()
                };
                render_caption(
                    img,
                    s(*pos),
                    &shown,
                    CaptionOptions {
                        color_index: ann.color,
                        size: ann.size,
                        metric_scale,
                        offset: (0.0, 0.0),
                        boxed: ann.text_style == TextStyle::Box,
                        box_opacity: ann.text_box_opacity,
                    },
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use image::{Rgba, RgbaImage};

    use super::{render, Annotation, Shape, TextStyle};

    #[test]
    fn freehand_strokes_render_every_segment() {
        let mut image = RgbaImage::from_pixel(64, 64, Rgba([0, 0, 0, 255]));
        let annotation = Annotation {
            shape: Shape::Freehand {
                points: vec![(8.0, 8.0), (32.0, 8.0), (32.0, 40.0)],
            },
            color: 0,
            size: 1.0,
            text_style: TextStyle::Shadow,
            text_box_opacity: 1.0,
        };

        render(&mut image, &[annotation], 1.0, (0.0, 0.0), None);

        assert!(image.get_pixel(20, 8)[0] > 0);
        assert!(image.get_pixel(32, 28)[0] > 0);
    }
}
