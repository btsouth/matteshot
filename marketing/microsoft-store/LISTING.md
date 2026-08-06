# Microsoft Store listing — Matteshot

Everything to paste into Partner Center. Matteshot lists as a **Win32 (EXE)
app**: the download is free, the 14-day trial and the Paddle checkout live in
the app exactly as they do for direct downloads, and Microsoft takes no cut of
purchases made outside Store billing. No repackaging, no MSIX, no Store
licensing work.

## Steps

1. [Partner Center](https://partner.microsoft.com/dashboard) → Apps and games →
   **New product → EXE or MSI app**.
2. Reserve the name **Matteshot**.
3. Fill the sections below, submit for certification. First-time EXE
   certification typically takes a few business days; updates after that are
   faster because the installer URL is what the Store keeps checking.

## Packages

| Field | Value |
|---|---|
| Installer URL | `https://download.matteshot.app/MatteshotSetup-0.14.10.exe` |
| Architecture | x64 |
| Language | English (United States) |
| Silent install switches | `/VERYSILENT /NORESTART` |
| App type | Free trial in-app; purchase handled by the app |
| Who handles updates | The app updates itself (signed in-app updater) |

When a new version ships, update the installer URL to the new
`MatteshotSetup-<version>.exe` in Partner Center. The URL is versioned and
permanent, the same contract winget relies on.

## Properties

| Field | Value |
|---|---|
| Category | Utilities & tools |
| Privacy policy URL | `https://matteshot.app/privacy` |
| Website | `https://matteshot.app` |
| Support contact | `support@matteshot.app` |
| Terms of use | `https://matteshot.app/terms` |
| Pricing | Free (purchase happens in-app via our merchant of record) |

Age ratings questionnaire: no objectionable content in any category → rated
for all ages.

## Store listing

**Description** (the 500-character one, shared with Product Hunt):

> Press PrtScn and Matteshot instantly turns any window, region, scrolling
> page, or screen into six polished looks ready to paste. Select and copy only
> the text you need directly from a screenshot with offline OCR, annotate or
> redact images, and record, trim, and annotate video. Native to Windows.
> Everything stays local. Free 14-day trial, then one $19 purchase. No
> subscription.

**What's new** (first submission): `First Microsoft Store release.`

**Screenshots**: use `screenshots/01-hero.jpg` through `07-private-native.jpg`
in this folder — 1366×818, above the Store's 1366×768 minimum, rendered from
the Product Hunt gallery. Order them the same way the PH gallery does: hero,
six looks, photo editor, select text, video editor, capture more, private and
native.

**Store logos**: reuse the app icon set from `assets/matteshot.ico` /
`thumbnail-240.png` sources; Partner Center asks for 1:1 PNGs (300×300 works
everywhere).

## Notes

- The installer is per-user (no UAC) and code-signed; certification's
  automated install test runs the silent switches above.
- `AppPublisher` in the installer currently says "SouthForge AI" (template
  leftover). Fix to "Southbound Software" in `installer/matteshot.iss` with the
  next release; harmless for certification but worth aligning.
- winget submission for the same binary:
  <https://github.com/microsoft/winget-pkgs/pull/413210> — package id
  `SouthboundSoftware.Matteshot`.
