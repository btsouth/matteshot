//! Theme palette: follows the Windows "app mode" (light/dark) setting.
//! Every custom-drawn surface reads from here so the app looks intentional
//! in both modes. `MATTESHOT_THEME=light|dark` overrides for testing.

use windows::core::PCSTR;
use windows::Win32::Foundation::{COLORREF, HWND};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWINDOWATTRIBUTE,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use winreg::enums::HKEY_CURRENT_USER;
use winreg::RegKey;

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

const DARK: Theme = Theme {
    light: false,
    bg: COLORREF(0x001C1A18),
    panel: COLORREF(0x00201E1C),
    chip: COLORREF(0x00282522),
    chip_line: COLORREF(0x003B3733),
    text: COLORREF(0x00E4E0DB),
    muted: COLORREF(0x00938C84),
    faint: COLORREF(0x00857D76),
    accent: COLORREF(0x00FAA560),
    accent_text: COLORREF(0x00151311),
    track: COLORREF(0x00343128),
};

const LIGHT: Theme = Theme {
    light: true,
    bg: COLORREF(0x00F1F4F6),
    panel: COLORREF(0x00FBFCFD),
    chip: COLORREF(0x00E4E9EC),
    chip_line: COLORREF(0x00C9D0D6),
    text: COLORREF(0x001E2123),
    muted: COLORREF(0x0061696F),
    faint: COLORREF(0x007C848A),
    accent: COLORREF(0x00EB6325),
    accent_text: COLORREF(0x00FFFFFF),
    track: COLORREF(0x00D1D8DD),
};

/// Opt classic Win32 popup menus (tray, pin context menu) into dark mode.
/// uxtheme ordinal 135 = SetPreferredAppMode(AllowDark) — undocumented but
/// the de-facto standard every dark-mode Win32 app relies on. Best-effort.
pub fn enable_dark_menus() {
    unsafe {
        if let Ok(lib) = LoadLibraryW(windows::core::w!("uxtheme.dll")) {
            if let Some(f) = GetProcAddress(lib, PCSTR(135 as *const u8)) {
                let set_preferred_app_mode: extern "system" fn(i32) -> i32 =
                    std::mem::transmute(f);
                set_preferred_app_mode(1); // AllowDark
            }
        }
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

/// System app-mode at call time. Windows broadcasts theme flips, but our
/// windows are short-lived, so reading at creation is enough.
pub fn current() -> Theme {
    match std::env::var("MATTESHOT_THEME").as_deref() {
        Ok("light") => return LIGHT,
        Ok("dark") => return DARK,
        _ => {}
    }
    let light = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
        .and_then(|k| k.get_value::<u32, _>("AppsUseLightTheme"))
        .map(|v| v != 0)
        .unwrap_or(false);
    if light {
        LIGHT
    } else {
        DARK
    }
}
