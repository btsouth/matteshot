//! PrintScreen key acquisition.
//!
//! Win11 routes PrtScn to Snipping Tool by default (registry value
//! `PrintScreenKeyForSnippingEnabled` under HKCU\Control Panel\Keyboard),
//! which makes `RegisterHotKey(VK_SNAPSHOT)` fail — the bug that kills most
//! third-party tools. Strategy: try to register; on failure, if the shell
//! owns the key, ask the user (one MessageBox) and flip the setting the same
//! way the Settings app does, then retry.

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc, Mutex, OnceLock};
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use windows::core::w;
use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, HOT_KEY_MODIFIERS, VK_SNAPSHOT};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, MessageBoxW, PeekMessageW, PostMessageW, PostThreadMessageW,
    SetWindowsHookExW, UnhookWindowsHookEx, HC_ACTION, HWND_BROADCAST, IDYES, KBDLLHOOKSTRUCT,
    MB_ICONQUESTION, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO, MSG, PM_NOREMOVE, WH_KEYBOARD_LL,
    WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SETTINGCHANGE, WM_SYSKEYDOWN, WM_SYSKEYUP,
};
use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE};
use winreg::RegKey;

const MOD_NOREPEAT: HOT_KEY_MODIFIERS = HOT_KEY_MODIFIERS(0x4000);
const KEYBOARD_KEY: &str = r"Control Panel\Keyboard";
const VALUE: &str = "PrintScreenKeyForSnippingEnabled";

// RegisterHotKey is not enough on newer Windows Insider builds: it can report
// success while the shell consumes PrtScn before WM_HOTKEY is delivered. A
// low-level hook is the primary owner and posts the same WM_HOTKEY message the
// rest of Matteshot already understands. The hook disappears automatically if
// the resident exits, so Windows immediately gets the key back after a crash.
static HOOK: AtomicIsize = AtomicIsize::new(0);
static TARGET_THREAD: AtomicU32 = AtomicU32::new(0);
static TARGET_ID: AtomicU32 = AtomicU32::new(0);
static KEY_DOWN: AtomicBool = AtomicBool::new(false);
static FALLBACK_REGISTERED: AtomicBool = AtomicBool::new(false);
static PREFERRED: AtomicBool = AtomicBool::new(true);
/// Tick of the last PrtScn key-down the hook saw, for the staleness check.
static LAST_DOWN_TICK: AtomicU64 = AtomicU64::new(0);
/// How many presses the staleness check rescued. Surfaced in diagnostics; a
/// non-zero value means key-ups are being missed on this machine.
static RECOVERED_PRESSES: AtomicU32 = AtomicU32::new(0);

/// Auto-repeat arrives as a continuous stream at the system repeat rate, which
/// tops out around 30 per second. Nobody presses a key twice inside this window
/// on purpose, so it separates a held key from a deliberate second press.
const REPEAT_WINDOW_MS: u64 = 300;

/// Whether a key-down should fire a capture.
///
/// Purely a function of time since the previous key-down, with no latch to get
/// stuck. Field data forced this: over one session the hook saw 37 key-downs
/// and only 26 key-ups, so a latch cleared solely by key-up spends much of its
/// life wrongly set. Windows stops calling a low-level hook whose thread did not
/// answer within `LowLevelHooksTimeout`, and the busiest instant on the machine
/// is right after PrtScn, when the freeze overlay is capturing every monitor.
///
/// A key-up, when one does arrive, zeroes the timestamp so the next press always
/// fires regardless of how quickly it follows.
fn should_fire(millis_since_last_down: u64) -> bool {
    millis_since_last_down >= REPEAT_WINDOW_MS
}

// Health counters. Incrementing an atomic is cheap enough for a low-level hook;
// anything touching a file or a lock in there would cause the very timeouts
// these exist to reveal.
static DOWNS_SEEN: AtomicU32 = AtomicU32::new(0);
static UPS_SEEN: AtomicU32 = AtomicU32::new(0);

/// Key-downs and key-ups the hook saw, and presses that fired while the latch
/// was still set from an earlier press.
///
/// Key-ups trailing key-downs is the signature of a machine where Windows is
/// dropping hook calls, which is what made PrtScn feel unreliable. Every one of
/// those recovered presses would have been swallowed by the old latch.
pub fn hook_health() -> (u32, u32, u32) {
    (
        DOWNS_SEEN.load(Ordering::SeqCst),
        UPS_SEEN.load(Ordering::SeqCst),
        RECOVERED_PRESSES.load(Ordering::SeqCst),
    )
}

struct HookRuntime {
    thread_id: u32,
    thread: JoinHandle<()>,
}

fn hook_runtime() -> &'static Mutex<Option<HookRuntime>> {
    static RUNTIME: OnceLock<Mutex<Option<HookRuntime>>> = OnceLock::new();
    RUNTIME.get_or_init(|| Mutex::new(None))
}

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

