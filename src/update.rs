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

fn available_from(body: &str, current: &str) -> Result<Option<AvailableUpdate>> {
    let manifest: VersionManifest = serde_json::from_str(body).context("parse update manifest")?;
    if manifest.version.len() > 32 {
        bail!("update version is too long");
    }

    let current = Version::parse(current.trim_start_matches('v')).context("parse app version")?;
    let latest =
        Version::parse(manifest.version.trim_start_matches('v')).context("parse update version")?;
    if latest <= current {
        return Ok(None);
    }

    let download_url = manifest
        .download
        .or(manifest.url)
        .context("update manifest has no URL")?;
    if download_url.len() > 2048 || !download_url.starts_with("https://") {
        bail!("update URL must be HTTPS");
    }

    Ok(Some(AvailableUpdate {
        version: latest.to_string(),
        download_url,
    }))
}

pub fn check_once() -> Result<Option<AvailableUpdate>> {
    available_from(&fetch_manifest()?, env!("CARGO_PKG_VERSION"))
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
                    let automatic = crate::config::Config::load().auto_update;
                    post(hwnd_value, crate::tray::WM_UPDATE_AVAILABLE, update.clone());
                    if automatic {
                        if let Err(error) = apply(hwnd_value, &update) {
                            // Staging failed or the installer would not start.
                            // The tray still offers the manual download, so
                            // this is a quiet degradation, not a dead end.
                            crate::diagnostics::log("update could not be applied");
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

    #[test]
    fn newer_version_is_available() {
        let update = available_from(
            &manifest("0.9.2", "https://download.matteshot.app/MatteshotSetup.exe"),
            "0.9.1",
        )
        .unwrap()
        .unwrap();
        assert_eq!(update.version, "0.9.2");
    }

    #[test]
    fn equal_or_older_version_is_ignored() {
        assert!(available_from(
            &manifest("0.9.1", "https://download.matteshot.app/MatteshotSetup.exe"),
            "0.9.1"
        )
        .unwrap()
        .is_none());
        assert!(available_from(
            &manifest("0.8.9", "https://download.matteshot.app/MatteshotSetup.exe"),
            "0.9.1"
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn non_https_update_url_is_rejected() {
        assert!(available_from(&manifest("1.0.0", "file:///tmp/setup.exe"), "0.9.1").is_err());
    }

    #[test]
    fn page_url_is_used_when_download_is_missing() {
        let body = r#"{"version":"1.0.0","url":"https://matteshot.app"}"#;
        let update = available_from(body, "0.9.1").unwrap().unwrap();
        assert_eq!(update.download_url, "https://matteshot.app");
    }
}
