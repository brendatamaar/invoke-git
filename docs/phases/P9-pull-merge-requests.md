# P9 - Pull Requests & Merge Requests
Status: todo
Depends on: P8
Parallel-safe with: P4b, P5, P6, P7, P10, P11

## Goal
Memungkinkan user melihat daftar Pull Request (GitHub) dan Merge Request (GitLab / self-hosted) pada repo yang terhubung, men-checkout PR/MR secara lokal, serta membuat PR/MR baru dari branch aktif (termasuk tautan dari layar pre-push review).

## Tasks
- [ ] Tulis unit/integration test mock HTTP untuk list PR/MR, detail & status review, pembuatan PR/MR, dan resolusi remote/branch target (termasuk fork) di `crates/shell/tests/forge_pr.rs`.
- [ ] Implementasikan endpoint list PR/MR, create PR/MR, dan fetch/checkout ref PR/MR (`refs/pull/<n>/head` / `refs/merge-requests/<n>/head`) melalui git CLI dengan expected-ref guard.
- [ ] Bangun panel daftar PR/MR & dialog Create PR/MR di `crates/ui/` dan `crates/app/` (terintegrasi setelah push di `crates/app/src/review_view.rs`) + Windows checklist di Handoff notes.

## Definition of Done
- `cargo xtask check --core` (VPS/CI) atau `cargo xtask check` (Windows) lulus.
- `cargo test -p shell --test forge_pr` lulus untuk GitHub PR & GitLab MR (list, checkout ref, create, penolakan saat working tree dirty / tanpa upstream).
- Checkout PR/MR tidak memindahkan branch lain (Law 9) dan langsung memperbarui state repo lewat watcher/fresh read (Law 11).

## Out of scope
- Issue boards, project management, dan fitur kolaborasi non-git (non-goal di `docs/PRD.md`).

## Handoff notes
- Gunakan akun forge yang cocok dengan host remote aktif dari `P8`; tampilkan pesan jelas jika remote belum terhubung ke akun forge.
