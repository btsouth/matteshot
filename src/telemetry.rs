//! Pseudonymous usage telemetry, sent to PostHog.
//!
//! Consent first: `Config::telemetry` is `Option<bool>` and starts as `None`,
//! meaning unanswered. Nothing is sent while that holds. The first-run screen
//! asks, with the box ticked where opt-out is lawful and empty across the EU,
//! EEA, UK and Switzerland, and Settings can change the answer later.
//!
//! Only event names, a random installation-scoped telemetry id, the app
//! version, and the Windows build are ever transmitted. The id is minted after
//! consent and is deliberately unrelated to `MachineGuid` and the licensing
//! device id: a stable machine-derived identifier would make usage history
//! joinable to a named customer and survive reinstalls, which "anonymous"
//! never was. A retained random id is pseudonymous, so that is the word.
//! Screenshots, OCR text, file names, and paths are never part of an event.
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
    WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetOption,
    WinHttpSetTimeouts, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE,
    WINHTTP_OPTION_REDIRECT_POLICY, WINHTTP_OPTION_REDIRECT_POLICY_NEVER,
    WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_LOCATION, WINHTTP_QUERY_STATUS_CODE,
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
    set_enabled(crate::config::Config::load().telemetry_enabled());
}

/// Fire-and-forget a single event on a background thread. Never blocks the UI;
/// a slow or unreachable network only delays the spawned thread's exit.
pub fn report(event: &str) {
    report_with(event, &[]);
}

/// Where consent has to be asked for rather than assumed.
///
/// EU27 plus the rest of the EEA, the UK, and Switzerland. In these a
/// pre-ticked box is not valid consent, so the box starts empty and telemetry
/// stays off unless someone actively turns it on. Everywhere else it starts
/// ticked and can be switched off.
const CONSENT_REQUIRED_REGIONS: [&str; 32] = [
    "AT", "BE", "BG", "HR", "CY", "CZ", "DK", "EE", "FI", "FR", "DE", "GR", "HU", "IE", "IT",
    "LV", "LT", "LU", "MT", "NL", "PL", "PT", "RO", "SK", "SI", "ES", "SE", // EU27
    "IS", "LI", "NO", // rest of the EEA
    "GB", "CH", // UK, and Switzerland's revised FADP
];

