//! Freeze-frame capture overlay: a fullscreen frozen snapshot of the monitor,
//! dimmed; hovering highlights whole windows (z-order hit test on DWM frame
//! bounds), dragging selects a region with a live size readout. Click picks
//! a window, drag picks a region, Esc cancels.

use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};

use anyhow::{Context, Result};
use image::RgbaImage;
use rayon::prelude::*;
use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
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
use windows::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_ESCAPE};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, EnumWindows,
    GetClassNameW, GetCursorPos, GetMessageW, GetWindowLongPtrW, GetWindowTextLengthW,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible, KBDLLHOOKSTRUCT, LoadCursorW, PostMessageW,
    PostQuitMessage, RegisterClassW, SetCursor, SetForegroundWindow, SetWindowLongPtrW,
    SetWindowPos, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, CREATESTRUCTW,
    CS_HREDRAW, CS_VREDRAW, GWL_EXSTYLE, GWLP_USERDATA, HC_ACTION, HWND_TOPMOST, IDC_ARROW,
    IDC_CROSS, MSG, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, WH_KEYBOARD_LL, WM_DESTROY,
    WM_ERASEBKGND, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCCREATE,
    WM_PAINT, WM_SETCURSOR, WM_SYSKEYDOWN, WM_SYSKEYUP, WNDCLASSW, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP, WS_VISIBLE,
};

const WHITE: COLORREF = COLORREF(0x00FFFFFF);

const DRAG_THRESHOLD: i32 = 6;

// The frozen overlay is modal even when Windows rejects a foreground request
// (common immediately after a tray menu closes). Route keyboard input directly
// to it and suppress delivery to the previously focused application.
static OVERLAY_KEY_TARGET: AtomicIsize = AtomicIsize::new(0);
static OVERLAY_KEYS_DOWN: AtomicU32 = AtomicU32::new(0);

fn shortcut_bit(vk: u32) -> Option<u32> {
    match vk as u16 {
        key if key == VK_ESCAPE.0 => Some(0),
        0x57 => Some(1), // W
        0x52 => Some(2), // R
        0x46 => Some(3), // F
        0x56 => Some(4), // V
        0x53 => Some(5), // S
        _ => None,
    }
}

