# PLAN

## Arsitektur ringkas

- **Workspace Rust (edition 2024, Windows-first):** UI GPUI (dipin ke Zed `v1.29.2`) + layer komponen sendiri (`crates/tokens`, `crates/ui`, `crates/app`).
- **Pure core (tanpa IO):** `crates/core` (`step(state, event) -> (state, commands)`), `crates/diff` (Myers + histogram + word diff + patch builder), `crates/graph` (lane layout + paging), `crates/highlight` (tree-sitter).
- **Shell & IO (`crates/shell`, `crates/askpass`):** semua baca repo lewat `gix`, semua tulis/jaringan/hook lewat system `git` CLI (≥ 2.40) dengan expected-value guard (`index` id, ref value, `index.lock`) dan file watcher (`notify` debounced).
- **Data flow satu arah:** UI mengirim `Event` → `core::step` menghasilkan `State` + `Command` → `shell` mengeksekusi di background thread & mengembalikan `Event` hasil; disk `.git` selalu jadi source of truth.
- **Penyimpanan lokal:** catatan review & undo stack disimpan di satu DB SQLite (`rusqlite`, bundled) di folder app data, tidak pernah di dalam `.git`.
- **Verifikasi:** 14 laws diuji lewat `proptest` & model-based harness (`crates/harness`), Quint (`specs/quint/`), dan `criterion` perf budgets (`docs/perf-budgets.toml`) via `cargo xtask`.
- **Arsip historis:** protokol Phase 0 & tabel backlog v0.1 (tasks 1–26) disimpan di `docs/PLAN-v0.1-archive.md` dan `tasks/`. Detail tiap fase ada di `docs/phases/`.

## Fase

| ID | Nama | Status | Depends on | Parallel-safe with |
|----|------|--------|------------|--------------------|
| P0 | GPUI Spike | done | - | - |
| P1 | v0.1 Daily Driver (Tasks 1–25) | done | P0 | - |
| P1b | Non-Text Staging & Discard | todo | P1 | P3, P5, P6, P7, P8 |
| P2 | Cherry-Pick & Revert | done | P1 | P3, P5, P6, P7, P8, P11 |
| P3 | Tags (Create, Push, Delete) | done | P1 | P1b, P2, P4a, P4b, P5, P6, P7, P8, P11 |
| P4a | Interactive Rebase Core & v0.2 Law Gate | done | P2 | P3, P5, P6, P7, P8, P11 |
| P4b | Interactive Rebase Editor UI | done | P4a | P3, P5, P6, P7, P8, P9, P10, P11 |
| P5 | Blame & File History | done | P1 | P1b, P2, P3, P4a, P4b, P7, P8, P9, P10, P11 |
| P6 | Commit Search | done | P1 | P1b, P2, P3, P4a, P4b, P7, P8, P9, P10, P11 |
| P7 | Reflog View | done | P1 | P1b, P2, P3, P4a, P4b, P5, P6, P8, P9, P10, P11 |
| P8 | Forge Auth & Repo List (GitHub/GitLab) | todo | P1 | P1b, P2, P3, P4a, P4b, P5, P6, P7, P10, P11 |
| P9 | Pull Requests & Merge Requests | todo | P8 | P4b, P5, P6, P7, P10, P11 |
| P10 | Worktrees, Submodules & Git LFS | todo | P1 | P4b, P5, P6, P7, P8, P9, P11 |
| P11 | Bundled MinGit Fallback (Windows) | todo | P1 | P2, P3, P4a, P4b, P5, P6, P7, P8, P9, P10 |
| P12 | Pre-Release Dogfood & Public Release Gate | todo | P1b, P3, P4b, P5, P6, P7, P9, P10, P11 | - |

## Review Windows

P2, P3, P5, P6, dan P7 memiliki implementasi core, shell, dan UI. `cargo xtask check` lulus sebelum perubahan UI dengan 256 urutan model-based, dan lulus pada gabungan akhir dengan `INVOKE_HARNESS_CASES=16`. `cargo build -p app` dan smoke test pembukaan repo fixture dengan profil terisolasi juga lulus. Pengguna melaporkan seluruh verifikasi manual Windows lulus. Hasil dicatat di Handoff notes tiap fase.
