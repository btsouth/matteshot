//! Freeze-frame capture overlay: a fullscreen frozen snapshot of the monitor,
//! dimmed; hovering highlights whole windows (z-order hit test on DWM frame
//! bounds), dragging selects a region with a live size readout. Click picks
//! a window, drag picks a region, Esc cancels.

use anyhow::{Context, Result};
use image::RgbaImage;
use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateDIBSection, CreateFontW,
    CreatePen, CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect,
    EnumDisplayMonitors, GetMonitorInfoW, InvalidateRect, MonitorFromPoint, RoundRect,
    SelectObject, SetBkColor,
    SetBkMode, SetTextColor, TextOutW, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CLEARTYPE_QUALITY,
    DEFAULT_CHARSET, DIB_RGB_COLORS, DT_CALCRECT, DT_CENTER, DT_SINGLELINE, DT_VCENTER,
    FF_DONTCARE, HBITMAP, HDC, HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST, OPAQUE,
    PAINTSTRUCT, PS_SOLID, SRCCOPY, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, EnumWindows, GetClassNameW,
    GetCursorPos, GetMessageW, GetWindowLongPtrW, GetWindowTextLengthW,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible, LoadCursorW, PostQuitMessage,
    RegisterClassW, SetForegroundWindow,
    SetCursor, SetWindowLongPtrW, TranslateMessage, CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW,
    GWL_EXSTYLE, GWLP_USERDATA, IDC_ARROW, IDC_CROSS, MSG, WM_DESTROY, WM_ERASEBKGND, WM_KEYDOWN,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCCREATE, WM_PAINT, WM_SETCURSOR, WNDCLASSW,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP, WS_VISIBLE,
};

const WHITE: COLORREF = COLORREF(0x00FFFFFF);

const DRAG_THRESHOLD: i32 = 6;

pub enum Selection {
    Window(HWND),
    Region(RgbaImage),
    /// Record instead of capture — carries virtual-screen geometry.
    RecordWindow(HWND),
    RecordRegion(RECT, HMONITOR),
    /// Scroll-capture a window or region. The point is the user's intended
    /// wheel target in virtual-screen coordinates.
    ScrollWindow(HWND, POINT),
    ScrollRegion(RECT, HMONITOR, POINT),
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Window,
    Region,
}

#[derive(Clone, Copy, PartialEq)]
enum Btn {
    Window,
    Region,
    Screen,
    Record,
    Scroll,
    Close,
}

struct Button {
    rect: RECT,
    btn: Btn,
    label: Vec<u16>,
}

struct WinEntry {
    /// Some = capture this window via WGC. None = crop the frozen image at
    /// `rect` (taskbar, desktop) — those are never occluded, and the shell's
    /// layer windows capture wrong through WGC.
    hwnd: Option<HWND>,
    /// Overlay-local rect, clamped to the monitor.
    rect: RECT,
}

struct State {
    frozen: RgbaImage,
    dim_dc: HDC,
    bright_dc: HDC,
    dim_bmp: HBITMAP,
    bright_bmp: HBITMAP,
    width: i32,
    height: i32,
    windows: Vec<WinEntry>,
    hover: Option<usize>,
    pressed: Option<POINT>,
    drag_to: Option<POINT>,
    selection: Option<Option<Selection>>,
    /// Persistent back buffer — repainting on every mouse move must not
    /// reallocate a monitor-sized bitmap.
    back: Option<(HDC, HBITMAP)>,
    font: windows::Win32::Graphics::Gdi::HFONT,
    mode: Mode,
    buttons: Vec<Button>,
    toolbar_rect: RECT,
    toolbar_hover: i32,
    /// Virtual-screen origin, for cursor-to-local conversions.
    origin: POINT,
    /// Local rects of every monitor, for Screen/desktop targets.
    monitors: Vec<RECT>,
    theme: crate::theme::Theme,
    /// Record mode armed: selections start a recording.
    recording: bool,
    /// Scroll mode armed: selections start a scrolling capture.
    scrolling: bool,
}

unsafe fn state_of(hwnd: HWND) -> Option<&'static mut State> {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
    ptr.as_mut()
}

