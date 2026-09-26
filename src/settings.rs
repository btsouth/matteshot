//! Settings window — custom-drawn dark UI matching the picker/overlay,
//! non-modal, single instance, running on the main thread's message loop.

use std::sync::atomic::{AtomicIsize, Ordering};

use anyhow::{Context, Result};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, CreatePen,
    CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect, GetMonitorInfoW,
    InvalidateRect, MonitorFromPoint, RoundRect, SelectObject, SetBkMode, SetTextColor,
    CLEARTYPE_QUALITY, DEFAULT_CHARSET, DT_END_ELLIPSIS, DT_LEFT, DT_RIGHT, DT_SINGLELINE,
    DT_VCENTER, FF_DONTCARE, HDC, HFONT, MONITORINFO, MONITOR_DEFAULTTONEAREST, PS_SOLID, SRCCOPY,
    TRANSPARENT,
};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN, VIRTUAL_KEY,
    VK_CONTROL, VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_LWIN, VK_MENU, VK_NEXT, VK_PRIOR,
    VK_RETURN, VK_RWIN, VK_SHIFT, VK_SPACE, VK_TAB, VK_UP,
};
use windows::Win32::UI::Shell::{
    FileOpenDialog, IFileOpenDialog, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, GetWindowLongPtrW,
    GetWindowRect, IsWindow, LoadCursorW, MessageBoxW, RegisterClassW, SetForegroundWindow,
    SetWindowLongPtrW, SetWindowPos, ShowWindow, CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW,
    GWLP_USERDATA, HWND_NOTOPMOST, HWND_TOPMOST, IDC_ARROW, IDYES, MB_ICONERROR,
    MB_ICONINFORMATION, MB_ICONWARNING, MB_OK, MB_YESNO, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_NOZORDER, SW_RESTORE, SW_SHOWNORMAL, WM_CLOSE, WM_DISPLAYCHANGE, WM_ERASEBKGND, WM_KEYDOWN,
    WM_KILLFOCUS, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCCREATE, WM_NCDESTROY, WM_PAINT,
    WM_SIZE, WM_SYSKEYDOWN, WM_WINDOWPOSCHANGED, WNDCLASSW, WS_CAPTION, WS_EX_APPWINDOW,
    WS_SYSMENU, WS_VISIBLE,
};

use crate::config::Config;
use crate::{prtscn, tray, HOTKEY_ID_PRTSCN};

static WINDOW: AtomicIsize = AtomicIsize::new(0);

#[derive(Clone, Copy, Debug, PartialEq)]
enum Ctrl {
    ChangeDir,
    OpenDir,
    ChangeVideoDir,
    OpenVideoDir,
    Scale(u32),
    OutputSize(u32),
    CustomSize,
    Autostart,
    Prtscn,
    RecordGif,
    FrameRate(u32),
    AutoUpdate,
    KeepEditorOpen,
    Audio(&'static str),
    CaptureHotkey,
    CaptureDelay(u32),
    Diagnostics,
    ClearHistoryTitles,
}

/// Painted furniture: section headers, the folder paths, and the one inline
/// label. Positioned by the same pass as the controls so the two can no longer
/// disagree.
///
/// The text is resolved at paint time rather than stored here, because
/// `refresh` reloads the config and repaints without rebuilding the layout, so
/// a baked string would go stale the moment a setting changed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Chrome {
    SaveFolderHeader,
    SavePath,
    VideoFolderHeader,
    VideoPath,
    RenderQualityHeader,
    ScreenshotSizeHeader,
    AudioLabel,
    FrameRateLabel,
    HotkeyLabel,
    DelayLabel,
}

/// Walks down the window handing out rects.
///
/// Every position used to be a literal, in two lists that had to agree by
/// hand: the builder placed controls, and the paint routine separately placed
/// headers at its own hardcoded coordinates. Inserting a row meant editing
/// both and shifting everything below in each. Here a row is one call, what
/// follows moves on its own, and the window height is wherever the cursor
/// stopped.
struct Layout {
    scale: f32,
    margin: i32,
    width: i32,
    y: i32,
    controls: Vec<(RECT, Ctrl)>,
    chrome: Vec<(RECT, Chrome)>,
}

impl Layout {
    fn new(scale: f32, width: i32) -> Layout {
        let margin = (24.0 * scale) as i32;
        Layout {
            scale,
            margin,
            width,
            y: 0,
            controls: Vec::new(),
            chrome: Vec::new(),
        }
    }

    fn sc(&self, v: i32) -> i32 {
        (v as f32 * self.scale) as i32
    }

    fn gap(&mut self, v: i32) {
        self.y += self.sc(v);
    }

    /// Full-width band at the cursor, which the cursor then moves past.
    fn band(&mut self, height: i32) -> RECT {
        let rect = RECT {
            left: self.margin,
            top: self.y,
            right: self.width - self.margin,
            bottom: self.y + self.sc(height),
        };
        self.y = rect.bottom;
        rect
    }

    fn header(&mut self, chrome: Chrome, height: i32) {
        let rect = self.band(height);
        self.chrome.push((rect, chrome));
    }

    /// A folder path with its Change / Open buttons on the same line.
    fn path_row(&mut self, chrome: Chrome, change: Ctrl, open: Ctrl) {
        let rect = self.band(30);
        self.chrome.push((rect, chrome));
        let right = self.width - self.margin;
        self.controls.push((
            RECT {
                left: right - self.sc(140),
                top: rect.top,
                right: right - self.sc(64),
                bottom: rect.bottom,
            },
            change,
        ));
        self.controls.push((
            RECT {
                left: right - self.sc(56),
                top: rect.top,
                right,
                bottom: rect.bottom,
            },
            open,
        ));
    }

    /// A run of chips starting at the left margin, or at `indent` when it
    /// shares its line with a label.
    fn chips(&mut self, items: &[Ctrl], width: i32, stride: i32, indent: i32, height: i32) {
        let rect = self.band(height);
        let start = self.margin + self.sc(indent);
        let inner_right = self.width - self.margin;
        let (chip_w, chip_stride) = fit_equal_row(
            start,
            inner_right,
            items.len(),
            self.sc(width),
            self.sc(stride),
        );
        for (i, ctrl) in items.iter().enumerate() {
            let x = start + i as i32 * chip_stride;
            self.controls.push((
                RECT {
                    left: x,
                    top: rect.top,
                    right: x + chip_w,
                    bottom: rect.bottom,
                },
                *ctrl,
            ));
        }
    }

    /// Furniture on a line the cursor has already passed, for a label that
    /// shares its row with controls.
    ///
    /// `width` stops short of whatever else is on the line. Without it the
    /// label gets the full band, and at a DPI or font where the text runs
    /// wider it would slide under the controls and be overdrawn by them.
    fn chrome_at(&mut self, top: i32, height: i32, width: i32, chrome: Chrome) {
        self.chrome.push((
            RECT {
                left: self.margin,
                top,
                right: self.margin + self.sc(width),
                bottom: top + self.sc(height),
            },
            chrome,
        ));
    }

    fn checkbox(&mut self, ctrl: Ctrl) {
        let rect = self.band(28);
        self.controls.push((rect, ctrl));
    }

    /// Two side-by-side actions at the foot of the window.
    fn button_pair(&mut self, left: Ctrl, right: Ctrl, width: i32, stride: i32) {
        let rect = self.band(30);
        let start = self.margin;
        let inner_right = self.width - self.margin;
        let (chip_w, chip_stride) =
            fit_equal_row(start, inner_right, 2, self.sc(width), self.sc(stride));
        for (i, ctrl) in [left, right].into_iter().enumerate() {
            let x = start + i as i32 * chip_stride;
            self.controls.push((
                RECT {
                    left: x,
                    top: rect.top,
                    right: x + chip_w,
                    bottom: rect.bottom,
                },
                ctrl,
            ));
        }
    }

    /// Client height: where the cursor stopped, plus room for the footer line.
    fn finish(&self) -> i32 {
        self.y + self.sc(38)
    }
}

/// Chip width and stride so `n` equal controls starting at `start` end on
/// or left of `inner_right`. Designed metrics are kept when they already fit;
/// a clamped client shrinks width and stride together.
fn fit_equal_row(start: i32, inner_right: i32, n: usize, chip_w: i32, stride: i32) -> (i32, i32) {
    let n = n.max(1) as i32;
    let available = (inner_right - start).max(1);
    let mut chip_w = chip_w.max(1);
    if n == 1 {
        return (chip_w.min(available), 0);
    }
    let mut stride = stride.max(1);
    let span = (n - 1) * stride + chip_w;
    if span > available {
        let q = available as f32 / span as f32;
        chip_w = ((chip_w as f32) * q) as i32;
        stride = ((stride as f32) * q) as i32;
        chip_w = chip_w.max(1);
        stride = stride.max(1);
        if (n - 1) * stride >= available {
            stride = ((available - 1) / (n - 1)).max(1);
        }
        let last = (n - 1) * stride + chip_w;
        if last > available {
            chip_w = (available - (n - 1) * stride).max(1);
        }
    }
    (chip_w, stride)
}

