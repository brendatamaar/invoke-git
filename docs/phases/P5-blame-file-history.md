# P5 - Blame & File History
Status: review
Depends on: P1
Parallel-safe with: P1b, P2, P3, P4a, P4b, P7, P8, P9, P10, P11

## Goal
Memungkinkan user membuka tampilan blame per baris (penulis, waktu, commit hash, klik untuk lompat ke detail commit) dan riwayat commit khusus satu file (file history) dengan performa responsif.

## Tasks
- [x] Tulis integration test blame & file history di `crates/shell/tests/blame_history.rs` dan unit test layout highlight `BlameView`.
- [x] Implementasikan pembacaan blame & file history via `gix` (fallback read-only git CLI, tercatat di `docs/DECISIONS.md`) di `crates/shell/src/blame.rs`, dijalankan off UI thread dan cancellable.
- [x] Bangun komponen virtualized `BlameView` (gutter commit/author/date + kode bersyntax highlight dari `crates/highlight`).
- [x] Integrasikan aksi Blame dan File history dari diff perubahan dan detail commit.

## Definition of Done
- `cargo xtask check --core` (VPS/CI) atau `cargo xtask check` (Windows) lulus.
- `cargo test -p shell --test blame_history` lulus dan membuktikan hasil blame/history cocok dengan oracle `git blame` / `git log -- <path>`.
- Pembacaan blame dapat dibatalkan saat user pindah file tanpa memblokir UI thread.

## Out of scope
- Pencarian commit lintas seluruh repo (dikerjakan di `P6`).

## Handoff notes
- Gunakan kembali pipeline tree-sitter dari `crates/highlight` untuk baris kode pada tampilan blame agar hanya mewarnai baris terlihat + buffer.
- Selesai di VPS Linux (core/shell): `crates/shell/src/blame.rs` (`BlameReader::{blame, file_history}` + varian `_blocking`; parser `parse_blame`/`parse_log`), `crates/shell/tests/blame_history.rs` (10 tes: file baru, edit multi-author, rename/follow, CRLF, 30k baris, uncommitted, rev, input tak valid, cancel, cancel membunuh git lambat). Oracle: `git blame --line-porcelain` dan `git log --follow --format=%H`. Tidak ada tipe di `core` yang berubah, jadi `ui`/`app` tidak terdampak.
- Cancel: set `Arc<AtomicBool>`; thread pembaca membunuh child git dalam <=10 ms dan channel ditutup tanpa event. UI harus membuat flag baru per file dan set flag lama saat pindah file.
- Batasan: path dengan karakter kontrol (tab/newline) pada `--name-only` bisa tampil ter-quote; `blame` tidak diteruskan ke `--ignore-revs-file` khusus (ikut config git user).
- Lanjutan Windows: aksi pada Changes membuka versi worktree untuk blame dan riwayat dari HEAD. Aksi pada detail commit memakai commit terpilih, atau parent pertama untuk file yang dihapus pada commit itu. Klik hash untuk membuka detail commit. Berpindah mode atau keluar dari tampilan membatalkan pembacaan.
- Blame memproses sintaks off-thread dengan konteks satu file agar capture multiline tepat. Daftar virtualized hanya membentuk style untuk baris yang diminta GPUI. Tes clipping capture multiline UTF-8, sembilan tes shell, dan Clippy UI/app lulus di Windows. Verifikasi klik manual masih terbuka.
- Windows checklist: (1) `cargo xtask check` penuh lulus; (2) `cargo test -p shell --test blame_history` (tes `cancel_kills_a_slow_git` hanya `cfg(unix)`; verifikasi kill di Windows manual: pindah file saat blame file besar, tidak ada `git.exe` yatim di Task Manager); (3) file CRLF (`core.autocrlf=true`) tampil tanpa `\r`; (4) rename + path panjang/non-ASCII; (5) klik hash di gutter membuka commit detail; (6) scroll 30k baris mulus, highlight hanya baris terlihat + buffer.
