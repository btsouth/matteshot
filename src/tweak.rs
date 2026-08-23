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
    /// The crop button. Its own control rather than `Tool(CROP_TOOL)` so the
    /// chip loop over `TOOLS` can never be asked to label it.
    Crop,
    Tool(usize),
    Color(usize),
    Size(usize),
    Undo,
    Clear,
    Ocr,
    Copy,
    Save,
    Share,
}

const TOOLS: [&str; 9] = ["Arrow", "Line", "Box", "Oval", "Mark", "Text", "Blur", "Step", "Pen"];
const SIZES: [f32; 3] = [0.7, 1.0, 1.4];
const STEP_TOOL: usize = 7;
const PEN_TOOL: usize = 8;
/// Crop arms like a tool — it takes over the preview and Esc puts it away —
/// but it reshapes the capture rather than drawing on it, so it lives with
/// padding and aspect instead of in the annotation grid. This index is one
/// past the palette on purpose: `state.tool` carries it so the whole
/// arm/disarm ladder works unchanged, while nothing iterating `TOOLS` ever
/// produces a chip for it.
const CROP_TOOL: usize = TOOLS.len();

/// A crop in raw-capture pixels. Always inside the capture and never empty.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Crop {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

/// The smallest crop worth having. Below this the preview has nothing to show
/// and the matte has nothing to frame.
const MIN_CROP: u32 = 16;

impl Crop {
    fn full(width: u32, height: u32) -> Self {
        Crop { x: 0, y: 0, w: width.max(1), h: height.max(1) }
    }

    fn is_full(&self, width: u32, height: u32) -> bool {
        self.x == 0 && self.y == 0 && self.w == width && self.h == height
    }

    /// Corners in raw-capture coordinates, in the `Grab::Corner` order the
    /// annotation handles already use: top-left, top-right, bottom-right,
    /// bottom-left.
    fn corners(&self) -> [(f32, f32); 4] {
        let (x0, y0) = (self.x as f32, self.y as f32);
        let (x1, y1) = ((self.x + self.w) as f32, (self.y + self.h) as f32);
        [(x0, y0), (x1, y0), (x1, y1), (x0, y1)]
    }
}

/// Build a crop from two dragged corners, clamped inside the capture and
/// widened to `MIN_CROP` rather than rejected — a slightly-too-small drag
/// should give you a small crop, not nothing.
fn crop_from_points(a: (f32, f32), b: (f32, f32), width: u32, height: u32) -> Crop {
    let clamp = |v: f32, hi: u32| v.round().clamp(0.0, hi as f32) as u32;
    let (x0, x1) = (clamp(a.0.min(b.0), width), clamp(a.0.max(b.0), width));
    let (y0, y1) = (clamp(a.1.min(b.1), height), clamp(a.1.max(b.1), height));
    let w = (x1 - x0).max(MIN_CROP.min(width));
    let h = (y1 - y0).max(MIN_CROP.min(height));
    Crop {
        x: x0.min(width.saturating_sub(w)),
        y: y0.min(height.saturating_sub(h)),
        w,
        h,
    }
}

/// Slide a crop by a raw-pixel delta without letting it leave the capture. The
/// size never changes, so dragging into an edge stops rather than shrinking.
fn move_crop(crop: Crop, dx: f32, dy: f32, width: u32, height: u32) -> Crop {
    let limit_x = width.saturating_sub(crop.w) as f32;
    let limit_y = height.saturating_sub(crop.h) as f32;
    Crop {
        x: (crop.x as f32 + dx).round().clamp(0.0, limit_x) as u32,
        y: (crop.y as f32 + dy).round().clamp(0.0, limit_y) as u32,
        ..crop
    }
}

/// Which corner of a rectangle `point` is, given the corner pinned opposite it.
/// In `Crop::corners` order: 0 top-left, 1 top-right, 2 bottom-right, 3
/// bottom-left.
fn corner_index(point: (f32, f32), opposite: (f32, f32)) -> u8 {
    match (point.0 > opposite.0, point.1 > opposite.1) {
        (false, false) => 0,
        (true, false) => 1,
        (true, true) => 2,
        (false, true) => 3,
    }
}

/// Drag one corner of a crop to `point`, keeping the opposite corner pinned.
///
/// Returns the new crop and which corner is now being held: dragging a corner
/// past its opposite flips it into the one it crossed to, and without that the
/// next mouse move would pin the wrong point and the rectangle would stick.
fn resize_crop(
    crop: Crop,
    corner: u8,
    point: (f32, f32),
    width: u32,
    height: u32,
) -> (Crop, u8) {
    let opposite = crop.corners()[((corner as usize) + 2) % 4];
    (
        crop_from_points(opposite, point, width, height),
        corner_index(point, opposite),
    )
}

/// What a drag on the crop rectangle is doing.
#[derive(Clone, Copy, PartialEq)]
enum CropDrag {
    /// Sweeping a brand new rectangle from the point it started at.
    New((f32, f32)),
    /// Sliding the whole rectangle; the point is where the cursor was last.
    Move((f32, f32)),
    /// Pulling one corner, in `Crop::corners` order.
    Corner(u8),
}

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
    /// Which recognition run these words came from. Re-arming Select Text
    /// issues a fresh generation, so a worker still chewing on the previous
    /// bitmap cannot hand its words to the new request.
    generation: u64,
    /// Replaces the hint line: progress, copy confirmation, or failure.
    message: Option<String>,
    dragging: bool,
}

/// What the OCR worker posts back: the recognition result plus enough identity
/// to route it. The words belong to one bitmap of one document, so a delayed
/// completion must never land on whichever tab happens to be active — that
/// exposed text from a different screenshot and ran its redaction checks
/// against the wrong coordinates.
struct OcrCompletion {
    /// `Document::id` of the capture that was recognized.
    doc: u64,
    /// `TextSelect::generation` of the request that started the worker.
    generation: u64,
    result: std::result::Result<Vec<crate::ocr::Word>, String>,
}

static OCR_COMPLETIONS: crate::completion::CompletionMailbox<OcrCompletion> =
    crate::completion::CompletionMailbox::new();

/// Hand a completion to the document and request it belongs to, or drop it.
/// Only an exact match on both identifiers is accepted: the document may have
/// been closed (id absent), the mode left (`text_select` gone), or Select Text
/// re-armed (newer generation) while the worker ran. Returns whether anything
/// changed and a repaint is due.
fn deliver_ocr<'a>(
    docs: impl IntoIterator<Item = (u64, &'a mut Option<TextSelect>)>,
    completion: OcrCompletion,
) -> bool {
    for (id, text_select) in docs {
        if id != completion.doc {
            continue;
        }
        let Some(select) = text_select.as_mut() else {
            return false;
        };
        if !select.pending || select.generation != completion.generation {
            return false;
        }
        select.pending = false;
        match completion.result {
            Ok(words) if words.is_empty() => {
                select.message = Some("no text found in this capture".into());
            }
            Ok(words) => select.words = words,
            Err(error) => {
                eprintln!("ocr failed: {error}");
                select.message = Some("could not read text from this capture".into());
            }
        }
        return true;
    }
    false
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
    /// Stable identity for routing async completions back to this capture.
    /// Tab indices shift as tabs close; this never does.
    id: u64,
    /// Tab label: the captured window's title, or the region's size.
    title: String,
    raw: RgbaImage,
    /// The kept part of `raw`, in raw-capture pixels. `None` is the whole
    /// capture. Cropping is not destructive: `raw` stays whole, so a crop can
    /// be reopened and nudged, cleared outright, or undone, and annotations
    /// that fall outside it are hidden rather than discarded.
    crop: Option<Crop>,
    /// `raw` reduced to `crop`. Rebuilt only when the crop changes, and absent
    /// while there is no crop so an uncropped capture never carries a second
    /// copy of itself.
    cropped: Option<RgbaImage>,
    /// The crop being adjusted while the Crop tool is armed. For its duration
    /// the preview shows the whole capture, which is the only way to pull an
    /// edge back out once it has been brought in.
    crop_edit: Option<Crop>,
    /// The crop changed, so the working bitmaps below no longer come from the
    /// right pixels. `ensure_preview_source` only grows on its own, and a crop
    /// usually shrinks.
    content_dirty: bool,
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
    /// Whether the current selector drag actually moved anything. A click that
    /// only selects must not leave an empty step on the undo stack.
    moved: bool,
    /// Clicked annotation — Delete / color / size act on it.
    selected: Option<usize>,
    /// Next step-badge number.
    counter_next: u32,
    /// Annotation under the cursor in selector mode (highlight only).
    hover_ann: Option<usize>,
    /// Select-text mode, when armed. None means normal annotation editing.
    text_select: Option<TextSelect>,
    history: History,
    last_rebuild: std::time::Instant,
}

/// One undo step. The badge counter travels with the annotations because
/// restoring a removed step badge has to restore the number it would hand out
/// next, or the following badge duplicates it.
#[derive(Clone)]
struct Snapshot {
    anns: Vec<crate::annotate::Annotation>,
    counter_next: u32,
    /// The crop travels with the annotations so Ctrl+Z steps back through
    /// framing and drawing in the one order they were done in. It is four
    /// integers, so carrying it on every step costs nothing.
    crop: Option<Crop>,
}

/// How many edits back you can go. Deep enough that nobody hits the wall in a
/// real session, shallow enough that the clones stay cheap.
const HISTORY_LIMIT: usize = 40;

/// Bounded undo stack. Each entry is the annotation state as it was
/// immediately before one edit, so an edit records its own "before" and undo
/// is a restore rather than an inverse operation.
#[derive(Default)]
struct History {
    steps: Vec<Snapshot>,
}

impl History {
    fn push(
        &mut self,
        anns: &[crate::annotate::Annotation],
        counter_next: u32,
        crop: Option<Crop>,
    ) {
        self.steps.push(Snapshot { anns: anns.to_vec(), counter_next, crop });
        if self.steps.len() > HISTORY_LIMIT {
            self.steps.remove(0);
        }
    }

    /// Drop the most recent step without applying it, for an edit that turned
    /// out not to be one (a stray click that drew nothing).
    fn discard(&mut self) {
        self.steps.pop();
    }

    fn undo(&mut self) -> Option<Snapshot> {
        self.steps.pop()
    }

    #[cfg(test)]
    fn depth(&self) -> usize {
        self.steps.len()
    }
}

impl Document {
    /// Record the current annotations as an undo point. Call before mutating.
    fn push_history(&mut self) {
        self.history.push(&self.anns, self.counter_next, self.crop);
    }

    fn discard_history(&mut self) {
        self.history.discard();
    }

    /// Step back one edit. False when there is nothing left to undo.
    fn undo(&mut self) -> bool {
        // A live drag holds an index into `anns`. Ctrl+Z is not blocked by
        // mouse capture the way the Undo chip is, so swapping the vector out
        // mid-drag would leave the next mouse move editing whichever
        // annotation happened to land at that index.
        if self.drawing || self.moving.is_some() {
            return false;
        }
        let Some(snapshot) = self.history.undo() else {
            return false;
        };
        self.anns = snapshot.anns;
        self.counter_next = snapshot.counter_next;
        self.set_crop(snapshot.crop);
        self.selected = None;
        self.hover_ann = None;
        self.editing = None;
        self.editing_original = None;
        true
    }

    /// The pixels every preview, measurement and export works from: the crop
    /// when there is one, the whole capture otherwise.
    fn content(&self) -> &RgbaImage {
        // While the crop is being adjusted the whole capture is on screen, so
        // an edge that was brought in can be pulled back out.
        if self.crop_edit.is_some() {
            return &self.raw;
        }
        self.cropped.as_ref().unwrap_or(&self.raw)
    }

    /// What the exported content will measure. Not always `content()`'s size:
    /// while the crop tool is armed the whole capture is on screen, but the
    /// size readout should be describing the frame the user is dragging.
    fn content_dimensions(&self) -> (u32, u32) {
        match self.crop_edit {
            Some(pending) => (pending.w, pending.h),
            None => self.content().dimensions(),
        }
    }

    /// Where `content()`'s top-left sits in raw-capture coordinates.
    /// Annotations are stored in raw coordinates and this is what puts them
    /// back in the right place once the picture around them has moved.
    fn content_origin(&self) -> (f32, f32) {
        match self.crop {
            Some(crop) if self.crop_edit.is_none() => (crop.x as f32, crop.y as f32),
            _ => (0.0, 0.0),
        }
    }

