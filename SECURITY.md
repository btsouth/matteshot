# Security policy

## Reporting a vulnerability

Please report security problems privately through GitHub: open the [Security tab](https://github.com/btsouth/matteshot/security) and choose **Report a vulnerability**. Do not open a public issue for anything that could put users at risk before a fix ships.

Include what you found, the Matteshot version (Settings shows it in the footer, and **Copy diagnostics** gives a full report), your Windows build, and the steps to reproduce it. A proof of concept helps.

You should hear back within a few days. Once a fix is ready it ships as a normal signed release, which installed copies pick up through the auto-updater, and the advisory is published after that. There is no bug bounty.

## Supported versions

Only the latest release gets fixes. Matteshot updates itself, so the latest release is what almost everyone runs.

## What counts

Things we want to hear about include:

- anything that gets the updater to install a file that is not an official release, or to skip the signed release record or the Authenticode check
- a way for another local process or a web page to make Matteshot read, write, delete or upload files it should not
- redaction (pixelate) that can be undone or read through in the exported image or copied OCR text
- memory safety problems in the Win32, WGC or Media Foundation paths
- problems in `share-server/` that let someone upload without the operator's token, read uploads they were not given a link to, or keep content past its expiry

Out of scope: denial of service against your own machine, problems that need an attacker who already has admin rights or your user account, and the behaviour of third-party builds or forks.

## How official builds are protected

- The installer, `matteshot.exe` and the uninstaller are Authenticode-signed with Azure Trusted Signing. The certificate subject is `Brandon South`, and the updater refuses anything signed by someone else.
- Each release also has an Ed25519-signed record (`MatteshotSetup-<version>.exe.release.json`) that binds the version, URL, length and SHA-256 of the installer. The public verification keys are compiled into the app in `src/release_manifest.rs`. The private key and the signing credentials live only in the GitHub Actions `release` environment, which only runs for version tags on commits already merged to `main`.
- To check a download by hand, compare its SHA-256 with the value in the GitHub release notes and run `Get-AuthenticodeSignature` on it in PowerShell. The status should be `Valid` and the signer `Brandon South`.
