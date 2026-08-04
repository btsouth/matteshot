# Matteshot interactive regression pass

Run this after `scripts\verify-code.ps1` and `scripts\run-probes.ps1`. These
checks deliberately remain human-driven: automating them would write the
clipboard, move the pointer, inject keys, or steal the foreground from Tyler.

A failed or ambiguous check blocks release. Record the Windows build, monitor
layout/DPI, theme, and audio mode with the result.

## Capture overlay

- [ ] PrtScn opens one frozen overlay spanning every monitor without a live-frame flash.
- [ ] Hover selects the expected window, including maximized windows and the taskbar.
- [ ] Window, region, screen, record, and scroll modes match their toolbar shortcuts.
- [ ] A cross-monitor region has the expected bounds and DPI on both displays.
- [ ] Esc restores the desktop without saving, copying, or leaving a surface behind.
- [ ] PrtScn from the picker or either editor returns to capture and replaces only the pending shot.

## Picker and zero-touch output

- [ ] The preselected matte is saved and already on the clipboard when the picker appears.
- [ ] Click, 1-7, Left/Right wrapping, and Enter all choose the displayed variant.
- [ ] T opens the selected variant in the tweak editor.
- [ ] P pins the raw capture; C copies recognized text; both leave the picker cleanly.
- [ ] Esc keeps the successful background auto-copy, and paste matches the preselected matte.
- [ ] A second PrtScn can capture the picker itself; cancelling the new overlay returns to the picker.
- [ ] A wide capture (the taskbar) stacks the variants into rows and stays inside one monitor.

## Screenshot editor

- [ ] Multiple captures become tabs; Ctrl+Tab, Ctrl+Shift+Tab, Ctrl+1-9, middle-click, and close choose the expected neighbour.
- [ ] Every annotation can be created, selected, moved, resized, deleted, and undone.
- [ ] Text accepts Unicode, can be re-edited, and preserves its style after matte/aspect changes.
- [ ] OCR selection tracks words after matte, padding, aspect, and window-size changes.
- [ ] Pixelated words cannot be selected or copied through the redaction.
- [ ] Copy keeps the active tab open with a brief confirmation (or closes it when the Settings toggle is off); Save closes only the active tab after output succeeds.
- [ ] If saving or clipboard copy fails, the error is visible and the active tab remains open.

## Recording and video editor

- [ ] Window and region recordings open promptly and keep a stable canvas through target resize.
- [ ] Off, System, and Mic modes produce a playable file with the expected audio.
- [ ] Play/Pause, Space, keyboard seeks, timeline scrubbing, and trim handles stay synchronized.
- [ ] Matte, padding, aspect, and annotations remain responsive during playback.
- [ ] The + Add drawer shows Arrow, Line, Box, Oval, Mark, Text, Blur, Step, and Pen in a balanced 3x3 grid; each renders in preview and export, Step numbers advance on consecutive clicks without re-picking, and Pen stays active across strokes until toggled or Escaped.
- [ ] Existing and newly added annotations remain visible and editable on the terminal frame.
- [ ] Export progress advances; cancellation keeps the original and removes incomplete output.
- [ ] Successful export preserves audio, never overwrites an earlier edit, and copies the edit.
- [ ] Clipboard failure reports "saved" without claiming the file was copied.
- [ ] Closing after export reveals the edited file; Original actions still target the original.

## Scrolling, display, and lifecycle

- [ ] Notepad instant scrolling and Chrome smooth scrolling stitch without repeated chrome or missing rows.
- [ ] Light/dark changes repaint every open surface, popup, and title bar.
- [ ] 100%, 125%, 150%, and mixed-DPI layouts keep controls visible and captures sharp.
- [ ] Duplicate launch opens Settings on the resident; quitting restores PrtScn to Windows.
- [ ] Tray menu is slim: capture, active window, open captures/videos folders, license, Settings. Deactivate and Copy diagnostics appear only in Settings, and Deactivate frees the license slot with the confirmation and hotkey teardown.
- [ ] Sleep/resume and display connect/disconnect leave the resident responsive.
- [ ] An available update defers while a capture, recording, editor, or export is active.

## Trial and purchase

Every machine that has tested Matteshot so far has been licensed, so this path
has never run end to end. Build with `cargo build --release --features
debug-license --target-dir target/debug-license`, so the override build never
replaces the binary a release was cut from, then set
`MATTESHOT_LICENSE_OVERRIDE` per row. Unset it and confirm `--license-status`
reports the real state before closing out.

Two things to know before running these:

- **The override has side effects, even though it stores nothing itself.**
  Everything downstream branches on what `status()` reports, so `expired` sends
  the background sync down the trial branch and registers a trial record for
  the device on the server. On a licensed machine that record is already past
  its 14 days, so it is harmless, but it outlives the test.
- **Launch without redirecting stderr.** `Start-Process -RedirectStandardError`
  forces `UseShellExecute=false`, and the activation window then reports
  `IsWindowVisible=false` and never paints. That is the harness, not a bug, and
  it looks exactly like a broken window if you are not expecting it. Redirect
  only for the console-output test flags.

- [ ] `not-started`: capture works and the tray reads "14-day trial ready".
- [ ] `trial:3`: the tray reads "Trial: 3 days left" and capture is unaffected.
- [ ] `expired`: PrtScn no longer captures and the activation window appears.
- [ ] Buy from the expired window, and from the tray, opens the pricing card at
      matteshot.app rather than the top of the page.
- [ ] Activating a real key from the expired state restores the hotkeys without
      a restart, and the tray switches to the licensed label.
- [ ] Unset the override: `--license-status` reports the machine's real state
      and no forced-state line.

## Closeout

- [ ] Restore Tyler's original audio, folders, theme override, and other settings.
- [ ] Confirm exactly one resident process is running the intended release candidate.
- [ ] Re-run `scripts\run-probes.ps1` after any fix made during this pass.
