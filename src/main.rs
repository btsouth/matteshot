#![windows_subsystem = "windows"]

mod annotate;
mod capture;
mod compose;
mod completion;
mod config;
mod delay;
mod diagnostics;
mod dpi;
mod history;
mod hotkey;
mod icon;
mod installer;
mod license;
mod license_ui;
mod number_prompt;
mod ocr;
mod pin;
mod audio;
mod recdone;
mod record;
mod recui;
mod scroll;
mod state_lock;
mod trim;
mod output;
mod overlay;
mod picker;
mod prtscn;
mod settings;
mod share;
mod spike;
mod style;
mod theme;
mod tray;
mod tweak;
mod update;
mod video_edit;
mod video_speed;
mod welcome;
mod window;
mod telemetry;

use anyhow::{bail, Context, Result};
use image::RgbaImage;
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::{MonitorFromWindow, HMONITOR, MONITOR_DEFAULTTOPRIMARY};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, IsWindow, MessageBoxW, PostMessageW, IDYES,
    MB_ICONERROR, MB_ICONINFORMATION, MB_ICONWARNING, MB_OK, MB_SETFOREGROUND, MB_TOPMOST,
    MB_YESNO, MSG, WM_CLOSE, WM_HOTKEY,
};

use std::sync::atomic::{AtomicBool, Ordering};

use crate::config::Config;
use crate::picker::PickAction;

const HOTKEY_ID: i32 = 1;
pub const HOTKEY_ID_PRTSCN: i32 = 2;

/// Max dimension of the downscaled capture used for picker previews.
const PREVIEW_MAX: u32 = 480;

