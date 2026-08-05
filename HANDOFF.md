# Matteshot — agent handoff

Written 2026-07-30 by the previous agent (Claude) for whoever picks this up.
Owner: Brandon Tyler South, who goes by **Tyler** (the code-signing certificate
reads `CN=Brandon South`, which is the legal name and is deliberate — the
updater pins that exact string). Works in IT/cybersecurity; this is his
commercial side project. Repo: `tsouth89/matteshot` (private). Everything below
is current as of v0.14.10.

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
| Version endpoint | https://matteshot.app/version.json | `{version, url, download, released, notes, releases[]}`. `releases` is the full served history, newest first; the app picks the newest entry it is entitled to. |

## Trial licensing (server-authoritative, done both sides)

The trial clock is authoritative on the server so deleting local state cannot
reset it. The client (src/license.rs) already implements this; the Cloudflare
Worker in `matteshot-site/license-worker` implements it too, using the **same
Ed25519 keypair** that signs license certs (client verifies both with the
embedded `PUBLIC_KEY_BASE64`).

- `POST /v1/trial/status` — `{device_id, app_version}`. If a record exists
  return `{certificate, signature}` (the signed start, **never** refreshed
  forward); else `{trial: "none"}`.
- `POST /v1/trial/start` — `{device_id, device_name, app_version,
  started_at?}`. Idempotent: if the device is known return the **existing**
  start; otherwise record `started_at` (client-supplied or server now) and
  return a signed cert. Never moves an existing start forward.
- Trial cert JSON (base64url, Ed25519-signed over the exact bytes, same schema
  shape as license certs): `{"version":1,"kind":"trial","device_id":…,
  "started_at":"RFC3339","issued_at":"RFC3339"}`.
- KV key by `device_id` (same namespace as license activations), stored
  forever. This is what defeats the wipe: device_id is machine-bound
  (MachineGuid), so it survives reinstall/uninstall.
- Residual hole (accepted, document in code): a machine that used the trial
  fully offline and wiped local state **before the server ever saw it** can
  still reset once; anything that ever contacted the server is pinned. Trial
  start stays "first capture", not "first launch" — that is a product decision.
- Client behavior: first capture kicks an immediate sync; unlicensed devices
  re-sync hourly while running; a wiped cert is restored from the server on
  the next sync, so a wipe buys at most the hours until the next tick.

## Analytics (one PostHog project, three sources)

Everything lands in one PostHog project so a visitor can be followed from the
site through purchase into daily app use. Every event is tagged
`product: "matteshot"` and filtered in the "Matteshot" dashboard.

- **Site** (`matteshot-site/public/index.html`): official PostHog snippet
  (pageviews + autocapture) plus manual events `matteshot_signup`,
  `matteshot_demo_played`, `matteshot_scroll_capture_demo`,
  `matteshot_editor_demo_opened`.
- **Purchases** (`matteshot-site/license-worker`): the Lemon Squeezy
  `order_created` webhook forwards `matteshot_purchase` (order id, amounts,
  license key) to PostHog, deduplicated by order id in KV.
- **GitHub downloads** (`matteshot-site/license-worker`): a daily 13:00 UTC
  cron posts `matteshot_github_downloads` with per-asset and total counts.
- **App** (src/telemetry.rs): launch, capture, trial start, license
  activation, OCR, editor, recording, scroll capture, plus
  `matteshot_failure` for the paths that go wrong. `distinct_id` is the
  SHA-256 machine GUID (same id as licensing) and the Settings window can
  opt out; it defaults on.
  - The failure event carries `operation` (capture / record / scroll /
    overlay / update) and `kind`, both from fixed sets. **Error messages are
    never sent**: `failure_kind` returns `&'static str`, so a path or window
    title inside an error context has no route out, and an unrecognised
    failure reports `other`.
  - Adding an event means updating the privacy policy, which lists them by
    name. `every_event_in_the_source_is_a_published_one` fails the build if
    you forget.

PostHog API key: `phc_piwT9huE46Hn8gZxs9X4SvjAHzgQVZGT9QipDWSq7cUx` (publishable;
embedded in the site snippet, `license-worker/wrangler.toml` vars, and
src/telemetry.rs). No event ever carries an email or device name.


## Release process

1. Bump `version` in `Cargo.toml` and the fallback in `installer/matteshot.iss`.
2. Commit, push, `git tag vX.Y.Z && git push origin vX.Y.Z`.
3. CI produces the signed installer on the GitHub release. Verify locally:
   `Get-AuthenticodeSignature` must be `Valid`, signer `CN=Brandon South`.
