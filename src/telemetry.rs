//! Anonymous usage telemetry, sent to PostHog.
//!
//! Opt-out by default: `Config::telemetry` defaults to true and the Settings
//! window can switch it off. Only event names, the anonymous `device_id()`,
//! the app version, and the Windows build are ever transmitted. Screenshots,
//! OCR text, file names, and paths are never part of an event.
//!
//! `matteshot_failure` additionally carries an `operation` and a `kind`, both
//! drawn from fixed sets. Error messages are never sent: `failure_kind`
//! classifies into `&'static str`, so there is no route by which a path or a
//! window title in an error context could reach an event.
//!
//! Anything added here has to be reflected in the privacy policy at
//! matteshot.app/privacy, which lists the events by name.
//!
//! Every event is tagged `product: "matteshot"` and named with a
//! `matteshot_` prefix so it stays isolated inside the shared PostHog project
//! and the dedicated dashboard can filter on it.

use anyhow::{Context, Result};
use serde::Serialize;
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Networking::WinHttp::{
    WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders,
    WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetTimeouts,
    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE, WINHTTP_QUERY_FLAG_NUMBER,
    WINHTTP_QUERY_STATUS_CODE,
};

const POSTHOG_HOST: &str = "us.i.posthog.com";
const POSTHOG_PATH: &str = "/capture/";
const POSTHOG_API_KEY: &str = "phc_piwT9huE46Hn8gZxs9X4SvjAHzgQVZGT9QipDWSq7cUx";
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

#[derive(Serialize)]
struct CaptureEvent<'a> {
    api_key: &'a str,
    event: &'a str,
    distinct_id: String,
    properties: serde_json::Map<String, serde_json::Value>,
}

/// Whether telemetry may run at all this process. Read once so a mid-run
/// settings flip only affects the next event, not the current burst.
static TELEMETRY_ENABLED: AtomicBool = AtomicBool::new(false);

pub fn set_enabled(enabled: bool) {
    TELEMETRY_ENABLED.store(enabled, Ordering::Relaxed);
}

/// Reload the toggle from config at launch.
pub fn init() {
    set_enabled(crate::config::Config::load().telemetry);
}

/// Fire-and-forget a single event on a background thread. Never blocks the UI;
/// a slow or unreachable network only delays the spawned thread's exit.
pub fn report(event: &str) {
    report_with(event, &[]);
}

/// Bounded, stable reason an operation failed.
///
/// Deliberately not the error message. Contexts in this codebase carry file
/// paths, folder names, and window titles, none of which may leave the machine,
/// and a future context could add anything. Returning `&'static str` is the
/// guarantee: there is no path by which caller text reaches an event, and an
/// unrecognised failure reports `other` rather than smuggling its message out.
fn failure_kind(error: &anyhow::Error) -> &'static str {
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
    } else if has(&["disk", "space", "write", "create ", "open ", "file", "directory", "io error"]) {
        "disk"
    } else if has(&["not found", "missing", "no such"]) {
        "not_found"
    } else {
        "other"
    }
}

/// Record that `operation` failed, with a classified reason and nothing else.
///
/// Success events tell us what people use; without this we learn nothing about
/// what breaks on machines we cannot reproduce, which is most of them.
pub fn report_failure(operation: &'static str, error: &anyhow::Error) {
    report_with(
        "matteshot_failure",
        &[
            ("operation", serde_json::Value::String(operation.into())),
            (
                "kind",
                serde_json::Value::String(failure_kind(error).into()),
            ),
        ],
    );
}

pub fn report_with(event: &str, properties: &[(&str, serde_json::Value)]) {
    if !TELEMETRY_ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let event = event.to_owned();
    let properties: Vec<(String, serde_json::Value)> = properties
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.clone()))
        .collect();
    std::thread::spawn(move || {
        let _ = send_event(&event, &properties);
    });
}

