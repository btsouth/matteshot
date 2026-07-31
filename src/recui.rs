//! The recording stop pill: a small always-on-top widget showing elapsed
//! time with a Stop button. Excluded from screen capture so it never
//! appears in the recording it controls.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::Result;
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect,
    GetMonitorInfoW, InvalidateRect, MonitorFromWindow, SelectObject, SetBkMode, SetTextColor,
    CLEARTYPE_QUALITY, DEFAULT_CHARSET, DT_CENTER, DT_SINGLELINE, DT_VCENTER, FF_DONTCARE, HDC,
    HFONT, MONITORINFO, MONITOR_DEFAULTTOPRIMARY, PAINTSTRUCT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    GetWindowLongPtrW, KillTimer, LoadCursorW, PostQuitMessage, RegisterClassW,
    SetForegroundWindow, SetTimer, SetWindowDisplayAffinity, SetWindowLongPtrW,
    TranslateMessage, CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, GWLP_USERDATA, HTCAPTION,
    IDC_ARROW, MSG, WDA_EXCLUDEFROMCAPTURE, WM_DESTROY, WM_ERASEBKGND, WM_HOTKEY,
    WM_LBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_NCCREATE, WM_NCHITTEST, WM_PAINT,
    WM_TIMER, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    WS_VISIBLE,
};

use crate::record::{Progress, Target};

struct UiState {
    progress: Arc<Progress>,
    theme: crate::theme::Theme,
    font: HFONT,
    font_small: HFONT,
    stop_rect: RECT,
    hover: bool,
    width: i32,
    height: i32,
}

unsafe fn state_of(hwnd: HWND) -> Option<&'static mut UiState> {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut UiState;
    ptr.as_mut()
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

unsafe fn paint(hdc: HDC, state: &UiState) {
    let bg = CreateSolidBrush(state.theme.panel);
    FillRect(hdc, &RECT { left: 0, top: 0, right: state.width, bottom: state.height }, bg);
    let _ = DeleteObject(bg);
    SetBkMode(hdc, windows::Win32::Graphics::Gdi::TRANSPARENT);

    // Red dot.
    let dot = CreateSolidBrush(windows::Win32::Foundation::COLORREF(0x004444EF));
    let cy = state.height / 2;
    FillRect(hdc, &RECT { left: 14, top: cy - 5, right: 24, bottom: cy + 5 }, dot);
    let _ = DeleteObject(dot);

    let secs = state.progress.started.elapsed().as_secs();
    let label = format!("{:02}:{:02}", secs / 60, secs % 60);
    SelectObject(hdc, state.font);
    SetTextColor(hdc, state.theme.text);
    let mut t = wide(&label);
    let mut rc = RECT { left: 34, top: 0, right: 104, bottom: state.height };
    DrawTextW(hdc, &mut t, &mut rc, DT_SINGLELINE | DT_VCENTER);

    // Stop button.
    let fill = CreateSolidBrush(if state.hover { state.theme.accent } else { state.theme.chip });
    FillRect(hdc, &state.stop_rect, fill);
    let _ = DeleteObject(fill);
    SelectObject(hdc, state.font_small);
    SetTextColor(
        hdc,
        if state.hover { state.theme.accent_text } else { state.theme.text },
    );
    let mut s = wide("Stop");
    let mut sr = state.stop_rect;
    DrawTextW(hdc, &mut s, &mut sr, DT_CENTER | DT_SINGLELINE | DT_VCENTER);

    SelectObject(hdc, state.font_small);
    SetTextColor(hdc, state.theme.faint);
    let mut h = wide("Ctrl+Shift+R");
    let mut hr = RECT {
        left: state.stop_rect.right + 10,
        top: 0,
        right: state.width - 8,
        bottom: state.height,
    };
    DrawTextW(hdc, &mut h, &mut hr, DT_SINGLELINE | DT_VCENTER);
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            let cs = &*(lparam.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_ERASEBKGND => LRESULT(1),
        // The recorder controls must never steal focus from a game or other
        // hardware-rendered target.
        WM_MOUSEACTIVATE => LRESULT(3), // MA_NOACTIVATE
        // Draggable by its body, but not by the Stop button.
        WM_NCHITTEST => {
            let res = DefWindowProcW(hwnd, msg, wparam, lparam);
            if let Some(state) = state_of(hwnd) {
                let mut pt = windows::Win32::Foundation::POINT {
                    x: (lparam.0 & 0xFFFF) as i16 as i32,
                    y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
                };
                let _ = windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut pt);
                let r = state.stop_rect;
                if pt.x >= r.left && pt.x < r.right && pt.y >= r.top && pt.y < r.bottom {
                    return res;
                }
            }
            LRESULT(HTCAPTION as isize)
        }
        WM_PAINT => {
            if let Some(state) = state_of(hwnd) {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                paint(hdc, state);
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        WM_TIMER => {
            if let Some(state) = state_of(hwnd) {
                if state.progress.stop.load(Ordering::Relaxed) {
                    // The worker died (encoder error) — close the pill.
                    let _ = DestroyWindow(hwnd);
                } else {
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
                let r = state.stop_rect;
                let hot = x >= r.left && x < r.right && y >= r.top && y < r.bottom;
                if hot != state.hover {
                    state.hover = hot;
                    let _ = InvalidateRect(hwnd, None, false);
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_HOTKEY => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            let _ = KillTimer(hwnd, 1);
            let _ = UnregisterHotKey(hwnd, 1);
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
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

/// Show the pill and pump messages until the user stops.
pub fn run(progress: Arc<Progress>, target: Target) -> Result<()> {
    let (cw, ch) = (260, 40);
    let theme = crate::theme::current();
    let mut state = Box::new(UiState {
        progress,
        theme,
        font: unsafe { make_font(-16, 600) },
        font_small: unsafe { make_font(-13, 400) },
        stop_rect: RECT { left: 108, top: 7, right: 166, bottom: ch - 7 },
        hover: false,
        width: cw,
        height: ch,
    });

    unsafe {
        let hinstance = GetModuleHandleW(None)?;
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            lpszClassName: w!("matteshot_recui"),
            ..Default::default()
        };
        RegisterClassW(&class);

        // Bottom-center of the monitor being recorded.
        let anchor = match target {
            Target::Window(h) => {
                MonitorFromWindow(HWND(h as *mut _), MONITOR_DEFAULTTOPRIMARY)
            }
            Target::Region(_, m) => {
                windows::Win32::Graphics::Gdi::HMONITOR(m as *mut _)
            }
        };
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetMonitorInfoW(anchor, &mut mi);
        let x = mi.rcWork.left + (mi.rcWork.right - mi.rcWork.left - cw) / 2;
        let y = mi.rcWork.bottom - ch - 24;

        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("matteshot_recui"),
            w!("Recording"),
            WS_POPUP | WS_VISIBLE,
            x,
            y,
            cw,
            ch,
            None,
            None,
            hinstance,
            Some(&mut *state as *mut UiState as *const _),
        )?;
        // Never let the controls appear in their own recording.
        let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
        // A chord remains available even though the pill deliberately never
        // activates, without stealing a normal application key from games.
        let _ = RegisterHotKey(
            hwnd,
            1,
            MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT,
            0x52, // R
        );
        // The freeze-frame overlay was foreground while the user chose the
        // target. Hand focus back to the selected window before it renders.
        if let Target::Window(h) = target {
            let _ = SetForegroundWindow(HWND(h as *mut _));
        }
        SetTimer(hwnd, 1, 250, None);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = DeleteObject(state.font);
        let _ = DeleteObject(state.font_small);
    }
    Ok(())
}
