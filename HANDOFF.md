# Matteshot — agent handoff

Written 2026-07-30 by the previous agent (Claude) for whoever picks this up.
Owner: Brandon Tyler South, who goes by **Tyler** (the code-signing certificate
reads `CN=Brandon South`, which is the legal name and is deliberate — the
updater pins that exact string). Works in IT/cybersecurity; this is his
commercial side project. Repo: `tsouth89/matteshot` (private). Everything below
is current as of v0.12.0.

## What this is

A premium screenshot + recording tool for Windows, sold as a one-time purchase.
Core loop: **PrtScn → screen freezes → click a window or drag a region → six
auto-styled "mattes" + the plain shot appear in a picker → the preselected one
is ALREADY on the clipboard (auto-copy) → picking another switches it.** Zero
effort premium output is the product; editing exists but is secondary.
Read `README.md` for the full feature map and architecture — it is accurate.

- Pricing (decided): **$19 one-time, $12.99 launch month.** Free during beta.
- Positioning: "CleanShot X for Windows" ($29/seat on Mac, 500k+ claimed users,
  no serious Windows equivalent).
- Site: https://matteshot.app (live). Download: https://download.matteshot.app/MatteshotSetup.exe (live, signed).

## Infra map (all live)

| Thing | Where | Notes |
|---|---|---|
| App repo | github.com/tsouth89/matteshot (private) | gh CLI has two accounts; **active must be tsouth89**, repos use `credential.helper = !gh auth git-credential` |
| Site repo | github.com/tsouth89/matteshot-site (private) | static, `public/` dir |
| Hosting | Cloudflare Pages, project `matteshot` | deploy: `npx wrangler pages deploy public --project-name matteshot --branch main` from the site repo. Wrangler is OAuth-authed locally. |
| Domain | matteshot.app, zone `771cdfef44652b2f2e10751563682e19` on CF account `330df221b1237f6154e7e4ce9452a81b` | **After every Pages deploy the custom domain can serve stale HTML — purge the zone cache** (Cloudflare API purge_everything). Deployment URLs (`<hash>.matteshot.pages.dev`) are always fresh. |
| Downloads | R2 bucket `matteshot-downloads`, custom domain `download.matteshot.app` | upload: `npx wrangler r2 object put "matteshot-downloads/<name>" --file <f> --content-type application/octet-stream --remote` |
| Code signing | Azure Trusted Signing, account `southforgesigning`, profile `conduit`, endpoint `https://eus.codesigning.azure.net/` | via GitHub Actions OIDC only (federated credential on Entra app `35f2e38f-8e1d-43a3-b4e2-c3b0be34b0e0`, tenant `b87fd204-c1aa-47fb-a84c-2e89f6ec5073`). Credential subject: `repo:tsouth89@258147599/matteshot@1317701781:environment:release`. |
| Release CI | `.github/workflows/release.yml` | push tag `v*` → build → sign exe → Inno installer (version from tag) → sign installer → verify → GitHub release → R2 publish (skips itself until the `CLOUDFLARE_R2_API_TOKEN` secret exists — see TODO). Signing config lives in GitHub **environment `release`** variables (not repo vars). |
| Version endpoint | https://matteshot.app/version.json | `{version, url, download, notes}` — for the future in-app update check |

## Release process

1. Bump `version` in `Cargo.toml` and the fallback in `installer/matteshot.iss`.
2. Commit, push, `git tag vX.Y.Z && git push origin vX.Y.Z`.
3. CI produces the signed installer on the GitHub release. Verify locally:
   `Get-AuthenticodeSignature` must be `Valid`, signer `CN=Brandon South`.
4. R2 publishing is automatic — the `CLOUDFLARE_R2_API_TOKEN` secret exists in
   the `release` environment. CI uploads both `MatteshotSetup-X.Y.Z.exe` and
   the stable `MatteshotSetup.exe`, then re-downloads the public URLs and fails
   the release if an installer and its checksum disagree.
5. Update `public/version.json` in the site repo and deploy. **`download` must
   point at the VERSIONED installer**: the stable name is mutable and can be
   momentarily out of step with its checksum, which would fail every client's
   hash gate. The custom domain can lag a Pages deploy by ~15s.

**v0.12.0 is fully shipped**: signed, uploaded to R2 (versioned + stable, verified matching its published checksum on the public URL), version.json bumped to 0.12.0 pointing at the versioned installer. Nothing in flight.

**The app updates itself.** `installer.rs` downloads the signed installer,
requires the published SHA-256 to match and Authenticode to be valid with the
subject `Brandon South`, then runs it `/VERYSILENT` under `SW_HIDE`. Verified
end to end: 0.11.0 updated itself to 0.11.1 unattended with no window shown.
Test flags: `--update-test`, `--update-stage-test [url]`,
`--verify-signature-test <exe>`, `--update-install-now [url]` (this one really
installs).

## The app icon / brand

The icon IS a matte shot: white card on a violet/teal/magenta aurora.
`matteshot --icon assets` regenerates `assets/matteshot.ico` + `icon-256.png`
using the product's own compose pipeline (hand-tuned blobs in `src/icon.rs`).
`build.rs` (winres) embeds it in the exe; tray + windows load resource id 1
with a runtime-drawn fallback; installer uses `SetupIconFile`. The site favicon
(`matteshot-site/public/favicon.svg`) is a hand-matched SVG of the same mark.
Keep all three in sync if the design changes.

