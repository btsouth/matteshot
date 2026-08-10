//! Small, bounded, privacy-safe lifecycle log and support report.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use winreg::enums::HKEY_LOCAL_MACHINE;
use winreg::RegKey;

const MAX_LOG_BYTES: u64 = 512 * 1024;
const RECENT_LINES: usize = 60;
const LOG_MUTEX: &str = "Local\\Matteshot.Diagnostics.Log";

fn log_path() -> Option<PathBuf> {
    dirs::data_local_dir().map(|dir| dir.join("Matteshot").join("matteshot.log"))
}

fn clean_event(event: &str) -> String {
    event
        .chars()
        .map(|character| {
            if matches!(character, '\r' | '\n' | '\t') {
                ' '
            } else {
                character
            }
        })
        .take(500)
        .collect()
}

pub fn log(event: &str) {
    let Some(path) = log_path() else { return };
    let _guard = crate::state_lock::lock(LOG_MUTEX).ok();
    let Some(parent) = path.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    if std::fs::metadata(&path).is_ok_and(|metadata| metadata.len() >= MAX_LOG_BYTES) {
        let old = path.with_extension("log.old");
        let _ = std::fs::remove_file(&old);
        let _ = std::fs::rename(&path, old);
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let _ = writeln!(
        file,
        "{} {}",
        chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%:z"),
        clean_event(event)
    );
}

pub fn init() {
    log(&format!("app start version={}", env!("CARGO_PKG_VERSION")));
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|location| format!("{}:{}", location.file(), location.line()))
            .unwrap_or_else(|| "unknown".into());
        log(&format!("panic location={location}"));
        previous(info);
    }));
}

fn windows_version() -> String {
    let Ok(key) = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion")
    else {
        return "Windows (version unavailable)".into();
    };
    let product: String = key
        .get_value("ProductName")
        .unwrap_or_else(|_| "Windows".into());
    let display: String = key.get_value("DisplayVersion").unwrap_or_default();
    let build: String = key.get_value("CurrentBuild").unwrap_or_default();
    let ubr: u32 = key.get_value("UBR").unwrap_or_default();
    format!("{product} {display} build {build}.{ubr}")
}

fn recent_events() -> String {
    let Some(path) = log_path() else {
        return "unavailable".into();
    };
    let Ok(contents) = std::fs::read_to_string(path) else {
        return "none".into();
    };
    let lines: Vec<_> = contents.lines().collect();
    lines[lines.len().saturating_sub(RECENT_LINES)..].join("\r\n")
}

/// Contains no account name, machine name, license key, capture title, or
/// save path. It is safe to paste into a support request after reviewing it.
pub fn report() -> String {
    let config = crate::config::Config::load();
    let save_location = if config.save_dir.is_some() {
        "custom"
    } else {
        "default"
    };
    let video_location = if config.video_dir.is_some() {
        "custom"
    } else {
        "default"
    };
    let (downs, ups, saved) = crate::prtscn::hook_health();
    format!(
        "Matteshot diagnostics\r\n\
         Version: {}\r\n\
         OS: {}\r\n\
         License: {}\r\n\
         PrtScn preferred: {}\r\n\
         PrtScn owned: {}\r\n\
         PrtScn hook: {} down / {} up seen, {} presses saved from a lost key-up\r\n\
         Capture folder: {}\r\n\
         Video folder: {}\r\n\
         Recording audio: {}\r\n\
         Recording frame rate: {} FPS\r\n\
         Recording GIF: {}\r\n\
         Export scale: {}x\r\n\r\n\
         Recent lifecycle events:\r\n{}",
        env!("CARGO_PKG_VERSION"),
        windows_version(),
        crate::license::status().tray_label(),
        crate::prtscn::preferred(),
        crate::prtscn::owns_key(),
        downs,
        ups,
        saved,
        save_location,
        video_location,
        config.record_audio,
        config.record_fps(),
        config.record_gif,
        config.export_scale,
        recent_events()
    )
}

pub fn copy_report() -> Result<()> {
    crate::output::text_to_clipboard(&report()).context("copy diagnostics")?;
    log("diagnostics copied");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_cleanup_keeps_the_log_one_line() {
        assert_eq!(clean_event("one\r\ntwo\tthree"), "one  two three");
    }

    #[test]
    fn report_labels_locations_without_exposing_paths() {
        let report = report();
        assert!(report.contains("Matteshot diagnostics"));
        assert!(report.contains("Capture folder: "));
        assert!(report.contains("Video folder: "));
    }
}
