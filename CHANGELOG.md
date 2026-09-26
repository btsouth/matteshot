# Changelog

Notable changes to Matteshot. Releases before 0.21.0 are listed on [GitHub Releases](https://github.com/btsouth/matteshot/releases).

## Unreleased

### Free and open source

- Matteshot is now free and open source under MIT OR Apache-2.0. The source is on GitHub.
- The 14-day trial, license keys, activation, device limits and the update term are gone. Nothing in the app checks a license, contacts a license server, or stops working offline.
- An install that had an ended trial or a paid license keeps working without doing anything. The old `license.json` and registry values are left where they were and ignored; settings, History and captures are untouched.
- Usage statistics are gone. Matteshot sends no telemetry of any kind, and the consent box on the welcome screen and the checkbox in Settings are removed. A config that still has the old `telemetry` keys loads normally.
- Share is no longer part of the official build, because there is no longer a hosted upload service. It is available to anyone who builds with `--features share` and runs their own server from `share-server/`; see `docs/self-hosting-share.md`.
- The tray menu no longer has Buy or license entries, and Settings no longer has Deactivate or the update-term note. Its footer reads "Free and open source".
- Updates still arrive automatically and are still verified against the signed release record and the Authenticode signature before they install.

### Fixes and hardening since 0.20.0

- The picker no longer shows "copied" until the background copy has actually landed.
- A recording or an export is no longer deleted when its validation check cannot run or the final rename fails; the partial file is kept for recovery.
- A corrupt config file stays in place until a good copy has been written.
- History opens straight away and decodes thumbnails in the background at a bounded size.
- History keeps rows for captures on an ejected drive or offline share instead of dropping them, and Delete on such a row fails without touching the index. The message now says the drive or folder is unavailable instead of guessing that a volume went offline.
- History drops rows for captures deleted in Explorer and writes the pruned list back. Settings has a new **Clear History titles** action.
- Settings fits inside the monitor's work area and scrolls, so its bottom controls stay reachable on small or scaled screens.
- Screenshot export composes at the output size before padding to an aspect ratio, so tall scroll captures no longer build huge canvases.
- Quitting or uninstalling puts the PrtScn key back the way it was before Matteshot took it, instead of always turning Snipping Tool on.
- Updates are only authorized by an Ed25519-signed release record, and the staged installer's hash and signature are checked again right before it runs. A failed or deferred automatic install is retried on the next check.
- The executable and installer carry complete publisher and version metadata.
- A second Share click while an upload is running is refused.
- Recording warns when part of the picture came out black because it was protected video.

### For contributors

- New: `LICENSE-MIT`, `LICENSE-APACHE`, `SECURITY.md`, `CONTRIBUTING.md`, issue and pull request templates, `docs/ARCHITECTURE.md`, `docs/RELEASING.md`, and `docs/self-hosting-share.md`.
- CI runs for pull requests from forks on GitHub-hosted runners with no secrets. It checks formatting, tests and lints both the default and the `share` build, enforces a dependency license and source policy with cargo-deny, and tests the share server.
- The version resource is generated with `embed-resource` in place of the unmaintained `winres`, which also lets the Windows target be type-checked and linted from Linux with `cargo xwin`.
- GitHub Actions and the share server's dependencies are updated, and Dependabot now watches Cargo, npm and Actions.
