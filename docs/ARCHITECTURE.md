# Matteshot architecture

Matteshot is one Rust binary, `matteshot.exe`, built for `x86_64-pc-windows-msvc`. It has no UI framework: every window is a Win32 window painted with GDI, capture goes through `Windows.Graphics.Capture`, recording and video export go through Media Foundation, and OCR is `Windows.Media.Ocr`. The `windows` crate is the only binding layer.

The app normally runs as a single resident tray process. The same binary also answers a set of diagnostic flags (`--bench`, `--record-test`, `--update-stage-test`, and so on; see the README) that CI and `scripts/run-probes.ps1` use as headless probes.

## Process model

- `main.rs` claims a per-session mutex so only one resident runs. A second launch asks the resident to open Settings and exits.
- The resident owns the capture hotkeys (PrtScn through a low-level keyboard hook, plus a configurable `RegisterHotKey` shortcut) and a single message loop on the main thread. Every window is created on that thread and shares the loop.
- Long work (auto-copy export, OCR, uploads, update checks, decoding) runs on worker threads. Results come back to the owning window as a posted message carrying an opaque token from `completion.rs`, never a pointer, so a stale or forged message cannot touch freed state.
- Quitting and auto-update both wait until no Matteshot surface is open and no recording is still finalizing (`window::any_surface_open`).

## Modules

- `capture.rs`: WGC single-frame grabs (window/monitor). Thread-local cached D3D device → ~20ms warm captures; prefers the second frame within 80ms (first can be stale). `device_pair()` for worker threads.
- `overlay.rs`: multi-monitor freeze-frame selector: combined virtual-screen image, dim/bright DIB layers, z-order hit-testing on `DWMWA_EXTENDED_FRAME_BOUNDS`, toolbar with record/scroll/delay arming.
- `style.rs` / `compose.rs`: hue-histogram matte families; pure-Rust compositing (gradient, blurred shadow, SDF corner mask, proportional padding, aspect extension). Export short-circuits the None matte and skips upscales ≥1600px.
- `picker.rs`: contact strip; `tweak.rs`; the tabbed screenshot editor; `annotate.rs`; shape model + capsule-band AA rasterizer + GDI text with halo; `number_prompt.rs`; the custom output-size prompt.
- `record.rs`: MF sink writer (H.264+AAC), WGC frame loop, audio-cursor muxing with silence fill; `audio.rs`; WASAPI loopback/mic + stateful linear resampler; `recui.rs`; stop pill; `recdone.rs`; matte-aware video editor; `video_edit.rs`; normalized time-ranged annotation model shared by preview and export; `video_speed.rs`; speed sections; `trim.rs`; source reader (`ENABLE_ADVANCED_VIDEO_PROCESSING`, streams resolved via `GetNativeMediaType`, never assume stream 0) → frame-accurate edit export; PCM packets are clipped to the selected `[start, end)` so ordinary trims share the speed-section boundary math.
- `scroll.rs`: scrolling capture (see the README).
- `ocr.rs`: Windows.Media.Ocr, whole-capture text plus per-word boxes mapped back out of the engine's input downscale.
- `pin.rs`: floating pinned captures; `delay.rs`; the countdown pill for delayed capture.
- `history.rs` / `thumb_decode.rs`: the local History index and its window, with thumbnails decoded off the UI thread.
- `settings.rs`: settings window, laid out by a cursor so a row is one call; `welcome.rs`; first-run window; `tray.rs`; tray icon/menu and autostart shortcut.
- `config.rs`: `%APPDATA%\matteshot\config.json`, with a last-known-good copy so a corrupt file never silently resets settings.
- `prtscn.rs`: PrtScn acquisition and release; `hotkey.rs`; shortcut parsing and labels.
- `theme.rs` / `theme_contrast.rs`: light, dark and High Contrast palettes, with contrast checked in tests; `dpi.rs`; per-monitor scaling helpers.
- `output.rs`: clipboard (manual CF_DIB + PNG + CF_HDROP), atomic save, reveal; `state_lock.rs`; named mutexes and atomic writes.
- `update.rs`: WinHTTP version check at startup and daily, then the background install; `installer.rs`; download, verification, and silent execution; `release_manifest.rs`; the Ed25519-signed release record and the embedded release public keys.
- `share.rs` / `share/upload.rs`: the optional Share action. The upload half only exists with `--features share`; see [self-hosting-share.md](self-hosting-share.md).
- `diagnostics.rs`: the bounded local log at `%LOCALAPPDATA%\Matteshot\matteshot.log` and the privacy-safe support report.
- `window.rs`: finding capture targets, and the list of surface window classes that idle and `--quit` watch; `completion.rs`; the token mailbox for worker results; `autostart_toggle.rs`; the Settings policy for the Startup shortcut.
- `icon.rs`: draws the app icon with the product's own compose pipeline (`matteshot --icon assets`); `spike.rs`; an experimental DPI supersampling probe behind `--spike-dpi`, not used by the app.

