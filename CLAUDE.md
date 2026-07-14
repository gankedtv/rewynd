# CLAUDE.md — working guide for rewynd

`PLAN.md` is the single source of truth for *what* we're building and *why*. This file is the *how to work* companion. `.claude/PROGRESS.md` (gitignored) tracks live state across sessions.

## Build & validate
- `cargo build --workspace` · `cargo clippy --workspace --all-targets -- -D warnings` · `cargo fmt --all --check` — all must be green.
- Tests needing a GPU/Vulkan are `#[ignore]`d (CI has no GPU). Run them on this box with `-- --ignored`.
- The Linux capture probes need the live Wayland/KDE session + PipeWire; the XDG ScreenCast restore token is saved under `$XDG_STATE_HOME/rewynd`, so reruns don't re-prompt. PNG/diagnostic probe output goes to the temp dir, never the repo.
- Dev box: CachyOS / RTX 3080 Ti / KWin Wayland.

### Coverage
- CI gates **85% line coverage** on CI-testable code, excluding GPU/portal/macOS/GUI/wiring code via `--ignore-filename-regex '(capture/src/(linux|macos)/|gpu/src/|encode/src/(gpu_video_backend|software_texture|videotoolbox)\.rs|app/src/|settings/src/|installer/src/main\.rs|vendor/mp4/)'` (no GPU/live Wayland/display/tray/ScreenCaptureKit in CI; `app/src/` is the binary wiring + the tray; `settings/src/` is the iced GUI; `installer/src/main.rs` is the installer's iced window, while its Setup.exe driver in `setup.rs` stays covered; `encode/src/software_texture.rs` is the CPU encoder's GPU-readback adapter, while its pure CPU core in `software.rs` stays covered; `encode/src/videotoolbox.rs` needs a live VideoToolbox session, while the pure AVCC→Annex-B converter in `annexb.rs` stays covered; `vendor/mp4/` is the vendored third-party muxer fork — ADR 0004). Testable logic lives in the library crates.
- That GPU/portal code is validated by `#[ignore]`d tests on a GPU box, and the macOS capture/encode path by `#[ignore]`d tests against live ScreenCaptureKit/VideoToolbox on a Mac. Full local coverage including them: `cargo llvm-cov --workspace --include-ignored`.

## Hard rules
- **GPU pin:** `wgpu` git rev `1503796` + `gpu-video` git `4fff151f`, unified via `[patch.crates-io]`. Do not bump `wgpu`/`gpu-video` without an ADR (`docs/adr/`). See ADR 0001.
- **Resolution / framerate / bitrate are parameters, never hard-coded** (target 1080p60).
- **Comments:** keep them minimal — only non-obvious rationale/invariants/SAFETY. **No GitHub issue/PR numbers in source comments** (they belong in commit messages / PR descriptions).
- **No AI attribution** in commits or PRs; no `Co-authored-by: Claude` trailer. Human-sounding messages.

## Workflow
- Issue-driven: branch `N-slug` off `main` per issue → implement → `/review` (+ apply) → push → PR → CodeRabbit review (address findings) → merge. The repo squash-merges, so stacked PRs need a rebase after the base lands.
- **Always run the `/review` skill before pushing a PR** — the actual skill, not a few ad-hoc review agents. Apply its findings first.
- **Bundle small changes into larger PRs** rather than opening a PR per tweak: CodeRabbit reviews at most once per hour, and every PR costs a review slot + a CI run.
- **Read CodeRabbit's review properly before merging.** Its author login is `coderabbitai[bot]`, so match case-insensitively. The verdict (`Actionable comments posted: N`) lives in the PR *reviews*, and the findings are *inline review comments*, not issue comments: `gh api repos/<owner>/<repo>/pulls/<n>/comments --paginate`. Only treat it as "no findings" after checking that endpoint. If it says rate-limited / "Review limit reached", skip it and merge once CI is green (per the user's standing permission).
- CI installs the Linux capture system deps (`libpipewire-0.3-dev`, `clang`); keep that in `.github/workflows/ci.yml`.

## Habits
- **Research before implementing** against fast-moving crates (`wgpu`, `ashpd`, `pipewire`, `gpu-video`, …) — **freely web-search** current versions/APIs/features; don't trust training-era memory.
- **Periodic review:** roughly every ~5 merged PRs, sweep `main` for DRY/refactor/best-practice/library improvements. Do it if worthwhile (its own PR), otherwise continue to the next issue — don't force changes.
