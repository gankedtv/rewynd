#!/usr/bin/env sh
# rewynd Linux installer: fetch the latest AppImage into ~/.local/bin and make it runnable.
#
#   curl -fsSL https://raw.githubusercontent.com/gankedtv/rewynd/main/install.sh | sh
#
# Override the destination with REWYND_INSTALL_DIR. The AppImage self-integrates into the app
# menu on first run and self-updates from GitHub Releases, so this only bootstraps it.
set -eu

REPO="gankedtv/rewynd"
DEST="${REWYND_INSTALL_DIR:-$HOME/.local/bin}"
BIN="$DEST/rewynd"

echo "Finding the latest rewynd release..."
# /releases (newest first) includes prereleases, so the beta is picked up; grab its AppImage.
URL=$(curl -fsSL "https://api.github.com/repos/$REPO/releases" \
  | grep -o 'https://[^"]*/rewynd\.AppImage' | head -n 1)
if [ -z "${URL:-}" ]; then
  echo "error: no rewynd.AppImage asset found in the $REPO releases" >&2
  exit 1
fi

mkdir -p "$DEST"
echo "Downloading $URL"
curl -fSL --progress-bar "$URL" -o "$BIN.tmp"
chmod +x "$BIN.tmp"
mv "$BIN.tmp" "$BIN"

echo "Installed rewynd to $BIN"
case ":${PATH}:" in
  *":$DEST:"*) ;;
  *) echo "note: $DEST is not on your PATH; add it, or run $BIN directly." ;;
esac
echo "Run 'rewynd' to open the app. It adds itself to your app menu and self-updates."
