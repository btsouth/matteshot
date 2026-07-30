#![windows_subsystem = "windows"]

mod annotate;
mod capture;
mod compose;
mod config;
mod ocr;
mod pin;
mod audio;
mod recdone;
mod record;
mod recui;
mod scroll;
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
mod window;

use anyhow::{bail, Context, Result};
use image::RgbaImage;
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{MonitorFromWindow, HMONITOR, MONITOR_DEFAULTTOPRIMARY};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, MOD_ALT, MOD_CONTROL};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, MessageBoxW, MB_ICONERROR, MB_OK, MSG, WM_HOTKEY,
};

use crate::config::Config;
use crate::picker::PickAction;

const HOTKEY_ID: i32 = 1;
pub const HOTKEY_ID_PRTSCN: i32 = 2;
const VK_S: u32 = 0x53;

/// Max dimension of the downscaled capture used for picker previews.
const PREVIEW_MAX: u32 = 480;

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
    let mut cfg = Config::load();
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
    let styles = style::variants(&raw);
    let names: Vec<&'static str> = styles.iter().map(|s| s.name).collect();

    let action = match pick_override {
        Some(i) => {
            if i >= styles.len() {
                bail!("--pick must be 1..={}", styles.len());
            }
            PickAction::Choose(i)
        }
        None => {
            let previews = previews_for(&raw, &styles);
            picker::pick(&previews, &names, monitor, cfg.last_style)?
        }
    };

    let (chosen, open_editor) = match action {
        PickAction::Choose(i) => (i, false),
        PickAction::Edit(i) => (i, true),
        PickAction::Cancel => {
            eprintln!("cancelled");
            return Ok(());
        }
        PickAction::Pin => {
            return pin::show(raw, monitor);
        }
        PickAction::CopyText => {
            return ocr::copy_text(&raw);
        }
        PickAction::Tweak(i) => {
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
                Some((overlay::Selection::ScrollWindow(h), mon)) => {
                    let img = scroll::capture(scroll::Target::Window(h))?;
                    shoot(Source::Image(img), mon, None)
                }
                Some((overlay::Selection::ScrollRegion(r, m), mon)) => {
                    let img = scroll::capture(scroll::Target::Region(r, m))?;
                    shoot(Source::Image(img), mon, None)
                }
                None => Ok(()),
            };
        }
        PickAction::Reshoot(sel, mon) => {
            // PrtScn mid-pick: the user re-snipped; replace the pending shot.
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
                overlay::Selection::ScrollWindow(h) => {
                    let img = scroll::capture(scroll::Target::Window(h))?;
                    shoot(Source::Image(img), mon, None)
                }
                overlay::Selection::ScrollRegion(r, m) => {
                    let img = scroll::capture(scroll::Target::Region(r, m))?;
                    shoot(Source::Image(img), mon, None)
                }
            };
        }
    };

    let styled = compose::export(&raw, &styles[chosen], 0.10, None, cfg.export_scale);
    let path = output::save_png(&styled, styles[chosen].name, &cfg.save_dir())?;
    output::to_clipboard(&styled, Some(&path)).context("clipboard failed")?;
    cfg.last_style = chosen;
    cfg.save();
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
            record::session(record::Target::window(hwnd), Config::load().record_gif)
        }
        Some((overlay::Selection::RecordRegion(r, mon), _)) => {
            record::session(record::Target::region(r, mon), Config::load().record_gif)
        }
        Some((overlay::Selection::ScrollWindow(h), mon)) => {
            let img = scroll::capture(scroll::Target::Window(h))?;
            shoot(Source::Image(img), mon, None)
        }
        Some((overlay::Selection::ScrollRegion(r, m), mon)) => {
            let img = scroll::capture(scroll::Target::Region(r, m))?;
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
    let fg = window::foreground();
    let mon = unsafe { MonitorFromWindow(fg, MONITOR_DEFAULTTOPRIMARY) };
    shoot(Source::Window(fg), mon, None)
}