/// Whether the consent box may start ticked for this region.
///
/// Anything that is not a plain two-letter code counts as unknown, including
/// the empty string `user_region` returns when the lookup fails. Unknown means
/// ask: guessing toward "assume consent" is the expensive direction to be
/// wrong in.
fn opt_out_allowed(region: &str) -> bool {
    let region = region.trim().to_ascii_uppercase();
    if region.len() != 2 || !region.chars().all(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    !CONSENT_REQUIRED_REGIONS.contains(&region.as_str())
}

/// Windows' idea of the user's country, e.g. "US" or "DE".
///
/// This is the region setting, not the display language: someone running an
/// English install in Berlin still reports DE, which is the thing that
/// matters here.
fn user_region() -> String {
    use windows::Win32::Globalization::GetUserDefaultGeoName;
    let mut buffer = [0u16; 16];
    let written = unsafe { GetUserDefaultGeoName(&mut buffer) };
    if written <= 1 {
        // Unknown region: treat it as one that requires asking. Guessing wrong
        // toward "assume consent" is the expensive direction.
        return String::new();
    }
    String::from_utf16_lossy(&buffer[..(written - 1) as usize])
}

/// What the first-run consent box should start as.
pub fn consent_default_checked() -> bool {
    opt_out_allowed(&user_region())
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

/// Every event this app may send, in the order the privacy policy lists them.
///
/// The policy at matteshot.app/privacy names these and promises nothing else
/// is transmitted, so this list is the contract. `report_with` refuses
/// anything absent from it: sending an undisclosed event would make that page
/// untrue, and dropping one costs only a metric.
pub const PUBLISHED_EVENTS: [&str; 9] = [
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

pub fn report_with(event: &str, properties: &[(&str, serde_json::Value)]) {
    // Enforced here rather than by scanning the source for call sites, because
    // a scan only sees the formatting it was written for. Loud in debug, and
    // silent-but-closed in release: never send what was not disclosed.
    debug_assert!(
        PUBLISHED_EVENTS.contains(&event),
        "{event} is not in PUBLISHED_EVENTS, so it is missing from the privacy policy"
    );
    if !PUBLISHED_EVENTS.contains(&event) {
        return;
    }
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

/// The pseudonymous, installation-scoped PostHog identity.
///
/// Minted lazily on the first event after consent, stored in config, and
/// cleared when telemetry is turned off — re-enabling starts an unlinkable
/// fresh history. Never derived from `MachineGuid` or the licensing device id;
/// those join usage to a customer and survive reinstalls. `None` means do not
/// send at all.
fn distinct_id() -> Option<String> {
    let config = crate::config::Config::load();
    if !config.telemetry_enabled() {
        return None;
    }
    if let Some(id) = config.telemetry_id {
        return Some(id);
    }
    let fresh = new_telemetry_id()?;
    // Persisted under Config::update's lock, where consent is re-checked:
    // Settings may have turned telemetry off since the load above, and an
    // opt-out must neither send this event nor have an id written back for a
    // later opt-in to rejoin histories with. If a concurrent event minted
    // first, the id already on disk is the one every thread must use.
    match crate::config::Config::update(|cfg| {
        if cfg.telemetry_enabled() && cfg.telemetry_id.is_none() {
            cfg.telemetry_id = Some(fresh.clone());
        }
    }) {
        Ok(saved) if saved.telemetry_enabled() => saved.telemetry_id,
        Ok(_) => None,
        // Config unwritable (read-only profile, full disk): keep one stable
        // per-process id rather than a fresh PostHog person per event.
        Err(_) => Some(SESSION_TELEMETRY_ID.get_or_init(|| fresh).clone()),
    }
}

/// Fallback identity for a process whose config cannot be written. Still
/// random and installation-unlinked; it just also stays stable across the
/// events of this run.
static SESSION_TELEMETRY_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// A random UUID, hyphenated lowercase. `None` if the system cannot produce
/// one, in which case nothing is sent.
fn new_telemetry_id() -> Option<String> {
    let guid = windows::core::GUID::new().ok()?;
    let d4 = guid.data4;
    Some(format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        guid.data1, guid.data2, guid.data3, d4[0], d4[1], d4[2], d4[3], d4[4], d4[5], d4[6], d4[7]
    ))
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

    // No id, no event: consent was withdrawn between the spawn and now, or
    // the id could not be minted. Never fall back to a machine identifier.
    let Some(distinct_id) = distinct_id() else {
        return Ok(());
    };
    let payload = CaptureEvent {
        api_key: POSTHOG_API_KEY,
        event,
        distinct_id,
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

fn winhttp_redirect_policy() -> u32 {
    WINHTTP_OPTION_REDIRECT_POLICY_NEVER
}

fn redirect_status_is_error(status: u32) -> bool {
    (300..400).contains(&status)
}

fn redirect_refusal(service: &str, status: u32, location: Option<&str>) -> String {
    format!(
        "{service} service redirected the request: status={status} location={}",
        location.filter(|value| !value.is_empty()).unwrap_or("none")
    )
}

fn query_location_header(request: *mut c_void) -> Option<String> {
    let mut buf = [0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    let mut index = 0u32;
    unsafe {
        WinHttpQueryHeaders(
            request,
            WINHTTP_QUERY_LOCATION,
            PCWSTR::null(),
            Some(buf.as_mut_ptr() as *mut c_void),
            &mut size,
            &mut index,
        )
        .ok()?;
    }
    let chars = (size as usize) / 2;
    let text = String::from_utf16_lossy(&buf[..chars.min(buf.len())]);
    let text = text.trim_end_matches('\0').trim();
    (!text.is_empty()).then(|| text.to_owned())
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
        // WinHTTP follows 307/308 by default and would resubmit the
        // request body to Location.
        let policy = winhttp_redirect_policy().to_ne_bytes();
        WinHttpSetOption(
            Some(session.0 as *const c_void),
            WINHTTP_OPTION_REDIRECT_POLICY,
            Some(policy.as_slice()),
        )
        .context("disable telemetry redirects")?;
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
        if redirect_status_is_error(status) {
            let location = query_location_header(request.0);
            anyhow::bail!("{}", redirect_refusal("telemetry", status, location.as_deref()));
        }

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

    #[test]
    fn telemetry_ids_are_random_and_unrelated_to_the_licensing_id() {
        let first = new_telemetry_id().expect("a UUID can be minted");
        let second = new_telemetry_id().expect("a UUID can be minted");
        // Random per mint: a repeat would mean it is derived from something
        // stable, which is exactly what this id must never be.
        assert_ne!(first, second);
        for id in [&first, &second] {
            assert_eq!(id.len(), 36);
            assert_eq!(id.matches('-').count(), 4);
            assert_eq!(id.to_ascii_lowercase(), *id);
            // The joinability this replaces: the licensing device id is a
            // stable machine-derived hash and must never be the PostHog
            // identity again.
            assert_ne!(*id, crate::license::device_id());
        }
    }

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
            // Scoped to report call sites, because a bare "matteshot_ scan
            // also matches the Win32 window class names (matteshot_overlay,
            // _picker, _tray), which are not events.
            //
            // Whitespace after the paren is skipped: report_with( is written
            // multiline in this very file, and a marker that assumed the
            // literal came straight after the paren silently missed it.
            for marker in ["report(", "report_with("] {
                for (index, _) in text.match_indices(marker) {
                    let rest = text[index + marker.len()..].trim_start();
                    let Some(literal) = rest.strip_prefix('"') else {
                        continue;
                    };
                    if let Some(end) = literal.find('"') {
                        let name = &literal[..end];
                        if name.starts_with("matteshot_") {
                            found.insert(name.to_owned());
                        }
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
    fn consent_must_be_asked_for_across_the_eea_uk_and_switzerland() {
        for region in ["DE", "FR", "IE", "PL", "GB", "NO", "IS", "LI", "CH", "de", " gb "] {
            assert!(!opt_out_allowed(region), "{region} may not be opted out by default");
        }
    }

    #[test]
    fn elsewhere_the_box_may_start_ticked() {
        for region in ["US", "CA", "AU", "JP", "BR", "IN", "us"] {
            assert!(opt_out_allowed(region), "{region} allows opt-out");
        }
    }

    /// Guessing toward "assume consent" is the expensive direction, so
    /// anything unrecognisable is treated as requiring consent. The empty
    /// string matters most: that is what user_region returns when the lookup
    /// fails, and it used to fall through to opt-out.
    #[test]
    fn an_unreadable_region_requires_asking() {
        for region in ["", "   ", "ZZ-not-a-country", "U", "USA", "12"] {
            assert!(!opt_out_allowed(region), "{region:?} must require asking");
        }
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

    #[test]
    fn winhttp_redirect_policy_is_never() {
        assert_eq!(
            winhttp_redirect_policy(),
            WINHTTP_OPTION_REDIRECT_POLICY_NEVER
        );
        assert_eq!(winhttp_redirect_policy(), 0);
    }

    /// A 307 to a second host must not transmit the body — NEVER plus
    /// treating 3xx as error is the probe.
    #[test]
    fn redirect_status_is_error_for_3xx_including_307_and_308() {
        for status in [301, 302, 303, 307, 308] {
            assert!(redirect_status_is_error(status), "HTTP {status}");
        }
        for status in [200, 403, 404, 500] {
            assert!(!redirect_status_is_error(status), "HTTP {status}");
        }
    }
}
