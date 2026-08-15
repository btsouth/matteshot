//! Silent background update checks against matteshot.app/version.json.
//!
//! Network work stays off the UI thread. When a newer version is found, the
//! worker posts an owned `AvailableUpdate` to the tray window, which takes
//! ownership on the main thread.

use std::ffi::c_void;
use std::ptr;
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, FixedOffset};
use semver::Version;
use serde::Deserialize;
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Networking::WinHttp::{
    WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders,
    WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetTimeouts,
    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE, WINHTTP_QUERY_FLAG_NUMBER,
    WINHTTP_QUERY_STATUS_CODE,
};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

const CHECK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
/// How often to re-ask whether the user has finished what they were doing.
const IDLE_POLL: Duration = Duration::from_secs(30);
/// Long enough for the "updating" balloon to be seen before the app restarts.
const BALLOON_GRACE: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvailableUpdate {
    pub version: String,
    pub download_url: String,
}

#[derive(Deserialize)]
struct VersionManifest {
    version: String,
    url: Option<String>,
    download: Option<String>,
    released: Option<String>,
    /// Every build still being served, so a customer whose update term has
    /// ended can be offered the newest release that term covered rather than
    /// nothing at all. Absent from older manifests, hence the default.
    #[serde(default)]
    releases: Vec<ManifestRelease>,
}

#[derive(Deserialize)]
struct ManifestRelease {
    version: String,
    download: Option<String>,
    released: Option<String>,
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

fn fetch_manifest() -> Result<String> {
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
            "open WinHTTP session",
        )?;
        WinHttpSetTimeouts(session.0, 5_000, 5_000, 5_000, 8_000)
            .context("set WinHTTP timeouts")?;

        let host = HSTRING::from("matteshot.app");
        let connection = InternetHandle::new(
            WinHttpConnect(session.0, &host, 443, 0),
            "connect to update host",
        )?;
        let request = InternetHandle::new(
            WinHttpOpenRequest(
                connection.0,
                w!("GET"),
                w!("/version.json"),
                PCWSTR::null(),
                PCWSTR::null(),
                ptr::null(),
                WINHTTP_FLAG_SECURE,
            ),
            "open update request",
        )?;

        let headers: Vec<u16> = "Accept: application/json\r\nCache-Control: no-cache\r\n"
            .encode_utf16()
            .collect();
        WinHttpSendRequest(request.0, Some(&headers), None, 0, 0, 0)
            .context("send update request")?;
        WinHttpReceiveResponse(request.0, ptr::null_mut()).context("receive update response")?;

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
        .context("read update response status")?;
        if status != 200 {
            bail!("update endpoint returned HTTP {status}");
        }

        let mut body = Vec::new();
        loop {
            let mut chunk = [0u8; 4096];
            let mut read = 0u32;
            WinHttpReadData(
                request.0,
                chunk.as_mut_ptr() as *mut c_void,
                chunk.len() as u32,
                &mut read,
            )
            .context("read update response")?;
            if read == 0 {
                break;
            }
            if body.len() + read as usize > MAX_RESPONSE_BYTES {
                bail!("update response is too large");
            }
            body.extend_from_slice(&chunk[..read as usize]);
        }
        String::from_utf8(body).context("update response is not UTF-8")
    }
}

/// Whether a release falls inside an update term.
///
/// Calendar dates rather than instants: a build put out during the final day
/// of a term counts, and nobody should lose one to a few hours. A release with
/// no date, or an unreadable one, counts as covered, because a mistake in our
/// own manifest must never withhold an update someone paid for.
fn covered(released: Option<&str>, deadline: Option<DateTime<FixedOffset>>) -> bool {
    let Some(deadline) = deadline else {
        return true;
    };
    let Some(released) = released.and_then(|text| DateTime::parse_from_rfc3339(text).ok()) else {
        return true;
    };
    released.date_naive() <= deadline.date_naive()
}

