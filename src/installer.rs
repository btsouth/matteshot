//! Silent self-update: fetch the signed installer, prove it is ours, run it
//! without a single visible window.
//!
//! Nothing here trusts the network. A download is only ever executed after it
//! matches the published SHA-256 *and* carries a valid Authenticode signature
//! whose subject is our own certificate. Those same checks run again
//! immediately before `CreateProcessW`, because the staged file sits unlocked
//! in %TEMP% until apply's idle wait or a tray "install now" click (SBS-911).
//! Any failure leaves the app exactly where it was and falls back to the
//! download page.

use std::ffi::c_void;
use std::io::Write;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use windows::core::{w, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Networking::WinHttp::{
    WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders,
    WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetTimeouts,
    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE, WINHTTP_QUERY_FLAG_NUMBER,
    WINHTTP_QUERY_STATUS_CODE,
};
use windows::Win32::System::Threading::{
    CreateProcessW, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION, STARTF_USESHOWWINDOW, STARTUPINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

/// The only certificate subject we will execute. Trusted Signing issues these
/// with the legal name on the account, which is not the brand name.
const EXPECTED_SIGNER: &str = "Brandon South";

/// An installer is ~3 MB. Anything wildly past that is not our installer.
const MAX_INSTALLER_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TEXT_BYTES: usize = 4 * 1024;

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

/// Split an HTTPS URL into host and path. Anything that is not plain HTTPS is
/// rejected outright rather than coerced.
fn split_https(url: &str) -> Result<(String, String)> {
    let rest = url
        .strip_prefix("https://")
        .context("update URL must be HTTPS")?;
    let (host, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    if host.is_empty() || host.contains('@') || host.contains(':') {
        bail!("update URL host is not usable");
    }
    Ok((host.to_string(), path.to_string()))
}

/// Open a GET and hand back the live request handle once the response is a 200.
fn begin_get(host: &str, path: &str) -> Result<(InternetHandle, InternetHandle, InternetHandle)> {
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
        // Generous read timeout: this is a multi-megabyte body on unknown links.
        WinHttpSetTimeouts(session.0, 10_000, 10_000, 30_000, 120_000)
            .context("set WinHTTP timeouts")?;

        let host_wide = HSTRING::from(host);
        let connection = InternetHandle::new(
            WinHttpConnect(session.0, &host_wide, 443, 0),
            "connect to download host",
        )?;
        let path_wide = HSTRING::from(path);
        let request = InternetHandle::new(
            WinHttpOpenRequest(
                connection.0,
                w!("GET"),
                &path_wide,
                PCWSTR::null(),
                PCWSTR::null(),
                ptr::null(),
                WINHTTP_FLAG_SECURE,
            ),
            "open download request",
        )?;

        let headers: Vec<u16> = "Cache-Control: no-cache\r\n".encode_utf16().collect();
        WinHttpSendRequest(request.0, Some(&headers), None, 0, 0, 0)
            .context("send download request")?;
        WinHttpReceiveResponse(request.0, ptr::null_mut()).context("receive download response")?;

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
        .context("read download response status")?;
        if status != 200 {
            bail!("download returned HTTP {status}");
        }
        Ok((session, connection, request))
    }
}

fn read_chunk(request: &InternetHandle, buffer: &mut [u8]) -> Result<usize> {
    let mut read = 0u32;
    unsafe {
        WinHttpReadData(
            request.0,
            buffer.as_mut_ptr() as *mut c_void,
            buffer.len() as u32,
            &mut read,
        )
        .context("read download body")?;
    }
    Ok(read as usize)
}

/// Small HTTPS GET into a string, for the published checksum file.
fn get_text(url: &str) -> Result<String> {
    let (host, path) = split_https(url)?;
    let (_session, _connection, request) = begin_get(&host, &path)?;
    let mut body = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = read_chunk(&request, &mut chunk)?;
        if read == 0 {
            break;
        }
        if body.len() + read > MAX_TEXT_BYTES {
            bail!("checksum response is too large");
        }
        body.extend_from_slice(&chunk[..read]);
    }
    String::from_utf8(body).context("checksum response is not UTF-8")
}

/// Stream an HTTPS GET to disk, reporting percent when the length is known.
fn download(url: &str, dest: &Path, mut progress: impl FnMut(u32)) -> Result<()> {
    let (host, path) = split_https(url)?;
    let (_session, _connection, request) = begin_get(&host, &path)?;

    // Content-Length is advisory: it drives the progress number only, never
    // the stop condition or the size guard.
    let mut total = 0u32;
    let mut total_size = std::mem::size_of::<u32>() as u32;
    let mut index = 0u32;
    let expected = unsafe {
        windows::Win32::Networking::WinHttp::WinHttpQueryHeaders(
            request.0,
            windows::Win32::Networking::WinHttp::WINHTTP_QUERY_CONTENT_LENGTH
                | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(&mut total as *mut u32 as *mut c_void),
            &mut total_size,
            &mut index,
        )
    }
    .is_ok()
    .then_some(total as u64)
    .filter(|value| *value > 0);

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).context("create download directory")?;
    }
    let mut file = std::fs::File::create(dest).context("create download file")?;
    let mut written: u64 = 0;
    let mut last_percent = u32::MAX;
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let read = read_chunk(&request, &mut chunk)?;
        if read == 0 {
            break;
        }
        written += read as u64;
        if written > MAX_INSTALLER_BYTES {
            bail!("download is larger than any Matteshot installer");
        }
        file.write_all(&chunk[..read]).context("write download")?;
        if let Some(expected) = expected {
            let percent = ((written * 100) / expected).min(100) as u32;
            if percent != last_percent {
                last_percent = percent;
                progress(percent);
            }
        }
    }
    file.flush().context("flush download")?;
    if written == 0 {
        bail!("download was empty");
    }
    Ok(())
}

