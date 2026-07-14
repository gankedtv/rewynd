#!/usr/bin/env bash
# Fetch the pinned BtbN ffmpeg build for the given platform, verify its sha256, and drop the
# ffmpeg binary (only — no ffprobe/ffplay) into the given vpk pack dir, so installs can play
# clips in-app without a system ffmpeg. The pin lives in release.yml (FFMPEG_* env): a monthly
# BtbN autobuild tag, which the project keeps for two years.
#
# Usage: fetch-ffmpeg.sh <linux64|win64> <pack-dir>
set -euo pipefail

platform="$1"
dest="$2"
case "$platform" in
    linux64)
        asset="$FFMPEG_LINUX_ASSET"
        sha256="$FFMPEG_LINUX_SHA256"
        bin="ffmpeg"
        ;;
    win64)
        asset="$FFMPEG_WIN_ASSET"
        sha256="$FFMPEG_WIN_SHA256"
        bin="ffmpeg.exe"
        ;;
    *)
        echo "unknown platform: $platform" >&2
        exit 1
        ;;
esac

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
curl -fsSLo "$work/$asset" \
    "https://github.com/BtbN/FFmpeg-Builds/releases/download/$FFMPEG_TAG/$asset"
echo "$sha256  $work/$asset" | sha256sum -c -

# The archive root dir matches the asset basename; bsdtar (present on every runner, Windows
# included) extracts both the .tar.xz and the .zip.
case "$asset" in
    *.tar.xz) root="${asset%.tar.xz}" ;;
    *.zip) root="${asset%.zip}" ;;
esac
tar -xf "$work/$asset" -C "$work" "$root/bin/$bin"
install -m 755 "$work/$root/bin/$bin" "$dest/$bin"
echo "bundled $("$dest/$bin" -version | head -n 1)"