/// The newest release this license is entitled to install, if any.
///
/// `entitled_through` is the certificate's `updates_until`; `None` means
/// unrestricted, which covers the trial and any certificate minted without a
/// term. A release counts when it came out on or before that date, so the term
/// bounds *which versions were bought*, not how long they may be installed.
/// Someone who lapses keeps every build their year paid for, forever, and can
/// still install one after two years offline. Comparing against today instead
/// would quietly turn a perpetual license into a subscription.
fn available_from(
    body: &str,
    current: &str,
    entitled_through: Option<&str>,
) -> Result<Option<AvailableUpdate>> {
    let manifest: VersionManifest = serde_json::from_str(body).context("parse update manifest")?;
    let current = Version::parse(current.trim_start_matches('v')).context("parse app version")?;
    let deadline = entitled_through.and_then(|text| DateTime::parse_from_rfc3339(text).ok());

    let mut candidates = manifest.releases;
    if candidates.is_empty() {
        candidates.push(ManifestRelease {
            version: manifest.version,
            download: manifest.download.or(manifest.url),
            released: manifest.released,
        });
    }

    // A bad entry is skipped, never fatal. This list only grows, and every
    // caller of `check_once` discards errors, so one malformed line in the
    // history would silently disable updates for everyone who reads it. The
    // entry is still never offered; it is dropped rather than rejected, and
    // `check-site.mjs` catches the mistake at publish time where it can
    // actually be fixed.
    let mut best: Option<(Version, String)> = None;
    for release in candidates {
        if release.version.len() > 32 {
            continue;
        }
        let Ok(version) = Version::parse(release.version.trim_start_matches('v')) else {
            continue;
        };
        if version <= current || !covered(release.released.as_deref(), deadline) {
            continue;
        }
        let Some(download_url) = release.download else {
            continue;
        };
        if download_url.len() > 2048 || !download_url.starts_with("https://") {
            continue;
        }
        if best.as_ref().is_none_or(|(highest, _)| version > *highest) {
            best = Some((version, download_url));
        }
    }

    Ok(best.map(|(version, download_url)| AvailableUpdate {
        version: version.to_string(),
        download_url,
    }))
}

/// The update term on the installed license, or `None` when nothing limits it.
fn entitled_through() -> Option<String> {
    match crate::license::status() {
        crate::license::Status::Licensed { updates_until, .. } => updates_until,
        _ => None,
    }
}

pub fn check_once() -> Result<Option<AvailableUpdate>> {
    let body = fetch_manifest()?;
    let current = env!("CARGO_PKG_VERSION");
    let term = entitled_through();
    let update = available_from(&body, current, term.as_deref())?;
    if update.is_none() && term.is_some() {
        // A lapsed customer sees nothing happen at all, which support cannot
        // tell apart from a broken update check unless we write it down.
        if let Ok(Some(newer)) = available_from(&body, current, None) {
            crate::diagnostics::log(&format!(
                "{} is available but falls outside this license's update term",
                newer.version
            ));
        }
    }
    Ok(update)
}

fn post<T>(hwnd_value: isize, message: u32, payload: T) {
    let raw = Box::into_raw(Box::new(payload));
    let posted = unsafe {
        PostMessageW(
            HWND(hwnd_value as *mut c_void),
            message,
            WPARAM(0),
            LPARAM(raw as isize),
        )
    };
    if posted.is_err() {
        unsafe {
            drop(Box::from_raw(raw));
        }
    }
}

/// Fetch and verify the installer, then install it the moment the user is not
/// in the middle of something. Returns only if the update could not be applied;
/// a successful install replaces this process.
fn apply(hwnd_value: isize, update: &AvailableUpdate) -> Result<()> {
    let staged = crate::installer::stage(&update.download_url, &update.version, |_| {})?;
    crate::diagnostics::log("update staged and verified");

    // Never interrupt a capture, a recording, or an open editor. The installer
    // would shut them down cleanly, but "cleanly" still means losing the work
    // in front of the user.
    let mut waited = Duration::ZERO;
    while crate::window::any_surface_open() {
        // A whole day of waiting means the next check can supersede this.
        if waited >= CHECK_EVERY {
            crate::diagnostics::log("update deferred: surfaces stayed open");
            return Ok(());
        }
        thread::sleep(IDLE_POLL);
        waited += IDLE_POLL;
    }

    post(
        hwnd_value,
        crate::tray::WM_UPDATE_INSTALLING,
        update.version.clone(),
    );
    thread::sleep(BALLOON_GRACE);
    crate::diagnostics::log("update installing");
    crate::installer::launch(&staged)
}

