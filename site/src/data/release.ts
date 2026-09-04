// Pinned release artifacts (/releases/latest 404s while every release is a prerelease).
// The release workflow bumps RELEASE_TAG after upload; edit it by hand only as a fallback.
export const RELEASE_TAG = 'v1.0.0-beta.9';

const base = `https://github.com/gankedtv/rewynd/releases/download/${RELEASE_TAG}`;
export const APPIMAGE_URL = `${base}/rewynd.AppImage`;
export const WIN_SETUP_URL = `${base}/rewynd-win-Setup.exe`;
export const OSX_SETUP_URL = `${base}/rewynd-osx-Setup.pkg`;
export const ALL_RELEASES_URL = 'https://github.com/gankedtv/rewynd/releases';

// One-line installer, shown in the hero, download card, and open-source strip.
// Points at the checked-in install.sh so it works today, no custom domain needed.
export const INSTALL_CMD = 'curl -fsSL https://raw.githubusercontent.com/gankedtv/rewynd/main/install.sh | sh';
