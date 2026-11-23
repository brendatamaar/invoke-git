# P4a - Interactive Rebase Core & v0.2 Law Gate
Status: in-progress
Depends on: P2
Parallel-safe with: P3, P5, P6, P7, P8, P11

## Goal
Mengimplementasikan state machine & eksekusi shell untuk interactive rebase (reorder, pick, squash, fixup, edit, drop, pause on conflict/edit, continue, skip, abort) beserta model-based test gate v0.2 untuk semua laws pada rebase dan cherry-pick.

## Tasks
- [ ] Tulis property test state machine interactive rebase di `crates/core/tests/rebase_props.rs`, integration test real git di `crates/shell/tests/rebase.rs`, dan perluas `crates/harness/src/` + `crates/harness/tests/model_based.rs` untuk mencakup operasi interactive rebase & cherry-pick terhadap Law 8–13.
- [ ] Tambah model rencana rebase (`RebasePlan`, `RebaseAction::{Pick, Squash, Fixup, Edit, Drop}`) dan state machine (Editing → Running → Paused on conflict/edit → Done / Abort) di `crates/core/src/rebase.rs`, `crates/core/src/event.rs`, dan `crates/core/src/step.rs`.
- [ ] Implementasikan `GIT_SEQUENCE_EDITOR` non-interaktif (helper atau env script yang aman di Windows) & eksekusi `git rebase -i` / `--continue` / `--skip` / `--abort` dengan expected-ref guard dan undo snapshot di `crates/shell/src/rebase.rs` dan `crates/shell/src/git.rs`.
- [ ] Validasi semua laws di model-based harness (v0.2 gate) termasuk skenario abort dari state pause/edit yang harus memulihkan branch asal secara utuh.

## Definition of Done
- `cargo xtask check --core` lulus tanpa warning.
- `cargo test -p invoke-core --test rebase_props`, `cargo test -p shell --test rebase`, dan `cargo test -p harness --test model_based` lulus.
- Semua 14 laws lulus pada model-based test dengan transisi interactive rebase dan cherry-pick aktif (memenuhi v0.2 gate di `docs/PRD.md`).

## Out of scope
- Antarmuka drag-to-reorder GPUI untuk interactive rebase (dikerjakan di `P4b`).
- Reword/exec step lanjutan di luar `pick`, `squash`, `fixup`, `edit`, `drop` (opsional nanti).

## Handoff notes
- Pastikan mekanisme `GIT_SEQUENCE_EDITOR` bekerja lintas Windows (path dengan spasi & backslash) dan Linux VPS tanpa bergantung pada POSIX shell; pola helper executable seperti `crates/askpass` dapat dipertimbangkan jika perlu (catat di `docs/DECISIONS.md` bila menambah binary).
- Basis P4a adalah `ca02a21`, yang memuat merge core/shell/harness P2 `302682a`. UI P2 yang berstatus review ditunda ke Windows dan bukan dependensi core P4a.
