//! Tweak panel: the optional editor for people who want to adjust the result.
//! Matte swap, padding slider, aspect presets, live preview. Reached with T
//! from the picker; the core loop never sees it.

use anyhow::Result;
use image::RgbaImage;
use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, CreatePen,
    CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect, GetMonitorInfoW,
    HALFTONE, InvalidateRect, RoundRect, SelectObject, SetBkMode, SetStretchBltMode,
    SetTextColor, StretchDIBits, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CLEARTYPE_QUALITY,
    DEFAULT_CHARSET, DIB_RGB_COLORS, DT_CENTER, DT_LEFT, DT_SINGLELINE, DT_VCENTER, FF_DONTCARE,
    HDC, HFONT, HMONITOR, MONITORINFO, PAINTSTRUCT, PS_SOLID, SRCCOPY, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, VK_ESCAPE, VK_RETURN};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    GetWindowLongPtrW, LoadCursorW, PostQuitMessage, RegisterClassW, SetForegroundWindow,
    SetWindowLongPtrW, TranslateMessage, CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, GWLP_USERDATA,
    IDC_ARROW, MSG, WM_CLOSE, WM_DESTROY, WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCCREATE, WM_PAINT, WNDCLASSW, WS_CAPTION,
    WS_SYSMENU, WS_VISIBLE,
};

use crate::compose::{self, ComposeOpts};
use crate::config::Config;
use crate::output;
use crate::style::Style;



const PAD_MIN: f32 = 0.04;
const PAD_MAX: f32 = 0.18;
const ASPECTS: [(&str, Option<f32>); 5] = [
    ("Auto", None),
    ("1:1", Some(1.0)),
    ("4:3", Some(4.0 / 3.0)),
    ("16:9", Some(16.0 / 9.0)),
    ("Social", Some(1.91)),
];

/// Posted when the resident PrtScn hotkey fires mid-tweak.
const WM_RESHOOT: u32 = windows::Win32::UI::WindowsAndMessaging::WM_USER + 42;

#[derive(Clone, Copy, PartialEq)]
enum Ctl {
    Matte(usize),
    Aspect(usize),
    Slider,
    Tool(usize),
    Color(usize),
    Size(usize),
    Undo,
    Clear,
    Ocr,
    Copy,
    Save,
    Edit,
}

const TOOLS: [&str; 8] = ["Arrow", "Line", "Box", "Oval", "Mark", "Text", "Blur", "Step"];
const SIZES: [f32; 3] = [0.7, 1.0, 1.4];

/// What part of an annotation a selector-mode drag grabbed.
#[derive(Clone, Copy, PartialEq)]
enum Grab {
    Whole,
    /// Arrow tail / tip.
    P0,
    P1,
    /// Rect/Blur corner (0 tl, 1 tr, 2 br, 3 bl) after normalization.
    Corner(u8),
}

struct State {
    raw: RgbaImage,
    small: RgbaImage,
    preview_metric: f32,
    styles: Vec<Style>,
    sel: usize,
    pad_factor: f32,
    aspect_idx: usize,
    // Cached preview composite as BGRA.
    preview: Vec<u8>,
    preview_w: i32,
    preview_h: i32,
    controls: Vec<(RECT, Ctl)>,
    preview_box: RECT,
    slider_rect: RECT,
    hover: i32,
    dragging: bool,
    done: bool,
    suspended: bool,
    reshoot: Option<(crate::overlay::Selection, HMONITOR)>,
    /// Cached matte+shadow+content canvas keyed by (matte, pad bits,
    /// aspect) — annotation edits only stamp onto a clone of this.
    base_cache: Option<((usize, u32, usize), RgbaImage)>,
    // Annotations, in raw-capture coordinates.
    anns: Vec<crate::annotate::Annotation>,
    tool: Option<usize>,
    color_idx: usize,
    /// In-progress drag annotation (last element of `anns` while active).
    drawing: bool,
    /// Text annotation being typed (index into `anns`).
    editing: Option<usize>,
    /// Caret blink phase while editing.
    caret_on: bool,
    /// Annotation being dragged in selector mode: (index, last raw point,
    /// grabbed part).
    moving: Option<(usize, (f32, f32), Grab)>,
    /// Clicked annotation — Delete / color / size act on it.
    selected: Option<usize>,
    /// Next step-badge number.
    counter_next: u32,
    /// Annotation under the cursor in selector mode (highlight only).
    hover_ann: Option<usize>,
    size_idx: usize,
    last_rebuild: std::time::Instant,
    font: HFONT,
    font_small: HFONT,
    scale: f32,
    width: i32,
    height: i32,
    theme: crate::theme::Theme,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

unsafe fn make_font(height: i32, weight: i32) -> HFONT {
    CreateFontW(
        height,
        0,
        0,
        0,
        weight,
        0,
        0,
        0,
        DEFAULT_CHARSET.0 as u32,
        0,
        0,
        CLEARTYPE_QUALITY.0 as u32,
        FF_DONTCARE.0 as u32,
        w!("Segoe UI"),
    )
}

unsafe fn state_of(hwnd: HWND) -> Option<&'static mut State> {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
    ptr.as_mut()
}

fn in_rect(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

fn opts_of(state: &State, metric: f32) -> ComposeOpts {
    ComposeOpts {
        metric_scale: metric,
        pad_factor: state.pad_factor,
        aspect: ASPECTS[state.aspect_idx].1,
    }
}

/// Shared view transform: preview blit offset/scale and content padding.
fn view_params(state: &State) -> (i32, i32, f32, f32, f32) {
    let bx = state.preview_box;
    let (bw, bh) = (bx.right - bx.left, bx.bottom - bx.top);
    let draw_scale = (bw as f32 / state.preview_w as f32)
        .min(bh as f32 / state.preview_h as f32)
        .min(1.0);
    let (dw, dh) = (
        (state.preview_w as f32 * draw_scale) as i32,
        (state.preview_h as f32 * draw_scale) as i32,
    );
    let (dx, dy) = (bx.left + (bw - dw) / 2, bx.top + (bh - dh) / 2);
    if compose::is_plain(&state.styles[state.sel]) {
        return (dx, dy, draw_scale, 0.0, 0.0);
    }
    let opts = opts_of(state, state.preview_metric);
    let l = compose::layout(state.small.width() as usize, state.small.height() as usize, &opts);
    (dx, dy, draw_scale, l.pad_x as f32, l.pad_y as f32)
}

/// Map a window point into raw-capture coordinates. None if far outside the
/// content area.
fn to_raw(state: &State, x: i32, y: i32) -> Option<(f32, f32)> {
    let (dx, dy, draw_scale, pad_x, pad_y) = view_params(state);
    let cx = (x - dx) as f32 / draw_scale;
    let cy = (y - dy) as f32 / draw_scale;
    let rx = (cx - pad_x) / state.preview_metric;
    let ry = (cy - pad_y) / state.preview_metric;
    let (rw, rh) = (state.raw.width() as f32, state.raw.height() as f32);
    if rx < -40.0 || ry < -40.0 || rx > rw + 40.0 || ry > rh + 40.0 {
        return None;
    }
    Some((rx.clamp(0.0, rw), ry.clamp(0.0, rh)))
}

fn raw_to_screen(state: &State, p: (f32, f32)) -> (i32, i32) {
    let (dx, dy, draw_scale, pad_x, pad_y) = view_params(state);
    (
        dx + ((p.0 * state.preview_metric + pad_x) * draw_scale) as i32,
        dy + ((p.1 * state.preview_metric + pad_y) * draw_scale) as i32,
    )
}

/// Raw-space bounding box of an annotation (for the selection overlay).
fn ann_bounds(ann: &crate::annotate::Annotation) -> (f32, f32, f32, f32) {
    match &ann.shape {
        crate::annotate::Shape::Arrow { from, to } | crate::annotate::Shape::Line { from, to } => (
            from.0.min(to.0),
            from.1.min(to.1),
            from.0.max(to.0),
            from.1.max(to.1),
        ),
        crate::annotate::Shape::Rect { a, b }
        | crate::annotate::Shape::Ellipse { a, b }
        | crate::annotate::Shape::Highlight { a, b }
        | crate::annotate::Shape::Blur { a, b } => {
            (a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1))
        }
        crate::annotate::Shape::Text { pos, text } => {
            let w = (text.chars().count() as f32 * 11.5 * ann.size).max(20.0);
            (pos.0, pos.1, pos.0 + w, pos.1 + 26.0 * ann.size)
        }
        crate::annotate::Shape::Counter { pos, .. } => {
            let r = 14.0 * ann.size;
            (pos.0 - r, pos.1 - r, pos.0 + r, pos.1 + r)
        }
    }
}

