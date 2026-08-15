//! Silent self-update: fetch the signed installer, prove it is ours, run it
//! without a single visible window.
//!
//! Nothing here trusts the network. A download is only ever executed after it
//! matches the published SHA-256 *and* carries a valid Authenticode signature
//! whose subject is our own certificate. Any failure leaves the app exactly
//! where it was and falls back to the download page.

use std::ffi::c_void;
use std::io::Write;
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
    let mut file = std::fs::File::open(path).context("open download for hashing")?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).context("hash download")?;
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

/// Download the installer, prove it is ours, and leave it staged on disk.
/// Returns the verified path. Never executes anything.
pub fn stage(url: &str, version: &str, progress: impl FnMut(u32)) -> Result<PathBuf> {
    let dest = staged_path(version);
    let partial = dest.with_extension("exe.partial");
    let _ = std::fs::remove_file(&partial);

    download(url, &partial, progress)?;

    let verified = (|| -> Result<()> {
        let published = expected_hash(&get_text(&format!("{url}.sha256"))?)?;
        let actual = sha256_of(&partial)?;
        if actual != published {
            bail!("installer hash {actual} does not match published {published}");
        }
        verify_signature(&partial)
    })();

    if let Err(error) = verified {
        let _ = std::fs::remove_file(&partial);
        return Err(error);
    }

    let _ = std::fs::remove_file(&dest);
    std::fs::rename(&partial, &dest).context("stage verified installer")?;
    Ok(dest)
}

const INSTALLER_ARGS: &str = "/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /NOCANCEL";

/// Quoted path plus Inno flags. `CreateProcessW` requires a mutable buffer.
fn launch_command_line(path: &Path) -> Vec<u16> {
    format!("\"{}\" {INSTALLER_ARGS}", path.display())
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect()
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
pub fn launch(path: &Path) -> Result<()> {
    if !path.is_file() {
        bail!("staged installer is missing");
    }
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
}
