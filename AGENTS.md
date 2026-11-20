# Invoke Git — agent guide

Invoke Git is a fast, native desktop git client (a GitKraken alternative without the bloat) with a first-class review step before every push. Rust end to end, UI in GPUI. Windows first; macOS and Linux builds are on hold, but core tests run on all three.

## Source of truth

Read only the docs for your task areas:

- Diff or staging → `docs/spec/diff-engine.md`
- Graph or history → `docs/spec/commit-graph.md`
- State, shell, undo or errors → `docs/spec/core-design.md`
- Test changes → `docs/spec/verification.md`
- UI → `docs/design/tokens.json` and the matching `docs/design/screens/*.dc.html`

Always read your task file and its cited PRD laws. For doc/code disagreements, follow Handoff.

| Doc | What it decides |
| --- | --- |
| `docs/PRD.md` | Scope, goals, non-goals, laws, performance budgets, decisions |
| `docs/PLAN.md` | Phases, the Phase 0 spike checklist, the v0.1 backlog in dependency order |
| `docs/spec/core-design.md` | Crates, `State` / `Event` / `Command`, repo-state detection, shell, undo, errors |
| `docs/spec/diff-engine.md` | Diff algorithm, data model, word diff, line staging, highlighting |
| `docs/spec/commit-graph.md` | Paging, lane assignment, rendering, edge cases |
| `docs/spec/verification.md` | Laws → tests, model-based harness, Quint, Kani, CI matrix |
| `docs/design/tokens.json`, `docs/design/README.md` | Colours, type, spacing, radii, sizes and their usage rules |
| `docs/design/screens/*.dc.html` | Reference mockups for History, Changes, Pre-push review, Conflict |

## Architecture rules (do not break)

1. **Pure core.** `core`, `diff`, `graph`, `highlight` do no IO: no filesystem, network, clock, threads or randomness. Logic is `step(state, event) -> (state, commands)`.
2. **IO only in `shell`.** It runs commands and turns every outcome into an event. No business logic there.
3. **Reads through `gix`, writes and network through the system git CLI** (≥ 2.40). Never implement push, fetch, merge or rebase on gix.
4. **Disk is the source of truth.** A watcher event triggers a fresh read; never patch state from memory.
5. **Every write carries what it expects to find** (index id, ref value) and is refused if disk changed.
6. **Phases are enums, not booleans.** Make impossible states unrepresentable.
7. **UI only renders state** and sends events. Colours, sizes and spacing come from the `tokens` crate, never hard-coded.
8. **GPUI stays pinned** to the revision recorded in `README.md`. Update it only when asked, in its own commit.

## Laws and tests

- The 14 laws in `docs/PRD.md` are the contract. Each has one owning test (see `docs/spec/verification.md`).
- New logic in a pure crate gets property tests (`proptest`). Prove each new test can fail by breaking the code once.
- Tests use only generated fixture repos in temp folders and never touch the user's real repositories. Normal git (commit, branch, worktree) in this project's own checkout is fine.
- Benchmarks (`criterion`) guard the performance budgets; a regression past budget is a failure.

## Working style

- You run on a headless Linux VPS. You cannot open windows or see the UI. Never build GPUI crates here.
- Before calling a task done, run `cargo xtask check`.
- While iterating, use `cargo check -p <crate>` or `cargo test -p <crate>`.
- Do not add dependencies beyond the PRD's build-vs-library table without asking.
- Windows matters most: watch for path length, case-insensitive paths, CRLF and file locking.

## Handoff

- Do only the task in `tasks/NNN.md` and say its backlog number.
- Write `tasks/NNN.DONE.md` with what changed, what is unfinished and anything surprising.
- For spec gaps or doc/code disagreements, record the question and proposed answer in DONE.md. Finish unblocked work, commit and stop. Do not guess silently.
- UI tasks end at compile-ready plus a Windows checklist in DONE.md. Include exact commands and checks. Never claim a UI change works.

## Out of scope (non-goals)

No account or login wall, no cloud features, no telemetry, no AI features, no issue boards, no code editor. A terminal panel may come later; do not build it now.

## Commits

Conventional, imperative subject under 72 characters (`diff: add histogram fallback to Myers`), body explaining why. Reference the backlog task number.
