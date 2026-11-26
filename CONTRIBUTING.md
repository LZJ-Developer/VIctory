# Contributing to VIctory

Start from `main` and open pull requests against `LZJ-Developer/VIctory:main`. It contains the RTX-tested Windows runtime. To inspect the original November 27 measurements in a clean checkout, select the historical tag with `git switch --detach refs/tags/community/rtx4060-handoff-2026-11-27`.

## Verify a change

For a Windows build, use Developer PowerShell with the pinned Rust toolchain, x64 MSVC tools and a Windows SDK. [Build and hardware A/B instructions](README.md)

```powershell
cargo build --locked --release --features diagnostics -p victory --bin victory
cargo xtask policy
```

Anyone with PowerShell 7 can check the published dataset, including on a machine without the game or a GPU. Run the command below from the repository root. The November 27 sample contains 240 host Present intervals and yields 11.2108 FPS and a P99 of 325.20 ms. [Frame-time samples](docs/benchmarks/present-window.json)

```powershell
pwsh -NoProfile -File ./scripts/Measure-PresentWindow.ps1
```

## Report the result

Describe the observed problem, the commit tested and how someone can repeat the check. A documentation or build fix can be verified by following the revised instructions or building the affected target.

For performance changes, record the GPU and driver, scene, resolution, cache state, changed setting and sample count for each arm. Report GPU work or CPU waiting in its own units. To claim a frame-rate improvement, also give the host Present interval distribution (P50 and P99), FPS, and the number of frames matched for equivalent work. If no slow frames matched, report that count.

Contributed source follows GPL-3.0-or-later. Keep game files, saves, shader dumps, binaries and raw profiler captures outside Git. Small aggregate measurements or synthetic fixtures are useful for reproducing a result. The format tables have their own upstream notice. [License](LICENSE) · [Asset policy](docs/asset-policy.md)
