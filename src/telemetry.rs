//! Anonymous usage telemetry, sent to PostHog.
//!
//! Opt-out by default: `Config::telemetry` defaults to true and the Settings
//! window can switch it off. Only event names, the anonymous `device_id()`,
//! the app version, and the Windows build are ever transmitted. Screenshots,
//! OCR text, file names, and paths are never part of an event.
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
