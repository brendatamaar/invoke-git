# Spec — Verification

## Laws mapped to tests

Each of the PRD's 14 laws has exactly one owning test; a law is done when its test runs in CI and has caught at least one planted bug.

| Law | Property | Test kind | Module |
| --- | --- | --- | --- |
| 1 | `apply(diff(a, b), a) == b` | proptest, random texts | diff |
| 2 | `diff(a, a)` is empty | proptest | diff |
| 3 | Hunk line numbers map to real lines | proptest | diff |
| 4 | Staging never changes the worktree file | model-based, real git | core + shell |
| 5 | Staging every hunk makes index = worktree | model-based, real git | core + shell |
| 6 | Stage then unstage restores the index | model-based, real git | core + shell |
| 7 | Discard changes only selected lines | proptest + real git | diff + shell |
| 8 | Undo restores refs and index exactly | model-based, real git | core + shell |
| 9 | No untargeted branch moves | model-based, real git | core |
| 10 | Force push only with lease on last fetched value | unit test on command building + integration test with a local bare remote | shell |
| 11 | App state = state derived from disk | model-based, with external git commands mixed in | core + shell |
| 12 | No write while another process holds `index.lock` | integration test + Quint spec | shell |
| 13 | Cancel leaves repo unchanged or resumable | integration test, cancel at random points | shell |
| 14 | Every commit placed once, edges point down | proptest, random DAGs | graph |

"Caught a planted bug" means: break the code on purpose (for example drop the end-of-file marker in patch building) and confirm the test fails. This proves each test can fail.

## Model-based test harness

The harness generates random sequences of user and outside actions, runs each on Invoke Git's core + shell and on plain git CLI in a twin repo, and compares the two after every step.

**Actions generated** (`proptest-state-machine`)

- Edit, create, delete or rename files in the worktree (random content, CRLF and unicode included)
- Stage / unstage / discard a random selection (file, hunk or lines)
- Commit, amend, create / switch / delete branch, stash save / pop
- Undo the last operation (on the twin, undo restores a copy of the twin's refs and index saved before that step)
- Cherry-pick and revert commits, resolving and continuing or aborting when Git pauses
- Interactive rebase of linear commit ranges, including reordered commits, squash, fixup, edit and drop. Compare every Git pause, drive continue/skip/abort, and undo successful rebases
- "Outside" actions run by plain git or the filesystem behind the app's back: `git commit -am`, `git checkout`, holding `index.lock` for a while

**After every step, compare**

- Index tree id, HEAD, every branch ref, worktree file contents (app repo vs twin repo)
- The app's `RepoState` against a fresh state read from disk (law 11)
- That every refused action was refused for a stated reason (precondition changed, lock held), not silently dropped

**Shrinking:** when a sequence fails, proptest shrinks it to the shortest reproducing sequence, and the harness writes it out as a plain list of git commands so the bug can be replayed by hand.

**Runtime budget:** 256 sequences of up to 40 steps on every CI run; a nightly job runs 10,000 sequences.

## Quint spec scope

Quint models only the concurrent parts, before they are coded; each spec stays under a few hundred lines and is model-checked in CI.

**Spec 1 — refresh vs user action**

- Actors: the user (stage, commit), the file watcher, an outside git process, the shell's write queue
- State: disk (index, refs, lock), the app's snapshot, queued commands with their expected values
- Invariants: a write never applies when its expected value differs from disk; the app's snapshot equals disk whenever no event is in flight (laws 11, 12)

**Spec 2 — background fetch vs local writes**

- Actors: a background fetch, the user committing and pushing
- Invariants: fetch never moves a local branch; push always leases against the remote value the user reviewed (law 10)

**Spec 3 — cancel**

- Actors: a running write, a cancel at any step
- Invariant: after cancel the repo is either unchanged or in git's own resumable state (law 13)

The Rust code mirrors the spec's actions by name, so a reviewer can line them up.

## Kani targets

Kani proves a small set of pure functions correct for all inputs up to a bound; it is added after v0.1 and limited to code where a bug corrupts data.

| Function | Property proved | Bound |
| --- | --- | --- |
| Hunk header recount (line staging step 3) | Header lengths always equal the counted `+`, `-` and context lines | Hunks up to 8 lines |
| Selection → patch rewrite | Output patch applied to the source gives exactly the selected change | Files up to 6 lines, any selection |
| End-of-file marker handling | `Eol::None` survives stage, unstage and discard | Files up to 4 lines |
| Lane placement step | No lane holds two commits; every parent of a placed commit stays waited for by at least one lane until it is placed | Up to 6 commits, 3 parents each |

These complement the proptests: proptest searches widely, Kani covers every case inside the bound.

## Fixtures and CI

Tests run against generated repos for correctness and real repos for speed; Windows is the primary platform, Linux and macOS run the core only.

**Generated fixtures** (built by a script in the repo, cached in CI)

- Deep history: 1M linear commits; wide: 200 branches with many merges, including octopus merges
- Wide tree: 100k files across 5k folders
- Content variety: CRLF and LF files, no trailing newline, unicode and emoji paths, very long paths (Windows), binary and lock files
- Shapes: shallow clone, multiple roots, submodule, a repo mid-rebase and mid-merge

**Real repos for benchmarks:** a pinned revision of the Linux kernel clone, used by the `criterion` benchmarks against the PRD's performance budgets.

| Job | Runner | Runs | When |
| --- | --- | --- | --- |
| Build + full tests + benchmarks | Windows | Everything, including UI smoke tests | Every push |
| Core tests | Linux, macOS | Diff, graph, core, shell, model-based harness | Every push |
| Quint model checking | Linux | Specs 1–3 | When a spec or the shell changes |
| Long model-based run | Linux | 10,000 sequences | Nightly |
| Kani | Linux | Kani targets | Nightly, from v0.2 |
