# P2 - Cherry-Pick & Revert
Status: in-progress
Depends on: P1
Parallel-safe with: P3, P5, P6, P7, P8, P11

## Goal
Menyediakan aksi cherry-pick dan revert commit dengan alur konflik terpadu (resolve, continue, abort) dan undo yang aman sesuai Law 8, 9, 11–13.

## Tasks
- [ ] Tulis property & integration tests untuk cherry-pick dan revert (clean, conflict → resolve → continue, conflict → abort, undo) di `crates/core/tests/conflict_props.rs`, `crates/shell/tests/conflicts.rs`, dan tambahkan transisi cherry-pick/revert ke model-based harness di `crates/harness/src/`.
- [ ] Tambah event, command, dan state transition untuk `CherryPick` dan `Revert` (termasuk menyimpulkan konflik via `ConcludeCherryPick` / `ConcludeRevert` dan undo point) di `crates/core/src/event.rs`, `crates/core/src/ops.rs`, `crates/core/src/conflict.rs`, dan `crates/core/src/step.rs`.
- [ ] Tambah eksekusi CLI `git cherry-pick` dan `git revert` (`--continue`, `--abort`) dengan expected-ref/lock guard di `crates/shell/src/git.rs` dan `crates/shell/src/conflict.rs`.
- [ ] Tambah aksi Cherry-pick dan Revert di context menu/detail commit (`crates/app/src/history_view.rs`, `crates/app/src/conflict_view.rs`, `crates/ui/src/commit_detail.rs`) + Windows checklist di Handoff notes.

## Definition of Done
- `cargo xtask check --core` (VPS/CI) atau `cargo xtask check` (Windows) lulus.
- `cargo test -p invoke-core --test conflict_props`, `cargo test -p shell --test conflicts`, dan `cargo test -p harness --test model_based` lulus dengan operasi cherry-pick dan revert aktif.
- Skenario konflik cherry-pick & revert dapat dilanjutkan setelah semua file resolved dan dibatalkan dengan memulihkan repo secara utuh.

## Out of scope
- Interactive rebase editor dan model-based law gate penuh untuk rebase (dikerjakan di `P4a` dan `P4b`).
- Multi-commit cherry-pick range sekaligus (dapat ditambah setelah alur single-commit stabil).

## Handoff notes
- Catatan dari `tasks/014.DONE.md`: `Phase::CherryPicking` dan `Phase::Reverting` sudah dikenali oleh deteksi `.git` (`CHERRY_PICK_HEAD`, `REVERT_HEAD`); tinggal buka izin `ConcludeCherryPick` / `ConcludeRevert` dan hubungkan ke alur konflik `P1`.
