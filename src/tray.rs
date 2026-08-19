//! System tray presence: runtime-generated icon, left-click captures,
//! right-click menu. The tray window shares the main thread's message loop;
//! menu picks surface as `Action`s the main loop polls after dispatch.

use std::ffi::c_void;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use windows::core::{w, Interface, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::Com::{
    CoCreateInstance, CoTaskMemFree, IPersistFile, CLSCTX_INPROC_SERVER,
};
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, DeleteObject, GetDC, ReleaseDC, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DIB_RGB_COLORS,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    SHGetKnownFolderPath, ShellLink, Shell_NotifyIconW, FOLDERID_Startup, IShellLinkW,
    KF_FLAG_DEFAULT, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyWindow,
    DestroyMenu, GetCursorPos, GetWindowLongPtrW, KillTimer, PostQuitMessage, RegisterClassW,
    SetForegroundWindow, SetTimer, SetWindowLongPtrW, TrackPopupMenu, CREATESTRUCTW, GWLP_USERDATA,
    HICON, ICONINFO, MF_GRAYED, MF_SEPARATOR, MF_STRING, TPM_BOTTOMALIGN,
    TPM_NONOTIFY, TPM_RETURNCMD, WM_CLOSE, WM_LBUTTONUP, WM_NCCREATE, WM_RBUTTONUP, WM_TIMER,
    WNDCLASSW, WS_EX_TOOLWINDOW, WS_POPUP,
};
use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE};
use winreg::RegKey;

const WM_TRAYICON: u32 = 0x8001; // WM_APP + 1
pub(crate) const WM_UPDATE_AVAILABLE: u32 = 0x8002; // WM_APP + 2
/// The verified installer is about to run and take the app with it.
pub(crate) const WM_UPDATE_INSTALLING: u32 = 0x8004; // WM_APP + 4
const WM_TRAY_ACTION: u32 = 0x8003; // defer until the native popup is fully dismissed
const CMD_CAPTURE: usize = 101;
const CMD_CAPTURE_ACTIVE: usize = 102;
const CMD_OPEN_FOLDER: usize = 103;
const CMD_QUIT: usize = 106;
const CMD_SETTINGS: usize = 107;
const CMD_UPDATE: usize = 108;
const CMD_BUY: usize = 109;
const CMD_ACTIVATE: usize = 110;
const CMD_OPEN_VIDEOS: usize = 113;
const CMD_CAPTURE_DELAYED: usize = 114;
const CMD_HISTORY: usize = 115;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "Matteshot";

#[derive(Clone, Copy, PartialEq)]
pub enum Action {
    Capture,
    CaptureActive,
    CaptureDelayed,
    OpenFolder,
    OpenVideos,
    Settings,
    OpenUpdate,
    Buy,
    Activate,
    Deactivate,
    Quit,
    // Appended rather than inserted: WM_TRAY_ACTION encodes these as `as
    // usize` discriminants, and posting/receiving can straddle an
    // auto-update where an older resident and a newer CLI invocation (or
    // vice versa) briefly coexist. Inserting a variant earlier would shift
    // every later discriminant and desync that protocol.
    History,
}

struct TrayState {
    pending: Option<Action>,
    update: Option<crate::update::AvailableUpdate>,
    active_window: Option<HWND>,
}

pub struct Tray {
    pub hwnd: HWND,
    state: Box<TrayState>,
    _icon: HICON,
}

/// Where the autostart shortcut lives. A Startup-folder `.lnk` reaches the same
/// end as an HKCU\...\Run value, but writing a Run key is the single strongest
/// feature in Defender's Behavior:Win32/Persistence family, and a freshly
/// downloaded installer doing it got Matteshot quarantined in the field.
fn autostart_link() -> Result<PathBuf> {
    unsafe {
        let raw = SHGetKnownFolderPath(&FOLDERID_Startup, KF_FLAG_DEFAULT, None)
            .context("locate the Startup folder")?;
        let path = PathBuf::from(raw.to_string().context("Startup folder path is not UTF-16")?);
        CoTaskMemFree(Some(raw.0 as *const c_void));
        Ok(path.join("Matteshot.lnk"))
    }
}