/// Check immediately, then once every 24 hours while Matteshot stays open.
/// Failures are deliberately silent and retried at the next interval.
pub fn start(hwnd: HWND) {
    let hwnd_value = hwnd.0 as isize;
    thread::spawn(move || {
        let mut handled_version: Option<String> = None;
        loop {
            if let Ok(Some(update)) = check_once() {
                if handled_version.as_deref() != Some(update.version.as_str()) {
                    handled_version = Some(update.version.clone());
                    let loaded = crate::config::Config::try_load();
                    if let Err(error) = &loaded {
                        crate::diagnostics::log(&format!(
                            "update will not auto-install: config could not be read: {error:#}"
                        ));
                    }
                    let automatic = crate::config::auto_update_from_load(loaded);
                    post(hwnd_value, crate::tray::WM_UPDATE_AVAILABLE, update.clone());
                    if automatic {
                        if let Err(error) = apply(hwnd_value, &update) {
                            // Staging failed or the installer would not start.
                            // The tray still offers the manual download, so
                            // this is a quiet degradation, not a dead end.
                            //
                            // Quiet for the user, but not for us: a silent
                            // update failure strands people on an old build
                            // with nothing to report, so it is the one failure
                            // most worth counting.
                            crate::diagnostics::log("update could not be applied");
                            crate::telemetry::report_failure("update", &error);
                            eprintln!("update failed: {error:#}");
                        }
                    }
                }
            }
            thread::sleep(CHECK_EVERY);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(version: &str, download: &str) -> String {
        format!(
            r#"{{"version":"{version}","url":"https://matteshot.app","download":"{download}","notes":"Test"}}"#
        )
    }

    const SETUP: &str = "https://download.matteshot.app/MatteshotSetup.exe";

    #[test]
    fn automatic_install_requires_a_successful_opt_in_read() {
        assert!(crate::config::auto_update_from_load::<()>(Ok(
            crate::config::Config {
                auto_update: true,
                ..Default::default()
            }
        )));
        assert!(!crate::config::auto_update_from_load::<()>(Ok(
            crate::config::Config {
                auto_update: false,
                ..Default::default()
            }
        )));
        assert!(!crate::config::auto_update_from_load::<&str>(Err("locked")));
        assert!(!crate::config::auto_update_from_load::<&str>(Err("truncated")));
    }

    /// A manifest listing several served builds, each with a release date.
    fn history() -> String {
        r#"{
          "version": "2.0.0",
          "url": "https://matteshot.app",
          "download": "https://download.matteshot.app/MatteshotSetup-2.0.0.exe",
          "released": "2028-03-01T00:00:00Z",
          "releases": [
            {"version":"2.0.0","released":"2028-03-01T00:00:00Z","download":"https://download.matteshot.app/MatteshotSetup-2.0.0.exe"},
            {"version":"1.5.0","released":"2027-06-01T00:00:00Z","download":"https://download.matteshot.app/MatteshotSetup-1.5.0.exe"},
            {"version":"1.1.0","released":"2026-11-01T00:00:00Z","download":"https://download.matteshot.app/MatteshotSetup-1.1.0.exe"}
          ]
        }"#
        .to_owned()
    }

    #[test]
    fn newer_version_is_available() {
        let update = available_from(&manifest("0.9.2", SETUP), "0.9.1", None)
            .unwrap()
            .unwrap();
        assert_eq!(update.version, "0.9.2");
    }

    /// Every comparison here is numeric, not textual. As strings "0.14.10" is
    /// *less* than "0.14.9", because the compare stops at the first differing
    /// character, so a textual version check would tell every 0.14.9 install it
    /// was current and leave it there permanently. 0.14.10 was this project's
    /// first double-digit patch, which is where that mistake first bites and
    /// the reason it is pinned here.
    #[test]
    fn a_double_digit_patch_is_newer_than_a_single_digit_one() {
        for (offered, running) in [
            ("0.14.10", "0.14.9"),
            ("0.14.10", "0.14.2"),
            ("0.15.0", "0.14.10"),
            ("1.0.10", "1.0.9"),
        ] {
            let update = available_from(&manifest(offered, SETUP), running, None)
                .unwrap()
                .unwrap_or_else(|| panic!("{offered} should be offered to {running}"));
            assert_eq!(update.version, offered);
        }

        // And the reverse never happens: a smaller patch number that sorts
        // later as text must not be mistaken for an upgrade.
        for (offered, running) in [("0.14.9", "0.14.10"), ("1.0.9", "1.0.10")] {
            assert!(
                available_from(&manifest(offered, SETUP), running, None)
                    .unwrap()
                    .is_none(),
                "{offered} must not be offered to {running}"
            );
        }
    }

    #[test]
    fn equal_or_older_version_is_ignored() {
        assert!(available_from(&manifest("0.9.1", SETUP), "0.9.1", None)
            .unwrap()
            .is_none());
        assert!(available_from(&manifest("0.8.9", SETUP), "0.9.1", None)
            .unwrap()
            .is_none());
    }

    /// What matters is that such a URL is never handed to the installer. It is
    /// dropped rather than raised, so one bad entry cannot take the whole
    /// update check down with it.
    #[test]
    fn a_non_https_url_is_never_offered() {
        for bad in ["file:///tmp/setup.exe", "http://download.matteshot.app/x.exe"] {
            assert!(
                available_from(&manifest("1.0.0", bad), "0.9.1", None)
                    .unwrap()
                    .is_none(),
                "for {bad}"
            );
        }
    }

    /// One malformed entry must not cost everyone their updates.
    #[test]
    fn a_broken_entry_does_not_disable_the_rest() {
        let body = r#"{
          "version": "1.5.0",
          "releases": [
            {"version":"not-a-version","released":"2027-06-02T00:00:00Z","download":"https://d/x.exe"},
            {"version":"1.5.0","released":"2027-06-01T00:00:00Z","download":"https://d/b.exe"},
            {"version":"1.4.0","released":"2027-05-01T00:00:00Z"},
            {"version":"1.3.0","released":"2027-01-01T00:00:00Z","download":"http://d/c.exe"}
          ]
        }"#;
        let update = available_from(body, "1.0.0", None).unwrap().unwrap();
        assert_eq!(update.version, "1.5.0");
        assert_eq!(update.download_url, "https://d/b.exe");
    }

    #[test]
    fn page_url_is_used_when_download_is_missing() {
        let body = r#"{"version":"1.0.0","url":"https://matteshot.app"}"#;
        let update = available_from(body, "0.9.1", None).unwrap().unwrap();
        assert_eq!(update.download_url, "https://matteshot.app");
    }

    /// The trial has no term, so it is always offered the newest build.
    #[test]
    fn no_entitlement_means_no_restriction() {
        let update = available_from(&history(), "1.0.0", None).unwrap().unwrap();
        assert_eq!(update.version, "2.0.0");
    }

    /// The heart of it: a lapsed license is not offered a build released after
    /// its term, but is still offered the newest one released inside it.
    #[test]
    fn a_lapsed_license_still_gets_the_builds_it_paid_for() {
        let update = available_from(&history(), "1.0.0", Some("2027-08-01T00:00:00Z"))
            .unwrap()
            .unwrap();
        assert_eq!(update.version, "1.5.0");
        assert!(update.download_url.ends_with("MatteshotSetup-1.5.0.exe"));
    }

    /// Time passing must not take anything away. The same lapsed license makes
    /// the same offer whenever it asks, because the comparison is against the
    /// release date and never against today.
    #[test]
    fn an_entitlement_does_not_decay() {
        // Whatever they are running, the ceiling their term bought is the same
        // one, and nothing here consults a clock to decide it.
        for current in ["0.9.0", "1.0.0", "v1.1.0", "1.4.9"] {
            let update = available_from(&history(), current, Some("2027-08-01T00:00:00Z"))
                .unwrap()
                .unwrap();
            assert_eq!(update.version, "1.5.0", "from {current}");
        }
        // And once on that build, there is nothing further owed.
        assert!(
            available_from(&history(), "1.5.0", Some("2027-08-01T00:00:00Z"))
                .unwrap()
                .is_none()
        );
    }

    /// A term that predates every served build offers nothing rather than
    /// falling back to the newest one.
    #[test]
    fn a_term_older_than_every_build_offers_nothing() {
        assert!(available_from(&history(), "1.0.0", Some("2026-01-01T00:00:00Z"))
            .unwrap()
            .is_none());
    }

    /// A build put out during the last day of a term belongs to that term.
    #[test]
    fn the_final_day_of_a_term_counts() {
        assert!(covered(
            Some("2027-08-01T23:59:00Z"),
            Some(DateTime::parse_from_rfc3339("2027-08-01T00:00:00Z").unwrap())
        ));
        assert!(!covered(
            Some("2027-08-02T00:00:01Z"),
            Some(DateTime::parse_from_rfc3339("2027-08-01T00:00:00Z").unwrap())
        ));
    }

    /// Our own mistake must never withhold an update someone paid for.
    #[test]
    fn a_manifest_without_dates_is_not_treated_as_expired() {
        let update = available_from(&manifest("1.0.0", SETUP), "0.9.1", Some("2020-01-01T00:00:00Z"))
            .unwrap()
            .unwrap();
        assert_eq!(update.version, "1.0.0");
        for unusable in [None, Some("not a date"), Some("")] {
            assert!(
                covered(
                    unusable,
                    Some(DateTime::parse_from_rfc3339("2020-01-01T00:00:00Z").unwrap())
                ),
                "for {unusable:?}"
            );
        }
    }

    /// An unreadable term restricts nothing, for the same reason.
    #[test]
    fn an_unreadable_term_restricts_nothing() {
        let update = available_from(&history(), "1.0.0", Some("whenever"))
            .unwrap()
            .unwrap();
        assert_eq!(update.version, "2.0.0");
    }

    /// Order in the manifest is not load-bearing.
    #[test]
    fn the_newest_covered_build_wins_regardless_of_order() {
        let body = r#"{
          "version": "1.1.0",
          "releases": [
            {"version":"1.1.0","released":"2026-11-01T00:00:00Z","download":"https://d/a.exe"},
            {"version":"1.5.0","released":"2027-06-01T00:00:00Z","download":"https://d/b.exe"},
            {"version":"1.3.0","released":"2027-01-01T00:00:00Z","download":"https://d/c.exe"}
          ]
        }"#;
        let update = available_from(body, "1.0.0", Some("2027-08-01T00:00:00Z"))
            .unwrap()
            .unwrap();
        assert_eq!(update.version, "1.5.0");
    }
}
