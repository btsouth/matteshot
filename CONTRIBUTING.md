# Contributing to Matteshot

Thanks for helping. Matteshot is a small Windows app with one maintainer, so a focused change with a clear reason is the easiest thing to review and merge.

## Before you start

- For anything bigger than a bug fix, open an issue first so we can agree on the approach before you spend the time.
- Security problems go through [SECURITY.md](SECURITY.md), not public issues.
- Matteshot has no account, no telemetry and no paid features, and it will stay that way. Changes that add tracking, ads, or anything that phones home will not be merged.

## Setting up

Follow [Building from source](README.md#building-from-source) in the README. You need Windows 10 or 11 to run the app; you can type-check and lint from Linux or macOS with `cargo xwin`.

[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) explains how the pieces fit together and lists the Windows behaviour that shaped them. Read the landmines section before you touch capture, PrtScn handling, Media Foundation or window management.

## What a pull request needs

1. `cargo fmt --all` with no changes left over.
2. `.\scripts\verify-code.ps1` passing. That runs the script tests, the RustSec audit, every unit test, strict Clippy for both the default and the `share` build, and a release build. CI runs the same script on every pull request.
3. A test for the behaviour you changed when it can be tested without a desktop. Most logic in this codebase is written so it can be: layout, parsing, state machines, file handling.
4. If you changed something only a person can check (capture, the editors, recording, the tray), say what you checked by hand and on which Windows build, monitor setup and scaling. [INTERACTIVE-REGRESSION.md](INTERACTIVE-REGRESSION.md) is the list we use before a release.
5. If your change affects what users see or what leaves their machine, update the README in the same pull request.

Keep commits focused and write the subject as what the change does ("Keep the crop handles inside a small window"). Squash-merging is the default, so a messy branch history is fine.

## CI and forks

Pull requests from forks run the normal checks on GitHub-hosted runners with a read-only token and no secrets. Signing and publishing only happen from version tags on `main`, in a separate protected environment, so nothing in a pull request can reach them. A maintainer may need to approve the first workflow run from a new contributor.

## License

By contributing, you agree that your contributions are licensed under the same terms as the project: MIT OR Apache-2.0, at the user's option. See the License section of the README.
