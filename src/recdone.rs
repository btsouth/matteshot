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
    GetKeyState, SetFocus, VK_CONTROL, VK_DELETE, VK_END, VK_ESCAPE, VK_HOME, VK_LEFT, VK_RETURN,
    VK_RIGHT, VK_SPACE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetCursorPos, GetWindowLongPtrW, LoadCursorW,
    MessageBoxW, PostMessageW, RegisterClassW, SetForegroundWindow, SetWindowLongPtrW,
    SetWindowPos, CREATESTRUCTW, CS_DBLCLKS, CS_HREDRAW, CS_VREDRAW, GWLP_USERDATA, IDC_ARROW,
    IDYES, MB_DEFBUTTON2, MB_ICONWARNING, MB_YESNO, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER,
    WINDOW_STYLE, WM_APP, WM_CHAR, WM_CLOSE, WM_CONTEXTMENU, WM_ERASEBKGND, WM_KEYDOWN,
    WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCCREATE, WM_NCDESTROY, WM_PAINT,
    WM_RBUTTONDOWN, WM_RBUTTONUP, WNDCLASSW, WS_CAPTION, WS_EX_APPWINDOW, WS_MAXIMIZEBOX,
    WS_SYSMENU, WS_THICKFRAME, WS_VISIBLE,
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

fn recording_delete_prompt(mp4: &std::path::Path, gif: Option<&std::path::Path>) -> String {
    let mp4_name = mp4
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "this recording".into());
    let names = gif
        .and_then(|path| path.file_name())
        .map(|name| format!("{mp4_name} and {}", name.to_string_lossy()))
        .unwrap_or(mp4_name);
    format!("Delete {names}?\n\nThis permanently removes the recording files and cannot be undone.")
}

fn recording_delete_stage_path(path: &std::path::Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    path.with_file_name(format!(
        "{name}.matteshot-delete-{}-{}",
        std::process::id(),
        NEXT_EXPORT_ID.fetch_add(1, Ordering::Relaxed)
    ))
}

fn delete_recording_files_with(
    mp4: &std::path::Path,
    gif: Option<&std::path::Path>,
    mut rename: impl FnMut(&std::path::Path, &std::path::Path) -> std::io::Result<()>,
) -> Result<()> {
    let mut staged: Vec<(PathBuf, PathBuf)> = Vec::new();
    for (source, label) in gif
        .into_iter()
        .map(|path| (path, "recording GIF"))
        .chain(std::iter::once((mp4, "recording")))
    {
        let temporary = recording_delete_stage_path(source);
        match rename(source, &temporary) {
            Ok(()) => staged.push((source.to_owned(), temporary)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                let mut rollback_failed = false;
                for (original, temporary) in staged.iter().rev() {
                    if rename(temporary, original).is_err() {
                        rollback_failed = true;
                    }
                }
                if rollback_failed {
                    crate::diagnostics::log("recording delete staging rollback failed");
                }
                return Err(error).with_context(|| format!("stage {label} for deletion"));
            }
        }
    }

    // Once every source has moved, the deletion is committed from the user's
    // perspective. Cleanup failures leave only Matteshot-specific staging
    // names and must not claim the originals were preserved.
    for (_, temporary) in staged {
        if let Err(error) = std::fs::remove_file(&temporary) {
            if error.kind() != std::io::ErrorKind::NotFound {
                crate::diagnostics::log("a staged recording deletion needs later cleanup");
            }
        }
    }
    Ok(())
}

