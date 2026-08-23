#!/usr/bin/env python3
"""Sign or verify a Matteshot release record (SBS-747).

Reads MATTESHOT_RELEASE_SIGNING_KEY as standard-base64 32-byte Ed25519 seed.
Writes the JSON document the updater verifies. The signed payload is the
canonical line format in src/release_manifest.rs, not the JSON wrapper.

Signing always verifies the result against the embedded public key for
--key-id, so a seed that is not 2026.1 cannot publish. OpenSSL 3 is required
(pkeyutl -rawin); Git's usr\\bin\\openssl.exe is preferred.

  python3 scripts/sign-release-manifest.py \\
    --version 0.21.0 \\
    --url https://download.matteshot.app/MatteshotSetup-0.21.0.exe \\
    --length 1234567 \\
    --sha256 <hex> \\
    --key-id 2026.1 \\
    --output MatteshotSetup-0.21.0.exe.release.json

  python3 scripts/sign-release-manifest.py --verify MatteshotSetup-0.21.0.exe.release.json
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path


CANONICAL = (
    "MATTESHOT-RELEASE-v1\n"
    "schema=1\n"
    "key_id={key_id}\n"
    "version={version}\n"
    "url={url}\n"
    "length={length}\n"
    "sha256={sha256}\n"
)

PKCS8_PREFIX = bytes.fromhex("302e020100300506032b657004220420")
SPKI_PREFIX = bytes.fromhex("302a300506032b6570032100")

# Same strings compiled into src/release_manifest.rs. A seed that does not
# produce the 2026.1 key must not publish. production_key_2026_1_is_trusted
# fails CI if this dict drifts from PROD_RELEASE_KEYS (SBS-1046).
EMBEDDED_PUBLIC_KEYS = {
    "2026.1": "JludjKQ0arQ6IRN5dQqncMzc8IoLeFFXFoI8oDelfqo=",
    "test.1": "0EqyMnQrtKs6E2i9RhXk5tAiSrcaAWuvhSCjMsl3hzc=",
}


def canonical_payload(key_id: str, version: str, url: str, length: int, sha256: str) -> bytes:
    return CANONICAL.format(
        key_id=key_id,
        version=version,
        url=url,
        length=length,
        sha256=sha256,
    ).encode("ascii")


def pem_from_seed(seed: bytes) -> str:
    if len(seed) != 32:
        raise SystemExit("MATTESHOT_RELEASE_SIGNING_KEY must be a 32-byte Ed25519 seed")
    der = PKCS8_PREFIX + seed
    b64 = base64.b64encode(der).decode("ascii")
    return f"-----BEGIN PRIVATE KEY-----\n{b64}\n-----END PRIVATE KEY-----\n"


def public_pem_from_raw(public_key: bytes) -> str:
    if len(public_key) != 32:
        raise SystemExit(f"embedded public key is {len(public_key)} bytes, expected 32")
    der = SPKI_PREFIX + public_key
    b64 = base64.b64encode(der).decode("ascii")
    return f"-----BEGIN PUBLIC KEY-----\n{b64}\n-----END PUBLIC KEY-----\n"


def embedded_public_key(key_id: str) -> bytes:
    b64 = EMBEDDED_PUBLIC_KEYS.get(key_id)
    if not b64:
        raise SystemExit(f"key_id {key_id} is not an embedded release key")
    try:
        raw = base64.b64decode(b64, validate=True)
    except Exception as error:
        raise SystemExit(f"embedded public key for {key_id} is not standard base64: {error}") from error
    if len(raw) != 32:
        raise SystemExit(f"embedded public key for {key_id} is {len(raw)} bytes, expected 32")
    return raw


def openssl_help_text(openssl: str) -> str:
    try:
        proc = subprocess.run(
            [openssl, "pkeyutl", "-help"],
            capture_output=True,
            check=False,
        )
    except FileNotFoundError:
        return ""
    return (proc.stdout or b"").decode("ascii", errors="replace") + (
        proc.stderr or b""
    ).decode("ascii", errors="replace")


def openssl_supports_rawin(openssl: str) -> bool:
    return "-rawin" in openssl_help_text(openssl)


def git_openssl_candidates() -> list[str]:
    roots = [
        os.environ.get("ProgramFiles") or r"C:\Program Files",
        os.environ.get("ProgramFiles(x86)") or r"C:\Program Files (x86)",
    ]
    seen: list[str] = []
    for root in roots:
        path = os.path.join(root, "Git", "usr", "bin", "openssl.exe")
        if path not in seen:
            seen.append(path)
        win64 = os.path.join(root, "OpenSSL-Win64", "bin", "openssl.exe")
        if win64 not in seen:
            seen.append(win64)
    return seen


def find_openssl(explicit: str | None = None) -> str:
    if explicit:
        if openssl_supports_rawin(explicit):
            return explicit
        raise SystemExit(
            f"{explicit} does not support pkeyutl -rawin (OpenSSL 3 is required)"
        )
    env = os.environ.get("OPENSSL_BIN", "").strip()
    if env:
        if openssl_supports_rawin(env):
            return env
        raise SystemExit(
            f"OPENSSL_BIN={env} does not support pkeyutl -rawin (OpenSSL 3 is required)"
        )
    for candidate in [*git_openssl_candidates(), "openssl"]:
        if openssl_supports_rawin(candidate):
            return candidate
    raise SystemExit(
        "OpenSSL 3 with pkeyutl -rawin is required to sign a release record. "
        "Install Git for Windows so usr\\bin\\openssl.exe is present, or set OPENSSL_BIN."
    )


def run_openssl(openssl: str, args: list[str], what: str) -> bytes:
    try:
        proc = subprocess.run(
            [openssl, *args],
            capture_output=True,
            check=False,
        )
    except FileNotFoundError as error:
        raise SystemExit(f"{openssl} was not found") from error
    if proc.returncode != 0:
        detail = (proc.stderr or proc.stdout or b"").decode("ascii", errors="replace").strip()
        raise SystemExit(f"{what} failed: {detail or f'exit {proc.returncode}'}")
    return proc.stdout


def public_key_from_pem(openssl: str, pem_path: Path) -> bytes:
    der = run_openssl(
        openssl,
        ["pkey", "-in", str(pem_path), "-pubout", "-outform", "DER"],
        "openssl pkey -pubout",
    )
    if not der.startswith(SPKI_PREFIX) or len(der) != len(SPKI_PREFIX) + 32:
        raise SystemExit("openssl did not emit an Ed25519 SubjectPublicKeyInfo")
    return der[len(SPKI_PREFIX) :]


def require_seed_matches_embedded(openssl: str, pem_path: Path, key_id: str) -> bytes:
    expected = embedded_public_key(key_id)
    actual = public_key_from_pem(openssl, pem_path)
    if actual != expected:
        raise SystemExit(
            f"MATTESHOT_RELEASE_SIGNING_KEY does not match the embedded {key_id} public key"
        )
    return expected


def verify_signature(openssl: str, payload: bytes, signature: bytes, public_key: bytes) -> None:
    if len(signature) != 64:
        raise SystemExit(f"signature is {len(signature)} bytes, expected 64")
    with tempfile.TemporaryDirectory() as tmp:
        pub_path = Path(tmp) / "pub.pem"
        payload_path = Path(tmp) / "payload.bin"
        sig_path = Path(tmp) / "payload.sig"
        pub_path.write_text(public_pem_from_raw(public_key), encoding="ascii")
        payload_path.write_bytes(payload)
        sig_path.write_bytes(signature)
        run_openssl(
            openssl,
            [
                "pkeyutl",
                "-verify",
                "-pubin",
                "-inkey",
                str(pub_path),
                "-rawin",
                "-in",
                str(payload_path),
                "-sigfile",
                str(sig_path),
            ],
            "openssl pkeyutl -verify against the embedded public key",
        )


def sign(openssl: str, payload: bytes, seed: bytes, key_id: str) -> bytes:
    with tempfile.TemporaryDirectory() as tmp:
        pem_path = Path(tmp) / "key.pem"
        payload_path = Path(tmp) / "payload.bin"
        sig_path = Path(tmp) / "payload.sig"
        pem_path.write_text(pem_from_seed(seed), encoding="ascii")
        public_key = require_seed_matches_embedded(openssl, pem_path, key_id)
        payload_path.write_bytes(payload)
        run_openssl(
            openssl,
            [
                "pkeyutl",
                "-sign",
                "-inkey",
                str(pem_path),
                "-rawin",
                "-in",
                str(payload_path),
                "-out",
                str(sig_path),
            ],
            "openssl pkeyutl -sign",
        )
        signature = sig_path.read_bytes()
    if len(signature) != 64:
        raise SystemExit(f"openssl produced a {len(signature)}-byte signature, expected 64")
    verify_signature(openssl, payload, signature, public_key)
    return signature


def load_document(path: Path) -> dict:
    try:
        doc = json.loads(path.read_text(encoding="ascii"))
    except Exception as error:
        raise SystemExit(f"{path} is not readable JSON: {error}") from error
    if not isinstance(doc, dict):
        raise SystemExit(f"{path} is not a JSON object")
    return doc


def document_payload_and_signature(doc: dict) -> tuple[bytes, bytes, str]:
    try:
        version = int(doc["v"])
        key_id = str(doc["key_id"])
        rel_version = str(doc["version"])
        url = str(doc["url"])
        length = int(doc["length"])
        sha256 = str(doc["sha256"])
        signature_text = str(doc.get("signature") or "")
    except (KeyError, TypeError, ValueError) as error:
        raise SystemExit(f"release record is incomplete: {error}") from error
    if version != 1:
        raise SystemExit("unsupported release manifest")
    if not signature_text:
        raise SystemExit("release signature is missing")
    try:
        signature = base64.b64decode(signature_text, validate=True)
    except Exception as error:
        raise SystemExit(f"release signature is not standard base64: {error}") from error
    if len(signature) != 64:
        raise SystemExit(f"signature is {len(signature)} bytes, expected 64")
    payload = canonical_payload(key_id, rel_version, url, length, sha256)
    return payload, signature, key_id


def verify_document(openssl: str, doc: dict, args: argparse.Namespace) -> None:
    payload, signature, key_id = document_payload_and_signature(doc)
    if args.expect_key_id and key_id != args.expect_key_id:
        raise SystemExit(f"key_id {key_id} does not match expected {args.expect_key_id}")
    if args.expect_version is not None and str(doc["version"]) != args.expect_version:
        raise SystemExit(f"version {doc['version']} does not match expected {args.expect_version}")
    if args.expect_url is not None and str(doc["url"]) != args.expect_url:
        raise SystemExit(f"url {doc['url']} does not match expected {args.expect_url}")
    if args.expect_length is not None and int(doc["length"]) != args.expect_length:
        raise SystemExit(f"length {doc['length']} does not match expected {args.expect_length}")
    if args.expect_sha256 is not None and str(doc["sha256"]) != args.expect_sha256:
        raise SystemExit(f"sha256 {doc['sha256']} does not match expected {args.expect_sha256}")
    verify_signature(openssl, payload, signature, embedded_public_key(key_id))


def decode_seed() -> bytes:
    raw = os.environ.get("MATTESHOT_RELEASE_SIGNING_KEY", "").strip()
    if not raw:
        raise SystemExit("MATTESHOT_RELEASE_SIGNING_KEY is not set")
    try:
        seed = base64.b64decode(raw, validate=True)
    except Exception as error:
        raise SystemExit(f"MATTESHOT_RELEASE_SIGNING_KEY is not standard base64: {error}") from error
    if len(seed) != 32:
        raise SystemExit("MATTESHOT_RELEASE_SIGNING_KEY must be a 32-byte Ed25519 seed")
    return seed


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--verify",
        metavar="FILE",
        help="Verify an existing .release.json against the embedded public key",
    )
    parser.add_argument("--version")
    parser.add_argument("--url")
    parser.add_argument("--length", type=int)
    parser.add_argument("--sha256")
    parser.add_argument("--key-id", default="2026.1")
    parser.add_argument("--output", "-o")
    parser.add_argument("--openssl", help="OpenSSL 3 binary with pkeyutl -rawin")
    parser.add_argument("--expect-version")
    parser.add_argument("--expect-url")
    parser.add_argument("--expect-length", type=int)
    parser.add_argument("--expect-sha256")
    parser.add_argument("--expect-key-id")
    args = parser.parse_args()

    openssl = find_openssl(args.openssl)

    if args.verify:
        doc = load_document(Path(args.verify))
        verify_document(openssl, doc, args)
        return

    if args.version is None or args.url is None or args.length is None or args.sha256 is None:
        raise SystemExit("sign mode requires --version --url --length --sha256")

    sha256 = args.sha256.strip().lower()
    if len(sha256) != 64 or any(c not in "0123456789abcdef" for c in sha256):
        raise SystemExit("--sha256 must be 64 lowercase hex characters")
    if args.length <= 0:
        raise SystemExit("--length must be a positive byte count")
    if not args.url.startswith("https://"):
        raise SystemExit("--url must be HTTPS")

    payload = canonical_payload(args.key_id, args.version, args.url, args.length, sha256)
    signature = base64.b64encode(sign(openssl, payload, decode_seed(), args.key_id)).decode("ascii")
    document = {
        "v": 1,
        "key_id": args.key_id,
        "version": args.version,
        "url": args.url,
        "length": args.length,
        "sha256": sha256,
        "signature": signature,
    }
    verify_document(openssl, document, args)
    encoded = json.dumps(document, separators=(",", ":"), ensure_ascii=True) + "\n"
    if args.output:
        Path(args.output).write_text(encoded, encoding="ascii")
    else:
        sys.stdout.write(encoded)


if __name__ == "__main__":
    main()
