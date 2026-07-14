# AUR package (`rewynd`)

The source PKGBUILD for Arch-family distros lives here so a release bump is one in-tree
commit. The AUR is a separate git remote; publishing is a copy, not a submodule.

## Release bump

1. Edit `PKGBUILD`: set `_tag` to the new tag, `pkgver` to its hyphen-free form
   (`v1.0.0-beta.4` → `1.0.0beta4`), reset `pkgrel=1`, then refresh the tarball hash:
   `updpkgsums` (or paste the sha256 of the tag tarball).
2. Validate locally: `makepkg -f` builds, tests and packages; `namcap *.pkg.tar.zst` for lint.
3. Regenerate metadata: `makepkg --printsrcinfo > .SRCINFO`.

## Publish to the AUR

```sh
git clone ssh://aur@aur.archlinux.org/rewynd.git aur-rewynd
cp PKGBUILD .SRCINFO tv.ganked.rewynd.desktop aur-rewynd/
cd aur-rewynd && git add -A && git commit -m "Update to <version>" && git push
```

The first push claims the package name (needs an AUR account with an SSH key). CI automation
of the bump (deploy key + push on tag) is tracked in the distribution issue; manual is fine
while releases are betas.

## Notes

- The app's self-updater sees no Velopack receipt under a pacman install and hides its update
  UI; updates flow through the AUR.
- An AUR build carries no compiled-in YouTube OAuth client (that is injected from repo secrets
  on official release builds). YouTube upload works once the user supplies their own client in
  the app's advanced settings (`docs/youtube-oauth.md`); everything else is unaffected.
- `packaging/aur/` is not part of the published crates; `.SRCINFO` here is regenerated, never
  hand-edited.