    /// Adopt a crop and rebuild the cropped pixels. A crop covering the whole
    /// capture is stored as `None`, so "cropped back to full" and "never
    /// cropped" cannot drift apart.
    fn set_crop(&mut self, crop: Option<Crop>) {
        let (width, height) = self.raw.dimensions();
        let crop = crop.filter(|crop| !crop.is_full(width, height));
        if crop == self.crop {
            return;
        }
        self.crop = crop;
        self.cropped = crop.map(|crop| {
            image::imageops::crop_imm(&self.raw, crop.x, crop.y, crop.w, crop.h).to_image()
        });
        self.content_dirty = true;
        self.base_cache = None;
    }
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
    /// Transient "copied" confirmation shown in the hint line after Copy.
    /// Copy keeps the tab open, so the confirmation replaces the old cue of
    /// the tab closing.
    copy_hint: Option<(String, std::time::Instant)>,
    /// In-flight Share request. A second click must not overwrite this
    /// or spawn another upload; `accept_completion` only drops a stale
    /// UI result (SBS-1075).
    pending_share: Option<u64>,
    /// Cached at open: Share is a paid-license action (SBS-906).
    can_share: bool,
    /// The crop drag in flight. Lives on the window rather than the document
    /// because, like the slider drags, it belongs to the mouse rather than to
    /// the picture.
    crop_drag: Option<CropDrag>,
    /// A button-up is still owed to a crop drag that a key already ended.
    crop_click_owed: bool,
    /// Caret blink phase while editing; one timer serves the window.
    caret_on: bool,
    font: HFONT,
    font_small: HFONT,
    scale: f32,
    width: i32,
    height: i32,
    /// Inside an interactive resize loop. Growing the working bitmap means
    /// resizing the capture, which is far too expensive to do on every WM_SIZE
    /// a drag emits; the preview stretches for the duration and sharpens when
    /// the mouse comes up.
    sizing: bool,
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

/// Picking a tool arms it until it is put away: picking the armed one again
/// disarms, picking another switches. Nothing else clears it except Esc, so a
/// run of boxes or arrows takes one trip to the palette.
fn tool_after_pick(current: Option<usize>, picked: usize) -> Option<usize> {
    (current != Some(picked)).then_some(picked)
}

/// Arm the crop tool: the whole capture comes back on screen with the current
/// crop drawn over it, so its edges can be pulled either way.
fn enter_crop(state: &mut State) {
    commit_editing(state);
    state.doc_mut().text_select = None;
    state.doc_mut().selected = None;
    state.doc_mut().hover_ann = None;
    state.doc_mut().moving = None;
    end_crop_drag(state);
    let (width, height) = state.doc().raw.dimensions();
    let pending = state.doc().crop.unwrap_or(Crop::full(width, height));
    state.doc_mut().crop_edit = Some(pending);
    invalidate_content(state);
}

/// End any crop drag in flight and give the mouse back.
///
/// The drag takes the capture on button-down, but cropping can be ended by a
/// key or another control while the button is still held — Esc, Enter, Ctrl+Z,
/// a tab switch, the Crop button itself. Those paths have to release it too,
/// or the mouse stays glued to the editor: the matching button-up finds
/// `crop_drag` already cleared and falls straight through to the control
/// handler without ever letting go.
fn end_crop_drag(state: &mut State) {
    if state.crop_drag.take().is_some() {
        state.crop_click_owed = true;
        unsafe {
            let _ = ReleaseCapture();
        }
    }
}

/// Consume a button-up that belongs to a crop drag, however that drag ended.
///
/// Ending a drag has two obligations, and each has its own failure: not
/// releasing the capture glues the mouse to the editor, and releasing without
/// remembering that an up is still owed lets that up fall through as a fresh
/// click on whatever sits under the cursor. True when the up was the drag's own
/// and must go no further.
fn take_crop_click(state: &mut State) -> bool {
    let dragging = state.crop_drag.is_some();
    if dragging {
        end_crop_drag(state);
    }
    std::mem::take(&mut state.crop_click_owed) || dragging
}

/// Arming or leaving the crop tool swaps `content()` between the cropped
/// pixels and the whole capture, which is what the working bitmaps and the
/// composite cache are built from. With no crop applied those are the same
/// image, and rebuilding them costs a full Lanczos resize of the capture for
/// nothing — so this only fires when there is actually a crop to swap.
fn invalidate_content(state: &mut State) {
    if state.doc().crop.is_none() {
        return;
    }
    state.doc_mut().content_dirty = true;
    state.doc_mut().base_cache = None;
}

/// Take the pending crop and put the tool away. Nothing is recorded when the
/// crop did not actually change, so arming the tool and thinking better of it
/// does not eat an undo step.
fn commit_crop(state: &mut State) {
    let Some(pending) = state.doc_mut().crop_edit.take() else {
        return;
    };
    end_crop_drag(state);
    let (width, height) = state.doc().raw.dimensions();
    let pending = (!pending.is_full(width, height)).then_some(pending);
    if pending != state.doc().crop {
        // Recorded against the pre-crop state, which `crop_edit` was masking
        // until the line above cleared it.
        state.doc_mut().push_history();
        // `set_crop` marks the content dirty itself when it changes anything.
        state.doc_mut().set_crop(pending);
    }
    invalidate_content(state);
}

/// Leave the crop tool without keeping the pending rectangle.
fn cancel_crop(state: &mut State) {
    if state.doc_mut().crop_edit.take().is_some() {
        end_crop_drag(state);
        invalidate_content(state);
    }
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
    if matches!(control, Ctl::Share) {
        return state.can_share;
    }
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

/// Longest edge the preview working bitmap should have for this viewport.
///
/// The editor stretches its working bitmap to fill the pane, so anything
/// smaller than the pane is upscaled and the text you are annotating against
/// goes soft — worse the larger the monitor, which is backwards. Matching the
/// pane means the picture is only ever downscaled.
///
/// Targeting the pane's own long edge is deliberately a little generous: the
/// composite carries the matte's padding on top of the content, so it is
/// always larger than the content and reaches the pane before the content
/// does. The slack is a margin against upscaling, not wasted work.
///
/// Bounded on both sides. There is nothing to gain above the capture's own
/// resolution, and `PREVIEW_CEILING` keeps an enormous monitor from turning
/// every rebuild into a visible pause.
fn preview_target_edge(preview_box: RECT, raw_w: u32, raw_h: u32) -> u32 {
    const PREVIEW_CEILING: u32 = 3200;
    const PREVIEW_FLOOR: u32 = 900;
    let pane = (preview_box.right - preview_box.left)
        .max(preview_box.bottom - preview_box.top)
        .max(0) as u32;
    pane.clamp(PREVIEW_FLOOR, PREVIEW_CEILING)
        .min(raw_w.max(raw_h))
}

/// `preview_sources` for `--preview-bench`: growing the working bitmap costs a
/// resize of the capture, and the benchmark has to measure the real one.
pub fn preview_sources_for_bench(
    raw: &RgbaImage,
    target_edge: u32,
) -> (RgbaImage, f32, RgbaImage, f32) {
    preview_sources(raw, target_edge)
}

/// Build the working bitmap and its drag-quality half at `target_edge`.
fn preview_sources(raw: &RgbaImage, target_edge: u32) -> (RgbaImage, f32, RgbaImage, f32) {
    let scale = (target_edge as f32 / raw.width().max(raw.height()) as f32).min(1.0);
    let small = if scale < 1.0 {
        image::imageops::resize(
            raw,
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
    (small, scale, small_fast, scale * 0.5)
}

/// Grow the working bitmap when the pane has outgrown it. Returns whether the
/// source changed, which means the composite cache is stale.
///
/// Only ever grows. Shrinking would reclaim memory that is already bounded,
/// and it would pay for a resize on the way back out of every window drag —
/// WM_SIZE arrives continuously, so this has to be cheap to call and quiet
/// when nothing is needed.
fn ensure_preview_source(state: &mut State) -> bool {
    let (content_w, content_h) = state.doc().content().dimensions();
    let target = preview_target_edge(state.preview_box, content_w, content_h);
    let current = state.doc().small.width().max(state.doc().small.height());
    // A few pixels either way is not worth a full resize of the capture. A
    // changed crop is not optional though: the working bitmaps come from the
    // wrong pixels until they are rebuilt, however big they happen to be.
    if !state.doc().content_dirty && target <= (current as f32 * 1.05) as u32 {
        return false;
    }
    let (small, metric, small_fast, metric_fast) = preview_sources(state.doc().content(), target);
    let doc = state.doc_mut();
    doc.content_dirty = false;
    doc.small = small;
    doc.preview_metric = metric;
    doc.small_fast = small_fast;
    doc.metric_fast = metric_fast;
    // Keyed by matte, padding, aspect and draft quality — not by resolution,
    // so a composite built against the old source would be reused at the wrong
    // size.
    doc.base_cache = None;
    true
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

/// Zoom a preview bitmap to fill its viewport. Draft previews contain fewer
/// pixels, but `quality_scale` expands them back to their full-preview logical
/// size so changing render quality never changes the geometry on screen. The
/// editor may upscale this working bitmap: exported pixels are unaffected, and
/// readable editing is more useful than leaving large monitors half empty.
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
    let fit = (bw as f32 / logical_w).min(bh as f32 / logical_h);
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
    let (dx, dy, draw_scale, _, _) = preview_draw_geometry(
        state.preview_box,
        state.doc().preview_w,
        state.doc().preview_h,
        preview_quality_scale(state),
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
///
/// Raw, not content: annotations and OCR words are stored against the original
/// capture so that changing the crop moves the picture under them instead of
/// invalidating them. The crop origin is what converts between the two.
fn to_raw(state: &State, x: i32, y: i32) -> Option<(f32, f32)> {
    let (rx, ry) = to_raw_unbounded(state, x, y);
    let (x0, y0, x1, y1) = content_bounds(state);
    if rx < x0 - 40.0 || ry < y0 - 40.0 || rx > x1 + 40.0 || ry > y1 + 40.0 {
        return None;
    }
    Some((rx.clamp(x0, x1), ry.clamp(y0, y1)))
}

/// The same mapping with no reject band, pinned to the capture's edges.
///
/// A crop drag holds the mouse, so sweeping well past the pane has to keep
/// tracking: `to_raw` would give up out there and the rectangle would freeze
/// at wherever it was when the cursor crossed the line, then jump when it came
/// back. Every crop helper clamps its inputs anyway, so the honest answer for a
/// point beyond the picture is the edge it is beyond.
fn to_raw_for_crop(state: &State, x: i32, y: i32) -> (f32, f32) {
    let (rx, ry) = to_raw_unbounded(state, x, y);
    let (x0, y0, x1, y1) = content_bounds(state);
    (rx.clamp(x0, x1), ry.clamp(y0, y1))
}

/// Window point to raw-capture coordinates, before any decision about whether
/// it landed on the picture.
fn to_raw_unbounded(state: &State, x: i32, y: i32) -> (f32, f32) {
    let (dx, dy, draw_scale, pad_x, pad_y) = view_params(state);
    let metric = preview_source(state).1;
    let (ox, oy) = state.doc().content_origin();
    (
        ((x - dx) as f32 / draw_scale - pad_x) / metric + ox,
        ((y - dy) as f32 / draw_scale - pad_y) / metric + oy,
    )
}

/// The content's extent in raw-capture coordinates: `(left, top, right, bottom)`.
fn content_bounds(state: &State) -> (f32, f32, f32, f32) {
    let (ox, oy) = state.doc().content_origin();
    let (cw, ch) = state.doc().content().dimensions();
    (ox, oy, ox + cw as f32, oy + ch as f32)
}

fn raw_to_screen(state: &State, p: (f32, f32)) -> (i32, i32) {
    let (dx, dy, draw_scale, pad_x, pad_y) = view_params(state);
    let (ox, oy) = state.doc().content_origin();
    let metric = preview_source(state).1;
    (
        dx + (((p.0 - ox) * metric + pad_x) * draw_scale) as i32,
        dy + (((p.1 - oy) * metric + pad_y) * draw_scale) as i32,
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
///
/// Any positive intersection counts, failing closed. Pixelating 40% of a
/// password hides those characters visually, and a majority threshold would
/// still copy the complete original word — the pixels say "redacted" while
/// the clipboard says otherwise.
fn redacted(anns: &[crate::annotate::Annotation], word: &crate::ocr::Word) -> bool {
    let (wx0, wy0, wx1, wy1) = word.rect;
    anns.iter().any(|ann| {
        let crate::annotate::Shape::Blur { a, b } = &ann.shape else {
            return false;
        };
        let overlap_x = wx1.min(a.0.max(b.0)) - wx0.max(a.0.min(b.0));
        let overlap_y = wy1.min(a.1.max(b.1)) - wy0.max(a.1.min(b.1));
        overlap_x > 0.0 && overlap_y > 0.0
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
    // A fresh generation per request: a worker still running for an earlier
    // arm of this mode — or for another tab — must not satisfy this one.
    static NEXT_OCR_GENERATION: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(1);
    let generation = NEXT_OCR_GENERATION.fetch_add(1, Ordering::Relaxed);
    let doc_id = state.doc().id;
    state.doc_mut().text_select = Some(TextSelect {
        words: Vec::new(),
        anchor: None,
        focus: None,
        pending: true,
        generation,
        message: None,
        dragging: false,
    });
    rebuild_preview(state);

    // Only the visible content is worth reading: text the crop removed is not
    // in the picture any more, and offering it for selection would be a lie.
    // The words come back in content coordinates and are shifted to raw ones —
    // the space the rest of the editor works in — here rather than on arrival,
    // because the crop that made this bitmap is the one that has to undo it and
    // an undo can change the document's crop while recognition is still
    // running.
    let raw = state.doc().content().clone();
    let (ox, oy) = state.doc().content_origin();
    let target = hwnd.0 as isize;
    let mailbox_generation = OCR_COMPLETIONS.generation_of(target);
    std::thread::spawn(move || {
        use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
        let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let completion = OcrCompletion {
            doc: doc_id,
            generation,
            result: crate::ocr::recognize_words(&raw)
                .map(|mut words| {
                    for word in &mut words {
                        word.rect.0 += ox;
                        word.rect.1 += oy;
                        word.rect.2 += ox;
                        word.rect.3 += oy;
                    }
                    words
                })
                .map_err(|error| format!("{error:#}")),
        };
        if com.is_ok() {
            unsafe { CoUninitialize() };
        }
        unsafe {
            let hwnd = HWND(target as *mut _);
            OCR_COMPLETIONS.post_with_at(target, mailbox_generation, completion, |token| {
                crate::window::has_class(hwnd, "matteshot_tweak")
                    && PostMessageW(hwnd, WM_OCR_READY, WPARAM(0), LPARAM(token as isize)).is_ok()
            });
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
    // Cheap and quiet unless the pane has outgrown the working bitmap. Here
    // rather than only on resize so every path that opens a capture — a new
    // tab, a tab switch, a second capture joining the window — gets a source
    // matched to the pane without having to remember to ask.
    ensure_preview_source(state);
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
    // They are in raw-capture coordinates, so a crop shifts them by its origin
    // and the renderer clips whatever now falls outside.
    let (ox, oy) = state.doc().content_origin();
    crate::annotate::render(
        &mut img,
        &state.doc_mut().anns,
        metric,
        (off_x - ox * metric, off_y - oy * metric),
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
    render_final(
        state.doc().content(),
        &state.doc().anns,
        state.doc().content_origin(),
        &state.doc().styles[state.doc().sel],
        (state.doc().pad_factor, ASPECTS[state.doc().aspect_idx].1),
        state.export_scale,
        state.doc().output_max_edge,
    )
}

/// Compose the finished image from content that has already been cropped.
///
/// Split out from `final_image` because this is where a crop has to line up.
/// Annotations are stored against the original capture, so `origin` — the
/// crop's top-left — is what brings them back over the content that is
/// actually being exported. Getting that offset wrong is invisible until
/// somebody crops and saves, which is exactly why it is reachable from a test.
fn render_final(
    content: &RgbaImage,
    anns: &[crate::annotate::Annotation],
    origin: (f32, f32),
    style: &Style,
    matte: (f32, Option<f32>),
    export_scale: u32,
    output_max_edge: u32,
) -> RgbaImage {
    let plain = compose::is_plain(style);
    let (pad_factor, aspect) = matte;
    let (ox, oy) = origin;
    if plain {
        // None: no matte, so no aspect-padded canvas. Same supersample
        // compose::export uses, then the output-size cap.
        let scale = compose::export_super_scale(content.width(), content.height(), export_scale);
        let mut scaled = compose::scale_plain(content, export_scale);
        crate::annotate::render(
            &mut scaled,
            anns,
            scale as f32,
            (-ox * scale as f32, -oy * scale as f32),
            None,
        );
        return output::resize_to_max_edge(&scaled, output_max_edge);
    }
    // SBS-1020: size the framed canvas *before* compose_base. Cap-then-compose
    // (or the 9.4 MP Original+aspect budget) so a 19k-px scroll + 16:9 cannot
    // allocate a 2.8 GB RGBA matte on the resident UI thread.
    let plan = compose::plan_framed_export(
        content.width(),
        content.height(),
        pad_factor,
        aspect,
        true,
        export_scale,
        output_max_edge,
    );
    let mut scaled = if (plan.content_w, plan.content_h) == content.dimensions() {
        content.clone()
    } else {
        image::imageops::resize(
            content,
            plan.content_w,
            plan.content_h,
            image::imageops::FilterType::Lanczos3,
        )
    };
    let ann_scale = plan.content_w as f32 / content.width().max(1) as f32;
    crate::annotate::render(
        &mut scaled,
        anns,
        ann_scale,
        (-ox * ann_scale, -oy * ann_scale),
        None,
    );
    let opts = ComposeOpts {
        metric_scale: plan.metric_scale,
        pad_factor,
        aspect,
    };
    let finished = compose::compose_with(&scaled, style, &opts);
    output::resize_to_max_edge(&finished, output_max_edge)
}

/// Exact matte dimensions before the optional output-size cap, without
/// rendering the full-size image. Includes the small-capture supersample so
/// custom-size bounds match Copy/Save.
fn composed_dimensions(state: &State) -> (u32, u32) {
    let (mut width, mut height) = state.doc().content_dimensions();
    let scale = compose::export_super_scale(width, height, state.export_scale);
    width *= scale;
    height *= scale;
    if !compose::is_plain(&state.doc().styles[state.doc().sel]) {
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
    let (cw, ch) = state.doc().content_dimensions();
    final_size(
        cw,
        ch,
        compose::is_plain(&state.doc().styles[state.doc().sel]),
        state.doc().pad_factor,
        ASPECTS[state.doc().aspect_idx].1,
        state.export_scale,
        state.doc().output_max_edge,
    )
}

fn final_size(
    content_w: u32,
    content_h: u32,
    plain: bool,
    pad_factor: f32,
    aspect: Option<f32>,
    export_scale: u32,
    max_edge: u32,
) -> (u32, u32) {
    if plain {
        let scale = compose::export_super_scale(content_w, content_h, export_scale);
        return output::resized_dimensions(content_w * scale, content_h * scale, max_edge);
    }
    // Same plan Copy/Save uses, so Original + a forced aspect reports the
    // 9.4 MP canvas rather than the native padded size we will not allocate.
    let plan = compose::plan_framed_export(
        content_w,
        content_h,
        pad_factor,
        aspect,
        true,
        export_scale,
        max_edge,
    );
    output::resized_dimensions(plan.canvas_w, plan.canvas_h, max_edge)
}

fn output_size_summary(max_edge: u32, dimensions: (u32, u32)) -> String {
    format!(
        "{}  \u{00b7}  {} \u{00d7} {} px",
        output::output_size_label(max_edge),
        dimensions.0,
        dimensions.1
    )
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

/// Prefill for the custom-size field. Original (`0`) is out of the typeable
/// range; use the long edge Copy/Save actually writes, not the native canvas
/// the 9.4 MP plan refused to allocate. Bounds follow `composed_dimensions`
/// (supersampled plain, native padded framed) up to `OUTPUT_CUSTOM_MAX`.
fn custom_size_initial(current: u32, native: (u32, u32), shown: (u32, u32)) -> u32 {
    let (minimum, maximum) = custom_size_bounds(native);
    if (minimum..=maximum).contains(&current) {
        current
    } else {
        shown.0.max(shown.1).clamp(minimum, maximum)
    }
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
    let (dx, dy, _, dw, dh) = preview_draw_geometry(
        state.preview_box,
        state.doc().preview_w,
        state.doc().preview_h,
        preview_quality_scale(state),
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

    // Crop overlay: everything the crop would discard is washed out, and the
    // kept rectangle carries the frame, corner handles and a size readout.
    // Screen-space, so dragging it never recomposes the preview.
    if let Some(pending) = state.doc().crop_edit {
        let (kx0, ky0) = raw_to_screen(state, (pending.x as f32, pending.y as f32));
        let (kx1, ky1) = raw_to_screen(
            state,
            ((pending.x + pending.w) as f32, (pending.y + pending.h) as f32),
        );
        let (fx0, fy0) = raw_to_screen(state, (0.0, 0.0));
        let (raw_w, raw_h) = state.doc().raw.dimensions();
        let (fx1, fy1) = raw_to_screen(state, (raw_w as f32, raw_h as f32));
        wash_all(
            hdc,
            state.theme.bg,
            &[
                (RECT { left: fx0, top: fy0, right: fx1, bottom: ky0 }, 170),
                (RECT { left: fx0, top: ky1, right: fx1, bottom: fy1 }, 170),
                (RECT { left: fx0, top: ky0, right: kx0, bottom: ky1 }, 170),
                (RECT { left: kx1, top: ky0, right: fx1, bottom: ky1 }, 170),
            ],
        );
        let pen = CreatePen(PS_SOLID, 1, state.theme.accent);
        let op = SelectObject(hdc, pen);
        let ob = SelectObject(
            hdc,
            windows::Win32::Graphics::Gdi::GetStockObject(
                windows::Win32::Graphics::Gdi::HOLLOW_BRUSH,
            ),
        );
        let _ = windows::Win32::Graphics::Gdi::Rectangle(hdc, kx0, ky0, kx1, ky1);
        SelectObject(hdc, ob);
        SelectObject(hdc, op);
        let _ = DeleteObject(pen);

        let fill = CreateSolidBrush(state.theme.accent);
        let edge = CreatePen(PS_SOLID, 1, state.theme.bg);
        let ob = SelectObject(hdc, fill);
        let op = SelectObject(hdc, edge);
        for corner in pending.corners() {
            let (sx, sy) = raw_to_screen(state, corner);
            let _ = windows::Win32::Graphics::Gdi::Rectangle(hdc, sx - 5, sy - 5, sx + 6, sy + 6);
        }
        SelectObject(hdc, ob);
        SelectObject(hdc, op);
        let _ = DeleteObject(fill);
        let _ = DeleteObject(edge);

        // The readout sits just inside the top-left corner, and flips below the
        // rectangle when the crop is pushed against the top of the pane.
        let readout_y = if ky0 - state.preview_box.top < (22.0 * state.scale) as i32 {
            ky0 + (6.0 * state.scale) as i32
        } else {
            ky0 - (20.0 * state.scale) as i32
        };
        label(
            hdc,
            state,
            kx0 + (4.0 * state.scale) as i32,
            readout_y,
            &format!("{} \u{00d7} {} px", pending.w, pending.h),
        );
    }

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
    let copy_feedback = state.copy_hint.as_ref().and_then(|(message, at)| {
        (at.elapsed() < std::time::Duration::from_secs(2)).then_some(message.as_str())
    });
    let hint: Option<&str> = if let Some(message) = copy_feedback {
        Some(message)
    } else if let Some(text) = &select_hint {
        Some(text.as_str())
    } else if state.custom_size_edit.is_some() {
        Some("type a size to preview it live   \u{00b7}   the other dimension adjusts automatically   \u{00b7}   Enter finishes   \u{00b7}   Esc restores")
    } else if state.doc().editing.is_some() {
        Some("type your caption   \u{00b7}   click anywhere to place   \u{00b7}   Esc cancel")
    } else if state.doc().crop_edit.is_some() {
        Some("drag to frame   \u{00b7}   pull a corner to resize   \u{00b7}   drag inside to move   \u{00b7}   Del uncrops   \u{00b7}   Enter applies   \u{00b7}   Esc cancels")
    } else if state.tool == Some(PEN_TOOL) {
        Some("drag on the preview to draw   \u{00b7}   Pen stays active   \u{00b7}   P or Esc exits")
    } else if state.tool == Some(STEP_TOOL) {
        Some("click the preview to place the next step   \u{00b7}   Step stays active   \u{00b7}   Esc exits")
    } else if state.tool == Some(5) {
        Some("click the preview to place a caption   \u{00b7}   Text stays active   \u{00b7}   Esc exits")
    } else if state.tool.is_some() {
        Some("drag on the preview to draw   \u{00b7}   tool stays active   \u{00b7}   click it again or Esc exits")
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
            &output_size_summary(state.doc().output_max_edge, (width, height)),
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
            Ctl::Share => chip(
                hdc,
                *r,
                if state.pending_share.is_some() {
                    "Sharing\u{2026}"
                } else {
                    "Share"
                },
                state,
                false,
                hot,
            ),
            Ctl::Crop => {
                // Says what it will do rather than what it is called: with a
                // crop already applied, the button is how you get back to the
                // whole capture.
                // Reads the frame on screen, not the committed one: pressing
                // Del while armed shows the whole capture again, and a chip
                // still saying "adjust" would describe something the user can
                // no longer see.
                let showing = match (state.doc().crop_edit, state.doc().crop) {
                    (Some(pending), _) => {
                        let (width, height) = state.doc().raw.dimensions();
                        (!pending.is_full(width, height)).then_some(pending)
                    }
                    (None, committed) => committed,
                };
                let label = if showing.is_some() { "Crop \u{00b7} adjust" } else { "Crop" };
                chip(hdc, *r, label, state, state.tool == Some(CROP_TOOL), hot)
            }
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

/// Save the edited result and put it on the clipboard, then either confirm and
/// keep the tab open (the default, so the capture can keep being refined) or
/// close it when the Settings toggle is off. Save remains the "I'm done"
/// action. Shared by the Copy chip and Ctrl+C so the keyboard can never do
/// something subtly different from the button.
unsafe fn copy_image(hwnd: HWND, state: &mut State) {
    commit_editing(state);
    let img = final_image(state);
    let cfg = Config::load();
    let style_name = state.doc().styles[state.doc().sel].name;
    match persist_and_copy_with(
        || output::save_png(&img, style_name, &cfg.save_dir(), Some(&state.doc().title)),
        |path| output::to_clipboard(&img, Some(path)),
    ) {
        Ok(_) => {
            let _ = Config::update(|cfg| cfg.last_style = state.doc_mut().sel);
            if cfg.keep_editor_open {
                state.copy_hint = Some(("copied".into(), std::time::Instant::now()));
                let _ = InvalidateRect(hwnd, None, false);
            } else {
                let active = state.active;
                close_tab(hwnd, state, active);
            }
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
            // Emptying an existing caption to delete it is a real edit and
            // stays undoable. A brand-new one that never held any text is not.
            let was_new = state.doc_mut().editing_original.is_none();
            state.doc_mut().anns.remove(i);
            state.doc_mut().editing_original = None;
            if was_new {
                state.doc_mut().discard_history();
            }
            return false;
        }
        // The Text tool stays armed after a successful add, so the next click
        // starts another caption. Callers that mean to leave annotation mode
        // clear `tool` themselves.
        state.doc_mut().editing_original = None;
        if state.tool.is_some() {
            // As with a finished shape: a selection that cannot be drawn would
            // still take the colour and size chips and the Delete key.
            state.doc_mut().selected = None;
        }
        return true;
    }
    false
}

fn begin_custom_size(state: &mut State) {
    commit_editing(state);
    state.tool = None;
    state.doc_mut().text_select = None;
    let native = composed_dimensions(state);
    let current = state.doc().output_max_edge;
    let initial = custom_size_initial(current, native, final_dimensions(state));
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

fn output_max_edge_after_custom_size_cancel(
    edit: Option<&CustomSizeEdit>,
    current: u32,
) -> u32 {
    edit.map_or(current, |edit| edit.original_max_edge)
}

fn cancel_custom_size(state: &mut State) {
    let restored = output_max_edge_after_custom_size_cancel(
        state.custom_size_edit.as_ref(),
        state.doc().output_max_edge,
    );
    state.custom_size_edit = None;
    state.doc_mut().output_max_edge = restored;
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

/// Which corner handle of the pending crop is under `p`, in raw-capture units.
fn crop_corner_at(crop: Crop, p: (f32, f32), tolerance: f32) -> Option<u8> {
    crop.corners()
        .into_iter()
        .enumerate()
        .find(|(_, corner)| {
            (p.0 - corner.0).abs() <= tolerance && (p.1 - corner.1).abs() <= tolerance
        })
        .map(|(index, _)| index as u8)
}

fn crop_contains(crop: Crop, p: (f32, f32)) -> bool {
    p.0 >= crop.x as f32
        && p.0 <= (crop.x + crop.w) as f32
        && p.1 >= crop.y as f32
        && p.1 <= (crop.y + crop.h) as f32
}

/// Decide what a press at `p` starts.
///
/// Corners win over the interior so the handles stay reachable on a crop that
/// fills the frame. A crop that covers everything sweeps a new rectangle
/// instead of moving: there is nowhere for it to move to, and "drag a box
/// round the part you want" is the whole gesture on a crop you have not
/// narrowed yet.
fn crop_drag_for(
    crop: Crop,
    p: (f32, f32),
    tolerance: f32,
    width: u32,
    height: u32,
) -> CropDrag {
    if let Some(corner) = crop_corner_at(crop, p, tolerance) {
        CropDrag::Corner(corner)
    } else if crop_contains(crop, p) && !crop.is_full(width, height) {
        CropDrag::Move(p)
    } else {
        CropDrag::New(p)
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
        cancel_custom_size(state);
    }
    // Reaching for an annotation tool leaves select-text mode. Matte, padding
    // and aspect are safe to keep it: word boxes live in capture coordinates,
    // so the overlay follows the new layout on its own.
    if matches!(ctl, Ctl::Tool(_) | Ctl::Crop | Ctl::Undo | Ctl::Clear) {
        state.doc_mut().text_select = None;
    }
    // Any control but the Crop button itself settles a pending crop first, so
    // Copy and Save produce the frame that is on screen rather than the whole
    // capture. Pressing Crop again is its own toggle, handled below.
    if state.doc().crop_edit.is_some() && ctl != Ctl::Crop {
        commit_crop(state);
        state.tool = None;
        rebuild_preview(state);
    }
    match ctl {
        // Arms and disarms like a tool, so it goes through the same ladder.
        // Pressing it again is the Apply; only Esc throws the frame away.
        Ctl::Crop => {
            commit_editing(state);
            match tool_after_pick(state.tool, CROP_TOOL) {
                Some(_) => {
                    state.tool = Some(CROP_TOOL);
                    state.doc_mut().selected = None;
                    enter_crop(state);
                }
                None => {
                    commit_crop(state);
                    state.tool = None;
                }
            }
            rebuild_preview(state);
            let _ = InvalidateRect(hwnd, None, false);
            return;
        }
        Ctl::Tool(n) => {
            commit_editing(state);
            state.tool = tool_after_pick(state.tool, n);
            if state.tool.is_some() {
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
                // Mid-caption, this belongs to that edit's single step — the
                // same way the caption sliders treat it.
                if state.doc().editing.is_none() {
                    state.doc_mut().push_history();
                }
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
                if state.doc().editing.is_none() {
                    state.doc_mut().push_history();
                }
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
            if state.doc_mut().undo() {
                rebuild_preview(state);
                let _ = InvalidateRect(hwnd, None, false);
            }
            return;
        }
        Ctl::Clear => {
            if !state.doc().anns.is_empty() {
                state.doc_mut().push_history();
            }
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
        Ctl::Copy => copy_image(hwnd, state),
        Ctl::Save => {
            let img = final_image(state);
            let cfg = Config::load();
            match output::save_png(
                &img,
                state.doc().styles[state.doc().sel].name,
                &cfg.save_dir(),
                Some(&state.doc().title),
            ) {
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
        Ctl::Share => share_current(hwnd, state),
        // Tool/Color/Undo/Clear handled above.
        _ => {}
    }
}

/// Saves the current tab (same as Save) then uploads it in the background and
/// copies the resulting link once it lands. Unlike Save, the tab stays open —
/// closer in spirit to Copy, since sharing is not "finished with this
/// capture" the way saving is.
unsafe fn share_current(hwnd: HWND, state: &mut State) {
    if let crate::share::ShareStart::Unavailable(reason) =
        crate::share::share_start(crate::license::can_share())
    {
        state.copy_hint = Some((reason.into(), std::time::Instant::now()));
        let _ = InvalidateRect(hwnd, None, false);
        return;
    }
    // Recdone already refuses a second Share with `if !state.sharing`.
    // A second click here used to save again and spawn another ≤300 MB
    // upload; `accept_completion` only dropped the stale UI result (SBS-1075).
    if !share_upload_idle(state.pending_share) {
        return;
    }
    commit_editing(state);
    let img = final_image(state);
    let cfg = Config::load();
    let style_name = state.doc().styles[state.doc().sel].name;
    match output::save_png(&img, style_name, &cfg.save_dir(), Some(&state.doc().title)) {
        Ok(path) => {
            if !start_share_upload(&mut state.pending_share, || {
                crate::share::share_in_background(hwnd, path)
            }) {
                return;
            }
            state.copy_hint = Some(("Sharing\u{2026}".into(), std::time::Instant::now()));
            let _ = InvalidateRect(hwnd, None, false);
        }
        Err(error) => show_output_error(
            hwnd,
            "The edited screenshot could not be saved. Your tab is still open.",
            &error,
            true,
        ),
    }
}

/// Tweak Share after the license check, before the save. Recdone already
/// applies this to `state.sharing` (SBS-1075).
fn share_upload_idle(pending: Option<u64>) -> bool {
    crate::share::share_idle(pending.is_some())
}

fn start_share_upload(pending: &mut Option<u64>, start: impl FnOnce() -> u64) -> bool {
    crate::share::begin_if_idle(pending, start)
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
                if let Some(drag) = state.crop_drag {
                    // Pinned to the capture's edges rather than abandoned once
                    // the cursor leaves the picture: the drag owns the mouse,
                    // so a sweep past the pane has to keep following it.
                    let p = to_raw_for_crop(state, x, y);
                    let (width, height) = state.doc().raw.dimensions();
                    let Some(pending) = state.doc().crop_edit else {
                        return LRESULT(0);
                    };
                    let (next, drag) = match drag {
                        CropDrag::New(start) => (
                            crop_from_points(start, p, width, height),
                            CropDrag::New(start),
                        ),
                        CropDrag::Move(last) => (
                            move_crop(pending, p.0 - last.0, p.1 - last.1, width, height),
                            CropDrag::Move(p),
                        ),
                        CropDrag::Corner(corner) => {
                            let (next, held) = resize_crop(pending, corner, p, width, height);
                            (next, CropDrag::Corner(held))
                        }
                    };
                    state.crop_drag = Some(drag);
                    if state.doc().crop_edit != Some(next) {
                        state.doc_mut().crop_edit = Some(next);
                        let _ = InvalidateRect(hwnd, None, false);
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
                        if p != last {
                            state.doc_mut().moved = true;
                        }
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
                                // Re-edit an existing caption. The click that
                                // opened it was a plain selection, so its undo
                                // step was already discarded on button-up;
                                // this edit needs one of its own.
                                state.doc_mut().push_history();
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
                // Settled before anything else, because several controls below
                // act on button-down and return: a drag ended by a key and
                // released outside the window leaves an up that never arrives,
                // and carrying that debt past here would let it swallow the
                // release of whatever this press starts.
                state.crop_click_owed = false;
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
                        cancel_custom_size(state);
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
                    // One undo step per drag, not per mouse move.
                    if state.doc().selected.is_some() && state.doc().editing.is_none() {
                        state.doc_mut().push_history();
                    }
                    state.dragging = Some(SliderDrag::CaptionSize);
                    SetCapture(hwnd);
                    caption_size_update(hwnd, state, x);
                    return LRESULT(0);
                }
                if text_context(state)
                    && state.caption_style == crate::annotate::TextStyle::Box
                    && in_rect(&state.caption_opacity_slider, x, y)
                {
                    if state.doc().selected.is_some() && state.doc().editing.is_none() {
                        state.doc_mut().push_history();
                    }
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
                            if state.doc().editing.is_none() {
                                state.doc_mut().push_history();
                            }
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
                // Cropping owns the preview while it is armed: no annotation is
                // created or selected until the frame is settled.
                if let Some(pending) = state.doc().crop_edit {
                    if in_rect(&state.preview_box, x, y) {
                        if let Some(p) = to_raw(state, x, y) {
                            let tolerance = (10.0 / state.doc().preview_metric).max(8.0);
                            let (width, height) = state.doc().raw.dimensions();
                            state.crop_drag =
                                Some(crop_drag_for(pending, p, tolerance, width, height));
                            SetCapture(hwnd);
                            let _ = InvalidateRect(hwnd, None, false);
                        }
                        return LRESULT(0);
                    }
                }
                if in_rect(&grab, x, y) {
                    commit_editing(state);
                    state.dragging = Some(SliderDrag::Padding);
                    // Padding changes the canvas geometry, so nothing caches
                    // between frames. Drop to drag quality for the duration.
                    state.doc_mut().fast_preview = true;
                    SetCapture(hwnd);
                    slider_update(hwnd, state, x);
                } else if let (Some(tool), Some(p)) =
                    // Crop is in the palette but draws nothing; without this it
                    // would fall through to the catch-all and place a caption.
                    (state.tool.filter(|tool| *tool != CROP_TOOL), to_raw(state, x, y))
                {
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
                        state.doc_mut().push_history();
                        state.doc_mut().anns.push(crate::annotate::Annotation {
                            shape,
                            color,
                            size,
                            text_style: crate::annotate::TextStyle::Shadow,
                            text_box_opacity: box_opacity,
                        });
                        state.doc_mut().drawing = true;
                        SetCapture(hwnd);
                    } else if tool == STEP_TOOL {
                        // Step badge: click places, auto-numbered. It stays
                        // armed so consecutive clicks drop 1, 2, 3, 4 without
                        // re-picking the tool.
                        state.doc_mut().push_history();
                        let n = state.doc_mut().counter_next;
                        state.doc_mut().counter_next += 1;
                        state.doc_mut().anns.push(crate::annotate::Annotation {
                            shape: Shape::Counter { pos: p, n },
                            color,
                            size,
                            text_style: crate::annotate::TextStyle::Shadow,
                            text_box_opacity: box_opacity,
                        });
                        // Armed tool, so no selection — see the drag release.
                        state.doc_mut().selected = None;
                        rebuild_preview(state);
                        let _ = InvalidateRect(hwnd, None, false);
                    } else {
                        // Text: click places, then type.
                        state.doc_mut().push_history();
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
                            // The whole move/reshape drag is one undo step, so
                            // the snapshot is taken here rather than per move.
                            state.doc_mut().push_history();
                            normalize_rect(&mut state.doc_mut().anns[i]);
                            let grab = grab_probe(&state.doc_mut().anns[i], p, tol);
                            state.doc_mut().selected = Some(i);
                            sync_annotation_controls(state, i);
                            state.doc_mut().moving = Some((i, p, grab));
                            state.doc_mut().moved = false;
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
                // The up belongs to the crop drag whether the drag ended here
                // or was already ended by a key; either way it goes no further.
                if take_crop_click(state) {
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
                    // A click that only selected something is not an edit.
                    if !state.doc_mut().moved {
                        state.doc_mut().discard_history();
                    }
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
                        state.doc_mut().discard_history();
                    } else {
                        // The tool stays armed after the stroke: drawing four
                        // boxes should not mean picking Box four times. Only
                        // Esc, the tool chip again, or another tool disarms.
                        //
                        // Nothing is left selected, because the selection
                        // overlay is hidden while a tool is armed. A shape
                        // selected with no handles on screen still answers the
                        // colour and size chips and the Delete key, so the
                        // palette would silently retarget the previous shape
                        // instead of setting up the next one.
                        state.doc_mut().selected = None;
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
                // Right-click reaches for an annotation's properties, which
                // means putting the tool away. While a crop is pending that
                // would strand the frame with nothing left to apply it, so the
                // crop keeps the preview to itself.
                if state.doc().crop_edit.is_some() {
                    return LRESULT(0);
                }
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
                // Cropping owns Esc, Enter, Delete and Ctrl+Z while it is
                // armed. Esc throws the pending frame away rather than peeling
                // one layer off the editor, because that rectangle is the only
                // thing the user is looking at. This sits above Ctrl+C so a
                // keyboard copy settles the frame first, exactly as the Copy
                // chip does.
                if state.doc().crop_edit.is_some() {
                    match wparam.0 as u16 {
                        v if v == VK_ESCAPE.0 || (ctrl_down && v == b'Z' as u16) => {
                            cancel_crop(state);
                            state.tool = None;
                            rebuild_preview(state);
                            let _ = InvalidateRect(hwnd, None, false);
                            return LRESULT(0);
                        }
                        v if v == VK_RETURN.0 => {
                            commit_crop(state);
                            state.tool = None;
                            rebuild_preview(state);
                            let _ = InvalidateRect(hwnd, None, false);
                            return LRESULT(0);
                        }
                        // Delete / Backspace: back to the whole capture, ready
                        // to apply as "no crop" or to re-frame from scratch.
                        0x2E | 0x08 => {
                            // Ends the drag too: leaving it live would hold
                            // the capture and let the next mouse move resize
                            // the frame that was just reset.
                            end_crop_drag(state);
                            let (width, height) = state.doc().raw.dimensions();
                            state.doc_mut().crop_edit = Some(Crop::full(width, height));
                            let _ = InvalidateRect(hwnd, None, false);
                            return LRESULT(0);
                        }
                        v if ctrl_down && v == b'C' as u16 => {
                            commit_crop(state);
                            state.tool = None;
                            rebuild_preview(state);
                        }
                        _ => {}
                    }
                }
                // Ctrl+C copies the finished image, exactly as the Copy chip
                // does. Not while typing a caption, where it means the text.
                if ctrl_down && wparam.0 as u16 == b'C' as u16 && state.doc().editing.is_none() {
                    copy_image(hwnd, state);
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
                            let reedit = state.doc_mut().editing_original.take().is_some();
                            // Cancel puts the caption back the way it was —
                            // not just its text. Colour, size and the caption
                            // style controls all retarget the annotation while
                            // it is being typed, so the whole pre-edit state
                            // has to come back. The step pushed when the edit
                            // began holds exactly that.
                            state.doc_mut().undo();
                            if reedit && i < state.doc_mut().anns.len() {
                                state.doc_mut().selected = Some(i);
                            }
                            // The Text tool stays armed: abandoning one caption
                            // is not a reason to leave caption mode. The next
                            // Esc puts the tool away.
                            rebuild_preview(state);
                            let _ = InvalidateRect(hwnd, None, false);
                        } else if state.tool.take().is_some() {
                            // Esc peels one layer at a time — caption, then the
                            // armed tool, then the selection, and only then the
                            // tab. Tools stay armed across uses, so without this
                            // ladder a single Esc would throw away the capture.
                            let _ = InvalidateRect(hwnd, None, false);
                        } else if state.doc_mut().selected.take().is_some() {
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
                    // Ctrl+Z. A slider drag lives on State rather than the
                    // document, so it is guarded here instead of in undo().
                    0x5A if ctrl_down && state.dragging.is_none() => {
                        commit_editing(state);
                        if state.doc_mut().undo() {
                            rebuild_preview(state);
                            let _ = InvalidateRect(hwnd, None, false);
                        }
                    }
                    // Delete removes the selected annotation.
                    0x2E if state.doc_mut().editing.is_none() => {
                        if let Some(i) = state.doc_mut().selected.take() {
                            if i < state.doc_mut().anns.len() {
                                state.doc_mut().push_history();
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
        crate::share::WM_SHARE_COMPLETE => {
            let Some(completion) = crate::share::take_completion(lparam.0 as u64, hwnd.0 as isize)
            else {
                return LRESULT(0);
            };
            let is_current = state_of(hwnd).is_some_and(|state| {
                crate::share::accept_completion(&mut state.pending_share, completion.request_id)
            });
            if !is_current {
                return LRESULT(0);
            }
            if let Some(state) = state_of(hwnd) {
                state.copy_hint = match completion.outcome {
                    Ok(url) => {
                        // Opening the page is the visible confirmation that
                        // something happened; the clipboard copy alone was
                        // easy to miss entirely.
                        crate::output::open_url(&url);
                        match crate::output::text_to_clipboard(&url) {
                            Ok(()) => Some(("Link copied".into(), std::time::Instant::now())),
                            Err(error) => {
                                show_output_error(
                                    hwnd,
                                    "The share link could not be copied.",
                                    &error,
                                    false,
                                );
                                None
                            }
                        }
                    }
                    Err(message) => {
                        show_output_error(
                            hwnd,
                            "This capture could not be shared.",
                            &anyhow::anyhow!(message),
                            false,
                        );
                        None
                    }
                };
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_OCR_READY => {
            let Some(payload) = OCR_COMPLETIONS.take(lparam.0 as u64, hwnd.0 as isize) else {
                return LRESULT(0);
            };
            if let Some(state) = state_of(hwnd) {
                // Routed by document id and request generation, not to whichever
                // tab is active: start Select Text on A, switch to B and start
                // it there, and A's slower result must land on A — not populate
                // B with another screenshot's words. Words are already in
                // raw-capture coordinates: the worker applied the crop origin
                // it was launched with, which is the one that produced these
                // boxes.
                let docs = state
                    .docs
                    .iter_mut()
                    .map(|doc| (doc.id, &mut doc.text_select));
                if deliver_ocr(docs, payload) {
                    let _ = InvalidateRect(hwnd, None, false);
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
                    // A bigger pane needs a bigger working bitmap, or the extra
                    // room is filled by stretching what is already there. A
                    // no-op unless the pane actually grew, and deferred to the
                    // end of a drag-resize so the window still follows the
                    // mouse. Maximize and restore arrive as a single WM_SIZE
                    // outside that loop, so they sharpen immediately.
                    if !state.sizing && ensure_preview_source(state) {
                        rebuild_preview(state);
                    }
                    let _ = InvalidateRect(hwnd, None, false);
                }
            }
            LRESULT(0)
        }
        windows::Win32::UI::WindowsAndMessaging::WM_ENTERSIZEMOVE => {
            if let Some(state) = state_of(hwnd) {
                state.sizing = true;
            }
            LRESULT(0)
        }
        windows::Win32::UI::WindowsAndMessaging::WM_EXITSIZEMOVE => {
            if let Some(state) = state_of(hwnd) {
                state.sizing = false;
                if ensure_preview_source(state) {
                    rebuild_preview(state);
                    let _ = InvalidateRect(hwnd, None, false);
                }
            }
            LRESULT(0)
        }
        windows::Win32::UI::WindowsAndMessaging::WM_GETMINMAXINFO => {
            let mmi = lparam.0 as *mut windows::Win32::UI::WindowsAndMessaging::MINMAXINFO;
            if !mmi.is_null() {
                let s = crate::dpi::scale_for_window(hwnd);
                (*mmi).ptMinTrackSize.x = (760.0 * s) as i32;
                (*mmi).ptMinTrackSize.y = (620.0 * s) as i32;
            }
            LRESULT(0)
        }
        // Alt+Tab or a system dialog can take the capture away mid-drag.
        // Without this the pending rectangle keeps following a cursor with no
        // button held, and the up that would have ended it never arrives.
        windows::Win32::UI::WindowsAndMessaging::WM_CAPTURECHANGED => {
            if let Some(state) = state_of(hwnd) {
                if state.crop_drag.take().is_some() {
                    state.crop_click_owed = false;
                    let _ = InvalidateRect(hwnd, None, false);
                }
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
            crate::share::discard_window(hwnd.0 as isize);
            OCR_COMPLETIONS.unbind(hwnd.0 as isize);
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
/// The editor's client size for a monitor's work area.
///
/// It takes a generous share of a large screen and nearly all of a small one.
/// The control column is a fixed stack of groups that cannot shrink past its
/// labels, so on a short display the difference between 85% and this is the
/// difference between the column fitting and running into Copy/Save/Share.
fn editor_client_size(work_w: i32, work_h: i32, scale: f32) -> (i32, i32) {
    let sc = |v: i32| (v as f32 * scale) as i32;
    let share = if work_h < sc(1000) { 0.94 } else { 0.85 };
    (
        ((work_w as f32 * 0.85) as i32).clamp(sc(760).min(work_w - sc(40)), work_w - sc(40)),
        ((work_h as f32 * share) as i32).clamp(sc(560).min(work_h - sc(40)), work_h - sc(40)),
    )
}

/// The bottom of the control column: the last palette row, below which only
/// the bottom-anchored action block lives.
fn column_bottom(controls: &[(RECT, Ctl)]) -> i32 {
    controls
        .iter()
        .filter(|(_, control)| matches!(control, Ctl::Size(_) | Ctl::Color(_) | Ctl::Clear))
        .map(|(rect, _)| rect.bottom)
        .max()
        .unwrap_or(0)
}

/// Clearance the output-size block needs above itself: it is captioned by two
/// lines of text — the section name and the live result — drawn one line pitch
/// apart above its first control.
fn action_headroom(scale: f32) -> i32 {
    2 * (20.0 * scale) as i32
}

/// Whether a laid-out column clears the bottom-anchored action block, captions
/// and all. Measured from the layout itself rather than predicted, so the two
/// can never be computed from different assumptions.
fn column_clears_actions(layout: &WindowLayout, scale: f32) -> bool {
    let action_top = layout
        .controls
        .iter()
        .filter(|(_, control)| matches!(control, Ctl::OutputSize(_) | Ctl::CustomSize))
        .map(|(rect, _)| rect.top)
        .min()
        .unwrap_or(i32::MAX);
    column_bottom(&layout.controls) + action_headroom(scale) <= action_top
}

/// Lay the editor out, giving up spacing — and then column width — only as far
/// as a short window forces it to.
///
/// The control column is a fixed stack of groups growing down from the tab
/// strip while Copy/Save/Share stay pinned to the bottom, so on a small screen
/// the two ran through each other. Two things are traded, in order of how
/// little they cost:
///
/// 1. vertical rhythm, down to what the section labels need;
/// 2. the matte grid's width — seven chips two-abreast is four rows with a
///    ragged last one, and three-abreast is three rows with the same ragged
///    row, so this buys a whole row back for nothing but narrower chips.
///
/// The first arrangement that fits wins, so anything roomy keeps wide chips at
/// natural spacing and only a genuinely small screen ever sees the rest.
fn layout_controls(scale: f32, cw: i32, ch: i32, n_styles: usize) -> WindowLayout {
    for matte_columns in [2, 3] {
        for step in 0..=10 {
            let layout =
                layout_column(scale, cw, ch, n_styles, matte_columns, step as f32 / 10.0);
            if column_clears_actions(&layout, scale) {
                return layout;
            }
        }
    }
    // Smaller than the controls can ever be. The tightest arrangement at least
    // keeps the overlap to the captions rather than the buttons.
    layout_column(scale, cw, ch, n_styles, 3, 1.0)
}

fn layout_column(
    scale: f32,
    cw: i32,
    ch: i32,
    n_styles: usize,
    matte_columns: i32,
    tighten: f32,
) -> WindowLayout {
    let sc = |v: i32| (v as f32 * scale) as i32;
    // Vertical rhythm only: chip heights stay put so nothing becomes harder to
    // read or to hit, and the horizontal grid never moves.
    let tighten = tighten.clamp(0.0, 1.0);
    let vs = |natural: i32, compact: i32| {
        sc((natural as f32 + (compact - natural) as f32 * tighten) as i32)
    };
    // Compact values are floored by what the section labels need, not by how
    // small the numbers can go: every group is headed by a line of text drawn
    // `LABEL` above its first control, and squeezing past that puts the label
    // through the row above it.
    const LABEL: i32 = 20;
    const CHIP: i32 = 30;
    // The chips themselves shrink a little before the gaps give out, which is
    // what keeps ten rows of them inside a short window.
    let chip_h = vs(CHIP, 28);
    let row = vs(38, 30);
    let head = vs(22, LABEL);
    // Clearance to the next label is the gap plus whatever the row pitch
    // leaves under the chip, so these floors are `LABEL` less that slack.
    let gap_matte = vs(30, LABEL - 2);
    let gap_slider = vs(52, 22 + LABEL);
    let gap_tools = vs(26, LABEL - 2);
    let gap_colors = vs(8, 4);
    let row_small = vs(34, 28);
    let m = sc(20);
    // The window's own bottom margin is part of the budget: giving some of it
    // back moves the action block down, which is worth as much to a short
    // window as tightening the column above it.
    let margin_v = vs(20, 12);
    let col_x = cw - sc(250);
    // The tab strip is always present, even with one capture open, so adding
    // a second one never reflows everything underneath it.
    let tab_strip = RECT { left: 0, top: 0, right: cw, bottom: sc(34) };
    let top = tab_strip.bottom + sc(10);
    // Bottom strip reserved for the contextual hint line.
    let preview_box = RECT { left: m, top, right: col_x - sc(16), bottom: ch - margin_v - sc(18) };
    let mut controls = Vec::new();
    let mut y = top + head;
    // Matte chips fill the column at whatever width they are given, and a
    // final row with fewer chips than the rest is centred rather than left
    // hanging on the left edge.
    let columns = matte_columns.max(1);
    let (matte_w, matte_pitch) = if columns >= 3 { (sc(66), sc(74)) } else { (sc(104), sc(112)) };
    let matte_rows = (n_styles as i32 + columns - 1) / columns;
    let last_row_count = match n_styles as i32 % columns {
        0 => columns,
        remainder => remainder,
    };
    for i in 0..n_styles {
        let line = i as i32 / columns;
        let colm = i as i32 % columns;
        let count = if line == matte_rows - 1 { last_row_count } else { columns };
        let indent = (sc(216) - ((count - 1) * matte_pitch + matte_w)) / 2;
        let x = col_x + if count == columns { 0 } else { indent } + colm * matte_pitch;
        controls.push((
            RECT {
                left: x,
                top: y + line * row,
                right: x + matte_w,
                bottom: y + line * row + chip_h,
            },
            Ctl::Matte(i),
        ));
    }
    y += matte_rows * row + gap_matte;
    let slider_rect = RECT { left: col_x, top: y, right: col_x + sc(216), bottom: y + sc(22) };
    controls.push((slider_rect, Ctl::Slider));
    y += gap_slider;
    for i in 0..ASPECTS.len() {
        let line = i as i32 / 3;
        let colm = i as i32 % 3;
        controls.push((
            RECT {
                left: col_x + colm * sc(74),
                top: y + line * row,
                right: col_x + colm * sc(74) + sc(66),
                bottom: y + line * row + chip_h,
            },
            Ctl::Aspect(i),
        ));
    }
    // Crop closes the "shape of the picture" group under the aspect presets,
    // rather than sitting in the annotation grid where it would be a tenth
    // chip stranded on a row of its own. Full column width so it reads as the
    // mode it is, not as one more thing to draw with.
    // A little air above it so it reads as its own control rather than a third
    // row of aspect presets. This is the first thing to go when the window is
    // short, because a slightly tighter group beats a group that does not fit.
    let crop_y = y + row * 2 + vs(10, 0);
    controls.push((
        RECT { left: col_x, top: crop_y, right: col_x + sc(216), bottom: crop_y + chip_h },
        Ctl::Crop,
    ));
    // Annotation tools.
    let tools_y = crop_y + row + gap_tools;
    for i in 0..TOOLS.len() {
        let line = i as i32 / 3;
        let colm = i as i32 % 3;
        controls.push((
            RECT {
                left: col_x + colm * sc(74),
                top: tools_y + line * row,
                right: col_x + colm * sc(74) + sc(66),
                bottom: tools_y + line * row + chip_h,
            },
            Ctl::Tool(i),
        ));
    }
    let tool_rows = (TOOLS.len() as i32 + 2) / 3;
    let colors_y = tools_y + row * tool_rows + gap_colors;
    for i in 0..crate::annotate::COLORS.len() {
        let x = col_x + i as i32 * sc(34);
        controls.push((
            RECT { left: x, top: colors_y, right: x + sc(26), bottom: colors_y + sc(26) },
            Ctl::Color(i),
        ));
    }
    let uc_y = colors_y + row_small;
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
    let by = ch - margin_v - sc(30);
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
        RECT { left: col_x + sc(144), top: by, right: col_x + sc(208), bottom: by + sc(30) },
        Ctl::Share,
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
    // Provisional preview source. The pane's real size is not known until the
    // window has been created and sized, and `ensure_preview_source` grows
    // this to match before the first paint. Opening at a modest size keeps
    // that first frame cheap on a large capture.
    const PREVIEW_INITIAL: u32 = 1200;
    let (small, scale, small_fast, metric_fast) = preview_sources(&raw, PREVIEW_INITIAL);
    static NEXT_DOC_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    Document {
        id: NEXT_DOC_ID.fetch_add(1, Ordering::Relaxed),
        title,
        raw,
        crop: None,
        cropped: None,
        crop_edit: None,
        content_dirty: false,
        small,
        preview_metric: scale,
        small_fast,
        metric_fast,
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
        moved: false,
        selected: None,
        counter_next: 1,
        hover_ann: None,
        text_select: None,
        history: History::default(),
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
    cancel_custom_size(state);
    // The pending crop belongs to the tab being left, and the tool is put away
    // by the switch, so settle it here or that tab keeps a frame nothing can
    // ever apply.
    commit_crop(state);
    release_inactive(state);
    state.active = index;
    state.tool = None;
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
    cancel_custom_size(state);
    // Closing another tab still puts the tool away, so a crop pending on the
    // one that survives has to be settled here or it is left armed with no
    // chip lit and no way to apply it. A crop on the tab being closed goes
    // with it.
    if index != state.active {
        commit_crop(state);
    }
    state.docs.remove(index);
    if state.docs.is_empty() {
        let _ = DestroyWindow(hwnd);
        return;
    }
    state.active = active_after_close(state.active, index, state.docs.len());
    state.tool = None;
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
                cancel_custom_size(state);
                // Same reason `activate_tab` settles it: the pending crop
                // belongs to the tab being left and the tool is put away by the
                // switch, so without this that tab keeps a frame nothing can
                // ever apply.
                commit_crop(state);
                release_inactive(state);
                state.docs.push(document);
                state.active = state.docs.len() - 1;
                state.tool = None;
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
    // The editor sizes itself to this monitor, so it is this monitor's scale
    // that its chrome is measured in, not the primary one's.
    let dpi_scale = crate::dpi::scale_for_monitor(monitor);
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
    let (cw, ch) = editor_client_size(work_w, work_h, dpi_scale);

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
        copy_hint: None,
        pending_share: None,
        can_share: crate::license::can_share(),
        crop_drag: None,
        crop_click_owed: false,
        font: unsafe { make_font(-sc(14), 400) },
        font_small: unsafe { make_font(-sc(12), 400) },
        scale: dpi_scale,
        width: cw,
        height: ch,
        sizing: false,
        theme: crate::theme::current(),
    });
    // `preview_box` is already known here, so this first rebuild composes at
    // the pane's size rather than opening soft and sharpening a frame later.
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
        let outer = crate::dpi::outer_bounds(
            RECT { left: 0, top: 0, right: cw, bottom: ch },
            style,
            windows::Win32::UI::WindowsAndMessaging::WS_EX_APPWINDOW,
            dpi_scale,
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
        apply_text_input, custom_size_axis, custom_size_bounds, custom_size_initial,
        custom_size_result, custom_size_value, final_size, freehand_length, join_words,
        layout_controls, nearest_word, persist_and_copy_with, share_upload_idle,
        start_share_upload, output_max_edge_after_custom_size_cancel, output_size_summary,
        preview_draw_geometry,
        corner_index, crop_contains, crop_drag_for, crop_from_points, deliver_ocr, move_crop,
        preview_sources, preview_target_edge, redacted, render_final, resize_crop, tab_for_digit,
        tool_after_pick, Crop, CropDrag, History, OcrCompletion, TextSelect, HISTORY_LIMIT,
        translate_ann, Ctl, CustomInput, CustomSizeEdit, FinishError, TextInput, ASPECTS,
        CROP_TOOL, MIN_CROP, TOOLS,
    };
    use image::{Rgba, RgbaImage};
    use windows::Win32::Foundation::RECT;

    #[test]
    fn preview_fills_the_workspace_and_padding_drag_quality_does_not_shrink_it() {
        for preview_box in [
            RECT { left: 0, top: 0, right: 1200, bottom: 800 },
            RECT { left: 50, top: 25, right: 750, bottom: 525 },
        ] {
            let full = preview_draw_geometry(preview_box, 960, 600, 1.0);
            let draft = preview_draw_geometry(preview_box, 480, 300, 2.0);

            assert_eq!((draft.0, draft.1, draft.3, draft.4), (full.0, full.1, full.3, full.4));
            assert!((draft.2 - full.2 * 2.0).abs() < f32::EPSILON);

            let available_width = preview_box.right - preview_box.left;
            let available_height = preview_box.bottom - preview_box.top;
            assert!(full.3 == available_width || full.4 == available_height);
        }
    }

    /// SBS-1075: a second Share while one is pending must not save-and-upload
    /// again. Recdone already refuses with `if !state.sharing`.
    #[test]
    fn tweak_share_refuses_a_second_in_flight_request() {
        let mut pending_share = None;
        let mut saved = 0u32;
        let mut uploads = 0u64;
        let click = |pending: &mut Option<u64>| {
            if !share_upload_idle(*pending) {
                return false;
            }
            saved += 1;
            start_share_upload(pending, || {
                uploads += 1;
                uploads
            })
        };
        assert!(click(&mut pending_share));
        assert!(!click(&mut pending_share));
        assert_eq!(saved, 1, "a second tweak Share saved another PNG");
        assert_eq!(uploads, 1, "a second tweak Share started another upload");
        assert_eq!(pending_share, Some(1));
        assert!(!crate::share::accept_completion(&mut pending_share, 2));
        assert_eq!(pending_share, Some(1));
        assert!(crate::share::accept_completion(&mut pending_share, 1));
        assert!(click(&mut pending_share));
        assert_eq!(saved, 2);
        assert_eq!(uploads, 2);
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

    #[test]
    fn undo_restores_the_state_from_before_each_edit() {
        let mut history = History::default();
        let one = vec![blur((0.0, 0.0), (10.0, 10.0))];
        let two = vec![blur((0.0, 0.0), (10.0, 10.0)), blur((5.0, 5.0), (20.0, 20.0))];

        history.push(&[], 1, None);
        history.push(&one, 1, None);
        history.push(&two, 3, None);
        assert_eq!(history.depth(), 3);

        // Each undo hands back the state recorded before that edit, newest
        // first, and the badge counter travels with it.
        let step = history.undo().unwrap();
        assert_eq!(step.anns.len(), 2);
        assert_eq!(step.counter_next, 3);
        assert_eq!(history.undo().unwrap().anns.len(), 1);
        assert_eq!(history.undo().unwrap().anns.len(), 0);
        assert!(history.undo().is_none());
    }

    #[test]
    fn a_discarded_step_is_not_undoable() {
        let mut history = History::default();
        history.push(&[], 1, None);
        // A click that drew nothing pushes, then takes it back.
        history.push(&[blur((0.0, 0.0), (1.0, 1.0))], 1, None);
        history.discard();
        assert_eq!(history.depth(), 1);
        assert_eq!(history.undo().unwrap().anns.len(), 0);
        assert!(history.undo().is_none());
        // Discarding an empty stack is a no-op, not a panic.
        history.discard();
    }

    #[test]
    fn history_forgets_the_oldest_steps_past_the_limit() {
        let mut history = History::default();
        for n in 0..HISTORY_LIMIT + 10 {
            history.push(&vec![blur((0.0, 0.0), (1.0, 1.0)); n], 1, None);
        }
        assert_eq!(history.depth(), HISTORY_LIMIT);
        // The newest step survives; the oldest reachable one is the tenth.
        assert_eq!(history.undo().unwrap().anns.len(), HISTORY_LIMIT + 9);
        let mut last = 0;
        while let Some(step) = history.undo() {
            last = step.anns.len();
        }
        assert_eq!(last, 10);
    }

    /// Screen size to editor client size, through the same rule the window
    /// creation path uses. Work area is the screen less the taskbar.
    fn client_for_screen(width: i32, height: i32, scale: f32) -> (i32, i32) {
        let sc = |v: i32| (v as f32 * scale) as i32;
        super::editor_client_size(width, height - sc(48), scale)
    }

    #[test]
    fn a_roomy_window_is_laid_out_at_natural_spacing() {
        // Compression is a last resort, not the normal case: a 1440p editor
        // must be pixel-identical to the uncompressed layout, or every screen
        // would quietly get the tight one.
        let (cw, ch) = client_for_screen(2560, 1440, 1.0);
        let chosen = layout_controls(1.0, cw, ch, 7);
        let natural = super::layout_column(1.0, cw, ch, 7, 2, 0.0);
        assert!(
            chosen
                .controls
                .iter()
                .zip(&natural.controls)
                .all(|((a, _), (b, _))| a == b),
            "a roomy window was given the compressed layout"
        );
    }

    #[test]
    fn below_the_supported_floor_controls_still_never_overlap() {
        // Past 1366x768 the captions above the output block lose their room,
        // which is cosmetic. Buttons landing on top of each other would not
        // be, so that is the line that has to hold all the way down.
        for (screen_w, screen_h, scale) in
            [(1280, 720, 1.0), (1920, 1080, 1.5), (1024, 768, 1.0)]
        {
            let (cw, ch) = client_for_screen(screen_w, screen_h, scale);
            let layout = layout_controls(scale, cw, ch, 7);
            let action_top = layout
                .controls
                .iter()
                .filter(|(_, control)| {
                    matches!(control, Ctl::OutputSize(_) | Ctl::CustomSize)
                })
                .map(|(rect, _)| rect.top)
                .min()
                .expect("the output-size row exists");
            assert!(
                super::column_bottom(&layout.controls) <= action_top,
                "{screen_w}x{screen_h} @{scale}: the palette overlaps the output block"
            );
        }
    }

    #[test]
    fn ordinary_screens_keep_the_wide_matte_chips() {
        // Narrowing the matte grid buys a row back on a small display, but the
        // chips have to hold names like "Adaptive". Anything with room keeps
        // the two-abreast grid, so this is never paid for by an ordinary user.
        for (screen_w, screen_h, scale) in
            [(1920, 1080, 1.0), (1920, 1080, 1.25), (2560, 1440, 1.0), (1600, 900, 1.0)]
        {
            let (cw, ch) = client_for_screen(screen_w, screen_h, scale);
            let layout = layout_controls(scale, cw, ch, 7);
            let widest = layout
                .controls
                .iter()
                .filter(|(_, control)| matches!(control, Ctl::Matte(_)))
                .map(|(rect, _)| rect.right - rect.left)
                .max()
                .expect("matte chips are laid out");
            assert_eq!(
                widest,
                (104.0 * scale) as i32,
                "{screen_w}x{screen_h} @{scale} lost the wide matte chips"
            );
        }
    }

    #[test]
    fn section_labels_keep_their_line_even_in_the_tightest_layout() {
        // Each group is captioned one line pitch above its first control, so
        // compression must never pull a group up into the row above it.
        let line = 20;
        let tight = super::layout_column(1.0, 1161, 676, 7, 3, 1.0);
        let first = |wanted: fn(&Ctl) -> bool| {
            tight
                .controls
                .iter()
                .filter(|(_, control)| wanted(control))
                .map(|(rect, _)| *rect)
                .min_by_key(|rect| rect.top)
                .expect("control is laid out")
        };
        let matte = first(|c| matches!(c, Ctl::Matte(_)));
        let aspect = first(|c| matches!(c, Ctl::Aspect(_)));
        let tool = first(|c| matches!(c, Ctl::Tool(_)));
        let last_matte = tight
            .controls
            .iter()
            .filter(|(_, c)| matches!(c, Ctl::Matte(_)))
            .map(|(rect, _)| rect.bottom)
            .max()
            .unwrap();

        assert!(matte.top >= tight.tab_strip.bottom + line, "MATTE label has no line");
        assert!(
            tight.padding_slider.top >= last_matte + line,
            "PADDING label runs into the matte chips"
        );
        assert!(
            aspect.top >= tight.padding_slider.bottom + line,
            "ASPECT label runs into the slider"
        );
        // Crop sits between them and is what ANNOTATE has to clear.
        let crop = first(|c| matches!(c, Ctl::Crop));
        assert!(tool.top >= crop.bottom + line, "ANNOTATE label runs into the crop button");
    }

    #[test]
    fn the_control_column_does_not_collide_with_the_action_block() {
        // Every screen the editor is expected to look right on. The column
        // grows downward from the tab strip while Copy/Save/Share are pinned to
        // the bottom, so anything added in the middle eats the gap between
        // them — this is what says how much is left.
        // 1366x768 — the smallest display in common use — is the floor this
        // holds at. A 720p-sized workspace still keeps every button clear of
        // every other; what it loses is the two caption lines above the output
        // block, and buying those back would need matte chips too narrow to
        // hold a name like "Adaptive".
        for (screen_w, screen_h, scale) in [
            (1920, 1080, 1.0),
            (1920, 1080, 1.25),
            (2560, 1440, 1.0),
            (2560, 1440, 1.5),
            (3840, 2160, 1.5),
            (3840, 2160, 2.0),
            (1600, 900, 1.0),
            (1366, 768, 1.0),
        ] {
            let (cw, ch) = client_for_screen(screen_w, screen_h, scale);
            let layout = layout_controls(scale, cw, ch, 7);
            let column_bottom = layout
                .controls
                .iter()
                .filter(|(_, control)| {
                    matches!(control, Ctl::Size(_) | Ctl::Color(_) | Ctl::Clear)
                })
                .map(|(rect, _)| rect.bottom)
                .max()
                .expect("the palette rows exist");
            let action_top = layout
                .controls
                .iter()
                .filter(|(_, control)| {
                    matches!(control, Ctl::OutputSize(_) | Ctl::CustomSize)
                })
                .map(|(rect, _)| rect.top)
                .min()
                .expect("the output-size row exists");
            // Two label lines sit above the output block, so the gap has to
            // clear them rather than merely not overlap.
            let headroom = super::action_headroom(scale);
            assert!(
                column_bottom + headroom <= action_top,
                "{screen_w}x{screen_h} @{scale}: column ends at {column_bottom}, \
                 actions start at {action_top} (client {cw}x{ch})"
            );
        }
    }

    #[test]
    fn a_swept_crop_normalizes_and_stays_inside_the_capture() {
        // Dragged up and to the left, and off the edge of the capture.
        let crop = crop_from_points((900.0, 700.0), (-50.0, -30.0), 1000, 800);
        assert_eq!(crop, Crop { x: 0, y: 0, w: 900, h: 700 });

        let crop = crop_from_points((600.0, 400.0), (2000.0, 2000.0), 1000, 800);
        assert_eq!(crop, Crop { x: 600, y: 400, w: 400, h: 400 });

        // A flick rather than a drag gives a small crop, not an empty one, and
        // is nudged back inside rather than hanging off the far edge.
        let crop = crop_from_points((1000.0, 800.0), (1000.0, 800.0), 1000, 800);
        assert_eq!((crop.w, crop.h), (MIN_CROP, MIN_CROP));
        assert_eq!((crop.x + crop.w, crop.y + crop.h), (1000, 800));
    }

    #[test]
    fn dragging_a_crop_into_an_edge_stops_it_instead_of_resizing_it() {
        let crop = Crop { x: 100, y: 100, w: 400, h: 300 };
        let moved = move_crop(crop, 50.0, -40.0, 1000, 800);
        assert_eq!(moved, Crop { x: 150, y: 60, w: 400, h: 300 });

        // Pushed hard into the bottom-right: it parks against the edge with
        // its size intact.
        let pinned = move_crop(crop, 9000.0, 9000.0, 1000, 800);
        assert_eq!(pinned, Crop { x: 600, y: 500, w: 400, h: 300 });
        let pinned = move_crop(crop, -9000.0, -9000.0, 1000, 800);
        assert_eq!(pinned, Crop { x: 0, y: 0, w: 400, h: 300 });
    }

    #[test]
    fn resizing_a_crop_pins_the_opposite_corner_and_survives_crossing_it() {
        let crop = Crop { x: 100, y: 100, w: 400, h: 300 };
        // Corner 0 is the top-left; the bottom-right at (500, 400) must not
        // move while it is dragged.
        let (resized, held) = resize_crop(crop, 0, (200.0, 250.0), 1000, 800);
        assert_eq!(resized, Crop { x: 200, y: 250, w: 300, h: 150 });
        assert_eq!(held, 0);

        // Dragged past the pinned corner, the grabbed handle becomes the
        // bottom-right one, so the next move still pins (500, 400).
        let (crossed, held) = resize_crop(crop, 0, (700.0, 600.0), 1000, 800);
        assert_eq!(crossed, Crop { x: 500, y: 400, w: 200, h: 200 });
        assert_eq!(held, 2);
        let (again, _) = resize_crop(crossed, held, (800.0, 700.0), 1000, 800);
        assert_eq!(again, Crop { x: 500, y: 400, w: 300, h: 300 });

        assert_eq!(corner_index((0.0, 0.0), (10.0, 10.0)), 0);
        assert_eq!(corner_index((20.0, 0.0), (10.0, 10.0)), 1);
        assert_eq!(corner_index((20.0, 20.0), (10.0, 10.0)), 2);
        assert_eq!(corner_index((0.0, 20.0), (10.0, 10.0)), 3);
    }

    #[test]
    fn a_press_on_the_crop_picks_the_handle_before_the_interior() {
        let crop = Crop { x: 100, y: 100, w: 400, h: 300 };
        // Right on the bottom-right corner, which is also inside the rect: the
        // handle has to win or a full-frame crop could never be resized.
        assert!(crop_contains(crop, (500.0, 400.0)));
        assert!(matches!(
            crop_drag_for(crop, (498.0, 398.0), 8.0, 1000, 800),
            CropDrag::Corner(2)
        ));
        assert!(matches!(
            crop_drag_for(crop, (300.0, 250.0), 8.0, 1000, 800),
            CropDrag::Move(_)
        ));
        assert!(matches!(
            crop_drag_for(crop, (900.0, 700.0), 8.0, 1000, 800),
            CropDrag::New(_)
        ));
    }

    #[test]
    fn the_first_drag_on_an_uncropped_capture_sweeps_a_new_frame() {
        // The opening state of the tool. Every point is inside it, so treating
        // an interior press as a move would make the obvious gesture — drag a
        // box round what you want — do nothing at all.
        let full = Crop::full(1000, 800);
        assert!(matches!(
            crop_drag_for(full, (400.0, 300.0), 8.0, 1000, 800),
            CropDrag::New(_)
        ));
        // Corners still resize, and once it has been narrowed the interior
        // moves it again.
        assert!(matches!(
            crop_drag_for(full, (2.0, 3.0), 8.0, 1000, 800),
            CropDrag::Corner(0)
        ));
        let narrowed = Crop { x: 0, y: 0, w: 999, h: 800 };
        assert!(matches!(
            crop_drag_for(narrowed, (400.0, 300.0), 8.0, 1000, 800),
            CropDrag::Move(_)
        ));
    }

    #[test]
    fn exporting_a_crop_keeps_the_right_pixels_and_the_annotations_over_them() {
        // A capture with one marked pixel column, so where it lands in the
        // export says exactly how the crop was applied.
        let mut raw = RgbaImage::from_pixel(400, 300, Rgba([0, 0, 0, 255]));
        for y in 0..300 {
            raw.put_pixel(250, y, Rgba([0, 255, 0, 255]));
        }
        let crop = Crop { x: 200, y: 100, w: 120, h: 90 };
        let content =
            image::imageops::crop_imm(&raw, crop.x, crop.y, crop.w, crop.h).to_image();
        let plain = crate::style::Style {
            name: "Plain",
            backdrop: crate::style::Backdrop::Plain,
        };

        // A blur placed in raw coordinates directly over the marked column,
        // which the crop's origin puts at content x=50, y=45..55.
        let anns = vec![blur((245.0, 145.0), (255.0, 155.0))];
        let exported = render_final(
            &content,
            &anns,
            (crop.x as f32, crop.y as f32),
            &plain,
            (0.14, None),
            1,
            crate::output::OUTPUT_ORIGINAL,
        );

        assert_eq!(exported.dimensions(), (120, 90));
        // The marked column was at raw x=250, so it must survive at x=50.
        assert_eq!(exported.get_pixel(50, 5)[1], 255);
        assert_eq!(exported.get_pixel(49, 5)[1], 0);
        // And the blur landed over that column rather than 200px off the left
        // edge — where an un-offset annotation would have fallen outside the
        // cropped image entirely, leaving the column pristine all the way down.
        assert_ne!(exported.get_pixel(50, 50), &Rgba([0, 255, 0, 255]));
        assert_eq!(exported.get_pixel(50, 5), &Rgba([0, 255, 0, 255]));
    }

    #[test]
    fn an_uncropped_export_is_unchanged_by_the_crop_machinery() {
        let mut raw = RgbaImage::from_pixel(120, 80, Rgba([10, 20, 30, 255]));
        raw.put_pixel(7, 9, Rgba([255, 0, 0, 255]));
        let plain = crate::style::Style {
            name: "Plain",
            backdrop: crate::style::Backdrop::Plain,
        };
        let exported = render_final(
            &raw,
            &[],
            (0.0, 0.0),
            &plain,
            (0.14, None),
            1,
            crate::output::OUTPUT_ORIGINAL,
        );
        assert_eq!(exported.dimensions(), (120, 80));
        assert_eq!(exported.get_pixel(7, 9), &Rgba([255, 0, 0, 255]));
    }

    fn aurora_style() -> crate::style::Style {
        crate::style::Style {
            name: "Test",
            backdrop: crate::style::Backdrop::Linear {
                c1: crate::style::Rgb(0.2, 0.3, 0.6),
                c2: crate::style::Rgb(0.4, 0.2, 0.5),
            },
        }
    }

    /// SBS-1020: Copy/Save of a tall capture + 16:9 used to compose the
    /// native padded canvas (here ~11 MP) before the Original no-op cap.
    /// The export must land at the planned size, not the native frame.
    #[test]
    fn copy_of_a_tall_capture_with_forced_aspect_composes_at_the_budget() {
        let raw = RgbaImage::from_pixel(160, 2400, Rgba([24, 32, 48, 255]));
        let native = crate::compose::framed_size(
            160,
            2400,
            &crate::compose::ComposeOpts {
                aspect: Some(16.0 / 9.0),
                ..Default::default()
            },
            true,
        );
        assert!(
            native.0 as u64 * native.1 as u64 > crate::compose::MAX_FRAMED_PIXELS,
            "fixture is not past the budget: {native:?}"
        );
        let exported = render_final(
            &raw,
            &[],
            (0.0, 0.0),
            &aurora_style(),
            (crate::compose::DEFAULT_PAD_FACTOR, Some(16.0 / 9.0)),
            1,
            crate::output::OUTPUT_ORIGINAL,
        );
        let pixels = exported.width() as u64 * exported.height() as u64;
        assert!(
            pixels <= crate::compose::MAX_FRAMED_PIXELS,
            "exported {}×{} = {pixels} px",
            exported.width(),
            exported.height()
        );
        assert!(exported.width() < native.0);
        assert!(exported.height() < native.1);
        let ratio = exported.width() as f64 / exported.height() as f64;
        assert!((ratio - 16.0 / 9.0).abs() < 0.03);
    }

    #[test]
    fn email_copy_of_a_tall_forced_aspect_fits_the_edge_cap() {
        let raw = RgbaImage::from_pixel(400, 2000, Rgba([24, 32, 48, 255]));
        let exported = render_final(
            &raw,
            &[],
            (0.0, 0.0),
            &aurora_style(),
            (crate::compose::DEFAULT_PAD_FACTOR, Some(16.0 / 9.0)),
            1,
            crate::output::OUTPUT_EMAIL,
        );
        assert!(
            exported.width().max(exported.height()) <= crate::output::OUTPUT_EMAIL,
            "exported {}×{}",
            exported.width(),
            exported.height()
        );
        let plan = crate::compose::plan_framed_export(
            400,
            2000,
            crate::compose::DEFAULT_PAD_FACTOR,
            Some(16.0 / 9.0),
            true,
            1,
            crate::output::OUTPUT_EMAIL,
        );
        assert_eq!(exported.dimensions(), (plan.canvas_w, plan.canvas_h));
        let native = crate::compose::framed_size(
            400,
            2000,
            &crate::compose::ComposeOpts {
                aspect: Some(16.0 / 9.0),
                ..Default::default()
            },
            true,
        );
        assert!(native.0.max(native.1) > crate::output::OUTPUT_EMAIL);
    }

    fn plain_style() -> crate::style::Style {
        crate::style::Style {
            name: "Plain",
            backdrop: crate::style::Backdrop::Plain,
        }
    }

    #[test]
    fn plain_copy_and_batch_export_agree_on_small_capture_size() {
        let raw = RgbaImage::from_pixel(400, 225, Rgba([24, 32, 48, 255]));
        let copy = render_final(
            &raw,
            &[],
            (0.0, 0.0),
            &plain_style(),
            (0.14, None),
            2,
            crate::output::OUTPUT_ORIGINAL,
        );
        let batch = crate::compose::export(
            &raw,
            &plain_style(),
            0.14,
            None,
            2,
            crate::output::OUTPUT_ORIGINAL,
        );
        assert_eq!(copy.dimensions(), (800, 450));
        assert_eq!(batch.dimensions(), copy.dimensions());
    }

    #[test]
    fn plain_size_label_matches_the_file_for_a_small_capture() {
        let raw = RgbaImage::from_pixel(400, 225, Rgba([24, 32, 48, 255]));
        let copy = render_final(
            &raw,
            &[],
            (0.0, 0.0),
            &plain_style(),
            (0.14, None),
            2,
            crate::output::OUTPUT_ORIGINAL,
        );
        let shown = final_size(400, 225, true, 0.14, None, 2, crate::output::OUTPUT_ORIGINAL);
        assert_eq!(shown, (800, 450));
        assert_eq!(shown, copy.dimensions());
    }

    #[test]
    fn confirming_custom_size_from_plain_original_keeps_the_supersampled_export() {
        let raw = RgbaImage::from_pixel(400, 225, Rgba([24, 32, 48, 255]));
        let shown = final_size(400, 225, true, 0.14, None, 2, crate::output::OUTPUT_ORIGINAL);
        let scale = crate::compose::export_super_scale(400, 225, 2);
        let composed = (400 * scale, 225 * scale);
        assert_eq!(shown, (800, 450));
        assert_eq!(composed, shown);
        let committed = custom_size_initial(0, composed, shown);
        assert_eq!(committed, 800);
        assert_eq!(custom_size_value(&committed.to_string(), composed), Some(800));
        let confirmed = render_final(
            &raw,
            &[],
            (0.0, 0.0),
            &plain_style(),
            (0.14, None),
            2,
            committed,
        );
        assert_eq!(confirmed.dimensions(), shown);
    }

    #[test]
    fn opening_custom_size_from_original_prefills_the_capped_long_edge() {
        let native = crate::compose::framed_size(
            1920,
            19_000,
            &crate::compose::ComposeOpts {
                aspect: Some(16.0 / 9.0),
                ..Default::default()
            },
            true,
        );
        assert_eq!(native, (35_200, 19_800));
        let shown = final_size(
            1920,
            19_000,
            false,
            crate::compose::DEFAULT_PAD_FACTOR,
            Some(16.0 / 9.0),
            1,
            crate::output::OUTPUT_ORIGINAL,
        );
        let long = shown.0.max(shown.1);
        assert!(
            (3500..=4500).contains(&long),
            "Original 16:9 label should be ~4K, got {shown:?}"
        );
        assert_eq!(custom_size_initial(0, native, shown), long);
        assert_eq!(custom_size_value("10000", native), Some(10_000));
        assert_eq!(
            custom_size_initial(crate::output::OUTPUT_EMAIL, native, shown),
            crate::output::OUTPUT_EMAIL
        );
    }

    #[test]
    fn a_sweep_past_the_edge_keeps_pinning_the_crop_to_the_capture() {
        // What the drag path does with a point the cursor has taken well
        // outside the picture: the helpers clamp, so the rectangle tracks up to
        // the edge instead of freezing where the cursor crossed it.
        let far_below_right = crop_from_points((400.0, 300.0), (99_999.0, 99_999.0), 1000, 800);
        assert_eq!(far_below_right, Crop { x: 400, y: 300, w: 600, h: 500 });

        let far_above_left = crop_from_points((400.0, 300.0), (-99_999.0, -99_999.0), 1000, 800);
        assert_eq!(far_above_left, Crop { x: 0, y: 0, w: 400, h: 300 });

        // Same for the two drags that adjust an existing frame.
        let crop = Crop { x: 100, y: 100, w: 400, h: 300 };
        let (resized, _) = resize_crop(crop, 0, (-5_000.0, -5_000.0), 1000, 800);
        assert_eq!(resized, Crop { x: 0, y: 0, w: 500, h: 400 });
        assert_eq!(
            move_crop(crop, 50_000.0, 50_000.0, 1000, 800),
            Crop { x: 600, y: 500, w: 400, h: 300 }
        );
    }

    #[test]
    fn a_crop_covering_everything_reads_as_no_crop() {
        assert!(Crop::full(1000, 800).is_full(1000, 800));
        assert!(!Crop { x: 0, y: 0, w: 999, h: 800 }.is_full(1000, 800));
        assert!(!Crop { x: 1, y: 0, w: 999, h: 800 }.is_full(1000, 800));
    }

    #[test]
    fn crop_arms_like_a_tool_without_being_one_of_them() {
        // It rides in `state.tool` so the arm/disarm ladder needs no special
        // case, but it must never index the palette: nothing iterating TOOLS
        // can be handed CROP_TOOL, and no annotation ever reports it.
        assert_eq!(CROP_TOOL, TOOLS.len());
        assert!(TOOLS.get(CROP_TOOL).is_none());
        for shape in [
            Shape::Arrow { from: (0.0, 0.0), to: (1.0, 1.0) },
            Shape::Text { pos: (0.0, 0.0), text: String::new() },
            Shape::Freehand { points: vec![(0.0, 0.0)] },
        ] {
            assert!(annotation_tool_index(&shape) < CROP_TOOL);
        }
    }

    #[test]
    fn crop_sits_with_the_framing_controls_and_spans_the_column() {
        let layout = layout_controls(1.0, 1600, 900, 7);
        let rect = |wanted: Ctl| {
            layout
                .controls
                .iter()
                .find(|(_, control)| *control == wanted)
                .map(|(rect, _)| *rect)
                .expect("control is laid out")
        };
        let crop = rect(Ctl::Crop);
        let last_aspect = rect(Ctl::Aspect(ASPECTS.len() - 1));
        let first_tool = rect(Ctl::Tool(0));

        // Between the aspect presets and the annotation grid — the group that
        // decides the shape of the picture, not the marks on it.
        assert!(crop.top >= last_aspect.bottom, "crop must follow the aspect presets");
        assert!(crop.bottom <= first_tool.top, "crop must precede the annotation tools");
        // Full column width, so it reads as its own mode rather than a tenth
        // chip stranded on a row of three.
        assert_eq!(crop.left, rect(Ctl::Aspect(0)).left);
        assert!(
            crop.right - crop.left > (first_tool.right - first_tool.left) * 2,
            "the crop button should be visibly wider than a tool chip"
        );
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

        // Fail closed: even a corner clip means part of the word is hidden in
        // the picture, so none of it may copy. The old majority threshold let
        // a word whose minority was pixelated copy in full.
        let grazed = [blur((190.0, 110.0), (300.0, 200.0))];
        assert!(redacted(&grazed, &covered));

        // A minority slice, exactly half, and a majority all redact alike.
        let minority = [blur((100.0, 100.0), (140.0, 120.0))];
        assert!(redacted(&minority, &covered));
        let exact_half = [blur((100.0, 100.0), (150.0, 120.0))];
        assert!(redacted(&exact_half, &covered));
        let majority = [blur((100.0, 100.0), (190.0, 120.0))];
        assert!(redacted(&majority, &covered));

        // Corners given in any order still describe the same box.
        let reversed = [blur((210.0, 130.0), (90.0, 90.0))];
        assert!(redacted(&reversed, &covered));

        // Several boxes: any one of them touching the word is enough.
        let several = [
            blur((0.0, 0.0), (10.0, 10.0)),
            blur((150.0, 105.0), (160.0, 115.0)),
        ];
        assert!(redacted(&several, &covered));

        // A box that merely shares an edge hides no pixels of the word.
        let adjacent = [blur((200.0, 100.0), (300.0, 120.0))];
        assert!(!redacted(&adjacent, &covered));

        // A neighbouring word clear of every box stays selectable.
        let neighbour = word("visible", 0, (220.0, 100.0, 300.0, 120.0));
        assert!(!redacted(&anns, &neighbour));

        // Other annotation kinds never hide text.
        let boxed = [Annotation {
            shape: Shape::Rect { a: (90.0, 90.0), b: (210.0, 130.0) },
            ..blur((0.0, 0.0), (0.0, 0.0))
        }];
        assert!(!redacted(&boxed, &covered));
    }

    fn pending_select(generation: u64) -> Option<TextSelect> {
        Some(TextSelect {
            words: Vec::new(),
            anchor: None,
            focus: None,
            pending: true,
            generation,
            message: None,
            dragging: false,
        })
    }

    fn ocr_ok(doc: u64, generation: u64, text: &str) -> OcrCompletion {
        OcrCompletion {
            doc,
            generation,
            result: Ok(vec![word(text, 0, (0.0, 0.0, 10.0, 10.0))]),
        }
    }

    #[test]
    fn a_delayed_ocr_result_lands_on_its_own_tab_never_the_active_one() {
        // Tab A started recognition, the user switched to tab B and started it
        // there too. A's worker is slower and finishes second — each result
        // must reach the tab whose bitmap it read.
        let mut a = pending_select(1);
        let mut b = pending_select(2);

        assert!(deliver_ocr([(1, &mut a), (2, &mut b)], ocr_ok(2, 2, "b-word")));
        let b_select = b.as_ref().unwrap();
        assert!(!b_select.pending);
        assert_eq!(b_select.words[0].text, "b-word");
        // A is untouched: still waiting on its own worker.
        assert!(a.as_ref().unwrap().pending);
        assert!(a.as_ref().unwrap().words.is_empty());

        assert!(deliver_ocr([(1, &mut a), (2, &mut b)], ocr_ok(1, 1, "a-word")));
        assert_eq!(a.as_ref().unwrap().words[0].text, "a-word");
        assert_eq!(b.as_ref().unwrap().words[0].text, "b-word");
    }

    #[test]
    fn an_earlier_request_cannot_satisfy_a_later_one_on_the_same_tab() {
        // Select Text was re-armed on the same tab, superseding request 1 with
        // request 2. Request 1's stale words must be dropped — and must leave
        // the tab pending so request 2's real result can still land.
        let mut select = pending_select(2);
        assert!(!deliver_ocr([(7, &mut select)], ocr_ok(7, 1, "stale")));
        assert!(select.as_ref().unwrap().pending);
        assert!(select.as_ref().unwrap().words.is_empty());

        assert!(deliver_ocr([(7, &mut select)], ocr_ok(7, 2, "fresh")));
        assert_eq!(select.as_ref().unwrap().words[0].text, "fresh");
    }

    #[test]
    fn completions_for_closed_tabs_or_a_left_mode_are_discarded() {
        // The tab was closed while the worker ran: its id is gone.
        let mut other = pending_select(5);
        assert!(!deliver_ocr([(9, &mut other)], ocr_ok(3, 1, "orphan")));
        assert!(other.as_ref().unwrap().pending);

        // The user left Select Text on a tab that still exists.
        let mut left: Option<TextSelect> = None;
        assert!(!deliver_ocr([(3, &mut left)], ocr_ok(3, 1, "late")));
        assert!(left.is_none());
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
        assert_eq!(custom_size_value("10000", (1200, 19_000)), Some(10_000));
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
    fn abandoning_custom_size_restores_the_size_from_before_live_preview() {
        let edit = CustomSizeEdit {
            input: "1200".into(),
            replace_on_type: false,
            invalid: false,
            original_max_edge: 2712,
        };

        assert_eq!(
            output_max_edge_after_custom_size_cancel(Some(&edit), 1200),
            2712
        );
        assert_eq!(output_max_edge_after_custom_size_cancel(None, 1920), 1920);
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
    fn the_preview_source_matches_the_pane_and_never_upscales() {
        let pane = |w: i32, h: i32| RECT { left: 0, top: 0, right: w, bottom: h };

        // The pane drives it, so a large monitor gets a sharp preview instead
        // of a stretched one. This is the case that was visibly soft: a 2862px
        // capture on a big screen used to work from a 1200px bitmap.
        assert_eq!(preview_target_edge(pane(2480, 1400), 2862, 1694), 2480);

        // Never more than the capture actually has. Pixels that do not exist
        // in the source cannot be recovered by composing at a larger size.
        assert_eq!(preview_target_edge(pane(2480, 1400), 1000, 600), 1000);

        // Bounded above, so an enormous monitor cannot turn every rebuild into
        // a visible pause, and below, so a tiny window still previews usably.
        assert_eq!(preview_target_edge(pane(7000, 4000), 8000, 6000), 3200);
        assert_eq!(preview_target_edge(pane(200, 120), 2862, 1694), 900);

        // Portrait panes and portrait captures are measured on their own long
        // edge, not on width.
        assert_eq!(preview_target_edge(pane(1000, 2000), 2000, 3000), 2000);

        // Whatever the target, the working bitmap keeps the capture's shape
        // and the drag source stays exactly half of it.
        let raw = RgbaImage::from_pixel(2862, 1694, Rgba([10, 20, 30, 255]));
        let (small, metric, fast, metric_fast) = preview_sources(&raw, 2400);
        assert_eq!(small.width(), 2400);
        assert!((metric - 2400.0 / 2862.0).abs() < 0.001);
        assert_eq!(fast.width(), small.width() / 2);
        assert!((metric_fast - metric * 0.5).abs() < 0.001);

        // A target at or above the capture uses it untouched, so a small
        // capture is never blurred by a pointless resize.
        let (same, metric, _, _) = preview_sources(&raw, 4000);
        assert_eq!((same.width(), same.height()), (2862, 1694));
        assert_eq!(metric, 1.0);
    }

    /// The bug this guards: a maximized 2560x1392 capture on a 1440p monitor
    /// worked from a 1200px bitmap and the editor stretched it ~1.39x to fill
    /// the pane, so the text being annotated was softer than the capture.
    /// Sizing the source to the pane has to leave the blit downscaling.
    #[test]
    fn the_editor_never_has_to_stretch_its_preview_to_fill_the_pane() {
        // Editor opens at 85% of the work area; layout_controls carves the
        // pane out of that. Both monitors here, plus a small window.
        for (work_w, work_h, raw_w, raw_h) in [
            (2560, 1392, 2560u32, 1392u32),
            (1920, 1032, 1920, 1032),
            (2560, 1392, 3840, 2160),
            (1280, 720, 2560, 1392),
        ] {
            let (cw, ch) = ((work_w as f32 * 0.85) as i32, (work_h as f32 * 0.85) as i32);
            let pane = layout_controls(1.0, cw, ch, 7).preview_box;

            let target = preview_target_edge(pane, raw_w, raw_h);
            let raw = RgbaImage::from_pixel(raw_w, raw_h, Rgba([9, 9, 9, 255]));
            let (small, metric, _, _) = preview_sources(&raw, target);

            // What rebuild_preview composes: content plus the matte's padding.
            let opts = crate::compose::ComposeOpts {
                metric_scale: metric,
                pad_factor: crate::compose::DEFAULT_PAD_FACTOR,
                aspect: None,
            };
            let layout =
                crate::compose::layout(small.width() as usize, small.height() as usize, &opts);
            let composite_w = small.width() as i32 + layout.pad_x as i32 * 2;
            let composite_h = small.height() as i32 + layout.pad_y as i32 * 2;

            let (_, _, draw_scale, _, _) =
                preview_draw_geometry(pane, composite_w, composite_h, 1.0);
            assert!(
                draw_scale <= 1.0,
                "pane {}x{} with a {raw_w}x{raw_h} capture stretches the preview {draw_scale:.3}x",
                pane.right - pane.left,
                pane.bottom - pane.top,
            );
        }
    }

    #[test]
    fn picking_a_tool_toggles_it_and_switching_keeps_the_new_one() {
        for tool in 0..TOOLS.len() {
            // Every tool arms from the selector and disarms on a second pick.
            assert_eq!(tool_after_pick(None, tool), Some(tool));
            assert_eq!(tool_after_pick(Some(tool), tool), None);
            // Any other tool takes over rather than clearing.
            let other = (tool + 1) % TOOLS.len();
            assert_eq!(tool_after_pick(Some(other), tool), Some(tool));
        }
    }

    #[test]
    fn output_size_feedback_names_the_choice_and_exact_pixels() {
        assert_eq!(
            output_size_summary(crate::output::OUTPUT_EMAIL, (1495, 1600)),
            "Email  \u{00b7}  1495 \u{00d7} 1600 px"
        );
        assert_eq!(
            output_size_summary(crate::output::OUTPUT_COMPACT, (1121, 1200)),
            "Compact  \u{00b7}  1121 \u{00d7} 1200 px"
        );
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
                .filter(|(_, control)| matches!(control, Ctl::Copy | Ctl::Save | Ctl::Share))
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

