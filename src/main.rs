#![windows_subsystem = "windows"]

mod annotate;
mod capture;
mod compose;
mod config;
mod diagnostics;
mod icon;
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
mod spike;
mod style;
mod theme;
mod tray;
mod tweak;
mod update;
mod video_edit;
mod window;

use anyhow::{bail, Context, Result};
use image::RgbaImage;
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::{MonitorFromWindow, HMONITOR, MONITOR_DEFAULTTOPRIMARY};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, MOD_ALT, MOD_CONTROL,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, IsWindow, MessageBoxW, PostMessageW, IDYES,
    MB_ICONERROR, MB_ICONWARNING, MB_OK, MB_YESNO, MSG, WM_CLOSE, WM_HOTKEY,
};

use crate::config::Config;
use crate::picker::PickAction;

const HOTKEY_ID: i32 = 1;
pub const HOTKEY_ID_PRTSCN: i32 = 2;
const VK_S: u32 = 0x53;

/// Max dimension of the downscaled capture used for picker previews.
const PREVIEW_MAX: u32 = 480;

/// Ask every Matteshot surface to close on its own UI thread, then stop the
/// resident. The installer uses this instead of taskkill so recordings and
/// exports get their normal cancellation/finalization path.
fn request_graceful_shutdown() -> Result<()> {
    fn close_all(class_name: &str, label: &str) -> Result<()> {
        loop {
            let hwnd = match crate::window::find_by_class(class_name) {
                Some(hwnd) => hwnd,
                None => return Ok(()),
            };
            unsafe {
                PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0))
                    .with_context(|| format!("ask {label} to close"))?;
            }
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
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
    close_all("matteshot_pin", "a pinned capture")?;
    close_all("matteshot_tray", "Matteshot")?;
    Ok(())
}

enum Source {
    Window(HWND),
    Image(RgbaImage),
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
    let raw = match source {
        Source::Window(hwnd) => {
            eprintln!("capturing: {}", window::title_of(hwnd));
            capture::capture_window(hwnd).context("capture failed")?
        }
        Source::Image(img) => {
            eprintln!("capturing: region {}x{}", img.width(), img.height());
            img
        }
    };
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
                std::thread::spawn(move || {
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
                    if let Ok(path) = output::save_png(&styled, style.name, &dir) {
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
                    }
                });
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

    let (chosen, open_editor) = match action {
        PickAction::Choose(i) => (i, false),
        PickAction::Edit(i) => (i, true),
        PickAction::Cancel => {
            // Esc keeps the auto-copy: the no-touch flow — PrtScn, select,
            // Esc, paste.
            eprintln!("cancelled (auto-copy stands)");
            return Ok(());
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
            return ocr::copy_text(&raw);
        }
        PickAction::Tweak(i) => {
            // The tweak editor owns every later Copy action. Synchronize with
            // the background auto-copy before opening it so a slow clipboard
            // write cannot overwrite the user's edited result afterward.
            if let Some(p) = cancel_auto() {
                let _ = std::fs::remove_file(p);
            }
            return match tweak::run(raw, styles, i, monitor)? {
                Some((overlay::Selection::Window(h), mon)) => shoot(Source::Window(h), mon, None),
                Some((overlay::Selection::Region(img), mon)) => {
                    shoot(Source::Image(img), mon, None)
                }
                Some((overlay::Selection::RecordWindow(h), _)) => {
                    record::session(record::Target::window(h), cfg.record_gif)
                }
                Some((overlay::Selection::RecordRegion(r, m), _)) => {
                    record::session(record::Target::region(r, m), cfg.record_gif)
                }
                Some((overlay::Selection::ScrollWindow(h, anchor), mon)) => {
                    let img = scroll::capture(scroll::Target::Window(h, anchor))?;
                    shoot(Source::Image(img), mon, None)
                }
                Some((overlay::Selection::ScrollRegion(r, m, anchor), mon)) => {
                    let img = scroll::capture(scroll::Target::Region(r, m, anchor))?;
                    shoot(Source::Image(img), mon, None)
                }
                None => Ok(()),
            };
        }
        PickAction::Reshoot(sel, mon) => {
            // PrtScn mid-pick: the user re-snipped; replace the pending shot
            // (and its auto-copy — the new capture makes its own).
            if let Some(p) = cancel_auto() {
                let _ = std::fs::remove_file(p);
            }
            eprintln!("reshoot: replacing the pending capture");
            return match sel {
                overlay::Selection::Window(hwnd) => shoot(Source::Window(hwnd), mon, None),
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
            };
        }
    };

