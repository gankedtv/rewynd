#!/usr/bin/env bash
# Fetch the pinned static ffmpeg build for the given platform, verify its sha256, and drop the
# ffmpeg binary (only — no ffprobe/ffplay) into the given vpk pack dir, so installs can play
# and crop clips in-app without a system ffmpeg. The pins live in release.yml (FFMPEG_* env):
# a monthly BtbN autobuild tag for Linux/Windows (the project keeps them for two years), and a
# Martin Riedl release build for macOS (BtbN ships no macOS builds).
#
# Usage: fetch-ffmpeg.sh <linux64|win64|macarm64> <pack-dir>
set -euo pipefail

platform="$1"
dest="$2"
btbn="https://github.com/BtbN/FFmpeg-Builds/releases/download/$FFMPEG_TAG"
# `member` is the binary's path inside the archive.
case "$platform" in
    linux64)
        url="$btbn/$FFMPEG_LINUX_ASSET"
        sha256="$FFMPEG_LINUX_SHA256"
        bin="ffmpeg"
        member="${FFMPEG_LINUX_ASSET%.tar.xz}/bin/$bin"
        ;;
    win64)
        url="$btbn/$FFMPEG_WIN_ASSET"
        sha256="$FFMPEG_WIN_SHA256"
        bin="ffmpeg.exe"
        member="${FFMPEG_WIN_ASSET%.zip}/bin/$bin"
        ;;
    macarm64)
        url="https://ffmpeg.martin-riedl.de/download/macos/arm64/$FFMPEG_MAC_BUILD/ffmpeg.zip"
        sha256="$FFMPEG_MAC_SHA256"
        bin="ffmpeg"
        member="$bin"
        ;;
    *)
        echo "unknown platform: $platform" >&2
        exit 1
        ;;
esac

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
archive="$work/${url##*/}"
curl -fsSLo "$archive" "$url"
# Compared by hand rather than with `sha256sum -c`: macOS's BSD sha256sum exits 0 on a malformed
# check line, so an empty pin would pass there.
actual="$(sha256sum < "$archive")"
actual="${actual%% *}"
if [[ "$actual" != "$sha256" ]]; then
    echo "sha256 mismatch for $url: got $actual, pinned $sha256" >&2
    exit 1
fi

# The .zip goes through Python's zipfile where there is no unzip: the Windows runner's git-bash
# resolves `tar` to GNU tar, which cannot read zip (only the system bsdtar can, and PATH hides
# it).
case "$archive" in
    *.tar.xz)
        tar -xf "$archive" -C "$work" "$member"
        ;;
    *.zip)
        if command -v unzip >/dev/null; then
            unzip -q "$archive" "$member" -d "$work"
        else
            python -m zipfile -e "$archive" "$work/"
        fi
        ;;
esac
install -m 755 "$work/$member" "$dest/$bin"
echo "bundled $("$dest/$bin" -version | head -n 1)"