fn rebuild_preview(state: &mut State) {
    let opts = opts_of(state, state.preview_metric);
    let plain = compose::is_plain(&state.styles[state.sel]);
    let caret = if state.caret_on { state.editing } else { None };
    let mut img;
    let (off_x, off_y);
    if plain {
        img = state.small.clone();
        (off_x, off_y) = (0.0, 0.0);
    } else {
        let key = (state.sel, state.pad_factor.to_bits(), state.aspect_idx);
        if state.base_cache.as_ref().map(|(k, _)| *k) != Some(key) {
            let mut base = compose::compose_base(
                state.small.width() as usize,
                state.small.height() as usize,
                &state.styles[state.sel],
                &opts,
            );
            compose::blend_content(&mut base, &state.small, &opts);
            state.base_cache = Some((key, base));
        }
        img = state.base_cache.as_ref().unwrap().1.clone();
        let l =
            compose::layout(state.small.width() as usize, state.small.height() as usize, &opts);
        (off_x, off_y) = (l.pad_x as f32, l.pad_y as f32);
    }
    // Annotations stamp directly onto the composite at the content offset.
    crate::annotate::render(
        &mut img,
        &state.anns,
        state.preview_metric,
        (off_x, off_y),
        caret,
    );
    state.last_rebuild = std::time::Instant::now();
    state.preview_w = img.width() as i32;
    state.preview_h = img.height() as i32;
    let mut bgra = Vec::with_capacity((img.width() * img.height() * 4) as usize);
    for p in img.pixels() {
        bgra.extend_from_slice(&[p[2], p[1], p[0], 255]);
    }
    state.preview = bgra;
}

/// Full-quality result with the current tweaks and annotations applied.
fn final_image(state: &State) -> RgbaImage {
    let cfg = Config::load();
    let raw = &state.raw;
    let plain = compose::is_plain(&state.styles[state.sel]);
    let scale = if plain || raw.width().max(raw.height()) >= 1600 {
        1
    } else {
        cfg.export_scale.clamp(1, 4)
    };
    let mut content = if scale > 1 {
        image::imageops::resize(
            raw,
            raw.width() * scale,
            raw.height() * scale,
            image::imageops::FilterType::Lanczos3,
        )
    } else {
        raw.clone()
    };
    crate::annotate::render(&mut content, &state.anns, scale as f32, (0.0, 0.0), None);
    if plain {
        return content;
    }
    let opts = ComposeOpts {
        metric_scale: scale as f32,
        pad_factor: state.pad_factor,
        aspect: ASPECTS[state.aspect_idx].1,
    };
    compose::compose_with(&content, &state.styles[state.sel], &opts)
}

unsafe fn chip(hdc: HDC, r: RECT, label: &str, state: &State, active: bool, hot: bool) {
    let fill = CreateSolidBrush(if active { state.theme.accent } else { state.theme.chip });
    let pen = CreatePen(PS_SOLID, 1, if active { state.theme.accent } else { state.theme.chip_line });
    let ob = SelectObject(hdc, fill);
    let op = SelectObject(hdc, pen);
    let _ = RoundRect(hdc, r.left, r.top, r.right, r.bottom, 10, 10);
    SelectObject(hdc, ob);
    SelectObject(hdc, op);
    let _ = DeleteObject(fill);
    let _ = DeleteObject(pen);
    SelectObject(hdc, state.font);
    SetTextColor(
        hdc,
        if active {
            state.theme.accent_text
        } else if hot {
            state.theme.text
        } else {
            state.theme.muted
        },
    );
    let mut t = wide(label);
    let mut rc = r;
    DrawTextW(hdc, &mut t, &mut rc, DT_CENTER | DT_SINGLELINE | DT_VCENTER);
}

unsafe fn label(hdc: HDC, state: &State, x: i32, y: i32, text: &str) {
    SelectObject(hdc, state.font_small);
    SetTextColor(hdc, state.theme.muted);
    let mut t = wide(text);
    let mut rc = RECT { left: x, top: y, right: x + 1400, bottom: y + 22 };
    DrawTextW(hdc, &mut t, &mut rc, DT_LEFT | DT_SINGLELINE | DT_VCENTER);
}