unsafe extern "system" fn overlay_keyboard_hook(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let target = OVERLAY_KEY_TARGET.load(Ordering::SeqCst);
    if code == HC_ACTION as i32 && target != 0 {
        match wparam.0 as u32 {
            WM_KEYDOWN | WM_SYSKEYDOWN => {
                let key = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
                if let Some(bit) = shortcut_bit(key.vkCode) {
                    let mask = 1u32 << bit;
                    if OVERLAY_KEYS_DOWN.fetch_or(mask, Ordering::SeqCst) & mask == 0 {
                        let hwnd = HWND(target as *mut _);
                        let _ = PostMessageW(
                            hwnd,
                            WM_KEYDOWN,
                            WPARAM(key.vkCode as usize),
                            LPARAM(0),
                        );
                    }
                }
                return LRESULT(1);
            }
            WM_KEYUP | WM_SYSKEYUP => {
                let key = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
                if let Some(bit) = shortcut_bit(key.vkCode) {
                    OVERLAY_KEYS_DOWN.fetch_and(!(1u32 << bit), Ordering::SeqCst);
                }
                return LRESULT(1);
            }
            _ => {}
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

pub enum Selection {
    /// Live WGC of `hwnd` when possible; `frozen` is the freeze-frame crop
    /// under the highlight and is used whenever WGC rejects the window.
    Window {
        hwnd: HWND,
        frozen: RgbaImage,
    },
    Region(RgbaImage),
    /// Record instead of capture — carries virtual-screen geometry.
    RecordWindow(HWND),
    RecordRegion(RECT, HMONITOR),
    /// Scroll-capture a window or region. The point is the user's intended
    /// wheel target in virtual-screen coordinates.
    ScrollWindow(HWND, POINT),
    ScrollRegion(RECT, HMONITOR, POINT),
}

fn crop_frozen(frozen: &RgbaImage, r: RECT) -> RgbaImage {
    image::imageops::crop_imm(
        frozen,
        r.left.max(0) as u32,
        r.top.max(0) as u32,
        (r.right - r.left).max(1) as u32,
        (r.bottom - r.top).max(1) as u32,
    )
    .to_image()
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

fn shortcut_button(vk: u16) -> Option<Btn> {
    match vk {
        0x57 => Some(Btn::Window), // W
        0x52 => Some(Btn::Region), // R
        0x46 => Some(Btn::Screen), // F
        0x56 => Some(Btn::Record), // V
        0x53 => Some(Btn::Scroll), // S
        _ => None,
    }
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

/// Allocate a 32bpp DIB-backed memory DC for one overlay layer.
unsafe fn make_layer_storage(reference: HDC, w: i32, h: i32) -> (HDC, HBITMAP, *mut u8) {
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
    let dc = CreateCompatibleDC(reference);
    SelectObject(dc, bmp);
    (dc, bmp, bits.cast())
}

fn write_overlay_layers(src: &[u8], dim: &mut [u8], bright: &mut [u8], dim_mul: f32) {
    debug_assert_eq!(src.len(), dim.len());
    debug_assert_eq!(src.len(), bright.len());
    // LUT beats per-pixel float math over a multi-megapixel desktop. Produce
    // both DIBs while the source pixel is hot instead of traversing it twice.
    let mut lut = [0u8; 256];
    for (i, v) in lut.iter_mut().enumerate() {
        *v = (i as f32 * dim_mul) as u8;
    }
    dim.par_chunks_exact_mut(4)
        .zip(bright.par_chunks_exact_mut(4))
        .zip(src.par_chunks_exact(4))
        .for_each(|((dim, bright), src)| {
            dim.copy_from_slice(&[
                lut[src[2] as usize],
                lut[src[1] as usize],
                lut[src[0] as usize],
                255,
            ]);
            bright.copy_from_slice(&[src[2], src[1], src[0], 255]);
        });
}

/// Create the dim and bright overlay DIBs in a single parallel traversal.
unsafe fn make_layers(reference: HDC, img: &RgbaImage) -> (HDC, HBITMAP, HDC, HBITMAP) {
    let (w, h) = (img.width() as i32, img.height() as i32);
    let len = (w * h * 4) as usize;
    let (dim_dc, dim_bmp, dim_bits) = make_layer_storage(reference, w, h);
    let (bright_dc, bright_bmp, bright_bits) = make_layer_storage(reference, w, h);
    let dim = std::slice::from_raw_parts_mut(dim_bits, len);
    let bright = std::slice::from_raw_parts_mut(bright_bits, len);
    write_overlay_layers(img.as_raw(), dim, bright, 0.42);
    (dim_dc, dim_bmp, bright_dc, bright_bmp)
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
                                    // Always keep the freeze-frame crop. WGC is
                                    // preferred later for a clean window, but
                                    // hosted/layered surfaces (and anything
                                    // CreateForWindow rejects) must not error.
                                    let frozen =
                                        crop_frozen(&state.frozen, state.windows[i].rect);
                                    finish(
                                        hwnd,
                                        state,
                                        Some(Selection::Window {
                                            hwnd: target,
                                            frozen,
                                        }),
                                    )
                                }
                                None => {
                                    // Shell surface or bare desktop: crop the
                                    // frozen image — exactly what was on screen.
                                    let crop =
                                        crop_frozen(&state.frozen, state.windows[i].rect);
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
                let key = wparam.0 as u16;
                if key == VK_ESCAPE.0 {
                    finish(hwnd, state, None);
                } else if let Some(button) = shortcut_button(key) {
                    press_button(hwnd, state, button);
                }
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            let raw = hwnd.0 as isize;
            let _ = OVERLAY_KEY_TARGET.compare_exchange(raw, 0, Ordering::SeqCst, Ordering::SeqCst);
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

fn monitors() -> Result<Vec<MonitorEntry>> {
    let mut monitors = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(mon_enum),
            LPARAM(&mut monitors as *mut Vec<MonitorEntry> as isize),
        );
    }
    if monitors.is_empty() {
        anyhow::bail!("no monitors found");
    }
    Ok(monitors)
}

fn virtual_rect(monitors: &[MonitorEntry]) -> RECT {
    RECT {
        left: monitors.iter().map(|monitor| monitor.rect.left).min().unwrap(),
        top: monitors.iter().map(|monitor| monitor.rect.top).min().unwrap(),
        right: monitors.iter().map(|monitor| monitor.rect.right).max().unwrap(),
        bottom: monitors.iter().map(|monitor| monitor.rect.bottom).max().unwrap(),
    }
}

fn freeze_monitors(monitors: &[MonitorEntry], bounds: RECT, batched: bool) -> Result<RgbaImage> {
    let captures = if batched {
        let handles: Vec<HMONITOR> = monitors.iter().map(|monitor| monitor.hmon).collect();
        crate::capture::capture_monitors(&handles).context("freeze monitors")?
    } else {
        monitors
            .iter()
            .map(|monitor| crate::capture::capture_monitor(monitor.hmon))
            .collect::<Result<Vec<_>>>()
            .context("freeze monitors sequentially")?
    };
    let mut frozen = RgbaImage::new(
        (bounds.right - bounds.left) as u32,
        (bounds.bottom - bounds.top) as u32,
    );
    for (monitor, image) in monitors.iter().zip(captures) {
        use image::GenericImage;
        let _ = frozen.copy_from(
            &image,
            (monitor.rect.left - bounds.left) as u32,
            (monitor.rect.top - bounds.top) as u32,
        );
    }
    Ok(frozen)
}

/// Headless timing rig for the expensive part of overlay startup. It captures
/// the real desktop and builds both GDI layers, but never opens a window,
/// changes focus, touches the clipboard, or injects input.
pub fn benchmark_freeze(batched: bool) -> Result<()> {
    let monitors = monitors()?;
    let bounds = virtual_rect(&monitors);
    crate::capture::warmup();
    let started = std::time::Instant::now();
    let frozen = freeze_monitors(&monitors, bounds, batched)?;
    let freeze_elapsed = started.elapsed();
    unsafe {
        let screen_dc = windows::Win32::Graphics::Gdi::GetDC(None);
        let (dim_dc, dim_bmp, bright_dc, bright_bmp) = make_layers(screen_dc, &frozen);
        windows::Win32::Graphics::Gdi::ReleaseDC(None, screen_dc);
        let layer_elapsed = started.elapsed() - freeze_elapsed;
        let _ = DeleteDC(dim_dc);
        let _ = DeleteDC(bright_dc);
        let _ = DeleteObject(dim_bmp);
        let _ = DeleteObject(bright_bmp);
        eprintln!(
            "overlay bench {}: {} monitor(s), {}x{}, freeze {:?}, layers {:?}, total {:?}",
            if batched { "batched" } else { "sequential" },
            monitors.len(),
            frozen.width(),
            frozen.height(),
            freeze_elapsed,
            layer_elapsed,
            started.elapsed()
        );
    }
    Ok(())
}

/// Run the overlay across every monitor. The freeze happens here, so
/// whatever is on screen at call time (including a live picker strip) is
/// snippable as a region. Returns the selection and the monitor to anchor
/// follow-up UI on, or None if cancelled.
pub fn select() -> Result<Option<(Selection, HMONITOR)>> {
    let mons = monitors()?;

    // Virtual-screen bounding box.
    let mrect = virtual_rect(&mons);
    let (vleft, vtop, vright, vbottom) = (mrect.left, mrect.top, mrect.right, mrect.bottom);
    let (mw, mh) = (vright - vleft, vbottom - vtop);

    // Freeze every monitor into one combined image; gaps stay black.
    let t0 = std::time::Instant::now();
    let frozen = freeze_monitors(&mons, mrect, true)?;
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
        let (dim_dc, dim_bmp, bright_dc, bright_bmp) = make_layers(screen_dc, &frozen);

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
        OVERLAY_KEY_TARGET.store(hwnd.0 as isize, Ordering::SeqCst);
        OVERLAY_KEYS_DOWN.store(0, Ordering::SeqCst);
        let keyboard_hook = SetWindowsHookExW(
            WH_KEYBOARD_LL,
            Some(overlay_keyboard_hook),
            HINSTANCE(hinstance.0),
            0,
        )
        .ok();
        if keyboard_hook.is_none() {
            crate::diagnostics::log("overlay keyboard guard unavailable");
        }
        // SetWindowPos can activate a newly shown topmost window even when a
        // preceding tray menu caused SetForegroundWindow permission to lapse.
        // The keyboard guard above remains the reliable fallback.
        let _ = SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
        );
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(hwnd);
        let visible_elapsed = t0.elapsed();
        eprintln!("timing: overlay visible — freeze {t_freeze:?}, total {visible_elapsed:?}");
        crate::diagnostics::log(&format!(
            "overlay visible freeze_ms={} total_ms={}",
            t_freeze.as_millis(),
            visible_elapsed.as_millis()
        ));

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        OVERLAY_KEY_TARGET.store(0, Ordering::SeqCst);
        OVERLAY_KEYS_DOWN.store(0, Ordering::SeqCst);
        if let Some(hook) = keyboard_hook {
            let _ = UnhookWindowsHookEx(hook);
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

#[cfg(test)]
mod tests {
    use super::{shortcut_bit, shortcut_button, write_overlay_layers, Btn};

    #[test]
    fn frozen_overlay_shortcuts_map_to_the_visible_toolbar() {
        assert!(matches!(shortcut_button(0x57), Some(Btn::Window)));
        assert!(matches!(shortcut_button(0x52), Some(Btn::Region)));
        assert!(matches!(shortcut_button(0x46), Some(Btn::Screen)));
        assert!(matches!(shortcut_button(0x56), Some(Btn::Record)));
        assert!(matches!(shortcut_button(0x53), Some(Btn::Scroll)));
        assert!(shortcut_button(0x41).is_none());
        assert!(shortcut_bit(0x1B).is_some());
        assert!(shortcut_bit(0x56).is_some());
        assert!(shortcut_bit(0x41).is_none());
    }

    #[test]
    fn overlay_layers_convert_rgba_to_dim_and_bright_bgra_together() {
        let source = [100, 150, 200, 17, 255, 10, 0, 99];
        let mut dim = [0; 8];
        let mut bright = [0; 8];

        write_overlay_layers(&source, &mut dim, &mut bright, 0.42);

        assert_eq!(dim, [84, 62, 42, 255, 0, 4, 107, 255]);
        assert_eq!(bright, [200, 150, 100, 255, 0, 10, 255, 255]);
    }
}

