//! Tweak panel: the optional editor for people who want to adjust the result.
//! Matte swap, padding slider, aspect presets, live preview. Reached with T
//! from the picker; the core loop never sees it.

use std::path::{Path, PathBuf};

use anyhow::{Error, Result};
use image::RgbaImage;
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    AlphaBlend, BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW,
    CreatePen, CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect,
    GetMonitorInfoW, HALFTONE, InvalidateRect, RoundRect, SelectObject, SetBkMode,
    SetStretchBltMode, SetTextColor, StretchDIBits, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, BLENDFUNCTION, CLEARTYPE_QUALITY, DEFAULT_CHARSET, DIB_RGB_COLORS, DT_CENTER, DT_LEFT,
    DT_RIGHT, DT_SINGLELINE, DT_VCENTER, FF_DONTCARE, HDC, HFONT, HMONITOR, MONITORINFO, PAINTSTRUCT,
    PS_SOLID, SRCCOPY, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, SetFocus, VK_ESCAPE, VK_RETURN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow,
    GetWindowLongPtrW, IsWindow, LoadCursorW, MessageBoxW, PostMessageW, RegisterClassW,
    SetForegroundWindow,
    SetWindowLongPtrW, CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, GWLP_USERDATA,
    IDC_ARROW, MB_ICONERROR, MB_ICONWARNING, MB_OK, WM_CLOSE, WM_CONTEXTMENU, WM_ERASEBKGND, WM_KEYDOWN,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCCREATE, WM_PAINT, WM_RBUTTONDOWN,
    WM_RBUTTONUP, WNDCLASSW, WS_CAPTION, WS_SYSMENU, WS_VISIBLE,
};

use rayon::prelude::*;

use crate::compose::{self, ComposeOpts};
use crate::config::Config;
use crate::output;
use crate::style::Style;



const ASPECTS: [(&str, Option<f32>); 6] = [
    ("Auto", None),
    ("1:1", Some(1.0)),
    ("4:3", Some(4.0 / 3.0)),
    ("3:2", Some(3.0 / 2.0)),
    ("16:9", Some(16.0 / 9.0)),
    ("Social", Some(1.91)),
];

/// Posted by the OCR worker with the recognized word boxes.
const WM_OCR_READY: u32 = windows::Win32::UI::WindowsAndMessaging::WM_USER + 43;
const WM_UNICHAR_MESSAGE: u32 = 0x0109;
const UNICODE_NOCHAR: usize = 0xFFFF;

/// The single editor window, or 0. One window holds every open capture.
static WINDOW: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
use std::sync::atomic::Ordering;