pub fn set_preferred(enabled: bool) {
    PREFERRED.store(enabled, Ordering::SeqCst);
}

pub fn preferred() -> bool {
    PREFERRED.load(Ordering::SeqCst)
}

pub fn owns_key() -> bool {
    HOOK.load(Ordering::SeqCst) != 0 || FALLBACK_REGISTERED.load(Ordering::SeqCst)
}

unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let key = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        if key.vkCode == VK_SNAPSHOT.0 as u32 {
            match wparam.0 as u32 {
                WM_KEYDOWN | WM_SYSKEYDOWN => {
                    // Nothing here may be slow: a hook that overruns
                    // LowLevelHooksTimeout stops being called at all, which is
                    // what causes the missed key-ups this guards against.
                    DOWNS_SEEN.fetch_add(1, Ordering::SeqCst);
                    let now = GetTickCount64();
                    let previous = LAST_DOWN_TICK.swap(now, Ordering::SeqCst);
                    let stale = KEY_DOWN.swap(true, Ordering::SeqCst);
                    if should_fire(now.saturating_sub(previous)) {
                        let posted = PostThreadMessageW(
                            TARGET_THREAD.load(Ordering::SeqCst),
                            windows::Win32::UI::WindowsAndMessaging::WM_HOTKEY,
                            WPARAM(TARGET_ID.load(Ordering::SeqCst) as usize),
                            LPARAM(0),
                        )
                        .is_ok();
                        if !posted {
                            KEY_DOWN.store(false, Ordering::SeqCst);
                            return CallNextHookEx(None, code, wparam, lparam);
                        }
                        // Counted only once the capture is genuinely on its
                        // way. This number exists to answer "were key-ups
                        // being missed?", so a press that went nowhere must
                        // not inflate it.
                        if stale {
                            RECOVERED_PRESSES.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                    return LRESULT(1);
                }
                WM_KEYUP | WM_SYSKEYUP => {
                    UPS_SEEN.fetch_add(1, Ordering::SeqCst);
                    KEY_DOWN.store(false, Ordering::SeqCst);
                    // The press finished, so whatever comes next is a new one.
                    // Zeroing this makes a press-release-press sequence fire
                    // however fast it is, and costs nothing when the key-up is
                    // the one that goes missing.
                    LAST_DOWN_TICK.store(0, Ordering::SeqCst);
                    return LRESULT(1);
                }
                _ => {}
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

fn install_hook(id: i32) -> bool {
    let Ok(mut runtime) = hook_runtime().lock() else {
        return false;
    };
    if runtime.is_some() {
        return true;
    }
    unsafe {
        // PostThreadMessage requires the target thread to already own a queue.
        let mut message = MSG::default();
        let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE);
        let target_thread = GetCurrentThreadId();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let thread = std::thread::spawn(move || {
            let mut message = MSG::default();
            let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE);
            let thread_id = GetCurrentThreadId();
            TARGET_THREAD.store(target_thread, Ordering::SeqCst);
            TARGET_ID.store(id as u32, Ordering::SeqCst);
            let module = GetModuleHandleW(None)
                .map(|module| HINSTANCE(module.0))
                .unwrap_or_default();
            let Ok(hook) = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), Some(module), 0)
            else {
                let _ = ready_tx.send(None);
                return;
            };
            HOOK.store(hook.0 as isize, Ordering::SeqCst);
            let _ = ready_tx.send(Some(thread_id));
            while GetMessageW(&mut message, None, 0, 0).as_bool() {}
            let _ = UnhookWindowsHookEx(hook);
            HOOK.store(0, Ordering::SeqCst);
            KEY_DOWN.store(false, Ordering::SeqCst);
        });
        match ready_rx.recv_timeout(std::time::Duration::from_secs(2)) {
            Ok(Some(thread_id)) => {
                *runtime = Some(HookRuntime { thread_id, thread });
                true
            }
            _ => {
                let _ = thread.join();
                false
            }
        }
    }
}

fn uninstall_hook() {
    if let Ok(mut runtime) = hook_runtime().lock() {
        if let Some(runtime) = runtime.take() {
            unsafe {
                let _ = PostThreadMessageW(runtime.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
            }
            let _ = runtime.thread.join();
        }
    }
    HOOK.store(0, Ordering::SeqCst);
    KEY_DOWN.store(false, Ordering::SeqCst);
}

/// What HKCU actually holds. Missing is not the same as `1`: Win11 treats
/// absent as enabled, but writing `1` when we never saw a value is how
/// release used to turn Snipping on for people who had it off (SBS-1050).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StoredBinding {
    Absent,
    Dword(u32),
}

/// A registry mutation, or the decision to leave HKCU alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BindingWrite {
    Leave,
    Dword(u32),
    Delete,
}

