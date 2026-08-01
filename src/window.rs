//! Finding the window to capture.

use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, HMONITOR, MONITORINFO};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetForegroundWindow, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible,
};

pub fn foreground() -> HWND {
    unsafe { GetForegroundWindow() }
}

fn rect_contains(outer: RECT, inner: RECT) -> bool {
    inner.left >= outer.left
        && inner.top >= outer.top
        && inner.right <= outer.right
        && inner.bottom <= outer.bottom
}

pub fn monitor_contains_rect(monitor: HMONITOR, rect: RECT) -> bool {
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !unsafe { GetMonitorInfoW(monitor, &mut info).as_bool() } {
        return false;
    }
    rect_contains(info.rcMonitor, rect)
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

struct ClassFindState<'a> {
    class_name: &'a str,
    found: Option<HWND>,
}

unsafe extern "system" fn enum_class_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let state = &mut *(lparam.0 as *mut ClassFindState<'_>);
    let mut class = [0u16; 128];
    let len = GetClassNameW(hwnd, &mut class) as usize;
    if String::from_utf16_lossy(&class[..len]) == state.class_name {
        state.found = Some(hwnd);
        return BOOL(0);
    }
    BOOL(1)
}

/// Exact top-level class match, including hidden tool windows. FindWindowW
/// misses several Matteshot tool surfaces on current Windows Insider builds.
pub fn find_by_class(class_name: &str) -> Option<HWND> {
    let mut state = ClassFindState {
        class_name,
        found: None,
    };
    unsafe {
        let _ = EnumWindows(
            Some(enum_class_proc),
            LPARAM(&mut state as *mut ClassFindState<'_> as isize),
        );
    }
    state.found
}

/// Every Matteshot surface that can hold work the user would lose. The
/// shutdown path closes these in order; the updater refuses to restart while
/// any of them is on screen.
pub const SURFACE_CLASSES: [&str; 10] = [
    "matteshot_recui",
    "matteshot_scrollpill",
    "matteshot_overlay",
    "matteshot_picker",
    "matteshot_tweak",
    "matteshot_recdone",
    "matteshot_settings",
    "matteshot_activation",
    "matteshot_welcome",
    "matteshot_pin",
];

/// True while the user has any Matteshot window in front of them.
pub fn any_surface_open() -> bool {
    SURFACE_CLASSES
        .iter()
        .any(|class| find_by_class(class).is_some())
}

pub fn has_class(hwnd: HWND, class_name: &str) -> bool {
    if hwnd.is_invalid() || !unsafe { IsWindow(hwnd).as_bool() } {
        return false;
    }
    let mut class = [0u16; 128];
    let len = unsafe { GetClassNameW(hwnd, &mut class) } as usize;
    String::from_utf16_lossy(&class[..len]) == class_name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_bounds_reject_crossing_regions() {
        let monitor = RECT { left: -1920, top: 0, right: 0, bottom: 1080 };
        assert!(rect_contains(
            monitor,
            RECT { left: -1900, top: 20, right: -20, bottom: 1060 }
        ));
        assert!(!rect_contains(
            monitor,
            RECT { left: -100, top: 20, right: 100, bottom: 500 }
        ));
    }
}
