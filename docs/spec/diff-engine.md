# Spec — Diff engine

## Algorithm

Histogram diff is the default; Myers is the fallback and the reference implementation the tests compare against.

- **Histogram** (as in git's `--histogram`): anchors on lines that are rare in both files, then recurses between anchors. It produces more readable hunks for code (moved blocks, repeated braces) and runs fast on typical edits.
- **Myers** (O(ND), with the linear-space refinement): used when a region has no unique anchor lines (histogram falls back to it there, as git does) and as the oracle in property tests.
- **Preprocessing:** split into lines keeping line endings; intern each line to a `u32` id (hash map) so comparisons are integer compares; strip the common prefix and suffix before running either algorithm.
- **Options:** ignore whitespace changes, ignore all whitespace, ignore blank lines; applied at interning time by normalising before hashing.
- **Post-processing:** slide hunks to align with indentation boundaries (the "indent heuristic" git uses) so a hunk starts at a natural block edge.
- **Budget:** files are diffed on a background thread; a diff that runs past 200 ms on one file is cancelled and replaced by a whole-file replace hunk with a "diff too expensive" note.

## Data model

A diff is a list of files, each a list of hunks, each a list of typed lines; every line keeps its number on both sides so staging and annotations can point at it exactly.

```rust
pub struct FileDiff {
    pub old: Option<FileSide>,      // None = added file
    pub new: Option<FileSide>,      // None = deleted file
    pub kind: DiffKind,             // Text | Binary | TooLarge | Generated | ModeOnly | Submodule
    pub hunks: Vec<Hunk>,
    pub stats: Stats,               // added, removed
}

pub struct FileSide { pub path: RepoPath, pub oid: ObjectId, pub mode: FileMode }

pub struct Hunk {
    pub old_start: u32, pub old_len: u32,
    pub new_start: u32, pub new_len: u32,
    pub header: Option<String>,     // nearest enclosing function, from tree-sitter
    pub lines: Vec<DiffLine>,
}

pub struct DiffLine {
    pub kind: LineKind,             // Context | Added | Removed
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
    pub text: TextRange,            // slice into the interned file buffer, no copy
    pub eol: Eol,                   // Lf | CrLf | None (no newline at end of file)
    pub inline: Option<InlineSpans>,// word-level changes, filled lazily
}
```

- Lines reference shared buffers rather than owning strings, so a 50k-line diff costs two file buffers plus small fixed-size line records.
- Hunks are built with 3 lines of context by default; "expand" in the UI asks the engine for more context without recomputing the diff.
- The "no newline at end of file" state is part of the model (`Eol::None`), because staging must preserve it exactly.

## Word-level diff

Inside each block of removed lines followed by added lines, a second diff runs on tokens to highlight exactly what changed.

1. Pair removed and added lines in the block by position (1st with 1st, and so on); unpaired lines get no inline highlight.
2. Skip pairs that are too different: if the shared tokens are under 30% of the longer line, show whole-line colouring only, because a highlight on nearly everything is noise.
3. Tokenise each line into words (identifier characters), runs of whitespace and single punctuation characters.
4. Run Myers on the token ids and turn the result into byte ranges (`InlineSpans`) on each side.
5. Merge spans separated by a single whitespace token so `foo bar` → `baz qux` reads as one change, not two.

Computed lazily, only for lines that scroll into view, and cached by line pair.

## Line staging

Staging, unstaging and discarding a selection all build a unified patch from the diff and the selection, then hand it to `git apply`; the rules differ only in direction.

| Action | Diff used | Selected `+` | Selected `-` | Unselected `+` | Unselected `-` | Applied with |
| --- | --- | --- | --- | --- | --- | --- |
| Stage | index → worktree | keep `+` | keep `-` | drop | becomes context | `git apply --cached` |
| Unstage | HEAD → index | becomes `-` | becomes `+` | becomes context | drop | `git apply --cached` |
| Discard | index → worktree | becomes `-` | becomes `+` | becomes context | drop | `git apply` (worktree) |

**Building the patch**

1. Walk each hunk that contains at least one selected line; skip hunks with none.
2. Rewrite each line by the table; existing context stays context.
3. Recount the header: old length = context + `-` lines; new length = context + `+` lines. Shift each hunk's new start by the net line change of the hunks before it in this patch.
4. Preserve end-of-file state: emit `\ No newline at end of file` exactly where the source line had `Eol::None`.
5. Keep line endings byte for byte (CRLF stays CRLF).

**Safety**

- The patch records the blob id it was built from; the shell checks the index (or worktree file) still matches before applying, otherwise the action is refused and the diff refreshed.
- Run `git apply --check` with the same flags first; apply only if it passes.
- Discard always takes an undo snapshot first (Core design → Undo).
- Laws 4–7 test this module directly: random files, random selections, compare against real git.

## Large, binary and generated files

Files that would be slow or meaningless to diff line by line are classified first and collapsed by default; the user can always expand.

| Kind | Detected by | Default view |
| --- | --- | --- |
| Binary | git's rule: a NUL byte in the first 8 KB, or `-diff` / `binary` in `.gitattributes` | "Binary file changed" with old and new sizes; image preview for png, jpg, gif, svg, webp |
| Too large | Either side over 5 MB or over 100k lines | Collapsed with stats; expand runs the diff on demand |
| Generated | `linguist-generated` in `.gitattributes`, or known names (`*.lock`, `package-lock.json`, `pnpm-lock.yaml`, `*.min.js`, `*.map`) | Collapsed with stats |
| Mode only | Same blob, different mode | One line: "mode 100644 → 100755" |
| Submodule | Gitlink entry | Old and new commit ids |

Thresholds are settings; the defaults above are a starting point to tune in the spike.

## Highlighting pipeline

Each side of a diff is parsed as a whole file with tree-sitter, so highlighting is correct across hunk boundaries, then only visible lines are styled.

1. **Language:** pick the grammar from `.gitattributes` (`linguist-language`), then file extension, then shebang. No match → plain text.
2. **Parse:** parse the full old and new file on a background thread; keep the trees, and reparse incrementally when the worktree file changes.
3. **Query:** run each grammar's highlight query to get captures (`keyword`, `string`, `function`, …) as byte ranges.
4. **Theme:** map captures to token colours from the design tokens (own layer; a short fixed list of capture names → token names).
5. **Visible window:** style only the lines on screen plus a 200-line buffer each way; cache styled lines by (blob id, line number).
6. **Fallback:** files over the "too large" threshold, or a parse slower than 100 ms, render as plain text with a note.

Grammars for v0.1: Rust, TypeScript/JavaScript, Go, Java, Kotlin, Python, JSON, YAML, TOML, Markdown, HTML, CSS, SQL, Bash.
