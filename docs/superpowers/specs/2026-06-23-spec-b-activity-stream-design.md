# Spec B — Cross-Project Activity Stream

**Date:** 2026-06-23
**Status:** Approved (design)
**Part of:** activity-visuals (A → B → C)

## Problem

The bottom "activity" panel (`render_activity` / `activity_lines` in `src/app.rs`) shows ~12 static,
per-session text lines for the current snapshot only. It is not cross-project in time order, cannot
be scrolled, and cannot be expanded to see detail. It is the weakest panel on screen.

## Goal

Build a unified, time-ordered event log across **all** projects/sessions, shown as a compact preview
in the monitor and as a dedicated full-screen, scrollable, **expandable** stream view. Reuse the
existing feed parsing and diff rendering.

Constraints: offline, read-only. Bounded work per tick (cap how many journals / records are read).

## Components

### Component 1 — `ActivityIndex` (new module `src/activity.rs`)

Merges recent feed records from every known session into one chronologically sorted list.

```rust
pub struct StreamEvent {
    pub timestamp: Option<SystemTime>,
    pub project: String,            // repo name
    pub agent: AgentKind,
    pub session_key: String,        // links back to a session
    pub kind: StreamKind,           // User | Assistant | Thinking | Tool | Result | FileEdit | Usage
    pub summary: String,            // one-line, already sanitized
    pub detail: Option<StreamDetail>, // expansion payload
    pub is_error: bool,
}

pub enum StreamDetail {
    Text(String),                   // result body / assistant text / command
    FileEdit { path: String, hunks: Vec<crate::feed::FileEditHunk> }, // reuse diffview
}

pub struct ActivityIndex {
    events: Vec<StreamEvent>,       // newest last
}

impl ActivityIndex {
    /// Build from the snapshot's sessions: for each session with a journal, read the last
    /// `per_session` records (bounded), convert FeedRecords → StreamEvents, merge, sort by
    /// timestamp ascending, and truncate to `max_total` newest.
    pub fn build(snapshot: &AmbientSnapshot, per_session: usize, max_total: usize) -> Self;
    pub fn events(&self) -> &[StreamEvent];
}
```

`StreamEvent` is produced by mapping `FeedRecord`/`FeedEvent` (from `feed::load_session_feed` or a new
lighter `feed::tail_records(path, agent, id, n)`): `FileEdit → FileEdit detail`, `ToolResult → Text`
detail, `Assistant/Thinking → Text`, etc. `summary` reuses the existing one-line formatting; all text
is run through `feed::sanitize_inline`.

Cost control: `build` reads at most `per_session` (e.g. 40) records from each session's journal tail
and caps the merged list at `max_total` (e.g. 500). Sessions without a journal are skipped.

`ActivityIndex` is rebuilt when a new snapshot arrives (same cadence as the feed today). For the
monitor preview this is cheap; for the full Stream view it is rebuilt on snapshot refresh, preserving
scroll/expansion state by event identity where possible.

### Component 2 — Monitor preview (replaces `render_activity`)

The bottom strip renders the **last K** `StreamEvent`s (cross-project) instead of per-session lines:
`15:42 aitop ✎ src/app.rs +12−3`, color-coded by project and kind. A hint shows `s: open stream`.

### Component 3 — `ViewMode::Stream` (new full-screen view)

A new view, opened with **`s`** from the monitor (and `esc` returns), structured like the tail view:
- Renders `ActivityIndex::events()` newest-at-bottom, following the tail by default (reuse Spec-9's
  `feed_scroll_offset` + follow semantics).
- Each event is one collapsed line. The **selected** event can be **expanded** (`enter` / `→`) to
  show its `StreamDetail`: `Text` as wrapped lines; `FileEdit` via `crate::diffview::render_file_edit`.
  `←` / `enter` again collapses.
- Navigation: `j`/`k` move selection, `PageUp`/`PageDown`, `g`/`G`, follow on `G`.
- **Filtering:** `p` cycles project filter (all → each project → all); `e` toggles errors-only.
- A footer shows counts and active filters.

State (`selected`, `expanded` set, `follow`, `scroll`, filters) lives in the `Stream` variant of
`ViewMode`.

### Component 4 — Wiring

`ViewMode` gains `Stream { selected, scroll, follow, expanded, project_filter, errors_only }`.
`handle_key` gains the `s` entry from monitor and the Stream-mode bindings. `draw` dispatches to a
new `draw_stream`. The `ActivityIndex` is built in `run_loop` from the current snapshot (and rebuilt
on refresh), passed via `DrawContext`.

## Data Flow

```
AmbientSnapshot.sessions
  → ActivityIndex::build (read bounded journal tails, map to StreamEvent, merge+sort+cap)
  → monitor: render_activity_preview (last K)
  → ViewMode::Stream: draw_stream (scroll/expand/filter; FileEdit via diffview)
```

## Dependencies

None new. Reuses `feed`, `diffview`, and the follow/scroll logic from the tail view.

## Testing (TDD)

- `ActivityIndex::build` merges multiple sessions into ascending timestamp order; respects
  `per_session` and `max_total` caps; skips journal-less sessions.
- `FeedRecord → StreamEvent` mapping (kind, summary, detail variant, is_error) for each event type.
- Project/errors filters select the right subset (pure function over `&[StreamEvent]`).
- Expansion toggle logic (pure over selection/expanded set).
- Detail rendering reuses `diffview::render_file_edit` for `FileEdit`.
- Scroll/expand interactive behavior verified manually (TUI) per the established pattern.

## Scope / YAGNI

- No full-text search yet (filters only).
- No persistence of stream state across runs.
- Reads journal tails only (bounded); no full-history indexing.
