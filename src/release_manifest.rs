//! Ed25519-signed release records (SBS-747).
//!
//! Updater authorization is this signature, not a same-origin `.sha256`
//! sidecar and not Authenticode. The record binds version, immutable HTTPS
//! URL, length, and SHA-256 to an embedded release public key. A CDN that
//! also serves the checksum cannot authorize an installer.
//!
//! The signed payload is a fixed line format, not the JSON wrapper, so key
//! order and extra fields cannot change what was signed. Authenticode stays
//! as a Windows-reputation check after this record accepts the bytes.

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::Deserialize;

/// Production release-signing keys. Append the next key here before it
/// starts signing, then drop a retired id only after every supported
/// client has picked up the overlap release.
const PROD_RELEASE_KEYS: &[ReleaseKey] = &[ReleaseKey {
    id: "2026.1",
    public_key_base64: "JludjKQ0arQ6IRN5dQqncMzc8IoLeFFXFoI8oDelfqo=",
}];

#[derive(Clone, Copy, Debug)]
pub struct ReleaseKey {
    pub id: &'static str,
    pub public_key_base64: &'static str,
}

/// Keys this build will accept. Tests inject a well-known test key so
/// installer launch tests can sign without the production seed.
#[cfg(not(test))]
pub fn trusted_release_keys() -> &'static [ReleaseKey] {
    PROD_RELEASE_KEYS
}

#[cfg(test)]
pub fn trusted_release_keys() -> &'static [ReleaseKey] {
    static KEYS: &[ReleaseKey] = &[
        ReleaseKey {
            id: "test.1",
            public_key_base64: "0EqyMnQrtKs6E2i9RhXk5tAiSrcaAWuvhSCjMsl3hzc=",
        },
        ReleaseKey {
            id: "2026.1",
            public_key_base64: "JludjKQ0arQ6IRN5dQqncMzc8IoLeFFXFoI8oDelfqo=",
        },
    ];
    KEYS
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedRelease {
    pub key_id: String,
    pub version: String,
    pub url: String,
    pub length: u64,
    pub sha256: String,
}

#[derive(Deserialize)]
struct ReleaseDocument {
    v: u32,
    key_id: String,
    version: String,
    url: String,
    length: u64,
    sha256: String,
    #[serde(default)]
    signature: Option<String>,
}

/// Bytes that are signed. Field order is fixed; do not sign the JSON.
pub fn canonical_payload(
    key_id: &str,
    version: &str,
    url: &str,
    length: u64,
    sha256: &str,
) -> Vec<u8> {
    format!(
        "MATTESHOT-RELEASE-v1\nschema=1\nkey_id={key_id}\nversion={version}\nurl={url}\nlength={length}\nsha256={sha256}\n"
    )
    .into_bytes()
}

pub fn release_manifest_url(download_url: &str) -> String {
    format!("{download_url}.release.json")
}

fn require_https_url(url: &str) -> Result<()> {
    let rest = url
        .strip_prefix("https://")
        .context("release URL must be HTTPS")?;
    let host = rest.split('/').next().unwrap_or("");
    if host.is_empty() || host.contains('@') || host.contains(':') {
        bail!("release URL host is not usable");
    }
    Ok(())
}

fn require_sha256_hex(hash: &str) -> Result<()> {
    if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("release manifest sha256 is unreadable");
    }
    if hash.chars().any(|c| c.is_ascii_uppercase()) {
        bail!("release manifest sha256 is unreadable");
    }
    Ok(())
}

fn require_version(version: &str) -> Result<()> {
    if version.is_empty() || version.len() > 32 {
        bail!("release version is invalid");
    }
    if version.contains('/') || version.contains('\\') || version.contains("..") {
        bail!("release version is invalid");
    }
    Ok(())
}

fn key_for_id<'a>(keys: &'a [ReleaseKey], key_id: &str) -> Result<&'a ReleaseKey> {
    if keys.is_empty() {
        bail!("release key is not trusted");
    }
    keys.iter()
        .find(|key| key.id == key_id)
        .ok_or_else(|| anyhow::anyhow!("release key is not trusted"))
}

fn verifying_key(key: &ReleaseKey) -> Result<VerifyingKey> {
    let public_bytes = STANDARD
        .decode(key.public_key_base64)
        .context("decode release public key")?;
    let public_array: [u8; 32] = public_bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid release public key"))?;
    VerifyingKey::from_bytes(&public_array).context("read release public key")
}

