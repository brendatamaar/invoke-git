# Invoke Git — prompts for the coding agent

Paste these in order. Each assumes `AGENTS.md` sits in the repo root and the docs are laid out like this:

```
AGENTS.md
docs/PRD.md
docs/Plan.md
docs/spec/core-design.md
docs/spec/diff-engine.md
docs/spec/commit-graph.md
docs/spec/verification.md
design/tokens.json
design/README.md
design/screens/*.dc.html
```

## 1. Orientation (no code yet)

```
Read AGENTS.md and everything under docs/ and design/. Summarize the architecture in 10 bullets, list any contradictions between the docs, and list open questions that block Phase 0. Do not write code.
```

## 2. Scaffold the spike

```
Phase 0 setup from docs/Plan.md. Create a Cargo workspace with two throwaway binary crates: spike-diff and spike-graph. Depend on GPUI as a git dependency on the zed-industries/zed repo, pinned to one recent commit; record the exact revision and the reason in README.md. Add a script that generates the fixtures: a 50k-line Rust-like file and a copy with about 2k changed lines spread through it. Do not clone the Linux kernel; I will do that myself. Make both crates open an empty window on Windows with `cargo run --release`.
```

## 3. Prototype A: diff view

```
Build Prototype A in spike-diff, per the Phase 0 section of docs/Plan.md. Load the two fixture files, compute a line diff (any crate is fine here; it is throwaway), and render a split view in a virtualized list: line numbers, add/remove backgrounds and +/- signs. Take colours and the code type style from design/tokens.json. Syntax-highlight only visible lines with tree-sitter (Rust grammar). Add a frame-time readout and log first-paint time. Mouse-wheel and scrollbar-drag scrolling must work end to end.
```

## 4. Prototype B: commit graph

```
Build Prototype B in spike-graph, per docs/Plan.md and docs/spec/commit-graph.md. Take a repo path as an argument. Walk commits with gix in topological order, in pages of 5,000. Use naive lane assignment and draw lanes and dots with a custom GPUI element on 28px rows. Columns: graph, subject, author, date, short hash; read the metadata lazily for visible rows only. Show frame time and first-paint time. It must scroll 100k commits deep on the Linux kernel clone.
```

## 5. Measure and decide

```
Help me run the Phase 0 pass/fail checklist in docs/Plan.md on release builds. For each box, tell me exactly what to run and what to look at, then record my results in docs/spike-results.md along with your notes on what was hard in GPUI. Apply the fallback rule: two or more failed boxes means we switch to Tauri.
```

## 6. Each v0.1 backlog task (reuse for every task)

Start with tasks 1–3 (workspace and CI, tokens crate, core types); every later task depends on them.

```
Work on v0.1 backlog task #<N> from docs/Plan.md. First read the spec sections it touches and list the files you will create or change, then implement it. Follow AGENTS.md: pure crates do no IO, add tests (property tests for pure logic) and prove each one can fail, then run fmt, clippy and test. Finish with a summary and anything you had to assume.
```