pub fn autostart_enabled() -> bool {
    autostart_link().is_ok_and(|link| link.is_file())
}

pub fn set_autostart(enabled: bool) -> Result<()> {
    let link = autostart_link()?;
    if !enabled {
        // Absent is the off state, so a missing file is success, not an error.
        match std::fs::remove_file(&link) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(error).context("remove the autostart shortcut")
            }
            _ => return Ok(()),
        }
    }

    write_shortcut(&link, &std::env::current_exe()?)
}

/// Write a `.lnk` at `link` pointing at `target`. Split out from
/// `set_autostart` so it can be exercised somewhere other than the real
/// Startup folder. The caller's thread must already be an STA, which the main
/// thread is (see main.rs).
fn write_shortcut(link: &Path, target: &Path) -> Result<()> {
    unsafe {
        let shell_link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
            .context("create the shell link object")?;
        shell_link
            .SetPath(&HSTRING::from(target.as_os_str()))
            .context("set the shortcut target")?;
        if let Some(dir) = target.parent() {
            shell_link
                .SetWorkingDirectory(&HSTRING::from(dir.as_os_str()))
                .context("set the shortcut working directory")?;
        }
        shell_link
            .SetDescription(w!("Matteshot"))
            .context("set the shortcut description")?;
        let file: IPersistFile = shell_link.cast().context("cast the shell link to a file")?;
        file.Save(&HSTRING::from(link.as_os_str()), true)
            .context("write the autostart shortcut")?;
    }
    Ok(())
}

/// Move anyone who installed before 0.13.2 off the Run key. The value is the
/// old source of truth, so its presence means autostart was on: recreate it as
/// a shortcut and delete the value. Idempotent, and a no-op for new installs.
pub fn migrate_autostart_from_run_key() {
    let had_run_value = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(RUN_KEY, KEY_READ)
        .and_then(|k| k.get_value::<String, _>(RUN_VALUE))
        .is_ok();
    if !had_run_value {
        return;
    }
    // Only claim the migration once the shortcut is actually on disk; dropping
    // the Run value after a failed write would silently disable autostart.
    if !autostart_enabled() {
        if let Err(error) = set_autostart(true) {
            crate::diagnostics::log("autostart migration could not write the shortcut");
            eprintln!("autostart migration failed: {error:#}");
            return;
        }
    }
    if let Ok(key) = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(RUN_KEY, KEY_SET_VALUE)
    {
        let _ = key.delete_value(RUN_VALUE);
    }
    crate::diagnostics::log("autostart migrated from the Run key to a Startup shortcut");
}

/// The embedded app icon (assets\matteshot.ico via build.rs), falling back to
/// the runtime-drawn one if the resource is somehow missing. Also used as the
/// window icon for taskbar-visible windows.
#[allow(clippy::manual_dangling_ptr)] // Win32 MAKEINTRESOURCE: resource ID encoded as a pointer.
pub(crate) unsafe fn app_icon() -> HICON {
    use windows::Win32::UI::WindowsAndMessaging::{LoadImageW, IMAGE_ICON, LR_DEFAULTSIZE, LR_SHARED};
    if let Ok(hinstance) = GetModuleHandleW(None) {
        if let Ok(h) = LoadImageW(
            hinstance,
            windows::core::PCWSTR(1 as *const u16),
            IMAGE_ICON,
            0,
            0,
            LR_DEFAULTSIZE | LR_SHARED,
        ) {
            if !h.is_invalid() {
                return HICON(h.0);
            }
        }
    }
    make_icon()
}

