# P3 - Tags (Create, Push, Delete)
Status: review
Depends on: P1
Parallel-safe with: P1b, P2, P4a, P4b, P5, P6, P7, P8, P11

## Goal
Memungkinkan user melihat tag di sidebar & commit graph, membuat tag (lightweight atau annotated), push tag ke remote, dan menghapus tag lokal/remote dengan undo untuk operasi lokal.

## Tasks
- [x] Tulis property test validasi nama tag & state transition di `crates/core/tests/tag_props.rs` dan integration test CLI/gix di `crates/shell/tests/tags.rs`.
- [x] Tambah tipe `TagName`, daftar tag di `RepoState`, serta event/command create, delete, dan push tag di `crates/core/src/ids.rs`, `crates/core/src/state.rs`, `crates/core/src/tag.rs`, dan `crates/core/src/step.rs`.
- [x] Baca daftar tag melalui `gix` di `crates/shell/src/read.rs` dan jalankan `git tag` / `git push <remote> refs/tags/<name>` / delete di `crates/shell/src/git.rs`.
- [ ] (DITUNDA ke Windows, lihat Handoff notes) Tampilkan section Tags di sidebar (`crates/ui/src/sidebar.rs`, `crates/app/src/branch_panel.rs`) dan badge tag di commit graph (`crates/ui/src/ref_labels.rs`) beserta form create/push/delete + Windows checklist di Handoff notes.

## Definition of Done
- `cargo xtask check --core` (VPS/CI) atau `cargo xtask check` (Windows) lulus.
- `cargo test -p invoke-core --test tag_props` dan `cargo test -p shell --test tags` lulus (create lightweight/annotated, delete lokal/remote, push tag, undo create/delete lokal).
- Tag baru muncul di `RepoState` setelah watcher refresh dan badge tag tampil pada commit target.

## Out of scope
- GPG/SSH signed tag UI (masuk scope *Later* di `docs/PRD.md`).

## Handoff notes
- Pastikan `TagName::parse` mengikuti aturan `git check-ref-format` mirip `BranchName::parse` di `crates/core/src/ids.rs`. Push/delete remote tag bersifat irreversible seperti push branch.

### Selesai di VPS Linux (core + shell)
- Core: `TagName` (`ids.rs`, aturan sama dengan `BranchName`), `tag.rs` (plan + undo point), `WriteRequest/GitArgs::{CreateTag, DeleteTag, PushTag, DeleteRemoteTag}`, event `Tag*Requested`/`RemoteTagDeleteRequested`, `TagRefusal`, `RefsSnapshot::tags()` (diturunkan dari `refs`, jadi tag baru muncul di `RepoState` setelah refresh) + `peeled_tags` (commit target tag annotated, untuk badge).
- Shell: `read_peeled_tags` (gix), argv `git tag --no-sign [--annotate --file=-]`, `tag --delete`, `push -- <remote> refs/tags/x:refs/tags/x`, `push -- <remote> :refs/tags/x`.
- Law 8: create/delete lokal punya undo point `Reversal::Refs` (hanya ref tag, index tidak disentuh). Law 9: push tag / hapus tag remote memakai `Irreversible::Push` (kode tersimpan tetap `push`, tanpa perubahan codec).
- Bug ditemukan & diperbaiki: `precondition::check_refs` mem-peel ref sampai commit sedangkan `read_refs` menyimpan target langsung ref; tag annotated selalu gagal guard. Kini keduanya konsisten.
- Push tag tidak memakai `+` (remote menolak memindah tag yang sudah ada; hasil `Expected`). Operasi tag diblokir saat fase bukan `Clean` (merge/rebase/dll).
- `crates/app/src/notice.rs` ditambah teks notice tag; `RefsSnapshot` literal di `ui/summary.rs` dan `app/review.rs` diberi `..RefsSnapshot::default()` (tidak dikompilasi di Linux).

### Ditunda (butuh Windows/GPUI) - sisa task 4
- Section Tags di `crates/ui/src/sidebar.rs` + `crates/app/src/branch_panel.rs` (daftar via `disk.refs.tags()`, aksi: create (lightweight/annotated, pesan), push (pilih remote), delete lokal, delete remote dengan konfirmasi irreversible).
- Badge tag di `crates/ui/src/ref_labels.rs` memakai `TagInfo::target` (bukan `tip`) agar tag annotated muncul di commit target. Warna/spasi dari `crates/tokens`.
- Event siap dipakai UI: `Event::TagCreateRequested{name,at,message}`, `TagDeleteRequested`, `TagPushRequested{remote,name}`, `RemoteTagDeleteRequested{remote,name}`.

### Windows verification checklist
1. `cargo xtask check` (fmt, clippy, test, build ui/app) lulus tanpa warning; pastikan `RefsSnapshot` literal ui/app masih compile.
2. Buat tag lightweight & annotated dari UI; tag muncul di sidebar dan badge di commit target (annotated: commit, bukan tag object).
3. Undo (Ctrl+Z) setelah create/delete tag lokal memulihkan tag; setelah push/delete remote muncul "cannot be undone".
4. Push tag ke remote nyata (kredensial/askpass), push tag yang sudah ada di remote beda nilai -> pesan ditolak, bukan error.
5. Nama tag dengan karakter terlarang (spasi, `..`, `~`) ditolak; nama dengan `/` (`rel/1`) dan CRLF pada pesan annotated OK.
6. Tag dibuat dari terminal terlihat setelah watcher refresh (refs berubah di `.git/refs/tags` dan `packed-refs`).

### Di luar scope
- Signed tag (GPG/SSH): `--no-sign` dipakai agar `tag.gpgSign` tidak memblokir. Membuat tag dari commit tertentu di graph (`BranchStart::Commit`) sudah didukung core, UI belum.