/// Ask every Matteshot surface to close on its own UI thread, then stop the
/// resident. The installer uses this instead of taskkill so recordings and
/// exports get their normal cancellation/finalization path.
fn request_graceful_shutdown() -> Result<()> {
    fn close_all(class_name: &str, label: &str) -> Result<()> {
        close_all_within(class_name, label, std::time::Duration::from_secs(30))
    }

    fn close_all_within(
        class_name: &str,
        label: &str,
        timeout: std::time::Duration,
    ) -> Result<()> {
        loop {
            let hwnd = match crate::window::find_by_class(class_name) {
                Some(hwnd) => hwnd,
                None => return Ok(()),
            };
            unsafe {
                PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0))
                    .with_context(|| format!("ask {label} to close"))?;
            }
            let deadline = std::time::Instant::now() + timeout;
            while unsafe { IsWindow(hwnd) }.as_bool() {
                if std::time::Instant::now() >= deadline {
                    bail!("{label} is still busy; finish or cancel the current operation and try again");
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }

    // Stop producers first. A recorder can open the editor while it finishes,
    // so the editor is deliberately closed after the capture surfaces.
    close_all("matteshot_recui", "the recorder")?;
    close_all("matteshot_scrollpill", "scroll capture")?;
    close_all("matteshot_overlay", "the capture overlay")?;
    close_all("matteshot_picker", "the picker")?;
    close_all("matteshot_tweak", "the image editor")?;
    close_all("matteshot_recdone", "the video editor")?;
    close_all("matteshot_settings", "settings")?;
    close_all("matteshot_activation", "activation")?;
    close_all("matteshot_welcome", "welcome")?;
    close_all("matteshot_pin", "a pinned capture")?;
    // SBS-893: supervise_late_finalize has no window. Closing the tray next
    // would exit the resident mid-Finalize; the next start would then delete
    // the mid-write MP4. Wait the supervisor bound so Finalize can publish
    // or park the partial as Kept. This process's counter is empty when
    // `--quit` is a helper; the resident honors the same bound on tray
    // WM_CLOSE, so the tray close itself is allowed that long plus the
    // usual 30s unwind. A 30s tray wait here would return busy and the
    // installer would taskkill.
    if !record::wait_until_late_finalize_idle(record::LATE_FINALIZE_BOUND) {
        bail!("the recorder is still busy; finish or cancel the current operation and try again");
    }
    close_all_within(
        "matteshot_tray",
        "Matteshot",
        record::QUIT_TRAY_WAIT,
    )?;
    Ok(())
}

enum Source {
    /// Capture `hwnd` via WGC (with monitor-crop fallback). When `frozen` is
    /// set (overlay window pick), any live-capture failure uses those pixels
    /// instead of erroring out.
    Window {
        hwnd: HWND,
        frozen: Option<RgbaImage>,
    },
    Image(RgbaImage),
}

/// Run a message loop until `open` reports the window is gone. Only for the
/// standalone diagnostic commands; the resident already has a loop.
fn pump_until_closed(open: fn() -> bool) {
    let mut msg = MSG::default();
    unsafe {
        while open() && GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = windows::Win32::UI::WindowsAndMessaging::TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Report a failed operation once, then hand the error back untouched.
///
/// Tagging happens at the branch that knows which operation ran, because every
/// capture path funnels into the same handler by the time the error surfaces,
/// and "something failed" is the one answer that would not help.
fn note_failure<T>(operation: &'static str, result: Result<T>) -> Result<T> {
    if let Err(error) = &result {
        telemetry::report_failure(operation, error);
    }
    result
}

fn error_box(text: &str) {
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(HSTRING::from(text).as_ptr()),
            w!("Matteshot"),
            MB_OK | MB_ICONERROR,
        );
    }
}

fn require_capture_license() -> Result<()> {
    if license::status().can_capture() {
        Ok(())
    } else {
        bail!(
            "Your 14-day trial has ended. Open Matteshot and enter a license key from the tray menu."
        )
    }
}

fn previews_for(raw: &RgbaImage, styles: &[style::Style]) -> Vec<RgbaImage> {
    let (w, h) = (raw.width(), raw.height());
    let scale = (PREVIEW_MAX as f32 / w.max(h) as f32).min(1.0);
    let small = if scale < 1.0 {
        image::imageops::resize(
            raw,
            (w as f32 * scale) as u32,
            (h as f32 * scale) as u32,
            image::imageops::FilterType::Triangle,
        )
    } else {
        raw.clone()
    };

    let t0 = std::time::Instant::now();
    let previews = std::thread::scope(|s| {
        let handles: Vec<_> = styles
            .iter()
            .map(|st| s.spawn(|| compose::compose_scaled(&small, st, scale)))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    eprintln!("timing: 6 previews in {:?}", t0.elapsed());
    previews
}

fn shoot(source: Source, monitor: HMONITOR, pick_override: Option<usize>) -> Result<()> {
    // Loaded fresh each capture so settings changes apply immediately.
    let cfg = Config::load();
    // Names the editor tab, so it is taken before the pixels are.
    let mut capture_title;
    let raw = match source {
        Source::Window { hwnd, frozen } => {
            capture_title = window::title_of(hwnd);
            eprintln!("capturing: {capture_title}");
            // Overlay picks carry the freeze-frame crop of what the user
            // highlighted. Prefer a live WGC frame when the window is
            // capturable, but never let the monitor-crop fallback inside
            // `capture_window` mask a WGC rejection: for a window that has
            // since moved or dismissed (Ceiling's hover flyout hides when the
            // pointer leaves the widget) the monitor crop would "succeed" by
            // capturing whatever is behind it. The frozen crop is always what
            // the user actually saw.
            match frozen {
                Some(frozen_img) => match capture::capture_window_wgc(hwnd) {
                    Ok(img) => img,
                    Err(e) => {
                        eprintln!("live window capture failed ({e:#}); using freeze-frame crop");
                        frozen_img
                    }
                },
                None => capture::capture_window(hwnd).context("capture failed")?,
            }
        }
        Source::Image(img) => {
            capture_title = format!("Region {}\u{00d7}{}", img.width(), img.height());
            eprintln!("capturing: region {}x{}", img.width(), img.height());
            img
        }
    };
    if capture_title.trim().is_empty() {
        capture_title = format!("Capture {}\u{00d7}{}", raw.width(), raw.height());
    }
    // A successful window or region capture starts the trial regardless of
    // which picker action follows. This includes the zero-touch auto-copy
    // path, Esc (where auto-copy stands), OCR, pinning, and the tweak editor.
    license::record_successful_capture();
    let styles = style::variants(&raw);
    let names: Vec<&'static str> = styles.iter().map(|s| s.name).collect();

    // The preselected matte is exported and copied in the background the
    // moment the picker opens: pressing PrtScn already means "I want this on
    // my clipboard" — picking a number just switches which one.
    struct AutoCopy {
        canceled: bool,
        path: Option<std::path::PathBuf>,
    }
    let auto = std::sync::Arc::new(std::sync::Mutex::new(AutoCopy { canceled: false, path: None }));
    let preselect = cfg.last_style.min(styles.len().saturating_sub(1));
    let mut auto_worker = None;

    let action = match pick_override {
        Some(i) => {
            if i >= styles.len() {
                bail!("--pick must be 1..={}", styles.len());
            }
            PickAction::Choose(i)
        }
        None => {
            {
                let auto = auto.clone();
                let raw = raw.clone();
                let style = styles[preselect].clone();
                let (scale, max_edge, dir) =
                    (cfg.export_scale, cfg.output_max_edge, cfg.save_dir());
                let source = capture_title.clone();
                let worker = std::thread::spawn(move || {
                    let styled = compose::export(
                        &raw,
                        &style,
                        compose::DEFAULT_PAD_FACTOR,
                        None,
                        scale,
                    );
                    let styled = output::resize_to_max_edge(&styled, max_edge);
                    let mut st = auto.lock().unwrap();
                    if st.canceled {
                        return;
                    }
                    if let Ok(path) = output::save_png(&styled, style.name, &dir, Some(&source)) {
                        match output::to_clipboard(&styled, Some(&path)) {
                            Ok(()) => {
                                eprintln!("auto-copy [{}]: {}", style.name, path.display());
                                st.path = Some(path);
                            }
                            Err(error) => {
                                eprintln!("auto-copy failed [{}]: {error:#}", style.name);
                                let _ = std::fs::remove_file(path);
                            }
                        }
                    } else {
                        eprintln!("auto-copy failed [{}]: could not save capture", style.name);
                    }
                });
                auto_worker = Some(worker);
            }
            let previews = previews_for(&raw, &styles);
            picker::pick(&previews, &names, monitor, cfg.last_style)?
        }
    };

    // Helper: stop the auto-copy (if still running) and remove its file (if
    // it already landed) — used when the user's explicit action supersedes it.
    let cancel_auto = || {
        let mut st = auto.lock().unwrap();
        st.canceled = true;
        st.path.take()
    };

    let chosen = match action {
        PickAction::Choose(i) => i,
        PickAction::Cancel => {
            // Esc keeps the auto-copy: the no-touch flow — PrtScn, select,
            // Esc, paste. Wait for the background attempt so a failed save or
            // clipboard write cannot disappear silently after the picker has
            // closed. Falling through retries the same output synchronously;
            // a second failure reaches the resident's visible error dialog.
            if let Some(worker) = auto_worker.take() {
                let _ = worker.join();
            }
            if let Some(path) = auto.lock().unwrap().path.take() {
                eprintln!("cancelled (auto-copy stands): {}", path.display());
                return Ok(());
            }
            eprintln!("background auto-copy failed; retrying synchronously");
            preselect
        }
        PickAction::Pin => {
            return pin::show(raw, monitor);
        }
        PickAction::CopyText => {
            // The user wants text, not the image — retire the auto-copy so a
            // late-finishing export can't clobber the OCR text on the clipboard.
            if let Some(p) = cancel_auto() {
                let _ = std::fs::remove_file(p);
            }
            telemetry::report("matteshot_ocr_used");
            return ocr::copy_text(&raw);
        }
        PickAction::Tweak(i) => {
            // The tweak editor owns every later Copy action. Synchronize with
            // the background auto-copy before opening it so a slow clipboard
            // write cannot overwrite the user's edited result afterward.
            if let Some(p) = cancel_auto() {
                let _ = std::fs::remove_file(p);
            }
            // The editor is non-modal, so this returns immediately. PrtScn
            // while it is open is now just another capture: the hotkey reaches
            // the resident's loop as normal instead of tearing down the
            // editor and replaying a reshoot through here.
            telemetry::report("matteshot_editor_opened");
            return tweak::open(raw, styles, i, monitor, capture_title);
        }
        PickAction::Share(i) => {
            // Same reasoning as Tweak: an in-flight auto-copy of a
            // different variant must not land after this one uploads.
            if let Some(p) = cancel_auto() {
                let _ = std::fs::remove_file(p);
            }
            let styled = compose::export(
                &raw,
                &styles[i],
                compose::DEFAULT_PAD_FACTOR,
                None,
                cfg.export_scale,
            );
            let styled = output::resize_to_max_edge(&styled, cfg.output_max_edge);
            let path = output::save_png(&styled, styles[i].name, &cfg.save_dir(), Some(&capture_title))?;
            let url = share::share_file(&path).context("could not share this capture")?;
            output::open_url(&url);
            // The share itself already succeeded and the link is already
            // open in the browser; a clipboard miss here is a lesser,
            // recoverable failure and must not read as "sharing failed".
            if let Err(error) = output::text_to_clipboard(&url) {
                eprintln!("share link clipboard copy failed: {error:#}");
            }
            let _ = Config::update(|cfg| cfg.last_style = i);
            eprintln!("shared [{}]: {}", styles[i].name, url);
            return Ok(());
        }
        PickAction::Reshoot(sel, mon) => {
            // PrtScn mid-pick: the user re-snipped; replace the pending shot
            // (and its auto-copy — the new capture makes its own).
            if let Some(p) = cancel_auto() {
                let _ = std::fs::remove_file(p);
            }
            eprintln!("reshoot: replacing the pending capture");
            return match sel {
                overlay::Selection::Window { hwnd, frozen } => shoot(
                    Source::Window {
                        hwnd,
                        frozen: Some(frozen),
                    },
                    mon,
                    None,
                ),
                overlay::Selection::Region(img) => shoot(Source::Image(img), mon, None),
                overlay::Selection::RecordWindow(h) => {
                    record::session(record::Target::window(h), cfg.record_gif)
                }
                overlay::Selection::RecordRegion(r, m) => {
                    record::session(record::Target::region(r, m), cfg.record_gif)
                }
                overlay::Selection::ScrollWindow(h, anchor) => {
                    let img = scroll::capture(scroll::Target::Window(h, anchor))?;
                    shoot(Source::Image(img), mon, None)
                }
                overlay::Selection::ScrollRegion(r, m, anchor) => {
                    let img = scroll::capture(scroll::Target::Region(r, m, anchor))?;
                    shoot(Source::Image(img), mon, None)
                }
                // Re-snipped into a delay. Run the countdown here rather than
                // handing back to shoot_overlay, which would open a fresh
                // undelayed overlay and make the user choose Delay twice.
                overlay::Selection::Delay(seconds) => {
                    // Persisted after the overlay closed, never during: the
                    // overlay's own timing log (freeze_ms) must not include
                    // config I/O.
                    if seconds != cfg.capture_delay_secs {
                        let _ = Config::update(|cfg| cfg.capture_delay_secs = seconds);
                    }
                    if note_failure("delay", delay::countdown(seconds))? {
                        shoot_overlay_delayed()
                    } else {
                        Ok(())
                    }
                }
            };
        }
    };

    // If they confirmed the preselected matte and the auto-copy already
    // landed, the work is done — don't export the same thing twice.
    let auto_path = cancel_auto();
    if chosen == preselect {
        if let Some(path) = &auto_path {
            let _ = Config::update(|cfg| cfg.last_style = chosen);
            eprintln!("done [{}] (auto-copy reused): {}", styles[chosen].name, path.display());
            return Ok(());
        }
    }

    let styled = compose::export(
        &raw,
        &styles[chosen],
        compose::DEFAULT_PAD_FACTOR,
        None,
        cfg.export_scale,
    );
    let styled = output::resize_to_max_edge(&styled, cfg.output_max_edge);
    let path = output::save_png(&styled, styles[chosen].name, &cfg.save_dir(), Some(&capture_title))?;
    output::to_clipboard(&styled, Some(&path)).context("clipboard failed")?;
    // A different pick supersedes the auto-copied file.
    if let Some(old) = auto_path {
        if old != path {
            let _ = std::fs::remove_file(old);
        }
    }
    let _ = Config::update(|cfg| cfg.last_style = chosen);
    eprintln!(
        "done [{}]: {}x{} -> clipboard + {}",
        styles[chosen].name,
        styled.width(),
        styled.height(),
        path.display()
    );
    Ok(())
}

/// Reopen after a countdown that has already run.
fn shoot_overlay_delayed() -> Result<()> {
    shoot_overlay_from(true)
}

/// PrtScn / tray-click flow: freeze-frame overlay, then the picker.
fn shoot_overlay() -> Result<()> {
    shoot_overlay_from(false)
}

fn shoot_overlay_from(start_delayed: bool) -> Result<()> {
    // A delay reopens the overlay, so this is a loop rather than recursion:
    // the screen freezes the moment the overlay opens, so the only way to
    // catch a menu is to get out of the way, count down, and freeze again.
    let mut delayed = start_delayed;
    let mut seconds = Config::load().capture_delay_secs;
    loop {
        let selection = note_failure("overlay", overlay::select(delayed, seconds))?;
        match selection {
            Some((overlay::Selection::Delay(chosen), _)) => {
                // Picking a new delay from the overlay's own list arms it
                // immediately and becomes the new default. Persisted after
                // the overlay closed, never during (config I/O on that path
                // shows up in freeze_ms).
                if chosen != seconds {
                    seconds = chosen;
                    let _ = Config::update(|cfg| cfg.capture_delay_secs = chosen);
                }
                // Escape during the countdown abandons the capture rather than
                // bringing the overlay back.
                if !note_failure("delay", delay::countdown(seconds))? {
                    return Ok(());
                }
                // The reopened overlay says so, or coming back looks like a glitch.
                delayed = true;
            }
            other => return dispatch_selection(other),
        }
    }
}

/// Act on what the overlay returned. Delay never reaches here: the loop
/// above consumes it.
fn dispatch_selection(
    selection: Option<(overlay::Selection, HMONITOR)>,
) -> Result<()> {
    match selection {
        Some((overlay::Selection::Window { hwnd, frozen }, mon)) => note_failure(
            "capture",
            shoot(
                Source::Window {
                    hwnd,
                    frozen: Some(frozen),
                },
                mon,
                None,
            ),
        ),
        Some((overlay::Selection::Region(img), mon)) => {
            note_failure("capture", shoot(Source::Image(img), mon, None))
        }
        Some((overlay::Selection::RecordWindow(hwnd), _)) => {
            let result = note_failure(
                "record",
                record::session(record::Target::window(hwnd), Config::load().record_gif),
            );
            if result.is_ok() {
                license::record_successful_capture();
                telemetry::report("matteshot_record_completed");
            }
            result
        }
        Some((overlay::Selection::RecordRegion(r, mon), _)) => {
            let result = note_failure(
                "record",
                record::session(record::Target::region(r, mon), Config::load().record_gif),
            );
            if result.is_ok() {
                license::record_successful_capture();
                telemetry::report("matteshot_record_completed");
            }
            result
        }
        Some((overlay::Selection::ScrollWindow(h, anchor), mon)) => {
            let img = note_failure("scroll", scroll::capture(scroll::Target::Window(h, anchor)))?;
            telemetry::report("matteshot_scroll_capture");
            note_failure("capture", shoot(Source::Image(img), mon, None))
        }
        Some((overlay::Selection::ScrollRegion(r, m, anchor), mon)) => {
            let img = note_failure(
                "scroll",
                scroll::capture(scroll::Target::Region(r, m, anchor)),
            )?;
            telemetry::report("matteshot_scroll_capture");
            note_failure("capture", shoot(Source::Image(img), mon, None))
        }
        // Consumed by the loop in shoot_overlay, so arriving here means that
        // stopped being true. Not worth panicking over, but silence would hide
        // a regression behind a capture that simply does nothing.
        Some((overlay::Selection::Delay(_), _)) => {
            diagnostics::log("delay reached the dispatcher; capture skipped");
            Ok(())
        }
        None => {
            eprintln!("cancelled");
            Ok(())
        }
    }
}

/// Ctrl+Alt+S flow: instant, zero-touch on the active window.
fn shoot_active() -> Result<()> {
    let fg = window::external_foreground().context("No active app window to capture")?;
    shoot_active_window(fg)
}

fn shoot_active_window(fg: HWND) -> Result<()> {
    let mon = unsafe { MonitorFromWindow(fg, MONITOR_DEFAULTTOPRIMARY) };
    note_failure(
        "capture",
        shoot(
            Source::Window {
                hwnd: fg,
                frozen: None,
            },
            mon,
            None,
        ),
    )
}

/// Whether the configured capture shortcut is currently registered. Settings
/// reads this to say so, since a shortcut another app already owns is
/// otherwise indistinguishable from one that simply does nothing.
static CAPTURE_HOTKEY_TAKEN: AtomicBool = AtomicBool::new(false);

pub fn capture_hotkey_taken() -> bool {
    CAPTURE_HOTKEY_TAKEN.load(Ordering::Relaxed)
}

/// Re-register the capture shortcut after Settings changes it, reporting
/// whether the new combo is actually available.
///
/// Safe to call from Settings because `settings::open` runs on the resident's
/// message loop, and `RegisterHotKey(None, ..)` binds to the calling thread,
/// so this is the same thread that holds the existing registration. The
/// standalone `--settings` process is the exception: it has no hotkeys of its
/// own to rebind, exactly as the PrtScn toggle already behaves there.
pub fn rebind_capture_hotkey() -> bool {
    unsafe {
        let _ = UnregisterHotKey(None, HOTKEY_ID);
    }
    let taken = match Config::load().capture_hotkey() {
        Some(hotkey) => {
            unsafe { RegisterHotKey(None, HOTKEY_ID, hotkey.modifiers, hotkey.vk) }.is_err()
        }
        // Unbound on purpose is not the same as unavailable.
        None => false,
    };
    CAPTURE_HOTKEY_TAKEN.store(taken, Ordering::Relaxed);
    if taken {
        diagnostics::log("new capture shortcut already owned by another app");
    }
    !taken
}

fn enable_capture_hotkeys() -> Result<bool> {
    // Never fatal. This used to be `?`, so a shortcut another app already
    // owned propagated out of run_app, and because the binary is a windows
    // subsystem app with no console, main's eprintln went nowhere: Matteshot
    // exited at launch with no tray icon and no message. It also aborted
    // before PrtScn was acquired, so one collision cost every hotkey and left
    // the user no way into Settings to change it.
    let configured = Config::load().capture_hotkey();
    let mut taken = false;
    if let Some(hotkey) = configured {
        let registered =
            unsafe { RegisterHotKey(None, HOTKEY_ID, hotkey.modifiers, hotkey.vk) }.is_ok();
        taken = !registered;
        if !registered {
            diagnostics::log("capture shortcut already owned by another app");
            telemetry::report_failure(
                "hotkey",
                &anyhow::anyhow!("register capture shortcut: already in use"),
            );
        }
    }
    CAPTURE_HOTKEY_TAKEN.store(taken, Ordering::Relaxed);

    if !prtscn::preferred() {
        return Ok(false);
    }
    Ok(matches!(
        prtscn::acquire(HOTKEY_ID_PRTSCN, true),
        prtscn::Acquire::Taken | prtscn::Acquire::TakenAfterToggle
    ))
}

fn disable_capture_hotkeys(restore_windows_prtscn: bool) {
    unsafe {
        let _ = UnregisterHotKey(None, HOTKEY_ID);
    }
    prtscn::release(HOTKEY_ID_PRTSCN);
    if restore_windows_prtscn {
        let _ = prtscn::set_snipping_binding(true);
    }
}

/// Confirm and deactivate this machine's license. Lives here rather than in the
/// Settings window because disabling capture needs the resident's hotkey
/// ownership (the Ctrl+Alt+S registration, the PrtScn hook, and handing PrtScn
/// back to Windows). Called when Settings' Deactivate button is clicked.
pub(crate) fn deactivate_license() -> Result<()> {
    let answer = unsafe {
        MessageBoxW(
            None,
            // No device count, and no implied plural: the limit lives on the
            // license and can be raised per key, and the certificate does not
            // carry it, so the client cannot know whether there is one slot or
            // ten. Say what the action does and leave the arithmetic alone.
            w!("Deactivate Matteshot on this PC? This frees up a device slot."),
            w!("Matteshot"),
            MB_YESNO | MB_ICONWARNING,
        )
    };
    if answer == IDYES {
        license::deactivate()?;
        settings::refresh();
        if !license::status().can_capture() {
            disable_capture_hotkeys(true);
        }
    }
    Ok(())
}

fn ensure_capture_allowed(hotkeys_active: &mut bool) -> Result<bool> {
    if license::status().can_capture() {
        if !*hotkeys_active {
            let _ = enable_capture_hotkeys()?;
            *hotkeys_active = true;
        }
        return Ok(true);
    }

    if *hotkeys_active {
        disable_capture_hotkeys(true);
        *hotkeys_active = false;
    }
    if license_ui::open()? {
        let _ = enable_capture_hotkeys()?;
        *hotkeys_active = true;
        settings::refresh();
        Ok(true)
    } else {
        Ok(false)
    }
}

fn run_app() -> Result<()> {
    let guard = match claim_resident_slot() {
        Ok(guard) => guard,
        Err(error) => {
            // CreateMutexW itself failed — nothing is held yet, but this is
            // exactly the silent exit the dialog exists for.
            report_resident_failure(&error, false);
            return Err(error);
        }
    };
    let Some(_resident_guard) = guard else {
        // A shortcut or autostart race should never create a second tray app
        // that competes for PrtScn. Make an intentional second launch useful
        // by opening Settings on the established resident.
        for _ in 0..20 {
            if tray::request_existing_settings() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        diagnostics::log("duplicate launch routed to resident");
        return Ok(());
    };
    let result = run_resident();
    if let Err(error) = &result {
        // Reported here, not in main: the resident mutex is still held, so
        // nothing races into the slot while the dialog is up, and the tray
        // (if it ever existed) has already destroyed its window on the way
        // out of run_resident, so the dialog's message pump has no freed
        // state to reach. Hand the capture hooks back first — "start
        // Matteshot again" has to be able to take them.
        disable_capture_hotkeys(true);
        report_resident_failure(error, RESIDENT_READY.load(Ordering::Relaxed));
    }
    result
}

/// `Some` when this process now owns the single-resident slot, `None` when
/// another resident holds it. A resident that is on its way out still owns
/// the slot for a moment — an update restarts the app the instant the
/// installer finishes — so give the previous process time to release before
/// deciding a resident is really there; otherwise the relaunch hands off to a
/// corpse and exits, leaving the user with no tray app and no PrtScn.
fn claim_resident_slot() -> Result<Option<state_lock::ProcessMutex>> {
    let mut guard = state_lock::try_process_mutex("Local\\Matteshot.Resident.Process")?;
    if guard.is_none() {
        for _ in 0..30 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            guard = state_lock::try_process_mutex("Local\\Matteshot.Resident.Process")?;
            if guard.is_some() {
                diagnostics::log("resident slot claimed after the previous process exited");
                break;
            }
        }
    }
    Ok(guard)
}

/// Everything after the resident slot is ours: telemetry, hotkeys, the tray,
/// and the message loop. Owns the `Tray` for its whole lifetime, so an error
/// anywhere in here drops it — and its window — before `run_app` reports.
fn run_resident() -> Result<()> {
    telemetry::init();
    telemetry::report("matteshot_launch");
    let config = Config::load();
    let cleaned = output::cleanup_stale_video_partials(&config.video_dir())
        + output::cleanup_stale_png_partials(&config.save_dir());
    if cleaned > 0 {
        diagnostics::log(&format!("recovered stale partials count={cleaned}"));
    }
    capture::warmup();
    prtscn::set_preferred(Config::load().capture_prtscn);
    let initial_license = license::status();
    let mut hotkeys_active = initial_license.can_capture();
    let prtscn_ours = if hotkeys_active {
        enable_capture_hotkeys()?
    } else {
        disable_capture_hotkeys(true);
        false
    };
    diagnostics::log(if prtscn_ours {
        "prtscn acquired at startup"
    } else {
        "prtscn NOT acquired at startup; see the tray menu"
    });

    let mut tray = tray::Tray::create()?;
    // Cheap and idempotent, but it has to run before anything reads the
    // autostart state, or the tray menu shows the box unchecked for someone
    // whose old Run value is still the thing starting Matteshot.
    tray::migrate_autostart_from_run_key();
    // A shortcut another app owns used to be fatal and silent. It is survivable
    // now, but survivable and unexplained is its own trap: the user presses the
    // key, nothing happens, and nothing ever says why. PrtScn and the tray are
    // unaffected, so this is the only place that can tell them.
    if capture_hotkey_taken() {
        // Canonical spelling, not whatever was typed, so "alt+ctrl+s" in the
        // config file still reads as Ctrl+Alt+S here.
        let configured = hotkey::label(Config::load().capture_hotkey());
        tray.notify(
            "Capture shortcut unavailable",
            &format!(
                "{configured} is already used by another app. \
                 PrtScn still works. Pick a different shortcut in Settings."
            ),
        );
    }
    diagnostics::log(if tray::autostart_enabled() {
        "autostart enabled at startup"
    } else {
        "autostart disabled at startup"
    });
    eprintln!(
        "matteshot: ready — PrtScn {} | Ctrl+Alt+S = active window",
        if prtscn_ours { "= capture overlay" } else { "not held (see tray menu)" }
    );
    diagnostics::log("resident ready");
    RESIDENT_READY.store(true, Ordering::Relaxed);

    // First run: show the core loop before the app disappears into the tray.
    // This is non-modal, so PrtScn and tray commands remain live.
    let cfg = Config::load();
    if !cfg.onboarded && initial_license.can_capture() {
        if let Err(error) = welcome::open_first_run() {
            diagnostics::log("welcome window failed");
            eprintln!("welcome: {error:#}");
            tray.notify(
                "Matteshot is ready",
                "Press PrtScn to capture. Right-click the tray icon for settings.",
            );
            let _ = Config::update(|cfg| cfg.onboarded = true);
        }
    }
    update::start(tray.hwnd);
    license::start_background_refresh();

    if matches!(initial_license, license::Status::Expired) && license_ui::open()? {
        let _ = enable_capture_hotkeys()?;
        hotkeys_active = true;
    }

    let mut msg = MSG::default();
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if msg.message == WM_HOTKEY {
                let result = if ensure_capture_allowed(&mut hotkeys_active)? {
                    match msg.wParam.0 as i32 {
                        HOTKEY_ID_PRTSCN => shoot_overlay(),
                        HOTKEY_ID => shoot_active(),
                        _ => Ok(()),
                    }
                } else {
                    Ok(())
                };
                if let Err(e) = result {
                    eprintln!("error: {e:#}");
                    error_box(&format!("Capture failed: {e:#}"));
                }
            }
            let _ = windows::Win32::UI::WindowsAndMessaging::TranslateMessage(&msg);
            DispatchMessageW(&msg);

            if let Some(action) = welcome::take_action() {
                let result = match action {
                    welcome::Action::Capture => {
                        if ensure_capture_allowed(&mut hotkeys_active)? {
                            shoot_overlay()
                        } else {
                            Ok(())
                        }
                    }
                    welcome::Action::Settings => settings::open(),
                };
                if let Err(error) = result {
                    eprintln!("welcome action: {error:#}");
                    error_box(&format!("{error:#}"));
                }
            }

            // A timer message reaches this loop every 250 ms. If Windows or
            // another exiting instance briefly owned PrtScn during startup,
            // recover automatically instead of believing a failed one-shot
            // registration succeeded forever.
            if hotkeys_active
                && prtscn::preferred()
                && !prtscn::owns_key()
                && prtscn::take(HOTKEY_ID_PRTSCN)
            {
                settings::refresh();
            }

            if let Some(action) = tray.take_action() {
                let result = match action {
                    tray::Action::Capture => {
                        if ensure_capture_allowed(&mut hotkeys_active)? {
                            shoot_overlay()
                        } else {
                            Ok(())
                        }
                    }
                    // The countdown exists so a menu can be opened during it,
                    // so the tray popup must be gone before it starts; the
                    // action is already deferred until the popup dismisses.
                    tray::Action::CaptureDelayed => {
                        if ensure_capture_allowed(&mut hotkeys_active)? {
                            let seconds = Config::load().capture_delay_secs;
                            if note_failure("delay", delay::countdown(seconds))? {
                                shoot_overlay()
                            } else {
                                Ok(())
                            }
                        } else {
                            Ok(())
                        }
                    }
                    tray::Action::CaptureActive => {
                        if ensure_capture_allowed(&mut hotkeys_active)? {
                            tray.active_window()
                                .context("No active app window to capture")
                                .and_then(shoot_active_window)
                        } else {
                            Ok(())
                        }
                    }
                    tray::Action::OpenFolder => {
                        output::open_folder(&Config::load().save_dir());
                        Ok(())
                    }
                    tray::Action::OpenVideos => {
                        output::open_folder(&Config::load().video_dir());
                        Ok(())
                    }
                    tray::Action::History => history::open(),
                    tray::Action::Settings => settings::open(),
                    tray::Action::OpenUpdate => {
                        // Prefer an installer we have already downloaded and
                        // proved is ours; the download page is the fallback
                        // when staging never happened or failed.
                        match tray.update_version().map(|v| installer::staged_path(&v)) {
                            Some(staged) if staged.is_file() => {
                                tray.notify(
                                    "Matteshot is updating",
                                    "Installing now. Matteshot will restart on its own.",
                                );
                                std::thread::sleep(std::time::Duration::from_millis(1200));
                                installer::launch(&staged)?;
                            }
                            _ => {
                                if let Some(url) = tray.update_url() {
                                    output::open_url(&url);
                                }
                            }
                        }
                        Ok(())
                    }
                    tray::Action::Buy => {
                        output::open_url(license::BUY_URL);
                        Ok(())
                    }
                    tray::Action::Activate => {
                        if license_ui::open()? && !hotkeys_active {
                            let _ = enable_capture_hotkeys()?;
                            hotkeys_active = true;
                        }
                        settings::refresh();
                        Ok(())
                    }
                    // Reached from Settings' Deactivate button via the tray
                    // window; routing here keeps `hotkeys_active` in sync when
                    // the loop hands the capture hotkeys back.
                    tray::Action::Deactivate => {
                        deactivate_license()?;
                        if !license::status().can_capture() {
                            hotkeys_active = false;
                        }
                        Ok(())
                    }
                    tray::Action::Quit => {
                        diagnostics::log("resident quit requested");
                        if record::late_finalize_outstanding() {
                            // Owned and topmost: the wait below can hold the
                            // thread for 15 minutes, and an unowned box can
                            // open behind the foreground app, which reads as
                            // a hang with no visible prompt.
                            MessageBoxW(
                                tray.hwnd,
                                w!("A recording is still finishing. Matteshot will quit when it is done."),
                                w!("Matteshot"),
                                MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND | MB_TOPMOST,
                            );
                            if !record::wait_until_late_finalize_idle(record::LATE_FINALIZE_BOUND) {
                                bail!("the recorder is still busy; finish or cancel the current operation and try again");
                            }
                        }
                        tray.remove();
                        tray::Tray::quit();
                        Ok(())
                    }
                };
                if let Err(e) = result {
                    eprintln!("error: {e:#}");
                    error_box(&format!("{e:#}"));
                }
            }
        }
    }
    Ok(())
}

/// Time the editor's two preview-rebuild paths across working-bitmap sizes.
///
/// The editor rebuilds in two shapes and they cost very differently, so a
/// single number would be misleading. A *cold* rebuild recomposes the matte
/// and is what a matte, padding, or aspect change pays. A *cached* rebuild
/// reuses that composite and only restamps annotations, which is what every
/// annotation edit pays — the interactive one, and the one that decides
/// whether a larger preview feels slower to draw on.
fn preview_bench(long_edge: u32) -> Result<()> {
    use rayon::prelude::*;

    let height = (long_edge as f32 * 1694.0 / 2862.0) as u32;
    // Fine detail, so a resize has real work to do rather than smearing flat
    // colour. Timing barely cares; realism costs nothing here.
    let mut raw = RgbaImage::new(long_edge, height);
    for (x, y, pixel) in raw.enumerate_pixels_mut() {
        let checker = ((x / 3 + y / 3) % 2) as u8;
        *pixel = image::Rgba([
            32u8.saturating_add(checker * 180),
            40u8.saturating_add((y % 251) as u8),
            60u8.saturating_add((x % 199) as u8),
            255,
        ]);
    }
    let style = style::variants(&raw)
        .into_iter()
        .next()
        .context("no matte styles")?;

    // A working set on the heavy side of typical: shapes cost per pixel they
    // cover, so under-annotating would flatter the larger sizes.
    let annotations: Vec<annotate::Annotation> = vec![
        annotate::Shape::Rect { a: (120.0, 140.0), b: (900.0, 700.0) },
        annotate::Shape::Arrow { from: (200.0, 900.0), to: (1200.0, 1300.0) },
        annotate::Shape::Ellipse { a: (1300.0, 200.0), b: (2000.0, 800.0) },
        annotate::Shape::Highlight { a: (300.0, 1400.0), b: (1800.0, 1500.0) },
        annotate::Shape::Text { pos: (400.0, 300.0), text: "Annotation".into() },
        annotate::Shape::Counter { pos: (1000.0, 1000.0), n: 3 },
    ]
    .into_iter()
    .map(|shape| annotate::Annotation {
        shape,
        color: 0,
        size: 1.0,
        text_style: annotate::TextStyle::Shadow,
        text_box_opacity: 1.0,
    })
    .collect();

    let time = |runs: u32, body: &mut dyn FnMut()| -> f64 {
        // One untimed pass first: the allocator and any lazy init should not
        // land on the first measured run.
        body();
        let start = std::time::Instant::now();
        for _ in 0..runs {
            body();
        }
        start.elapsed().as_secs_f64() * 1000.0 / runs as f64
    };

    // Order-of-magnitude guards, deliberately not performance targets. The
    // real interactive claim is that an annotation edit lands inside a frame,
    // which is ~5ms on a developer machine, but this runs on CI hardware too
    // and a bound tight enough to be a target would fail there for no reason.
    // These survive a runner several times slower while still catching an edit
    // that got an order of magnitude more expensive — which is what a
    // regression here would look like.
    const CACHED_BUDGET_MS: f64 = 40.0;
    const COLD_BUDGET_MS: f64 = 200.0;

    eprintln!("preview bench: source {long_edge}x{height}, {} annotations", annotations.len());
    eprintln!(
        "{:>6}  {:>11}  {:>7}  {:>7}  {:>7}",
        "cap", "preview", "source", "cold", "cached"
    );

    let mut over_budget: Vec<String> = Vec::new();
    let (mut worst_cold, mut worst_cached) = (0.0f64, 0.0f64);

    for cap in [1200u32, 1600, 2000, 2400, 2862, long_edge] {
        if cap > long_edge {
            continue;
        }
        let metric = (cap as f32 / long_edge as f32).min(1.0);
        let small = if metric < 1.0 {
            image::imageops::resize(
                &raw,
                (long_edge as f32 * metric) as u32,
                (height as f32 * metric) as u32,
                image::imageops::FilterType::Triangle,
            )
        } else {
            raw.clone()
        };
        let opts = compose::ComposeOpts {
            metric_scale: metric,
            pad_factor: compose::DEFAULT_PAD_FACTOR,
            aspect: None,
        };
        let (sw, sh) = (small.width() as usize, small.height() as usize);
        let layout = compose::layout(sw, sh, &opts);
        let offset = (layout.pad_x as f32, layout.pad_y as f32);

        // Source build: downscaling the capture and its drag-quality half.
        // Paid once when the pane grows, not per rebuild.
        let source = time(3, &mut || {
            let built = tweak::preview_sources_for_bench(&raw, cap);
            std::hint::black_box(&built);
        });

        // Cold: recompose the matte, blend the content in, stamp annotations,
        // swizzle to BGRA. What a matte/padding/aspect change pays.
        let cold = time(3, &mut || {
            let mut base = compose::compose_base(sw, sh, &style, &opts);
            compose::blend_content(&mut base, &small, &opts);
            annotate::render(&mut base, &annotations, metric, offset, None);
            let mut bgra = base.into_raw();
            bgra.par_chunks_mut(4).for_each(|pixel| {
                pixel.swap(0, 2);
                pixel[3] = 255;
            });
            std::hint::black_box(&bgra);
        });

        // Cached: the composite is reused, so this is the clone, the
        // annotation stamp, and the swizzle. What every annotation edit pays.
        let mut base = compose::compose_base(sw, sh, &style, &opts);
        compose::blend_content(&mut base, &small, &opts);
        let cached = time(5, &mut || {
            let mut img = base.clone();
            annotate::render(&mut img, &annotations, metric, offset, None);
            let mut bgra = img.into_raw();
            bgra.par_chunks_mut(4).for_each(|pixel| {
                pixel.swap(0, 2);
                pixel[3] = 255;
            });
            std::hint::black_box(&bgra);
        });

        eprintln!(
            "{:>6}  {:>11}  {:>5.1}ms  {:>5.1}ms  {:>5.1}ms",
            cap,
            format!("{}x{}", small.width(), small.height()),
            source,
            cold,
            cached
        );

        worst_cold = worst_cold.max(cold);
        worst_cached = worst_cached.max(cached);
        if cached > CACHED_BUDGET_MS {
            over_budget.push(format!(
                "an annotation edit at {cap} takes {cached:.1}ms, over {CACHED_BUDGET_MS:.0}ms"
            ));
        }
        if cold > COLD_BUDGET_MS {
            over_budget.push(format!(
                "a matte change at {cap} takes {cold:.1}ms, over {COLD_BUDGET_MS:.0}ms"
            ));
        }
    }

    if !over_budget.is_empty() {
        for line in &over_budget {
            eprintln!("over budget: {line}");
        }
        bail!("preview rebuild is over budget");
    }
    // run-probes.ps1 matches this line, so keep the wording stable.
    eprintln!(
        "preview rebuild within budget: worst cold {worst_cold:.1}ms of {COLD_BUDGET_MS:.0}ms, \
         worst cached {worst_cached:.1}ms of {CACHED_BUDGET_MS:.0}ms"
    );
    Ok(())
}

/// Auto-stop for the headless recording probes: close this session's stop
/// pill `secs` after the capture loop's encoding clock starts.
///
/// The countdown must not start at spawn, and the pill's appearance is not
/// enough either: recui creates it before WGC and the encoder finish setting
/// up on the worker. Counted from either of those, a 3-second probe on a
/// starved CI VM once shipped a 0.17s fixture and the strict probe suite
/// failed on it. `record::PROBE_CAPTURE_RUNNING` flips exactly when encoded
/// time starts accumulating, so the requested seconds measure recording.
///
/// The pill is looked up at fire time, restricted to this process: caching a
/// desktop-wide handle across the sleep could close the resident app's pill
/// (leaving this session recording until the job timeout) or post to a
/// recycled handle.
fn schedule_recording_stop(secs: u64) {
    std::thread::spawn(move || {
        use std::sync::atomic::Ordering;
        // Bounded wait: if capture never starts the session failed and its
        // own error already ended the probe; there is nothing to stop.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        while !record::PROBE_CAPTURE_RUNNING.load(Ordering::Acquire) {
            if std::time::Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        std::thread::sleep(std::time::Duration::from_secs(secs));
        if let Some(pill) = crate::window::find_own_by_class("matteshot_recui") {
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    pill,
                    windows::Win32::UI::WindowsAndMessaging::WM_CLOSE,
                    windows::Win32::Foundation::WPARAM(0),
                    windows::Win32::Foundation::LPARAM(0),
                );
            }
        }
    });
}

