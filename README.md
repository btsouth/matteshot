# Matteshot

**Every screenshot, a matte shot.**

Press PrtScn → the screen freezes → click a window or drag a region → six finished mattes appear → click one → premium PNG on your clipboard. No editor in the flow; choosing between finished results replaces tweaking one.

The name is literal: a [matte shot](https://en.wikipedia.org/wiki/Matte_(filmmaking)) composites a subject over a painted background — exactly what this app does to your windows. The backgrounds are mattes; you pick one.

A Windows tray app in pure Rust. No UI framework — Win32 + GDI + `Windows.Graphics.Capture` + Media Foundation. Single small release binary.

## The flow

1. **PrtScn** (or left-click the tray icon): freeze-frame overlay across **all monitors** — dimmed frozen screens, hover highlights whole windows (taskbar included), drag selects a region (cross-monitor works), clicking bare desktop grabs that monitor. Toolbar: **Window / Region / Screen / ● Record / ↓ Scroll / ✕** (keys W/R/F/V/S). Esc cancels.
2. **Pick a matte**: contact strip of auto-styled variants. Click / 1–7 / arrows+Enter chooses, **T** opens the tweak editor, **E** opens in your default editor, **P** pins the raw capture, **C** copies its text via OCR, **PrtScn re-snips** (the strip itself is snippable), Esc cancels. Last-used matte preselected.
3. **Done**: PNG on the clipboard (as bitmap + PNG + file, so paste works everywhere) and in your captures folder. Output-size presets can preserve the original pixels or cap the finished matte to Email (1600 px), Compact (1200 px), or a custom longest edge without ever upscaling.

**Ctrl+Alt+S** skips the overlay: instant capture of the active window.

## Tweak editor (T in the picker)

**Tabbed**: every capture you open joins the same window instead of stacking up new ones. Tabs are named after the window you captured, and PrtScn while the editor is open adds a tab rather than replacing what you were working on. Click to switch, **Ctrl+Tab** / **Ctrl+Shift+Tab** to cycle, **Ctrl+1**–**8** to jump (**Ctrl+9** is the last tab), middle-click or the **×** to close. Copy, Save and Editor finish that capture and close its tab; the window closes with the last one. The capture leaving the screen drops its render caches and rebuilds them on the way back, so open tabs cost their pixels and little else.

Opens at 85% of the monitor, resizable. Live preview with matte swap (7 chips incl. None), padding slider, aspect presets (Auto / 1:1 / 4:3 / 16:9 / Social 1.91:1), a per-capture output-size override, and **annotations**: arrow, line, box, ellipse, highlighter, text (blinking caret, double-click to re-edit), pixelate-redact, and auto-numbered step badges — four colors, S/M/L sizes. Everything is selectable and draggable afterward: solid outline + handles on selection, dotted on hover, truthful cursors on endpoints/corners, Delete removes, Ctrl+Z undoes. Annotations live in content coordinates, render at export scale, and sit under the matte. The completed result is resized as one image so the matte and annotations stay sharp and aligned. **Select text** turns the preview into selectable text. Windows' offline OCR returns word boxes, every recognized word is faintly marked, and you drag across them like real text: double-click a word, Ctrl+A for all, Ctrl+C to copy, Esc to leave. Selections join with spaces inside a line and newlines between them. Words behind a pixelate-redact box are neither highlighted nor copyable, so redaction holds even though OCR reads the raw capture. Recognition runs off the message loop, so the editor stays live while it works, and word boxes are in capture coordinates, so matte, padding, and aspect changes keep the overlay aligned. PrtScn re-snips from here too.

## Recording

**● Record** (V) in the overlay, then the same gesture — click a window or drag a region. A floating pill shows elapsed time with Stop (`Ctrl+Shift+R` also stops); it excludes itself from the video via `WDA_EXCLUDEFROMCAPTURE`.

- H.264 MP4 via Media Foundation, ~30fps, bitrate scaled to pixel count, saved to the videos folder, file on clipboard. A keyframe every second: seeking decodes forward from the preceding keyframe, so the encoder default is what makes scrubbing and filmstrip loading slow, and the tighter spacing costs about 2% in file size.
- Window recording keeps a stable canvas if the target is resized, ignores duplicate high-refresh frames, and fails clearly instead of saving an all-black capture when a hardware surface never produces an initial frame.
- **Audio**: Off / System (WASAPI loopback) / Mic in settings. Float PCM → resampled to an AAC-legal rate (192 kHz interfaces are common; AAC takes only 44.1/48 kHz) → stereo downmix → AAC muxed into the same MP4.
- Optional share-sized **GIF** alongside (settings toggle).
- **Scrubbing decodes live.** A decoder thread holds one reader open for the life of the editor and chases the playhead, so dragging shows the real frame rather than the nearest of a couple of dozen cached ones. Requests coalesce: whatever you scrub past is dropped and only the newest position is served, so it never falls behind the cursor. The cached frame still paints immediately, so the picture always tracks the drag. Keyboard seeking goes the same way instead of blocking the message loop per keypress.
- **On stop**, the editor opens on one decoded frame and fills its filmstrip and scrub cache on a worker thread. Building them up front meant dozens of seeks before the window existed, at roughly 100ms each, so opening got slower the longer the recording was. It is now a fixed cost regardless of length.
- A focused video editor opens at 85% of the active monitor with a large frame preview, native Play/Pause, Spacebar control, synchronized playhead, the same seven matte choices as screenshots, adjustable padding, Auto / 1:1 / 4:3 / 16:9 / Social aspect presets, keyboard seeking, and two trim handles. Playback keeps running through visual changes and resumes after timeline or trim seeks; its bounded preview decoder, cached matte, and coalesced frame delivery keep background switches responsive. A compact **+ Add** drawer places time-ranged text, arrows, boxes, and pixelate-redaction; every tool returns to Select after placement, annotations stay attached to the recorded content across matte and aspect changes, remain freely movable, and their timeline clips can be moved or resized. **Export edit** renders the chosen layout and annotations at full resolution through a responsive background re-encode with progress and audio preserved. Export is cancelable, finalizes through a same-folder temporary file, never overwrites an earlier edit, and keeps the untouched original in place. Closing during export offers a safe cancel-and-cleanup path. A finished export lands on the clipboard by itself and Explorer opens on it, selected, once the editor closes. Show in folder / Copy / Delete act on the recorded original and say so ("Show original" / "Copy original") as soon as an edit exists, so neither one silently replaces the export you just made. Resizable, double-buffered, no flicker.
- Recordings and edited exports remain private `.partial` files until they finalize and pass a real Media Foundation decode check. A crash cannot surface a truncated MP4 as finished work; stale partials are removed on the next clean start.

## Scrolling capture

**↓ Scroll** (S) in the overlay, then click a window or drag a region. Matteshot scrolls the target with synthetic wheel input and stitches frames into one tall image. The algorithm measures instead of assumes, so it's app-agnostic:

- SAD band matching finds the true per-step shift (never trusts the scroll amount sent).
- A shift=0 baseline answers "did anything move at all?" — catches bottom-of-page, wheel-ignoring apps, and periodic content that would otherwise self-match forever.
- A match must beat that baseline 2×, **or** agree with the established per-notch scroll rate (the prior that makes animated GIFs/video on the page survivable).
- Settle-detection (grab until two consecutive frames agree, ≤700ms) handles smooth-scroll browsers and instant apps with no per-app tuning.
- Sticky chrome (toolbars, status bars) is detected as contiguous unchanged edge rows, deliberately over-biased (over-detect = smaller viewport, harmless; under-detect = repeated footers), captured once.

Verified on Notepad (instant scroll), Chrome (smooth scroll, 19k-px pages), VS Code (webview + animated content). Esc aborts; progress pill excluded from capture.

## Mattes

All derived from the capture's dominant hue (indigo fallback for grayscale UIs), deterministic per capture:
**Adaptive** (light gradient) · **Deep** (dark gradient) · **Aurora** (mesh blobs) · **Slate** (neutral dark) · **Paper** (neutral light) · **Pop** (complementary hue) · **None** (raw capture — Matteshot as a plain screenshot tool)

## Theming

Dark + light themes follow the system setting (`AppsUseLightTheme`), live-switch on `WM_SETTINGCHANGE`, and cover every surface: windows, popup menus (uxtheme ordinal 135 `SetPreferredAppMode`), titlebars (`DWMWA_USE_IMMERSIVE_DARK_MODE` + caption color). `MATTESHOT_THEME=dark|light` overrides for testing.

## Updating

Matteshot updates itself, and you never see it happen. A daily silent check against `matteshot.app/version.json` finds the new version, the signed installer downloads in the background, and the update applies the moment no Matteshot window is open, then the tray app comes back on its own. A balloon says what is happening; nothing else interrupts.

Nothing downloaded is trusted on the strength of where it came from:

- HTTPS only, to a fixed host. Plaintext, embedded credentials, and alternate ports are rejected before a connection is opened.
- The download must match the SHA-256 published beside it.
- It must carry a valid Authenticode signature **and** the certificate subject must be ours. A validly signed binary from anyone else is refused, which is what stops a compromised mirror from shipping somebody else's real installer.

Any failure removes the download and leaves the running app untouched; the tray menu still offers the manual download. The installer is per-user, so nothing prompts for elevation, and it runs `/VERYSILENT` under `SW_HIDE` with no shell in the chain, so no console window ever appears. Work in progress is never interrupted: a recording, an export, or an open editor defers the restart until it is finished. **Install updates automatically** in Settings turns the whole thing off and goes back to notify-only.

## Settings (tray menu)

Save folder + video folder (`IFileDialog` pickers, open buttons), render quality 1x/2x/3x, screenshot size Original/Email/Compact/Custom, start with Windows, PrtScn capture toggle, GIF toggle, automatic updates toggle, recording audio Off/System/Mic. First run shows a tray balloon.

Only one resident can run at a time. Launching Matteshot again opens Settings on the existing resident instead of competing for hotkeys. **Copy diagnostics** in the tray produces a bounded privacy-safe support report with no license key, account name, machine name, window title, or filesystem path.

On first run, a compact native welcome surface explains the PrtScn-to-paste loop, the no-card 14-day trial, and opens the real capture flow in one click. It follows Windows light and dark app mode, stays non-modal so capture hotkeys remain responsive, and never appears again after it has been shown.

## CLI / test rig

```
matteshot                    # tray app (normal mode)
matteshot --take-printscreen # unbind PrtScn from Snipping Tool
matteshot --restore-printscreen
matteshot --bench <substr>   # timed capture of a window, raw PNG to %TEMP%
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
matteshot --update-stage-test [url] # download + verify hash and signature; never installs
matteshot --verify-signature-test <exe> # Authenticode gate: accept ours, reject everything else
matteshot --update-install-now [url] # the real thing: download, verify, install silently
matteshot --settings         # open the settings window directly
matteshot --once --tweak     # capture the foreground window and open the tweak editor
matteshot --tweak-tabs-test <title>... # open several captures as tabs in one editor
```

Capture-producing diagnostic commands honor the same trial and license gate as the resident app.

Run every headless probe at once and fail loudly if one breaks:

```powershell
.\scripts\run-probes.ps1
```

Covers capture, OCR, playback, all three export paths, and the auto-update trust gates (ours accepted, a foreign signature and a tampered copy both refused). Safe to run while working: it writes no clipboard, injects no input (`--scroll-test` is opt-in behind `-IncludeScroll`), and opens no window unless it has to record a fixture. `-Offline` skips the network checks. The interactive surfaces — picker, editors, overlay — still need a human.

Release acceptance runs in Windows Sandbox with clipboard redirection disabled:

```powershell
.\scripts\run-sandbox-smoke.ps1 `
  -InstallerPath <signed-installer> `
  -LicenseKeyPath <test-key-file> `
  -WorkDirectory <temporary-directory>
```

Config: `%APPDATA%\matteshot\config.json`. Default dirs: `Pictures\Matteshot`, `Videos\Matteshot` (note: often OneDrive-redirected).

## Architecture

~20 modules, all Win32-direct:

- `capture.rs` — WGC single-frame grabs (window/monitor). Thread-local cached D3D device → ~20ms warm captures; prefers the second frame within 80ms (first can be stale). `device_pair()` for worker threads.
- `overlay.rs` — multi-monitor freeze-frame selector: combined virtual-screen image, dim/bright DIB layers, z-order hit-testing on `DWMWA_EXTENDED_FRAME_BOUNDS`, toolbar with record/scroll arming.
- `style.rs` / `compose.rs` — hue-histogram matte families; pure-Rust compositing (gradient, blurred shadow, SDF corner mask, proportional padding, aspect extension). Export short-circuits the None matte and skips upscales ≥1600px.
- `picker.rs` — contact strip; `tweak.rs` — the editor; `annotate.rs` — shape model + capsule-band AA rasterizer + GDI text with halo.
- `record.rs` — MF sink writer (H.264+AAC), WGC frame loop, audio-cursor muxing with silence fill; `audio.rs` — WASAPI loopback/mic + stateful linear resampler; `recui.rs` — stop pill; `recdone.rs` — matte-aware video editor; `video_edit.rs` — normalized time-ranged annotation model shared by preview and export; `trim.rs` — source reader (`ENABLE_ADVANCED_VIDEO_PROCESSING`, streams resolved via `GetNativeMediaType`, never assume stream 0) → frame-accurate edit export.
- `scroll.rs` — scrolling capture (see above).
- `theme.rs` — theme plumbing; `settings.rs` — settings window; `pin.rs` — floating pinned captures; `ocr.rs` — Windows.Media.Ocr, whole-capture text plus per-word boxes mapped back out of the engine's input downscale; `prtscn.rs` — PrtScn acquisition; `tray.rs` — tray icon/menu; `output.rs` — clipboard (manual CF_DIB + PNG + CF_HDROP), save, reveal.
- `update.rs` — silent WinHTTP version check on startup and daily, then the background install; `installer.rs` — download, verification, and silent execution.

## Windows landmines (hard-won)

- **DPI**: `PerMonitorV2` at startup or captures come out soft on mixed-DPI setups.
- **PrtScn**: Win11 routes it to Snipping Tool. On 23H2/24H2 that's `PrintScreenKeyForSnippingEnabled` (HKCU\Control Panel\Keyboard, missing = enabled), but Insider 26220+ can ignore that value and consume the key even after `RegisterHotKey(VK_SNAPSHOT)` reports success. The resident therefore owns PrtScn with a `WH_KEYBOARD_LL` hook and posts the same `WM_HOTKEY` used by every nested picker/editor loop. The hook is removed on toggle, license expiry, or process exit, so Snipping Tool immediately gets the key back. Registry routing remains only as a compatibility fallback.
- **Synthetic PrtScn is untestable** while Snipping routing is on — injected VK_SNAPSHOT never reaches hotkey dispatch.
- **WGC corner alpha varies by build** — Matteshot applies its own SDF corner mask unconditionally.
- **`FindWindowW` doesn't match** Matteshot's toolwindow popups even though `EnumWindows` sees them — don't use it in tests.
- **MF AAC** rejects float PCM (`0xC00D36B4`) and non-44.1/48k rates — convert to 16-bit PCM and resample first.
- **MF source reader**: stream 0 is not necessarily video — resolve via `GetNativeMediaType` or you'll mux garbage.
- **AdjustWindowRectEx everywhere** — never guess non-client frame sizes.
- **H.264 caps a frame** near 9.4M luma samples. A matte with a forced aspect can compose a large recording past that (a 2560x1392 source at 1:1 lands on 3088x3088), and Media Foundation reports only an invalid-media-type error. Exports shrink the content until the framed result fits rather than failing.
- **OCR input is capped** at `MaxImageDimension` (2600). Oversized captures are downscaled before recognition and word boxes are scaled back out, so select-text stays aligned — but a tall scrolling stitch loses enough detail that recognition itself suffers. Tiled recognition is the fix if that matters.
- **GDI has no alpha on `Rectangle`** — translucent overlays (select-text highlights) stretch a 1x1 solid through `AlphaBlend`.

## Ship status

- [x] Feature-complete core: capture, mattes, picker, tweak editor, annotations, OCR, pin, recording + audio + trim, scrolling capture, themes, settings, multi-monitor
- [x] Inno Setup installer + Azure Trusted Signing release pipeline
- [ ] winget manifest
- [x] Silent verified auto-update
- [x] 14-day trial + Lemon Squeezy license activation
- [ ] Lemon Squeezy merchant approval + live checkout
- [x] matteshot.app site + assets
