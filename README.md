# Invoke Git

[![CI](https://github.com/brendatamaar/invoke-git/actions/workflows/ci.yml/badge.svg)](https://github.com/brendatamaar/invoke-git/actions/workflows/ci.yml)

A fast, native desktop git client with a review step before every push. Rust end to end, UI in GPUI. Windows first.

Status: Phase 0, the GPUI spike. See [docs/PLAN.md](docs/PLAN.md) and [docs/PRD.md](docs/PRD.md).

## GPUI pin

GPUI is a git dependency on [zed-industries/zed](https://github.com/zed-industries/zed), pinned in the root `Cargo.toml`:

| | |
| --- | --- |
| Revision | `2de9e435a5d98657f35c260b5f4dd0979df659e6` |
| Zed tag | `v1.29.2` (stable, committed 2026-11-18) |
| Crates used | `gpui`, `gpui_platform` |
| Patch carried over | `async-task` → `zed-industries/async-task` @ `015845a19fc31752c02367567d3b8900a406f9da` |

**Why this commit:** it is the newest Zed *stable* release when Phase 0 started (2026-11-20). A stable tag has shipped to Zed's Windows users, so its GPUI Windows backend (Win32, DirectX 11, DirectWrite) has had real-world use, which an arbitrary `main` commit has not. It builds on our stable toolchain; Zed itself pins Rust 1.98.1 at this tag.

**Why the patch:** Zed's workspace patches `async-task` to its own fork, and GPUI is built against that fork. Cargo does not apply a dependency's `[patch]` entries to dependents, so we repeat it at the same revision.

**Updating:** only when asked, in its own commit (see `AGENTS.md`). Bump `rev` on both crates together, re-copy any `[patch.crates-io]` entries GPUI needs from Zed's `Cargo.toml` at the new commit, and update this table.

## Building

Windows needs Rust (`stable-x86_64-pc-windows-msvc`) and Visual Studio Build Tools with the "Desktop development with C++" workload.

```powershell
cargo xtask fixtures                    # generate fixtures/diff/{old,new}.rs
cargo run --release -p spike-diff       # Phase 0 Prototype A
cargo run --release -p spike-graph      # Phase 0 Prototype B
cargo run --release -p spike-diff -- --commit-box   # Phase 0 text-input check
cargo xtask check                       # fmt, clippy, tests
```

On the headless VPS, build only the non-UI crates (see `docs/PLAN.md`). `cargo xtask check` skips the UI crates by itself when Linux has no display; `cargo xtask check --core` skips them on any machine.

## CI

GitHub Actions runs on every push (`.github/workflows/ci.yml`), following the matrix in `docs/spec/verification.md`:

| Job | Runner | Runs |
| --- | --- | --- |
| Windows (everything) | `windows-latest` | `cargo xtask check`: fmt, clippy and tests for every crate, GPUI included |
| Core | `ubuntu-latest`, `macos-latest` | `cargo xtask check --core`: the same, without the UI crates |

Benchmarks, Quint model checking and the nightly long runs are added by their own backlog tasks (24 and 25).

## License

Apache-2.0. See [LICENSE](LICENSE).