unsafe fn paint(hdc: HDC, state: &State) {
    let bg = CreateSolidBrush(state.theme.bg);
    FillRect(hdc, &RECT { left: 0, top: 0, right: state.width, bottom: state.height }, bg);
    let _ = DeleteObject(bg);
    SetBkMode(hdc, TRANSPARENT);

    // Preview, letterboxed into its box.
    let bx = state.preview_box;
    let (bw, bh) = (bx.right - bx.left, bx.bottom - bx.top);
    let scale = (bw as f32 / state.preview_w as f32)
        .min(bh as f32 / state.preview_h as f32)
        .min(1.0);
    let (dw, dh) = (
        (state.preview_w as f32 * scale) as i32,
        (state.preview_h as f32 * scale) as i32,
    );
    let (dx, dy) = (bx.left + (bw - dw) / 2, bx.top + (bh - dh) / 2);
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: state.preview_w,
            biHeight: -state.preview_h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    SetStretchBltMode(hdc, HALFTONE);
    StretchDIBits(
        hdc,
        dx,
        dy,
        dw,
        dh,
        0,
        0,
        state.preview_w,
        state.preview_h,
        Some(state.preview.as_ptr() as *const _),
        &info,
        DIB_RGB_COLORS,
        SRCCOPY,
    );

    // Selection / hover overlay (screen-space, no recompose).
    if state.tool.is_none() {
        for (idx, solid) in [(state.selected, true), (state.hover_ann, false)] {
            let Some(i) = idx else { continue };
            if !solid && state.selected == Some(i) {
                continue;
            }
            let Some(ann) = state.anns.get(i) else { continue };
            let (x0, y0, x1, y1) = ann_bounds(ann);
            let (sx0, sy0) = raw_to_screen(state, (x0, y0));
            let (sx1, sy1) = raw_to_screen(state, (x1, y1));
            let pen = CreatePen(
                if solid { PS_SOLID } else { windows::Win32::Graphics::Gdi::PS_DOT },
                1,
                if solid { state.theme.accent } else { state.theme.muted },
            );
            let op = SelectObject(hdc, pen);
            let ob = SelectObject(
                hdc,
                windows::Win32::Graphics::Gdi::GetStockObject(
                    windows::Win32::Graphics::Gdi::HOLLOW_BRUSH,
                ),
            );
            let _ = windows::Win32::Graphics::Gdi::Rectangle(
                hdc,
                sx0 - 6,
                sy0 - 6,
                sx1 + 6,
                sy1 + 6,
            );
            SelectObject(hdc, ob);
            SelectObject(hdc, op);
            let _ = DeleteObject(pen);
        }

        // Grab handles on the selected annotation: arrow tips and box
        // corners are pullable, and the squares say so.
        if let Some(i) = state.selected {
            if let Some(ann) = state.anns.get(i) {
                let pts: Vec<(f32, f32)> = match &ann.shape {
                    crate::annotate::Shape::Arrow { from, to }
                    | crate::annotate::Shape::Line { from, to } => vec![*from, *to],
                    crate::annotate::Shape::Rect { .. }
                    | crate::annotate::Shape::Ellipse { .. }
                    | crate::annotate::Shape::Highlight { .. }
                    | crate::annotate::Shape::Blur { .. } => {
                        let (x0, y0, x1, y1) = ann_bounds(ann);
                        vec![(x0, y0), (x1, y0), (x1, y1), (x0, y1)]
                    }
                    crate::annotate::Shape::Text { .. }
                    | crate::annotate::Shape::Counter { .. } => Vec::new(),
                };
                if !pts.is_empty() {
                    let fill = CreateSolidBrush(state.theme.accent);
                    let pen = CreatePen(PS_SOLID, 1, state.theme.bg);
                    let ob = SelectObject(hdc, fill);
                    let op = SelectObject(hdc, pen);
                    for p in pts {
                        let (sx, sy) = raw_to_screen(state, p);
                        let _ = windows::Win32::Graphics::Gdi::Rectangle(
                            hdc,
                            sx - 4,
                            sy - 4,
                            sx + 5,
                            sy + 5,
                        );
                    }
                    SelectObject(hdc, ob);
                    SelectObject(hdc, op);
                    let _ = DeleteObject(fill);
                    let _ = DeleteObject(pen);
                }
            }
        }
    }

    // Contextual hint line under the preview.
    let hint = if state.editing.is_some() {
        "type your caption   \u{00b7}   Enter done   \u{00b7}   Esc cancel"
    } else if state.tool.is_some() {
        "drag on the preview to draw   \u{00b7}   tool clears after each add"
    } else {
        "drag annotations to move   \u{00b7}   grab arrow tips / box corners to reshape   \u{00b7}   double-click text to edit   \u{00b7}   Del removes   \u{00b7}   A/R/T/B tools"
    };
    label(
        hdc,
        state,
        state.preview_box.left,
        state.height - (34.0 * state.scale) as i32,
        hint,
    );

    // Section labels.
    let lh = (20.0 * state.scale) as i32;
    if let Some((r, _)) = state.controls.iter().find(|(_, c)| matches!(c, Ctl::Matte(0))) {
        label(hdc, state, r.left, r.top - lh, "MATTE");
    }
    label(hdc, state, state.slider_rect.left, state.slider_rect.top - lh, "PADDING");
    if let Some((r, _)) = state.controls.iter().find(|(_, c)| matches!(c, Ctl::Aspect(0))) {
        label(hdc, state, r.left, r.top - lh, "ASPECT");
    }
    if let Some((r, _)) = state.controls.iter().find(|(_, c)| matches!(c, Ctl::Tool(0))) {
        label(hdc, state, r.left, r.top - lh, "ANNOTATE  (drag on the preview)");
    }

    // Controls.
    for (i, (r, c)) in state.controls.iter().enumerate() {
        let hot = i as i32 == state.hover;
        match c {
            Ctl::Matte(n) => chip(hdc, *r, state.styles[*n].name, state, state.sel == *n, hot),
            Ctl::Aspect(n) => {
                chip(hdc, *r, ASPECTS[*n].0, state, state.aspect_idx == *n, hot)
            }
            Ctl::Copy => chip(hdc, *r, "Copy", state, true, hot),
            Ctl::Save => chip(hdc, *r, "Save", state, false, hot),
            Ctl::Edit => chip(hdc, *r, "Editor", state, false, hot),
            Ctl::Tool(n) => chip(hdc, *r, TOOLS[*n], state, state.tool == Some(*n), hot),
            Ctl::Size(n) => chip(
                hdc,
                *r,
                ["S", "M", "L"][*n],
                state,
                state.size_idx == *n,
                hot,
            ),
            Ctl::Undo => chip(hdc, *r, "Undo", state, false, hot),
            Ctl::Clear => chip(hdc, *r, "Clear", state, false, hot),
            Ctl::Ocr => chip(hdc, *r, "Copy text (OCR)", state, false, hot),
            Ctl::Color(n) => {
                let c = crate::annotate::COLORS[*n];
                let color = COLORREF((c[2] as u32) << 16 | (c[1] as u32) << 8 | c[0] as u32);
                let fill = CreateSolidBrush(color);
                let ring = CreatePen(
                    PS_SOLID,
                    2,
                    if state.color_idx == *n { state.theme.accent } else { state.theme.chip_line },
                );
                let ob = SelectObject(hdc, fill);
                let op = SelectObject(hdc, ring);
                let _ = RoundRect(hdc, r.left, r.top, r.right, r.bottom, 8, 8);
                SelectObject(hdc, ob);
                SelectObject(hdc, op);
                let _ = DeleteObject(fill);
                let _ = DeleteObject(ring);
            }
            Ctl::Slider => {}
        }
    }

    // Slider.
    let sr = state.slider_rect;
    let cy = (sr.top + sr.bottom) / 2;
    let track = CreateSolidBrush(state.theme.track);
    FillRect(hdc, &RECT { left: sr.left, top: cy - 2, right: sr.right, bottom: cy + 2 }, track);
    let _ = DeleteObject(track);
    let t = (state.pad_factor - PAD_MIN) / (PAD_MAX - PAD_MIN);
    let tx = sr.left + ((sr.right - sr.left) as f32 * t) as i32;
    let filled = CreateSolidBrush(state.theme.accent);
    FillRect(hdc, &RECT { left: sr.left, top: cy - 2, right: tx, bottom: cy + 2 }, filled);
    let thumb_pen = CreatePen(PS_SOLID, 1, state.theme.accent);
    let ob = SelectObject(hdc, filled);
    let op = SelectObject(hdc, thumb_pen);
    let th = 7;
    let _ = RoundRect(hdc, tx - th, cy - th, tx + th, cy + th, th * 2, th * 2);
    SelectObject(hdc, ob);
    SelectObject(hdc, op);
    let _ = DeleteObject(filled);
    let _ = DeleteObject(thumb_pen);
}

unsafe fn slider_update(hwnd: HWND, state: &mut State, x: i32) {
    let sr = state.slider_rect;
    let t = ((x - sr.left) as f32 / (sr.right - sr.left).max(1) as f32).clamp(0.0, 1.0);
    state.pad_factor = PAD_MIN + t * (PAD_MAX - PAD_MIN);
    rebuild_preview(state);
    let _ = InvalidateRect(hwnd, None, false);
}

