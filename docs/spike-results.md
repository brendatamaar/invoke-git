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

The gate runs in P0-4. Below are only the agent's preliminary runs.

### Prototype A — preliminary (P0-2, agent runs)

**These do not count for the gate:** the laptop was **on battery** (37 %, against the plugged-in rule), at 60 Hz, with no human input. Scrolling came from `spike-diff --sweep N`, which moves the scroll offset at constant speed from top to bottom after both parses finish.

| | Value |
| --- | --- |
| Rows | 50,290 (50,000 old lines, 50,104 new lines; +1,709 −1,605 by histogram) |
| Read both files | 3–5 ms warm (347 ms on the very first, cold run) |
| After data ready: index / diff / rows | 10–15 / 21–34 / 1–2 ms |
| First paint after data ready | 79, 100, 101, 115 ms (four runs) |
| Tree-sitter parse, full file | old 730–1,160 ms, new 880–1,440 ms (both sides at once on background threads); 700 / 800 ms when run alone |
| Idle (no scrolling) | 59.2–60.2 fps, build p95 7–11 ms |
| Sweep 20 s (~42 new rows per frame) | 58.9–60.0 fps, build p95 16.4–18.6 ms, worst interval 34.8 ms, 0 frames > 50 ms, 0 blank rows |
| Sweep 60 s (~14 new rows per frame) | 58.2–60.1 fps, build p95 11.0–18.5 ms, worst interval 36.9 ms, 0 frames > 50 ms, 0 blank rows |
| Peak working set | about 200–235 MB |

How to read these numbers:

- **The frame-interval p95 is inflated by paint-time jitter**, not by dropped frames. At idle it reads 22–25 ms while the window still paints 60 frames per second. GPUI's Windows backend paces frames from a `DwmFlush` vsync thread and presents with `Present(0)`, so in-app timestamps cannot see when a frame actually reaches the screen. See the open question in `tasks/P0-2.DONE.md`.
- **Build time is the CPU work** from the root view's render to the end of paint. It barely depends on how many rows are new each frame (the fast and slow sweeps overlap), so the cost is per-frame layout and paint of about 45 visible rows built from nested divs, not text shaping.
- "Unstyled" rows are rows painted before their highlight chunk was ready. They appear at startup while the parse runs (about 1 s), then 87 during a whole sweep.

## Risks and GPUI notes

- GPUI's git dependency needs Zed's `async-task` `[patch]` mirrored in our root `Cargo.toml` (see `README.md`). Every GPUI bump must re-check Zed's `[patch.crates-io]` section.
- GPUI core has no scrollbar at this revision (Zed's `ui` crate has one). The spike draws its own: a track, a thumb, and window-level mouse listeners while dragging. About 80 lines, and not hard.
- `uniform_list` handles wheel scrolling and virtualization well. Its `UniformListScrollHandle` exposes the offset, so a custom scrollbar and programmatic scrolling were simple.
- Building each diff cell from nested divs costs about 11–18 ms of CPU per frame for about 45 rows on this laptop on battery. That is close to the 16.7 ms frame budget. The PRD already plans own GPUI elements for the diff viewer; a custom element that shapes and paints lines directly (as Zed's editor does) is the obvious next step if P0-4 shows dropped frames.
- Tree-sitter parses 1.5 MB of Rust in about 0.7 s on this CPU, far above the spec's 100 ms fallback threshold (see `tasks/P0-2.DONE.md`).