/// Snapshot the first stored value we replace. A later write (take, then a
/// retry) must still restore that original, not the intermediate `0`.
fn remember_on_change(
    remembered: Option<StoredBinding>,
    current: StoredBinding,
    next: u32,
) -> (Option<StoredBinding>, BindingWrite) {
    let next_stored = StoredBinding::Dword(next);
    if current == next_stored {
        return (remembered, BindingWrite::Leave);
    }
    (remembered.or(Some(current)), BindingWrite::Dword(next))
}

fn restore_write(remembered: Option<StoredBinding>) -> BindingWrite {
    match remembered {
        None => BindingWrite::Leave,
        Some(StoredBinding::Absent) => BindingWrite::Delete,
        Some(StoredBinding::Dword(value)) => BindingWrite::Dword(value),
    }
}

fn prior_binding() -> &'static Mutex<Option<StoredBinding>> {
    static PRIOR: OnceLock<Mutex<Option<StoredBinding>>> = OnceLock::new();
    PRIOR.get_or_init(|| Mutex::new(None))
}

fn peek_prior() -> Option<StoredBinding> {
    prior_binding().lock().ok().and_then(|guard| *guard)
}

fn take_prior() -> Option<StoredBinding> {
    prior_binding()
        .lock()
        .ok()
        .and_then(|mut guard| guard.take())
}

fn set_prior(next: Option<StoredBinding>) {
    if let Ok(mut guard) = prior_binding().lock() {
        *guard = next;
    }
}

fn read_stored_binding() -> StoredBinding {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(KEYBOARD_KEY, KEY_READ)
        .and_then(|k| k.get_value::<u32, _>(VALUE))
        .map(StoredBinding::Dword)
        .unwrap_or(StoredBinding::Absent)
}

fn write_stored_binding(write: BindingWrite) -> Result<()> {
    match write {
        BindingWrite::Leave => Ok(()),
        BindingWrite::Dword(value) => {
            RegKey::predef(HKEY_CURRENT_USER)
                .open_subkey_with_flags(KEYBOARD_KEY, KEY_SET_VALUE)
                .context("open HKCU Control Panel\\Keyboard")?
                .set_value(VALUE, &value)
                .context("write PrintScreenKeyForSnippingEnabled")?;
            broadcast_setting_change();
            Ok(())
        }
        BindingWrite::Delete => {
            if let Ok(key) = RegKey::predef(HKEY_CURRENT_USER)
                .open_subkey_with_flags(KEYBOARD_KEY, KEY_SET_VALUE)
            {
                let _ = key.delete_value(VALUE);
            }
            broadcast_setting_change();
            Ok(())
        }
    }
}

/// Missing value defaults to enabled on current Win11 builds.
pub fn snipping_owns_prtscn() -> bool {
    match read_stored_binding() {
        StoredBinding::Absent => true,
        StoredBinding::Dword(value) => value != 0,
    }
}

pub fn set_snipping_binding(enabled: bool) -> Result<()> {
    let current = read_stored_binding();
    let (next_prior, write) = remember_on_change(peek_prior(), current, enabled as u32);
    write_stored_binding(write)?;
    if write != BindingWrite::Leave {
        set_prior(next_prior);
    }
    Ok(())
}

fn restore_snipping_binding() {
    let _ = write_stored_binding(restore_write(take_prior()));
}

/// Nudge the shell without ever waiting on a slow desktop process.
fn broadcast_setting_change() {
    unsafe {
        let _ = PostMessageW(
            Some(HWND_BROADCAST),
            WM_SETTINGCHANGE,
            WPARAM(0),
            LPARAM(w!("Control Panel\\Keyboard").0 as isize),
        );
    }
}

fn try_register(id: i32) -> bool {
    let registered =
        unsafe { RegisterHotKey(None, id, MOD_NOREPEAT, VK_SNAPSHOT.0 as u32).is_ok() };
    FALLBACK_REGISTERED.store(registered, Ordering::SeqCst);
    registered
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
    if install_hook(id) {
        return true;
    }
    // Compatibility fallback for systems that reject the low-level hook.
    // This is intentionally one-shot; the resident heartbeat retries without
    // freezing Settings for two seconds.
    let _ = set_snipping_binding(false);
    try_register(id)
}

/// Explicit release: let go of the key and put HKCU back only if we flipped it.
pub fn release(id: i32) {
    uninstall_hook();
    unsafe {
        let _ = windows::Win32::UI::Input::KeyboardAndMouse::UnregisterHotKey(None, id);
    }
    FALLBACK_REGISTERED.store(false, Ordering::SeqCst);
    restore_snipping_binding();
}

