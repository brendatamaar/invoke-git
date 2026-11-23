# Invoke Git — agent guide

Fast, native desktop git client with a first-class pre-push review step. **Stack:** Rust (edition 2024), GPUI (pinned in `README.md`), `gix` (reads), system `git` CLI ≥ 2.40 (writes/network/hooks), tree-sitter, `notify`, `rusqlite` (bundled), `proptest`, `criterion`. Windows first; macOS/Linux builds on hold, core tests run on all three.

## Source of truth

Read only the docs for your phase area (`tasks/` and `docs/PLAN-v0.1-archive.md` are historical archives):
- Scope, 14 laws, perf budgets, build-vs-library → `docs/PRD.md`
- Diff/staging → `docs/spec/diff-engine.md` | Graph/history → `docs/spec/commit-graph.md`
- State/shell/undo/errors → `docs/spec/core-design.md` | Tests/laws → `docs/spec/verification.md`
- UI → `docs/design/tokens.json`, `docs/design/README.md`, `docs/design/screens/*.dc.html`

## Architecture rules (do not break)

1. **Pure core.** `core`, `diff`, `graph`, `highlight` do no IO (no fs, network, clock, threads, randomness); logic is `step(state, event) -> (state, commands)`.
2. **IO only in `shell`** (no business logic). Reads through `gix`; writes, network, and hooks through system `git` CLI (≥ 2.40).
3. **Disk is source of truth.** Watcher events trigger a fresh read; never patch state from memory.
4. **Expected-state guards.** Every write carries expected `index` id / ref value and refuses if disk changed or `index.lock` is held.
5. **Phases are enums, not booleans.** Make impossible states unrepresentable.
6. **UI only renders state** and sends events. Colours, sizes, and spacing come from `crates/tokens`, never hard-coded.
7. **GPUI stays pinned** to the revision in `README.md`; update only when asked, in its own commit.

## Commands (build / lint / test / bench)

- Iterate: `cargo check -p <crate>` | `cargo test -p <crate>`
- Full verification (fmt, clippy `-D warnings`, tests): `cargo xtask check` (auto-skips UI on headless Linux) or `cargo xtask check --core`
- Performance budgets: `cargo xtask bench-check` (core) | `cargo xtask bench-check --ui` (Windows)

## Laws, environment & conventions

- **Laws & tests:** Each of the 14 PRD laws has one owning test (`docs/spec/verification.md`). New pure logic gets `proptest` (prove each test can fail once). Tests use generated temp fixtures only and never touch real user repos; normal git in this checkout is fine. Benchmark regressions past `docs/perf-budgets.toml` fail CI.
- **Dependencies:** Do not add crates beyond the `docs/PRD.md` build-vs-library table without asking.
- **Headless VPS vs Windows:** On headless Linux (no display), never build or run `ui`, `app`, or `spike-*`, and never claim a UI change works; verify core DoD with `cargo xtask check --core` and put a Windows verification checklist in the phase's `Handoff notes`. On Windows, watch for path length, case-insensitive paths, CRLF, and file locking. One machine owns a branch at a time.
- **Spec gaps:** Record the question and proposed answer in the phase's `Handoff notes`, finish unblocked work, commit, and stop; do not guess silently.
- **Non-goals:** No login wall, cloud sync, telemetry, AI features, issue boards, code editor, or embedded terminal.
- **Commits:** Conventional, imperative subject < 72 chars (`diff: add histogram fallback to Myers`), body explaining why, reference the phase ID (e.g. `P1b`).

<!-- repo-playbook:start (dikelola oleh repo-playbook/install.sh; jangan edit manual) -->
## Workflow per fase

- Sumber kebenaran: `docs/PLAN.md` (status fase), detail fase di PLAN.md atau `docs/phases/`, keputusan di `docs/DECISIONS.md`. Format: `.agents/playbook/formats.md`.
- Skill tersedia: phase-setup-new, phase-setup-existing, phase-start, phase-resume, phase-status, phase-routing. Jika skill tidak termuat otomatis, baca `.agents/skills/<nama>/SKILL.md` lalu ikuti isinya.
- Sebelum kerja: baca PLAN.md + detail fase yang ditugaskan. Kerjakan HANYA scope fase; hal di luar scope dicatat di Handoff notes.
- Selesai = DoD fase lulus, dijalankan sendiri. DoD gagal 3x berturut-turut: berhenti dan laporkan.
- Commit kecil. Update Handoff notes di tengah & akhir fase, dan status di PLAN.md.
- Keputusan teknis baru: tambah 1 entri di DECISIONS.md.
- Delegasi ke provider lain dan fallback saat limit: ikuti skill phase-routing (`.agents/playbook/routing.md`).
- Laporan ringkas; jangan dump file atau log panjang.
<!-- repo-playbook:end -->
