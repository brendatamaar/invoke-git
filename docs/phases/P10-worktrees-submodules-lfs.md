# P10 - Worktrees, Submodules & Git LFS
Status: todo
Depends on: P1
Parallel-safe with: P4b, P5, P6, P7, P8, P9, P11

## Goal
Menyediakan dukungan operasional untuk git worktrees (list, add, switch/open in tab, remove), submodules (status, init, update, sync), dan Git LFS (deteksi pointer, pull/status dasar, tidak merusak diff/staging).

## Tasks
- [ ] Tulis integration test fixture repo untuk worktrees, submodules, dan deteksi Git LFS di `crates/shell/tests/advanced_repos.rs` serta property test state di `crates/core/tests/advanced_props.rs`.
- [ ] Tambah deteksi & pembacaan worktree list, submodule status, dan LFS status/pointer di `crates/shell/src/` dan `crates/core/src/state.rs`.
- [ ] Tambah perintah CLI untuk `git worktree add/remove`, `git submodule update --init --recursive`, dan `git lfs pull` (bila `git-lfs` terpasang) dengan progress/cancel & lock guard di `crates/shell/src/git.rs`.
- [ ] Tambah section Worktrees & Submodules di sidebar (`crates/ui/src/sidebar.rs`, `crates/app/src/branch_panel.rs`) serta indikator file LFS di diff viewer + Windows checklist di Handoff notes.

## Definition of Done
- `cargo xtask check --core` (VPS/CI) atau `cargo xtask check` (Windows) lulus.
- `cargo test -p invoke-core --test advanced_props` dan `cargo test -p shell --test advanced_repos` lulus untuk worktree, submodule, dan deteksi LFS (termasuk saat `git-lfs` tidak terpasang).
- Membuka linked worktree mengikuti `.git` file pointer dengan benar tanpa merusak tab repo utama.

## Out of scope
- Partial clone dan sparse checkout UI (masuk scope *Later* di `docs/PRD.md`).

## Handoff notes
- Catatan Windows: path worktree dan submodule rawan panjang path dan file lock; uji juga kasus `.git` file yang menunjuk ke `../.git/worktrees/<name>`.
