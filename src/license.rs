//! Trial and perpetual-license state.
//!
//! The app never embeds a payment-provider credential. Activation goes through
//! license.matteshot.app and stores a device-bound Ed25519 certificate. The
//! certificate can be verified offline; network refreshes only propagate
//! refunds, disabled keys, and device deactivations.

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Networking::WinHttp::{
    WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders,
    WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetTimeouts,
    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE, WINHTTP_QUERY_FLAG_NUMBER,
    WINHTTP_QUERY_STATUS_CODE,
};
use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY};
use winreg::RegKey;

pub const BUY_URL: &str = "https://matteshot.app/#buy";
const LICENSE_HOST: &str = "license.matteshot.app";
const LICENSE_PATH_ACTIVATE: &str = "/v1/license/activate";
const LICENSE_PATH_REFRESH: &str = "/v1/license/refresh";
const LICENSE_PATH_DEACTIVATE: &str = "/v1/license/deactivate";
const LICENSE_PATH_TRIAL_START: &str = "/v1/trial/start";
const LICENSE_PATH_TRIAL_STATUS: &str = "/v1/trial/status";
const PUBLIC_KEY_BASE64: &str = "JSooNvlMugs9h9gRkeF7MruQswJnzAjHVrRNf/cXhqA=";
const TRIAL_SECONDS: i64 = 14 * 24 * 60 * 60;
/// How often the app talks to license.matteshot.app: a trial re-syncs its
/// signed start date and server timestamp, and a licensed device re-validates
/// so refunds and revocations propagate without waiting for a restart.
const SYNC_EVERY: Duration = Duration::from_secs(60 * 60);
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const REGISTRY_KEY: &str = r"Software\Southbound Software\Matteshot";
const REGISTRY_TRIAL_START: &str = "TrialStartedAt";
const REGISTRY_LAST_SEEN: &str = "TrialLastSeenAt";
const LICENSE_MUTEX: &str = "Local\\Matteshot.License.State";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Licensed {
        customer_email: Option<String>,
        updates_until: Option<String>,
    },
    TrialNotStarted,
    Trial {
        days_left: u32,
    },
    /// Persistent state is corrupt or could not be read before any valid
    /// status was cached. This is distinct from an ended trial so activation
    /// remains an explicit recovery path, but capture stays blocked because
    /// the app cannot prove an entitlement from damaged state.
    Unavailable,
    Expired,
}

impl Status {
    pub fn can_capture(&self) -> bool {
        !matches!(self, Status::Expired | Status::Unavailable)
    }

    /// How long this license is entitled to new versions, in words.
    ///
    /// Worth stating plainly. The term is the part of a perpetual license
    /// people misremember, and it decides only which versions arrive, never
    /// whether the app keeps working.
    pub fn updates_note(&self) -> Option<String> {
        let Status::Licensed {
            updates_until: Some(until),
            ..
        } = self
        else {
            return None;
        };
        let until = DateTime::parse_from_rfc3339(until).ok()?;
        Some(format!("Updates through {}", until.format("%-d %B %Y")))
    }

