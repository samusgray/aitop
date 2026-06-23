# Spec B — Cross-Project Activity Stream — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A unified, time-ordered event log across all projects — a compact monitor preview plus a full-screen, scrollable, expandable stream view.

**Architecture:** New `src/activity.rs` builds `ActivityIndex` by reading bounded journal tails for every session, mapping `FeedRecord`s to `StreamEvent`s, merging by timestamp. The monitor bottom strip shows the latest events; a new `ViewMode::Stream` renders the full feed with scroll/expand/filter, reusing `diffview` and the tail view's `feed_scroll_offset`.

**Tech Stack:** Rust 2024, ratatui 0.29, chrono (already a dependency) for timestamp parsing, existing `feed`/`diffview`.

## Global Constraints

- Edition 2024. `cargo clippy --all-targets --all-features -- -D warnings` and `cargo test` must pass after every task.
- Offline, read-only. Bounded work: read at most `per_session` records per journal and cap the merged list at `max_total`.
- All displayed text goes through `crate::feed::sanitize_inline`.
- Session identity reuses `crate::metrics::session_key` if Spec A is merged; otherwise replicate `format!("{}:{}", agent, native_id||cwd)`. (Specs A and B may be implemented independently; do not assume A's symbols exist — guard with the local format if `metrics` is absent.)

---

### Task 1: `feed::tail_records` — lightweight journal tail

**Files:**
- Modify: `src/feed.rs` (add function; reuse `read_tail`, `parse_claude_line`/`parse_codex_line` already present)
- Test: `src/feed.rs` tests

**Interfaces:**
- Produces: `pub fn tail_records(path: &std::path::Path, agent: crate::model::AgentKind, session_id: &str, max_records: usize) -> Vec<FeedRecord>` — parse the journal tail and return the last `max_records` parsed records (newest last). Never errors (returns empty on failure).

- [ ] **Step 1: Write the failing test**

Add to `src/feed.rs` tests (write a small temp journal with two claude lines):
```rust
    #[test]
    fn tail_records_returns_last_n_records() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        for i in 0..5 {
            writeln!(f, "{}", serde_json::json!({
                "type":"user","message":{"role":"user","content":format!("msg {i}")}
            })).unwrap();
        }
        let recs = tail_records(&path, crate::model::AgentKind::Claude, "s", 2);
        assert!(recs.len() <= 2, "respects cap, got {}", recs.len());
        assert!(!recs.is_empty(), "parsed something");
    }
```
(If the claude line shape in this test does not match `parse_claude_line`'s expectations, read that
function and adjust the JSON to a shape it parses; the assertion on the cap is the point.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib feed::tests::tail_records`
Expected: FAIL — `cannot find function tail_records`.

- [ ] **Step 3: Implement `tail_records`**

Model it on the existing `load_session_feed` flow but return raw records without cost/usage rollup. Read
the existing `load_session_feed` and `read_tail` in `src/feed.rs` and mirror their line-iteration and
per-agent parse dispatch (`parse_claude_line` for Claude, `parse_codex_line` for Codex). Collect all
parsed `FeedRecord`s, then keep only the last `max_records`. On any IO/parse failure return `Vec::new()`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib feed::tests::tail_records`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/feed.rs
git commit -m "feat: feed::tail_records lightweight journal tail parser"
```

---

### Task 2: `StreamEvent` model + `FeedRecord` mapping

**Files:**
- Create: `src/activity.rs`
- Modify: `src/lib.rs` (add `pub mod activity;`)
- Test: `src/activity.rs` tests

**Interfaces:**
- Produces:
```rust
pub enum StreamKind { User, Assistant, Thinking, Tool, Result, FileEdit, Usage }
pub enum StreamDetail { Text(String), FileEdit { path: String, hunks: Vec<crate::feed::FileEditHunk> } }
pub struct StreamEvent {
    pub timestamp: Option<std::time::SystemTime>,
    pub project: String,
    pub agent: crate::model::AgentKind,
    pub session_key: String,
    pub kind: StreamKind,
    pub summary: String,
    pub detail: Option<StreamDetail>,
    pub is_error: bool,
}
pub fn event_from_record(record: &crate::feed::FeedRecord, project: &str,
    agent: crate::model::AgentKind, session_key: &str) -> StreamEvent;
```

- [ ] **Step 1: Register module**

In `src/lib.rs` add `pub mod activity;` (alphabetical, after `app`).

- [ ] **Step 2: Write the failing test**

Create `src/activity.rs` with the types (enums/structs above) and a stub `event_from_record` returning a
`User` event, plus:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed::{FeedEvent, FeedRecord, FileEditHunk};
    use crate::model::AgentKind;

    fn rec(event: FeedEvent) -> FeedRecord {
        FeedRecord { session_id: "s".into(), timestamp: None, event, annotations: vec![] }
    }

    #[test]
    fn file_edit_maps_to_fileedit_detail() {
        let r = rec(FeedEvent::FileEdit {
            path: "src/x.rs".into(),
            hunks: vec![FileEditHunk { old_text: "a".into(), new_text: "b".into() }],
        });
        let e = event_from_record(&r, "proj", AgentKind::Claude, "k");
        assert!(matches!(e.kind, StreamKind::FileEdit));
        assert!(matches!(e.detail, Some(StreamDetail::FileEdit { .. })));
        assert!(e.summary.contains("src/x.rs"));
    }

    #[test]
    fn tool_result_error_sets_is_error_and_text_detail() {
        let r = {
            let mut r = rec(FeedEvent::ToolResult { id: "1".into(), ok: false,
                summary: "boom".into(), detail: "stack".into() });
            r.annotations.push(crate::feed::Annotation::Error);
            r
        };
        let e = event_from_record(&r, "proj", AgentKind::Claude, "k");
        assert!(e.is_error);
        assert!(matches!(e.detail, Some(StreamDetail::Text(_))));
    }
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test --lib activity::tests`
Expected: FAIL — stub returns wrong kind/detail.

- [ ] **Step 4: Implement `event_from_record`**

Map each `FeedEvent` variant:
- `FileEdit { path, hunks }` → kind `FileEdit`, summary `format!("✎ {path}")` (sanitize), detail `FileEdit { path, hunks: hunks.clone() }`.
- `ToolCall { name, summary, .. }` → kind `Tool`, summary `sanitize_inline(&format!("{name} {summary}"))`, detail `None`.
- `ToolResult { ok, summary, detail, .. }` → kind `Result`, summary sanitized, detail `Text(detail.clone())`; `is_error = !ok || annotations contains Error`.
- `Assistant { text, .. }` → kind `Assistant`, summary = `truncate_summary(text, 120)`, detail `Text(text.clone())`.
- `Thinking { text }` → kind `Thinking`, summary sanitized+truncated, detail `Text(text.clone())`.
- `User { text }` → kind `User`, summary sanitized+truncated, detail `None`.
- `Usage { input, output, .. }` → kind `Usage`, summary `format!("{} in {} out", compact_tokens(*input), compact_tokens(*output))`, detail `None`.
- `Unknown { kind }` → kind `Result`, summary `format!("? {kind}")`, detail `None`.

`is_error` defaults to `record.annotations.contains(&crate::feed::Annotation::Error)`. Parse
`timestamp` from `record.timestamp` via `chrono::DateTime::parse_from_rfc3339(...).ok()` → convert to
`SystemTime` (`Into::into`); `None` if absent/unparseable. Set `project`, `agent`, `session_key` from
args. Use `crate::feed::{sanitize_inline, truncate_summary}` and `crate::pricing::compact_tokens`.

- [ ] **Step 5: Run tests + clippy + commit**

Run: `cargo test --lib activity::tests && cargo clippy --all-targets --all-features -- -D warnings`
```bash
git add src/lib.rs src/activity.rs
git commit -m "feat: StreamEvent model and FeedRecord mapping"
```

---

### Task 3: `ActivityIndex::build` — merge across sessions

**Files:**
- Modify: `src/activity.rs`
- Test: `src/activity.rs` tests

**Interfaces:**
- Consumes: `event_from_record` (Task 2), `feed::tail_records` (Task 1).
- Produces:
```rust
pub struct ActivityIndex { events: Vec<StreamEvent> }
impl ActivityIndex {
    pub fn build(snapshot: &crate::app::AmbientSnapshot, per_session: usize, max_total: usize) -> Self;
    pub fn events(&self) -> &[StreamEvent];
    pub fn from_events(events: Vec<StreamEvent>) -> Self; // test/helper constructor
}
```

- [ ] **Step 1: Write the failing test**

`build` reads real journals (hard to unit-test), so test the merge/sort/cap via `from_events`:
```rust
    fn ev(at: Option<u64>, project: &str) -> StreamEvent {
        StreamEvent {
            timestamp: at.map(|s| std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(s)),
            project: project.into(), agent: AgentKind::Claude, session_key: "k".into(),
            kind: StreamKind::User, summary: format!("{project}@{at:?}"), detail: None, is_error: false,
        }
    }

    #[test]
    fn merge_sorts_ascending_and_caps() {
        let idx = ActivityIndex::from_events(vec![ev(Some(3),"a"), ev(Some(1),"b"), ev(Some(2),"a")]);
        let times: Vec<_> = idx.events().iter().map(|e| e.timestamp).collect();
        assert!(times.windows(2).all(|w| w[0] <= w[1]), "ascending by time");
    }
```
And a cap test using a `sort_and_cap` free function if you factor one out (optional). Keep `from_events`
sorting ascending by `timestamp` (None first) on construction.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib activity::tests::merge_sorts`
Expected: FAIL — `from_events` not defined / not sorting.

- [ ] **Step 3: Implement `from_events` and `build`**

`from_events`: store events, sort ascending by `timestamp` (use `sort_by(|a,b| a.timestamp.cmp(&b.timestamp))`). `events()` returns the slice.

`build`:
```rust
pub fn build(snapshot: &AmbientSnapshot, per_session: usize, max_total: usize) -> Self {
    let mut all = Vec::new();
    for session in &snapshot.sessions {
        let Some(path) = session.journal_path.as_ref() else { continue };
        let key = /* metrics::session_key if available, else local format */;
        let project = session.repo_name();
        let id = session.native_id.clone().unwrap_or_default();
        for record in crate::feed::tail_records(path, session.agent, &id, per_session) {
            all.push(event_from_record(&record, &project, session.agent, &key));
        }
    }
    let mut idx = Self::from_events(all);
    if idx.events.len() > max_total {
        let cut = idx.events.len() - max_total;
        idx.events.drain(0..cut); // keep newest max_total
    }
    idx
}
```
Use `use crate::app::AmbientSnapshot;` at top.

- [ ] **Step 4: Run tests + clippy + commit**

Run: `cargo test --lib activity::tests && cargo clippy --all-targets --all-features -- -D warnings`
```bash
git add src/activity.rs
git commit -m "feat: ActivityIndex merges journal tails across sessions"
```

---

### Task 4: Filters (pure logic)

**Files:**
- Modify: `src/activity.rs`
- Test: `src/activity.rs` tests

**Interfaces:**
- Produces: `pub fn filter_events<'a>(events: &'a [StreamEvent], project: Option<&str>, errors_only: bool) -> Vec<&'a StreamEvent>`

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn filters_by_project_and_errors() {
        let mut a = ev(Some(1), "a"); a.is_error = true;
        let b = ev(Some(2), "b");
        let events = vec![a, b];
        assert_eq!(filter_events(&events, Some("a"), false).len(), 1);
        assert_eq!(filter_events(&events, None, true).len(), 1);
        assert_eq!(filter_events(&events, Some("b"), true).len(), 0);
    }
```

- [ ] **Step 2: Run / fail / implement / pass**

Run: `cargo test --lib activity::tests::filters` → FAIL (missing fn). Implement:
```rust
pub fn filter_events<'a>(events: &'a [StreamEvent], project: Option<&str>, errors_only: bool) -> Vec<&'a StreamEvent> {
    events.iter()
        .filter(|e| project.map(|p| e.project == p).unwrap_or(true))
        .filter(|e| !errors_only || e.is_error)
        .collect()
}
```
Run again → PASS.

- [ ] **Step 3: Commit**

```bash
git add src/activity.rs
git commit -m "feat: project/errors filters for the activity stream"
```

---

### Task 5: Monitor preview (replace `render_activity`)

**Files:**
- Modify: `src/dashboard.rs` (`render_activity` and its call in `draw_monitor`), `src/app.rs` (the `ActivityIndex` is built where the snapshot is consumed — pass it in)
- Test: smoke via TestBackend.

**Interfaces:**
- Consumes: `ActivityIndex::events`.
- Produces: `fn render_activity_preview(frame: &mut Frame<'_>, index: &crate::activity::ActivityIndex, area: Rect)`.

- [ ] **Step 1: Build the index in `run_loop`**

In `src/dashboard.rs::run_loop`, after each snapshot is received/created, build the index:
`let activity_index = crate::activity::ActivityIndex::build(&snapshot, 40, 500);` and store it alongside
`snapshot` (rebuild on refresh and on each worker snapshot). Add `activity_index: &ActivityIndex` to
`DrawContext`.

- [ ] **Step 2: Write the failing smoke test + implement preview**

Add a `render_activity_preview` that renders the last K (e.g. area height) events as
`{HH:MM} {project} {glyph} {summary}` lines colored by project/kind, with a title `activity` and a
hint `s: stream`. Replace the `render_activity(frame, snapshot, vertical[..])` call in `draw_monitor`
with `render_activity_preview(frame, activity_index, <activity row>)`. Delete the old
`render_activity` and `app::activity_lines`/its use if now unused (verify nothing else references
`snapshot.activity`; if the snapshot still populates `activity`, leave the field but stop rendering it).
Smoke test: render into TestBackend(80, 6) with an index from `from_events`, assert the buffer contains
a project name. (`ActivityIndex::from_events` is public from Task 3.)

- [ ] **Step 3: Run smoke + clippy + manual + commit**

Run: `cargo test --lib && cargo clippy --all-targets --all-features -- -D warnings`; `cargo run` to eyeball.
```bash
git add src/dashboard.rs src/app.rs
git commit -m "feat: cross-project activity preview in the monitor"
```

---

### Task 6: `ViewMode::Stream` — scroll, expand, filter

**Files:**
- Modify: `src/dashboard.rs` (`ViewMode` enum, `handle_key`, `draw` dispatch, new `draw_stream`)
- Test: pure expand/selection logic test + manual for rendering.

**Interfaces:**
- Consumes: `ActivityIndex`, `filter_events`, `feed_scroll_offset` (existing), `diffview::render_file_edit` (existing).
- Produces: `ViewMode::Stream { selected: usize, scroll: usize, follow: bool, expanded: std::collections::BTreeSet<usize>, project_filter: Option<String>, errors_only: bool }`.

- [ ] **Step 1: Extend `ViewMode` + key entry**

Add the `Stream { .. }` variant. In `handle_key`, from `ViewMode::Monitor` add `KeyCode::Char('s') =>`
set `mode = ViewMode::Stream { selected: 0, scroll: 0, follow: true, expanded: BTreeSet::new(),
project_filter: None, errors_only: false }`. In `ViewMode::Stream`, `Esc` → `Monitor`. Ensure all
existing `match mode` sites stay exhaustive (the `Esc` arm and any `ViewMode::Tail`/`Monitor` matches).

- [ ] **Step 2: Stream key bindings (TDD the pure parts)**

Add bindings handled when `mode` is `Stream`: `j`/`k` move `selected` (clamp to filtered len), with
`k` setting `follow=false`; `G` → follow=true; `g` → follow=false, selected=0; `PageUp`/`PageDown`;
`enter`/`Right` toggle `expanded.contains(selected)`; `Left` removes from `expanded`; `p` cycles
`project_filter` across the distinct projects present; `e` toggles `errors_only`. Factor the
project-cycle into a pure helper and test it:
```rust
    #[test]
    fn project_cycle_wraps_through_all_then_none() {
        let projects = vec!["a".to_string(), "b".to_string()];
        assert_eq!(super::next_project_filter(None, &projects), Some("a".to_string()));
        assert_eq!(super::next_project_filter(Some("a".into()), &projects), Some("b".to_string()));
        assert_eq!(super::next_project_filter(Some("b".into()), &projects), None);
    }
```
Implement `fn next_project_filter(current: Option<String>, projects: &[String]) -> Option<String>`
(None → first; last → None; else next). Run RED→GREEN.

- [ ] **Step 3: Implement `draw_stream`**

Render the filtered events (`filter_events(index.events(), project_filter.as_deref(), errors_only)`):
- Build a `Vec<Line>`: for each event, a one-line row `{HH:MM} {project} {glyph} {summary}` (colored
  by kind; error rows red; selected row reverse/highlighted). If the event index is in `expanded`,
  append its detail below — `StreamDetail::Text` as wrapped/`truncate`d lines (prefixed `  │ `),
  `StreamDetail::FileEdit` via `crate::diffview::render_file_edit(path, hunks, width)`.
- Scroll with `feed_scroll_offset(follow, scroll, lines.len(), viewport)` (compute viewport from the
  area inner height, as `draw_tail` does).
- A footer line: `{n} events · filter: {project|all} · errors:{on/off} · s/esc back`.
- Never panic at tiny sizes.

- [ ] **Step 4: Dispatch + manual verify**

Add the `ViewMode::Stream { .. } => draw_stream(...)` arm to `draw`. `cargo build`, then drive via tmux:
press `s` from the monitor → stream opens following the bottom; `j/k` move selection; `enter` expands a
FileEdit row into a colored diff and a result row into its body; `p`/`e` filter; `esc` returns. Confirm
no corruption and no panic on resize.

- [ ] **Step 5: Clippy + suite + commit**

Run: `cargo clippy --all-targets --all-features -- -D warnings && cargo test`
```bash
git add src/dashboard.rs
git commit -m "feat: full-screen scrollable, expandable activity stream view"
```

---

### Task 7: Document the stream controls

**Files:**
- Modify: `README.md` (Controls), `src/dashboard.rs` (monitor footer hint if it lists keys)

- [ ] **Step 1:** Add a "Stream view" controls section to `README.md`: `s` opens it from the monitor; `j`/`k` select; `enter`/`→` expand, `←` collapse; `p` cycles project filter; `e` errors-only; `G`/`g` bottom/top; `esc` back. Note the monitor's bottom panel is now a cross-project activity preview.
- [ ] **Step 2:** If `monitor_footer` lists keys, add `s stream`. `cargo build` to confirm no breakage.
- [ ] **Step 3: Commit**
```bash
git add README.md src/dashboard.rs
git commit -m "docs: activity stream controls"
```

---

## Self-Review

**Spec coverage:** ActivityIndex/StreamEvent (Tasks 1-3) ✓; monitor preview (Task 5) ✓; full Stream view with scroll/expand/filter (Task 6) ✓; reuse diffview + feed_scroll_offset ✓; bounded reads (per_session=40, max_total=500) ✓; sanitize_inline on text ✓; docs (Task 7) ✓.

**Placeholder scan:** Logic tasks (1-4, 6 Step 2) have complete code + tests. Render tasks (5, 6 Step 3) give signatures + behavior + smoke/manual verification, consistent with the spec's manual-visual posture. The `session_key` source is explicitly guarded for the A-not-present case.

**Type consistency:** `StreamEvent`/`StreamKind`/`StreamDetail`/`ActivityIndex`/`event_from_record`/`filter_events`/`next_project_filter`/`tail_records` are used identically across tasks. `ViewMode::Stream` field set is fixed in Task 6 Step 1 and consumed unchanged in Steps 2-3.