## Build & test on Brandon's machine (Win11 Insider 26220, 2 monitors, 1440p)

- **The resident app locks the exe.** Always: `Stop-Process -Name matteshot` →
  `cargo build --release` → `Start-Process .\target\release\matteshot.exe` in
  ONE command, or PrtScn dies on his machine and he notices fast.
- GUI subsystem: the exe prints NOTHING to a console. Test flags must be run
  with `Start-Process -RedirectStandardError <log> -Wait`, then read the log.
- Test flags: `--bench <title>` (timed capture, raw PNG to %TEMP%),
  `--overlay-bench [batched|sequential]` (headless multi-monitor freeze/layer timing),
  `--scroll-test <title>` (+ env `MATTESHOT_SCROLL_DEBUG=1`), `--record-test`,
  `--trim-test`, `--ocr`, `--assets <title> <dir>` (marketing exports),
  `--icon [dir]`, `--settings`, `--tweak`, `--once [--window <t>] [--pick N]`.
- **Run `.\scripts\run-probes.ps1` before calling anything done.** It runs every
  headless probe (capture, OCR, playback, all three export paths, the
  auto-update trust gates) and fails loudly. It is safe mid-work: no clipboard,
  no injected input, no windows. Two silent breakages got shipped for want of
  this — `--video-edit-test` was broken for weeks, and clippy was red through a
  release. `reliability-soak.ps1` covers the static side (test, clippy, audit);
  **CI gates on `cargo clippy -- -D warnings`, so build+test passing locally is
  not enough.** `.\scripts\verify-code.ps1` is the shared code gate used by CI,
  signed-candidate builds, and tagged releases: all tests, strict Clippy, then a
  release build. Use `-SkipReleaseBuild` only while the resident owns the local
  release binary. The surfaces that must remain human-driven are covered by
  `INTERACTIVE-REGRESSION.md`; a failed or ambiguous row blocks release.
- **NEVER run `--once --pick N` or anything that writes his clipboard** while
  he's active; he has complained about test artifacts on his clipboard.
- **Never inject keyboard/mouse input during his work hours** — foreground
  locks break the tests and keys leak into his apps. He tests by hand.
- `MATTESHOT_THEME=dark|light` overrides the theme for testing.
- Win11 Notepad is tabbed (title matching can grab the wrong tab); use
  Calculator or a fresh file for capture tests. His Pictures dir is
  OneDrive-redirected.
- Don't leave test values in his real config (`%APPDATA%\matteshot\config.json`);
  revert in the same turn.

## Windows landmines

All documented in `README.md` under "Windows landmines" — read them before
touching capture, PrtScn, Media Foundation, or window management. Highlights:
signing action needs ROOTED paths; MF AAC takes only 16-bit PCM at 44.1/48k;
never assume MF stream 0 is video; `FindWindowW` can't see our toolwindows;
after `TrackPopupMenu` the process loses foreground permission (new windows
must SetWindowPos topmost→notopmost to surface); AdjustWindowRectEx always.

## TODO queue (in priority order)

1. **Cache purge is still refused** even after the permission was granted, so
   something about the token is still wrong: it may not be the same token as
   the `CLOUDFLARE_R2_API_TOKEN` secret in the `release` environment, or the
   grant may not cover zone `771cdfef44652b2f2e10751563682e19`. The workflow
   now prints the token status and the Cloudflare error, so the next release
   says why. **This is no longer urgent**: R2 uploads set a 60s TTL on the
   stable objects, and v0.12.0 verified the public stable URL and its checksum
   agreed immediately after publishing. Purging only removes the short wait.
   Auto-update never depended on it — it reads the immutable versioned URL.
2. **winget manifest**: unblocked (signed installer, versioned public URL,
   published sha256). `winget-pkgs` PR: package id `SouthForgeAI.Matteshot`,
   installer type inno, VERSIONED R2 URL (winget requires stable per-version
   URLs + sha256). Local clone of `winget-pkgs` is next to this repo.
3. **version.json is still hand-maintained** in the site repo after each
   release, and it must point `download` at the VERSIONED installer or
   auto-update breaks on edge caching. Worth folding into the release workflow.
5. **Stripe checkout + license keys**: $19 one-time (price `$12.99` launch
   coupon). Suggested shape: Stripe Payment Link or Checkout → webhook on a
   Cloudflare Worker → generate license key (signed token), email via Resend
   (Brandon has a 10-domain Resend account; see his memory notes), validate
   offline in-app (ed25519 signature check). Keep the beta free until he says
   launch.
6. **Marketing prep** (task #14): demo GIF/video of the core loop, PH launch
   kit, beta wave. Marketing asset library: `marketing/` in this repo (matte
   exports, UI shots, scroll captures — all generated via `--assets`).
7. Backlog ideas already discussed: hotkey customization UI, tweak-editor
   polish, DPI supersampling via IddCx virtual monitor (moat, big), CDP
   integration for true-2x browser captures.

## Brandon's working style (matters)

- He wants "an extremely full featured and polished product before release" —
  quality over speed, but he ships fast and tests personally.
- Report failures plainly; he reacts well to "found it, fixed it, here's proof".
- No AI-sounding copy anywhere public: no em dashes, no "genuinely/curious/
  honestly", short plain sentences (see his global CLAUDE.md rules).
- When blocked, don't invent new projects; finish the current one.
- He gives feedback in bursts mid-work; fold it in immediately.
