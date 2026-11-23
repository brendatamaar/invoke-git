# P5 - Blame & File History
Status: in-progress
Depends on: P1
Parallel-safe with: P1b, P2, P3, P4a, P4b, P7, P8, P9, P10, P11

## Goal
Memungkinkan user membuka tampilan blame per baris (penulis, waktu, commit hash, klik untuk lompat ke detail commit) dan riwayat commit khusus satu file (file history) dengan performa responsif.

## Tasks
- [ ] Tulis integration test blame & file history (termasuk rename/follow, file baru, baris CRLF, dan file besar) di `crates/shell/tests/blame_history.rs` serta unit test layout/model di `crates/ui/src/blame_view.rs`.
- [ ] Implementasikan pembacaan blame & file history via `gix` (atau fallback read-only yang terdokumentasi di `docs/DECISIONS.md` bila fitur `gix` belum lengkap) di `crates/shell/src/blame.rs`, dijalankan off UI thread dan cancellable.
- [ ] Bangun komponen virtualized `BlameView` (gutter commit/author/date + kode bersyntax highlight dari `crates/highlight`) di `crates/ui/src/blame_view.rs`.
- [ ] Integrasikan aksi "Blame" dan "File history" dari diff viewer & commit detail di `crates/app/src/history_view.rs` dan `crates/app/src/repo_view.rs` + Windows checklist di Handoff notes.

## Definition of Done
- `cargo xtask check --core` (VPS/CI) atau `cargo xtask check` (Windows) lulus.
- `cargo test -p shell --test blame_history` lulus dan membuktikan hasil blame/history cocok dengan oracle `git blame` / `git log -- <path>`.
- Pembacaan blame dapat dibatalkan saat user pindah file tanpa memblokir UI thread.

## Out of scope
- Pencarian commit lintas seluruh repo (dikerjakan di `P6`).

## Handoff notes
- Gunakan kembali pipeline tree-sitter dari `crates/highlight` untuk baris kode pada tampilan blame agar hanya mewarnai baris terlihat + buffer.
