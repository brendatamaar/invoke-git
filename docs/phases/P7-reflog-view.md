# P7 - Reflog View
Status: done
Depends on: P1
Parallel-safe with: P1b, P2, P3, P4a, P4b, P5, P6, P8, P9, P10, P11

## Goal
Menampilkan reflog lokal (`HEAD` dan branch reflog) agar user dapat meninjau riwayat pergerakan ref serta memulihkan commit (buat branch baru atau reset dengan konfirmasi & undo point).

## Tasks
- [x] Tulis integration test pembacaan reflog (`HEAD` & branch, repo tanpa reflog/unborn, aksi recover create-branch) di `crates/shell/tests/reflog.rs` dan unit test state di `crates/core/tests/reflog_props.rs`.
- [x] Implementasikan pembacaan entri reflog (old/new OID, pesan aksi, timestamp, selector `HEAD@{n}`) melalui `gix` di `crates/shell/src/reflog.rs`.
- [x] Tambah event/command recover dari entri reflog (create branch at OID, atau reset dengan expected-ref guard & undo snapshot) di `crates/core/src/`.
- [x] Bangun tampilan virtualized `ReflogView` dan integrasikan halaman Reflog dengan pemilihan ref serta aksi pemulihan.

## Definition of Done
- `cargo xtask check --core` (VPS/CI) atau `cargo xtask check` (Windows) lulus.
- `cargo test -p invoke-core --test reflog_props` dan `cargo test -p shell --test reflog` lulus dan cocok dengan `git reflog`.
- User dapat menelusuri entri reflog dan membuat branch pemulihan atau reset yang tetap bisa di-undo (Law 8 & 9).

## Out of scope
- Menulis atau mengedit file reflog secara manual; reflog hanya dibaca dari `.git/logs/`.
- `git reset --hard` dari tampilan reflog (menimpa file worktree sehingga `Reversal::Refs` tidak dapat memulihkan state persis per Law 8; untuk checkout file commit lama, gunakan aksi create branch + switch yang undoable).

## Handoff notes
- **Integrasi undo stack SQLite (`P1`)**: `Event::ResetRequested` (`ResetMode::Soft` dan `ResetMode::Mixed`) menggunakan `Reversal::Refs` dengan `IndexEffect::Untouched` / `IndexEffect::Rewrites`, sedangkan pemulihan branch memakai `Event::BranchCreateRequested` (`BranchStart::Commit(oid)`). Format `undo_codec` (`u1`), skema SQLite `P1`, dan `shell::undo::reconcile` tetap kompatibel penuh tanpa migrasi — diverifikasi end-to-end (save → load → reconcile → `Event::UndoStackRestored` → undo) di `crates/shell/tests/reflog.rs::sqlite_undo_stack_survives_restart_after_reflog_recovery`.
- **Lanjutan Windows:** halaman Reflog menampilkan HEAD dan branch lokal, memperbarui isi saat snapshot berubah, dan membatalkan pengiriman hasil baca saat pindah target, halaman, atau tab. Detail entri menampilkan kedua OID. Form pemulihan dapat membuat branch dan opsional melakukan switch. Konfirmasi reset menyebut branch aktif atau HEAD detached serta target OID. Jika HEAD atau index berubah saat konfirmasi terbuka, pengguna harus membuka konfirmasi baru.
- `reflog_props` (9), `reflog` (8), dan `cargo xtask check` pada gabungan akhir lulus di Windows. Pengguna melaporkan seluruh verifikasi manual Windows lulus.
- Reflog dimuat kembali saat halaman dibuka atau snapshot repo berganti. Gunakan F5 setelah perubahan yang hanya menyentuh log, misalnya `git reflog expire`, karena watcher mengabaikan `.git/logs/`.
- **Windows verification checklist (GPUI UI + full workspace)**:
  1. Jalankan `cargo xtask check` (full workspace termasuk `ui`, `app`, dan `crates/app/src/notice.rs`) dan pastikan lulus tanpa warning.
  2. Verifikasi selector, hash, badge aksi, actor, timestamp dan kedua OID pada detail.
  3. Ganti target atau tutup halaman saat pembacaan berjalan. Coba buka dialog reset lalu pindahkan HEAD atau ubah index dari terminal. Pastikan konfirmasi lama tidak bisa dipakai.
  4. Verifikasi manual di Windows (light & dark theme, DPI 100%/200%): buka repo nyata dengan banyak entri reflog & repo unborn, ganti target `HEAD` ↔ branch lokal, lakukan recover create-branch dan reset Soft/Mixed, tekan Undo (termasuk setelah restart aplikasi), dan cek scrolling virtualized tetap mulus tanpa blank rows.