fn sha256_of(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path).context("open installer for hashing")?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).context("hash installer")?;
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// The published `.sha256` files are `<hex>  <filename>`.
fn expected_hash(body: &str) -> Result<String> {
    let hash = body
        .split_whitespace()
        .next()
        .context("checksum file is empty")?
        .to_ascii_lowercase();
    if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("checksum file does not contain a SHA-256");
    }
    Ok(hash)
}

/// Authenticode check. `WinVerifyTrust` alone only proves *somebody* Windows
/// trusts signed this, so the certificate subject is pinned as well.
pub fn verify_signature(path: &Path) -> Result<()> {
    use windows::Win32::Security::Cryptography::{
        CertCloseStore, CertFindCertificateInStore, CertFreeCertificateContext,
        CertGetNameStringW, CryptMsgClose, CryptMsgGetParam, CryptQueryObject,
        CERT_FIND_SUBJECT_CERT, CERT_INFO, CERT_NAME_SIMPLE_DISPLAY_TYPE,
        CERT_QUERY_CONTENT_FLAG_PKCS7_SIGNED_EMBED, CERT_QUERY_ENCODING_TYPE,
        CERT_QUERY_FORMAT_FLAG_BINARY, CERT_QUERY_OBJECT_FILE, CMSG_SIGNER_INFO,
        CMSG_SIGNER_INFO_PARAM, HCERTSTORE, PKCS_7_ASN_ENCODING, X509_ASN_ENCODING,
    };
    use windows::Win32::Security::WinTrust::{
        WinVerifyTrust, WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA,
        WINTRUST_DATA_0, WINTRUST_FILE_INFO, WTD_CHOICE_FILE, WTD_REVOKE_WHOLECHAIN,
        WTD_STATEACTION_CLOSE, WTD_STATEACTION_VERIFY, WTD_UI_NONE,
    };

    let wide = HSTRING::from(path.as_os_str());
    unsafe {
        let mut file_info = WINTRUST_FILE_INFO {
            cbStruct: std::mem::size_of::<WINTRUST_FILE_INFO>() as u32,
            pcwszFilePath: PCWSTR(wide.as_ptr()),
            hFile: HANDLE::default(),
            pgKnownSubject: ptr::null_mut(),
        };
        let mut data = WINTRUST_DATA {
            cbStruct: std::mem::size_of::<WINTRUST_DATA>() as u32,
            dwUIChoice: WTD_UI_NONE,
            fdwRevocationChecks: WTD_REVOKE_WHOLECHAIN,
            dwUnionChoice: WTD_CHOICE_FILE,
            Anonymous: WINTRUST_DATA_0 { pFile: &mut file_info },
            dwStateAction: WTD_STATEACTION_VERIFY,
            ..Default::default()
        };
        let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
        let status = WinVerifyTrust(None, &mut action, &mut data as *mut _ as *mut c_void);
        data.dwStateAction = WTD_STATEACTION_CLOSE;
        let _ = WinVerifyTrust(None, &mut action, &mut data as *mut _ as *mut c_void);
        if status != 0 {
            bail!("installer signature is not valid (0x{status:08X})");
        }
    }

    // Signature is valid; now confirm whose it is.
    let mut store = HCERTSTORE::default();
    let mut message: *mut c_void = ptr::null_mut();
    unsafe {
        CryptQueryObject(
            CERT_QUERY_OBJECT_FILE,
            wide.as_ptr() as *const c_void,
            CERT_QUERY_CONTENT_FLAG_PKCS7_SIGNED_EMBED,
            CERT_QUERY_FORMAT_FLAG_BINARY,
            0,
            None,
            None,
            None,
            Some(&mut store),
            Some(&mut message),
            None,
        )
        .context("read installer signature")?;
    }

    let result = (|| -> Result<()> {
        let mut needed = 0u32;
        unsafe {
            CryptMsgGetParam(message, CMSG_SIGNER_INFO_PARAM, 0, None, &mut needed)
                .context("size signer info")?;
        }
        let mut buffer = vec![0u8; needed as usize];
        unsafe {
            CryptMsgGetParam(
                message,
                CMSG_SIGNER_INFO_PARAM,
                0,
                Some(buffer.as_mut_ptr() as *mut c_void),
                &mut needed,
            )
            .context("read signer info")?;
        }
        let signer = unsafe { &*(buffer.as_ptr() as *const CMSG_SIGNER_INFO) };
        let mut info = CERT_INFO {
            Issuer: signer.Issuer,
            SerialNumber: signer.SerialNumber,
            ..Default::default()
        };
        let context = unsafe {
            CertFindCertificateInStore(
                store,
                CERT_QUERY_ENCODING_TYPE(X509_ASN_ENCODING.0 | PKCS_7_ASN_ENCODING.0),
                0,
                CERT_FIND_SUBJECT_CERT,
                Some(&mut info as *mut _ as *const c_void),
                None,
            )
        };
        if context.is_null() {
            bail!("installer signature has no matching certificate");
        }

        let length =
            unsafe { CertGetNameStringW(context, CERT_NAME_SIMPLE_DISPLAY_TYPE, 0, None, None) };
        let mut name = vec![0u16; length as usize];
        unsafe {
            CertGetNameStringW(
                context,
                CERT_NAME_SIMPLE_DISPLAY_TYPE,
                0,
                None,
                Some(&mut name),
            );
            let _ = CertFreeCertificateContext(Some(context));
        }
        let subject = String::from_utf16_lossy(&name)
            .trim_end_matches('\0')
            .to_string();
        if subject != EXPECTED_SIGNER {
            bail!("installer is signed by {subject:?}, not Matteshot");
        }
        Ok(())
    })();

    unsafe {
        let _ = CryptMsgClose(Some(message));
        let _ = CertCloseStore(store, 0);
    }
    result
}

