# P1b - Non-Text Staging & Discard
Status: review
Depends on: P1
Parallel-safe with: P3, P5, P6, P7, P8

## Goal
Memungkinkan user melakukan stage, unstage, dan discard (yang tetap undoable & terlindungi expected-index guard) untuk file binary, non-UTF-8, symlink, mode-only (chmod), dan entri intent-to-add yang sebelumnya ditolak oleh text patch builder.

## Tasks
- [x] Tulis unit/property test di `crates/core/tests/stage_props.rs` dan `crates/core/tests/discard_props.rs` serta integration test di `crates/shell/tests/staging.rs` dan `crates/shell/tests/discard.rs` untuk file binary, non-UTF-8, mode-only, dan untracked/tracked discard + undo (termasuk kasus `core.autocrlf=true` untuk discard).
- [x] Tambah varian perintah git non-patch (`Add`, `RestoreStaged`, `RestoreWorktree`, `CleanPath` atau setara) di `crates/core/src/ops.rs`, `crates/core/src/step.rs`, dan `crates/shell/src/git.rs` dengan expected-index guard & undo snapshot.
- [x] Implementasikan eksekusi stage/unstage/discard non-text di `crates/shell/src/staging.rs` dan `crates/shell/src/discard.rs`.
- [x] Hubungkan tombol checkbox/discard di `crates/app/src/staging.rs`, `crates/app/src/discard.rs`, dan `crates/app/src/repo_view.rs` agar file non-text memakai jalur non-patch (tidak lagi menampilkan notice penolakan), serta siapkan Windows checklist di Handoff notes.

## Definition of Done
- `cargo xtask check --core` (VPS/CI) atau `cargo xtask check` (Windows) lulus tanpa warning.
- `cargo test -p invoke-core --test stage_props --test discard_props` dan `cargo test -p shell --test staging --test discard` lulus mencakup file binary, non-UTF-8, dan undo.
- File binary/mode-only dapat di-stage, di-unstage, di-discard, dan di-undo kembali ke state persis sebelumnya (Law 4, 5, 6, 8 terjaga).

## Out of scope
- Dukungan khusus Git LFS pointer/filter dan submodule management (dikerjakan di `P10`).
- Combined single undo point untuk multi-file `Stage all` / `Discard all` (opsional nanti).

