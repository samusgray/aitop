# Tail Feed: Inline Syntax-Highlighted Diffs + Auto-Scroll

**Date:** 2026-06-23
**Status:** Approved (design)

## Problem

Two issues with the focused tail view (`ViewMode::Tail`) in `src/dashboard.rs`:

1. **Code edits are not shown as diffs.** When an agent runs `Edit`/`MultiEdit`/`Write`,
   the feed only records a one-line summary and a `FileTouched(path)` annotation
   (`summarize_tool` in `src/feed.rs`). The actual `old_string`/`new_string` content is
   discarded, so there is no way to see what changed.
2. **The feed does not follow the bottom.** Tail opens at `scroll: 0` (top), and the
   "jump to bottom" path (`scroll == usize::MAX`) clamps to `lines.len() - 1`, which puts
   the last line at the *top* of the pane instead of anchoring the newest content to the
   bottom. New records never pull the view down.

## Goal

Render code edits inline in the feed as syntax-highlighted diffs that look like a modern
editor diff gutter (line numbers, `+`/`-` markers, full-width red/green row fills,
syntax-colored tokens, word-level emphasis on changed spans), and make the feed follow the
newest events like `tail -f`.

Explicitly **not** using the external `hunk` viewer: it is a separate full-screen TUI app
and cannot render inside aitop's ratatui feed pane. All rendering is native.

## Components

### Component 1 — Auto-scroll / follow mode

**State.** `ViewMode::Tail { scroll: usize }` becomes `ViewMode::Tail { scroll: usize, follow: bool }`.

**Behavior.**
- Opening tail starts with `follow: true`, pinned to the newest events.
- While `follow` is on and the feed grows on refresh, the view stays pinned to the bottom.
- Scrolling up (`k`, `PageUp`, `g`) sets `follow: false` and freezes the view at an offset.
- Reaching the bottom again (scrolling down past the last line) or pressing `G` sets
  `follow: true`.

**Correct bottom anchoring.** The displayed scroll offset is computed where the rendered
area height is known (in `draw_tail`, which has the layout rects), not inside `tail_feed`.
A pure function makes the math testable:

```rust
/// Returns the paragraph scroll offset (first visible line index).
fn feed_scroll_offset(follow: bool, manual_scroll: usize, total_lines: usize, viewport: usize) -> usize
```

- `follow == true`  → `total_lines.saturating_sub(viewport)` (last page fills the pane,
  newest line on the bottom row).
- `follow == false` → `manual_scroll.min(total_lines.saturating_sub(viewport))`.

`viewport` is the inner height of the feed block (area height minus borders).

### Component 2 — Native syntax-highlighted inline diffs

**Capture.** Add a structured edit to the feed model. In `src/feed.rs`:

```rust
pub struct FileEditHunk {
    pub old_text: String, // "" for a pure addition (e.g. Write of a new file)
    pub new_text: String, // "" for a pure deletion
}

pub enum FeedEvent {
    // ...existing variants...
    FileEdit {
        path: String,
        hunks: Vec<FileEditHunk>,
    },
}
```

Parsing of tool-call inputs (in/near `summarize_tool`):
- `Edit` → one hunk `{ old_string, new_string }`.
- `MultiEdit` → one hunk per entry in the `edits` array.
- `Write` → one hunk `{ old_text: "", new_text: content }` (treated as all-added).
- The existing `FileTouched(path)` annotation is kept for the badge.

These tool calls currently become `FeedEvent::ToolCall`. They instead produce a
`FeedEvent::FileEdit` so the renderer can lay them out as diffs. The matching
`FeedEvent::ToolResult` record (success/failure) is unaffected and still rendered as today.
Non-edit tool calls are unchanged.

**Diff computation.** Use the `similar` crate:
- Line-level diff between `old_text` and `new_text` (`TextDiff::from_lines`) to classify
  each line as context / removed / added.
- Word-level diff (`TextDiff::from_words` / inline change iteration) within changed regions
  to emphasize the specific tokens that changed.

**Syntax highlighting.** Use `syntect`:
- Pick the syntax from the file extension of `path`; fall back to plain (diff-colored only)
  when the language is unknown.
- Highlight the *content* lines, then overlay the diff row background and word-level
  emphasis.
- **Caching:** highlighting is computed once per `FileEdit` and cached (keyed by a hash of
  path + hunk contents), not recomputed on every frame. The feed only re-renders on
  `needs_draw`, and the journal content for a given record is immutable once written.

**Layout (matches the target screenshot).** Each rendered diff line is a ratatui `Line`:
- a left gutter: line number + `+` / `-` / ` ` marker,
- a **full-width background fill**: dark green for added rows, dark red for removed rows,
  none for context. The line text is padded with spaces to the pane width so the background
  spans the full row (ratatui only colors cells that contain spans).
- syntax-colored token spans layered on top of the row background,
- word-level changed spans rendered with a brighter background.
- A header line per edit: `± <path>  +<added> -<removed>`.
- **Collapse:** diffs longer than a cap (~20 visible lines) show the first lines then a
  `… +<N> more lines` marker. (No external full view; the cap just keeps the feed scannable.)

**Module boundary.** Diff rendering is non-trivial and should not bloat `dashboard.rs`
(already ~1700 lines). Put the diff-to-`Line<'static>` rendering and syntect/similar wiring
in a new `src/diffview.rs` module with a focused public function, e.g.:

```rust
pub fn render_file_edit(path: &str, hunks: &[FileEditHunk], width: usize) -> Vec<Line<'static>>
```

`dashboard.rs::feed_record_lines` calls it for `FeedEvent::FileEdit`. The syntect
syntax/theme sets load once (lazily, process-global).

## Data Flow

```
journal file
  → load_session_feed (src/feed.rs)        // now emits FeedEvent::FileEdit for edits
  → FeedRecord list
  → tail_feed (src/dashboard.rs)            // builds Vec<Line>; FileEdit -> diffview::render_file_edit (cached)
  → feed_scroll_offset(follow, scroll, total_lines, viewport)
  → Paragraph::scroll((offset, 0))
```

## Dependencies

- `similar` — line + word diffing (light, pure Rust).
- `syntect` — syntax highlighting with bundled syntaxes/themes. Accepted binary-size cost;
  the syntax coloring is the point of the feature.

## Testing (TDD)

Unit tests:
- `Edit` / `MultiEdit` / `Write` tool inputs parse into the expected `FileEdit` model
  (path + hunks), including `Write` as all-added.
- Line + word diff classification: given old/new text, the expected lines are marked
  context / added / removed, and the changed word spans are identified.
- Collapse: a diff longer than the cap yields exactly the cap of visible lines plus the
  `… +N more lines` summary line.
- `feed_scroll_offset`: follow vs. manual, clamping at `total_lines - viewport`, and the
  small-feed case (`total_lines <= viewport` → offset 0).

Manual verification:
- Color/layout fidelity (red/green fills, syntax colors, word emphasis) confirmed by
  running `cargo run --release` and opening a session with recent edits.
- Follow behavior confirmed live: open tail on an active session, watch new events pin to
  the bottom; scroll up to freeze; press `G` to re-follow.

## Scope / YAGNI

- No external `hunk` launch, no terminal handoff, no spawning external processes.
- No `--watch`, no config file, no theme selection UI (single built-in theme to start).
- No diff for non-edit tool calls; they keep their current summary rendering.
```