## Update trust chain

`version.json` on matteshot.app is discovery only. Before anything is offered, the client fetches `{installer}.release.json` from download.matteshot.app and verifies its Ed25519 signature against a public key compiled into `release_manifest.rs`. That record binds the version, the immutable versioned URL, the length and the SHA-256. The downloaded installer must match it and must carry a valid Authenticode signature whose subject is the release signer, and both checks run again immediately before the installer is launched. The release signing key never leaves the `release` environment in GitHub Actions; see [RELEASING.md](RELEASING.md).

## Windows landmines

- **DPI**: `PerMonitorV2` at startup or captures come out soft on mixed-DPI setups.
- **PrtScn**: Win11 routes it to Snipping Tool. On 23H2/24H2 that's `PrintScreenKeyForSnippingEnabled` (HKCU\Control Panel\Keyboard, missing = enabled), but Insider 26220+ can ignore that value and consume the key even after `RegisterHotKey(VK_SNAPSHOT)` reports success. The resident therefore owns PrtScn with a `WH_KEYBOARD_LL` hook and posts the same `WM_HOTKEY` used by every nested picker/editor loop. The hook is removed on toggle or process exit, so Matteshot stops consuming the key. Registry routing remains only as a compatibility fallback: `release` puts `PrintScreenKeyForSnippingEnabled` back only if we flipped it this run. A prior-off value stays off; missing stays missing. `--restore-printscreen` is the explicit force-on write.
- **Synthetic PrtScn is untestable** while Snipping routing is on: injected VK_SNAPSHOT never reaches hotkey dispatch.
- **WGC corner alpha varies by build**: Matteshot applies its own SDF corner mask unconditionally.
- **`FindWindowW` doesn't match** Matteshot's toolwindow popups even though `EnumWindows` sees them: don't use it in tests.
- **Z-bands outrank `WS_EX_TOPMOST` absolutely.** Every top-level window sits in a band, and nothing an ordinary process creates is ever placed above a window in a higher one. Measured on 26220: the notification center and Quick Settings are band 4, Task View 5, Start and Search 6, against band 1 for anything Matteshot can make. So a capture overlay is drawn *under* an open flyout and never receives the hit test: the crosshair reverts to an arrow and drags land on a panel that is still scrolling and dismissing under the cursor. They are also invisible to `EnumWindows`, so they cannot be listed as targets either. The freeze itself is fine, so the overlay notes the flyout before freezing, dismisses the live one afterwards (Esc; the foreground drops back ~190-200ms later and it stops answering `WindowFromPoint` in the same frame), and injects its rect as a target by hand. `GetWindowBand` is an undocumented user32 export, resolved at runtime. `uiAccess` + `CreateWindowInBand` is the usual advice and is a dead end: `ZBID_UIACCESS` is band 2, still below the notification center, and it would force a Program Files install.
- **MF AAC** rejects float PCM (`0xC00D36B4`) and non-44.1/48k rates: convert to 16-bit PCM and resample first.
- **MF source reader**: stream 0 is not necessarily video: resolve via `GetNativeMediaType` or you'll mux garbage.
- **AdjustWindowRectEx everywhere**: never guess non-client frame sizes.
- **H.264 caps a frame** near 9.4M luma samples. A matte with a forced aspect can compose a large recording past that (a 2560x1392 source at 1:1 lands on 3088x3088), and Media Foundation reports only an invalid-media-type error. Exports shrink the content until the framed result fits rather than failing.
- **OCR input is capped** at `MaxImageDimension` (2600). Oversized captures are downscaled before recognition and word boxes are scaled back out, so select-text stays aligned: but a tall scrolling stitch loses enough detail that recognition itself suffers. Tiled recognition is the fix if that matters.
- **GDI has no alpha on `Rectangle`**: translucent overlays (select-text highlights) stretch a 1x1 solid through `AlphaBlend`.