/// Verify a published `.release.json` body against `keys`.
///
/// Missing, unreadable, untrusted, and invalid signatures are distinct
/// errors. Unknown JSON fields are ignored; they are not in the payload.
pub fn verify_signed_release_with_keys(body: &str, keys: &[ReleaseKey]) -> Result<SignedRelease> {
    let doc: ReleaseDocument = match serde_json::from_str(body) {
        Ok(doc) => doc,
        Err(_) => bail!("release manifest is unreadable"),
    };
    if doc.v != 1 {
        bail!("unsupported release manifest");
    }
    if doc.key_id.is_empty() || doc.version.is_empty() || doc.url.is_empty() {
        bail!("release manifest is incomplete");
    }
    require_version(&doc.version)?;
    require_https_url(&doc.url)?;
    require_sha256_hex(&doc.sha256)?;
    if doc.length == 0 {
        bail!("release length is invalid");
    }

    let signature_text = match doc.signature.as_deref().map(str::trim) {
        None | Some("") => bail!("release signature is missing"),
        Some(text) => text,
    };
    let signature_bytes = match STANDARD.decode(signature_text) {
        Ok(bytes) => bytes,
        Err(_) => bail!("release signature is unreadable"),
    };
    let signature = match Signature::from_slice(&signature_bytes) {
        Ok(signature) => signature,
        Err(_) => bail!("release signature is unreadable"),
    };

    let key = key_for_id(keys, &doc.key_id)?;
    let verifying = verifying_key(key)?;
    let payload = canonical_payload(&doc.key_id, &doc.version, &doc.url, doc.length, &doc.sha256);
    if verifying.verify(&payload, &signature).is_err() {
        bail!("release signature is invalid");
    }

    Ok(SignedRelease {
        key_id: doc.key_id,
        version: doc.version,
        url: doc.url,
        length: doc.length,
        sha256: doc.sha256,
    })
}

pub fn verify_signed_release(body: &str) -> Result<SignedRelease> {
    verify_signed_release_with_keys(body, trusted_release_keys())
}

/// The download we were pointed at must be the one the release key signed.
pub fn bind_download(record: &SignedRelease, url: &str, version: Option<&str>) -> Result<()> {
    if record.url != url {
        bail!("signed release URL does not match download URL");
    }
    if let Some(version) = version {
        if record.version != version {
            bail!(
                "signed release version {} does not match claimed {version}",
                record.version
            );
        }
    }
    Ok(())
}

impl SignedRelease {
    /// Downloaded bytes must be exactly the signed length and digest.
    pub fn require_bytes(&self, sha256_hex: &str, length: u64) -> Result<()> {
        if length != self.length {
            bail!(
                "installer length {length} does not match signed {}",
                self.length
            );
        }
        if sha256_hex != self.sha256 {
            bail!(
                "installer hash {sha256_hex} does not match signed {}",
                self.sha256
            );
        }
        Ok(())
    }
}

#[cfg(test)]
pub const TEST_RELEASE_KEY_ID: &str = "test.1";

#[cfg(test)]
pub const TEST_RELEASE_KEY_SEED: [u8; 32] = [0x11; 32];

