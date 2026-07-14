#!/usr/bin/env bash
# Regenerate every raster brand asset from the SVG masters in this directory.
#
# Run after editing logo.svg or play-badge.svg:   docs/design/render-assets.sh
# Needs: rsvg-convert, ImageMagick (magick). The .icns needs python3 with the icnsutil
# package (override the interpreter with ICNS_PYTHON); it is skipped with a warning otherwise.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
brand="$repo/crates/config/assets/brand"
play="$repo/crates/settings/assets/play"
pkg="$repo/packaging"
fonts="$repo/crates/settings/assets/fonts"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# Brand mark: the sizes embedded in rewynd-config (BRAND_ICONS), plus the larger packaging
# renders (Linux vpk pack icon, macOS Dock override).
for s in 24 32 48 64 128; do
    rsvg-convert -w "$s" -h "$s" "$here/logo.svg" -o "$brand/logo-$s.png"
done
for s in 256 512; do
    rsvg-convert -w "$s" -h "$s" "$here/logo.svg" -o "$pkg/logo-$s.png"
done

# Play badge: the clip-preview play control in the settings app (its two-size decode cache).
mkdir -p "$play"
for s in 24 128; do
    rsvg-convert -w "$s" -h "$s" "$here/play-badge.svg" -o "$play/play-$s.png"
done

# Windows .ico: every size the existing resource shipped.
for s in 16 24 32 48 64 128 256; do
    rsvg-convert -w "$s" -h "$s" "$here/logo.svg" -o "$tmp/ico-$s.png"
done
magick "$tmp"/ico-{16,24,32,48,64,128,256}.png "$pkg/rewynd.ico"

# Installer splash (Velopack --splashImage, 480x272): background = the app's BACKGROUND
# (#0b0b0f), the mark, the Barlow Condensed wordmark, a mint status line.
rsvg-convert -w 96 -h 96 "$here/logo.svg" -o "$tmp/splash-logo.png"
magick -size 480x272 xc:'#0b0b0f' \
    "$tmp/splash-logo.png" -geometry +192+40 -composite \
    -font "$fonts/BarlowCondensed-Black.ttf" -pointsize 46 -fill '#f0f0f4' -kerning 2 \
    -gravity North -annotate +0+148 'REWYND' \
    -font "$fonts/Inter-Bold.ttf" -pointsize 13 -fill '#00e5a0' -kerning 6 \
    -gravity North -annotate +3+207 'SETTING UP' \
    "$pkg/splash.png"

# macOS .icns for the Velopack .app bundle.
icns_python="${ICNS_PYTHON:-python3}"
if "$icns_python" -c 'import icnsutil' 2>/dev/null; then
    for s in 16 32 64 128 512; do
        rsvg-convert -w "$s" -h "$s" "$here/logo.svg" -o "$tmp/icns-$s.png"
    done
    "$icns_python" - "$pkg/rewynd.icns" "$tmp" "$pkg/logo-256.png" <<'PY'
import sys
import icnsutil

out, tmp, png256 = sys.argv[1:4]
img = icnsutil.IcnsFile()
for key, size in (("icp4", 16), ("icp5", 32), ("icp6", 64), ("ic07", 128), ("ic09", 512)):
    img.add_media(key, file=f"{tmp}/icns-{size}.png")
img.add_media("ic08", file=png256)
img.write(out)
PY
else
    echo "warning: icnsutil not importable via '$icns_python'; skipped $pkg/rewynd.icns" >&2
fi

echo "regenerated brand PNGs, play badge, ico, splash$( "$icns_python" -c 'import icnsutil' 2>/dev/null && echo ', icns' )"
