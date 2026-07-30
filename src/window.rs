//! Finding the window to capture.

use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetForegroundWindow, GetWindowTextLengthW, GetWindowTextW, IsWindowVisible,
};

pub fn foreground() -> HWND {
    unsafe { GetForegroundWindow() }
}

pub fn title_of(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len == 0 {
            return String::new();
        }
        let mut buf = vec![0u16; len as usize + 1];
        let copied = GetWindowTextW(hwnd, &mut buf);
        String::from_utf16_lossy(&buf[..copied as usize])
    }
}

struct FindState {
    needle: String,
    found: Option<HWND>,
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let state = &mut *(lparam.0 as *mut FindState);
    if IsWindowVisible(hwnd).as_bool() {
        let title = title_of(hwnd);
        if !title.is_empty() && title.to_lowercase().contains(&state.needle) {
            state.found = Some(hwnd);
            return BOOL(0); // stop enumeration
        }
    }
    BOOL(1)
}

/// Case-insensitive substring match against visible window titles.
pub fn find_by_title(substr: &str) -> Option<HWND> {
    let mut state = FindState {
        needle: substr.to_lowercase(),
        found: None,
    };
    unsafe {
        // EnumWindows returns an error when the callback stops it early; ignore.
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut state as *mut FindState as isize));
    }
    state.found
}
