//! Finding the window to capture.

use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetForegroundWindow, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible,
};

pub fn foreground() -> HWND {
    unsafe { GetForegroundWindow() }
}

/// A real foreground window owned by another process. Tray popup menus make
/// Matteshot's hidden window foreground, so callers that mean "the user's
/// active app" must use this instead of reading the foreground afterward.
pub fn external_foreground() -> Option<HWND> {
    let hwnd = foreground();
    if is_capture_candidate(hwnd) {
        Some(hwnd)
    } else {
        None
    }
}

pub fn is_external(hwnd: HWND) -> bool {
    if hwnd.is_invalid() || !unsafe { IsWindow(hwnd).as_bool() } {
        return false;
    }
    let mut pid = 0u32;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    pid != 0 && pid != std::process::id()
}

/// Excludes shell surfaces that become foreground when the user opens a tray
/// menu. Without this, "Capture active window" captures the 48px taskbar.
pub fn is_capture_candidate(hwnd: HWND) -> bool {
    if !is_external(hwnd)
        || !unsafe { IsWindowVisible(hwnd).as_bool() }
        || unsafe { IsIconic(hwnd).as_bool() }
    {
        return false;
    }
    let mut class = [0u16; 64];
    let len = unsafe { GetClassNameW(hwnd, &mut class) } as usize;
    !matches!(
        String::from_utf16_lossy(&class[..len]).as_str(),
        "Shell_TrayWnd" | "Shell_SecondaryTrayWnd" | "NotifyIconOverflowWindow" | "#32768"
    )
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