/// Commit any in-progress text annotation (drop it if empty). Returns true
/// if a non-empty annotation was committed.
fn commit_editing(state: &mut State) -> bool {
    if let Some(i) = state.editing.take() {
        let empty = matches!(
            state.anns.get(i).map(|a| &a.shape),
            Some(crate::annotate::Shape::Text { text, .. }) if text.is_empty()
        );
        if empty {
            state.anns.remove(i);
            return false;
        }
        // One-shot tools: a successful add returns to the selector.
        state.tool = None;
        return true;
    }
    false
}

fn dist_seg(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = (dx * dx + dy * dy).max(1e-6);
    let t = (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0);
    let (cx, cy) = (a.0 + t * dx, a.1 + t * dy);
    ((p.0 - cx).powi(2) + (p.1 - cy).powi(2)).sqrt()
}

/// Topmost annotation under a raw-space point, for selector-mode dragging.
fn hit_ann(state: &State, p: (f32, f32)) -> Option<usize> {
    let tol = (8.0 / state.preview_metric).max(6.0);
    for (i, ann) in state.anns.iter().enumerate().rev() {
        let hit = match &ann.shape {
            crate::annotate::Shape::Arrow { from, to }
            | crate::annotate::Shape::Line { from, to } => dist_seg(p, *from, *to) <= tol,
            crate::annotate::Shape::Ellipse { a, b } => {
                let (cx, cy) = ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0);
                let (rx, ry) = (((a.0 - b.0) / 2.0).abs().max(1.0), ((a.1 - b.1) / 2.0).abs().max(1.0));
                let v = (((p.0 - cx) / rx).powi(2) + ((p.1 - cy) / ry).powi(2)).sqrt();
                ((v - 1.0) * rx.min(ry)).abs() <= tol
            }
            crate::annotate::Shape::Highlight { a, b } => {
                p.0 >= a.0.min(b.0) - tol
                    && p.0 <= a.0.max(b.0) + tol
                    && p.1 >= a.1.min(b.1) - tol
                    && p.1 <= a.1.max(b.1) + tol
            }
            crate::annotate::Shape::Counter { pos, .. } => {
                ((p.0 - pos.0).powi(2) + (p.1 - pos.1).powi(2)).sqrt() <= 14.0 * ann.size + tol
            }
            crate::annotate::Shape::Rect { a, b } => {
                let (x0, y0) = (a.0.min(b.0), a.1.min(b.1));
                let (x1, y1) = (a.0.max(b.0), a.1.max(b.1));
                dist_seg(p, (x0, y0), (x1, y0)) <= tol
                    || dist_seg(p, (x1, y0), (x1, y1)) <= tol
                    || dist_seg(p, (x1, y1), (x0, y1)) <= tol
                    || dist_seg(p, (x0, y1), (x0, y0)) <= tol
            }
            crate::annotate::Shape::Blur { a, b } => {
                p.0 >= a.0.min(b.0) - tol
                    && p.0 <= a.0.max(b.0) + tol
                    && p.1 >= a.1.min(b.1) - tol
                    && p.1 <= a.1.max(b.1) + tol
            }
            crate::annotate::Shape::Text { pos, text } => {
                let w = (text.chars().count() as f32 * 11.5).max(20.0);
                p.0 >= pos.0 - tol
                    && p.0 <= pos.0 + w + tol
                    && p.1 >= pos.1 - tol
                    && p.1 <= pos.1 + 26.0 + tol
            }
        };
        if hit {
            return Some(i);
        }
    }
    None
}

/// Which part of the annotation is under the point (read-only probe, used
/// for both cursor feedback and grabbing).
fn grab_probe(ann: &crate::annotate::Annotation, p: (f32, f32), tol: f32) -> Grab {
    match &ann.shape {
        crate::annotate::Shape::Arrow { from, to }
        | crate::annotate::Shape::Line { from, to } => {
            let d0 = ((p.0 - from.0).powi(2) + (p.1 - from.1).powi(2)).sqrt();
            let d1 = ((p.0 - to.0).powi(2) + (p.1 - to.1).powi(2)).sqrt();
            if d1 <= tol * 1.5 {
                Grab::P1
            } else if d0 <= tol * 1.5 {
                Grab::P0
            } else {
                Grab::Whole
            }
        }
        crate::annotate::Shape::Rect { a, b }
        | crate::annotate::Shape::Ellipse { a, b }
        | crate::annotate::Shape::Highlight { a, b }
        | crate::annotate::Shape::Blur { a, b } => {
            let (x0, y0) = (a.0.min(b.0), a.1.min(b.1));
            let (x1, y1) = (a.0.max(b.0), a.1.max(b.1));
            let corners = [(x0, y0), (x1, y0), (x1, y1), (x0, y1)];
            for (k, c) in corners.iter().enumerate() {
                if ((p.0 - c.0).powi(2) + (p.1 - c.1).powi(2)).sqrt() <= tol * 1.5 {
                    return Grab::Corner(k as u8);
                }
            }
            Grab::Whole
        }
        crate::annotate::Shape::Text { .. } | crate::annotate::Shape::Counter { .. } => {
            Grab::Whole
        }
    }
}

/// Normalize a boxy shape so corner indices stay stable during a drag.
fn normalize_rect(ann: &mut crate::annotate::Annotation) {
    if let crate::annotate::Shape::Rect { a, b }
    | crate::annotate::Shape::Ellipse { a, b }
    | crate::annotate::Shape::Highlight { a, b }
    | crate::annotate::Shape::Blur { a, b } = &mut ann.shape
    {
        let (x0, y0) = (a.0.min(b.0), a.1.min(b.1));
        let (x1, y1) = (a.0.max(b.0), a.1.max(b.1));
        *a = (x0, y0);
        *b = (x1, y1);
    }
}

/// The cursor that tells the truth about what a grab would do.
fn cursor_for(grab: Grab) -> windows::core::PCWSTR {
    use windows::Win32::UI::WindowsAndMessaging::{
        IDC_CROSS, IDC_SIZEALL, IDC_SIZENESW, IDC_SIZENWSE,
    };
    match grab {
        Grab::Whole => IDC_SIZEALL,
        // Endpoints re-aim: precision crosshair.
        Grab::P0 | Grab::P1 => IDC_CROSS,
        // Corner resize: diagonal arrows matching the corner.
        Grab::Corner(0) | Grab::Corner(2) => IDC_SIZENWSE,
        Grab::Corner(_) => IDC_SIZENESW,
    }
}

fn apply_grab(ann: &mut crate::annotate::Annotation, grab: Grab, p: (f32, f32), d: (f32, f32)) {
    match grab {
        Grab::Whole => translate_ann(ann, d),
        Grab::P0 => {
            if let crate::annotate::Shape::Arrow { from, .. }
            | crate::annotate::Shape::Line { from, .. } = &mut ann.shape
            {
                *from = p;
            }
        }
        Grab::P1 => {
            if let crate::annotate::Shape::Arrow { to, .. }
            | crate::annotate::Shape::Line { to, .. } = &mut ann.shape
            {
                *to = p;
            }
        }
        Grab::Corner(k) => {
            if let crate::annotate::Shape::Rect { a, b }
            | crate::annotate::Shape::Ellipse { a, b }
            | crate::annotate::Shape::Highlight { a, b }
            | crate::annotate::Shape::Blur { a, b } = &mut ann.shape
            {
                match k {
                    0 => *a = p,
                    1 => {
                        b.0 = p.0;
                        a.1 = p.1;
                    }
                    2 => *b = p,
                    _ => {
                        a.0 = p.0;
                        b.1 = p.1;
                    }
                }
            }
        }
    }
}

