# P2 - Cherry-Pick & Revert
Status: review
Depends on: P1
Parallel-safe with: P3, P5, P6, P7, P8, P11

## Goal
Menyediakan aksi cherry-pick dan revert commit dengan alur konflik terpadu (resolve, continue, abort) dan undo yang aman sesuai Law 8, 9, 11–13.

## Tasks
- [x] Tulis property & integration tests untuk cherry-pick dan revert (clean, conflict → resolve → continue, conflict → abort, undo) di `crates/core/tests/conflict_props.rs`, `crates/shell/tests/conflicts.rs`, dan tambahkan transisi cherry-pick/revert ke model-based harness di `crates/harness/src/`.
- [x] Tambah event, command, dan state transition untuk `CherryPick` dan `Revert` (termasuk menyimpulkan konflik via `ConcludeCherryPick` / `ConcludeRevert` dan undo point) di `crates/core/src/event.rs`, `crates/core/src/ops.rs`, `crates/core/src/conflict.rs`, dan `crates/core/src/step.rs`.
- [x] Tambah eksekusi CLI `git cherry-pick` dan `git revert` (`--continue`, `--abort`) dengan expected-ref/lock guard di `crates/shell/src/git.rs` dan `crates/shell/src/conflict.rs`.
- [ ] (ditunda ke Windows, lihat Handoff notes) Tambah aksi Cherry-pick dan Revert di context menu/detail commit (`crates/app/src/history_view.rs`, `crates/app/src/conflict_view.rs`, `crates/ui/src/commit_detail.rs`) + Windows checklist di Handoff notes.

## Definition of Done
- `cargo xtask check --core` (VPS/CI) atau `cargo xtask check` (Windows) lulus.
- `cargo test -p invoke-core --test conflict_props`, `cargo test -p shell --test conflicts`, dan `cargo test -p harness --test model_based` lulus dengan operasi cherry-pick dan revert aktif.
- Skenario konflik cherry-pick & revert dapat dilanjutkan setelah semua file resolved dan dibatalkan dengan memulihkan repo secara utuh.

## Out of scope
- Interactive rebase editor dan model-based law gate penuh untuk rebase (dikerjakan di `P4a` dan `P4b`).
- Multi-commit cherry-pick range sekaligus (dapat ditambah setelah alur single-commit stabil).

## Handoff notes
- Catatan dari `tasks/014.DONE.md`: `Phase::CherryPicking` dan `Phase::Reverting` sudah dikenali oleh deteksi `.git` (`CHERRY_PICK_HEAD`, `REVERT_HEAD`); tinggal buka izin `ConcludeCherryPick` / `ConcludeRevert` dan hubungkan ke alur konflik `P1`.

### Hasil sesi headless Linux (2026-11-23, branch `t3/p2-cherry-pick-revert`)
- **Core:** `Event::CherryPickRequested(Oid)` / `RevertRequested(Oid)` -> `WriteRequest::{CherryPick,Revert}` -> `GitArgs::{CherryPick,Revert}`. Dispatch menolak: repo tidak `Clean` (`RepoBusy`), HEAD unborn (`NoCommitsYet`), HEAD berbeda dari saat diminta (`BranchChanged`, law 9), file tracked berubah (`Refusal::UncommittedChanges`, baru), lock dipegang proses lain (antre, law 12). Hasil bersih = satu commit di HEAD, undo point `Reversal::Refs` (law 8). Konflik = `OpFinished(Failed(Expected))`: undo point dibuang, state `CherryPicking`/`Reverting` ditangani alur konflik P1 (resolve / continue / abort tidak berubah). Pick/revert yang berjalan tidak bisa di-cancel (seperti alur konflik). `Action::{CherryPick,Revert}` ditambahkan.
- **Shell:** argv `cherry-pick -- <oid>` dan `revert --no-edit -- <oid>`, `GIT_EDITOR=true`; marker klasifikasi baru: merge commit tanpa `-m`, `bad revision`, "empty, possibly due to conflict resolution" (semua `Expected`).
- **Harness:** transisi `Replay { commit, revert, end: Abort | Resolve }` (juga di bawah `WhileLocked`), dibandingkan dengan twin plain git; abort memverifikasi repo kembali persis (law 13), resolve+continue memverifikasi satu commit dan undo berhenti di sana. Test terarah: `self_check::conflicting_cherry_picks_and_reverts_abort_or_continue_like_git`.
- **Temuan harness:** git memulai cherry-pick/revert walau file tracked punya edit unstaged, lalu `--abort` menimpa edit itu. Karena itu core menolak saat ada perubahan pada file tracked (staged atau unstaged); file untracked tidak masalah. Dicatat di DECISIONS.md.
- **UI ditunda (butuh Windows):** hanya `crates/app/src/notice.rs` yang disentuh (arm string untuk `Action`/`Refusal` baru, tanpa GPUI). Belum ada menu konteks Cherry-pick / Revert di `history_view.rs`, `commit_detail.rs`, `conflict_view.rs`; judul/label konflik cherry-pick & revert sudah ada dari P1 (`crates/app/src/conflict.rs`).

### Pertanyaan spec / di luar scope (diusulkan, tidak ditebak)
1. Pick/revert yang menghasilkan commit kosong (perubahan sudah ada) berhenti dengan `CHERRY_PICK_HEAD` tanpa file konflik; Continue gagal ("nothing to commit"), hanya Abort yang jalan. `Skip` hanya untuk rebase (PRD). Usul: izinkan Skip (`git cherry-pick --skip`, git >= 2.23) untuk cherry-pick/revert, atau tampilkan pesan khusus. Menunggu keputusan.
2. Merge commit butuh `-m <parent>`: sekarang ditolak git (`Expected`, repo tidak berubah). Usul: pemilih parent di UI.
3. Guard `UncommittedChanges` hanya dari snapshot; edit tersimpan setelah baca dan sebelum git jalan tidak terlindungi oleh `Expect` (git sendiri menolak bila tumpang tindih).
4. Multi-commit range: di luar scope (sesuai fase).

### Windows verification checklist (belum dijalankan, UI tidak dibangun di Linux)
- [ ] `cargo xtask check` penuh lulus di Windows (termasuk `ui`/`app`; `notice.rs` kini punya arm `Action::CherryPick/Revert` dan `Refusal::UncommittedChanges`).
- [ ] Tambah item "Cherry-pick" dan "Revert" di menu konteks riwayat (`history_view.rs`) dan tombol di detail commit (`commit_detail.rs`), kirim `Event::CherryPickRequested(oid)` / `RevertRequested(oid)` mengikuti pola `deliver` yang ada; nonaktifkan saat fase bukan `Clean`.
- [ ] Cherry-pick bersih: commit baru muncul di graph, branch lain tidak bergerak, Undo mengembalikan ref dan index.
- [ ] Revert bersih: commit `Revert "..."` muncul; Undo mengembalikan.
- [ ] Konflik: layar konflik membuka dengan judul "Cherry-picking onto ..." / "Reverting on ..."; Continue terkunci sampai semua file resolved; Abort memulihkan semuanya.
- [ ] Pesan penolakan saat ada perubahan tracked ("Commit or stash your changes...") dan saat repo sedang paused.
- [ ] Windows: CRLF pada file konflik dan path panjang tetap benar setelah continue.
