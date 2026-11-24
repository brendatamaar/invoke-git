# P8 - Forge Auth & Repo List (GitHub/GitLab)
Status: in-progress
Depends on: P1
Parallel-safe with: P1b, P2, P3, P4a, P4b, P5, P6, P7, P10, P11

## Goal
Menyediakan koneksi opsional akun GitHub (OAuth device flow), GitLab.com (OAuth PKCE / PAT), dan self-hosted GitLab (PAT + instance URL) dengan penyimpanan token di OS keychain (`keyring`), pemilihan akun per host remote, serta daftar repo untuk clone langsung dari aplikasi.

## Tasks
- [x] Tulis unit/integration test mock HTTP untuk client GitHub & GitLab API, pencocokan host multi-akun, dan alur penyimpanan/hapus token (dengan backend in-memory untuk CI) di `crates/shell/tests/forge_auth.rs` (atau crate `forge` bila dipisah).
- [x] Tambah `reqwest` dan `keyring` sesuai tabel Build-vs-Library `docs/PRD.md`, lalu implementasikan auth flow (GitHub device flow, GitLab OAuth PKCE / PAT, self-hosted PAT) dan API list repositories.
- [ ] Tambah UI manajemen akun forge (opsional, tanpa login wall) dan tab "Clone from account" pada dialog clone (`crates/app/src/clone_view.rs`, `crates/app/src/workspace.rs`) + Windows checklist di Handoff notes.

## Definition of Done
- `cargo xtask check --core` (VPS/CI) atau `cargo xtask check` (Windows) lulus.
- `cargo test -p shell --test forge_auth` lulus (mock GitHub/GitLab auth, multi-account host matching, repo list pagination, error handling).
- Token tidak pernah ditulis ke file konfigurasi teks biasa (selalu melalui `keyring`), dan semua fitur git inti tetap berjalan tanpa akun forge.

## Out of scope
- List, checkout, dan pembuatan PR/MR (dikerjakan di `P9`).
- Menyimpan password/private key git CLI (tetap dikelola sistem melalui `GIT_ASKPASS`).

## Handoff notes
- Catat di `docs/DECISIONS.md` bila memutuskan membuat crate `crates/forge` terpisah vs modul di `crates/shell/src/forge/`. **Diputuskan:** modul di `crates/shell/src/forge/` (bagian murni di `crates/core/src/forge.rs`); lihat DECISIONS.md.
- **Task 1 + 2 selesai (2026-11-24).** Urutan commit: bagian murni core dulu (akun, host matching, mesin sign-in), lalu `reqwest`/`keyring` + klien API + runner + `crates/shell/tests/forge_auth.rs` (30 tes, server mock `std::net` di file tes; tidak ada crate mock baru, tidak ada jaringan nyata, tidak menyentuh keychain OS).
- Peta kode: `core/src/forge.rs` (Origin, ForgeAccount, AccountBook + teks, `step_sign_in`, `RemoteRepo`, `filter_repos`); `shell/src/forge/` = `json` (pembaca JSON kecil, tanpa serde_json), `codec` (pct-encode, base64url, SHA-256, PKCE, header `Link`), `secrets` (`Secret`, `Credential`, `MemoryStore`, `KeyringStore`), `http` (satu-satunya pengirim; https-only kecuali loopback, tanpa redirect, body maks 8 MiB), `api` (GitHub device flow, GitLab PKCE/refresh, `/user`, daftar repo), `service` (`ForgeService`: akun + token + daftar repo + refresh), `signin` (`SignInRunner`: device flow polling + listener loopback PKCE).
- Token hanya lewat `TokenStore` (keychain OS di app, memori di tes). File daftar akun (`forge-accounts.txt` di folder data app) hanya berisi jenis/host/username; tes memeriksa tidak ada token di disk. Pesan error dibuat dari teks tetap + kode status/kode OAuth yang disaring, tidak pernah dari body/URL/teks pustaka.
- **Spec gap 1 (client id OAuth):** `GITHUB_OAUTH_CLIENT_ID` dan `GITLAB_OAUTH_CLIENT_ID` di `crates/shell/src/forge/service.rs` masih `None` (aplikasi OAuth belum didaftarkan). Selama kosong, sign-in OAuth menjawab "belum diatur, pakai access token"; PAT tetap jalan. Pengembangan: env `INVOKE_GIT_GITHUB_CLIENT_ID` / `INVOKE_GIT_GITLAB_CLIENT_ID`. Pertanyaan: siapa mendaftarkan OAuth app GitHub (device flow aktif) dan aplikasi GitLab.com (non-confidential, redirect `http://127.0.0.1:47821/callback`)?
- **Spec gap 2 (port redirect GitLab):** GitLab mencocokkan redirect URI persis, jadi port loopback tetap `47821` (`GITLAB_LOOPBACK_PORT`). Usulan: pakai port ini; jika sibuk, tampilkan pesan dan sarankan PAT.
- **Spec gap 3 (scope):** GitHub `repo` (satu-satunya scope yang melihat repo privat), GitLab `read_api`. P9 (buat PR/MR) mungkin butuh `api` di GitLab → perlu sign-in ulang; usulan: naikkan scope di P9.
- **Spec gap 4 (GitLab di sub-path / GitHub Enterprise):** instance di bawah sub-path (`https://host/gitlab`) dan GitHub Enterprise Server belum didukung (PRD hanya github.com, gitlab.com, GitLab self-hosted). `Origin` menolak path.
- **Spec gap 5 (TLS):** `reqwest` memakai `native-tls` (Schannel di Windows, Security.framework di macOS, OpenSSL di Linux → butuh `libssl-dev` untuk build shell di Linux/VPS). Alternatif `rustls` butuh toolchain C (aws-lc). Usulan: tetap native-tls; konfirmasi bila CI Linux tidak punya OpenSSL.
- Dependensi baru: `reqwest` (blocking, native-tls, tanpa fitur json/form) dan `keyring` 4 (keduanya ada di tabel PRD); fitur `Win32_Security_Cryptography` pada `windows-sys` (sudah ada di workspace) untuk `BCryptGenRandom` (verifier PKCE dan `state`). Tanpa `serde_json`/`sha2`/`rand`: JSON, SHA-256 dan base64url ditulis sendiri dan diuji dengan vektor publik + proptest.