fn send_event(event: &str, properties: &[(String, serde_json::Value)]) -> Result<()> {
    let mut props = serde_json::Map::new();
    props.insert("product".into(), serde_json::Value::String("matteshot".into()));
    props.insert(
        "app_version".into(),
        serde_json::Value::String(env!("CARGO_PKG_VERSION").into()),
    );
    props.insert("os".into(), serde_json::Value::String("windows".into()));
    props.insert(
        "os_build".into(),
        serde_json::Value::String(windows_build_number()),
    );
    props.insert(
        "$lib".into(),
        serde_json::Value::String("matteshot-rust".into()),
    );
    for (key, value) in properties {
        props.insert(key.clone(), value.clone());
    }

    let payload = CaptureEvent {
        api_key: POSTHOG_API_KEY,
        event,
        distinct_id: crate::license::device_id(),
        properties: props,
    };
    let body = serde_json::to_vec(&payload).context("serialize telemetry event")?;
    let (_status, _response) = post_json(&body)?;
    Ok(())
}

/// Windows build number (e.g. 22631) from the registry, so usage can be split
/// between 10 and 11 without sending anything identifying.
fn windows_build_number() -> String {
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ};
    use winreg::RegKey;
    let key = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion", KEY_READ);
    match key.and_then(|k| k.get_value::<String, _>("CurrentBuildNumber")) {
        Ok(build) if !build.trim().is_empty() => build.trim().to_owned(),
        _ => "unknown".into(),
    }
}

struct InternetHandle(*mut c_void);

impl InternetHandle {
    fn new(raw: *mut c_void, what: &str) -> Result<Self> {
        if raw.is_null() {
            Err(windows::core::Error::from_win32()).with_context(|| what.to_owned())
        } else {
            Ok(Self(raw))
        }
    }
}

impl Drop for InternetHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = WinHttpCloseHandle(self.0);
        }
    }
}

