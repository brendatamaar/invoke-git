# P10 - Worktrees, Submodules & Git LFS
Status: in-progress
Depends on: P1
Parallel-safe with: P4b, P5, P6, P7, P8, P9, P11

## Goal
Menyediakan dukungan operasional untuk git worktrees (list, add, switch/open in tab, remove), submodules (status, init, update, sync), dan Git LFS (deteksi pointer, pull/status dasar, tidak merusak diff/staging).

## Tasks
- [x] Tulis integration test fixture repo untuk worktrees, submodules, dan deteksi Git LFS di `crates/shell/tests/advanced_repos.rs` serta property test state di `crates/core/tests/advanced_props.rs`.
- [x] Tambah deteksi & pembacaan worktree list, submodule status, dan LFS status/pointer di `crates/shell/src/` dan `crates/core/src/state.rs`.
- [x] Tambah perintah CLI untuk `git worktree add/remove`, `git submodule update --init --recursive`, dan `git lfs pull` (bila `git-lfs` terpasang) dengan progress/cancel & lock guard di `crates/shell/src/git.rs`.
- [ ] Tambah section Worktrees & Submodules di sidebar (`crates/ui/src/sidebar.rs`, `crates/app/src/branch_panel.rs`) serta indikator file LFS di diff viewer + Windows checklist di Handoff notes.

## Definition of Done
- `cargo xtask check --core` (VPS/CI) atau `cargo xtask check` (Windows) lulus.
- `cargo test -p invoke-core --test advanced_props` dan `cargo test -p shell --test advanced_repos` lulus untuk worktree, submodule, dan deteksi LFS (termasuk saat `git-lfs` tidak terpasang).
- Membuka linked worktree mengikuti `.git` file pointer dengan benar tanpa merusak tab repo utama.

## Out of scope
- Partial clone dan sparse checkout UI (masuk scope *Later* di `docs/PRD.md`).

## Handoff notes
- Catatan Windows: path worktree dan submodule rawan panjang path dan file lock; uji juga kasus `.git` file yang menunjuk ke `../.git/worktrees/<name>`.
- Task 1 selesai: fixture integration tests (`crates/shell/tests/advanced_repos.rs`) dan property test awal (`crates/core/tests/advanced_props.rs`) ditambahkan untuk linked worktree (`.git` relatif `../.git/worktrees/<name>`), submodule, dan file pointer Git LFS tanpa `git-lfs`.
- Task 2 selesai: `AdvancedSnapshot` (worktrees, submodules, LFS) ditambahkan ke `DiskState` (`crates/core/src/advanced.rs`, `state.rs`, `watch.rs`) dan pembacaan read-only berbasis `gix`/file ditambahkan di `crates/shell/src/advanced.rs`, `read.rs`, `precondition.rs`, `staging.rs` (termasuk fallback status/diff saat `filter.lfs.required=true` tetapi `git-lfs` tidak terpasang).
- Task 3 selesai: planner & event core (`WorktreeAddRequested`, `WorktreeRemoveRequested`, `SubmoduleRequested`, `LfsPullRequested`) dan CLI runner (`git worktree add/remove`, `git submodule init/update --init --recursive/sync --recursive`, `git lfs pull`) ditambahkan dengan expected-state guard, `index.lock` guard, progress/cancel, serta fallback `ErrorClass::Environment` saat `git-lfs` tidak terpasang. Proptest `invalid_lfs_oid_is_rejected` terbukti menangkap planted bug.
