# P9 - Pull Requests & Merge Requests
Status: review
Depends on: P8
Parallel-safe with: P4b, P5, P6, P7, P10, P11

## Goal
Memungkinkan user melihat daftar Pull Request (GitHub) dan Merge Request (GitLab / self-hosted) pada repo yang terhubung, men-checkout PR/MR secara lokal, serta membuat PR/MR baru dari branch aktif (termasuk tautan dari layar pre-push review).

## Tasks
- [x] Tulis unit/integration test mock HTTP untuk list PR/MR, detail & status review, pembuatan PR/MR, dan resolusi remote/branch target (termasuk fork) di `crates/shell/tests/forge_pr.rs`.
- [x] Implementasikan endpoint list PR/MR, create PR/MR, dan fetch/checkout ref PR/MR (`refs/pull/<n>/head` / `refs/merge-requests/<n>/head`) melalui git CLI dengan expected-ref guard.
- [x] Bangun panel daftar PR/MR & dialog Create PR/MR di `crates/ui/` dan `crates/app/` (terintegrasi setelah push di `crates/app/src/review_view.rs`) + Windows checklist di Handoff notes.

## Definition of Done
- `cargo xtask check --core` (VPS/CI) atau `cargo xtask check` (Windows) lulus.
- `cargo test -p shell --test forge_pr` lulus untuk GitHub PR & GitLab MR (list, checkout ref, create, penolakan saat working tree dirty / tanpa upstream).
- Checkout PR/MR tidak memindahkan branch lain (Law 9) dan langsung memperbarui state repo lewat watcher/fresh read (Law 11).

## Out of scope
- Issue boards, project management, dan fitur kolaborasi non-git (non-goal di `docs/PRD.md`).

## Handoff notes
- Gunakan akun forge yang cocok dengan host remote aktif dari `P8`; tampilkan pesan jelas jika remote belum terhubung ke akun forge.
- **Implementasi selesai (2026-11-25):**
  - **Core (`crates/core`):** `forge.rs` (`ConfiguredRemote`, `RemoteRepoRef`, `parse_remote_repo`, `PrState`, `PrFilter`, `ReviewStatus`, `PullRequest`, `PrListTarget`, `PrCreateTarget`, `PrTargetError`, `resolve_remote_for_list`, `resolve_create_target` dengan deteksi fork `origin` → `upstream`, `validate_create_pr`, `pr_fetch_ref`, `is_valid_pr_ref`, `default_pr_branch`, `filter_prs`); `ops.rs`/`event.rs`/`state.rs`/`step.rs` (`Event::CheckoutPrRequested`, `WriteRequest::CheckoutPr`, `GitArgs::CheckoutPr` dengan `expected_refs` = `[refs/heads/<local_branch>: None]`, `PrCheckoutRefusal::DirtyWorktree` / `BranchAlreadyExists` / `BranchNameConflict`).
  - **Shell (`crates/shell`):** `forge/json.rs` (`Json::encode` publik), `forge/http.rs` (`Http::post_json`), `forge/api.rs` (upgrade `GITLAB_SCOPE` dari `read_api` ke `api`; endpoint GitHub `list_prs_page`, `get_pr`, `list_pr_reviews`, `create_pr` dan GitLab `list_mrs_page`, `get_mr`, `get_mr_approvals`, `get_project_id`, `create_mr` termasuk fork `target_project_id` & prefix `Draft:`), `forge/service.rs` (`list_prs`, `get_pr_detail`, `create_pr` dengan refresh token sekali + scrubbing token di semua field PR), `read.rs` (`read_remotes` via `gix` tanpa lock), `git.rs`/`runner.rs` (`CheckoutPr` dua langkah: `git fetch --progress --no-tags -- <remote> <remote_ref>` → re-verifikasi `precondition::check` + `check_refs` → `git switch --no-track -c <local_branch> FETCH_HEAD`).
  - **Tests:** `crates/shell/tests/forge_pr.rs` (6 tes integrasi: mock HTTP GitHub & GitLab list/detail/reviews/approvals/create/fork/scrubbing + real git CLI checkout `refs/pull/42/head` & `refs/merge-requests/7/head` memverifikasi Law 9, Law 11, Law 12, penolakan dirty worktree & branch existing).
  - **UI (`crates/app`):** `pr_view.rs` (`PrPane`: daftar PR/MR, filter `Open | Closed | All`, pencarian, pemilih remote, detail + status review + "Checkout as pr/<n>" + "Open in browser", form Create PR/MR dengan dukungan fork & draft, overlay `AccountsView` jika host belum terhubung), integrasi tab `Pull Requests` di `repo_view.rs`, tombol `Create Pull / Merge Request` pasca-push di `review_view.rs`, serta pesan penolakan di `notice.rs` dan status progress di `sync.rs`.
- **Checklist verifikasi manual Windows:**
  1. Build `cargo build -p app -p askpass`, jalankan dengan profil bersih: `$env:INVOKE_GIT_DATA_DIR = "$env:TEMP\ig-p9"`.
  2. Buka repo tanpa akun forge terhubung → klik tab `Pull Requests`: tampil pesan jelas bahwa remote belum terhubung ke akun forge + tombol `Connect account…`; klik → sambungkan PAT GitHub (scope `repo`) atau GitLab (scope `api`).
  3. Setelah terhubung, daftar PR/MR muncul; uji filter `Open | Closed | All`, kotak pencarian, dan klik salah satu PR/MR untuk melihat detail, status review/approval, dan tombol `Open in browser`.
  4. Pastikan working tree kotor (ubah 1 file tanpa commit) → klik `Checkout as pr/<n>` → ditolak dengan banner jelas meminta commit/stash/discard dulu; tidak ada branch berubah.
  5. Bersihkan working tree → klik `Checkout as pr/<n>` → aplikasi fetch `refs/pull/<n>/head` (atau `refs/merge-requests/<n>/head`), membuat branch lokal baru `pr/<n>` / `mr/<n>`, switch ke sana, dan branch lain (`main`, dll.) tetap di commit sebelumnya; klik lagi pada PR yang sama → ditolak karena branch lokal sudah ada.
  6. Buat branch baru lokal tanpa push → buka `New Pull Request` → banner menolak karena branch belum punya upstream (minta push dulu).
  7. Push branch lewat layar pre-push Review (`Push` → `Review` → `Push`) → setelah push selesai (`Nothing to push`), tombol `Create Pull / Merge Request` muncul → klik → form terbuka dengan title & base branch terisi → buat PR/MR (uji juga checkbox `Create as draft`) → banner sukses + tombol `Open in browser` muncul dan PR/MR tampil di daftar.
  8. (Opsional fork) Pada repo dengan remote `origin` (fork) dan `upstream` (repo induk) di host forge yang sama, form Create PR/MR otomatis menargetkan `upstream` (`origin_owner/repo:branch -> upstream_owner/repo`).