/// Try to own PrtScn under the given hotkey id. `interactive` controls
/// whether we may show the takeover prompt.
pub fn acquire(id: i32, interactive: bool) -> Acquire {
    if install_hook(id) {
        return Acquire::Taken;
    }
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

#[cfg(test)]
mod tests {
    use super::{
        remember_on_change, restore_write, should_fire, BindingWrite, StoredBinding,
        REPEAT_WINDOW_MS,
    };

    #[test]
    fn holding_the_key_still_only_captures_once() {
        // Windows repeats a held key at up to ~30/sec, so repeats land ~33ms
        // apart and must be swallowed.
        assert!(!should_fire(0));
        assert!(!should_fire(33));
        assert!(!should_fire(120));
        assert!(!should_fire(REPEAT_WINDOW_MS - 1));
    }

    #[test]
    fn pressing_again_after_a_dead_press_is_never_treated_as_repeat() {
        // The bug this replaces: the window was 1500ms, so pressing again
        // after nothing happened was read as auto-repeat and eaten too. Field
        // data showed every miss landing here. Nobody re-presses inside 300ms,
        // so anything past it has to fire.
        assert!(should_fire(REPEAT_WINDOW_MS));
        assert!(should_fire(400));
        assert!(should_fire(1_000));
        assert!(should_fire(60_000));
    }

    #[test]
    fn a_completed_press_lets_the_next_one_through_immediately() {
        // A key-up zeroes the timestamp, so the gap becomes the full tick
        // count and even a fast press-release-press fires both times.
        let ticks_since_boot = 900_000u64;
        assert!(should_fire(ticks_since_boot.saturating_sub(0)));
    }

    /// Pins SBS-1050: the LL hook is the primary owner and usually never
    /// writes HKCU. Release used to force `1`, which turned Snipping on for
    /// anyone who had it off (or left the value absent).
    #[test]
    fn release_leaves_hkcu_alone_when_we_never_wrote_it() {
        assert_eq!(restore_write(None), BindingWrite::Leave);
    }

    /// Pins SBS-1050: a fallback takeover that replaced an explicit `1`
    /// must put that `1` back, not invent a different enabled encoding.
    #[test]
    fn release_restores_the_prior_dword_we_replaced() {
        let (remembered, write) = remember_on_change(None, StoredBinding::Dword(1), 0);
        assert_eq!(write, BindingWrite::Dword(0));
        assert_eq!(remembered, Some(StoredBinding::Dword(1)));
        assert_eq!(restore_write(remembered), BindingWrite::Dword(1));
    }

    /// Pins SBS-1050: missing defaults to enabled, but it is still missing.
    /// Restoring by writing `1` would create a value we never saw.
    #[test]
    fn release_deletes_the_value_when_it_was_absent() {
        let (remembered, write) = remember_on_change(None, StoredBinding::Absent, 0);
        assert_eq!(write, BindingWrite::Dword(0));
        assert_eq!(remembered, Some(StoredBinding::Absent));
        assert_eq!(restore_write(remembered), BindingWrite::Delete);
    }

    /// Pins SBS-1050: writing the dword that is already there is not a
    /// change, so release must not later "restore" by writing anything.
    #[test]
    fn writing_the_same_dword_does_not_count_as_a_change() {
        let (remembered, write) = remember_on_change(None, StoredBinding::Dword(0), 0);
        assert_eq!(write, BindingWrite::Leave);
        assert_eq!(remembered, None);
        assert_eq!(restore_write(remembered), BindingWrite::Leave);
    }

    /// Pins SBS-1050: take can write more than once. Release still restores
    /// the original, not the last intermediate value.
    #[test]
    fn a_later_write_still_restores_the_original_prior() {
        let (first, write) = remember_on_change(None, StoredBinding::Dword(1), 0);
        assert_eq!(write, BindingWrite::Dword(0));
        let (second, write) = remember_on_change(first, StoredBinding::Dword(0), 0);
        assert_eq!(write, BindingWrite::Leave);
        let (third, write) = remember_on_change(second, StoredBinding::Dword(0), 1);
        assert_eq!(write, BindingWrite::Dword(1));
        assert_eq!(third, Some(StoredBinding::Dword(1)));
        assert_eq!(restore_write(third), BindingWrite::Dword(1));
    }

    /// Pins SBS-1050: the only time release writes `1` is when that was the
    /// prior dword. The old path wrote `1` for every other case too.
    #[test]
    fn release_does_not_force_snipping_on() {
        assert_ne!(restore_write(None), BindingWrite::Dword(1));
        assert_ne!(
            restore_write(Some(StoredBinding::Dword(0))),
            BindingWrite::Dword(1)
        );
        assert_ne!(
            restore_write(Some(StoredBinding::Absent)),
            BindingWrite::Dword(1)
        );
        assert_eq!(
            restore_write(Some(StoredBinding::Dword(0))),
            BindingWrite::Dword(0)
        );
    }
}