fn delete_recording_files(mp4: &std::path::Path, gif: Option<&std::path::Path>) -> Result<()> {
    delete_recording_files_with(mp4, gif, |from, to| std::fs::rename(from, to))
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

/// After export encode returns, decide whether the same-folder `.partial`
/// may be thrown away. Unavailable is not a verdict on the bytes — a
/// Defender/OneDrive sharing violation used to delete a finished export
/// here (SBS-923). Recording publish and startup cleanup already keep
/// those files; this path still deletes a cancelled or failed encode,
/// and proven-undecodable bytes.
enum ExportCleanup {
    Cancelled,
    Encode(String),
    Validate(anyhow::Error),
    Rename(std::io::Error),
}

fn dispose_export_partial(temporary: &std::path::Path, error: ExportCleanup) -> String {
    match error {
        ExportCleanup::Cancelled => {
            let _ = std::fs::remove_file(temporary);
            "export cancelled".into()
        }
        ExportCleanup::Encode(message) => {
            let _ = std::fs::remove_file(temporary);
            message
        }
        ExportCleanup::Validate(error) => match crate::trim::validation_fault(&error) {
            crate::trim::ValidationFault::Undecodable => {
                let _ = std::fs::remove_file(temporary);
                format!("validate export: {error:#}")
            }
            crate::trim::ValidationFault::Unavailable => {
                crate::diagnostics::log(&format!(
                    "export validation unavailable: {error:#}; partial kept for recovery on the next start"
                ));
                "export could not be checked this time; the file is still in your videos folder and will be recovered the next time Matteshot starts"
                    .into()
            }
        },
        ExportCleanup::Rename(error) => {
            crate::diagnostics::log(&format!(
                "export publish failed: {error}; partial kept for recovery on the next start"
            ));
            format!(
                "finalize export: {error}; recovery file kept at {}",
                temporary.display()
            )
        }
    }
}

fn export_failure_status(error: &str) -> String {
    if error == "export cancelled" {
        "export cancelled · original kept".into()
    } else if error.contains("could not be checked") {
        "export could not be checked · edit kept".into()
    } else if error.contains("recovery file kept") {
        "export save failed · edit kept".into()
    } else {
        format!("export failed: {error}")
    }
}

static EXPORT_COMPLETIONS: crate::completion::CompletionMailbox<ExportDone> =
    crate::completion::CompletionMailbox::new();
static PROBE_COMPLETIONS: crate::completion::CompletionMailbox<crate::trim::Probe> =
    crate::completion::CompletionMailbox::new();
static SCRUB_COMPLETIONS: crate::completion::CompletionMailbox<(u64, Vec<u8>, u32, u32)> =
    crate::completion::CompletionMailbox::new();

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

/// What a drag on the crop rectangle is doing. Points are normalized to the
/// recording, the same space the crop itself lives in.
#[derive(Clone, Copy, PartialEq)]
enum CropDrag {
    /// Sweeping a new rectangle from the point it started at.
    New((f32, f32)),
    /// Sliding the whole rectangle; the point is where the cursor last was.
    Move((f32, f32)),
    /// Pulling one corner, in `Crop::corners` order.
    Corner(u8),
}

#[derive(Clone, Copy, PartialEq)]
enum Drag {
    Trim(Handle),
    Speed {
        index: usize,
        handle: Handle,
    },
    SpeedNew {
        index: usize,
        anchor: i64,
    },
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

#[derive(Clone)]
struct EditSnapshot {
    annotations: Vec<crate::video_edit::Item>,
    speed_ranges: Vec<crate::video_speed::SpeedRange>,
    /// The crop travels with the rest so Ctrl+Z steps back through framing
    /// and drawing in the one order they were done. Without it, applying a
    /// crop pushed a step that restored nothing and the press read as
    /// swallowed.
    crop: crate::video_edit::Crop,
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
    crop_control: RECT,
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
    speed_add_control: RECT,
    speed_rate_controls: Vec<(RECT, u32)>,
    speed_remove_control: RECT,
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
    /// The window of the recording being kept, normalized to the source.
    /// `Crop::FULL` is the whole thing. Annotations stay normalized to the
    /// source, so this moves the picture under them rather than invalidating
    /// them — the same model the photo editor uses.
    crop: crate::video_edit::Crop,
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
    crop_control: RECT,
    /// Pixel size of the content the last compose actually used — the frame
    /// after cropping, and after any draft-quality halving. Stored rather than
    /// re-derived, because hit-testing deriving it a second time is how it
    /// drifted from the composed picture in the first place.
    preview_content_size: (u32, u32),
    /// A button-up is still owed to a crop drag that a key already ended.
    crop_click_owed: bool,
    /// The crop being adjusted. While it is set the preview shows the whole
    /// recording, which is the only way to pull an edge back out.
    crop_edit: Option<crate::video_edit::Crop>,
    crop_drag: Option<CropDrag>,
    annotations: Vec<crate::video_edit::Item>,
    speed_ranges: Vec<crate::video_speed::SpeedRange>,
    selected_speed: Option<usize>,
    speed_armed: bool,
    undo: Vec<EditSnapshot>,
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
    speed_add_control: RECT,
    speed_rate_controls: Vec<(RECT, u32)>,
    speed_remove_control: RECT,
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
    /// Guards against a completion posted to a destroyed/reused HWND.
    share_request_id: Option<u64>,
    /// Cached at open: Share is offered only in a build with the `share`
    /// feature and a configured share server.
    can_share: bool,
}

/// Recdone Share click. History and tweak now use the same idle rule on
/// `pending_share` (SBS-1075).
fn start_share_upload(
    sharing: &mut bool,
    pending: &mut Option<u64>,
    start: impl FnOnce() -> u64,
) -> bool {
    if !crate::share::share_idle(*sharing) || !crate::share::begin_if_idle(pending, start) {
        return false;
    }
    *sharing = true;
    true
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn available_export_path(candidate: PathBuf) -> PathBuf {
    if !candidate.exists() {
        return candidate;
    }
    let parent = candidate
        .parent()
        .unwrap_or_else(|| std::path::Path::new(""));
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
        DEFAULT_CHARSET,
        windows::Win32::Graphics::Gdi::FONT_OUTPUT_PRECISION(0),
        windows::Win32::Graphics::Gdi::FONT_CLIP_PRECISION(0),
        CLEARTYPE_QUALITY,
        FF_DONTCARE.0 as u32,
        w!("Segoe UI"),
    )
}

unsafe fn state_of(hwnd: HWND) -> Option<&'static mut State> {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
    ptr.as_mut()
}

/// Layout for a client size. Rerun on resize.
fn layout(scale: f32, cw: i32, ch: i32, style_count: usize, can_share: bool) -> WindowLayout {
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
    // Crop closes the settings row, with padding and aspect — the group that
    // decides the shape of the picture rather than the marks on it.
    let crop_control = RECT {
        left: cw - m - sc(62),
        top: settings_top,
        right: cw - m,
        bottom: settings_top + sc(26),
    };
    let mut aspect_controls = Vec::new();
    let aspect_start = m + sc(344);
    let aspect_gap = sc(6);
    let aspect_available =
        (crop_control.left - sc(12) - aspect_start - aspect_gap * (ASPECTS.len() as i32 - 1))
            .max(1);
    let aspect_width = (aspect_available / ASPECTS.len() as i32).max(sc(48));
    for index in 0..ASPECTS.len() {
        let left = aspect_start + index as i32 * (aspect_width + aspect_gap);
        aspect_controls.push((
            RECT {
                left,
                top: settings_top,
                right: (left + aspect_width).min(crop_control.left - sc(12)),
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
        if act == Act::Share && !can_share {
            continue;
        }
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
    let speed_row_top = strip.bottom + sc(3);
    let speed_remove_control = RECT {
        left: strip.right - sc(64),
        top: speed_row_top,
        right: strip.right,
        bottom: speed_row_top + sc(24),
    };
    let mut speed_rate_controls = Vec::new();
    let rate_right = speed_remove_control.left - sc(6);
    for (index, rate) in [2u32, 4, 8, 16].into_iter().enumerate() {
        let left = rate_right - sc(38 * (4 - index) as i32);
        speed_rate_controls.push((
            RECT {
                left,
                top: speed_row_top,
                right: left + sc(34),
                bottom: speed_row_top + sc(24),
            },
            rate,
        ));
    }
    let speed_add_control = RECT {
        left: strip.right - sc(104),
        top: speed_row_top,
        right: strip.right,
        bottom: speed_row_top + sc(24),
    };
    WindowLayout {
        controls,
        matte_controls,
        padding_slider,
        aspect_controls,
        crop_control,
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
        speed_add_control,
        speed_rate_controls,
        speed_remove_control,
        preview,
        strip,
    }
}

fn thumb_image(bytes: &[u8], w: u32, h: u32) -> RgbaImage {
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for pixel in bytes.as_chunks::<4>().0.iter() {
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
    source: (u32, u32),
    raw: &[(Vec<u8>, u32, u32)],
    style: &crate::style::Style,
    opts: &crate::compose::ComposeOpts,
    crop: crate::video_edit::Crop,
) -> Vec<(Vec<u8>, u32, u32)> {
    let cropped = crop != crate::video_edit::Crop::FULL;
    if crate::compose::is_plain(style) && !cropped {
        return raw.to_vec();
    }
    if crate::compose::is_plain(style) {
        return raw
            .iter()
            .map(|(bytes, w, h)| {
                image_thumb(&crop_frame(&thumb_image(bytes, *w, *h), crop, source))
            })
            .collect();
    }
    let mut bases = std::collections::HashMap::<(u32, u32), RgbaImage>::new();
    raw.iter()
        .map(|(bytes, w, h)| {
            let image = crop_frame(&thumb_image(bytes, *w, *h), crop, source);
            let (w, h) = (&image.width(), &image.height());
            let base = bases.entry((*w, *h)).or_insert_with(|| {
                crate::compose::compose_base(*w as usize, *h as usize, style, opts)
            });
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

/// Reduce a frame to the crop.
///
/// Preview frames and filmstrip thumbnails are both scaled copies of the whole
/// recording, so a crop normalized to the source applies to either directly,
/// whatever size it happens to have been decoded at.
fn crop_frame(image: &RgbaImage, crop: crate::video_edit::Crop, source: (u32, u32)) -> RgbaImage {
    if crop == crate::video_edit::Crop::FULL {
        return image.clone();
    }
    // Framed from the source-pixel rect the export will use, scaled to this
    // frame — not by rounding the normalized crop against this frame's own
    // dimensions. That would be a second rounding, and it drifts from the
    // encoder's by up to a source pixel on an odd source, so the preview would
    // show a sliver the file does not keep.
    let (source_w, source_h) = (source.0.max(1), source.1.max(1));
    let (kept_x, kept_y, kept_w, kept_h) = crop.pixel_rect(source_w, source_h);
    let (width, height) = (image.width().max(1), image.height().max(1));
    let (scale_x, scale_y) = (
        width as f32 / source_w as f32,
        height as f32 / source_h as f32,
    );
    // Clamped the way the encoder clamps: hold the size and bring the origin
    // back so it fits. Shrinking the size at the far edge instead would frame
    // a different region from the one being encoded, which is the drift this
    // whole function exists to remove.
    let place = |origin: u32, size: u32, scale: f32, limit: u32| {
        let size = ((size as f32 * scale).round() as u32).max(1).min(limit);
        let origin = ((origin as f32 * scale).round() as u32).min(limit - size);
        (origin, size)
    };
    let (x, w) = place(kept_x, kept_w, scale_x, width);
    let (y, h) = place(kept_y, kept_h, scale_y, height);
    image::imageops::crop_imm(image, x, y, w, h).to_image()
}

/// The crop the preview and the annotation frame should currently use.
///
/// While the crop tool is armed the whole recording is on screen, so an edge
/// that was brought in can be pulled back out; everywhere else it is the crop
/// that has been applied.
/// How close a click has to be to a crop corner to take hold of it, in the
/// normalized units the crop is stored in, from a grab radius in screen pixels.
///
/// Measured in screen pixels rather than as a fraction of the source, which is
/// what the photo editor does. A fraction sounds equivalent and is not: it makes
/// the target an anisotropic rectangle that shrinks with the clip's aspect, so
/// the old flat 0.02 gave ~14.6px horizontally but only ~8.2px vertically on a
/// 16:9 preview, against a handle drawn ~11px wide and DPI-scaled. A click could
/// land on a lit pixel of the handle and still miss it.
fn crop_grab_tolerance(content: RECT, grab: i32) -> (f32, f32) {
    let grab = grab as f32;
    (
        grab / ((content.right - content.left) as f32).max(1.0),
        grab / ((content.bottom - content.top) as f32).max(1.0),
    )
}

fn active_crop(state: &State) -> crate::video_edit::Crop {
    if state.crop_edit.is_some() {
        crate::video_edit::Crop::FULL
    } else {
        state.crop
    }
}

/// End any crop drag in flight: give the mouse back, and see off the
/// button-up that is still coming.
///
/// A drag can be ended by the button coming up, or by a key while it is still
/// held — Esc, Enter, Del — or by any control. Each of those has to do two
/// things, and forgetting either has its own failure. Not releasing the capture
/// glues the mouse to the editor. Releasing without remembering that an up is
/// still owed lets that up fall through as a fresh click on whatever sits under
/// the cursor. Both belong here rather than at each exit.
fn end_crop_drag(state: &mut State) {
    if state.crop_drag.take().is_some() {
        state.crop_click_owed = true;
        unsafe {
            let _ = windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture();
        }
    }
}

/// Consume a button-up that belongs to a crop drag, however that drag ended.
/// True when the up was the drag's own and must go no further.
fn take_crop_click(state: &mut State) -> bool {
    let dragging = state.crop_drag.is_some();
    if dragging {
        end_crop_drag(state);
    }
    std::mem::take(&mut state.crop_click_owed) || dragging
}

/// Arm the crop tool: the whole recording comes back with the current crop
/// drawn over it.
fn enter_crop(state: &mut State) {
    commit_text(state);
    state.selected = None;
    state.tool = None;
    // The drawer is hit-tested before the preview, so leaving it open would
    // cover part of the crop rectangle and eat the clicks meant for it.
    state.tools_open = false;
    end_crop_drag(state);
    state.crop_edit = Some(state.crop);
    recompose_preview(state);
    refresh_matte_thumbs(state);
}

/// Take the pending crop and put the tool away. Nothing is recorded when it did
/// not change, so arming the tool and thinking better of it costs no undo step.
fn commit_crop(state: &mut State) {
    let Some(pending) = state.crop_edit.take() else {
        return;
    };
    end_crop_drag(state);
    if pending != state.crop {
        push_undo(state);
        state.crop = pending;
    }
    recompose_preview(state);
    refresh_matte_thumbs(state);
}

/// Leave the crop tool without keeping the pending rectangle.
fn cancel_crop(state: &mut State) {
    if state.crop_edit.take().is_some() {
        end_crop_drag(state);
        recompose_preview(state);
        refresh_matte_thumbs(state);
    }
}

fn recompose_preview(state: &mut State) {
    let opts = state_compose_opts(state);
    let Some(frame) = state.preview_raw.as_ref() else {
        state.preview = None;
        return;
    };
    let mut raw = crop_frame(
        &thumb_image(&frame.0, frame.1, frame.2),
        active_crop(state),
        state.source_size,
    );
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
    state.preview_content_size = content_size;
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
        crate::video_edit::Frame {
            crop: active_crop(state),
            content: content_size,
        },
        cropped_source_size(state),
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
            crate::video_edit::Frame {
                crop: active_crop(state),
                content: content_size,
            },
            cropped_source_size(state),
            content_offset,
        );
    }
    state.preview = Some(image_thumb(&image));
}

fn update_padding(state: &mut State, x: i32) {
    let slider = state.padding_slider;
    let t = ((x - slider.left) as f32 / (slider.right - slider.left).max(1) as f32).clamp(0.0, 1.0);
    state.pad_factor = crate::compose::PAD_SLIDER_MIN
        + t * (crate::compose::PAD_SLIDER_MAX - crate::compose::PAD_SLIDER_MIN);
    recompose_preview(state);
}

fn refresh_matte_thumbs(state: &mut State) {
    let opts = state_compose_opts(state);
    state.thumbs = matte_thumbs(
        state.source_size,
        &state.raw_thumbs,
        &state.styles[state.matte_index],
        &opts,
        active_crop(state),
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
    let speed_ranges = state.speed_ranges.clone();
    let max_w = ((state.preview_rect.right - state.preview_rect.left).max(2) as u32).min(1440);
    let max_h = ((state.preview_rect.bottom - state.preview_rect.top).max(2) as u32).min(900);
    let mailbox = state.playback_mailbox.clone();
    let hwnd_raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let result = crate::trim::playback_frames_with_speed(
            &source,
            start,
            end,
            max_w,
            max_h,
            &speed_ranges,
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
                        Some(HWND(hwnd_raw as *mut _)),
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
                Some(HWND(hwnd_raw as *mut _)),
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
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

fn toggle_playback(hwnd: HWND, state: &mut State) {
    if state.playing {
        stop_playback(state);
        state.status = Some("paused".into());
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
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
    let t = ((x - slider.left) as f32 / (slider.right - slider.left).max(1) as f32).clamp(0.0, 1.0);
    state.caption_size = MIN + t * (MAX - MIN);
    if let Some(index) = state
        .selected
        .filter(|index| *index < state.annotations.len())
    {
        if matches!(
            state.annotations[index].shape,
            crate::video_edit::Shape::Text { .. }
        ) {
            state.annotations[index].size = state.caption_size;
        }
    }
    recompose_preview(state);
}

fn update_caption_opacity(state: &mut State, x: i32) {
    let slider = state.caption_opacity_slider;
    let t = ((x - slider.left) as f32 / (slider.right - slider.left).max(1) as f32).clamp(0.0, 1.0);
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
        || !state.speed_ranges.is_empty()
        // A crop-only edit is still an edit: without this, Copy and Show in
        // folder keep the labels that promise the original and quietly hand
        // over the uncropped recording.
        || state.crop != crate::video_edit::Crop::FULL
}

fn push_undo(state: &mut State) {
    state.undo.push(EditSnapshot {
        annotations: state.annotations.clone(),
        speed_ranges: state.speed_ranges.clone(),
        crop: state.crop,
    });
    if state.undo.len() > 40 {
        state.undo.remove(0);
    }
}

fn undo(state: &mut State) {
    // A live drag holds an index into `annotations`. Swapping the vector out
    // underneath it would leave the rest of the drag moving whichever
    // annotation happened to land at that index.
    if state.dragging.is_some() || state.crop_drag.is_some() {
        return;
    }
    if let Some(previous) = state.undo.pop() {
        state.annotations = previous.annotations;
        state.speed_ranges = previous.speed_ranges;
        let crop_changed = state.crop != previous.crop;
        state.crop = previous.crop;
        state.selected = None;
        state.selected_speed = None;
        state.text_entry = None;
        recompose_preview(state);
        if crop_changed {
            refresh_matte_thumbs(state);
        }
    }
}

fn delete_selected(state: &mut State) {
    if let Some(index) = state
        .selected_speed
        .filter(|index| *index < state.speed_ranges.len())
    {
        push_undo(state);
        state.speed_ranges.remove(index);
        state.selected_speed = None;
        return;
    }
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
    let output = preview_image_rect(state, frame);
    if crate::compose::is_plain(&state.styles[state.matte_index]) {
        return Some(output);
    }
    // The size the last compose actually used, not the decoded frame's. Those
    // are the same thing only until a crop is applied: the matte's padding is
    // proportional to the content it frames, so laying it out from the whole
    // frame put every click somewhere other than where the picture was drawn.
    let (content_w, content_h) = state.preview_content_size;
    let opts = state_compose_opts(state);
    let layout = crate::compose::layout(content_w as usize, content_h as usize, &opts);
    let scale = ((output.right - output.left) as f32 / frame.1.max(1) as f32)
        .min((output.bottom - output.top) as f32 / frame.2.max(1) as f32);
    Some(RECT {
        left: output.left + (layout.pad_x as f32 * scale).round() as i32,
        top: output.top + (layout.pad_y as f32 * scale).round() as i32,
        right: output.left + ((layout.pad_x as u32 + content_w) as f32 * scale).round() as i32,
        bottom: output.top + ((layout.pad_y as u32 + content_h) as f32 * scale).round() as i32,
    })
}

/// Where a window point falls in the recording, normalized to the whole
/// source. `None` when it is not on the picture.
///
/// Source, not the picture on screen: annotations are stored against the
/// recording so that reframing moves the picture under them, and this is the
/// only way a click becomes a coordinate — so putting the crop here means no
/// caller can be written that forgets it.
fn screen_to_preview(state: &State, x: i32, y: i32) -> Option<(f32, f32)> {
    let rect = preview_content_rect(state)?;
    if x < rect.left || x > rect.right || y < rect.top || y > rect.bottom {
        return None;
    }
    Some(preview_point(state, rect, x, y))
}

/// The same mapping with no reject, pinned to the edges of the picture.
///
/// A crop drag holds the mouse, so sweeping past the pane has to keep
/// tracking: rejecting out there would freeze the rectangle where the cursor
/// crossed the line and make it jump on the way back.
fn screen_to_preview_clamped(state: &State, x: i32, y: i32) -> Option<(f32, f32)> {
    let rect = preview_content_rect(state)?;
    Some(preview_point(state, rect, x, y))
}

/// A recording coordinate back as a fraction of the picture on screen — the
/// inverse of `preview_point`, and the only way anything should go that way.
///
/// The two directions are a pair: whatever the crop does to one it must undo
/// on the other, and having them written apart is how the selection outline
/// came to be drawn where its shape was not.
fn preview_across(state: &State, x: f32) -> f32 {
    let crop = active_crop(state);
    ((x - crop.x) / crop.w.max(f32::EPSILON)).clamp(0.0, 1.0)
}

fn preview_down(state: &State, y: f32) -> f32 {
    let crop = active_crop(state);
    ((y - crop.y) / crop.h.max(f32::EPSILON)).clamp(0.0, 1.0)
}

fn preview_point(state: &State, rect: RECT, x: i32, y: i32) -> (f32, f32) {
    let across = ((x - rect.left) as f32 / (rect.right - rect.left).max(1) as f32).clamp(0.0, 1.0);
    let down = ((y - rect.top) as f32 / (rect.bottom - rect.top).max(1) as f32).clamp(0.0, 1.0);
    // What is on screen is the crop, so a fraction of the picture is that
    // fraction *of the crop*. `Crop::FULL` — including the whole time the crop
    // tool is armed — leaves this an identity.
    let crop = active_crop(state);
    (crop.x + across * crop.w, crop.y + down * crop.h)
}

/// The recording window annotations are addressed against: the crop, at the
/// pixel size it will be exported at. Identical to the source while uncropped.
fn annotation_frame(state: &State) -> crate::video_edit::Frame {
    crate::video_edit::Frame {
        crop: active_crop(state),
        content: cropped_source_size(state),
    }
}

/// Source pixels the crop keeps.
fn cropped_source_size(state: &State) -> (u32, u32) {
    cropped_source_size_for(state, active_crop(state))
}

/// What the export will actually produce, through the same rounding it uses.
/// Deriving it separately had the readout promising a pixel the encoder was
/// never going to keep.
fn cropped_source_size_for(state: &State, crop: crate::video_edit::Crop) -> (u32, u32) {
    let (width, height) = (state.source_size.0.max(1), state.source_size.1.max(1));
    let (_, _, kept_w, kept_h) = crop.pixel_rect(width, height);
    (kept_w.max(1), kept_h.max(1))
}

fn hit_annotation(state: &State, point: (f32, f32)) -> Option<usize> {
    let frame = annotation_frame(state);
    let time = current_annotation_time(state);
    state
        .annotations
        .iter()
        .enumerate()
        .rev()
        .find(|(_, item)| item.active_at(time) && crate::video_edit::hit(item, point, 0.018, frame))
        .map(|(index, _)| index)
}

fn hit_annotation_handle(
    state: &State,
    point: (f32, f32),
) -> Option<(usize, crate::video_edit::ShapeHandle)> {
    let frame = annotation_frame(state);
    let time = current_annotation_time(state);
    let index = state
        .selected
        .filter(|index| *index < state.annotations.len())?;
    let item = &state.annotations[index];
    if !item.active_at(time) {
        return None;
    }
    crate::video_edit::hit_handle(item, point, 0.026, frame).map(|handle| (index, handle))
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

fn update_draw_shape(shape: &mut crate::video_edit::Shape, start: (f32, f32), point: (f32, f32)) {
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
            if points
                .last()
                .is_none_or(|last| (last.0 - point.0).hypot(last.1 - point.1) >= 0.001)
            {
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
        | crate::video_edit::Shape::Blur { a, b } => (a.0 - b.0).hypot(a.1 - b.1) < 0.008,
        crate::video_edit::Shape::Freehand { points } => {
            points
                .windows(2)
                .map(|segment| (segment[0].0 - segment[1].0).hypot(segment[0].1 - segment[1].1))
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
    state.selected_speed = None;
    state.speed_armed = false;
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

fn timeline_time(state: &State, x: i32) -> i64 {
    let span = (state.strip.right - state.strip.left).max(1) as f64;
    ((((x - state.strip.left) as f64 / span) * state.duration as f64) as i64)
        .clamp(0, state.duration)
}

fn speed_time_map(state: &State) -> Option<crate::video_speed::TimeMap> {
    crate::video_speed::TimeMap::new(state.trim_start, state.trim_end, &state.speed_ranges).ok()
}

fn speed_gap(
    ranges: &[crate::video_speed::SpeedRange],
    index: usize,
    trim_start: i64,
    trim_end: i64,
    replacing: bool,
) -> (i64, i64) {
    let lower = index
        .checked_sub(1)
        .and_then(|previous| ranges.get(previous))
        .map_or(trim_start, |range| range.end.max(trim_start));
    let next = if replacing { index + 1 } else { index };
    let upper = ranges
        .get(next)
        .map_or(trim_end, |range| range.start.min(trim_end));
    (lower, upper)
}

fn set_speed_handle(state: &mut State, index: usize, handle: Handle, x: i32) {
    const MIN: i64 = 3_000_000;
    if index >= state.speed_ranges.len() {
        return;
    }
    let time = timeline_time(state, x);
    let (lower, upper) = speed_gap(
        &state.speed_ranges,
        index,
        state.trim_start,
        state.trim_end,
        true,
    );
    if upper - lower < MIN {
        return;
    }
    let range = &mut state.speed_ranges[index];
    range.start = range.start.clamp(lower, upper - MIN);
    range.end = range.end.clamp(range.start + MIN, upper);
    match handle {
        Handle::Start => range.start = time.clamp(lower, range.end - MIN),
        Handle::End => range.end = time.clamp(range.start + MIN, upper),
    }
    state.playhead = match handle {
        Handle::Start => range.start,
        Handle::End => range.end,
    };
    refresh_preview(state);
}

fn set_new_speed_range(state: &mut State, index: usize, anchor: i64, x: i32) {
    const MIN: i64 = 3_000_000;
    if index >= state.speed_ranges.len() {
        return;
    }
    let time = timeline_time(state, x);
    let (lower, upper) = speed_gap(
        &state.speed_ranges,
        index,
        state.trim_start,
        state.trim_end,
        true,
    );
    if upper - lower < MIN {
        return;
    }
    let mut start = anchor.min(time).clamp(lower, upper);
    let mut end = anchor.max(time).clamp(lower, upper);
    if end - start < MIN {
        if start + MIN <= upper {
            end = start + MIN;
        } else {
            start = (end - MIN).max(lower);
        }
    }
    state.speed_ranges[index].start = start;
    state.speed_ranges[index].end = end;
    state.playhead = time.clamp(start, end);
    refresh_preview(state);
}

fn add_speed_range(state: &mut State, anchor: i64) -> Option<usize> {
    const MIN: i64 = 3_000_000;
    let insertion = state
        .speed_ranges
        .partition_point(|range| range.start < anchor);
    let (lower, upper) = speed_gap(
        &state.speed_ranges,
        insertion,
        state.trim_start,
        state.trim_end,
        false,
    );
    if upper - lower < MIN {
        return None;
    }
    let start = anchor.clamp(lower, upper - MIN);
    state.speed_ranges.insert(
        insertion,
        crate::video_speed::SpeedRange::new(start, start + MIN, 8),
    );
    Some(insertion)
}

fn speed_range_at(state: &State, x: i32, y: i32) -> Option<usize> {
    if y < state.strip.top || y > state.strip.top + s(state, 22) || state.duration <= 0 {
        return None;
    }
    let time = timeline_time(state, x);
    state
        .speed_ranges
        .iter()
        .position(|range| time >= range.start && time <= range.end)
}

fn s(state: &State, v: i32) -> i32 {
    (v as f32 * state.scale) as i32
}

unsafe fn paint_bgra_fit(hdc: HDC, rect: RECT, frame: &(Vec<u8>, u32, u32), state: &State) {
    let panel = CreateSolidBrush(state.theme.chip);
    let panel_pen = CreatePen(PS_SOLID, 1, state.theme.chip_line);
    let old_brush = SelectObject(hdc, panel.into());
    let old_pen = SelectObject(hdc, panel_pen.into());
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
    let _ = DeleteObject(panel.into());
    let _ = DeleteObject(panel_pen.into());

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

/// Translucent fill over the preview. GDI has no alpha on `Rectangle`, so a
/// 1x1 solid is stretched through `AlphaBlend`.
unsafe fn wash(hdc: HDC, rect: RECT, color: windows::Win32::Foundation::COLORREF, alpha: u8) {
    let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
    if w <= 0 || h <= 0 {
        return;
    }
    let mem = windows::Win32::Graphics::Gdi::CreateCompatibleDC(Some(hdc));
    let bmp = windows::Win32::Graphics::Gdi::CreateCompatibleBitmap(hdc, 1, 1);
    let old = SelectObject(mem, bmp.into());
    let brush = CreateSolidBrush(color);
    FillRect(
        mem,
        &RECT {
            left: 0,
            top: 0,
            right: 1,
            bottom: 1,
        },
        brush,
    );
    let _ = windows::Win32::Graphics::Gdi::AlphaBlend(
        hdc,
        rect.left,
        rect.top,
        w,
        h,
        mem,
        0,
        0,
        1,
        1,
        windows::Win32::Graphics::Gdi::BLENDFUNCTION {
            BlendOp: windows::Win32::Graphics::Gdi::AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: alpha,
            AlphaFormat: 0,
        },
    );
    SelectObject(mem, old);
    let _ = DeleteObject(bmp.into());
    let _ = DeleteObject(brush.into());
    let _ = windows::Win32::Graphics::Gdi::DeleteDC(mem);
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
    let old_brush = SelectObject(hdc, fill.into());
    let old_pen = SelectObject(hdc, pen.into());
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
    let _ = DeleteObject(fill.into());
    let _ = DeleteObject(pen.into());
    SelectObject(hdc, state.font_small.into());
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
    let _ = DeleteObject(bg.into());
    SetBkMode(hdc, TRANSPARENT);

    let m = s(state, 22);
    SelectObject(hdc, state.font_big.into());
    SetTextColor(hdc, state.theme.text);
    let mut t = wide("Recording editor");
    let mut rc = RECT {
        left: m,
        top: s(state, 6),
        right: state.width - m,
        bottom: s(state, 28),
    };
    DrawTextW(hdc, &mut t, &mut rc, DT_LEFT | DT_SINGLELINE | DT_VCENTER);

    SelectObject(hdc, state.font_small.into());
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
                let image_rect =
                    preview_content_rect(state).unwrap_or_else(|| preview_image_rect(state, frame));
                let (x0, y0, x1, y1) = crate::video_edit::bounds(item, annotation_frame(state));
                // `bounds` is in the recording's coordinates, so it has to come
                // back through the crop — the exact inverse of `preview_point`.
                // Multiplying it straight into the picture on screen drew the
                // outline and its handles somewhere the shape was not, and left
                // the real (crop-correct) handles invisible but clickable.
                let map_x = |x: f32| {
                    let across = preview_across(state, x);
                    image_rect.left + (across * (image_rect.right - image_rect.left) as f32) as i32
                };
                let map_y = |y: f32| {
                    let down = preview_down(state, y);
                    image_rect.top + (down * (image_rect.bottom - image_rect.top) as f32) as i32
                };
                let pen = CreatePen(windows::Win32::Graphics::Gdi::PS_DOT, 1, state.theme.accent);
                let old_pen = SelectObject(hdc, pen.into());
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
                let _ = DeleteObject(pen.into());

                if let Some(handles) = crate::video_edit::handles(item) {
                    let handle_pen = CreatePen(PS_SOLID, s(state, 2).max(1), state.theme.accent);
                    let handle_fill = CreateSolidBrush(state.theme.bg);
                    let old_pen = SelectObject(hdc, handle_pen.into());
                    let old_brush = SelectObject(hdc, handle_fill.into());
                    let radius = s(state, 7);
                    for (_, point) in handles {
                        let (x, y) = (map_x(point.0), map_y(point.1));
                        let _ =
                            Ellipse(hdc, x - radius, y - radius, x + radius + 1, y + radius + 1);
                    }
                    SelectObject(hdc, old_brush);
                    SelectObject(hdc, old_pen);
                    let _ = DeleteObject(handle_fill.into());
                    let _ = DeleteObject(handle_pen.into());
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
        let old_brush = SelectObject(hdc, fill.into());
        let old_pen = SelectObject(hdc, pen.into());
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
        let _ = DeleteObject(fill.into());
        let _ = DeleteObject(pen.into());

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
            let old_brush = SelectObject(hdc, fill.into());
            let old_pen = SelectObject(hdc, pen.into());
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
            let _ = DeleteObject(fill.into());
            let _ = DeleteObject(pen.into());
        }
        if caption_controls_active(state) {
            SelectObject(hdc, state.font_small.into());
            SetTextColor(hdc, state.theme.muted);
            let mut size_label = wide(&format!(
                "Size {}",
                (state.caption_size * 24.0).round() as i32
            ));
            let mut size_rect = RECT {
                left: tool_panel(state).left + s(state, 12),
                top: state.caption_size_slider.top,
                right: state.caption_size_slider.left - s(state, 6),
                bottom: state.caption_size_slider.bottom,
            };
            DrawTextW(
                hdc,
                &mut size_label,
                &mut size_rect,
                DT_LEFT | DT_SINGLELINE | DT_VCENTER,
            );
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
            let _ = DeleteObject(track.into());
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
            let old = SelectObject(hdc, fill.into());
            let r = s(state, 6);
            let _ = RoundRect(hdc, thumb_x - r, cy - r, thumb_x + r, cy + r, r * 2, r * 2);
            SelectObject(hdc, old);
            let _ = DeleteObject(fill.into());
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
            let _ = DeleteObject(track.into());
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
            let old = SelectObject(hdc, fill.into());
            let r = s(state, 6);
            let _ = RoundRect(hdc, thumb_x - r, cy - r, thumb_x + r, cy + r, r * 2, r * 2);
            SelectObject(hdc, old);
            let _ = DeleteObject(fill.into());
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
            SelectObject(hdc, state.font_small.into());
            SetTextColor(hdc, state.theme.muted);
            let mut timing_label = wide("Timing");
            let mut timing_rect = RECT {
                left: tool_panel(state).left + s(state, 12),
                top: state.timing_controls[0].0.top,
                right: state.timing_controls[0].0.left - s(state, 6),
                bottom: state.timing_controls[0].0.bottom,
            };
            DrawTextW(
                hdc,
                &mut timing_label,
                &mut timing_rect,
                DT_LEFT | DT_SINGLELINE | DT_VCENTER,
            );
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
        SelectObject(hdc, state.font_small.into());
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

    SelectObject(hdc, state.font_small.into());
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
        let old_brush = SelectObject(hdc, fill.into());
        let old_pen = SelectObject(hdc, pen.into());
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
        let _ = DeleteObject(fill.into());
        let _ = DeleteObject(pen.into());

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
    let _ = DeleteObject(track.into());
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
    let old_brush = SelectObject(hdc, filled.into());
    let old_pen = SelectObject(hdc, thumb_pen.into());
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
    let _ = DeleteObject(filled.into());
    let _ = DeleteObject(thumb_pen.into());

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
        paint_chip(
            hdc,
            *rect,
            ASPECTS[*index].0,
            state.aspect_idx == *index,
            state,
        );
    }
    // Says what it will do rather than what it is called: with a crop applied,
    // this is how you get back to the whole recording.
    // Reads the frame that is on screen, not the committed one: pressing Del
    // while armed shows the whole recording again, and a chip still saying
    // "Cropped" would be describing something the user cannot see. Kept short
    // because the chip closes the settings row and has no width to spare, and
    // the accent fill already says the tool is armed.
    let crop_label = if state.crop_edit.is_some() {
        // Pressing it again is the Apply, and until this nothing said so: the
        // accent fill announced that the tool was armed but never how to
        // finish. Shorter than "Cropped", so the tight chip still fits it.
        "Apply"
    } else if state.crop != crate::video_edit::Crop::FULL {
        "Cropped"
    } else {
        "Crop"
    };
    paint_chip(
        hdc,
        state.crop_control,
        crop_label,
        state.crop_edit.is_some(),
        state,
    );

    // Crop overlay: everything the crop would discard is washed out, and the
    // kept rectangle carries the frame, corner handles and a size readout.
    // Screen-space, so dragging it never recomposes the preview.
    if let Some(pending) = state.crop_edit {
        if let Some(content) = preview_content_rect(state) {
            let (cw, chh) = (
                (content.right - content.left) as f32,
                (content.bottom - content.top) as f32,
            );
            let at = |nx: f32, ny: f32| {
                (
                    content.left + (nx * cw).round() as i32,
                    content.top + (ny * chh).round() as i32,
                )
            };
            let (kx0, ky0) = at(pending.x, pending.y);
            let (kx1, ky1) = at(pending.x + pending.w, pending.y + pending.h);
            for band in [
                RECT {
                    left: content.left,
                    top: content.top,
                    right: content.right,
                    bottom: ky0,
                },
                RECT {
                    left: content.left,
                    top: ky1,
                    right: content.right,
                    bottom: content.bottom,
                },
                RECT {
                    left: content.left,
                    top: ky0,
                    right: kx0,
                    bottom: ky1,
                },
                RECT {
                    left: kx1,
                    top: ky0,
                    right: content.right,
                    bottom: ky1,
                },
            ] {
                if band.right > band.left && band.bottom > band.top {
                    wash(hdc, band, state.theme.bg, 170);
                }
            }

            let pen = CreatePen(PS_SOLID, 1, state.theme.accent);
            let old_pen = SelectObject(hdc, pen.into());
            let old_brush = SelectObject(
                hdc,
                windows::Win32::Graphics::Gdi::GetStockObject(
                    windows::Win32::Graphics::Gdi::HOLLOW_BRUSH,
                ),
            );
            let _ = windows::Win32::Graphics::Gdi::Rectangle(hdc, kx0, ky0, kx1, ky1);
            SelectObject(hdc, old_brush);
            SelectObject(hdc, old_pen);
            let _ = DeleteObject(pen.into());

            let fill = CreateSolidBrush(state.theme.accent);
            let edge = CreatePen(PS_SOLID, 1, state.theme.bg);
            let ob = SelectObject(hdc, fill.into());
            let op = SelectObject(hdc, edge.into());
            for corner in pending.corners() {
                let (hx, hy) = at(corner.0, corner.1);
                let grab = s(state, 5);
                let _ = windows::Win32::Graphics::Gdi::Rectangle(
                    hdc,
                    hx - grab,
                    hy - grab,
                    hx + grab + 1,
                    hy + grab + 1,
                );
            }
            SelectObject(hdc, ob);
            SelectObject(hdc, op);
            let _ = DeleteObject(fill.into());
            let _ = DeleteObject(edge.into());

            let (source_w, source_h) = cropped_source_size_for(state, pending);
            SelectObject(hdc, state.font_small.into());
            SetTextColor(hdc, state.theme.text);
            let mut readout = wide(&format!("{source_w} \u{00d7} {source_h} px"));
            let readout_top = if ky0 - content.top < s(state, 20) {
                ky0 + s(state, 6)
            } else {
                ky0 - s(state, 18)
            };
            let mut readout_rect = RECT {
                left: kx0 + s(state, 4),
                top: readout_top,
                right: kx0 + s(state, 160),
                bottom: readout_top + s(state, 18),
            };
            DrawTextW(
                hdc,
                &mut readout,
                &mut readout_rect,
                DT_LEFT | DT_SINGLELINE | DT_VCENTER,
            );
        }
    }

    // Filmstrip + trim handles.
    if !state.thumbs.is_empty() && state.duration > 0 {
        let strip = state.strip;
        SelectObject(hdc, state.font_small.into());
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

        // Speed sections live in source time, so their bands stay attached to
        // the recorded moments even though the resulting output is shorter.
        for (index, range) in state.speed_ranges.iter().enumerate() {
            let left = to_x(range.start).max(strip.left);
            let right = to_x(range.end).min(strip.right);
            if right <= left {
                continue;
            }
            let selected = state.selected_speed == Some(index);
            let brush = CreateSolidBrush(if selected {
                state.theme.accent
            } else {
                state.theme.chip_line
            });
            FillRect(
                hdc,
                &RECT {
                    left,
                    top: strip.top,
                    right,
                    bottom: strip.top + s(state, 20),
                },
                brush,
            );
            let _ = DeleteObject(brush.into());
            SelectObject(hdc, state.font_small.into());
            SetTextColor(
                hdc,
                if selected {
                    state.theme.accent_text
                } else {
                    state.theme.text
                },
            );
            let mut text = wide(&format!("{}\u{00d7}", range.rate));
            let mut rect = RECT {
                left: left + s(state, 4),
                top: strip.top,
                right: right - s(state, 4),
                bottom: strip.top + s(state, 20),
            };
            DrawTextW(
                hdc,
                &mut text,
                &mut rect,
                DT_LEFT | DT_SINGLELINE | DT_VCENTER,
            );
        }

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
                let mem = windows::Win32::Graphics::Gdi::CreateCompatibleDC(Some(hdc));
                let bmp = windows::Win32::Graphics::Gdi::CreateCompatibleBitmap(hdc, 1, 1);
                let old = SelectObject(mem, bmp.into());
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
                let _ = DeleteObject(bmp.into());
                let _ = windows::Win32::Graphics::Gdi::DeleteDC(mem);
            }
        }
        let _ = DeleteObject(shade.into());

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
        let _ = DeleteObject(acc.into());

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
        let old_brush = SelectObject(hdc, playhead.into());
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
        let _ = DeleteObject(playhead.into());

        // Playhead + selected-range label.
        let whole_secs = |t: i64| (t / 10_000_000) as u64;
        let tenths = |t: i64| ((t.max(0) / 1_000_000) % 10) as u64;
        let selected_seconds = whole_secs(state.trim_end - state.trim_start).max(1);
        let duration_tail = speed_time_map(state)
            .filter(|map| !map.is_empty())
            .map(|map| {
                format!(
                    "{selected_seconds}s \u{2192} {}s output",
                    whole_secs(map.output_duration()).max(1)
                )
            })
            .unwrap_or_else(|| format!("{selected_seconds}s selected"));
        let label = format!(
            "{:02}:{:02}.{}     Trim  {:02}:{:02} \u{2013} {:02}:{:02}     {}",
            whole_secs(state.playhead) / 60,
            whole_secs(state.playhead) % 60,
            tenths(state.playhead),
            whole_secs(state.trim_start) / 60,
            whole_secs(state.trim_start) % 60,
            whole_secs(state.trim_end) / 60,
            whole_secs(state.trim_end) % 60,
            duration_tail
        );
        SelectObject(hdc, state.font_small.into());
        SetTextColor(hdc, state.theme.muted);
        let mut l = wide(&label);
        let mut lr = RECT {
            left: strip.left,
            top: strip.bottom + s(state, 6),
            right: if state.selected_speed.is_some() {
                state
                    .speed_rate_controls
                    .first()
                    .map_or(strip.right, |(rect, _)| rect.left)
                    - s(state, 8)
            } else {
                state.speed_add_control.left - s(state, 8)
            },
            bottom: strip.bottom + s(state, 28),
        };
        DrawTextW(hdc, &mut l, &mut lr, DT_LEFT | DT_SINGLELINE | DT_VCENTER);

        if let Some(index) = state
            .selected_speed
            .filter(|index| *index < state.speed_ranges.len())
        {
            let selected_rate = state.speed_ranges[index].rate;
            for (rect, rate) in &state.speed_rate_controls {
                paint_chip(
                    hdc,
                    *rect,
                    &format!("{rate}\u{00d7}"),
                    *rate == selected_rate,
                    state,
                );
            }
            paint_chip(hdc, state.speed_remove_control, "Remove", false, state);
        } else {
            paint_chip(
                hdc,
                state.speed_add_control,
                "+ Speed",
                state.speed_armed,
                state,
            );
        }
    }

    // A tool that arms and then waits has to say what it is waiting for. Both
    // of these did neither: the chip filled with accent to show it was armed,
    // and nothing named the gesture that follows or how to finish. A real
    // status still wins — an export result or an error outranks a reminder.
    let hint = if state.crop_edit.is_some() {
        Some("drag to frame \u{00b7} Apply when done \u{00b7} Esc cancels")
    } else if state.speed_armed {
        Some("drag across the filmstrip to pick the stretch")
    } else {
        None
    };
    if let Some(msg) = state.status.as_deref().or(hint) {
        SelectObject(hdc, state.font_small.into());
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
        let ob = SelectObject(hdc, fill.into());
        let op = SelectObject(hdc, pen.into());
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
        let _ = DeleteObject(fill.into());
        let _ = DeleteObject(pen.into());
        SelectObject(hdc, state.font.into());
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
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
            LRESULT(0)
        }
        WM_EXPORT_DONE => {
            let Some(done) = EXPORT_COMPLETIONS.take(lparam.0 as u64, hwnd.0 as isize) else {
                return LRESULT(0);
            };
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
                            state.status =
                                Some(match crate::output::file_to_clipboard(&done.path) {
                                    Ok(()) => format!("saved {name} · edit on clipboard"),
                                    Err(error) => {
                                        crate::diagnostics::log(
                                            "video export clipboard copy failed",
                                        );
                                        eprintln!("video export clipboard copy failed: {error:#}");
                                        format!("saved {name} · clipboard unavailable")
                                    }
                                });
                            state.exported = Some(done.path.clone());
                        }
                        Err(error) if error == "export cancelled" => {
                            crate::diagnostics::log("video export cancelled");
                            state.status = Some(export_failure_status(error));
                        }
                        Err(error) => {
                            crate::diagnostics::log("video export failed");
                            state.status = Some(export_failure_status(error));
                        }
                    }
                    close = state.close_after_export;
                    state.close_after_export = false;
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
            if close {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        crate::share::WM_SHARE_COMPLETE => {
            let Some(completion) = crate::share::take_completion(lparam.0 as u64, hwnd.0 as isize)
            else {
                return LRESULT(0);
            };
            let is_current = state_of(hwnd).is_some_and(|state| {
                crate::share::accept_completion(&mut state.share_request_id, completion.request_id)
            });
            if !is_current {
                return LRESULT(0);
            }
            if let Some(state) = state_of(hwnd) {
                state.sharing = false;
                let message = match completion.outcome {
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
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_PROBE_READY => {
            let Some(probe) = PROBE_COMPLETIONS.take(lparam.0 as u64, hwnd.0 as isize) else {
                return LRESULT(0);
            };
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
                let _ = InvalidateRect(Some(hwnd), None, false);
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
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
            LRESULT(0)
        }
        WM_SCRUB_FRAME => {
            let Some(frame) = SCRUB_COMPLETIONS.take(lparam.0 as u64, hwnd.0 as isize) else {
                return LRESULT(0);
            };
            if let Some(state) = state_of(hwnd) {
                // Playback owns the preview while it runs, and a frame for a
                // position the user has already scrubbed past is stale.
                if !state.playing && frame.0 == state.scrub_generation {
                    state.preview_raw = Some((frame.1, frame.2, frame.3));
                    recompose_preview(state);
                    let _ = InvalidateRect(Some(hwnd), None, false);
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
                        let _ = InvalidateRect(Some(hwnd), None, false);
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
                        let _ = InvalidateRect(Some(hwnd), None, false);
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
                let mem = windows::Win32::Graphics::Gdi::CreateCompatibleDC(Some(hdc));
                let bmp = windows::Win32::Graphics::Gdi::CreateCompatibleBitmap(
                    hdc,
                    state.width,
                    state.height,
                );
                let old = SelectObject(mem, bmp.into());
                paint(mem, state);
                let _ = windows::Win32::Graphics::Gdi::BitBlt(
                    hdc,
                    0,
                    0,
                    state.width,
                    state.height,
                    Some(mem),
                    0,
                    0,
                    windows::Win32::Graphics::Gdi::SRCCOPY,
                );
                SelectObject(mem, old);
                let _ = DeleteObject(bmp.into());
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
                if let Some(drag) = state.crop_drag {
                    if let Some(point) = screen_to_preview_clamped(state, x, y) {
                        if let Some(pending) = state.crop_edit {
                            let (next, drag) = match drag {
                                CropDrag::New(start) => {
                                    (crate::video_edit::Crop::from_points(start, point), drag)
                                }
                                CropDrag::Move(last) => (
                                    pending.moved(point.0 - last.0, point.1 - last.1),
                                    CropDrag::Move(point),
                                ),
                                CropDrag::Corner(corner) => {
                                    let (next, held) = pending.resized(corner, point);
                                    (next, CropDrag::Corner(held))
                                }
                            };
                            state.crop_drag = Some(drag);
                            if state.crop_edit != Some(next) {
                                state.crop_edit = Some(next);
                                let _ = InvalidateRect(Some(hwnd), None, false);
                            }
                        }
                    }
                    return LRESULT(0);
                }
                if let Some(drag) = state.dragging {
                    match drag {
                        Drag::Trim(handle) => {
                            set_handle(state, handle, x);
                            state.playhead = state.playhead.clamp(state.trim_start, state.trim_end);
                            refresh_preview(state);
                        }
                        Drag::Speed { index, handle } => set_speed_handle(state, index, handle, x),
                        Drag::SpeedNew { index, anchor } => {
                            set_new_speed_range(state, index, anchor, x)
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
                                let frame = annotation_frame(state);
                                if let Some(item) = state.annotations.get_mut(index) {
                                    crate::video_edit::translate(item, dx, dy, frame);
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
                    let _ = InvalidateRect(Some(hwnd), None, false);
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
                    let _ = InvalidateRect(Some(hwnd), None, false);
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
                let _ = SetFocus(Some(hwnd));
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
                // Clicking anywhere accepts the current caption, returns to
                // Select, and then continues handling that same click. Enter
                // remains a convenient shortcut, never a requirement.
                if state.text_entry.is_some() {
                    commit_text(state);
                }
                if state.selected_speed.is_none() && contains(state.speed_add_control, x, y) {
                    stop_playback(state);
                    state.speed_armed = !state.speed_armed;
                    state.tool = None;
                    state.tools_open = false;
                    state.selected = None;
                    state.status = state
                        .speed_armed
                        .then(|| "drag across the timeline to speed up that section".into());
                    let _ = InvalidateRect(Some(hwnd), None, false);
                    return LRESULT(0);
                }
                if let Some(index) = state
                    .selected_speed
                    .filter(|index| *index < state.speed_ranges.len())
                {
                    if let Some((_, rate)) = state
                        .speed_rate_controls
                        .iter()
                        .find(|(rect, _)| contains(*rect, x, y))
                        .copied()
                    {
                        stop_playback(state);
                        if state.speed_ranges[index].rate != rate {
                            push_undo(state);
                            state.speed_ranges[index].rate = rate;
                        }
                        state.status = Some("sped sections are muted during export".into());
                        let _ = InvalidateRect(Some(hwnd), None, false);
                        return LRESULT(0);
                    }
                    if contains(state.speed_remove_control, x, y) {
                        stop_playback(state);
                        push_undo(state);
                        state.speed_ranges.remove(index);
                        state.selected_speed = None;
                        state.status = None;
                        let _ = InvalidateRect(Some(hwnd), None, false);
                        return LRESULT(0);
                    }
                }
                if contains(state.padding_slider, x, y) {
                    state.dragging = Some(Drag::Padding);
                    update_padding(state, x);
                    windows::Win32::UI::Input::KeyboardAndMouse::SetCapture(hwnd);
                    let _ = InvalidateRect(Some(hwnd), None, false);
                    return LRESULT(0);
                }
                if contains(state.add_control, x, y) {
                    stop_playback(state);
                    // Settle the frame before opening the drawer over it.
                    // `enter_crop` closes the drawer for a reason — it is
                    // hit-tested ahead of the preview, so reopening it while
                    // cropping would take the clicks meant for the rectangle
                    // and arm a tool that could never draw.
                    commit_crop(state);
                    state.selected_speed = None;
                    state.speed_armed = false;
                    state.tools_open = !state.tools_open;
                    if !state.tools_open {
                        state.tool = None;
                        state.text_entry = None;
                        recompose_preview(state);
                    }
                    let _ = InvalidateRect(Some(hwnd), None, false);
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
                        let _ = InvalidateRect(Some(hwnd), None, false);
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
                        let _ = InvalidateRect(Some(hwnd), None, false);
                        return LRESULT(0);
                    }
                    if caption_controls_active(state) && contains(state.caption_size_slider, x, y) {
                        stop_playback(state);
                        if state.selected.is_some() {
                            push_undo(state);
                        }
                        state.dragging = Some(Drag::CaptionSize);
                        update_caption_size(state, x);
                        windows::Win32::UI::Input::KeyboardAndMouse::SetCapture(hwnd);
                        let _ = InvalidateRect(Some(hwnd), None, false);
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
                        let _ = InvalidateRect(Some(hwnd), None, false);
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
                            let _ = InvalidateRect(Some(hwnd), None, false);
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
                            let _ = InvalidateRect(Some(hwnd), None, false);
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
                            let _ = InvalidateRect(Some(hwnd), None, false);
                            return LRESULT(0);
                        }
                    }
                    if contains(state.undo_control, x, y) {
                        stop_playback(state);
                        undo(state);
                        let _ = InvalidateRect(Some(hwnd), None, false);
                        return LRESULT(0);
                    }
                    if contains(state.delete_control, x, y) {
                        stop_playback(state);
                        delete_selected(state);
                        let _ = InvalidateRect(Some(hwnd), None, false);
                        return LRESULT(0);
                    }
                    if contains(tool_panel(state), x, y) {
                        return LRESULT(0);
                    }
                }

                // Cropping owns the preview while it is armed: nothing is drawn
                // or selected until the frame is settled.
                // Cropping owns the preview, but only the preview: returning
                // for every click would leave the padding slider and anything
                // else that acts on button-down dead until the frame is
                // settled.
                if let Some(pending) = state.crop_edit {
                    if let Some(point) = screen_to_preview(state, x, y) {
                        stop_playback(state);
                        // Corners win over the interior so the handles stay
                        // reachable, and a crop covering everything sweeps a
                        // new rectangle rather than trying to move — there is
                        // nowhere for it to go, and "drag a box round the part
                        // you want" is the whole gesture on a fresh crop.
                        let (tol_x, tol_y) = preview_content_rect(state)
                            .map(|content| crop_grab_tolerance(content, s(state, 10)))
                            .unwrap_or((0.02, 0.02));
                        let corner = pending.corners().iter().position(|corner| {
                            (point.0 - corner.0).abs() <= tol_x
                                && (point.1 - corner.1).abs() <= tol_y
                        });
                        state.crop_drag = Some(match corner {
                            Some(index) => CropDrag::Corner(index as u8),
                            None if pending.contains(point)
                                && pending != crate::video_edit::Crop::FULL =>
                            {
                                CropDrag::Move(point)
                            }
                            None => CropDrag::New(point),
                        });
                        windows::Win32::UI::Input::KeyboardAndMouse::SetCapture(hwnd);
                        let _ = InvalidateRect(Some(hwnd), None, false);
                        return LRESULT(0);
                    }
                }
                if let Some(point) = screen_to_preview(state, x, y) {
                    stop_playback(state);
                    state.selected_speed = None;
                    state.speed_armed = false;
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
                            state.dragging = Some(Drag::Draw {
                                index,
                                start: point,
                            });
                            windows::Win32::UI::Input::KeyboardAndMouse::SetCapture(hwnd);
                            recompose_preview(state);
                        }
                        let _ = InvalidateRect(Some(hwnd), None, false);
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
                    let _ = InvalidateRect(Some(hwnd), None, false);
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
                    if state.speed_armed {
                        stop_playback(state);
                        let anchor =
                            timeline_time(state, x).clamp(state.trim_start, state.trim_end);
                        push_undo(state);
                        if let Some(index) = add_speed_range(state, anchor) {
                            state.selected_speed = Some(index);
                            state.speed_armed = false;
                            state.status = Some("sped sections are muted during export".into());
                            state.dragging = Some(Drag::SpeedNew { index, anchor });
                            windows::Win32::UI::Input::KeyboardAndMouse::SetCapture(hwnd);
                        } else {
                            state.undo.pop();
                            state.status =
                                Some("there is no room for another speed section here".into());
                        }
                        let _ = InvalidateRect(Some(hwnd), None, false);
                        return LRESULT(0);
                    }
                    if y <= strip.top + s(state, 22) {
                        if let Some(index) = state
                            .selected_speed
                            .filter(|index| *index < state.speed_ranges.len())
                        {
                            let range = state.speed_ranges[index];
                            let start_x = to_x(range.start);
                            let end_x = to_x(range.end);
                            let handle = if (x - start_x).abs() <= grab {
                                Some(Handle::Start)
                            } else if (x - end_x).abs() <= grab {
                                Some(Handle::End)
                            } else {
                                None
                            };
                            if let Some(handle) = handle {
                                stop_playback(state);
                                push_undo(state);
                                state.dragging = Some(Drag::Speed { index, handle });
                                windows::Win32::UI::Input::KeyboardAndMouse::SetCapture(hwnd);
                                let _ = InvalidateRect(Some(hwnd), None, false);
                                return LRESULT(0);
                            }
                        }
                        if let Some(index) = speed_range_at(state, x, y) {
                            stop_playback(state);
                            state.selected_speed = Some(index);
                            state.selected = None;
                            state.playhead = timeline_time(state, x);
                            refresh_preview(state);
                            request_scrub_frame(state);
                            let _ = InvalidateRect(Some(hwnd), None, false);
                            return LRESULT(0);
                        }
                    }
                    state.selected_speed = None;
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
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
            LRESULT(0)
        }
        WM_RBUTTONDOWN => {
            if let Some(state) = state_of(hwnd) {
                // Cropping owns the preview. Without this a right-click puts
                // the tool away and reopens the drawer over the rectangle,
                // which is the guard the photo editor already has.
                if state.crop_edit.is_some() {
                    return LRESULT(0);
                }
                let _ = SetFocus(Some(hwnd));
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
                        let _ = InvalidateRect(Some(hwnd), None, false);
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
                // The up belongs to the crop drag whether the drag ended here or
                // was already ended by a key; either way it goes no further.
                if take_crop_click(state) {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                    return LRESULT(0);
                }
                if let Some(drag) = state.dragging.take() {
                    let _ = windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture();
                    if matches!(
                        drag,
                        Drag::Trim(_) | Drag::Speed { .. } | Drag::SpeedNew { .. } | Drag::Playhead
                    ) {
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
                    let _ = InvalidateRect(Some(hwnd), None, false);
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
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    return LRESULT(0);
                }
                if contains(state.crop_control, x, y) {
                    // Pressing it again applies; only Esc throws the frame away.
                    if state.crop_edit.is_some() {
                        commit_crop(state);
                    } else {
                        enter_crop(state);
                    }
                    state.status = None;
                    let _ = InvalidateRect(Some(hwnd), None, false);
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
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    return LRESULT(0);
                }
                if let Some(i) = state
                    .controls
                    .iter()
                    .position(|(r, ..)| x >= r.left && x < r.right && y >= r.top && y < r.bottom)
                {
                    // Any action settles a pending crop first, so Export edit
                    // produces the frame that is on screen rather than the last
                    // committed one — the same rule the photo editor uses for
                    // Copy and Save.
                    commit_crop(state);
                    match state.controls[i].1 {
                        Act::Play => toggle_playback(hwnd, state),
                        Act::Reveal => crate::output::reveal_in_explorer(&state.mp4),
                        Act::Copy => {
                            state.status =
                                Some(match crate::output::file_to_clipboard(&state.mp4) {
                                    Ok(()) => "original copied to clipboard".into(),
                                    Err(error) => {
                                        crate::diagnostics::log("video clipboard copy failed");
                                        eprintln!("video clipboard copy failed: {error:#}");
                                        "could not copy original to the clipboard".into()
                                    }
                                });
                            let _ = InvalidateRect(Some(hwnd), None, false);
                        }
                        // Shares the original recording, same as Copy and Show
                        // in folder always act on the original rather than an
                        // edit export — the recorded file is what these three
                        // buttons agree the "real" artifact is.
                        Act::Share => {
                            if let crate::share::ShareStart::Unavailable(reason) =
                                crate::share::share_start()
                            {
                                state.status = Some(reason.into());
                                let _ = InvalidateRect(Some(hwnd), None, false);
                                return LRESULT(0);
                            }
                            // A second click while one upload is already in
                            // flight would start a redundant upload and let
                            // whichever WM_SHARE_COMPLETE lands last silently
                            // win. A dedicated flag rather than checking
                            // `status` directly: that string gets overwritten
                            // by unrelated handlers while the upload runs.
                            // History and tweak now use the same idle rule
                            // on `pending_share` (SBS-1075).
                            if start_share_upload(
                                &mut state.sharing,
                                &mut state.share_request_id,
                                || crate::share::share_in_background(hwnd, state.mp4.clone()),
                            ) {
                                state.status = Some("Sharing\u{2026}".into());
                                let _ = InvalidateRect(Some(hwnd), None, false);
                            }
                        }
                        Act::Delete => {
                            if state.exporting {
                                state.status =
                                    Some("finish the current export before deleting".into());
                                let _ = InvalidateRect(Some(hwnd), None, false);
                                return LRESULT(0);
                            }
                            let prompt = HSTRING::from(recording_delete_prompt(
                                &state.mp4,
                                state.gif.as_deref(),
                            ));
                            let confirmed = MessageBoxW(
                                Some(hwnd),
                                PCWSTR(prompt.as_ptr()),
                                w!("Matteshot"),
                                MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2,
                            ) == IDYES;
                            if !confirmed {
                                return LRESULT(0);
                            }
                            stop_playback(state);
                            if let Err(error) =
                                delete_recording_files(&state.mp4, state.gif.as_deref())
                            {
                                crate::diagnostics::log(&format!(
                                    "recording could not be deleted: {error:#}"
                                ));
                                state.status =
                                    Some("could not delete recording · original kept".into());
                                let _ = InvalidateRect(Some(hwnd), None, false);
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
                                let _ = InvalidateRect(Some(hwnd), None, false);
                                return LRESULT(0);
                            }
                            stop_playback(state);
                            let style = state.styles[state.matte_index].clone();
                            let has_matte = !crate::compose::is_plain(&style);
                            let has_trim = state.trim_start > 0 || state.trim_end < state.duration;
                            let style_slug = style.name.to_ascii_lowercase();
                            let has_annotations = !state.annotations.is_empty();
                            let has_speed = !state.speed_ranges.is_empty();
                            let mut suffix = match (has_matte, has_trim) {
                                (true, true) => format!("{style_slug}-trim"),
                                (true, false) => style_slug,
                                (false, true) => "trim".into(),
                                (false, false) => "copy".into(),
                            };
                            if has_annotations {
                                suffix.push_str("-edit");
                            }
                            if has_speed {
                                suffix.push_str("-speed");
                            }
                            if state.crop != crate::video_edit::Crop::FULL {
                                suffix.push_str("-crop");
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
                                "exporting {}{}{}{} \u{00b7} 0%",
                                style.name,
                                if has_trim { " + trim" } else { "" },
                                annotation_label,
                                if has_speed { " + speed sections" } else { "" }
                            ));
                            state.exporting = true;
                            crate::diagnostics::log("video export start");
                            let export_id = NEXT_EXPORT_ID.fetch_add(1, Ordering::Relaxed);
                            let export_cancel = Arc::new(AtomicBool::new(false));
                            state.export_id = Some(export_id);
                            state.export_cancel = Some(export_cancel.clone());
                            state.close_after_export = false;
                            state.export_stalled = false;
                            let _ = InvalidateRect(Some(hwnd), None, false);
                            let _ = windows::Win32::Graphics::Gdi::UpdateWindow(hwnd);
                            let src = state.mp4.clone();
                            let start = state.trim_start;
                            let end = state.trim_end;
                            let annotations = state.annotations.clone();
                            let speed_ranges = state.speed_ranges.clone();
                            let compose_opts = state_compose_opts(state);
                            let crop = state.crop;
                            let hwnd_raw = hwnd.0 as isize;
                            let mailbox_generation = EXPORT_COMPLETIONS.generation_of(hwnd_raw);
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
                                                Some(HWND(hwnd_raw as *mut _)),
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
                                let encoded = crate::trim::cut_with_speed_edit_progress_cancel(
                                    &src,
                                    &temporary,
                                    start,
                                    end,
                                    Some((&style, &compose_opts)),
                                    &annotations,
                                    &speed_ranges,
                                    crop,
                                    &export_cancel,
                                    |percent| {
                                        *activity.lock().unwrap() = std::time::Instant::now();
                                        if percent != last_progress {
                                            last_progress = percent;
                                            unsafe {
                                                let _ = PostMessageW(
                                                    Some(HWND(hwnd_raw as *mut _)),
                                                    WM_EXPORT_PROGRESS,
                                                    WPARAM(percent as usize),
                                                    LPARAM(export_id as isize),
                                                );
                                            }
                                        }
                                    },
                                );
                                // Unavailable / rename-after-validate keep the
                                // `.partial` (SBS-923). Cancel, encode failure,
                                // and proven-undecodable bytes still delete.
                                let result = match encoded {
                                    Err(error) => Err(dispose_export_partial(
                                        &temporary,
                                        ExportCleanup::Encode(format!("{error:#}")),
                                    )),
                                    Ok(()) if export_cancel.load(Ordering::Relaxed) => {
                                        Err(dispose_export_partial(
                                            &temporary,
                                            ExportCleanup::Cancelled,
                                        ))
                                    }
                                    Ok(()) => match crate::trim::validate_video(&temporary) {
                                        Err(error) => Err(dispose_export_partial(
                                            &temporary,
                                            ExportCleanup::Validate(error),
                                        )),
                                        Ok(()) => match std::fs::rename(&temporary, &dst) {
                                            Err(error) => Err(dispose_export_partial(
                                                &temporary,
                                                ExportCleanup::Rename(error),
                                            )),
                                            Ok(()) => Ok(()),
                                        },
                                    },
                                };
                                if com.is_ok() {
                                    unsafe { CoUninitialize() };
                                }
                                finished.store(true, Ordering::Relaxed);
                                let done = ExportDone {
                                    id: export_id,
                                    path: dst,
                                    result,
                                };
                                unsafe {
                                    let target = HWND(hwnd_raw as *mut _);
                                    EXPORT_COMPLETIONS.post_with_at(
                                        hwnd_raw,
                                        mailbox_generation,
                                        done,
                                        |token| {
                                            crate::window::has_class(target, "matteshot_recdone")
                                                && PostMessageW(
                                                    Some(target),
                                                    WM_EXPORT_DONE,
                                                    WPARAM(0),
                                                    LPARAM(token as isize),
                                                )
                                                .is_ok()
                                        },
                                    );
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
                let _ = SetFocus(Some(hwnd));
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
                            let _ = InvalidateRect(Some(hwnd), None, false);
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
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            if let Some(state) = state_of(hwnd) {
                // Cropping owns Esc, Enter and Delete while it is armed: the
                // rectangle on screen is the only thing the user is looking at.
                if state.crop_edit.is_some() {
                    match wparam.0 as u16 {
                        key if key == VK_ESCAPE.0 => {
                            cancel_crop(state);
                            let _ = InvalidateRect(Some(hwnd), None, false);
                            return LRESULT(0);
                        }
                        key if key == VK_RETURN.0 => {
                            commit_crop(state);
                            let _ = InvalidateRect(Some(hwnd), None, false);
                            return LRESULT(0);
                        }
                        // Back to the whole recording, ready to apply as "no
                        // crop" or to re-frame from scratch.
                        key if key == VK_DELETE.0 || key == 0x08 => {
                            end_crop_drag(state);
                            state.crop_edit = Some(crate::video_edit::Crop::FULL);
                            let _ = InvalidateRect(Some(hwnd), None, false);
                            return LRESULT(0);
                        }
                        // Ctrl+Z means "undo the frame I am drawing". Letting it
                        // reach the editor's undo restored a crop the armed
                        // preview cannot show, and putting the tool away then
                        // re-applied the pending one over the top of it.
                        key if key == 0x5A
                            && windows::Win32::UI::Input::KeyboardAndMouse::GetKeyState(
                                VK_CONTROL.0 as i32,
                            ) < 0 =>
                        {
                            cancel_crop(state);
                            let _ = InvalidateRect(Some(hwnd), None, false);
                            return LRESULT(0);
                        }
                        // Cropping swallows every preview click, so an
                        // annotation tool armed from here could never draw:
                        // the chip would light up and nothing would happen.
                        // A/R/T/B/P, the tool shortcuts handled below.
                        0x41 | 0x52 | 0x54 | 0x42 | 0x50 => return LRESULT(0),
                        _ => {}
                    }
                }
                match wparam.0 as u16 {
                    key if key == VK_ESCAPE.0 => {
                        if state.playing {
                            stop_playback(state);
                            state.status = Some("paused".into());
                            let _ = InvalidateRect(Some(hwnd), None, false);
                        } else if state.text_entry.is_some() {
                            state.text_entry = None;
                            recompose_preview(state);
                            let _ = InvalidateRect(Some(hwnd), None, false);
                        } else if state.speed_armed || state.selected_speed.is_some() {
                            state.speed_armed = false;
                            state.selected_speed = None;
                            state.status = None;
                            let _ = InvalidateRect(Some(hwnd), None, false);
                        } else if state.tool.is_some() || state.tools_open {
                            state.tool = None;
                            state.tools_open = false;
                            let _ = InvalidateRect(Some(hwnd), None, false);
                        } else if state.selected.is_some() {
                            state.selected = None;
                            let _ = InvalidateRect(Some(hwnd), None, false);
                        } else {
                            let _ = DestroyWindow(hwnd);
                        }
                    }
                    key if key == VK_SPACE.0 => {
                        if state.text_entry.is_none() {
                            // Settles the frame first, as the Play chip does.
                            // Otherwise the same gesture gets two answers, and
                            // playback runs the whole recording underneath a
                            // crop overlay that says otherwise.
                            commit_crop(state);
                            toggle_playback(hwnd, state);
                        }
                    }
                    key if key == VK_DELETE.0 => {
                        stop_playback(state);
                        delete_selected(state);
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    0x5A if GetKeyState(VK_CONTROL.0 as i32) < 0 => {
                        stop_playback(state);
                        undo(state);
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    0x41 if state.text_entry.is_none() => {
                        state.tools_open = true;
                        select_annotation_tool(state, Tool::Arrow);
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    0x52 if state.text_entry.is_none() => {
                        state.tools_open = true;
                        select_annotation_tool(state, Tool::Rect);
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    0x54 if state.text_entry.is_none() => {
                        state.tools_open = true;
                        select_annotation_tool(state, Tool::Text);
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    0x42 if state.text_entry.is_none() => {
                        state.tools_open = true;
                        select_annotation_tool(state, Tool::Blur);
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    0x50 if state.text_entry.is_none() => {
                        state.tools_open = true;
                        select_annotation_tool(state, Tool::Pen);
                        let _ = InvalidateRect(Some(hwnd), None, false);
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
                        let _ = InvalidateRect(Some(hwnd), None, false);
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
                        let _ = InvalidateRect(Some(hwnd), None, false);
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
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    key if key == VK_END.0 => {
                        stop_playback(state);
                        state.playhead = state.trim_end;
                        refresh_preview(state);
                        request_scrub_frame(state);
                        let _ = InvalidateRect(Some(hwnd), None, false);
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
                    let next = layout(state.scale, w, h, state.styles.len(), state.can_share);
                    state.crop_control = next.crop_control;
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
                    state.speed_add_control = next.speed_add_control;
                    state.speed_rate_controls = next.speed_rate_controls;
                    state.speed_remove_control = next.speed_remove_control;
                    state.preview_rect = next.preview;
                    state.strip = next.strip;
                    let _ = InvalidateRect(Some(hwnd), None, true);
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
                    RECT {
                        left: 0,
                        top: 0,
                        right: minimum_w,
                        bottom: minimum_h,
                    },
                    editor_style(),
                    WS_EX_APPWINDOW,
                    s,
                );
                (*mmi).ptMinTrackSize.x = outer.right - outer.left;
                (*mmi).ptMinTrackSize.y = outer.bottom - outer.top;
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
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            if let Some(state) = state_of(hwnd) {
                if state.exporting {
                    if state.export_stalled {
                        let answer = MessageBoxW(
                            Some(hwnd),
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
                        Some(hwnd),
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
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    return LRESULT(0);
                }
                stop_playback(state);
            }
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_NCDESTROY => {
            crate::share::discard_window(hwnd.0 as isize);
            EXPORT_COMPLETIONS.unbind(hwnd.0 as isize);
            PROBE_COMPLETIONS.unbind(hwnd.0 as isize);
            SCRUB_COMPLETIONS.unbind(hwnd.0 as isize);
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
                let _ = DeleteObject(state.font.into());
                let _ = DeleteObject(state.font_small.into());
                let _ = DeleteObject(state.font_big.into());
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
    unsafe {
        let _ = GetCursorPos(&mut cursor);
    }
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
    let cw = ((work_w as f32 * 0.85) as i32).clamp(sc(760).min(work_w - sc(40)), work_w - sc(40));
    let ch = ((work_h as f32 * 0.85) as i32).clamp(sc(560).min(work_h - sc(40)), work_h - sc(40));
    let window_x = monitor_info.rcWork.left + (work_w - cw) / 2;
    let window_y = monitor_info.rcWork.top + (work_h - ch) / 2;

    // Filmstrip: best-effort, never blocks showing the window.
    let (probe_w, probe_h, preview_w, preview_h) = {
        let initial = layout(scale, cw, ch, 7, true);
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
    let thumbs = matte_thumbs(
        source_size,
        &raw_thumbs,
        &styles[matte_index],
        &opts,
        crate::video_edit::Crop::FULL,
    );

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

    let initial = layout(scale, cw, ch, styles.len(), crate::share::available());
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
        crop: crate::video_edit::Crop::FULL,
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
        crop_control: initial.crop_control,
        crop_edit: None,
        crop_drag: None,
        crop_click_owed: false,
        preview_content_size: source_size,
        annotations: Vec::new(),
        speed_ranges: Vec::new(),
        selected_speed: None,
        speed_armed: false,
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
        speed_add_control: initial.speed_add_control,
        speed_rate_controls: initial.speed_rate_controls,
        speed_remove_control: initial.speed_remove_control,
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
        share_request_id: None,
        can_share: crate::share::available(),
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
            Some(hinstance.into()),
            Some(state as *const _),
        ) {
            Ok(hwnd) => {
                crate::theme::apply_titlebar(hwnd, &(*state).theme);
                let _ = SetForegroundWindow(hwnd);
                let _ = SetFocus(Some(hwnd));
                // Live scrubbing: one long-lived decoder chasing the playhead.
                let (scrub_tx, scrub_rx) = std::sync::mpsc::channel();
                (*state).scrub_tx = Some(scrub_tx);
                {
                    let source = (*state).mp4.clone();
                    let target = hwnd.0 as isize;
                    let mailbox_generation = SCRUB_COMPLETIONS.generation_of(target);
                    std::thread::spawn(move || {
                        let com = CoInitializeEx(None, COINIT_MULTITHREADED);
                        let result = crate::trim::scrub_worker(
                            &source,
                            preview_w,
                            preview_h,
                            scrub_rx,
                            |generation, bytes, fw, fh| {
                                let hwnd = HWND(target as *mut _);
                                let mut posted = false;
                                SCRUB_COMPLETIONS.post_with_at(
                                    target,
                                    mailbox_generation,
                                    (generation, bytes, fw, fh),
                                    |token| {
                                        posted =
                                            crate::window::has_class(hwnd, "matteshot_recdone")
                                                && PostMessageW(
                                                    Some(hwnd),
                                                    WM_SCRUB_FRAME,
                                                    WPARAM(0),
                                                    LPARAM(token as isize),
                                                )
                                                .is_ok();
                                        posted
                                    },
                                );
                                posted
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
                let mailbox_generation = PROBE_COMPLETIONS.generation_of(target);
                std::thread::spawn(move || {
                    let com = CoInitializeEx(None, COINIT_MULTITHREADED);
                    let started = std::time::Instant::now();
                    let probed =
                        crate::trim::probe_editor(&source, probe_w, probe_h, preview_w, preview_h);
                    eprintln!("timing: filmstrip + scrub cache in {:?}", started.elapsed());
                    if com.is_ok() {
                        CoUninitialize();
                    }
                    let Ok(probed) = probed else {
                        crate::diagnostics::log("editor filmstrip probe failed");
                        return;
                    };
                    let hwnd = HWND(target as *mut _);
                    PROBE_COMPLETIONS.post_with_at(target, mailbox_generation, probed, |token| {
                        crate::window::has_class(hwnd, "matteshot_recdone")
                            && PostMessageW(
                                Some(hwnd),
                                WM_PROBE_READY,
                                WPARAM(0),
                                LPARAM(token as isize),
                            )
                            .is_ok()
                    });
                });
            }
            Err(_) => drop(Box::from_raw(state)),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_preview_frames_exactly_what_the_encoder_keeps() {
        use image::{Rgba, RgbaImage};

        // An odd source, where the encoder's even rounding and a naive
        // proportional one disagree: half of 321 is 160.5, which the export
        // keeps as 160. A preview that rounded for itself would show 161
        // source pixels' worth and promise a sliver the file does not hold.
        let source = (321u32, 241u32);
        let crop = crate::video_edit::Crop {
            x: 0.0,
            y: 0.0,
            w: 0.5,
            h: 0.5,
        };
        let (_, _, kept_w, kept_h) = crop.pixel_rect(source.0, source.1);
        assert_eq!((kept_w, kept_h), (160, 120), "the encoder's rounding");

        // A preview decoded at the source's own size must frame exactly that.
        let full = RgbaImage::from_pixel(source.0, source.1, Rgba([1, 2, 3, 255]));
        let framed = super::crop_frame(&full, crop, source);
        assert_eq!(framed.dimensions(), (kept_w, kept_h));

        // And at half size it frames the same region, proportionally.
        let half = RgbaImage::from_pixel(source.0 / 2, source.1 / 2, Rgba([1, 2, 3, 255]));
        let framed = super::crop_frame(&half, crop, source);
        let expected = (
            (kept_w as f32 * (half.width() as f32 / source.0 as f32)).round() as u32,
            (kept_h as f32 * (half.height() as f32 / source.1 as f32)).round() as u32,
        );
        assert_eq!(framed.dimensions(), expected);

        // The whole recording is still handed back untouched.
        assert_eq!(
            super::crop_frame(&full, crate::video_edit::Crop::FULL, source).dimensions(),
            source
        );
    }

    #[test]
    fn the_crop_chip_closes_the_settings_row_without_crowding_the_aspects() {
        // Real editor sizes, including the wide one this was first driven at.
        for (scale, cw, ch) in [(1.0, 1280, 760), (1.36, 2176, 1183), (1.0, 900, 640)] {
            let l = super::layout(scale, cw, ch, 7, true);
            let m = (20.0 * scale) as i32;
            assert!(
                l.crop_control.right <= cw - m,
                "{cw}x{ch} @{scale}: crop chip runs off the right edge"
            );
            assert!(
                l.crop_control.left > l.padding_slider.right,
                "{cw}x{ch} @{scale}: crop chip overlaps the padding slider"
            );
            // It shares the settings row with the aspect presets and must not
            // sit on top of the last one.
            let last_aspect = l
                .aspect_controls
                .last()
                .map(|(rect, _)| *rect)
                .expect("aspect presets are laid out");
            assert!(
                last_aspect.right <= l.crop_control.left,
                "{cw}x{ch} @{scale}: aspect chips run into the crop chip \
                 ({} > {})",
                last_aspect.right,
                l.crop_control.left
            );
            assert_eq!(
                l.crop_control.top, last_aspect.top,
                "{cw}x{ch} @{scale}: crop chip is off the settings row"
            );
            // Below the preview, not over it.
            assert!(
                l.crop_control.top >= l.preview.bottom,
                "{cw}x{ch} @{scale}: crop chip overlaps the preview"
            );
        }
    }

    use super::{
        add_chip_label, annotation_preview_time, apply_caption_input, available_export_path,
        delete_recording_files, delete_recording_files_with, dispose_export_partial,
        export_failure_status, layout, minimum_client_size, next_counter_number,
        recording_delete_prompt, speed_gap, tool_after_pick, CaptionInput, ExportCleanup,
        NEXT_EXPORT_ID, VIDEO_TOOLS,
    };
    use std::sync::atomic::Ordering;

    #[test]
    fn speed_section_gaps_are_bounded_by_the_active_trim() {
        use crate::video_speed::SpeedRange;

        let ranges = [
            SpeedRange::new(0, 15, 2),
            SpeedRange::new(25, 35, 4),
            SpeedRange::new(45, 100, 8),
        ];
        assert_eq!(speed_gap(&ranges, 0, 10, 80, true), (10, 25));
        assert_eq!(speed_gap(&ranges, 1, 10, 80, true), (15, 45));
        assert_eq!(speed_gap(&ranges, 2, 10, 80, true), (35, 80));
        assert_eq!(speed_gap(&ranges, 1, 10, 80, false), (15, 25));
    }

    #[test]
    fn maximized_1080p_layout_keeps_the_preview_dominant() {
        // A 1920x1080 display at 125% scaling leaves roughly a 1000px client
        // after the title bar/taskbar. Caption placement should still get the
        // majority of that height rather than a postage-stamp preview.
        let window = layout(1.25, 1920, 1000, 7, true);
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
        let window = layout(scale, requested_w, requested_h, 7, true);
        let preview_h = window.preview.bottom - window.preview.top;
        let timeline_h = window.strip.bottom - window.strip.top;
        assert!(
            preview_h >= (180.0 * scale) as i32,
            "preview was {preview_h}px"
        );
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
        assert!(matte_bottom + (6.0 * scale).round() as i32 <= window.aspect_controls[0].0.top);
        assert!(window.padding_slider.bottom < window.strip.top);
        assert!(window.tool_controls.iter().all(
            |(rect, ..)| rect.left >= window.preview.left && rect.right <= window.preview.right
        ));
        assert!(window.timing_controls.iter().all(|(rect, _)| {
            rect.top >= window.preview.top && rect.bottom <= window.preview.bottom
        }));
        assert!(window.strip.bottom < window.controls[0].0.top);
        assert!(window.speed_add_control.top > window.strip.bottom);
        assert!(window.speed_add_control.bottom <= window.controls[0].0.top);
        assert_eq!(
            window
                .speed_rate_controls
                .iter()
                .map(|(_, rate)| *rate)
                .collect::<Vec<_>>(),
            vec![2, 4, 8, 16]
        );
        assert!(window
            .speed_rate_controls
            .windows(2)
            .all(|pair| pair[0].0.right < pair[1].0.left));
        assert!(window
            .speed_rate_controls
            .last()
            .is_some_and(|(rect, _)| rect.right < window.speed_remove_control.left));
        assert!(window.speed_remove_control.right <= width);
        assert!(window
            .controls
            .iter()
            .all(|(rect, ..)| rect.bottom <= height));
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

    /// SBS-1075: Recdone already guards Share; this pins that a second
    /// click neither spawns nor overwrites `share_request_id`.
    #[test]
    fn recording_share_refuses_a_second_in_flight_request() {
        let mut sharing = false;
        let mut share_request_id = None;
        let mut uploads = Vec::new();
        assert!(super::start_share_upload(
            &mut sharing,
            &mut share_request_id,
            || {
                uploads.push(1);
                1
            }
        ));
        assert!(sharing);
        assert!(
            !super::start_share_upload(&mut sharing, &mut share_request_id, || {
                uploads.push(2);
                2
            }),
            "a second recording Share started another upload"
        );
        assert_eq!(uploads, [1]);
        assert_eq!(share_request_id, Some(1));
        assert!(!crate::share::accept_completion(&mut share_request_id, 2));
        assert_eq!(share_request_id, Some(1));
        assert!(crate::share::accept_completion(&mut share_request_id, 1));
        sharing = false;
        assert!(super::start_share_upload(
            &mut sharing,
            &mut share_request_id,
            || {
                uploads.push(3);
                3
            }
        ));
        assert_eq!(uploads, [1, 3]);
    }

    #[test]
    fn share_sits_between_copy_and_delete_without_overlap() {
        // Copy and Show in folder always act on the recorded original, and
        // Share follows the same rule (see the click handler), so it belongs
        // in the same row rather than the annotation tool panel.
        let window = layout(1.0, 1280, 720, 7, true);
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
        assert!(
            share_rect.right <= 1280,
            "Share runs off the minimum-width window"
        );
    }

    #[test]
    fn share_is_hidden_when_it_cannot_upload() {
        let window = layout(1.0, 1280, 720, 7, false);
        assert!(
            window
                .controls
                .iter()
                .all(|(_, act, _)| *act != super::Act::Share),
            "an editor that cannot upload must not show Share"
        );
    }

    #[test]
    fn caption_input_accepts_typing_without_requiring_enter() {
        let mut text = String::new();
        for ch in "Smooth caption 👍".chars() {
            assert_eq!(apply_caption_input(&mut text, ch), CaptionInput::Changed);
        }
        assert_eq!(text, "Smooth caption 👍");
        assert_eq!(
            apply_caption_input(&mut text, '\u{8}'),
            CaptionInput::Changed
        );
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

        assert_eq!(
            super::tool_for_shape(&Shape::Arrow {
                from: (0.0, 0.0),
                to: (1.0, 1.0)
            }),
            super::Tool::Arrow
        );
        assert_eq!(
            super::tool_for_shape(&Shape::Line {
                from: (0.0, 0.0),
                to: (1.0, 1.0)
            }),
            super::Tool::Line
        );
        assert_eq!(
            super::tool_for_shape(&Shape::Rect {
                a: (0.0, 0.0),
                b: (1.0, 1.0)
            }),
            super::Tool::Rect
        );
        assert_eq!(
            super::tool_for_shape(&Shape::Ellipse {
                a: (0.0, 0.0),
                b: (1.0, 1.0)
            }),
            super::Tool::Ellipse
        );
        assert_eq!(
            super::tool_for_shape(&Shape::Highlight {
                a: (0.0, 0.0),
                b: (1.0, 1.0)
            }),
            super::Tool::Highlight
        );
        assert_eq!(
            super::tool_for_shape(&Shape::Text {
                pos: (0.0, 0.0),
                text: String::new()
            }),
            super::Tool::Text
        );
        assert_eq!(
            super::tool_for_shape(&Shape::Blur {
                a: (0.0, 0.0),
                b: (1.0, 1.0)
            }),
            super::Tool::Blur
        );
        assert_eq!(
            super::tool_for_shape(&Shape::Counter {
                pos: (0.0, 0.0),
                n: 1
            }),
            super::Tool::Counter
        );
        assert_eq!(
            super::tool_for_shape(&Shape::Freehand {
                points: vec![(0.0, 0.0), (1.0, 1.0)]
            }),
            super::Tool::Pen
        );
    }

    #[test]
    fn video_editor_exposes_the_same_nine_annotation_tools_as_photo() {
        assert_eq!(
            VIDEO_TOOLS.map(|(_, label)| label),
            ["Arrow", "Line", "Box", "Oval", "Mark", "Text", "Blur", "Step", "Pen"]
        );
        let controls = layout(1.0, 1280, 720, 7, true).tool_controls;
        assert_eq!(controls.len(), VIDEO_TOOLS.len());
        for row in controls.chunks(3) {
            assert_eq!(row.len(), 3);
            assert!(row.windows(2).all(|pair| pair[0].0.right < pair[1].0.left));
        }
        assert!(controls
            .windows(4)
            .all(|window| window[0].0.bottom < window[3].0.top));
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
        let prompt = recording_delete_prompt(
            std::path::Path::new("C:\\Videos\\demo.mp4"),
            Some(std::path::Path::new("C:\\Videos\\demo.gif")),
        );
        assert!(prompt.contains("demo.mp4"));
        assert!(prompt.contains("demo.gif"));
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

    #[test]
    fn failed_recording_delete_staging_restores_every_original() {
        let id = NEXT_EXPORT_ID.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "matteshot-recording-delete-rollback-{}-{id}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mp4 = dir.join("recording.mp4");
        let gif = dir.join("recording.gif");
        std::fs::write(&mp4, b"video").unwrap();
        std::fs::write(&gif, b"gif").unwrap();

        let result = delete_recording_files_with(&mp4, Some(&gif), |from, to| {
            if from == mp4 {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "recording is in use",
                ))
            } else {
                std::fs::rename(from, to)
            }
        });

        assert!(result.is_err());
        assert_eq!(std::fs::read(&mp4).unwrap(), b"video");
        assert_eq!(std::fs::read(&gif).unwrap(), b"gif");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
        std::fs::remove_file(mp4).unwrap();
        std::fs::remove_file(gif).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    /// The grab zone has to be the same size in both directions on screen, at
    /// every clip shape. Expressed as a flat fraction of the source it was not:
    /// it stretched with the preview, and on anything wider than it was tall the
    /// vertical reach fell below the handle actually drawn there.
    #[test]
    fn crop_grab_zone_is_square_on_screen_at_any_aspect() {
        use super::crop_grab_tolerance;
        use windows::Win32::Foundation::RECT;
        for (w, h) in [(728, 568), (728, 410), (728, 312), (400, 900)] {
            let content = RECT {
                left: 0,
                top: 0,
                right: w,
                bottom: h,
            };
            let (tol_x, tol_y) = crop_grab_tolerance(content, 10);
            // Back into screen pixels, which is where "square" has to hold.
            let across = tol_x * w as f32;
            let down = tol_y * h as f32;
            assert!(
                (across - 10.0).abs() < 0.01 && (down - 10.0).abs() < 0.01,
                "{w}x{h}: {across}px across, {down}px down"
            );
        }
    }

    /// The shape it replaced, kept as the reason the function exists: a flat
    /// fraction reaches much further across a wide preview than down it.
    #[test]
    fn a_flat_fraction_would_not_have_been_square() {
        let (w, h) = (728.0f32, 410.0f32);
        assert!((0.02 * w - 14.56).abs() < 0.01);
        assert!((0.02 * h - 8.2).abs() < 0.01);
    }

    /// A preview collapsed to nothing must not divide by zero.
    #[test]
    fn crop_grab_tolerance_survives_an_empty_preview() {
        use super::crop_grab_tolerance;
        use windows::Win32::Foundation::RECT;
        let empty = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        let (tol_x, tol_y) = crop_grab_tolerance(empty, 10);
        assert!(tol_x.is_finite() && tol_y.is_finite());
    }

    fn export_partial_dir(label: &str) -> std::path::PathBuf {
        let id = NEXT_EXPORT_ID.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "matteshot-export-partial-{label}-{}-{id}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn unavailable_export_faults() -> [anyhow::Error; 2] {
        use windows::Win32::Foundation::ERROR_SHARING_VIOLATION;
        [
            anyhow::Error::from(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "sharing violation",
            ))
            .context("read video metadata"),
            anyhow::Error::from(windows::core::Error::from(
                windows::core::HRESULT::from_win32(ERROR_SHARING_VIOLATION.0),
            ))
            .context("decode finalized video"),
        ]
    }

    /// Pins SBS-923: export used to delete the same-folder `.partial` on
    /// any post-encode error, including a check that could not run.
    #[test]
    fn export_keeps_a_partial_when_the_check_cannot_run() {
        let dir = export_partial_dir("unavailable");
        for (index, fault) in unavailable_export_faults().into_iter().enumerate() {
            let temporary = dir.join(format!("edit.partial-1-{index}.mp4"));
            std::fs::write(&temporary, b"maybe a finished export").unwrap();

            assert_eq!(
                crate::trim::validation_fault(&fault),
                crate::trim::ValidationFault::Unavailable,
                "{fault:#}"
            );
            let message = dispose_export_partial(&temporary, ExportCleanup::Validate(fault));
            assert!(
                temporary.exists(),
                "Unavailable deleted the export: {message}"
            );
            assert!(
                message.contains("could not be checked"),
                "user-facing error must say the check did not run: {message}"
            );
            assert!(
                message.contains("recovered the next time Matteshot starts"),
                "Unavailable must not promise a same-session retry: {message}"
            );
            assert!(
                !message.contains("will be retried"),
                "Unavailable must not promise a retry this session does not run: {message}"
            );
            assert!(
                !message.contains("integrity check"),
                "Unavailable must not be described as a failed integrity check: {message}"
            );
            assert_eq!(
                export_failure_status(&message),
                "export could not be checked · edit kept"
            );
            std::fs::remove_file(temporary).unwrap();
        }
        std::fs::remove_dir(dir).unwrap();
    }

    /// Proven-bad bytes are still thrown away. SBS-923 must not weaken that.
    #[test]
    fn export_deletes_proven_undecodable_bytes() {
        let dir = export_partial_dir("undecodable");
        let temporary = dir.join("edit.partial-1-1.mp4");
        std::fs::write(&temporary, b"not an mp4").unwrap();

        let fault = anyhow::anyhow!("video has no decodable frames");
        assert_eq!(
            crate::trim::validation_fault(&fault),
            crate::trim::ValidationFault::Undecodable,
            "{fault:#}"
        );
        let message = dispose_export_partial(&temporary, ExportCleanup::Validate(fault));
        assert!(
            !temporary.exists(),
            "Undecodable left the export in place: {message}"
        );
        assert!(
            message.contains("validate export"),
            "Undecodable must still be a validate export error: {message}"
        );
        assert_eq!(
            export_failure_status(&message),
            format!("export failed: {message}")
        );

        std::fs::remove_dir(dir).unwrap();
    }

    /// The worker must still delete through a real `validate_video`
    /// rejection, not only a constructed fault.
    #[test]
    fn export_deletes_a_file_validate_video_rejects() {
        let dir = export_partial_dir("validate-video");
        let temporary = dir.join("edit.partial-1-1.mp4");
        std::fs::write(&temporary, b"not an mp4").unwrap();

        let error = crate::trim::validate_video(&temporary).unwrap_err();
        assert_eq!(
            crate::trim::validation_fault(&error),
            crate::trim::ValidationFault::Undecodable,
            "{error:#}"
        );
        let message = dispose_export_partial(&temporary, ExportCleanup::Validate(error));
        assert!(
            !temporary.exists(),
            "a real validate_video rejection left the export: {message}"
        );
        assert!(message.contains("validate export"), "{message}");

        std::fs::remove_dir(dir).unwrap();
    }

    /// A lost rename after a successful validate used to delete the
    /// `.partial`. The bytes are user data now; keep them for retry.
    #[test]
    fn export_keeps_a_partial_when_rename_fails_after_validate() {
        let dir = export_partial_dir("rename");
        let temporary = dir.join("edit.partial-1-1.mp4");
        std::fs::write(&temporary, b"validated export bytes").unwrap();

        let error = std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "destination is in use",
        );
        let message = dispose_export_partial(&temporary, ExportCleanup::Rename(error));
        assert!(
            temporary.exists(),
            "rename-after-validate deleted the export: {message}"
        );
        assert!(
            message.contains("recovery file kept"),
            "rename failure must name the kept file: {message}"
        );
        assert_eq!(
            export_failure_status(&message),
            "export save failed · edit kept"
        );

        std::fs::remove_file(temporary).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    /// Cancel and a failed encode are not "validated or uncheckable".
    /// Those files are incomplete and still go away.
    #[test]
    fn export_deletes_a_partial_on_cancel_or_encode_failure() {
        let dir = export_partial_dir("cancel-encode");
        let cancelled = dir.join("edit.partial-1-cancel.mp4");
        let encoded = dir.join("edit.partial-1-encode.mp4");
        std::fs::write(&cancelled, b"incomplete export").unwrap();
        std::fs::write(&encoded, b"incomplete export").unwrap();

        let cancel_message = dispose_export_partial(&cancelled, ExportCleanup::Cancelled);
        assert_eq!(cancel_message, "export cancelled");
        assert!(
            !cancelled.exists(),
            "cancel left the export: {cancel_message}"
        );
        assert_eq!(
            export_failure_status(&cancel_message),
            "export cancelled · original kept"
        );

        let encode_message =
            dispose_export_partial(&encoded, ExportCleanup::Encode("encoder failed".into()));
        assert_eq!(encode_message, "encoder failed");
        assert!(
            !encoded.exists(),
            "encode failure left the export: {encode_message}"
        );
        assert_eq!(
            export_failure_status(&encode_message),
            "export failed: encoder failed"
        );

        std::fs::remove_dir(dir).unwrap();
    }
}