4. R2 publishing is automatic — the `CLOUDFLARE_R2_API_TOKEN` secret exists in
   the `release` environment. CI uploads both `MatteshotSetup-X.Y.Z.exe` and
   the stable `MatteshotSetup.exe`, then re-downloads the public URLs and fails
   the release if an installer and its checksum disagree.
5. Update `public/version.json` in the site repo and deploy. Add the new build
   to the top of the `releases` array with its real `released` date, and set
   the matching top-level `version`/`download`/`released`. `check-site.mjs`
   gates all of it. **Never remove an entry, and never delete an installer from
   R2**: the update term is enforced by offering a lapsed license the newest
   build its year covered, which may be several versions back. **`download` must
   point at the VERSIONED installer**: the stable name is mutable and can be
   momentarily out of step with its checksum, which would fail every client's
   hash gate. The custom domain can lag a Pages deploy by ~15s.

**v0.14.10 is fully shipped**, as are 0.14.8 and 0.14.9 before it: each signed, uploaded to R2 (versioned + stable), GitHub release published, version.json bumped and deployed. Verified the same way every time: the published SHA-256 matches the GitHub asset digest, and the *installed* build is pointed at the live manifest and made to download and verify the new installer through the app's own trust gates (`--update-test` then `--update-stage-test`), which is the path a customer actually takes.

What went out in each:

- **0.14.8** — telemetry asks before it sends anything (`Config::telemetry` is `Option<bool>`, empty by default across the EU, EEA, UK and Switzerland), and update entitlements are enforced for the first time.
- **0.14.9** — annotation tools stay armed until they are put away, instead of clearing after a single use. Escape peels one layer per press in both editors: caption, then tool, then selection, then close.
- **0.14.10** — the tweak editor previews at the size it displays. It had been composing a fixed 1200px working bitmap and stretching it to fill the pane, so a maximized capture on a 1440p monitor was rebuilt from 47% of its pixels and magnified 1.39x. Text went soft exactly where annotation happens.

Three traps this run, all cheap to hit again:

- **Check `git tag -l` before assuming a prepared version is unreleased.** `Cargo.toml` said 0.14.8 and the working tree looked mid-flight, but v0.14.8 was already tagged and published; the work had to become 0.14.9. `validate-release-version.ps1 -Tag vX.Y.Z` catches the mismatch, and CI refuses the build, but only after you have pushed a tag.
- **`released` in version.json must be UTC.** `git log --date=format-local` without `TZ=UTC` hands back local time, which would have dated 0.14.10 four hours early. That field decides which licenses are offered the build, so it is not cosmetic. Use `TZ=UTC git log -1 --format=%cd --date=format-local:"%Y-%m-%dT%H:%M:%SZ" <commit>`.
- **A publish can look like a failed update for a minute.** Straight after the 0.14.10 deploy, one `--update-stage-test` reported "already current" while checks either side of it offered 0.14.10 — most likely an edge node still serving the previous manifest. Five consecutive runs after were clean. Re-run before believing it.

0.14.10 was also this project's first double-digit patch number, which is where a textual version compare breaks: as strings `"0.14.10" < "0.14.9"`, so every 0.14.9 install would have been told it was current, permanently. `update.rs` uses semver and is fine, and `a_double_digit_patch_is_newer_than_a_single_digit_one` now pins it.

Delayed capture reached from the capture toolbar, not just the tray. A clock chip labelled from `capture_delay_secs`, keyboard **D**. Choosing it ends the overlay with `Selection::Delay`; `shoot_overlay_from` counts down and opens again, as a loop rather than recursion. The reopened toolbar keeps that chip lit, reusing the same `selected` rendering Record and Scroll use, because without it the return reads as a glitch rather than a continuation.

Two traps worth remembering here:

- **A toolbar shortcut must be in BOTH `shortcut_button` and `shortcut_bit`.** The first only applies once the overlay owns the foreground; `shortcut_bit` is what carries the key through the low-level hook when it does not, which is the case the hook exists for. `D` was missing from it and the shortcut silently did nothing. A test now asserts every key is in both tables.
- **Do not call `Config::load()` while the overlay is opening.** It is file I/O under a lock on the path whose latency is logged as `freeze_ms`. Pass values in instead.

