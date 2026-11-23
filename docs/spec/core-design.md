# Spec — Core design

How Invoke Git works, written to code from. This file covers the core; `diff-engine.md`, `commit-graph.md` and `verification.md` cover the rest.

## Crate layout

Nine crates in one Cargo workspace; the dependency rule is that nothing under `core` may do IO, so `diff`, `graph`, `highlight` and `core` are testable without a disk.

```mermaid
flowchart TB
  app["app<br/>binary crate; wires ui, core, shell"]
  ui["ui<br/>GPUI views and component layer"]
  shell["shell<br/>gix, git CLI, notify, SQLite store, keyring"]
  tokens["tokens<br/>design tokens, JSON to Rust consts"]
  core["core<br/>pure step(), state, events, commands"]
  askpass["askpass<br/>tiny binary git runs for auth prompts"]
  diff["diff<br/>diff engine, patch building, staging"]
  graph["graph<br/>lane layout, row records"]
  highlight["highlight<br/>tree-sitter parse, theme mapping"]
  app --> ui
  app --> shell
  ui --> tokens
  ui --> core
  shell --> core
  shell -. named pipe .- askpass
  core --> diff
  core --> graph
  core --> highlight
```

Arrows point from a crate to what it depends on. `ui` also reads `diff`, `graph` and `highlight` output types to render them (not drawn). `askpass` is not linked into the app: git launches it, and it talks to `shell` over a named pipe.

## Core types

The core is one pure function per repo: `step(state, event) -> (state, commands)`. It never touches the disk, the network or the clock; the shell does, and reports back with events.

```rust
pub struct RepoState {
    pub id: RepoId,                 // root commit id + normalized remote URL
    pub head: Head,                 // Branch(name) | Detached(oid) | Unborn(name)
    pub phase: Phase,               // what git itself is in the middle of
    pub status: StatusSnapshot,     // index + worktree changes, versioned
    pub refs: RefsSnapshot,         // branches, tags, remotes, upstream ahead/behind
    pub ops: OpQueue,               // running and queued long operations
    pub undo: UndoStack,
    pub lock: LockState,            // Free | HeldByUs | HeldByOther
}

pub enum Phase {
    Clean,
    Merging { conflicts: Conflicts },
    Rebasing { step: u32, total: u32, conflicts: Conflicts },
    CherryPicking { conflicts: Conflicts },
    Reverting { conflicts: Conflicts },
}

pub enum Event {
    // from the UI
    StageRequested(StageRequest),   // selection (for labels) + the patch the UI built + the index id the diff was made against
    UnstageRequested(StageRequest),
    CommitRequested { message: String, amend: bool },
    PushRequested { force_with_lease: bool },
    OpCancelRequested(OpId),
    UndoRequested,
    // from the shell
    DiskChanged(ChangeSet),
    StateRead(DiskState),          // fresh read of .git + status
    OpProgress(OpId, Progress),
    OpFinished(OpId, OpResult),
    AuthNeeded(OpId, AuthPrompt),
}

pub enum Command {
    ReadState { scope: ReadScope },
    ApplyPatchToIndex { patch: Patch, expect_index: IndexId },
    RunGit { op: OpId, args: GitArgs },
    CancelOp(OpId),
    ShowAuthPrompt(OpId, AuthPrompt),
    SnapshotForUndo(UndoPoint),
}

pub fn step(state: RepoState, event: Event) -> (RepoState, Vec<Command>);
```

**Rules**

- Every write command carries what it expects to find (`expect_index`, the expected ref value), so the shell refuses a write when the disk changed underneath it. This enforces law 12 and the lease on force push.
- `DiskChanged` never updates state directly: it emits `ReadState`, and only `StateRead` replaces the snapshot (law 11).
- Phases are enums, not booleans, so impossible combinations (merging and rebasing at once) cannot be represented.
- Typestate is used at API edges where it pays off, for example a `PushPlan` can only be built from a phase of `Clean`.

## Repo state detection

State is read from `.git` in a fixed order, and the first matching rule decides the phase; the same function runs on open and after every watcher event.

| Order | Check (paths relative to the git dir) | Result |
| --- | --- | --- |
| 1 | `rebase-merge/` or `rebase-apply/` exists | `Rebasing`; step and total from `msgnum` and `end` (rebase-merge) or `next` and `last` (rebase-apply) |
| 2 | `MERGE_HEAD` exists | `Merging` |
| 3 | `CHERRY_PICK_HEAD` exists | `CherryPicking` |
| 4 | `REVERT_HEAD` exists | `Reverting` |
| 5 | none of the above | `Clean` (the worktree may still be dirty) |

- **Head:** `HEAD` holding `ref: refs/heads/x` is a branch; a bare object id is detached; a branch that does not exist yet is unborn.
- **Conflicts:** files with index stages 1–3, read through gix.
- **Lock:** `index.lock` present and not created by us → `HeldByOther`; all writes wait and the UI shows "another git process is running".
- **Worktrees:** resolve the git dir and common dir first (`.git` may be a file pointing elsewhere); in-progress markers live in the worktree's own git dir.
- **Watcher scope:** watch the worktree (respecting ignore rules) and the git dir; changes inside `objects/` are ignored, changes to `HEAD`, `index`, `refs/`, `packed-refs` and the in-progress markers trigger a read.
- **Debounce:** coalesce events for 50 ms after the last one, then issue one `ReadState` with the narrowest scope that covers the changes.

## Shell and command runner

