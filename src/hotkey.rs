//! Parsing and formatting for the configurable capture shortcut.
//!
//! Stored in config as text ("Ctrl+Alt+S") rather than a modifier bitmask and
//! a virtual-key number, so the file stays readable and a user can set a combo
//! the Settings window does not offer. Anything unparseable falls back to the
//! default instead of leaving the app with no shortcut at all.

use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN,
};

/// What ships, and what an unreadable setting falls back to.
pub const DEFAULT: &str = "Ctrl+Alt+S";

/// The setting that turns the shortcut off entirely. Distinct from a parse
/// failure: chosen absence must not be quietly replaced with the default.
pub const NONE: &str = "None";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hotkey {
    pub modifiers: HOT_KEY_MODIFIERS,
    pub vk: u32,
}

/// Parse "Ctrl+Alt+S" into something `RegisterHotKey` accepts.
///
/// Returns `None` for the off setting and for anything malformed; the caller
/// decides which of those means "fall back" and which means "leave it unbound".
pub fn parse(text: &str) -> Option<Hotkey> {
    let text = text.trim();
    if text.is_empty() || text.eq_ignore_ascii_case(NONE) {
        return None;
    }

    let mut modifiers = HOT_KEY_MODIFIERS(0);
    let mut key = None;
    for part in text.split('+') {
        let part = part.trim();
        if part.is_empty() {
            return None;
        }
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => modifiers |= MOD_CONTROL,
            "alt" => modifiers |= MOD_ALT,
            "shift" => modifiers |= MOD_SHIFT,
            "win" | "windows" => modifiers |= MOD_WIN,
            // The last non-modifier token is the key. A second one means the
            // text is malformed rather than that the later token wins.
            _ if key.is_some() => return None,
            other => key = Some(virtual_key(other)?),
        }
    }

    // Windows registers a bare key happily, which would swallow that key
    // system-wide. Requiring a modifier keeps a stray "S" in config from
    // making the keyboard unusable.
    let vk = key?;
    if modifiers.0 == 0 {
        return None;
    }
    Some(Hotkey { modifiers, vk })
}

fn virtual_key(token: &str) -> Option<u32> {
    let mut chars = token.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_alphanumeric() => Some(c.to_ascii_uppercase() as u32),
        _ => {
            let number = token
                .strip_prefix('f')
                .or_else(|| token.strip_prefix('F'))?;
            let index: u32 = number.parse().ok()?;
            // VK_F1 is 0x70 and the function keys run contiguously to F24.
            (1..=24).contains(&index).then_some(0x70 + index - 1)
        }
    }
}

/// Canonical text for a parsed hotkey, so what is written back to config and
/// shown in Settings matches regardless of how it was typed.
pub fn label(hotkey: Option<Hotkey>) -> String {
    let Some(hotkey) = hotkey else {
        return NONE.to_owned();
    };
    let mut parts = Vec::new();
    if hotkey.modifiers & MOD_CONTROL == MOD_CONTROL {
        parts.push("Ctrl".to_owned());
    }
    if hotkey.modifiers & MOD_ALT == MOD_ALT {
        parts.push("Alt".to_owned());
    }
    if hotkey.modifiers & MOD_SHIFT == MOD_SHIFT {
        parts.push("Shift".to_owned());
    }
    if hotkey.modifiers & MOD_WIN == MOD_WIN {
        parts.push("Win".to_owned());
    }
    parts.push(key_label(hotkey.vk));
    parts.join("+")
}

fn key_label(vk: u32) -> String {
    match vk {
        0x70..=0x87 => format!("F{}", vk - 0x70 + 1),
        other => char::from_u32(other)
            .map(|c| c.to_string())
            .unwrap_or_else(|| format!("0x{other:02X}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_default_parses() {
        let parsed = parse(DEFAULT).expect("default must parse");
        assert_eq!(parsed.modifiers, MOD_CONTROL | MOD_ALT);
        assert_eq!(parsed.vk, 'S' as u32);
    }

    #[test]
    fn parsing_is_case_and_space_insensitive() {
        let canonical = parse("Ctrl+Alt+S").unwrap();
        for variant in ["ctrl+alt+s", "CTRL + ALT + S", " Control+Alt+S "] {
            assert_eq!(parse(variant), Some(canonical), "for {variant:?}");
        }
    }

    #[test]
    fn function_keys_and_digits_are_supported() {
        assert_eq!(parse("Ctrl+F1").unwrap().vk, 0x70);
        assert_eq!(parse("Ctrl+F24").unwrap().vk, 0x87);
        assert_eq!(parse("Alt+4").unwrap().vk, '4' as u32);
        assert_eq!(parse("Ctrl+F25"), None, "only F1 to F24 exist");
    }

    /// A bare key would register system-wide and swallow that key everywhere,
    /// which is a keyboard someone cannot type on.
    #[test]
    fn a_key_without_a_modifier_is_refused() {
        for text in ["S", "F5", "9"] {
            assert_eq!(parse(text), None, "for {text:?}");
        }
    }

    #[test]
    fn malformed_text_is_refused_rather_than_guessed() {
        for text in [
            "",
            "Ctrl+",
            "+S",
            "Ctrl++S",
            "Ctrl+Alt",
            "Ctrl+S+A",
            "Ctrl+Nope",
        ] {
            assert_eq!(parse(text), None, "for {text:?}");
        }
    }

    #[test]
    fn off_is_distinct_from_malformed() {
        assert_eq!(parse(NONE), None);
        assert_eq!(parse("none"), None);
        // Both return None, so callers must use the raw text to tell "the user
        // turned it off" from "this is gibberish, use the default".
        assert_eq!(label(None), NONE);
    }

    #[test]
    fn labels_round_trip_through_parsing() {
        for text in [
            "Ctrl+Alt+S",
            "Ctrl+Shift+S",
            "Win+Alt+F4",
            "Ctrl+Alt+Shift+Win+A",
        ] {
            let parsed = parse(text).expect(text);
            assert_eq!(parse(&label(Some(parsed))), Some(parsed), "for {text:?}");
        }
    }

    #[test]
    fn labels_are_canonical_regardless_of_input_order() {
        let parsed = parse("alt+ctrl+s").unwrap();
        assert_eq!(label(Some(parsed)), "Ctrl+Alt+S");
    }
}