- A capture shortcut another app already owns is no longer fatal. Registration used `?`, and with no console `main`'s `eprintln!` went nowhere, so Matteshot exited at launch with no window and no tray icon. It also aborted before PrtScn was acquired, so one collision cost every hotkey and left no way into Settings to fix it.
- The shortcut is configurable (`config.capture_hotkey`, text like `Ctrl+Alt+S`, `None` unbinds) and can be set in Settings by pressing it. Bare keys are refused: Windows would register one and swallow that key system-wide.
- **Delayed capture** (tray item, delay in Settings). Windows suspends hotkey delivery for the duration of a menu's modal loop, so menus and hover flyouts were previously impossible to capture at all. The pill is `WS_EX_NOACTIVATE` so it cannot dismiss what it is waiting for, and excludes itself from capture. `--delay-test [seconds]` exercises it.
- `matteshot_failure` reports what breaks, classified into a fixed set. **Error text is never sent**: `failure_kind` returns `&'static str`. `report_with` refuses any event absent from `PUBLISHED_EVENTS`, because the privacy policy lists them by name.
- Settings is laid out by a cursor rather than ~37 literal coordinates plus 17 more in the paint routine. Adding a row is one call; the window height falls out of where the cursor stops.
- Removed device counts the app cannot know: the Deactivate prompt, the activation-limit error, and the tray's hardcoded `Ctrl+Alt+S` accelerator.

- The trial certificate is re-signed on every `trial/status`, so its `issued_at` is a server timestamp the machine cannot forge. The client folds it into the seen-at floor, which is what stops a rolled-back clock from buying trial days.
- `status()` takes the *earliest* of the signed start and local state, so a certificate can never hand back time a spent trial already used. The worker no longer clamps an old `started_at` to now, which is what created that hole after a refund.
- `refresh` distinguishes "Lemon Squeezy said no" from "no answer": outages map to 502/429 rather than the 403 that makes the app delete an activation. Background refresh now runs all session instead of once at launch.
- The unauthenticated endpoints and `/v1/license/activate` are rate limited per address; bad input answers 4xx instead of 500.

Note: releases are still created as GitHub drafts (`--draft` in release.yml); publishing a draft is a manual `gh release edit vX.Y.Z --draft=false`. Nothing in flight.

**Before taking real money**, the Lemon Squeezy store is still in test mode and `activation_status: "in_review"`. `LEMON_PRODUCT_ID` (1258447), `LEMON_VARIANT_ID` (1966803), and `LEMON_WEBHOOK_SECRET` in `license-worker/wrangler.toml` are all **test-mode** objects and will need re-pointing at the live equivalents once approval lands, or `validateProduct` rejects every real purchase. Tester keys minted in test mode will not survive the switch.

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
  `--preview-bench [long edge]` (tweak-editor rebuild timing per working-bitmap
  size: source build, cold recompose, cached annotation restamp — the three
  costs that decide how sharp the preview can afford to be),
  `--scroll-test <title>` (+ env `MATTESHOT_SCROLL_DEBUG=1`), `--record-test`,
  `--trim-test`, `--ocr`, `--assets <title> <dir>` (marketing exports),
  `--icon [dir]`, `--settings`, `--tweak`, `--once [--window <t>] [--pick N]`.
- **Run `.\scripts\run-probes.ps1` before calling anything done.** It runs every
  headless probe (capture, OCR, playback, all three export paths, the
  auto-update trust gates) and fails loudly. It is safe mid-work: no clipboard,
  no injected input, no windows. Two silent breakages got shipped for want of
  this — `--video-edit-test` was broken for weeks, and clippy was red through a
  release. `release-candidate.yml` now runs it too, with `-Strict` (a skipped
  probe fails the run) and `-SignedFile` pointed at the installer it just
  signed, so a candidate proves it passes its own auto-update gate before
  anyone can download it. `reliability-soak.ps1` covers the static side (test, clippy, audit);
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
- `MATTESHOT_LICENSE_OVERRIDE=not-started|trial|trial:<days>|expired|licensed[:<email>]`
  forces the licensing state, so the trial and purchase flow can be walked
  without waiting 14 days or hand-editing `license.json` and the registry. It
  only changes what is reported, never what is stored, so nothing needs
  undoing. **Requires `--features debug-license`**: no shipped binary honours
  it, and the variable name is not even present in a default build.
  `--license-status` prints a second line when a state is forced.
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
   launch. **The trial endpoints `POST /v1/trial/start` and
   `POST /v1/trial/status` are already implemented in the Worker (see "Trial
   licensing" above); Lemon Squeezy checkout integration and the buy button on
   the site are what remains.**
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
