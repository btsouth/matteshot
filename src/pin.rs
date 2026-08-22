//! Pin-to-screen: float a capture topmost as a reference while you work.
//! Drag anywhere to move, scroll to zoom, Esc or double-click to dismiss.
//! Multiple pins can coexist; they share the main thread's message loop.

use anyhow::Result;
use image::RgbaImage;
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, GetMonitorInfoW, HALFTONE,
    SetStretchBltMode, StretchDIBits, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
    HMONITOR, MONITORINFO, PAINTSTRUCT, SRCCOPY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetCursorPos, GetWindowLongPtrW, LoadCursorW,
    MessageBoxW, RegisterClassW, SetWindowLongPtrW, SetWindowPos, CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW,
    GWLP_USERDATA, HTCAPTION, HWND_TOPMOST, IDC_SIZEALL, MB_ICONWARNING, MB_OK, SWP_NOMOVE, SWP_NOZORDER,
    WM_ERASEBKGND, WM_KEYDOWN, WM_MOUSEWHEEL, WM_NCCREATE, WM_NCDESTROY, WM_NCHITTEST,
    WM_NCLBUTTONDBLCLK, WM_PAINT, WNDCLASSW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    WS_VISIBLE,
};

struct PinState {
    bgra: Vec<u8>,
    w: i32,
    h: i32,
    zoom: f32,
    border: COLORREF,
}

thread_local! {
    /// Live pin windows, for "Close all pins".
    static PINS: std::cell::RefCell<Vec<isize>> = const { std::cell::RefCell::new(Vec::new()) };
}

unsafe fn context_menu(hwnd: HWND) {
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, DestroyMenu, SetForegroundWindow, TrackPopupMenu,
        MF_STRING, TPM_NONOTIFY, TPM_RETURNCMD,
    };
    crate::theme::enable_dark_menus();
    let Ok(menu) = CreatePopupMenu() else { return };
    let _ = AppendMenuW(menu, MF_STRING, 1, w!("Copy image"));
    let _ = AppendMenuW(menu, MF_STRING, 2, w!("Close"));
    let _ = AppendMenuW(menu, MF_STRING, 3, w!("Close all pins"));
    let mut pt = windows::Win32::Foundation::POINT::default();
    let _ = GetCursorPos(&mut pt);
    let _ = SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_NONOTIFY, pt.x, pt.y, 0, hwnd, None);
    let _ = DestroyMenu(menu);
    match cmd.0 {
        1 => {
            if let Some(state) = state_of(hwnd) {
                let mut rgba = Vec::with_capacity(state.bgra.len());
                for px in state.bgra.as_chunks::<4>().0.iter() {
                    rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
                }
                if let Some(img) =
                    RgbaImage::from_raw(state.w as u32, state.h as u32, rgba)
                {
                    let img = crate::output::resize_to_max_edge(
                        &img,
                        crate::config::Config::load().output_max_edge,
                    );
                    if let Err(error) = crate::output::to_clipboard(&img, None) {
                        crate::diagnostics::log("pinned image clipboard copy failed");
                        let message = HSTRING::from(format!(
                            "The pinned image could not be copied. The pin is still open so you can try again.\n\n{error:#}"
                        ));
                        let _ = MessageBoxW(
                            hwnd,
                            PCWSTR(message.as_ptr()),
                            w!("Matteshot"),
                            MB_OK | MB_ICONWARNING,
                        );
                    }
                }
            }
        }
        2 => {
            let _ = DestroyWindow(hwnd);
        }
        3 => {
            let all: Vec<isize> = PINS.with(|p| p.borrow().clone());
            for h in all {
                let _ = DestroyWindow(HWND(h as *mut _));
            }
        }
        _ => {}
    }
}

