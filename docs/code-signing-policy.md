# Code signing policy

Windows release binaries of rewynd are code-signed with a certificate issued to the
[SignPath Foundation](https://signpath.org/), which provides free code signing for open
source projects. Free code signing provided by [SignPath.io](https://signpath.io/),
certificate by SignPath Foundation.

This document describes who controls the project, how releases are built, and what the
signature guarantees. Signing starts with the first release after SignPath Foundation
onboarding completes; earlier beta builds are unsigned.

## Team and roles

| Role | Who |
| --- | --- |
| Maintainer (committer, reviewer, release approver) | Thijs Herman ([@Turbootzz](https://github.com/Turbootzz)) |

rewynd is maintained by a single developer. The maintainer is the only person with write
access to the repository and the only person who can approve signing requests. External
contributions are accepted via pull request only and are reviewed by the maintainer
before merging. All accounts with repository or signing access have two-factor
authentication enabled.

## Build provenance

Releases are built exclusively by GitHub Actions from a tagged commit of the public
repository ([`.github/workflows/release.yml`](../.github/workflows/release.yml)). The
release workflow runs the same format/lint/test/dependency-audit gates as every push,
builds with `cargo build --locked` so dependencies match the committed `Cargo.lock`, and
packages with [Velopack](https://velopack.io/). No release artifact is ever built or
modified on a developer machine.

Once signing is active, signing requests come from that CI workflow only, never from a
developer machine, and each release is manually approved by the maintainer in SignPath
before the certificate is applied.

The Windows artifacts to be signed are the executables Velopack packages and installs:
`rewynd.exe` (the settings/library app), `rewynd-recorder.exe` (the background
recorder), the bundled `Update.exe` updater, and the `rewynd-win-Setup.exe` installer.

Dependencies are open source Rust crates compiled from source into the binaries (plus
the vendored fork in [`vendor/`](../vendor)); the release ships no separate third-party
executables.

## What rewynd does with your data

rewynd is a local instant-replay recorder. Recorded clips are written to a folder on
your machine and stay there. Nothing is uploaded unless you explicitly trigger an
upload, and uploads only go to destinations you configured with your own credentials:
ganked.tv (your API key) or YouTube (your Google account, via OAuth with your consent).

The app contacts GitHub on launch to check for updates (Velopack against this
repository's releases). There is no telemetry, no analytics, and no account
requirement.

## Reporting

If you believe a signed rewynd binary is misbehaving or a certificate is being misused,
open an issue on this repository or contact the maintainer. Confirmed abuse can also be
reported to SignPath Foundation, which can revoke the certificate.
