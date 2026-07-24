#!/usr/bin/env sh
# rewynd installer: fetch the latest release for this OS and install it.
#
#   curl -fsSL https://raw.githubusercontent.com/gankedtv/rewynd/main/install.sh | sh
#
# Linux gets the self-updating AppImage in ~/.local/bin; macOS (Apple Silicon) gets
# rewynd.app in ~/Applications. Override the destination with REWYND_INSTALL_DIR. Both
# builds self-update from GitHub Releases, so this only bootstraps them.
set -eu

REPO="gankedtv/rewynd"

# Resolve a release asset's download URL by exact filename. /releases (newest first)
# includes prereleases, so the beta is picked up; match only asset download URLs, not
# URLs that happen to appear in the release notes. Dots in the name are escaped so they
# match literally rather than as regex wildcards.
resolve_url() {
  esc=$(printf '%s' "$1" | sed 's/\./\\./g')
  curl -fsSL "https://api.github.com/repos/$REPO/releases" \
    | grep -o "\"browser_download_url\": *\"[^\"]*/$esc\"" \
    | grep -o 'https://[^"]*' | head -n 1
}

install_linux() {
  DEST="${REWYND_INSTALL_DIR:-$HOME/.local/bin}"
  BIN="$DEST/rewynd"

  URL=$(resolve_url "rewynd.AppImage")
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
}

install_macos() {
  # Apple Silicon only (ADR 0015). sysctl reports the hardware even from a Rosetta
  # x86_64 shell, so a real Apple Silicon Mac is never falsely rejected; uname is the
  # fallback when the sysctl key is unavailable.
  if [ "$(sysctl -n hw.optional.arm64 2>/dev/null || echo 0)" != "1" ] \
     && [ "$(uname -m)" != "arm64" ]; then
    echo "error: rewynd ships an Apple Silicon build only; this Mac reports $(uname -m)." >&2
    exit 1
  fi

  APPDIR="${REWYND_INSTALL_DIR:-$HOME/Applications}"

  URL=$(resolve_url "rewynd-osx-Portable.zip")
  if [ -z "${URL:-}" ]; then
    echo "error: no rewynd-osx-Portable.zip asset found in the $REPO releases" >&2
    exit 1
  fi

  work=$(mktemp -d)
  trap 'rm -rf "$work"' EXIT

  echo "Downloading $URL"
  curl -fSL --progress-bar "$URL" -o "$work/rewynd.zip"
  # ditto is the macOS-native unzip and preserves the bundle; fall back to unzip.
  if command -v ditto >/dev/null 2>&1; then
    ditto -x -k "$work/rewynd.zip" "$work/extract"
  else
    unzip -q "$work/rewynd.zip" -d "$work/extract"
  fi

  app=$(find "$work/extract" -maxdepth 1 -name '*.app' -type d | head -n 1)
  if [ -z "${app:-}" ]; then
    echo "error: no .app bundle found in the downloaded archive" >&2
    exit 1
  fi

  mkdir -p "$APPDIR"
  # Stage the new bundle beside the target, then swap it in with same-directory
  # renames, so a failed (or cross-volume) copy leaves any existing install intact.
  target="$APPDIR/rewynd.app"
  staged="$target.new"
  backup="$target.bak"
  rm -rf "$staged" "$backup"
  mv "$app" "$staged"
  # The build is unsigned; clearing quarantine lets it open on a plain double-click
  # instead of Gatekeeper's "damaged" wall.
  xattr -dr com.apple.quarantine "$staged" 2>/dev/null || true
  if [ -e "$target" ]; then mv "$target" "$backup"; fi
  mv "$staged" "$target"
  rm -rf "$backup"

  echo "Installed rewynd to $target"
  echo "Open rewynd from Spotlight or Launchpad. It self-updates from GitHub Releases."
}

echo "Finding the latest rewynd release..."
case "$(uname -s)" in
  Linux) install_linux ;;
  Darwin) install_macos ;;
  *) echo "error: unsupported OS '$(uname -s)'; see the README for manual install." >&2; exit 1 ;;
esac