fn translate_ann(ann: &mut crate::annotate::Annotation, d: (f32, f32)) {
    let shift = |p: &mut (f32, f32)| {
        p.0 += d.0;
        p.1 += d.1;
    };
    match &mut ann.shape {
        crate::annotate::Shape::Arrow { from, to } | crate::annotate::Shape::Line { from, to } => {
            shift(from);
            shift(to);
        }
        crate::annotate::Shape::Rect { a, b }
        | crate::annotate::Shape::Ellipse { a, b }
        | crate::annotate::Shape::Highlight { a, b }
        | crate::annotate::Shape::Blur { a, b } => {
            shift(a);
            shift(b);
        }
        crate::annotate::Shape::Text { pos, .. } | crate::annotate::Shape::Counter { pos, .. } => {
            shift(pos)
        }
    }
}

unsafe fn activate(hwnd: HWND, state: &mut State, ctl: Ctl) {
    match ctl {
        Ctl::Tool(n) => {
            commit_editing(state);
            state.tool = if state.tool == Some(n) { None } else { Some(n) };
            rebuild_preview(state);
            let _ = InvalidateRect(hwnd, None, false);
            return;
        }
        Ctl::Color(n) => {
            state.color_idx = n;
            // Recolor whatever is selected or being typed.
            let target = state.editing.or(state.selected);
            if let Some(i) = target {
                if let Some(ann) = state.anns.get_mut(i) {
                    ann.color = n;
                }
                rebuild_preview(state);
            }
            let _ = InvalidateRect(hwnd, None, false);
            return;
        }
        Ctl::Size(n) => {
            state.size_idx = n;
            let target = state.editing.or(state.selected);
            if let Some(i) = target {
                if let Some(ann) = state.anns.get_mut(i) {
                    ann.size = SIZES[n];
                }
                rebuild_preview(state);
            }
            let _ = InvalidateRect(hwnd, None, false);
            return;
        }
        Ctl::Undo => {
            commit_editing(state);
            if let Some(ann) = state.anns.pop() {
                if matches!(ann.shape, crate::annotate::Shape::Counter { .. }) {
                    state.counter_next = state.counter_next.saturating_sub(1).max(1);
                }
            }
            state.selected = None;
            state.hover_ann = None;
            rebuild_preview(state);
            let _ = InvalidateRect(hwnd, None, false);
            return;
        }
        Ctl::Clear => {
            state.editing = None;
            state.anns.clear();
            state.selected = None;
            state.hover_ann = None;
            state.counter_next = 1;
            rebuild_preview(state);
            let _ = InvalidateRect(hwnd, None, false);
            return;
        }
        _ => {}
    }
    match ctl {
        Ctl::Matte(n) => {
            state.sel = n;
            rebuild_preview(state);
            let _ = InvalidateRect(hwnd, None, false);
        }
        Ctl::Aspect(n) => {
            state.aspect_idx = n;
            rebuild_preview(state);
            let _ = InvalidateRect(hwnd, None, false);
        }
        Ctl::Slider => {}
        Ctl::Ocr => {
            if let Err(e) = crate::ocr::copy_text(&state.raw) {
                eprintln!("ocr failed: {e:#}");
            }
        }
        Ctl::Copy => {
            let img = final_image(state);
            let cfg = Config::load();
            let path = output::save_png(&img, state.styles[state.sel].name, &cfg.save_dir());
            let _ = output::to_clipboard(&img, path.as_deref().ok());
            Config::update(|cfg| cfg.last_style = state.sel);
            state.done = true;
            let _ = DestroyWindow(hwnd);
        }
        Ctl::Save => {
            let img = final_image(state);
            let cfg = Config::load();
            let _ = output::save_png(&img, state.styles[state.sel].name, &cfg.save_dir());
            state.done = true;
            let _ = DestroyWindow(hwnd);
        }
        Ctl::Edit => {
            let img = final_image(state);
            let cfg = Config::load();
            if let Ok(path) = output::save_png(&img, state.styles[state.sel].name, &cfg.save_dir())
            {
                output::open_in_editor(&path);
            }
            state.done = true;
            let _ = DestroyWindow(hwnd);
        }
        // Tool/Color/Undo/Clear handled above.
        _ => {}
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            let cs = &*(lparam.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            if let Some(state) = state_of(hwnd) {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                let mem = CreateCompatibleDC(hdc);
                let bmp = CreateCompatibleBitmap(hdc, state.width, state.height);
                let old = SelectObject(mem, bmp);
                paint(mem, state);
                let _ = BitBlt(hdc, 0, 0, state.width, state.height, mem, 0, 0, SRCCOPY);
                SelectObject(mem, old);
                let _ = DeleteObject(bmp);
                let _ = DeleteDC(mem);
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if let Some(state) = state_of(hwnd) {
                let (x, y) = (
                    (lparam.0 & 0xFFFF) as i16 as i32,
                    ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
                );
                if state.dragging {
                    slider_update(hwnd, state, x);
                } else if let Some((i, last, grab)) = state.moving {
                    if let Some(p) = to_raw(state, x, y) {
                        if let Some(ann) = state.anns.get_mut(i) {
                            apply_grab(ann, grab, p, (p.0 - last.0, p.1 - last.1));
                        }
                        state.moving = Some((i, p, grab));
                        if state.last_rebuild.elapsed().as_millis() > 15 {
                            rebuild_preview(state);
                            let _ = InvalidateRect(hwnd, None, false);
                        }
                    }
                } else if state.drawing {
                    if let (Some(p), Some(ann)) = (to_raw(state, x, y), state.anns.last_mut()) {
                        match &mut ann.shape {
                            crate::annotate::Shape::Arrow { to, .. }
                            | crate::annotate::Shape::Line { to, .. } => *to = p,
                            crate::annotate::Shape::Rect { b, .. }
                            | crate::annotate::Shape::Ellipse { b, .. }
                            | crate::annotate::Shape::Highlight { b, .. }
                            | crate::annotate::Shape::Blur { b, .. } => *b = p,
                            _ => {}
                        }
                        // Annotation stamping is cheap now; near-frame-rate.
                        if state.last_rebuild.elapsed().as_millis() > 15 {
                            rebuild_preview(state);
                            let _ = InvalidateRect(hwnd, None, false);
                        }
                    }
                } else {
                    let hover = state
                        .controls
                        .iter()
                        .position(|(r, _)| in_rect(r, x, y))
                        .map(|i| i as i32)
                        .unwrap_or(-1);
                    let hover_ann = if state.tool.is_none() {
                        to_raw(state, x, y).and_then(|p| hit_ann(state, p))
                    } else {
                        None
                    };
                    if hover != state.hover || hover_ann != state.hover_ann {
                        state.hover = hover;
                        state.hover_ann = hover_ann;
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                }
            }
            LRESULT(0)
        }
        windows::Win32::UI::WindowsAndMessaging::WM_LBUTTONDBLCLK => {
            if let Some(state) = state_of(hwnd) {
                let (x, y) = (
                    (lparam.0 & 0xFFFF) as i16 as i32,
                    ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
                );
                if state.tool.is_none() {
                    if let Some(p) = to_raw(state, x, y) {
                        if let Some(i) = hit_ann(state, p) {
                            if matches!(
                                state.anns[i].shape,
                                crate::annotate::Shape::Text { .. }
                            ) {
                                // Re-edit an existing caption.
                                state.moving = None;
                                let _ = ReleaseCapture();
                                state.editing = Some(i);
                                state.selected = Some(i);
                                state.caret_on = true;
                                rebuild_preview(state);
                                let _ = InvalidateRect(hwnd, None, false);
                            }
                        }
                    }
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            if let Some(state) = state_of(hwnd) {
                let (x, y) = (
                    (lparam.0 & 0xFFFF) as i16 as i32,
                    ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
                );
                let grab = RECT {
                    left: state.slider_rect.left - 8,
                    top: state.slider_rect.top - 8,
                    right: state.slider_rect.right + 8,
                    bottom: state.slider_rect.bottom + 8,
                };
                if in_rect(&grab, x, y) {
                    commit_editing(state);
                    state.dragging = true;
                    SetCapture(hwnd);
                    slider_update(hwnd, state, x);
                } else if let (Some(tool), Some(p)) = (state.tool, to_raw(state, x, y)) {
                    commit_editing(state);
                    let color = state.color_idx;
                    let size = SIZES[state.size_idx];
                    use crate::annotate::Shape;
                    let drag_shape = match tool {
                        0 => Some(Shape::Arrow { from: p, to: p }),
                        1 => Some(Shape::Line { from: p, to: p }),
                        2 => Some(Shape::Rect { a: p, b: p }),
                        3 => Some(Shape::Ellipse { a: p, b: p }),
                        4 => Some(Shape::Highlight { a: p, b: p }),
                        6 => Some(Shape::Blur { a: p, b: p }),
                        _ => None,
                    };
                    if let Some(shape) = drag_shape {
                        state.anns.push(crate::annotate::Annotation { shape, color, size });
                        state.drawing = true;
                        SetCapture(hwnd);
                    } else if tool == 7 {
                        // Step badge: click places, auto-numbered, one-shot.
                        let n = state.counter_next;
                        state.counter_next += 1;
                        state.anns.push(crate::annotate::Annotation {
                            shape: Shape::Counter { pos: p, n },
                            color,
                            size,
                        });
                        state.selected = Some(state.anns.len() - 1);
                        state.tool = None;
                        rebuild_preview(state);
                        let _ = InvalidateRect(hwnd, None, false);
                    } else {
                        // Text: click places, then type.
                        state.anns.push(crate::annotate::Annotation {
                            shape: Shape::Text { pos: p, text: String::new() },
                            color,
                            size,
                        });
                        state.editing = Some(state.anns.len() - 1);
                        state.selected = Some(state.anns.len() - 1);
                        state.caret_on = true;
                        rebuild_preview(state);
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                } else {
                    if state.editing.is_some() {
                        commit_editing(state);
                        rebuild_preview(state);
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                    // Selector mode: grab an existing annotation (whole,
                    // endpoint, or corner) to manipulate it.
                    if let Some(p) = to_raw(state, x, y) {
                        if let Some(i) = hit_ann(state, p) {
                            let tol = (8.0 / state.preview_metric).max(6.0);
                            normalize_rect(&mut state.anns[i]);
                            let grab = grab_probe(&state.anns[i], p, tol);
                            state.selected = Some(i);
                            state.moving = Some((i, p, grab));
                            SetCapture(hwnd);
                            let _ = InvalidateRect(hwnd, None, false);
                        } else if state.selected.take().is_some() {
                            let _ = InvalidateRect(hwnd, None, false);
                        }
                    }
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if let Some(state) = state_of(hwnd) {
                if state.dragging {
                    state.dragging = false;
                    let _ = ReleaseCapture();
                    return LRESULT(0);
                }
                if state.moving.take().is_some() {
                    let _ = ReleaseCapture();
                    rebuild_preview(state);
                    let _ = InvalidateRect(hwnd, None, false);
                    return LRESULT(0);
                }
                if state.drawing {
                    state.drawing = false;
                    let _ = ReleaseCapture();
                    // Drop degenerate shapes (a stray click).
                    let degenerate = match state.anns.last().map(|a| &a.shape) {
                        Some(crate::annotate::Shape::Arrow { from, to })
                        | Some(crate::annotate::Shape::Line { from, to }) => {
                            (from.0 - to.0).abs() < 3.0 && (from.1 - to.1).abs() < 3.0
                        }
                        Some(crate::annotate::Shape::Rect { a, b })
                        | Some(crate::annotate::Shape::Ellipse { a, b })
                        | Some(crate::annotate::Shape::Highlight { a, b })
                        | Some(crate::annotate::Shape::Blur { a, b }) => {
                            (a.0 - b.0).abs() < 3.0 && (a.1 - b.1).abs() < 3.0
                        }
                        _ => false,
                    };
                    if degenerate {
                        state.anns.pop();
                    } else {
                        // One-shot tools: a successful add returns to the
                        // selector so the next drag moves instead of drawing.
                        state.tool = None;
                        state.selected = Some(state.anns.len() - 1);
                    }
                    rebuild_preview(state);
                    let _ = InvalidateRect(hwnd, None, false);
                    return LRESULT(0);
                }
                let (x, y) = (
                    (lparam.0 & 0xFFFF) as i16 as i32,
                    ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
                );
                if let Some(i) = state.controls.iter().position(|(r, _)| in_rect(r, x, y)) {
                    let ctl = state.controls[i].1;
                    activate(hwnd, state, ctl);
                }
            }
            LRESULT(0)
        }
        windows::Win32::UI::WindowsAndMessaging::WM_TIMER => {
            if let Some(state) = state_of(hwnd) {
                if state.editing.is_some() {
                    state.caret_on = !state.caret_on;
                    rebuild_preview(state);
                    let _ = InvalidateRect(hwnd, None, false);
                } else if !state.caret_on {
                    // Next edit starts with a visible caret.
                    state.caret_on = true;
                }
            }
            LRESULT(0)
        }
        windows::Win32::UI::WindowsAndMessaging::WM_CHAR => {
            if let Some(state) = state_of(hwnd) {
                // Typing keeps the caret solid, like every real text box.
                state.caret_on = true;
                if let Some(i) = state.editing {
                    let c = wparam.0 as u32;
                    if let Some(crate::annotate::Shape::Text { text, .. }) =
                        state.anns.get_mut(i).map(|a| &mut a.shape)
                    {
                        match c {
                            0x08 => {
                                text.pop();
                            }
                            0x0D | 0x1B => {}
                            _ if c >= 0x20 => {
                                if let Some(ch) = char::from_u32(c) {
                                    text.push(ch);
                                }
                            }
                            _ => {}
                        }
                    }
                    rebuild_preview(state);
                    let _ = InvalidateRect(hwnd, None, false);
                }
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            if let Some(state) = state_of(hwnd) {
                let ctrl_down = windows::Win32::UI::Input::KeyboardAndMouse::GetKeyState(
                    windows::Win32::UI::Input::KeyboardAndMouse::VK_CONTROL.0 as i32,
                ) < 0;
                match wparam.0 as u16 {
                    v if v == VK_ESCAPE.0 => {
                        if let Some(i) = state.editing.take() {
                            // Cancel the text being typed.
                            state.anns.remove(i);
                            rebuild_preview(state);
                            let _ = InvalidateRect(hwnd, None, false);
                        } else {
                            state.done = true;
                            let _ = DestroyWindow(hwnd);
                        }
                    }
                    v if v == VK_RETURN.0 => {
                        if state.editing.is_some() {
                            commit_editing(state);
                            rebuild_preview(state);
                            let _ = InvalidateRect(hwnd, None, false);
                        } else {
                            activate(hwnd, state, Ctl::Copy);
                        }
                    }
                    0x5A if ctrl_down => {
                        // Ctrl+Z
                        commit_editing(state);
                        state.anns.pop();
                        state.selected = None;
                        state.hover_ann = None;
                        rebuild_preview(state);
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                    // Delete removes the selected annotation.
                    0x2E if state.editing.is_none() => {
                        if let Some(i) = state.selected.take() {
                            if i < state.anns.len() {
                                state.anns.remove(i);
                            }
                            state.hover_ann = None;
                            rebuild_preview(state);
                            let _ = InvalidateRect(hwnd, None, false);
                        }
                    }
                    // Tool shortcuts (A/R/T/B) and colors (1-4).
                    0x41 if state.editing.is_none() => activate(hwnd, state, Ctl::Tool(0)), // Arrow
                    0x52 if state.editing.is_none() => activate(hwnd, state, Ctl::Tool(2)), // Rectangle
                    0x54 if state.editing.is_none() => activate(hwnd, state, Ctl::Tool(5)), // Text
                    0x42 if state.editing.is_none() => activate(hwnd, state, Ctl::Tool(6)), // Blur
                    v @ 0x31..=0x34 if state.editing.is_none() => {
                        activate(hwnd, state, Ctl::Color((v - 0x31) as usize))
                    }
                    _ => {}
                }
            }
            LRESULT(0)
        }
        // PrtScn mid-tweak: nested overlay; the frozen image includes this
        // window, so it's snippable. Esc there returns here untouched.
        WM_RESHOOT => {
            if let Some(state) = state_of(hwnd) {
                if !state.suspended {
                    state.suspended = true;
                    let result = crate::overlay::select();
                    state.suspended = false;
                    match result {
                        Ok(Some(r)) => {
                            state.reshoot = Some(r);
                            state.done = true;
                            let _ = DestroyWindow(hwnd);
                        }
                        _ => {
                            let _ = SetForegroundWindow(hwnd);
                        }
                    }
                }
            }
            LRESULT(0)
        }
        windows::Win32::UI::WindowsAndMessaging::WM_SETCURSOR => {
            // Only inside the client area (hit-test HTCLIENT = 1).
            if (lparam.0 & 0xFFFF) as u32 == 1 {
                if let Some(state) = state_of(hwnd) {
                    use windows::Win32::UI::WindowsAndMessaging::{
                        GetCursorPos, LoadCursorW as LC, SetCursor, IDC_ARROW as ARROW,
                        IDC_CROSS,
                    };
                    let mut pt = windows::Win32::Foundation::POINT::default();
                    let _ = GetCursorPos(&mut pt);
                    let _ = windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut pt);
                    let raw = to_raw(state, pt.x, pt.y);
                    let id = if state.tool.is_some() && raw.is_some() {
                        IDC_CROSS
                    } else if let Some((_, _, grab)) = state.moving {
                        // Mid-drag: keep showing what the drag is doing.
                        cursor_for(grab)
                    } else if let Some(p) = raw {
                        match hit_ann(state, p) {
                            Some(i) => {
                                let tol = (8.0 / state.preview_metric).max(6.0);
                                cursor_for(grab_probe(&state.anns[i], p, tol))
                            }
                            None => ARROW,
                        }
                    } else {
                        ARROW
                    };
                    if let Ok(c) = LC(None, id) {
                        SetCursor(c);
                    }
                    return LRESULT(1);
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        windows::Win32::UI::WindowsAndMessaging::WM_SETTINGCHANGE => {
            if let Some(state) = state_of(hwnd) {
                state.theme = crate::theme::current();
                crate::theme::apply_titlebar(hwnd, &state.theme);
                let _ = InvalidateRect(hwnd, None, true);
            }
            LRESULT(0)
        }
        windows::Win32::UI::WindowsAndMessaging::WM_SIZE => {
            if let Some(state) = state_of(hwnd) {
                let (w, h) = ((lparam.0 & 0xFFFF) as i32, ((lparam.0 >> 16) & 0xFFFF) as i32);
                if w > 0 && h > 0 {
                    state.width = w;
                    state.height = h;
                    let (controls, preview_box, slider_rect) =
                        layout_controls(state.scale, w, h, state.styles.len());
                    state.controls = controls;
                    state.preview_box = preview_box;
                    state.slider_rect = slider_rect;
                    let _ = InvalidateRect(hwnd, None, false);
                }
            }
            LRESULT(0)
        }
        windows::Win32::UI::WindowsAndMessaging::WM_GETMINMAXINFO => {
            let mmi = lparam.0 as *mut windows::Win32::UI::WindowsAndMessaging::MINMAXINFO;
            if !mmi.is_null() {
                let s = unsafe { GetDpiForSystem() } as f32 / 96.0;
                (*mmi).ptMinTrackSize.x = (760.0 * s) as i32;
                (*mmi).ptMinTrackSize.y = (560.0 * s) as i32;
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Column + preview layout for the given client size. Rerun on resize.
fn layout_controls(
    scale: f32,
    cw: i32,
    ch: i32,
    n_styles: usize,
) -> (Vec<(RECT, Ctl)>, RECT, RECT) {
    let sc = |v: i32| (v as f32 * scale) as i32;
    let m = sc(20);
    let col_x = cw - sc(250);
    // Bottom strip reserved for the contextual hint line.
    let preview_box = RECT { left: m, top: m, right: col_x - sc(16), bottom: ch - m - sc(18) };
    let mut controls = Vec::new();
    let mut y = m + sc(22);
    for i in 0..n_styles {
        let row = i as i32 / 2;
        let colm = i as i32 % 2;
        controls.push((
            RECT {
                left: col_x + colm * sc(112),
                top: y + row * sc(38),
                right: col_x + colm * sc(112) + sc(104),
                bottom: y + row * sc(38) + sc(30),
            },
            Ctl::Matte(i),
        ));
    }
    y += ((n_styles as i32 + 1) / 2) * sc(38) + sc(30);
    let slider_rect = RECT { left: col_x, top: y, right: col_x + sc(216), bottom: y + sc(22) };
    controls.push((slider_rect, Ctl::Slider));
    y += sc(52);
    for i in 0..ASPECTS.len() {
        let row = i as i32 / 3;
        let colm = i as i32 % 3;
        controls.push((
            RECT {
                left: col_x + colm * sc(74),
                top: y + row * sc(38),
                right: col_x + colm * sc(74) + sc(66),
                bottom: y + row * sc(38) + sc(30),
            },
            Ctl::Aspect(i),
        ));
    }
    // Annotation tools.
    let tools_y = y + sc(38) * 2 + sc(26);
    for i in 0..TOOLS.len() {
        let row = i as i32 / 3;
        let colm = i as i32 % 3;
        controls.push((
            RECT {
                left: col_x + colm * sc(74),
                top: tools_y + row * sc(38),
                right: col_x + colm * sc(74) + sc(66),
                bottom: tools_y + row * sc(38) + sc(30),
            },
            Ctl::Tool(i),
        ));
    }
    let tool_rows = (TOOLS.len() as i32 + 2) / 3;
    let colors_y = tools_y + sc(38) * tool_rows + sc(8);
    for i in 0..crate::annotate::COLORS.len() {
        let x = col_x + i as i32 * sc(34);
        controls.push((
            RECT { left: x, top: colors_y, right: x + sc(26), bottom: colors_y + sc(26) },
            Ctl::Color(i),
        ));
    }
    let uc_y = colors_y + sc(34);
    controls.push((
        RECT {
            left: col_x + sc(148),
            top: colors_y,
            right: col_x + sc(216),
            bottom: colors_y + sc(26),
        },
        Ctl::Undo,
    ));
    controls.push((
        RECT { left: col_x + sc(148), top: uc_y, right: col_x + sc(216), bottom: uc_y + sc(26) },
        Ctl::Clear,
    ));
    // Size chips (S/M/L) on the row under the colors.
    for (i, _) in SIZES.iter().enumerate() {
        let x = col_x + i as i32 * sc(46);
        controls.push((
            RECT { left: x, top: uc_y, right: x + sc(40), bottom: uc_y + sc(26) },
            Ctl::Size(i),
        ));
    }
    let by = ch - m - sc(30);
    controls.push((
        RECT { left: col_x, top: by - sc(38), right: col_x + sc(216), bottom: by - sc(8) },
        Ctl::Ocr,
    ));
    controls.push((
        RECT { left: col_x, top: by, right: col_x + sc(64), bottom: by + sc(30) },
        Ctl::Copy,
    ));
    controls.push((
        RECT { left: col_x + sc(72), top: by, right: col_x + sc(136), bottom: by + sc(30) },
        Ctl::Save,
    ));
    controls.push((
        RECT { left: col_x + sc(144), top: by, right: col_x + sc(216), bottom: by + sc(30) },
        Ctl::Edit,
    ));
    (controls, preview_box, slider_rect)
}

/// Modal tweak panel for one capture. Returns a reshoot selection if the
/// user pressed PrtScn mid-tweak and snipped something new.
pub fn run(
    raw: RgbaImage,
    styles: Vec<Style>,
    initial: usize,
    monitor: HMONITOR,
) -> Result<Option<(crate::overlay::Selection, HMONITOR)>> {
    let dpi_scale = unsafe { GetDpiForSystem() } as f32 / 96.0;
    let sc = |v: i32| (v as f32 * dpi_scale) as i32;

    // Size to the monitor: the editor earns its screen space.
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe {
        let _ = GetMonitorInfoW(monitor, &mut mi);
    }
    let work_w = mi.rcWork.right - mi.rcWork.left;
    let work_h = mi.rcWork.bottom - mi.rcWork.top;
    let cw = ((work_w as f32 * 0.85) as i32).clamp(sc(760).min(work_w - sc(40)), work_w - sc(40));
    let ch = ((work_h as f32 * 0.85) as i32).clamp(sc(560).min(work_h - sc(40)), work_h - sc(40));

    // Preview source — large enough that text stays readable.
    const PREVIEW_MAX: u32 = 1200;
    let scale = (PREVIEW_MAX as f32 / raw.width().max(raw.height()) as f32).min(1.0);
    let small = if scale < 1.0 {
        image::imageops::resize(
            &raw,
            (raw.width() as f32 * scale) as u32,
            (raw.height() as f32 * scale) as u32,
            image::imageops::FilterType::Triangle,
        )
    } else {
        raw.clone()
    };

    let (controls, preview_box, slider_rect) = layout_controls(dpi_scale, cw, ch, styles.len());

    let preview_metric = scale;
    let mut state = Box::new(State {
        raw,
        small,
        preview_metric,
        styles,
        sel: initial,
        pad_factor: compose::DEFAULT_PAD_FACTOR,
        aspect_idx: 0,
        preview: Vec::new(),
        preview_w: 1,
        preview_h: 1,
        controls,
        preview_box,
        slider_rect,
        hover: -1,
        dragging: false,
        done: false,
        suspended: false,
        reshoot: None,
        base_cache: None,
        anns: Vec::new(),
        tool: None,
        color_idx: 0,
        drawing: false,
        editing: None,
        caret_on: true,
        moving: None,
        selected: None,
        counter_next: 1,
        hover_ann: None,
        size_idx: 1,
        last_rebuild: std::time::Instant::now(),
        font: unsafe { make_font(-sc(14), 400) },
        font_small: unsafe { make_font(-sc(12), 400) },
        scale: dpi_scale,
        width: cw,
        height: ch,
        theme: crate::theme::current(),
    });
    rebuild_preview(&mut state);

    unsafe {
        // Center on the anchor monitor.
        let wx = mi.rcWork.left + (work_w - cw) / 2;
        let wy = mi.rcWork.top + (work_h - ch) / 2;

        let hinstance = GetModuleHandleW(None)?;
        let class = WNDCLASSW {
            style: CS_HREDRAW
                | CS_VREDRAW
                | windows::Win32::UI::WindowsAndMessaging::CS_DBLCLKS,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hIcon: crate::tray::app_icon(),
            lpszClassName: w!("matteshot_tweak"),
            ..Default::default()
        };
        RegisterClassW(&class);

        // Exact outer size for our client area — guessing causes unpainted
        // strips and clipped controls.
        let style = WS_CAPTION
            | WS_SYSMENU
            | WS_VISIBLE
            | windows::Win32::UI::WindowsAndMessaging::WS_THICKFRAME
            | windows::Win32::UI::WindowsAndMessaging::WS_MAXIMIZEBOX
            | windows::Win32::UI::WindowsAndMessaging::WS_MINIMIZEBOX;
        let mut outer = RECT { left: 0, top: 0, right: cw, bottom: ch };
        let _ = windows::Win32::UI::WindowsAndMessaging::AdjustWindowRectEx(
            &mut outer,
            style,
            false,
            windows::Win32::UI::WindowsAndMessaging::WS_EX_APPWINDOW,
        );
        // An editing session must be findable: taskbar button + Alt+Tab.
        let hwnd = CreateWindowExW(
            windows::Win32::UI::WindowsAndMessaging::WS_EX_APPWINDOW,
            w!("matteshot_tweak"),
            w!("Matteshot — tweak"),
            style,
            wx.max(mi.rcWork.left),
            wy.max(mi.rcWork.top),
            outer.right - outer.left,
            outer.bottom - outer.top,
            None,
            None,
            hinstance,
            Some(&mut *state as *mut State as *const _),
        )?;
        // Caret blink timer at the system rate; a rate of INFINITE means the
        // user disabled blinking — respect that and keep the caret static.
        let blink = windows::Win32::UI::WindowsAndMessaging::GetCaretBlinkTime();
        if blink != u32::MAX {
            let _ = windows::Win32::UI::WindowsAndMessaging::SetTimer(
                hwnd,
                1,
                blink.clamp(200, 1200),
                None,
            );
        }

        crate::theme::apply_titlebar(hwnd, &crate::theme::current());
        let _ = SetForegroundWindow(hwnd);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if msg.hwnd.0.is_null()
                && msg.message == windows::Win32::UI::WindowsAndMessaging::WM_HOTKEY
            {
                let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    hwnd,
                    WM_RESHOOT,
                    WPARAM(0),
                    LPARAM(0),
                );
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = DeleteObject(state.font);
        let _ = DeleteObject(state.font_small);
    }
    Ok(state.reshoot.take())
}