/// Where a pending download lives. Keyed by version so a stale partial from a
/// previous attempt can never be mistaken for the current one.
pub fn staged_path(version: &str) -> PathBuf {
    // The version is already semver-parsed before it gets here, so this is
    // defence in depth. Separators are dropped and dot runs are collapsed, so
    // no input can produce a relative path component.
    let mut safe = String::new();
    for c in version.chars() {
        if c.is_ascii_alphanumeric() {
            safe.push(c);
        } else if c == '.' && !safe.ends_with('.') && !safe.is_empty() {
            safe.push('.');
        }
    }
    let safe = safe.trim_matches('.');
    let name = if safe.is_empty() { "pending" } else { safe };
    std::env::temp_dir().join(format!("MatteshotSetup-{name}.exe"))
}

/// SHA-256 sidecar written next to a staged installer. Launch re-reads this
/// so a replacement in %TEMP% cannot keep the hash we checked at stage time.
fn hash_sidecar_path(installer: &Path) -> PathBuf {
    let mut sidecar = installer.as_os_str().to_os_string();
    sidecar.push(".sha256");
    PathBuf::from(sidecar)
}

fn write_staged_hash(installer: &Path, hash: &str) -> Result<()> {
    let dest = hash_sidecar_path(installer);
    let mut partial = dest.as_os_str().to_os_string();
    partial.push(".partial");
    let partial = PathBuf::from(partial);
    std::fs::write(&partial, format!("{hash}\n")).context("write staged hash")?;
    let _ = std::fs::remove_file(&dest);
    std::fs::rename(&partial, &dest).context("stage verified hash")?;
    Ok(())
}

