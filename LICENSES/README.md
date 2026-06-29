# Third-party license notices

rewynd is distributed under the GPL-3.0-or-later license (see the root `LICENSE`).
This directory collects the attribution notices for bundled third-party dependencies
that require their copyright/permission notice to be reproduced in distributed builds.
These notices must ship with any binary distribution (an "about" screen referencing
this directory is sufficient).

| Dependency | License | Notice |
| --- | --- | --- |
| [`gpu-video`](https://github.com/software-mansion/smelter) (Software Mansion) | MIT | [gpu-video-MIT.txt](gpu-video-MIT.txt) |

> Note: `gpu-video` the **crate** is MIT (per its own `LICENSE`); the broader *smelter*
> product is under a separate source-available license that does **not** govern this
> library dependency (PLAN §3.7).

Other core dependencies (`wgpu`, `naga`, `ash`, `tracing`, …) are MIT/Apache-2.0.
Their notices will be folded in here (or generated via `cargo-about`) before the
first distributed release.