unsafe fn make_icon() -> HICON {
    const S: i32 = 32;
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: S,
            biHeight: -S,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let screen = GetDC(None);
    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
    let color = CreateDIBSection(screen, &info, DIB_RGB_COLORS, &mut bits, None, 0)
        .expect("icon dib");
    ReleaseDC(None, screen);
    let px = std::slice::from_raw_parts_mut(bits as *mut u8, (S * S * 4) as usize);

    for y in 0..S {
        for x in 0..S {
            // Rounded-square coverage.
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let r = 8.0f32;
            let (cx, cy) = (16.0, 16.0);
            let (qx, qy) = ((fx - cx).abs() - (14.0 - r), (fy - cy).abs() - (14.0 - r));
            let dist = if qx > 0.0 && qy > 0.0 {
                (qx * qx + qy * qy).sqrt() - r
            } else {
                qx.max(qy) - r
            };
            let cov = (0.5 - dist).clamp(0.0, 1.0);

            // Diagonal indigo -> teal gradient.
            let t = (fx + fy) / 64.0;
            let (mut rr, mut gg, mut bb) = (
                0.36 + (0.13 - 0.36) * t,
                0.32 + (0.72 - 0.32) * t,
                0.92 + (0.78 - 0.92) * t,
            );

            // Lens dot.
            let d = ((fx - 16.0).powi(2) + (fy - 16.0).powi(2)).sqrt();
            let ring = ((0.5 - (d - 6.0).abs() + 1.4).clamp(0.0, 1.0)) * 0.9;
            rr += (1.0 - rr) * ring;
            gg += (1.0 - gg) * ring;
            bb += (1.0 - bb) * ring;

            let a = cov;
            let i = ((y * S + x) * 4) as usize;
            // Premultiplied BGRA.
            px[i] = (bb * a * 255.0) as u8;
            px[i + 1] = (gg * a * 255.0) as u8;
            px[i + 2] = (rr * a * 255.0) as u8;
            px[i + 3] = (a * 255.0) as u8;
        }
    }

    let mask = CreateBitmap(S, S, 1, 1, None);
    let icon_info = ICONINFO {
        fIcon: true.into(),
        hbmColor: color,
        hbmMask: mask,
        ..Default::default()
    };
    let icon = CreateIconIndirect(&icon_info).expect("icon");
    let _ = DeleteObject(color);
    let _ = DeleteObject(mask);
    icon
}

