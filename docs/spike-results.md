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


**Same sweep on AC power** (P0-3 session, plugged in, 60 Hz): `spike-diff --sweep 20` ran at **60.0 fps in every 5 s window, build p95 4.7–4.9 ms, frame-interval p95 17.8–17.9 ms, worst frame 20.5 ms**, 0 frames over 50 ms, 0 blank rows. First paint was **39 ms** after data ready. **Battery power, not GPUI, caused the high build times and the interval jitter above.** The battery figures are kept only as a warning about the plugged-in rule.

### Prototype B — preliminary (P0-3, agent runs)

On AC power at 60 Hz, with no human input. The kernel clone is described above; the walk starts from HEAD only and stops at 200,000 commits (`--limit`). `spike-graph --sweep N` scrolls from the newest commit to 100,000 deep in N seconds.

| | Value |
| --- | --- |
| Walk + naive layout, first page (5,000 commits) | 76–78 ms (737 ms on the very first, cold run) |
| Walk + layout, 200,000 commits | 0.70–0.74 s, using the commit-graph file |
| First paint after first page ready | 27–45 ms |
| Widest row | 691 open lanes (drawn up to 16, with a "+N" marker) |
| Stored edges | 430,984 for 200k rows (only edges touching a dot; see the risk notes) |
| Idle | 60 fps, build p95 1.9–2.6 ms |
| Sweep 60 s (~28 new rows per frame) | 60.0 fps in every window, build p95 3.6–5.5 ms, worst frame 25.0 ms, 0 frames > 50 ms, 0 blank rows |
| Sweep 20 s (~83 new rows per frame), metadata read on the UI thread | first 5 s window: 41 fps, 14 frames > 50 ms, worst 183 ms (cold disk cache, 43 µs per commit read); then 60 fps |
| Sweep 20 s, metadata read ahead on a background thread | 60.0 fps in every window, build p95 3.5–3.7 ms, worst frame 21.1 ms, 0 frames > 50 ms, 0 blank rows, 0 reads on the UI thread (disk cache was warm) |
| Peak working set | 396 MB peak, 236 MB steady (was 773 MB when pass-through lanes were stored per row) |

## Gate run (P0-4)

Release builds, laptop plugged in (battery status "AC"), display at 60 Hz. The automated sweeps were run by the agent on 2026-11-20. The "agent-driven input" runs below were done on 2026-11-22 from `main` at 7c52a88. The IME and text-input checks, the 90 Hz run and the verdict still need the user (runbook in `tasks/P0-4.md`).

### Automated sweeps (agent)

| Sweep | fps (each 5 s window) | Frame interval p95 | Build p95 | Worst frame | Frames > 50 ms | Blank rows | First paint | Peak memory |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `spike-diff --sweep 20` (all 50,290 rows) | 60.0 | 17.7–17.9 ms | 4.7–4.9 ms | 23.8 ms | 0 | 0 | 37.7 ms | 206 MB |
| `spike-diff --sweep 60` | 60.0 | 17.4–17.8 ms | 3.2–3.8 ms | 19.5 ms | 0 | 0 | 54.2 ms | 212 MB |
| `spike-graph --sweep 20` (100k deep) | 60.0 | 17.4–17.6 ms | 3.3–4.3 ms | 20.1 ms | 0 | 0 | 36.6 ms | 506 MB |
| `spike-graph --sweep 60` (100k deep) | 60.0 | 17.3–17.9 ms | 3.2–3.8 ms | 21.9 ms | 0 | 0 | 35.0 ms | 507 MB |

Peak memory was sampled every 0.5 s from the process's peak working set. Unstyled rows in the diff sweeps (43) are rows shown before their highlight chunk arrived, all in the first frames.

### Agent-driven input runs (2026-11-22)

