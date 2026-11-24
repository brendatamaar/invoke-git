# P1b - Non-Text Staging & Discard
Status: in-progress
Depends on: P1
Parallel-safe with: P3, P5, P6, P7, P8

## Goal
Memungkinkan user melakukan stage, unstage, dan discard (yang tetap undoable & terlindungi expected-index guard) untuk file binary, non-UTF-8, symlink, mode-only (chmod), dan entri intent-to-add yang sebelumnya ditolak oleh text patch builder.

## Tasks
- [x] Tulis unit/property test di `crates/core/tests/stage_props.rs` dan `crates/core/tests/discard_props.rs` serta integration test di `crates/shell/tests/staging.rs` dan `crates/shell/tests/discard.rs` untuk file binary, non-UTF-8, mode-only, dan untracked/tracked discard + undo (termasuk kasus `core.autocrlf=true` untuk discard).
- [ ] Tambah varian perintah git non-patch (`Add`, `RestoreStaged`, `RestoreWorktree`, `CleanPath` atau setara) di `crates/core/src/ops.rs`, `crates/core/src/step.rs`, dan `crates/shell/src/git.rs` dengan expected-index guard & undo snapshot.
- [ ] Implementasikan eksekusi stage/unstage/discard non-text di `crates/shell/src/staging.rs` dan `crates/shell/src/discard.rs`.
- [ ] Hubungkan tombol checkbox/discard di `crates/app/src/staging.rs`, `crates/app/src/discard.rs`, dan `crates/app/src/repo_view.rs` agar file non-text memakai jalur non-patch (tidak lagi menampilkan notice penolakan), serta siapkan Windows checklist di Handoff notes.

## Definition of Done
- `cargo xtask check --core` (VPS/CI) atau `cargo xtask check` (Windows) lulus tanpa warning.
- `cargo test -p invoke-core --test stage_props --test discard_props` dan `cargo test -p shell --test staging --test discard` lulus mencakup file binary, non-UTF-8, dan undo.
- File binary/mode-only dapat di-stage, di-unstage, di-discard, dan di-undo kembali ke state persis sebelumnya (Law 4, 5, 6, 8 terjaga).

## Out of scope
- Dukungan khusus Git LFS pointer/filter dan submodule management (dikerjakan di `P10`).
- Combined single undo point untuk multi-file `Stage all` / `Discard all` (opsional nanti).

## Handoff notes
- Tes lebih dulu mendefinisikan kontrak perintah per file, pemulihan byte index (termasuk intent-to-add), dan pemulihan worktree. Tes baru bergantung pada tipe dan runner dari task berikutnya.
- Spec gap: apa arti discard untuk intent-to-add? Usulan: hapus file worktree seperti file untracked, pertahankan flag intent-to-add di index, dan pulihkan byte asli lewat undo.
- Konteks awal lihat `tasks/011.DONE.md` (bagian What is unfinished) dan `tasks/013.DONE.md` (bagian What is unfinished): sebelumnya `load_inputs` mengembalikan `NotText` karena tidak ada varian `GitArgs` untuk `git add` / `git restore --staged` / `git checkout` / `git clean`.
