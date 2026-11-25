# P10 - Worktrees, Submodules & Git LFS
Status: done
Depends on: P1
Parallel-safe with: P4b, P5, P6, P7, P8, P9, P11

## Goal
Menyediakan dukungan operasional untuk git worktrees (list, add, switch/open in tab, remove), submodules (status, init, update, sync), dan Git LFS (deteksi pointer, pull/status dasar, tidak merusak diff/staging).

## Tasks
- [x] Tulis integration test fixture repo untuk worktrees, submodules, dan deteksi Git LFS di `crates/shell/tests/advanced_repos.rs` serta property test state di `crates/core/tests/advanced_props.rs`.
- [x] Tambah deteksi & pembacaan worktree list, submodule status, dan LFS status/pointer di `crates/shell/src/` dan `crates/core/src/state.rs`.
- [x] Tambah perintah CLI untuk `git worktree add/remove`, `git submodule update --init --recursive`, dan `git lfs pull` (bila `git-lfs` terpasang) dengan progress/cancel & lock guard di `crates/shell/src/git.rs`.
- [x] Tambah section Worktrees & Submodules di sidebar (`crates/ui/src/sidebar.rs`, `crates/app/src/branch_panel.rs`) serta indikator file LFS di diff viewer + Windows checklist di Handoff notes.

## Definition of Done
- `cargo xtask check --core` (VPS/CI) atau `cargo xtask check` (Windows) lulus.
- `cargo test -p invoke-core --test advanced_props` dan `cargo test -p shell --test advanced_repos` lulus untuk worktree, submodule, dan deteksi LFS (termasuk saat `git-lfs` tidak terpasang).
- Membuka linked worktree mengikuti `.git` file pointer dengan benar tanpa merusak tab repo utama.

## Out of scope
- Partial clone dan sparse checkout UI (masuk scope *Later* di `docs/PRD.md`).

## Handoff notes
- Catatan Windows: path worktree dan submodule rawan panjang path dan file lock; uji juga kasus `.git` file yang menunjuk ke `../.git/worktrees/<name>`.
- Task 1 selesai: fixture integration tests (`crates/shell/tests/advanced_repos.rs`) dan property test awal (`crates/core/tests/advanced_props.rs`) ditambahkan untuk linked worktree (`.git` relatif `../.git/worktrees/<name>`), submodule, dan file pointer Git LFS tanpa `git-lfs`.
- Task 2 selesai: `AdvancedSnapshot` (worktrees, submodules, LFS) ditambahkan ke `DiskState` (`crates/core/src/advanced.rs`, `state.rs`, `watch.rs`) dan pembacaan read-only berbasis `gix`/file ditambahkan di `crates/shell/src/advanced.rs`, `read.rs`, `precondition.rs`, `staging.rs` (termasuk fallback status/diff saat `filter.lfs.required=true` tetapi `git-lfs` tidak terpasang).
- Task 3 selesai: planner & event core (`WorktreeAddRequested`, `WorktreeRemoveRequested`, `SubmoduleRequested`, `LfsPullRequested`) dan CLI runner (`git worktree add/remove`, `git submodule init/update --init --recursive/sync --recursive`, `git lfs pull`) ditambahkan dengan expected-state guard, `index.lock` guard, progress/cancel, serta fallback `ErrorClass::Environment` saat `git-lfs` tidak terpasang. Proptest `invalid_lfs_oid_is_rejected` terbukti menangkap planted bug.
- Task 4 selesai: section `WORKTREES` (Open di tab baru / Remove) dan `SUBMODULES` (Update / Sync + badge status) ditambahkan di `crates/ui/src/sidebar.rs` dan `crates/app/src/branch_panel.rs`; event buka tab worktree disambungkan lewat `repo_view.rs` -> `workspace.rs`; indikator file LFS (pill `LFS` + banner ukuran/SHA-256 + tombol `Pull LFS`) ditambahkan di `crates/ui/src/diff_view.rs` & `crates/app/src/diff_load.rs`; label progress/cancel ditambahkan di `crates/app/src/sync.rs`.
- Checklist verifikasi manual Windows (UI):
  - [ ] Buka repo utama, klik `+ Worktree`, buat linked worktree baru dengan branch baru; pastikan muncul di section `WORKTREES` sidebar.
  - [ ] Klik `Open` (atau double-click) pada linked worktree di sidebar (termasuk worktree dengan `.git` relatif `../.git/worktrees/<name>`); pastikan terbuka di tab terpisah tanpa merusak atau mengganti tab repo utama, dan masing-masing tab menampilkan branch/status sendiri.
  - [ ] Klik `Remove` pada linked worktree non-aktif (uji normal & force pada worktree locked/modified); pastikan dialog konfirmasi muncul dan worktree terhapus dari disk serta sidebar.
  - [ ] Buka repo dengan submodule (uninitialized, clean, dirty, out-of-sync); verifikasi badge status di section `SUBMODULES`, klik `Update` dan `Sync`, cek progress bar + tombol `Cancel` di strip atas.
  - [ ] Buka repo dengan file pointer Git LFS di halaman Changes; verifikasi pill `LFS` dan banner ukuran/SHA-256 tampil di diff viewer, staging/unstaging pointer tetap berfungsi, dan tombol `Pull LFS` berjalan saat `git-lfs` terpasang (serta menampilkan error Environment yang jelas saat `git-lfs` tidak tersedia).
  - [ ] Cek tema light & dark memastikan seluruh badge/warna/spacing berasal dari `crates/tokens`.
- Cross review fixes (Sol #1–#12):
  - Submodule update forces `--checkout`, refuses dirty or branch-checked-out submodules in core + fresh shell guard (Law 9), and wraps per-submodule paths with `:(literal)` (#1, #8).
  - Worktree removal guards the target worktree's own `index.lock`, `HEAD`, and `index`, never injects `--force` from stale `prunable` (prunes only when missing on disk at run time), refuses non-forced removal when ignored/untracked/modified files exist (e.g. `.env`), defaults Force remove to `false`, and uses `SPACE_2`/`RADIUS_SM` tokens for the sidebar worktree dot (#2, #3, #4, #5, #12).
  - LFS staging refuses raw content when the LFS clean filter is missing/failing, `git lfs version` runs via cancellable `run_child` with a fresh precondition re-check before `git lfs pull`, per-file `--include` refuses comma/wildcard paths, pointer parser enforces Git LFS v1 key ordering and `ext-<priority>-<name> <method>:<hex>` grammar, and missing-LFS tests run unconditionally via a deterministic wrapper (#6, #7, #9, #10, #11).
  - Re-check follow-ups (Opus #1–#4): added `GIT_OPTIONAL_LOCKS=0` to the pre-remove worktree status probe so stat-dirty files never rewrite the target index, replaced global `git worktree prune` with single-target `git worktree remove` for missing worktrees, refused submodule updates when submodule open/HEAD/status reads fail instead of treating them as clean, and re-ran `precondition::check` right before spawning `git submodule update`.
