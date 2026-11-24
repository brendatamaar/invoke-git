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
- `cargo xtask check` lulus di Windows, termasuk Clippy dengan `-D warnings`, 256 urutan model-based, dan compile check UI. Kedua suite fase lulus dengan 25 tes core dan 46 tes shell. `cargo build -p app` juga lulus.
- Gate yang lulus memakai `CARGO_PROFILE_DEV_DEBUG=0`, `CARGO_PROFILE_TEST_DEBUG=0`, dan `CARGO_INCREMENTAL=0` untuk menghemat disk setelah MSVC gagal menulis PDB. Cakupan tes dan jumlah urutan model-based tetap default.
- Fixture Windows untuk emulasi symlink lulus. Fixture symlink native dilewati karena akun Windows ini mengembalikan error 1314. Checklist manual di bawah mencakup verifikasi UI dan symlink native dengan hak pembuatan symlink.
- Timestamp index dipertahankan saat disalin untuk operasi Git dan saat undo memulihkan index. Tes undo intent-to-add membandingkan byte serta timestamp.
- Snapshot index juga menyimpan tree staged agar blob tetap hidup saat Git prune, serta shared index agar split index dapat dipulihkan. Versi untuk Discard all dicatat sebelum konfirmasi dan berasal dari satu index.
- Shell menjalankan add/restore staged pada salinan index sambil memegang `index.lock`. Discard menyiapkan checkout dengan filter Git, mencatat hasilnya untuk undo sebelum mengganti file, dan menjaga index persis. Snapshot file menyimpan mode Git, termasuk symlink. Tes fokus binary/non-UTF-8, intent-to-add, dan CRLF lulus di Windows.
- Atribut filesystem seperti ACL atau bit izin selain executable berada di luar cakupan mode Git. Usulan untuk celah spesifikasi ini adalah mempertahankan cakupan byte, tipe, target symlink, dan mode Git.
- Core menjadwalkan aksi file melalui antrean tulis. Stage/unstage menyimpan byte index sebagai blob agar flag intent-to-add dan index yang semula absen dapat dipulihkan. Kedua property test baru gagal saat guard expected-index sengaja dibalik, lalu perubahan tersebut dikembalikan.
- Tes lebih dulu mendefinisikan kontrak perintah per file, pemulihan byte index (termasuk intent-to-add), dan pemulihan worktree. Tes baru bergantung pada tipe dan runner dari task berikutnya.
- Untuk celah spesifikasi discard intent-to-add, file worktree dihapus seperti file untracked. Flag intent-to-add tetap di index dan undo memulihkan byte asli.
- Konteks awal lihat `tasks/011.DONE.md` (bagian What is unfinished) dan `tasks/013.DONE.md` (bagian What is unfinished): sebelumnya `load_inputs` mengembalikan `NotText` karena tidak ada varian `GitArgs` untuk `git add` / `git restore --staged` / `git checkout` / `git clean`.

### Checklist manual Windows

- Buka repo fixture dengan file binary, file berisi byte non-UTF-8, dan file teks CRLF dengan `core.autocrlf=true`. Ubah ketiganya, lalu centang file dan folder, dan coba Stage all serta Unstage all. Bandingkan isi worktree dan output `git ls-files --stage` sebelum dan setelah Undo.
- Pilih setiap file unstaged, klik Discard file, lalu Undo. Periksa bahwa checkout mengikuti filter Git, byte yang diedit kembali persis, dan file lain tetap sama. Coba juga file baru (termasuk `git add -N`) dan file tracked yang dihapus.
- Jalankan Discard all, ubah salah satu file dari editor setelah konfirmasi terbuka, lalu konfirmasi. Pastikan perubahan baru pada file itu selamat. Periksa file lain dan Undo satu per satu.
- Dengan pembuatan symlink diaktifkan dan `core.symlinks=true`, ulangi stage, unstage, discard, dan undo pada symlink dengan target berbeda. Periksa target dengan `Get-Item` serta pastikan isi file target tetap sama.
- NTFS tidak memiliki bit executable Git. Buat perubahan staged mode-only dengan `git update-index --chmod=+x -- script`, lalu verifikasi unstage dan Undo mengembalikan mode index.
