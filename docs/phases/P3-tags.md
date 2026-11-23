# P3 - Tags (Create, Push, Delete)
Status: todo
Depends on: P1
Parallel-safe with: P1b, P2, P4a, P4b, P5, P6, P7, P8, P11

## Goal
Memungkinkan user melihat tag di sidebar & commit graph, membuat tag (lightweight atau annotated), push tag ke remote, dan menghapus tag lokal/remote dengan undo untuk operasi lokal.

## Tasks
- [ ] Tulis property test validasi nama tag & state transition di `crates/core/tests/tag_props.rs` dan integration test CLI/gix di `crates/shell/tests/tags.rs`.
- [ ] Tambah tipe `TagName`, daftar tag di `RepoState`, serta event/command create, delete, dan push tag di `crates/core/src/ids.rs`, `crates/core/src/state.rs`, `crates/core/src/tag.rs`, dan `crates/core/src/step.rs`.
- [ ] Baca daftar tag melalui `gix` di `crates/shell/src/read.rs` dan jalankan `git tag` / `git push <remote> refs/tags/<name>` / delete di `crates/shell/src/git.rs`.
- [ ] Tampilkan section Tags di sidebar (`crates/ui/src/sidebar.rs`, `crates/app/src/branch_panel.rs`) dan badge tag di commit graph (`crates/ui/src/ref_labels.rs`) beserta form create/push/delete + Windows checklist di Handoff notes.

## Definition of Done
- `cargo xtask check --core` (VPS/CI) atau `cargo xtask check` (Windows) lulus.
- `cargo test -p invoke-core --test tag_props` dan `cargo test -p shell --test tags` lulus (create lightweight/annotated, delete lokal/remote, push tag, undo create/delete lokal).
- Tag baru muncul di `RepoState` setelah watcher refresh dan badge tag tampil pada commit target.

## Out of scope
- GPG/SSH signed tag UI (masuk scope *Later* di `docs/PRD.md`).

## Handoff notes
- Pastikan `TagName::parse` mengikuti aturan `git check-ref-format` mirip `BranchName::parse` di `crates/core/src/ids.rs`. Push/delete remote tag bersifat irreversible seperti push branch.
