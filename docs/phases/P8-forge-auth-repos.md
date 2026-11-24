# P8 - Forge Auth & Repo List (GitHub/GitLab)
Status: review
Depends on: P1
Parallel-safe with: P1b, P2, P3, P4a, P4b, P5, P6, P7, P10, P11

## Goal
Menyediakan koneksi opsional akun GitHub (OAuth device flow), GitLab.com (OAuth PKCE / PAT), dan self-hosted GitLab (PAT + instance URL) dengan penyimpanan token di OS keychain (`keyring`), pemilihan akun per host remote, serta daftar repo untuk clone langsung dari aplikasi.

## Tasks
- [x] Tulis unit/integration test mock HTTP untuk client GitHub & GitLab API, pencocokan host multi-akun, dan alur penyimpanan/hapus token (dengan backend in-memory untuk CI) di `crates/shell/tests/forge_auth.rs` (atau crate `forge` bila dipisah).
- [x] Tambah `reqwest` dan `keyring` sesuai tabel Build-vs-Library `docs/PRD.md`, lalu implementasikan auth flow (GitHub device flow, GitLab OAuth PKCE / PAT, self-hosted PAT) dan API list repositories.
- [x] Tambah UI manajemen akun forge (opsional, tanpa login wall) dan tab "Clone from account" pada dialog clone (`crates/app/src/clone_view.rs`, `crates/app/src/workspace.rs`) + Windows checklist di Handoff notes.

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
- **Task 3 selesai (UI, 2026-11-24).** `crates/app/src/forge_accounts.rs` (layar Accounts: Connect GitHub / Connect GitLab.com / access token, daftar akun, "Use for host", Disconnect) dan `crates/app/src/repo_picker.rs` (daftar repo + filter + Load more); `clone_view.rs` mendapat tab "Address | From account" (additif; layar Accounts dibuka dari picker). Bagian forge dibuat malas: tidak ada kontak jaringan/keychain sampai pengguna membuka tab "From account"; kegagalan membuka layanan forge hanya menampilkan banner dan clone lewat alamat tetap jalan. Memilih repo hanya mengisi alamat; clone memakai kredensial git biasa (askpass/credential helper), tidak memakai token forge. `workspace.rs` tidak diubah.
- Di luar scope / dicatat: tidak ada tombol Accounts di layar Welcome (hanya lewat dialog clone); token forge tidak dipakai git clone (P9/kemudian bila diinginkan); tidak ada refresh otomatis sebelum klik "Load more" selain jalur refresh di `ForgeService`.
- **Checklist verifikasi manual Windows** (belum diverifikasi visual; semua langkah tanpa akun harus tetap jalan):
  1. Build `cargo build -p app -p askpass`, jalankan dengan profil bersih: `$env:INVOKE_GIT_DATA_DIR = "$env:TEMP\ig-p8"`.
  2. Tanpa akun: Welcome → Clone: tab "Address" tampil seperti sebelumnya; clone repo publik lewat alamat berhasil. Buka tab "From account": pesan "No account connected" + tombol "Connect an account…"; tidak ada error dan tidak ada permintaan jaringan.
  3. Accounts → "Use an access token…": pilih GitHub, tempel PAT (kolom bertanda bintang; Copy/Cut tidak menyalin). Token salah → banner "server did not accept the token" dan tidak ada akun tersimpan. Token benar → akun muncul, kolom token kosong. Cek Windows Credential Manager (Generic credentials) ada entri "Invoke Git forge accounts"; `%INVOKE_GIT_DATA_DIR%\forge-accounts.txt` berisi jenis/host/username saja (tidak ada token): `Select-String -Path ... -Pattern <token>` kosong.
  4. Kembali ke "From account": daftar repo termuat, filter mengecilkan daftar, hitungan "N of M" benar, "Load more" muncul bila >500 repo. Klik repo → pindah ke tab Address dengan alamat terisi; pilih folder → Clone berjalan lewat alur biasa.
  5. Tambah akun kedua pada host yang sama: kontrol segmented memilih akun; Accounts menampilkan "Use for github.com" dan label "Used for …" untuk pilihan. Disconnect → entri Credential Manager hilang dan akun hilang dari daftar; restart app: akun tidak kembali.
  6. GitLab self-hosted: access token + `https://host` ; `http://host` ditolak ("Use https://"), alamat dengan path/user ditolak.
  7. OAuth: `Connect GitHub` / `Connect GitLab.com` tanpa client id menampilkan "no sign-in app set up yet". Dengan `$env:INVOKE_GIT_GITHUB_CLIENT_ID` (app OAuth dengan device flow aktif): browser terbuka ke halaman verifikasi, kode tampil + Copy code; setelah disetujui akun muncul. GitLab dengan `INVOKE_GIT_GITLAB_CLIENT_ID` + redirect `http://127.0.0.1:47821/callback`: tab browser terbuka, setelah setuju halaman "Signed in" muncul dan akun tersambung; Cancel di tengah proses menutup listener (port 47821 bebas lagi: `netstat -ano | findstr 47821`).
  8. Matikan jaringan: daftar repo menampilkan "Could not reach the server" + "Try again". Cabut token di situs lalu "Load more"/ganti akun: banner "did not accept the token" + "Reconnect…".
  9. Windows Firewall prompt: listener loopback hanya bind 127.0.0.1; tidak boleh memunculkan prompt firewall.
