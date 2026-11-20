# Invoke Git — agent guide

Invoke Git is a fast, native desktop git client (a GitKraken alternative without the bloat) with a first-class review step before every push. Rust end to end, UI in GPUI. Windows first; macOS and Linux builds are on hold, but core tests run on all three.

## Source of truth

Read the relevant doc before changing anything. If code and docs disagree, stop and ask.

| Doc | What it decides |
| --- | --- |
| `docs/PRD.md` | Scope, goals, non-goals, laws, performance budgets, decisions |
| `docs/Plan.md` | Phases, the Phase 0 spike checklist, the v0.1 backlog in dependency order |
| `docs/spec/core-design.md` | Crates, `State` / `Event` / `Command`, repo-state detection, shell, undo, errors |
| `docs/spec/diff-engine.md` | Diff algorithm, data model, word diff, line staging, highlighting |
| `docs/spec/commit-graph.md` | Paging, lane assignment, rendering, edge cases |
| `docs/spec/verification.md` | Laws → tests, model-based harness, Quint, Kani, CI matrix |
| `design/tokens.json`, `design/README.md` | Colours, type, spacing, radii, sizes and their usage rules |
| `design/screens/*.dc.html` | Reference mockups for History, Changes, Pre-push review, Conflict |

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
- Tests touch only generated fixture repos in temp folders. **Never run git commands against the user's real repositories.**
- Benchmarks (`criterion`) guard the performance budgets; a regression past budget is a failure.

## Working style

- Work in small steps that compile and pass tests. Run `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and `cargo test` before calling a task done.
- One backlog task per change. Say which task number you are working on.
- If a spec gap blocks you, write down the question and your proposed answer, then ask. Do not invent behaviour silently.
- Do not add dependencies beyond the PRD's build-vs-library table without asking.
- Windows matters most: watch for path length, case-insensitive paths, CRLF and file locking.

## Out of scope (non-goals)

No account or login wall, no cloud features, no telemetry, no AI features, no issue boards, no code editor. A terminal panel may come later; do not build it now.

## Commits

Conventional, imperative subject under 72 characters (`diff: add histogram fallback to Myers`), body explaining why. Reference the backlog task number.