/// Create a 32bpp DIB-backed memory DC holding `img` at brightness `mul`.
unsafe fn make_layer(reference: HDC, img: &RgbaImage, mul: f32) -> (HDC, HBITMAP) {
    let (w, h) = (img.width() as i32, img.height() as i32);
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
    let bmp = CreateDIBSection(reference, &info, DIB_RGB_COLORS, &mut bits, None, 0)
        .expect("CreateDIBSection");
    let dst = std::slice::from_raw_parts_mut(bits as *mut u8, (w * h * 4) as usize);
    // LUT beats per-pixel float math over a multi-megapixel monitor.
    let mut lut = [0u8; 256];
    for (i, v) in lut.iter_mut().enumerate() {
        *v = (i as f32 * mul) as u8;
    }
    let src = img.as_raw();
    for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
        d[0] = lut[s[2] as usize];
        d[1] = lut[s[1] as usize];
        d[2] = lut[s[0] as usize];
        d[3] = 255;
    }
    let dc = CreateCompatibleDC(reference);
    SelectObject(dc, bmp);
    (dc, bmp)
}

fn norm_rect(a: POINT, b: POINT) -> RECT {
    RECT {
        left: a.x.min(b.x),
        top: a.y.min(b.y),
        right: a.x.max(b.x),
        bottom: a.y.max(b.y),
    }
}

