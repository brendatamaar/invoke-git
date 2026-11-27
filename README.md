# Invoke Git

[![CI](https://github.com/brendatamaar/invoke-git/actions/workflows/ci.yml/badge.svg)](https://github.com/brendatamaar/invoke-git/actions/workflows/ci.yml)

A fast, native desktop Git client. Built end to end in Rust with a GPU-rendered [GPUI](https://www.gpui.rs/) interface.

**Status:** Pre-release. All v0.1–v0.3 feature and daily-driver UX phases (`P0`–`P12a`) are complete; the pre-release and public release gate (`P12`) is next. See [docs/PLAN.md](docs/PLAN.md) and [docs/PRD.md](docs/PRD.md).

## Features

- **Daily workflow & staging:** Multi-repo tabs (open, init, clone with progress and cancel), separate Changes and History views, file/hunk/line staging and unstaging (including binary, mode, and symlink handling), undoable discard, commit and amend with subject-length guidance and per-worktree saved drafts.
- **Diff viewer & pre-push review:** Custom Myers + histogram diff engine with word/character inline highlighting, tree-sitter syntax highlighting, split and unified layouts, and collapsed unchanged regions. Dedicated pre-push review screen covering outgoing commits, combined and per-commit diffs, viewed checkmarks, and local SQLite-backed line notes with to-do warnings before push.
- **History & power-user Git:** Virtualized commit graph with lane layout and paging, commit search (message, author, hash, path), blame and `--follow` file history, interactive rebase editor (reorder, squash, fixup, edit, drop), cherry-pick, revert, tag management (create, push, delete), stash (save including untracked, apply, pop, drop), conflict resolution (merge, rebase, cherry-pick), reflog recovery (`--soft` / `--mixed` reset), and reflog-backed undo.
- **Sync & forges:** Fetch, pull (merge or rebase), push with `--force-with-lease`, upstream tracking, GitHub / GitLab / self-hosted GitLab accounts (tokens stored in the OS keychain via `keyring`, never plaintext), clone from forge repo lists, and Pull Request / Merge Request listing, creation, and checkout.
- **Advanced repositories:** Worktrees, submodules, and Git LFS pointer inspection and pull.
- **No bloat:** No account or login wall, cloud sync, telemetry, AI features, issue boards, or embedded IDE. Uses your existing Git configuration, credentials, and hooks.

## Architecture

- **Pure core, IO in `shell`:** Unidirectional data flow: UI sends events to `core::step(state, event) -> (state, commands)`, and `shell` executes commands on background threads with progress/cancel and feeds result events back. `core`, `diff`, `graph`, and `highlight` perform zero IO.
- **Hybrid Git engine:** Read-only operations use `gix` (fast, lock-free); writes, network operations, and hooks run through the system `git` CLI (≥ 2.40, or bundled MinGit fallback on Windows).
- **Disk as source of truth:** A debounced `notify` watcher triggers fresh reads on external changes. Every write carries expected `index` / ref guards and refuses if disk state changed or `index.lock` is held.
- **Local storage:** Pre-push notes, undo stack, and worktree UI layout/drafts are stored in local SQLite (`rusqlite`, bundled) in the app data directory, never inside `.git`.

### Workspace layout

| Crate | Purpose |
| --- | --- |
| `crates/app` | `invoke-git` desktop binary wiring `ui`, `invoke-core`, and `shell` |
| `crates/ui` | GPUI component layer reading all colours, sizes, and spacing from `crates/tokens` |
| `crates/tokens` | Design tokens generated from [docs/design/tokens.json](docs/design/tokens.json) |
| `crates/core` | Pure state machines (`invoke-core`), events, and command definitions (no IO) |
| `crates/shell` | IO layer (`gix`, `git` CLI, `invoke-rebase-editor`, file watcher, SQLite, forge APIs, keychain) |
| `crates/askpass` | `GIT_ASKPASS` / `SSH_ASKPASS` helper communicating with the app over a local pipe (`std` only) |
| `crates/diff` | Pure Myers + histogram diff engine, hunks, word-level diff, and patch builder (no IO) |
| `crates/graph` | Pure commit-graph lane layout and paging (no IO) |
| `crates/highlight` | Pure tree-sitter syntax highlighting layer (no IO) |
| `crates/harness` | Test-only registry for the 14 PRD correctness laws and model-based harness against real `git` |
| `crates/spike-diff`, `crates/spike-graph` | Phase 0 GPUI prototypes (non-shipping) |
| `xtask` | Workspace checks, fixture generation, performance budget checks, and Windows packaging |

## Building & running

**Prerequisites:**
- **Windows (full app):** Rust (`stable-x86_64-pc-windows-msvc`), Visual Studio Build Tools with the "Desktop development with C++" workload, and Git ≥ 2.40 (or package with bundled MinGit).
- **Linux / macOS (core crates & tests):** Rust stable toolchain and Git ≥ 2.40.

```powershell
# Run the desktop app (Windows)
cargo run --release -p app
cargo run --release -p app -- C:\path\to\repo   # open one or more repo folders on launch

# Run formatting, clippy (-D warnings), and tests
cargo xtask check                               # full workspace (auto-skips UI on headless Linux)
cargo xtask check --core                        # core crates only on any machine

# Run criterion benchmarks against docs/perf-budgets.toml
cargo xtask bench-check                         # pure core benchmarks (diff, graph)
cargo xtask bench-check --ui                    # include UI layout benchmarks (Windows)

# Build a standalone Windows dist\windows\ folder with bundled MinGit
cargo xtask package-windows
```

## Verification & CI

GitHub Actions runs three workflows matching [docs/spec/verification.md](docs/spec/verification.md):

| Workflow | Trigger | Runner | Runs |
| --- | --- | --- | --- |
| [ci.yml](.github/workflows/ci.yml) — Windows | Every push | `windows-latest` | `cargo xtask check` (all crates, GPUI included; `INVOKE_HARNESS_CASES=32`) |
| [ci.yml](.github/workflows/ci.yml) — Core | Every push | `ubuntu-latest`, `macos-latest` | `cargo xtask check --core` (fmt, clippy, unit/proptest, 256-sequence model-based harness) |
| [ci.yml](.github/workflows/ci.yml) — Benchmarks | Every push | `ubuntu-latest` | `cargo xtask bench-check` against [docs/perf-budgets.toml](docs/perf-budgets.toml) |
| [ci.yml](.github/workflows/ci.yml) — Benchmarks (UI) | Every push | `windows-latest` | `cargo xtask bench-check --ui` |
| [quint.yml](.github/workflows/quint.yml) | Push to `specs/quint/**` or `crates/shell/**` | `ubuntu-latest` | `quint typecheck` and `quint run` on the 3 concurrency specs in `specs/quint/` |
| [nightly.yml](.github/workflows/nightly.yml) | Daily schedule | `ubuntu-latest` | 10,000-sequence model-based harness (`cargo test -p harness --test model_based`) |

## License

Apache-2.0. See [LICENSE](LICENSE).
