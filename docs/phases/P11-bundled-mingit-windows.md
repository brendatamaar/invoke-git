# P11 - Bundled MinGit Fallback (Windows)
Status: in-progress
Depends on: P1
Parallel-safe with: P2, P3, P4a, P4b, P5, P6, P7, P8, P9, P10

## Goal
Menyediakan fallback MinGit terbundel di Windows bila system git (< 2.40 atau tidak terpasang) tidak ditemukan, sambil tetap selalu memprioritaskan system git ketika tersedia, serta menyiapkan packaging rilis Windows.

## Tasks
- [ ] Tulis unit/integration test resolusi binary git (prioritas system git ≥ 2.40, fallback ke bundled MinGit bila system git absen/terlalu lama, pesan error jelas bila keduanya tidak ada) di `crates/shell/tests/git_discovery.rs`.
- [ ] Perbarui deteksi binary git saat startup di `crates/shell/src/git.rs` dan `crates/app/src/setup.rs` agar memeriksa system `git` terlebih dahulu lalu fallback ke path MinGit terbundel di direktori instalasi aplikasi.
- [ ] Tambah skrip/workflow packaging Windows (download & verifikasi SHA-256 MinGit yang dipin, bundling binary + `askpass`) di `xtask/` atau `.github/workflows/`.
- [ ] Dokumentasikan versi MinGit yang dipin & prosedur update di `README.md` / `docs/DECISIONS.md` + Windows checklist di Handoff notes.

## Definition of Done
- `cargo xtask check --core` (VPS/CI) atau `cargo xtask check` (Windows) lulus.
- `cargo test -p shell --test git_discovery` lulus untuk skenario system git valid, system git versi lama, system git tidak ada + fallback MinGit tersedia, dan keduanya tidak ada.
- Aplikasi tetap memilih system git saat versi ≥ 2.40 tersedia di `PATH` dan menggunakan MinGit hanya sebagai fallback.

## Out of scope
- Packaging installer macOS dan Linux (build macOS/Linux ditunda sesuai `docs/PRD.md`).

## Handoff notes
- Pastikan `GIT_ASKPASS` dan credential helper sistem tetap bekerja saat fallback ke MinGit digunakan.