    /// Entitlement class only, for the privacy-safe support report. The tray
    /// label greets the licensed user by email; a diagnostics paste must never
    /// carry that identity, so this deliberately collapses every variant to a
    /// coarse word.
    pub fn diagnostics_label(&self) -> &'static str {
        match self {
            Status::Licensed { .. } => "Licensed",
            Status::TrialNotStarted | Status::Trial { .. } => "Trial",
            Status::Unavailable => "Unavailable",
            Status::Expired => "Expired",
        }
    }

    pub fn tray_label(&self) -> String {
        match self {
            Status::Licensed {
                customer_email: Some(email),
                ..
            } => {
                format!("Licensed to {email}")
            }
            Status::Licensed { .. } => "Licensed".into(),
            Status::TrialNotStarted => "14-day trial ready".into(),
            Status::Trial { days_left: 1 } => "Trial: 1 day left".into(),
            Status::Trial { days_left } => format!("Trial: {days_left} days left"),
            Status::Unavailable => "License state temporarily unavailable".into(),
            Status::Expired => "Trial ended".into(),
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    trial_started_at: Option<i64>,
    #[serde(default)]
    last_seen_at: Option<i64>,
    #[serde(default)]
    license: Option<StoredLicense>,
    /// Server-issued trial certificate. Present once the start date has been
    /// recorded on license.matteshot.app; the signed start never changes.
    #[serde(default)]
    trial: Option<StoredTrial>,
}

#[derive(Clone, Serialize, Deserialize)]
struct StoredLicense {
    certificate: String,
    signature: String,
    /// Legacy plaintext refresh token from installs that predate DPAPI
    /// protection. Read for migration and never written by current builds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
    /// The refresh token as a DPAPI current-user blob, base64. Useless off
    /// this machine and account: a profile backup, support bundle, or
    /// infostealer copying `license.json` gets ciphertext, not a reusable
    /// session credential. Signature checks stop forgery; this stops theft.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refresh_token_protected: Option<String>,
}

impl StoredLicense {
    fn new(certificate: String, signature: String, refresh_token: String) -> Self {
        match dpapi_protect(refresh_token.as_bytes()) {
            Ok(blob) => Self {
                certificate,
                signature,
                refresh_token: None,
                refresh_token_protected: Some(STANDARD.encode(blob)),
            },
            Err(error) => {
                // DPAPI failing for the current user is close to theoretical.
                // If it does, a working activation beats an unusable one: keep
                // the legacy shape and let a later load retry the migration.
                crate::diagnostics::log(&format!(
                    "license token protection unavailable, storing legacy shape: {error:#}"
                ));
                Self {
                    certificate,
                    signature,
                    refresh_token: Some(refresh_token),
                    refresh_token_protected: None,
                }
            }
        }
    }

    /// The usable refresh token, whichever shape holds it. `None` when the
    /// protected blob cannot be decrypted here — copied from another machine
    /// or user profile, or corrupt — which callers treat as "no session".
    fn token(&self) -> Option<String> {
        if let Some(blob) = &self.refresh_token_protected {
            let decoded = STANDARD.decode(blob).ok()?;
            let secret = dpapi_unprotect(&decoded).ok()?;
            return String::from_utf8(secret).ok();
        }
        self.refresh_token.clone()
    }

    /// Drain a legacy plaintext token into the protected shape. Identity for
    /// already-protected state; on DPAPI failure the legacy shape survives so
    /// nothing is lost and the next load retries.
    fn into_protected(self) -> Self {
        match (&self.refresh_token, &self.refresh_token_protected) {
            (Some(token), None) => {
                let token = token.clone();
                Self::new(self.certificate, self.signature, token)
            }
            _ => self,
        }
    }
}

/// Encrypt for the current Windows user (DPAPI), no UI ever.
fn dpapi_protect(secret: &[u8]) -> Result<Vec<u8>> {
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    let input = CRYPT_INTEGER_BLOB {
        cbData: secret.len() as u32,
        pbData: secret.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptProtectData(
            &input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
        .context("protect secret with DPAPI")?;
        let bytes = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        let _ = windows::Win32::Foundation::LocalFree(windows::Win32::Foundation::HLOCAL(
            output.pbData as *mut core::ffi::c_void,
        ));
        Ok(bytes)
    }
}

fn dpapi_unprotect(blob: &[u8]) -> Result<Vec<u8>> {
    use windows::Win32::Security::Cryptography::{
        CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    let input = CRYPT_INTEGER_BLOB {
        cbData: blob.len() as u32,
        pbData: blob.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptUnprotectData(
            &input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
        .context("unprotect secret with DPAPI")?;
        let bytes = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        let _ = windows::Win32::Foundation::LocalFree(windows::Win32::Foundation::HLOCAL(
            output.pbData as *mut core::ffi::c_void,
        ));
        Ok(bytes)
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct StoredTrial {
    certificate: String,
    signature: String,
}

#[derive(Debug, Deserialize)]
struct TrialCertificate {
    version: u32,
    kind: String,
    device_id: String,
    started_at: String,
    issued_at: String,
}

#[derive(Debug, Deserialize)]
struct Certificate {
    version: u32,
    kind: String,
    license_id: String,
    instance_id: String,
    device_id: String,
    customer_email: Option<String>,
    purchased_at: String,
    updates_until: Option<String>,
    issued_at: String,
}

#[derive(Serialize)]
struct ActivateRequest {
    license_key: String,
    device_id: String,
    device_name: String,
    app_version: &'static str,
}

#[derive(Serialize)]
struct SessionRequest {
    refresh_token: String,
    device_id: String,
}

#[derive(Deserialize)]
struct ActivationResponse {
    activated: bool,
    certificate: String,
    signature: String,
    refresh_token: String,
}

#[derive(Deserialize)]
struct ErrorResponse {
    error: Option<String>,
}

#[derive(Serialize)]
struct TrialStatusRequest {
    device_id: String,
    app_version: &'static str,
}

#[derive(Serialize)]
struct TrialStartRequest {
    device_id: String,
    device_name: String,
    app_version: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    started_at: Option<String>,
}

#[derive(Deserialize)]
struct TrialResponse {
    certificate: Option<String>,
    signature: Option<String>,
    trial: Option<String>,
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

fn state_path() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|dir| dir.join("matteshot").join("license.json"))
}

fn load_state_from(path: &std::path::Path) -> Result<State> {
    let body = match std::fs::read_to_string(path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(State::default()),
        Err(error) => return Err(error).context("read license state"),
    };
    serde_json::from_str(&body).context("parse license state")
}

fn needs_token_protection(state: &State) -> bool {
    state
        .license
        .as_ref()
        .is_some_and(|stored| stored.refresh_token.is_some())
}

fn load_state() -> Result<State> {
    let path = state_path().context("Windows has no application data directory")?;
    load_state_at(&path)
}

/// The downgrade escape hatch beside `license.json`. Builds that predate the
/// protected token cannot parse the migrated file and report the license
/// unavailable; renaming this sidecar back over `license.json` restores them.
/// It holds the pre-migration plaintext, so it is deleted the moment the
/// server rotates the token (next refresh) — from then on its contents are
/// dead credentials and a downgraded build needs a fresh activation anyway.
fn pre_dpapi_backup_path(path: &std::path::Path) -> std::path::PathBuf {
    path.with_extension("json.pre-dpapi")
}

fn remove_pre_dpapi_backup() {
    if let Some(path) = state_path() {
        let _ = remove_pre_dpapi_backup_at(&path);
    }
}

/// Best-effort but never silent: the sidecar holds a plaintext token, so a
/// delete that fails (a scanner or indexer holding the file open) leaves a
/// breadcrumb, and `load_state_at` retries on every later load until the
/// file is gone. `Ok(false)` is "already gone".
fn remove_pre_dpapi_backup_at(path: &std::path::Path) -> std::io::Result<bool> {
    match std::fs::remove_file(pre_dpapi_backup_path(path)) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => {
            crate::diagnostics::log(&format!(
                "pre-migration license backup could not be removed yet: {error}"
            ));
            Err(error)
        }
    }
}

/// The sidecar outlives its purpose the moment the token it holds stops
/// being the live one — rotated by a refresh, released by deactivation,
/// rejected by the server. Each of those paths deletes it, but a delete can
/// fail (see `remove_pre_dpapi_backup_at`), so every load looks again: a
/// sidecar whose token no longer matches the live state is retired here.
/// A sidecar still holding the live token is the downgrade escape hatch and
/// stays.
fn retire_stale_pre_dpapi_backup(path: &std::path::Path, state: &State) {
    // Absent is the steady state; unreadable this time just means next time.
    let Ok(sidecar) = std::fs::read(pre_dpapi_backup_path(path)) else {
        return;
    };
    let sidecar_token = serde_json::from_slice::<State>(&sidecar)
        .ok()
        .and_then(|backup| backup.license)
        .and_then(|stored| stored.token());
    let dead = match (state.license.as_ref(), sidecar_token) {
        // Deactivated or rejected: nothing is live, so nothing to downgrade to.
        (None, _) => true,
        // The sidecar holds no usable token: junk, whatever the live state.
        (Some(_), None) => true,
        (Some(live), Some(sidecar)) => match live.token() {
            // Rotated server-side; the sidecar's copy stops working with it.
            Some(live) => live != sidecar,
            // The live blob does not decrypt here (copied profile, damaged
            // blob). That says nothing about the sidecar's token being dead
            // — and it may be the only copy that still works — so do not
            // guess; the next refresh or deactivation settles it.
            None => false,
        },
    };
    if dead {
        let _ = remove_pre_dpapi_backup_at(path);
    }
}

fn load_state_at(path: &std::path::Path) -> Result<State> {
    let state = load_state_from(path)?;
    if !needs_token_protection(&state) {
        retire_stale_pre_dpapi_backup(path, &state);
        return Ok(state);
    }
    // Drain the pre-DPAPI plaintext token into the protected shape. The write
    // happens under LICENSE_MUTEX with the state re-read inside it, so a
    // concurrent activation or refresh save cannot be clobbered with what was
    // read above. (The named mutex is recursive per thread, so callers that
    // already hold it are fine.) save_state is atomic, and everything past
    // the lock is best-effort: on any failure the plaintext keeps working and
    // the next load retries.
    let Ok(_guard) = crate::state_lock::lock(LICENSE_MUTEX) else {
        return Ok(state);
    };
    let mut state = load_state_from(path)?;
    if needs_token_protection(&state) {
        // The sidecar first: a migrated file with no backup strands anyone
        // who rolls back to a build that cannot parse the new shape. Backup
        // failure is not fatal, but it is worth a breadcrumb.
        let backup = pre_dpapi_backup_path(path);
        if let Err(error) = std::fs::copy(path, &backup) {
            crate::diagnostics::log(&format!(
                "pre-migration license backup failed (migrating anyway): {error}"
            ));
        }
        let migrated = state.license.take().map(StoredLicense::into_protected);
        let protected = migrated
            .as_ref()
            .is_some_and(|stored| stored.refresh_token.is_none());
        state.license = migrated;
        if protected {
            match save_state_at(path, &state) {
                Ok(()) => crate::diagnostics::log("license refresh token now DPAPI-protected"),
                Err(error) => crate::diagnostics::log(&format!(
                    "license token protection migration not saved yet: {error:#}"
                )),
            }
        }
    }
    Ok(state)
}

fn corrupt_state_backup_path(path: &std::path::Path) -> std::path::PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    path.with_extension(format!("json.corrupt-{unique}"))
}

/// A newly verified paid certificate is the one safe recovery point for a
/// corrupt local state file. Preserve the bad bytes for support, then replace
/// them with the server-verified activation; transient I/O errors still fail
/// without touching anything on disk.
fn load_state_for_activation_from(path: &std::path::Path) -> Result<State> {
    match load_state_from(path) {
        Ok(state) => Ok(state),
        Err(error) if error.downcast_ref::<serde_json::Error>().is_some() => {
            let backup = corrupt_state_backup_path(path);
            std::fs::rename(path, &backup).context("quarantine corrupt license state")?;
            crate::diagnostics::log(
                "corrupt license state was quarantined after verifying a replacement activation",
            );
            Ok(State::default())
        }
        Err(error) => Err(error),
    }
}

fn load_state_for_activation() -> Result<State> {
    let path = state_path().context("Windows has no application data directory")?;
    load_state_for_activation_from(&path)
}

fn save_state(state: &State) -> Result<()> {
    let path = state_path().context("Windows has no application data directory")?;
    save_state_at(&path, state)
}

fn save_state_at(path: &std::path::Path, state: &State) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("create Matteshot data directory")?;
    }
    let body = serde_json::to_vec_pretty(state).context("serialize license state")?;
    crate::state_lock::atomic_write(path, &body).context("replace license state")
}

static LAST_VALID_STATUS: OnceLock<Mutex<Option<Status>>> = OnceLock::new();

fn remember_status(status: Status) -> Status {
    if let Ok(mut cached) = LAST_VALID_STATUS.get_or_init(|| Mutex::new(None)).lock() {
        *cached = Some(status.clone());
    }
    status
}

fn remembered_status() -> Option<Status> {
    LAST_VALID_STATUS
        .get_or_init(|| Mutex::new(None))
        .lock()
        .ok()
        .and_then(|cached| cached.clone())
}

fn status_after_load_error(error: &anyhow::Error, cached: Option<Status>) -> Status {
    if error.downcast_ref::<serde_json::Error>().is_some() {
        Status::Unavailable
    } else {
        cached.unwrap_or(Status::Unavailable)
    }
}

fn same_activation(state: &State, expected: &StoredLicense) -> bool {
    // Compared through `token()` so a protected and a legacy shape holding
    // the same credential still count as the same activation mid-migration.
    // Two undecryptable tokens are never "the same": that would let damaged
    // state replace or release an activation it cannot prove it owns.
    match (
        state.license.as_ref().and_then(StoredLicense::token),
        expected.token(),
    ) {
        (Some(current), Some(expected)) => current == expected,
        _ => false,
    }
}

/// Once this PC has held a paid activation it cannot fall back into an unused
/// trial after a refund, revocation, or manual deactivation.
fn close_trial_after_activation(state: &mut State, now: i64) {
    let expired_start = now.saturating_sub(TRIAL_SECONDS);
    let started = earliest(
        earliest(state.trial_started_at, registry_time(REGISTRY_TRIAL_START)),
        Some(expired_start),
    )
    .unwrap_or(expired_start);
    state.trial_started_at = Some(started);
    state.last_seen_at = Some(now.max(state.last_seen_at.unwrap_or(now)));
    set_registry_time(REGISTRY_TRIAL_START, started);
    set_registry_time(REGISTRY_LAST_SEEN, state.last_seen_at.unwrap_or(now));
}

fn registry_time(name: &str) -> Option<i64> {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(REGISTRY_KEY, KEY_READ)
        .ok()
        .and_then(|key| key.get_value::<u64, _>(name).ok())
        .and_then(|value| i64::try_from(value).ok())
}

fn set_registry_time(name: &str, value: i64) {
    if value < 0 {
        return;
    }
    if let Ok((key, _)) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(REGISTRY_KEY) {
        let _ = key.set_value(name, &(value as u64));
    }
}

fn earliest(left: Option<i64>, right: Option<i64>) -> Option<i64> {
    match (left, right) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn latest(left: Option<i64>, right: Option<i64>) -> Option<i64> {
    match (left, right) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn trial_status_at(started: Option<i64>, last_seen: Option<i64>, now: i64) -> Status {
    let Some(started) = started else {
        return Status::TrialNotStarted;
    };
    let effective_now = now.max(last_seen.unwrap_or(now));
    let remaining = started.saturating_add(TRIAL_SECONDS) - effective_now;
    if remaining <= 0 {
        Status::Expired
    } else {
        Status::Trial {
            days_left: ((remaining + 86_399) / 86_400) as u32,
        }
    }
}

/// Force what `status()` reports, so the trial and purchase flow can be walked
/// through without waiting out a real 14 days.
///
/// `MATTESHOT_LICENSE_OVERRIDE=not-started | trial | trial:<days> | expired |
/// licensed | licensed:<email>`. Anything else is ignored, so a typo fails
/// safe by reporting the machine's real state.
///
/// This function writes nothing itself, but do not read that as "the override
/// has no side effects". Everything downstream branches on what `status()`
/// reports, so forcing a state steers real behaviour: running `expired` makes
/// `start_background_refresh` take the trial branch, and `sync_trial_once`
/// will then register a trial record for this device on the server. That is
/// harmless on a licensed machine, because `close_trial_after_activation`
/// already backdated the local start past its 14 days, so what gets pinned is
/// an already-spent trial. It is still a server-side write that outlives the
/// test, so use `licensed` rather than `expired` when you only need the app to
/// stop nagging.
///
/// Compiled only under the `debug-license` feature, which no shipped binary
/// has: `verify-code.ps1` lints and tests with `--all-features` to keep this
/// correct, then builds the release binary with none of them.
#[cfg(feature = "debug-license")]
fn debug_override() -> Option<Status> {
    let raw = std::env::var("MATTESHOT_LICENSE_OVERRIDE").ok()?;
    let raw = raw.trim();
    let (kind, argument) = match raw.split_once(':') {
        Some((kind, argument)) => (kind.trim(), Some(argument.trim())),
        None => (raw, None),
    };
    let status = match kind {
        "not-started" => Status::TrialNotStarted,
        // A bare "trial" means the full window. An argument that does not parse
        // is a typo, not a request for the default: silently handing back 14
        // days would be exactly the permissive guess this is meant to avoid.
        // Zero is not a state the real clock can produce, so it means expiry.
        "trial" => match argument {
            None => Status::Trial { days_left: 14 },
            Some(value) => match value.parse::<u32>() {
                Ok(0) => Status::Expired,
                Ok(days_left) => Status::Trial { days_left },
                Err(_) => return None,
            },
        },
        "expired" => Status::Expired,
        "licensed" => Status::Licensed {
            customer_email: argument
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
            updates_until: None,
        },
        _ => return None,
    };
    // Say so once per process. A forced state is otherwise indistinguishable
    // from a real one in the tray, and that is a confusing way to lose an hour.
    static ANNOUNCED: std::sync::Once = std::sync::Once::new();
    ANNOUNCED.call_once(|| {
        crate::diagnostics::log(&format!("license overridden by environment: {raw}"));
    });
    Some(status)
}

/// Whether the environment is actually forcing a state, as opposed to merely
/// setting the variable to something unrecognized. Callers report the two
/// cases differently, because "forced" next to the machine's real state is a
/// worse lie than no message at all.
#[cfg(feature = "debug-license")]
pub fn debug_override_active() -> bool {
    debug_override().is_some()
}

pub fn status() -> Status {
    #[cfg(feature = "debug-license")]
    if let Some(forced) = debug_override() {
        return forced;
    }
    let _guard = crate::state_lock::lock(LICENSE_MUTEX).ok();
    let mut state = match load_state() {
        Ok(state) => state,
        Err(error) => {
            crate::diagnostics::log(&format!(
                "license state could not be loaded; leaving it untouched: {error:#}"
            ));
            return status_after_load_error(&error, remembered_status());
        }
    };
    let device = device_id();
    if let Some(stored) = state.license.as_ref() {
        if let Ok(certificate) = verify(stored, &device) {
            return remember_status(Status::Licensed {
                customer_email: certificate.customer_email,
                updates_until: certificate.updates_until,
            });
        }
        state.license = None;
        let _ = save_state(&state);
    }

    // A server-issued trial certificate carries the clock, and both halves of
    // it only ever move against the user. The signed start survives a local
    // wipe, and local state can pull it earlier but never later, so time a
    // spent trial has already used cannot be handed back.
    //
    // The certificate is re-signed on every sync, so its issue date is a server
    // timestamp the machine cannot forge. Folding it into the seen-at floor is
    // what stops a rolled-back system clock from buying trial days: wiping the
    // local floor only replaces it with the server's on the next sync.
    if let Some(stored) = state.trial.as_ref() {
        if let Ok(certificate) = verify_trial(stored, &device) {
            let signed_start = DateTime::parse_from_rfc3339(&certificate.started_at)
                .map(|value| value.timestamp())
                .unwrap_or_else(|_| Utc::now().timestamp());
            let issued = DateTime::parse_from_rfc3339(&certificate.issued_at)
                .map(|value| value.timestamp())
                .ok();
            let now = Utc::now().timestamp();
            // The start only ever moves earlier. A certificate can undo a wipe,
            // but it can never hand back time a local record says is spent:
            // after a refund the server may not know this device ever trialed.
            let started = earliest(
                Some(signed_start),
                earliest(state.trial_started_at, registry_time(REGISTRY_TRIAL_START)),
            )
            .unwrap_or(signed_start);
            let previous_seen = latest(
                latest(state.last_seen_at, registry_time(REGISTRY_LAST_SEEN)),
                issued,
            );
            let effective_now = now.max(previous_seen.unwrap_or(now));
            state.trial_started_at = Some(started);
            state.last_seen_at = Some(effective_now);
            set_registry_time(REGISTRY_TRIAL_START, started);
            set_registry_time(REGISTRY_LAST_SEEN, effective_now);
            let _ = save_state(&state);
            return remember_status(trial_status_at(Some(started), previous_seen, now));
        }
        state.trial = None;
        let _ = save_state(&state);
    }

    let now = Utc::now().timestamp();
    let started = earliest(state.trial_started_at, registry_time(REGISTRY_TRIAL_START));
    let previous_seen = latest(state.last_seen_at, registry_time(REGISTRY_LAST_SEEN));
    let effective_now = now.max(previous_seen.unwrap_or(now));
    state.trial_started_at = started;
    state.last_seen_at = Some(effective_now);
    set_registry_time(REGISTRY_LAST_SEEN, effective_now);
    let _ = save_state(&state);
    remember_status(trial_status_at(started, previous_seen, now))
}

/// The signed certificate + signature this device's license holds, for
/// callers (like the Share action) that need to attach proof of license to a
/// request without a license.matteshot.app round trip. `None` unless
/// `status()` would currently return `Licensed`, so a caller never attaches
/// a certificate this device already knows is stale or mismatched.
pub fn signed_certificate() -> Option<(String, String)> {
    let state = load_state().map_err(|error| {
        crate::diagnostics::log(&format!(
            "signed license certificate could not be loaded: {error:#}"
        ));
        error
    }).ok()?;
    let stored = state.license.as_ref()?;
    verify(stored, &device_id()).ok()?;
    Some((stored.certificate.clone(), stored.signature.clone()))
}

/// Begin the trial after the first completed capture. Calling this again never
/// moves the start date forward.
static TRIAL_SYNC_SPAWNED: AtomicBool = AtomicBool::new(false);

pub fn record_successful_capture() {
    if matches!(status(), Status::Licensed { .. }) {
        return;
    }
    let _guard = crate::state_lock::lock(LICENSE_MUTEX).ok();
    let Ok(mut state) = load_state() else {
        crate::diagnostics::log(
            "capture state update skipped because license state could not be loaded",
        );
        return;
    };
    let has_server_trial = state.trial.is_some();
    let began_now = state.trial_started_at.is_none() && registry_time(REGISTRY_TRIAL_START).is_none();
    let now = Utc::now().timestamp();
    let started =
        earliest(state.trial_started_at, registry_time(REGISTRY_TRIAL_START)).unwrap_or(now);
    state.trial_started_at = Some(started);
    state.last_seen_at = Some(now.max(state.last_seen_at.unwrap_or(now)));
    set_registry_time(REGISTRY_TRIAL_START, started);
    set_registry_time(REGISTRY_LAST_SEEN, state.last_seen_at.unwrap_or(now));
    let _ = save_state(&state);
    crate::telemetry::report("matteshot_capture");
    if began_now {
        crate::telemetry::report("matteshot_trial_started");
    }
    // Lock the start date in server-side now that the trial exists, so a
    // wiped state can be recovered. At most one spawned sync per launch.
    if !has_server_trial && !TRIAL_SYNC_SPAWNED.swap(true, Ordering::SeqCst) {
        std::thread::spawn(|| {
            let _ = sync_trial_once();
        });
    }
}

pub fn activate(license_key: &str) -> Result<Status> {
    let license_key = license_key.trim();
    if license_key.is_empty() || license_key.len() > 200 {
        bail!("Enter the license key from your purchase email.");
    }

    let device = device_id();
    let request = ActivateRequest {
        license_key: license_key.to_owned(),
        device_id: device.clone(),
        device_name: device_name(),
        app_version: env!("CARGO_PKG_VERSION"),
    };
    let (status_code, response) = post_json(LICENSE_PATH_ACTIVATE, &serde_json::to_vec(&request)?)?;
    if status_code != 200 {
        bail!(
            "{}",
            response_error(&response, "The license could not be activated.")
        );
    }

    let response: ActivationResponse =
        serde_json::from_slice(&response).context("read activation response")?;
    if !response.activated {
        bail!("The license could not be activated.");
    }
    let stored = StoredLicense::new(
        response.certificate,
        response.signature,
        response.refresh_token,
    );
    let certificate = verify(&stored, &device).context("verify activation certificate")?;
    let _guard = crate::state_lock::lock(LICENSE_MUTEX)?;
    let mut state = load_state_for_activation()?;
    close_trial_after_activation(&mut state, Utc::now().timestamp());
    state.license = Some(stored);
    save_state(&state)?;
    crate::telemetry::report("matteshot_license_activated");
    Ok(Status::Licensed {
        customer_email: certificate.customer_email,
        updates_until: certificate.updates_until,
    })
}

pub fn refresh_once() -> Result<Status> {
    let stored = {
        let _guard = crate::state_lock::lock(LICENSE_MUTEX)?;
        load_state()?
            .license
            .context("Matteshot is not activated")?
    };
    let request = SessionRequest {
        refresh_token: stored
            .token()
            .context("the stored session credential cannot be read on this machine")?,
        device_id: device_id(),
    };
    let (status_code, response) = post_json(LICENSE_PATH_REFRESH, &serde_json::to_vec(&request)?)?;
    if status_code == 403 {
        let _guard = crate::state_lock::lock(LICENSE_MUTEX)?;
        let mut state = load_state()?;
        if same_activation(&state, &stored) {
            state.license = None;
            save_state(&state)?;
        }
        // The server just rejected this activation, so the sidecar's copy of
        // its token is equally dead.
        remove_pre_dpapi_backup();
        bail!(
            "{}",
            response_error(&response, "This activation is no longer valid.")
        );
    }
    if status_code != 200 {
        bail!(
            "{}",
            response_error(&response, "The license could not be refreshed.")
        );
    }

    let response: ActivationResponse =
        serde_json::from_slice(&response).context("read refresh response")?;
    let replacement = StoredLicense::new(
        response.certificate,
        response.signature,
        response.refresh_token,
    );
    let certificate = verify(&replacement, &device_id())?;
    let _guard = crate::state_lock::lock(LICENSE_MUTEX)?;
    let mut state = load_state()?;
    if !same_activation(&state, &stored) {
        bail!("activation changed while it was being refreshed");
    }
    state.license = Some(replacement);
    save_state(&state)?;
    // The refresh rotated the token server-side, so the pre-migration
    // sidecar now holds a dead credential: no downgrade can use it, and
    // keeping plaintext around past its usefulness was never the deal.
    remove_pre_dpapi_backup();
    Ok(Status::Licensed {
        customer_email: certificate.customer_email,
        updates_until: certificate.updates_until,
    })
}

/// The earliest local time that suggests the trial has begun, if any. Absent
/// on a machine that has never completed a capture.
fn trial_grace_start() -> Option<i64> {
    let state = load_state().map_err(|error| {
        crate::diagnostics::log(&format!("trial state could not be loaded: {error:#}"));
        error
    }).ok()?;
    earliest(state.trial_started_at, registry_time(REGISTRY_TRIAL_START))
}

fn store_trial_certificate(stored: StoredTrial, device: &str) -> Result<()> {
    verify_trial(&stored, device).context("verify server trial certificate")?;
    let _guard = crate::state_lock::lock(LICENSE_MUTEX)?;
    let mut state = load_state()?;
    state.trial = Some(stored);
    save_state(&state)
}

/// Recover or record the server-authoritative trial start for this device.
///
/// When license.matteshot.app already knows this device it returns the
/// original signed start, undoing a local state wipe. When the device is
/// unknown but a capture has already happened, the local start is sent up and
/// locked in. Machines that have never captured keep the trial unstarted.
fn sync_trial_once() -> Result<()> {
    let device = device_id();
    let request = TrialStatusRequest {
        device_id: device.clone(),
        app_version: env!("CARGO_PKG_VERSION"),
    };
    let (status_code, body) =
        post_json(LICENSE_PATH_TRIAL_STATUS, &serde_json::to_vec(&request)?)?;
    if status_code != 200 {
        bail!("trial status returned HTTP {status_code}");
    }
    let response: TrialResponse = serde_json::from_slice(&body).context("read trial status")?;
    if let (Some(certificate), Some(signature)) = (response.certificate, response.signature) {
        return store_trial_certificate(
            StoredTrial {
                certificate,
                signature,
            },
            &device,
        );
    }
    if response.trial.as_deref() == Some("none") {
        let Some(started) = trial_grace_start() else {
            return Ok(());
        };
        let started_at = DateTime::from_timestamp(started, 0)
            .context("trial start is out of range")?
            .to_rfc3339();
        let request = TrialStartRequest {
            device_id: device.clone(),
            device_name: device_name(),
            app_version: env!("CARGO_PKG_VERSION"),
            started_at: Some(started_at),
        };
        let (status_code, body) =
            post_json(LICENSE_PATH_TRIAL_START, &serde_json::to_vec(&request)?)?;
        if status_code != 200 {
            bail!("trial start returned HTTP {status_code}");
        }
        let response: TrialResponse =
            serde_json::from_slice(&body).context("read trial start response")?;
        let stored = StoredTrial {
            certificate: response
                .certificate
                .context("trial start response has no certificate")?,
            signature: response
                .signature
                .context("trial start response has no signature")?,
        };
        store_trial_certificate(stored, &device)?;
    }
    Ok(())
}

pub fn start_background_refresh() {
    // Sync immediately, then hourly, for the whole run. A licensed device
    // re-validates so a refund or revocation lands within the hour instead of
    // waiting for the next launch; an unlicensed one keeps its trial record in
    // step, so a wipe is corrected on the very next tick. Network failures are
    // deliberately ignored: only an explicit 403 from the server drops an
    // activation, so an offline machine is never stranded.
    std::thread::spawn(move || loop {
        if matches!(status(), Status::Licensed { .. }) {
            let _ = refresh_once();
        } else {
            let _ = sync_trial_once();
        }
        std::thread::sleep(SYNC_EVERY);
    });
}

pub fn deactivate() -> Result<()> {
    let stored = {
        let _guard = crate::state_lock::lock(LICENSE_MUTEX)?;
        load_state()?
            .license
            .context("Matteshot is not activated")?
    };
    let request = SessionRequest {
        refresh_token: stored
            .token()
            .context("the stored session credential cannot be read on this machine")?,
        device_id: device_id(),
    };
    let (status_code, response) =
        post_json(LICENSE_PATH_DEACTIVATE, &serde_json::to_vec(&request)?)?;
    if status_code != 200 && status_code != 403 {
        bail!(
            "{}",
            response_error(&response, "The activation could not be released.")
        );
    }
    let _guard = crate::state_lock::lock(LICENSE_MUTEX)?;
    let mut state = load_state()?;
    if same_activation(&state, &stored) {
        state.license = None;
        save_state(&state)?;
    }
    // Deactivation released the seat; the sidecar's token died with it.
    remove_pre_dpapi_backup();
    Ok(())
}

fn verify(stored: &StoredLicense, expected_device: &str) -> Result<Certificate> {
    let public_bytes = STANDARD
        .decode(PUBLIC_KEY_BASE64)
        .context("decode Matteshot license public key")?;
    let public_array: [u8; 32] = public_bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid Matteshot license public key"))?;
    let key = VerifyingKey::from_bytes(&public_array).context("read license public key")?;
    let signature_bytes = URL_SAFE_NO_PAD
        .decode(&stored.signature)
        .context("decode license signature")?;
    let signature = Signature::from_slice(&signature_bytes).context("read license signature")?;
    key.verify(stored.certificate.as_bytes(), &signature)
        .context("license signature is invalid")?;

    let body = URL_SAFE_NO_PAD
        .decode(&stored.certificate)
        .context("decode license certificate")?;
    let certificate: Certificate =
        serde_json::from_slice(&body).context("read license certificate")?;
    if certificate.version != 1 || certificate.kind != "license" {
        bail!("unsupported license certificate");
    }
    if certificate.device_id != expected_device {
        bail!("license belongs to a different device");
    }
    if certificate.license_id.is_empty() || certificate.instance_id.is_empty() {
        bail!("license certificate is incomplete");
    }
    DateTime::parse_from_rfc3339(&certificate.purchased_at)
        .context("license purchase date is invalid")?;
    DateTime::parse_from_rfc3339(&certificate.issued_at)
        .context("license issue date is invalid")?;
    if let Some(value) = &certificate.updates_until {
        DateTime::parse_from_rfc3339(value).context("license update date is invalid")?;
    }
    Ok(certificate)
}

fn verify_trial(stored: &StoredTrial, expected_device: &str) -> Result<TrialCertificate> {
    let public_bytes = STANDARD
        .decode(PUBLIC_KEY_BASE64)
        .context("decode Matteshot license public key")?;
    let public_array: [u8; 32] = public_bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid Matteshot license public key"))?;
    let key = VerifyingKey::from_bytes(&public_array).context("read license public key")?;
    let signature_bytes = URL_SAFE_NO_PAD
        .decode(&stored.signature)
        .context("decode trial signature")?;
    let signature = Signature::from_slice(&signature_bytes).context("read trial signature")?;
    key.verify(stored.certificate.as_bytes(), &signature)
        .context("trial signature is invalid")?;

    let body = URL_SAFE_NO_PAD
        .decode(&stored.certificate)
        .context("decode trial certificate")?;
    let certificate: TrialCertificate =
        serde_json::from_slice(&body).context("read trial certificate")?;
    if certificate.version != 1 || certificate.kind != "trial" {
        bail!("unsupported trial certificate");
    }
    if certificate.device_id != expected_device {
        bail!("trial belongs to a different device");
    }
    DateTime::parse_from_rfc3339(&certificate.started_at)
        .context("trial start date is invalid")?;
    DateTime::parse_from_rfc3339(&certificate.issued_at)
        .context("trial issue date is invalid")?;
    Ok(certificate)
}

/// Stable anonymous device identity: SHA-256 of the machine GUID. The same id
/// powers licensing and telemetry, so an install can be joined to a purchase
/// without ever exposing a machine name or email.
pub fn device_id() -> String {
    let machine_guid = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(
            r"SOFTWARE\Microsoft\Cryptography",
            KEY_READ | KEY_WOW64_64KEY,
        )
        .and_then(|key| key.get_value::<String, _>("MachineGuid"))
        .unwrap_or_else(|_| device_name());
    let mut digest = Sha256::new();
    digest.update(b"matteshot-device-v1\0");
    digest.update(machine_guid.trim().to_ascii_lowercase().as_bytes());
    format!("{:x}", digest.finalize())
}

fn device_name() -> String {
    std::env::var("COMPUTERNAME")
        .ok()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "Windows PC".into())
        .chars()
        .take(64)
        .collect()
}

fn response_error(body: &[u8], fallback: &str) -> String {
    serde_json::from_slice::<ErrorResponse>(body)
        .ok()
        .and_then(|response| response.error)
        .filter(|message| !message.trim().is_empty())
        .unwrap_or_else(|| fallback.to_owned())
}

fn post_json(path: &str, body: &[u8]) -> Result<(u32, Vec<u8>)> {
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
            "open license connection",
        )?;
        WinHttpSetTimeouts(session.0, 5_000, 5_000, 8_000, 10_000)
            .context("set license connection timeouts")?;
        let host = HSTRING::from(LICENSE_HOST);
        let connection = InternetHandle::new(
            WinHttpConnect(session.0, &host, 443, 0),
            "connect to Matteshot license service",
        )?;
        let method = HSTRING::from("POST");
        let path = HSTRING::from(path);
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
            "open license request",
        )?;
        let headers: Vec<u16> = "Accept: application/json\r\nContent-Type: application/json\r\n"
            .encode_utf16()
            .collect();
        WinHttpSendRequest(
            request.0,
            Some(&headers),
            Some(body.as_ptr() as *const c_void),
            body.len() as u32,
            body.len() as u32,
            0,
        )
        .context("send license request")?;
        WinHttpReceiveResponse(request.0, ptr::null_mut()).context("receive license response")?;

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
        .context("read license response status")?;

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
            .context("read license response")?;
            if read == 0 {
                break;
            }
            if response.len() + read as usize > MAX_RESPONSE_BYTES {
                bail!("license response is too large");
            }
            response.extend_from_slice(&chunk[..read as usize]);
        }
        Ok((status, response))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_state_path(name: &str) -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "matteshot-license-{name}-{}-{unique}.json",
            std::process::id()
        ))
    }

    #[test]
    fn a_missing_license_state_is_a_fresh_install() {
        let path = temporary_state_path("missing");
        let state = load_state_from(&path).expect("missing state uses defaults");
        assert!(state.license.is_none());
        assert!(state.trial.is_none());
        assert!(!path.exists());
    }

    #[test]
    fn corrupt_license_state_is_not_mistaken_for_an_empty_install() {
        let path = temporary_state_path("corrupt");
        std::fs::write(&path, b"{ paid activation interrupted").unwrap();
        assert!(load_state_from(&path).is_err());
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"{ paid activation interrupted"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn verified_activation_can_recover_without_destroying_corrupt_state() {
        let path = temporary_state_path("recover");
        let corrupt = b"{ paid activation interrupted";
        std::fs::write(&path, corrupt).unwrap();

        let state = load_state_for_activation_from(&path)
            .expect("quarantine state after replacement certificate verification");
        assert!(state.license.is_none());
        assert!(!path.exists());

        let backup = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&format!(
                        "{}.corrupt-",
                        path.file_name().unwrap().to_string_lossy()
                    ))
            })
            .expect("corrupt backup");
        assert_eq!(std::fs::read(backup.path()).unwrap(), corrupt);
        let _ = std::fs::remove_file(backup.path());
    }

    #[test]
    fn corrupt_state_never_reuses_a_cached_entitlement() {
        let parse_error: anyhow::Error = serde_json::from_str::<State>("{")
            .err()
            .expect("invalid state")
            .into();
        let cached = Status::Licensed {
            customer_email: None,
            updates_until: None,
        };
        assert_eq!(
            status_after_load_error(&parse_error, Some(cached)),
            Status::Unavailable
        );
        assert!(!Status::Unavailable.can_capture());
        assert_eq!(
            Status::Unavailable.tray_label(),
            "License state temporarily unavailable"
        );
    }

    #[test]
    fn transient_read_failure_keeps_the_last_verified_status() {
        let io_error: anyhow::Error =
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, "temporarily locked").into();
        let cached = Status::Licensed {
            customer_email: None,
            updates_until: None,
        };
        assert_eq!(
            status_after_load_error(&io_error, Some(cached.clone())),
            cached
        );
    }

    #[test]
    fn the_update_term_is_stated_in_plain_words() {
        let licensed = Status::Licensed {
            customer_email: None,
            updates_until: Some("2027-07-30T00:00:00.000Z".into()),
        };
        assert_eq!(
            licensed.updates_note().as_deref(),
            Some("Updates through 30 July 2027")
        );
    }

    /// Nothing to say when nothing is limited, and the row is hidden instead
    /// of showing an empty or guessed date.
    #[test]
    fn no_term_means_no_note() {
        for status in [
            Status::Licensed {
                customer_email: None,
                updates_until: None,
            },
            Status::Licensed {
                customer_email: None,
                updates_until: Some("not a date".into()),
            },
            Status::Trial { days_left: 7 },
            Status::TrialNotStarted,
            Status::Unavailable,
            Status::Expired,
        ] {
            assert_eq!(status.updates_note(), None, "for {status:?}");
        }
    }

    /// The pre-DPAPI on-disk shape, as older installs still hold it.
    fn stored_legacy(token: &str) -> StoredLicense {
        StoredLicense {
            certificate: "certificate".into(),
            signature: "signature".into(),
            refresh_token: Some(token.into()),
            refresh_token_protected: None,
        }
    }

    #[test]
    fn refresh_results_cannot_replace_a_newer_activation() {
        let mut state = State { license: Some(stored_legacy("new")), ..Default::default() };
        assert!(!same_activation(&state, &stored_legacy("old")));
        assert!(same_activation(&state, &stored_legacy("new")));
        state.license = None;
        assert!(!same_activation(&state, &stored_legacy("new")));
    }

    #[test]
    fn protected_state_never_serializes_the_plaintext_token() {
        let stored = StoredLicense::new("certificate".into(), "signature".into(), "tok-secret-123".into());
        // DPAPI is available for the CI user; a legacy fallback here would
        // silently void the whole protection.
        assert!(stored.refresh_token.is_none(), "token stored in legacy plaintext");
        let json = serde_json::to_string(&State {
            license: Some(stored.clone()),
            ..Default::default()
        })
        .unwrap();
        assert!(!json.contains("tok-secret-123"), "plaintext token reached license.json");
        // The credential still round-trips for this user on this machine.
        assert_eq!(stored.token().as_deref(), Some("tok-secret-123"));
    }

    #[test]
    fn legacy_plaintext_migrates_and_still_matches_its_activation() {
        let migrated = stored_legacy("tok-legacy").into_protected();
        assert!(migrated.refresh_token.is_none());
        assert_eq!(migrated.token().as_deref(), Some("tok-legacy"));
        // Mid-migration, the protected shape and the legacy shape holding the
        // same credential are one activation.
        let state = State { license: Some(migrated), ..Default::default() };
        assert!(same_activation(&state, &stored_legacy("tok-legacy")));
        assert!(!same_activation(&state, &stored_legacy("tok-other")));
    }

    #[test]
    fn migration_leaves_a_downgrade_sidecar_beside_the_protected_state() {
        let path = temporary_state_path("pre-dpapi-sidecar");
        let _ = std::fs::remove_file(&path);
        let legacy = serde_json::to_vec_pretty(&State {
            license: Some(stored_legacy("tok-sidecar")),
            ..Default::default()
        })
        .unwrap();
        std::fs::write(&path, &legacy).unwrap();

        let state = load_state_at(&path).unwrap();
        assert_eq!(
            state.license.as_ref().and_then(StoredLicense::token).as_deref(),
            Some("tok-sidecar")
        );
        // The migrated file no longer carries the plaintext...
        let migrated = std::fs::read_to_string(&path).unwrap();
        assert!(!migrated.contains("tok-sidecar"));
        // ...and the sidecar holds the exact pre-migration bytes, so a build
        // that cannot parse the new shape can be restored by renaming it
        // back. Without this, an 0.18.0 next to a newer build reported
        // "License state temporarily unavailable" with no way home.
        let sidecar = pre_dpapi_backup_path(&path);
        assert_eq!(std::fs::read(&sidecar).unwrap(), legacy);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&sidecar);
    }

    #[test]
    fn the_sidecar_is_retired_once_its_token_is_no_longer_the_live_one() {
        let path = temporary_state_path("pre-dpapi-retire");
        let _ = std::fs::remove_file(&path);
        let sidecar = pre_dpapi_backup_path(&path);
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&State {
                license: Some(stored_legacy("tok-live")),
                ..Default::default()
            })
            .unwrap(),
        )
        .unwrap();

        // Migration leaves the sidecar, and later loads keep it while its
        // token is still the live one: that is the whole point of it.
        load_state_at(&path).unwrap();
        assert!(sidecar.exists());
        load_state_at(&path).unwrap();
        assert!(sidecar.exists(), "a still-live sidecar was retired early");

        // A refresh rotated the token server-side (or a delete failed at the
        // time). The next load notices the sidecar is dead and removes it.
        save_state_at(
            &path,
            &State {
                license: Some(StoredLicense::new(
                    "certificate".into(),
                    "signature".into(),
                    "tok-rotated".into(),
                )),
                ..Default::default()
            },
        )
        .unwrap();
        load_state_at(&path).unwrap();
        assert!(!sidecar.exists(), "a dead sidecar survived the next load");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_undecryptable_live_token_says_nothing_so_the_sidecar_stays() {
        let path = temporary_state_path("pre-dpapi-undecryptable");
        let _ = std::fs::remove_file(&path);
        let sidecar = pre_dpapi_backup_path(&path);
        std::fs::write(
            &sidecar,
            serde_json::to_vec_pretty(&State {
                license: Some(stored_legacy("tok-live")),
                ..Default::default()
            })
            .unwrap(),
        )
        .unwrap();
        // The live blob cannot be decrypted here — a copied profile or a
        // damaged blob. Whether the sidecar's token still works is unknown,
        // and it may be the only copy that does, so it is not retired.
        save_state_at(
            &path,
            &State {
                license: Some(StoredLicense {
                    certificate: "certificate".into(),
                    signature: "signature".into(),
                    refresh_token: None,
                    refresh_token_protected: Some(STANDARD.encode(b"not a dpapi blob")),
                }),
                ..Default::default()
            },
        )
        .unwrap();
        load_state_at(&path).unwrap();
        assert!(sidecar.exists(), "the sidecar was retired on a guess");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&sidecar);
    }

    #[test]
    fn a_sidecar_that_cannot_be_deleted_yet_is_reported_and_retried() {
        use std::os::windows::fs::OpenOptionsExt;

        let path = temporary_state_path("pre-dpapi-locked");
        let _ = std::fs::remove_file(&path);
        let sidecar = pre_dpapi_backup_path(&path);
        std::fs::write(&sidecar, b"{}").unwrap();
        // Deactivated: no live license at all, so the sidecar is dead weight.
        save_state_at(&path, &State::default()).unwrap();

        // Something else holds the file with no sharing at all — the shape
        // of a scanner or indexer mid-read.
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&sidecar)
            .unwrap();
        assert!(remove_pre_dpapi_backup_at(&path).is_err());
        load_state_at(&path).unwrap();
        assert!(sidecar.exists(), "the locked sidecar cannot have gone anywhere");
        drop(lock);

        // The next load after the lock clears finishes the job.
        load_state_at(&path).unwrap();
        assert!(!sidecar.exists(), "the retry never removed the sidecar");
        assert!(matches!(remove_pre_dpapi_backup_at(&path), Ok(false)));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_undecryptable_blob_is_no_session_rather_than_a_crash() {
        let damaged = StoredLicense {
            certificate: "certificate".into(),
            signature: "signature".into(),
            refresh_token: None,
            refresh_token_protected: Some(STANDARD.encode(b"not a dpapi blob")),
        };
        assert_eq!(damaged.token(), None);
        // Damaged state can never prove it owns an activation, so it must not
        // be able to replace or release one.
        let state = State { license: Some(damaged.clone()), ..Default::default() };
        assert!(!same_activation(&state, &damaged));
    }

    #[test]
    fn trial_does_not_begin_until_first_capture() {
        assert_eq!(trial_status_at(None, None, 1_000), Status::TrialNotStarted);
    }

    #[test]
    fn trial_counts_partial_days_up() {
        assert_eq!(
            trial_status_at(Some(1_000), None, 1_001),
            Status::Trial { days_left: 14 }
        );
        assert_eq!(
            trial_status_at(Some(1_000), None, 1_000 + 13 * 86_400 + 1),
            Status::Trial { days_left: 1 }
        );
    }

    #[test]
    fn trial_expires_after_fourteen_days() {
        assert_eq!(
            trial_status_at(Some(1_000), None, 1_000 + TRIAL_SECONDS),
            Status::Expired
        );
    }

    #[test]
    fn clock_rollback_does_not_restore_trial_time() {
        assert_eq!(
            trial_status_at(Some(1_000), Some(1_000 + TRIAL_SECONDS), 1_000 + 86_400),
            Status::Expired
        );
    }

    #[test]
    fn a_signed_server_timestamp_survives_a_wiped_local_floor() {
        // Both local floors deleted and the clock rolled back to the trial
        // start. The re-signed certificate's issue date is the only survivor,
        // and it alone has to keep the trial expired.
        let started = 1_000;
        let issued = started + TRIAL_SECONDS;
        let floor = latest(latest(None, None), Some(issued));
        assert_eq!(
            trial_status_at(Some(started), floor, started + 60),
            Status::Expired
        );
    }

    #[test]
    fn a_stale_certificate_never_lowers_the_local_floor() {
        // A replayed old certificate must not undo a newer local seen-at.
        let started = 1_000;
        let local = Some(started + TRIAL_SECONDS);
        let floor = latest(local, Some(started + 60));
        assert_eq!(floor, local);
        assert_eq!(
            trial_status_at(Some(started), floor, started + 60),
            Status::Expired
        );
    }

    #[test]
    fn a_certificate_never_moves_the_start_later_than_local_state() {
        // After a refund the server may have no trial record for a device that
        // bought outright, so its certificate can carry a start of "now". The
        // local record of a spent trial has to win.
        let spent = 1_000;
        let signed_start = spent + 30 * 86_400;
        let started = earliest(Some(signed_start), Some(spent)).unwrap_or(signed_start);
        assert_eq!(started, spent);
        assert_eq!(
            trial_status_at(Some(started), Some(signed_start), signed_start),
            Status::Expired
        );
    }

    #[test]
    fn a_certificate_still_restores_a_start_that_local_state_lost() {
        let signed_start = 1_000;
        let started = earliest(Some(signed_start), None).unwrap_or(signed_start);
        assert_eq!(started, signed_start);
        assert_eq!(
            trial_status_at(Some(started), None, signed_start + TRIAL_SECONDS),
            Status::Expired
        );
    }

    #[cfg(feature = "debug-license")]
    #[test]
    fn the_license_override_parses_every_documented_form() {
        // Serialized against the other override test: std::env is process-wide.
        let _guard = crate::state_lock::lock("Local\\Matteshot.Test.LicenseOverride").ok();
        let cases = [
            ("not-started", Status::TrialNotStarted),
            ("trial", Status::Trial { days_left: 14 }),
            ("trial:3", Status::Trial { days_left: 3 }),
            (" trial : 3 ", Status::Trial { days_left: 3 }),
            ("trial:0", Status::Expired),
            ("expired", Status::Expired),
            (
                "licensed",
                Status::Licensed {
                    customer_email: None,
                    updates_until: None,
                },
            ),
            (
                "licensed:person@example.com",
                Status::Licensed {
                    customer_email: Some("person@example.com".into()),
                    updates_until: None,
                },
            ),
        ];
        for (value, expected) in cases {
            std::env::set_var("MATTESHOT_LICENSE_OVERRIDE", value);
            assert_eq!(debug_override(), Some(expected), "override {value:?}");
        }
        std::env::remove_var("MATTESHOT_LICENSE_OVERRIDE");
    }

    #[cfg(feature = "debug-license")]
    #[test]
    fn an_unknown_override_falls_back_to_the_real_state() {
        let _guard = crate::state_lock::lock("Local\\Matteshot.Test.LicenseOverride").ok();
        // A malformed day count is a typo, not a request for the default.
        for value in [
            "", "nonsense", "trial-ish", "Expired", "trial:abc", "trial:", "trial:-1", "trial:1e3",
        ] {
            std::env::set_var("MATTESHOT_LICENSE_OVERRIDE", value);
            assert_eq!(debug_override(), None, "override {value:?} must be ignored");
        }
        std::env::remove_var("MATTESHOT_LICENSE_OVERRIDE");
    }

    #[test]
    fn tampered_or_wrong_device_trial_certificate_is_rejected() {
        let stored = StoredTrial {
            certificate: "not-a-certificate".into(),
            signature: "not-a-signature".into(),
        };
        assert!(verify_trial(&stored, "device").is_err());
    }
}