    // If they confirmed the preselected matte and the auto-copy already
    // landed, the work is done — don't export the same thing twice.
    let auto_path = cancel_auto();
    if chosen == preselect {
        if let Some(path) = &auto_path {
            Config::update(|cfg| cfg.last_style = chosen);
            if open_editor {
                output::open_in_editor(path);
            }
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
    let path = output::save_png(&styled, styles[chosen].name, &cfg.save_dir())?;
    output::to_clipboard(&styled, Some(&path)).context("clipboard failed")?;
    // A different pick supersedes the auto-copied file.
    if let Some(old) = auto_path {
        if old != path {
            let _ = std::fs::remove_file(old);
        }
    }
    Config::update(|cfg| cfg.last_style = chosen);
    if open_editor {
        output::open_in_editor(&path);
    }
    eprintln!(
        "done [{}]: {}x{} -> clipboard + {}",
        styles[chosen].name,
        styled.width(),
        styled.height(),
        path.display()
    );
    Ok(())
}

/// PrtScn / tray-click flow: freeze-frame overlay, then the picker.
fn shoot_overlay() -> Result<()> {
    match overlay::select()? {
        Some((overlay::Selection::Window(hwnd), mon)) => shoot(Source::Window(hwnd), mon, None),
        Some((overlay::Selection::Region(img), mon)) => shoot(Source::Image(img), mon, None),
        Some((overlay::Selection::RecordWindow(hwnd), _)) => {
            let result =
                record::session(record::Target::window(hwnd), Config::load().record_gif);
            if result.is_ok() {
                license::record_successful_capture();
            }
            result
        }
        Some((overlay::Selection::RecordRegion(r, mon), _)) => {
            let result =
                record::session(record::Target::region(r, mon), Config::load().record_gif);
            if result.is_ok() {
                license::record_successful_capture();
            }
            result
        }
        Some((overlay::Selection::ScrollWindow(h, anchor), mon)) => {
            let img = scroll::capture(scroll::Target::Window(h, anchor))?;
            shoot(Source::Image(img), mon, None)
        }
        Some((overlay::Selection::ScrollRegion(r, m, anchor), mon)) => {
            let img = scroll::capture(scroll::Target::Region(r, m, anchor))?;
            shoot(Source::Image(img), mon, None)
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
    shoot(Source::Window(fg), mon, None)
}

fn enable_capture_hotkeys() -> Result<bool> {
    unsafe {
        RegisterHotKey(None, HOTKEY_ID, MOD_CONTROL | MOD_ALT, VK_S)
            .context("Ctrl+Alt+S is already taken by another app")?;
    }
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
    let Some(_resident_guard) =
        state_lock::try_process_mutex("Local\\Matteshot.Resident.Process")?
    else {
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
    let cleaned = output::cleanup_stale_video_partials(&Config::load().video_dir());
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

    let mut tray = tray::Tray::create()?;
    eprintln!(
        "matteshot: ready — PrtScn {} | Ctrl+Alt+S = active window",
        if prtscn_ours { "= capture overlay" } else { "not held (see tray menu)" }
    );
    diagnostics::log("resident ready");

    // First run: a single balloon so the user knows where the app lives.
    let cfg = Config::load();
    if !cfg.onboarded && initial_license.can_capture() {
        tray.notify(
            "Matteshot is ready",
            "Press PrtScn to capture. Right-click the tray icon for settings.",
        );
        Config::update(|cfg| cfg.onboarded = true);
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
                    tray::Action::Settings => settings::open(),
                    tray::Action::ToggleAutostart => {
                        tray::set_autostart(!tray::autostart_enabled())
                    }
                    tray::Action::TogglePrtscn => {
                        if license::status().can_capture() {
                            let enabled = !prtscn::preferred();
                            prtscn::set_preferred(enabled);
                            Config::update(|cfg| cfg.capture_prtscn = enabled);
                            if enabled {
                                let _ = prtscn::take(HOTKEY_ID_PRTSCN);
                            } else {
                                prtscn::release(HOTKEY_ID_PRTSCN);
                            }
                        }
                        Ok(())
                    }
                    tray::Action::OpenUpdate => {
                        if let Some(url) = tray.update_url() {
                            output::open_url(&url);
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
                    tray::Action::Deactivate => {
                        let answer = MessageBoxW(
                            None,
                            w!("Deactivate Matteshot on this PC? This frees one of your three device slots."),
                            w!("Matteshot"),
                            MB_YESNO | MB_ICONWARNING,
                        );
                        if answer == IDYES {
                            license::deactivate()?;
                            settings::refresh();
                            if !license::status().can_capture() {
                                disable_capture_hotkeys(true);
                                hotkeys_active = false;
                            }
                        }
                        Ok(())
                    }
                    tray::Action::Diagnostics => {
                        diagnostics::copy_report()?;
                        tray.notify("Matteshot diagnostics", "Copied a privacy-safe support report.");
                        Ok(())
                    }
                    tray::Action::Quit => {
                        diagnostics::log("resident quit requested");
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
                tweak::run(raw, styles, 0, mon).map(|_| ())
            } else {
                let mon = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTOPRIMARY) };
                shoot(Source::Window(hwnd), mon, pick_override)
            }
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
            // Auto-stop: close the pill from a timer thread.
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(secs));
                if let Some(h) = crate::window::find_by_class("matteshot_recui") {
                    unsafe {
                        let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                            h,
                            windows::Win32::UI::WindowsAndMessaging::WM_CLOSE,
                            windows::Win32::Foundation::WPARAM(0),
                            windows::Win32::Foundation::LPARAM(0),
                        );
                    }
                }
            });
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
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(secs));
                if let Some(controls) = crate::window::find_by_class("matteshot_recui") {
                    unsafe {
                        let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                            controls,
                            windows::Win32::UI::WindowsAndMessaging::WM_CLOSE,
                            windows::Win32::Foundation::WPARAM(0),
                            windows::Win32::Foundation::LPARAM(0),
                        );
                    }
                }
            });
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
            "unknown argument {other:?}; usage: matteshot [--once [--window <title-substring>] [--pick <1-7>] [--overlay] | --record-window-test <title> [seconds] | --review-test <mp4> | --video-edit-test <mp4> | --license | --license-status | --activate-stdin | --take-printscreen | --restore-printscreen | --quit]"
        ),
        None => run_app(),
    };

    if let Err(e) = &result {
        eprintln!("error: {e:#}");
    }
    result
}