/// Inset frame: all four edges stay visible even when the rect spans the
/// whole monitor (a maximized window must read as "selected", not as a
/// stray line above the taskbar).
unsafe fn frame_rect(hdc: HDC, r: &RECT, thickness: i32, color: COLORREF) {
    let brush = CreateSolidBrush(color);
    let t = thickness;
    for rr in [
        RECT { left: r.left, top: r.top, right: r.right, bottom: r.top + t },
        RECT { left: r.left, top: r.bottom - t, right: r.right, bottom: r.bottom },
        RECT { left: r.left, top: r.top, right: r.left + t, bottom: r.bottom },
        RECT { left: r.right - t, top: r.top, right: r.right, bottom: r.bottom },
    ] {
        FillRect(hdc, &rr, brush);
    }
    let _ = DeleteObject(brush);
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// The capture toolbar: Window / Region / Screen / close. Snipping-Tool
/// table stakes, drawn in plain GDI.
unsafe fn draw_toolbar(hdc: HDC, state: &State) {
    let pill = state.toolbar_rect;
    let brush = CreateSolidBrush(state.theme.panel);
    let pen = CreatePen(PS_SOLID, 1, state.theme.chip_line);
    let old_brush = SelectObject(hdc, brush);
    let old_pen = SelectObject(hdc, pen);
    let _ = RoundRect(hdc, pill.left, pill.top, pill.right, pill.bottom, 20, 20);
    SelectObject(hdc, old_brush);
    SelectObject(hdc, old_pen);
    let _ = DeleteObject(brush);
    let _ = DeleteObject(pen);

    SelectObject(hdc, state.font);
    SetBkMode(hdc, TRANSPARENT);
    for (i, b) in state.buttons.iter().enumerate() {
        let selected = matches!(
            (b.btn, state.mode),
            (Btn::Window, Mode::Window) | (Btn::Region, Mode::Region)
        ) || (b.btn == Btn::Record && state.recording)
            || (b.btn == Btn::Scroll && state.scrolling);
        if selected {
            let bg = CreateSolidBrush(state.theme.chip);
            let nopen = CreatePen(PS_SOLID, 1, state.theme.chip);
            let ob = SelectObject(hdc, bg);
            let op = SelectObject(hdc, nopen);
            let _ = RoundRect(hdc, b.rect.left, b.rect.top, b.rect.right, b.rect.bottom, 12, 12);
            SelectObject(hdc, ob);
            SelectObject(hdc, op);
            let _ = DeleteObject(bg);
            let _ = DeleteObject(nopen);
        }
        SetTextColor(
            hdc,
            if selected {
                state.theme.accent
            } else if i as i32 == state.toolbar_hover {
                state.theme.text
            } else {
                state.theme.muted
            },
        );
        let mut label = b.label.clone();
        let mut rc = b.rect;
        DrawTextW(hdc, &mut label, &mut rc, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    }
}

fn in_rect(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

unsafe fn paint(hdc: HDC, state: &State) {
    let _ = BitBlt(hdc, 0, 0, state.width, state.height, state.dim_dc, 0, 0, SRCCOPY);

    let bright = |hdc: HDC, r: &RECT| {
        let (x, y) = (r.left.max(0), r.top.max(0));
        let (x1, y1) = (r.right.min(state.width), r.bottom.min(state.height));
        if x1 > x && y1 > y {
            let _ = BitBlt(hdc, x, y, x1 - x, y1 - y, state.bright_dc, x, y, SRCCOPY);
        }
    };

    if let (Some(start), Some(to)) = (state.pressed, state.drag_to) {
        let r = norm_rect(start, to);
        bright(hdc, &r);
        frame_rect(hdc, &r, 1, WHITE);
        // Size chip near the active corner.
        let label: Vec<u16> = format!(" {} \u{00d7} {} ", r.right - r.left, r.bottom - r.top)
            .encode_utf16()
            .collect();
        SetBkMode(hdc, OPAQUE);
        SetBkColor(hdc, state.theme.panel);
        SetTextColor(hdc, state.theme.text);
        let (tx, ty) = (
            (to.x + 14).min(state.width - 90),
            (to.y + 14).min(state.height - 24),
        );
        let _ = TextOutW(hdc, tx, ty, &label);
    } else if let Some(i) = state.hover {
        let r = state.windows[i].rect;
        bright(hdc, &r);
        frame_rect(hdc, &r, 2, state.theme.accent);
    }

    if state.pressed.is_none() {
        draw_toolbar(hdc, state);
    }
}

/// Toolbar button actions, shared by clicks and keyboard shortcuts.
unsafe fn press_button(hwnd: HWND, state: &mut State, btn: Btn) {
    match btn {
        Btn::Window => {
            state.mode = Mode::Window;
            let _ = InvalidateRect(hwnd, None, false);
        }
        Btn::Region => {
            state.mode = Mode::Region;
            state.hover = None;
            let _ = InvalidateRect(hwnd, None, false);
        }
        Btn::Screen => {
            // The monitor under the cursor.
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let (x, y) = (pt.x - state.origin.x, pt.y - state.origin.y);
            let r = state
                .monitors
                .iter()
                .find(|r| in_rect(r, x, y))
                .copied()
                .unwrap_or(RECT { left: 0, top: 0, right: state.width, bottom: state.height });
            let crop = image::imageops::crop_imm(
                &state.frozen,
                r.left.max(0) as u32,
                r.top.max(0) as u32,
                (r.right - r.left).max(1) as u32,
                (r.bottom - r.top).max(1) as u32,
            )
            .to_image();
            finish(hwnd, state, Some(Selection::Region(crop)));
        }
        Btn::Record => {
            // Arm recording: the next window click or region drag records
            // instead of capturing.
            state.recording = !state.recording;
            state.scrolling = false;
            let _ = InvalidateRect(hwnd, None, false);
        }
        Btn::Scroll => {
            state.scrolling = !state.scrolling;
            state.recording = false;
            let _ = InvalidateRect(hwnd, None, false);
        }
        Btn::Close => finish(hwnd, state, None),
    }
}

unsafe fn finish(hwnd: HWND, state: &mut State, selection: Option<Selection>) {
    state.selection = Some(selection);
    let _ = DestroyWindow(hwnd);
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
                if state.back.is_none() {
                    let mem = CreateCompatibleDC(hdc);
                    let bmp = CreateCompatibleBitmap(hdc, state.width, state.height);
                    SelectObject(mem, bmp);
                    state.back = Some((mem, bmp));
                }
                let (mem, _) = state.back.unwrap();
                paint(mem, state);
                let _ = BitBlt(hdc, 0, 0, state.width, state.height, mem, 0, 0, SRCCOPY);
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if let Some(state) = state_of(hwnd) {
                let pt = POINT {
                    x: (lparam.0 & 0xFFFF) as i16 as i32,
                    y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
                };
                if let Some(start) = state.pressed {
                    if state.drag_to.is_some()
                        || (pt.x - start.x).abs() > DRAG_THRESHOLD
                        || (pt.y - start.y).abs() > DRAG_THRESHOLD
                    {
                        state.drag_to = Some(pt);
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                } else if in_rect(&state.toolbar_rect, pt.x, pt.y) {
                    let th = state
                        .buttons
                        .iter()
                        .position(|b| in_rect(&b.rect, pt.x, pt.y))
                        .map(|i| i as i32)
                        .unwrap_or(-1);
                    if th != state.toolbar_hover || state.hover.is_some() {
                        state.toolbar_hover = th;
                        state.hover = None;
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                } else {
                    let hover = if state.mode == Mode::Window {
                        state.windows.iter().position(|e| {
                            pt.x >= e.rect.left
                                && pt.x < e.rect.right
                                && pt.y >= e.rect.top
                                && pt.y < e.rect.bottom
                        })
                    } else {
                        None
                    };
                    if hover != state.hover || state.toolbar_hover != -1 {
                        state.hover = hover;
                        state.toolbar_hover = -1;
                        let _ = InvalidateRect(hwnd, None, false);
                    }
                }
            }
            LRESULT(0)
        }
        WM_SETCURSOR => {
            if let Some(state) = state_of(hwnd) {
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let (x, y) = (pt.x - state.origin.x, pt.y - state.origin.y);
                let cursor = if in_rect(&state.toolbar_rect, x, y) {
                    LoadCursorW(None, IDC_ARROW)
                } else {
                    LoadCursorW(None, IDC_CROSS)
                };
                if let Ok(c) = cursor {
                    SetCursor(c);
                }
                return LRESULT(1);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_LBUTTONDOWN => {
            if let Some(state) = state_of(hwnd) {
                let pt = POINT {
                    x: (lparam.0 & 0xFFFF) as i16 as i32,
                    y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
                };
                // Toolbar clicks act on button-up, never start a drag.
                if !in_rect(&state.toolbar_rect, pt.x, pt.y) {
                    state.pressed = Some(pt);
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if let Some(state) = state_of(hwnd) {
                let pt = POINT {
                    x: (lparam.0 & 0xFFFF) as i16 as i32,
                    y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
                };
                if state.pressed.is_none() {
                    if let Some(i) = state.buttons.iter().position(|b| in_rect(&b.rect, pt.x, pt.y))
                    {
                        let btn = state.buttons[i].btn;
                        press_button(hwnd, state, btn);
                    }
                    return LRESULT(0);
                }
                match (state.pressed, state.drag_to) {
                    (Some(start), Some(to)) => {
                        let r = norm_rect(start, to);
                        if r.right - r.left >= 8 && r.bottom - r.top >= 8 {
                            if state.recording || state.scrolling {
                                // Local → virtual-screen for the recorder.
                                let v = RECT {
                                    left: r.left + state.origin.x,
                                    top: r.top + state.origin.y,
                                    right: r.right + state.origin.x,
                                    bottom: r.bottom + state.origin.y,
                                };
                                let mon = MonitorFromPoint(
                                    POINT { x: v.left, y: v.top },
                                    MONITOR_DEFAULTTONEAREST,
                                );
                                let sel = if state.scrolling {
                                    Selection::ScrollRegion(
                                        v,
                                        mon,
                                        POINT {
                                            x: start.x + state.origin.x,
                                            y: start.y + state.origin.y,
                                        },
                                    )
                                } else {
                                    Selection::RecordRegion(v, mon)
                                };
                                finish(hwnd, state, Some(sel));
                                return LRESULT(0);
                            }
                            let crop = image::imageops::crop_imm(
                                &state.frozen,
                                r.left.max(0) as u32,
                                r.top.max(0) as u32,
                                (r.right - r.left) as u32,
                                (r.bottom - r.top) as u32,
                            )
                            .to_image();
                            finish(hwnd, state, Some(Selection::Region(crop)));
                        } else {
                            state.pressed = None;
                            state.drag_to = None;
                            let _ = InvalidateRect(hwnd, None, false);
                        }
                    }
                    _ => {
                        if let Some(i) = state.hover {
                            if state.recording || state.scrolling {
                                let r = state.windows[i].rect;
                                let scrolling = state.scrolling;
                                let anchor = POINT {
                                    x: pt.x + state.origin.x,
                                    y: pt.y + state.origin.y,
                                };
                                let sel = match state.windows[i].hwnd {
                                    Some(t) if scrolling => Selection::ScrollWindow(t, anchor),
                                    Some(t) => Selection::RecordWindow(t),
                                    None => {
                                        let v = RECT {
                                            left: r.left + state.origin.x,
                                            top: r.top + state.origin.y,
                                            right: r.right + state.origin.x,
                                            bottom: r.bottom + state.origin.y,
                                        };
                                        let mon = MonitorFromPoint(
                                            POINT { x: v.left, y: v.top },
                                            MONITOR_DEFAULTTONEAREST,
                                        );
                                        if scrolling {
                                            Selection::ScrollRegion(v, mon, anchor)
                                        } else {
                                            Selection::RecordRegion(v, mon)
                                        }
                                    }
                                };
                                finish(hwnd, state, Some(sel));
                                return LRESULT(0);
                            }
                            match state.windows[i].hwnd {
                                Some(target) => {
                                    finish(hwnd, state, Some(Selection::Window(target)))
                                }
                                None => {
                                    // Shell surface or bare desktop: crop the
                                    // frozen image — exactly what was on screen.
                                    let r = state.windows[i].rect;
                                    let crop = image::imageops::crop_imm(
                                        &state.frozen,
                                        r.left.max(0) as u32,
                                        r.top.max(0) as u32,
                                        (r.right - r.left).max(1) as u32,
                                        (r.bottom - r.top).max(1) as u32,
                                    )
                                    .to_image();
                                    finish(hwnd, state, Some(Selection::Region(crop)));
                                }
                            }
                        } else {
                            state.pressed = None;
                        }
                    }
                }
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            if let Some(state) = state_of(hwnd) {
                match wparam.0 as u16 {
                    v if v == VK_ESCAPE.0 => finish(hwnd, state, None),
                    0x57 => press_button(hwnd, state, Btn::Window), // W
                    0x52 => press_button(hwnd, state, Btn::Region), // R
                    0x46 => press_button(hwnd, state, Btn::Screen), // F
                    0x56 => press_button(hwnd, state, Btn::Record), // V
                    0x53 => press_button(hwnd, state, Btn::Scroll), // S
                    _ => {}
                }
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

struct EnumState {
    monitor_rect: RECT,
    our_pid: u32,
    /// (hwnd, rect, is_shell_surface) — shell surfaces get frozen-crop
    /// capture instead of WGC.
    list: Vec<(HWND, RECT, bool)>,
}

unsafe extern "system" fn enum_proc(
    hwnd: HWND,
    lparam: LPARAM,
) -> windows::Win32::Foundation::BOOL {
    let state = &mut *(lparam.0 as *mut EnumState);

    if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
        return true.into();
    }
    // Click-through overlay windows are not real capture targets. Treating
    // their DWM bounds as ordinary windows can select a transparent GPU
    // helper surface instead of the app beneath it (MuMuPlayer's Qt
    // ToolSaveBits window is one example), producing a black recording with
    // only the hardware cursor visible.
    let exstyle = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
    if exstyle & WS_EX_TRANSPARENT.0 != 0 {
        return true.into();
    }
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    let mut cls_buf = [0u16; 64];
    let n = GetClassNameW(hwnd, &mut cls_buf);
    let cls = String::from_utf16_lossy(&cls_buf[..n.max(0) as usize]);
    // Of our own windows, visible UI (strip, tweak, settings) is a valid
    // target as a frozen crop — users reshoot to capture our UI itself.
    let ours = pid == state.our_pid;
    if ours
        && !matches!(cls.as_str(), "matteshot_picker" | "matteshot_tweak" | "matteshot_settings")
    {
        return true.into();
    }
    // The desktop layer spans all monitors and hit-tests as a giant
    // catch-all window; it must never be a target.
    if cls == "Progman" || cls == "WorkerW" {
        return true.into();
    }
    // Taskbars have no title but are legitimate targets. Shell surfaces and
    // our strip capture as frozen crops rather than WGC.
    let is_shell_surface =
        ours || cls == "Shell_TrayWnd" || cls == "Shell_SecondaryTrayWnd";
    if !is_shell_surface && GetWindowTextLengthW(hwnd) == 0 {
        return true.into();
    }
    let mut cloaked = 0u32;
    if DwmGetWindowAttribute(
        hwnd,
        DWMWA_CLOAKED,
        &mut cloaked as *mut u32 as *mut _,
        std::mem::size_of::<u32>() as u32,
    )
    .is_ok()
        && cloaked != 0
    {
        return true.into();
    }
    let mut rect = RECT::default();
    if DwmGetWindowAttribute(
        hwnd,
        DWMWA_EXTENDED_FRAME_BOUNDS,
        &mut rect as *mut RECT as *mut _,
        std::mem::size_of::<RECT>() as u32,
    )
    .is_err()
    {
        return true.into();
    }
    let m = &state.monitor_rect;
    if rect.right <= m.left || rect.left >= m.right || rect.bottom <= m.top || rect.top >= m.bottom
    {
        return true.into();
    }
    state.list.push((hwnd, rect, is_shell_surface));
    true.into()
}

struct MonitorEntry {
    hmon: HMONITOR,
    /// Virtual-screen coordinates.
    rect: RECT,
}

unsafe extern "system" fn mon_enum(
    hmon: HMONITOR,
    _hdc: HDC,
    _rc: *mut RECT,
    lparam: LPARAM,
) -> windows::Win32::Foundation::BOOL {
    let list = &mut *(lparam.0 as *mut Vec<MonitorEntry>);
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if GetMonitorInfoW(hmon, &mut mi).as_bool() {
        list.push(MonitorEntry { hmon, rect: mi.rcMonitor });
    }
    true.into()
}

/// Run the overlay across every monitor. The freeze happens here, so
/// whatever is on screen at call time (including a live picker strip) is
/// snippable as a region. Returns the selection and the monitor to anchor
/// follow-up UI on, or None if cancelled.
pub fn select() -> Result<Option<(Selection, HMONITOR)>> {
    let mut mons: Vec<MonitorEntry> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(mon_enum),
            LPARAM(&mut mons as *mut Vec<MonitorEntry> as isize),
        );
    }
    if mons.is_empty() {
        anyhow::bail!("no monitors found");
    }

    // Virtual-screen bounding box.
    let vleft = mons.iter().map(|m| m.rect.left).min().unwrap();
    let vtop = mons.iter().map(|m| m.rect.top).min().unwrap();
    let vright = mons.iter().map(|m| m.rect.right).max().unwrap();
    let vbottom = mons.iter().map(|m| m.rect.bottom).max().unwrap();
    let mrect = RECT { left: vleft, top: vtop, right: vright, bottom: vbottom };
    let (mw, mh) = (vright - vleft, vbottom - vtop);

    // Freeze every monitor into one combined image; gaps stay black.
    let t0 = std::time::Instant::now();
    let mut frozen = RgbaImage::new(mw as u32, mh as u32);
    for m in &mons {
        let img = crate::capture::capture_monitor(m.hmon).context("freeze monitor")?;
        use image::GenericImage;
        let _ = frozen.copy_from(&img, (m.rect.left - vleft) as u32, (m.rect.top - vtop) as u32);
    }
    let t_freeze = t0.elapsed();

    // Local (overlay-relative) monitor rects, for Screen/desktop targets.
    let local_monitors: Vec<RECT> = mons
        .iter()
        .map(|m| RECT {
            left: m.rect.left - vleft,
            top: m.rect.top - vtop,
            right: m.rect.right - vleft,
            bottom: m.rect.bottom - vtop,
        })
        .collect();

    // Window list in z-order, converted to overlay-local coordinates.
    let mut enum_state = EnumState {
        monitor_rect: mrect,
        our_pid: std::process::id(),
        list: Vec::new(),
    };
    unsafe {
        let _ = EnumWindows(
            Some(enum_proc),
            LPARAM(&mut enum_state as *mut EnumState as isize),
        );
    }
    let mut windows: Vec<WinEntry> = enum_state
        .list
        .into_iter()
        .map(|(hwnd, r, is_shell)| WinEntry {
            hwnd: if is_shell { None } else { Some(hwnd) },
            rect: RECT {
                left: (r.left - mrect.left).max(0),
                top: (r.top - mrect.top).max(0),
                right: (r.right - mrect.left).min(mw),
                bottom: (r.bottom - mrect.top).min(mh),
            },
        })
        .collect();
    // Bottom of z-order: clicking bare desktop captures that monitor.
    for r in &local_monitors {
        windows.push(WinEntry { hwnd: None, rect: *r });
    }

    unsafe {
        let hinstance = GetModuleHandleW(None)?;
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            hCursor: LoadCursorW(None, IDC_CROSS)?,
            lpszClassName: w!("matteshot_overlay"),
            ..Default::default()
        };
        RegisterClassW(&class);

        // Layers are created against the screen DC before the window exists.
        let screen_dc = windows::Win32::Graphics::Gdi::GetDC(None);
        let (dim_dc, dim_bmp) = make_layer(screen_dc, &frozen, 0.42);
        let (bright_dc, bright_bmp) = make_layer(screen_dc, &frozen, 1.0);

        let font = CreateFontW(
            -15,
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
        );

        // Toolbar layout, measured with the real font.
        let old_font = SelectObject(screen_dc, font);
        let specs: [(&str, Btn); 6] = [
            ("Window", Btn::Window),
            ("Region", Btn::Region),
            ("Screen", Btn::Screen),
            ("\u{25CF} Record", Btn::Record),
            ("\u{2193} Scroll", Btn::Scroll),
            ("\u{2715}", Btn::Close),
        ];
        const BTN_PAD: i32 = 16;
        const BTN_GAP: i32 = 6;
        const PILL_PAD: i32 = 9;
        let mut sizes = Vec::new();
        let mut text_h = 0;
        for (label, _) in &specs {
            let mut t = wide(label);
            let mut rc = RECT::default();
            DrawTextW(screen_dc, &mut t, &mut rc, DT_CALCRECT | DT_SINGLELINE);
            sizes.push(rc.right - rc.left + BTN_PAD * 2);
            text_h = text_h.max(rc.bottom - rc.top);
        }
        SelectObject(screen_dc, old_font);
        windows::Win32::Graphics::Gdi::ReleaseDC(None, screen_dc);

        let btn_h = text_h + 12;
        let total_w: i32 = sizes.iter().sum::<i32>()
            + BTN_GAP * (specs.len() as i32 - 1)
            + PILL_PAD * 2;
        // Toolbar lives on the monitor the cursor is on.
        let mut cursor = POINT::default();
        let _ = GetCursorPos(&mut cursor);
        let (cx, cy) = (cursor.x - vleft, cursor.y - vtop);
        let cur_mon = local_monitors
            .iter()
            .find(|r| in_rect(r, cx, cy))
            .copied()
            .unwrap_or(local_monitors[0]);
        let pill_left = cur_mon.left + (cur_mon.right - cur_mon.left - total_w) / 2;
        let pill_top = cur_mon.top + 26;
        let toolbar_rect = RECT {
            left: pill_left,
            top: pill_top,
            right: pill_left + total_w,
            bottom: pill_top + btn_h + PILL_PAD * 2,
        };
        let mut buttons = Vec::new();
        let mut bx = pill_left + PILL_PAD;
        for ((label, btn), w) in specs.iter().zip(&sizes) {
            buttons.push(Button {
                rect: RECT {
                    left: bx,
                    top: pill_top + PILL_PAD,
                    right: bx + w,
                    bottom: pill_top + PILL_PAD + btn_h,
                },
                btn: *btn,
                label: wide(label),
            });
            bx += w + BTN_GAP;
        }

        let mut state = Box::new(State {
            frozen,
            dim_dc,
            bright_dc,
            dim_bmp,
            bright_bmp,
            width: mw,
            height: mh,
            windows,
            hover: None,
            pressed: None,
            drag_to: None,
            selection: None,
            back: None,
            font,
            mode: Mode::Window,
            buttons,
            toolbar_rect,
            toolbar_hover: -1,
            origin: POINT { x: mrect.left, y: mrect.top },
            monitors: local_monitors,
            theme: crate::theme::current(),
            recording: false,
            scrolling: false,
        });
        eprintln!(
            "timing: overlay ready — freeze {t_freeze:?}, total {:?}",
            t0.elapsed()
        );

        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            w!("matteshot_overlay"),
            w!("matteshot overlay"),
            WS_POPUP | WS_VISIBLE,
            mrect.left,
            mrect.top,
            mw,
            mh,
            None,
            None,
            hinstance,
            Some(&mut *state as *mut State as *const _),
        )?;
        let _ = SetForegroundWindow(hwnd);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        let _ = DeleteDC(state.dim_dc);
        let _ = DeleteDC(state.bright_dc);
        let _ = DeleteObject(state.dim_bmp);
        let _ = DeleteObject(state.bright_bmp);
        if let Some((mem, bmp)) = state.back.take() {
            let _ = DeleteDC(mem);
            let _ = DeleteObject(bmp);
        }
        let _ = DeleteObject(state.font);

        // Anchor follow-up UI (the picker) on the monitor the cursor ended on.
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let anchor = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);

        Ok(state.selection.take().flatten().map(|sel| (sel, anchor)))
    }
}

