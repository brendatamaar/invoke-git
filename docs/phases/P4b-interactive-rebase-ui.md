# P4b - Interactive Rebase Editor UI
Status: in-progress
Depends on: P4a
Parallel-safe with: P3, P5, P6, P7, P8, P9, P10, P11

## Goal
Menyediakan layar Interactive Rebase Editor di GPUI dengan drag-to-reorder commit, pilihan aksi (pick, squash, fixup, edit, drop), kontrol Start/Continue/Skip/Abort, dan integrasi ke layar konflik serta banner state `Rebasing (step n/m)`.

## Tasks
- [ ] Tulis unit/property test model urutan & validasi UI plan (mis. tidak boleh squash/fixup commit pertama) di `crates/ui/src/rebase_editor.rs` dan `crates/app/src/rebase_view.rs`.
- [ ] Bangun komponen `RebaseEditor` (list baris commit, drag-to-reorder berbasis mouse + tombol naik/turun, dropdown/badge aksi dari `tokens`) di `crates/ui/src/rebase_editor.rs`.
- [ ] Hubungkan trigger "Interactive rebase onto…" dari commit graph (`crates/app/src/history_view.rs`), layar edit plan & kontrol saat paused di `crates/app/src/rebase_view.rs` dan `crates/app/src/repo_view.rs`.
- [ ] Tulis Windows checklist lengkap di Handoff notes (reorder drag, squash/fixup, pause on `edit`, conflict continue/skip/abort).

## Definition of Done
- `cargo xtask check` (Windows) atau `cargo xtask check --core` + Windows CI hijau.
- `cargo test -p ui` dan `cargo test -p app` lulus mencakup validasi reorder & aksi editor rebase.
- Tidak ada warna/spacing hard-coded di `crates/ui/src/rebase_editor.rs` (semua dari `crates/tokens`).

## Out of scope
- Logika state machine rebase & eksekusi git CLI (sudah selesai di `P4a`).

## Handoff notes
- Fase ini berat di GPUI; di headless VPS hanya boleh mengedit & mengetes logika pure/non-GPUI, verifikasi visual dilakukan di laptop Windows.
- Ditunda di headless Linux VPS: dependensi `P4a` sudah `done` di `main`, mulai `P4b` di Windows agar kompilasi & verifikasi GPUI (`ui`/`app`) bisa dijalankan end-to-end.
