//! Focused post-recording editor: large matte preview, trim timeline, and
//! export actions. Non-modal on the main loop.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use image::RgbaImage;
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreatePen, CreateSolidBrush, DeleteObject, DrawTextW, Ellipse,
    EndPaint, FillRect, GetMonitorInfoW, InvalidateRect, MonitorFromPoint, RoundRect, SelectObject,
    SetBkMode, SetTextColor, CLEARTYPE_QUALITY, DEFAULT_CHARSET, DT_CENTER, DT_END_ELLIPSIS,
    DT_LEFT, DT_SINGLELINE, DT_VCENTER, FF_DONTCARE, HDC, HFONT, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, PAINTSTRUCT, PS_SOLID, TRANSPARENT,
};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, SetFocus, VK_CONTROL, VK_DELETE, VK_END, VK_ESCAPE, VK_HOME, VK_LEFT, VK_RIGHT,
    VK_SPACE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetWindowLongPtrW,
    GetCursorPos, LoadCursorW, MessageBoxW, PostMessageW, RegisterClassW, SetForegroundWindow,
    SetWindowLongPtrW, SetWindowPos, CREATESTRUCTW, CS_DBLCLKS, CS_HREDRAW, CS_VREDRAW,
    GWLP_USERDATA, IDC_ARROW, IDYES, MB_DEFBUTTON2, MB_ICONWARNING, MB_YESNO, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOZORDER, WM_APP, WM_CHAR, WM_CLOSE, WM_CONTEXTMENU, WM_ERASEBKGND,
    WM_KEYDOWN, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCCREATE, WM_NCDESTROY,
    WM_PAINT, WM_RBUTTONDOWN, WM_RBUTTONUP, WNDCLASSW, WINDOW_STYLE, WS_CAPTION,
    WS_EX_APPWINDOW, WS_MAXIMIZEBOX, WS_SYSMENU, WS_THICKFRAME, WS_VISIBLE,
};

const WM_EXPORT_PROGRESS: u32 = WM_APP + 20;
const WM_UNICHAR_MESSAGE: u32 = 0x0109;
const UNICODE_NOCHAR: usize = 0xFFFF;
const WM_EXPORT_DONE: u32 = WM_APP + 21;
const WM_PLAYBACK_FRAME: u32 = WM_APP + 22;
const WM_PLAYBACK_DONE: u32 = WM_APP + 23;
const WM_EXPORT_STALLED: u32 = WM_APP + 24;
/// The filmstrip and scrub cache finished loading in the background.
const WM_PROBE_READY: u32 = WM_APP + 25;
/// The scrub decoder produced the exact frame under the playhead.
const WM_SCRUB_FRAME: u32 = WM_APP + 26;
static NEXT_EXPORT_ID: AtomicU64 = AtomicU64::new(1);

fn recording_delete_prompt(path: &std::path::Path) -> String {
    let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_else(|| "this recording".into());
    format!("Delete {name}?\n\nThis permanently removes the recording and cannot be undone.")
}

fn delete_recording_files(mp4: &std::path::Path, gif: Option<&std::path::Path>) -> Result<()> {
    if let Some(gif) = gif {
        match std::fs::remove_file(gif) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("delete recording GIF"),
        }
    }
    match std::fs::remove_file(mp4) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("delete recording"),
    }
}

const ASPECTS: [(&str, Option<f32>); 5] = [
    ("Auto", None),
    ("1:1", Some(1.0)),
    ("4:3", Some(4.0 / 3.0)),
    ("16:9", Some(16.0 / 9.0)),
    ("Social", Some(1.91)),
];

struct ExportDone {
    id: u64,
    path: PathBuf,
    result: std::result::Result<(), String>,
}

#[derive(Default)]
struct PlaybackMailbox {
    frame: Option<(u64, crate::trim::PlaybackFrame)>,
    frame_posted: bool,
    done: Option<(u64, std::result::Result<(), String>, bool)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct PreviewBaseKey {
    width: u32,
    height: u32,
    matte: usize,
    padding: u32,
    aspect: usize,
}

#[derive(Clone, Copy, PartialEq)]
enum Act {
    Play,
    Reveal,
    Copy,
    Share,
    Delete,
    SaveTrim,
}

#[derive(Clone, Copy, PartialEq)]
enum Handle {
    Start,
    End,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Tool {
    Arrow,
    Line,
    Rect,
    Ellipse,
    Highlight,
    Text,
    Blur,
    Counter,
    Pen,
}

const VIDEO_TOOLS: [(Tool, &str); 9] = [
    (Tool::Arrow, "Arrow"),
    (Tool::Line, "Line"),
    (Tool::Rect, "Box"),
    (Tool::Ellipse, "Oval"),
    (Tool::Highlight, "Mark"),
    (Tool::Text, "Text"),
    (Tool::Blur, "Blur"),
    (Tool::Counter, "Step"),
    (Tool::Pen, "Pen"),
];

/// The drawer's own name for a tool, reused on the + Add chip so a closed
/// drawer still says what is armed.
fn tool_label(tool: Tool) -> &'static str {
    VIDEO_TOOLS
        .iter()
        .find(|(candidate, _)| *candidate == tool)
        .map(|(_, label)| *label)
        .unwrap_or("Tool")
}

/// Adding closes the drawer so it stops covering the frame, but the tool it
/// armed is still live. The chip carries that state: with the drawer shut it
/// reads as the armed tool's name, which is both the reminder and the way back
/// to the lit chip that turns it off.
fn add_chip_label(tools_open: bool, tool: Option<Tool>) -> &'static str {
    match (tools_open, tool) {
        (true, _) => "Done",
        (false, Some(tool)) => tool_label(tool),
        (false, None) => "+ Add",
    }
}

#[derive(Clone, Copy, PartialEq)]
enum TimingChoice {
    WholeVideo,
    ThreeSeconds,
}

#[derive(Clone, Copy, PartialEq)]
enum Drag {
    Trim(Handle),
    Playhead,
    Padding,
    CaptionSize,
    CaptionOpacity,
    Draw {
        index: usize,
        start: (f32, f32),
    },
    Move {
        index: usize,
        last: (f32, f32),
        undo_pushed: bool,
    },
    Reshape {
        index: usize,
        handle: crate::video_edit::ShapeHandle,
        undo_pushed: bool,
    },
}

struct TextEntry {
    pos: (f32, f32),
    text: String,
    editing: Option<usize>,
}

