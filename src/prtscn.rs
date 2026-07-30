//! PrintScreen key acquisition.
//!
//! Win11 routes PrtScn to Snipping Tool by default (registry value
//! `PrintScreenKeyForSnippingEnabled` under HKCU\Control Panel\Keyboard),
//! which makes `RegisterHotKey(VK_SNAPSHOT)` fail — the bug that kills most
//! third-party tools. Strategy: try to register; on failure, if the shell
//! owns the key, ask the user (one MessageBox) and flip the setting the same
//! way the Settings app does, then retry.

use anyhow::{Context, Result};
use windows::core::w;
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, HOT_KEY_MODIFIERS, VK_SNAPSHOT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, SendMessageTimeoutW, HWND_BROADCAST, IDYES, MB_ICONQUESTION, MB_SETFOREGROUND,
    MB_TOPMOST, MB_YESNO, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE,
};
use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE};
use winreg::RegKey;

const MOD_NOREPEAT: HOT_KEY_MODIFIERS = HOT_KEY_MODIFIERS(0x4000);
const KEYBOARD_KEY: &str = r"Control Panel\Keyboard";
const VALUE: &str = "PrintScreenKeyForSnippingEnabled";

pub enum Acquire {
    /// PrtScn is ours.
    Taken,
    /// PrtScn is ours after flipping the Snipping Tool binding (user said yes).
    TakenAfterToggle,
    /// The user declined the takeover.
    Declined,
    /// Setting flipped but the shell hasn't released the key; sign-out needed.
    ShellStillOwns,
    /// Something else (another screenshot tool?) holds PrtScn.
    OtherOwner,
}

/// Missing value defaults to enabled on current Win11 builds.
pub fn snipping_owns_prtscn() -> bool {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(KEYBOARD_KEY, KEY_READ)
        .and_then(|k| k.get_value::<u32, _>(VALUE))
        .map(|v| v != 0)
        .unwrap_or(true)
}

pub fn set_snipping_binding(enabled: bool) -> Result<()> {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(KEYBOARD_KEY, KEY_SET_VALUE)
        .context("open HKCU Control Panel\\Keyboard")?
        .set_value(VALUE, &(enabled as u32))
        .context("write PrintScreenKeyForSnippingEnabled")?;
    broadcast_setting_change();
    Ok(())
}

/// Nudge the shell the same way the Settings app does.
fn broadcast_setting_change() {
    unsafe {
        let _ = SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            WPARAM(0),
            LPARAM(w!("Control Panel\\Keyboard").0 as isize),
            SMTO_ABORTIFHUNG,
            200,
            None,
        );
    }
}

fn try_register(id: i32) -> bool {
    unsafe { RegisterHotKey(None, id, MOD_NOREPEAT, VK_SNAPSHOT.0 as u32).is_ok() }
}

fn prompt_takeover() -> bool {
    unsafe {
        MessageBoxW(
            None,
            w!("Windows currently routes the PrtScn key to Snipping Tool.\n\nLet Matteshot take it over? This flips one Windows setting (undo with matteshot --restore-printscreen). Win+Shift+S will still open Snipping Tool.\n\nIf PrtScn still opens Snipping Tool afterwards (newer Windows builds ignore this setting), turn off \"Use the Print screen key to open screen capture\" in Settings > Bluetooth & devices > Keyboard."),
            w!("Matteshot — take over PrintScreen?"),
            MB_YESNO | MB_ICONQUESTION | MB_SETFOREGROUND | MB_TOPMOST,
        ) == IDYES
    }
}

/// Explicit takeover (tray menu / CLI): flip the binding and grab the key.
pub fn take(id: i32) -> bool {
    let _ = set_snipping_binding(false);
    for _ in 0..20 {
        if try_register(id) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    false
}

/// Explicit release: let go of the key and restore the Snipping binding.
pub fn release(id: i32) {
    unsafe {
        let _ = windows::Win32::UI::Input::KeyboardAndMouse::UnregisterHotKey(None, id);
    }
    let _ = set_snipping_binding(true);
}

/// Try to own PrtScn under the given hotkey id. `interactive` controls
/// whether we may show the takeover prompt.
pub fn acquire(id: i32, interactive: bool) -> Acquire {
    if try_register(id) {
        return Acquire::Taken;
    }
    if !snipping_owns_prtscn() {
        return Acquire::OtherOwner;
    }
    if !interactive || !prompt_takeover() {
        return Acquire::Declined;
    }
    if set_snipping_binding(false).is_err() {
        return Acquire::ShellStillOwns;
    }
    // The shell releases the key when it sees the setting change; give it a
    // moment and retry.
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if try_register(id) {
            return Acquire::TakenAfterToggle;
        }
    }
    Acquire::ShellStillOwns
}