fn post_json(body: &[u8]) -> Result<(u32, Vec<u8>)> {
    unsafe {
        let agent = HSTRING::from(concat!("Matteshot/", env!("CARGO_PKG_VERSION")));
        let session = InternetHandle::new(
            WinHttpOpen(
                &agent,
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                PCWSTR::null(),
                PCWSTR::null(),
                0,
            ),
            "open telemetry connection",
        )?;
        WinHttpSetTimeouts(session.0, 5_000, 5_000, 8_000, 10_000)
            .context("set telemetry connection timeouts")?;
        let host = HSTRING::from(POSTHOG_HOST);
        let connection = InternetHandle::new(
            WinHttpConnect(session.0, &host, 443, 0),
            "connect to PostHog",
        )?;
        let method = HSTRING::from("POST");
        let path = HSTRING::from(POSTHOG_PATH);
        let request = InternetHandle::new(
            WinHttpOpenRequest(
                connection.0,
                &method,
                &path,
                PCWSTR::null(),
                PCWSTR::null(),
                ptr::null(),
                WINHTTP_FLAG_SECURE,
            ),
            "open telemetry request",
        )?;
        let headers: Vec<u16> = "Content-Type: application/json\r\n".encode_utf16().collect();
        WinHttpSendRequest(
            request.0,
            Some(&headers),
            Some(body.as_ptr() as *const c_void),
            body.len() as u32,
            body.len() as u32,
            0,
        )
        .context("send telemetry request")?;
        WinHttpReceiveResponse(request.0, ptr::null_mut()).context("receive telemetry response")?;

        let mut status = 0u32;
        let mut status_size = std::mem::size_of::<u32>() as u32;
        let mut index = 0u32;
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(&mut status as *mut u32 as *mut c_void),
            &mut status_size,
            &mut index,
        )
        .context("read telemetry response status")?;

        let mut response = Vec::new();
        loop {
            let mut chunk = [0u8; 4096];
            let mut read = 0u32;
            WinHttpReadData(
                request.0,
                chunk.as_mut_ptr() as *mut c_void,
                chunk.len() as u32,
                &mut read,
            )
            .context("read telemetry response")?;
            if read == 0 {
                break;
            }
            if response.len() + read as usize > MAX_RESPONSE_BYTES {
                anyhow::bail!("telemetry response is too large");
            }
            response.extend_from_slice(&chunk[..read as usize]);
        }
        Ok((status, response))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{anyhow, Context};

    /// The whole point of the classifier: an operator can never be told what a
    /// user's files are called. Every one of these errors carries something
    /// that must not leave the machine.
    /// Every kind the classifier may ever return. An allowlist rather than a
    /// blocklist: a new branch that returns anything derived from the error
    /// fails here, instead of passing because it happened to avoid the few
    /// substrings a blocklist thought to check for.
    const ALLOWED: [&str; 11] = [
        "cancelled",
        "access_denied",
        "verification",
        "timeout",
        "network",
        "clipboard",
        "encoder",
        "capture_unavailable",
        "disk",
        "not_found",
        "other",
    ];

    #[test]
    fn no_error_detail_ever_reaches_a_reported_kind() {
        let leaky = [
            anyhow!(r"create C:\Users\tyler\OneDrive\Pictures\Matteshot: access is denied"),
            anyhow!("capture failed for window 'Quarterly Results - Confidential.xlsx'"),
            anyhow!(r"save D:\clients\acme\nda-draft.png: disk full"),
            anyhow!("token sk-live-4f9a2b in the message somehow"),
            anyhow!("something entirely new and unclassified"),
        ];
        for error in leaky {
            let kind = failure_kind(&error);
            assert!(
                ALLOWED.contains(&kind),
                "kind {kind:?} is not one of the bounded labels, so it may carry error detail"
            );
        }
    }

    /// Every event this app can send. The privacy policy at
    /// matteshot.app/privacy lists these by name and promises nothing else is
    /// transmitted, so adding one here without publishing it makes that page
    /// untrue. This test is the reminder.
    const PUBLISHED_EVENTS: [&str; 9] = [
        "matteshot_launch",
        "matteshot_capture",
        "matteshot_trial_started",
        "matteshot_license_activated",
        "matteshot_ocr_used",
        "matteshot_editor_opened",
        "matteshot_record_completed",
        "matteshot_scroll_capture",
        "matteshot_failure",
    ];

    #[test]
    fn every_event_in_the_source_is_a_published_one() {
        let mut found = std::collections::BTreeSet::new();
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        for entry in std::fs::read_dir(&src).expect("read src") {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read source file");
            // Scoped to report call sites. A bare "matteshot_" scan also picks
            // up the Win32 window class names (matteshot_overlay, _picker,
            // _tray and friends), which are not events and must not be
            // disclosed as if they were.
            for marker in ["report(\"matteshot_", "report_with(\"matteshot_"] {
                for (index, _) in text.match_indices(marker) {
                    let rest = &text[index + marker.len() - "matteshot_".len()..];
                    if let Some(end) = rest.find('"') {
                        found.insert(rest[..end].to_owned());
                    }
                }
            }
        }
        let published: std::collections::BTreeSet<String> =
            PUBLISHED_EVENTS.iter().map(|e| (*e).to_owned()).collect();
        let undisclosed: Vec<_> = found.difference(&published).collect();
        assert!(
            undisclosed.is_empty(),
            "these events are sent but not in PUBLISHED_EVENTS, and so are probably \
             missing from matteshot.app/privacy: {undisclosed:?}"
        );
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
            ("wobbling gizmo misaligned", "other"),
        ];
        for (message, expected) in cases {
            assert_eq!(failure_kind(&anyhow!("{message}")), expected, "for {message:?}");
        }
    }

    /// Classification reads the whole chain, so context added on the way up
    /// still lands in the right bucket.
    #[test]
    fn a_wrapped_error_is_classified_by_its_cause() {
        let wrapped = Err::<(), _>(anyhow!("access is denied"))
            .context("save the finished matte")
            .unwrap_err();
        assert_eq!(failure_kind(&wrapped), "access_denied");
    }
}
