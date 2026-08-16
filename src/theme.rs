//! Theme palette: follows the Windows "app mode" (light/dark) setting,
//! and High Contrast system colors when that mode is on (SBS-762).
//! Every custom-drawn surface reads from here so the app looks intentional
//! in both modes. `MATTESHOT_THEME=light|dark` overrides for testing.

use std::ffi::c_void;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::OnceLock;

use windows::core::PCSTR;
use windows::Win32::Foundation::{COLORREF, HWND};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWINDOWATTRIBUTE,
};
use windows::Win32::Graphics::Gdi::{
    GetSysColor, COLOR_BTNFACE, COLOR_GRAYTEXT, COLOR_HIGHLIGHT, COLOR_HIGHLIGHTTEXT, COLOR_WINDOW,
    COLOR_WINDOWFRAME, COLOR_WINDOWTEXT,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::UI::WindowsAndMessaging::{
    SystemParametersInfoW, SPI_GETHIGHCONTRAST, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
};
use winreg::enums::HKEY_CURRENT_USER;
use winreg::RegKey;

use crate::theme_contrast::{self, Palette, SystemColors};

#[derive(Clone, Copy)]
pub struct Theme {
    pub light: bool,
    /// Window background.
    pub bg: COLORREF,
    /// Floating strip / toolbar background.
    pub panel: COLORREF,
    /// Buttons and chips.
    pub chip: COLORREF,
    pub chip_line: COLORREF,
    pub text: COLORREF,
    pub muted: COLORREF,
    pub faint: COLORREF,
    pub accent: COLORREF,
    /// Text on accent-filled chips.
    pub accent_text: COLORREF,
    /// Slider track.
    pub track: COLORREF,
}

fn theme_from_palette(palette: Palette) -> Theme {
    Theme {
        light: palette.light,
        bg: COLORREF(palette.bg),
        panel: COLORREF(palette.panel),
        chip: COLORREF(palette.chip),
        chip_line: COLORREF(palette.chip_line),
        text: COLORREF(palette.text),
        muted: COLORREF(palette.muted),
        faint: COLORREF(palette.faint),
        accent: COLORREF(palette.accent),
        accent_text: COLORREF(palette.accent_text),
        track: COLORREF(palette.track),
    }
}

/// uxtheme `SetPreferredAppMode` values. `Default` hands the menu colors
/// back to the system, which is what High Contrast needs.
const APP_MODE_DEFAULT: i32 = 0;
const APP_MODE_ALLOW_DARK: i32 = 1;

/// The mode the classic popup menus should be in right now.
///
/// Same precedence as `theme_contrast::resolve`: `MATTESHOT_THEME` wins,
/// then High Contrast owns the colors, then AllowDark lets the menus track
/// the system app mode. Split out so the precedence is testable off Windows.
fn preferred_menu_mode(override_theme: Option<&str>, high_contrast: bool) -> i32 {
    match override_theme {
        Some("light") => return APP_MODE_DEFAULT,
        Some("dark") => return APP_MODE_ALLOW_DARK,
        _ => {}
    }
    if high_contrast {
        APP_MODE_DEFAULT
    } else {
        APP_MODE_ALLOW_DARK
    }
}

/// uxtheme ordinal 135 = `SetPreferredAppMode` — undocumented but the
/// de-facto standard every dark-mode Win32 app relies on. Resolved once:
/// this runs before every popup menu, and a `LoadLibraryW` per menu would
/// leak a module handle each time. `None` means uxtheme did not export it.
fn set_preferred_app_mode_proc() -> Option<extern "system" fn(i32) -> i32> {
    static PROC: OnceLock<Option<usize>> = OnceLock::new();
    let addr = *PROC.get_or_init(|| unsafe {
        let lib = LoadLibraryW(windows::core::w!("uxtheme.dll")).ok()?;
        GetProcAddress(lib, PCSTR(135 as *const u8)).map(|f| f as usize)
    });
    addr.map(|a| unsafe { std::mem::transmute::<usize, extern "system" fn(i32) -> i32>(a) })
}

/// Point classic Win32 popup menus (tray, pin, history) at the current mode.
///
/// Called before each menu is built, not only at startup: High Contrast and
/// the app mode can both be toggled while Matteshot is resident, and a stale
/// AllowDark would keep dark menu colors over a live High Contrast theme.
/// Best-effort; a missing export leaves the menus classic.
pub fn enable_dark_menus() {
    let override_theme = std::env::var("MATTESHOT_THEME").ok();
    let mode = preferred_menu_mode(override_theme.as_deref(), high_contrast_on());
    // Skip the call when nothing moved so repeated menus are free.
    static LAST: AtomicI32 = AtomicI32::new(i32::MIN);
    if LAST.swap(mode, Ordering::Relaxed) == mode {
        return;
    }
    if let Some(set_preferred_app_mode) = set_preferred_app_mode_proc() {
        set_preferred_app_mode(mode);
    }
}

/// Titlebar to match the theme: dark-mode attribute + pinned caption color
/// (Windows accent titlebars would otherwise override).
pub fn apply_titlebar(hwnd: HWND, theme: &Theme) {
    unsafe {
        let dark: i32 = if theme.light { 0 } else { 1 };
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &dark as *const i32 as *const _,
            4,
        );
        let caption: u32 = theme.bg.0;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWINDOWATTRIBUTE(35), // DWMWA_CAPTION_COLOR
            &caption as *const u32 as *const _,
            4,
        );
    }
}