unsafe fn state_of(hwnd: HWND) -> Option<&'static mut PinState> {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PinState;
    ptr.as_mut()
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            let cs = &*(lparam.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_ERASEBKGND => LRESULT(1),
        // Borderless but resizable: kill the default frame entirely...
        windows::Win32::UI::WindowsAndMessaging::WM_NCCALCSIZE => {
            if wparam.0 != 0 {
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        // ...and hand-roll the hit-test: edges resize, interior drags.
        WM_NCHITTEST => {
            use windows::Win32::UI::WindowsAndMessaging::{
                GetWindowRect, HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT, HTLEFT, HTRIGHT, HTTOP,
                HTTOPLEFT, HTTOPRIGHT,
            };
            let (px, py) = (
                (lparam.0 & 0xFFFF) as i16 as i32,
                ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
            );
            let mut r = RECT::default();
            let _ = GetWindowRect(hwnd, &mut r);
            const M: i32 = 10;
            let (l, t) = (px - r.left < M, py - r.top < M);
            let (rt, b) = (r.right - px < M, r.bottom - py < M);
            let code = match (l, rt, t, b) {
                (true, _, true, _) => HTTOPLEFT,
                (_, true, true, _) => HTTOPRIGHT,
                (true, _, _, true) => HTBOTTOMLEFT,
                (_, true, _, true) => HTBOTTOMRIGHT,
                (true, ..) => HTLEFT,
                (_, true, ..) => HTRIGHT,
                (_, _, true, _) => HTTOP,
                (_, _, _, true) => HTBOTTOM,
                _ => HTCAPTION,
            };
            LRESULT(code as isize)
        }
        // Aspect-locked resize: the image never distorts.
        windows::Win32::UI::WindowsAndMessaging::WM_SIZING => {
            if let Some(state) = state_of(hwnd) {
                let aspect = (state.w as f32 + 2.0) / (state.h as f32 + 2.0);
                let r = &mut *(lparam.0 as *mut RECT);
                let edge = wparam.0 as u32;
                // WMSZ_TOP=3 / WMSZ_BOTTOM=6 drive by height; others by width.
                if edge == 3 || edge == 6 {
                    r.right = r.left + (((r.bottom - r.top) as f32) * aspect) as i32;
                } else {
                    r.bottom = r.top + (((r.right - r.left) as f32) / aspect) as i32;
                }
            }
            LRESULT(1)
        }
        windows::Win32::UI::WindowsAndMessaging::WM_SIZE => {
            if let Some(state) = state_of(hwnd) {
                let cw = (lparam.0 & 0xFFFF) as i32;
                if cw > 2 {
                    state.zoom = (cw - 2) as f32 / state.w as f32;
                    let _ = windows::Win32::Graphics::Gdi::InvalidateRect(hwnd, None, true);
                }
            }
            LRESULT(0)
        }
        windows::Win32::UI::WindowsAndMessaging::WM_GETMINMAXINFO => {
            let mmi = lparam.0 as *mut windows::Win32::UI::WindowsAndMessaging::MINMAXINFO;
            if !mmi.is_null() {
                (*mmi).ptMinTrackSize.x = 60;
                (*mmi).ptMinTrackSize.y = 40;
            }
            LRESULT(0)
        }
        WM_PAINT => {
            if let Some(state) = state_of(hwnd) {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                let (dw, dh) = (
                    (state.w as f32 * state.zoom) as i32,
                    (state.h as f32 * state.zoom) as i32,
                );
                let info = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: state.w,
                        biHeight: -state.h,
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
                    1,
                    1,
                    dw,
                    dh,
                    0,
                    0,
                    state.w,
                    state.h,
                    Some(state.bgra.as_ptr() as *const _),
                    &info,
                    DIB_RGB_COLORS,
                    SRCCOPY,
                );
                // Hairline border so the pin reads as an object, not glitch.
                let brush = CreateSolidBrush(state.border);
                for r in [
                    RECT { left: 0, top: 0, right: dw + 2, bottom: 1 },
                    RECT { left: 0, top: dh + 1, right: dw + 2, bottom: dh + 2 },
                    RECT { left: 0, top: 0, right: 1, bottom: dh + 2 },
                    RECT { left: dw + 1, top: 0, right: dw + 2, bottom: dh + 2 },
                ] {
                    FillRect(hdc, &r, brush);
                }
                let _ = DeleteObject(brush);
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            if let Some(state) = state_of(hwnd) {
                let delta = ((wparam.0 >> 16) & 0xFFFF) as u16 as i16;
                let factor = if delta > 0 { 1.1 } else { 1.0 / 1.1 };
                state.zoom = (state.zoom * factor).clamp(0.15, 4.0);
                let (dw, dh) = (
                    (state.w as f32 * state.zoom) as i32 + 2,
                    (state.h as f32 * state.zoom) as i32 + 2,
                );
                let _ = SetWindowPos(
                    hwnd,
                    HWND_TOPMOST,
                    0,
                    0,
                    dw,
                    dh,
                    SWP_NOMOVE | SWP_NOZORDER,
                );
                let _ = windows::Win32::Graphics::Gdi::InvalidateRect(hwnd, None, true);
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            if wparam.0 as u16 == VK_ESCAPE.0 {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_NCLBUTTONDBLCLK => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        windows::Win32::UI::WindowsAndMessaging::WM_NCRBUTTONUP
        | windows::Win32::UI::WindowsAndMessaging::WM_RBUTTONUP => {
            context_menu(hwnd);
            LRESULT(0)
        }
        WM_NCDESTROY => {
            PINS.with(|p| p.borrow_mut().retain(|h| *h != hwnd.0 as isize));
            let ptr = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) as *mut PinState;
            if !ptr.is_null() {
                drop(Box::from_raw(ptr));
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Float `img` as a topmost reference near the cursor. Returns immediately;
/// the window lives on the main message loop.
pub fn show(img: RgbaImage, monitor: HMONITOR) -> Result<()> {
    let (w, h) = (img.width() as i32, img.height() as i32);
    let mut bgra = Vec::with_capacity((w * h * 4) as usize);
    for p in img.pixels() {
        bgra.extend_from_slice(&[p[2], p[1], p[0], 255]);
    }

    unsafe {
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetMonitorInfoW(monitor, &mut mi);
        let work_w = (mi.rcWork.right - mi.rcWork.left).max(400);
        let work_h = (mi.rcWork.bottom - mi.rcWork.top).max(300);
        // Open at natural size, but never dominating the screen.
        let zoom = (work_w as f32 * 0.55 / w as f32)
            .min(work_h as f32 * 0.55 / h as f32)
            .min(1.0);
        let (dw, dh) = ((w as f32 * zoom) as i32 + 2, (h as f32 * zoom) as i32 + 2);

        let mut pt = windows::Win32::Foundation::POINT::default();
        let _ = GetCursorPos(&mut pt);
        let x = (pt.x - dw / 2).clamp(mi.rcWork.left, (mi.rcWork.right - dw).max(mi.rcWork.left));
        let y = (pt.y - 40).clamp(mi.rcWork.top, (mi.rcWork.bottom - dh).max(mi.rcWork.top));

        let hinstance = GetModuleHandleW(None)?;
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            hCursor: LoadCursorW(None, IDC_SIZEALL)?,
            lpszClassName: w!("matteshot_pin"),
            ..Default::default()
        };
        RegisterClassW(&class);

        let state = Box::into_raw(Box::new(PinState {
            bgra,
            w,
            h,
            zoom,
            border: crate::theme::current().accent,
        }));
        match CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            w!("matteshot_pin"),
            w!("Matteshot pin"),
            WS_POPUP
                | WS_VISIBLE
                | windows::Win32::UI::WindowsAndMessaging::WS_THICKFRAME,
            x,
            y,
            dw,
            dh,
            None,
            None,
            hinstance,
            Some(state as *const _),
        ) {
            Ok(hwnd) => {
                PINS.with(|p| p.borrow_mut().push(hwnd.0 as isize));
                // Focus so wheel-zoom and Esc work immediately.
                let _ = windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow(hwnd);
            }
            Err(_) => drop(Box::from_raw(state)),
        }
    }
    Ok(())
}
