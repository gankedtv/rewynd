// Pinned release artifacts. Bump RELEASE_TAG when a new release ships —
// /releases/latest 404s on GitHub while every release is a prerelease.
export const RELEASE_TAG = 'v1.0.0-beta.4';

const base = `https://github.com/gankedtv/rewynd/releases/download/${RELEASE_TAG}`;
export const APPIMAGE_URL = `${base}/rewynd.AppImage`;
export const WIN_SETUP_URL = `${base}/rewynd-win-Setup.exe`;
export const ALL_RELEASES_URL = 'https://github.com/gankedtv/rewynd/releases';

// One-line installer, shown in the hero, download card, and open-source strip.
// Points at the checked-in install.sh so it works today — no custom domain needed.
export const INSTALL_CMD = 'curl -fsSL https://raw.githubusercontent.com/gankedtv/rewynd/main/install.sh | sh';