The agent drove the real release windows with synthetic Windows mouse input (`mouse_event`): a click on the readout to reset the stats, 10 s of wheel events (616 and 626 events of 8 notches each; the diff run scrolls back up every 4th second, the graph run only goes down), then three scrollbar drags from the top of the track to the bottom (150 mouse moves each, about 2.4 s per drag). Then a 6 s idle tail. The console stats were read from stdout. This is **not** a human run: no one watched the window, so stutter, flashes or half-drawn rows that the counters cannot see are not covered. Setup: laptop on AC (battery status 2, 9 % charge), 60 Hz, window 2800×1704 physical pixels (200 % scaling), `spike-diff` and `spike-graph` release builds from `main` at 7c52a88.

Every stats line after the reset (each is a 5 s window of about 300 frames; peak memory is the process peak working set, read at the end of the run):

| Run | Window | fps | Interval p95 / max (ms) | Build p95 / max (ms) | Unstyled rows (cumulative) |
| --- | --- | --- | --- | --- | --- |
| A. Diff | 1 (wheel) | 60.0 | 18.0 / 20.7 | 3.7 / 4.3 | 0 |
| A. Diff | 2 (wheel) | 60.0 | 17.8 / 19.9 | 3.7 / 4.7 | 0 |
| A. Diff | 3 (drags) | 60.0 | 18.3 / 23.5 | 5.2 / 11.2 | 8,667 |
| A. Diff | 4 (drags) | 60.0 | 18.4 / 20.0 | 5.8 / 7.1 | 9,075 |
| A. Diff | 5 (idle tail) | 60.0 | 17.2 / 18.2 | 2.0 / 2.6 | 9,075 |
| B. Graph | 1 (wheel) | 59.9 | 17.6 / 28.1 | 2.9 / 13.4 | n/a |
| B. Graph | 2 (wheel, drags) | 60.0 | 18.1 / 20.9 | 4.4 / 6.5 | n/a |
| B. Graph | 3 (drags) | 60.0 | 18.0 / 22.7 | 4.7 / 8.7 | n/a |
| B. Graph | 4 (drags) | 60.0 | 17.5 / 18.6 | 3.1 / 4.1 | n/a |
| B. Graph | 5 (idle tail) | 60.0 | 17.5 / 18.7 | 1.7 / 2.5 | n/a |

| Run | Since reset: worst frame | Frames > 50 ms | Blank rows | First paint | Peak memory |
| --- | --- | --- | --- | --- | --- |
| A. Diff | 23.5 ms | 0 | 0 | 47.1 ms after data ready | 203 MB |
| B. Graph (200,000 commits walked in 0.79 s, 100k+ deep reached by the drags) | 28.1 ms | 0 | 0 | 37.9 ms after the first page | 626 MB |

Reading these numbers:

- **fps, blank rows and >50 ms frames pass in every window.** The worst frame is 23.5 ms (diff) and 28.1 ms (graph).
- **Interval p95 is 17.2–18.4 ms**, so the literal "≤ 16.7 ms" fails in every window, as in the earlier sweeps. Under the proposed reading (≤ 18 ms) three of the ten windows miss by a little: diff 18.3 and 18.4 ms, graph 18.1 ms. Build p95 (the CPU time per frame) is at most 5.8 ms in every window. Which of these counts is the open decision below.
- **Unstyled rows appear during the scrollbar drags, not during the wheel:** 0 during 10 s of wheel, then about 9,000 rows (roughly 200 frames of about 45 rows) while the thumb jumped through the 50k rows in about 2.4 s per drag (about 360 rows per frame). Rows were painted in plain ink until the highlight chunk arrived. These are not blank rows (blank stayed 0), but they are visible as colours popping in, and the earlier constant-speed sweeps hid the problem (43 unstyled rows in all). Whether "painted without their data" in the protocol covers unstyled rows is a call for the user. It is a likely retry item if it does: prefetch highlight chunks around the thumb or shape the first frame from the chunk nearest in the cache.
- Graph peak memory was 626 MB, above the 506 MB of the earlier sweeps, because the drags touched more rows' metadata. It is not a gate box.
- The reset click landed (`stats reset` was printed), so the numbers cover only the scroll phases. The first printed window of each run, taken before the reset, is not included. That window also held the first frames (diff: interval max 37.5 ms and 2,440 unstyled rows while parsing; graph: interval max 46.4 ms).

