# P11 - Bundled MinGit Fallback (Windows)
Status: review
Depends on: P1
Parallel-safe with: P2, P3, P4a, P4b, P5, P6, P7, P8, P9, P10

## Goal
Menyediakan fallback MinGit terbundel di Windows bila system git (< 2.40 atau tidak terpasang) tidak ditemukan, sambil tetap selalu memprioritaskan system git ketika tersedia, serta menyiapkan packaging rilis Windows.

## Tasks
- [x] Tulis unit/integration test resolusi binary git (prioritas system git ≥ 2.40, fallback ke bundled MinGit bila system git absen/terlalu lama, pesan error jelas bila keduanya tidak ada) di `crates/shell/tests/git_discovery.rs`.
- [x] Perbarui deteksi binary git saat startup di `crates/shell/src/git.rs` dan `crates/app/src/setup.rs` agar memeriksa system `git` terlebih dahulu lalu fallback ke path MinGit terbundel di direktori instalasi aplikasi.
- [x] Tambah skrip/workflow packaging Windows (download & verifikasi SHA-256 MinGit yang dipin, bundling binary + `askpass`) di `xtask/` atau `.github/workflows/`.
- [x] Dokumentasikan versi MinGit yang dipin & prosedur update di `README.md` / `docs/DECISIONS.md` + Windows checklist di Handoff notes.

## Definition of Done
- `cargo xtask check --core` (VPS/CI) atau `cargo xtask check` (Windows) lulus.
- `cargo test -p shell --test git_discovery` lulus untuk skenario system git valid, system git versi lama, system git tidak ada + fallback MinGit tersedia, dan keduanya tidak ada.
- Aplikasi tetap memilih system git saat versi ≥ 2.40 tersedia di `PATH` dan menggunakan MinGit hanya sebagai fallback.

## Out of scope
- Packaging installer macOS dan Linux (build macOS/Linux ditunda sesuai `docs/PRD.md`).

## Handoff notes
- Pastikan `GIT_ASKPASS` dan credential helper sistem tetap bekerja saat fallback ke MinGit digunakan.

### Handoff notes (P11, 2026-11-24)
- Done: `shell::git_discovery::resolve` (pure; PATH probe and file-exists injected) + `Git::detect_with_fallback[_at]`; `Workspace` uses it at startup and on "check again". New `SetupError::FallbackFailed` (system + bundled both unusable) with a setup message. If no MinGit is installed the error is the system git's own (`Missing`/`TooOld`). `Git::source()` says System or Bundled.
- Bundled layout: `<app dir>\mingit\cmd\git.exe` (the zip is extracted as is). `askpass.exe` and `invoke-rebase-editor.exe` sit beside the app exe, which is where the app already looks for them, so `GIT_ASKPASS` is unaffected by which git runs.
- Packaging: `cargo xtask package-windows` -> `dist\windows\`. Pin: MinGit 2.56.1 64-bit, tag `v2.56.1.windows.1`, SHA-256 `4a92850b...dfefdafd`, taken from the release page and recomputed locally from the downloaded zip (same size 39849768, same hash). Hash via PowerShell `Get-FileHash`, unzip via `System32\tar.exe`, download via `curl.exe`: no new crate. Tests never download; only the pure digest check is tested. The full `package-windows` run (release build + download) was not executed here.
- Credentials: this MinGit zip ships `git-credential-manager.exe` and `git-credential-wincred.exe` and its `etc/gitconfig` has a `[credential]` section; `GIT_ASKPASS` is set per command by the shell, independent of the git binary.
- Spec gap / question: MinGit's own `etc/gitconfig` includes `C:/Program Files/Git/etc/gitconfig`; so a user with an old system git still gets that config. Proposed: accept. Another: ARM64 Windows would need the `arm64` zip; proposed: out of scope until requested.
- Windows manual checklist:
  1. Normal machine with git >= 2.40 on PATH: app starts, `Git::source()` is System (no `mingit` folder needed).
  2. Run `cargo xtask package-windows`; confirm `dist\windows\mingit\cmd\git.exe --version` prints 2.56.1.
  3. Remove git from PATH (run `dist\windows\invoke-git.exe` from a shell without git): repo opens, status/commit work using MinGit.
  4. Same, but also delete `mingit\`: blocking setup screen explains git is required; "check again" after restoring MinGit works.
  5. With MinGit: clone an HTTPS repo that needs auth: the app's askpass prompt appears; then one with a stored Windows credential (GCM): no prompt needed.
  6. Interactive rebase still launches the editor (`invoke-rebase-editor.exe` beside the app).
  7. Install under a path with spaces and a long path.
- DoD run (2026-11-24): fmt + clippy `-D warnings` clean; `cargo check -p ui -p app` ok; `cargo test -p shell --test git_discovery` 10/10. In `cargo xtask check` the `harness` `first_half::model_based_sequences` failed once ("read ref: Could not traverse reference directory", ~32 min run while other worktrees were building) and `shell --test user_config` failed once in a workspace run; both passed when re-run alone and the rest of the workspace passed with `--no-fail-fast`. Looks like Windows file-system contention across parallel worktrees, not P11 code.
