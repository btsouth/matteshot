//! Finding the window to capture.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use windows::core::s;
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, HMONITOR, MONITORINFO};
use windows::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    VK_ESCAPE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetForegroundWindow, GetWindowRect, GetWindowTextLengthW,
    GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible,
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

/// Windows puts every top-level window in a z-band, and a band outranks
/// `WS_EX_TOPMOST` absolutely: nothing an ordinary process creates is ever
/// placed above a window in a higher one. Ordinary windows, the taskbar and
/// Widgets all sit in the desktop band; the shell's flyouts are in the
/// immersive bands well above it.
///
/// The floor is the notification band rather than merely "above the desktop".
/// `ZBID_UIACCESS` (2) and the immersive input host (3) also outrank a topmost
/// window, but they belong to accessibility tools and the touch keyboard —
/// things that must never be sent the Esc meant for a shell flyout.
const ZBID_IMMERSIVE_NOTIFICATION: u32 = 4;

type GetWindowBandFn = unsafe extern "system" fn(HWND, *mut u32) -> BOOL;

/// `GetWindowBand` is an undocumented user32 export, so it is resolved once at
/// runtime. A Windows build without it reports no bands, which degrades to the
/// behaviour that existed before flyouts were handled at all.
fn get_window_band() -> Option<GetWindowBandFn> {
    static RESOLVED: OnceLock<Option<GetWindowBandFn>> = OnceLock::new();
    *RESOLVED.get_or_init(|| unsafe {
        let user32 = GetModuleHandleA(s!("user32.dll")).ok()?;
        let address = GetProcAddress(user32, s!("GetWindowBand"))?;
        Some(std::mem::transmute::<
            unsafe extern "system" fn() -> isize,
            GetWindowBandFn,
        >(address))
    })
}

fn window_band(hwnd: HWND) -> Option<u32> {
    let get_band = get_window_band()?;
    let mut band = 0u32;
    unsafe { get_band(hwnd, &mut band).as_bool() }.then_some(band)
}

/// A shell flyout that currently owns the foreground, and the screen rect it
/// covers.
///
/// On Windows 11 26220 these are the notification center and Quick Settings
/// (band 4), Task View (band 5) and Start/Search (band 6). Every one of them
/// is also invisible to `EnumWindows`, so the capture overlay can neither draw
/// above one nor list it as a target without being told about it here.
pub struct ShellFlyout {
    pub hwnd: HWND,
    pub rect: RECT,
}

/// The shell flyout in front of the user, if there is one. `None` is the
/// overwhelmingly common case and costs one `GetWindowBand` call.
pub fn shell_flyout() -> Option<ShellFlyout> {
    let hwnd = foreground();
    if hwnd.is_invalid() || !unsafe { IsWindowVisible(hwnd).as_bool() } {
        return None;
    }
    if window_band(hwnd)? < ZBID_IMMERSIVE_NOTIFICATION {
        return None;
    }
    let mut rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut rect) }.ok()?;
    (rect.right > rect.left && rect.bottom > rect.top).then_some(ShellFlyout { hwnd, rect })
}

/// Close a live shell flyout and wait for it to let go of the screen.
///
/// Callers freeze first, so the flyout's pixels are already captured and the
/// user loses nothing: they go on to aim at the frozen copy, on a surface that
/// finally receives the crosshair and the clicks. The flyout would have closed
/// on their first click regardless.
///
/// Esc is what all of them respond to. Measured on Windows 11 26220, the
/// foreground drops back to an ordinary window ~190-200ms later, and the
/// flyout stops answering `WindowFromPoint` in the same frame, so the poll
/// normally ends far inside the cap. Returns whether it actually went away;
/// the caller carries on either way, because a flyout that will not close
/// leaves things no worse than not having tried.
pub fn dismiss_shell_flyout(flyout: &ShellFlyout) -> bool {
    const CAP: Duration = Duration::from_millis(600);
    let escape = |flags: KEYBD_EVENT_FLAGS| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VK_ESCAPE,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    // The flyout is noted before the freeze, and freezing every monitor takes
    // long enough for the user to have dismissed it themselves or moved on.
    // Injecting the key regardless would send an Esc to whichever application
    // is in front now, cancelling whatever it happened to be doing.
    if foreground().0 != flyout.hwnd.0 {
        return true;
    }
    unsafe {
        SendInput(
            &[escape(KEYBD_EVENT_FLAGS(0)), escape(KEYEVENTF_KEYUP)],
            std::mem::size_of::<INPUT>() as i32,
        );
    }
    let deadline = Instant::now() + CAP;
    loop {
        // Something else has to be genuinely in front, not merely "not the
        // flyout": the foreground goes briefly null during the handover, and
        // taking that as success would put the overlay up while the panel is
        // still on screen and still above it.
        let front = foreground();
        if !front.is_invalid() && front.0 != flyout.hwnd.0 {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(8));
    }
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
    /// Only accept windows owned by this process id. `None` accepts any owner.
    process: Option<u32>,
    found: Option<HWND>,
}

unsafe extern "system" fn enum_class_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let state = &mut *(lparam.0 as *mut ClassFindState<'_>);
    let mut class = [0u16; 128];
    let len = GetClassNameW(hwnd, &mut class) as usize;
    if String::from_utf16_lossy(&class[..len]) == state.class_name {
        if let Some(process) = state.process {
            let mut owner = 0u32;
            let _ = windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(
                hwnd,
                Some(&mut owner),
            );
            if owner != process {
                return BOOL(1);
            }
        }
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
        process: None,
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

/// `find_by_class` restricted to windows this process owns. EnumWindows sees
/// the whole desktop, and a caller reacting to its *own* surface must not
/// latch onto the resident app's identically classed window.
pub fn find_own_by_class(class_name: &str) -> Option<HWND> {
    let mut state = ClassFindState {
        class_name,
        process: Some(std::process::id()),
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

/// True while the user has any Matteshot window in front of them, or while
/// a late-finalize supervisor is still encoding (SBS-893). The supervisor
/// has no window; quit and auto-update used to treat that as idle and tear
/// the process down mid-write.
pub fn any_surface_open() -> bool {
    crate::record::late_finalize_outstanding()
        || SURFACE_CLASSES
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

    /// Auto-update's idle check is `any_surface_open()`. A late-finalize
    /// supervisor has no Matteshot window, so without the outstanding flag
    /// a pending silent update launched the installer mid-Finalize (SBS-893).
    #[test]
    fn any_surface_open_is_true_while_late_finalize_is_outstanding() {
        let _serial = crate::record::lock_late_finalize_for_test();
        let guard = crate::record::LateFinalizeGuard::acquire();
        assert!(
            any_surface_open(),
            "late-finalize work was invisible to the idle check"
        );
        drop(guard);
        if SURFACE_CLASSES
            .iter()
            .all(|class| find_by_class(class).is_none())
        {
            assert!(
                !any_surface_open(),
                "dropping the guard did not return the idle check to false"
            );
        }
    }
}