### Manual runs (user)

| Run | Result |
| --- | --- |
| A. Diff: human wheel 10 s + scrollbar drag ×3 | _pending_ (agent-driven run above) |
| B. Graph: human wheel 10 s + scrollbar drag ×3, 100k+ deep | _pending_ (agent-driven run above) |
| C. Text input: typing, Shift+arrows, drag, double-click, Ctrl+A/C/V | _pending_ (needs hands) |
| C. Microsoft Japanese IME inline composition | _pending_ (needs hands and eyes) |
| C. Microsoft Pinyin (optional) | _pending_ |
| D. 90 Hz (information) | _not run_ (changing the display refresh rate was left to the user) |
| Peak memory from Task Manager | Process peak working set: 203 MB (diff), 626 MB (graph); see above |
| E. "API felt workable" | _pending_ (the user's call) |

### Checklist

| Box | Kind | Status | Evidence |
| --- | --- | --- | --- |
| Diff view scrolls at 60 fps or better, no blank rows, release build | Must pass | **60 fps and blank rows: pass.** p95: depends on the reading (see below). Human run: pending | Automated sweeps: 60.0 fps, 0 blank rows, worst 23.8 ms. Agent-driven wheel and drags: 60.0 fps, 0 blank, 0 frames > 50 ms, worst 23.5 ms, interval p95 17.2–18.4 ms. About 9,000 unstyled rows during the drags |
| Graph scrolls 100k commits deep at 60 fps or better, no blank rows | Must pass | **60 fps and blank rows: pass.** p95: depends on the reading (see below). Human run: pending | Automated sweeps: 60.0 fps, 0 blank rows, worst 21.9 ms. Agent-driven wheel and drags: 59.9–60.0 fps, 0 blank, 0 frames > 50 ms, worst 28.1 ms, interval p95 17.5–18.1 ms |
| First paint under 100 ms for the diff view once files are in memory | Soft | **Pass** | 37.7 and 54.2 ms (diff); 35–37 ms (graph, for information) |
| Text input works for a commit message box (IME and selection behave) | Soft | Pending | `spike-diff --commit-box` |
| Custom drawing (lanes, highlight backgrounds) was doable without forking GPUI | Soft | **Pass** | Lanes, curves and dots with `canvas`, `paint_quad` and `PathBuilder`; diff backgrounds and syntax colours with divs and `StyledText` highlights. GPUI is used unmodified; the only change is mirroring Zed's own `async-task` patch, which is not a fork. |
| The API felt workable: you would choose to keep going | Must pass | Pending | The user's call; the agent's notes are under "Risks and GPUI notes" |

**Strict reading of "p95 ≤ 16.7 ms".** The frame-interval p95 is 17.3–17.9 ms even at a perfect 60.0 fps with no frame over 24 ms, because paint timestamps jitter about ±1 ms around a 16.67 ms vsync. Read literally, the rule would fail every vsync-paced app measured this way. The proposed reading is in `tasks/P0-3.DONE.md`, question 3: 60 fps, interval p95 ≤ 18 ms, no frame over 50 ms. It is an open decision for the user.

### Gate outcome

**Not decided.** Three inputs are still the user's, and the agent does not decide them. By the rules in [the plan](PLAN.md#gate-rules), nothing here counts as a failed box yet, and Phase 0 does not end until all three must-pass boxes pass.

1. **How to read "p95 ≤ 16.7 ms".**
   - *Literal reading* (frame-interval p95 ≤ 16.7 ms): diff scroll and graph scroll both fail in every run so far (17.2–18.4 ms at a steady 60.0 fps). Two must-pass boxes failing means a retry weekend, and two still failing after it means Tauri + React. This reading would fail any vsync-paced app measured this way (paint timestamps jitter about ±1 ms around 16.67 ms).
   - *Proposed reading* (60 fps in every window, interval p95 ≤ 18 ms, no frame over 50 ms): the 2026-11-20 automated sweeps pass. The 2026-11-22 agent-driven run is within 0.1–0.4 ms of it in three of ten windows (diff 18.3 and 18.4 ms, graph 18.1 ms). That is a marginal miss at the edge of the measurement noise, not a clear pass.
   - *Build p95 reading* (CPU time of the frame, at most 5.8 ms here) passes by a wide margin, but it is not what the protocol measures.
2. **Unstyled rows during scrollbar drags** (about 9,000 rows, none blank): does "visible rows painted without their data" include rows painted without syntax colours? If yes, the diff box fails on the drag, and it is the only real, fixable finding of this run.
3. **"API felt workable"** (must pass) and the soft text-input box (IME, selection): human checks, still pending.

No fallback has been chosen. If the user rules for the literal reading, or for counting unstyled rows, the apply step is one retry weekend (the plan's rule), not an immediate switch to Tauri.

## Risks and GPUI notes

- GPUI's git dependency needs Zed's `async-task` `[patch]` mirrored in our root `Cargo.toml` (see `README.md`). Every GPUI bump must re-check Zed's `[patch.crates-io]` section.
- GPUI core has no scrollbar at this revision (Zed's `ui` crate has one). The spike draws its own: a track, a thumb, and window-level mouse listeners while dragging. About 80 lines, and not hard.
- `uniform_list` handles wheel scrolling and virtualization well. Its `UniformListScrollHandle` exposes the offset, so a custom scrollbar and programmatic scrolling were simple.
- Building each diff cell from nested divs costs about 4.7–4.9 ms of CPU per frame for about 45 rows on AC power (11–18 ms on battery). That leaves room under the 16.7 ms budget. The PRD's own GPUI elements for the diff viewer (a custom element that shapes and paints lines directly, as Zed's editor does) would cut it further, but the spike does not need it.
- Tree-sitter parses 1.5 MB of Rust in about 0.7 s on this CPU, far above the spec's 100 ms fallback threshold (see `tasks/P0-2.DONE.md`).
- **The naive layout on the kernel reaches 691 open lanes.** Topological order keeps many lanes waiting for a long time. Storing every passing lane per row cost 70.7 M edge records and 773 MB for 200k commits. Storing only the edges that touch each dot, plus a lane snapshot every 128 rows that is replayed for the visible rows, cut that to 431 k edges. The spec's row record ("an offset into a shared edge arena") should say passing lanes are not stored.
- **Commit metadata must not be read on the UI thread.** A cold read is about 43 µs, and fast scrolling needs about 80 new rows per frame, which gave frames up to 183 ms. A background reader 1,000 rows ahead removed every UI-thread read in the fast sweep. This matches the core design (IO only in `shell`).
- Custom lane drawing with `canvas`, `paint_quad` (straight lanes, dots) and `PathBuilder` (curves) was straightforward and cheap. Build p95 is about 3.5 ms with 16 lanes drawn per row.
- **The laptop must be plugged in.** On battery, the same diff sweep took 3–4 times the CPU per frame and paint times jittered by ±5 ms.
- **GPUI core has no text-input widget.** It provides the platform plumbing (`EntityInputHandler`, `ElementInputHandler`, marked-text ranges for IME, shaped lines for hit-testing), and its `examples/input.rs` shows a working single-line field. The spike's commit box adapts that example (about 600 lines). A multi-line commit editor with word navigation, undo and wrapping would be our own work in v0.1 (backlog task 14), or would borrow from Zed's `editor` crate, which is far heavier.