unsafe fn show_menu(hwnd: HWND, state: &mut TrayState) {
    // Keep a real app foreground if one is still available. If clicking the
    // tray already foregrounded the taskbar, retain the last timer snapshot.
    if let Some(active) = crate::window::external_foreground() {
        state.active_window = Some(active);
    }

    // Re-point the menu mode here so a High Contrast or app-mode flip since
    // startup lands on this menu instead of the next process launch.
    crate::theme::enable_dark_menus();
    let menu = CreatePopupMenu().expect("menu");
    let license = crate::license::status();
    let capture_flags = if license.can_capture() {
        MF_STRING
    } else {
        MF_STRING | MF_GRAYED
    };
    let _ = AppendMenuW(menu, capture_flags, CMD_CAPTURE, w!("Capture\tPrtScn"));
    // The accelerator is read from config rather than baked in: it stopped
    // being Ctrl+Alt+S the moment the shortcut became configurable.
    let cfg = crate::config::Config::load();
    let active_label: Vec<u16> = match cfg.capture_hotkey() {
        Some(hotkey) => format!("Capture active window	{}", crate::hotkey::label(Some(hotkey))),
        None => "Capture active window".to_string(),
    }
    .encode_utf16()
    .chain(std::iter::once(0))
    .collect();
    let _ = AppendMenuW(
        menu,
        capture_flags,
        CMD_CAPTURE_ACTIVE,
        PCWSTR(active_label.as_ptr()),
    );
    let delayed_label: Vec<u16> = format!("Capture after {} seconds", cfg.capture_delay_secs)
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let _ = AppendMenuW(
        menu,
        capture_flags,
        CMD_CAPTURE_DELAYED,
        PCWSTR(delayed_label.as_ptr()),
    );
    let _ = AppendMenuW(menu, MF_STRING, CMD_OPEN_FOLDER, w!("Open captures folder"));
    let _ = AppendMenuW(menu, MF_STRING, CMD_OPEN_VIDEOS, w!("Open videos folder"));
    let _ = AppendMenuW(menu, MF_STRING, CMD_HISTORY, w!("History\u{2026}"));
    if let Some(update) = &state.update {
        // A staged installer plus its hash sidecar means we can re-check
        // before launch. A leftover file alone is not an install (SBS-911).
        let staged = crate::installer::is_ready_to_launch(&crate::installer::staged_path(
            &update.version,
        ));
        let text = if staged {
            format!("Install update v{} now", update.version)
        } else {
            format!("Update available: v{}\u{2026}", update.version)
        };
        let label: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let _ = AppendMenuW(menu, MF_STRING, CMD_UPDATE, PCWSTR(label.as_ptr()));
    }
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
    let license_label: Vec<u16> = license
        .tray_label()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let _ = AppendMenuW(
        menu,
        MF_STRING | MF_GRAYED,
        0,
        PCWSTR(license_label.as_ptr()),
    );
    match license {
        // Deactivation and diagnostics now live in Settings, so a licensed
        // user's license section is just the status line above.
        crate::license::Status::Licensed { .. } => {}
        _ => {
            let _ = AppendMenuW(menu, MF_STRING, CMD_BUY, w!("Buy Matteshot\u{2026}"));
            let _ = AppendMenuW(
                menu,
                MF_STRING,
                CMD_ACTIVATE,
                w!("Enter license key\u{2026}"),
            );
        }
    }
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
    let _ = AppendMenuW(menu, MF_STRING, CMD_SETTINGS, w!("Settings\u{2026}"));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
    let _ = AppendMenuW(menu, MF_STRING, CMD_QUIT, w!("Quit Matteshot"));

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    // Required so the menu dismisses when clicking elsewhere.
    let _ = SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_NONOTIFY | TPM_BOTTOMALIGN,
        pt.x,
        pt.y,
        0,
        hwnd,
        None,
    );
    eprintln!("tray: command {}", cmd.0);
    let _ = DestroyMenu(menu);
    // Documented TrackPopupMenu quirk: without this, the next click on the
    // tray icon can be swallowed by leftover menu state.
    let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
        hwnd,
        windows::Win32::UI::WindowsAndMessaging::WM_NULL,
        WPARAM(0),
        LPARAM(0),
    );

    let action = match cmd.0 as usize {
        CMD_CAPTURE => Some(Action::Capture),
        CMD_CAPTURE_ACTIVE => Some(Action::CaptureActive),
        CMD_CAPTURE_DELAYED => Some(Action::CaptureDelayed),
        CMD_OPEN_FOLDER => Some(Action::OpenFolder),
        CMD_OPEN_VIDEOS => Some(Action::OpenVideos),
        CMD_HISTORY => Some(Action::History),
        CMD_SETTINGS => Some(Action::Settings),
        CMD_UPDATE => Some(Action::OpenUpdate),
        CMD_BUY => Some(Action::Buy),
        CMD_ACTIVATE => Some(Action::Activate),
        CMD_QUIT => Some(Action::Quit),
        _ => None,
    };
    if let Some(action) = action {
        let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
            hwnd,
            WM_TRAY_ACTION,
            WPARAM(action as usize),
            LPARAM(0),
        );
    }
}

pub fn request_existing_settings() -> bool {
    post_action(Action::Settings)
}

/// Ask the resident's loop to deactivate this machine's license. Routing
/// through the loop lets it hand the capture hotkeys back to Windows and keep
/// its own hotkey state in sync, which a direct call from the Settings window
/// cannot. False when there is no resident (standalone `--settings` mode), so
/// the caller can fall back to a direct deactivation.
pub fn request_deactivate() -> bool {
    post_action(Action::Deactivate)
}

fn post_action(action: Action) -> bool {
    let Some(hwnd) = crate::window::find_by_class("matteshot_tray") else {
        return false;
    };
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::PostMessageW(
            hwnd,
            WM_TRAY_ACTION,
            WPARAM(action as usize),
            LPARAM(0),
        )
        .is_ok()
    }
}

