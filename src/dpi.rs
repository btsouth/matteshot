//! Per-monitor DPI, in one place.
//!
//! The process declares `PER_MONITOR_AWARE_V2`, which is a promise to Windows
//! that every window re-lays-out when its DPI changes and that Windows should
//! therefore not compensate on the app's behalf. Honouring that means two
//! things, and both used to be missing: a window must take its scale from the
//! monitor it is actually on rather than from the primary one, and it must act
//! on `WM_DPICHANGED` rather than letting `DefWindowProc` drop it.
//!
//! A laptop at 150% with an external monitor at 100% is an ordinary setup, and
//! it is the one where getting this wrong is most visible.

use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    MonitorFromPoint, MonitorFromWindow, HMONITOR, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER, WINDOW_EX_STYLE, WINDOW_STYLE,
};

/// 96 DPI is 1.0. Everything in the UI is expressed as a multiple of it.
pub const BASE_DPI: f32 = 96.0;

fn scale_of(dpi: u32) -> f32 {
    // A monitor that reports nothing usable must not collapse the whole UI to
    // nothing, so an unreadable DPI reads as 100%.
    if dpi == 0 {
        1.0
    } else {
        dpi as f32 / BASE_DPI
    }
}

pub fn scale_for_monitor(monitor: HMONITOR) -> f32 {
    let (mut dpi_x, mut dpi_y) = (0u32, 0u32);
    unsafe {
        if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y).is_err() {
            return 1.0;
        }
    }
    // Windows reports both axes and they match on every shipping display; the
    // horizontal one is what the rest of the UI is written against.
    scale_of(dpi_x)
}

/// Scale of the monitor a window will open on, for use *before* the window
/// exists — `GetDpiForWindow` needs an HWND, and creation geometry does not
/// have one yet.
pub fn scale_for_point(point: POINT) -> f32 {
    scale_for_monitor(unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) })
}

/// Scale of the monitor a window is currently on.
/// Nearest rather than primary: a window that is off-screen, or not shown
/// yet, is still going to appear somewhere, and the monitor closest to it is a
/// far better guess than whichever one happens to be primary.
pub fn scale_for_window(hwnd: HWND) -> f32 {
    scale_for_monitor(unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) })
}

/// The scale `WM_DPICHANGED` is announcing.
///
/// Read from `wParam` rather than re-queried from the monitor: the message is
/// the authority at the moment it arrives, and a fresh query can disagree
/// mid-move.
pub fn scale_from_message(wparam: WPARAM) -> f32 {
    scale_of((wparam.0 & 0xFFFF) as u32)
}

/// Move and resize to the bounds `WM_DPICHANGED` suggests in `lParam`.
///
/// Windows computes these, having already accounted for the non-client area
/// and for keeping the window on screen. Substituting our own arithmetic here
/// is how windows end up drifting across monitors on every DPI change.
///
/// # Safety
/// `lparam` must be the one delivered with `WM_DPICHANGED`; it points at a
/// `RECT` owned by the message.
pub unsafe fn apply_suggested_bounds(hwnd: HWND, lparam: LPARAM) {
    let suggested = lparam.0 as *const RECT;
    let Some(bounds) = suggested.as_ref() else {
        return;
    };
    let _ = SetWindowPos(
        hwnd,
        None,
        bounds.left,
        bounds.top,
        bounds.right - bounds.left,
        bounds.bottom - bounds.top,
        SWP_NOZORDER | SWP_NOACTIVATE,
    );
}

/// Outer window size for a wanted client size, measured at a given scale.
///
/// `AdjustWindowRectEx` takes its border and caption metrics from the *system*
/// DPI, so under PMv2 it is wrong on any monitor that is not the primary one:
/// the client area comes out short by the difference in non-client thickness,
/// which is the same bug this module exists to remove. The `ForDpi` form is
/// told which DPI to measure at.
///
/// Takes the scale the window is already working in, so a caller cannot supply
/// a DPI that disagrees with the layout it just built.
pub fn outer_bounds(
    client: RECT,
    style: WINDOW_STYLE,
    ex_style: WINDOW_EX_STYLE,
    scale: f32,
) -> RECT {
    let mut bounds = client;
    let dpi = (scale * BASE_DPI).round() as u32;
    unsafe {
        if AdjustWindowRectExForDpi(&mut bounds, style, false, ex_style, dpi).is_err() {
            // Falling back to the un-scaled form is still better than giving
            // up on a size: the window is a little off rather than absent.
            let _ = AdjustWindowRectEx(&mut bounds, style, false, ex_style);
        }
    }
    bounds
}

/// Origin `WM_DPICHANGED` suggests in `lParam`.
///
/// For a window whose size falls out of its own layout pass (then clamped to
/// the work area), the suggested size is only the old one scaled by the DPI
/// ratio. The suggested *position* is still what keeps the window on the
/// monitor it was dragged to. Callers apply the fitted size themselves.
///
/// # Safety
/// `lparam` must be the one delivered with `WM_DPICHANGED`; it points at a
/// `RECT` owned by the message. A null pointer is an empty origin, not (0, 0).
pub unsafe fn suggested_origin(lparam: LPARAM) -> Option<(i32, i32)> {
    let suggested = lparam.0 as *const RECT;
    suggested.as_ref().map(|bounds| (bounds.left, bounds.top))
}

#[cfg(test)]
mod tests {
    use super::{scale_from_message, scale_of, suggested_origin};
    use windows::Win32::Foundation::{LPARAM, RECT, WPARAM};

    #[test]
    fn scale_reads_the_low_word_and_survives_a_useless_dpi() {
        // WM_DPICHANGED packs the same DPI into both halves of wParam.
        for (dpi, expected) in [(96u32, 1.0f32), (120, 1.25), (144, 1.5), (192, 2.0)] {
            let wparam = WPARAM(((dpi << 16) | dpi) as usize);
            assert_eq!(scale_from_message(wparam), expected, "for {dpi} dpi");
        }
        // Nothing usable must read as 100%, never as zero: a zero scale would
        // multiply every control in the window down to nothing.
        assert_eq!(scale_of(0), 1.0);
    }

    #[test]
    fn suggested_origin_reads_the_rect_and_survives_a_null_lparam() {
        let bounds = RECT {
            left: 120,
            top: 80,
            right: 640,
            bottom: 480,
        };
        let lparam = LPARAM((&bounds as *const RECT) as isize);
        assert_eq!(unsafe { suggested_origin(lparam) }, Some((120, 80)));
        assert_eq!(unsafe { suggested_origin(LPARAM(0)) }, None);
    }
}
