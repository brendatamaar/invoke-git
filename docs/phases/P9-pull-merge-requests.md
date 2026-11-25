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
  - **Tests:** `crates/shell/tests/forge_pr.rs` (8 tes integrasi: mock HTTP GitHub & GitLab list/detail/reviews/approvals/create/fork/scrubbing + real git CLI checkout `refs/pull/42/head` & `refs/merge-requests/7/head` memverifikasi Law 9, Law 11, Law 12, penolakan dirty worktree, branch existing, custom fetch refmap, dan file ignored).
- **Perbaikan cross-review ronde 1 (Sol, 2026-11-25):** Seluruh 4 blocker, 7 medium, dan 2 minor diperbaiki:
  - `runner.rs`/`git.rs`: `run_checkout_pr` memakai temp ref unik milik operasi (`refs/invoke-git/pr-checkout/op-<id>-<pid>`) dengan `--no-tags --no-prune --no-write-fetch-head --refmap=` dan RAII drop guard pembersih, verifikasi ulang `precondition::check` + `expected_refs` + kebersihan worktree (`read_state`) sesudah fetch, switch ke OID eksak dengan `--no-overwrite-ignore` serta deferred cancellation agar `post-checkout` hook tidak menghasilkan status `Cancelled` setelah branch berpindah (Law 9, 12, 13).
  - `forge/api.rs`/`service.rs`: `web_url` diikat ke origin forge yang diharapkan; `text_contains_secret` memeriksa teks mentah dan `pct_decode` serta membuang cursor `next` yang mengandung token; `create_pr` GitHub mengirim `head_repo` untuk fork same-org; `list_pr_reviews` mengikuti pagination `Link: rel="next"`; `get_mr_approvals` menghormati `approved: false`.
  - `core/forge.rs`/`shell/read.rs`: tambah `read_branch_upstream` (`branch.<name>.remote` & `branch.<name>.merge`) untuk remote bernama `team/fork` atau refspec kustom; fallback `resolve_remote_for_list` memilih remote yang valid terlebih dulu.
  - `app/src/pr_view.rs`: inisialisasi `open_service()` dipindah ke background thread; `create_gen` & perlindungan `CreateState::Submitting` mencegah double-POST; pemilih target remote tetap tampil saat `CrossHostMismatch`; tutup `Accounts` kembali ke `Create` bila dibuka dari sana; pesan 403 GitLab memberi petunjuk scope `api` + tombol `Reconnect…`; toolbar flex-wrap dengan lebar berbasis token.
- **Perbaikan cross-review ronde 2 (Sol, 2026-11-25):** Seluruh 2 blocker dan 5 medium diperbaiki:
  - `runner.rs`: temp ref `refs/invoke-git/pr-checkout/op-<pid>-<seq>` memakai `AtomicU64` global per proses (aman lintas linked worktrees); urutan pemeriksaan pasca-fetch diperketat: cek `read_state` & resolusi OID dulu, lalu tepat sebelum `git switch` cek ulang `cancelled()`, `precondition::check`, dan `precondition::check_refs`.
  - `app/src/pr_view.rs`: `configured_upstream` kini diikat ke nama branch lokal (`Option<(String, BranchUpstream)>`) sehingga pergantian branch tidak pernah memakai upstream branch lama.
  - `shell/src/forge/service.rs` & `api.rs`: `head_repo` GitHub mengirim nama repo saja (`head_repo_name`); GitLab `get_pr_detail` meneruskan status eksplisit `ReviewStatus::Pending` saat `approved: false`; origin `web_url` GitLab selalu diikat ke `account.origin` (termasuk loopback); `pct_decode_lossy` memindai token ter-encode meski ada escape `%` cacat di bagian lain URL/teks.
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