fn run_app() -> Result<()> {
    capture::warmup();
    unsafe {
        RegisterHotKey(None, HOTKEY_ID, MOD_CONTROL | MOD_ALT, VK_S)
            .context("Ctrl+Alt+S is already taken by another app")?;
    }

    let prtscn_ours = matches!(
        prtscn::acquire(HOTKEY_ID_PRTSCN, true),
        prtscn::Acquire::Taken | prtscn::Acquire::TakenAfterToggle
    );

    let mut tray = tray::Tray::create()?;
    eprintln!(
        "matteshot: ready — PrtScn {} | Ctrl+Alt+S = active window",
        if prtscn_ours { "= capture overlay" } else { "not held (see tray menu)" }
    );

    // First run: a single balloon so the user knows where the app lives.
    let mut cfg = Config::load();
    if !cfg.onboarded {
        tray.notify(
            "Matteshot is ready",
            "Press PrtScn to capture. Right-click the tray icon for settings.",
        );
        cfg.onboarded = true;
        cfg.save();
    }

    let mut msg = MSG::default();
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if msg.message == WM_HOTKEY {
                let result = match msg.wParam.0 as i32 {
                    HOTKEY_ID_PRTSCN => shoot_overlay(),
                    HOTKEY_ID => shoot_active(),
                    _ => Ok(()),
                };
                if let Err(e) = result {
                    eprintln!("error: {e:#}");
                    error_box(&format!("Capture failed: {e:#}"));
                }
            }
            DispatchMessageW(&msg);

            if let Some(action) = tray.take_action() {
                let result = match action {
                    tray::Action::Capture => shoot_overlay(),
                    tray::Action::CaptureActive => shoot_active(),
                    tray::Action::OpenFolder => {
                        output::open_folder(&Config::load().save_dir());
                        Ok(())
                    }
                    tray::Action::Settings => {
                        settings::open();
                        Ok(())
                    }
                    tray::Action::ToggleAutostart => {
                        tray::set_autostart(!tray::autostart_enabled())
                    }
                    tray::Action::TogglePrtscn => {
                        if prtscn::snipping_owns_prtscn() {
                            let _ = prtscn::take(HOTKEY_ID_PRTSCN);
                        } else {
                            prtscn::release(HOTKEY_ID_PRTSCN);
                        }
                        Ok(())
                    }
                    tray::Action::Quit => {
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

    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        // One-shot mode, mainly for testing: capture a window by title substring
        // (or the current foreground window) and exit. `--pick N` skips the
        // picker UI. `--overlay` runs the freeze-frame overlay instead.
        Some("--once") => {
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
                let styles = style::variants(&raw);
                tweak::run(raw, styles, 0, mon).map(|_| ())
            } else {
                let mon = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTOPRIMARY) };
                shoot(Source::Window(hwnd), mon, pick_override)
            }
        }
        // Warm-path capture benchmark: same window three times in-process.
        Some("--bench") => {
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
                let p = std::env::temp_dir().join("matteshot-bench.png");
                img.save(&p)?;
                eprintln!("bench: raw capture saved to {}", p.display());
            }
            Ok(())
        }
        // Marketing/site asset generator: capture a window and export every
        // matte style as a PNG into a directory.
        Some("--assets") => {
            let needle = args.get(1).context("--assets <title substr> <outdir>")?;
            let outdir = std::path::PathBuf::from(args.get(2).context("--assets <title substr> <outdir>")?);
            std::fs::create_dir_all(&outdir)?;
            let hwnd = window::find_by_title(needle)
                .with_context(|| format!("no visible window matching {needle:?}"))?;
            let raw = capture::capture_window(hwnd).context("capture failed")?;
            for s in style::variants(&raw) {
                let img = compose::export(&raw, &s, 0.10, None, 2);
                let p = outdir.join(format!("matte-{}.png", s.name.to_lowercase()));
                img.save(&p)?;
                eprintln!("{} {}x{}", p.display(), img.width(), img.height());
            }
            raw.save(outdir.join("raw.png"))?;
            Ok(())
        }
        // Record the primary monitor region for N seconds (testing).
        Some("--record-test") => {
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
                unsafe {
                    let h = windows::Win32::UI::WindowsAndMessaging::FindWindowW(
                        w!("matteshot_recui"),
                        None,
                    );
                    if let Ok(h) = h {
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
        // Scroll-capture a window by title, save raw (testing).
        Some("--scroll-test") => {
            let needle = args.get(1).context("--scroll-test <title>")?;
            let hwnd = window::find_by_title(needle)
                .with_context(|| format!("no visible window matching {needle:?}"))?;
            let img = scroll::capture(scroll::Target::Window(hwnd))?;
            let p = std::env::temp_dir().join("matteshot-scroll.png");
            img.save(&p)?;
            eprintln!("saved {}x{} -> {}", img.width(), img.height(), p.display());
            Ok(())
        }
        // Trim probe: cut [start,end] seconds from an mp4 (testing).
        Some("--trim-test") => {
            let src = std::path::PathBuf::from(args.get(1).context("--trim-test <file> <a> <b>")?);
            let a: f64 = args.get(2).map(|s| s.parse().unwrap_or(1.0)).unwrap_or(1.0);
            let b: f64 = args.get(3).map(|s| s.parse().unwrap_or(3.0)).unwrap_or(3.0);
            let probe = trim::probe(&src, 6, 54)?;
            eprintln!(
                "probe: {:.2}s, {} thumbs",
                probe.duration_100ns as f64 / 1e7,
                probe.thumbs.len()
            );
            let dst = src.with_extension("trim.mp4");
            trim::cut(&src, &dst, (a * 1e7) as i64, (b * 1e7) as i64)?;
            eprintln!("trimmed -> {}", dst.display());
            Ok(())
        }
        // OCR probe: recognize and print (no clipboard) — testing.
        Some("--ocr") => {
            let needle = args.get(1).context("--ocr needs a title substring")?;
            let hwnd = window::find_by_title(needle)
                .with_context(|| format!("no visible window matching {needle:?}"))?;
            let img = capture::capture_window(hwnd)?;
            let text = ocr::recognize(&img)?;
            eprintln!("--- {} chars ---", text.len());
            for line in text.lines().take(12) {
                eprintln!("{line}");
            }
            Ok(())
        }
        // Render sample annotations onto a capture and save raw (testing).
        Some("--annotate-demo") => {
            let needle = args.get(1).context("--annotate-demo needs a title substring")?;
            let hwnd = window::find_by_title(needle)
                .with_context(|| format!("no visible window matching {needle:?}"))?;
            let mut img = capture::capture_window(hwnd)?;
            let (w, h) = (img.width() as f32, img.height() as f32);
            let anns = vec![
                annotate::Annotation {
                    shape: annotate::Shape::Arrow {
                        from: (w * 0.15, h * 0.75),
                        to: (w * 0.45, h * 0.4),
                    },
                    color: 0,
                    size: 1.0,
                },
                annotate::Annotation {
                    shape: annotate::Shape::Rect {
                        a: (w * 0.55, h * 0.3),
                        b: (w * 0.9, h * 0.5),
                    },
                    color: 2,
                    size: 1.0,
                },
                annotate::Annotation {
                    shape: annotate::Shape::Blur {
                        a: (w * 0.55, h * 0.6),
                        b: (w * 0.9, h * 0.75),
                    },
                    color: 0,
                    size: 1.0,
                },
                annotate::Annotation {
                    shape: annotate::Shape::Text {
                        pos: (w * 0.12, h * 0.82),
                        text: "the bug is here".into(),
                    },
                    color: 1,
                    size: 1.0,
                },
                annotate::Annotation {
                    shape: annotate::Shape::Line {
                        from: (w * 0.1, h * 0.12),
                        to: (w * 0.45, h * 0.12),
                    },
                    color: 3,
                    size: 1.0,
                },
                annotate::Annotation {
                    shape: annotate::Shape::Ellipse {
                        a: (w * 0.52, h * 0.08),
                        b: (w * 0.95, h * 0.24),
                    },
                    color: 1,
                    size: 1.0,
                },
                annotate::Annotation {
                    shape: annotate::Shape::Highlight {
                        a: (w * 0.08, h * 0.28),
                        b: (w * 0.45, h * 0.36),
                    },
                    color: 1,
                    size: 1.0,
                },
                annotate::Annotation {
                    shape: annotate::Shape::Counter { pos: (w * 0.2, h * 0.55), n: 1 },
                    color: 0,
                    size: 1.0,
                },
                annotate::Annotation {
                    shape: annotate::Shape::Counter { pos: (w * 0.5, h * 0.62), n: 2 },
                    color: 0,
                    size: 1.0,
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
            settings::open();
            let mut msg = MSG::default();
            unsafe {
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    let _ = windows::Win32::UI::WindowsAndMessaging::TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            Ok(())
        }
        Some("--spike-dpi") => {
            let needle = args.get(1).context("--spike-dpi needs a window title substring")?;
            let hwnd = window::find_by_title(needle)
                .with_context(|| format!("no visible window matching {needle:?}"))?;
            spike::run(hwnd, needle)
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
        Some(other) => bail!(
            "unknown argument {other:?}; usage: matteshot [--once [--window <title-substring>] [--pick <1-6>] [--overlay] | --take-printscreen | --restore-printscreen]"
        ),
        None => run_app(),
    };

    if let Err(e) = &result {
        eprintln!("error: {e:#}");
    }
    result
}