fn read_staged_hash(installer: &Path) -> Result<String> {
    let dest = hash_sidecar_path(installer);
    match std::fs::read_to_string(&dest) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!("staged installer hash is missing");
        }
        Err(error) => Err(error).context("staged installer hash could not be read"),
        Ok(body) => expected_hash(&body).context("staged installer hash is unreadable"),
    }
}

fn discard_tampered_stage(path: &Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(hash_sidecar_path(path));
}

/// File plus hash sidecar. The menu must not promise "install now" for a
/// leftover installer we can no longer re-check.
pub fn is_ready_to_launch(path: &Path) -> bool {
    path.is_file() && hash_sidecar_path(path).is_file()
}

/// Re-prove the staged installer is still the one we verified.
///
/// `stage` checks hash + Authenticode, then the file sits in shared %TEMP%
/// until apply's idle wait (up to 24h) or a tray "install now" click.
/// Existence is not proof it is still ours (SBS-911).
pub fn verify_still_ours(path: &Path) -> Result<()> {
    if !path.is_file() {
        bail!("staged installer is missing");
    }
    let expected = match read_staged_hash(path) {
        Ok(hash) => hash,
        Err(error) => {
            // Without a sidecar we cannot prove the bytes are still ours.
            discard_tampered_stage(path);
            return Err(error);
        }
    };
    let actual = sha256_of(path)?;
    if actual != expected {
        discard_tampered_stage(path);
        bail!("staged installer hash {actual} does not match staged {expected}");
    }
    // Hash match means the bytes are still what we staged. Authenticode or
    // revocation can fail transiently (OCSP/network); keep the pair so a
    // later retry can still install.
    verify_signature(path)
}

/// Drop only an in-progress download. Dest and its hash sidecar stay so a
/// failed re-download leaves a previously verified pair launchable.
fn drop_in_progress_partial(dest: &Path) {
    let _ = std::fs::remove_file(dest.with_extension("exe.partial"));
}

/// Download the installer, prove it is ours, and leave it staged on disk
/// with the verified hash beside it. Returns the verified path. Never
/// executes anything.
pub fn stage(url: &str, version: &str, progress: impl FnMut(u32)) -> Result<PathBuf> {
    let dest = staged_path(version);
    let partial = dest.with_extension("exe.partial");
    drop_in_progress_partial(&dest);

    download(url, &partial, progress)?;

    let hash = (|| -> Result<String> {
        let published = expected_hash(&get_text(&format!("{url}.sha256"))?)?;
        let actual = sha256_of(&partial)?;
        if actual != published {
            bail!("installer hash {actual} does not match published {published}");
        }
        verify_signature(&partial)?;
        Ok(actual)
    })();

    let hash = match hash {
        Ok(hash) => hash,
        Err(error) => {
            let _ = std::fs::remove_file(&partial);
            return Err(error);
        }
    };

    let _ = std::fs::remove_file(&dest);
    std::fs::rename(&partial, &dest).context("stage verified installer")?;
    if let Err(error) = write_staged_hash(&dest, &hash) {
        let _ = std::fs::remove_file(&dest);
        let _ = std::fs::remove_file(hash_sidecar_path(&dest));
        return Err(error);
    }
    Ok(dest)
}

const INSTALLER_ARGS: &str = "/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /NOCANCEL";

/// Quoted path plus Inno flags. `CreateProcessW` requires a mutable UTF-16
/// buffer; the path is encoded losslessly so `lpCommandLine` matches
/// `lpApplicationName` on non-UTF-8 temp directories.
fn launch_command_line(path: &Path) -> Vec<u16> {
    let mut line = Vec::new();
    line.push(u16::from(b'"'));
    line.extend(path.as_os_str().encode_wide());
    line.push(u16::from(b'"'));
    line.push(u16::from(b' '));
    line.extend(INSTALLER_ARGS.encode_utf16());
    line.push(0);
    line
}

