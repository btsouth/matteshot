//! Palette numbers and WCAG 2.1 contrast (SBS-762).
//!
//! Kept free of `windows` so the ratios can be proven with `rustc --test`
//! on a Linux box. `theme.rs` wraps these `u32` COLORREF values for GDI.

/// WCAG 2.1 normal-text minimum. Large text may use 3:1; every role here
/// is used at small/body size, so 4.5:1 is the floor.
pub const NORMAL_TEXT_MIN: f64 = 4.5;

/// Light `faint` as shipped before SBS-762.
/// Against light `panel` this is about 3.61:1.
#[cfg(test)]
pub const PRE_SBS762_LIGHT_FAINT: u32 = 0x007C848A;

/// Dark `faint` as shipped before SBS-762. Against dark `chip` this is
/// about 3.70:1 — the ticket named the light pairing; dark failed too.
#[cfg(test)]
pub const PRE_SBS762_DARK_FAINT: u32 = 0x00857D76;

/// Light `muted` as shipped before SBS-762. Against light `chip` this is
/// about 4.48:1, just under the floor. Settings paints inactive chip
/// labels with `muted`.
#[cfg(test)]
pub const PRE_SBS762_LIGHT_MUTED: u32 = 0x0061696F;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    pub light: bool,
    pub bg: u32,
    pub panel: u32,
    pub chip: u32,
    pub chip_line: u32,
    pub text: u32,
    pub muted: u32,
    pub faint: u32,
    pub accent: u32,
    pub accent_text: u32,
    pub track: u32,
}

pub const DARK: Palette = Palette {
    light: false,
    bg: 0x001C1A18,
    panel: 0x00201E1C,
    chip: 0x00282522,
    chip_line: 0x003B3733,
    text: 0x00E4E0DB,
    // Lightened so muted stays above faint and both clear 4.5:1 on chip.
    muted: 0x00A8A198,
    faint: 0x009A948C,
    accent: 0x00FAA560,
    accent_text: 0x00151311,
    track: 0x00343128,
};

pub const LIGHT: Palette = Palette {
    light: true,
    bg: 0x00F1F4F6,
    panel: 0x00FBFCFD,
    chip: 0x00E4E9EC,
    chip_line: 0x00C9D0D6,
    text: 0x001E2123,
    // Darkened so muted stays above faint and both clear 4.5:1 on chip.
    muted: 0x004E565C,
    faint: 0x005C646A,
    accent: 0x00EB6325,
    accent_text: 0x00FFFFFF,
    track: 0x00D1D8DD,
};

/// System colors used when High Contrast is on. Values are COLORREF
/// (`0x00BBGGRR`), matching `GetSysColor`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SystemColors {
    pub window: u32,
    pub window_text: u32,
    pub button_face: u32,
    pub window_frame: u32,
    pub highlight: u32,
    pub highlight_text: u32,
    pub gray_text: u32,
}

/// Typical High Contrast White / Black fixtures for tests. These are not
/// a live `GetSysColor` dump; they pin the constructor, not a particular
/// Windows build.
#[cfg(test)]
pub const HC_WHITE: SystemColors = SystemColors {
    window: 0x00FFFFFF,
    window_text: 0x00000000,
    button_face: 0x00FFFFFF,
    window_frame: 0x00000000,
    highlight: 0x00000000,
    highlight_text: 0x00FFFFFF,
    // ~3.94:1 on white — below 4.5:1, so the constructor must not use it.
    gray_text: 0x00808080,
};

#[cfg(test)]
pub const HC_BLACK: SystemColors = SystemColors {
    window: 0x00000000,
    window_text: 0x00FFFFFF,
    button_face: 0x00000000,
    window_frame: 0x00FFFFFF,
    highlight: 0x00FFFFFF,
    highlight_text: 0x00000000,
    gray_text: 0x00C0C0C0,
};

fn srgb_channel(component: u8) -> f64 {
    let s = f64::from(component) / 255.0;
    if s <= 0.04045 {
        s / 12.92
    } else {
        ((s + 0.055) / 1.055).powf(2.4)
    }
}