## Handoff notes
- Re-check follow-up from `48873b5`. All five regression tests failed on the original code.
  - Native symlink checkout invoked Git without emulation. Windows now forces `core.symlinks=false` for the private checkout, preserves configured emulation, and creates native links in Rust. Only bounded relative targets through real directories determine directory-link type. The trace regression runs without symlink privilege. Invalid UTF-8 targets are refused in native mode and retained byte-for-byte in emulation mode. [Git for Windows target lookup](https://github.com/git-for-windows/git/blob/v2.53.0.windows.1/compat/mingw.c#L374-L382)
  - A smudge filter observed scratch under an in-worktree separate Git directory. Scratch now uses a normal `.git` directory or an external location. For discard/undo it must be on the worktree volume. If neither the Git directory nor system temp is eligible, the operation refuses before changing the worktree. No second physical volume was available for testing.
  - Git staged `entry/inside` through a junction in the fixture. Stage and discard now refuse junction leaves. Windows staging pins existing path components and uses exact index removal for a missing path. Incompatible open write handles can block staging. Discard refuses before saving a junction into a snapshot that cannot restore its type (Law 8). Older snapshots did not record a junction tag, so they cannot be repaired automatically.
  - An atomic editor save after the final read was overwritten. Whole-file discard and discard undo now move the old entry aside, verify its bytes and mode there, and install or restore only at a vacant path. If another file takes that path before rollback, the error names the preserved backup. This protects atomic saves by rename. An edit through an already open handle can still write to the moved entry.
  - Final Windows targeted runs exited 0. Core reported 25 passed and shell reported 61 (37 discard and 24 staging, including three native symlink cases that emitted SKIPPED for error 1314). `cargo xtask check` exited 0. Formatting, Clippy with `-D warnings`, and the Windows app/UI compile check passed. The workspace harness reported 899 passed, 2 ignored, and 0 failed, including all 256 model-based sequences. The three native symlink skip messages are included in the reported pass total. Linux/macOS tests were not run.
- `cargo xtask check` lulus di Windows (juga setelah cross review fixes), termasuk Clippy dengan `-D warnings`, 256 urutan model-based, dan compile check UI. Kedua suite fase lulus dengan 25 tes core dan 57 tes shell (22 staging, 35 discard; tiga tes symlink native dilewati dengan `SKIPPED` di mesin ini). `cargo build -p app` juga lulus.
- Gate yang lulus memakai `CARGO_PROFILE_DEV_DEBUG=0`, `CARGO_PROFILE_TEST_DEBUG=0`, dan `CARGO_INCREMENTAL=0` untuk menghemat disk setelah MSVC gagal menulis PDB. Cakupan tes dan jumlah urutan model-based tetap default.
- Fixture Windows untuk emulasi symlink lulus. Fixture symlink native dilewati karena akun Windows ini mengembalikan error 1314. Checklist manual di bawah mencakup verifikasi UI dan symlink native dengan hak pembuatan symlink.
- Timestamp index dipertahankan saat disalin untuk operasi Git dan saat undo memulihkan index. Tes undo intent-to-add membandingkan byte serta timestamp.
- Snapshot index juga menyimpan tree staged agar blob tetap hidup saat Git prune, serta shared index agar split index dapat dipulihkan. Versi untuk Discard all dicatat sebelum konfirmasi dan berasal dari satu index.
- Shell menjalankan add/restore staged pada salinan index sambil memegang `index.lock`. Discard menyiapkan checkout dengan filter Git, mencatat hasilnya untuk undo sebelum mengganti file, dan menjaga index persis. Snapshot file menyimpan mode Git, termasuk symlink. Tes fokus binary/non-UTF-8, intent-to-add, dan CRLF lulus di Windows.
- Atribut filesystem seperti ACL atau bit izin selain executable berada di luar cakupan mode Git. Usulan untuk celah spesifikasi ini adalah mempertahankan cakupan byte, tipe, target symlink, dan mode Git.
- Core menjadwalkan aksi file melalui antrean tulis. Stage/unstage menyimpan byte index sebagai blob agar flag intent-to-add dan index yang semula absen dapat dipulihkan. Kedua property test baru gagal saat guard expected-index sengaja dibalik, lalu perubahan tersebut dikembalikan.
- Tes lebih dulu mendefinisikan kontrak perintah per file, pemulihan byte index (termasuk intent-to-add), dan pemulihan worktree. Tes baru bergantung pada tipe dan runner dari task berikutnya.
- Untuk celah spesifikasi discard intent-to-add, file worktree dihapus seperti file untracked. Flag intent-to-add tetap di index dan undo memulihkan byte asli.
- Cross review fixes (review Gemini, 2026-11-24). Semua temuan diverifikasi dulu. Tes baru gagal di kode lama, kecuali yang dicatat:
  - (1) `is_link` memakai atribut reparse apa pun. Sekarang hanya name surrogate (symlink, junction) via `FileType` std. Tes unit Windows memasang tag pihak ketiga non-surrogate pada folder dan file, sementara junction tetap ditolak.
  - (2) Rename gagal di atas file read-only (os error 5, terbukti). Hanya target yang memang menghalangi rename, yaitu file read-only atau symlink/junction direktori, yang dipindahkan ke folder backup sendiri di dalam scratch. Target tidak pernah dihapus. Pemindahan balik dan pemasangan memakai `MoveFileExW` tanpa replace, jadi file lain yang muncul di target tetap aman. Jika pemindahan balik gagal, scratch dipertahankan dan error menyebut lokasinya.
  - (3) `fix_link_kind` membuat symlink direktori hanya untuk target relatif yang tetap di dalam worktree secara teks dan menuju folder asli lewat folder asli. Target absolut, UNC, drive-relative, keluar via `..`, atau melewati link tidak pernah diperiksa ke disk atau jaringan dan tetap menjadi symlink file, default Git untuk target yang tidak ada. Batasan: link ke folder di luar repo dibuat sebagai link file.
  - (4) Path scratch berlipat ganda dan gagal "Filename too long" (terbukti). `checkout-index` privat memakai `-c core.longpaths=true`. Lokasi scratch dibahas di re-check.
  - (5) `UndoRunner::run` deadlock dengan stdin 4 MB dan stderr besar (terbukti, timeout). Stdin ditulis di thread scoped sambil output dikuras.
  - (6) Unstage menolak saat folder induk di worktree jadi file (terbukti). Unstage kini hanya memeriksa teks path (`check_repo_path`).
  - (7) Beda huruf (`foo.txt` vs `Foo.txt`) di-hash sebagai file yang sama (terbukti). Nama on-disk dicek dengan `FindFirstFileW`, yang tidak membuka file.
  - (8) Persiapan Discard file/all memakai counter diff yang dinaikkan tiap watcher read. Lihat re-check untuk bentuk akhirnya.
  - (9) Snapshot dengan folder di posisi file kini `PreconditionChanged`, bukan `Bug` (terbukti). Error baca jadi `Environment`.
- Re-check (Gemini Neon, 2026-11-24). Setiap tes baru gagal di kode sebelumnya:
  - Data loss (2): `install_file` lama menghapus target writable saat rename gagal karena ACL source. Tes unit ACL membuktikan file hilang. Kini target dipindah ke backup, bukan dihapus. Tracked file bernama `previous` bertabrakan dengan nama backup lama, sehingga discard berhasil tanpa mengubah file (dibuktikan dengan mutasi). Folder backup per operasi dari `04e0217` memperbaikinya, dan kasus ini kini diuji end-to-end.
  - Undo blob (Law 8): error penulisan stdin tidak lagi diabaikan. `save_blob` membandingkan ID dari `hash-object` dengan ID byte input.
  - Scratch: kini di bawah git dir, yang diabaikan status dan watcher, selama satu volume dengan worktree (`dev` di Unix, `GetVolumePathNameW` di Windows), jadi rename tetap atomik. Git dir di volume lain kembali ke root worktree, dan foldernya bisa sesaat terlihat untracked. Folder `.invoke-*` sisa crash di git dir tidak dibersihkan otomatis, sama seperti scratch lama di sana.
  - Case mismatch: folder di jalur juga dicek. Runner patch kini memberi `PreconditionChanged` untuk mismatch folder maupun leaf, bukan `Environment`/`Bug`.
  - App: `discard::Confirmation` memegang konfirmasi yang tampil dan tiketnya. `clear()` (cancel, pilih atau kosongkan file) dan `show()` membatalkan persiapan yang masih jalan. Reload diff tidak menyentuhnya.
  - Symlink native: tes dilewati dengan pesan `SKIPPED` langsung ke stderr, dan `INVOKE_REQUIRE_SYMLINKS=1` membuatnya gagal. Mesin ini mengembalikan error 1314, jadi overwrite symlink direktori dan jenis link hanya terverifikasi lewat checklist manual.
  - Riwayat: commit `04e0217` dari run Codex yang berjalan bersamaan ikut mengambil panggilan `save_blob` yang belum di-commit. Commit itu tidak bisa dikompilasi sendiri; `save_blob` baru ada di `6de84bb`. Riwayat sengaja tidak di-rebase.
- Konteks awal lihat `tasks/011.DONE.md` (bagian What is unfinished) dan `tasks/013.DONE.md` (bagian What is unfinished): sebelumnya `load_inputs` mengembalikan `NotText` karena tidak ada varian `GitArgs` untuk `git add` / `git restore --staged` / `git checkout` / `git clean`.

### Checklist manual Windows

- Buka repo fixture dengan file binary, file berisi byte non-UTF-8, dan file teks CRLF dengan `core.autocrlf=true`. Ubah ketiganya, lalu centang file dan folder, dan coba Stage all serta Unstage all. Bandingkan isi worktree dan output `git ls-files --stage` sebelum dan setelah Undo.
- Pilih setiap file unstaged, klik Discard file, lalu Undo. Periksa bahwa checkout mengikuti filter Git, byte yang diedit kembali persis, dan file lain tetap sama. Coba juga file baru (termasuk `git add -N`) dan file tracked yang dihapus.
- Jalankan Discard all, ubah salah satu file dari editor setelah konfirmasi terbuka, lalu konfirmasi. Pastikan perubahan baru pada file itu selamat. Periksa file lain dan Undo satu per satu.
- Dengan pembuatan symlink diaktifkan dan `core.symlinks=true`, ulangi stage, unstage, discard, dan undo pada symlink dengan target berbeda. Periksa target dengan `Get-Item` serta pastikan isi file target tetap sama.
- NTFS tidak memiliki bit executable Git. Buat perubahan staged mode-only dengan `git update-index --chmod=+x -- script`, lalu verifikasi unstage dan Undo mengembalikan mode index.
- Folder OneDrive (Files On-Demand): buka repo di dalam OneDrive dengan file yang belum diunduh ("online-only"). Stage, unstage, Discard file, dan Undo harus berhasil tanpa error "not a file or symlink" atau "not a path inside the working tree".
- Hak symlink: aktifkan Settings > System > For developers > Developer Mode, lalu buka terminal baru. Alternatifnya, jalankan PowerShell sebagai Administrator. Verifikasi dengan `New-Item -ItemType SymbolicLink -Path probe -Target .` tanpa error 1314, lalu hapus `probe`. Kemudian jalankan `$env:INVOKE_REQUIRE_SYMLINKS='1'; cargo test -p shell --test discard symlink`. Semua tes harus lulus tanpa baris `SKIPPED`.
- Overwrite symlink direktori (dengan hak di atas): commit file `entry`, ganti dengan `New-Item -ItemType SymbolicLink -Path entry -Target dir`, lalu Discard file. `entry` harus kembali menjadi file dan isi `dir` tetap sama. Undo harus mengembalikan symlink direktori.
- Jenis link (dengan hak di atas dan `core.symlinks=true`): commit `link -> dir` sebagai symlink direktori, hapus `link`, lalu Discard file. `(Get-Item link).Attributes` harus memuat `Directory` dan `cd link` harus berhasil. Ulangi dengan target `..\luar-repo` atau path absolut: link harus menjadi symlink file tanpa akses jaringan atau keluar repo.
- Path panjang: buat file tracked dengan path penuh sekitar 240 karakter tanpa `core.longpaths`. Ubah file itu, lalu jalankan Discard file dan Undo. Keduanya harus berhasil. Selama discard, tidak ada folder `.invoke-*` yang muncul di root worktree maupun di daftar untracked.