/// `--video-speed-test` compresses the middle half. A source under one
/// second is not a broken exporter — it is a fixture the harness should
/// have replaced. 10_000_000 is one second in Media Foundation's 100ns units.
fn meets_speed_export_min_duration(duration_100ns: i64) -> bool {
    duration_100ns >= 10_000_000
}

/// Integer 100ns ticks so the probe harness cannot round 0.9995s into 1.00s
/// and then send a sub-second clip into `--video-speed-test`.
fn duration_test_report(duration_100ns: i64) -> String {
    format!("duration_100ns: {duration_100ns}")
}

fn main() -> Result<()> {
    unsafe {
        // STA: the folder picker (IFileDialog) requires it; WGC's
        // free-threaded frame pool is unaffected.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    theme::enable_dark_menus();
    diagnostics::init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        // One-shot mode, mainly for testing: capture a window by title substring
        // (or the current foreground window) and exit. `--pick N` skips the
        // picker UI. `--overlay` runs the freeze-frame overlay instead.
        Some("--once") => {
            require_capture_license()?;
            let mut hwnd = window::foreground();
            let mut pick_override = None;
            let mut use_overlay = false;
            let mut use_tweak = false;
            let mut rest = args[1..].iter();
            while let Some(flag) = rest.next() {
                match flag.as_str() {
                    "--window" => {
                        let needle = rest.next().context("--window needs a title substring")?;
                        hwnd = window::find_by_title(needle)
                            .with_context(|| format!("no visible window matching {needle:?}"))?;
                    }
                    "--pick" => {
                        let n: usize = rest
                            .next()
                            .context("--pick needs a number")?
                            .parse()
                            .context("--pick needs a number")?;
                        pick_override = Some(n.saturating_sub(1));
                    }
                    "--overlay" => use_overlay = true,
                    "--tweak" => use_tweak = true,
                    other => bail!("unknown flag {other:?}"),
                }
            }
            if use_overlay {
                shoot_overlay()
            } else if use_tweak {
                let mon = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTOPRIMARY) };
                let raw = capture::capture_window(hwnd).context("capture failed")?;
                license::record_successful_capture();
                let styles = style::variants(&raw);
                let title = window::title_of(hwnd);
                tweak::open(raw, styles, 0, mon, title)?;
                // The editor no longer owns a loop of its own, so this
                // standalone probe has to pump one until the window closes.
                pump_until_closed(tweak::is_open);
                Ok(())
            } else {
                let mon = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTOPRIMARY) };
                shoot(
                    Source::Window {
                        hwnd,
                        frozen: None,
                    },
                    mon,
                    pick_override,
                )
            }
        }
        // Open several captures as tabs in one editor (testing). Each argument
        // is a window title substring.
        Some("--tweak-tabs-test") => {
            require_capture_license()?;
            let needles: Vec<&String> = args[1..].iter().collect();
            if needles.is_empty() {
                bail!("--tweak-tabs-test needs one or more window title substrings");
            }
            for needle in needles {
                let hwnd = window::find_by_title(needle)
                    .with_context(|| format!("no visible window matching {needle:?}"))?;
                let mon = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTOPRIMARY) };
                let raw = capture::capture_window(hwnd).context("capture failed")?;
                license::record_successful_capture();
                let styles = style::variants(&raw);
                let title = window::title_of(hwnd);
                eprintln!("tab: {title}");
                tweak::open(raw, styles, 0, mon, title)?;
            }
            pump_until_closed(tweak::is_open);
            Ok(())
        }
        // Warm-path capture benchmark: same window three times in-process.
        Some("--bench") => {
            require_capture_license()?;
            let needle = args.get(1).context("--bench needs a window title substring")?;
            let hwnd = window::find_by_title(needle)
                .with_context(|| format!("no visible window matching {needle:?}"))?;
            let mut last = None;
            for i in 1..=3 {
                let t = std::time::Instant::now();
                let img = capture::capture_window(hwnd)?;
                eprintln!("bench {i}: {}x{} in {:?}", img.width(), img.height(), t.elapsed());
                last = Some(img);
            }
            if let Some(img) = last {
                license::record_successful_capture();
                let p = std::env::temp_dir().join("matteshot-bench.png");
                img.save(&p)?;
                eprintln!("bench: raw capture saved to {}", p.display());
            }
            Ok(())
        }
        // Headless timing of the tweak editor's preview rebuild at a range of
        // working-bitmap sizes. The editor caps its preview source and then
        // stretches it to fill the pane, so on a large monitor the picture it
        // annotates against is softer than the capture. Raising the cap costs
        // time on every rebuild, and this is what says how much.
        Some("--preview-bench") => {
            let long_edge: u32 = args
                .get(1)
                .and_then(|value| value.parse().ok())
                .unwrap_or(2862);
            preview_bench(long_edge)
        }
        // Headless timing of the multi-monitor freeze and GDI-layer path.
        // `sequential` keeps the old capture order as a local baseline.
        Some("--overlay-bench") => {
            require_capture_license()?;
            let batched = match args.get(1).map(String::as_str) {
                None | Some("batched") => true,
                Some("sequential") => false,
                Some(other) => bail!("unknown overlay benchmark mode {other:?}"),
            };
            overlay::benchmark_freeze(batched)
        }
        // Regenerate the app icon (assets\matteshot.ico) with the product's
        // own compositing pipeline.
        Some("--icon") => {
            let outdir = std::path::PathBuf::from(args.get(1).map(String::as_str).unwrap_or("assets"));
            icon::generate(&outdir)
        }
        // Marketing/site asset generator: capture a window and export every
        // matte style as a PNG into a directory.
        Some("--assets") => {
            require_capture_license()?;
            let needle = args.get(1).context("--assets <title substr> <outdir>")?;
            let outdir = std::path::PathBuf::from(args.get(2).context("--assets <title substr> <outdir>")?);
            std::fs::create_dir_all(&outdir)?;
            let hwnd = window::find_by_title(needle)
                .with_context(|| format!("no visible window matching {needle:?}"))?;
            let raw = capture::capture_window(hwnd).context("capture failed")?;
            license::record_successful_capture();
            for s in style::variants(&raw) {
                let img = compose::export(&raw, &s, compose::DEFAULT_PAD_FACTOR, None, 2);
                let p = outdir.join(format!("matte-{}.png", s.name.to_lowercase()));
                img.save(&p)?;
                eprintln!("{} {}x{}", p.display(), img.width(), img.height());
            }
            raw.save(outdir.join("raw.png"))?;
            Ok(())
        }
        // Record the primary monitor region for N seconds (testing).
        Some("--record-test") => {
            require_capture_license()?;
            let secs: u64 = args.get(1).map(|s| s.parse().unwrap_or(4)).unwrap_or(4);
            let mon = unsafe {
                windows::Win32::Graphics::Gdi::MonitorFromPoint(
                    windows::Win32::Foundation::POINT { x: 0, y: 0 },
                    MONITOR_DEFAULTTOPRIMARY,
                )
            };
            let rect = windows::Win32::Foundation::RECT {
                left: 0,
                top: 0,
                right: 640,
                bottom: 480,
            };
            schedule_recording_stop(secs);
            record::session(record::Target::region(rect, mon), true)?;
            license::record_successful_capture();
            // Keep pumping briefly so the review window can be inspected.
            let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let mut msg = MSG::default();
            unsafe {
                while std::time::Instant::now() < until {
                    while windows::Win32::UI::WindowsAndMessaging::PeekMessageW(
                        &mut msg,
                        None,
                        0,
                        0,
                        windows::Win32::UI::WindowsAndMessaging::PM_REMOVE,
                    )
                    .as_bool()
                    {
                        let _ =
                            windows::Win32::UI::WindowsAndMessaging::TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
            Ok(())
        }
        // Hands-off window-recording probe. The recording path is real; only
        // the stop action is timed so resize and editor behavior can be tested
        // without synthesizing Matteshot's global shortcuts.
        Some("--record-window-test") => {
            require_capture_license()?;
            let needle = args
                .get(1)
                .context("--record-window-test <title> [seconds]")?;
            let secs: u64 = args.get(2).and_then(|value| value.parse().ok()).unwrap_or(6);
            let hwnd = window::find_by_title(needle)
                .with_context(|| format!("no visible window matching {needle:?}"))?;
            schedule_recording_stop(secs);
            record::session(record::Target::window(hwnd), false)?;
            license::record_successful_capture();

            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10 * 60);
            let mut msg = MSG::default();
            unsafe {
                while std::time::Instant::now() < deadline
                    && window::find_by_class("matteshot_recdone").is_some()
                {
                    while windows::Win32::UI::WindowsAndMessaging::PeekMessageW(
                        &mut msg,
                        None,
                        0,
                        0,
                        windows::Win32::UI::WindowsAndMessaging::PM_REMOVE,
                    )
                    .as_bool()
                    {
                        let _ = windows::Win32::UI::WindowsAndMessaging::TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(15));
                }
            }
            Ok(())
        }
        // Scroll-capture a window by title, save raw (testing).
        Some("--scroll-test") => {
            require_capture_license()?;
            let needle = args.get(1).context("--scroll-test <title>")?;
            let hwnd = window::find_by_title(needle)
                .with_context(|| format!("no visible window matching {needle:?}"))?;
            let img = scroll::capture(scroll::Target::centered_window(hwnd))?;
            license::record_successful_capture();
            let p = std::env::temp_dir().join("matteshot-scroll.png");
            img.save(&p)?;
            eprintln!("saved {}x{} -> {}", img.width(), img.height(), p.display());
            Ok(())
        }
        // Video export probe: cut [start,end] seconds and optionally apply
        // one of the 1-based matte choices.
        Some("--trim-test") => {
            let src = std::path::PathBuf::from(
                args.get(1).context("--trim-test <file> <a> <b> [1-7]")?,
            );
            let a: f64 = args.get(2).map(|s| s.parse().unwrap_or(1.0)).unwrap_or(1.0);
            let b: f64 = args.get(3).map(|s| s.parse().unwrap_or(3.0)).unwrap_or(3.0);
            let probe = trim::probe(&src, 6, 54)?;
            eprintln!(
                "probe: {:.2}s, {} thumbs",
                probe.duration_100ns as f64 / 1e7,
                probe.thumbs.len()
            );
            let selected = if let Some(value) = args.get(4) {
                let index = value
                    .parse::<usize>()
                    .ok()
                    .filter(|index| (1..=7).contains(index))
                    .context("matte must be 1-7")?
                    - 1;
                let (bytes, w, h) = probe.thumbs.first().context("video has no preview frame")?;
                let frame = trim::bgra_to_rgba(bytes, *w, *h);
                crate::style::variants(&frame).get(index).cloned()
            } else {
                None
            };
            let dst = src.with_extension(if selected.is_some() { "matte.mp4" } else { "trim.mp4" });
            trim::cut_with_matte(
                &src,
                &dst,
                (a * 1e7) as i64,
                (b * 1e7) as i64,
                selected.as_ref(),
            )?;
            eprintln!("exported -> {}", dst.display());
            Ok(())
        }
        // Full video editor export probe. Uses a deterministic matte and one
        // sample of every annotation type without touching the clipboard.
        Some("--video-edit-test") => {
            let src =
                std::path::PathBuf::from(args.get(1).context("--video-edit-test <mp4>")?);
            let probe = trim::probe(&src, 8, 72)?;
            let duration = probe.duration_100ns.max(5_000_000);
            let (bytes, w, h) = probe.thumbs.first().context("video has no preview frame")?;
            let frame = trim::bgra_to_rgba(bytes, *w, *h);
            let style = crate::style::variants(&frame)
                .into_iter()
                .next()
                .context("no matte styles")?;
            let items = vec![
                video_edit::Item {
                    shape: video_edit::Shape::Text {
                        pos: (0.08, 0.08),
                        text: "Matteshot video edit".into(),
                    },
                    start: 0,
                    end: duration,
                    color: 3,
                    size: 1.35,
                    caption_style: video_edit::CaptionStyle::Box,
                    caption_box_opacity: 0.68,
                },
                video_edit::Item {
                    shape: video_edit::Shape::Arrow {
                        from: (0.14, 0.72),
                        to: (0.38, 0.50),
                    },
                    start: 0,
                    end: duration,
                    color: 0,
                    size: 1.0,
                    caption_style: video_edit::CaptionStyle::Shadow,
                    caption_box_opacity: 0.68,
                },
                video_edit::Item {
                    shape: video_edit::Shape::Rect {
                        a: (0.54, 0.22),
                        b: (0.82, 0.52),
                    },
                    start: 0,
                    end: duration,
                    color: 2,
                    size: 1.0,
                    caption_style: video_edit::CaptionStyle::Shadow,
                    caption_box_opacity: 0.68,
                },
                video_edit::Item {
                    shape: video_edit::Shape::Blur {
                        a: (0.58, 0.68),
                        b: (0.84, 0.82),
                    },
                    start: 0,
                    end: duration,
                    color: 0,
                    size: 1.0,
                    caption_style: video_edit::CaptionStyle::Shadow,
                    caption_box_opacity: 0.68,
                },
            ];
            let dst = src.with_extension("edit.mp4");
            let opts = compose::ComposeOpts {
                metric_scale: 1.0,
                pad_factor: 0.14,
                aspect: Some(1.0),
            };
            trim::cut_with_edit_progress(
                &src,
                &dst,
                0,
                duration,
                Some((&style, &opts)),
                &items,
                |_| {},
            )?;
            eprintln!("video editor export (1:1, 14% padding) -> {}", dst.display());
            Ok(())
        }
        Some("--duration-test") => {
            let src =
                std::path::PathBuf::from(args.get(1).context("--duration-test <mp4>")?);
            let source = trim::probe_opening(&src, 320, 180)?;
            eprintln!("{}", duration_test_report(source.duration_100ns));
            Ok(())
        }
        // Speed-section export probe. Compresses the middle half to 4x and
        // verifies the produced file reports the correspondingly shorter
        // playable duration.
        Some("--video-speed-test") => {
            let src =
                std::path::PathBuf::from(args.get(1).context("--video-speed-test <mp4>")?);
            let source = trim::probe_opening(&src, 320, 180)?;
            let duration = source.duration_100ns;
            anyhow::ensure!(
                meets_speed_export_min_duration(duration),
                "video must be at least one second"
            );
            let speed = video_speed::SpeedRange::new(duration / 4, duration * 3 / 4, 4);
            let map = video_speed::TimeMap::new(0, duration, &[speed])
                .map_err(anyhow::Error::msg)?;
            let expected = map.output_duration();
            let dst = src.with_extension("speed.mp4");
            let cancel = std::sync::atomic::AtomicBool::new(false);
            trim::cut_with_speed_edit_progress_cancel(
                &src,
                &dst,
                0,
                duration,
                None,
                &[],
                &[speed],
                video_edit::Crop::FULL,
                &cancel,
                |_| {},
            )?;
            trim::validate_video(&dst)?;
            let actual = trim::probe_opening(&dst, 320, 180)?.duration_100ns;
            const DURATION_TOLERANCE: i64 = 1_000_000; // 100ms in 100ns ticks.
            anyhow::ensure!(
                (actual - expected).abs() <= DURATION_TOLERANCE,
                "speed export duration was {:.2}s; expected {:.2}s",
                actual as f64 / 1e7,
                expected as f64 / 1e7
            );
            eprintln!(
                "video speed export: {:.2}s -> {:.2}s -> {}",
                duration as f64 / 1e7,
                actual as f64 / 1e7,
                dst.display()
            );
            Ok(())
        }
        // Paced editor-playback probe. Decodes three seconds at preview size,
        // never opens a window, writes a file, or touches the clipboard.
        Some("--playback-test") => {
            let src =
                std::path::PathBuf::from(args.get(1).context("--playback-test <mp4>")?);
            let paused = trim::preview_frame(&src, 0, 960, 540)?;
            let cancel = std::sync::atomic::AtomicBool::new(false);
            let started = std::time::Instant::now();
            let mut frames = 0u32;
            let mut last = 0i64;
            let mut playback_size = None;
            trim::playback_frames(
                &src,
                0,
                30_000_000,
                960,
                540,
                &cancel,
                |frame| {
                    frames += 1;
                    last = frame.timestamp;
                    playback_size.get_or_insert((frame.width, frame.height));
                    true
                },
            )?;
            eprintln!(
                "paused: {}x{} · playback: {}x{} · {frames} frames through {:.2}s in {:.2}s",
                paused.1,
                paused.2,
                playback_size.unwrap_or_default().0,
                playback_size.unwrap_or_default().1,
                last as f64 / 10_000_000.0,
                started.elapsed().as_secs_f64()
            );
            Ok(())
        }
        // Open the focused recording editor around an existing MP4. This is
        // visual-only: it does not capture or touch the clipboard.
        Some("--review-test") => {
            let src =
                std::path::PathBuf::from(args.get(1).context("--review-test <mp4>")?);
            let probe = trim::probe(&src, 8, 72)?;
            let duration = probe.duration_100ns.max(0);
            recdone::show(
                src,
                None,
                ((duration as f64 / 10_000_000.0) * 30.0).round() as u32,
                (duration as u64 / 10_000_000).max(1),
                None,
            )?;
            let mut msg = MSG::default();
            unsafe {
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    let _ = windows::Win32::UI::WindowsAndMessaging::TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            Ok(())
        }
        // OCR probe: recognize and print (no clipboard) — testing.
        Some("--ocr") => {
            require_capture_license()?;
            let needle = args.get(1).context("--ocr needs a title substring")?;
            let hwnd = window::find_by_title(needle)
                .with_context(|| format!("no visible window matching {needle:?}"))?;
            let img = capture::capture_window(hwnd)?;
            license::record_successful_capture();
            let text = ocr::recognize(&img)?;
            eprintln!("--- {} chars ---", text.len());
            for line in text.lines().take(12) {
                eprintln!("{line}");
            }
            Ok(())
        }
        // Word-box probe for select-text mode: prints each recognized word in
        // capture coordinates so the overlay geometry can be checked headlessly.
        // With a destination path, writes the word boxes as JSON instead of
        // printing them. matteshot.app's demo editor ships that file so its
        // select-text is this engine's real output over the sample capture
        // rather than a mock.
        Some("--ocr-words") => {
            require_capture_license()?;
            if args.len() > 3 {
                bail!("--ocr-words <title|png> [out.json]");
            }
            let target = args
                .get(1)
                .context("--ocr-words <title|png> [out.json]")?;
            // A path exercises oversized captures (scroll stitches) that no
            // live window can reach.
            let img = if std::path::Path::new(target).is_file() {
                image::open(target).context("open image")?.to_rgba8()
            } else {
                let hwnd = window::find_by_title(target)
                    .with_context(|| format!("no visible window matching {target:?}"))?;
                let captured = capture::capture_window(hwnd)?;
                license::record_successful_capture();
                captured
            };
            let words = ocr::recognize_words(&img)?;
            if let Some(out) = args.get(2) {
                let round = |value: f32| (value * 10.0).round() / 10.0;
                let payload = serde_json::json!({
                    "width": img.width(),
                    "height": img.height(),
                    "words": words
                        .iter()
                        .map(|word| {
                            let (x0, y0, x1, y1) = word.rect;
                            serde_json::json!({
                                "text": word.text,
                                "line": word.line,
                                "rect": [round(x0), round(y0), round(x1), round(y1)],
                            })
                        })
                        .collect::<Vec<_>>(),
                });
                // Compact: a generated asset a browser downloads, not
                // something anyone hand-edits.
                std::fs::write(out, serde_json::to_string(&payload)?)
                    .with_context(|| format!("write {out}"))?;
                eprintln!(
                    "{} words over {}x{} -> {out}",
                    words.len(),
                    img.width(),
                    img.height()
                );
                return Ok(());
            }
            eprintln!(
                "--- {} words over {}x{} ---",
                words.len(),
                img.width(),
                img.height()
            );
            for word in words.iter() {
                let (x0, y0, x1, y1) = word.rect;
                eprintln!(
                    "line {:>2}  [{:>6.1},{:>6.1} {:>6.1}x{:>5.1}]  {}",
                    word.line,
                    x0,
                    y0,
                    x1 - x0,
                    y1 - y0,
                    word.text
                );
            }
            Ok(())
        }
        // Render sample annotations onto a capture and save raw (testing).
        Some("--annotate-demo") => {
            require_capture_license()?;
            let needle = args.get(1).context("--annotate-demo needs a title substring")?;
            let hwnd = window::find_by_title(needle)
                .with_context(|| format!("no visible window matching {needle:?}"))?;
            let mut img = capture::capture_window(hwnd)?;
            license::record_successful_capture();
            let (w, h) = (img.width() as f32, img.height() as f32);
            let anns = vec![
                annotate::Annotation {
                    shape: annotate::Shape::Arrow {
                        from: (w * 0.15, h * 0.75),
                        to: (w * 0.45, h * 0.4),
                    },
                    color: 0,
                    size: 1.0,
                    text_style: annotate::TextStyle::Shadow,
                    text_box_opacity: 0.68,
                },
                annotate::Annotation {
                    shape: annotate::Shape::Rect {
                        a: (w * 0.55, h * 0.3),
                        b: (w * 0.9, h * 0.5),
                    },
                    color: 2,
                    size: 1.0,
                    text_style: annotate::TextStyle::Shadow,
                    text_box_opacity: 0.68,
                },
                annotate::Annotation {
                    shape: annotate::Shape::Blur {
                        a: (w * 0.55, h * 0.6),
                        b: (w * 0.9, h * 0.75),
                    },
                    color: 0,
                    size: 1.0,
                    text_style: annotate::TextStyle::Shadow,
                    text_box_opacity: 0.68,
                },
                annotate::Annotation {
                    shape: annotate::Shape::Text {
                        pos: (w * 0.12, h * 0.82),
                        text: "the bug is here".into(),
                    },
                    color: 1,
                    size: 1.0,
                    text_style: annotate::TextStyle::Box,
                    text_box_opacity: 0.68,
                },
                annotate::Annotation {
                    shape: annotate::Shape::Line {
                        from: (w * 0.1, h * 0.12),
                        to: (w * 0.45, h * 0.12),
                    },
                    color: 3,
                    size: 1.0,
                    text_style: annotate::TextStyle::Shadow,
                    text_box_opacity: 0.68,
                },
                annotate::Annotation {
                    shape: annotate::Shape::Ellipse {
                        a: (w * 0.52, h * 0.08),
                        b: (w * 0.95, h * 0.24),
                    },
                    color: 1,
                    size: 1.0,
                    text_style: annotate::TextStyle::Shadow,
                    text_box_opacity: 0.68,
                },
                annotate::Annotation {
                    shape: annotate::Shape::Highlight {
                        a: (w * 0.08, h * 0.28),
                        b: (w * 0.45, h * 0.36),
                    },
                    color: 1,
                    size: 1.0,
                    text_style: annotate::TextStyle::Shadow,
                    text_box_opacity: 0.68,
                },
                annotate::Annotation {
                    shape: annotate::Shape::Counter { pos: (w * 0.2, h * 0.55), n: 1 },
                    color: 0,
                    size: 1.0,
                    text_style: annotate::TextStyle::Shadow,
                    text_box_opacity: 0.68,
                },
                annotate::Annotation {
                    shape: annotate::Shape::Counter { pos: (w * 0.5, h * 0.62), n: 2 },
                    color: 0,
                    size: 1.0,
                    text_style: annotate::TextStyle::Shadow,
                    text_box_opacity: 0.68,
                },
            ];
            annotate::render(&mut img, &anns, 1.0, (0.0, 0.0), None);
            let p = std::env::temp_dir().join("matteshot-annotate-demo.png");
            img.save(&p)?;
            eprintln!("saved {}", p.display());
            Ok(())
        }
        // Open only the settings window (testing).
        Some("--settings") => {
            settings::open()?;
            let mut msg = MSG::default();
            unsafe {
                while settings::is_open() && GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    let _ = windows::Win32::UI::WindowsAndMessaging::TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            Ok(())
        }
        // Open only the history window (testing).
        Some("--history") => {
            history::open()?;
            let mut msg = MSG::default();
            unsafe {
                while history::is_open() && GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    let _ = windows::Win32::UI::WindowsAndMessaging::TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            Ok(())
        }
        // Open the first-run surface without changing the user's real config.
        Some("--welcome") => {
            welcome::open_preview()?;
            let mut msg = MSG::default();
            unsafe {
                while welcome::is_open() && GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    let _ = windows::Win32::UI::WindowsAndMessaging::TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            Ok(())
        }
        Some("--spike-dpi") => {
            require_capture_license()?;
            let needle = args.get(1).context("--spike-dpi needs a window title substring")?;
            let hwnd = window::find_by_title(needle)
                .with_context(|| format!("no visible window matching {needle:?}"))?;
            spike::run(hwnd, needle)?;
            license::record_successful_capture();
            Ok(())
        }
        Some("--take-printscreen") => {
            if prtscn::set_snipping_binding(false).is_ok() {
                eprintln!("PrtScn unbound from Snipping Tool. Matteshot will grab it while running. Undo: matteshot --restore-printscreen");
            }
            Ok(())
        }
        Some("--restore-printscreen") => {
            prtscn::set_snipping_binding(true)?;
            eprintln!("PrtScn re-bound to Snipping Tool.");
            Ok(())
        }
        Some("--quit") => request_graceful_shutdown(),
        // Download and fully verify the published installer without running
        // it. Proves the hash and signature gates before anything is trusted
        // enough to execute.
        // Authenticode gate probe: must accept our installer and reject
        // everything else, including files Windows itself trusts.
        Some("--verify-signature-test") => {
            let path = args
                .get(1)
                .context("--verify-signature-test needs a file path")?;
            match installer::verify_signature(std::path::Path::new(path)) {
                Ok(()) => eprintln!("ACCEPT {path}"),
                Err(error) => eprintln!("REJECT {path}: {error:#}"),
            }
            Ok(())
        }
        Some("--update-stage-test") => {
            // An explicit URL lets this exercise the gates even when the app is
            // already current.
            let (url, version) = match args.get(1) {
                Some(url) => (url.clone(), "probe".to_string()),
                None => match update::check_once()? {
                    Some(update) => (update.download_url, update.version),
                    None => {
                        eprintln!("already current at {}", env!("CARGO_PKG_VERSION"));
                        return Ok(());
                    }
                },
            };
            eprintln!("staging {version} from {url}");
            let staged = installer::stage(&url, &version, |percent| {
                if percent % 25 == 0 {
                    eprintln!("  {percent}%");
                }
            })?;
            let bytes = std::fs::metadata(&staged).map(|m| m.len()).unwrap_or(0);
            eprintln!(
                "verified hash + signature: {} ({bytes} bytes)",
                staged.display()
            );
            eprintln!("not installing (probe only)");
            Ok(())
        }
        // The whole production path end to end, minus the wait for an idle
        // moment: check, download, verify, install silently (testing).
        Some("--update-install-now") => {
            // An explicit URL skips the version comparison so a build can
            // install the published artifact regardless of its own version.
            let (url, version) = match args.get(1) {
                Some(url) => (url.clone(), "probe".to_string()),
                None => {
                    let update =
                        update::check_once()?.context("no newer version published")?;
                    (update.download_url, update.version)
                }
            };
            eprintln!("staging {version} from {url}");
            let staged = installer::stage(&url, &version, |_| {})?;
            eprintln!("verified: {}", staged.display());
            installer::launch(&staged)?;
            eprintln!("installer started silently");
            Ok(())
        }
        // Live update endpoint probe (testing; never downloads anything).
        Some("--update-test") => {
            match update::check_once()? {
                Some(update) => eprintln!(
                    "update available: {} -> {}",
                    update.version, update.download_url
                ),
                None => eprintln!("update check: current ({})", env!("CARGO_PKG_VERSION")),
            }
            Ok(())
        }
        // Open only the activation window (testing; does not capture or write
        // to the clipboard).
        // Countdown only, with no capture after it: proves the pill paints,
        // counts, and can be cancelled without writing anything.
        Some("--delay-test") => {
            let seconds = args
                .get(1)
                .and_then(|v| v.parse().ok())
                .unwrap_or(delay::DEFAULT_SECONDS);
            let started = std::time::Instant::now();
            let completed = delay::countdown(seconds)?;
            eprintln!(
                "delay: requested={seconds}s effective={}s elapsed={:.1}s {}",
                delay::sanitize(seconds),
                started.elapsed().as_secs_f32(),
                if completed { "completed" } else { "cancelled" }
            );
            Ok(())
        }
        Some("--license") => {
            let activated = license_ui::open()?;
            eprintln!(
                "license window closed: {}",
                if activated { "activated" } else { "unchanged" }
            );
            Ok(())
        }
        Some("--license-status") => {
            eprintln!("{}", license::status().tray_label());
            #[cfg(feature = "debug-license")]
            if let Ok(value) = std::env::var("MATTESHOT_LICENSE_OVERRIDE") {
                if license::debug_override_active() {
                    eprintln!("(forced by MATTESHOT_LICENSE_OVERRIDE={value})");
                } else {
                    eprintln!(
                        "(ignoring unrecognized MATTESHOT_LICENSE_OVERRIDE={value}; \
                         the state above is real)"
                    );
                }
            }
            Ok(())
        }
        // Support-safe activation path: the key is read from redirected stdin
        // so it never appears in process arguments or diagnostic output.
        Some("--activate-stdin") => {
            use std::io::Read;
            let mut key = String::new();
            std::io::stdin()
                .read_to_string(&mut key)
                .context("read license key from stdin")?;
            let activated = license::activate(&key)?;
            eprintln!("{}", activated.tray_label());
            Ok(())
        }
        Some(other) => bail!(
            "unknown argument {other:?}; usage: matteshot [--once [--window <title-substring>] [--pick <1-7>] [--overlay] | --bench <title> | --overlay-bench [batched|sequential] | --record-window-test <title> [seconds] | --review-test <mp4> | --video-edit-test <mp4> | --video-speed-test <mp4> | --duration-test <mp4> | --welcome | --delay-test [seconds] | --license | --license-status | --activate-stdin | --take-printscreen | --restore-printscreen | --quit]"
        ),
        None => run_app(),
    };

    if let Err(e) = &result {
        // The CLI probes run from a console, so this is where their errors
        // belong. The resident has no console; its failure was shown above.
        eprintln!("error: {e:#}");
    }
    result
}

/// Flipped once the tray exists and the resident is usable. `run_app` can
/// still fail after that (a `?` on the message-loop side of Activate,
/// Deactivate, or the expired-license window), and that is a running app
/// stopping, not a startup that never happened — the dialog, the diagnostic
/// event, and the telemetry kind all say which.
static RESIDENT_READY: AtomicBool = AtomicBool::new(false);

/// The resident is a windows-subsystem process: nothing it prints is ever
/// seen. When `run_app` fails, the app used to just not be there — no tray
/// icon, no PrtScn, and nothing to say why. Show one dialog the user can act
/// on, and leave one diagnostic event and one telemetry failure behind it.
/// Every other mode (`--once`, probes, `--license`) is a console flow and
/// keeps stderr.
fn report_resident_failure(error: &anyhow::Error, was_running: bool) {
    let message = startup_failure_message(error);
    let (event, operation, title) = resident_failure_wording(was_running);
    diagnostics::log(&format!(
        "{event}: {}",
        without_paths(&format!("{error:#}"))
    ));
    telemetry::report_failure(operation, error);
    let title = HSTRING::from(title);
    let text = HSTRING::from(message.as_str());
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND | MB_TOPMOST,
        );
    }
}

/// (diagnostic event, telemetry operation, dialog title) for a resident that
/// failed before it was usable versus one that was running and stopped.
fn resident_failure_wording(was_running: bool) -> (&'static str, &'static str, &'static str) {
    if was_running {
        ("resident stopped", "resident", "Matteshot stopped unexpectedly")
    } else {
        ("resident startup failed", "startup", "Matteshot could not start")
    }
}

/// What the dialog says: a plain reason and the one thing to try, chosen from
/// the failure's own context. The technical detail follows, scrubbed of local
/// paths — the dialog is on the user's own screen, but its text gets pasted
/// into support mail as-is, so it carries no user names or folders.
fn startup_failure_message(error: &anyhow::Error) -> String {
    let detail = without_paths(&format!("{error:#}"));
    let lower = detail.to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|needle| lower.contains(needle));
    // Exact contexts, not loose words: "create resident mutex" is
    // CreateMutexW itself failing (a second copy takes a different, silent
    // path), and "create state mutex" is the license lock, which has nothing
    // to do with a stuck copy.
    let advice = if has(&["create resident mutex"]) {
        "Windows refused Matteshot's start-up lock. Sign out and back in, then start Matteshot again."
    } else if has(&["create tray window", "notify icon", "shell_notifyicon"]) {
        "Windows did not let Matteshot create its tray icon. Restart Windows Explorer, or sign out and back in, then start Matteshot again."
    } else if has(&["application data", "app data", "data directory", "create directory"]) {
        "Matteshot could not use its application data folder. Check that your AppData folder is writable, then start Matteshot again."
    } else if has(&["access is denied", "access denied", "permission"]) {
        "Windows denied Matteshot something it needs. Start Matteshot again; if that fails, try once as administrator to see the cause."
    } else {
        "Start Matteshot again. If this keeps happening, copy this message into an email to support@matteshot.app."
    };
    format!("{advice}\n\nDetail: {detail}\n\nThe diagnostics log has more (Settings > Diagnostics, or %LOCALAPPDATA%\\Matteshot\\matteshot.log).")
}

/// Drop anything shaped like a local file system path — a drive-letter path,
/// a UNC path — so a folder or user name never rides along in text that is
/// pasted into support mail or replayed into the shared support report.
/// Windows paths contain spaces ("C:\\Users\\John Doe\\…", "Program Files"),
/// so a space ends the path only when none of the next few words carries
/// another separator; over-scrubbing a word is fine, leaking a name is not.
fn without_paths(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let word_start = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
        let drive = word_start
            && i + 2 < bytes.len()
            && bytes[i].is_ascii_alphabetic()
            && bytes[i + 1] == b':'
            && (bytes[i + 2] == b'\\' || bytes[i + 2] == b'/');
        let unc = word_start && bytes[i..].starts_with(b"\\\\");
        if drive || unc {
            out.push_str("<path>");
            i += if drive { 3 } else { 2 };
            loop {
                match bytes.get(i) {
                    None | Some(b'"' | b'\'' | b',' | b';' | b'\n' | b'\r') => break,
                    // "x.txt: Access is denied" ends the path; the drive colon
                    // inside an extended path ("\\\\?\\C:\\Users\\…") does not.
                    Some(b':') if !separator_follows(bytes, i + 1) => break,
                    Some(b' ') if !path_continues_after_space(&bytes[i + 1..]) => break,
                    Some(_) => i += 1,
                }
            }
            continue;
        }
        // Advance one whole char, not one byte, so multibyte text survives.
        let ch = text[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// After a space inside a suspected path: does one of the next three words
/// (before any hard delimiter) carry a separator? "John Doe\\AppData" and
/// "Program Files (x86)\\Matteshot" continue; "shot.png was not found" stops.
fn separator_follows(bytes: &[u8], at: usize) -> bool {
    matches!(bytes.get(at), Some(b'\\' | b'/'))
}

fn path_continues_after_space(rest: &[u8]) -> bool {
    let mut words = 0;
    let mut in_word = false;
    for &byte in rest {
        match byte {
            // A colon here is either punctuation or the drive of a *new*
            // path ("… c.txt to D:\\…"); either way this one is over.
            b'"' | b'\'' | b',' | b';' | b':' | b'\n' | b'\r' => return false,
            b' ' => {
                if in_word {
                    words += 1;
                    in_word = false;
                    if words == 3 {
                        return false;
                    }
                }
            }
            b'\\' | b'/' => return true,
            _ => in_word = true,
        }
    }
    false
}

#[cfg(test)]
mod startup_failure_tests {
    use super::{resident_failure_wording, startup_failure_message, without_paths};

    fn injected(context: &'static str, root: &'static str) -> anyhow::Error {
        anyhow::anyhow!(root).context(context)
    }

    #[test]
    fn each_startup_failure_gets_its_own_actionable_advice() {
        // The three initializations that can fail before the tray exists.
        let mutex = injected("create resident mutex", "Access is denied. (0x80070005)");
        let tray = injected("create tray window", "Not enough quota (0x800705AD)");
        let app_data = injected(
            "Windows has no application data directory",
            "C:\\Users\\tyler\\AppData\\Roaming was not found",
        );

        let mutex_text = startup_failure_message(&mutex);
        assert!(mutex_text.contains("start-up lock"), "{mutex_text}");
        let tray_text = startup_failure_message(&tray);
        assert!(tray_text.contains("tray icon"), "{tray_text}");
        let app_data_text = startup_failure_message(&app_data);
        assert!(app_data_text.contains("AppData folder"), "{app_data_text}");

        // Every message names the way back in, and carries the detail.
        for text in [&mutex_text, &tray_text, &app_data_text] {
            assert!(text.contains("start Matteshot again"), "{text}");
            assert!(text.contains("Detail: "), "{text}");
        }
        // The default still tells the user what to do.
        let unknown = startup_failure_message(&anyhow::anyhow!("something odd"));
        assert!(unknown.contains("support@matteshot.app"), "{unknown}");
    }

    #[test]
    fn advice_matches_exact_contexts_not_loose_words() {
        // The license lock is a mutex too, but a stuck copy has nothing to
        // do with it: this is an access problem and says so.
        let state_lock = injected("create state mutex", "Access is denied. (0x80070005)");
        let text = startup_failure_message(&state_lock);
        assert!(!text.contains("start-up lock"), "{text}");
        assert!(!text.contains("Task Manager"), "{text}");
        assert!(text.contains("denied"), "{text}");

        // "config" alone is not the application data folder.
        let config = anyhow::anyhow!("invalid config value for capture_hotkey");
        let text = startup_failure_message(&config);
        assert!(!text.contains("AppData folder"), "{text}");

        // A module-handle failure is not a tray-icon failure.
        let module = injected("resolve module handle", "The specified module could not be found.");
        let text = startup_failure_message(&module);
        assert!(!text.contains("tray icon"), "{text}");
    }

    #[test]
    fn a_stopped_resident_and_a_failed_startup_are_worded_apart() {
        let (event, operation, title) = resident_failure_wording(false);
        assert_eq!(event, "resident startup failed");
        assert_eq!(operation, "startup");
        assert_eq!(title, "Matteshot could not start");
        let (event, operation, title) = resident_failure_wording(true);
        assert_eq!(event, "resident stopped");
        assert_eq!(operation, "resident");
        assert_eq!(title, "Matteshot stopped unexpectedly");
    }

    #[test]
    fn a_startup_failure_message_carries_no_local_paths() {
        let error = anyhow::anyhow!("open C:\\Users\\tyler\\AppData\\Local\\Matteshot\\state.json")
            .context("read \\\\server\\share\\config.json")
            .context("Windows has no application data directory");
        let text = startup_failure_message(&error);
        assert!(!text.contains("tyler"), "{text}");
        assert!(!text.contains("C:\\Users"), "{text}");
        assert!(!text.contains("\\\\server"), "{text}");
        assert!(text.contains("<path>"), "{text}");
    }

    #[test]
    fn path_scrubbing_leaves_ordinary_text_alone() {
        assert_eq!(without_paths("create resident mutex: Access is denied."), "create resident mutex: Access is denied.");
        assert_eq!(without_paths("ratio 3:4 stays"), "ratio 3:4 stays");
        assert_eq!(without_paths("(0x80070005) at 12:30"), "(0x80070005) at 12:30");
        assert_eq!(without_paths("saved to D:\\Captures\\shot.png, then failed"), "saved to <path>, then failed");
        assert_eq!(without_paths("share \\\\nas\\media\\clip.mp4 locked"), "share <path> locked");
        assert_eq!(without_paths("unicode ünïcode C:/x/y end"), "unicode ünïcode <path> end");
    }

    #[test]
    fn path_scrubbing_swallows_spaces_inside_a_path() {
        assert_eq!(
            without_paths("open C:\\Users\\John Doe\\AppData\\Local\\x.json was not found"),
            "open <path> was not found"
        );
        assert_eq!(
            without_paths("C:\\Program Files (x86)\\Matteshot\\matteshot.exe failed to start"),
            "<path> failed to start"
        );
        assert_eq!(
            without_paths("read C:\\Users\\Jane Q Public\\a.txt: Access is denied."),
            "read <path>: Access is denied."
        );
        assert_eq!(
            without_paths("share \\\\nas\\my share\\clip.mp4 locked"),
            "share <path> locked"
        );
        // Two paths in one sentence stay two paths.
        assert_eq!(
            without_paths("copy C:\\a b\\c.txt to D:\\d e\\f.txt, then retry"),
            "copy <path> to <path>, then retry"
        );
        // Extended-length and device paths carry the drive colon inside.
        assert_eq!(
            without_paths("open \\\\?\\C:\\Users\\John Doe\\x.json failed"),
            "open <path> failed"
        );
        assert_eq!(
            without_paths("open \\\\.\\C:\\Users\\tyler\\x.json: Access is denied."),
            "open <path>: Access is denied."
        );
    }
}

#[cfg(test)]
mod speed_export_min_duration_tests {
    use super::{duration_test_report, meets_speed_export_min_duration};

    #[test]
    fn duration_test_prints_integer_ticks_not_rounded_seconds() {
        // `{:.2}s` would print 0.9995s as 1.00s; the harness used that to
        // decide speed-export was ready while Rust still required 10_000_000.
        assert_eq!(duration_test_report(9_995_000), "duration_100ns: 9995000");
        assert!(!meets_speed_export_min_duration(9_995_000));
    }

    #[test]
    fn ci_short_fixture_is_below_the_floor() {
        assert!(!meets_speed_export_min_duration(6_000_000));
    }

    #[test]
    fn exactly_one_second_meets_the_floor() {
        assert!(meets_speed_export_min_duration(10_000_000));
    }

    #[test]
    fn just_under_one_second_is_below_the_floor() {
        assert!(!meets_speed_export_min_duration(9_999_999));
    }

    #[test]
    fn good_ci_clip_meets_the_floor() {
        assert!(meets_speed_export_min_duration(29_000_000));
    }

    #[test]
    fn zero_and_negative_are_below_the_floor() {
        assert!(!meets_speed_export_min_duration(0));
        assert!(!meets_speed_export_min_duration(-1));
    }
}
