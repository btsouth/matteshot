# Releasing Matteshot

Official builds are signed by the maintainer's Azure Trusted Signing identity and published to `download.matteshot.app` and GitHub Releases. Only a maintainer can cut one; this page is how.

## What a release needs

The GitHub Actions `release` environment holds everything the signing and publishing jobs use. Its deployment policy only admits `v*` tags and `main`, so a pull request, a fork, or a branch can never reach it.

| Name | Kind | Used for |
|---|---|---|
| `AZURE_CLIENT_ID`, `AZURE_TENANT_ID`, `AZURE_SUBSCRIPTION_ID` | variables | GitHub OIDC login to Azure (no stored Azure secret) |
| `ARTIFACT_SIGNING_ENDPOINT`, `ARTIFACT_SIGNING_ACCOUNT_NAME`, `ARTIFACT_SIGNING_CERTIFICATE_PROFILE_NAME` | variables | Trusted Signing for the exe, installer and uninstaller |
| `CLOUDFLARE_ACCOUNT_ID`, `CLOUDFLARE_ZONE_ID` | variables | R2 publishing and the edge purge |
| `CLOUDFLARE_R2_API_TOKEN` | secret | uploads to the `matteshot-downloads` bucket |
| `CLOUDFLARE_CACHE_PURGE_API_TOKEN` | secret | purges the stable download names (optional) |
| `MATTESHOT_RELEASE_SIGNING_KEY` | secret | the Ed25519 seed for key id `2026.1`, which signs each release record |

`Verify release credentials` (workflow_dispatch) checks the Cloudflare tokens without publishing anything.

## Steps

1. **Version.** Set the new version in `Cargo.toml`, run `cargo update -p matteshot` so `Cargo.lock` agrees, and update the fallback `AppVersion` in `installer/matteshot.iss`. `scripts/validate-release-version.ps1` checks that all three match the tag.
2. **Changelog.** Add a section to `CHANGELOG.md`.
3. **Merge.** Open a pull request with those changes and merge it once CI is green.
4. **Candidate (recommended).** Run the `Release candidate` workflow on `main`. It builds, signs, and probes a private candidate and uploads it as an artifact. Install that candidate and run `scripts\run-sandbox-smoke.ps1` against it, then go through `INTERACTIVE-REGRESSION.md`. A failed or unclear row blocks the release.
5. **Tag.** `git tag vX.Y.Z <merged commit> && git push origin vX.Y.Z`. The tag must point at a commit on `main`; `release.yml` refuses anything else. That workflow:
   - builds and runs `scripts/verify-code.ps1`,
   - signs `matteshot.exe`, builds the installer and signs it and its uninstaller,
   - probes the signed artifact on a fresh runner, including the updater's own signature gate,
   - signs the release record, uploads `MatteshotSetup-X.Y.Z.exe`, its `.sha256` and `.release.json` plus the stable `MatteshotSetup.exe` names to R2, and re-downloads them to check the hashes,
   - creates a **draft** GitHub release with the installer, checksum, and release record.
6. **Check and publish the draft.** Download the installer from the draft, compare its SHA-256 with the notes, and confirm `Get-AuthenticodeSignature` reports `Valid` with signer `Brandon South`. Replace the draft notes with the changelog section, then publish it: `gh release edit vX.Y.Z --draft=false --latest`.
7. **Point the updater at it.** Add the release to the top of `releases` in the site's `public/version.json` and set the top-level `version`, `download` and `released` to match. `download` must be the versioned URL (`MatteshotSetup-X.Y.Z.exe`), because that is the URL the signed record binds. `released` is UTC: `TZ=UTC git log -1 --format=%cd --date=format-local:"%Y-%m-%dT%H:%M:%SZ" vX.Y.Z`. Never remove an entry or delete an installer from R2; older builds still read that list. Deploy the site and check `https://matteshot.app/version.json`.
8. **Confirm an update end to end.** On a machine with the previous release, run `matteshot --update-stage-test`. It must download and verify the new installer through the same gates a user's copy uses. An edge node can serve the old `version.json` for a minute after a deploy, so re-run it once before believing a failure.
9. **winget.** Submit the new version to [microsoft/winget-pkgs](https://github.com/microsoft/winget-pkgs) under `SouthboundSoftware.Matteshot`, with the versioned installer URL and its SHA-256. Copy the previous manifest and change the version, URL, hash and release notes URL.

## Rolling the release record key

The updater trusts every public key listed in `src/release_manifest.rs`. To roll: generate a new seed offline, append its public key under a new id, and ship a release signed with the old key. Once that release is out, sign with the new id (`--key-id` in `release.yml` and `scripts/sign-release-manifest.py`). Keep the old key listed until no supported build still needs it.
