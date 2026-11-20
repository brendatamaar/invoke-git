# Phase 0 spike results

Results and notes for the GPUI spike in [the plan](PLAN.md#phase-0--gpui-spike). Measurements go here as each prototype is run.

## Fixtures

### Linux kernel clone (graph gate)

| | |
| --- | --- |
| Path | `C:\dev\fixtures\linux` (outside this repo; read-only) |
| Source | `https://github.com/torvalds/linux.git`, `git clone --filter=blob:none` |
| HEAD | `65e4fd942f72a9dba0e4e257290c4fa1aa756854` |
| HEAD date | 2026-11-19T14:05:24-08:00 (committer and author) |
| HEAD subject | Merge tag 'selinux-pr-20261119' of git://git.kernel.org/pub/scm/linux/kernel/git/pcmoore/selinux |
| Commits reachable from HEAD | 1,507,472 |
| Commit-graph | `git commit-graph write --reachable` on 2026-11-20 (21 s, 90 MB file, `git commit-graph verify` passes) |
| `.git` size | 2.5 GB |

The graph spike walks from `HEAD` only. Do not fetch, check out or otherwise write to this clone; if it must move, re-record the HEAD above.

On Windows the checkout reports 13 files as modified. The kernel has path pairs that differ only by case (for example `net/netfilter/xt_TCPMSS.c` and `xt_tcpmss.c`), and NTFS is case-insensitive, so one file of each pair overwrites the other. This does not affect commit reads. It is a real case for Invoke Git's status view later.

### Diff fixtures (diff gate)

`cargo xtask fixtures` writes `fixtures/diff/old.rs` and `fixtures/diff/new.rs` (gitignored). Generation is deterministic, so every machine gets identical bytes.

| | |
| --- | --- |
| `old.rs` | 50,000 lines of Rust-like code (structs, impls, match arms, strings, comments, numbers) |
| `new.rs` | 50,104 lines |
| Changes | 200 hunks of 10 lines, one per 250-line stretch: 1,314 modified, 291 deleted, 395 inserted (2,000 changed) |
| `git diff --numstat` | 1,722 added, 1,618 removed |

## Results

Not measured yet. Prototype A and Prototype B come in later Phase 0 tasks.

## Risks and GPUI notes

- GPUI's git dependency needs Zed's `async-task` `[patch]` mirrored in our root `Cargo.toml` (see `README.md`). Every GPUI bump must re-check Zed's `[patch.crates-io]` section.