/// Image and command line for `CreateProcessW`. No shell verb: a hijacked
/// `HKCU\Software\Classes\exefile\shell\open\command` cannot interpose.
struct LaunchPlan {
    application_name: PathBuf,
    command_line: Vec<u16>,
    show_window: u16,
}

fn launch_plan(path: &Path) -> LaunchPlan {
    LaunchPlan {
        application_name: path.to_path_buf(),
        command_line: launch_command_line(path),
        show_window: SW_HIDE.0 as u16,
    }
}

/// Run a staged installer with no window of any kind. Inno is a GUI process,
/// so `/VERYSILENT` plus `SW_HIDE` means nothing ever paints; the per-user
/// install directory means no elevation prompt either.
///
/// `CreateProcessW` runs the verified image directly (`lpApplicationName` is
/// the staged path), so a hijacked `.exe` open association cannot interpose.
///
/// The installer stops the resident through `--quit`, replaces the binary, and
/// relaunches it, so this call is the last thing this process usefully does.
///
/// Hash + Authenticode are checked again here, not only at stage time. The
/// staged file is unlocked in %TEMP% for up to a day (SBS-911).
pub fn launch(path: &Path) -> Result<()> {
    verify_still_ours(path)?;
    let plan = launch_plan(path);
    let application = HSTRING::from(plan.application_name.as_os_str());
    let mut command_line = plan.command_line;
    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        dwFlags: STARTF_USESHOWWINDOW,
        wShowWindow: plan.show_window,
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    unsafe {
        CreateProcessW(
            &application,
            PWSTR(command_line.as_mut_ptr()),
            None,
            None,
            false,
            PROCESS_CREATION_FLAGS(0),
            None,
            None,
            &startup,
            &mut process,
        )
        .context("could not start the installer")?;
        // This process is about to be replaced; do not wait.
        let _ = CloseHandle(process.hThread);
        let _ = CloseHandle(process.hProcess);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_https_urls_are_accepted() {
        assert_eq!(
            split_https("https://download.matteshot.app/MatteshotSetup.exe").unwrap(),
            ("download.matteshot.app".into(), "/MatteshotSetup.exe".into())
        );
        assert_eq!(
            split_https("https://matteshot.app").unwrap(),
            ("matteshot.app".into(), "/".into())
        );
        // No plaintext, no credentials in the host, no port redirection.
        assert!(split_https("http://matteshot.app/x.exe").is_err());
        assert!(split_https("https://evil@matteshot.app/x.exe").is_err());
        assert!(split_https("https://matteshot.app:8080/x.exe").is_err());
        assert!(split_https("file:///C:/x.exe").is_err());
    }

    #[test]
    fn published_checksum_files_parse_and_bad_ones_do_not() {
        let good = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  MatteshotSetup.exe";
        assert_eq!(
            expected_hash(good).unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert!(expected_hash("").is_err());
        assert!(expected_hash("nothex  file.exe").is_err());
        // Truncated hash must not be accepted as a prefix match.
        assert!(expected_hash("e3b0c442  file.exe").is_err());
    }

    #[test]
    fn staged_path_cannot_be_steered_by_a_hostile_version_string() {
        let path = staged_path("../../windows/system32/evil");
        assert_eq!(
            path.parent().unwrap(),
            std::env::temp_dir(),
            "staging must stay in TEMP"
        );
        assert!(!path.to_string_lossy().contains(".."));
        assert_eq!(
            staged_path("0.10.0").file_name().unwrap(),
            "MatteshotSetup-0.10.0.exe"
        );
        // An all-punctuation version still yields one usable filename.
        assert_eq!(
            staged_path("../..").file_name().unwrap(),
            "MatteshotSetup-pending.exe"
        );
    }

    fn utf16_cstr(buf: &[u16]) -> String {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }

    #[test]
    fn launch_command_line_quotes_paths_with_spaces_and_keeps_inno_flags() {
        let path = Path::new(r"C:\Users\Tyler South\AppData\Local\Temp\MatteshotSetup-0.19.0.exe");
        let line = utf16_cstr(&launch_command_line(path));
        assert_eq!(
            line,
            r#""C:\Users\Tyler South\AppData\Local\Temp\MatteshotSetup-0.19.0.exe" /VERYSILENT /SUPPRESSMSGBOXES /NORESTART /NOCANCEL"#
        );
    }

    #[test]
    fn launch_command_line_preserves_non_unicode_path_units() {
        use std::ffi::OsString;
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        let mut wide: Vec<u16> = OsString::from(r"C:\Users\").encode_wide().collect();
        wide.push(0xD800); // unpaired surrogate, not valid UTF-8/WTF-8
        wide.extend(OsString::from(r"\AppData\Local\Temp\MatteshotSetup.exe").encode_wide());
        let path = PathBuf::from(OsString::from_wide(&wide));
        let line = launch_command_line(&path);
        assert_eq!(line.first().copied(), Some(u16::from(b'"')));
        let quoted = &line[1..];
        assert!(
            quoted.windows(wide.len()).any(|window| window == wide),
            "lossy Display must not replace unpaired surrogates in lpCommandLine"
        );
        let as_text = utf16_cstr(&line);
        assert!(as_text.contains(INSTALLER_ARGS), "{as_text}");
    }

    #[test]
    fn launch_plan_uses_createprocess_so_hkcu_exefile_open_cannot_interpose() {
        // CreateProcessW(lpApplicationName = staged path) so a hijacked
        // HKCU\Software\Classes\exefile\shell\open\command cannot interpose.
        let path = Path::new(r"C:\Users\Tyler South\AppData\Local\Temp\MatteshotSetup-0.19.0.exe");
        let plan = launch_plan(path);
        assert_eq!(plan.application_name.as_path(), path);
        assert_eq!(plan.show_window, SW_HIDE.0 as u16);
        let line = utf16_cstr(&plan.command_line);
        assert!(
            !line.split_whitespace().any(|token| token == "open"),
            "launch must not carry a shell verb: {line}"
        );
    }

    #[test]
    fn launch_errors_when_the_staged_installer_is_missing() {
        let missing =
            std::env::temp_dir().join("MatteshotSetup-missing-sbs-858-does-not-exist.exe");
        let _ = std::fs::remove_file(&missing);
        let error = launch(&missing).unwrap_err().to_string();
        assert!(error.contains("staged installer is missing"), "{error}");
    }

    fn sbs_911_temp(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "MatteshotSetup-sbs-911-{label}-{}.exe",
            std::process::id()
        ))
    }

    fn cleanup_staged(path: &Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(hash_sidecar_path(path));
    }

    /// Pins SBS-911: the sidecar lives next to the installer, not under a
    /// path the version string can steer.
    #[test]
    fn hash_sidecar_sits_beside_the_installer() {
        let path = Path::new(r"C:\Users\Tyler South\AppData\Local\Temp\MatteshotSetup-0.20.0.exe");
        assert_eq!(
            hash_sidecar_path(path).file_name().unwrap(),
            "MatteshotSetup-0.20.0.exe.sha256"
        );
        assert_eq!(
            hash_sidecar_path(path).parent().unwrap(),
            path.parent().unwrap()
        );
    }

    /// Pins SBS-911: a leftover installer without its hash is not "ready".
    #[test]
    fn is_ready_to_launch_requires_both_the_installer_and_its_hash() {
        let path = sbs_911_temp("ready");
        cleanup_staged(&path);
        assert!(!is_ready_to_launch(&path));
        std::fs::write(&path, b"not-an-installer").unwrap();
        assert!(
            !is_ready_to_launch(&path),
            "file without sidecar is not ready"
        );
        write_staged_hash(
            &path,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        )
        .unwrap();
        assert!(is_ready_to_launch(&path));
        cleanup_staged(&path);
    }

    /// Pins SBS-911: missing vs garbage sidecar are both refused, and are
    /// not collapsed into one message.
    #[test]
    fn read_staged_hash_keeps_missing_and_unreadable_distinct() {
        let path = sbs_911_temp("hash-states");
        cleanup_staged(&path);
        let missing = read_staged_hash(&path).unwrap_err().to_string();
        assert!(missing.contains("hash is missing"), "{missing}");
        assert!(!missing.contains("unreadable"), "{missing}");

        std::fs::write(hash_sidecar_path(&path), "not-a-sha256\n").unwrap();
        let garbage = read_staged_hash(&path).unwrap_err().to_string();
        assert!(garbage.contains("unreadable"), "{garbage}");
        assert!(!garbage.contains("hash is missing"), "{garbage}");
        cleanup_staged(&path);
    }

    /// Pins SBS-911: launch must not CreateProcessW a file that is merely
    /// present. The old gate was `is_file()` only.
    #[test]
    fn launch_refuses_a_present_installer_with_no_hash_sidecar() {
        let path = sbs_911_temp("no-sidecar");
        cleanup_staged(&path);
        std::fs::write(&path, b"not-an-installer").unwrap();
        let error = launch(&path).unwrap_err().to_string();
        assert!(
            error.contains("staged installer hash is missing"),
            "must fail the re-check, not CreateProcessW: {error}"
        );
        assert!(
            !error.contains("could not start the installer"),
            "reached CreateProcessW without a hash re-check: {error}"
        );
        assert!(!path.is_file(), "unprovable installer must be discarded");
        cleanup_staged(&path);
    }

    /// Pins SBS-911: bytes that no longer match the staged hash are
    /// discarded and never launched.
    #[test]
    fn launch_refuses_a_present_installer_whose_hash_changed() {
        let path = sbs_911_temp("hash-changed");
        cleanup_staged(&path);
        std::fs::write(&path, b"not-an-installer").unwrap();
        write_staged_hash(
            &path,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )
        .unwrap();
        let error = launch(&path).unwrap_err().to_string();
        assert!(
            error.contains("does not match staged"),
            "must fail the hash re-check, not CreateProcessW: {error}"
        );
        assert!(
            !error.contains("could not start the installer"),
            "reached CreateProcessW after a hash change: {error}"
        );
        assert!(!path.is_file(), "tampered installer must be discarded");
        assert!(
            !hash_sidecar_path(&path).is_file(),
            "tampered hash sidecar must be discarded"
        );
        cleanup_staged(&path);
    }

    /// Pins SBS-911: a hash match is not enough. Authenticode + subject
    /// must still pass on the bytes about to run.
    #[test]
    fn launch_refuses_a_present_installer_that_fails_authenticode() {
        let path = sbs_911_temp("unsigned");
        cleanup_staged(&path);
        std::fs::write(&path, b"not-an-installer").unwrap();
        let hash = sha256_of(&path).unwrap();
        write_staged_hash(&path, &hash).unwrap();
        let error = launch(&path).unwrap_err().to_string();
        assert!(
            error.contains("signature") || error.contains("signed"),
            "must fail Authenticode, not CreateProcessW: {error}"
        );
        assert!(
            !error.contains("could not start the installer"),
            "reached CreateProcessW without an Authenticode re-check: {error}"
        );
        assert!(
            path.is_file() && hash_sidecar_path(&path).is_file(),
            "hash-verified bytes stay staged when Authenticode is transiently unavailable"
        );
        cleanup_staged(&path);
    }

    /// Pins SBS-911: a failed re-download must not drop the hash sidecar
    /// of a previously verified pair, or Install now disappears.
    #[test]
    fn a_failed_restage_leaves_a_verified_pair_launchable() {
        let path = sbs_911_temp("restage-keep");
        cleanup_staged(&path);
        std::fs::write(&path, b"previously-verified").unwrap();
        write_staged_hash(
            &path,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        )
        .unwrap();
        assert!(is_ready_to_launch(&path));
        std::fs::write(path.with_extension("exe.partial"), b"in-progress").unwrap();
        drop_in_progress_partial(&path);
        assert!(
            is_ready_to_launch(&path),
            "clearing the in-progress partial must not drop dest or its hash"
        );
        assert!(
            !path.with_extension("exe.partial").is_file(),
            "the leftover partial is what restage is allowed to drop"
        );
        cleanup_staged(&path);
    }

    /// Pins SBS-764: the legacy nonzero `--quit` path must still exist, and
    /// it must resolve taskkill through the Windows system directory rather
    /// than by bare name.
    #[test]
    fn installer_legacy_quit_execs_taskkill_from_the_system_directory() {
        let source = include_str!("../installer/matteshot.iss");
        assert!(
            source.contains("if R <> 0 then"),
            "legacy nonzero --quit fallback is missing"
        );
        assert!(
            !source.contains("Exec('taskkill.exe'"),
            "unqualified taskkill.exe is the SBS-764 failure mode"
        );
        assert!(
            source.contains("ExpandConstant('{sys}\\taskkill.exe')"),
            "legacy close must Exec {{sys}}\\taskkill.exe"
        );
    }
}
