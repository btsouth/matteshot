# Microsoft Store listing: Matteshot

What to paste into Partner Center. Matteshot lists as a **Win32 (EXE) app**: the Store points at the same signed installer as direct downloads and winget, and the app keeps updating itself. No repackaging and no MSIX.

## Steps

1. [Partner Center](https://partner.microsoft.com/dashboard) > Apps and games > **New product > EXE or MSI app**.
2. Reserve the name **Matteshot**.
3. Fill in the sections below and submit for certification. Updates after the first submission only need the installer URL changed.

## Packages

| Field | Value |
|---|---|
| Installer URL | `https://download.matteshot.app/MatteshotSetup-<version>.exe` |
| Architecture | x64 |
| Language | English (United States) |
| Silent install switches | `/VERYSILENT /NORESTART` |
| App type | Free |
| Who handles updates | The app updates itself (signed in-app updater) |

When a new version ships, change the installer URL to the new versioned `MatteshotSetup-<version>.exe`. Versioned URLs never change, the same contract winget relies on.

## Properties

| Field | Value |
|---|---|
| Category | Utilities & tools |
| Privacy policy URL | `https://matteshot.app/privacy` |
| Website | `https://matteshot.app` |
| Support contact | `https://github.com/btsouth/matteshot/issues` |
| Terms of use | `https://github.com/btsouth/matteshot#license` |
| Pricing | Free |

Age ratings questionnaire: no objectionable content in any category, rated for all ages.

## Store listing

**Description:**

> Press PrtScn and Matteshot instantly turns any window, region, scrolling page, or screen into six polished looks ready to paste. Select and copy only the text you need directly from a screenshot with offline OCR, annotate or redact images, and record, trim, and annotate video. Native to Windows. Everything stays on your PC. Free and open source, with no account.

**What's new:** the release notes for the version the installer URL points at.

**Screenshots:** `screenshots/01-hero.png` through `07-private-native.png`, 1366x818 PNG (above the Store's 1366x768 minimum), in that order. Regenerate them with `python marketing/build_store_screenshots.py`.

**Store logos:** 1:1 PNGs from `assets/icon-256.png` (300x300 works everywhere).

## Notes

- The installer is per-user (no UAC) and code-signed; certification's automated install test runs the silent switches above.
- `AppPublisher` in the installer is "Southbound Software".
- The same installer is on winget as `SouthboundSoftware.Matteshot`.
