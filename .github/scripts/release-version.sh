#!/usr/bin/env bash
# Resolve the release version from the pushed tag and assert it matches the workspace version, so
# a mis-tag fails the release instead of shipping a mislabelled build. Emits `version` and `pre`
# (whether it is a prerelease) to $GITHUB_OUTPUT. Shared by both pack jobs to stay DRY.
set -euo pipefail

tag="${GITHUB_REF_NAME:?GITHUB_REF_NAME is not set}"
version="${tag#v}"

# The workspace version is the first `version = "..."` line in the root manifest
# ([workspace.package]); every crate inherits it via `version.workspace = true`.
cargo_version="$(grep -m1 -E '^version = "' Cargo.toml | sed -E 's/^version = "(.*)"/\1/')"

if [ "$version" != "$cargo_version" ]; then
  echo "::error::tag '$tag' (version '$version') does not match the workspace version '$cargo_version'"
  exit 1
fi

echo "version=$version" >>"$GITHUB_OUTPUT"
# A SemVer prerelease carries a `-` (e.g. 1.0.0-beta.1); a stable release does not.
if [[ "$version" == *-* ]]; then
  echo "pre=true" >>"$GITHUB_OUTPUT"
else
  echo "pre=false" >>"$GITHUB_OUTPUT"
fi
echo "Releasing version $version (prerelease: $([[ "$version" == *-* ]] && echo yes || echo no))"