The shell executes commands and turns every outcome into an event; it holds no business logic.

- **Reads** (status, refs, log pages, blobs) run on a background executor through gix and return `StateRead` or data events. Reads never take `index.lock`.
- **Writes and network** run the system git as a child process: `git -c core.quotepath=false -c color.ui=false <args>`, with `GIT_TERMINAL_PROMPT=0` so git never blocks on a hidden terminal.
- **Index writes from staging** go through `git apply --cached` with the generated patch, the same machinery `git add -p` uses, so whitespace and line-ending handling match the CLI.
- **Progress:** pass `--progress` and parse stderr lines (`Receiving objects: 45% (…)`) into `OpProgress`.
- **Cancel:** kill the child process tree; on Windows use a job object so grandchildren (ssh, credential helpers) die too. After a cancel the shell always issues `ReadState` (law 13).
- **Auth:** set `GIT_ASKPASS` and `SSH_ASKPASS` to a small helper binary shipped with the app. The helper connects back over a local named pipe, the core emits `AuthNeeded`, the UI shows the dialog, and the answer flows back to the helper. Nothing is written to disk.
- **Concurrency:** one write at a time per repo (a queue in `OpQueue`); reads run freely alongside.
- **Environment:** inherit the user's environment and git config; never pass `-c` options that change behaviour (only output formatting). Three additions, none of which changes what git does:
  - `GIT_TERMINAL_PROMPT=0`, so git never blocks on a hidden terminal.
  - `LANGUAGE=en`, so git's messages are English and the classifier can match them. It changes message text only (git has no `en` catalogue and prints its built-in English).
  - `GIT_EDITOR=true`, for the operations that would open an editor (continue a merge or rebase, `commit --no-edit` flows), so git never waits for one.
- **Git config in reads:** reads through gix honour the user's and the system's git config, like git itself (`core.autocrlf`, `core.excludesFile`, `.gitattributes` filters). The app switches this on at start-up (`shell::use_user_config`); tests leave it off so they do not depend on the machine. The text a diff shows for a worktree file is that file after the same to-git filters `git add` applies, so a CRLF checkout of an LF blob shows only its real edits; the blob id kept for discard is that of the file's raw bytes.
- **Stale lock files after a cancel:** terminating git hard (a Windows job object) can leave a `*.lock` it could not remove, which would look like "another git process is running" for ever. The shell never deletes a lock on its own. After a cancel, the core asks the user before removing a `*.lock` the cancelled operation could have created, and only when it is not newer than the kill.

## Undo design

Undo restores refs and the index to their exact values before an operation (law 8); it never rewrites working-tree files except to reverse a discard or a branch switch.

**What an undo point records** (taken by `SnapshotForUndo` before every write)

- Every ref the operation may move, with its old object id (HEAD, the current branch, branches named in the op)
- The index tree, written as a tree object (`git write-tree`) so it is kept in the object store
- For discards only: the discarded content, stored as blobs, plus file paths and modes
- The operation's label ("Commit 'Fix login'", "Discard 3 lines in auth.rs") for the UI

**Replaying an undo**

1. Check each recorded ref still points where the operation left it, and that `HEAD` is where the operation left it; if anything else moved them since, refuse and explain (never undo over someone else's change). This includes the index: an undo after the index changed for another reason (an outside `git add`, or a refresh that rewrote it) is refused, and the UI says the repo changed.
2. Restore refs with `git update-ref` using the old value as the expected value, so it is atomic against races. When the operation moved `HEAD` (switch, rename, create and switch), move `HEAD` back first.
3. Restore the index with `git read-tree` from the saved tree.
4. For a discard, write the saved blobs back to their paths.
5. Undoing a branch switch also puts the old branch's files back in the worktree (it is a checkout), because the user asked for the files to change.

**Scope and limits**

- The stack keeps the last 50 points per repo, in memory and in the app's SQLite store so it survives a restart (task 22). Undo refs are named by op id, so on start-up the stored points are checked against `refs/invoke-git/undo/*` (a point whose saved tree is gone is left out; refs of ops that are not kept are deleted), and the core continues numbering ops after the highest restored id. No new snapshot is taken before that check is done, and points are taken only while nothing has been recorded in the new run.
- Push is not undoable (it changed the remote); the UI says so in the push confirmation instead.
- Saved trees and blobs are referenced from `refs/invoke-git/undo/*` so `git gc` keeps them; the oldest are pruned with their points.

## Errors

Every failure is classified once, in the shell, and the core decides what the user sees; raw git stderr is shown only behind a "Details" toggle.

| Class | Examples | What the user sees |
| --- | --- | --- |
| Expected outcome | Push rejected (non-fast-forward), merge conflict, nothing to commit | A state change, not an error: the push flow offers fetch + rebase, the conflict flow opens |
| Precondition changed | Index or ref moved since we read it, `index.lock` held | "The repo changed" banner, state re-read, action can be retried |
| Auth | Bad credentials, unknown host key, expired token | Auth dialog again with the reason; never a silent retry loop |
| Environment | git missing or below 2.40, path too long on Windows | Blocking setup screen with the fix |
| Network | Timeout, DNS, TLS | Inline error on the operation with Retry |
| Bug | Unexpected git output, a broken invariant | Error card with a copyable report; the operation is aborted and state re-read |

- The core never panics on bad input from the shell; unparseable output becomes a `Bug`-class event.
- A broken law detected at runtime (for example a stage that changed the working tree) aborts the operation and logs everything needed to reproduce it.