/// Logical client width Settings is designed around, before DPI scale.
const LOGICAL_WIDTH: i32 = 500;
/// Wheel and arrow-key scroll step, matching History.
const WHEEL_STEP: i32 = 90;
/// Smallest visible client we will ask for when the work area is short.
/// Below this only a couple of rows would show; the window still shrinks
/// further if the work area itself is smaller.
const MIN_VISIBLE_CLIENT: i32 = 160;

/// Monitor work area as integers, so clamp arithmetic does not need a live
/// `MONITORINFO`. A zero-sized rect is the unknown/unreadable case.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WorkRect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl WorkRect {
    fn from_rect(r: RECT) -> WorkRect {
        WorkRect {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        }
    }

    fn width(self) -> i32 {
        self.right - self.left
    }

    fn height(self) -> i32 {
        self.bottom - self.top
    }

    /// A failed `GetMonitorInfoW` leaves zeros. That is not "a 0x0 desktop";
    /// it is unknown, and must not collapse the window to nothing.
    fn is_usable(self) -> bool {
        self.width() > 0 && self.height() > 0
    }
}

/// Outer position/size after fitting content into a work area (SBS-753).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FittedWindow {
    x: i32,
    y: i32,
    outer_w: i32,
    outer_h: i32,
    /// Client width after clamping `outer_w` to the work area. Layout must
    /// use this, not the designed width, or the right-hand chips sit past
    /// the client and cannot be hit-tested.
    client_w: i32,
    viewport_h: i32,
    scrollable: bool,
}

/// Fit a designed client into `work`.
///
/// `non_client_*` is `outer - client` from `dpi::outer_bounds` (or a stand-in
/// in tests). When `work` is unreadable the designed size and preferred
/// origin are kept: inventing a desktop would hide the same overflow this
/// helper exists to stop, and inventing a 0x0 desktop would hide the window.
fn fit_settings_window(
    content_w: i32,
    content_h: i32,
    non_client_w: i32,
    non_client_h: i32,
    work: WorkRect,
    preferred_origin: Option<(i32, i32)>,
) -> FittedWindow {
    let nc_w = non_client_w.max(0);
    let nc_h = non_client_h.max(0);
    let content_w = content_w.max(1);
    let content_h = content_h.max(1);
    let desired_outer_w = content_w + nc_w;
    let desired_outer_h = content_h + nc_h;
    if !work.is_usable() {
        let (x, y) = preferred_origin.unwrap_or((0, 0));
        return FittedWindow {
            x,
            y,
            outer_w: desired_outer_w,
            outer_h: desired_outer_h,
            client_w: content_w,
            viewport_h: content_h,
            scrollable: false,
        };
    }
    let work_w = work.width();
    let work_h = work.height();
    let outer_w = desired_outer_w.min(work_w).max(1);
    let min_outer_h = (MIN_VISIBLE_CLIENT + nc_h).min(work_h).max(1);
    let outer_h = desired_outer_h.min(work_h).max(min_outer_h);
    let client_w = (outer_w - nc_w).max(1);
    let viewport_h = (outer_h - nc_h).max(1);
    let scrollable = content_h > viewport_h;
    let (x, y) = match preferred_origin {
        Some(origin) => origin,
        None => (
            work.left + (work_w - outer_w) / 2,
            work.top + (work_h - outer_h) / 2,
        ),
    };
    let max_x = work.right - outer_w;
    let max_y = work.bottom - outer_h;
    let x = if max_x >= work.left {
        x.clamp(work.left, max_x)
    } else {
        work.left
    };
    let y = if max_y >= work.top {
        y.clamp(work.top, max_y)
    } else {
        work.top
    };
    FittedWindow {
        x,
        y,
        outer_w,
        outer_h,
        client_w,
        viewport_h,
        scrollable,
    }
}

/// Designed layout, then the same pass at the clamped client width so
/// right-aligned controls stay inside a work area narrower than 500 logical.
fn layout_to_work(
    scale: f32,
    work: WorkRect,
    preferred_origin: Option<(i32, i32)>,
    nc_w: i32,
    nc_h: i32,
) -> (LaidOut, FittedWindow) {
    let designed_cw = (LOGICAL_WIDTH as f32 * scale) as i32;
    let laid = build_layout(scale, designed_cw);
    let fitted = fit_settings_window(designed_cw, laid.height, nc_w, nc_h, work, preferred_origin);
    let laid = if fitted.client_w != designed_cw {
        build_layout(scale, fitted.client_w)
    } else {
        laid
    };
    (laid, fitted)
}

fn max_scroll(content_h: i32, viewport_h: i32) -> i32 {
    (content_h - viewport_h).max(0)
}

/// Scroll so `[top, bottom)` sits inside the viewport, then clamp.
fn scroll_rect_into_view(
    scroll_y: i32,
    top: i32,
    bottom: i32,
    viewport_h: i32,
    content_h: i32,
) -> i32 {
    let mut y = scroll_y;
    if top < y {
        y = top;
    } else if bottom > y + viewport_h {
        y = bottom - viewport_h;
    }
    y.clamp(0, max_scroll(content_h, viewport_h))
}

/// Keyboard/hover index, or -1 when a relayout left that slot empty.
fn reachable_index(index: i32, controls: &[(RECT, Ctrl)]) -> i32 {
    if index < 0 || index as usize >= controls.len() {
        -1
    } else {
        index
    }
}

/// Space/Return activate the focused control. Alt+Space is the system menu
/// (WM_SYSKEYDOWN) and must reach DefWindowProc.
fn key_activates_focus(vk: u32, alt_down: bool) -> bool {
    !alt_down && (vk == VK_RETURN.0 as u32 || vk == VK_SPACE.0 as u32)
}

fn hit_test_control(controls: &[(RECT, Ctrl)], x: i32, y: i32, scroll_y: i32) -> i32 {
    let content_y = y + scroll_y;
    controls
        .iter()
        .position(|(r, _)| x >= r.left && x < r.right && content_y >= r.top && content_y < r.bottom)
        .map(|i| i as i32)
        .unwrap_or(-1)
}

/// Next control, wrapping. `from == -1` starts at the beginning (or the end
/// when reverse).
fn next_reachable(controls: &[(RECT, Ctrl)], from: i32, reverse: bool) -> i32 {
    let n = controls.len() as i32;
    if n == 0 {
        return -1;
    }
    let start = if from < 0 {
        if reverse {
            n
        } else {
            -1
        }
    } else {
        from
    };
    if reverse {
        (start - 1).rem_euclid(n)
    } else {
        (start + 1).rem_euclid(n)
    }
}

fn work_area_at(point: POINT) -> WorkRect {
    let monitor = unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) };
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !unsafe { GetMonitorInfoW(monitor, &mut mi) }.as_bool() {
        return WorkRect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
    }
    WorkRect::from_rect(mi.rcWork)
}

fn non_client_delta(content_w: i32, content_h: i32, scale: f32) -> (i32, i32) {
    let outer = crate::dpi::outer_bounds(
        RECT {
            left: 0,
            top: 0,
            right: content_w,
            bottom: content_h,
        },
        WS_CAPTION | WS_SYSMENU,
        WS_EX_APPWINDOW,
        scale,
    );
    (
        (outer.right - outer.left) - content_w,
        (outer.bottom - outer.top) - content_h,
    )
}

