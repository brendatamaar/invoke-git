# P4a - Interactive Rebase Core & v0.2 Law Gate
Status: in-progress
Depends on: P2
Parallel-safe with: P3, P5, P6, P7, P8, P11

## Goal
Mengimplementasikan state machine & eksekusi shell untuk interactive rebase (reorder, pick, squash, fixup, edit, drop, pause on conflict/edit, continue, skip, abort) beserta model-based test gate v0.2 untuk semua laws pada rebase dan cherry-pick.

## Tasks
- [ ] Tulis property test state machine interactive rebase di `crates/core/tests/rebase_props.rs`, integration test real git di `crates/shell/tests/rebase.rs`, dan perluas `crates/harness/src/` + `crates/harness/tests/model_based.rs` untuk mencakup operasi interactive rebase & cherry-pick terhadap Law 8–13.
- [x] Tambah model rencana rebase (`RebasePlan`, `RebaseAction::{Pick, Squash, Fixup, Edit, Drop}`) dan state machine (Editing → Running → Paused on conflict/edit → Done / Abort) di `crates/core/src/rebase.rs`, `crates/core/src/event.rs`, dan `crates/core/src/step.rs`.
- [x] Implementasikan `GIT_SEQUENCE_EDITOR` non-interaktif (helper atau env script yang aman di Windows) & eksekusi `git rebase -i` / `--continue` / `--skip` / `--abort` dengan expected-ref guard dan undo snapshot di `crates/shell/src/rebase.rs` dan `crates/shell/src/git.rs`.
- [ ] Validasi semua laws di model-based harness (v0.2 gate) termasuk skenario abort dari state pause/edit yang harus memulihkan branch asal secara utuh.

## Definition of Done
- `cargo xtask check --core` lulus tanpa warning.
- `cargo test -p invoke-core --test rebase_props`, `cargo test -p shell --test rebase`, dan `cargo test -p harness --test model_based` lulus.
- Semua 14 laws lulus pada model-based test dengan transisi interactive rebase dan cherry-pick aktif (memenuhi v0.2 gate di `docs/PRD.md`).

## Out of scope
- Antarmuka drag-to-reorder GPUI untuk interactive rebase (dikerjakan di `P4b`).
- Reword/exec step lanjutan di luar `pick`, `squash`, `fixup`, `edit`, `drop` (opsional nanti).

## Handoff notes
- Basis P4a adalah `ca02a21`, yang memuat merge core/shell/harness P2 `302682a`. UI P2 yang berstatus review ditunda ke Windows dan bukan dependensi core P4a.
- Core memakai draft yang terikat ke HEAD yang dilihat saat mengedit, antrean write, dan pause yang dibaca dari marker Git. Satu snapshot undo diambil sebelum start dan dipertahankan selama pause. Abort saat draft atau antrean membuang rencana. Abort saat Git aktif menunggu pause Git berikutnya. Jika Git sudah selesai saat hasil dibaca, operasi masuk Done dan undo tersedia. Amend saat edit dan stage terkait dikelompokkan ke undo rebase.
- Shell memvalidasi cakupan rencana linear melalui gix, menjaga HEAD/index, dan mengecek step serta ref awal sebelum melanjutkan rebase yang dijeda. Konfigurasi autosquash, autostash, dan update-refs dinonaktifkan untuk menjalankan rencana yang diminta dan melindungi branch lain.
- Helper native `invoke-rebase-editor` menerima path todo dari argv Git dan membaca rencana dari file sementara. Hanya nama executable yang masuk `GIT_SEQUENCE_EDITOR`. Direktori executable dipasang di awal `PATH` child Git melalui `join_paths`. Editor pesan memakai sentinel internal Git `:`.
