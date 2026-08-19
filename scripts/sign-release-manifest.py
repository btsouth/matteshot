#!/usr/bin/env python3
"""Sign a Matteshot release record (SBS-747).

Reads MATTESHOT_RELEASE_SIGNING_KEY as standard-base64 32-byte Ed25519 seed.
Writes the JSON document the updater verifies. The signed payload is the
canonical line format in src/release_manifest.rs, not the JSON wrapper.

  python3 scripts/sign-release-manifest.py \\
    --version 0.21.0 \\
    --url https://download.matteshot.app/MatteshotSetup-0.21.0.exe \\
    --length 1234567 \\
    --sha256 <hex> \\
    --key-id 2026.1 \\
    --output MatteshotSetup-0.21.0.exe.release.json
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


def find_openssl() -> str:
    for candidate in (
        "openssl",
        r"C:\Program Files\Git\usr\bin\openssl.exe",
        r"C:\Program Files\OpenSSL-Win64\bin\openssl.exe",
    ):
        try:
            subprocess.run(
                [candidate, "version"],
                check=True,
                capture_output=True,
            )
            return candidate
        except (FileNotFoundError, subprocess.CalledProcessError):
            continue
    raise SystemExit(
        "openssl is required to sign a release record and was not found on PATH."
    )


def sign(payload: bytes, seed: bytes) -> bytes:
    openssl = find_openssl()
    with tempfile.TemporaryDirectory() as tmp:
        pem_path = Path(tmp) / "key.pem"
        payload_path = Path(tmp) / "payload.bin"
        sig_path = Path(tmp) / "payload.sig"
        pem_path.write_text(pem_from_seed(seed), encoding="ascii")
        payload_path.write_bytes(payload)
        subprocess.run(
            [
                openssl,
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
            check=True,
        )
        signature = sig_path.read_bytes()
    if len(signature) != 64:
        raise SystemExit(f"openssl produced a {len(signature)}-byte signature, expected 64")
    return signature


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True)
    parser.add_argument("--url", required=True)
    parser.add_argument("--length", required=True, type=int)
    parser.add_argument("--sha256", required=True)
    parser.add_argument("--key-id", default="2026.1")
    parser.add_argument("--output", "-o")
    args = parser.parse_args()

    sha256 = args.sha256.strip().lower()
    if len(sha256) != 64 or any(c not in "0123456789abcdef" for c in sha256):
        raise SystemExit("--sha256 must be 64 lowercase hex characters")
    if args.length <= 0:
        raise SystemExit("--length must be a positive byte count")
    if not args.url.startswith("https://"):
        raise SystemExit("--url must be HTTPS")

    raw = os.environ.get("MATTESHOT_RELEASE_SIGNING_KEY", "").strip()
    if not raw:
        raise SystemExit("MATTESHOT_RELEASE_SIGNING_KEY is not set")
    try:
        seed = base64.b64decode(raw, validate=True)
    except Exception as error:
        raise SystemExit(f"MATTESHOT_RELEASE_SIGNING_KEY is not standard base64: {error}") from error

    payload = canonical_payload(args.key_id, args.version, args.url, args.length, sha256)
    signature = base64.b64encode(sign(payload, seed)).decode("ascii")
    document = {
        "v": 1,
        "key_id": args.key_id,
        "version": args.version,
        "url": args.url,
        "length": args.length,
        "sha256": sha256,
        "signature": signature,
    }
    encoded = json.dumps(document, separators=(",", ":"), ensure_ascii=True) + "\n"
    if args.output:
        Path(args.output).write_text(encoded, encoding="ascii")
    else:
        sys.stdout.write(encoded)


if __name__ == "__main__":
    main()