struct State {
    cfg: Config,
    font: HFONT,
    font_small: HFONT,
    controls: Vec<(RECT, Ctrl)>,
    chrome: Vec<(RECT, Chrome)>,
    /// True while the shortcut chip is waiting for a key. Purely a window
    /// mode: nothing is written until a usable combo arrives.
    capturing: bool,
    hover: i32,
    /// Keyboard focus into `controls`, or -1 until Tab/arrows give one.
    focus: i32,
    scale: f32,
    width: i32,
    height: i32,
    /// Visible client height after clamping to the work area (SBS-753).
    viewport_h: i32,
    scroll_y: i32,
    theme: crate::theme::Theme,
    /// Last work area we fitted to. Compared on WM_WINDOWPOSCHANGED so a
    /// same-DPI drag onto a smaller monitor refits without fighting a drag
    /// that stays on the same monitor.
    last_work: WorkRect,
    /// SetWindowPos from apply_fit re-enters WM_WINDOWPOSCHANGED.
    reclamping: bool,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

unsafe fn make_font(height: i32) -> HFONT {
    CreateFontW(
        height,
        0,
        0,
        0,
        400,
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

fn s(state: &State, v: i32) -> i32 {
    (v as f32 * state.scale) as i32
}

/// A finished layout pass: where every control and every piece of painted
/// furniture goes, and how tall the window ended up.
struct LaidOut {
    controls: Vec<(RECT, Ctrl)>,
    chrome: Vec<(RECT, Chrome)>,
    height: i32,
}

/// Lay the window out top to bottom for a given monitor scale.
///
/// Extracted from `open` so `WM_DPICHANGED` can re-run exactly the same pass
/// rather than a second copy of it. Returns the controls, the chrome, and the
/// client height the sequence ended up needing.
fn build_layout(scale: f32, cw: i32) -> LaidOut {
    let mut l = Layout::new(scale, cw);

    l.gap(20);
    l.header(Chrome::SaveFolderHeader, 20);
    l.gap(2);
    l.path_row(Chrome::SavePath, Ctrl::ChangeDir, Ctrl::OpenDir);

    l.gap(10);
    l.header(Chrome::VideoFolderHeader, 20);
    l.gap(4);
    l.path_row(Chrome::VideoPath, Ctrl::ChangeVideoDir, Ctrl::OpenVideoDir);

    l.gap(16);
    l.header(Chrome::RenderQualityHeader, 20);
    l.gap(4);
    l.chips(
        &[Ctrl::Scale(1), Ctrl::Scale(2), Ctrl::Scale(3)],
        54,
        62,
        0,
        30,
    );

    // Finished screenshot size. These cap the completed matte and never
    // upscale a smaller image.
    l.gap(10);
    l.header(Chrome::ScreenshotSizeHeader, 22);
    l.gap(2);
    l.chips(
        &[
            Ctrl::OutputSize(crate::output::OUTPUT_ORIGINAL),
            Ctrl::OutputSize(crate::output::OUTPUT_EMAIL),
            Ctrl::OutputSize(crate::output::OUTPUT_COMPACT),
            Ctrl::CustomSize,
        ],
        102,
        110,
        0,
        30,
    );

    // Checkboxes, grouped: app and editor behavior, then recording,
    // then updates.
    l.gap(16);
    l.checkbox(Ctrl::Autostart);
    l.gap(4);
    l.checkbox(Ctrl::Prtscn);
    // Sits with the PrtScn toggle: both decide how capture is reached.
    l.gap(4);
    let hotkey_top = l.y;
    l.chips(&[Ctrl::CaptureHotkey], 190, 0, 150, 28);
    l.chrome_at(hotkey_top, 28, 145, Chrome::HotkeyLabel);

    // Delay sits under the shortcut: both are about arming a capture.
    l.gap(4);
    let delay_top = l.y;
    let delays: Vec<Ctrl> = crate::delay::CHOICES
        .iter()
        .map(|seconds| Ctrl::CaptureDelay(*seconds))
        .collect();
    l.chips(&delays, 54, 62, 150, 28);
    l.chrome_at(delay_top, 28, 145, Chrome::DelayLabel);

    l.gap(4);
    l.checkbox(Ctrl::KeepEditorOpen);
    l.gap(4);
    l.checkbox(Ctrl::RecordGif);

    // Frame rate stays intentionally constrained: 30 for efficiency, 60 for
    // smooth motion without turning Settings into an encoder control panel.
    l.gap(4);
    let frame_rate_top = l.y;
    l.chips(
        &[Ctrl::FrameRate(30), Ctrl::FrameRate(60)],
        104,
        112,
        150,
        28,
    );
    l.chrome_at(frame_rate_top, 28, 145, Chrome::FrameRateLabel);

    // Recording audio: the label shares its line with the chips.
    l.gap(4);
    let audio_top = l.y;
    l.chips(
        &[
            Ctrl::Audio("off"),
            Ctrl::Audio("system"),
            Ctrl::Audio("mic"),
        ],
        70,
        78,
        150,
        28,
    );
    l.chrome_at(audio_top, 28, 145, Chrome::AudioLabel);

    l.gap(4);
    l.checkbox(Ctrl::AutoUpdate);

    // Support and privacy actions, above the footer.
    l.gap(6);
    l.button_pair(Ctrl::Diagnostics, Ctrl::ClearHistoryTitles, 190, 198);

    let height = l.finish();
    LaidOut {
        controls: l.controls,
        chrome: l.chrome,
        height,
    }
}

unsafe fn draw_text_in(hdc: HDC, font: HFONT, color: COLORREF, r: RECT, text: &str, flags: u32) {
    SelectObject(hdc, font);
    SetTextColor(hdc, color);
    let mut t = wide(text);
    let mut rc = r;
    DrawTextW(
        hdc,
        &mut t,
        &mut rc,
        windows::Win32::Graphics::Gdi::DRAW_TEXT_FORMAT(flags) | DT_SINGLELINE | DT_VCENTER,
    );
}

/// Selected chips fill *and* stroke with the accent, so hover/focus would
/// match the unfocused selected chip unless the ring is a distinct stroke.
fn chip_focus_ring_visible(_active: bool, focused: bool) -> bool {
    focused
}

unsafe fn draw_chip_button(hdc: HDC, r: RECT, label: &str, state: &State, active: bool, hot: bool) {
    let fill = CreateSolidBrush(if active {
        state.theme.accent
    } else {
        state.theme.chip
    });
    let ring = chip_focus_ring_visible(active, hot);
    let pen = CreatePen(
        PS_SOLID,
        if ring { 2 } else { 1 },
        if ring {
            if active {
                state.theme.accent_text
            } else {
                state.theme.text
            }
        } else if active {
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
    let color = if active {
        state.theme.accent_text
    } else if hot {
        state.theme.text
    } else {
        state.theme.muted
    };
    SelectObject(hdc, state.font);
    SetTextColor(hdc, color);
    let mut t = wide(label);
    let mut rc = r;
    DrawTextW(
        hdc,
        &mut t,
        &mut rc,
        windows::Win32::Graphics::Gdi::DT_CENTER | DT_SINGLELINE | DT_VCENTER,
    );
}

unsafe fn draw_checkbox(hdc: HDC, r: RECT, label: &str, state: &State, checked: bool, hot: bool) {
    let box_r = RECT {
        left: r.left,
        top: r.top + (r.bottom - r.top - s(state, 18)) / 2,
        right: r.left + s(state, 18),
        bottom: r.top + (r.bottom - r.top - s(state, 18)) / 2 + s(state, 18),
    };
    let fill = CreateSolidBrush(if checked {
        state.theme.accent
    } else {
        state.theme.chip
    });
    let pen = CreatePen(
        PS_SOLID,
        1,
        if checked {
            state.theme.accent
        } else {
            state.theme.chip_line
        },
    );
    let ob = SelectObject(hdc, fill);
    let op = SelectObject(hdc, pen);
    let _ = RoundRect(
        hdc,
        box_r.left,
        box_r.top,
        box_r.right,
        box_r.bottom,
        s(state, 6),
        s(state, 6),
    );
    SelectObject(hdc, ob);
    SelectObject(hdc, op);
    let _ = DeleteObject(fill);
    let _ = DeleteObject(pen);
    if checked {
        draw_text_in(
            hdc,
            state.font_small,
            state.theme.accent_text,
            box_r,
            "\u{2713}",
            1, /*DT_CENTER*/
        );
    }
    let label_r = RECT {
        left: box_r.right + s(state, 10),
        ..r
    };
    draw_text_in(
        hdc,
        state.font,
        if hot {
            state.theme.accent
        } else {
            state.theme.text
        },
        label_r,
        label,
        0,
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

    // Furniture first, from the same layout pass that placed the controls.
    // These used to be drawn at their own hardcoded coordinates, which is what
    // made inserting a row a two-list edit.
    for (r, chrome) in &state.chrome {
        match chrome {
            Chrome::SaveFolderHeader => draw_text_in(
                hdc,
                state.font_small,
                state.theme.muted,
                *r,
                "SAVE FOLDER",
                0,
            ),
            Chrome::VideoFolderHeader => draw_text_in(
                hdc,
                state.font_small,
                state.theme.muted,
                *r,
                "VIDEO FOLDER  (recordings)",
                0,
            ),
            Chrome::RenderQualityHeader => draw_text_in(
                hdc,
                state.font_small,
                state.theme.muted,
                *r,
                "RENDER QUALITY  (2x recommended for sharing)",
                0,
            ),
            Chrome::ScreenshotSizeHeader => {
                // Resolved here, not at layout time: refresh reloads the
                // config and repaints without rebuilding the layout.
                let summary = match state.cfg.output_max_edge {
                    crate::output::OUTPUT_ORIGINAL => "Original pixels".to_string(),
                    value => format!("{value} px maximum edge"),
                };
                draw_text_in(
                    hdc,
                    state.font_small,
                    state.theme.muted,
                    *r,
                    &format!("SCREENSHOT SIZE  ({summary})"),
                    0,
                );
            }
            Chrome::AudioLabel => {
                draw_text_in(hdc, state.font, state.theme.text, *r, "Recording audio", 0)
            }
            Chrome::FrameRateLabel => {
                draw_text_in(hdc, state.font, state.theme.text, *r, "Frame rate", 0)
            }
            Chrome::HotkeyLabel => {
                draw_text_in(hdc, state.font, state.theme.text, *r, "Capture shortcut", 0)
            }
            Chrome::DelayLabel => {
                draw_text_in(hdc, state.font, state.theme.text, *r, "Capture delay", 0)
            }
            // The paths stop short of the buttons sharing their line.
            Chrome::SavePath | Chrome::VideoPath => {
                let text = if *chrome == Chrome::SavePath {
                    state.cfg.save_dir().display().to_string()
                } else {
                    state.cfg.video_dir().display().to_string()
                };
                SelectObject(hdc, state.font);
                SetTextColor(hdc, state.theme.text);
                let mut wide_text = wide(&text);
                let mut rc = RECT {
                    right: r.right - s(state, 150),
                    ..*r
                };
                DrawTextW(
                    hdc,
                    &mut wide_text,
                    &mut rc,
                    DT_LEFT | DT_END_ELLIPSIS | DT_SINGLELINE | DT_VCENTER,
                );
            }
        }
    }

    for (r, c) in &state.controls {
        let hot = state
            .controls
            .iter()
            .position(|(rr, _)| rr == r)
            .map(|i| {
                let i = i as i32;
                i == state.hover || i == state.focus
            })
            .unwrap_or(false);
        match c {
            Ctrl::ChangeDir | Ctrl::ChangeVideoDir => {
                draw_chip_button(hdc, *r, "Change\u{2026}", state, false, hot)
            }
            Ctrl::OpenDir | Ctrl::OpenVideoDir => {
                draw_chip_button(hdc, *r, "Open", state, false, hot)
            }
            Ctrl::Scale(n) => draw_chip_button(
                hdc,
                *r,
                &format!("{n}x"),
                state,
                state.cfg.export_scale == *n,
                hot,
            ),
            Ctrl::OutputSize(max_edge) => draw_chip_button(
                hdc,
                *r,
                &crate::output::output_size_label(*max_edge),
                state,
                state.cfg.output_max_edge == *max_edge,
                hot,
            ),
            Ctrl::CustomSize => {
                let custom = ![
                    crate::output::OUTPUT_ORIGINAL,
                    crate::output::OUTPUT_EMAIL,
                    crate::output::OUTPUT_COMPACT,
                ]
                .contains(&state.cfg.output_max_edge);
                let custom_label = format!("{}px", state.cfg.output_max_edge);
                draw_chip_button(
                    hdc,
                    *r,
                    if custom { &custom_label } else { "Custom" },
                    state,
                    custom,
                    hot,
                )
            }
            Ctrl::Autostart => draw_checkbox(
                hdc,
                *r,
                "Start with Windows",
                state,
                tray::autostart_enabled(),
                hot,
            ),
            Ctrl::Prtscn => {
                draw_checkbox(
                    hdc,
                    *r,
                    "Capture the PrtScn key",
                    state,
                    state.cfg.capture_prtscn,
                    hot,
                );
                let (label, color) = if !state.cfg.capture_prtscn {
                    ("Off", state.theme.muted)
                } else if prtscn::owns_key() {
                    ("Active", state.theme.accent)
                } else {
                    ("Reconnecting\u{2026}", state.theme.muted)
                };
                draw_text_in(
                    hdc,
                    state.font_small,
                    color,
                    RECT {
                        left: r.right - s(state, 130),
                        top: r.top,
                        right: r.right,
                        bottom: r.bottom,
                    },
                    label,
                    DT_RIGHT.0,
                );
            }
            Ctrl::RecordGif => draw_checkbox(
                hdc,
                *r,
                "Also save a GIF when recording",
                state,
                state.cfg.record_gif,
                hot,
            ),
            Ctrl::FrameRate(fps) => draw_chip_button(
                hdc,
                *r,
                if *fps == 60 {
                    "Smooth 60"
                } else {
                    "Standard 30"
                },
                state,
                state.cfg.record_fps() == *fps,
                hot,
            ),
            Ctrl::AutoUpdate => draw_checkbox(
                hdc,
                *r,
                "Install updates automatically",
                state,
                state.cfg.auto_update,
                hot,
            ),
            Ctrl::KeepEditorOpen => draw_checkbox(
                hdc,
                *r,
                "Keep the editor open after Copy",
                state,
                state.cfg.keep_editor_open,
                hot,
            ),
            Ctrl::Audio(mode) => draw_chip_button(
                hdc,
                *r,
                match *mode {
                    "system" => "System",
                    "mic" => "Mic",
                    _ => "Off",
                },
                state,
                state.cfg.record_audio == *mode,
                hot,
            ),
            Ctrl::CaptureHotkey => {
                // While capturing, the chip is the prompt; there is nowhere
                // else on this row to put one.
                let label = if state.capturing {
                    "Press a key\u{2026}".to_string()
                } else {
                    crate::hotkey::label(state.cfg.capture_hotkey())
                };
                draw_chip_button(hdc, *r, &label, state, state.capturing, hot);
                if !state.capturing && crate::capture_hotkey_taken() {
                    draw_text_in(
                        hdc,
                        state.font_small,
                        state.theme.muted,
                        RECT {
                            left: r.right + s(state, 8),
                            top: r.top,
                            right: state.width - s(state, 24),
                            bottom: r.bottom,
                        },
                        "In use",
                        0,
                    );
                }
            }
            Ctrl::CaptureDelay(seconds) => draw_chip_button(
                hdc,
                *r,
                &format!("{seconds}s"),
                state,
                crate::delay::sanitize(state.cfg.capture_delay_secs) == *seconds,
                hot,
            ),
            Ctrl::Diagnostics => draw_chip_button(hdc, *r, "Copy diagnostics", state, false, hot),
            Ctrl::ClearHistoryTitles => {
                draw_chip_button(hdc, *r, "Clear History titles\u{2026}", state, false, hot)
            }
        }
    }

    let m = s(state, 24);

    // Footer
    let footer = RECT {
        left: m,
        top: state.height - s(state, 34),
        right: state.width - m,
        bottom: state.height - s(state, 10),
    };
    let footer_text = format!(
        "Matteshot {}   \u{00b7}   Free and open source",
        env!("CARGO_PKG_VERSION")
    );
    draw_text_in(
        hdc,
        state.font_small,
        state.theme.muted,
        footer,
        &footer_text,
        0,
    );
}

unsafe fn pick_folder(hwnd: HWND) -> Option<String> {
    let dialog: IFileOpenDialog =
        CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
    let opts = dialog.GetOptions().ok()?;
    dialog
        .SetOptions(opts | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM)
        .ok()?;
    dialog.Show(hwnd).ok()?;
    let item = dialog.GetResult().ok()?;
    let pw = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
    let path = pw.to_string().ok();
    CoTaskMemFree(Some(pw.0 as *const _));
    path
}

unsafe fn update_config(hwnd: HWND, state: &mut State, change: impl FnOnce(&mut Config)) -> bool {
    match Config::update(change) {
        Ok(config) => {
            state.cfg = config;
            true
        }
        Err(error) => {
            let message = format!(
                "Matteshot could not save that setting. Your existing settings were left unchanged.\n\n{error:#}"
            );
            let title: Vec<u16> = "Matteshot\0".encode_utf16().collect();
            let message: Vec<u16> = format!("{message}\0").encode_utf16().collect();
            let _ = MessageBoxW(
                hwnd,
                windows::core::PCWSTR(message.as_ptr()),
                windows::core::PCWSTR(title.as_ptr()),
                MB_OK | MB_ICONERROR,
            );
            false
        }
    }
}

unsafe fn activate(hwnd: HWND, state: &mut State, ctrl: Ctrl) {
    match ctrl {
        Ctrl::ChangeDir => {
            if let Some(path) = pick_folder(hwnd) {
                update_config(hwnd, state, |cfg| cfg.save_dir = Some(path.into()));
            }
        }
        Ctrl::OpenDir => crate::output::open_folder(&state.cfg.save_dir()),
        Ctrl::ChangeVideoDir => {
            if let Some(path) = pick_folder(hwnd) {
                update_config(hwnd, state, |cfg| cfg.video_dir = Some(path.into()));
            }
        }
        Ctrl::OpenVideoDir => crate::output::open_folder(&state.cfg.video_dir()),
        Ctrl::Scale(n) => {
            update_config(hwnd, state, |cfg| cfg.export_scale = n);
        }
        Ctrl::OutputSize(max_edge) => {
            update_config(hwnd, state, |cfg| cfg.output_max_edge = max_edge);
        }
        Ctrl::CustomSize => {
            if let Ok(Some(max_edge)) = crate::number_prompt::ask(hwnd, state.cfg.output_max_edge) {
                update_config(hwnd, state, |cfg| cfg.output_max_edge = max_edge);
            }
        }
        Ctrl::Autostart => {
            // SBS-759: the write can fail (denied, missing, or read-only
            // Startup folder). Discarding that Result made the checkbox look
            // like it changed or did nothing, with no explanation. Re-read
            // the shortcut after the attempt so the paint pass, which uses
            // `tray::autostart_enabled`, stays on the actual file, and show
            // the same class of error dialog other Settings writes use.
            let want = !tray::autostart_enabled();
            let outcome = crate::autostart_toggle::apply_autostart_toggle(
                want,
                tray::set_autostart,
                tray::autostart_enabled,
            );
            if let Some(event) = outcome.diagnostic {
                crate::diagnostics::log(event);
            }
            if let Some(error) = outcome.error {
                let title: Vec<u16> = "Matteshot\0".encode_utf16().collect();
                let message: Vec<u16> = format!("{error}\0").encode_utf16().collect();
                let _ = MessageBoxW(
                    hwnd,
                    windows::core::PCWSTR(message.as_ptr()),
                    windows::core::PCWSTR(title.as_ptr()),
                    MB_OK | MB_ICONERROR,
                );
            }
        }
        Ctrl::Prtscn => {
            let enabled = !state.cfg.capture_prtscn;
            if update_config(hwnd, state, |cfg| cfg.capture_prtscn = enabled) {
                prtscn::set_preferred(state.cfg.capture_prtscn);
                if state.cfg.capture_prtscn {
                    let _ = prtscn::take(HOTKEY_ID_PRTSCN);
                } else {
                    prtscn::release(HOTKEY_ID_PRTSCN);
                }
            }
        }
        Ctrl::CaptureHotkey => {
            state.capturing = true;
        }
        Ctrl::CaptureDelay(seconds) => {
            update_config(hwnd, state, |cfg| cfg.capture_delay_secs = seconds);
        }
        Ctrl::RecordGif => {
            let enabled = !state.cfg.record_gif;
            update_config(hwnd, state, |cfg| cfg.record_gif = enabled);
        }
        Ctrl::FrameRate(fps) => {
            update_config(hwnd, state, |cfg| cfg.record_fps = fps);
        }
        Ctrl::AutoUpdate => {
            let enabled = !state.cfg.auto_update;
            update_config(hwnd, state, |cfg| cfg.auto_update = enabled);
        }
        Ctrl::KeepEditorOpen => {
            let enabled = !state.cfg.keep_editor_open;
            update_config(hwnd, state, |cfg| cfg.keep_editor_open = enabled);
        }
        Ctrl::Audio(mode) => {
            update_config(hwnd, state, |cfg| cfg.record_audio = mode.to_string());
        }
        Ctrl::Diagnostics => match crate::diagnostics::copy_report() {
            Ok(()) => {
                let _ = MessageBoxW(
                    None,
                    w!("Copied a privacy-safe support report."),
                    w!("Matteshot"),
                    MB_OK | MB_ICONINFORMATION,
                );
            }
            Err(error) => {
                eprintln!("diagnostics copy failed: {error:#}");
                let text = format!("Could not copy diagnostics:\n\n{error:#}");
                let _ = MessageBoxW(
                    None,
                    PCWSTR(HSTRING::from(text).as_ptr()),
                    w!("Matteshot"),
                    MB_OK | MB_ICONERROR,
                );
            }
        },
        Ctrl::ClearHistoryTitles => {
            // Confirmation is the explicit request. Yes clears titles only;
            // capture files stay unless the user uses History Delete.
            let confirmed = MessageBoxW(
                hwnd,
                w!("Remove stored window titles from History?\r\n\r\nScreenshot and video files stay on disk."),
                w!("Matteshot"),
                MB_YESNO | MB_ICONWARNING,
            ) == IDYES;
            if confirmed {
                match crate::history::clear_source_metadata() {
                    Ok(cleared) => {
                        crate::history::reload_if_open();
                        let text = if cleared == 0 {
                            "History had no stored titles.".to_string()
                        } else {
                            format!(
                                "Cleared {cleared} stored title{}. Capture files were not deleted.",
                                if cleared == 1 { "" } else { "s" }
                            )
                        };
                        let _ = MessageBoxW(
                            hwnd,
                            PCWSTR(HSTRING::from(text).as_ptr()),
                            w!("Matteshot"),
                            MB_OK | MB_ICONINFORMATION,
                        );
                    }
                    Err(error) => {
                        let text = format!("History titles could not be cleared.\n\n{error:#}");
                        let _ = MessageBoxW(
                            hwnd,
                            PCWSTR(HSTRING::from(text).as_ptr()),
                            w!("Matteshot"),
                            MB_OK | MB_ICONERROR,
                        );
                    }
                }
            }
        }
    }
    let _ = InvalidateRect(hwnd, None, false);
}

unsafe fn sync_viewport(hwnd: HWND, state: &mut State) {
    let mut client = RECT::default();
    if GetClientRect(hwnd, &mut client).is_err() {
        // Unknown client: keep the last viewport rather than treating a
        // failed query as a zero-height window that cannot scroll.
        return;
    }
    let vh = client.bottom - client.top;
    if vh <= 0 {
        return;
    }
    state.viewport_h = vh;
    state.scroll_y = state
        .scroll_y
        .clamp(0, max_scroll(state.height, state.viewport_h));
}

fn window_origin(hwnd: HWND) -> Option<(i32, i32)> {
    let mut rc = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut rc) }.is_ok() {
        Some((rc.left, rc.top))
    } else {
        None
    }
}

fn work_area_for_origin(origin: Option<(i32, i32)>) -> WorkRect {
    match origin {
        Some((x, y)) => work_area_at(POINT { x, y }),
        None => WorkRect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        },
    }
}

/// Relayout, clamp to `work`, and move. `reclamping` stops SetWindowPos
/// from re-entering WM_WINDOWPOSCHANGED.
unsafe fn apply_fit(hwnd: HWND, state: &mut State, work: WorkRect, origin: Option<(i32, i32)>) {
    state.reclamping = true;
    let (nc_w, nc_h) = {
        let designed_cw = (LOGICAL_WIDTH as f32 * state.scale) as i32;
        let laid = build_layout(state.scale, designed_cw);
        non_client_delta(designed_cw, laid.height, state.scale)
    };
    let (laid, fitted) = layout_to_work(state.scale, work, origin, nc_w, nc_h);
    state.controls = laid.controls;
    state.chrome = laid.chrome;
    state.width = fitted.client_w;
    state.height = laid.height;
    state.hover = reachable_index(state.hover, &state.controls);
    state.focus = reachable_index(state.focus, &state.controls);
    let _ = SetWindowPos(
        hwnd,
        None,
        fitted.x,
        fitted.y,
        fitted.outer_w,
        fitted.outer_h,
        SWP_NOZORDER | SWP_NOACTIVATE,
    );
    sync_viewport(hwnd, state);
    state.last_work = work;
    state.reclamping = false;
    if state.focus >= 0 {
        move_focus(state, state.focus);
    }
}

/// Refit using the window's current origin. Used when the window is already
/// open and when the display topology changes.
unsafe fn reclamp_to_monitor(hwnd: HWND, state: &mut State) {
    if state.reclamping {
        return;
    }
    let origin = window_origin(hwnd);
    let work = work_area_for_origin(origin);
    apply_fit(hwnd, state, work, origin);
}

/// Same-DPI monitor changes do not send WM_DPICHANGED. Skip when the
/// nearest work rect is unchanged so a drag on one monitor is not fought.
unsafe fn reclamp_if_work_changed(hwnd: HWND, state: &mut State) {
    if state.reclamping {
        return;
    }
    let origin = window_origin(hwnd);
    let work = work_area_for_origin(origin);
    if work == state.last_work {
        return;
    }
    apply_fit(hwnd, state, work, origin);
}

fn move_focus(state: &mut State, index: i32) {
    state.focus = index;
    if index < 0 {
        return;
    }
    if let Some((r, _)) = state.controls.get(index as usize) {
        state.scroll_y = scroll_rect_into_view(
            state.scroll_y,
            r.top,
            r.bottom,
            state.viewport_h,
            state.height,
        );
    }
}

unsafe fn handle_settings_key(
    hwnd: HWND,
    state: &mut State,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let vk = wparam.0 as u32;
    let shift = (GetKeyState(VK_SHIFT.0 as i32) as u16 & 0x8000) != 0;
    if vk == VK_TAB.0 as u32 {
        let next = next_reachable(&state.controls, state.focus, shift);
        move_focus(state, next);
        let _ = InvalidateRect(hwnd, None, false);
        return LRESULT(0);
    }
    if vk == VK_DOWN.0 as u32 || vk == VK_UP.0 as u32 {
        let next = next_reachable(&state.controls, state.focus, vk == VK_UP.0 as u32);
        move_focus(state, next);
        let _ = InvalidateRect(hwnd, None, false);
        return LRESULT(0);
    }
    let alt = (GetKeyState(VK_MENU.0 as i32) as u16 & 0x8000) != 0;
    if key_activates_focus(vk, alt) && state.focus >= 0 {
        if let Some((_, ctrl)) = state.controls.get(state.focus as usize) {
            let ctrl = *ctrl;
            activate(hwnd, state, ctrl);
        }
        return LRESULT(0);
    }
    if vk == VK_PRIOR.0 as u32 {
        state.scroll_y = (state.scroll_y - state.viewport_h.max(1))
            .clamp(0, max_scroll(state.height, state.viewport_h));
        let _ = InvalidateRect(hwnd, None, false);
        return LRESULT(0);
    }
    if vk == VK_NEXT.0 as u32 {
        state.scroll_y = (state.scroll_y + state.viewport_h.max(1))
            .clamp(0, max_scroll(state.height, state.viewport_h));
        let _ = InvalidateRect(hwnd, None, false);
        return LRESULT(0);
    }
    if vk == VK_HOME.0 as u32 {
        state.scroll_y = 0;
        let _ = InvalidateRect(hwnd, None, false);
        return LRESULT(0);
    }
    if vk == VK_END.0 as u32 {
        state.scroll_y = max_scroll(state.height, state.viewport_h);
        let _ = InvalidateRect(hwnd, None, false);
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            let cs = &*(lparam.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        // The window's height falls out of the layout pass, so re-run that
        // pass at the new scale rather than accepting the size Windows
        // suggests, which is only the old one multiplied by the DPI ratio. Its
        // suggested position is still taken: that is what keeps the window on
        // the monitor it was dragged to.
        windows::Win32::UI::WindowsAndMessaging::WM_DPICHANGED => {
            if let Some(state) = state_of(hwnd) {
                let scale = crate::dpi::scale_from_message(wparam);
                state.scale = scale;
                // Create first, then swap: deleting up front leaves the
                // window with no font at all if a creation fails.
                for (slot, height) in [
                    (&mut state.font as *mut HFONT, -(15.0 * scale) as i32),
                    (&mut state.font_small as *mut HFONT, -(12.0 * scale) as i32),
                ] {
                    let replacement = make_font(height);
                    if replacement.is_invalid() {
                        continue;
                    }
                    let previous = std::mem::replace(&mut *slot, replacement);
                    if !previous.is_invalid() {
                        let _ = DeleteObject(previous);
                    }
                }
                // Hover is an index into the controls just replaced.
                state.hover = -1;
                // Suggested origin keeps the window on the monitor it was
                // dragged to; apply_fit then clamps size to that work area.
                let origin = crate::dpi::suggested_origin(lparam).or_else(|| window_origin(hwnd));
                let work = work_area_for_origin(origin);
                apply_fit(hwnd, state, work, origin);
                state.scroll_y = 0;
                if state.focus >= 0 {
                    move_focus(state, state.focus);
                }
                let _ = InvalidateRect(hwnd, None, true);
            }
            LRESULT(0)
        }
        // Resolution / monitor topology. Same-DPI moves are handled below.
        WM_DISPLAYCHANGE => {
            if let Some(state) = state_of(hwnd) {
                reclamp_to_monitor(hwnd, state);
                let _ = InvalidateRect(hwnd, None, true);
            }
            LRESULT(0)
        }
        // Dragging onto a smaller same-DPI monitor does not send
        // WM_DPICHANGED. Refit only when the nearest work rect changes.
        WM_WINDOWPOSCHANGED => {
            let result = DefWindowProcW(hwnd, msg, wparam, lparam);
            if let Some(state) = state_of(hwnd) {
                reclamp_if_work_changed(hwnd, state);
            }
            result
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            if let Some(state) = state_of(hwnd) {
                let mut ps = windows::Win32::Graphics::Gdi::PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                let mut client = RECT::default();
                let (vw, vh) = if GetClientRect(hwnd, &mut client).is_ok() {
                    (
                        (client.right - client.left).max(0),
                        (client.bottom - client.top).max(0),
                    )
                } else {
                    (state.width, state.viewport_h.max(1))
                };
                let mem = CreateCompatibleDC(hdc);
                let bmp = CreateCompatibleBitmap(hdc, state.width.max(1), state.height.max(1));
                let old = SelectObject(mem, bmp);
                paint(mem, state);
                let _ = BitBlt(hdc, 0, 0, vw, vh, mem, 0, state.scroll_y, SRCCOPY);
                SelectObject(mem, old);
                let _ = DeleteObject(bmp);
                let _ = DeleteDC(mem);
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        // Alt combinations arrive as WM_SYSKEYDOWN, so both are needed or
        // every shortcut containing Alt would be uncapturable.
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            // state_of hands out a &'static mut from a raw pointer, so it is
            // taken exactly once. Testing `capturing` in a match guard and
            // again in the body would alias two &mut to the same State.
            let Some(state) = state_of(hwnd) else {
                return DefWindowProcW(hwnd, msg, wparam, lparam);
            };
            if !state.capturing {
                return handle_settings_key(hwnd, state, msg, wparam, lparam);
            }
            {
                let vk = wparam.0 as u32;
                let down = |key: VIRTUAL_KEY| (GetKeyState(key.0 as i32) as u16 & 0x8000) != 0;
                if vk == VK_ESCAPE.0 as u32 {
                    state.capturing = false;
                    let _ = InvalidateRect(hwnd, None, false);
                    return LRESULT(0);
                }
                // Holding a modifier is not yet a choice; keep waiting for the
                // key it modifies rather than binding Ctrl on its own.
                let modifier_only = [VK_CONTROL, VK_MENU, VK_SHIFT, VK_LWIN, VK_RWIN]
                    .iter()
                    .any(|m| vk == m.0 as u32);
                if modifier_only {
                    return LRESULT(0);
                }
                let mut modifiers = HOT_KEY_MODIFIERS(0);
                if down(VK_CONTROL) {
                    modifiers |= MOD_CONTROL;
                }
                if down(VK_MENU) {
                    modifiers |= MOD_ALT;
                }
                if down(VK_SHIFT) {
                    modifiers |= MOD_SHIFT;
                }
                if down(VK_LWIN) || down(VK_RWIN) {
                    modifiers |= MOD_WIN;
                }
                // A bare key would register system-wide and swallow that key
                // everywhere. Stay in capture mode rather than accept it.
                if modifiers.0 == 0 {
                    return LRESULT(0);
                }
                let chosen = crate::hotkey::Hotkey { modifiers, vk };
                let text = crate::hotkey::label(Some(chosen));
                // Round-trip through the parser so what is stored is something
                // the app can read back. label() has a "0x.." fallback for keys
                // it has no name for, and parse() rejects those, so this
                // refuses them at capture rather than writing a combo that is
                // silently ignored at startup.
                if crate::hotkey::parse(&text).is_none() {
                    return LRESULT(0);
                }
                if update_config(hwnd, state, |cfg| cfg.capture_hotkey = text) {
                    state.capturing = false;
                    crate::rebind_capture_hotkey();
                }
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        // Clicking away or alt-tabbing abandons the capture, so the window is
        // never left silently swallowing keys.
        WM_KILLFOCUS => {
            if let Some(state) = state_of(hwnd) {
                if state.capturing {
                    state.capturing = false;
                    let _ = InvalidateRect(hwnd, None, false);
                }
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if let Some(state) = state_of(hwnd) {
                let (x, y) = (
                    (lparam.0 & 0xFFFF) as i16 as i32,
                    ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
                );
                let hover = hit_test_control(&state.controls, x, y, state.scroll_y);
                if hover != state.hover {
                    state.hover = hover;
                    let _ = InvalidateRect(hwnd, None, false);
                }
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            if let Some(state) = state_of(hwnd) {
                let delta = ((wparam.0 >> 16) & 0xFFFF) as u16 as i16;
                let step = if delta > 0 { -WHEEL_STEP } else { WHEEL_STEP };
                state.scroll_y =
                    (state.scroll_y + step).clamp(0, max_scroll(state.height, state.viewport_h));
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_SIZE => {
            if let Some(state) = state_of(hwnd) {
                sync_viewport(hwnd, state);
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if let Some(state) = state_of(hwnd) {
                let (x, y) = (
                    (lparam.0 & 0xFFFF) as i16 as i32,
                    ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
                );
                let i = hit_test_control(&state.controls, x, y, state.scroll_y);
                if i >= 0 {
                    state.focus = i;
                    let ctrl = state.controls[i as usize].1;
                    activate(hwnd, state, ctrl);
                }
            }
            LRESULT(0)
        }
        // The settings window is persistent: follow live theme flips.
        windows::Win32::UI::WindowsAndMessaging::WM_SETTINGCHANGE => {
            if let Some(state) = state_of(hwnd) {
                state.theme = crate::theme::current();
                crate::theme::apply_titlebar(hwnd, &state.theme);
                let _ = InvalidateRect(hwnd, None, true);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_NCDESTROY => {
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

/// Open (or focus) the settings window. Non-modal; shares the main loop.
pub fn open() -> Result<()> {
    unsafe {
        let existing = HWND(WINDOW.load(Ordering::SeqCst) as *mut _);
        if !existing.0.is_null() && IsWindow(existing).as_bool() {
            let _ = ShowWindow(existing, SW_RESTORE);
            let _ = SetWindowPos(existing, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
            let _ = SetWindowPos(
                existing,
                HWND_NOTOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE,
            );
            let _ = SetForegroundWindow(existing);
            // The window may have been dragged onto a smaller same-DPI
            // monitor, or the taskbar may have grown, since it last fitted.
            if let Some(state) = state_of(existing) {
                reclamp_to_monitor(existing, state);
                let _ = InvalidateRect(existing, None, false);
            }
            eprintln!("settings: focused existing window");
            return Ok(());
        }

        // Centres on the cursor's monitor below (the tray was just clicked
        // there), so that monitor's scale is the one this is sized for.
        let mut cursor = POINT::default();
        let _ = windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut cursor);
        let scale = crate::dpi::scale_for_point(cursor);
        let sc = |v: i32| (v as f32 * scale) as i32;
        let cw = sc(LOGICAL_WIDTH);

        let font = make_font(-sc(15));
        let font_small = make_font(-sc(12));

        // One pass, top to bottom. The gaps below are the spacing that used
        // to be baked into ~37 literal coordinates in this function and 17
        // more in the paint routine; they are derived from that geometry, so
        // the window is pixel-identical to before the cursor existed.

        // Same cursor reading that chose the scale. Sampling it twice lets
        // the pointer cross monitors in between, pairing one monitor's scale
        // with another's work area.
        let work = work_area_at(cursor);
        let probe_h = build_layout(scale, cw).height;
        let (nc_w, nc_h) = non_client_delta(cw, probe_h, scale);
        let (laid_out, fitted) = layout_to_work(scale, work, None, nc_w, nc_h);
        let (controls, chrome, ch) = (laid_out.controls, laid_out.chrome, laid_out.height);

        let state = Box::new(State {
            cfg: Config::load(),
            font,
            font_small,
            controls,
            chrome,
            capturing: false,
            hover: -1,
            focus: -1,
            scale,
            width: fitted.client_w,
            height: ch,
            viewport_h: fitted.viewport_h,
            scroll_y: 0,
            theme: crate::theme::current(),
            last_work: work,
            reclamping: false,
        });

        let hinstance = GetModuleHandleW(None).context("get app module for settings")?;
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hIcon: crate::tray::app_icon(),
            lpszClassName: w!("matteshot_settings"),
            hbrBackground: Default::default(),
            ..Default::default()
        };
        RegisterClassW(&class);

        let leaked = Box::into_raw(state);
        let hwnd = match CreateWindowExW(
            WS_EX_APPWINDOW,
            w!("matteshot_settings"),
            w!("Matteshot settings"),
            WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
            fitted.x,
            fitted.y,
            fitted.outer_w,
            fitted.outer_h,
            None,
            None,
            hinstance,
            Some(leaked as *const _),
        ) {
            Ok(h) => h,
            Err(error) => {
                drop(Box::from_raw(leaked));
                return Err(error).context("create settings window");
            }
        };

        crate::theme::apply_titlebar(hwnd, &crate::theme::current());
        WINDOW.store(hwnd.0 as isize, Ordering::SeqCst);
        if let Some(state) = state_of(hwnd) {
            sync_viewport(hwnd, state);
        }
        let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
        // After a tray menu closes our process has lost its foreground
        // permission, so SetForegroundWindow alone silently fails and the new
        // window is born BEHIND the active app. The topmost toggle forces
        // z-order without needing activation rights.
        let _ = SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
        let _ = SetWindowPos(hwnd, HWND_NOTOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
        let _ = SetForegroundWindow(hwnd);
        eprintln!("settings: opened");
        Ok(())
    }
}

pub fn refresh() {
    unsafe {
        let hwnd = HWND(WINDOW.load(Ordering::SeqCst) as *mut _);
        if !hwnd.0.is_null() && IsWindow(hwnd).as_bool() {
            if let Some(state) = state_of(hwnd) {
                state.cfg = Config::load();
                state.hover = -1;
                reclamp_to_monitor(hwnd, state);
            }
            let _ = InvalidateRect(hwnd, None, false);
        }
    }
}

/// Whether the settings surface still exists.
///
/// The resident shares its normal message loop with this non-modal window, but
/// the standalone `--settings` diagnostic needs to know when the window was
/// closed so it can end its own message loop and exit cleanly.
pub fn is_open() -> bool {
    unsafe {
        let hwnd = HWND(WINDOW.load(Ordering::SeqCst) as *mut _);
        !hwnd.0.is_null() && IsWindow(hwnd).as_bool()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_layout, chip_focus_ring_visible, fit_equal_row, fit_settings_window,
        hit_test_control, key_activates_focus, layout_to_work, max_scroll, next_reachable,
        reachable_index, scroll_rect_into_view, Ctrl, WorkRect, LOGICAL_WIDTH, MIN_VISIBLE_CLIENT,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{VK_RETURN, VK_SPACE};

    /// The window is laid out for the monitor it is on, and `WM_DPICHANGED`
    /// re-runs this same pass. Both only work if the pass is a pure function of
    /// scale: same controls, in the same order, at proportional positions. A
    /// literal that forgot to be scaled would hold still here while everything
    /// around it moved.
    #[test]
    fn the_layout_scales_whole_rather_than_in_parts() {
        let base = build_layout(1.0, 500);

        for scale in [1.25f32, 1.5, 2.0] {
            let width = (500.0 * scale) as i32;
            let scaled = build_layout(scale, width);

            // Same rows, in the same order: scale must not change what the
            // window contains, only how big it is.
            assert_eq!(
                scaled.controls.len(),
                base.controls.len(),
                "control count at {scale}x"
            );
            assert_eq!(
                scaled.chrome.len(),
                base.chrome.len(),
                "chrome count at {scale}x"
            );
            assert!(
                scaled
                    .controls
                    .iter()
                    .zip(&base.controls)
                    .all(|((_, a), (_, b))| a == b),
                "control order changed at {scale}x"
            );

            // Proportional within rounding: `sc` truncates, and the errors
            // accumulate down a column of stacked rows, so this is a couple of
            // pixels per row rather than exact.
            let slack = 4.0 * scale;
            assert!(
                ((scaled.height as f32) - base.height as f32 * scale).abs() <= slack * 4.0,
                "height {} at {scale}x is not {} scaled",
                scaled.height,
                base.height
            );
            for ((rect, _), (base_rect, _)) in scaled.controls.iter().zip(&base.controls) {
                let expected_top = base_rect.top as f32 * scale;
                assert!(
                    (rect.top as f32 - expected_top).abs() <= slack * 4.0,
                    "a control sits at {} at {scale}x, expected about {expected_top}",
                    rect.top
                );
                assert!(rect.right > rect.left && rect.bottom > rect.top);
            }
        }
    }

    /// 1366x768 laptop with a ~40px taskbar. 1920x1080 with the same.
    const LAPTOP: WorkRect = WorkRect {
        left: 0,
        top: 0,
        right: 1366,
        bottom: 728,
    };
    const DESKTOP: WorkRect = WorkRect {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1040,
    };

    /// Typical caption+frame at 96 DPI. Live `outer_bounds` is used on
    /// Windows; tests pin the clamp arithmetic against this stand-in.
    fn non_client(scale: f32) -> (i32, i32) {
        ((16.0 * scale) as i32, (39.0 * scale) as i32)
    }

    /// SBS-753: the unclamped client already exceeds a 1366x768 work area
    /// at 125%. Without `fit_settings_window`, Update/privacy/diagnostics
    /// sit below the taskbar.
    #[test]
    fn unclamped_content_overflows_a_1366x768_work_area_from_125_percent() {
        for scale in [1.25f32, 1.5, 2.0] {
            let width = (LOGICAL_WIDTH as f32 * scale) as i32;
            let laid = build_layout(scale, width);
            let (_nc_w, nc_h) = non_client(scale);
            assert!(
                laid.height + nc_h > LAPTOP.height(),
                "content {} + frame {nc_h} at {scale}x should overflow 728",
                laid.height
            );
        }
    }

    /// SBS-753: final outer size stays inside representative work areas at
    /// 100-200%.
    #[test]
    fn fitted_window_stays_inside_representative_work_areas() {
        for work in [LAPTOP, DESKTOP] {
            for scale in [1.0f32, 1.25, 1.5, 2.0] {
                let width = (LOGICAL_WIDTH as f32 * scale) as i32;
                let laid = build_layout(scale, width);
                let (nc_w, nc_h) = non_client(scale);
                let fitted = fit_settings_window(width, laid.height, nc_w, nc_h, work, None);
                assert!(
                    fitted.outer_w <= work.width(),
                    "outer width {} > work {} at {scale}x",
                    fitted.outer_w,
                    work.width()
                );
                assert!(
                    fitted.outer_h <= work.height(),
                    "outer height {} > work {} at {scale}x",
                    fitted.outer_h,
                    work.height()
                );
                assert!(fitted.x >= work.left && fitted.x + fitted.outer_w <= work.right);
                assert!(fitted.y >= work.top && fitted.y + fitted.outer_h <= work.bottom);
                assert!(fitted.viewport_h >= 1);
                assert_eq!(fitted.scrollable, laid.height > fitted.viewport_h);
                if laid.height + nc_h > work.height() {
                    assert!(
                        fitted.scrollable,
                        "overflow at {scale}x must become scrollable"
                    );
                }
            }
        }
    }

    /// Keyboard navigation can bring the bottom control into a viewport
    /// shorter than the content (the 1366x768 @ 200% case).
    #[test]
    fn keyboard_scroll_reaches_the_bottom_control() {
        let scale = 2.0f32;
        let width = (LOGICAL_WIDTH as f32 * scale) as i32;
        let laid = build_layout(scale, width);
        let (nc_w, nc_h) = non_client(scale);
        let fitted = fit_settings_window(width, laid.height, nc_w, nc_h, LAPTOP, None);
        assert!(fitted.scrollable, "200% on 1366x768 must scroll");

        let last = laid
            .controls
            .iter()
            .enumerate()
            .next_back()
            .expect("settings has controls");
        let (idx, (rect, ctrl)) = last;
        assert!(
            matches!(*ctrl, Ctrl::Diagnostics | Ctrl::ClearHistoryTitles),
            "bottom control should be a footer action, got {ctrl:?}"
        );
        assert!(
            rect.bottom > fitted.viewport_h,
            "bottom control at {} should start below the clamped viewport {}",
            rect.bottom,
            fitted.viewport_h
        );
        let scrolled =
            scroll_rect_into_view(0, rect.top, rect.bottom, fitted.viewport_h, laid.height);
        assert!(
            rect.top >= scrolled && rect.bottom <= scrolled + fitted.viewport_h,
            "scroll {scrolled} should reveal {}-{} in viewport {}",
            rect.top,
            rect.bottom,
            fitted.viewport_h
        );
        // Tab from the start eventually lands on that last control.
        let mut focus = -1;
        let mut seen_last = false;
        for _ in 0..laid.controls.len() + 2 {
            focus = next_reachable(&laid.controls, focus, false);
            if focus == idx as i32 {
                seen_last = true;
                break;
            }
        }
        assert!(seen_last, "Tab should reach control {idx}");
        let _ = idx;
    }

    /// Tab walks every control once per cycle, in layout order, and wraps.
    #[test]
    fn tab_visits_every_control_once_per_cycle() {
        let laid = build_layout(1.0, LOGICAL_WIDTH);
        let n = laid.controls.len() as i32;
        let mut focus = -1;
        for expected in 0..n {
            focus = next_reachable(&laid.controls, focus, false);
            assert_eq!(focus, expected);
        }
        assert_eq!(next_reachable(&laid.controls, focus, false), 0, "Tab wraps");
        assert_eq!(
            next_reachable(&laid.controls, -1, true),
            n - 1,
            "Shift+Tab starts at the end"
        );
        assert_eq!(
            next_reachable(&laid.controls, 0, true),
            n - 1,
            "Shift+Tab wraps"
        );
        assert_eq!(next_reachable(&[], -1, false), -1);
    }

    #[test]
    fn hit_test_uses_scroll_offset() {
        let laid = build_layout(1.0, LOGICAL_WIDTH);
        let (rect, _) = laid.controls[0];
        let x = rect.left + 1;
        let y = 10;
        // Unscrolled, y=10 near the top may miss the first control (it sits
        // below the header). Use the control's own top as the pointer.
        let y_on = 0;
        assert_eq!(hit_test_control(&laid.controls, x, rect.top + 1, 0), 0);
        assert_eq!(
            hit_test_control(&laid.controls, x, y_on, rect.top + 1),
            0,
            "scrolled so the control sits at the top of the viewport"
        );
        let _ = y;
    }

    #[test]
    fn an_unreadable_work_area_does_not_collapse_the_window() {
        let empty = WorkRect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        let fitted = fit_settings_window(500, 676, 16, 39, empty, None);
        assert_eq!(fitted.outer_w, 516);
        assert_eq!(fitted.outer_h, 715);
        assert!(!fitted.scrollable);
        assert_eq!(fitted.viewport_h, 676);
        assert_eq!(fitted.client_w, 500);
        assert_eq!((fitted.x, fitted.y), (0, 0));

        let kept = fit_settings_window(500, 676, 16, 39, empty, Some((100, 200)));
        assert_eq!((kept.x, kept.y), (100, 200));
        assert_eq!(kept.outer_w, 516);
        assert_eq!(kept.outer_h, 715);
        assert_eq!(kept.client_w, 500);
    }

    #[test]
    fn scroll_is_clamped_to_the_overflow_past_the_viewport() {
        assert_eq!(max_scroll(2000, 600), 1400);
        assert_eq!(max_scroll(400, 600), 0);
    }

    #[test]
    fn min_visible_client_is_honoured_until_the_work_area_itself_is_smaller() {
        let tiny = WorkRect {
            left: 0,
            top: 0,
            right: 800,
            bottom: 120,
        };
        let fitted = fit_settings_window(500, 676, 16, 39, tiny, None);
        assert_eq!(fitted.outer_h, 120);
        assert!(fitted.viewport_h < MIN_VISIBLE_CLIENT);
        assert!(fitted.scrollable);
    }

    /// Same-DPI laptop + external: a window fitted to the desktop hangs past
    /// the laptop taskbar until it is re-fitted (SBS-753).
    #[test]
    fn a_taller_window_becomes_scrollable_on_a_shorter_same_dpi_work_area() {
        let scale = 1.25f32;
        let width = (LOGICAL_WIDTH as f32 * scale) as i32;
        let laid = build_layout(scale, width);
        let (nc_w, nc_h) = non_client(scale);
        let on_desktop = fit_settings_window(width, laid.height, nc_w, nc_h, DESKTOP, None);
        assert!(
            laid.height + nc_h > LAPTOP.height(),
            "125% layout must overflow a 728px laptop work area"
        );
        assert!(
            on_desktop.outer_h > LAPTOP.height(),
            "desktop outer height {} must exceed the laptop work area",
            on_desktop.outer_h
        );
        let on_laptop = fit_settings_window(
            width,
            laid.height,
            nc_w,
            nc_h,
            LAPTOP,
            Some((on_desktop.x, on_desktop.y)),
        );
        assert!(
            on_laptop.scrollable,
            "moving onto the laptop must enable scroll"
        );
        assert!(on_laptop.outer_h <= LAPTOP.height());
    }

    #[test]
    fn active_and_focused_chip_has_a_visible_focus_ring() {
        assert!(
            chip_focus_ring_visible(true, true),
            "selected+focused chip must show a ring"
        );
        assert!(
            !chip_focus_ring_visible(true, false),
            "selected without focus must not look focused"
        );
        assert!(chip_focus_ring_visible(false, true));
    }

    #[test]
    fn alt_space_is_not_treated_as_activate() {
        assert!(!key_activates_focus(VK_SPACE.0 as u32, true));
        assert!(!key_activates_focus(VK_RETURN.0 as u32, true));
        assert!(key_activates_focus(VK_SPACE.0 as u32, false));
        assert!(key_activates_focus(VK_RETURN.0 as u32, false));
    }

    #[test]
    fn a_relayout_clears_focus_that_no_longer_points_at_a_control() {
        let laid = build_layout(1.0, LOGICAL_WIDTH);
        let last = laid.controls.len() as i32 - 1;
        assert_eq!(reachable_index(last, &laid.controls), last);
        assert_eq!(reachable_index(-1, &laid.controls), -1);
        assert_eq!(
            reachable_index(last + 1, &laid.controls),
            -1,
            "out-of-range focus clears"
        );
    }

    /// 800px-wide work at 200% is narrower than the designed 1000px client.
    /// Rebuilding at the clamped client keeps every control fully inside
    /// the window so hit-testing still finds it, including the right edge.
    #[test]
    fn a_narrow_work_area_keeps_controls_hit_testable() {
        let scale = 2.0f32;
        let (nc_w, nc_h) = non_client(scale);
        let designed_cw = (LOGICAL_WIDTH as f32 * scale) as i32;
        let desired_outer_w = designed_cw + nc_w;
        let work = WorkRect {
            left: 0,
            top: 0,
            right: 800,
            bottom: 728,
        };
        assert!(work.width() < desired_outer_w);
        let (laid, fitted) = layout_to_work(scale, work, None, nc_w, nc_h);
        assert!(fitted.outer_w <= work.width());
        assert!(fitted.client_w < designed_cw);
        assert_eq!(fitted.client_w, (fitted.outer_w - nc_w).max(1));
        for (i, (rect, ctrl)) in laid.controls.iter().enumerate() {
            assert!(
                rect.left >= 0 && rect.right <= fitted.client_w,
                "{ctrl:?} {}-{} is outside client {}",
                rect.left,
                rect.right,
                fitted.client_w
            );
            assert!(
                rect.right > rect.left,
                "{ctrl:?} must have a positive width"
            );
            let y = rect.top + 1;
            assert_eq!(
                hit_test_control(&laid.controls, rect.left + 1, y, 0),
                i as i32,
                "{ctrl:?} left edge must remain hit-testable"
            );
            assert_eq!(
                hit_test_control(&laid.controls, rect.right - 1, y, 0),
                i as i32,
                "{ctrl:?} right edge must remain hit-testable"
            );
        }
    }

    #[test]
    fn designed_width_does_not_shrink_chip_rows() {
        let laid = build_layout(1.0, LOGICAL_WIDTH);
        let custom = laid
            .controls
            .iter()
            .find(|(_, c)| matches!(c, Ctrl::CustomSize))
            .expect("screenshot-size row");
        assert_eq!(custom.0.left, 24 + 3 * 110);
        assert_eq!(custom.0.right, 24 + 3 * 110 + 102);
        let clear = laid
            .controls
            .iter()
            .find(|(_, c)| matches!(c, Ctrl::ClearHistoryTitles))
            .expect("footer pair");
        assert_eq!(clear.0.left, 24 + 198);
        assert_eq!(clear.0.right, 24 + 198 + 190);
    }

    #[test]
    fn equal_row_shrinks_only_when_the_designed_span_overflows() {
        assert_eq!(fit_equal_row(24, 476, 4, 102, 110), (102, 110));
        let (w, stride) = fit_equal_row(48, 720, 4, 204, 220);
        assert!(w < 204 && stride < 220);
        assert!(48 + 3 * stride + w <= 720);
        let (w1, stride1) = fit_equal_row(48, 720, 1, 380, 0);
        assert_eq!((w1, stride1), (380, 0));
    }
}
