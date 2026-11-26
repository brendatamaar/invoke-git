# P12a - Daily-driver UI and UX overhaul
Status: review
Depends on: P1b, P3, P4b, P5, P6, P7, P9, P10, P11
Parallel-safe with: -

## Goal

Make a daily Git workflow discoverable from the repository tab on Windows. Give the working tree, history, and review their own space and make stalled operations easy to resume.

## Tasks

- [x] Separate Changes and History. Surface repository, worktree, branch, upstream and sync status.
- [x] Clarify staged and unstaged file actions, diff scope, and commit composition.
- [x] Clarify outgoing review progress, note navigation, final push target, and next actions.
- [x] Improve recovery feedback and bring History search and Recovery into the view header.
- [x] Clarify sidebar grouping, view-specific search, commands, keyboard focus, and controls at Windows display scaling.
- [x] Restore worktree layout and commit drafts, with a layout reset.
- [x] Clarify conflict resolution, history-changing actions, and the pull request inspector.

## Definition of Done

- Run `cargo xtask check` and `cargo build -p app` on Windows.
- Open a generated fixture in an isolated Windows profile. Exercise Changes → Commit → Review & push, History search by path, returning from blame, and resolving a paused rebase. Check keyboard focus, readable controls and pane widths at 100%, 125%, and 150%.

## Out of scope

The two-week dogfood and public release belong to P12.

## Handoff notes

- The pre-existing Windows UI edits are in checkpoint `8ad9f2f`. The overhaul is in `d39b766` on `ux/p12a-daily-driver`.
- The three T3 workstreams exported their patches before reaching provider limits. The parent integrated and verified them on Windows.
- Windows `cargo xtask check` passed with `INVOKE_HARNESS_CASES=16`. The final Windows build, focused Clippy, and app, UI, and history tests passed. Native symlink cases in the workspace suite used its existing Windows privilege skip. The native smoke test used an off-screen window, a generated repo, an isolated profile and a local bare remote. It passed at 1150 × 760 and 960 × 660 physical pixels. The smaller run reported 192 DPI (200% Windows scaling). It staged and committed a change, reviewed and pushed two commits, searched History by file path, returned from blame, resolved a conflict and continued a rebase, then restored the view and draft after restarting.
- The verified build is in `target/p12a/debug/invoke-git.exe`. Its separate output directory let the existing Windows instance keep running.
- Manual visual review at 100%, 125%, and 150% Windows scaling is pending.
