//! Focused post-recording editor: large matte preview, trim timeline, and
//! export actions. Non-modal on the main loop.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use image::RgbaImage;
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreatePen, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint,
    FillRect, GetMonitorInfoW, InvalidateRect, MonitorFromPoint, RoundRect, SelectObject,
    SetBkMode, SetTextColor, CLEARTYPE_QUALITY, DEFAULT_CHARSET, DT_CENTER, DT_END_ELLIPSIS,
    DT_LEFT, DT_SINGLELINE, DT_VCENTER, FF_DONTCARE, HDC, HFONT, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, PAINTSTRUCT, PS_SOLID, TRANSPARENT,
};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, VK_CONTROL, VK_DELETE, VK_END, VK_ESCAPE, VK_HOME, VK_LEFT, VK_RIGHT, VK_SPACE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CreateWindowExW, DefWindowProcW, DestroyWindow, GetWindowLongPtrW,
    GetCursorPos, LoadCursorW, MessageBoxW, PostMessageW, RegisterClassW, SetForegroundWindow,
    SetWindowLongPtrW, CREATESTRUCTW, CS_DBLCLKS, CS_HREDRAW, CS_VREDRAW, GWLP_USERDATA, IDC_ARROW,
    IDYES, MB_ICONWARNING, MB_YESNO, WM_APP, WM_CHAR, WM_CLOSE, WM_ERASEBKGND, WM_KEYDOWN,
    WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCCREATE, WM_NCDESTROY, WM_PAINT, WNDCLASSW,
    WS_CAPTION, WS_EX_APPWINDOW, WS_SYSMENU, WS_VISIBLE,
};

const WM_EXPORT_PROGRESS: u32 = WM_APP + 20;
const WM_EXPORT_DONE: u32 = WM_APP + 21;
const WM_PLAYBACK_FRAME: u32 = WM_APP + 22;
const WM_PLAYBACK_DONE: u32 = WM_APP + 23;
const WM_EXPORT_STALLED: u32 = WM_APP + 24;
static NEXT_EXPORT_ID: AtomicU64 = AtomicU64::new(1);

const PAD_MIN: f32 = 0.04;
const PAD_MAX: f32 = 0.18;
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
    Delete,
    SaveTrim,
}

#[derive(Clone, Copy, PartialEq)]
enum Handle {
    Start,
    End,
}

#[derive(Clone, Copy, PartialEq)]
enum Tool {
    Text,
    Arrow,
    Rect,
    Blur,
}

