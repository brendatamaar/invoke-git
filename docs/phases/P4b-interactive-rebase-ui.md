# P4b - Interactive Rebase Editor UI
Status: review
Depends on: P4a
Parallel-safe with: P3, P5, P6, P7, P8, P9, P10, P11

## Goal
Menyediakan layar Interactive Rebase Editor di GPUI dengan drag-to-reorder commit, pilihan aksi (pick, squash, fixup, edit, drop), kontrol Start/Continue/Skip/Abort, dan integrasi ke layar konflik serta banner state `Rebasing (step n/m)`.

## Tasks
- [x] Tulis unit/property test model urutan & validasi UI plan (mis. tidak boleh squash/fixup commit pertama) di `crates/ui/src/rebase_editor.rs` dan `crates/app/src/rebase_view.rs`.
- [x] Bangun komponen `RebaseEditor` (list baris commit, drag-to-reorder berbasis mouse + tombol naik/turun, dropdown/badge aksi dari `tokens`) di `crates/ui/src/rebase_editor.rs`.
- [x] Hubungkan trigger "Interactive rebase onto…" dari commit graph (`crates/app/src/history_view.rs`), layar edit plan & kontrol saat paused di `crates/app/src/rebase_view.rs` dan `crates/app/src/repo_view.rs`.
- [x] Tulis Windows checklist lengkap di Handoff notes (reorder drag, squash/fixup, pause on `edit`, conflict continue/skip/abort).

## Definition of Done
- `cargo xtask check` (Windows) atau `cargo xtask check --core` + Windows CI hijau.
- `cargo test -p ui` dan `cargo test -p app` lulus mencakup validasi reorder & aksi editor rebase.
- Tidak ada warna/spacing hard-coded di `crates/ui/src/rebase_editor.rs` (semua dari `crates/tokens`).

## Out of scope
- Logika state machine rebase & eksekusi git CLI (sudah selesai di `P4a`).

## Handoff notes
- Menambahkan helper baca `shell::rebase::read_rebase_commits` (via `gix`, read-only, mengembalikan commit `upstream..HEAD` oldest-first dan menolak range non-linear/merge) agar `RebaseView` memuat daftar commit konsisten dengan `check_plan` di P4a.
- `crates/ui/src/rebase_editor.rs` menyediakan `RebaseEditorModel` (pure, dengan unit + `proptest` untuk permutasi reorder & aturan first-kept squash/fixup) dan komponen GPUI `RebaseEditor` (handle drag-to-reorder berbasis mouse + tombol `↑`/`↓`, badge + picker aksi `pick`/`squash`/`fixup`/`edit`/`drop`, banner validasi) memakai 100% warna & spacing dari `crates/tokens`.
- `crates/app/src/rebase_view.rs` menghubungkan `RebaseEditor` ke `Event::RebasePlanEdited` / `RebaseStartRequested` / `AbortRequested`, menampilkan status running & paused (`Rebasing <branch>: paused for edit (step n of m)`), dan menyediakan tombol `Open Changes` / `Open Conflicts`, `Skip this commit`, `Abort rebase` (dengan konfirmasi), dan `Continue rebase`.
- Trigger "Interactive rebase onto…" terpasang di menu klik-kanan commit graph (`crates/app/src/history_view.rs`) dan panel detail commit (`crates/app/src/commit_pane.rs`). `crates/app/src/repo_view.rs` otomatis mengarahkan pause `edit` ke `Page::Rebase`, pause konflik ke `Page::Conflicts`, dan menampilkan banner kontrol rebase ringkas saat pengguna berpindah ke `Changes` untuk mengedit/amend.
- `crates/ui/src/summary.rs` diperluas agar `Phase::Rebasing` saat `RebasePause::Edit` menampilkan `Rebasing n/m: paused for edit`.

### Checklist Windows (verifikasi manual)
- [ ] Bangun app + helper rebase bersama: `cargo build -p app -p shell --bin invoke-rebase-editor` (memastikan `invoke-rebase-editor.exe` berada di direktori yang sama dengan `invoke-git.exe`).
- [ ] Buka repo fixture dengan ≥ 4 commit linear di branch lokal, buka tab **History**, klik kanan commit ancestor (atau pilih di detail commit) lalu klik **Interactive rebase onto…**.
- [ ] Uji **drag-to-reorder** menggunakan handle `⋮⋮` dan tombol `↑`/`↓`; verifikasi indikator garis aksen muncul di target drop dan urutan commit berubah benar.
- [ ] Uji **validasi squash/fixup**: ubah commit pertama menjadi `squash` atau `fixup` (atau drop commit pertama lalu squash commit kedua) — pastikan banner peringatan & badge baris muncul dan tombol **Start rebase** nonaktif; ubah kembali ke `pick` atau geser commit `pick` ke atas dan pastikan validasi bersih.
- [ ] Uji **squash / fixup / drop / reorder** end-to-end: klik **Start rebase**, verifikasi History otomatis kembali dan menampilkan urutan & pesan commit baru, serta tombol **Undo** mengembalikan branch asal.
- [ ] Uji **pause on `edit`**: pilih aksi `edit` pada salah satu commit dan jalankan rebase; verifikasi layar beralih ke `Rebase` dengan banner `Rebasing <branch>: paused for edit (step n of m)`, klik **Open Changes**, ubah file lalu commit `--amend`, lalu klik **Continue rebase** dari banner Changes dan pastikan rebase selesai bersih.
- [ ] Uji **conflict continue / skip / abort**: buat rencana yang memicu konflik; verifikasi layar otomatis pindah ke **Conflicts** dengan banner `Rebasing <branch>: step n of m`, uji resolve + **Continue rebase**, uji **Skip this commit**, dan uji **Abort rebase** (dengan konfirmasi) untuk memastikan branch kembali utuh.