unsafe fn notify(hwnd: HWND, title: &str, text: &str) {
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_INFO,
        ..Default::default()
    };
    let t: Vec<u16> = title.encode_utf16().collect();
    let x: Vec<u16> = text.encode_utf16().collect();
    data.szInfoTitle[..t.len().min(63)].copy_from_slice(&t[..t.len().min(63)]);
    data.szInfo[..x.len().min(255)].copy_from_slice(&x[..x.len().min(255)]);
    let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            let cs = &*(lparam.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_TRAYICON => {
            let state = (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut TrayState).as_mut();
            if let Some(state) = state {
                match lparam.0 as u32 {
                    WM_LBUTTONUP => state.pending = Some(Action::Capture),
                    WM_RBUTTONUP => show_menu(hwnd, state),
                    _ => {}
                }
            }
            LRESULT(0)
        }
        WM_UPDATE_AVAILABLE => {
            let Some(update) = crate::update::take_available(lparam.0 as u64, hwnd.0 as isize)
            else {
                return LRESULT(0);
            };
            let state = (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut TrayState).as_mut();
            if let Some(state) = state {
                let changed = state.update.as_ref().map(|u| &u.version) != Some(&update.version);
                state.update = Some(update);
                if changed {
                    let version = &state.update.as_ref().unwrap().version;
                    let automatic =
                        crate::config::auto_update_from_load(crate::config::Config::try_load());
                    notify(
                        hwnd,
                        "Matteshot update available",
                        &if automatic {
                            format!("Version {version} is downloading in the background.")
                        } else {
                            format!("Version {version} is ready. Right-click Matteshot to download.")
                        },
                    );
                }
            }
            LRESULT(0)
        }
        WM_UPDATE_INSTALLING => {
            let Some(version) = crate::update::take_installing(lparam.0 as u64, hwnd.0 as isize)
            else {
                return LRESULT(0);
            };
            notify(
                hwnd,
                "Matteshot is updating",
                &format!("Installing version {version}. Matteshot will restart on its own."),
            );
            LRESULT(0)
        }
        WM_TRAY_ACTION => {
            let state = (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut TrayState).as_mut();
            if let Some(state) = state {
                state.pending = match wparam.0 {
                    x if x == Action::Capture as usize => Some(Action::Capture),
                    x if x == Action::CaptureActive as usize => Some(Action::CaptureActive),
                    x if x == Action::CaptureDelayed as usize => Some(Action::CaptureDelayed),
                    x if x == Action::OpenFolder as usize => Some(Action::OpenFolder),
                    x if x == Action::OpenVideos as usize => Some(Action::OpenVideos),
                    x if x == Action::History as usize => Some(Action::History),
                    x if x == Action::Settings as usize => Some(Action::Settings),
                    x if x == Action::OpenUpdate as usize => Some(Action::OpenUpdate),
                    x if x == Action::Buy as usize => Some(Action::Buy),
                    x if x == Action::Activate as usize => Some(Action::Activate),
                    x if x == Action::Deactivate as usize => Some(Action::Deactivate),
                    x if x == Action::Quit as usize => Some(Action::Quit),
                    _ => None,
                };
            }
            LRESULT(0)
        }
        WM_TIMER => {
            let state = (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut TrayState).as_mut();
            if let (Some(state), Some(active)) = (state, crate::window::external_foreground()) {
                state.active_window = Some(active);
            }
            LRESULT(0)
        }
        // Used by the installer and uninstaller. This runs on the resident's
        // own UI thread, allowing the message loop to unwind normally instead
        // of terminating the process during an active file operation.
        WM_CLOSE => {
            // SBS-893: `--quit` is another process and cannot see our
            // in-memory counter. Wait here so Finalize can publish or park
            // the partial. Silent: a MessageBox would hang the installer
            // on UI. Tray-menu Quit prompts and waits before this.
            let _ = crate::record::wait_until_late_finalize_idle(
                crate::record::LATE_FINALIZE_BOUND,
            );
            // SBS-743: after the wait, not before. That wait pumps, so an
            // update completion posted during it is still delivered; a
            // discard first would bump the generation and drop it.
            crate::update::discard_window(hwnd.0 as isize);
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

impl Tray {
    pub fn create() -> Result<Tray> {
        unsafe {
            let hinstance = GetModuleHandleW(None).context("resolve module handle")?;
            let class = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: hinstance.into(),
                lpszClassName: w!("matteshot_tray"),
                ..Default::default()
            };
            RegisterClassW(&class);

            let mut state = Box::new(TrayState {
                pending: None,
                update: None,
                active_window: crate::window::external_foreground(),
            });
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                w!("matteshot_tray"),
                w!("Matteshot"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                hinstance,
                Some(&mut *state as *mut TrayState as *const _),
            )
            .context("create tray window")?;

            let icon = make_icon();
            let mut data = NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: hwnd,
                uID: 1,
                uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
                uCallbackMessage: WM_TRAYICON,
                hIcon: icon,
                ..Default::default()
            };
            let tip: Vec<u16> = "Matteshot \u{2014} PrtScn to capture".encode_utf16().collect();
            data.szTip[..tip.len()].copy_from_slice(&tip);
            let _ = Shell_NotifyIconW(NIM_ADD, &data);
            let _ = SetTimer(hwnd, 1, 250, None);

            Ok(Tray { hwnd, state, _icon: icon })
        }
    }

    /// One-shot balloon notification (first run).
    pub fn notify(&self, title: &str, text: &str) {
        unsafe {
            notify(self.hwnd, title, text);
        }
    }

    pub fn update_url(&self) -> Option<String> {
        self.state.update.as_ref().map(|u| u.download_url.clone())
    }

    pub fn update_version(&self) -> Option<String> {
        self.state.update.as_ref().map(|u| u.version.clone())
    }

    pub fn active_window(&self) -> Option<HWND> {
        self.state
            .active_window
            .filter(|hwnd| crate::window::is_external(*hwnd))
    }

    /// Poll and clear the pending menu action.
    pub fn take_action(&mut self) -> Option<Action> {
        self.state.pending.take()
    }

    pub fn remove(&self) {
        unsafe {
            let _ = KillTimer(self.hwnd, 1);
            let data = NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: self.hwnd,
                uID: 1,
                ..Default::default()
            };
            let _ = Shell_NotifyIconW(NIM_DELETE, &data);
        }
    }

    pub fn quit() {
        unsafe { PostQuitMessage(0) };
    }
}