struct WindowLayout {
    controls: Vec<(RECT, Act, &'static str)>,
    matte_controls: Vec<(RECT, usize)>,
    padding_slider: RECT,
    aspect_controls: Vec<(RECT, usize)>,
    add_control: RECT,
    tool_controls: Vec<(RECT, Tool, &'static str)>,
    color_controls: Vec<(RECT, usize)>,
    size_controls: Vec<(RECT, usize)>,
    caption_size_slider: RECT,
    caption_opacity_slider: RECT,
    caption_style_controls: Vec<(RECT, crate::video_edit::CaptionStyle)>,
    timing_controls: Vec<(RECT, TimingChoice)>,
    undo_control: RECT,
    delete_control: RECT,
    preview: RECT,
    strip: RECT,
}

struct State {
    mp4: PathBuf,
    gif: Option<PathBuf>,
    summary: String,
    controls: Vec<(RECT, Act, &'static str)>,
    hover: i32,
    theme: crate::theme::Theme,
    font: HFONT,
    font_small: HFONT,
    font_big: HFONT,
    scale: f32,
    width: i32,
    height: i32,
    // Trim state.
    duration: i64,
    raw_thumbs: Vec<(Vec<u8>, u32, u32)>,
    scrub_previews: Vec<(Vec<u8>, u32, u32)>,
    source_size: (u32, u32),
    thumbs: Vec<(Vec<u8>, u32, u32)>,
    preview_raw: Option<(Vec<u8>, u32, u32)>,
    preview: Option<(Vec<u8>, u32, u32)>,
    preview_base_cache: Option<(PreviewBaseKey, RgbaImage)>,
    preview_rect: RECT,
    strip: RECT,
    trim_start: i64,
    trim_end: i64,
    playhead: i64,
    dragging: Option<Drag>,
    styles: Vec<crate::style::Style>,
    matte_index: usize,
    matte_controls: Vec<(RECT, usize)>,
    pad_factor: f32,
    aspect_idx: usize,
    padding_slider: RECT,
    aspect_controls: Vec<(RECT, usize)>,
    annotations: Vec<crate::video_edit::Item>,
    undo: Vec<Vec<crate::video_edit::Item>>,
    selected: Option<usize>,
    tools_open: bool,
    tool: Option<Tool>,
    color_idx: usize,
    size_idx: usize,
    caption_size: f32,
    caption_style: crate::video_edit::CaptionStyle,
    caption_box_opacity: f32,
    text_entry: Option<TextEntry>,
    add_control: RECT,
    tool_controls: Vec<(RECT, Tool, &'static str)>,
    color_controls: Vec<(RECT, usize)>,
    size_controls: Vec<(RECT, usize)>,
    caption_size_slider: RECT,
    caption_opacity_slider: RECT,
    caption_style_controls: Vec<(RECT, crate::video_edit::CaptionStyle)>,
    timing_controls: Vec<(RECT, TimingChoice)>,
    undo_control: RECT,
    delete_control: RECT,
    status: Option<String>,
    /// Last successful edit export. The action buttons keep pointing at the
    /// original recording; this only drives their labels and the reveal on
    /// close.
    exported: Option<PathBuf>,
    exporting: bool,
    export_id: Option<u64>,
    export_cancel: Option<Arc<AtomicBool>>,
    close_after_export: bool,
    export_stalled: bool,
    playing: bool,
    playback_generation: u64,
    playback_cancel: Option<Arc<AtomicBool>>,
    playback_mailbox: Arc<Mutex<PlaybackMailbox>>,
    resume_after_drag: bool,
    /// Requests to the scrub decoder. Dropped with the window, which ends the
    /// worker's loop.
    scrub_tx: Option<std::sync::mpsc::Sender<crate::trim::ScrubRequest>>,
    /// Bumped on every scrub request so frames for a position the user has
    /// already left can be discarded on arrival.
    scrub_generation: u64,
    /// True while a share upload is in flight. Deliberately separate from
    /// `status` — that string gets overwritten by unrelated handlers (export
    /// progress, playback) while a share runs in the background, which would
    /// silently defeat a guard built on top of it.
    sharing: bool,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn available_export_path(candidate: PathBuf) -> PathBuf {
    if !candidate.exists() {
        return candidate;
    }
    let parent = candidate.parent().unwrap_or_else(|| std::path::Path::new(""));
    let stem = candidate
        .file_stem()
        .map(|value| value.to_string_lossy())
        .unwrap_or_default();
    let extension = candidate
        .extension()
        .map(|value| value.to_string_lossy())
        .unwrap_or_default();
    for number in 2..10_000 {
        let name = if extension.is_empty() {
            format!("{stem}-{number}")
        } else {
            format!("{stem}-{number}.{extension}")
        };
        let path = parent.join(name);
        if !path.exists() {
            return path;
        }
    }
    candidate
}

unsafe fn make_font(h: i32, weight: i32) -> HFONT {
    CreateFontW(
        h,
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

/// Layout for a client size. Rerun on resize.
fn layout(scale: f32, cw: i32, ch: i32, style_count: usize) -> WindowLayout {
    let sc = |v: i32| (v as f32 * scale) as i32;
    // Programmatic resizes and bad restored placements are not constrained by
    // WM_GETMINMAXINFO. Normalize defensively so no rectangle can invert even
    // for a transient undersized WM_SIZE.
    let (minimum_w, minimum_h) = minimum_client_size(scale);
    let (cw, ch) = (cw.max(minimum_w), ch.max(minimum_h));
    let m = sc(24);
    // Keep the recording itself dominant. The previous layout devoted nearly
    // half of a maximized 1080p client to header/controls/timeline, which made
    // precise caption placement needlessly difficult at common DPI scales.
    let timeline_h = (ch / 9).clamp(sc(64), sc(80));
    let strip_bottom = ch - sc(80);
    let strip = RECT {
        left: m,
        top: strip_bottom - timeline_h,
        right: cw - m,
        bottom: strip_bottom,
    };
    let preview = RECT {
        left: m,
        top: sc(74),
        right: cw - m,
        bottom: strip.top - sc(86),
    };
    let mut matte_controls = Vec::new();
    if style_count > 0 {
        let start = m + sc(52);
        let gap = sc(7);
        let available = (cw - m - start - gap * (style_count as i32 - 1)).max(style_count as i32);
        let chip_w = available / style_count as i32;
        let chip_top = preview.bottom + sc(4);
        for i in 0..style_count {
            let left = start + i as i32 * (chip_w + gap);
            matte_controls.push((
                RECT {
                    left,
                    top: chip_top,
                    right: left + chip_w,
                    bottom: chip_top + sc(28),
                },
                i,
            ));
        }
    }
    // The second settings row must be visually separate from the matte chips.
    // They previously shared the exact same boundary, which made the controls
    // run together (and overlap after DPI rounding).
    let settings_top = matte_controls
        .first()
        .map(|(rect, _)| rect.bottom + sc(8))
        .unwrap_or(preview.bottom + sc(12));
    let padding_slider = RECT {
        left: m + sc(68),
        top: settings_top,
        right: m + sc(258),
        bottom: settings_top + sc(26),
    };
    let mut aspect_controls = Vec::new();
    let aspect_start = m + sc(344);
    let aspect_gap = sc(6);
    let aspect_available =
        (cw - m - aspect_start - aspect_gap * (ASPECTS.len() as i32 - 1)).max(1);
    let aspect_width = (aspect_available / ASPECTS.len() as i32).max(sc(48));
    for index in 0..ASPECTS.len() {
        let left = aspect_start + index as i32 * (aspect_width + aspect_gap);
        aspect_controls.push((
            RECT {
                left,
                top: settings_top,
                right: (left + aspect_width).min(cw - m),
                bottom: settings_top + sc(26),
            },
            index,
        ));
    }
    let mut controls = Vec::new();
    let by = ch - sc(52);
    let labels: [(Act, &'static str, i32); 6] = [
        (Act::SaveTrim, "Export edit", 108),
        (Act::Play, "Play", 82),
        (Act::Reveal, "Show in folder", 126),
        (Act::Copy, "Copy", 112),
        (Act::Share, "Share", 90),
        (Act::Delete, "Delete", 80),
    ];
    let mut x = m;
    for (act, label, w) in labels {
        controls.push((
            RECT {
                left: x,
                top: by,
                right: x + sc(w),
                bottom: by + sc(34),
            },
            act,
            label,
        ));
        x += sc(w) + sc(9);
    }

    let add_control = RECT {
        left: preview.right - sc(88),
        top: preview.top + sc(12),
        right: preview.right - sc(12),
        bottom: preview.top + sc(42),
    };
    let panel_left = preview.right - sc(382);
    let panel_top = add_control.bottom + sc(8);
    let mut tool_controls = Vec::new();
    for (index, (tool, label)) in VIDEO_TOOLS.into_iter().enumerate() {
        let column = index as i32 % 3;
        let row = index as i32 / 3;
        let left = panel_left + sc(10) + column * sc(122);
        let top = panel_top + sc(10) + row * sc(34);
        tool_controls.push((
            RECT {
                left,
                top,
                right: left + sc(114),
                bottom: top + sc(28),
            },
            tool,
            label,
        ));
    }
    let mut color_controls = Vec::new();
    for index in 0..crate::annotate::COLORS.len() {
        let left = panel_left + sc(12) + index as i32 * sc(30);
        color_controls.push((
            RECT {
                left,
                top: panel_top + sc(116),
                right: left + sc(22),
                bottom: panel_top + sc(138),
            },
            index,
        ));
    }
    let mut size_controls = Vec::new();
    for index in 0..3 {
        let left = panel_left + sc(146) + index as i32 * sc(34);
        size_controls.push((
            RECT {
                left,
                top: panel_top + sc(114),
                right: left + sc(28),
                bottom: panel_top + sc(140),
            },
            index,
        ));
    }
    let caption_size_slider = RECT {
        left: panel_left + sc(58),
        top: panel_top + sc(152),
        right: panel_left + sc(190),
        bottom: panel_top + sc(178),
    };
    let caption_opacity_slider = RECT {
        left: panel_left + sc(92),
        top: panel_top + sc(186),
        right: panel_left + sc(190),
        bottom: panel_top + sc(212),
    };
    let caption_style_controls = vec![
        (
            RECT {
                left: panel_left + sc(206),
                top: panel_top + sc(152),
                right: panel_left + sc(278),
                bottom: panel_top + sc(178),
            },
            crate::video_edit::CaptionStyle::Shadow,
        ),
        (
            RECT {
                left: panel_left + sc(286),
                top: panel_top + sc(152),
                right: panel_left + sc(372),
                bottom: panel_top + sc(178),
            },
            crate::video_edit::CaptionStyle::Box,
        ),
    ];
    let timing_controls = vec![
        (
            RECT {
                left: panel_left + sc(90),
                top: panel_top + sc(222),
                right: panel_left + sc(214),
                bottom: panel_top + sc(250),
            },
            TimingChoice::WholeVideo,
        ),
        (
            RECT {
                left: panel_left + sc(222),
                top: panel_top + sc(222),
                right: panel_left + sc(372),
                bottom: panel_top + sc(250),
            },
            TimingChoice::ThreeSeconds,
        ),
    ];
    let undo_control = RECT {
        left: panel_left + sc(254),
        top: panel_top + sc(114),
        right: panel_left + sc(308),
        bottom: panel_top + sc(140),
    };
    let delete_control = RECT {
        left: panel_left + sc(314),
        top: panel_top + sc(114),
        right: panel_left + sc(372),
        bottom: panel_top + sc(140),
    };
    WindowLayout {
        controls,
        matte_controls,
        padding_slider,
        aspect_controls,
        add_control,
        tool_controls,
        color_controls,
        size_controls,
        caption_size_slider,
        caption_opacity_slider,
        caption_style_controls,
        timing_controls,
        undo_control,
        delete_control,
        preview,
        strip,
    }
}

fn thumb_image(bytes: &[u8], w: u32, h: u32) -> RgbaImage {
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for pixel in bytes.chunks_exact(4) {
        rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
    }
    RgbaImage::from_raw(w, h, rgba).expect("filmstrip frame has exact dimensions")
}

fn image_thumb(image: &RgbaImage) -> (Vec<u8>, u32, u32) {
    let mut bgra = Vec::with_capacity((image.width() * image.height() * 4) as usize);
    for pixel in image.pixels() {
        bgra.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
    }
    (bgra, image.width(), image.height())
}

fn compose_opts(pad_factor: f32, aspect_idx: usize) -> crate::compose::ComposeOpts {
    crate::compose::ComposeOpts {
        metric_scale: 1.0,
        pad_factor,
        aspect: ASPECTS[aspect_idx.min(ASPECTS.len() - 1)].1,
    }
}

fn state_compose_opts(state: &State) -> crate::compose::ComposeOpts {
    compose_opts(state.pad_factor, state.aspect_idx)
}

fn matte_thumbs(
    raw: &[(Vec<u8>, u32, u32)],
    style: &crate::style::Style,
    opts: &crate::compose::ComposeOpts,
) -> Vec<(Vec<u8>, u32, u32)> {
    if crate::compose::is_plain(style) {
        return raw.to_vec();
    }
    let mut bases = std::collections::HashMap::<(u32, u32), RgbaImage>::new();
    raw.iter()
        .map(|(bytes, w, h)| {
            let image = thumb_image(bytes, *w, *h);
            let base = bases
                .entry((*w, *h))
                .or_insert_with(|| crate::compose::compose_base(*w as usize, *h as usize, style, opts));
            let mut composed = base.clone();
            crate::compose::blend_content(&mut composed, &image, opts);
            image_thumb(&composed)
        })
        .collect()
}

fn set_playhead(state: &mut State, x: i32) {
    let span = (state.strip.right - state.strip.left).max(1) as f64;
    state.playhead = ((((x - state.strip.left) as f64 / span) * state.duration as f64) as i64)
        .clamp(0, state.duration);
    // Show the nearest cached frame straight away so the picture always tracks
    // the cursor, then ask the decoder for the exact one.
    refresh_preview(state);
    request_scrub_frame(state);
}

/// Ask the scrub decoder for the frame under the playhead. Cheap and
/// non-blocking: the worker coalesces, so spamming this during a drag is fine.
fn request_scrub_frame(state: &mut State) {
    state.scrub_generation = state.scrub_generation.wrapping_add(1);
    let request = crate::trim::ScrubRequest {
        position: state.playhead,
        generation: state.scrub_generation,
    };
    if let Some(tx) = &state.scrub_tx {
        if tx.send(request).is_err() {
            // The decoder died; cached frames still drive the preview.
            state.scrub_tx = None;
        }
    }
}

fn refresh_preview(state: &mut State) {
    if state.scrub_previews.is_empty() {
        state.preview_raw = None;
        state.preview = None;
        return;
    }
    let index = if state.duration > 0 {
        ((state.playhead as f64 / state.duration as f64) * state.scrub_previews.len() as f64)
            .floor()
            .min((state.scrub_previews.len() - 1) as f64) as usize
    } else {
        0
    };
    state.preview_raw = Some(state.scrub_previews[index].clone());
    recompose_preview(state);
}

fn minimum_client_size(scale: f32) -> (i32, i32) {
    (
        (760.0 * scale).round() as i32,
        (620.0 * scale).round() as i32,
    )
}

fn editor_style() -> WINDOW_STYLE {
    WS_CAPTION | WS_SYSMENU | WS_VISIBLE | WS_THICKFRAME | WS_MAXIMIZEBOX
}

fn recompose_preview(state: &mut State) {
    let opts = state_compose_opts(state);
    let Some(frame) = state.preview_raw.as_ref() else {
        state.preview = None;
        return;
    };
    let mut raw = thumb_image(&frame.0, frame.1, frame.2);
    // Padding changes the canvas geometry, so the matte has to be rebuilt from
    // scratch on every mouse move. Drag at quarter the pixels; the mouse-up
    // handler recomposes at full quality.
    if state.dragging == Some(Drag::Padding) {
        raw = image::imageops::resize(
            &raw,
            (raw.width() / 2).max(1),
            (raw.height() / 2).max(1),
            image::imageops::FilterType::Triangle,
        );
    }
    let content_size = (raw.width(), raw.height());
    let plain = crate::compose::is_plain(&state.styles[state.matte_index]);
    let content_offset = if plain {
        (0.0, 0.0)
    } else {
        let layout = crate::compose::layout(raw.width() as usize, raw.height() as usize, &opts);
        (layout.pad_x as f32, layout.pad_y as f32)
    };
    let mut image = if plain {
        raw
    } else {
        let key = PreviewBaseKey {
            width: raw.width(),
            height: raw.height(),
            matte: state.matte_index,
            padding: state.pad_factor.to_bits(),
            aspect: state.aspect_idx,
        };
        if state.preview_base_cache.as_ref().map(|(cached, _)| *cached) != Some(key) {
            state.preview_base_cache = Some((
                key,
                crate::compose::compose_base(
                    raw.width() as usize,
                    raw.height() as usize,
                    &state.styles[state.matte_index],
                    &opts,
                ),
            ));
        }
        let mut composed = state.preview_base_cache.as_ref().unwrap().1.clone();
        crate::compose::blend_content(&mut composed, &raw, &opts);
        composed
    };
    let skip = state.text_entry.as_ref().and_then(|entry| entry.editing);
    let annotation_time = current_annotation_time(state);
    crate::video_edit::render_preview_at(
        &mut image,
        &state.annotations,
        annotation_time,
        skip,
        content_size,
        state.source_size,
        content_offset,
    );
    if let Some(entry) = &state.text_entry {
        let (start, end) = default_range(state);
        let draft = crate::video_edit::Item {
            shape: crate::video_edit::Shape::Text {
                pos: entry.pos,
                text: format!("{}|", entry.text),
            },
            start,
            end,
            color: state.color_idx,
            size: state.caption_size,
            caption_style: state.caption_style,
            caption_box_opacity: state.caption_box_opacity,
        };
        crate::video_edit::render_preview_at(
            &mut image,
            std::slice::from_ref(&draft),
            annotation_time,
            None,
            content_size,
            state.source_size,
            content_offset,
        );
    }
    state.preview = Some(image_thumb(&image));
}

fn update_padding(state: &mut State, x: i32) {
    let slider = state.padding_slider;
    let t = ((x - slider.left) as f32 / (slider.right - slider.left).max(1) as f32)
        .clamp(0.0, 1.0);
    state.pad_factor = crate::compose::PAD_SLIDER_MIN
        + t * (crate::compose::PAD_SLIDER_MAX - crate::compose::PAD_SLIDER_MIN);
    recompose_preview(state);
}

fn refresh_matte_thumbs(state: &mut State) {
    let opts = state_compose_opts(state);
    state.thumbs = matte_thumbs(
        &state.raw_thumbs,
        &state.styles[state.matte_index],
        &opts,
    );
}

fn refresh_preview_exact(state: &mut State) {
    let max_w = (state.preview_rect.right - state.preview_rect.left).max(2) as u32;
    let max_h = (state.preview_rect.bottom - state.preview_rect.top).max(2) as u32;
    match crate::trim::preview_frame(&state.mp4, state.playhead, max_w, max_h) {
        Ok(frame) => {
            state.preview_raw = Some(frame);
            recompose_preview(state);
        }
        Err(_) => refresh_preview(state),
    }
}

fn stop_playback(state: &mut State) {
    if let Some(cancel) = state.playback_cancel.take() {
        cancel.store(true, Ordering::Relaxed);
    }
    state.playing = false;
    state.playback_generation = state.playback_generation.wrapping_add(1);
}

fn start_playback(hwnd: HWND, state: &mut State) {
    if state.duration <= 0 || state.trim_end <= state.trim_start {
        return;
    }
    stop_playback(state);
    if state.playhead < state.trim_start || state.playhead >= state.trim_end - 100_000 {
        state.playhead = state.trim_start;
    }

    let generation = state.playback_generation;
    let cancel = Arc::new(AtomicBool::new(false));
    state.playback_cancel = Some(cancel.clone());
    state.playing = true;
    state.status = None;

    let source = state.mp4.clone();
    let start = state.playhead;
    let end = state.trim_end;
    let max_w = ((state.preview_rect.right - state.preview_rect.left).max(2) as u32).min(1440);
    let max_h = ((state.preview_rect.bottom - state.preview_rect.top).max(2) as u32).min(900);
    let mailbox = state.playback_mailbox.clone();
    let hwnd_raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let result = crate::trim::playback_frames(
            &source,
            start,
            end,
            max_w,
            max_h,
            &cancel,
            |frame| {
                let should_post = {
                    let mut mailbox = mailbox.lock().unwrap();
                    mailbox.frame = Some((generation, frame));
                    if mailbox.frame_posted {
                        false
                    } else {
                        mailbox.frame_posted = true;
                        true
                    }
                };
                if !should_post {
                    return true;
                }
                let posted = unsafe {
                    PostMessageW(
                        HWND(hwnd_raw as *mut _),
                        WM_PLAYBACK_FRAME,
                        WPARAM(0),
                        LPARAM(0),
                    )
                    .is_ok()
                };
                if !posted {
                    mailbox.lock().unwrap().frame_posted = false;
                }
                posted
            },
        )
        .map_err(|error| format!("{error:#}"));
        let cancelled = cancel.load(Ordering::Relaxed);
        mailbox.lock().unwrap().done = Some((generation, result, cancelled));
        unsafe {
            let _ = PostMessageW(
                HWND(hwnd_raw as *mut _),
                WM_PLAYBACK_DONE,
                WPARAM(0),
                LPARAM(0),
            );
        }
        if com.is_ok() {
            unsafe { CoUninitialize() };
        }
    });
    unsafe {
        let _ = InvalidateRect(hwnd, None, false);
    }
}

fn toggle_playback(hwnd: HWND, state: &mut State) {
    if state.playing {
        stop_playback(state);
        state.status = Some("paused".into());
        unsafe {
            let _ = InvalidateRect(hwnd, None, false);
        }
    } else {
        start_playback(hwnd, state);
    }
}

fn current_size(state: &State) -> f32 {
    [0.82, 1.0, 1.35][state.size_idx.min(2)]
}

fn default_range(state: &State) -> (i64, i64) {
    (0, state.duration.max(1))
}

/// Annotation ranges are end-exclusive, but the editor deliberately leaves
/// the visible playhead on `trim_end` when playback finishes or End is pressed.
/// Evaluate preview annotations on the final representable instant so the held
/// last frame remains editable without changing timeline or export semantics.
fn annotation_preview_time(playhead: i64, trim_start: i64, trim_end: i64) -> i64 {
    if trim_end > trim_start && playhead >= trim_end {
        trim_end.saturating_sub(1).max(trim_start)
    } else {
        playhead
    }
}

fn current_annotation_time(state: &State) -> i64 {
    annotation_preview_time(state.playhead, state.trim_start, state.trim_end)
}

fn caption_controls_active(state: &State) -> bool {
    state.text_entry.is_some()
        || state.tool == Some(Tool::Text)
        || state
            .selected
            .and_then(|index| state.annotations.get(index))
            .is_some_and(|item| matches!(item.shape, crate::video_edit::Shape::Text { .. }))
}

fn selected_timing(state: &State) -> TimingChoice {
    state
        .selected
        .and_then(|index| state.annotations.get(index))
        .map(|item| {
            if item.start <= 0 && item.end >= state.duration {
                TimingChoice::WholeVideo
            } else {
                TimingChoice::ThreeSeconds
            }
        })
        .unwrap_or(TimingChoice::WholeVideo)
}

fn apply_timing(state: &mut State, choice: TimingChoice) {
    let Some(index) = state
        .selected
        .filter(|index| *index < state.annotations.len())
    else {
        return;
    };
    push_undo(state);
    let (start, end) = match choice {
        TimingChoice::WholeVideo => default_range(state),
        TimingChoice::ThreeSeconds => {
            let duration = state.duration.max(1);
            let start = state.playhead.clamp(0, duration);
            let end = (start + 30_000_000).min(duration);
            if end > start {
                (start, end)
            } else {
                ((duration - 30_000_000).max(0), duration)
            }
        }
    };
    state.annotations[index].start = start;
    state.annotations[index].end = end;
    recompose_preview(state);
}

fn update_caption_size(state: &mut State, x: i32) {
    const MIN: f32 = 0.7;
    const MAX: f32 = 3.4;
    let slider = state.caption_size_slider;
    let t = ((x - slider.left) as f32 / (slider.right - slider.left).max(1) as f32)
        .clamp(0.0, 1.0);
    state.caption_size = MIN + t * (MAX - MIN);
    if let Some(index) = state
        .selected
        .filter(|index| *index < state.annotations.len())
    {
        if matches!(state.annotations[index].shape, crate::video_edit::Shape::Text { .. }) {
            state.annotations[index].size = state.caption_size;
        }
    }
    recompose_preview(state);
}

fn update_caption_opacity(state: &mut State, x: i32) {
    let slider = state.caption_opacity_slider;
    let t = ((x - slider.left) as f32 / (slider.right - slider.left).max(1) as f32)
        .clamp(0.0, 1.0);
    // Keep the plate useful at the low end while still offering a light,
    // glassy treatment. Zero opacity is already available via Shadow style.
    state.caption_box_opacity = 0.20 + t * 0.75;
    if let Some(index) = state
        .selected
        .filter(|index| *index < state.annotations.len())
    {
        if matches!(
            state.annotations[index].shape,
            crate::video_edit::Shape::Text { .. }
        ) {
            state.annotations[index].caption_box_opacity = state.caption_box_opacity;
        }
    }
    recompose_preview(state);
}

fn sync_selected_controls(state: &mut State, index: usize) {
    let Some(item) = state.annotations.get(index) else {
        return;
    };
    state.color_idx = item.color;
    if matches!(item.shape, crate::video_edit::Shape::Text { .. }) {
        state.caption_size = item.size;
        state.caption_style = item.caption_style;
        state.caption_box_opacity = item.caption_box_opacity;
    } else {
        state.size_idx = if item.size < 0.91 {
            0
        } else if item.size > 1.17 {
            2
        } else {
            1
        };
    }
}

/// True once the editor holds changes the recorded file on disk does not have.
/// The action buttons act on that original file either way; this only decides
/// whether their labels have to say so.
fn has_edits(state: &State) -> bool {
    let matte = state
        .styles
        .get(state.matte_index)
        .is_some_and(|style| !crate::compose::is_plain(style));
    matte
        || state.trim_start > 0
        || state.trim_end < state.duration
        || !state.annotations.is_empty()
}

fn push_undo(state: &mut State) {
    state.undo.push(state.annotations.clone());
    if state.undo.len() > 40 {
        state.undo.remove(0);
    }
}

fn undo(state: &mut State) {
    if let Some(previous) = state.undo.pop() {
        state.annotations = previous;
        state.selected = None;
        state.text_entry = None;
        recompose_preview(state);
    }
}

fn delete_selected(state: &mut State) {
    if let Some(index) = state
        .selected
        .filter(|index| *index < state.annotations.len())
    {
        push_undo(state);
        state.annotations.remove(index);
        state.selected = None;
        recompose_preview(state);
    }
}

fn preview_image_rect(state: &State, frame: &(Vec<u8>, u32, u32)) -> RECT {
    let inset = s(state, 12);
    let available_w = (state.preview_rect.right - state.preview_rect.left - inset * 2).max(1);
    let available_h = (state.preview_rect.bottom - state.preview_rect.top - inset * 2).max(1);
    let scale = (available_w as f32 / frame.1 as f32).min(available_h as f32 / frame.2 as f32);
    let width = (frame.1 as f32 * scale).round().max(1.0) as i32;
    let height = (frame.2 as f32 * scale).round().max(1.0) as i32;
    RECT {
        left: state.preview_rect.left
            + (state.preview_rect.right - state.preview_rect.left - width) / 2,
        top: state.preview_rect.top
            + (state.preview_rect.bottom - state.preview_rect.top - height) / 2,
        right: state.preview_rect.left
            + (state.preview_rect.right - state.preview_rect.left - width) / 2
            + width,
        bottom: state.preview_rect.top
            + (state.preview_rect.bottom - state.preview_rect.top - height) / 2
            + height,
    }
}

fn preview_content_rect(state: &State) -> Option<RECT> {
    let frame = state.preview.as_ref()?;
    let raw = state.preview_raw.as_ref()?;
    let output = preview_image_rect(state, frame);
    if crate::compose::is_plain(&state.styles[state.matte_index]) {
        return Some(output);
    }
    let opts = state_compose_opts(state);
    let layout = crate::compose::layout(raw.1 as usize, raw.2 as usize, &opts);
    let scale = ((output.right - output.left) as f32 / frame.1.max(1) as f32)
        .min((output.bottom - output.top) as f32 / frame.2.max(1) as f32);
    Some(RECT {
        left: output.left + (layout.pad_x as f32 * scale).round() as i32,
        top: output.top + (layout.pad_y as f32 * scale).round() as i32,
        right: output.left + ((layout.pad_x as u32 + raw.1) as f32 * scale).round() as i32,
        bottom: output.top + ((layout.pad_y as u32 + raw.2) as f32 * scale).round() as i32,
    })
}

fn screen_to_preview(state: &State, x: i32, y: i32) -> Option<(f32, f32)> {
    let rect = preview_content_rect(state)?;
    if x < rect.left || x > rect.right || y < rect.top || y > rect.bottom {
        return None;
    }
    Some((
        ((x - rect.left) as f32 / (rect.right - rect.left).max(1) as f32).clamp(0.0, 1.0),
        ((y - rect.top) as f32 / (rect.bottom - rect.top).max(1) as f32).clamp(0.0, 1.0),
    ))
}

fn annotation_content_size(state: &State) -> (u32, u32) {
    (state.source_size.0.max(1), state.source_size.1.max(1))
}

fn hit_annotation(state: &State, point: (f32, f32)) -> Option<usize> {
    let content_size = annotation_content_size(state);
    let time = current_annotation_time(state);
    state
        .annotations
        .iter()
        .enumerate()
        .rev()
        .find(|(_, item)| {
            item.active_at(time)
                && crate::video_edit::hit(item, point, 0.018, content_size)
        })
        .map(|(index, _)| index)
}

fn hit_annotation_handle(
    state: &State,
    point: (f32, f32),
) -> Option<(usize, crate::video_edit::ShapeHandle)> {
    let content_size = annotation_content_size(state);
    let time = current_annotation_time(state);
    let index = state.selected.filter(|index| *index < state.annotations.len())?;
    let item = &state.annotations[index];
    if !item.active_at(time) {
        return None;
    }
    crate::video_edit::hit_handle(item, point, 0.026, content_size)
        .map(|handle| (index, handle))
}

fn contains(rect: RECT, x: i32, y: i32) -> bool {
    x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom
}

fn tool_panel(state: &State) -> RECT {
    let top = state
        .tool_controls
        .first()
        .map(|(rect, ..)| rect.top - s(state, 10))
        .unwrap_or(state.add_control.bottom);
    let mut bottom = state.delete_control.bottom + s(state, 10);
    if caption_controls_active(state) {
        bottom = bottom.max(state.caption_opacity_slider.bottom + s(state, 10));
    }
    if state.selected.is_some() {
        bottom = bottom.max(
            state
                .timing_controls
                .last()
                .map(|(rect, _)| rect.bottom + s(state, 10))
                .unwrap_or(bottom),
        );
    }
    RECT {
        left: state
            .tool_controls
            .first()
            .map(|(rect, ..)| rect.left - s(state, 10))
            .unwrap_or(state.add_control.left),
        top,
        right: state.delete_control.right + s(state, 10),
        bottom,
    }
}

fn shape_for_tool(tool: Tool, start: (f32, f32)) -> crate::video_edit::Shape {
    match tool {
        Tool::Arrow => crate::video_edit::Shape::Arrow {
            from: start,
            to: start,
        },
        Tool::Line => crate::video_edit::Shape::Line {
            from: start,
            to: start,
        },
        Tool::Rect => crate::video_edit::Shape::Rect { a: start, b: start },
        Tool::Ellipse => crate::video_edit::Shape::Ellipse { a: start, b: start },
        Tool::Highlight => crate::video_edit::Shape::Highlight { a: start, b: start },
        Tool::Blur => crate::video_edit::Shape::Blur { a: start, b: start },
        Tool::Pen => crate::video_edit::Shape::Freehand {
            points: vec![start],
        },
        Tool::Text | Tool::Counter => unreachable!("click tools use direct placement"),
    }
}

fn update_draw_shape(
    shape: &mut crate::video_edit::Shape,
    start: (f32, f32),
    point: (f32, f32),
) {
    match shape {
        crate::video_edit::Shape::Arrow { from, to }
        | crate::video_edit::Shape::Line { from, to } => {
            *from = start;
            *to = point;
        }
        crate::video_edit::Shape::Rect { a, b }
        | crate::video_edit::Shape::Ellipse { a, b }
        | crate::video_edit::Shape::Highlight { a, b }
        | crate::video_edit::Shape::Blur { a, b } => {
            *a = start;
            *b = point;
        }
        crate::video_edit::Shape::Freehand { points } => {
            if points.last().is_none_or(|last| {
                (last.0 - point.0).hypot(last.1 - point.1) >= 0.001
            }) {
                points.push(point);
            }
        }
        crate::video_edit::Shape::Counter { .. } | crate::video_edit::Shape::Text { .. } => {}
    }
}

fn draw_shape_is_degenerate(shape: &crate::video_edit::Shape) -> bool {
    match shape {
        crate::video_edit::Shape::Arrow { from, to }
        | crate::video_edit::Shape::Line { from, to } => {
            (from.0 - to.0).hypot(from.1 - to.1) < 0.008
        }
        crate::video_edit::Shape::Rect { a, b }
        | crate::video_edit::Shape::Ellipse { a, b }
        | crate::video_edit::Shape::Highlight { a, b }
        | crate::video_edit::Shape::Blur { a, b } => {
            (a.0 - b.0).hypot(a.1 - b.1) < 0.008
        }
        crate::video_edit::Shape::Freehand { points } => {
            points
                .windows(2)
                .map(|segment| {
                    (segment[0].0 - segment[1].0).hypot(segment[0].1 - segment[1].1)
                })
                .sum::<f32>()
                < 0.008
        }
        crate::video_edit::Shape::Counter { .. } | crate::video_edit::Shape::Text { .. } => false,
    }
}

fn next_counter_number(annotations: &[crate::video_edit::Item]) -> u32 {
    annotations
        .iter()
        .filter_map(|item| match &item.shape {
            crate::video_edit::Shape::Counter { n, .. } => Some(*n),
            _ => None,
        })
        .max()
        .unwrap_or(0)
        .saturating_add(1)
}

/// Picking a tool arms it until it is put away: picking the armed one again
/// disarms, picking another switches. Nothing else clears it except Esc, so a
/// run of boxes or arrows takes one trip to the drawer.
fn tool_after_pick(current: Option<Tool>, picked: Tool) -> Option<Tool> {
    (current != Some(picked)).then_some(picked)
}

fn select_annotation_tool(state: &mut State, tool: Tool) {
    stop_playback(state);
    state.text_entry = None;
    state.tool = tool_after_pick(state.tool, tool);
    if state.tool.is_none() {
        return;
    }
    state.selected = None;
    if tool == Tool::Text {
        state.color_idx = 3;
        state.caption_style = crate::video_edit::CaptionStyle::Box;
    }
}

fn commit_text(state: &mut State) {
    let Some(entry) = state.text_entry.take() else {
        return;
    };
    let text = entry.text.trim().to_string();
    if text.is_empty() {
        recompose_preview(state);
        return;
    }
    push_undo(state);
    let (start, end) = entry
        .editing
        .and_then(|index| {
            state
                .annotations
                .get(index)
                .map(|item| (item.start, item.end))
        })
        .unwrap_or_else(|| default_range(state));
    let item = crate::video_edit::Item {
        shape: crate::video_edit::Shape::Text {
            pos: entry.pos,
            text,
        },
        start,
        end,
        color: state.color_idx,
        size: state.caption_size,
        caption_style: state.caption_style,
        caption_box_opacity: state.caption_box_opacity,
    };
    if let Some(index) = entry
        .editing
        .filter(|index| *index < state.annotations.len())
    {
        state.annotations[index] = item;
        state.selected = Some(index);
    } else {
        state.annotations.push(item);
        state.selected = Some(state.annotations.len() - 1);
    }
    // The Text tool stays armed after a caption lands, so the next click on
    // the preview starts another one. Callers that mean to leave annotation
    // mode clear `tool` themselves.
    recompose_preview(state);
}

fn tool_for_shape(shape: &crate::video_edit::Shape) -> Tool {
    match shape {
        crate::video_edit::Shape::Text { .. } => Tool::Text,
        crate::video_edit::Shape::Arrow { .. } => Tool::Arrow,
        crate::video_edit::Shape::Line { .. } => Tool::Line,
        crate::video_edit::Shape::Freehand { .. } => Tool::Pen,
        crate::video_edit::Shape::Rect { .. } => Tool::Rect,
        crate::video_edit::Shape::Ellipse { .. } => Tool::Ellipse,
        crate::video_edit::Shape::Highlight { .. } => Tool::Highlight,
        crate::video_edit::Shape::Counter { .. } => Tool::Counter,
        crate::video_edit::Shape::Blur { .. } => Tool::Blur,
    }
}

fn selected_tool(state: &State) -> Option<Tool> {
    state
        .selected
        .and_then(|index| state.annotations.get(index))
        .map(|item| tool_for_shape(&item.shape))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaptionInput {
    Changed,
    Commit,
    Ignored,
}

/// Apply one character delivered by WM_CHAR/WM_UNICHAR. Keeping this logic
/// independent from the window procedure makes caption input regression
/// testable without synthetic keyboard input or stealing foreground focus.
fn apply_caption_input(text: &mut String, ch: char) -> CaptionInput {
    match ch {
        '\r' => CaptionInput::Commit,
        '\u{8}' => {
            text.pop();
            CaptionInput::Changed
        }
        ch if !ch.is_control() && text.chars().count() < 160 => {
            text.push(ch);
            CaptionInput::Changed
        }
        _ => CaptionInput::Ignored,
    }
}

/// Move a trim handle to window x, keeping at least ~0.3s between them.
fn set_handle(state: &mut State, h: Handle, x: i32) {
    let strip = state.strip;
    let span = (strip.right - strip.left).max(1) as f64;
    let t = (((x - strip.left) as f64 / span) * state.duration as f64) as i64;
    let t = t.clamp(0, state.duration);
    const MIN: i64 = 3_000_000;
    match h {
        Handle::Start => state.trim_start = t.min(state.trim_end - MIN).max(0),
        Handle::End => state.trim_end = t.max(state.trim_start + MIN).min(state.duration),
    }
}

fn s(state: &State, v: i32) -> i32 {
    (v as f32 * state.scale) as i32
}

unsafe fn paint_bgra_fit(hdc: HDC, rect: RECT, frame: &(Vec<u8>, u32, u32), state: &State) {
    let panel = CreateSolidBrush(state.theme.chip);
    let panel_pen = CreatePen(PS_SOLID, 1, state.theme.chip_line);
    let old_brush = SelectObject(hdc, panel);
    let old_pen = SelectObject(hdc, panel_pen);
    let _ = RoundRect(
        hdc,
        rect.left,
        rect.top,
        rect.right,
        rect.bottom,
        s(state, 14),
        s(state, 14),
    );
    SelectObject(hdc, old_brush);
    SelectObject(hdc, old_pen);
    let _ = DeleteObject(panel);
    let _ = DeleteObject(panel_pen);

    let inset = s(state, 12);
    let available_w = (rect.right - rect.left - inset * 2).max(1);
    let available_h = (rect.bottom - rect.top - inset * 2).max(1);
    let scale = (available_w as f32 / frame.1 as f32).min(available_h as f32 / frame.2 as f32);
    let draw_w = (frame.1 as f32 * scale).round().max(1.0) as i32;
    let draw_h = (frame.2 as f32 * scale).round().max(1.0) as i32;
    let left = rect.left + (rect.right - rect.left - draw_w) / 2;
    let top = rect.top + (rect.bottom - rect.top - draw_h) / 2;
    let info = windows::Win32::Graphics::Gdi::BITMAPINFO {
        bmiHeader: windows::Win32::Graphics::Gdi::BITMAPINFOHEADER {
            biSize: std::mem::size_of::<windows::Win32::Graphics::Gdi::BITMAPINFOHEADER>() as u32,
            biWidth: frame.1 as i32,
            biHeight: -(frame.2 as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: windows::Win32::Graphics::Gdi::BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    windows::Win32::Graphics::Gdi::SetStretchBltMode(hdc, windows::Win32::Graphics::Gdi::HALFTONE);
    windows::Win32::Graphics::Gdi::StretchDIBits(
        hdc,
        left,
        top,
        draw_w,
        draw_h,
        0,
        0,
        frame.1 as i32,
        frame.2 as i32,
        Some(frame.0.as_ptr() as *const _),
        &info,
        windows::Win32::Graphics::Gdi::DIB_RGB_COLORS,
        windows::Win32::Graphics::Gdi::SRCCOPY,
    );
}

unsafe fn paint_chip(hdc: HDC, rect: RECT, label: &str, selected: bool, state: &State) {
    let fill = CreateSolidBrush(if selected {
        state.theme.accent
    } else {
        state.theme.chip
    });
    let pen = CreatePen(
        PS_SOLID,
        1,
        if selected {
            state.theme.accent
        } else {
            state.theme.chip_line
        },
    );
    let old_brush = SelectObject(hdc, fill);
    let old_pen = SelectObject(hdc, pen);
    let _ = RoundRect(
        hdc,
        rect.left,
        rect.top,
        rect.right,
        rect.bottom,
        s(state, 9),
        s(state, 9),
    );
    SelectObject(hdc, old_brush);
    SelectObject(hdc, old_pen);
    let _ = DeleteObject(fill);
    let _ = DeleteObject(pen);
    SelectObject(hdc, state.font_small);
    SetTextColor(
        hdc,
        if selected {
            state.theme.accent_text
        } else {
            state.theme.muted
        },
    );
    let mut text = wide(label);
    let mut label_rect = rect;
    DrawTextW(
        hdc,
        &mut text,
        &mut label_rect,
        DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
    );
}

unsafe fn paint(hdc: HDC, state: &State) {
    let bg = CreateSolidBrush(state.theme.bg);
    FillRect(
        hdc,
        &RECT {
            left: 0,
            top: 0,
            right: state.width,
            bottom: state.height,
        },
        bg,
    );
    let _ = DeleteObject(bg);
    SetBkMode(hdc, TRANSPARENT);

    let m = s(state, 22);
    SelectObject(hdc, state.font_big);
    SetTextColor(hdc, state.theme.text);
    let mut t = wide("Recording editor");
    let mut rc = RECT {
        left: m,
        top: s(state, 6),
        right: state.width - m,
        bottom: s(state, 28),
    };
    DrawTextW(hdc, &mut t, &mut rc, DT_LEFT | DT_SINGLELINE | DT_VCENTER);

    SelectObject(hdc, state.font_small);
    SetTextColor(hdc, state.theme.muted);
    let mut sum = wide(&state.summary);
    let mut rc2 = RECT {
        left: m,
        top: s(state, 28),
        right: state.width - m,
        bottom: s(state, 48),
    };
    DrawTextW(
        hdc,
        &mut sum,
        &mut rc2,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER,
    );

    SetTextColor(hdc, state.theme.faint);
    let mut p = wide(&state.mp4.display().to_string());
    let mut rc3 = RECT {
        left: m,
        top: s(state, 48),
        right: state.width - m,
        bottom: s(state, 68),
    };
    DrawTextW(
        hdc,
        &mut p,
        &mut rc3,
        DT_LEFT | DT_END_ELLIPSIS | DT_SINGLELINE | DT_VCENTER,
    );

    if let Some(frame) = &state.preview {
        paint_bgra_fit(hdc, state.preview_rect, frame, state);

        if let Some(index) = state
            .selected
            .filter(|index| *index < state.annotations.len())
        {
            let item = &state.annotations[index];
            if item.active_at(current_annotation_time(state)) {
                let image_rect = preview_content_rect(state)
                    .unwrap_or_else(|| preview_image_rect(state, frame));
                let (x0, y0, x1, y1) =
                    crate::video_edit::bounds(item, annotation_content_size(state));
                let map_x = |x: f32| {
                    image_rect.left
                        + (x.clamp(0.0, 1.0) * (image_rect.right - image_rect.left) as f32) as i32
                };
                let map_y = |y: f32| {
                    image_rect.top
                        + (y.clamp(0.0, 1.0) * (image_rect.bottom - image_rect.top) as f32) as i32
                };
                let pen = CreatePen(windows::Win32::Graphics::Gdi::PS_DOT, 1, state.theme.accent);
                let old_pen = SelectObject(hdc, pen);
                let old_brush = SelectObject(
                    hdc,
                    windows::Win32::Graphics::Gdi::GetStockObject(
                        windows::Win32::Graphics::Gdi::HOLLOW_BRUSH,
                    ),
                );
                let pad = s(state, 5);
                let _ = windows::Win32::Graphics::Gdi::Rectangle(
                    hdc,
                    map_x(x0) - pad,
                    map_y(y0) - pad,
                    map_x(x1) + pad,
                    map_y(y1) + pad,
                );
                SelectObject(hdc, old_brush);
                SelectObject(hdc, old_pen);
                let _ = DeleteObject(pen);

                if let Some(handles) = crate::video_edit::handles(item) {
                    let handle_pen = CreatePen(PS_SOLID, s(state, 2).max(1), state.theme.accent);
                    let handle_fill = CreateSolidBrush(state.theme.bg);
                    let old_pen = SelectObject(hdc, handle_pen);
                    let old_brush = SelectObject(hdc, handle_fill);
                    let radius = s(state, 7);
                    for (_, point) in handles {
                        let (x, y) = (map_x(point.0), map_y(point.1));
                        let _ = Ellipse(
                            hdc,
                            x - radius,
                            y - radius,
                            x + radius + 1,
                            y + radius + 1,
                        );
                    }
                    SelectObject(hdc, old_brush);
                    SelectObject(hdc, old_pen);
                    let _ = DeleteObject(handle_fill);
                    let _ = DeleteObject(handle_pen);
                }
            }
        }
    }

    paint_chip(
        hdc,
        state.add_control,
        add_chip_label(state.tools_open, state.tool),
        state.tools_open || state.tool.is_some(),
        state,
    );
    if state.tools_open {
        let panel = tool_panel(state);
        let fill = CreateSolidBrush(state.theme.bg);
        let pen = CreatePen(PS_SOLID, 1, state.theme.chip_line);
        let old_brush = SelectObject(hdc, fill);
        let old_pen = SelectObject(hdc, pen);
        let _ = RoundRect(
            hdc,
            panel.left,
            panel.top,
            panel.right,
            panel.bottom,
            s(state, 12),
            s(state, 12),
        );
        SelectObject(hdc, old_brush);
        SelectObject(hdc, old_pen);
        let _ = DeleteObject(fill);
        let _ = DeleteObject(pen);

        let property_tool = if state.tool.is_none() {
            selected_tool(state)
        } else {
            None
        };
        for (rect, tool, label) in &state.tool_controls {
            paint_chip(
                hdc,
                *rect,
                label,
                state.tool == Some(*tool) || property_tool == Some(*tool),
                state,
            );
        }
        for (rect, index) in &state.color_controls {
            let [r, g, b] = crate::annotate::COLORS[*index];
            let color = windows::Win32::Foundation::COLORREF(
                r as u32 | ((g as u32) << 8) | ((b as u32) << 16),
            );
            let fill = CreateSolidBrush(color);
            let pen = CreatePen(
                PS_SOLID,
                if state.color_idx == *index { 3 } else { 1 },
                if state.color_idx == *index {
                    state.theme.text
                } else {
                    state.theme.chip_line
                },
            );
            let old_brush = SelectObject(hdc, fill);
            let old_pen = SelectObject(hdc, pen);
            let _ = RoundRect(
                hdc,
                rect.left,
                rect.top,
                rect.right,
                rect.bottom,
                s(state, 8),
                s(state, 8),
            );
            SelectObject(hdc, old_brush);
            SelectObject(hdc, old_pen);
            let _ = DeleteObject(fill);
            let _ = DeleteObject(pen);
        }
        if caption_controls_active(state) {
            SelectObject(hdc, state.font_small);
            SetTextColor(hdc, state.theme.muted);
            let mut size_label = wide(&format!("Size {}", (state.caption_size * 24.0).round() as i32));
            let mut size_rect = RECT {
                left: tool_panel(state).left + s(state, 12),
                top: state.caption_size_slider.top,
                right: state.caption_size_slider.left - s(state, 6),
                bottom: state.caption_size_slider.bottom,
            };
            DrawTextW(hdc, &mut size_label, &mut size_rect, DT_LEFT | DT_SINGLELINE | DT_VCENTER);
            let slider = state.caption_size_slider;
            let cy = (slider.top + slider.bottom) / 2;
            let track = CreateSolidBrush(state.theme.track);
            FillRect(
                hdc,
                &RECT {
                    left: slider.left,
                    top: cy - s(state, 2),
                    right: slider.right,
                    bottom: cy + s(state, 2),
                },
                track,
            );
            let _ = DeleteObject(track);
            let t = ((state.caption_size - 0.7) / (3.4 - 0.7)).clamp(0.0, 1.0);
            let thumb_x = slider.left + ((slider.right - slider.left) as f32 * t) as i32;
            let fill = CreateSolidBrush(state.theme.accent);
            FillRect(
                hdc,
                &RECT {
                    left: slider.left,
                    top: cy - s(state, 2),
                    right: thumb_x,
                    bottom: cy + s(state, 2),
                },
                fill,
            );
            let old = SelectObject(hdc, fill);
            let r = s(state, 6);
            let _ = RoundRect(hdc, thumb_x - r, cy - r, thumb_x + r, cy + r, r * 2, r * 2);
            SelectObject(hdc, old);
            let _ = DeleteObject(fill);
            for (rect, style) in &state.caption_style_controls {
                paint_chip(
                    hdc,
                    *rect,
                    match style {
                        crate::video_edit::CaptionStyle::Shadow => "Shadow",
                        crate::video_edit::CaptionStyle::Box => "Caption box",
                    },
                    state.caption_style == *style,
                    state,
                );
            }

            let opacity_active = state.caption_style == crate::video_edit::CaptionStyle::Box;
            SetTextColor(
                hdc,
                if opacity_active {
                    state.theme.muted
                } else {
                    state.theme.faint
                },
            );
            let mut opacity_label = wide(&format!(
                "Box {}%",
                (state.caption_box_opacity * 100.0).round() as i32
            ));
            let mut opacity_rect = RECT {
                left: tool_panel(state).left + s(state, 12),
                top: state.caption_opacity_slider.top,
                right: state.caption_opacity_slider.left - s(state, 6),
                bottom: state.caption_opacity_slider.bottom,
            };
            DrawTextW(
                hdc,
                &mut opacity_label,
                &mut opacity_rect,
                DT_LEFT | DT_SINGLELINE | DT_VCENTER,
            );
            let slider = state.caption_opacity_slider;
            let cy = (slider.top + slider.bottom) / 2;
            let track = CreateSolidBrush(state.theme.track);
            FillRect(
                hdc,
                &RECT {
                    left: slider.left,
                    top: cy - s(state, 2),
                    right: slider.right,
                    bottom: cy + s(state, 2),
                },
                track,
            );
            let _ = DeleteObject(track);
            let t = ((state.caption_box_opacity - 0.20) / 0.75).clamp(0.0, 1.0);
            let thumb_x = slider.left + ((slider.right - slider.left) as f32 * t) as i32;
            let fill = CreateSolidBrush(if opacity_active {
                state.theme.accent
            } else {
                state.theme.chip_line
            });
            FillRect(
                hdc,
                &RECT {
                    left: slider.left,
                    top: cy - s(state, 2),
                    right: thumb_x,
                    bottom: cy + s(state, 2),
                },
                fill,
            );
            let old = SelectObject(hdc, fill);
            let r = s(state, 6);
            let _ = RoundRect(hdc, thumb_x - r, cy - r, thumb_x + r, cy + r, r * 2, r * 2);
            SelectObject(hdc, old);
            let _ = DeleteObject(fill);
        } else {
            for (rect, index) in &state.size_controls {
                paint_chip(
                    hdc,
                    *rect,
                    ["S", "M", "L"][*index],
                    state.size_idx == *index,
                    state,
                );
            }
        }
        paint_chip(hdc, state.undo_control, "Undo", false, state);
        paint_chip(hdc, state.delete_control, "Delete", false, state);
        if state.selected.is_some() {
            SelectObject(hdc, state.font_small);
            SetTextColor(hdc, state.theme.muted);
            let mut timing_label = wide("Timing");
            let mut timing_rect = RECT {
                left: tool_panel(state).left + s(state, 12),
                top: state.timing_controls[0].0.top,
                right: state.timing_controls[0].0.left - s(state, 6),
                bottom: state.timing_controls[0].0.bottom,
            };
            DrawTextW(hdc, &mut timing_label, &mut timing_rect, DT_LEFT | DT_SINGLELINE | DT_VCENTER);
            let active_timing = selected_timing(state);
            for (rect, choice) in &state.timing_controls {
                paint_chip(
                    hdc,
                    *rect,
                    match choice {
                        TimingChoice::WholeVideo => "Whole video",
                        TimingChoice::ThreeSeconds => "3 sec here",
                    },
                    active_timing == *choice,
                    state,
                );
            }
        }
    }

    if state.text_entry.is_some() || state.tool.is_some() || state.selected.is_some() {
        let hint = if state.text_entry.is_some() {
            "Type caption   \u{00b7}   click anywhere to place   \u{00b7}   Esc cancel"
        } else if let Some(tool) = state.tool {
            match tool {
                Tool::Text => "Click the preview to place a caption   \u{00b7}   Text stays active   \u{00b7}   Esc exits",
                Tool::Arrow => "Drag on the preview to draw an arrow   \u{00b7}   Arrow stays active   \u{00b7}   Esc exits",
                Tool::Line => "Drag on the preview to draw a line   \u{00b7}   Line stays active   \u{00b7}   Esc exits",
                Tool::Rect => "Drag on the preview to draw a box   \u{00b7}   Box stays active   \u{00b7}   Esc exits",
                Tool::Ellipse => "Drag on the preview to draw an oval   \u{00b7}   Oval stays active   \u{00b7}   Esc exits",
                Tool::Highlight => "Drag on the preview to mark an area   \u{00b7}   Mark stays active   \u{00b7}   Esc exits",
                Tool::Blur => "Drag over anything sensitive to blur it   \u{00b7}   Blur stays active   \u{00b7}   Esc exits",
                Tool::Counter => "Click the preview to place the next step   \u{00b7}   Step stays active   \u{00b7}   Esc exits",
                Tool::Pen => "Draw on the preview   \u{00b7}   Pen stays active   \u{00b7}   Esc exits",
            }
        } else if let Some(index) = state.selected {
            match state.annotations.get(index).map(|item| &item.shape) {
                Some(crate::video_edit::Shape::Arrow { .. })
                | Some(crate::video_edit::Shape::Line { .. }) => {
                    "Selected   \u{00b7}   drag line to move   \u{00b7}   drag endpoints to redirect   \u{00b7}   right-click for properties"
                }
                Some(crate::video_edit::Shape::Rect { .. })
                | Some(crate::video_edit::Shape::Ellipse { .. })
                | Some(crate::video_edit::Shape::Highlight { .. })
                | Some(crate::video_edit::Shape::Blur { .. }) => {
                    "Selected   \u{00b7}   drag to move   \u{00b7}   drag corner handles to resize   \u{00b7}   right-click for properties"
                }
                _ => "Selected   \u{00b7}   drag to move   \u{00b7}   right-click for properties",
            }
        } else {
            ""
        };
        SelectObject(hdc, state.font_small);
        SetTextColor(hdc, state.theme.text);
        let mut text = wide(hint);
        let mut rect = RECT {
            left: state.preview_rect.left + s(state, 16),
            top: state.preview_rect.bottom - s(state, 34),
            right: state.preview_rect.right - s(state, 16),
            bottom: state.preview_rect.bottom - s(state, 12),
        };
        DrawTextW(
            hdc,
            &mut text,
            &mut rect,
            DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
        );
    }

    SelectObject(hdc, state.font_small);
    SetTextColor(hdc, state.theme.muted);
    let mut matte_label = wide("Matte");
    let matte_top = state
        .matte_controls
        .first()
        .map(|(rect, _)| rect.top)
        .unwrap_or(state.preview_rect.bottom + s(state, 16));
    let mut matte_label_rect = RECT {
        left: m,
        top: matte_top,
        right: m + s(state, 48),
        bottom: matte_top + s(state, 28),
    };
    DrawTextW(
        hdc,
        &mut matte_label,
        &mut matte_label_rect,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER,
    );

    for (rect, index) in &state.matte_controls {
        let selected = *index == state.matte_index;
        let fill = CreateSolidBrush(if selected {
            state.theme.accent
        } else {
            state.theme.chip
        });
        let pen = CreatePen(
            PS_SOLID,
            1,
            if selected {
                state.theme.accent
            } else {
                state.theme.chip_line
            },
        );
        let old_brush = SelectObject(hdc, fill);
        let old_pen = SelectObject(hdc, pen);
        let _ = RoundRect(
            hdc,
            rect.left,
            rect.top,
            rect.right,
            rect.bottom,
            s(state, 9),
            s(state, 9),
        );
        SelectObject(hdc, old_brush);
        SelectObject(hdc, old_pen);
        let _ = DeleteObject(fill);
        let _ = DeleteObject(pen);

        SetTextColor(
            hdc,
            if selected {
                state.theme.accent_text
            } else {
                state.theme.muted
            },
        );
        let mut name = wide(state.styles[*index].name);
        let mut label_rect = *rect;
        DrawTextW(
            hdc,
            &mut name,
            &mut label_rect,
            DT_CENTER | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
        );
    }

    let settings_top = state.padding_slider.top;
    SetTextColor(hdc, state.theme.muted);
    let mut padding_label = wide("Padding");
    let mut padding_label_rect = RECT {
        left: m,
        top: settings_top,
        right: state.padding_slider.left - s(state, 8),
        bottom: state.padding_slider.bottom,
    };
    DrawTextW(
        hdc,
        &mut padding_label,
        &mut padding_label_rect,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER,
    );

    let plain = crate::compose::is_plain(&state.styles[state.matte_index]);
    let slider = state.padding_slider;
    let cy = (slider.top + slider.bottom) / 2;
    let track_color = if plain {
        state.theme.chip_line
    } else {
        state.theme.track
    };
    let track = CreateSolidBrush(track_color);
    FillRect(
        hdc,
        &RECT {
            left: slider.left,
            top: cy - s(state, 2),
            right: slider.right,
            bottom: cy + s(state, 2),
        },
        track,
    );
    let _ = DeleteObject(track);
    let pad_t = ((state.pad_factor - crate::compose::PAD_SLIDER_MIN)
        / (crate::compose::PAD_SLIDER_MAX - crate::compose::PAD_SLIDER_MIN))
        .clamp(0.0, 1.0);
    let thumb_x = slider.left + ((slider.right - slider.left) as f32 * pad_t) as i32;
    let active_color = if plain {
        state.theme.faint
    } else {
        state.theme.accent
    };
    let filled = CreateSolidBrush(active_color);
    FillRect(
        hdc,
        &RECT {
            left: slider.left,
            top: cy - s(state, 2),
            right: thumb_x,
            bottom: cy + s(state, 2),
        },
        filled,
    );
    let thumb_pen = CreatePen(PS_SOLID, 1, active_color);
    let old_brush = SelectObject(hdc, filled);
    let old_pen = SelectObject(hdc, thumb_pen);
    let thumb_r = s(state, 7);
    let _ = RoundRect(
        hdc,
        thumb_x - thumb_r,
        cy - thumb_r,
        thumb_x + thumb_r,
        cy + thumb_r,
        thumb_r * 2,
        thumb_r * 2,
    );
    SelectObject(hdc, old_brush);
    SelectObject(hdc, old_pen);
    let _ = DeleteObject(filled);
    let _ = DeleteObject(thumb_pen);

    if let Some((first, _)) = state.aspect_controls.first() {
        SetTextColor(hdc, state.theme.muted);
        let mut aspect_label = wide("Aspect");
        let mut aspect_label_rect = RECT {
            left: state.padding_slider.right + s(state, 18),
            top: settings_top,
            right: first.left - s(state, 8),
            bottom: state.padding_slider.bottom,
        };
        DrawTextW(
            hdc,
            &mut aspect_label,
            &mut aspect_label_rect,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER,
        );
    }
    for (rect, index) in &state.aspect_controls {
        paint_chip(hdc, *rect, ASPECTS[*index].0, state.aspect_idx == *index, state);
    }

    // Filmstrip + trim handles.
    if !state.thumbs.is_empty() && state.duration > 0 {
        let strip = state.strip;
        SelectObject(hdc, state.font_small);
        SetTextColor(hdc, state.theme.faint);
        let mut timeline = wide("TIMELINE");
        let mut timeline_rect = RECT {
            left: strip.left,
            top: strip.top - s(state, 16),
            right: strip.right,
            bottom: strip.top - s(state, 2),
        };
        DrawTextW(
            hdc,
            &mut timeline,
            &mut timeline_rect,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER,
        );
        let mut x = strip.left;
        for (bgra, tw, th) in &state.thumbs {
            if x >= strip.right {
                break;
            }
            let info = windows::Win32::Graphics::Gdi::BITMAPINFO {
                bmiHeader: windows::Win32::Graphics::Gdi::BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<windows::Win32::Graphics::Gdi::BITMAPINFOHEADER>()
                        as u32,
                    biWidth: *tw as i32,
                    biHeight: -(*th as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: windows::Win32::Graphics::Gdi::BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            // Natural aspect: the strip is a filmstrip, not a stretch.
            let strip_h = strip.bottom - strip.top;
            let natural = (*tw as f32 * strip_h as f32 / *th as f32).max(2.0) as i32;
            let draw_w = natural.min(strip.right - x);
            windows::Win32::Graphics::Gdi::SetStretchBltMode(
                hdc,
                windows::Win32::Graphics::Gdi::HALFTONE,
            );
            // Source-clip the last (partial) cell so it isn't squeezed.
            let src_w = ((*tw as f32) * (draw_w as f32 / natural as f32)).max(1.0) as i32;
            windows::Win32::Graphics::Gdi::StretchDIBits(
                hdc,
                x,
                strip.top,
                draw_w,
                strip.bottom - strip.top,
                0,
                0,
                src_w,
                *th as i32,
                Some(bgra.as_ptr() as *const _),
                &info,
                windows::Win32::Graphics::Gdi::DIB_RGB_COLORS,
                windows::Win32::Graphics::Gdi::SRCCOPY,
            );
            x += draw_w;
        }

        let to_x = |t: i64| {
            strip.left
                + ((t as f64 / state.duration as f64) * (strip.right - strip.left) as f64) as i32
        };
        let (sx, ex) = (to_x(state.trim_start), to_x(state.trim_end));

        // Dim the trimmed-away ends with a 50% wash so frames stay visible.
        let shade = CreateSolidBrush(state.theme.bg);
        for r in [
            RECT {
                left: strip.left,
                top: strip.top,
                right: sx,
                bottom: strip.bottom,
            },
            RECT {
                left: ex,
                top: strip.top,
                right: strip.right,
                bottom: strip.bottom,
            },
        ] {
            if r.right > r.left {
                let bf = windows::Win32::Graphics::Gdi::BLENDFUNCTION {
                    BlendOp: 0,
                    BlendFlags: 0,
                    SourceConstantAlpha: 165,
                    AlphaFormat: 0,
                };
                let mem = windows::Win32::Graphics::Gdi::CreateCompatibleDC(hdc);
                let bmp = windows::Win32::Graphics::Gdi::CreateCompatibleBitmap(hdc, 1, 1);
                let old = SelectObject(mem, bmp);
                FillRect(
                    mem,
                    &RECT {
                        left: 0,
                        top: 0,
                        right: 1,
                        bottom: 1,
                    },
                    shade,
                );
                let _ = windows::Win32::Graphics::Gdi::AlphaBlend(
                    hdc,
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    mem,
                    0,
                    0,
                    1,
                    1,
                    bf,
                );
                SelectObject(mem, old);
                let _ = DeleteObject(bmp);
                let _ = windows::Win32::Graphics::Gdi::DeleteDC(mem);
            }
        }
        let _ = DeleteObject(shade);

        // Handles.
        let acc = CreateSolidBrush(state.theme.accent);
        let hw = s(state, 5);
        for hx in [sx, ex] {
            FillRect(
                hdc,
                &RECT {
                    left: hx - hw / 2,
                    top: strip.top - s(state, 3),
                    right: hx + hw / 2,
                    bottom: strip.bottom + s(state, 3),
                },
                acc,
            );
        }
        let _ = DeleteObject(acc);

        // Playhead sits above the trim range so scrubbing remains obvious.
        let px = to_x(state.playhead);
        let playhead = CreateSolidBrush(state.theme.text);
        FillRect(
            hdc,
            &RECT {
                left: px - 1,
                top: strip.top - s(state, 7),
                right: px + 1,
                bottom: strip.bottom + s(state, 5),
            },
            playhead,
        );
        let old_brush = SelectObject(hdc, playhead);
        let _ = RoundRect(
            hdc,
            px - s(state, 5),
            strip.top - s(state, 11),
            px + s(state, 5),
            strip.top - s(state, 3),
            s(state, 4),
            s(state, 4),
        );
        SelectObject(hdc, old_brush);
        let _ = DeleteObject(playhead);

        // Playhead + selected-range label.
        let whole_secs = |t: i64| (t / 10_000_000) as u64;
        let tenths = |t: i64| ((t.max(0) / 1_000_000) % 10) as u64;
        let label = format!(
            "{:02}:{:02}.{}     Trim  {:02}:{:02} \u{2013} {:02}:{:02}     {}s selected",
            whole_secs(state.playhead) / 60,
            whole_secs(state.playhead) % 60,
            tenths(state.playhead),
            whole_secs(state.trim_start) / 60,
            whole_secs(state.trim_start) % 60,
            whole_secs(state.trim_end) / 60,
            whole_secs(state.trim_end) % 60,
            whole_secs(state.trim_end - state.trim_start).max(1)
        );
        SelectObject(hdc, state.font_small);
        SetTextColor(hdc, state.theme.muted);
        let mut l = wide(&label);
        let mut lr = RECT {
            left: strip.left,
            top: strip.bottom + s(state, 6),
            right: strip.right,
            bottom: strip.bottom + s(state, 28),
        };
        DrawTextW(hdc, &mut l, &mut lr, DT_LEFT | DT_SINGLELINE | DT_VCENTER);

    }

    if let Some(msg) = &state.status {
        SelectObject(hdc, state.font_small);
        SetTextColor(hdc, state.theme.accent);
        let mut t = wide(msg);
        let mut r = RECT {
            left: state.width / 2,
            top: s(state, 44),
            right: state.width - s(state, 24),
            bottom: s(state, 66),
        };
        DrawTextW(
            hdc,
            &mut t,
            &mut r,
            DT_END_ELLIPSIS | DT_SINGLELINE | DT_VCENTER | windows::Win32::Graphics::Gdi::DT_RIGHT,
        );
    }

    for (i, (r, act, label)) in state.controls.iter().enumerate() {
        let hot = i as i32 == state.hover;
        let primary = i == 0;
        let fill = CreateSolidBrush(if primary {
            state.theme.accent
        } else {
            state.theme.chip
        });
        let pen = CreatePen(
            PS_SOLID,
            1,
            if primary {
                state.theme.accent
            } else {
                state.theme.chip_line
            },
        );
        let ob = SelectObject(hdc, fill);
        let op = SelectObject(hdc, pen);
        let _ = RoundRect(
            hdc,
            r.left,
            r.top,
            r.right,
            r.bottom,
            s(state, 10),
            s(state, 10),
        );
        SelectObject(hdc, ob);
        SelectObject(hdc, op);
        let _ = DeleteObject(fill);
        let _ = DeleteObject(pen);
        SelectObject(hdc, state.font);
        SetTextColor(
            hdc,
            if primary {
                state.theme.accent_text
            } else if hot {
                state.theme.text
            } else {
                state.theme.muted
            },
        );
        // Copy and Show in folder always act on the recorded original, never on
        // an edit export. Say so as soon as an edit exists, or the buttons look
        // like they hand you what the preview is showing.
        let original_only = state.exported.is_some() || has_edits(state);
        let shown = if *act == Act::SaveTrim && state.exporting {
            "Cancel export"
        } else if *act == Act::Play && state.playing {
            "Pause"
        } else if *act == Act::Reveal && original_only {
            "Show original"
        } else if *act == Act::Copy && original_only {
            "Copy original"
        } else if *act == Act::Share && state.sharing {
            "Sharing\u{2026}"
        } else if *act == Act::Share && original_only {
            "Share original"
        } else {
            label
        };
        let mut l = wide(shown);
        let mut lr = *r;
        DrawTextW(hdc, &mut l, &mut lr, DT_CENTER | DT_SINGLELINE | DT_VCENTER);
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
        WM_EXPORT_PROGRESS => {
            if let Some(state) = state_of(hwnd) {
                if state.exporting && state.export_id == Some(lparam.0 as u64) {
                    state.status = Some(format!("exporting full resolution · {}%", wparam.0));
                    let _ = InvalidateRect(hwnd, None, false);
                }
            }
            LRESULT(0)
        }
        WM_EXPORT_DONE => {
            if lparam.0 == 0 {
                return LRESULT(0);
            }
            let done = Box::from_raw(lparam.0 as *mut ExportDone);
            let mut close = false;
            if let Some(state) = state_of(hwnd) {
                if state.export_id == Some(done.id) {
                    state.exporting = false;
                    state.export_id = None;
                    state.export_cancel = None;
                    state.export_stalled = false;
                    match &done.result {
                        Ok(()) => {
                            crate::diagnostics::log("video export complete");
                            let name = done.path.file_name().unwrap_or_default().to_string_lossy();
                            state.status = Some(match crate::output::file_to_clipboard(&done.path) {
                                Ok(()) => format!("saved {name} · edit on clipboard"),
                                Err(error) => {
                                    crate::diagnostics::log("video export clipboard copy failed");
                                    eprintln!("video export clipboard copy failed: {error:#}");
                                    format!("saved {name} · clipboard unavailable")
                                }
                            });
                            state.exported = Some(done.path.clone());
                        }
                        Err(error) if error == "export cancelled" => {
                            crate::diagnostics::log("video export cancelled");
                            state.status = Some("export cancelled · original kept".into());
                        }
                        Err(error) => {
                            crate::diagnostics::log("video export failed");
                            state.status = Some(format!("export failed: {error}"));
                        }
                    }
                    close = state.close_after_export;
                    state.close_after_export = false;
                    let _ = InvalidateRect(hwnd, None, false);
                }
            }
            if close {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        crate::share::WM_SHARE_COMPLETE => {
            if lparam.0 == 0 {
                return LRESULT(0);
            }
            let outcome = *Box::from_raw(lparam.0 as *mut crate::share::ShareOutcome);
            if let Some(state) = state_of(hwnd) {
                state.sharing = false;
                let message = match outcome {
                    Ok(url) => {
                        // Opening the page is the visible confirmation that
                        // something happened; the clipboard copy alone was
                        // easy to miss entirely.
                        crate::output::open_url(&url);
                        match crate::output::text_to_clipboard(&url) {
                            Ok(()) => "link copied to clipboard".into(),
                            Err(error) => {
                                crate::diagnostics::log("share link clipboard copy failed");
                                eprintln!("share link clipboard copy failed: {error:#}");
                                format!("shared, but the link could not be copied: {url}")
                            }
                        }
                    }
                    Err(message) => {
                        crate::diagnostics::log("video share failed");
                        format!("could not share: {message}")
                    }
                };
                // An export's own progress ("exporting … 45%") is more
                // time-sensitive than the share result and still ticking
                // forward; do not stomp on it. The clipboard action above
                // still happens either way — only the status line waits.
                if !state.exporting {
                    state.status = Some(message);
                }
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_PROBE_READY => {
            if lparam.0 == 0 {
                return LRESULT(0);
            }
            let probe = Box::from_raw(lparam.0 as *mut crate::trim::Probe);
            if let Some(state) = state_of(hwnd) {
                if !probe.thumbs.is_empty() {
                    state.raw_thumbs = probe.thumbs;
                    refresh_matte_thumbs(state);
                }
                if !probe.previews.is_empty() {
                    state.scrub_previews = probe.previews;
                    // Only take the newly cached frame if the user is not
                    // mid-drag and playback is not driving the preview.
                    if state.dragging.is_none() && !state.playing {
                        refresh_preview(state);
                    }
                }
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_EXPORT_STALLED => {
            if let Some(state) = state_of(hwnd) {
                if state.exporting && state.export_id == Some(lparam.0 as u64) {
                    state.export_stalled = true;
                    state.status = Some(
                        "export stopped responding · cancelling safely · original kept".into(),
                    );
                    let _ = InvalidateRect(hwnd, None, false);
                }
            }
            LRESULT(0)
        }
        WM_SCRUB_FRAME => {
            if lparam.0 == 0 {
                return LRESULT(0);
            }
            let frame = Box::from_raw(lparam.0 as *mut (u64, Vec<u8>, u32, u32));
            if let Some(state) = state_of(hwnd) {
                // Playback owns the preview while it runs, and a frame for a
                // position the user has already scrubbed past is stale.
                if !state.playing && frame.0 == state.scrub_generation {
                    state.preview_raw = Some((frame.1, frame.2, frame.3));
                    recompose_preview(state);
                    let _ = InvalidateRect(hwnd, None, false);
                }
            }
            LRESULT(0)
        }
        WM_PLAYBACK_FRAME => {
            if let Some(state) = state_of(hwnd) {
                let next = {
                    let mut mailbox = state.playback_mailbox.lock().unwrap();
                    mailbox.frame_posted = false;
                    mailbox.frame.take()
                };
                if let Some((generation, frame)) = next {
                    if state.playing && generation == state.playback_generation {
                        state.playhead = frame.timestamp.clamp(state.trim_start, state.trim_end);
                        state.preview_raw = Some((frame.bytes, frame.width, frame.height));
                        recompose_preview(state);
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                }
            }
            LRESULT(0)
        }
        WM_PLAYBACK_DONE => {
            if let Some(state) = state_of(hwnd) {
                let done = state.playback_mailbox.lock().unwrap().done.take();
                if let Some((generation, result, cancelled)) = done {
                    if generation == state.playback_generation {
                        state.playing = false;
                        state.playback_cancel = None;
                        if !cancelled {
                            match result {
                                Ok(()) => {
                                    state.playhead = state.trim_end;
                                    refresh_preview_exact(state);
                                    state.status = None;
                                }
                                Err(error) => {
                                    state.status = Some(format!("playback failed: {error}"));
                                }
                            }
                        }
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                }
            }
            LRESULT(0)
        }
        WM_PAINT => {
            if let Some(state) = state_of(hwnd) {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                // Double-buffered: dragging the trim handles repaints the
                // whole filmstrip, which flickers when drawn to screen.
                let mem = windows::Win32::Graphics::Gdi::CreateCompatibleDC(hdc);
                let bmp = windows::Win32::Graphics::Gdi::CreateCompatibleBitmap(
                    hdc,
                    state.width,
                    state.height,
                );
                let old = SelectObject(mem, bmp);
                paint(mem, state);
                let _ = windows::Win32::Graphics::Gdi::BitBlt(
                    hdc,
                    0,
                    0,
                    state.width,
                    state.height,
                    mem,
                    0,
                    0,
                    windows::Win32::Graphics::Gdi::SRCCOPY,
                );
                SelectObject(mem, old);
                let _ = DeleteObject(bmp);
                let _ = windows::Win32::Graphics::Gdi::DeleteDC(mem);
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
                if let Some(drag) = state.dragging {
                    match drag {
                        Drag::Trim(handle) => {
                            set_handle(state, handle, x);
                            state.playhead = state.playhead.clamp(state.trim_start, state.trim_end);
                            refresh_preview(state);
                        }
                        Drag::Playhead => set_playhead(state, x),
                        Drag::Padding => update_padding(state, x),
                        Drag::CaptionSize => update_caption_size(state, x),
                        Drag::CaptionOpacity => update_caption_opacity(state, x),
                        Drag::Draw { index, start } => {
                            if let Some(point) = screen_to_preview(state, x, y) {
                                if let Some(item) = state.annotations.get_mut(index) {
                                    update_draw_shape(&mut item.shape, start, point);
                                }
                                recompose_preview(state);
                            }
                        }
                        Drag::Move {
                            index,
                            last,
                            undo_pushed,
                        } => {
                            if let Some(point) = screen_to_preview(state, x, y) {
                                let dx = point.0 - last.0;
                                let dy = point.1 - last.1;
                                let moved = dx.abs() > f32::EPSILON || dy.abs() > f32::EPSILON;
                                if moved && !undo_pushed {
                                    push_undo(state);
                                }
                                let content_size = annotation_content_size(state);
                                if let Some(item) = state.annotations.get_mut(index) {
                                    crate::video_edit::translate(
                                        item,
                                        dx,
                                        dy,
                                        content_size,
                                    );
                                }
                                state.dragging = Some(Drag::Move {
                                    index,
                                    last: point,
                                    undo_pushed: undo_pushed || moved,
                                });
                                recompose_preview(state);
                            }
                        }
                        Drag::Reshape {
                            index,
                            handle,
                            undo_pushed,
                        } => {
                            if let Some(point) = screen_to_preview(state, x, y) {
                                if !undo_pushed {
                                    push_undo(state);
                                }
                                if let Some(item) = state.annotations.get_mut(index) {
                                    crate::video_edit::set_handle(item, handle, point);
                                }
                                state.dragging = Some(Drag::Reshape {
                                    index,
                                    handle,
                                    undo_pushed: true,
                                });
                                recompose_preview(state);
                            }
                        }
                    }
                    let _ = InvalidateRect(hwnd, None, false);
                    return LRESULT(0);
                }
                let hover = state
                    .controls
                    .iter()
                    .position(|(r, ..)| x >= r.left && x < r.right && y >= r.top && y < r.bottom)
                    .map(|i| i as i32)
                    .unwrap_or(-1);
                if hover != state.hover {
                    state.hover = hover;
                    let _ = InvalidateRect(hwnd, None, false);
                }
            }
            LRESULT(0)
        }
        windows::Win32::UI::WindowsAndMessaging::WM_LBUTTONDOWN => {
            if let Some(state) = state_of(hwnd) {
                // The editor owns all keyboard-driven caption entry. Restore
                // focus explicitly on every click so tray/recording teardown
                // or another foreground transition cannot leave a visible
                // insertion caret that receives no characters.
                let _ = SetFocus(hwnd);
                let (x, y) = (
                    (lparam.0 & 0xFFFF) as i16 as i32,
                    ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
                );
                // Clicking anywhere accepts the current caption, returns to
                // Select, and then continues handling that same click. Enter
                // remains a convenient shortcut, never a requirement.
                if state.text_entry.is_some() {
                    commit_text(state);
                }
                if contains(state.padding_slider, x, y) {
                    state.dragging = Some(Drag::Padding);
                    update_padding(state, x);
                    windows::Win32::UI::Input::KeyboardAndMouse::SetCapture(hwnd);
                    let _ = InvalidateRect(hwnd, None, false);
                    return LRESULT(0);
                }
                if contains(state.add_control, x, y) {
                    stop_playback(state);
                    state.tools_open = !state.tools_open;
                    if !state.tools_open {
                        state.tool = None;
                        state.text_entry = None;
                        recompose_preview(state);
                    }
                    let _ = InvalidateRect(hwnd, None, false);
                    return LRESULT(0);
                }
                if state.tools_open {
                    if let Some((_, tool, _)) = state
                        .tool_controls
                        .iter()
                        .find(|(rect, ..)| contains(*rect, x, y))
                        .copied()
                    {
                        select_annotation_tool(state, tool);
                        let _ = InvalidateRect(hwnd, None, false);
                        return LRESULT(0);
                    }
                    if let Some((_, index)) = state
                        .color_controls
                        .iter()
                        .find(|(rect, _)| contains(*rect, x, y))
                        .copied()
                    {
                        stop_playback(state);
                        state.color_idx = index;
                        if let Some(selected) = state
                            .selected
                            .filter(|index| *index < state.annotations.len())
                        {
                            push_undo(state);
                            state.annotations[selected].color = index;
                            recompose_preview(state);
                        }
                        let _ = InvalidateRect(hwnd, None, false);
                        return LRESULT(0);
                    }
                    if caption_controls_active(state)
                        && contains(state.caption_size_slider, x, y)
                    {
                        stop_playback(state);
                        if state.selected.is_some() {
                            push_undo(state);
                        }
                        state.dragging = Some(Drag::CaptionSize);
                        update_caption_size(state, x);
                        windows::Win32::UI::Input::KeyboardAndMouse::SetCapture(hwnd);
                        let _ = InvalidateRect(hwnd, None, false);
                        return LRESULT(0);
                    }
                    if caption_controls_active(state)
                        && state.caption_style == crate::video_edit::CaptionStyle::Box
                        && contains(state.caption_opacity_slider, x, y)
                    {
                        stop_playback(state);
                        if state.selected.is_some() {
                            push_undo(state);
                        }
                        state.dragging = Some(Drag::CaptionOpacity);
                        update_caption_opacity(state, x);
                        windows::Win32::UI::Input::KeyboardAndMouse::SetCapture(hwnd);
                        let _ = InvalidateRect(hwnd, None, false);
                        return LRESULT(0);
                    }
                    if caption_controls_active(state) {
                        if let Some((_, style)) = state
                            .caption_style_controls
                            .iter()
                            .find(|(rect, _)| contains(*rect, x, y))
                            .copied()
                        {
                            stop_playback(state);
                            state.caption_style = style;
                            if let Some(selected) = state
                                .selected
                                .filter(|index| *index < state.annotations.len())
                            {
                                push_undo(state);
                                state.annotations[selected].caption_style = style;
                            }
                            recompose_preview(state);
                            let _ = InvalidateRect(hwnd, None, false);
                            return LRESULT(0);
                        }
                    }
                    if state.selected.is_some() {
                        if let Some((_, choice)) = state
                            .timing_controls
                            .iter()
                            .find(|(rect, _)| contains(*rect, x, y))
                            .copied()
                        {
                            stop_playback(state);
                            apply_timing(state, choice);
                            let _ = InvalidateRect(hwnd, None, false);
                            return LRESULT(0);
                        }
                    }
                    if !caption_controls_active(state) {
                        if let Some((_, index)) = state
                            .size_controls
                            .iter()
                            .find(|(rect, _)| contains(*rect, x, y))
                            .copied()
                        {
                            stop_playback(state);
                            state.size_idx = index;
                            if let Some(selected) = state
                                .selected
                                .filter(|index| *index < state.annotations.len())
                            {
                                push_undo(state);
                                state.annotations[selected].size = current_size(state);
                                recompose_preview(state);
                            }
                            let _ = InvalidateRect(hwnd, None, false);
                            return LRESULT(0);
                        }
                    }
                    if contains(state.undo_control, x, y) {
                        stop_playback(state);
                        undo(state);
                        let _ = InvalidateRect(hwnd, None, false);
                        return LRESULT(0);
                    }
                    if contains(state.delete_control, x, y) {
                        stop_playback(state);
                        delete_selected(state);
                        let _ = InvalidateRect(hwnd, None, false);
                        return LRESULT(0);
                    }
                    if contains(tool_panel(state), x, y) {
                        return LRESULT(0);
                    }
                }

                if let Some(point) = screen_to_preview(state, x, y) {
                    stop_playback(state);
                    if let Some(tool) = state.tool {
                        if tool == Tool::Text {
                            state.text_entry = Some(TextEntry {
                                pos: point,
                                text: String::new(),
                                editing: None,
                            });
                            state.tools_open = false;
                            state.selected = None;
                            recompose_preview(state);
                        } else if tool == Tool::Counter {
                            push_undo(state);
                            let (start, end) = default_range(state);
                            let n = next_counter_number(&state.annotations);
                            state.annotations.push(crate::video_edit::Item {
                                shape: crate::video_edit::Shape::Counter { pos: point, n },
                                start,
                                end,
                                color: state.color_idx,
                                size: current_size(state),
                                caption_style: crate::video_edit::CaptionStyle::Shadow,
                                caption_box_opacity: state.caption_box_opacity,
                            });
                            state.selected = Some(state.annotations.len() - 1);
                            state.tools_open = false;
                            recompose_preview(state);
                        } else {
                            push_undo(state);
                            let (start, end) = default_range(state);
                            state.annotations.push(crate::video_edit::Item {
                                shape: shape_for_tool(tool, point),
                                start,
                                end,
                                color: state.color_idx,
                                size: current_size(state),
                                caption_style: crate::video_edit::CaptionStyle::Shadow,
                                caption_box_opacity: state.caption_box_opacity,
                            });
                            let index = state.annotations.len() - 1;
                            state.selected = Some(index);
                            state.tools_open = false;
                            state.dragging = Some(Drag::Draw { index, start: point });
                            windows::Win32::UI::Input::KeyboardAndMouse::SetCapture(hwnd);
                            recompose_preview(state);
                        }
                        let _ = InvalidateRect(hwnd, None, false);
                        return LRESULT(0);
                    }
                    if let Some((index, handle)) = hit_annotation_handle(state, point) {
                        state.selected = Some(index);
                        sync_selected_controls(state, index);
                        state.dragging = Some(Drag::Reshape {
                            index,
                            handle,
                            undo_pushed: false,
                        });
                        windows::Win32::UI::Input::KeyboardAndMouse::SetCapture(hwnd);
                    } else if let Some(index) = hit_annotation(state, point) {
                        state.selected = Some(index);
                        sync_selected_controls(state, index);
                        state.dragging = Some(Drag::Move {
                            index,
                            last: point,
                            undo_pushed: false,
                        });
                        windows::Win32::UI::Input::KeyboardAndMouse::SetCapture(hwnd);
                    } else {
                        state.selected = None;
                    }
                    let _ = InvalidateRect(hwnd, None, false);
                    return LRESULT(0);
                }

                let strip = state.strip;
                if state.duration > 0
                    && y >= strip.top - 6
                    && y <= strip.bottom + 6
                    && x >= strip.left - 8
                    && x <= strip.right + 8
                {
                    state.resume_after_drag = state.playing;
                    if state.playing {
                        stop_playback(state);
                    }
                    let to_x = |t: i64| {
                        strip.left
                            + ((t as f64 / state.duration as f64)
                                * (strip.right - strip.left) as f64)
                                as i32
                    };
                    let (sx, ex) = (to_x(state.trim_start), to_x(state.trim_end));
                    let grab = s(state, 10);
                    let drag = if (x - sx).abs() <= grab {
                        Drag::Trim(Handle::Start)
                    } else if (x - ex).abs() <= grab {
                        Drag::Trim(Handle::End)
                    } else {
                        Drag::Playhead
                    };
                    state.dragging = Some(drag);
                    match drag {
                        Drag::Trim(handle) => {
                            set_handle(state, handle, x);
                            state.playhead = state.playhead.clamp(state.trim_start, state.trim_end);
                            refresh_preview(state);
                        }
                        Drag::Playhead => set_playhead(state, x),
                        _ => {}
                    }
                    windows::Win32::UI::Input::KeyboardAndMouse::SetCapture(hwnd);
                    let _ = InvalidateRect(hwnd, None, false);
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
                if state.text_entry.is_some() {
                    commit_text(state);
                }
                if let Some(point) = screen_to_preview(state, x, y) {
                    if let Some(index) = hit_annotation(state, point) {
                        stop_playback(state);
                        state.selected = Some(index);
                        sync_selected_controls(state, index);
                        state.tool = None;
                        state.tools_open = true;
                        state.status = None;
                        recompose_preview(state);
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                }
            }
            LRESULT(0)
        }
        WM_RBUTTONUP | WM_CONTEXTMENU => LRESULT(0),
        WM_LBUTTONUP => {
            if let Some(state) = state_of(hwnd) {
                let (x, y) = (
                    (lparam.0 & 0xFFFF) as i16 as i32,
                    ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
                );
                if let Some(drag) = state.dragging.take() {
                    let _ = windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture();
                    if matches!(drag, Drag::Trim(_) | Drag::Playhead) {
                        // The decoder is already chasing this position, so
                        // releasing no longer blocks on a fresh seek.
                        request_scrub_frame(state);
                        if state.resume_after_drag {
                            state.resume_after_drag = false;
                            start_playback(hwnd, state);
                        }
                    } else {
                        if drag == Drag::Padding {
                            refresh_matte_thumbs(state);
                            // `dragging` is already cleared, so this recomposes
                            // at full resolution.
                            recompose_preview(state);
                        }
                        // The tool is still armed here: releasing the mouse
                        // ends the shape, not the tool. Only a stray click
                        // that drew nothing is thrown away.
                        if let Drag::Draw { index, .. } = drag {
                            if let Some(item) = state.annotations.get(index) {
                                if draw_shape_is_degenerate(&item.shape) {
                                    state.annotations.remove(index);
                                    state.selected = None;
                                }
                            }
                        }
                        recompose_preview(state);
                    }
                    let _ = InvalidateRect(hwnd, None, false);
                    return LRESULT(0);
                }
                if let Some((_, index)) = state
                    .matte_controls
                    .iter()
                    .find(|(r, _)| x >= r.left && x < r.right && y >= r.top && y < r.bottom)
                    .copied()
                {
                    if index != state.matte_index {
                        state.matte_index = index;
                        refresh_matte_thumbs(state);
                        recompose_preview(state);
                        state.status = None;
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                    return LRESULT(0);
                }
                if let Some((_, index)) = state
                    .aspect_controls
                    .iter()
                    .find(|(r, _)| contains(*r, x, y))
                    .copied()
                {
                    if index != state.aspect_idx {
                        state.aspect_idx = index;
                        refresh_matte_thumbs(state);
                        recompose_preview(state);
                        state.status = None;
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                    return LRESULT(0);
                }
                if let Some(i) = state
                    .controls
                    .iter()
                    .position(|(r, ..)| x >= r.left && x < r.right && y >= r.top && y < r.bottom)
                {
                    match state.controls[i].1 {
                        Act::Play => toggle_playback(hwnd, state),
                        Act::Reveal => crate::output::reveal_in_explorer(&state.mp4),
                        Act::Copy => {
                            state.status = Some(match crate::output::file_to_clipboard(&state.mp4) {
                                Ok(()) => "original copied to clipboard".into(),
                                Err(error) => {
                                    crate::diagnostics::log("video clipboard copy failed");
                                    eprintln!("video clipboard copy failed: {error:#}");
                                    "could not copy original to the clipboard".into()
                                }
                            });
                            let _ = InvalidateRect(hwnd, None, false);
                        }
                        // Shares the original recording, same as Copy and Show
                        // in folder always act on the original rather than an
                        // edit export — the recorded file is what these three
                        // buttons agree the "real" artifact is.
                        Act::Share => {
                            // A second click while one upload is already in
                            // flight would start a redundant upload and let
                            // whichever WM_SHARE_COMPLETE lands last silently
                            // win. A dedicated flag rather than checking
                            // `status` directly: that string gets overwritten
                            // by unrelated handlers while the upload runs.
                            if !state.sharing {
                                state.sharing = true;
                                state.status = Some("Sharing\u{2026}".into());
                                let _ = InvalidateRect(hwnd, None, false);
                                crate::share::share_in_background(hwnd, state.mp4.clone());
                            }
                        }
                        Act::Delete => {
                            if state.exporting {
                                state.status =
                                    Some("finish the current export before deleting".into());
                                let _ = InvalidateRect(hwnd, None, false);
                                return LRESULT(0);
                            }
                            let prompt = HSTRING::from(recording_delete_prompt(&state.mp4));
                            let confirmed = MessageBoxW(
                                hwnd,
                                PCWSTR(prompt.as_ptr()),
                                w!("Matteshot"),
                                MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2,
                            ) == IDYES;
                            if !confirmed {
                                return LRESULT(0);
                            }
                            stop_playback(state);
                            if let Err(error) = delete_recording_files(
                                &state.mp4,
                                state.gif.as_deref(),
                            ) {
                                crate::diagnostics::log(&format!(
                                    "recording could not be deleted: {error:#}"
                                ));
                                state.status = Some("could not delete recording · original kept".into());
                                let _ = InvalidateRect(hwnd, None, false);
                                return LRESULT(0);
                            }
                            let _ = DestroyWindow(hwnd);
                        }
                        Act::SaveTrim => {
                            if state.exporting {
                                if let Some(cancel) = &state.export_cancel {
                                    cancel.store(true, Ordering::Relaxed);
                                }
                                state.status = Some("cancelling export · original kept".into());
                                crate::diagnostics::log("video export cancel requested");
                                let _ = InvalidateRect(hwnd, None, false);
                                return LRESULT(0);
                            }
                            stop_playback(state);
                            let style = state.styles[state.matte_index].clone();
                            let has_matte = !crate::compose::is_plain(&style);
                            let has_trim = state.trim_start > 0 || state.trim_end < state.duration;
                            let style_slug = style.name.to_ascii_lowercase();
                            let has_annotations = !state.annotations.is_empty();
                            let mut suffix = match (has_matte, has_trim) {
                                (true, true) => format!("{style_slug}-trim"),
                                (true, false) => style_slug,
                                (false, true) => "trim".into(),
                                (false, false) => "copy".into(),
                            };
                            if has_annotations {
                                suffix.push_str("-edit");
                            }
                            let dst = available_export_path(state.mp4.with_file_name(format!(
                                "{}-{suffix}.mp4",
                                state
                                    .mp4
                                    .file_stem()
                                    .map(|s| s.to_string_lossy().to_string())
                                    .unwrap_or_default()
                            )));
                            let annotation_label = if has_annotations {
                                format!(
                                    " + {} annotation{}",
                                    state.annotations.len(),
                                    if state.annotations.len() == 1 {
                                        ""
                                    } else {
                                        "s"
                                    }
                                )
                            } else {
                                String::new()
                            };
                            state.status = Some(format!(
                                "exporting {}{}{} \u{00b7} 0%",
                                style.name,
                                if has_trim { " + trim" } else { "" },
                                annotation_label
                            ));
                            state.exporting = true;
                            crate::diagnostics::log("video export start");
                            let export_id = NEXT_EXPORT_ID.fetch_add(1, Ordering::Relaxed);
                            let export_cancel = Arc::new(AtomicBool::new(false));
                            state.export_id = Some(export_id);
                            state.export_cancel = Some(export_cancel.clone());
                            state.close_after_export = false;
                            state.export_stalled = false;
                            let _ = InvalidateRect(hwnd, None, false);
                            let _ = windows::Win32::Graphics::Gdi::UpdateWindow(hwnd);
                            let src = state.mp4.clone();
                            let start = state.trim_start;
                            let end = state.trim_end;
                            let annotations = state.annotations.clone();
                            let compose_opts = state_compose_opts(state);
                            let hwnd_raw = hwnd.0 as isize;
                            let activity = Arc::new(Mutex::new(std::time::Instant::now()));
                            let finished = Arc::new(AtomicBool::new(false));
                            {
                                let activity = activity.clone();
                                let finished = finished.clone();
                                let cancel = export_cancel.clone();
                                std::thread::spawn(move || loop {
                                    std::thread::sleep(std::time::Duration::from_secs(1));
                                    if finished.load(Ordering::Relaxed) {
                                        break;
                                    }
                                    if activity.lock().unwrap().elapsed()
                                        >= std::time::Duration::from_secs(45)
                                    {
                                        cancel.store(true, Ordering::Relaxed);
                                        crate::diagnostics::log("video export inactivity timeout");
                                        unsafe {
                                            let _ = PostMessageW(
                                                HWND(hwnd_raw as *mut _),
                                                WM_EXPORT_STALLED,
                                                WPARAM(0),
                                                LPARAM(export_id as isize),
                                            );
                                        }
                                        break;
                                    }
                                });
                            }
                            std::thread::spawn(move || {
                                let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
                                let temporary = crate::output::partial_video_path(&dst, export_id);
                                let _ = std::fs::remove_file(&temporary);
                                let mut last_progress = u32::MAX;
                                let result = crate::trim::cut_with_edit_progress_cancel(
                                    &src,
                                    &temporary,
                                    start,
                                    end,
                                    Some((&style, &compose_opts)),
                                    &annotations,
                                    &export_cancel,
                                    |percent| {
                                        *activity.lock().unwrap() = std::time::Instant::now();
                                        if percent != last_progress {
                                            last_progress = percent;
                                            unsafe {
                                                let _ = PostMessageW(
                                                    HWND(hwnd_raw as *mut _),
                                                    WM_EXPORT_PROGRESS,
                                                    WPARAM(percent as usize),
                                                    LPARAM(export_id as isize),
                                                );
                                            }
                                        }
                                    },
                                )
                                .map_err(|error| format!("{error:#}"))
                                .and_then(|()| {
                                    if export_cancel.load(Ordering::Relaxed) {
                                        Err("export cancelled".into())
                                    } else {
                                        crate::trim::validate_video(&temporary)
                                            .map_err(|error| format!("validate export: {error:#}"))?;
                                        std::fs::rename(&temporary, &dst)
                                            .map_err(|error| format!("finalize export: {error}"))
                                    }
                                });
                                if result.is_err() {
                                    let _ = std::fs::remove_file(&temporary);
                                }
                                if com.is_ok() {
                                    unsafe { CoUninitialize() };
                                }
                                finished.store(true, Ordering::Relaxed);
                                let done = Box::new(ExportDone {
                                    id: export_id,
                                    path: dst,
                                    result,
                                });
                                let done_ptr = Box::into_raw(done);
                                unsafe {
                                    let target = HWND(hwnd_raw as *mut _);
                                    if !crate::window::has_class(target, "matteshot_recdone")
                                        || PostMessageW(
                                            target,
                                            WM_EXPORT_DONE,
                                            WPARAM(0),
                                            LPARAM(done_ptr as isize),
                                        )
                                        .is_err()
                                    {
                                        drop(Box::from_raw(done_ptr));
                                    }
                                }
                            });
                        }
                    }
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONDBLCLK => {
            if let Some(state) = state_of(hwnd) {
                let _ = SetFocus(hwnd);
                state.dragging = None;
                let _ = windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture();
                let (x, y) = (
                    (lparam.0 & 0xFFFF) as i16 as i32,
                    ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
                );
                if let Some(point) = screen_to_preview(state, x, y) {
                    if let Some(index) = hit_annotation(state, point) {
                        if let crate::video_edit::Shape::Text { pos, text } =
                            &state.annotations[index].shape
                        {
                            let pos = *pos;
                            let text = text.clone();
                            state.color_idx = state.annotations[index].color;
                            state.caption_size = state.annotations[index].size;
                            state.caption_style = state.annotations[index].caption_style;
                            state.caption_box_opacity =
                                state.annotations[index].caption_box_opacity;
                            state.selected = Some(index);
                            state.text_entry = Some(TextEntry {
                                pos,
                                text,
                                editing: Some(index),
                            });
                            recompose_preview(state);
                            let _ = InvalidateRect(hwnd, None, false);
                        }
                    }
                }
            }
            LRESULT(0)
        }
        WM_CHAR | WM_UNICHAR_MESSAGE => {
            if let Some(state) = state_of(hwnd) {
                if msg == WM_UNICHAR_MESSAGE && wparam.0 == UNICODE_NOCHAR {
                    // Advertise support for full Unicode code points. Normal
                    // keyboard input still arrives through WM_CHAR.
                    return LRESULT(1);
                }
                if state.text_entry.is_some() {
                    let ch = char::from_u32(wparam.0 as u32).unwrap_or('\0');
                    let action = state
                        .text_entry
                        .as_mut()
                        .map(|entry| apply_caption_input(&mut entry.text, ch))
                        .unwrap_or(CaptionInput::Ignored);
                    match action {
                        CaptionInput::Commit => commit_text(state),
                        CaptionInput::Changed => recompose_preview(state),
                        CaptionInput::Ignored => {}
                    }
                    let _ = InvalidateRect(hwnd, None, false);
                }
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            if let Some(state) = state_of(hwnd) {
                match wparam.0 as u16 {
                    key if key == VK_ESCAPE.0 => {
                        if state.playing {
                            stop_playback(state);
                            state.status = Some("paused".into());
                            let _ = InvalidateRect(hwnd, None, false);
                        } else if state.text_entry.is_some() {
                            state.text_entry = None;
                            recompose_preview(state);
                            let _ = InvalidateRect(hwnd, None, false);
                        } else if state.tool.is_some() || state.tools_open {
                            state.tool = None;
                            state.tools_open = false;
                            let _ = InvalidateRect(hwnd, None, false);
                        } else if state.selected.is_some() {
                            state.selected = None;
                            let _ = InvalidateRect(hwnd, None, false);
                        } else {
                            let _ = DestroyWindow(hwnd);
                        }
                    }
                    key if key == VK_SPACE.0 => {
                        if state.text_entry.is_none() {
                            toggle_playback(hwnd, state);
                        }
                    }
                    key if key == VK_DELETE.0 => {
                        stop_playback(state);
                        delete_selected(state);
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                    0x5A if GetKeyState(VK_CONTROL.0 as i32) < 0 => {
                        stop_playback(state);
                        undo(state);
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                    0x41 if state.text_entry.is_none() => {
                        state.tools_open = true;
                        select_annotation_tool(state, Tool::Arrow);
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                    0x52 if state.text_entry.is_none() => {
                        state.tools_open = true;
                        select_annotation_tool(state, Tool::Rect);
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                    0x54 if state.text_entry.is_none() => {
                        state.tools_open = true;
                        select_annotation_tool(state, Tool::Text);
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                    0x42 if state.text_entry.is_none() => {
                        state.tools_open = true;
                        select_annotation_tool(state, Tool::Blur);
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                    0x50 if state.text_entry.is_none() => {
                        state.tools_open = true;
                        select_annotation_tool(state, Tool::Pen);
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                    key if key == VK_LEFT.0 => {
                        let resume = state.playing;
                        stop_playback(state);
                        state.playhead = (state.playhead - 5_000_000).max(state.trim_start);
                        refresh_preview(state);
                        request_scrub_frame(state);
                        if resume {
                            start_playback(hwnd, state);
                        }
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                    key if key == VK_RIGHT.0 => {
                        let resume = state.playing;
                        stop_playback(state);
                        state.playhead = (state.playhead + 5_000_000).min(state.trim_end);
                        refresh_preview(state);
                        request_scrub_frame(state);
                        if resume && state.playhead < state.trim_end {
                            start_playback(hwnd, state);
                        }
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                    key if key == VK_HOME.0 => {
                        let resume = state.playing;
                        stop_playback(state);
                        state.playhead = state.trim_start;
                        refresh_preview(state);
                        request_scrub_frame(state);
                        if resume {
                            start_playback(hwnd, state);
                        }
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                    key if key == VK_END.0 => {
                        stop_playback(state);
                        state.playhead = state.trim_end;
                        refresh_preview(state);
                        request_scrub_frame(state);
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                    _ => {}
                }
            }
            LRESULT(0)
        }
        windows::Win32::UI::WindowsAndMessaging::WM_SIZE => {
            if let Some(state) = state_of(hwnd) {
                let (w, h) = (
                    (lparam.0 & 0xFFFF) as i32,
                    ((lparam.0 >> 16) & 0xFFFF) as i32,
                );
                if w > 0 && h > 0 {
                    let (minimum_w, minimum_h) = minimum_client_size(state.scale);
                    if w < minimum_w || h < minimum_h {
                        // Track-size limits only constrain mouse resizing. A
                        // stale restore placement or programmatic resize can
                        // still deliver a tiny client. Repair it immediately
                        // instead of painting an inverted preview and a clipped
                        // filmstrip.
                        let outer = crate::dpi::outer_bounds(
                            RECT {
                                left: 0,
                                top: 0,
                                right: w.max(minimum_w),
                                bottom: h.max(minimum_h),
                            },
                            editor_style(),
                            WS_EX_APPWINDOW,
                            crate::dpi::scale_for_window(hwnd),
                        );
                        let _ = SetWindowPos(
                            hwnd,
                            None,
                            0,
                            0,
                            outer.right - outer.left,
                            outer.bottom - outer.top,
                            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                        );
                        return LRESULT(0);
                    }
                    state.width = w;
                    state.height = h;
                    let next = layout(state.scale, w, h, state.styles.len());
                    state.controls = next.controls;
                    state.matte_controls = next.matte_controls;
                    state.padding_slider = next.padding_slider;
                    state.aspect_controls = next.aspect_controls;
                    state.add_control = next.add_control;
                    state.tool_controls = next.tool_controls;
                    state.color_controls = next.color_controls;
                    state.size_controls = next.size_controls;
                    state.caption_size_slider = next.caption_size_slider;
                    state.caption_opacity_slider = next.caption_opacity_slider;
                    state.caption_style_controls = next.caption_style_controls;
                    state.timing_controls = next.timing_controls;
                    state.undo_control = next.undo_control;
                    state.delete_control = next.delete_control;
                    state.preview_rect = next.preview;
                    state.strip = next.strip;
                    let _ = InvalidateRect(hwnd, None, true);
                }
            }
            LRESULT(0)
        }
        windows::Win32::UI::WindowsAndMessaging::WM_GETMINMAXINFO => {
            let mmi = lparam.0 as *mut windows::Win32::UI::WindowsAndMessaging::MINMAXINFO;
            if !mmi.is_null() {
                let s = GetDpiForWindow(hwnd).max(96) as f32 / 96.0;
                let (minimum_w, minimum_h) = minimum_client_size(s);
                let outer = crate::dpi::outer_bounds(
                    RECT { left: 0, top: 0, right: minimum_w, bottom: minimum_h },
                    editor_style(),
                    WS_EX_APPWINDOW,
                    s,
                );
                (*mmi).ptMinTrackSize.x = outer.right - outer.left;
                (*mmi).ptMinTrackSize.y = outer.bottom - outer.top;
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            if let Some(state) = state_of(hwnd) {
                if state.exporting {
                    if state.export_stalled {
                        let answer = MessageBoxW(
                            hwnd,
                            w!("The export stopped responding and cancellation was requested.\n\nClose the editor now? Your original is safe, and the incomplete export will be removed after restart."),
                            w!("Matteshot"),
                            MB_YESNO | MB_ICONWARNING,
                        );
                        if answer == IDYES {
                            if let Some(cancel) = &state.export_cancel {
                                cancel.store(true, Ordering::Relaxed);
                            }
                            let _ = DestroyWindow(hwnd);
                        }
                        return LRESULT(0);
                    }
                    // MessageBox runs a nested message loop. The export can
                    // finish while the confirmation is open, and
                    // WM_EXPORT_DONE will then clear the active export before
                    // MessageBox returns. Remember which export prompted the
                    // dialog and re-check it afterwards so a late Yes closes
                    // immediately instead of waiting forever for a second
                    // completion message that will never arrive.
                    let prompted_export = state.export_id;
                    let answer = MessageBoxW(
                        hwnd,
                        w!("An export is still running.\n\nCancel it and close the editor after cleanup finishes?"),
                        w!("Matteshot"),
                        MB_YESNO | MB_ICONWARNING,
                    );
                    if answer == IDYES {
                        if !state.exporting || state.export_id != prompted_export {
                            stop_playback(state);
                            let _ = DestroyWindow(hwnd);
                            return LRESULT(0);
                        }
                        if let Some(cancel) = &state.export_cancel {
                            cancel.store(true, Ordering::Relaxed);
                        }
                        state.close_after_export = true;
                        state.status = Some("cancelling export · closing after cleanup".into());
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                    return LRESULT(0);
                }
                stop_playback(state);
            }
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_NCDESTROY => {
            let ptr = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) as *mut State;
            if !ptr.is_null() {
                let state = Box::from_raw(ptr);
                // The export already put itself on the clipboard, so Explorer
                // only earns a window once the editor is done. Show the edit
                // there rather than leaving the user to hunt for it.
                if let Some(exported) = &state.exported {
                    if exported.exists() {
                        crate::output::reveal_in_explorer(exported);
                    }
                }
                if let Some(cancel) = &state.export_cancel {
                    cancel.store(true, Ordering::Relaxed);
                }
                if let Some(cancel) = &state.playback_cancel {
                    cancel.store(true, Ordering::Relaxed);
                }
                let _ = DeleteObject(state.font);
                let _ = DeleteObject(state.font_small);
                let _ = DeleteObject(state.font_big);
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Show the review window. Returns immediately; lives on the main loop.
pub fn show(
    mp4: PathBuf,
    gif: Option<PathBuf>,
    frames: u32,
    secs: u64,
    initial_status: Option<String>,
) -> Result<()> {
    if let Some(parent) = mp4.parent() {
        crate::output::cleanup_stale_video_partials(parent);
    }
    let mut cursor = POINT::default();
    unsafe { let _ = GetCursorPos(&mut cursor); }
    // Opens on the cursor's monitor, so that is the scale its chrome uses.
    let scale = crate::dpi::scale_for_point(cursor);
    let sc = |v: i32| (v as f32 * scale) as i32;
    let mut monitor_info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe {
        let _ = GetCursorPos(&mut cursor);
        let monitor = MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST);
        let _ = GetMonitorInfoW(monitor, &mut monitor_info);
    }
    let work_w = monitor_info.rcWork.right - monitor_info.rcWork.left;
    let work_h = monitor_info.rcWork.bottom - monitor_info.rcWork.top;
    let cw = ((work_w as f32 * 0.85) as i32)
        .clamp(sc(760).min(work_w - sc(40)), work_w - sc(40));
    let ch = ((work_h as f32 * 0.85) as i32)
        .clamp(sc(560).min(work_h - sc(40)), work_h - sc(40));
    let window_x = monitor_info.rcWork.left + (work_w - cw) / 2;
    let window_y = monitor_info.rcWork.top + (work_h - ch) / 2;

    // Filmstrip: best-effort, never blocks showing the window.
    let (probe_w, probe_h, preview_w, preview_h) = {
        let initial = layout(scale, cw, ch, 7);
        (
            (initial.strip.right - initial.strip.left) as u32,
            (initial.strip.bottom - initial.strip.top) as u32,
            (initial.preview.right - initial.preview.left).max(2) as u32,
            (initial.preview.bottom - initial.preview.top).max(2) as u32,
        )
    };
    // Open on one decoded frame. The filmstrip and the scrub cache are dozens
    // of seeks and arrive on a worker thread.
    let opening = crate::trim::probe_opening(&mp4, preview_w, preview_h)
        .context("open the finished recording in the editor")?;
    if opening.duration_100ns <= 0 {
        anyhow::bail!("the finished recording has no decodable video frames");
    }
    let duration = opening.duration_100ns;
    let source_size = opening.source_size;
    let scrub_previews = vec![opening.first];
    let raw_thumbs: Vec<(Vec<u8>, u32, u32)> = Vec::new();
    let style_source = scrub_previews
        .first()
        .map(|(bytes, w, h)| thumb_image(bytes, *w, *h))
        .unwrap_or_else(|| RgbaImage::from_pixel(1, 1, image::Rgba([42, 46, 58, 255])));
    let styles = crate::style::variants(&style_source);
    let matte_index = 0;
    let pad_factor = crate::compose::DEFAULT_PAD_FACTOR;
    let aspect_idx = 0;
    let opts = compose_opts(pad_factor, aspect_idx);
    let thumbs = matte_thumbs(&raw_thumbs, &styles[matte_index], &opts);

    let size_mb = std::fs::metadata(&mp4)
        .map(|m| m.len() as f64 / 1_048_576.0)
        .unwrap_or(0.0);
    let mut summary = format!(
        "{:02}:{:02}   \u{00b7}   {} frames   \u{00b7}   {:.1} MB",
        secs / 60,
        secs % 60,
        frames,
        size_mb
    );
    if gif.is_some() {
        summary.push_str("   \u{00b7}   + GIF");
    }

    let initial = layout(scale, cw, ch, styles.len());
    // The frame the opening probe already decoded; re-decoding it here cost
    // another seek for the same picture.
    let preview_raw = scrub_previews.first().cloned();
    // Build the opening still through the same cached composition path used
    // by playback frames. Two subtly different paths made the first frame
    // change shape as soon as Play delivered its first decoded frame.
    let mut state = Box::new(State {
        mp4,
        gif,
        summary,
        controls: initial.controls,
        hover: -1,
        theme: crate::theme::current(),
        font: unsafe { make_font(-sc(14), 400) },
        font_small: unsafe { make_font(-sc(12), 400) },
        font_big: unsafe { make_font(-sc(17), 600) },
        scale,
        width: cw,
        height: ch,
        duration,
        raw_thumbs,
        scrub_previews,
        source_size,
        thumbs,
        preview_raw,
        preview: None,
        preview_base_cache: None,
        preview_rect: initial.preview,
        strip: initial.strip,
        trim_start: 0,
        trim_end: duration,
        playhead: 0,
        dragging: None,
        styles,
        matte_index,
        matte_controls: initial.matte_controls,
        pad_factor,
        aspect_idx,
        padding_slider: initial.padding_slider,
        aspect_controls: initial.aspect_controls,
        annotations: Vec::new(),
        undo: Vec::new(),
        selected: None,
        tools_open: false,
        tool: None,
        color_idx: 0,
        size_idx: 1,
        caption_size: 1.75,
        caption_style: crate::video_edit::CaptionStyle::Box,
        caption_box_opacity: 0.68,
        text_entry: None,
        add_control: initial.add_control,
        tool_controls: initial.tool_controls,
        color_controls: initial.color_controls,
        size_controls: initial.size_controls,
        caption_size_slider: initial.caption_size_slider,
        caption_opacity_slider: initial.caption_opacity_slider,
        caption_style_controls: initial.caption_style_controls,
        timing_controls: initial.timing_controls,
        undo_control: initial.undo_control,
        delete_control: initial.delete_control,
        status: initial_status,
        exported: None,
        exporting: false,
        export_id: None,
        export_cancel: None,
        close_after_export: false,
        export_stalled: false,
        playing: false,
        playback_generation: 0,
        playback_cancel: None,
        playback_mailbox: Arc::new(Mutex::new(PlaybackMailbox::default())),
        resume_after_drag: false,
        scrub_tx: None,
        scrub_generation: 0,
        sharing: false,
    });
    recompose_preview(&mut state);
    let state = Box::into_raw(state);

    unsafe {
        let hinstance = GetModuleHandleW(None)?;
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hIcon: crate::tray::app_icon(),
            lpszClassName: w!("matteshot_recdone"),
            ..Default::default()
        };
        RegisterClassW(&class);

        let style = editor_style();
        let outer = RECT {
            left: 0,
            top: 0,
            right: cw,
            bottom: ch,
        };
        let outer = crate::dpi::outer_bounds(outer, style, WS_EX_APPWINDOW, scale);
        match CreateWindowExW(
            WS_EX_APPWINDOW,
            w!("matteshot_recdone"),
            w!("Matteshot — video editor"),
            style,
            window_x,
            window_y,
            outer.right - outer.left,
            outer.bottom - outer.top,
            None,
            None,
            hinstance,
            Some(state as *const _),
        ) {
            Ok(hwnd) => {
                crate::theme::apply_titlebar(hwnd, &(*state).theme);
                let _ = SetForegroundWindow(hwnd);
                let _ = SetFocus(hwnd);
                // Live scrubbing: one long-lived decoder chasing the playhead.
                let (scrub_tx, scrub_rx) = std::sync::mpsc::channel();
                (*state).scrub_tx = Some(scrub_tx);
                {
                    let source = (*state).mp4.clone();
                    let target = hwnd.0 as isize;
                    std::thread::spawn(move || {
                        let com = CoInitializeEx(None, COINIT_MULTITHREADED);
                        let result = crate::trim::scrub_worker(
                            &source,
                            preview_w,
                            preview_h,
                            scrub_rx,
                            |generation, bytes, fw, fh| {
                                let payload =
                                    Box::into_raw(Box::new((generation, bytes, fw, fh)));
                                let hwnd = HWND(target as *mut _);
                                if !crate::window::has_class(hwnd, "matteshot_recdone")
                                    || PostMessageW(
                                        hwnd,
                                        WM_SCRUB_FRAME,
                                        WPARAM(0),
                                        LPARAM(payload as isize),
                                    )
                                    .is_err()
                                {
                                    drop(Box::from_raw(payload));
                                    return false;
                                }
                                true
                            },
                        );
                        if result.is_err() {
                            crate::diagnostics::log("scrub decoder unavailable");
                        }
                        if com.is_ok() {
                            CoUninitialize();
                        }
                    });
                }

                // Now that the editor is on screen, fill in the filmstrip and
                // the scrub cache behind it.
                let source = (*state).mp4.clone();
                let target = hwnd.0 as isize;
                std::thread::spawn(move || {
                    let com = CoInitializeEx(None, COINIT_MULTITHREADED);
                    let started = std::time::Instant::now();
                    let probed = crate::trim::probe_editor(
                        &source, probe_w, probe_h, preview_w, preview_h,
                    );
                    eprintln!("timing: filmstrip + scrub cache in {:?}", started.elapsed());
                    if com.is_ok() {
                        CoUninitialize();
                    }
                    let Ok(probed) = probed else {
                        crate::diagnostics::log("editor filmstrip probe failed");
                        return;
                    };
                    let payload = Box::into_raw(Box::new(probed));
                    let hwnd = HWND(target as *mut _);
                    if !crate::window::has_class(hwnd, "matteshot_recdone")
                        || PostMessageW(hwnd, WM_PROBE_READY, WPARAM(0), LPARAM(payload as isize))
                            .is_err()
                    {
                        drop(Box::from_raw(payload));
                    }
                });
            }
            Err(_) => drop(Box::from_raw(state)),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        add_chip_label, annotation_preview_time, apply_caption_input, available_export_path,
        delete_recording_files, layout, minimum_client_size, next_counter_number,
        recording_delete_prompt, tool_after_pick, CaptionInput, NEXT_EXPORT_ID, VIDEO_TOOLS,
    };
    use std::sync::atomic::Ordering;

    #[test]
    fn maximized_1080p_layout_keeps_the_preview_dominant() {
        // A 1920x1080 display at 125% scaling leaves roughly a 1000px client
        // after the title bar/taskbar. Caption placement should still get the
        // majority of that height rather than a postage-stamp preview.
        let window = layout(1.25, 1920, 1000, 7);
        let preview_h = window.preview.bottom - window.preview.top;
        let timeline_h = window.strip.bottom - window.strip.top;
        assert!(preview_h >= 600, "preview was only {preview_h}px tall");
        assert!(preview_h >= timeline_h * 6);
        assert!(window.padding_slider.bottom <= window.strip.top - 20);
        assert!(window.caption_size_slider.bottom < window.caption_opacity_slider.top);
        assert!(window.caption_opacity_slider.bottom < window.timing_controls[0].0.top);
    }

    fn assert_layout_is_usable(scale: f32, requested_w: i32, requested_h: i32) {
        let (minimum_w, minimum_h) = minimum_client_size(scale);
        let (width, height) = (requested_w.max(minimum_w), requested_h.max(minimum_h));
        let window = layout(scale, requested_w, requested_h, 7);
        let preview_h = window.preview.bottom - window.preview.top;
        let timeline_h = window.strip.bottom - window.strip.top;
        assert!(preview_h >= (180.0 * scale) as i32, "preview was {preview_h}px");
        assert!(timeline_h >= (60.0 * scale) as i32);
        assert!(window.preview.left >= 0 && window.preview.right <= width);
        assert!(window.preview.top >= 0 && window.preview.bottom < window.strip.top);
        let matte_bottom = window
            .matte_controls
            .iter()
            .map(|(rect, _)| rect.bottom)
            .max()
            .unwrap();
        assert!(
            matte_bottom + (6.0 * scale).round() as i32 <= window.padding_slider.top,
            "matte row ended at {matte_bottom}, settings began at {}",
            window.padding_slider.top
        );
        assert!(
            matte_bottom + (6.0 * scale).round() as i32
                <= window.aspect_controls[0].0.top
        );
        assert!(window.padding_slider.bottom < window.strip.top);
        assert!(window
            .tool_controls
            .iter()
            .all(|(rect, ..)| rect.left >= window.preview.left && rect.right <= window.preview.right));
        assert!(window.timing_controls.iter().all(|(rect, _)| {
            rect.top >= window.preview.top && rect.bottom <= window.preview.bottom
        }));
        assert!(window.strip.bottom < window.controls[0].0.top);
        assert!(window.controls.iter().all(|(rect, ..)| rect.bottom <= height));
        assert!(window
            .matte_controls
            .iter()
            .all(|(rect, _)| rect.left >= 0 && rect.right <= width));
        assert!(window
            .aspect_controls
            .iter()
            .all(|(rect, _)| rect.left >= 0 && rect.right <= width));
    }

    #[test]
    fn minimum_and_transient_tiny_layouts_never_collapse() {
        for scale in [1.0, 1.25, 1.5, 2.0] {
            let (minimum_w, minimum_h) = minimum_client_size(scale);
            assert_layout_is_usable(scale, minimum_w, minimum_h);
            // A bad restored placement can briefly send a tiny WM_SIZE. The
            // layout must remain valid while the window self-repairs.
            assert_layout_is_usable(scale, 320, 180);
        }
    }

    #[test]
    fn share_sits_between_copy_and_delete_without_overlap() {
        // Copy and Show in folder always act on the recorded original, and
        // Share follows the same rule (see the click handler), so it belongs
        // in the same row rather than the annotation tool panel.
        let window = layout(1.0, 1280, 720, 7);
        let find = |act: super::Act| {
            window
                .controls
                .iter()
                .find(|(_, a, _)| *a == act)
                .unwrap_or_else(|| panic!("missing control"))
        };
        let (copy_rect, _, copy_label) = find(super::Act::Copy);
        let (share_rect, _, share_label) = find(super::Act::Share);
        let (delete_rect, _, _) = find(super::Act::Delete);
        assert_eq!(*copy_label, "Copy");
        assert_eq!(*share_label, "Share");
        assert!(copy_rect.right < share_rect.left, "Share overlaps Copy");
        assert!(share_rect.right < delete_rect.left, "Delete overlaps Share");
        assert!(share_rect.right <= 1280, "Share runs off the minimum-width window");
    }

    #[test]
    fn caption_input_accepts_typing_without_requiring_enter() {
        let mut text = String::new();
        for ch in "Smooth caption 👍".chars() {
            assert_eq!(apply_caption_input(&mut text, ch), CaptionInput::Changed);
        }
        assert_eq!(text, "Smooth caption 👍");
        assert_eq!(apply_caption_input(&mut text, '\u{8}'), CaptionInput::Changed);
        assert_eq!(text, "Smooth caption ");
        assert_eq!(apply_caption_input(&mut text, '\r'), CaptionInput::Commit);
        assert_eq!(apply_caption_input(&mut text, '\n'), CaptionInput::Ignored);

        let mut capped = "x".repeat(160);
        assert_eq!(apply_caption_input(&mut capped, 'y'), CaptionInput::Ignored);
        assert_eq!(capped.chars().count(), 160);
    }

    #[test]
    fn terminal_preview_keeps_end_exclusive_annotations_visible_and_editable() {
        let trim_start = 10_000_000;
        let trim_end = 40_000_000;
        let item = crate::video_edit::Item {
            shape: crate::video_edit::Shape::Arrow {
                from: (0.1, 0.2),
                to: (0.8, 0.7),
            },
            start: trim_start,
            end: trim_end,
            color: 0,
            size: 1.0,
            caption_style: crate::video_edit::CaptionStyle::Shadow,
            caption_box_opacity: 0.68,
        };

        // Timing/export semantics remain end-exclusive.
        assert!(!item.active_at(trim_end));
        // Only preview interaction maps the held terminal frame inward.
        let preview_time = annotation_preview_time(trim_end, trim_start, trim_end);
        assert_eq!(preview_time, trim_end - 1);
        assert!(item.active_at(preview_time));
        assert_eq!(
            annotation_preview_time(trim_end - 1, trim_start, trim_end),
            trim_end - 1
        );
        assert_eq!(
            annotation_preview_time(trim_end, trim_end - 1, trim_end),
            trim_end - 1
        );
    }

    #[test]
    fn every_annotation_shape_opens_the_matching_property_tool() {
        use crate::video_edit::Shape;

        assert_eq!(super::tool_for_shape(&Shape::Arrow { from: (0.0, 0.0), to: (1.0, 1.0) }), super::Tool::Arrow);
        assert_eq!(super::tool_for_shape(&Shape::Line { from: (0.0, 0.0), to: (1.0, 1.0) }), super::Tool::Line);
        assert_eq!(super::tool_for_shape(&Shape::Rect { a: (0.0, 0.0), b: (1.0, 1.0) }), super::Tool::Rect);
        assert_eq!(super::tool_for_shape(&Shape::Ellipse { a: (0.0, 0.0), b: (1.0, 1.0) }), super::Tool::Ellipse);
        assert_eq!(super::tool_for_shape(&Shape::Highlight { a: (0.0, 0.0), b: (1.0, 1.0) }), super::Tool::Highlight);
        assert_eq!(super::tool_for_shape(&Shape::Text { pos: (0.0, 0.0), text: String::new() }), super::Tool::Text);
        assert_eq!(super::tool_for_shape(&Shape::Blur { a: (0.0, 0.0), b: (1.0, 1.0) }), super::Tool::Blur);
        assert_eq!(super::tool_for_shape(&Shape::Counter { pos: (0.0, 0.0), n: 1 }), super::Tool::Counter);
        assert_eq!(super::tool_for_shape(&Shape::Freehand { points: vec![(0.0, 0.0), (1.0, 1.0)] }), super::Tool::Pen);
    }

    #[test]
    fn video_editor_exposes_the_same_nine_annotation_tools_as_photo() {
        assert_eq!(
            VIDEO_TOOLS.map(|(_, label)| label),
            ["Arrow", "Line", "Box", "Oval", "Mark", "Text", "Blur", "Step", "Pen"]
        );
        let controls = layout(1.0, 1280, 720, 7).tool_controls;
        assert_eq!(controls.len(), VIDEO_TOOLS.len());
        for row in controls.chunks(3) {
            assert_eq!(row.len(), 3);
            assert!(row.windows(2).all(|pair| pair[0].0.right < pair[1].0.left));
        }
        assert!(controls.windows(4).all(|window| window[0].0.bottom < window[3].0.top));
        // Every tool arms until it is put away: a second pick disarms it, and
        // any other pick switches instead of clearing.
        for (tool, _) in VIDEO_TOOLS {
            assert_eq!(tool_after_pick(None, tool), Some(tool));
            assert_eq!(tool_after_pick(Some(tool), tool), None);
            let other = if tool == super::Tool::Arrow {
                super::Tool::Pen
            } else {
                super::Tool::Arrow
            };
            assert_eq!(tool_after_pick(Some(other), tool), Some(tool));
        }
    }

    #[test]
    fn the_add_chip_names_the_armed_tool_while_the_drawer_is_shut() {
        assert_eq!(add_chip_label(false, None), "+ Add");
        assert_eq!(add_chip_label(true, None), "Done");
        for (tool, label) in VIDEO_TOOLS {
            // Adding closes the drawer, so this is the only thing left on
            // screen saying a tool is still armed.
            assert_eq!(add_chip_label(false, Some(tool)), label);
            // Open, the lit chip in the drawer says which one; the add chip is
            // the way out of the drawer.
            assert_eq!(add_chip_label(true, Some(tool)), "Done");
        }
    }

    #[test]
    fn video_steps_continue_from_the_highest_visible_number() {
        use crate::video_edit::{CaptionStyle, Item, Shape};

        let item = |n| Item {
            shape: Shape::Counter { pos: (0.5, 0.5), n },
            start: 0,
            end: 10,
            color: 0,
            size: 1.0,
            caption_style: CaptionStyle::Shadow,
            caption_box_opacity: 0.68,
        };
        assert_eq!(next_counter_number(&[]), 1);
        assert_eq!(next_counter_number(&[item(1), item(4), item(2)]), 5);
    }

    #[test]
    fn exports_never_overwrite_an_existing_edit() {
        let id = NEXT_EXPORT_ID.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "matteshot-export-path-test-{}-{id}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let original = dir.join("recording-edit.mp4");
        std::fs::File::create(&original).unwrap();

        let available = available_export_path(original.clone());
        assert_eq!(available, dir.join("recording-edit-2.mp4"));
        assert_ne!(crate::output::partial_video_path(&available, id), available);
        assert_eq!(
            crate::output::partial_video_path(&available, id).parent(),
            available.parent()
        );

        std::fs::remove_file(original).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn recording_delete_confirmation_names_the_irrecoverable_file() {
        let prompt = recording_delete_prompt(std::path::Path::new("C:\\Videos\\demo.mp4"));
        assert!(prompt.contains("demo.mp4"));
        assert!(prompt.contains("cannot be undone"));
    }

    #[test]
    fn confirmed_recording_delete_removes_mp4_and_gif() {
        let id = NEXT_EXPORT_ID.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "matteshot-recording-delete-{}-{id}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mp4 = dir.join("recording.mp4");
        let gif = dir.join("recording.gif");
        std::fs::write(&mp4, b"video").unwrap();
        std::fs::write(&gif, b"gif").unwrap();

        delete_recording_files(&mp4, Some(&gif)).unwrap();

        assert!(!mp4.exists());
        assert!(!gif.exists());
        std::fs::remove_dir(dir).unwrap();
    }
}
