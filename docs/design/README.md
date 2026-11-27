# Invoke Git

A fast, native git client for Windows. Review before you push.

This is the design system for Invoke Git: colours, type, spacing, radii and fixed sizes for a dense, calm desktop UI in the spirit of Linear. It was written from the product PRD, not from existing brand files: there is no logo yet, so the name is set in plain type. The values here are the source of truth for the `tokens` crate, which turns `tokens.json` into Rust constants for the GPUI app.

## Principles

1. **State first.** Branch, ahead/behind, an in-progress merge or rebase and conflicts are always on screen. Colour is spent on state, not on decoration.
2. **One hue.** Surfaces are neutral; `accent` is the only brand colour. Diff colours and lane colours carry meaning and appear only where that meaning applies.
3. **Lines, not shadows.** Panels sit flush and are separated by `line`. Only things that float above the window (menus, popovers, dialogs) get `shadow-overlay`.
4. **Dense but readable.** 13px text on 28px rows. Nothing smaller than 11px, and 11px only for uppercase section labels.
5. **Dark first.** Dark is the default theme and the first one defined; light mirrors every token.

## Repository workspace

- Changes, History, and Pull Requests are the main views. Keep the current repository, worktree, branch, and upstream visible above them.
- In Changes, keep the staged and unstaged lists and commit composer together. The diff header identifies the selected file and what is being compared.
- History groups the commit graph and the resizable commit inspector. Commit search, the branch scope, and reflog recovery belong in this view.
- The toolbar opens Review & push. The review owns the final Push action and shows its destination, remaining files, and unresolved local notes.
- Show keyboard shortcuts on their controls. Icon-only buttons carry a tooltip and an accessible name.

## Content fundamentals

- **Plain git words, used exactly.** Stage, unstage, commit, amend, push, pull, fetch, rebase, cherry-pick, stash. Never invent synonyms ("save", "sync", "upload").
- **Sentence case** everywhere: buttons, menus, titles. Uppercase only for `label` section headers.
- **Say what will happen, then the button names the action.** "This removes 3 lines from auth.rs. You can undo it." → button: **Discard lines**.
- **Numbers over adjectives.** "2 commits to push", "+12 −4", "Rebasing 3/7".
- **No exclamation marks, no emoji, no blame.** Errors name the cause and the fix: "Push rejected: the remote has 2 new commits. Fetch and rebase to continue."
- **Git's own names stay as git prints them**: branch names, hashes and file paths are never translated or prettified; they render in `code-small` or `body` without quotes.

## Visual foundations

### Surfaces and text

| Use | Token |
| --- | --- |
| Window background | `bg-app` |
| Panels (sidebar, commit list, diff) | `bg-panel` |
| Menus, selected row, buttons at rest | `bg-raised` |
| Hover | `bg-hover` |
| Dividers | `line` |
| Input and checkbox borders | `line-control` |
| Text / secondary / line numbers | `ink` / `ink-muted` / `ink-faint` |

Every text token meets 4.5:1 on every `bg-*` token in both themes, and `ink-faint` still does on `bg-panel`, so line numbers stay legible.

### Accent and actions

- **Primary button:** `accent` fill, `on-accent` label, `radius-md`, 28px tall. One per view at most (Commit, Push).
- **Secondary button:** `bg-raised` fill, `ink` label; `bg-hover` on hover.
- **Danger button:** `danger` fill, `on-danger` label, only for discard, delete branch and force push, and always after a description of what will change.
- **Focus:** a 2px `focus` ring outside the control, on every focusable element.
- **Current branch pill:** `accent-soft` background, `accent` text, `radius-sm`.

### Diff colours

| Line | Background | Gutter sign and counts | Word-level highlight |
| --- | --- | --- | --- |
| Added | `add-bg` | `add-ink` `+` | `add-bg-strong` |
| Removed | `remove-bg` | `remove-ink` `−` | `remove-bg-strong` |
| Context | `bg-panel` | none | none |

Code text on every diff background is `ink`, never green or red text. A change is never shown by colour alone: the `+`/`−` sign in the gutter and the old/new line numbers always carry it, so the viewer works for people who cannot tell red from green. The two inks also differ in lightness, not only hue.

### State colours

- **Conflicts and paused operations:** `warning` text or icon; banners on `warning-soft`.
- **Destructive:** `danger`. Never used for "removed lines"; that is `remove-ink`.

### Commit graph

- Lanes take `lane-1` to `lane-8` in rotation as they open and keep their colour until they close.
- Every lane colour has at least 3:1 contrast on `bg-panel`; hovering a lane highlights it end to end, so a branch can be followed without relying on colour.
- HEAD's commit is a ring in `accent`; merges are hollow dots; normal commits are filled dots in their lane colour.
- Sizes: `row-graph` rows, `lane` per column.

### Typography

- **Inter** for the interface, **JetBrains Mono** for code, both loaded from Google Fonts in previews and bundled in the app. Windows fallbacks: Segoe UI Variable and Cascadia Mono.
- Styles: `display`, `title`, `body`, `body-medium`, `small`, `label` (Interface); `code`, `code-small` (Code).
- Diff lines use `code` on `row-diff` rows; line height equals row height so the list can be virtualized with fixed rows.

### Spacing, radii, sizes

- A 4px grid: `space-1` to `space-8`. Rows pad `space-2` horizontally; panels pad `space-4`; dialogs pad `space-6`.
- Radii: `radius-sm` for pills and checkboxes, `radius-md` for buttons, inputs and row hover, `radius-lg` for dialogs and menus.
- Fixed sizes (`row-list`, `row-graph`, `row-diff`, `toolbar`, `sidebar`, `lane`) are layout constants the Rust code reads.

### Motion

Not a token family yet; the rules are:

- 120ms ease-out for hover, press, menus opening and panel toggles.
- No motion on scrolling, diff rendering or graph updates: content appears in place.
- Progress is shown with a determinate bar whenever git reports a percentage.

## Iconography

There is no icon set yet. Until one is chosen:

- Use a single outline set at 16px with a 1.5px stroke, in `ink-muted`, `ink` on hover, `accent` when active.
- Icons always sit beside a text label in toolbars and menus; an icon alone needs a tooltip.
- No emoji and no filled, multicolour icons. Git objects (branch, tag, commit, stash) each get one fixed icon.

## Logo

The **Helix Staff** rune: braided branches weaving around a central `main` spine into a hollow `HEAD` ring, rendered in brushed platinum on an obsidian tile with a subtle platinum rim (part of the Invoke suite alongside Mint for Invoke API and Gold for Invoke DB). Source vector: `crates/app/assets/invoke-git.svg`; Windows multi-size icon: `crates/app/assets/invoke-git.ico`. Set the wordmark "Invoke Git" in Inter 600 in `ink`.
