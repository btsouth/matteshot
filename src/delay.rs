//! Countdown before a capture, so surfaces that vanish on input can be caught.
//!
//! Menus, tooltips, hover flyouts and open dropdowns all close the moment you
//! press a key or click elsewhere, which makes them unreachable through the
//! normal flow. Worse, Windows suspends `RegisterHotKey` delivery for the
//! duration of a menu's modal loop, so PrtScn does not even arrive while one
//! is open. Arming a timer first is the only way in: start the countdown, open
//! the thing, and the overlay freezes with it still on screen.
//!
//! The pill deliberately does not take focus and does not block the user's
//! app. Anything else would dismiss the very surface being waited for.

use anyhow::Result;
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect,
    GetMonitorInfoW, InvalidateRect, SelectObject, SetBkMode, SetTextColor, CLEARTYPE_QUALITY, DEFAULT_CHARSET,
    DT_CENTER, DT_SINGLELINE, DT_VCENTER, FF_DONTCARE, HFONT, MONITORINFO, PAINTSTRUCT, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_ESCAPE};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetWindowLongPtrW,
    PeekMessageW, RegisterClassW, SetWindowDisplayAffinity, SetWindowLongPtrW,
    CREATESTRUCTW, GWLP_USERDATA, MSG, PM_REMOVE, WDA_EXCLUDEFROMCAPTURE, WM_ERASEBKGND,
    WM_NCCREATE, WM_PAINT, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    WS_VISIBLE,
};

/// Delays the Settings row offers. Three is enough to open a menu, ten covers
/// a submenu you have to walk into.
pub const CHOICES: [u32; 3] = [3, 5, 10];
pub const DEFAULT_SECONDS: u32 = 5;

/// Clamp to something a user could have chosen, so a hand-edited config cannot
/// arm a countdown that never visibly ends.
pub fn sanitize(seconds: u32) -> u32 {
    if CHOICES.contains(&seconds) {
        seconds
    } else {
        DEFAULT_SECONDS
    }
}

struct Pill {
    text: Vec<u16>,
    theme: crate::theme::Theme,
    font: HFONT,
    w: i32,
    h: i32,
}

unsafe extern "system" fn pill_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            let cs = &*(lparam.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Pill;
            if let Some(p) = ptr.as_mut() {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                let bg = CreateSolidBrush(p.theme.panel);
                FillRect(hdc, &RECT { left: 0, top: 0, right: p.w, bottom: p.h }, bg);
                let _ = DeleteObject(bg);
                SetBkMode(hdc, TRANSPARENT);
                SelectObject(hdc, p.font);
                SetTextColor(hdc, p.theme.text);
                let mut t = p.text.clone();
                let mut r = RECT { left: 0, top: 0, right: p.w, bottom: p.h };
                DrawTextW(hdc, &mut t, &mut r, DT_CENTER | DT_SINGLELINE | DT_VCENTER);
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn pump() {
    unsafe {
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            DispatchMessageW(&msg);
        }
    }
}

fn esc_pressed() -> bool {
    unsafe { GetAsyncKeyState(VK_ESCAPE.0 as i32) as u16 & 0x8000 != 0 }
}

fn label(remaining: u32) -> String {
    if remaining == 1 {
        "Capturing in 1 second\u{2026}  Esc to cancel".into()
    } else {
        format!("Capturing in {remaining} seconds\u{2026}  Esc to cancel")
    }
}

/// Count down on the monitor under the cursor, returning false if cancelled.
///
/// Cancellation reads the Escape key globally rather than through a message,
/// because the pill never has focus and the user is meant to be driving
/// another app throughout.
pub fn countdown(seconds: u32) -> Result<bool> {
    let seconds = sanitize(seconds);
    unsafe {
        let theme = crate::theme::current();
        let (w, h) = (320, 44);
        let hinstance = GetModuleHandleW(None)?;
        let mut pill = Box::new(Pill {
            text: label(seconds).encode_utf16().collect(),
            theme,
            font: CreateFontW(
                -15,
                0,
                0,
                0,
                500,
                0,
                0,
                0,
                DEFAULT_CHARSET.0 as u32,
                0,
                0,
                CLEARTYPE_QUALITY.0 as u32,
                FF_DONTCARE.0 as u32,
                w!("Segoe UI"),
            ),
            w,
            h,
        });
        let class = WNDCLASSW {
            lpfnWndProc: Some(pill_proc),
            hInstance: hinstance.into(),
            lpszClassName: w!("matteshot_delaypill"),
            ..Default::default()
        };
        RegisterClassW(&class);

        let mut cursor = windows::Win32::Foundation::POINT::default();
        let _ = windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut cursor);
        let monitor = windows::Win32::Graphics::Gdi::MonitorFromPoint(
            cursor,
            windows::Win32::Graphics::Gdi::MONITOR_DEFAULTTONEAREST,
        );
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetMonitorInfoW(monitor, &mut mi);
        let x = mi.rcWork.left + (mi.rcWork.right - mi.rcWork.left - w) / 2;
        let y = mi.rcWork.top + 40;

        // NOACTIVATE is the load-bearing flag: taking focus would dismiss the
        // menu the countdown exists to wait for.
        let hwnd = match CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("matteshot_delaypill"),
            w!("Matteshot"),
            WS_POPUP | WS_VISIBLE,
            x,
            y,
            w,
            h,
            None,
            None,
            hinstance,
            Some(&mut *pill as *mut Pill as *const _),
        ) {
            Ok(hwnd) => hwnd,
            Err(error) => {
                let _ = DeleteObject(pill.font);
                return Err(error.into());
            }
        };
        // Belt and braces: the pill is destroyed before the overlay freezes,
        // but nothing about that ordering should be load-bearing.
        let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);

        let mut cancelled = false;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds as u64);
        let mut shown = seconds;
        while std::time::Instant::now() < deadline {
            if esc_pressed() {
                cancelled = true;
                break;
            }
            let left = (deadline - std::time::Instant::now()).as_secs_f32().ceil() as u32;
            let left = left.max(1);
            if left != shown {
                shown = left;
                pill.text = label(left).encode_utf16().collect();
                let _ = InvalidateRect(hwnd, None, false);
            }
            pump();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        let _ = DestroyWindow(hwnd);
        let _ = DeleteObject(pill.font);
        // Let the pill actually leave the screen before anything freezes it.
        pump();
        Ok(!cancelled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_offered_delays_survive_sanitizing() {
        for seconds in CHOICES {
            assert_eq!(sanitize(seconds), seconds);
        }
        // A hand-edited config should not arm a countdown nobody can wait out.
        for nonsense in [0, 1, 4, 7, 60, u32::MAX] {
            assert_eq!(sanitize(nonsense), DEFAULT_SECONDS, "for {nonsense}");
        }
    }

    #[test]
    fn the_last_second_reads_singular() {
        assert!(label(1).starts_with("Capturing in 1 second\u{2026}"));
        assert!(label(2).starts_with("Capturing in 2 seconds"));
    }

    #[test]
    fn every_label_says_how_to_cancel() {
        for seconds in CHOICES {
            assert!(label(seconds).contains("Esc"), "for {seconds}");
        }
    }
}
