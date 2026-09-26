# Matteshot

**Every screenshot, a matte shot.**

Press PrtScn → the screen freezes → click a window or drag a region → six finished mattes appear → click one → premium PNG on your clipboard. No editor in the flow; choosing between finished results replaces tweaking one.

The name is literal: a [matte shot](https://en.wikipedia.org/wiki/Matte_(filmmaking)) composites a subject over a painted background — exactly what this app does to your windows. The backgrounds are mattes; you pick one.

A Windows tray app in pure Rust. No UI framework — Win32 + GDI + `Windows.Graphics.Capture` + Media Foundation. Single small release binary.

Matteshot is free and open source under [MIT OR Apache-2.0](#license). There is no account, no trial, no license key, and no usage tracking. Every feature works offline.

## Install

- **Download:** [MatteshotSetup.exe](https://download.matteshot.app/MatteshotSetup.exe) (signed, per-user, no admin prompt), or pick a version from [GitHub Releases](https://github.com/btsouth/matteshot/releases). Each release lists the installer's SHA-256.
- **winget:** `winget install SouthboundSoftware.Matteshot`
- **From source:** see [Building from source](#building-from-source).

Supported: Windows 10 version 2004 (build 19041) or later and Windows 11, x64. Capture uses `Windows.Graphics.Capture` and OCR uses `Windows.Media.Ocr`, both part of Windows. There is no macOS or Linux build; the code is Win32 throughout.

## Privacy

Matteshot sends nothing about you or your captures anywhere. It has no telemetry, no crash reporting, and no account. The only network traffic in a default build is the update check: at startup and then once a day it fetches `https://matteshot.app/version.json`, and when a newer version is listed it fetches that version's signed release record, and with automatic updates on the installer itself, from `https://download.matteshot.app`. Those requests carry your IP address and a `Matteshot/<version>` user agent, nothing else. Turning off **Install updates automatically** in Settings stops Matteshot from downloading and installing new versions on its own; it still checks and offers the update in the tray menu. Captures, History, and settings stay on your PC.

Share is not in the default build. If you build with `--features share` and point it at your own server, uploads go to that server only. See [docs/self-hosting-share.md](docs/self-hosting-share.md).

## The flow

1. **PrtScn** (or left-click the tray icon): freeze-frame overlay across **all monitors** — dimmed frozen screens, hover highlights whole windows (taskbar included), drag selects a region (cross-monitor works), clicking bare desktop grabs that monitor. Toolbar: **Window / Region / Screen / ● Record / ↓ Scroll / ✕** (keys W/R/F/V/S). Esc cancels. **Shell flyouts are capturable**: the notification center, Quick Settings, Task View, Start and Search all land in the freeze and are offered as one-click targets, which nothing else on Windows manages — see the landmine note on z-bands for why.
2. **Pick a matte**: contact strip of auto-styled variants. Click / 1–7 / arrows+Enter chooses, **T** opens the tweak editor, **S** shares a link when Share is built in and a server is configured (hidden otherwise), **P** pins the raw capture, **C** copies its text via OCR, **PrtScn re-snips** (the strip itself is snippable), Esc cancels. Last-used matte preselected.
3. **Done**: PNG on the clipboard (as bitmap + PNG + file, so paste works everywhere) and in your captures folder. Output-size presets can preserve the original pixels or cap the finished matte to Email (1600 px), Compact (1200 px), or a custom final width or height without ever upscaling. The screenshot editor updates the preset name and exact final dimensions live while keeping the working preview large and readable; custom sizing uses a focused inline number field with Done/Cancel instead of blocking capture.

**Ctrl+Alt+S** skips the overlay: instant capture of the active window.

## Tweak editor (T in the picker)

**Tabbed**: every capture you open joins the same window instead of stacking up new ones. Tabs are named after the window you captured, and PrtScn while the editor is open adds a tab rather than replacing what you were working on. Click to switch, **Ctrl+Tab** / **Ctrl+Shift+Tab** to cycle, **Ctrl+1**–**8** to jump (**Ctrl+9** is the last tab), middle-click or the **×** to close. Copy saves the result and puts it on the clipboard while keeping the tab open for more refinement (toggleable in Settings); Save finishes that capture and closes its tab; the window closes with the last one. The capture leaving the screen drops its render caches and rebuilds them on the way back, so open tabs cost their pixels and little else.

Opens at 85% of the monitor, resizable — nearly all of a small one, and the control column compresses its spacing, then the matte grid's width, only as far as a short window forces it to, so 1366x768 fits and anything roomier is untouched. Live preview with matte swap (7 chips incl. None), padding slider, aspect presets (Auto / 1:1 / 4:3 / 16:9 / Social 1.91:1), a per-capture output-size override, and **annotations**: arrow, line, box, ellipse, highlighter, text (blinking caret, double-click to re-edit), pixelate-redact, and auto-numbered step badges — four colors, S/M/L sizes. A picked tool stays armed for repeated use until you click it again, pick another, or press Escape. Everything is selectable and draggable afterward: solid outline + handles on selection, dotted on hover, truthful cursors on endpoints/corners, Delete removes, Ctrl+Z undoes. Annotations live in content coordinates, render at export scale, and sit under the matte. Copy and Save compose at the output size so a forced aspect on a tall scroll cannot allocate a multi-gigabyte canvas; Original plus 1:1 / 16:9 / Social keeps the framed result under the same 9.4 MP budget video export already uses. The live preview stays pane-sized. **Select text** turns the preview into selectable text. Windows' offline OCR returns word boxes, every recognized word is faintly marked, and you drag across them like real text: double-click a word, Ctrl+A for all, Ctrl+C to copy, Esc to leave. Selections join with spaces inside a line and newlines between them. Words behind a pixelate-redact box are neither highlighted nor copyable, so redaction holds even though OCR reads the raw capture. Recognition runs off the message loop, so the editor stays live while it works, and word boxes are in capture coordinates, so matte, padding, and aspect changes keep the overlay aligned. **Crop** sits with padding and aspect rather than in the annotation grid, because it reshapes the capture instead of drawing on it, and it is **non-destructive**: the whole capture is kept, so the frame can be reopened and nudged, cleared back to full, or undone, and annotations outside it are hidden rather than discarded. Arming it brings the whole picture back with the current frame drawn over it — drag to sweep a new one, pull a corner, drag inside to move, Del uncrops, Enter applies, Esc cancels. Annotations stay in capture coordinates and the crop's origin shifts them, so cropping moves the picture under your marks instead of invalidating them, and Ctrl+Z steps back through framing and drawing in the one order they were done. PrtScn re-snips from here too.

## Recording

**● Record** (V) in the overlay, then the same gesture — click a window or drag a region. A floating pill shows elapsed time with Stop (`Ctrl+Shift+R` also stops); it excludes itself from the video via `WDA_EXCLUDEFROMCAPTURE`.

- H.264 MP4 via Media Foundation, ~30fps, bitrate scaled to pixel count, saved to the videos folder, file on clipboard. A keyframe every second: seeking decodes forward from the preceding keyframe, so the encoder default is what makes scrubbing and filmstrip loading slow, and the tighter spacing costs about 2% in file size.
- Window recording keeps a stable canvas if the target is resized, ignores duplicate high-refresh frames, and fails clearly instead of saving an all-black capture when a hardware surface never produces an initial frame.
- **DRM video cannot be recorded, by anyone.** Windows composites protected playback (Netflix, Prime Video and the rest) outside the surface `Windows.Graphics.Capture` can read, so the player chrome and subtitles arrive and the film is a black rectangle. Recording a region instead of the window reads the same surface, so it does not help. Matteshot cannot lift that and does not try; what it does is notice when a large area stayed black while everything around it moved, and say so on the finished recording rather than leaving you with an unexplained black file.
- **Audio**: Off / System (WASAPI loopback) / Mic in settings. Float PCM → resampled to an AAC-legal rate (192 kHz interfaces are common; AAC takes only 44.1/48 kHz) → stereo downmix → AAC muxed into the same MP4.
- Optional share-sized **GIF** alongside (settings toggle).
- **Scrubbing decodes live.** A decoder thread holds one reader open for the life of the editor and chases the playhead, so dragging shows the real frame rather than the nearest of a couple of dozen cached ones. Requests coalesce: whatever you scrub past is dropped and only the newest position is served, so it never falls behind the cursor. The cached frame still paints immediately, so the picture always tracks the drag. Keyboard seeking goes the same way instead of blocking the message loop per keypress.
- **On stop**, the editor opens on one decoded frame and fills its filmstrip and scrub cache on a worker thread. Building them up front meant dozens of seeks before the window existed, at roughly 100ms each, so opening got slower the longer the recording was. It is now a fixed cost regardless of length.
- A focused video editor opens at 85% of the active monitor with a large frame preview, native Play/Pause, Spacebar control, synchronized playhead, the same seven matte choices as screenshots, adjustable padding, Auto / 1:1 / 4:3 / 16:9 / Social aspect presets, keyboard seeking, and two trim handles. Playback keeps running through visual changes and resumes after timeline or trim seeks; its bounded preview decoder, cached matte, and coalesced frame delivery keep background switches responsive. A compact **+ Add** drawer has the same nine annotation tools as the photo editor: arrow, line, box, oval, mark, text, blur, auto-numbered steps, and freehand Pen. Every tool stays armed until you put it away, so four boxes take one trip to the drawer; consecutive Step clicks drop 1, 2, 3, 4, and clicking the tool again, picking another, or pressing Escape disarms. Every annotation can cover the whole video or three seconds from the playhead, stays attached to the recorded content across matte and aspect changes, and remains freely movable and editable. **Crop** sits beside padding and aspect and works exactly as it does for screenshots: arming it brings the whole recording back with the current frame over it, drag to sweep, pull a corner, drag inside to move, Del uncrops, Enter applies, Esc cancels. It is non-destructive — the recording is untouched, annotations stay normalized to it, so cropping moves the picture under them and the frame can be reopened, cleared or undone. The preview, the filmstrip and the export all follow it, and the export sizes the encoder from the kept region rather than the recording. **Export edit** renders the chosen layout and annotations at full resolution through a responsive background re-encode with progress and audio preserved. Export is cancelable, finalizes through a same-folder temporary file, never overwrites an earlier edit, and keeps the untouched original in place. If the export check cannot run or the final rename fails, the `.partial` stays for recovery on the next start; proven-undecodable export bytes are still deleted. Closing during export offers a safe cancel-and-cleanup path. A finished export lands on the clipboard by itself and Explorer opens on it, selected, once the editor closes. Show in folder / Copy / Share / Delete act on the recorded original and say so ("Show original" / "Copy original" / "Share original") as soon as an edit exists, so neither one silently replaces the export you just made. Share is hidden unless this build can actually upload. Resizable, double-buffered, no flicker.
- Recordings and edited exports remain private `.partial` files until they finalize and pass a real Media Foundation decode check. A crash cannot surface a truncated MP4 as finished work. A check that cannot run (Defender or OneDrive locking the file), or a rename that fails after a successful check, keeps the `.partial` for recovery on the next start instead of deleting it; proven-undecodable bytes are still deleted. Stale proven-undecodable partials are removed on the next clean start.

## Scrolling capture

**↓ Scroll** (S) in the overlay, then click a window or drag a region. Matteshot scrolls the target with synthetic wheel input and stitches frames into one tall image. The algorithm measures instead of assumes, so it's app-agnostic:

- SAD band matching finds the true per-step shift (never trusts the scroll amount sent).
- A shift=0 baseline answers "did anything move at all?" — catches bottom-of-page, wheel-ignoring apps, and periodic content that would otherwise self-match forever.
- A match must beat that baseline 2×, **or** agree with the established per-notch scroll rate (the prior that makes animated GIFs/video on the page survivable).
- Settle-detection (grab until two consecutive frames agree, ≤700ms) handles smooth-scroll browsers and instant apps with no per-app tuning.
- Sticky chrome (toolbars, status bars) is detected as contiguous unchanged edge rows, deliberately over-biased (over-detect = smaller viewport, harmless; under-detect = repeated footers), captured once.

**Stopping early.** **Esc**, **Ctrl+Shift+S**, or the **Stop** button on the progress pill all do the same thing: end the capture where it is and hand back what has been stitched. That is how you take the top of a page without the rest of it. There is deliberately no mid-capture discard — getting here takes three steps (PrtScn, S, click), after which the user is a passenger watching the page scroll, so the common intent is "that's far enough", and Esc is the key they reach for to say it. Pointing the most reachable key at "throw away the last minute of scrolling" had it backwards. Abandoning a capture is the picker's job: Esc there closes it, so the way out is the same key twice.

A stop landing mid-step discards that step's frame rather than stitching a half-scrolled one, so the image always ends on a clean boundary. The hotkey is a registered chord rather than a polled key, so the keystroke never reaches the page — a bare Space or Enter would page-down the target out from under the step measuring it. Both are checked every settle tick, not once per step, since a step can run most of a second. PrtScn during a capture does nothing: starting a second capture inside a running one would open a nested overlay and fight for the cursor, so `pump` drops the resident's thread-posted hotkeys.

Verified on Notepad (instant scroll), Chrome (smooth scroll, 19k-px pages), VS Code (webview + animated content). Progress pill excluded from capture, non-activating so it never takes focus off the target.

## Mattes

All derived from the capture's dominant hue (indigo fallback for grayscale UIs), deterministic per capture:
**Adaptive** (light gradient) · **Deep** (dark gradient) · **Aurora** (mesh blobs) · **Slate** (neutral dark) · **Paper** (neutral light) · **Pop** (complementary hue) · **None** (raw capture — Matteshot as a plain screenshot tool)

## Theming

Dark + light themes follow the system setting (`AppsUseLightTheme`), live-switch on `WM_SETTINGCHANGE`, and cover every surface: windows, popup menus (uxtheme ordinal 135 `SetPreferredAppMode`), titlebars (`DWMWA_USE_IMMERSIVE_DARK_MODE` + caption color). Text-bearing roles (`text`, `muted`, `faint`, `accent_text`) are at least 4.5:1 on `bg` / `panel` / `chip` in both modes. When High Contrast is on, the palette uses `GetSysColor` (`COLOR_WINDOW` / `COLOR_WINDOWTEXT` / `COLOR_BTNFACE` / `COLOR_HIGHLIGHT`); `COLOR_GRAYTEXT` is used for `muted`/`faint` only if it already meets 4.5:1, otherwise those roles use window text. A failed High Contrast query is not treated as High Contrast on. `MATTESHOT_THEME=dark|light` overrides for testing and still wins over High Contrast.

## Updating

Matteshot updates itself, and you never see it happen. A daily silent check against `matteshot.app/version.json` finds the new version, the signed installer downloads in the background, and the update applies the moment no Matteshot window is open, then the tray app comes back on its own. A balloon says what is happening; nothing else interrupts.

Nothing downloaded is trusted on the strength of where it came from:

- HTTPS only, to a fixed host. Plaintext, embedded credentials, and alternate ports are rejected before a connection is opened.
- An Ed25519-signed release record, verified against an embedded release public key, binds version, immutable URL, length, and SHA-256. A same-origin `.sha256` sidecar is not authorization: compromising the CDN and the checksum cannot approve an installer without the release key.
- It must carry a valid Authenticode signature **and** the certificate subject must be ours. Authenticode is Windows reputation, not the authorization that binds the release.
- Immediately before that installer is executed — after it has sat in `%TEMP%` through an idle wait, or until you click **Install update now** — the signed record and Authenticode checks run again. A file that is merely still present, or that still matches a hash sidecar we wrote, is not enough.

Any failure leaves the running app untouched and the tray menu still offers the manual download. A failure that proves the bytes are wrong — a hash mismatch, or a missing hash to check against — also deletes the staged file. A signature check that fails on its own does not: revocation and timestamp checks need the network, so the verified file is kept and the download page is offered instead, and a later attempt can still install it. The installer is per-user, so nothing prompts for elevation, and it runs `/VERYSILENT` under `SW_HIDE` with no shell in the chain, so no console window ever appears. Work in progress is never interrupted: a recording, an export, or an open editor defers the restart until it is finished. **Install updates automatically** in Settings turns the whole thing off and goes back to notify-only.

## Settings (tray menu)

Save folder + video folder (`IFileDialog` pickers, open buttons), render quality 1x/2x/3x, screenshot size Original/Email/Compact/Custom, start with Windows (a denied or missing Startup folder on enable, or a locked or read-only shortcut on disable, shows an error and leaves the checkbox matching the shortcut on disk), PrtScn capture toggle, GIF toggle, automatic updates toggle, keep the editor open after Copy, recording audio Off/System/Mic, plus **Copy diagnostics** (a bounded privacy-safe support report with no account name, machine name, window title, or filesystem path) and **Clear History titles** (strips stored window titles from the History index and leaves capture files alone).

The Settings window clamps to the active monitor work area and scrolls, so the update, diagnostics, and History controls stay reachable at 100-200% even on 1366x768. Tab/arrows move focus and scroll the focused control into view; mouse wheel and Page Up/Down scroll the rest of the way.

Only one resident can run at a time. Launching Matteshot again opens Settings on the existing resident instead of competing for hotkeys. The tray menu is capture (and the active-window and delayed-capture variants), open captures/videos folders, History, and Settings — Copy diagnostics and Clear History titles live in the Settings window.

On first run, a compact native welcome surface explains the PrtScn-to-paste loop and opens the real capture flow in one click. It follows Windows light and dark app mode, stays non-modal so capture hotkeys remain responsive, and never appears again after it has been shown. If that window fails to open, a tray balloon explains PrtScn and the tray menu, and first-run is marked done so it does not retry.

## History privacy

History is a local index at `%APPDATA%\matteshot\history.json`. Each entry stores the save path, time, size, matte name, and — by default — the captured window's title (or a region-size label), sanitized and capped at 200 characters. Titles stay on this PC; nothing sends them anywhere, and the diagnostics report leaves them out.

Opening History drops entries whose files are confirmed gone — a deleted file whose folder is still reachable — and writes that pruned list back to disk, so a capture deleted in Explorer does not leave its title behind. An ejected USB or offline share stays in the index until the volume is back and the file is actually missing. Per-item Delete of that same offline path fails and leaves the row; it does not persist-prune the index. **Clear History titles…** in Settings removes stored titles without deleting screenshots or videos. Per-item Delete is what removes a file. Uninstall asks before deleting `history.json` (plus leftover `history.json.tmp` and quarantined copies); it does not delete captures. The default is still the full window title, not the app name only.

## CLI / test rig

```
matteshot                    # tray app (normal mode)
matteshot --take-printscreen # unbind PrtScn from Snipping Tool
matteshot --restore-printscreen # explicit force-on; quit/toggle do not do this
matteshot --bench <substr>   # timed capture of a window, raw PNG to %TEMP%
matteshot --overlay-bench [batched|sequential] # headless multi-monitor freeze/layer timing
matteshot --scroll-test <t>  # scroll-capture a window headlessly (MATTESHOT_SCROLL_DEBUG=1 for per-step diagnostics)
matteshot --record-test [s]  # short recording smoke test, optionally auto-stop after s seconds
matteshot --record-window-test <title> [s] # real named-window recording with timed stop
matteshot --trim-test <mp4> <a> <b> [1-7] # trim/export probe; optional matte
matteshot --video-edit-test <mp4> # matte + text/arrow/box/blur export probe
matteshot --playback-test <mp4> # paced 3s editor preview decode, no UI/clipboard
matteshot --review-test <mp4> # open the video editor around an existing file
matteshot --welcome          # preview first-run onboarding without changing config
matteshot --ocr <substr>     # capture a window and print its OCR text
matteshot --ocr-words <substr|png> # print every OCR word box in capture coordinates
matteshot --update-test      # probe version.json; never downloads
matteshot --update-stage-test [url] # download + verify signed release and Authenticode; never installs
matteshot --verify-signature-test <exe> # Authenticode gate: accept ours, reject everything else
matteshot --update-install-now [url] # the real thing: download, verify, install silently
matteshot --settings         # open the settings window directly
matteshot --once --tweak     # capture the foreground window and open the tweak editor
matteshot --tweak-tabs-test <title>... # open several captures as tabs in one editor
```

None of these need a network connection except the `--update-*` probes.

Run every headless probe at once and fail loudly if one breaks:

```powershell
.\scripts\run-probes.ps1
```

Covers capture, OCR, playback, all three export paths, and the auto-update trust gates (ours accepted, a foreign signature and a tampered copy both refused). Safe to run while working: it writes no clipboard, injects no input (`--scroll-test` is opt-in behind `-IncludeScroll`), and opens no window unless it has to record a fixture. `-Offline` skips the network checks. The interactive surfaces — picker, editors, overlay — still need a human.

`release-candidate.yml` runs the video and signature probes against every signed candidate, so the artifact proves it would pass its own auto-update gate before it can be downloaded. Three flags exist for that: `-SignedFile` aims the Authenticode check at a chosen file (the freshly signed installer in CI, the installed build by default, since a local `cargo build` is unsigned); `-Strict` fails the run on a skipped probe, because a SKIP that quietly passes is the same hole as never running the probe; and `-NoCapture` drops `--bench` and `--ocr`, which need a visible window that a GitHub-hosted runner does not have — it records its own fixture instead, and capture and OCR stay in the local run.

Use [`INTERACTIVE-REGRESSION.md`](INTERACTIVE-REGRESSION.md) for the release-blocking human pass over those surfaces.

Run the same static code gate used by CI, signed candidates, and releases:

```powershell
.\scripts\verify-code.ps1
```

That gate now includes a locked RustSec `cargo audit` (pinned `cargo-audit` 0.22.2 from crates.io). A missing or wrong-version tool, an expired or undocumented ignore, or an actionable advisory fails the run. Exceptions live in `.cargo/rustsec-exceptions.json` and must match `.cargo/audit.toml`.

Release acceptance runs in Windows Sandbox with clipboard redirection disabled.
The guest seeds the state a paid-era install left behind (an ended trial in
`license.json` and the registry, a config with the retired telemetry keys),
installs the signed build, blocks `matteshot.exe` from the network with a
firewall rule, captures a window, runs the resident, uninstalls, and checks
that the old files and settings are still there. The host waits for the guest
`result.json` and exits 0 only when `passed` is true. It exits non-zero on
guest FAIL, timeout, or an unreadable/missing result. `-TimeoutSeconds`
(default 1200) bounds the wait.

```powershell
.\scripts\run-sandbox-smoke.ps1 `
  -InstallerPath <signed-installer> `
  -WorkDirectory <temporary-directory>
```

Config: `%APPDATA%\matteshot\config.json`. Default dirs: `Pictures\Matteshot`, `Videos\Matteshot` (note: often OneDrive-redirected).

## Building from source

You need Windows 10 or 11 (x64) with:

- Rust stable (`rustup default stable`), with the `x86_64-pc-windows-msvc` toolchain
- Visual Studio 2022 Build Tools with the "Desktop development with C++" workload (MSVC linker, `rc.exe`, Windows SDK)
- PowerShell 7 (`pwsh`) for the scripts
- [Inno Setup 6](https://jrsoftware.org/isinfo.php) only if you want to build the installer

```powershell
cargo build --release                 # target\release\matteshot.exe
cargo test --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
.\scripts\verify-code.ps1            # the full gate CI runs
& "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe" installer\matteshot.iss   # unsigned local installer
```

`cargo build --release --features share` adds Share, which then needs a server of your own (see [docs/self-hosting-share.md](docs/self-hosting-share.md)).

The resident app locks its own exe, so quit it from the tray (or `matteshot --quit`) before rebuilding over an installed or running copy. It is a GUI-subsystem program and prints nothing to a console; run the diagnostic flags with `Start-Process -RedirectStandardError <log> -Wait` and read the log.

From Linux or macOS you can type-check and lint the Windows target with [cargo-xwin](https://github.com/rust-cross/cargo-xwin) and `llvm-rc` on the `PATH`, which is how much of this code is reviewed:

```bash
rustup target add x86_64-pc-windows-msvc
cargo xwin clippy --target x86_64-pc-windows-msvc --all-targets --all-features -- -D warnings
```

Running it still needs Windows.

## Architecture

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the module map and the Windows behaviour that shaped it.

## Contributing

Bug reports and pull requests are welcome. Read [CONTRIBUTING.md](CONTRIBUTING.md) first; it covers the checks a change needs and what can only be verified by hand on Windows. Security problems go through [SECURITY.md](SECURITY.md), not public issues.

## License

Matteshot is licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in Matteshot by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.

The Matteshot name and icon identify the official builds signed by Southbound Software. Forks are welcome under the license; please give them a different name and icon so people can tell them apart.
