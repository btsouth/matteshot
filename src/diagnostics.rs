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

/// Classify a failure into a fixed label. The label is all that gets logged:
/// error text can carry a window title or a path, and the log is replayed
/// into the support report people paste into public issues.
pub fn failure_kind(error: &anyhow::Error) -> &'static str {
    let text = format!("{error:#}").to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|needle| text.contains(needle));

    if has(&["cancel", "aborted"]) {
        "cancelled"
    } else if has(&["access is denied", "access denied", "permission", "privilege", "0x80070005"]) {
        "access_denied"
    } else if has(&["signature", "checksum", "sha-256", "sha256", "not trusted"]) {
        "verification"
    } else if has(&["timed out", "timeout"]) {
        "timeout"
    } else if has(&["winhttp", "connect", "network", "dns", "http ", "resolve"]) {
        "network"
    } else if has(&["clipboard"]) {
        "clipboard"
    } else if has(&["media foundation", "encoder", "aac", "h.264", "mfstartup", "no video stream"]) {
        "encoder"
    } else if has(&["graphics capture", "wgc", "dwm", "d3d", "direct3d", "adapter", "surface"]) {
        "capture_unavailable"
    // Before the disk bucket: "no such file or directory" contains both "file"
    // and "directory", so checking disk first would swallow every not-found
    // and leave that bucket permanently empty.
    } else if has(&[
        "not found",
        "not be found",
        "does not exist",
        "missing",
        "no such",
        "cannot find",
    ]) {
        "not_found"
    } else if has(&["disk", "space", "write", "create ", "open ", "file", "directory", "io error"]) {
        "disk"
    } else {
        "other"
    }
}

/// Note in the local log that `operation` failed, with a classified reason
/// and nothing else. Nothing leaves the machine.
pub fn log_failure(operation: &'static str, error: &anyhow::Error) {
    log(&format!(
        "failure operation={operation} kind={}",
        failure_kind(error)
    ));
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

/// Contains no account name, machine name, capture title, or save path. It is
/// safe to paste into a public issue after reviewing it.
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
         PrtScn preferred: {}\r\n\
         PrtScn owned: {}\r\n\
         PrtScn hook: {} down / {} up seen, {} presses saved from a lost key-up\r\n\
         Capture folder: {}\r\n\
         Video folder: {}\r\n\
         Recording audio: {}\r\n\
         Recording frame rate: {}\r\n\
         Recording GIF: {}\r\n\
         Export scale: {}x\r\n\r\n\
         Recent lifecycle events:\r\n{}",
        env!("CARGO_PKG_VERSION"),
        windows_version(),
        crate::prtscn::preferred(),
        crate::prtscn::owns_key(),
        downs,
        ups,
        saved,
        save_location,
        video_location,
        config.record_audio,
        recording_frame_rate(&config),
        config.record_gif,
        config.export_scale,
        recent_events()
    )
}

fn recording_frame_rate(config: &crate::config::Config) -> String {
    let safe = config.record_fps();
    if safe == config.record_fps {
        format!("{safe} FPS")
    } else {
        format!("{safe} FPS (raw {})", config.record_fps)
    }
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
    fn invalid_recording_rate_remains_visible_in_diagnostics() {
        let mut config = crate::config::Config::default();
        assert_eq!(recording_frame_rate(&config), "30 FPS");
        config.record_fps = 60;
        assert_eq!(recording_frame_rate(&config), "60 FPS");
        config.record_fps = 144;
        assert_eq!(recording_frame_rate(&config), "30 FPS (raw 144)");
    }

    #[test]
    fn report_labels_locations_without_exposing_paths() {
        let report = report();
        assert!(report.contains("Matteshot diagnostics"));
        assert!(report.contains("Capture folder: "));
        assert!(report.contains("Video folder: "));
    }

    #[test]
    fn report_carries_no_entitlement_line() {
        let report = report();
        let header = report
            .split("Recent lifecycle events:")
            .next()
            .expect("report has a header");
        assert!(!header.contains("License:"));
    }

    #[test]
    fn failures_classify_into_their_buckets() {
        let cases = [
            ("access is denied", "access_denied"),
            ("installer signature is not valid", "verification"),
            ("published checksum did not match", "verification"),
            ("the operation timed out", "timeout"),
            ("winhttp send request failed", "network"),
            ("open the clipboard", "clipboard"),
            ("media foundation could not start", "encoder"),
            ("graphics capture item was 0x0", "capture_unavailable"),
            ("user cancelled the export", "cancelled"),
            ("no such file or directory", "not_found"),
            ("the installer could not be found", "not_found"),
            ("save the png: disk full", "disk"),
            ("wobbling gizmo misaligned", "other"),
        ];
        for (message, expected) in cases {
            assert_eq!(
                failure_kind(&anyhow::anyhow!("{message}")),
                expected,
                "for {message:?}"
            );
        }
    }

    /// Classification reads the whole chain, so context added on the way up
    /// still lands in the right bucket.
    #[test]
    fn a_wrapped_error_is_classified_by_its_cause() {
        let wrapped = Err::<(), _>(anyhow::anyhow!("access is denied"))
            .context("save the finished matte")
            .unwrap_err();
        assert_eq!(failure_kind(&wrapped), "access_denied");
    }

    #[test]
    fn no_error_detail_ever_reaches_a_logged_kind() {
        const ALLOWED: &[&str] = &[
            "cancelled",
            "access_denied",
            "verification",
            "timeout",
            "network",
            "clipboard",
            "encoder",
            "capture_unavailable",
            "not_found",
            "disk",
            "other",
        ];
        for error in [
            anyhow::anyhow!(r"create C:\Users\someone\Pictures\Matteshot: access is denied"),
            anyhow::anyhow!("capture failed for window 'Quarterly Results - Confidential.xlsx'"),
            anyhow::anyhow!(r"save D:\clients\acme\nda-draft.png: disk full"),
            anyhow::anyhow!("something entirely new and unclassified"),
        ] {
            let kind = failure_kind(&error);
            assert!(ALLOWED.contains(&kind), "{kind:?} is not a bounded label");
        }
    }
}