#[cfg(test)]
pub fn sign_release_json(
    key_id: &str,
    seed: &[u8; 32],
    version: &str,
    url: &str,
    length: u64,
    sha256: &str,
) -> String {
    use ed25519_dalek::{Signer, SigningKey};
    let signing = SigningKey::from_bytes(seed);
    let payload = canonical_payload(key_id, version, url, length, sha256);
    let signature = STANDARD.encode(signing.sign(&payload).to_bytes());
    serde_json::json!({
        "v": 1,
        "key_id": key_id,
        "version": version,
        "url": url,
        "length": length,
        "sha256": sha256,
        "signature": signature,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://download.matteshot.app/MatteshotSetup-0.21.0.exe";
    const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const NEXT_SEED: [u8; 32] = [0x22; 32];
    const NEXT_PUB: &str = "oJql9HpnWYAv+VX43C0qFKXJnSO+l/hkEn/5ODRVpPA=";

    fn signed(version: &str, url: &str, length: u64, sha256: &str) -> String {
        sign_release_json(
            TEST_RELEASE_KEY_ID,
            &TEST_RELEASE_KEY_SEED,
            version,
            url,
            length,
            sha256,
        )
    }

    fn test_ring() -> Vec<ReleaseKey> {
        vec![ReleaseKey {
            id: TEST_RELEASE_KEY_ID,
            public_key_base64: "0EqyMnQrtKs6E2i9RhXk5tAiSrcaAWuvhSCjMsl3hzc=",
        }]
    }

    fn overlap_ring() -> Vec<ReleaseKey> {
        vec![
            ReleaseKey {
                id: TEST_RELEASE_KEY_ID,
                public_key_base64: "0EqyMnQrtKs6E2i9RhXk5tAiSrcaAWuvhSCjMsl3hzc=",
            },
            ReleaseKey {
                id: "2026.2",
                public_key_base64: NEXT_PUB,
            },
        ]
    }

    /// Pins SBS-747: the signed bytes are this exact line format.
    #[test]
    fn canonical_payload_is_stable() {
        let payload = canonical_payload("2026.1", "0.21.0", URL, 10, HASH);
        assert_eq!(
            String::from_utf8(payload).unwrap(),
            format!(
                "MATTESHOT-RELEASE-v1\nschema=1\nkey_id=2026.1\nversion=0.21.0\nurl={URL}\nlength=10\nsha256={HASH}\n"
            )
        );
    }

    /// Pins SBS-747: a valid record is accepted and exposes the bound fields.
    #[test]
    fn a_valid_signature_authorizes_the_bound_release() {
        let body = signed("0.21.0", URL, 3145728, HASH);
        let record = verify_signed_release_with_keys(&body, &test_ring()).unwrap();
        assert_eq!(record.version, "0.21.0");
        assert_eq!(record.url, URL);
        assert_eq!(record.length, 3145728);
        assert_eq!(record.sha256, HASH);
        assert_eq!(record.key_id, "test.1");
    }

    /// Pins SBS-747: JSON key order is not load-bearing because we sign fields.
    #[test]
    fn json_key_order_does_not_change_what_was_signed() {
        let body = signed("0.21.0", URL, 1, HASH);
        let record = verify_signed_release_with_keys(&body, &test_ring()).unwrap();
        let shuffled = format!(
            r#"{{"sha256":"{HASH}","url":"{URL}","signature":{},"length":1,"key_id":"test.1","version":"0.21.0","v":1}}"#,
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["signature"]
        );
        let again = verify_signed_release_with_keys(&shuffled, &test_ring()).unwrap();
        assert_eq!(record, again);
    }

    /// Pins SBS-747: extra JSON fields cannot authorize anything.
    #[test]
    fn extra_json_fields_are_ignored() {
        let mut value: serde_json::Value =
            serde_json::from_str(&signed("0.21.0", URL, 1, HASH)).unwrap();
        value["trusted"] = serde_json::json!(true);
        value["sha256_sidecar"] = serde_json::json!(HASH);
        verify_signed_release_with_keys(&value.to_string(), &test_ring()).unwrap();
    }

    fn assert_rejects(body: &str, needle: &str) {
        let error = verify_signed_release_with_keys(body, &test_ring())
            .unwrap_err()
            .to_string();
        assert!(error.contains(needle), "{error}");
    }

    /// Pins SBS-747: tampering any bound field invalidates the signature.
    #[test]
    fn tampered_bound_fields_are_rejected() {
        let good: serde_json::Value =
            serde_json::from_str(&signed("0.21.0", URL, 10, HASH)).unwrap();
        let sig = good["signature"].as_str().unwrap().to_owned();

        let mut version = good.clone();
        version["version"] = serde_json::json!("9.9.9");
        assert_rejects(&version.to_string(), "release signature is invalid");

        let mut url = good.clone();
        url["url"] = serde_json::json!("https://evil.example/setup.exe");
        assert_rejects(&url.to_string(), "release signature is invalid");

        let mut length = good.clone();
        length["length"] = serde_json::json!(11);
        assert_rejects(&length.to_string(), "release signature is invalid");

        let mut hash = good.clone();
        hash["sha256"] =
            serde_json::json!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        assert_rejects(&hash.to_string(), "release signature is invalid");

        // The original signature string still belongs to the original fields.
        assert_eq!(good["signature"], sig);
    }

    /// Pins SBS-747: CDN + checksum is not authorization.
    #[test]
    fn a_matching_checksum_without_a_signature_cannot_authorize() {
        let unsigned = format!(
            r#"{{"v":1,"key_id":"test.1","version":"0.21.0","url":"{URL}","length":10,"sha256":"{HASH}"}}"#
        );
        let error = verify_signed_release_with_keys(&unsigned, &test_ring())
            .unwrap_err()
            .to_string();
        assert!(error.contains("release signature is missing"), "{error}");
        assert!(!error.contains("invalid"), "{error}");
        assert!(!error.contains("unreadable"), "{error}");
    }

    /// Pins SBS-747: missing, garbage, and wrong signatures stay distinct.
    #[test]
    fn missing_unreadable_and_invalid_signatures_are_distinct() {
        let missing = format!(
            r#"{{"v":1,"key_id":"test.1","version":"0.21.0","url":"{URL}","length":10,"sha256":"{HASH}","signature":""}}"#
        );
        let missing_err = verify_signed_release_with_keys(&missing, &test_ring())
            .unwrap_err()
            .to_string();
        assert!(
            missing_err.contains("signature is missing"),
            "{missing_err}"
        );

        let garbage = format!(
            r#"{{"v":1,"key_id":"test.1","version":"0.21.0","url":"{URL}","length":10,"sha256":"{HASH}","signature":"not-base64"}}"#
        );
        let garbage_err = verify_signed_release_with_keys(&garbage, &test_ring())
            .unwrap_err()
            .to_string();
        assert!(
            garbage_err.contains("signature is unreadable"),
            "{garbage_err}"
        );
        assert!(!garbage_err.contains("missing"), "{garbage_err}");

        let mut bad: serde_json::Value =
            serde_json::from_str(&signed("0.21.0", URL, 10, HASH)).unwrap();
        bad["signature"] = serde_json::json!(STANDARD.encode([0u8; 64]));
        let invalid = verify_signed_release_with_keys(&bad.to_string(), &test_ring())
            .unwrap_err()
            .to_string();
        assert!(invalid.contains("signature is invalid"), "{invalid}");
        assert!(!invalid.contains("missing"), "{invalid}");
        assert!(!invalid.contains("unreadable"), "{invalid}");

        let unreadable = verify_signed_release_with_keys("not-json", &test_ring())
            .unwrap_err()
            .to_string();
        assert!(unreadable.contains("unreadable"), "{unreadable}");
        assert!(!unreadable.contains("signature"), "{unreadable}");
    }

    /// Pins SBS-747: an unknown or retired key cannot authorize.
    #[test]
    fn unknown_and_retired_keys_are_rejected() {
        let body = signed("0.21.0", URL, 10, HASH);
        let only_next = [ReleaseKey {
            id: "2026.2",
            public_key_base64: NEXT_PUB,
        }];
        let retired = verify_signed_release_with_keys(&body, &only_next)
            .unwrap_err()
            .to_string();
        assert!(retired.contains("release key is not trusted"), "{retired}");

        let empty = verify_signed_release_with_keys(&body, &[])
            .unwrap_err()
            .to_string();
        assert!(empty.contains("release key is not trusted"), "{empty}");

        let mut unknown: serde_json::Value = serde_json::from_str(&body).unwrap();
        unknown["key_id"] = serde_json::json!("evil.1");
        let error = verify_signed_release_with_keys(&unknown.to_string(), &test_ring())
            .unwrap_err()
            .to_string();
        // key_id is in the signed payload, so this is an invalid signature
        // under the claimed id, or an unknown id. Either must refuse.
        assert!(
            error.contains("not trusted") || error.contains("invalid"),
            "{error}"
        );
    }

    /// Pins SBS-747: current and next keys both work during overlap.
    #[test]
    fn production_key_rollover_accepts_current_and_next() {
        let current = signed("0.21.0", URL, 10, HASH);
        verify_signed_release_with_keys(&current, &overlap_ring()).unwrap();

        let next = sign_release_json("2026.2", &NEXT_SEED, "0.21.1", URL, 11, HASH);
        let record = verify_signed_release_with_keys(&next, &overlap_ring()).unwrap();
        assert_eq!(record.key_id, "2026.2");
        assert_eq!(record.version, "0.21.1");
    }

    /// Pins SBS-747: signing with key A but naming key B is refused.
    #[test]
    fn key_id_must_name_the_key_that_produced_the_signature() {
        let body = sign_release_json("2026.2", &TEST_RELEASE_KEY_SEED, "0.21.0", URL, 10, HASH);
        let error = verify_signed_release_with_keys(&body, &overlap_ring())
            .unwrap_err()
            .to_string();
        assert!(error.contains("release signature is invalid"), "{error}");
    }

    /// Pins SBS-747: advertised URL/version must be the signed ones.
    #[test]
    fn bind_download_rejects_a_mismatched_url_or_version() {
        let record =
            verify_signed_release_with_keys(&signed("0.21.0", URL, 10, HASH), &test_ring())
                .unwrap();
        bind_download(&record, URL, Some("0.21.0")).unwrap();
        bind_download(&record, URL, None).unwrap();
        let url = bind_download(&record, "https://evil.example/x.exe", Some("0.21.0"))
            .unwrap_err()
            .to_string();
        assert!(url.contains("does not match download URL"), "{url}");
        let version = bind_download(&record, URL, Some("9.9.9"))
            .unwrap_err()
            .to_string();
        assert!(version.contains("does not match claimed"), "{version}");
    }

    /// Pins SBS-747: downloaded bytes must match the signed length and hash.
    #[test]
    fn require_bytes_rejects_length_or_hash_mismatch() {
        let record =
            verify_signed_release_with_keys(&signed("0.21.0", URL, 10, HASH), &test_ring())
                .unwrap();
        record.require_bytes(HASH, 10).unwrap();
        let length = record.require_bytes(HASH, 9).unwrap_err().to_string();
        assert!(
            length.contains("installer length 9 does not match signed 10"),
            "{length}"
        );
        let hash = record
            .require_bytes(
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                10,
            )
            .unwrap_err()
            .to_string();
        assert!(hash.contains("does not match signed"), "{hash}");
    }

    #[test]
    fn plaintext_and_credentialed_urls_are_rejected() {
        for bad in [
            "http://download.matteshot.app/x.exe",
            "https://evil@download.matteshot.app/x.exe",
            "https://download.matteshot.app:8443/x.exe",
            "file:///C:/x.exe",
        ] {
            let body = signed("0.21.0", bad, 10, HASH);
            assert!(
                verify_signed_release_with_keys(&body, &test_ring()).is_err(),
                "accepted {bad}"
            );
        }
    }

    #[test]
    fn release_manifest_url_is_the_download_url_plus_suffix() {
        assert_eq!(
            release_manifest_url(URL),
            "https://download.matteshot.app/MatteshotSetup-0.21.0.exe.release.json"
        );
    }

    /// Pins SBS-747: the build's trusted list includes the production key.
    #[test]
    fn production_key_2026_1_is_trusted() {
        assert_eq!(PROD_RELEASE_KEYS[0].id, "2026.1");
        assert!(
            trusted_release_keys().iter().any(|key| key.id == "2026.1"
                && key.public_key_base64 == "JludjKQ0arQ6IRN5dQqncMzc8IoLeFFXFoI8oDelfqo="),
            "{:?}",
            trusted_release_keys()
                .iter()
                .map(|k| k.id)
                .collect::<Vec<_>>()
        );
    }

    fn is_windows_apps(path: &std::path::Path) -> bool {
        path.components()
            .any(|c| c.as_os_str().eq_ignore_ascii_case("WindowsApps"))
    }

    fn where_all(name: &str) -> Vec<std::path::PathBuf> {
        let output = std::process::Command::new("where.exe").arg(name).output();
        let Ok(output) = output else {
            return Vec::new();
        };
        if !output.status.success() {
            return Vec::new();
        }
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(std::path::PathBuf::from)
            .filter(|path| !is_windows_apps(path))
            .collect()
    }

    fn python_exe() -> std::path::PathBuf {
        if let Some(from_env) = std::env::var_os("MATTESHOT_PYTHON") {
            let path = std::path::PathBuf::from(from_env);
            if path.is_file() && !is_windows_apps(&path) {
                return path;
            }
        }

        for py in where_all("py") {
            let probe = std::process::Command::new(&py)
                .args(["-3", "-c", "import sys; print(sys.executable)"])
                .output();
            if let Ok(out) = probe {
                if out.status.success() {
                    let exe = String::from_utf8_lossy(&out.stdout);
                    let exe = exe
                        .lines()
                        .map(str::trim)
                        .filter(|line| !line.is_empty())
                        .next_back()
                        .unwrap_or("");
                    let exe = std::path::PathBuf::from(exe);
                    if exe.is_file() && !is_windows_apps(&exe) {
                        return exe;
                    }
                }
            }
        }

        for name in ["python3", "python"] {
            if let Some(path) = where_all(name).into_iter().next() {
                return path;
            }
        }

        panic!(
            "Python 3 is required for the openssl release-signer round-trip (SBS-747). \
             Install Python 3 or set MATTESHOT_PYTHON. WindowsApps stubs are ignored."
        );
    }

    fn signer_script() -> std::path::PathBuf {
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("scripts")
            .join("sign-release-manifest.py");
        assert!(
            script.is_file(),
            "missing release signer {}",
            script.display()
        );
        script
    }

    fn run_signer(
        args: &[&str],
        seed: &[u8; 32],
        output: Option<&std::path::Path>,
    ) -> std::process::Output {
        let mut cmd = std::process::Command::new(python_exe());
        cmd.arg(signer_script()).args(args);
        if let Some(path) = output {
            cmd.arg("--output").arg(path);
        }
        cmd.env("MATTESHOT_RELEASE_SIGNING_KEY", STANDARD.encode(seed))
            .output()
            .unwrap_or_else(|error| panic!("failed to spawn sign-release-manifest.py: {error}"))
    }

    fn require_signer_tools(output: &std::process::Output, context: &str) {
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if combined.contains("pkeyutl -rawin") || combined.contains("OpenSSL 3") {
            panic!("OpenSSL 3 with pkeyutl -rawin is required for {context}: {combined}");
        }
        if combined.to_ascii_lowercase().contains("python")
            && combined.to_ascii_lowercase().contains("not found")
        {
            panic!("Python 3 is required for {context}: {combined}");
        }
    }

    /// Pins SBS-747: CI signs with openssl; clients verify with dalek.
    #[test]
    fn openssl_python_signer_is_accepted_by_the_rust_verifier() {
        let dir = std::env::temp_dir().join(format!(
            "matteshot-openssl-roundtrip-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let output_path = dir.join("MatteshotSetup-0.21.0.exe.release.json");
        let output = run_signer(
            &[
                "--version",
                "0.21.0",
                "--url",
                URL,
                "--length",
                "10",
                "--sha256",
                HASH,
                "--key-id",
                TEST_RELEASE_KEY_ID,
            ],
            &TEST_RELEASE_KEY_SEED,
            Some(&output_path),
        );
        require_signer_tools(&output, "the openssl release-signer round-trip");
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "sign-release-manifest.py failed: status={} stdout={stdout} stderr={stderr}",
            output.status
        );
        let body = std::fs::read_to_string(&output_path).unwrap_or_else(|error| {
            panic!(
                "signed record was not written to {}: {error}",
                output_path.display()
            );
        });
        let record = verify_signed_release(&body).expect("openssl signature must verify in Rust");
        assert_eq!(record.key_id, TEST_RELEASE_KEY_ID);
        assert_eq!(record.version, "0.21.0");
        assert_eq!(record.url, URL);
        assert_eq!(record.length, 10);
        assert_eq!(record.sha256, HASH);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Pins SBS-747: a seed that is not the embedded 2026.1 key cannot sign.
    #[test]
    fn openssl_python_signer_rejects_a_seed_that_is_not_the_embedded_key() {
        let output = run_signer(
            &[
                "--version",
                "0.21.0",
                "--url",
                URL,
                "--length",
                "10",
                "--sha256",
                HASH,
                "--key-id",
                "2026.1",
            ],
            &TEST_RELEASE_KEY_SEED,
            None,
        );
        require_signer_tools(&output, "the openssl release-signer wrong-seed check");
        assert!(
            !output.status.success(),
            "a test seed must not produce a 2026.1 record: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("embedded") || stderr.contains("does not match"),
            "{stderr}"
        );
    }
}