/// Relative luminance of a COLORREF (`0x00BBGGRR`).
fn relative_luminance(colorref: u32) -> f64 {
    let r = srgb_channel((colorref & 0xFF) as u8);
    let g = srgb_channel(((colorref >> 8) & 0xFF) as u8);
    let b = srgb_channel(((colorref >> 16) & 0xFF) as u8);
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

fn contrast_ratio(a: u32, b: u32) -> f64 {
    let la = relative_luminance(a);
    let lb = relative_luminance(b);
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

fn gray_is_usable(gray: u32, window: u32, button_face: u32) -> bool {
    contrast_ratio(gray, window) >= NORMAL_TEXT_MIN
        && contrast_ratio(gray, button_face) >= NORMAL_TEXT_MIN
}

/// Map High Contrast system colors into the palette roles.
///
/// `COLOR_GRAYTEXT` is used for `muted`/`faint` only when it already
/// meets 4.5:1 on both window and button face. Otherwise those roles
/// use `COLOR_WINDOWTEXT`. Unreadable SPI is handled by the caller
/// (`None` is not "High Contrast on").
pub fn from_system_colors(c: SystemColors) -> Palette {
    let secondary = if gray_is_usable(c.gray_text, c.window, c.button_face) {
        c.gray_text
    } else {
        c.window_text
    };
    Palette {
        light: relative_luminance(c.window) > 0.5,
        bg: c.window,
        panel: c.window,
        chip: c.button_face,
        chip_line: c.window_frame,
        text: c.window_text,
        muted: secondary,
        faint: secondary,
        accent: c.highlight,
        accent_text: c.highlight_text,
        track: c.button_face,
    }
}

/// Same precedence as `theme::current`, without talking to Windows.
///
/// 1. `MATTESHOT_THEME=light|dark` (test override)
/// 2. High Contrast system colors, when the caller observed them
/// 3. `AppsUseLightTheme`, when the caller could read it
/// 4. Dark, matching the existing registry `unwrap_or(false)`
///
/// A failed High Contrast query is `None` here, not a palette.
pub fn resolve(
    override_theme: Option<&str>,
    high_contrast: Option<SystemColors>,
    apps_use_light: Option<bool>,
) -> Palette {
    match override_theme {
        Some("light") => return LIGHT,
        Some("dark") => return DARK,
        _ => {}
    }
    if let Some(colors) = high_contrast {
        return from_system_colors(colors);
    }
    if apps_use_light == Some(true) {
        LIGHT
    } else {
        DARK
    }
}

#[cfg(test)]
fn fill_surfaces(palette: &Palette) -> [(&'static str, u32); 3] {
    [
        ("bg", palette.bg),
        ("panel", palette.panel),
        ("chip", palette.chip),
    ]
}

#[cfg(test)]
fn text_roles(palette: &Palette) -> [(&'static str, u32); 3] {
    [
        ("text", palette.text),
        ("muted", palette.muted),
        ("faint", palette.faint),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SBS-762: the cited light `faint`/`panel` pair was 3.61:1.
    #[test]
    fn pre_sbs762_light_faint_on_panel_is_below_4_5() {
        let ratio = contrast_ratio(PRE_SBS762_LIGHT_FAINT, LIGHT.panel);
        assert!(
            (ratio - 3.61).abs() < 0.02,
            "historical ratio moved: {ratio}"
        );
        assert!(
            ratio < NORMAL_TEXT_MIN,
            "the bug this ticket names must stay a failing pair: {ratio}"
        );
    }

    /// SBS-762: dark `faint` also missed 4.5:1 on the chip fill.
    #[test]
    fn pre_sbs762_dark_faint_on_chip_is_below_4_5() {
        let ratio = contrast_ratio(PRE_SBS762_DARK_FAINT, DARK.chip);
        assert!(
            ratio < NORMAL_TEXT_MIN,
            "pre-fix dark faint on chip must stay below 4.5:1: {ratio}"
        );
    }

    /// SBS-762: Settings paints inactive chip labels with `muted`.
    #[test]
    fn pre_sbs762_light_muted_on_chip_is_below_4_5() {
        let ratio = contrast_ratio(PRE_SBS762_LIGHT_MUTED, LIGHT.chip);
        assert!(
            ratio < NORMAL_TEXT_MIN,
            "pre-fix light muted on chip must stay below 4.5:1: {ratio}"
        );
    }

    /// SBS-762: every normal-text role on every fill surface is >= 4.5:1.
    #[test]
    fn light_and_dark_text_roles_meet_4_5_on_fill_surfaces() {
        for palette in [LIGHT, DARK] {
            let mode = if palette.light { "light" } else { "dark" };
            for (role, fg) in text_roles(&palette) {
                for (surface, bg) in fill_surfaces(&palette) {
                    let ratio = contrast_ratio(fg, bg);
                    assert!(
                        ratio >= NORMAL_TEXT_MIN,
                        "{mode} {role} on {surface} is {ratio:.2}:1"
                    );
                }
            }
            let accent = contrast_ratio(palette.accent_text, palette.accent);
            assert!(
                accent >= NORMAL_TEXT_MIN,
                "{mode} accent_text on accent is {accent:.2}:1"
            );
        }
    }

    /// SBS-762: hierarchy must not collapse to one passing gray.
    #[test]
    fn text_roles_keep_visual_hierarchy() {
        for palette in [LIGHT, DARK] {
            let mode = if palette.light { "light" } else { "dark" };
            for (surface, bg) in fill_surfaces(&palette) {
                let text = contrast_ratio(palette.text, bg);
                let muted = contrast_ratio(palette.muted, bg);
                let faint = contrast_ratio(palette.faint, bg);
                assert!(
                    text > muted && muted > faint && faint >= NORMAL_TEXT_MIN,
                    "{mode} on {surface}: text {text:.2} muted {muted:.2} faint {faint:.2}"
                );
            }
        }
    }

    /// SBS-762: High Contrast White must not paint with failing GrayText.
    #[test]
    fn high_contrast_white_uses_window_text_when_gray_fails() {
        let palette = from_system_colors(HC_WHITE);
        assert!(palette.light);
        assert_eq!(palette.bg, HC_WHITE.window);
        assert_eq!(palette.panel, HC_WHITE.window);
        assert_eq!(palette.chip, HC_WHITE.button_face);
        assert_eq!(palette.text, HC_WHITE.window_text);
        assert_eq!(palette.faint, HC_WHITE.window_text);
        assert_eq!(palette.muted, HC_WHITE.window_text);
        assert_eq!(palette.accent, HC_WHITE.highlight);
        assert_eq!(palette.accent_text, HC_WHITE.highlight_text);
        assert!(
            contrast_ratio(HC_WHITE.gray_text, HC_WHITE.window) < NORMAL_TEXT_MIN,
            "fixture gray_text must be the failing color this test names"
        );
        for (role, fg) in text_roles(&palette) {
            for (surface, bg) in fill_surfaces(&palette) {
                let ratio = contrast_ratio(fg, bg);
                assert!(
                    ratio >= NORMAL_TEXT_MIN,
                    "HC white {role} on {surface} is {ratio:.2}:1"
                );
            }
        }
    }

    /// SBS-762: High Contrast Black is a usable explicit treatment.
    #[test]
    fn high_contrast_black_meets_4_5_and_may_use_gray_text() {
        let palette = from_system_colors(HC_BLACK);
        assert!(!palette.light);
        assert_eq!(palette.faint, HC_BLACK.gray_text);
        assert_eq!(palette.muted, HC_BLACK.gray_text);
        for (role, fg) in text_roles(&palette) {
            for (surface, bg) in fill_surfaces(&palette) {
                let ratio = contrast_ratio(fg, bg);
                assert!(
                    ratio >= NORMAL_TEXT_MIN,
                    "HC black {role} on {surface} is {ratio:.2}:1"
                );
            }
        }
        let accent = contrast_ratio(palette.accent_text, palette.accent);
        assert!(accent >= NORMAL_TEXT_MIN, "HC black accent {accent:.2}:1");
    }

    /// SBS-762: `MATTESHOT_THEME` still wins so tests can pin light/dark.
    #[test]
    fn env_override_wins_over_high_contrast() {
        let light = resolve(Some("light"), Some(HC_BLACK), Some(false));
        assert_eq!(light, LIGHT);
        let dark = resolve(Some("dark"), Some(HC_WHITE), Some(true));
        assert_eq!(dark, DARK);
    }

    /// SBS-762: a failed High Contrast query is not "High Contrast on".
    #[test]
    fn missing_high_contrast_is_not_a_high_contrast_palette() {
        assert_eq!(resolve(None, None, Some(true)), LIGHT);
        assert_eq!(resolve(None, None, Some(false)), DARK);
        assert_eq!(resolve(None, None, None), DARK);
    }

    /// SBS-762: when SPI says High Contrast is on, system colors win.
    #[test]
    fn high_contrast_wins_over_apps_use_light_theme() {
        let palette = resolve(None, Some(HC_WHITE), Some(false));
        assert_eq!(palette, from_system_colors(HC_WHITE));
        assert_ne!(palette, DARK);
        assert_ne!(palette, LIGHT);
    }

    /// SBS-762: an unknown override value is not a third theme.
    #[test]
    fn unknown_override_falls_through() {
        assert_eq!(resolve(Some("hc"), None, Some(true)), LIGHT);
        assert_eq!(
            resolve(Some("hc"), Some(HC_BLACK), Some(true)),
            from_system_colors(HC_BLACK)
        );
    }
}
