# P7 - Reflog View
Status: todo
Depends on: P1
Parallel-safe with: P1b, P2, P3, P4a, P4b, P5, P6, P8, P9, P10, P11

## Goal
Menampilkan reflog lokal (`HEAD` dan branch reflog) agar user dapat meninjau riwayat pergerakan ref serta memulihkan commit (buat branch baru atau reset dengan konfirmasi & undo point).

## Tasks
- [ ] Tulis integration test pembacaan reflog (`HEAD` & branch, repo tanpa reflog/unborn, aksi recover create-branch) di `crates/shell/tests/reflog.rs` dan unit test state di `crates/core/tests/reflog_props.rs`.
- [ ] Implementasikan pembacaan entri reflog (old/new OID, pesan aksi, timestamp, selector `HEAD@{n}`) melalui `gix` di `crates/shell/src/reflog.rs`.
- [ ] Tambah event/command recover dari entri reflog (create branch at OID, atau reset dengan expected-ref guard & undo snapshot) di `crates/core/src/`.
- [ ] Bangun tampilan virtualized `ReflogView` di `crates/ui/src/reflog_view.rs` dan integrasikan ke `crates/app/src/repo_view.rs` + Windows checklist di Handoff notes.

## Definition of Done
- `cargo xtask check --core` (VPS/CI) atau `cargo xtask check` (Windows) lulus.
- `cargo test -p invoke-core --test reflog_props` dan `cargo test -p shell --test reflog` lulus dan cocok dengan `git reflog`.
- User dapat menelusuri entri reflog dan membuat branch pemulihan atau reset yang tetap bisa di-undo (Law 8 & 9).

## Out of scope
- Menulis atau mengedit file reflog secara manual; reflog hanya dibaca dari `.git/logs/`.

## Handoff notes
- Pastikan integrasi undo stack SQLite (`P1`) tetap sinkron ketika user melakukan pemulihan dari reflog.
