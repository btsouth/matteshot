# Matteshot

**Every screenshot, a matte shot.**

Press PrtScn → the screen freezes → click a window or drag a region → six finished mattes appear → click one → premium PNG on your clipboard. No editor in the flow; choosing between finished results replaces tweaking one.

The name is literal: a [matte shot](https://en.wikipedia.org/wiki/Matte_(filmmaking)) composites a subject over a painted background — exactly what this app does to your windows. The backgrounds are mattes; you pick one.

A Windows tray app in pure Rust. No UI framework — Win32 + GDI + `Windows.Graphics.Capture` + Media Foundation. Single small release binary.

## The flow

1. **PrtScn** (or left-click the tray icon): freeze-frame overlay across **all monitors** — dimmed frozen screens, hover highlights whole windows (taskbar included), drag selects a region (cross-monitor works), clicking bare desktop grabs that monitor. Toolbar: **Window / Region / Screen / ● Record / ↓ Scroll / ✕** (keys W/R/F/V/S). Esc cancels.
2. **Pick a matte**: contact strip of auto-styled variants. Click / 1–7 / arrows+Enter chooses, **T** opens the tweak editor, **E** opens in your default editor, **P** pins the raw capture, **C** copies its text via OCR, **PrtScn re-snips** (the strip itself is snippable), Esc cancels. Last-used matte preselected.
3. **Done**: full-res PNG on the clipboard (as bitmap + PNG + file, so paste works everywhere) and in your captures folder.

**Ctrl+Alt+S** skips the overlay: instant capture of the active window.

## Tweak editor (T in the picker)

Opens at 85% of the monitor, resizable. Live preview with matte swap (7 chips incl. None), padding slider, aspect presets (Auto / 1:1 / 4:3 / 16:9 / Social 1.91:1), and **annotations**: arrow, line, box, ellipse, highlighter, text (blinking caret, double-click to re-edit), pixelate-redact, and auto-numbered step badges — four colors, S/M/L sizes. Everything is selectable and draggable afterward: solid outline + handles on selection, dotted on hover, truthful cursors on endpoints/corners, Delete removes, Ctrl+Z undoes. Annotations live in content coordinates, render at export scale, and sit under the matte. **Copy text (OCR)** reads the capture via Windows' offline OCR. PrtScn re-snips from here too.

## Recording

**● Record** (V) in the overlay, then the same gesture — click a window or drag a region. A floating pill shows elapsed time with Stop (Esc also stops); it excludes itself from the video via `WDA_EXCLUDEFROMCAPTURE`.

- H.264 MP4 via Media Foundation, ~30fps, bitrate scaled to pixel count, saved to the videos folder, file on clipboard.
- **Audio**: Off / System (WASAPI loopback) / Mic in settings. Float PCM → resampled to an AAC-legal rate (192 kHz interfaces are common; AAC takes only 44.1/48 kHz) → stereo downmix → AAC muxed into the same MP4.
- Optional share-sized **GIF** alongside (settings toggle).
- **On stop**, a review window: filmstrip of natural-aspect thumbnails, drag two handles to set in/out, **Save trim** does a frame-accurate re-encode of video+audio to `-trim.mp4`. Play / Show in folder / Copy / Delete included. Resizable, double-buffered, no flicker.

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

## Settings (tray menu)

Save folder + video folder (`IFileDialog` pickers, open buttons), export quality 1x/2x/3x, start with Windows, PrtScn capture toggle, GIF toggle, recording audio Off/System/Mic. First run shows a tray balloon.

## CLI / test rig

```
matteshot                    # tray app (normal mode)
matteshot --take-printscreen # unbind PrtScn from Snipping Tool
matteshot --restore-printscreen
matteshot --bench <substr>   # timed capture of a window, raw PNG to %TEMP%
matteshot --scroll-test <t>  # scroll-capture a window headlessly (MATTESHOT_SCROLL_DEBUG=1 for per-step diagnostics)
matteshot --record-test [s]  # short recording smoke test, optionally auto-stop after s seconds
matteshot --trim-test <mp4>  # probe + cut smoke test
matteshot --ocr <substr>     # capture a window and print its OCR text
matteshot --update-test      # probe version.json; never downloads
matteshot --settings         # open the settings window directly
matteshot --once --tweak     # capture the foreground window and open the tweak editor
```

Capture-producing diagnostic commands honor the same trial and license gate as the resident app.

Config: `%APPDATA%\matteshot\config.json`. Default dirs: `Pictures\Matteshot`, `Videos\Matteshot` (note: often OneDrive-redirected).

## Architecture

~20 modules, all Win32-direct:

- `capture.rs` — WGC single-frame grabs (window/monitor). Thread-local cached D3D device → ~20ms warm captures; prefers the second frame within 80ms (first can be stale). `device_pair()` for worker threads.
- `overlay.rs` — multi-monitor freeze-frame selector: combined virtual-screen image, dim/bright DIB layers, z-order hit-testing on `DWMWA_EXTENDED_FRAME_BOUNDS`, toolbar with record/scroll arming.
- `style.rs` / `compose.rs` — hue-histogram matte families; pure-Rust compositing (gradient, blurred shadow, SDF corner mask, proportional padding, aspect extension). Export short-circuits the None matte and skips upscales ≥1600px.
- `picker.rs` — contact strip; `tweak.rs` — the editor; `annotate.rs` — shape model + capsule-band AA rasterizer + GDI text with halo.
- `record.rs` — MF sink writer (H.264+AAC), WGC frame loop, audio-cursor muxing with silence fill; `audio.rs` — WASAPI loopback/mic + stateful linear resampler; `recui.rs` — stop pill; `recdone.rs` — review/trim window; `trim.rs` — source reader (`ENABLE_ADVANCED_VIDEO_PROCESSING`, streams resolved via `GetNativeMediaType`, never assume stream 0) → frame-accurate cut.
- `scroll.rs` — scrolling capture (see above).
- `theme.rs` — theme plumbing; `settings.rs` — settings window; `pin.rs` — floating pinned captures; `ocr.rs` — Windows.Media.Ocr; `prtscn.rs` — PrtScn acquisition; `tray.rs` — tray icon/menu; `output.rs` — clipboard (manual CF_DIB + PNG + CF_HDROP), save, reveal.
- `update.rs` — silent WinHTTP version check on startup and daily; newer versions surface through a tray balloon and download menu item.

## Windows landmines (hard-won)

- **DPI**: `PerMonitorV2` at startup or captures come out soft on mixed-DPI setups.
- **PrtScn**: Win11 routes it to Snipping Tool. On 23H2/24H2 that's `PrintScreenKeyForSnippingEnabled` (HKCU\Control Panel\Keyboard, missing = enabled) — Matteshot flips it with consent, live. **On Insider 26220+ the value is ignored**: routing consumes the key ahead of hotkey dispatch even when `RegisterHotKey(VK_SNAPSHOT)` succeeds; the real toggle is Settings > Bluetooth & devices > Keyboard, with an untraceable backing store. Matteshot detects and guides. Fallback if it regresses: WH_KEYBOARD_LL hook.
- **Synthetic PrtScn is untestable** while Snipping routing is on — injected VK_SNAPSHOT never reaches hotkey dispatch.
- **WGC corner alpha varies by build** — Matteshot applies its own SDF corner mask unconditionally.
- **`FindWindowW` doesn't match** Matteshot's toolwindow popups even though `EnumWindows` sees them — don't use it in tests.
- **MF AAC** rejects float PCM (`0xC00D36B4`) and non-44.1/48k rates — convert to 16-bit PCM and resample first.
- **MF source reader**: stream 0 is not necessarily video — resolve via `GetNativeMediaType` or you'll mux garbage.
- **AdjustWindowRectEx everywhere** — never guess non-client frame sizes.

## Ship status

- [x] Feature-complete core: capture, mattes, picker, tweak editor, annotations, OCR, pin, recording + audio + trim, scrolling capture, themes, settings, multi-monitor
- [x] Inno Setup installer + Azure Trusted Signing release pipeline
- [ ] winget manifest
- [x] Update check
- [x] 14-day trial + Lemon Squeezy license activation
- [ ] Lemon Squeezy merchant approval + live checkout
- [x] matteshot.app site + assets