#[derive(Clone, Copy, PartialEq)]
enum Drag {
    Trim(Handle),
    Playhead,
    Padding,
    Draw { index: usize, start: (f32, f32) },
    Move { index: usize, last: (f32, f32) },
    ClipStart(usize),
    ClipEnd(usize),
    ClipMove { index: usize, last_time: i64 },
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
    text_entry: Option<TextEntry>,
    add_control: RECT,
    tool_controls: Vec<(RECT, Tool, &'static str)>,
    color_controls: Vec<(RECT, usize)>,
    size_controls: Vec<(RECT, usize)>,
    undo_control: RECT,
    delete_control: RECT,
    status: Option<String>,
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
    let m = sc(24);
    let timeline_h = (ch / 6).clamp(sc(88), sc(126));
    let strip = RECT {
        left: m,
        top: ch - sc(140) - timeline_h,
        right: cw - m,
        bottom: ch - sc(140),
    };
    let preview = RECT {
        left: m,
        top: sc(96),
        right: cw - m,
        bottom: strip.top - sc(92),
    };
    let mut matte_controls = Vec::new();
    if style_count > 0 {
        let start = m + sc(52);
        let gap = sc(7);
        let available = (cw - m - start - gap * (style_count as i32 - 1)).max(style_count as i32);
        let chip_w = available / style_count as i32;
        let chip_top = preview.bottom + sc(8);
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
    let settings_top = preview.bottom + sc(40);
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
    let labels: [(Act, &'static str, i32); 5] = [
        (Act::SaveTrim, "Export edit", 108),
        (Act::Play, "Play", 82),
        (Act::Reveal, "Show in folder", 126),
        (Act::Copy, "Copy", 72),
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
    for (index, (tool, label)) in [
        (Tool::Text, "Text"),
        (Tool::Arrow, "Arrow"),
        (Tool::Rect, "Box"),
        (Tool::Blur, "Blur"),
    ]
    .into_iter()
    .enumerate()
    {
        let left = panel_left + sc(10) + index as i32 * sc(76);
        tool_controls.push((
            RECT {
                left,
                top: panel_top + sc(10),
                right: left + sc(68),
                bottom: panel_top + sc(38),
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
                top: panel_top + sc(52),
                right: left + sc(22),
                bottom: panel_top + sc(74),
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
                top: panel_top + sc(50),
                right: left + sc(28),
                bottom: panel_top + sc(76),
            },
            index,
        ));
    }
    let undo_control = RECT {
        left: panel_left + sc(254),
        top: panel_top + sc(50),
        right: panel_left + sc(308),
        bottom: panel_top + sc(76),
    };
    let delete_control = RECT {
        left: panel_left + sc(314),
        top: panel_top + sc(50),
        right: panel_left + sc(372),
        bottom: panel_top + sc(76),
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

fn matte_frame(
    raw: &(Vec<u8>, u32, u32),
    style: &crate::style::Style,
    opts: &crate::compose::ComposeOpts,
) -> (Vec<u8>, u32, u32) {
    if crate::compose::is_plain(style) {
        return raw.clone();
    }
    let image = thumb_image(&raw.0, raw.1, raw.2);
    image_thumb(&crate::compose::compose_with(&image, style, opts))
}

fn set_playhead(state: &mut State, x: i32) {
    let span = (state.strip.right - state.strip.left).max(1) as f64;
    state.playhead = ((((x - state.strip.left) as f64 / span) * state.duration as f64) as i64)
        .clamp(0, state.duration);
    refresh_preview(state);
}

fn refresh_preview(state: &mut State) {
    if state.raw_thumbs.is_empty() {
        state.preview_raw = None;
        state.preview = None;
        return;
    }
    let index = if state.duration > 0 {
        ((state.playhead as f64 / state.duration as f64) * state.raw_thumbs.len() as f64)
            .floor()
            .min((state.raw_thumbs.len() - 1) as f64) as usize
    } else {
        0
    };
    state.preview_raw = Some(state.raw_thumbs[index].clone());
    recompose_preview(state);
}

fn recompose_preview(state: &mut State) {
    let opts = state_compose_opts(state);
    let Some(frame) = state.preview_raw.as_ref() else {
        state.preview = None;
        return;
    };
    let raw = thumb_image(&frame.0, frame.1, frame.2);
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
    crate::video_edit::render_at(
        &mut image,
        &state.annotations,
        state.playhead,
        skip,
        content_size,
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
            size: current_size(state),
        };
        crate::video_edit::render_one_at(&mut image, &draft, content_size, content_offset);
    }
    state.preview = Some(image_thumb(&image));
}

fn update_padding(state: &mut State, x: i32) {
    let slider = state.padding_slider;
    let t = ((x - slider.left) as f32 / (slider.right - slider.left).max(1) as f32)
        .clamp(0.0, 1.0);
    state.pad_factor = PAD_MIN + t * (PAD_MAX - PAD_MIN);
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
    const DEFAULT: i64 = 30_000_000;
    const MINIMUM: i64 = 5_000_000;
    if state.duration <= 0 {
        return (0, DEFAULT);
    }
    let mut start = state.playhead.clamp(0, state.duration);
    let end = (start + DEFAULT).min(state.duration);
    if end - start < MINIMUM {
        start = (end - DEFAULT).max(0);
    }
    (start, end.max(start + MINIMUM).min(state.duration))
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

fn annotation_name(item: &crate::video_edit::Item) -> &'static str {
    match &item.shape {
        crate::video_edit::Shape::Text { .. } => "Text",
        crate::video_edit::Shape::Arrow { .. } => "Arrow",
        crate::video_edit::Shape::Rect { .. } => "Box",
        crate::video_edit::Shape::Blur { .. } => "Blur",
    }
}

fn annotation_lane(state: &State) -> RECT {
    RECT {
        left: state.strip.left,
        top: state.strip.bottom + s(state, 34),
        right: state.strip.right,
        bottom: state.strip.bottom + s(state, 56),
    }
}

fn time_to_x(state: &State, time: i64) -> i32 {
    if state.duration <= 0 {
        return state.strip.left;
    }
    state.strip.left
        + ((time.clamp(0, state.duration) as f64 / state.duration as f64)
            * (state.strip.right - state.strip.left) as f64) as i32
}

fn x_to_time(state: &State, x: i32) -> i64 {
    let span = (state.strip.right - state.strip.left).max(1) as f64;
    ((((x - state.strip.left) as f64 / span) * state.duration as f64) as i64)
        .clamp(0, state.duration)
}

fn item_clip_rect(state: &State, index: usize, item: &crate::video_edit::Item) -> RECT {
    let lane = annotation_lane(state);
    let row_h = ((lane.bottom - lane.top) / 2).max(1);
    let row = (index % 2) as i32;
    RECT {
        left: time_to_x(state, item.start),
        top: lane.top + row * row_h,
        right: time_to_x(state, item.end).max(time_to_x(state, item.start) + s(state, 12)),
        bottom: lane.top + (row + 1) * row_h - 1,
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

fn hit_annotation(state: &State, point: (f32, f32)) -> Option<usize> {
    state
        .annotations
        .iter()
        .enumerate()
        .rev()
        .find(|(_, item)| {
            item.active_at(state.playhead) && crate::video_edit::hit(item, point, 0.018)
        })
        .map(|(index, _)| index)
}

fn contains(rect: RECT, x: i32, y: i32) -> bool {
    x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom
}

fn tool_panel(state: &State) -> RECT {
    RECT {
        left: state
            .tool_controls
            .first()
            .map(|(rect, ..)| rect.left - s(state, 10))
            .unwrap_or(state.add_control.left),
        top: state
            .tool_controls
            .first()
            .map(|(rect, ..)| rect.top - s(state, 10))
            .unwrap_or(state.add_control.bottom),
        right: state.delete_control.right + s(state, 10),
        bottom: state.delete_control.bottom + s(state, 10),
    }
}

fn shape_for_tool(tool: Tool, start: (f32, f32)) -> crate::video_edit::Shape {
    match tool {
        Tool::Arrow => crate::video_edit::Shape::Arrow {
            from: start,
            to: start,
        },
        Tool::Rect => crate::video_edit::Shape::Rect { a: start, b: start },
        Tool::Blur => crate::video_edit::Shape::Blur { a: start, b: start },
        Tool::Text => unreachable!("text uses direct entry"),
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
        size: current_size(state),
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
    state.tool = None;
    state.tools_open = false;
    recompose_preview(state);
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
        top: s(state, 14),
        right: state.width - m,
        bottom: s(state, 42),
    };
    DrawTextW(hdc, &mut t, &mut rc, DT_LEFT | DT_SINGLELINE | DT_VCENTER);

    SelectObject(hdc, state.font_small);
    SetTextColor(hdc, state.theme.muted);
    let mut sum = wide(&state.summary);
    let mut rc2 = RECT {
        left: m,
        top: s(state, 44),
        right: state.width - m,
        bottom: s(state, 66),
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
        top: s(state, 66),
        right: state.width - m,
        bottom: s(state, 88),
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
            if item.active_at(state.playhead) {
                let image_rect = preview_content_rect(state)
                    .unwrap_or_else(|| preview_image_rect(state, frame));
                let (x0, y0, x1, y1) = crate::video_edit::bounds(item);
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
            }
        }
    }

    paint_chip(
        hdc,
        state.add_control,
        if state.tools_open { "Done" } else { "+ Add" },
        state.tools_open,
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

        for (rect, tool, label) in &state.tool_controls {
            paint_chip(hdc, *rect, label, state.tool == Some(*tool), state);
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
        for (rect, index) in &state.size_controls {
            paint_chip(
                hdc,
                *rect,
                ["S", "M", "L"][*index],
                state.size_idx == *index,
                state,
            );
        }
        paint_chip(hdc, state.undo_control, "Undo", false, state);
        paint_chip(hdc, state.delete_control, "Delete", false, state);
    }

    if state.text_entry.is_some() || state.tool.is_some() || state.selected.is_some() {
        let hint = if state.text_entry.is_some() {
            "Type caption   \u{00b7}   Enter place   \u{00b7}   Esc cancel"
        } else if let Some(tool) = state.tool {
            match tool {
                Tool::Text => "Click the preview to place a caption",
                Tool::Arrow => "Drag on the preview to draw an arrow",
                Tool::Rect => "Drag on the preview to draw a box",
                Tool::Blur => "Drag over anything sensitive to blur it",
            }
        } else {
            "Selected   \u{00b7}   drag to move   \u{00b7}   Delete removes"
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
    let pad_t = ((state.pad_factor - PAD_MIN) / (PAD_MAX - PAD_MIN)).clamp(0.0, 1.0);
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
            top: strip.top - s(state, 24),
            right: strip.right,
            bottom: strip.top - s(state, 4),
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

        if !state.annotations.is_empty() {
            let lane = annotation_lane(state);
            let lane_fill = CreateSolidBrush(state.theme.chip);
            FillRect(hdc, &lane, lane_fill);
            let _ = DeleteObject(lane_fill);
            for (index, item) in state.annotations.iter().enumerate() {
                let rect = item_clip_rect(state, index, item);
                let selected = state.selected == Some(index);
                let [r, g, b] =
                    crate::annotate::COLORS[item.color.min(crate::annotate::COLORS.len() - 1)];
                let color = windows::Win32::Foundation::COLORREF(
                    r as u32 | ((g as u32) << 8) | ((b as u32) << 16),
                );
                let fill = CreateSolidBrush(if selected {
                    state.theme.accent
                } else {
                    state.theme.bg
                });
                let pen = CreatePen(PS_SOLID, if selected { 2 } else { 1 }, color);
                let old_brush = SelectObject(hdc, fill);
                let old_pen = SelectObject(hdc, pen);
                let _ = RoundRect(
                    hdc,
                    rect.left,
                    rect.top,
                    rect.right,
                    rect.bottom,
                    s(state, 5),
                    s(state, 5),
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
                let mut clip_label = wide(annotation_name(item));
                let mut clip_label_rect = RECT {
                    left: rect.left + s(state, 4),
                    top: rect.top,
                    right: rect.right - s(state, 4),
                    bottom: rect.bottom,
                };
                DrawTextW(
                    hdc,
                    &mut clip_label,
                    &mut clip_label_rect,
                    DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
                );
                if selected {
                    let handles = CreateSolidBrush(state.theme.text);
                    for edge in [rect.left, rect.right] {
                        FillRect(
                            hdc,
                            &RECT {
                                left: edge - s(state, 2),
                                top: rect.top + s(state, 1),
                                right: edge + s(state, 2),
                                bottom: rect.bottom - s(state, 1),
                            },
                            handles,
                        );
                    }
                    let _ = DeleteObject(handles);
                }
            }
        }
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
        let shown = if *act == Act::SaveTrim && state.exporting {
            "Cancel export"
        } else if *act == Act::Play && state.playing {
            "Pause"
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
                            let _ = crate::output::file_to_clipboard(&done.path);
                            state.status = Some(format!(
                                "saved {} · copied",
                                done.path.file_name().unwrap_or_default().to_string_lossy()
                            ));
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
                        Drag::Draw { index, start } => {
                            if let Some(point) = screen_to_preview(state, x, y) {
                                if let Some(item) = state.annotations.get_mut(index) {
                                    item.shape = match item.shape {
                                        crate::video_edit::Shape::Arrow { .. } => {
                                            crate::video_edit::Shape::Arrow {
                                                from: start,
                                                to: point,
                                            }
                                        }
                                        crate::video_edit::Shape::Rect { .. } => {
                                            crate::video_edit::Shape::Rect { a: start, b: point }
                                        }
                                        crate::video_edit::Shape::Blur { .. } => {
                                            crate::video_edit::Shape::Blur { a: start, b: point }
                                        }
                                        crate::video_edit::Shape::Text { .. } => item.shape.clone(),
                                    };
                                }
                                recompose_preview(state);
                            }
                        }
                        Drag::Move { index, last } => {
                            if let Some(point) = screen_to_preview(state, x, y) {
                                if let Some(item) = state.annotations.get_mut(index) {
                                    crate::video_edit::translate(
                                        item,
                                        point.0 - last.0,
                                        point.1 - last.1,
                                    );
                                }
                                state.dragging = Some(Drag::Move { index, last: point });
                                recompose_preview(state);
                            }
                        }
                        Drag::ClipStart(index) => {
                            let end = state.annotations.get(index).map(|item| item.end);
                            let time = x_to_time(state, x);
                            if let (Some(end), Some(item)) = (end, state.annotations.get_mut(index))
                            {
                                item.start = time.min(end - 5_000_000).max(0);
                            }
                            recompose_preview(state);
                        }
                        Drag::ClipEnd(index) => {
                            let start = state.annotations.get(index).map(|item| item.start);
                            let time = x_to_time(state, x);
                            let duration = state.duration;
                            if let (Some(start), Some(item)) =
                                (start, state.annotations.get_mut(index))
                            {
                                item.end = time.max(start + 5_000_000).min(duration);
                            }
                            recompose_preview(state);
                        }
                        Drag::ClipMove { index, last_time } => {
                            let now = x_to_time(state, x);
                            if let Some(item) = state.annotations.get_mut(index) {
                                let length = item.end - item.start;
                                let start = (item.start + now - last_time)
                                    .clamp(0, state.duration - length);
                                item.start = start;
                                item.end = start + length;
                            }
                            state.dragging = Some(Drag::ClipMove {
                                index,
                                last_time: now,
                            });
                            recompose_preview(state);
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
                let (x, y) = (
                    (lparam.0 & 0xFFFF) as i16 as i32,
                    ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
                );
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
                        stop_playback(state);
                        state.tool = Some(tool);
                        state.selected = None;
                        state.text_entry = None;
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

                if contains(annotation_lane(state), x, y) {
                    if let Some(index) = state
                        .annotations
                        .iter()
                        .enumerate()
                        .rev()
                        .find(|(index, item)| contains(item_clip_rect(state, *index, item), x, y))
                        .map(|(index, _)| index)
                    {
                        stop_playback(state);
                        let rect = item_clip_rect(state, index, &state.annotations[index]);
                        state.selected = Some(index);
                        state.playhead = state.annotations[index]
                            .start
                            .clamp(state.trim_start, state.trim_end);
                        push_undo(state);
                        let grab = s(state, 7);
                        state.dragging = Some(if (x - rect.left).abs() <= grab {
                            Drag::ClipStart(index)
                        } else if (x - rect.right).abs() <= grab {
                            Drag::ClipEnd(index)
                        } else {
                            Drag::ClipMove {
                                index,
                                last_time: x_to_time(state, x),
                            }
                        });
                        refresh_preview(state);
                        windows::Win32::UI::Input::KeyboardAndMouse::SetCapture(hwnd);
                        let _ = InvalidateRect(hwnd, None, false);
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
                            state.tool = None;
                            state.tools_open = false;
                            state.selected = None;
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
                            });
                            let index = state.annotations.len() - 1;
                            state.selected = Some(index);
                            state.tool = None;
                            state.tools_open = false;
                            state.dragging = Some(Drag::Draw {
                                index,
                                start: point,
                            });
                            windows::Win32::UI::Input::KeyboardAndMouse::SetCapture(hwnd);
                            recompose_preview(state);
                        }
                        let _ = InvalidateRect(hwnd, None, false);
                        return LRESULT(0);
                    }
                    if let Some(index) = hit_annotation(state, point) {
                        push_undo(state);
                        state.selected = Some(index);
                        state.dragging = Some(Drag::Move { index, last: point });
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
        WM_LBUTTONUP => {
            if let Some(state) = state_of(hwnd) {
                let (x, y) = (
                    (lparam.0 & 0xFFFF) as i16 as i32,
                    ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
                );
                if let Some(drag) = state.dragging.take() {
                    let _ = windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture();
                    if matches!(drag, Drag::Trim(_) | Drag::Playhead) {
                        refresh_preview_exact(state);
                        if state.resume_after_drag {
                            state.resume_after_drag = false;
                            start_playback(hwnd, state);
                        }
                    } else {
                        if drag == Drag::Padding {
                            refresh_matte_thumbs(state);
                        }
                        if let Drag::Draw { index, .. } = drag {
                            if let Some(item) = state.annotations.get(index) {
                                let (x0, y0, x1, y1) = crate::video_edit::bounds(item);
                                if (x1 - x0).hypot(y1 - y0) < 0.008 {
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
                            let _ = crate::output::file_to_clipboard(&state.mp4);
                        }
                        Act::Delete => {
                            if state.exporting {
                                state.status =
                                    Some("finish the current export before deleting".into());
                                let _ = InvalidateRect(hwnd, None, false);
                                return LRESULT(0);
                            }
                            stop_playback(state);
                            let _ = std::fs::remove_file(&state.mp4);
                            if let Some(g) = &state.gif {
                                let _ = std::fs::remove_file(g);
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
                            state.size_idx = if state.annotations[index].size < 0.91 {
                                0
                            } else if state.annotations[index].size > 1.17 {
                                2
                            } else {
                                1
                            };
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
        WM_CHAR => {
            if let Some(state) = state_of(hwnd) {
                if state.text_entry.is_some() {
                    let ch = char::from_u32(wparam.0 as u32).unwrap_or('\0');
                    match ch {
                        '\r' => commit_text(state),
                        '\u{8}' => {
                            if let Some(entry) = &mut state.text_entry {
                                entry.text.pop();
                            }
                            recompose_preview(state);
                        }
                        ch if !ch.is_control() => {
                            if let Some(entry) = &mut state.text_entry {
                                if entry.text.chars().count() < 160 {
                                    entry.text.push(ch);
                                }
                            }
                            recompose_preview(state);
                        }
                        _ => {}
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
                    key if key == VK_LEFT.0 => {
                        let resume = state.playing;
                        stop_playback(state);
                        state.playhead = (state.playhead - 5_000_000).max(state.trim_start);
                        refresh_preview_exact(state);
                        if resume {
                            start_playback(hwnd, state);
                        }
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                    key if key == VK_RIGHT.0 => {
                        let resume = state.playing;
                        stop_playback(state);
                        state.playhead = (state.playhead + 5_000_000).min(state.trim_end);
                        refresh_preview_exact(state);
                        if resume && state.playhead < state.trim_end {
                            start_playback(hwnd, state);
                        }
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                    key if key == VK_HOME.0 => {
                        let resume = state.playing;
                        stop_playback(state);
                        state.playhead = state.trim_start;
                        refresh_preview_exact(state);
                        if resume {
                            start_playback(hwnd, state);
                        }
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                    key if key == VK_END.0 => {
                        stop_playback(state);
                        state.playhead = state.trim_end;
                        refresh_preview_exact(state);
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
                let s = GetDpiForSystem() as f32 / 96.0;
                (*mmi).ptMinTrackSize.x = (760.0 * s) as i32;
                (*mmi).ptMinTrackSize.y = (560.0 * s) as i32;
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
pub fn show(mp4: PathBuf, gif: Option<PathBuf>, frames: u32, secs: u64) -> Result<()> {
    if let Some(parent) = mp4.parent() {
        crate::output::cleanup_stale_video_partials(parent);
    }
    let scale = unsafe { GetDpiForSystem() } as f32 / 96.0;
    let sc = |v: i32| (v as f32 * scale) as i32;
    let mut cursor = POINT::default();
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
    let (probe_w, probe_h) = {
        let initial = layout(scale, cw, ch, 7);
        (
            (initial.strip.right - initial.strip.left) as u32,
            (initial.strip.bottom - initial.strip.top) as u32,
        )
    };
    let probe = crate::trim::probe(&mp4, probe_w, probe_h)
        .context("open the finished recording in the editor")?;
    if probe.duration_100ns <= 0 || probe.thumbs.is_empty() {
        anyhow::bail!("the finished recording has no decodable video frames");
    }
    let duration = probe.duration_100ns;
    let raw_thumbs = probe.thumbs;
    let style_source = raw_thumbs
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
    let preview_raw = crate::trim::preview_frame(
        &mp4,
        0,
        (initial.preview.right - initial.preview.left).max(2) as u32,
        (initial.preview.bottom - initial.preview.top).max(2) as u32,
    )
    .ok()
    .or_else(|| raw_thumbs.first().cloned());
    let preview = preview_raw
        .as_ref()
        .map(|frame| matte_frame(frame, &styles[matte_index], &opts));

    let state = Box::into_raw(Box::new(State {
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
        thumbs,
        preview_raw,
        preview,
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
        text_entry: None,
        add_control: initial.add_control,
        tool_controls: initial.tool_controls,
        color_controls: initial.color_controls,
        size_controls: initial.size_controls,
        undo_control: initial.undo_control,
        delete_control: initial.delete_control,
        status: None,
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
    }));

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

        let style = WS_CAPTION
            | WS_SYSMENU
            | WS_VISIBLE
            | windows::Win32::UI::WindowsAndMessaging::WS_THICKFRAME
            | windows::Win32::UI::WindowsAndMessaging::WS_MAXIMIZEBOX;
        let mut outer = RECT {
            left: 0,
            top: 0,
            right: cw,
            bottom: ch,
        };
        let _ = AdjustWindowRectEx(&mut outer, style, false, WS_EX_APPWINDOW);
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
            }
            Err(_) => drop(Box::from_raw(state)),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{available_export_path, NEXT_EXPORT_ID};
    use std::sync::atomic::Ordering;

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
}