/// The window's `GWLP_USERDATA` points into `state`, and the 250ms timer and
/// the notify icon keep sending it messages for as long as the window
/// exists. Destroy the window here, synchronously, before the fields drop:
/// otherwise a `Tray` dropped on an error path leaves a live window whose
/// next `WM_TIMER` — pumped by, say, the failure dialog — dereferences freed
/// state.
impl Drop for Tray {
    fn drop(&mut self) {
        self.remove();
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED, STGM_READ};

    /// The autostart shortcut is the whole persistence mechanism now, so prove
    /// the shell can read back what we wrote rather than trusting that a file
    /// of some kind landed on disk.
    #[test]
    fn autostart_shortcut_resolves_to_its_target() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        let dir = std::env::temp_dir().join("matteshot-autostart-test");
        std::fs::create_dir_all(&dir).unwrap();
        let link = dir.join("Matteshot.lnk");
        let _ = std::fs::remove_file(&link);
        let target = std::env::current_exe().unwrap();

        write_shortcut(&link, &target).expect("write the shortcut");
        assert!(link.is_file(), "no shortcut was created");

        let resolved = unsafe {
            let shell_link: IShellLinkW =
                CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).unwrap();
            let file: IPersistFile = shell_link.cast().unwrap();
            file.Load(&HSTRING::from(link.as_os_str()), STGM_READ).unwrap();
            let mut buffer = [0u16; 260];
            shell_link.GetPath(&mut buffer, std::ptr::null_mut(), 0).unwrap();
            String::from_utf16_lossy(&buffer)
                .trim_end_matches('\0')
                .to_string()
        };
        assert_eq!(
            resolved.to_lowercase(),
            target.to_string_lossy().to_lowercase(),
            "the shortcut does not point at its target"
        );

        let _ = std::fs::remove_file(&link);
    }

    /// The Startup folder is per-user and must never resolve to a machine-wide
    /// location; a shortcut written there would need elevation we do not have.
    #[test]
    fn autostart_link_is_a_per_user_startup_path() {
        let link = autostart_link().expect("resolve the Startup folder");
        assert_eq!(link.file_name().unwrap(), "Matteshot.lnk");
        let text = link.to_string_lossy().to_lowercase();
        assert!(text.contains("startup"), "not a Startup folder path: {text}");
        assert!(!text.contains("programdata"), "resolved machine-wide: {text}");
    }
}
