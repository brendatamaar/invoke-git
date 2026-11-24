# P8 - Forge Auth & Repo List (GitHub/GitLab)
Status: in-progress
Depends on: P1
Parallel-safe with: P1b, P2, P3, P4a, P4b, P5, P6, P7, P10, P11

## Goal
Menyediakan koneksi opsional akun GitHub (OAuth device flow), GitLab.com (OAuth PKCE / PAT), dan self-hosted GitLab (PAT + instance URL) dengan penyimpanan token di OS keychain (`keyring`), pemilihan akun per host remote, serta daftar repo untuk clone langsung dari aplikasi.

## Tasks
- [ ] Tulis unit/integration test mock HTTP untuk client GitHub & GitLab API, pencocokan host multi-akun, dan alur penyimpanan/hapus token (dengan backend in-memory untuk CI) di `crates/shell/tests/forge_auth.rs` (atau crate `forge` bila dipisah).
- [ ] Tambah `reqwest` dan `keyring` sesuai tabel Build-vs-Library `docs/PRD.md`, lalu implementasikan auth flow (GitHub device flow, GitLab OAuth PKCE / PAT, self-hosted PAT) dan API list repositories.
- [ ] Tambah UI manajemen akun forge (opsional, tanpa login wall) dan tab "Clone from account" pada dialog clone (`crates/app/src/clone_view.rs`, `crates/app/src/workspace.rs`) + Windows checklist di Handoff notes.

## Definition of Done
- `cargo xtask check --core` (VPS/CI) atau `cargo xtask check` (Windows) lulus.
- `cargo test -p shell --test forge_auth` lulus (mock GitHub/GitLab auth, multi-account host matching, repo list pagination, error handling).
- Token tidak pernah ditulis ke file konfigurasi teks biasa (selalu melalui `keyring`), dan semua fitur git inti tetap berjalan tanpa akun forge.

## Out of scope
- List, checkout, dan pembuatan PR/MR (dikerjakan di `P9`).
- Menyimpan password/private key git CLI (tetap dikelola sistem melalui `GIT_ASKPASS`).

## Handoff notes
- Catat di `docs/DECISIONS.md` bila memutuskan membuat crate `crates/forge` terpisah vs modul di `crates/shell/src/forge/`.