/// `SPI_GETHIGHCONTRAST` failed or High Contrast is off. Failed is not on.
fn high_contrast_on() -> bool {
    read_high_contrast_colors().is_some()
}

/// Layout matches `HIGHCONTRASTW` in winuser.h. Kept local so this file
/// does not depend on which `windows` 0.58 module exports the type.
#[repr(C)]
struct HighContrastW {
    cb_size: u32,
    dw_flags: u32,
    default_scheme: *mut u16,
}

const HCF_HIGHCONTRASTON: u32 = 0x0001;

fn read_high_contrast_colors() -> Option<SystemColors> {
    unsafe {
        let mut hc = HighContrastW {
            cb_size: std::mem::size_of::<HighContrastW>() as u32,
            dw_flags: 0,
            default_scheme: std::ptr::null_mut(),
        };
        SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            hc.cb_size,
            Some((&mut hc) as *mut HighContrastW as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .ok()?;
        if hc.dw_flags & HCF_HIGHCONTRASTON == 0 {
            return None;
        }
        Some(SystemColors {
            window: GetSysColor(COLOR_WINDOW),
            window_text: GetSysColor(COLOR_WINDOWTEXT),
            button_face: GetSysColor(COLOR_BTNFACE),
            window_frame: GetSysColor(COLOR_WINDOWFRAME),
            highlight: GetSysColor(COLOR_HIGHLIGHT),
            highlight_text: GetSysColor(COLOR_HIGHLIGHTTEXT),
            gray_text: GetSysColor(COLOR_GRAYTEXT),
        })
    }
}

fn apps_use_light_theme() -> Option<bool> {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
        .and_then(|k| k.get_value::<u32, _>("AppsUseLightTheme"))
        .ok()
        .map(|v| v != 0)
}

/// System app-mode at call time. Persistent windows already reload on
/// `WM_SETTINGCHANGE`; High Contrast is re-read here so that path picks
/// it up. A failed High Contrast query is not treated as High Contrast on.
pub fn current() -> Theme {
    let override_theme = std::env::var("MATTESHOT_THEME").ok();
    theme_from_palette(theme_contrast::resolve(
        override_theme.as_deref(),
        read_high_contrast_colors(),
        apps_use_light_theme(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SBS-762: the menus follow the same precedence as the palette, and
    /// High Contrast picks `Default` rather than leaving a stale AllowDark
    /// behind when it is switched on after startup.
    #[test]
    fn menu_mode_follows_override_then_high_contrast() {
        assert_eq!(preferred_menu_mode(None, false), APP_MODE_ALLOW_DARK);
        assert_eq!(preferred_menu_mode(None, true), APP_MODE_DEFAULT);
        assert_eq!(preferred_menu_mode(Some("dark"), true), APP_MODE_ALLOW_DARK);
        assert_eq!(preferred_menu_mode(Some("light"), false), APP_MODE_DEFAULT);
        // An unknown override value is not a third mode.
        assert_eq!(preferred_menu_mode(Some("hc"), true), APP_MODE_DEFAULT);
        assert_eq!(preferred_menu_mode(Some("hc"), false), APP_MODE_ALLOW_DARK);
    }
}
