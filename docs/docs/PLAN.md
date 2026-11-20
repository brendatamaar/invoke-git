# Invoke Git — Plan

Nov 20, 2026 · Paduka Raja

What gets built in what order: a GPUI spike first, then v0.1 as a daily driver on Windows. Scope and reasoning live in the PRD; how each part works lives in the Spec.

## Milestones

Four phases, Windows only, each closed by a gate you can check; no dates are set yet, so the phases are not to scale.

```mermaid
flowchart LR
  P0["Phase 0 — Spike<br/>1–2 weekends<br/>diff + graph prototypes"] --> V1["v0.1 — Daily driver<br/>staging, commit, sync,<br/>graph, review, undo"]
  V1 --> V2["v0.2 — Power user<br/>interactive rebase, cherry-pick,<br/>blame, history, search"]
  V2 --> V3["v0.3 — Forges<br/>GitHub, GitLab, worktrees,<br/>submodules, LFS"]
  P0 -.- G0{{"Gate: 60 fps on Windows,<br/>API feels workable"}}
  V1 -.- G1{{"Gate: two weeks of daily use,<br/>no GitKraken"}}
  V2 -.- G2{{"Gate: all laws pass for<br/>rebase and cherry-pick"}}
  V3 -.- G3{{"Gate: public open-source release"}}
```

A failed gate means the phase continues, not that the next one starts with debt.

## Phase 0 — GPUI spike

The spike answers one question in 1–2 weekends: can GPUI render a huge diff and a huge commit graph at full frame rate on Windows, with an API you can live with?

**Setup**

1. Create the Cargo workspace with two crates: `spike-diff` and `spike-graph` (throwaway; nothing here ships).
2. Pin GPUI to one snapshot (a git revision of the Zed repo or a `gpui-pre` version) in `Cargo.toml`; record the revision in the README.
3. Prepare fixtures: a generated 50k-line file and a modified copy (≈ 2k changed lines spread across it); a local clone of the Linux kernel; one large repo you use at work, if allowed.
4. Write the commit-graph file for the kernel clone (`git commit-graph write --reachable`).

**Prototype A — diff view**

- Load both 50k-line files, compute a line diff (any quick crate is fine here; the real engine comes later)
- Render split view in a virtualized list with line numbers and add/remove colouring
- Syntax-highlight visible lines with tree-sitter (Rust or TypeScript grammar)
- Scroll with the mouse wheel and by dragging the scrollbar end to end

**Prototype B — commit graph**

- Read commits from the kernel clone with `gix`, in pages of 5k
- Naive lane assignment (no optimisation yet) and draw lanes + dots with a custom GPUI element
- Columns: graph, subject, author, date; virtualized rows
- Scroll from the newest commit to 100k commits deep

**Measure**

- Frame time while scrolling (GPUI's own frame stats or a simple per-frame timer), on your Windows laptop, release build
- Time to first paint for each prototype
- Peak memory (Task Manager is enough)
- Your own notes: how hard was each thing to do in GPUI, and what was missing

**Pass/fail checklist**

- [ ] Diff view scrolls at 60 fps or better, no blank rows, release build
- [ ] Graph scrolls 100k commits deep at 60 fps or better, no blank rows
- [ ] First paint under 100 ms for the diff view once files are in memory
- [ ] Text input works for a commit message box (IME and selection behave)
- [ ] Custom drawing (lanes, highlight backgrounds) was doable without forking GPUI
- [ ] The API felt workable: you would choose to keep going

**Fallback:** if two or more boxes fail, switch to Tauri + React with the same Rust core. The spike's diff and graph code moves to the core unchanged; only the rendering is rewritten.

## v0.1 backlog

Twenty-six tasks take Invoke Git from an empty repo to a daily driver; rows are in dependency order, so working top to bottom never blocks.

| # | Task | Area | Depends on | Status |
| --- | --- | --- | --- | --- |
| 1 | Cargo workspace, CI (Windows build; core tests on Linux and macOS), license, README | Foundation | — | Not started |
| 2 | Design tokens crate and component layer skeleton over GPUI | UI | 1 | Not started |
| 3 | Core types: `State`, `Event`, `Command`, `step()` skeleton | Core | 1 | Not started |
| 4 | Shell: command runner, git CLI wrapper with progress and cancel | Shell | 3 | Not started |
| 5 | Repo discovery and repo-state detection from `.git` | Core | 3 | Not started |
| 6 | File watcher with debounce, emitting refresh events | Shell | 4 | Not started |
| 7 | App window, open and init repo, repo tabs | UI | 2, 5 | Not started |
| 8 | Diff engine: Myers + histogram, hunks, word-level diff | Diff | 1 | Not started |
| 9 | Syntax highlighting layer on tree-sitter | Diff | 8 | Not started |
| 10 | Diff viewer: split and unified, virtualized | UI | 2, 8, 9 | Not started |
| 11 | Status tree with file, hunk and line staging | Core + UI | 5, 10 | Not started |
| 12 | Undo system: reflog and index snapshots | Core | 5 | Not started |
| 13 | Discard file, hunk or lines, undoable | Core + UI | 11, 12 | Not started |
| 14 | Commit, amend, commit message editor | Core + UI | 11 | Not started |
| 15 | Branches: create, switch, rename, delete, detached HEAD | Core + UI | 5 | Not started |
| 16 | Clone with progress, cancel and in-app auth prompt (`GIT_ASKPASS`) | Shell + UI | 4 | Not started |
| 17 | Fetch, pull (merge or rebase), push with `--force-with-lease`, set upstream | Shell + UI | 4, 16 | Not started |
| 18 | Commit graph: lane layout, paging, renderer | Graph | 2 | Not started |
| 19 | Commit detail view with its diff | UI | 10, 18 | Not started |
| 20 | Stash: save (incl. untracked), apply, pop, drop | Core + UI | 5 | Not started |
| 21 | Pre-push review screen: outgoing commits, combined and per-commit diffs | UI | 10, 17 | Not started |
| 22 | Notes and annotations: SQLite store, anchoring, to-do warning on push | Core + UI | 21 | Not started |
| 23 | Conflict resolution UI with continue and abort | Core + UI | 10, 17 | Not started |
| 24 | Laws as `proptest` tests and the model-based test harness, in CI | Verification | 3, 11, 12 | Not started |
| 25 | Benchmarks with performance budgets, failing CI on regression | Verification | 10, 11, 18 | Not started |
| 26 | Two-week dogfood: all git work in Invoke Git (the v0.1 gate) | Release | 1–25 | Not started |

## After v0.1

The next two releases get their own backlog tables once v0.1 ships; until then this is the outline.

**v0.2 — Power user** (gate: all laws pass under model-based tests for rebase and cherry-pick)

- Interactive rebase editor: reorder by drag, squash, fixup, edit, drop
- Cherry-pick and revert, with the conflict flow from v0.1
- Tags: create, push, delete
- Blame and file history
- Commit search by message, author, hash and path
- Reflog view

**v0.3 — Forges and advanced repos** (gate: public open-source release)

- GitHub, GitLab and self-hosted GitLab accounts; clone from a repo list
- List and check out PRs/MRs; create a PR/MR from the current branch
- Worktrees, submodules, Git LFS
- Bundled MinGit fallback on Windows