#[derive(Clone, Copy, PartialEq)]
enum Ctl {
    Matte(usize),
    Aspect(usize),
    OutputSize(u32),
    CustomSize,
    CustomSizeField,
    CustomSizeDone,
    CustomSizeCancel,
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

const TOOLS: [&str; 9] = ["Arrow", "Line", "Box", "Oval", "Mark", "Text", "Blur", "Step", "Pen"];
const SIZES: [f32; 3] = [0.7, 1.0, 1.4];
const PEN_TOOL: usize = 8;

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

/// Select-text mode: OCR word boxes over the preview, selected like real text.
struct TextSelect {
    /// Every recognized word in reading order. Empty while `pending`.
    words: Vec<crate::ocr::Word>,
    /// Selection ends, as indices into `words`.
    anchor: Option<usize>,
    focus: Option<usize>,
    /// Recognition is still running on the worker thread.
    pending: bool,
    /// Replaces the hint line: progress, copy confirmation, or failure.
    message: Option<String>,
    dragging: bool,
}

impl TextSelect {
    fn range(&self) -> Option<(usize, usize)> {
        match (self.anchor, self.focus) {
            (Some(a), Some(b)) => Some((a.min(b), a.max(b))),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum SliderDrag {
    Padding,
    CaptionSize,
    CaptionOpacity,
}

struct WindowLayout {
    controls: Vec<(RECT, Ctl)>,
    tab_strip: RECT,
    preview_box: RECT,
    padding_slider: RECT,
    caption_size_slider: RECT,
    caption_opacity_slider: RECT,
    caption_style_controls: Vec<(RECT, crate::annotate::TextStyle)>,
}

/// One capture being edited: its pixels, its matte choices, its annotations,
/// and the render caches derived from them.
///
/// Everything here is per-capture and will become one tab. Nothing in here may
/// reference the window: the split is what lets several of these live in a
/// single editor at once.
struct Document {
    /// Tab label: the captured window's title, or the region's size.
    title: String,
    raw: RgbaImage,
    small: RgbaImage,
    preview_metric: f32,
    /// Half-scale preview source. Dragging the padding slider changes the
    /// canvas geometry, so the whole matte has to be recomposed on every mouse
    /// move; at full resolution that is ~20ms a frame and the slider visibly
    /// trails the cursor. The drag runs at quarter the pixels and snaps back to
    /// full quality on release.
    small_fast: RgbaImage,
    metric_fast: f32,
    fast_preview: bool,
    styles: Vec<Style>,
    sel: usize,
    pad_factor: f32,
    aspect_idx: usize,
    /// Per-capture override for the finished image's longest edge.
    output_max_edge: u32,
    // Cached preview composite as BGRA.
    preview: Vec<u8>,
    preview_w: i32,
    preview_h: i32,
    /// Cached matte+shadow+content canvas keyed by (matte, pad bits, aspect,
    /// draft quality) — annotation edits only stamp onto a clone of this.
    base_cache: Option<((usize, u32, usize, bool), RgbaImage)>,
    // Annotations, in raw-capture coordinates.
    anns: Vec<crate::annotate::Annotation>,
    /// In-progress drag annotation (last element of `anns` while active).
    drawing: bool,
    /// Text annotation being typed (index into `anns`).
    editing: Option<usize>,
    /// Original content when re-editing. None means this is a new caption.
    editing_original: Option<String>,
    /// Annotation being dragged in selector mode: (index, last raw point,
    /// grabbed part).
    moving: Option<(usize, (f32, f32), Grab)>,
    /// Clicked annotation — Delete / color / size act on it.
    selected: Option<usize>,
    /// Next step-badge number.
    counter_next: u32,
    /// Annotation under the cursor in selector mode (highlight only).
    hover_ann: Option<usize>,
    /// Select-text mode, when armed. None means normal annotation editing.
    text_select: Option<TextSelect>,
    last_rebuild: std::time::Instant,
}

/// The editor window: chrome, layout, and the tool palette shared across every
/// open capture.
struct State {
    /// Open captures, in tab order.
    docs: Vec<Document>,
    /// Index into `docs` of the tab on screen. Always valid: the window closes
    /// when the last tab does.
    active: usize,
    /// Supersampling factor from settings; the same for every capture.
    export_scale: u32,
    controls: Vec<(RECT, Ctl)>,
    tab_strip: RECT,
    preview_box: RECT,
    slider_rect: RECT,
    caption_size_slider: RECT,
    caption_opacity_slider: RECT,
    caption_style_controls: Vec<(RECT, crate::annotate::TextStyle)>,
    hover: i32,
    dragging: Option<SliderDrag>,
    // Tool palette. Window-level on purpose: switching capture must not make
    // you re-pick your pen.
    tool: Option<usize>,
    color_idx: usize,
    size_idx: usize,
    caption_size: f32,
    caption_style: crate::annotate::TextStyle,
    caption_box_opacity: f32,
    /// Inline numeric editor for a per-capture output width or height.
    custom_size_edit: Option<CustomSizeEdit>,
    /// Caret blink phase while editing; one timer serves the window.
    caret_on: bool,
    font: HFONT,
    font_small: HFONT,
    scale: f32,
    width: i32,
    height: i32,
    theme: crate::theme::Theme,
}

struct CustomSizeEdit {
    input: String,
    replace_on_type: bool,
    invalid: bool,
    /// Restored by Cancel/Esc after live preview changes.
    original_max_edge: u32,
}

/// A drawn tab and the two things you can click on it.
#[derive(Clone, Copy)]
struct TabHit {
    rect: RECT,
    close: RECT,
}

/// Where each tab sits. Derived rather than stored so paint and hit-testing
/// can never disagree about it.
fn tab_rects(state: &State) -> Vec<TabHit> {
    let strip = state.tab_strip;
    let sc = |v: i32| (v as f32 * state.scale) as i32;
    let count = state.docs.len().max(1) as i32;
    let available = (strip.right - strip.left - sc(16)).max(sc(60));
    let width = (available / count).min(sc(190)).max(sc(64));
    let height = strip.bottom - strip.top - sc(6);
    (0..state.docs.len())
        .map(|i| {
            let left = strip.left + sc(8) + i as i32 * width;
            let rect = RECT {
                left,
                top: strip.top + sc(4),
                right: left + width - sc(4),
                bottom: strip.top + sc(4) + height,
            };
            let close = RECT {
                left: rect.right - sc(22),
                top: rect.top + sc(5),
                right: rect.right - sc(6),
                bottom: rect.bottom - sc(5),
            };
            TabHit { rect, close }
        })
        .collect()
}

impl State {
    fn doc(&self) -> &Document {
        &self.docs[self.active]
    }

    fn doc_mut(&mut self) -> &mut Document {
        &mut self.docs[self.active]
    }
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

fn annotation_tool_index(shape: &crate::annotate::Shape) -> usize {
    match shape {
        crate::annotate::Shape::Arrow { .. } => 0,
        crate::annotate::Shape::Line { .. } => 1,
        crate::annotate::Shape::Rect { .. } => 2,
        crate::annotate::Shape::Ellipse { .. } => 3,
        crate::annotate::Shape::Highlight { .. } => 4,
        crate::annotate::Shape::Text { .. } => 5,
        crate::annotate::Shape::Blur { .. } => 6,
        crate::annotate::Shape::Counter { .. } => 7,
        crate::annotate::Shape::Freehand { .. } => PEN_TOOL,
    }
}

fn tool_stays_active_after_use(tool: usize) -> bool {
    tool == PEN_TOOL
}

fn text_context(state: &State) -> bool {
    state.doc().editing.is_some()
        || state.tool == Some(5)
        || state.doc()
            .selected
            .and_then(|index| state.doc().anns.get(index))
            .is_some_and(|ann| matches!(ann.shape, crate::annotate::Shape::Text { .. }))
}

fn control_visible(state: &State, control: Ctl) -> bool {
    let custom_size = state.custom_size_edit.is_some();
    if matches!(
        control,
        Ctl::CustomSizeField | Ctl::CustomSizeDone | Ctl::CustomSizeCancel
    ) {
        return custom_size && !text_context(state);
    }
    if custom_size && matches!(control, Ctl::OutputSize(_) | Ctl::CustomSize) {
        return false;
    }
    !text_context(state)
        || !matches!(
            control,
            Ctl::OutputSize(_)
                | Ctl::CustomSize
                | Ctl::Ocr
                | Ctl::Size(_)
                | Ctl::CustomSizeField
                | Ctl::CustomSizeDone
                | Ctl::CustomSizeCancel
        )
}

fn sync_annotation_controls(state: &mut State, index: usize) {
    // Copied out first: the palette lives on the window and the annotation on
    // the document, and both hang off `state`.
    let Some(ann) = state.doc().anns.get(index).cloned() else {
        return;
    };
    let ann = &ann;
    state.color_idx = ann.color.min(crate::annotate::COLORS.len() - 1);
    state.size_idx = SIZES
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            (**a - ann.size)
                .abs()
                .partial_cmp(&(**b - ann.size).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(index, _)| index)
        .unwrap_or(1);
    state.caption_size = ann.size;
    state.caption_style = ann.text_style;
    state.caption_box_opacity = ann.text_box_opacity;
}

/// Preview source for the current interaction: reduced while a geometry drag
/// is in flight, full otherwise. Every coordinate mapping divides by the
/// metric, so both paths agree on raw-capture coordinates.
fn preview_source(state: &State) -> (&RgbaImage, f32) {
    if state.doc().fast_preview {
        (&state.doc().small_fast, state.doc().metric_fast)
    } else {
        (&state.doc().small, state.doc().preview_metric)
    }
}

fn opts_of(state: &State, metric: f32) -> ComposeOpts {
    ComposeOpts {
        metric_scale: metric,
        pad_factor: state.doc().pad_factor,
        aspect: ASPECTS[state.doc().aspect_idx].1,
    }
}

/// Locate a preview bitmap inside its viewport. Draft previews contain fewer
/// pixels, but `quality_scale` expands them back to their full-preview logical
/// size so changing render quality never changes the geometry on screen.
fn preview_draw_geometry(
    preview_box: RECT,
    preview_w: i32,
    preview_h: i32,
    quality_scale: f32,
) -> (i32, i32, f32, i32, i32) {
    let (bw, bh) = (
        preview_box.right - preview_box.left,
        preview_box.bottom - preview_box.top,
    );
    let logical_w = preview_w as f32 * quality_scale;
    let logical_h = preview_h as f32 * quality_scale;
    let fit = (bw as f32 / logical_w)
        .min(bh as f32 / logical_h)
        .min(1.0);
    let source_scale = quality_scale * fit;
    let (dw, dh) = (
        (preview_w as f32 * source_scale) as i32,
        (preview_h as f32 * source_scale) as i32,
    );
    let (dx, dy) = (
        preview_box.left + (bw - dw) / 2,
        preview_box.top + (bh - dh) / 2,
    );
    (dx, dy, source_scale, dw, dh)
}

/// Keep the selected output size visible in the editor. The fitted preview is
/// treated as Original size; smaller exports occupy proportionally less of the
/// viewport. Extremely small exports retain a practical editing floor.
fn output_preview_geometry(
    preview_box: RECT,
    preview_w: i32,
    preview_h: i32,
    quality_scale: f32,
    output_scale: f32,
) -> (i32, i32, f32, i32, i32) {
    let (_, _, source_scale, fitted_w, fitted_h) =
        preview_draw_geometry(preview_box, preview_w, preview_h, quality_scale);
    let output_scale = output_scale.clamp(0.30, 1.0);
    let draw_scale = source_scale * output_scale;
    let (dw, dh) = (
        (fitted_w as f32 * output_scale).round() as i32,
        (fitted_h as f32 * output_scale).round() as i32,
    );
    let (bw, bh) = (
        preview_box.right - preview_box.left,
        preview_box.bottom - preview_box.top,
    );
    let (dx, dy) = (
        preview_box.left + (bw - dw) / 2,
        preview_box.top + (bh - dh) / 2,
    );
    (dx, dy, draw_scale, dw, dh)
}

fn output_preview_scale(state: &State) -> f32 {
    let composed = composed_dimensions(state);
    let finished = final_dimensions(state);
    let composed_edge = composed.0.max(composed.1);
    if composed_edge == 0 {
        1.0
    } else {
        finished.0.max(finished.1) as f32 / composed_edge as f32
    }
}

fn preview_quality_scale(state: &State) -> f32 {
    let active_metric = preview_source(state).1;
    if active_metric > 0.0 {
        state.doc().preview_metric / active_metric
    } else {
        1.0
    }
}

/// Shared view transform: preview blit offset/scale and content padding.
fn view_params(state: &State) -> (i32, i32, f32, f32, f32) {
    let (dx, dy, draw_scale, _, _) = output_preview_geometry(
        state.preview_box,
        state.doc().preview_w,
        state.doc().preview_h,
        preview_quality_scale(state),
        output_preview_scale(state),
    );
    if compose::is_plain(&state.doc().styles[state.doc().sel]) {
        return (dx, dy, draw_scale, 0.0, 0.0);
    }
    let (source, metric) = preview_source(state);
    let opts = opts_of(state, metric);
    let l = compose::layout(source.width() as usize, source.height() as usize, &opts);
    (dx, dy, draw_scale, l.pad_x as f32, l.pad_y as f32)
}

/// Map a window point into raw-capture coordinates. None if far outside the
/// content area.
fn to_raw(state: &State, x: i32, y: i32) -> Option<(f32, f32)> {
    let (dx, dy, draw_scale, pad_x, pad_y) = view_params(state);
    let cx = (x - dx) as f32 / draw_scale;
    let cy = (y - dy) as f32 / draw_scale;
    let metric = preview_source(state).1;
    let rx = (cx - pad_x) / metric;
    let ry = (cy - pad_y) / metric;
    let (rw, rh) = (state.doc().raw.width() as f32, state.doc().raw.height() as f32);
    if rx < -40.0 || ry < -40.0 || rx > rw + 40.0 || ry > rh + 40.0 {
        return None;
    }
    Some((rx.clamp(0.0, rw), ry.clamp(0.0, rh)))
}

fn raw_to_screen(state: &State, p: (f32, f32)) -> (i32, i32) {
    let (dx, dy, draw_scale, pad_x, pad_y) = view_params(state);
    (
        dx + ((p.0 * preview_source(state).1 + pad_x) * draw_scale) as i32,
        dy + ((p.1 * preview_source(state).1 + pad_y) * draw_scale) as i32,
    )
}

fn freehand_bounds(points: &[(f32, f32)]) -> (f32, f32, f32, f32) {
    let Some(first) = points.first().copied() else {
        return (0.0, 0.0, 0.0, 0.0);
    };
    points.iter().copied().skip(1).fold(
        (first.0, first.1, first.0, first.1),
        |(x0, y0, x1, y1), point| {
            (x0.min(point.0), y0.min(point.1), x1.max(point.0), y1.max(point.1))
        },
    )
}

fn freehand_length(points: &[(f32, f32)]) -> f32 {
    points
        .windows(2)
        .map(|segment| {
            let dx = segment[1].0 - segment[0].0;
            let dy = segment[1].1 - segment[0].1;
            (dx * dx + dy * dy).sqrt()
        })
        .sum()
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
        crate::annotate::Shape::Freehand { points } => freehand_bounds(points),
        crate::annotate::Shape::Rect { a, b }
        | crate::annotate::Shape::Ellipse { a, b }
        | crate::annotate::Shape::Highlight { a, b }
        | crate::annotate::Shape::Blur { a, b } => {
            (a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1))
        }
        crate::annotate::Shape::Text { pos, text } => {
            let (width, height) = crate::annotate::caption_text_size(text, ann.size, 1.0)
                .unwrap_or((20, (24.0 * ann.size).max(12.0) as i32));
            (pos.0, pos.1, pos.0 + width as f32, pos.1 + height as f32)
        }
        crate::annotate::Shape::Counter { pos, .. } => {
            let r = 14.0 * ann.size;
            (pos.0 - r, pos.1 - r, pos.0 + r, pos.1 + r)
        }
    }
}

fn word_contains(word: &crate::ocr::Word, p: (f32, f32)) -> bool {
    let (x0, y0, x1, y1) = word.rect;
    p.0 >= x0 && p.0 <= x1 && p.1 >= y0 && p.1 <= y1
}

fn line_distance(word: &crate::ocr::Word, y: f32) -> f32 {
    let (_, y0, _, y1) = word.rect;
    if y < y0 {
        y0 - y
    } else if y > y1 {
        y - y1
    } else {
        0.0
    }
}

fn column_distance(word: &crate::ocr::Word, x: f32) -> f32 {
    let (x0, _, x1, _) = word.rect;
    if x < x0 {
        x0 - x
    } else if x > x1 {
        x - x1
    } else {
        0.0
    }
}

/// Nearest word for extending a selection: settle on the closest line band
/// first, then the closest word within it. Straight point-to-box distance
/// jumps between lines mid-sweep and feels broken.
fn nearest_word(words: &[crate::ocr::Word], p: (f32, f32)) -> Option<usize> {
    let closer = |a: f32, b: f32| a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal);
    let line = words
        .iter()
        .min_by(|a, b| closer(line_distance(a, p.1), line_distance(b, p.1)))?
        .line;
    words
        .iter()
        .enumerate()
        .filter(|(_, word)| word.line == line)
        .min_by(|(_, a), (_, b)| closer(column_distance(a, p.0), column_distance(b, p.0)))
        .map(|(index, _)| index)
}

/// Words under a pixelate-redact box are not selectable. OCR reads the raw
/// capture, so without this the redaction would be decorative: the hidden
/// text would still highlight and still copy.
fn redacted(anns: &[crate::annotate::Annotation], word: &crate::ocr::Word) -> bool {
    let (wx0, wy0, wx1, wy1) = word.rect;
    let area = ((wx1 - wx0) * (wy1 - wy0)).max(1.0);
    anns.iter().any(|ann| {
        let crate::annotate::Shape::Blur { a, b } = &ann.shape else {
            return false;
        };
        let overlap_x = (wx1.min(a.0.max(b.0)) - wx0.max(a.0.min(b.0))).max(0.0);
        let overlap_y = (wy1.min(a.1.max(b.1)) - wy0.max(a.1.min(b.1))).max(0.0);
        overlap_x * overlap_y > area * 0.5
    })
}

/// First selectable word containing the point, if any.
fn word_at(state: &State, p: (f32, f32)) -> Option<usize> {
    let select = state.doc().text_select.as_ref()?;
    select
        .words
        .iter()
        .position(|word| word_contains(word, p) && !redacted(&state.doc().anns, word))
}

/// Word to anchor a selection on: the one under the cursor, or the nearest one
/// within reach so a sweep can start in the margin beside a paragraph rather
/// than having to land exactly on the first character.
fn anchor_word(state: &State, p: (f32, f32)) -> Option<usize> {
    if let Some(index) = word_at(state, p) {
        return Some(index);
    }
    let select = state.doc().text_select.as_ref()?;
    let index = nearest_word(&select.words, p)?;
    let word = select.words.get(index)?;
    if redacted(&state.doc().anns, word) {
        return None;
    }
    let reach = (word.rect.3 - word.rect.1).max(8.0) * 1.5;
    (line_distance(word, p.1) <= reach && column_distance(word, p.0) <= reach).then_some(index)
}

/// Words as text: spaces within a line, newlines between them. `skip` drops
/// words without collapsing the line breaks around them.
fn join_words(words: &[crate::ocr::Word], skip: impl Fn(&crate::ocr::Word) -> bool) -> String {
    let mut text = String::new();
    let mut line: Option<usize> = None;
    for word in words {
        if skip(word) {
            continue;
        }
        match line {
            Some(previous) if previous == word.line => text.push(' '),
            Some(_) => text.push('\n'),
            None => {}
        }
        text.push_str(&word.text);
        line = Some(word.line);
    }
    text
}

fn selected_text(state: &State) -> String {
    let Some(select) = state.doc().text_select.as_ref() else {
        return String::new();
    };
    let Some((start, end)) = select.range() else {
        return String::new();
    };
    let Some(words) = select.words.get(start..=end.min(select.words.len() - 1)) else {
        return String::new();
    };
    join_words(words, |word| redacted(&state.doc().anns, word))
}

/// Arm select-text mode and kick off recognition. The engine takes a few
/// hundred milliseconds on a large capture, so it runs off the message loop
/// and the editor stays live while it works.
fn enter_text_select(hwnd: HWND, state: &mut State) {
    commit_editing(state);
    state.tool = None;
    state.doc_mut().selected = None;
    state.doc_mut().hover_ann = None;
    state.doc_mut().moving = None;
    state.doc_mut().text_select = Some(TextSelect {
        words: Vec::new(),
        anchor: None,
        focus: None,
        pending: true,
        message: None,
        dragging: false,
    });
    rebuild_preview(state);

    let raw = state.doc_mut().raw.clone();
    let target = hwnd.0 as isize;
    std::thread::spawn(move || {
        use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
        let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let payload = Box::into_raw(Box::new(
            crate::ocr::recognize_words(&raw).map_err(|error| format!("{error:#}")),
        ));
        if com.is_ok() {
            unsafe { CoUninitialize() };
        }
        unsafe {
            let hwnd = HWND(target as *mut _);
            if !crate::window::has_class(hwnd, "matteshot_tweak")
                || PostMessageW(hwnd, WM_OCR_READY, WPARAM(0), LPARAM(payload as isize)).is_err()
            {
                drop(Box::from_raw(payload));
            }
        }
    });
}

/// Translucent fills over the preview. GDI has no alpha on `Rectangle`, so a
/// 1x1 solid gets stretched through `AlphaBlend`. The source is built once per
/// call because select-text repaints hundreds of boxes on every mouse move.
unsafe fn wash_all(hdc: HDC, color: COLORREF, rects: &[(RECT, u8)]) {
    if rects.is_empty() {
        return;
    }
    let mem = CreateCompatibleDC(hdc);
    let bmp = CreateCompatibleBitmap(hdc, 1, 1);
    let old = SelectObject(mem, bmp);
    let brush = CreateSolidBrush(color);
    FillRect(mem, &RECT { left: 0, top: 0, right: 1, bottom: 1 }, brush);
    for (r, alpha) in rects {
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        if w <= 0 || h <= 0 {
            continue;
        }
        let _ = AlphaBlend(
            hdc,
            r.left,
            r.top,
            w,
            h,
            mem,
            0,
            0,
            1,
            1,
            BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: *alpha,
                AlphaFormat: 0,
            },
        );
    }
    SelectObject(mem, old);
    let _ = DeleteObject(bmp);
    let _ = DeleteObject(brush);
    let _ = DeleteDC(mem);
}

fn rebuild_preview(state: &mut State) {
    let (source, metric) = preview_source(state);
    let source = source.clone();
    let opts = opts_of(state, metric);
    let plain = compose::is_plain(&state.doc().styles[state.doc().sel]);
    let caret = if state.caret_on { state.doc_mut().editing } else { None };
    let mut img;
    let (off_x, off_y);
    if plain {
        img = source;
        (off_x, off_y) = (0.0, 0.0);
    } else {
        // The resolution is part of the key: a drag-quality base must never be
        // mistaken for the full-quality one when the drag ends.
        let key = (
            state.doc_mut().sel,
            state.doc_mut().pad_factor.to_bits(),
            state.doc_mut().aspect_idx,
            state.doc_mut().fast_preview,
        );
        if state.doc_mut().base_cache.as_ref().map(|(k, _)| *k) != Some(key) {
            let mut base = compose::compose_base(
                source.width() as usize,
                source.height() as usize,
                &state.doc().styles[state.doc().sel],
                &opts,
            );
            compose::blend_content(&mut base, &source, &opts);
            state.doc_mut().base_cache = Some((key, base));
        }
        img = state.doc_mut().base_cache.as_ref().unwrap().1.clone();
        let l = compose::layout(source.width() as usize, source.height() as usize, &opts);
        (off_x, off_y) = (l.pad_x as f32, l.pad_y as f32);
    }
    // Annotations stamp directly onto the composite at the content offset.
    crate::annotate::render(
        &mut img,
        &state.doc_mut().anns,
        metric,
        (off_x, off_y),
        caret,
    );
    state.doc_mut().last_rebuild = std::time::Instant::now();
    state.doc_mut().preview_w = img.width() as i32;
    state.doc_mut().preview_h = img.height() as i32;
    // RGBA to BGRA in place. A per-pixel push over a megapixel canvas was
    // several milliseconds of every rebuild on its own.
    let mut bgra = img.into_raw();
    bgra.par_chunks_mut(4).for_each(|pixel| {
        pixel.swap(0, 2);
        pixel[3] = 255;
    });
    state.doc_mut().preview = bgra;
}

/// Full-quality result with the current tweaks and annotations applied.
fn final_image(state: &State) -> RgbaImage {
    let raw = &state.doc().raw;
    let plain = compose::is_plain(&state.doc().styles[state.doc().sel]);
    let scale = if plain || raw.width().max(raw.height()) >= 1600 {
        1
    } else {
        state.export_scale.clamp(1, 4)
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
    crate::annotate::render(&mut content, &state.doc().anns, scale as f32, (0.0, 0.0), None);
    let finished = if plain {
        content
    } else {
        let opts = ComposeOpts {
            metric_scale: scale as f32,
            pad_factor: state.doc().pad_factor,
            aspect: ASPECTS[state.doc().aspect_idx].1,
        };
        compose::compose_with(&content, &state.doc().styles[state.doc().sel], &opts)
    };
    output::resize_to_max_edge(&finished, state.doc().output_max_edge)
}

/// Exact matte dimensions before the optional output-size cap, without
/// rendering the full-size image.
fn composed_dimensions(state: &State) -> (u32, u32) {
    let (mut width, mut height) = state.doc().raw.dimensions();
    if !compose::is_plain(&state.doc().styles[state.doc().sel]) {
        let scale = if width.max(height) >= 1600 {
            1
        } else {
            state.export_scale.clamp(1, 4)
        };
        width *= scale;
        height *= scale;
        let opts = ComposeOpts {
            metric_scale: scale as f32,
            pad_factor: state.doc().pad_factor,
            aspect: ASPECTS[state.doc().aspect_idx].1,
        };
        let layout = compose::layout(width as usize, height as usize, &opts);
        width += (layout.pad_x * 2) as u32;
        height += (layout.pad_y * 2) as u32;
    }
    (width, height)
}

/// Exact Copy/Save dimensions without rendering the full-size image.
fn final_dimensions(state: &State) -> (u32, u32) {
    let (width, height) = composed_dimensions(state);
    output::resized_dimensions(width, height, state.doc().output_max_edge)
}

fn custom_size_axis(dimensions: (u32, u32)) -> &'static str {
    if dimensions.0 >= dimensions.1 { "width" } else { "height" }
}

fn custom_size_bounds(dimensions: (u32, u32)) -> (u32, u32) {
    let maximum = dimensions.0.max(dimensions.1).min(output::OUTPUT_CUSTOM_MAX);
    (output::OUTPUT_CUSTOM_MIN.min(maximum), maximum)
}

fn custom_size_value(input: &str, dimensions: (u32, u32)) -> Option<u32> {
    let (minimum, maximum) = custom_size_bounds(dimensions);
    input
        .parse::<u32>()
        .ok()
        .filter(|value| (minimum..=maximum).contains(value))
}

fn custom_size_result(input: &str, dimensions: (u32, u32)) -> Option<(u32, u32)> {
    custom_size_value(input, dimensions)
        .map(|maximum| output::resized_dimensions(dimensions.0, dimensions.1, maximum))
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

unsafe fn clipped_label(hdc: HDC, state: &State, mut r: RECT, text: &str) {
    SelectObject(hdc, state.font_small);
    SetTextColor(hdc, state.theme.muted);
    let mut t = wide(text);
    DrawTextW(
        hdc,
        &mut t,
        &mut r,
        DT_LEFT
            | DT_SINGLELINE
            | DT_VCENTER
            | windows::Win32::Graphics::Gdi::DT_END_ELLIPSIS,
    );
}

unsafe fn custom_size_field(hdc: HDC, r: RECT, state: &State, _hot: bool) {
    let fill = CreateSolidBrush(state.theme.bg);
    let pen = CreatePen(PS_SOLID, 2, state.theme.accent);
    let old_brush = SelectObject(hdc, fill);
    let old_pen = SelectObject(hdc, pen);
    let _ = RoundRect(hdc, r.left, r.top, r.right, r.bottom, 10, 10);
    SelectObject(hdc, old_brush);
    SelectObject(hdc, old_pen);
    let _ = DeleteObject(fill);
    let _ = DeleteObject(pen);

    let edit = state.custom_size_edit.as_ref().unwrap();
    let axis = custom_size_axis(composed_dimensions(state)).to_uppercase();
    let pad = (10.0 * state.scale) as i32;
    let split = r.left + (96.0 * state.scale) as i32;
    let label_rect = RECT {
        left: r.left + pad,
        top: r.top,
        right: split,
        bottom: r.bottom,
    };
    clipped_label(hdc, state, label_rect, &format!("{axis} (PX)"));

    SelectObject(hdc, state.font);
    SetTextColor(hdc, state.theme.text);
    let caret = if state.caret_on { "\u{2502}" } else { " " };
    let mut value = wide(&format!("{}{caret}", edit.input));
    let mut value_rect = RECT {
        left: split,
        top: r.top,
        right: r.right - pad,
        bottom: r.bottom,
    };
    DrawTextW(hdc, &mut value, &mut value_rect, DT_RIGHT | DT_SINGLELINE | DT_VCENTER);
}

unsafe fn paint_slider(hdc: HDC, state: &State, rect: RECT, t: f32, active: bool) {
    let cy = (rect.top + rect.bottom) / 2;
    let track = CreateSolidBrush(state.theme.track);
    FillRect(
        hdc,
        &RECT { left: rect.left, top: cy - 2, right: rect.right, bottom: cy + 2 },
        track,
    );
    let _ = DeleteObject(track);
    let tx = rect.left + ((rect.right - rect.left) as f32 * t.clamp(0.0, 1.0)) as i32;
    let color = if active { state.theme.accent } else { state.theme.chip_line };
    let fill = CreateSolidBrush(color);
    FillRect(
        hdc,
        &RECT { left: rect.left, top: cy - 2, right: tx, bottom: cy + 2 },
        fill,
    );
    let pen = CreatePen(PS_SOLID, 1, color);
    let old_brush = SelectObject(hdc, fill);
    let old_pen = SelectObject(hdc, pen);
    let radius = (6.0 * state.scale).round() as i32;
    let _ = RoundRect(
        hdc,
        tx - radius,
        cy - radius,
        tx + radius,
        cy + radius,
        radius * 2,
        radius * 2,
    );
    SelectObject(hdc, old_brush);
    SelectObject(hdc, old_pen);
    let _ = DeleteObject(fill);
    let _ = DeleteObject(pen);
}

/// Tab strip across the top: one per open capture, active one lit.
unsafe fn paint_tabs(hdc: HDC, state: &State) {
    let sc = |v: i32| (v as f32 * state.scale) as i32;
    for (index, tab) in tab_rects(state).iter().enumerate() {
        let active = index == state.active;
        let fill = CreateSolidBrush(if active { state.theme.panel } else { state.theme.bg });
        let pen = CreatePen(
            PS_SOLID,
            1,
            if active { state.theme.accent } else { state.theme.chip_line },
        );
        let ob = SelectObject(hdc, fill);
        let op = SelectObject(hdc, pen);
        let _ = RoundRect(hdc, tab.rect.left, tab.rect.top, tab.rect.right, tab.rect.bottom, 8, 8);
        SelectObject(hdc, ob);
        SelectObject(hdc, op);
        let _ = DeleteObject(fill);
        let _ = DeleteObject(pen);

        SelectObject(hdc, state.font_small);
        SetTextColor(
            hdc,
            if active { state.theme.text } else { state.theme.muted },
        );
        let mut label = wide(
            state
                .docs
                .get(index)
                .map(|doc| doc.title.as_str())
                .unwrap_or("Capture"),
        );
        let mut text_rect = RECT {
            left: tab.rect.left + sc(10),
            top: tab.rect.top,
            right: tab.close.left - sc(4),
            bottom: tab.rect.bottom,
        };
        DrawTextW(
            hdc,
            &mut label,
            &mut text_rect,
            DT_LEFT
                | DT_SINGLELINE
                | DT_VCENTER
                | windows::Win32::Graphics::Gdi::DT_END_ELLIPSIS,
        );
        // Close affordance. Only worth drawing when closing a tab is not the
        // same as closing the window.
        if state.docs.len() > 1 {
            let mut cross = wide("\u{00d7}");
            let mut close_rect = tab.close;
            SetTextColor(hdc, state.theme.muted);
            DrawTextW(
                hdc,
                &mut cross,
                &mut close_rect,
                DT_CENTER | DT_SINGLELINE | DT_VCENTER,
            );
        }
    }
}

unsafe fn paint(hdc: HDC, state: &State) {
    let bg = CreateSolidBrush(state.theme.bg);
    FillRect(hdc, &RECT { left: 0, top: 0, right: state.width, bottom: state.height }, bg);
    let _ = DeleteObject(bg);
    SetBkMode(hdc, TRANSPARENT);
    paint_tabs(hdc, state);

    // Preview, letterboxed into its box.
    let (dx, dy, _, dw, dh) = output_preview_geometry(
        state.preview_box,
        state.doc().preview_w,
        state.doc().preview_h,
        preview_quality_scale(state),
        output_preview_scale(state),
    );
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: state.doc().preview_w,
            biHeight: -state.doc().preview_h,
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
        state.doc().preview_w,
        state.doc().preview_h,
        Some(state.doc().preview.as_ptr() as *const _),
        &info,
        DIB_RGB_COLORS,
        SRCCOPY,
    );

    // Selection / hover overlay (screen-space, no recompose).
    if state.tool.is_none() {
        for (idx, solid) in [(state.doc().selected, true), (state.doc().hover_ann, false)] {
            let Some(i) = idx else { continue };
            if !solid && state.doc().selected == Some(i) {
                continue;
            }
            let Some(ann) = state.doc().anns.get(i) else { continue };
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
        if let Some(i) = state.doc().selected {
            if let Some(ann) = state.doc().anns.get(i) {
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
                    | crate::annotate::Shape::Counter { .. }
                    | crate::annotate::Shape::Freehand { .. } => Vec::new(),
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

    // Select-text overlay: every recognized word faintly marked so you can
    // see what is grabbable, the selection filled solid. Screen-space, so a
    // sweep never recomposes the preview.
    if let Some(select) = state.doc().text_select.as_ref() {
        let range = select.range();
        let boxes: Vec<(RECT, u8)> = select
            .words
            .iter()
            .enumerate()
            .filter(|(_, word)| !redacted(&state.doc().anns, word))
            .map(|(index, word)| {
                let (x0, y0, x1, y1) = word.rect;
                let (sx0, sy0) = raw_to_screen(state, (x0, y0));
                let (sx1, sy1) = raw_to_screen(state, (x1, y1));
                let picked = range.is_some_and(|(a, b)| index >= a && index <= b);
                (
                    RECT { left: sx0 - 1, top: sy0 - 1, right: sx1 + 1, bottom: sy1 + 1 },
                    if picked { 96 } else { 26 },
                )
            })
            .collect();
        wash_all(hdc, state.theme.accent, &boxes);
    }

    // Contextual hint line under the preview.
    let select_hint = state.doc().text_select.as_ref().map(|select| {
        if let Some(message) = &select.message {
            message.clone()
        } else if select.pending {
            "reading text\u{2026}".into()
        } else {
            "drag to select text   \u{00b7}   double-click a word   \u{00b7}   Ctrl+A all   \u{00b7}   Ctrl+C copy   \u{00b7}   Esc exits"
                .to_string()
        }
    });
    // Only say something when there is something to say. A fresh editor with
    // no annotations does not need a list of things you cannot do yet.
    let hint: Option<&str> = if let Some(text) = &select_hint {
        Some(text.as_str())
    } else if state.custom_size_edit.is_some() {
        Some("type a size to preview it live   \u{00b7}   the other dimension adjusts automatically   \u{00b7}   Enter finishes   \u{00b7}   Esc restores")
    } else if state.doc().editing.is_some() {
        Some("type your caption   \u{00b7}   click anywhere to place   \u{00b7}   Esc cancel")
    } else if state.tool == Some(PEN_TOOL) {
        Some("drag on the preview to draw   \u{00b7}   Pen stays active   \u{00b7}   P or Esc exits")
    } else if state.tool.is_some() {
        Some("drag on the preview to draw   \u{00b7}   tool clears after each add")
    } else if !state.doc().anns.is_empty() {
        Some("drag to move   \u{00b7}   grab handles to reshape   \u{00b7}   right-click for properties   \u{00b7}   double-click text to edit   \u{00b7}   Del removes")
    } else {
        None
    };
    if let Some(hint) = hint {
        label(
            hdc,
            state,
            state.preview_box.left,
            state.height - (34.0 * state.scale) as i32,
            hint,
        );
    }

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
    if text_context(state) {
        label(
            hdc,
            state,
            state.caption_size_slider.left - (52.0 * state.scale) as i32,
            state.caption_size_slider.top - lh,
            "TEXT PROPERTIES",
        );
    } else if state.custom_size_edit.is_some() {
        let (r, _) = state
            .controls
            .iter()
            .find(|(_, control)| matches!(control, Ctl::CustomSizeField))
            .unwrap();
        let current = composed_dimensions(state);
        let edit = state.custom_size_edit.as_ref().unwrap();
        let input = &edit.input;
        let result = custom_size_result(input, current);
        let detail = if edit.invalid {
            let (minimum, maximum) = custom_size_bounds(current);
            format!("Use {minimum} to {maximum} px")
        } else if let Some((width, height)) = result {
            format!("Live result  {width} \u{00d7} {height} px")
        } else {
            let (minimum, maximum) = custom_size_bounds(current);
            format!("Use {minimum} to {maximum} px")
        };
        clipped_label(
            hdc,
            state,
            RECT { left: r.left, top: r.top - lh * 2, right: r.right, bottom: r.top - lh },
            "CUSTOM OUTPUT SIZE",
        );
        clipped_label(
            hdc,
            state,
            RECT { left: r.left, top: r.top - lh, right: r.right, bottom: r.top },
            &detail,
        );
    } else if let Some((r, _)) = state.controls.iter().find(|(_, c)| matches!(c, Ctl::OutputSize(_))) {
        clipped_label(
            hdc,
            state,
            RECT { left: r.left, top: r.top - lh * 2, right: r.left + (216.0 * state.scale) as i32, bottom: r.top - lh },
            "OUTPUT SIZE",
        );
        let (width, height) = final_dimensions(state);
        clipped_label(
            hdc,
            state,
            RECT { left: r.left, top: r.top - lh, right: r.left + (216.0 * state.scale) as i32, bottom: r.top },
            &format!("Final size: {width} × {height} px"),
        );
    }

    // Controls.
    for (i, (r, c)) in state.controls.iter().enumerate() {
        if !control_visible(state, *c) {
            continue;
        }
        let hot = i as i32 == state.hover;
        match c {
            Ctl::Matte(n) => chip(hdc, *r, state.doc().styles[*n].name, state, state.doc().sel == *n, hot),
            Ctl::Aspect(n) => {
                chip(hdc, *r, ASPECTS[*n].0, state, state.doc().aspect_idx == *n, hot)
            }
            Ctl::OutputSize(max_edge) => chip(
                hdc,
                *r,
                &output::output_size_label(*max_edge),
                state,
                state.doc().output_max_edge == *max_edge,
                hot,
            ),
            Ctl::CustomSize => {
                let custom = ![
                    output::OUTPUT_ORIGINAL,
                    output::OUTPUT_EMAIL,
                    output::OUTPUT_COMPACT,
                ]
                .contains(&state.doc().output_max_edge);
                let custom_label = if custom {
                    format!("{}px", state.doc().output_max_edge)
                } else {
                    format!("Set {}\u{2026}", custom_size_axis(composed_dimensions(state)))
                };
                chip(
                    hdc,
                    *r,
                    &custom_label,
                    state,
                    custom,
                    hot,
                )
            }
            Ctl::CustomSizeField => {
                custom_size_field(hdc, *r, state, hot)
            }
            Ctl::CustomSizeDone => chip(hdc, *r, "Done", state, true, hot),
            Ctl::CustomSizeCancel => chip(hdc, *r, "Cancel", state, false, hot),
            Ctl::Copy => chip(hdc, *r, "Copy", state, true, hot),
            Ctl::Save => chip(hdc, *r, "Save", state, false, hot),
            Ctl::Edit => chip(hdc, *r, "Editor", state, false, hot),
            Ctl::Tool(n) => {
                let property_tool = if state.tool.is_none() {
                    state.doc()
                        .selected
                        .and_then(|index| state.doc().anns.get(index))
                        .map(|ann| annotation_tool_index(&ann.shape))
                } else {
                    None
                };
                chip(
                    hdc,
                    *r,
                    TOOLS[*n],
                    state,
                    state.tool == Some(*n) || property_tool == Some(*n),
                    hot,
                )
            }
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
            Ctl::Ocr => chip(
                hdc,
                *r,
                "Select text",
                state,
                state.doc().text_select.is_some(),
                hot,
            ),
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

    if text_context(state) {
        let size_value = (state.caption_size * 24.0).round() as i32;
        label(
            hdc,
            state,
            state.caption_size_slider.left - (52.0 * state.scale) as i32,
            state.caption_size_slider.top,
            &format!("Size {size_value}"),
        );
        paint_slider(
            hdc,
            state,
            state.caption_size_slider,
            (state.caption_size - 0.7) / (3.4 - 0.7),
            true,
        );
        for (rect, style) in &state.caption_style_controls {
            chip(
                hdc,
                *rect,
                match style {
                    crate::annotate::TextStyle::Shadow => "Shadow",
                    crate::annotate::TextStyle::Box => "Caption box",
                },
                state,
                state.caption_style == *style,
                false,
            );
        }
        let opacity_active = state.caption_style == crate::annotate::TextStyle::Box;
        label(
            hdc,
            state,
            state.caption_opacity_slider.left - (52.0 * state.scale) as i32,
            state.caption_opacity_slider.top,
            &format!("Box {}%", (state.caption_box_opacity * 100.0).round() as i32),
        );
        paint_slider(
            hdc,
            state,
            state.caption_opacity_slider,
            (state.caption_box_opacity - 0.20) / 0.75,
            opacity_active,
        );
    }

    // Slider.
    let sr = state.slider_rect;
    let cy = (sr.top + sr.bottom) / 2;
    let track = CreateSolidBrush(state.theme.track);
    FillRect(hdc, &RECT { left: sr.left, top: cy - 2, right: sr.right, bottom: cy + 2 }, track);
    let _ = DeleteObject(track);
    let t = (state.doc().pad_factor - compose::PAD_SLIDER_MIN)
        / (compose::PAD_SLIDER_MAX - compose::PAD_SLIDER_MIN);
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
    let padding = compose::PAD_SLIDER_MIN
        + t * (compose::PAD_SLIDER_MAX - compose::PAD_SLIDER_MIN);
    if padding == state.doc_mut().pad_factor {
        return;
    }
    state.doc_mut().pad_factor = padding;
    rebuild_preview(state);
    let _ = InvalidateRect(hwnd, None, false);
}

#[derive(Debug)]
enum FinishError {
    Save(Error),
    Clipboard { path: PathBuf, source: Error },
}

fn persist_and_copy_with(
    save: impl FnOnce() -> Result<PathBuf>,
    copy: impl FnOnce(&Path) -> Result<()>,
) -> std::result::Result<PathBuf, FinishError> {
    let path = save().map_err(FinishError::Save)?;
    copy(&path).map_err(|source| FinishError::Clipboard {
        path: path.clone(),
        source,
    })?;
    Ok(path)
}

unsafe fn show_output_error(hwnd: HWND, summary: &str, error: &Error, save_failed: bool) {
    let message = HSTRING::from(format!("{summary}\n\n{error:#}"));
    let _ = MessageBoxW(
        hwnd,
        PCWSTR(message.as_ptr()),
        w!("Matteshot"),
        MB_OK | if save_failed { MB_ICONERROR } else { MB_ICONWARNING },
    );
}

/// Save the edited result, put it on the clipboard, and close. Shared by the
/// Copy chip and Ctrl+C so the keyboard can never do something subtly
/// different from the button.
unsafe fn copy_and_finish(hwnd: HWND, state: &mut State) {
    commit_editing(state);
    let img = final_image(state);
    let cfg = Config::load();
    let style_name = state.doc().styles[state.doc().sel].name;
    match persist_and_copy_with(
        || output::save_png(&img, style_name, &cfg.save_dir()),
        |path| output::to_clipboard(&img, Some(path)),
    ) {
        Ok(_) => {
            Config::update(|cfg| cfg.last_style = state.doc_mut().sel);
            let active = state.active;
            close_tab(hwnd, state, active);
        }
        Err(FinishError::Save(error)) => show_output_error(
            hwnd,
            "The edited screenshot could not be saved. Your tab is still open.",
            &error,
            true,
        ),
        Err(FinishError::Clipboard { path, source }) => show_output_error(
            hwnd,
            &format!(
                "The edited screenshot was saved to {} but could not be copied. Your tab is still open so you can try again.",
                path.display()
            ),
            &source,
            false,
        ),
    }
}

/// Leave drag quality and repaint at full resolution.
unsafe fn end_fast_preview(hwnd: HWND, state: &mut State) {
    if state.doc_mut().fast_preview {
        state.doc_mut().fast_preview = false;
        rebuild_preview(state);
        let _ = InvalidateRect(hwnd, None, false);
    }
}

unsafe fn caption_size_update(hwnd: HWND, state: &mut State, x: i32) {
    let rect = state.caption_size_slider;
    let t = ((x - rect.left) as f32 / (rect.right - rect.left).max(1) as f32).clamp(0.0, 1.0);
    state.caption_size = 0.7 + t * (3.4 - 0.7);
    let size = state.caption_size;
    if let Some(index) = state.doc().editing.or(state.doc().selected) {
        if let Some(ann) = state.doc_mut().anns.get_mut(index) {
            if matches!(ann.shape, crate::annotate::Shape::Text { .. }) {
                ann.size = size;
            }
        }
    }
    rebuild_preview(state);
    let _ = InvalidateRect(hwnd, None, false);
}

unsafe fn caption_opacity_update(hwnd: HWND, state: &mut State, x: i32) {
    let rect = state.caption_opacity_slider;
    let t = ((x - rect.left) as f32 / (rect.right - rect.left).max(1) as f32).clamp(0.0, 1.0);
    state.caption_box_opacity = 0.20 + t * 0.75;
    let opacity = state.caption_box_opacity;
    if let Some(index) = state.doc().editing.or(state.doc().selected) {
        if let Some(ann) = state.doc_mut().anns.get_mut(index) {
            if matches!(ann.shape, crate::annotate::Shape::Text { .. }) {
                ann.text_box_opacity = opacity;
            }
        }
    }
    rebuild_preview(state);
    let _ = InvalidateRect(hwnd, None, false);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TextInput {
    Changed,
    Commit,
    Ignored,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CustomInput {
    Changed,
    Commit,
    Ignored,
}

fn apply_text_input(text: &mut String, ch: char) -> TextInput {
    match ch {
        '\r' => TextInput::Commit,
        '\u{8}' => {
            text.pop();
            TextInput::Changed
        }
        ch if !ch.is_control() && text.chars().count() < 160 => {
            text.push(ch);
            TextInput::Changed
        }
        _ => TextInput::Ignored,
    }
}

/// Commit any in-progress text annotation (drop it if empty). Returns true
/// if a non-empty annotation was committed.
fn commit_editing(state: &mut State) -> bool {
    if let Some(i) = state.doc_mut().editing.take() {
        let empty = matches!(
            state.doc_mut().anns.get(i).map(|a| &a.shape),
            Some(crate::annotate::Shape::Text { text, .. }) if text.is_empty()
        );
        if empty {
            state.doc_mut().anns.remove(i);
            state.doc_mut().editing_original = None;
            return false;
        }
        // One-shot tools: a successful add returns to the selector.
        state.tool = None;
        state.doc_mut().editing_original = None;
        return true;
    }
    false
}

fn begin_custom_size(state: &mut State) {
    commit_editing(state);
    state.tool = None;
    state.doc_mut().text_select = None;
    let dimensions = composed_dimensions(state);
    let (minimum, maximum) = custom_size_bounds(dimensions);
    let current = state.doc().output_max_edge;
    let initial = if (minimum..=maximum).contains(&current) {
        current
    } else {
        maximum
    };
    state.custom_size_edit = Some(CustomSizeEdit {
        input: initial.to_string(),
        replace_on_type: true,
        invalid: false,
        original_max_edge: current,
    });
    state.caret_on = true;
}

fn preview_custom_size(state: &mut State) {
    let Some(input) = state.custom_size_edit.as_ref().map(|edit| edit.input.clone()) else {
        return;
    };
    if let Some(value) = custom_size_value(&input, composed_dimensions(state)) {
        state.doc_mut().output_max_edge = value;
    }
}

fn cancel_custom_size(state: &mut State) {
    if let Some(edit) = state.custom_size_edit.take() {
        state.doc_mut().output_max_edge = edit.original_max_edge;
    }
}

fn commit_custom_size(state: &mut State) -> bool {
    let Some(input) = state.custom_size_edit.as_ref().map(|edit| edit.input.clone()) else {
        return false;
    };
    let dimensions = composed_dimensions(state);
    if let Some(value) = custom_size_value(&input, dimensions) {
        state.doc_mut().output_max_edge = value;
        state.custom_size_edit = None;
        true
    } else {
        if let Some(edit) = state.custom_size_edit.as_mut() {
            edit.invalid = true;
        }
        false
    }
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
    let tol = (8.0 / state.doc().preview_metric).max(6.0);
    for (i, ann) in state.doc().anns.iter().enumerate().rev() {
        let hit = match &ann.shape {
            crate::annotate::Shape::Arrow { from, to }
            | crate::annotate::Shape::Line { from, to } => dist_seg(p, *from, *to) <= tol,
            crate::annotate::Shape::Freehand { points } => points
                .windows(2)
                .any(|segment| dist_seg(p, segment[0], segment[1]) <= tol),
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
            crate::annotate::Shape::Text { .. } => {
                let (x0, y0, x1, y1) = ann_bounds(ann);
                p.0 >= x0 - tol
                    && p.0 <= x1 + tol
                    && p.1 >= y0 - tol
                    && p.1 <= y1 + tol
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
        crate::annotate::Shape::Text { .. }
        | crate::annotate::Shape::Counter { .. }
        | crate::annotate::Shape::Freehand { .. } => {
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
        crate::annotate::Shape::Freehand { points } => points.iter_mut().for_each(shift),
    }
}

fn apply_custom_size_input(edit: &mut CustomSizeEdit, ch: char) -> CustomInput {
    match ch {
        '\r' => CustomInput::Commit,
        '\u{8}' => {
            if edit.replace_on_type {
                edit.input.clear();
            } else {
                edit.input.pop();
            }
            edit.replace_on_type = false;
            edit.invalid = false;
            CustomInput::Changed
        }
        ch if ch.is_ascii_digit()
            && (edit.replace_on_type
                || edit.input.len() < output::OUTPUT_CUSTOM_MAX.to_string().len()) =>
        {
            if edit.replace_on_type {
                edit.input.clear();
            }
            edit.input.push(ch);
            edit.replace_on_type = false;
            edit.invalid = false;
            CustomInput::Changed
        }
        _ => CustomInput::Ignored,
    }
}

unsafe fn activate(hwnd: HWND, state: &mut State, ctl: Ctl) {
    if state.custom_size_edit.is_some()
        && !matches!(
            ctl,
            Ctl::CustomSizeField | Ctl::CustomSizeDone | Ctl::CustomSizeCancel
        )
    {
        state.custom_size_edit = None;
    }
    // Reaching for an annotation tool leaves select-text mode. Matte, padding
    // and aspect are safe to keep it: word boxes live in capture coordinates,
    // so the overlay follows the new layout on its own.
    if matches!(ctl, Ctl::Tool(_) | Ctl::Undo | Ctl::Clear) {
        state.doc_mut().text_select = None;
    }
    match ctl {
        Ctl::Tool(n) => {
            commit_editing(state);
            if state.tool == Some(n) {
                state.tool = None;
            } else {
                state.tool = Some(n);
                state.doc_mut().selected = None;
                if n == 5 {
                    state.color_idx = 3;
                    state.caption_style = crate::annotate::TextStyle::Box;
                }
            }
            rebuild_preview(state);
            let _ = InvalidateRect(hwnd, None, false);
            return;
        }
        Ctl::Color(n) => {
            state.color_idx = n;
            // Recolor whatever is selected or being typed.
            let target = state.doc_mut().editing.or(state.doc_mut().selected);
            if let Some(i) = target {
                if let Some(ann) = state.doc_mut().anns.get_mut(i) {
                    ann.color = n;
                }
                rebuild_preview(state);
            }
            let _ = InvalidateRect(hwnd, None, false);
            return;
        }
        Ctl::Size(n) => {
            state.size_idx = n;
            let target = state.doc_mut().editing.or(state.doc_mut().selected);
            if let Some(i) = target {
                if let Some(ann) = state.doc_mut().anns.get_mut(i) {
                    ann.size = SIZES[n];
                }
                rebuild_preview(state);
            }
            let _ = InvalidateRect(hwnd, None, false);
            return;
        }
        Ctl::Undo => {
            commit_editing(state);
            if let Some(ann) = state.doc_mut().anns.pop() {
                if matches!(ann.shape, crate::annotate::Shape::Counter { .. }) {
                    state.doc_mut().counter_next = state.doc_mut().counter_next.saturating_sub(1).max(1);
                }
            }
            state.doc_mut().selected = None;
            state.doc_mut().hover_ann = None;
            rebuild_preview(state);
            let _ = InvalidateRect(hwnd, None, false);
            return;
        }
        Ctl::Clear => {
            state.doc_mut().editing = None;
            state.doc_mut().editing_original = None;
            state.doc_mut().anns.clear();
            state.doc_mut().selected = None;
            state.doc_mut().hover_ann = None;
            state.doc_mut().counter_next = 1;
            rebuild_preview(state);
            let _ = InvalidateRect(hwnd, None, false);
            return;
        }
        _ => {}
    }
    match ctl {
        Ctl::Matte(n) => {
            state.doc_mut().sel = n;
            rebuild_preview(state);
            let _ = InvalidateRect(hwnd, None, false);
        }
        Ctl::Aspect(n) => {
            state.doc_mut().aspect_idx = n;
            rebuild_preview(state);
            let _ = InvalidateRect(hwnd, None, false);
        }
        Ctl::OutputSize(max_edge) => {
            state.doc_mut().output_max_edge = max_edge;
            let _ = InvalidateRect(hwnd, None, false);
        }
        Ctl::CustomSize => {
            begin_custom_size(state);
            let _ = InvalidateRect(hwnd, None, false);
        }
        Ctl::CustomSizeField => {
            if let Some(edit) = state.custom_size_edit.as_mut() {
                edit.replace_on_type = true;
                edit.invalid = false;
            }
            state.caret_on = true;
            let _ = InvalidateRect(hwnd, None, false);
        }
        Ctl::CustomSizeDone => {
            commit_custom_size(state);
            let _ = InvalidateRect(hwnd, None, false);
        }
        Ctl::CustomSizeCancel => {
            cancel_custom_size(state);
            let _ = InvalidateRect(hwnd, None, false);
        }
        Ctl::Slider => {}
        Ctl::Ocr => {
            if state.doc_mut().text_select.is_some() {
                state.doc_mut().text_select = None;
            } else {
                enter_text_select(hwnd, state);
            }
            let _ = InvalidateRect(hwnd, None, false);
        }
        Ctl::Copy => copy_and_finish(hwnd, state),
        Ctl::Save => {
            let img = final_image(state);
            let cfg = Config::load();
            match output::save_png(&img, state.doc().styles[state.doc().sel].name, &cfg.save_dir()) {
                Ok(_) => {
                    let active = state.active;
                    close_tab(hwnd, state, active);
                }
                Err(error) => show_output_error(
                    hwnd,
                    "The edited screenshot could not be saved. Your tab is still open.",
                    &error,
                    true,
                ),
            }
        }
        Ctl::Edit => {
            let img = final_image(state);
            let cfg = Config::load();
            match output::save_png(&img, state.doc().styles[state.doc().sel].name, &cfg.save_dir()) {
                Ok(path) => {
                    output::open_in_editor(&path);
                    let active = state.active;
                    close_tab(hwnd, state, active);
                }
                Err(error) => show_output_error(
                    hwnd,
                    "The edited screenshot could not be saved or opened. Your tab is still open.",
                    &error,
                    true,
                ),
            }
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
                if state.doc_mut().text_select.as_ref().is_some_and(|s| s.dragging) {
                    let focus = to_raw(state, x, y).and_then(|p| {
                        nearest_word(&state.doc_mut().text_select.as_ref()?.words, p)
                    });
                    if let Some(select) = state.doc_mut().text_select.as_mut() {
                        if focus.is_some() && select.focus != focus {
                            select.focus = focus;
                            let _ = InvalidateRect(hwnd, None, false);
                        }
                    }
                    return LRESULT(0);
                }
                if let Some(drag) = state.dragging {
                    match drag {
                        SliderDrag::Padding => slider_update(hwnd, state, x),
                        SliderDrag::CaptionSize => caption_size_update(hwnd, state, x),
                        SliderDrag::CaptionOpacity => caption_opacity_update(hwnd, state, x),
                    }
                } else if let Some((i, last, grab)) = state.doc_mut().moving {
                    if let Some(p) = to_raw(state, x, y) {
                        if let Some(ann) = state.doc_mut().anns.get_mut(i) {
                            apply_grab(ann, grab, p, (p.0 - last.0, p.1 - last.1));
                        }
                        state.doc_mut().moving = Some((i, p, grab));
                        if state.doc_mut().last_rebuild.elapsed().as_millis() > 15 {
                            rebuild_preview(state);
                            let _ = InvalidateRect(hwnd, None, false);
                        }
                    }
                } else if state.doc_mut().drawing {
                    if let (Some(p), Some(ann)) = (to_raw(state, x, y), state.doc_mut().anns.last_mut()) {
                        match &mut ann.shape {
                            crate::annotate::Shape::Arrow { to, .. }
                            | crate::annotate::Shape::Line { to, .. } => *to = p,
                            crate::annotate::Shape::Rect { b, .. }
                            | crate::annotate::Shape::Ellipse { b, .. }
                            | crate::annotate::Shape::Highlight { b, .. }
                            | crate::annotate::Shape::Blur { b, .. } => *b = p,
                            crate::annotate::Shape::Freehand { points } => {
                                let far_enough = points.last().is_none_or(|last| {
                                    let dx = p.0 - last.0;
                                    let dy = p.1 - last.1;
                                    dx * dx + dy * dy >= 2.25
                                });
                                if far_enough {
                                    points.push(p);
                                }
                            }
                            _ => {}
                        }
                        // Annotation stamping is cheap now; near-frame-rate.
                        if state.doc_mut().last_rebuild.elapsed().as_millis() > 15 {
                            rebuild_preview(state);
                            let _ = InvalidateRect(hwnd, None, false);
                        }
                    }
                } else {
                    let hover = state
                        .controls
                        .iter()
                        .position(|(r, control)| {
                            control_visible(state, *control) && in_rect(r, x, y)
                        })
                        .map(|i| i as i32)
                        .unwrap_or(-1);
                    let hover_ann = if state.tool.is_none() && state.doc_mut().text_select.is_none() {
                        to_raw(state, x, y).and_then(|p| hit_ann(state, p))
                    } else {
                        None
                    };
                    if hover != state.hover || hover_ann != state.doc_mut().hover_ann {
                        state.hover = hover;
                        state.doc_mut().hover_ann = hover_ann;
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                }
            }
            LRESULT(0)
        }
        windows::Win32::UI::WindowsAndMessaging::WM_LBUTTONDBLCLK => {
            if let Some(state) = state_of(hwnd) {
                let _ = SetFocus(hwnd);
                let (x, y) = (
                    (lparam.0 & 0xFFFF) as i16 as i32,
                    ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
                );
                if state.doc_mut().text_select.is_some() {
                    // Double-click picks the single word under the cursor.
                    let hit = to_raw(state, x, y).and_then(|p| word_at(state, p));
                    if let Some(select) = state.doc_mut().text_select.as_mut() {
                        if hit.is_some() {
                            select.anchor = hit;
                            select.focus = hit;
                            select.message = None;
                        }
                    }
                    let _ = InvalidateRect(hwnd, None, false);
                    return LRESULT(0);
                }
                if state.tool.is_none() {
                    if let Some(p) = to_raw(state, x, y) {
                        if let Some(i) = hit_ann(state, p) {
                            if let crate::annotate::Shape::Text { text, .. } =
                                &state.doc_mut().anns[i].shape
                            {
                                let original = text.clone();
                                // Re-edit an existing caption.
                                state.doc_mut().moving = None;
                                let _ = ReleaseCapture();
                                state.doc_mut().editing_original = Some(original);
                                state.doc_mut().editing = Some(i);
                                state.doc_mut().selected = Some(i);
                                sync_annotation_controls(state, i);
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
                let _ = SetFocus(hwnd);
                let (x, y) = (
                    (lparam.0 & 0xFFFF) as i16 as i32,
                    ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
                );
                // Tabs first: they sit above everything else.
                if in_rect(&state.tab_strip, x, y) {
                    if let Some((index, tab)) = tab_rects(state)
                        .into_iter()
                        .enumerate()
                        .find(|(_, tab)| in_rect(&tab.rect, x, y))
                    {
                        if state.docs.len() > 1 && in_rect(&tab.close, x, y) {
                            close_tab(hwnd, state, index);
                        } else {
                            activate_tab(hwnd, state, index);
                        }
                    }
                    return LRESULT(0);
                }
                if state.custom_size_edit.is_some() {
                    let inside_inline_editor = state.controls.iter().any(|(rect, control)| {
                        matches!(
                            control,
                            Ctl::CustomSizeField
                                | Ctl::CustomSizeDone
                                | Ctl::CustomSizeCancel
                        ) && in_rect(rect, x, y)
                    });
                    if !inside_inline_editor {
                        state.custom_size_edit = None;
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                }
                if state.doc_mut().text_select.is_some() && in_rect(&state.preview_box, x, y) {
                    let hit = to_raw(state, x, y).and_then(|p| anchor_word(state, p));
                    if let Some(select) = state.doc_mut().text_select.as_mut() {
                        select.anchor = hit;
                        select.focus = hit;
                        select.dragging = hit.is_some();
                        select.message = None;
                    }
                    if hit.is_some() {
                        SetCapture(hwnd);
                    }
                    let _ = InvalidateRect(hwnd, None, false);
                    return LRESULT(0);
                }
                let grab = RECT {
                    left: state.slider_rect.left - 8,
                    top: state.slider_rect.top - 8,
                    right: state.slider_rect.right + 8,
                    bottom: state.slider_rect.bottom + 8,
                };
                if text_context(state) && in_rect(&state.caption_size_slider, x, y) {
                    state.dragging = Some(SliderDrag::CaptionSize);
                    SetCapture(hwnd);
                    caption_size_update(hwnd, state, x);
                    return LRESULT(0);
                }
                if text_context(state)
                    && state.caption_style == crate::annotate::TextStyle::Box
                    && in_rect(&state.caption_opacity_slider, x, y)
                {
                    state.dragging = Some(SliderDrag::CaptionOpacity);
                    SetCapture(hwnd);
                    caption_opacity_update(hwnd, state, x);
                    return LRESULT(0);
                }
                if text_context(state) {
                    if let Some((_, style)) = state
                        .caption_style_controls
                        .iter()
                        .find(|(rect, _)| in_rect(rect, x, y))
                        .copied()
                    {
                        state.caption_style = style;
                        if let Some(index) = state.doc_mut().editing.or(state.doc_mut().selected) {
                            if let Some(ann) = state.doc_mut().anns.get_mut(index) {
                                if matches!(ann.shape, crate::annotate::Shape::Text { .. }) {
                                    ann.text_style = style;
                                }
                            }
                        }
                        rebuild_preview(state);
                        let _ = InvalidateRect(hwnd, None, false);
                        return LRESULT(0);
                    }
                }
                // Clicking away accepts the caption and returns to Select.
                // Enter remains optional, matching the video editor.
                if state.doc_mut().editing.is_some() {
                    commit_editing(state);
                    rebuild_preview(state);
                }
                if in_rect(&grab, x, y) {
                    commit_editing(state);
                    state.dragging = Some(SliderDrag::Padding);
                    // Padding changes the canvas geometry, so nothing caches
                    // between frames. Drop to drag quality for the duration.
                    state.doc_mut().fast_preview = true;
                    SetCapture(hwnd);
                    slider_update(hwnd, state, x);
                } else if let (Some(tool), Some(p)) = (state.tool, to_raw(state, x, y)) {
                    commit_editing(state);
                    let color = state.color_idx;
                    let size = SIZES[state.size_idx];
                    // Palette values copied out: pushing into the document
                    // borrows all of `state`.
                    let box_opacity = state.caption_box_opacity;
                    let caption_size = state.caption_size;
                    let caption_style = state.caption_style;
                    use crate::annotate::Shape;
                    let drag_shape = match tool {
                        0 => Some(Shape::Arrow { from: p, to: p }),
                        1 => Some(Shape::Line { from: p, to: p }),
                        2 => Some(Shape::Rect { a: p, b: p }),
                        3 => Some(Shape::Ellipse { a: p, b: p }),
                        4 => Some(Shape::Highlight { a: p, b: p }),
                        6 => Some(Shape::Blur { a: p, b: p }),
                        PEN_TOOL => Some(Shape::Freehand { points: vec![p] }),
                        _ => None,
                    };
                    if let Some(shape) = drag_shape {
                        state.doc_mut().anns.push(crate::annotate::Annotation {
                            shape,
                            color,
                            size,
                            text_style: crate::annotate::TextStyle::Shadow,
                            text_box_opacity: box_opacity,
                        });
                        state.doc_mut().drawing = true;
                        SetCapture(hwnd);
                    } else if tool == 7 {
                        // Step badge: click places, auto-numbered, one-shot.
                        let n = state.doc_mut().counter_next;
                        state.doc_mut().counter_next += 1;
                        state.doc_mut().anns.push(crate::annotate::Annotation {
                            shape: Shape::Counter { pos: p, n },
                            color,
                            size,
                            text_style: crate::annotate::TextStyle::Shadow,
                            text_box_opacity: box_opacity,
                        });
                        state.doc_mut().selected = Some(state.doc_mut().anns.len() - 1);
                        state.tool = None;
                        rebuild_preview(state);
                        let _ = InvalidateRect(hwnd, None, false);
                    } else {
                        // Text: click places, then type.
                        state.doc_mut().anns.push(crate::annotate::Annotation {
                            shape: Shape::Text { pos: p, text: String::new() },
                            color: 3,
                            size: caption_size,
                            text_style: caption_style,
                            text_box_opacity: box_opacity,
                        });
                        state.doc_mut().editing = Some(state.doc_mut().anns.len() - 1);
                        state.doc_mut().editing_original = None;
                        state.doc_mut().selected = Some(state.doc_mut().anns.len() - 1);
                        state.caret_on = true;
                        rebuild_preview(state);
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                } else {
                    if state.doc_mut().editing.is_some() {
                        commit_editing(state);
                        rebuild_preview(state);
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                    // Selector mode: grab an existing annotation (whole,
                    // endpoint, or corner) to manipulate it.
                    if let Some(p) = to_raw(state, x, y) {
                        if let Some(i) = hit_ann(state, p) {
                            let tol = (8.0 / state.doc_mut().preview_metric).max(6.0);
                            normalize_rect(&mut state.doc_mut().anns[i]);
                            let grab = grab_probe(&state.doc_mut().anns[i], p, tol);
                            state.doc_mut().selected = Some(i);
                            sync_annotation_controls(state, i);
                            state.doc_mut().moving = Some((i, p, grab));
                            SetCapture(hwnd);
                            let _ = InvalidateRect(hwnd, None, false);
                        } else if state.doc_mut().selected.take().is_some() {
                            let _ = InvalidateRect(hwnd, None, false);
                        }
                    }
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if let Some(state) = state_of(hwnd) {
                if state.doc_mut().text_select.as_ref().is_some_and(|s| s.dragging) {
                    if let Some(select) = state.doc_mut().text_select.as_mut() {
                        select.dragging = false;
                    }
                    let _ = ReleaseCapture();
                    let _ = InvalidateRect(hwnd, None, false);
                    return LRESULT(0);
                }
                if state.dragging.take().is_some() {
                    let _ = ReleaseCapture();
                    end_fast_preview(hwnd, state);
                    return LRESULT(0);
                }
                if state.doc_mut().moving.take().is_some() {
                    let _ = ReleaseCapture();
                    rebuild_preview(state);
                    let _ = InvalidateRect(hwnd, None, false);
                    return LRESULT(0);
                }
                if state.doc_mut().drawing {
                    state.doc_mut().drawing = false;
                    let _ = ReleaseCapture();
                    // Drop degenerate shapes (a stray click).
                    let degenerate = match state.doc_mut().anns.last().map(|a| &a.shape) {
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
                        Some(crate::annotate::Shape::Freehand { points }) => {
                            freehand_length(points) < 3.0
                        }
                        _ => false,
                    };
                    if degenerate {
                        state.doc_mut().anns.pop();
                    } else {
                        // Shapes are one-shot, but Pen stays armed so lifting
                        // the mouse does not interrupt handwriting or a
                        // multi-stroke drawing.
                        if !state.tool.is_some_and(tool_stays_active_after_use) {
                            state.tool = None;
                        }
                        state.doc_mut().selected = Some(state.doc_mut().anns.len() - 1);
                    }
                    rebuild_preview(state);
                    let _ = InvalidateRect(hwnd, None, false);
                    return LRESULT(0);
                }
                let (x, y) = (
                    (lparam.0 & 0xFFFF) as i16 as i32,
                    ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
                );
                if let Some(i) = state.controls.iter().position(|(r, control)| {
                    control_visible(state, *control) && in_rect(r, x, y)
                }) {
                    let ctl = state.controls[i].1;
                    activate(hwnd, state, ctl);
                }
            }
            LRESULT(0)
        }
        WM_RBUTTONDOWN => {
            if let Some(state) = state_of(hwnd) {
                let _ = SetFocus(hwnd);
                let (x, y) = (
                    (lparam.0 & 0xFFFF) as i16 as i32,
                    ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
                );
                if state.doc_mut().editing.is_some() {
                    commit_editing(state);
                }
                if let Some(point) = to_raw(state, x, y) {
                    if let Some(index) = hit_ann(state, point) {
                        state.tool = None;
                        state.doc_mut().selected = Some(index);
                        sync_annotation_controls(state, index);
                        rebuild_preview(state);
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                }
            }
            LRESULT(0)
        }
        windows::Win32::UI::WindowsAndMessaging::WM_MBUTTONDOWN => {
            if let Some(state) = state_of(hwnd) {
                let (x, y) = (
                    (lparam.0 & 0xFFFF) as i16 as i32,
                    ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
                );
                if let Some((index, _)) = tab_rects(state)
                    .into_iter()
                    .enumerate()
                    .find(|(_, tab)| in_rect(&tab.rect, x, y))
                {
                    close_tab(hwnd, state, index);
                }
            }
            LRESULT(0)
        }
        WM_RBUTTONUP | WM_CONTEXTMENU => LRESULT(0),
        windows::Win32::UI::WindowsAndMessaging::WM_TIMER => {
            if let Some(state) = state_of(hwnd) {
                if state.custom_size_edit.is_some() {
                    state.caret_on = !state.caret_on;
                    let _ = InvalidateRect(hwnd, None, false);
                } else if state.doc_mut().editing.is_some() {
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
        windows::Win32::UI::WindowsAndMessaging::WM_CHAR | WM_UNICHAR_MESSAGE => {
            if let Some(state) = state_of(hwnd) {
                if msg == WM_UNICHAR_MESSAGE && wparam.0 == UNICODE_NOCHAR {
                    return LRESULT(1);
                }
                // Typing keeps the caret solid, like every real text box.
                state.caret_on = true;
                if state.custom_size_edit.is_some() {
                    let ch = char::from_u32(wparam.0 as u32).unwrap_or('\0');
                    let action = apply_custom_size_input(state.custom_size_edit.as_mut().unwrap(), ch);
                    if action == CustomInput::Commit {
                        commit_custom_size(state);
                    } else if action == CustomInput::Changed {
                        preview_custom_size(state);
                    }
                    if action != CustomInput::Ignored {
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                    return LRESULT(0);
                }
                if let Some(i) = state.doc_mut().editing {
                    let ch = char::from_u32(wparam.0 as u32).unwrap_or('\0');
                    let action = state.doc_mut()
                        .anns
                        .get_mut(i)
                        .and_then(|ann| match &mut ann.shape {
                            crate::annotate::Shape::Text { text, .. } => {
                                Some(apply_text_input(text, ch))
                            }
                            _ => None,
                        })
                        .unwrap_or(TextInput::Ignored);
                    if action == TextInput::Commit {
                        commit_editing(state);
                    }
                    if action != TextInput::Ignored {
                        rebuild_preview(state);
                    }
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
                if state.custom_size_edit.is_some() {
                    let key = wparam.0 as u16;
                    if key == VK_ESCAPE.0 {
                        cancel_custom_size(state);
                        let _ = InvalidateRect(hwnd, None, false);
                    } else if key == VK_RETURN.0 {
                        commit_custom_size(state);
                        let _ = InvalidateRect(hwnd, None, false);
                    } else if ctrl_down && key == b'A' as u16 {
                        if let Some(edit) = state.custom_size_edit.as_mut() {
                            edit.replace_on_type = true;
                            edit.invalid = false;
                        }
                        state.caret_on = true;
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                    return LRESULT(0);
                }
                // Select-text mode owns Esc and the clipboard keys while armed;
                // everything else still falls through to the editor.
                if state.doc_mut().text_select.is_some() {
                    let key = wparam.0 as u16;
                    if key == VK_ESCAPE.0 {
                        state.doc_mut().text_select = None;
                        let _ = InvalidateRect(hwnd, None, false);
                        return LRESULT(0);
                    }
                    if ctrl_down && key == b'A' as u16 {
                        let last = state.doc_mut()
                            .text_select
                            .as_ref()
                            .map(|select| select.words.len())
                            .unwrap_or(0);
                        if let Some(select) = state.doc_mut().text_select.as_mut() {
                            if last > 0 {
                                select.anchor = Some(0);
                                select.focus = Some(last - 1);
                                select.message = None;
                            }
                        }
                        let _ = InvalidateRect(hwnd, None, false);
                        return LRESULT(0);
                    }
                    if ctrl_down && key == b'C' as u16 {
                        let text = selected_text(state);
                        let message = if text.is_empty() {
                            "select some text first".to_string()
                        } else {
                            let count = text.chars().count();
                            match output::text_to_clipboard(&text) {
                                Ok(()) => format!("copied {count} characters"),
                                Err(error) => {
                                    eprintln!("ocr copy failed: {error:#}");
                                    "could not copy to the clipboard".to_string()
                                }
                            }
                        };
                        if let Some(select) = state.doc_mut().text_select.as_mut() {
                            select.message = Some(message);
                        }
                        let _ = InvalidateRect(hwnd, None, false);
                        return LRESULT(0);
                    }
                }
                // Ctrl+C copies the finished image, exactly as the Copy chip
                // does. Not while typing a caption, where it means the text.
                if ctrl_down && wparam.0 as u16 == b'C' as u16 && state.doc().editing.is_none() {
                    copy_and_finish(hwnd, state);
                    return LRESULT(0);
                }
                if ctrl_down {
                    let key = wparam.0 as u16;
                    let shift = windows::Win32::UI::Input::KeyboardAndMouse::GetKeyState(
                        windows::Win32::UI::Input::KeyboardAndMouse::VK_SHIFT.0 as i32,
                    ) < 0;
                    let count = state.docs.len();
                    if key == windows::Win32::UI::Input::KeyboardAndMouse::VK_TAB.0 && count > 1 {
                        let next = if shift {
                            (state.active + count - 1) % count
                        } else {
                            (state.active + 1) % count
                        };
                        activate_tab(hwnd, state, next);
                        return LRESULT(0);
                    }
                    if key == b'W' as u16 {
                        let active = state.active;
                        close_tab(hwnd, state, active);
                        return LRESULT(0);
                    }
                    // Ctrl+1..8 pick a tab directly, Ctrl+9 the last one,
                    // matching every browser.
                    if (b'1' as u16..=b'9' as u16).contains(&key) {
                        if let Some(target) =
                            tab_for_digit((key - b'1' as u16) as usize, count)
                        {
                            activate_tab(hwnd, state, target);
                        }
                        return LRESULT(0);
                    }
                }
                match wparam.0 as u16 {
                    v if v == VK_ESCAPE.0 => {
                        if let Some(i) = state.doc_mut().editing.take() {
                            if let Some(original) = state.doc_mut().editing_original.take() {
                                if let Some(crate::annotate::Shape::Text { text, .. }) =
                                    state.doc_mut().anns.get_mut(i).map(|ann| &mut ann.shape)
                                {
                                    *text = original;
                                }
                                state.doc_mut().selected = Some(i);
                            } else if i < state.doc_mut().anns.len() {
                                // A brand-new empty caption is discarded.
                                state.doc_mut().anns.remove(i);
                                state.doc_mut().selected = None;
                            }
                            state.tool = None;
                            rebuild_preview(state);
                            let _ = InvalidateRect(hwnd, None, false);
                        } else if state.tool.take().is_some() {
                            // Esc leaves an armed tool first. A second Esc can
                            // close the tab, which is especially important for
                            // the persistent Pen mode.
                            let _ = InvalidateRect(hwnd, None, false);
                        } else {
                            let active = state.active;
                            close_tab(hwnd, state, active);
                        }
                    }
                    v if v == VK_RETURN.0 => {
                        if state.doc_mut().editing.is_some() {
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
                        state.doc_mut().anns.pop();
                        state.doc_mut().selected = None;
                        state.doc_mut().hover_ann = None;
                        rebuild_preview(state);
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                    // Delete removes the selected annotation.
                    0x2E if state.doc_mut().editing.is_none() => {
                        if let Some(i) = state.doc_mut().selected.take() {
                            if i < state.doc_mut().anns.len() {
                                state.doc_mut().anns.remove(i);
                            }
                            state.doc_mut().hover_ann = None;
                            rebuild_preview(state);
                            let _ = InvalidateRect(hwnd, None, false);
                        }
                    }
                    // Tool shortcuts (A/R/T/B/P) and colors (1-4).
                    0x41 if state.doc_mut().editing.is_none() => activate(hwnd, state, Ctl::Tool(0)), // Arrow
                    0x52 if state.doc_mut().editing.is_none() => activate(hwnd, state, Ctl::Tool(2)), // Rectangle
                    0x54 if state.doc_mut().editing.is_none() => activate(hwnd, state, Ctl::Tool(5)), // Text
                    0x42 if state.doc_mut().editing.is_none() => activate(hwnd, state, Ctl::Tool(6)), // Blur
                    0x50 if state.doc_mut().editing.is_none() => activate(
                        hwnd,
                        state,
                        Ctl::Tool(PEN_TOOL),
                    ), // Pen
                    v @ 0x31..=0x34 if state.doc_mut().editing.is_none() => {
                        activate(hwnd, state, Ctl::Color((v - 0x31) as usize))
                    }
                    _ => {}
                }
            }
            LRESULT(0)
        }
        // PrtScn mid-tweak: nested overlay; the frozen image includes this
        // window, so it's snippable. Esc there returns here untouched.
        WM_OCR_READY => {
            if lparam.0 == 0 {
                return LRESULT(0);
            }
            let payload = Box::from_raw(
                lparam.0 as *mut std::result::Result<Vec<crate::ocr::Word>, String>,
            );
            if let Some(state) = state_of(hwnd) {
                if let Some(select) = state.doc_mut().text_select.as_mut() {
                    // A stale result from a mode the user already left has
                    // nothing to attach to.
                    if select.pending {
                        select.pending = false;
                        match *payload {
                            Ok(words) if words.is_empty() => {
                                select.message = Some("no text found in this capture".into());
                            }
                            Ok(words) => select.words = words,
                            Err(error) => {
                                eprintln!("ocr failed: {error}");
                                select.message =
                                    Some("could not read text from this capture".into());
                            }
                        }
                        let _ = InvalidateRect(hwnd, None, false);
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
                    let id = if state.doc_mut().text_select.is_some() {
                        if raw.is_some() && in_rect(&state.preview_box, pt.x, pt.y) {
                            windows::Win32::UI::WindowsAndMessaging::IDC_IBEAM
                        } else {
                            ARROW
                        }
                    } else if state.tool.is_some() && raw.is_some() {
                        IDC_CROSS
                    } else if let Some((_, _, grab)) = state.doc_mut().moving {
                        // Mid-drag: keep showing what the drag is doing.
                        cursor_for(grab)
                    } else if let Some(p) = raw {
                        match hit_ann(state, p) {
                            Some(i) => {
                                let tol = (8.0 / state.doc_mut().preview_metric).max(6.0);
                                cursor_for(grab_probe(&state.doc_mut().anns[i], p, tol))
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
                    let next = layout_controls(state.scale, w, h, state.doc_mut().styles.len());
                    state.controls = next.controls;
                    state.preview_box = next.preview_box;
                    state.slider_rect = next.padding_slider;
                    state.caption_size_slider = next.caption_size_slider;
                    state.caption_opacity_slider = next.caption_opacity_slider;
                    state.caption_style_controls = next.caption_style_controls;
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
                (*mmi).ptMinTrackSize.y = (620.0 * s) as i32;
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        windows::Win32::UI::WindowsAndMessaging::WM_NCDESTROY => {
            // The editor lives on the resident's message loop now, so its
            // state and GDI objects are released here rather than after a
            // nested loop returns.
            let ptr = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) as *mut State;
            if !ptr.is_null() {
                let state = Box::from_raw(ptr);
                let _ = DeleteObject(state.font);
                let _ = DeleteObject(state.font_small);
            }
            WINDOW.store(0, Ordering::SeqCst);
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
) -> WindowLayout {
    let sc = |v: i32| (v as f32 * scale) as i32;
    let m = sc(20);
    let col_x = cw - sc(250);
    // The tab strip is always present, even with one capture open, so adding
    // a second one never reflows everything underneath it.
    let tab_strip = RECT { left: 0, top: 0, right: cw, bottom: sc(34) };
    let top = tab_strip.bottom + sc(10);
    // Bottom strip reserved for the contextual hint line.
    let preview_box = RECT { left: m, top, right: col_x - sc(16), bottom: ch - m - sc(18) };
    let mut controls = Vec::new();
    let mut y = top + sc(22);
    for i in 0..n_styles {
        let row = i as i32 / 2;
        let colm = i as i32 % 2;
        let x = if n_styles % 2 == 1 && i + 1 == n_styles {
            col_x + sc(56)
        } else {
            col_x + colm * sc(112)
        };
        controls.push((
            RECT {
                left: x,
                top: y + row * sc(38),
                right: x + sc(104),
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
    let output_y = by - sc(114);
    let caption_size_slider = RECT {
        left: col_x + sc(64),
        top: output_y,
        right: col_x + sc(216),
        bottom: output_y + sc(26),
    };
    let caption_style_controls = vec![
        (
            RECT {
                left: col_x,
                top: output_y + sc(34),
                right: col_x + sc(104),
                bottom: output_y + sc(62),
            },
            crate::annotate::TextStyle::Shadow,
        ),
        (
            RECT {
                left: col_x + sc(112),
                top: output_y + sc(34),
                right: col_x + sc(216),
                bottom: output_y + sc(62),
            },
            crate::annotate::TextStyle::Box,
        ),
    ];
    let caption_opacity_slider = RECT {
        left: col_x + sc(64),
        top: output_y + sc(68),
        right: col_x + sc(216),
        bottom: output_y + sc(94),
    };
    for (i, ctl) in [
        Ctl::OutputSize(output::OUTPUT_ORIGINAL),
        Ctl::OutputSize(output::OUTPUT_EMAIL),
        Ctl::OutputSize(output::OUTPUT_COMPACT),
        Ctl::CustomSize,
    ]
    .into_iter()
    .enumerate()
    {
        let row = i as i32 / 2;
        let colm = i as i32 % 2;
        controls.push((
            RECT {
                left: col_x + colm * sc(112),
                top: output_y + row * sc(34),
                right: col_x + colm * sc(112) + sc(104),
                bottom: output_y + row * sc(34) + sc(28),
            },
            ctl,
        ));
    }
    controls.push((
        RECT {
            left: col_x,
            top: output_y,
            right: col_x + sc(216),
            bottom: output_y + sc(34),
        },
        Ctl::CustomSizeField,
    ));
    controls.push((
        RECT {
            left: col_x,
            top: output_y + sc(40),
            right: col_x + sc(104),
            bottom: output_y + sc(68),
        },
        Ctl::CustomSizeDone,
    ));
    controls.push((
        RECT {
            left: col_x + sc(112),
            top: output_y + sc(40),
            right: col_x + sc(216),
            bottom: output_y + sc(68),
        },
        Ctl::CustomSizeCancel,
    ));
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
    WindowLayout {
        controls,
        tab_strip,
        preview_box,
        padding_slider: slider_rect,
        caption_size_slider,
        caption_opacity_slider,
        caption_style_controls,
    }
}

/// Build the per-capture half of the editor: preview sources and the starting
/// matte choices.
fn build_document(
    raw: RgbaImage,
    styles: Vec<Style>,
    initial: usize,
    title: String,
) -> Document {
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
    // Drag-quality source, built once so a padding drag never pays for a
    // resize per frame.
    let small_fast = image::imageops::resize(
        &small,
        (small.width() / 2).max(1),
        (small.height() / 2).max(1),
        image::imageops::FilterType::Triangle,
    );
    Document {
        title,
        raw,
        small,
        preview_metric: scale,
        small_fast,
        metric_fast: scale * 0.5,
        fast_preview: false,
        styles,
        sel: initial,
        pad_factor: compose::DEFAULT_PAD_FACTOR,
        aspect_idx: 0,
        output_max_edge: Config::load().output_max_edge,
        preview: Vec::new(),
        preview_w: 1,
        preview_h: 1,
        base_cache: None,
        anns: Vec::new(),
        drawing: false,
        editing: None,
        editing_original: None,
        moving: None,
        selected: None,
        counter_next: 1,
        hover_ann: None,
        text_select: None,
        last_rebuild: std::time::Instant::now(),
    }
}

/// Drop the render caches of the capture leaving the screen. `raw` and the
/// user's choices stay; everything else is rebuilt on the way back.
///
/// This matters more than it looks: a scrolling capture can be 19000px tall,
/// which is ~146MB of raw pixels on its own, and the derived layers add most
/// of that again. Rebuilding costs one compose, which is cheap now.
fn release_inactive(state: &mut State) {
    let doc = state.doc_mut();
    doc.base_cache = None;
    doc.preview = Vec::new();
    doc.preview_w = 1;
    doc.preview_h = 1;
    // Word boxes are tied to this capture and cheap to keep, but the selection
    // highlight would be stale against a rebuilt preview.
    doc.fast_preview = false;
}

/// Show tab `index`, rebuilding what `release_inactive` threw away.
unsafe fn activate_tab(hwnd: HWND, state: &mut State, index: usize) {
    if index >= state.docs.len() || index == state.active {
        return;
    }
    release_inactive(state);
    state.active = index;
    state.tool = None;
    state.custom_size_edit = None;
    rebuild_preview(state);
    let _ = InvalidateRect(hwnd, None, false);
}

/// Which tab to show after closing one. `remaining` is the count *after* the
/// removal and is never zero (the window closes instead).
fn active_after_close(active: usize, closed: usize, remaining: usize) -> usize {
    debug_assert!(remaining > 0);
    if closed < active {
        // Everything after the closed tab shifted left, including the active
        // one, so follow it.
        active - 1
    } else {
        // Closing the active tab lands on its right-hand neighbour, or the new
        // last tab when it was the rightmost.
        active.min(remaining - 1)
    }
}

/// Tab index for Ctrl+1..9. 9 always means the last tab, as in every browser.
fn tab_for_digit(digit: usize, count: usize) -> Option<usize> {
    if count == 0 {
        return None;
    }
    let target = if digit == 8 { count - 1 } else { digit };
    (target < count).then_some(target)
}

/// Close one capture. The window goes with the last of them.
unsafe fn close_tab(hwnd: HWND, state: &mut State, index: usize) {
    if index >= state.docs.len() {
        return;
    }
    state.docs.remove(index);
    if state.docs.is_empty() {
        let _ = DestroyWindow(hwnd);
        return;
    }
    state.active = active_after_close(state.active, index, state.docs.len());
    state.tool = None;
    state.custom_size_edit = None;
    rebuild_preview(state);
    let _ = InvalidateRect(hwnd, None, false);
}

/// Bring an existing editor to the front. After a tray menu the process has
/// lost foreground rights, so a plain SetForegroundWindow is not enough.
unsafe fn surface(hwnd: HWND) {
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, ShowWindow, HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOMOVE, SWP_NOSIZE, SW_RESTORE,
    };
    let _ = ShowWindow(hwnd, SW_RESTORE);
    let _ = SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
    let _ = SetWindowPos(hwnd, HWND_NOTOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
    let _ = SetForegroundWindow(hwnd);
}

/// Whether an editor window is currently open.
pub fn is_open() -> bool {
    let hwnd = HWND(WINDOW.load(Ordering::SeqCst) as *mut _);
    !hwnd.0.is_null() && unsafe { IsWindow(hwnd) }.as_bool()
}

/// Open the editor on a capture. Non-modal: this returns immediately and the
/// window lives on the resident's message loop, like settings and pins.
pub fn open(
    raw: RgbaImage,
    styles: Vec<Style>,
    initial: usize,
    monitor: HMONITOR,
    title: String,
) -> Result<()> {
    let document = build_document(raw, styles, initial, title);
    unsafe {
        let existing = HWND(WINDOW.load(Ordering::SeqCst) as *mut _);
        if !existing.0.is_null() && IsWindow(existing).as_bool() {
            if let Some(state) = state_of(existing) {
                // A further capture joins the window as its own tab rather
                // than replacing what is already being edited.
                release_inactive(state);
                state.docs.push(document);
                state.active = state.docs.len() - 1;
                state.tool = None;
                state.custom_size_edit = None;
                rebuild_preview(state);
                let _ = InvalidateRect(existing, None, false);
            }
            surface(existing);
            return Ok(());
        }
    }
    create_window(document, monitor)
}

fn create_window(document: Document, monitor: HMONITOR) -> Result<()> {
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

    let initial_layout = layout_controls(dpi_scale, cw, ch, document.styles.len());
    let mut state = Box::new(State {
        docs: vec![document],
        active: 0,
        export_scale: Config::load().export_scale,
        controls: initial_layout.controls,
        tab_strip: initial_layout.tab_strip,
        preview_box: initial_layout.preview_box,
        slider_rect: initial_layout.padding_slider,
        caption_size_slider: initial_layout.caption_size_slider,
        caption_opacity_slider: initial_layout.caption_opacity_slider,
        caption_style_controls: initial_layout.caption_style_controls,
        hover: -1,
        dragging: None,
        tool: None,
        color_idx: 0,
        caret_on: true,
        size_idx: 1,
        caption_size: 1.75,
        caption_style: crate::annotate::TextStyle::Box,
        caption_box_opacity: 0.68,
        custom_size_edit: None,
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
            // The window owns the state from here; WM_NCDESTROY reclaims it.
            Some(Box::into_raw(state) as *const _),
        )?;
        WINDOW.store(hwnd.0 as isize, Ordering::SeqCst);
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
        surface(hwnd);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::path::PathBuf;

    use super::{
        active_after_close, ann_bounds, annotation_tool_index, apply_custom_size_input,
        apply_text_input, custom_size_axis, custom_size_bounds, custom_size_result,
        freehand_length, join_words, layout_controls, nearest_word, persist_and_copy_with,
        output_preview_geometry, preview_draw_geometry, redacted, tab_for_digit,
        tool_stays_active_after_use,
        translate_ann, Ctl, CustomInput, CustomSizeEdit, FinishError, TextInput, ASPECTS,
        PEN_TOOL, TOOLS,
    };
    use windows::Win32::Foundation::RECT;

    #[test]
    fn padding_drag_quality_does_not_shrink_preview() {
        for preview_box in [
            RECT { left: 0, top: 0, right: 1200, bottom: 800 },
            RECT { left: 50, top: 25, right: 750, bottom: 525 },
        ] {
            let full = preview_draw_geometry(preview_box, 960, 600, 1.0);
            let draft = preview_draw_geometry(preview_box, 480, 300, 2.0);

            assert_eq!((draft.0, draft.1, draft.3, draft.4), (full.0, full.1, full.3, full.4));
            assert!((draft.2 - full.2 * 2.0).abs() < f32::EPSILON);
        }
    }

    #[test]
    fn failed_save_never_attempts_the_clipboard() {
        let copied = Cell::new(false);
        let result = persist_and_copy_with(
            || Err(anyhow::anyhow!("disk full")),
            |_| {
                copied.set(true);
                Ok(())
            },
        );
        assert!(matches!(result, Err(FinishError::Save(_))));
        assert!(!copied.get());
    }

    #[test]
    fn clipboard_failure_keeps_the_saved_path() {
        let saved = PathBuf::from(r"C:\captures\kept.png");
        let result = persist_and_copy_with(
            || Ok(saved.clone()),
            |_| Err(anyhow::anyhow!("clipboard busy")),
        );
        match result {
            Err(FinishError::Clipboard { path, .. }) => assert_eq!(path, saved),
            other => panic!("expected clipboard failure, got {other:?}"),
        }
    }

    #[test]
    fn successful_copy_returns_the_saved_path() {
        let saved = PathBuf::from(r"C:\captures\finished.png");
        let copied = Cell::new(false);
        let result = persist_and_copy_with(
            || Ok(saved.clone()),
            |path| {
                copied.set(path == saved);
                Ok(())
            },
        );
        assert_eq!(result.unwrap(), saved);
        assert!(copied.get());
    }

    #[test]
    fn closing_a_tab_lands_on_the_right_neighbour() {
        // [a b* c] close c -> b stays active at 1.
        assert_eq!(active_after_close(1, 2, 2), 1);
        // [a b c*] close c -> the new last tab.
        assert_eq!(active_after_close(2, 2, 2), 1);
        // [a* b c] close a -> b, which slid into slot 0.
        assert_eq!(active_after_close(0, 0, 2), 0);
        // [a b* c] close a -> b moved left, follow it.
        assert_eq!(active_after_close(1, 0, 2), 0);
        // [a b c*] close a -> c moved left to 1.
        assert_eq!(active_after_close(2, 0, 2), 1);
        // Down to one tab, whichever went.
        assert_eq!(active_after_close(1, 0, 1), 0);
        assert_eq!(active_after_close(0, 1, 1), 0);
    }

    #[test]
    fn ctrl_nine_is_always_the_last_tab() {
        assert_eq!(tab_for_digit(0, 5), Some(0));
        assert_eq!(tab_for_digit(3, 5), Some(3));
        assert_eq!(tab_for_digit(8, 5), Some(4));
        assert_eq!(tab_for_digit(8, 1), Some(0));
        // A digit past the open tabs does nothing rather than jumping.
        assert_eq!(tab_for_digit(4, 3), None);
        assert_eq!(tab_for_digit(0, 0), None);
    }
    use crate::annotate::{Annotation, Shape, TextStyle};

    fn word(text: &str, line: usize, rect: (f32, f32, f32, f32)) -> crate::ocr::Word {
        crate::ocr::Word { text: text.into(), rect, line }
    }

    fn blur(a: (f32, f32), b: (f32, f32)) -> Annotation {
        Annotation {
            shape: Shape::Blur { a, b },
            color: 0,
            size: 1.0,
            text_style: TextStyle::Shadow,
            text_box_opacity: 1.0,
        }
    }

    #[test]
    fn sweeping_a_selection_stays_on_the_line_under_the_cursor() {
        let words = [
            word("alpha", 0, (0.0, 0.0, 50.0, 20.0)),
            word("beta", 1, (200.0, 100.0, 250.0, 120.0)),
        ];
        // The cursor sits inside line 0's band but far to the right, closer to
        // beta's box by raw distance. Reading order has to win, or a sweep
        // jumps lines the moment it passes the end of a short line.
        assert_eq!(nearest_word(&words, (210.0, 12.0)), Some(0));
        assert_eq!(nearest_word(&words, (210.0, 108.0)), Some(1));
        assert_eq!(nearest_word(&[], (0.0, 0.0)), None);
    }

    #[test]
    fn redaction_removes_the_words_it_covers_and_leaves_the_rest() {
        let covered = word("secret", 0, (100.0, 100.0, 200.0, 120.0));
        let anns = [blur((90.0, 90.0), (210.0, 130.0))];
        assert!(redacted(&anns, &covered));

        // A box that only clips a corner is not redaction.
        let grazed = [blur((190.0, 110.0), (300.0, 200.0))];
        assert!(!redacted(&grazed, &covered));

        // Corners given in any order still describe the same box.
        let reversed = [blur((210.0, 130.0), (90.0, 90.0))];
        assert!(redacted(&reversed, &covered));

        // Other annotation kinds never hide text.
        let boxed = [Annotation {
            shape: Shape::Rect { a: (90.0, 90.0), b: (210.0, 130.0) },
            ..blur((0.0, 0.0), (0.0, 0.0))
        }];
        assert!(!redacted(&boxed, &covered));
    }

    #[test]
    fn copied_text_keeps_line_breaks_and_omits_redacted_words() {
        let words = [
            word("Hello", 0, (0.0, 0.0, 40.0, 10.0)),
            word("world", 0, (45.0, 0.0, 90.0, 10.0)),
            word("second", 1, (0.0, 20.0, 50.0, 30.0)),
            word("line", 1, (55.0, 20.0, 90.0, 30.0)),
        ];
        assert_eq!(join_words(&words, |_| false), "Hello world\nsecond line");
        // Dropping a word must not weld its neighbours' lines together.
        assert_eq!(
            join_words(&words, |w| w.text == "world"),
            "Hello\nsecond line"
        );
        assert_eq!(join_words(&words, |_| true), "");
    }

    #[test]
    fn caption_input_accepts_unicode_without_requiring_enter() {
        let mut text = String::new();
        for ch in "Clean caption 👍".chars() {
            assert_eq!(apply_text_input(&mut text, ch), TextInput::Changed);
        }
        assert_eq!(text, "Clean caption 👍");
        assert_eq!(apply_text_input(&mut text, '\u{8}'), TextInput::Changed);
        assert_eq!(text, "Clean caption ");
        assert_eq!(apply_text_input(&mut text, '\r'), TextInput::Commit);
        assert_eq!(apply_text_input(&mut text, '\n'), TextInput::Ignored);

        let mut capped = "x".repeat(160);
        assert_eq!(apply_text_input(&mut capped, 'y'), TextInput::Ignored);
        assert_eq!(capped.chars().count(), 160);
    }

    #[test]
    fn custom_size_uses_the_dimension_people_expect_and_shows_the_result() {
        let landscape = (2712, 1592);
        assert_eq!(custom_size_axis(landscape), "width");
        assert_eq!(custom_size_bounds(landscape), (320, 2712));
        assert_eq!(custom_size_result("1920", landscape), Some((1920, 1127)));
        assert_eq!(custom_size_result("2713", landscape), None);
        assert_eq!(custom_size_result("319", landscape), None);

        let portrait = (1200, 2000);
        assert_eq!(custom_size_axis(portrait), "height");
        assert_eq!(custom_size_result("1000", portrait), Some((600, 1000)));

        // Very small captures keep their current size as the only meaningful
        // custom value; very tall captures retain the existing 10k guardrail.
        assert_eq!(custom_size_bounds((200, 100)), (200, 200));
        assert_eq!(custom_size_bounds((1200, 19_000)), (320, 10_000));
    }

    #[test]
    fn custom_size_typing_replaces_the_prefilled_dimension() {
        let mut edit = CustomSizeEdit {
            input: "2712".into(),
            replace_on_type: true,
            invalid: true,
            original_max_edge: 0,
        };
        for ch in "1920".chars() {
            assert_eq!(apply_custom_size_input(&mut edit, ch), CustomInput::Changed);
        }
        assert_eq!(edit.input, "1920");
        assert!(!edit.replace_on_type);
        assert!(!edit.invalid);
        assert_eq!(apply_custom_size_input(&mut edit, '\u{8}'), CustomInput::Changed);
        assert_eq!(edit.input, "192");
        assert_eq!(apply_custom_size_input(&mut edit, 'x'), CustomInput::Ignored);
        assert_eq!(apply_custom_size_input(&mut edit, '\r'), CustomInput::Commit);
    }

    #[test]
    fn every_annotation_shape_opens_the_matching_property_tool() {
        assert_eq!(
            annotation_tool_index(&Shape::Arrow { from: (0.0, 0.0), to: (1.0, 1.0) }),
            0
        );
        assert_eq!(
            annotation_tool_index(&Shape::Line { from: (0.0, 0.0), to: (1.0, 1.0) }),
            1
        );
        assert_eq!(
            annotation_tool_index(&Shape::Rect { a: (0.0, 0.0), b: (1.0, 1.0) }),
            2
        );
        assert_eq!(
            annotation_tool_index(&Shape::Ellipse { a: (0.0, 0.0), b: (1.0, 1.0) }),
            3
        );
        assert_eq!(
            annotation_tool_index(&Shape::Highlight { a: (0.0, 0.0), b: (1.0, 1.0) }),
            4
        );
        assert_eq!(
            annotation_tool_index(&Shape::Text { pos: (0.0, 0.0), text: String::new() }),
            5
        );
        assert_eq!(
            annotation_tool_index(&Shape::Blur { a: (0.0, 0.0), b: (1.0, 1.0) }),
            6
        );
        assert_eq!(annotation_tool_index(&Shape::Counter { pos: (0.0, 0.0), n: 1 }), 7);
        assert_eq!(
            annotation_tool_index(&Shape::Freehand { points: vec![(0.0, 0.0), (1.0, 1.0)] }),
            8
        );
    }

    #[test]
    fn pen_is_the_only_tool_that_stays_armed_after_a_stroke() {
        for tool in 0..TOOLS.len() {
            assert_eq!(tool_stays_active_after_use(tool), tool == PEN_TOOL);
        }
    }

    #[test]
    fn output_size_changes_are_visible_without_moving_the_preview() {
        let preview_box = RECT { left: 100, top: 50, right: 1100, bottom: 750 };
        let original = output_preview_geometry(preview_box, 960, 600, 1.0, 1.0);
        let compact = output_preview_geometry(preview_box, 960, 600, 1.0, 0.5);
        let tiny = output_preview_geometry(preview_box, 960, 600, 1.0, 0.1);

        assert_eq!((compact.3, compact.4), (original.3 / 2, original.4 / 2));
        assert!(compact.0 > original.0 && compact.1 > original.1);
        assert_eq!(compact.0 + compact.3 / 2, original.0 + original.3 / 2);
        assert_eq!(compact.1 + compact.4 / 2, original.1 + original.4 / 2);
        // Keep very small outputs editable while still making the reduction
        // unmistakable.
        assert_eq!(tiny.3, (original.3 as f32 * 0.30).round() as i32);
        assert_eq!(tiny.4, (original.4 as f32 * 0.30).round() as i32);

        let draft = output_preview_geometry(preview_box, 480, 300, 2.0, 0.5);
        assert_eq!((draft.0, draft.1, draft.3, draft.4), (compact.0, compact.1, compact.3, compact.4));
        assert!((draft.2 - compact.2 * 2.0).abs() < f32::EPSILON);
    }

    #[test]
    fn freehand_annotations_keep_their_path_when_moved() {
        let mut annotation = Annotation {
            shape: Shape::Freehand {
                points: vec![(4.0, 8.0), (10.0, 16.0), (20.0, 12.0)],
            },
            color: 0,
            size: 1.0,
            text_style: TextStyle::Shadow,
            text_box_opacity: 1.0,
        };

        assert_eq!(ann_bounds(&annotation), (4.0, 8.0, 20.0, 16.0));
        assert!((freehand_length(&[(0.0, 0.0), (3.0, 4.0)]) - 5.0).abs() < 1e-6);

        translate_ann(&mut annotation, (5.0, -3.0));
        let Shape::Freehand { points } = annotation.shape else {
            panic!("freehand shape changed while moving");
        };
        assert_eq!(points, vec![(9.0, 5.0), (15.0, 13.0), (25.0, 9.0)]);
    }

    #[test]
    fn editor_choice_groups_are_visually_balanced() {
        let layout = layout_controls(1.0, 1180, 760, 7);
        let rect_for = |control| {
            layout
                .controls
                .iter()
                .find(|(_, candidate)| *candidate == control)
                .map(|(rect, _)| *rect)
                .unwrap()
        };

        let first = rect_for(Ctl::Matte(0));
        let second = rect_for(Ctl::Matte(1));
        let none = rect_for(Ctl::Matte(6));
        assert_eq!(none.left + none.right, first.left + second.right);

        let aspects: Vec<_> = layout
            .controls
            .iter()
            .filter(|(_, control)| matches!(control, Ctl::Aspect(_)))
            .collect();
        let tools: Vec<_> = layout
            .controls
            .iter()
            .filter(|(_, control)| matches!(control, Ctl::Tool(_)))
            .collect();
        assert_eq!(aspects.len(), ASPECTS.len());
        assert_eq!(tools.len(), TOOLS.len());
        assert_eq!(aspects.iter().filter(|(rect, _)| rect.top == aspects[0].0.top).count(), 3);
        assert_eq!(tools.iter().filter(|(rect, _)| rect.top == tools[0].0.top).count(), 3);
        assert_eq!(tools.iter().filter(|(rect, _)| rect.top == tools[8].0.top).count(), 3);
    }

    #[test]
    fn inline_custom_size_controls_fit_above_the_ocr_action() {
        for scale in [1.0, 1.25, 1.5, 2.0] {
            let layout = layout_controls(scale, (1180.0 * scale) as i32, (760.0 * scale) as i32, 7);
            let rect_for = |control| {
                layout
                    .controls
                    .iter()
                    .find(|(_, candidate)| *candidate == control)
                    .map(|(rect, _)| *rect)
                    .unwrap()
            };
            let field = rect_for(Ctl::CustomSizeField);
            let done = rect_for(Ctl::CustomSizeDone);
            let cancel = rect_for(Ctl::CustomSizeCancel);
            let ocr = rect_for(Ctl::Ocr);

            assert_eq!(field.left, done.left);
            assert_eq!(field.right, cancel.right);
            assert!(field.bottom < done.top);
            assert_eq!((done.top, done.bottom), (cancel.top, cancel.bottom));
            assert!(done.bottom < ocr.top);
        }
    }

    #[test]
    fn text_properties_fit_cleanly_above_picture_actions_at_every_dpi() {
        for scale in [1.0, 1.25, 1.5, 2.0] {
            let width = (1180.0 * scale) as i32;
            let height = (760.0 * scale) as i32;
            let layout = layout_controls(scale, width, height, 7);
            let action_top = layout
                .controls
                .iter()
                .filter(|(_, control)| matches!(control, Ctl::Copy | Ctl::Save | Ctl::Edit))
                .map(|(rect, _)| rect.top)
                .min()
                .unwrap();
            let opacity_bottom = layout.caption_opacity_slider.bottom;
            assert!(
                opacity_bottom + (12.0 * scale).round() as i32 <= action_top,
                "text properties ended at {opacity_bottom}, actions began at {action_top}"
            );
            assert!(layout.caption_size_slider.bottom < layout.caption_style_controls[0].0.top);
            assert!(layout.caption_style_controls[0].0.bottom < layout.caption_opacity_slider.top);
            assert!(layout.caption_style_controls.iter().all(|(rect, _)| {
                rect.left >= 0 && rect.right <= width && rect.bottom <= height
            }));
        }
    }
}

