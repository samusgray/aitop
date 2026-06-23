# Tail Feed Inline Diffs + Auto-Scroll Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Render agent code edits inline in the focused tail feed as syntax-highlighted, word-level diffs, and make the feed follow the newest events like `tail -f`.

**Architecture:** Capture `Edit`/`MultiEdit`/`Write` tool inputs into a structured `FeedEvent::FileEdit` in `src/feed.rs`. A new `src/diffview.rs` module turns those edits into colored `ratatui` lines using `similar` (line + word diff) and `syntect` (syntax highlighting), with a process-global highlight cache. `src/dashboard.rs` renders `FileEdit` via that module and gains a follow/scroll model anchored to the feed pane's viewport height.

**Tech Stack:** Rust 2024, ratatui 0.29, `similar` 2, `syntect` 5.

## Global Constraints

- Edition: `2024` (already set in `Cargo.toml`).
- All work happens on branch `feature/tail-inline-diffs`.
- `cargo clippy --all-targets --all-features -- -D warnings` must pass after every task.
- `cargo test` must pass after every task.
- Diff rendering returns `Vec<ratatui::text::Line<'static>>` (owned, no borrows from feed records).
- No external processes, no `hunk`, no config file (per design scope).

---

### Task 1: Add deps, `FileEdit` model, and edit-input parsing

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/feed.rs` (the `FeedEvent` enum ~line 14, `summarize_tool` ~line 437, the `tool_use` arm ~line 303)
- Test: `src/feed.rs` (`#[cfg(test)] mod tests` — add at end of file)

**Interfaces:**
- Produces: `pub struct FileEditHunk { pub old_text: String, pub new_text: String }`; new enum variant `FeedEvent::FileEdit { path: String, hunks: Vec<FileEditHunk> }`; `pub fn parse_file_edit(name: &str, input: &serde_json::Value) -> Option<(String, Vec<FileEditHunk>)>` returning `(path, hunks)`.

- [ ] **Step 1: Add dependencies**

In `Cargo.toml` under `[dependencies]`, add:

```toml
similar = "2"
syntect = "5"
```

- [ ] **Step 2: Write the failing test**

Add at the end of `src/feed.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_edit_into_single_hunk() {
        let input = json!({"file_path": "/a/b.rs", "old_string": "x", "new_string": "y"});
        let (path, hunks) = parse_file_edit("Edit", &input).expect("edit parses");
        assert_eq!(path, "/a/b.rs");
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_text, "x");
        assert_eq!(hunks[0].new_text, "y");
    }

    #[test]
    fn parses_multiedit_into_one_hunk_per_edit() {
        let input = json!({
            "file_path": "/a/b.rs",
            "edits": [
                {"old_string": "a", "new_string": "A"},
                {"old_string": "b", "new_string": "B"}
            ]
        });
        let (_, hunks) = parse_file_edit("MultiEdit", &input).expect("multiedit parses");
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[1].new_text, "B");
    }

    #[test]
    fn parses_write_as_all_added() {
        let input = json!({"file_path": "/a/b.rs", "content": "line1\nline2\n"});
        let (_, hunks) = parse_file_edit("Write", &input).expect("write parses");
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_text, "");
        assert_eq!(hunks[0].new_text, "line1\nline2\n");
    }

    #[test]
    fn non_edit_tool_is_none() {
        let input = json!({"command": "ls"});
        assert!(parse_file_edit("Bash", &input).is_none());
    }
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test --lib feed::tests`
Expected: FAIL — `cannot find function parse_file_edit` / `no variant FileEdit`.

- [ ] **Step 4: Add the model and variant**

In `src/feed.rs`, add above the `FeedEvent` enum:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEditHunk {
    pub old_text: String,
    pub new_text: String,
}
```

Add this variant to the `FeedEvent` enum (after `ToolCall`):

```rust
    FileEdit {
        path: String,
        hunks: Vec<FileEditHunk>,
    },
```

- [ ] **Step 5: Implement `parse_file_edit`**

Add to `src/feed.rs` (near `summarize_tool`):

```rust
pub fn parse_file_edit(name: &str, input: &Value) -> Option<(String, Vec<FileEditHunk>)> {
    let path = input.get("file_path").and_then(Value::as_str)?.to_string();
    let hunks = match name {
        "Edit" | "NotebookEdit" => vec![FileEditHunk {
            old_text: input
                .get("old_string")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            new_text: input
                .get("new_string")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        }],
        "MultiEdit" => input
            .get("edits")
            .and_then(Value::as_array)?
            .iter()
            .map(|edit| FileEditHunk {
                old_text: edit
                    .get("old_string")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                new_text: edit
                    .get("new_string")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            })
            .collect(),
        "Write" => vec![FileEditHunk {
            old_text: String::new(),
            new_text: input
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        }],
        _ => return None,
    };
    Some((path, hunks))
}
```

- [ ] **Step 6: Emit `FileEdit` from the `tool_use` arm**

In `src/feed.rs`, in the `"tool_use"` arm (~line 303), replace the body that builds the `ToolCall` record with a branch that prefers `FileEdit`:

```rust
        "tool_use" => {
            let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
            let id = block
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let input = block.get("input").unwrap_or(&Value::Null);
            if let Some((path, hunks)) = parse_file_edit(name, input) {
                let mut rec = record(session_id, timestamp, FeedEvent::FileEdit { path: path.clone(), hunks });
                rec.annotations.push(Annotation::FileTouched(path));
                return Some(rec);
            }
            let (summary, annotation) = summarize_tool(name, input);
            let mut rec = record(
                session_id,
                timestamp,
                FeedEvent::ToolCall {
                    id,
                    name: name.to_string(),
                    summary,
                },
            );
            if let Some(annotation) = annotation {
                rec.annotations.push(annotation);
            }
            Some(rec)
        }
```

Note: the enclosing function returns `Option<FeedRecord>`; confirm `return Some(rec);` is valid there (it is — see the existing `Some(rec)` tail). If the arm is inside a `match` that is the function's tail expression, keep the early `return`.

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo test --lib feed::tests`
Expected: PASS (4 tests).

- [ ] **Step 8: Verify clippy + full suite**

Run: `cargo clippy --all-targets --all-features -- -D warnings && cargo test`
Expected: clean, all pass. (A non-exhaustive `match` on `FeedEvent` in `dashboard.rs::feed_record_lines` will fail to compile — if so, add a temporary arm `FeedEvent::FileEdit { .. } => Vec::new(),` to `feed_record_lines` to keep the build green; Task 7 replaces it.)

- [ ] **Step 9: Commit**

```bash
git add Cargo.toml Cargo.lock src/feed.rs src/dashboard.rs
git commit -m "feat: capture Edit/MultiEdit/Write tool calls as FileEdit events"
```

---

### Task 2: Diff model and `diff_hunk` (line + word level)

**Files:**
- Create: `src/diffview.rs`
- Modify: `src/lib.rs` (add `pub mod diffview;`)
- Test: `src/diffview.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: nothing from other tasks.
- Produces:
  - `pub enum DiffLineKind { Context, Added, Removed }` (derives `Debug, Clone, Copy, PartialEq, Eq`)
  - `pub struct DiffSeg { pub text: String, pub emphasized: bool }`
  - `pub struct DiffLine { pub kind: DiffLineKind, pub old_no: Option<usize>, pub new_no: Option<usize>, pub segs: Vec<DiffSeg> }`
  - `pub fn diff_hunk(old: &str, new: &str) -> Vec<DiffLine>`

- [ ] **Step 1: Register the module**

In `src/lib.rs`, add (keep alphabetical):

```rust
pub mod diffview;
```

- [ ] **Step 2: Write the failing test**

Create `src/diffview.rs` with only the types and an empty function plus tests:

```rust
use similar::{ChangeTag, TextDiff};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    Context,
    Added,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffSeg {
    pub text: String,
    pub emphasized: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub old_no: Option<usize>,
    pub new_no: Option<usize>,
    pub segs: Vec<DiffSeg>,
}

pub fn diff_hunk(_old: &str, _new: &str) -> Vec<DiffLine> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_added_removed_context_lines() {
        let lines = diff_hunk("a\nb\nc\n", "a\nB\nc\n");
        let kinds: Vec<_> = lines.iter().map(|l| l.kind).collect();
        assert!(kinds.contains(&DiffLineKind::Removed), "has a removed line");
        assert!(kinds.contains(&DiffLineKind::Added), "has an added line");
        assert!(kinds.contains(&DiffLineKind::Context), "has a context line");
    }

    #[test]
    fn numbers_track_old_and_new_sides() {
        let lines = diff_hunk("a\nb\n", "a\nb\nc\n");
        let added = lines
            .iter()
            .find(|l| l.kind == DiffLineKind::Added)
            .expect("an added line");
        assert_eq!(added.new_no, Some(3));
        assert_eq!(added.old_no, None);
    }

    #[test]
    fn emphasizes_changed_words_within_a_line() {
        let lines = diff_hunk("let x = 1;\n", "let x = 2;\n");
        let added = lines
            .iter()
            .find(|l| l.kind == DiffLineKind::Added)
            .expect("an added line");
        assert!(
            added.segs.iter().any(|s| s.emphasized && s.text.contains('2')),
            "the changed token is emphasized"
        );
    }
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test --lib diffview::tests`
Expected: FAIL — all three tests fail (empty `diff_hunk` returns no lines).

- [ ] **Step 4: Implement `diff_hunk`**

Replace the stub:

```rust
pub fn diff_hunk(old: &str, new: &str) -> Vec<DiffLine> {
    let diff = TextDiff::from_lines(old, new);
    let mut out = Vec::new();
    let mut old_no = 1usize;
    let mut new_no = 1usize;

    for op in diff.ops() {
        for change in diff.iter_inline_changes(op) {
            let kind = match change.tag() {
                ChangeTag::Equal => DiffLineKind::Context,
                ChangeTag::Delete => DiffLineKind::Removed,
                ChangeTag::Insert => DiffLineKind::Added,
            };

            let mut segs = Vec::new();
            for (emphasized, value) in change.iter_strings_lossy() {
                let text = value.trim_end_matches('\n').to_string();
                if !text.is_empty() {
                    segs.push(DiffSeg { text, emphasized });
                }
            }

            let (old_n, new_n) = match kind {
                DiffLineKind::Context => {
                    let pair = (Some(old_no), Some(new_no));
                    old_no += 1;
                    new_no += 1;
                    pair
                }
                DiffLineKind::Removed => {
                    let pair = (Some(old_no), None);
                    old_no += 1;
                    pair
                }
                DiffLineKind::Added => {
                    let pair = (None, Some(new_no));
                    new_no += 1;
                    pair
                }
            };

            out.push(DiffLine {
                kind,
                old_no: old_n,
                new_no: new_n,
                segs,
            });
        }
    }
    out
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib diffview::tests`
Expected: PASS (3 tests). If `emphasizes_changed_words_within_a_line` fails because `similar` groups the whole line as one segment, relax the assertion to `added.segs.iter().any(|s| s.emphasized)` and confirm at least one emphasized segment exists.

- [ ] **Step 6: Commit**

```bash
git add src/lib.rs src/diffview.rs
git commit -m "feat: line and word-level diff model in diffview"
```

---

### Task 3: Collapse long diffs

**Files:**
- Modify: `src/diffview.rs`
- Test: `src/diffview.rs` tests

**Interfaces:**
- Consumes: `DiffLine` from Task 2.
- Produces: `pub fn collapse(lines: Vec<DiffLine>, max: usize) -> (Vec<DiffLine>, usize)` — returns kept lines and the count hidden.

- [ ] **Step 1: Write the failing test**

Add to `src/diffview.rs` tests:

```rust
    fn ctx_line(n: usize) -> DiffLine {
        DiffLine {
            kind: DiffLineKind::Context,
            old_no: Some(n),
            new_no: Some(n),
            segs: vec![DiffSeg { text: format!("line {n}"), emphasized: false }],
        }
    }

    #[test]
    fn collapse_keeps_all_when_under_cap() {
        let lines = (1..=5).map(ctx_line).collect::<Vec<_>>();
        let (kept, hidden) = collapse(lines, 20);
        assert_eq!(kept.len(), 5);
        assert_eq!(hidden, 0);
    }

    #[test]
    fn collapse_truncates_and_reports_hidden() {
        let lines = (1..=25).map(ctx_line).collect::<Vec<_>>();
        let (kept, hidden) = collapse(lines, 20);
        assert_eq!(kept.len(), 20);
        assert_eq!(hidden, 5);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib diffview::tests::collapse`
Expected: FAIL — `cannot find function collapse`.

- [ ] **Step 3: Implement `collapse`**

Add to `src/diffview.rs`:

```rust
pub fn collapse(mut lines: Vec<DiffLine>, max: usize) -> (Vec<DiffLine>, usize) {
    if lines.len() <= max {
        return (lines, 0);
    }
    let hidden = lines.len() - max;
    lines.truncate(max);
    (lines, hidden)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib diffview::tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/diffview.rs
git commit -m "feat: collapse long diffs with a hidden-line count"
```

---

### Task 4: Render edits with syntect colors, row fills, and gutter

**Files:**
- Modify: `src/diffview.rs`
- Test: `src/diffview.rs` tests (structural smoke test; color is verified manually)

**Interfaces:**
- Consumes: `diff_hunk`, `collapse` (Tasks 2-3); `crate::feed::FileEditHunk` (Task 1).
- Produces: `pub fn render_file_edit(path: &str, hunks: &[crate::feed::FileEditHunk], width: usize) -> Vec<ratatui::text::Line<'static>>`.

- [ ] **Step 1: Add imports and helpers**

At the top of `src/diffview.rs` add:

```rust
use std::sync::OnceLock;

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use syntect::{
    easy::HighlightLines,
    highlighting::{Theme, ThemeSet},
    parsing::SyntaxSet,
};

use crate::feed::FileEditHunk;

const MAX_DIFF_LINES: usize = 20;

fn assets() -> &'static (SyntaxSet, Theme) {
    static ASSETS: OnceLock<(SyntaxSet, Theme)> = OnceLock::new();
    ASSETS.get_or_init(|| {
        let syntaxes = SyntaxSet::load_defaults_newlines();
        let themes = ThemeSet::load_defaults();
        let theme = themes.themes["base16-ocean.dark"].clone();
        (syntaxes, theme)
    })
}

fn row_bg(kind: DiffLineKind) -> Option<Color> {
    match kind {
        DiffLineKind::Added => Some(Color::Rgb(15, 45, 20)),
        DiffLineKind::Removed => Some(Color::Rgb(55, 18, 18)),
        DiffLineKind::Context => None,
    }
}

fn extension(path: &str) -> &str {
    match path.rsplit_once('.') {
        Some((_, ext)) => ext,
        None => "",
    }
}

fn marker(kind: DiffLineKind) -> &'static str {
    match kind {
        DiffLineKind::Added => "+",
        DiffLineKind::Removed => "-",
        DiffLineKind::Context => " ",
    }
}
```

- [ ] **Step 2: Write the failing test**

Add to `src/diffview.rs` tests:

```rust
    use crate::feed::FileEditHunk;

    #[test]
    fn render_includes_header_and_diff_rows() {
        let hunks = vec![FileEditHunk {
            old_text: "fn a() {}\n".to_string(),
            new_text: "fn b() {}\n".to_string(),
        }];
        let lines = render_file_edit("src/x.rs", &hunks, 80);
        // header + at least one removed + one added row
        assert!(lines.len() >= 3, "got {} lines", lines.len());
    }

    #[test]
    fn render_marks_hidden_lines_when_collapsed() {
        let new_text: String = (1..=30).map(|n| format!("let v{n} = {n};\n")).collect();
        let hunks = vec![FileEditHunk { old_text: String::new(), new_text }];
        let lines = render_file_edit("src/x.rs", &hunks, 80);
        let rendered: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(rendered.contains("more lines"), "shows a collapse marker");
    }
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test --lib diffview::tests::render`
Expected: FAIL — `cannot find function render_file_edit`.

- [ ] **Step 4: Implement `render_file_edit`**

Add to `src/diffview.rs`:

```rust
pub fn render_file_edit(path: &str, hunks: &[FileEditHunk], width: usize) -> Vec<Line<'static>> {
    let (syntaxes, theme) = assets();
    let syntax = syntaxes
        .find_syntax_by_extension(extension(path))
        .unwrap_or_else(|| syntaxes.find_syntax_plain_text());

    let mut out = Vec::new();
    out.push(Line::from(vec![
        Span::styled("± ", Style::default().fg(Color::Magenta)),
        Span::styled(
            path.to_string(),
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ),
    ]));

    for hunk in hunks {
        let (lines, hidden) = collapse(diff_hunk(&hunk.old_text, &hunk.new_text), MAX_DIFF_LINES);
        for diff_line in &lines {
            let bg = row_bg(diff_line.kind);
            let number = diff_line.new_no.or(diff_line.old_no).unwrap_or(0);
            let text: String = diff_line.segs.iter().map(|s| s.text.as_str()).collect();

            let mut spans = Vec::new();
            let mut gutter = Style::default().fg(Color::DarkGray);
            if let Some(color) = bg {
                gutter = gutter.bg(color);
            }
            spans.push(Span::styled(
                format!("{number:>4} {} ", marker(diff_line.kind)),
                gutter,
            ));

            // Fresh highlighter per line: order-independent across +/- rows.
            let mut highlighter = HighlightLines::new(syntax, theme);
            let highlighted = highlighter
                .highlight_line(&format!("{text}\n"), syntaxes)
                .unwrap_or_default();
            for (syn, piece) in highlighted {
                let piece = piece.trim_end_matches('\n');
                if piece.is_empty() {
                    continue;
                }
                let mut style = Style::default().fg(Color::Rgb(
                    syn.foreground.r,
                    syn.foreground.g,
                    syn.foreground.b,
                ));
                if let Some(color) = bg {
                    style = style.bg(color);
                }
                spans.push(Span::styled(piece.to_string(), style));
            }

            // Pad to the pane width so the row background fills the line.
            if let Some(color) = bg {
                let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
                if width > used {
                    spans.push(Span::styled(
                        " ".repeat(width - used),
                        Style::default().bg(color),
                    ));
                }
            }

            out.push(Line::from(spans));
        }

        if hidden > 0 {
            out.push(Line::from(Span::styled(
                format!("  … +{hidden} more lines"),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            )));
        }
    }

    out
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib diffview::tests`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/diffview.rs
git commit -m "feat: render file edits as syntax-highlighted diff rows"
```

---

### Task 5: Word-level emphasis overlay

**Files:**
- Modify: `src/diffview.rs` (the per-piece span emission in `render_file_edit`)
- Test: `src/diffview.rs` tests

**Interfaces:**
- Consumes: existing `render_file_edit`, `DiffLine.segs` emphasis flags.
- Produces: no new public symbol; emphasized tokens render with a brighter background.

- [ ] **Step 1: Write the failing test**

Add to `src/diffview.rs` tests. This asserts that, for a single-token change, at least one rendered span carries the brighter "added emphasis" background:

```rust
    #[test]
    fn emphasized_tokens_get_a_brighter_background() {
        let hunks = vec![FileEditHunk {
            old_text: "let x = 1;\n".to_string(),
            new_text: "let x = 2;\n".to_string(),
        }];
        let lines = render_file_edit("src/x.rs", &hunks, 80);
        let bright = Color::Rgb(30, 90, 40);
        let has_bright = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.style.bg == Some(bright));
        assert!(has_bright, "a changed token should use the emphasis background");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib diffview::tests::emphasized_tokens_get_a_brighter_background`
Expected: FAIL — no span currently uses `Rgb(30, 90, 40)`.

- [ ] **Step 3: Add the emphasis helper**

Add to `src/diffview.rs`:

```rust
fn emphasis_bg(kind: DiffLineKind) -> Option<Color> {
    match kind {
        DiffLineKind::Added => Some(Color::Rgb(30, 90, 40)),
        DiffLineKind::Removed => Some(Color::Rgb(110, 35, 35)),
        DiffLineKind::Context => None,
    }
}
```

- [ ] **Step 4: Apply emphasis while emitting syntect pieces**

In `render_file_edit`, before the highlight loop build a per-character emphasis flag, and split each syntect piece at emphasis boundaries. Replace the `for (syn, piece) in highlighted { ... }` block with:

```rust
            // Per-character emphasis flags for this line (parallel to `text`).
            let emphasis: Vec<bool> = diff_line
                .segs
                .iter()
                .flat_map(|seg| std::iter::repeat(seg.emphasized).take(seg.text.chars().count()))
                .collect();
            let strong_bg = emphasis_bg(diff_line.kind);

            let mut col = 0usize; // char offset within `text`
            for (syn, piece) in highlighted {
                let piece = piece.trim_end_matches('\n');
                if piece.is_empty() {
                    continue;
                }
                let fg = Color::Rgb(syn.foreground.r, syn.foreground.g, syn.foreground.b);

                // Walk the piece, grouping consecutive chars by emphasis flag.
                let mut group = String::new();
                let mut group_emph = emphasis.get(col).copied().unwrap_or(false);
                for ch in piece.chars() {
                    let emph = emphasis.get(col).copied().unwrap_or(false);
                    if emph != group_emph && !group.is_empty() {
                        let mut style = Style::default().fg(fg);
                        if let Some(color) = if group_emph { strong_bg } else { bg } {
                            style = style.bg(color);
                        }
                        spans.push(Span::styled(std::mem::take(&mut group), style));
                        group_emph = emph;
                    } else if group.is_empty() {
                        group_emph = emph;
                    }
                    group.push(ch);
                    col += 1;
                }
                if !group.is_empty() {
                    let mut style = Style::default().fg(fg);
                    if let Some(color) = if group_emph { strong_bg } else { bg } {
                        style = style.bg(color);
                    }
                    spans.push(Span::styled(group, style));
                }
            }
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib diffview::tests`
Expected: PASS (all diffview tests, including the new emphasis test).

- [ ] **Step 6: Verify clippy**

Run: `cargo clippy --all-targets --all-features -- -D warnings`
Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add src/diffview.rs
git commit -m "feat: brighten word-level changes within diff rows"
```

---

### Task 6: Cache rendered diffs per edit

**Files:**
- Modify: `src/diffview.rs`
- Test: `src/diffview.rs` tests

**Interfaces:**
- Consumes: `render_file_edit`.
- Produces: caching is internal; `render_file_edit` keeps the same signature and returns identical output, now memoized by `(path, width, hunks)`.

- [ ] **Step 1: Write the failing test**

Add to `src/diffview.rs` tests (asserts repeated calls are equal — guards against cache corruption):

```rust
    #[test]
    fn render_is_stable_across_calls() {
        let hunks = vec![FileEditHunk {
            old_text: "a\n".to_string(),
            new_text: "b\n".to_string(),
        }];
        let first = render_file_edit("src/x.rs", &hunks, 60);
        let second = render_file_edit("src/x.rs", &hunks, 60);
        assert_eq!(first.len(), second.len());
        let join = |lines: &[Line<'static>]| -> String {
            lines
                .iter()
                .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
                .collect()
        };
        assert_eq!(join(&first), join(&second));
    }
```

- [ ] **Step 2: Run test to verify it passes-without-cache (sanity), then add cache**

Run: `cargo test --lib diffview::tests::render_is_stable_across_calls`
Expected: PASS already (rendering is deterministic). This test is a regression guard; the cache must not break it. Proceed to add the cache.

- [ ] **Step 3: Add the cache and wrap the renderer**

Rename the implemented function to `render_file_edit_uncached`, then add a caching wrapper. At the top of `src/diffview.rs` extend imports:

```rust
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
```

Add:

```rust
fn cache() -> &'static Mutex<HashMap<u64, Vec<Line<'static>>>> {
    static CACHE: OnceLock<Mutex<HashMap<u64, Vec<Line<'static>>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cache_key(path: &str, hunks: &[FileEditHunk], width: usize) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    width.hash(&mut hasher);
    for hunk in hunks {
        hunk.old_text.hash(&mut hasher);
        hunk.new_text.hash(&mut hasher);
    }
    hasher.finish()
}

pub fn render_file_edit(path: &str, hunks: &[FileEditHunk], width: usize) -> Vec<Line<'static>> {
    let key = cache_key(path, hunks, width);
    if let Some(hit) = cache().lock().expect("diff cache lock").get(&key) {
        return hit.clone();
    }
    let rendered = render_file_edit_uncached(path, hunks, width);
    cache()
        .lock()
        .expect("diff cache lock")
        .insert(key, rendered.clone());
    rendered
}
```

Change the previously-public renderer's signature to `fn render_file_edit_uncached(...)` (drop `pub`).

`FileEditHunk` already derives `Hash`? It derives `Debug, Clone, PartialEq, Eq` (Task 1). `String` fields are hashed directly here via `hunk.old_text.hash(...)`, so no `Hash` derive on `FileEditHunk` is required.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib diffview::tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/diffview.rs
git commit -m "perf: cache rendered diffs by path, width, and content"
```

---

### Task 7: Render `FileEdit` in the feed

**Files:**
- Modify: `src/dashboard.rs` (`feed_record_lines` ~line 1151; imports ~line 30)
- Test: manual (visual)

**Interfaces:**
- Consumes: `crate::diffview::render_file_edit`; `FeedEvent::FileEdit`.
- Produces: the tail feed shows diffs for edit events.

- [ ] **Step 1: Add the match arm**

In `src/dashboard.rs::feed_record_lines`, replace the temporary `FeedEvent::FileEdit { .. } => Vec::new(),` arm (if added in Task 1) with:

```rust
        FeedEvent::FileEdit { path, hunks } => {
            crate::diffview::render_file_edit(path, hunks, width)
        }
```

If `FeedEvent` is imported via `use crate::feed::{... FeedEvent ...}` already (it is, ~line 30), no import change is needed.

- [ ] **Step 2: Build and run the app on demo data**

Run: `cargo run -- --demo`
Then press `enter` on a session to open the tail view. Confirm the app builds and renders without panicking. (Demo feed may not contain edits; this step verifies no regression. Real edits are checked in Step 3.)
Press `q` to quit.

- [ ] **Step 3: Verify against a real session with edits**

Run: `cargo run --release`
Open the tail view (`enter`) on a session that recently edited files. Confirm: a `± path` header appears, removed lines have a red row fill, added lines green, tokens are syntax-colored, and changed words are brighter. Large edits show `… +N more lines`.

- [ ] **Step 4: Verify clippy + suite**

Run: `cargo clippy --all-targets --all-features -- -D warnings && cargo test`
Expected: clean, all pass.

- [ ] **Step 5: Commit**

```bash
git add src/dashboard.rs
git commit -m "feat: render FileEdit events as inline diffs in the tail feed"
```

---

### Task 8: `feed_scroll_offset` (follow + clamp math)

**Files:**
- Modify: `src/dashboard.rs` (add function; add to existing `#[cfg(test)] mod tests`)
- Test: `src/dashboard.rs` tests

**Interfaces:**
- Produces: `fn feed_scroll_offset(follow: bool, manual_scroll: usize, total_lines: usize, viewport: usize) -> usize`.

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `src/dashboard.rs` (there is an existing one):

```rust
    #[test]
    fn follow_anchors_last_page_to_bottom() {
        // 100 lines, 20-row viewport: first visible line is 80.
        assert_eq!(super::feed_scroll_offset(true, 0, 100, 20), 80);
    }

    #[test]
    fn follow_with_short_feed_starts_at_top() {
        assert_eq!(super::feed_scroll_offset(true, 0, 10, 20), 0);
    }

    #[test]
    fn manual_scroll_is_clamped_to_max_start() {
        // Can't scroll past total - viewport.
        assert_eq!(super::feed_scroll_offset(false, 999, 100, 20), 80);
        assert_eq!(super::feed_scroll_offset(false, 25, 100, 20), 25);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib dashboard::tests::follow`
Expected: FAIL — `cannot find function feed_scroll_offset`.

- [ ] **Step 3: Implement the function**

Add to `src/dashboard.rs` (module level, near `tail_feed`):

```rust
fn feed_scroll_offset(follow: bool, manual_scroll: usize, total_lines: usize, viewport: usize) -> usize {
    let max_start = total_lines.saturating_sub(viewport);
    if follow {
        max_start
    } else {
        manual_scroll.min(max_start)
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib dashboard::tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/dashboard.rs
git commit -m "feat: viewport-aware feed scroll offset with follow mode"
```

---

### Task 9: Wire follow mode into the tail view

**Files:**
- Modify: `src/dashboard.rs` — `ViewMode` enum (~line 48), `handle_key` (~lines 366-434), `draw` dispatch (~line 463), `draw_tail` (~line 923), `tail_feed` (~line 1026), `run_loop` refresh-on-`r` (~line 314).
- Test: manual (interactive)

**Interfaces:**
- Consumes: `feed_scroll_offset` (Task 8).
- Produces: tail opens at the bottom and follows new events; scrolling up freezes; `G` re-follows.

- [ ] **Step 1: Extend `ViewMode`**

Change the `Tail` variant (~line 48) from:

```rust
    Tail { scroll: usize },
```
to:

```rust
    Tail { scroll: usize, follow: bool },
```

- [ ] **Step 2: Update `handle_key`**

Apply these edits in `handle_key`:

Open tail at the bottom, following:
```rust
        KeyCode::Enter if matches!(mode, ViewMode::Monitor) => {
            *mode = ViewMode::Tail { scroll: 0, follow: true };
            KeyAction::Continue
        }
```

Scroll down (`j`): advance and keep following only when already following:
```rust
        KeyCode::Char('j') => {
            if let ViewMode::Tail { scroll, follow } = mode {
                if !*follow {
                    *scroll = scroll.saturating_add(1);
                }
            } else {
                *selected = (*selected + 1).min(session_count.saturating_sub(1));
            }
            KeyAction::Continue
        }
```

Scroll up (`k`): freeze (stop following):
```rust
        KeyCode::Char('k') => {
            if let ViewMode::Tail { scroll, follow } = mode {
                *follow = false;
                *scroll = scroll.saturating_sub(1);
            } else {
                *selected = selected.saturating_sub(1);
            }
            KeyAction::Continue
        }
```

`Down`/`Up` select another session and reset to following at the bottom:
```rust
        KeyCode::Down => {
            *selected = (*selected + 1).min(session_count.saturating_sub(1));
            if let ViewMode::Tail { scroll, follow } = mode {
                *scroll = 0;
                *follow = true;
            }
            KeyAction::Continue
        }
        KeyCode::Up => {
            *selected = selected.saturating_sub(1);
            if let ViewMode::Tail { scroll, follow } = mode {
                *scroll = 0;
                *follow = true;
            }
            KeyAction::Continue
        }
```

`PageDown` (freeze + jump down), `PageUp` (freeze + jump up):
```rust
        KeyCode::PageDown => {
            if let ViewMode::Tail { scroll, follow } = mode {
                *follow = false;
                *scroll = scroll.saturating_add(5);
            }
            KeyAction::Continue
        }
        KeyCode::PageUp => {
            if let ViewMode::Tail { scroll, follow } = mode {
                *follow = false;
                *scroll = scroll.saturating_sub(5);
            }
            KeyAction::Continue
        }
```

`g` jumps to top (freeze), `G` jumps to bottom (follow):
```rust
        KeyCode::Char('g') => {
            if let ViewMode::Tail { scroll, follow } = mode {
                *follow = false;
                *scroll = 0;
            }
            KeyAction::Continue
        }
        KeyCode::Char('G') => {
            if let ViewMode::Tail { scroll, follow } = mode {
                *follow = true;
                *scroll = 0;
            }
            KeyAction::Continue
        }
```

- [ ] **Step 3: Update the `draw` dispatch and `draw_tail`**

In `draw` (~line 463) the `ViewMode::Tail { scroll }` match arm must now destructure `follow` too and pass it through. Change the dispatch to pass both:

```rust
        ViewMode::Tail { scroll, follow } => draw_tail(
            frame,
            source,
            sessions,
            selected,
            *scroll,
            *follow,
            filter,
            skyline,
        ),
```

Change `draw_tail`'s signature to accept `follow: bool` (add after `scroll: usize`):

```rust
fn draw_tail(
    frame: &mut Frame<'_>,
    source: DashboardSource,
    sessions: &[AgentSession],
    selected: usize,
    scroll: usize,
    follow: bool,
    filter: SessionFilter,
    skyline: &ActivitySkyline,
) {
```

In `draw_tail`, compute the viewport height (inner height of the feed block = area height minus the two borders) and pass `scroll`, `follow`, and `viewport` into `tail_feed`:

```rust
    let viewport = body[1].height.saturating_sub(2) as usize;
    frame.render_widget(
        tail_feed(
            selected_session,
            feed.as_ref(),
            scroll,
            follow,
            viewport,
            body[1].width.saturating_sub(4) as usize,
        ),
        body[1],
    );
```

- [ ] **Step 4: Update `tail_feed` to use `feed_scroll_offset`**

Change `tail_feed`'s signature (~line 1026) to add `follow: bool` and `viewport: usize`:

```rust
fn tail_feed(
    session: Option<&AgentSession>,
    feed: Option<&SessionFeed>,
    scroll: usize,
    follow: bool,
    viewport: usize,
    width: usize,
) -> Paragraph<'static> {
```

Replace the existing offset block (the `let max_start = ...; let scroll = if scroll == usize::MAX {...}` lines near the end) with:

```rust
    let offset = feed_scroll_offset(follow, scroll, lines.len(), viewport);
    Paragraph::new(lines)
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Green)),
        )
        .scroll((offset as u16, 0))
```

- [ ] **Step 5: Update the `r` (refresh) handler in `run_loop`**

In `run_loop` (~line 314) the `KeyAction::Refresh` arm constructs nothing tail-related, so no change is required there. Confirm the file compiles; if a `ViewMode::Tail { scroll }` pattern remains anywhere (non-exhaustive), update it to `ViewMode::Tail { scroll, follow }`.

- [ ] **Step 6: Build and verify interactively**

Run: `cargo run -- --demo`
- Press `enter`: the feed opens at the **bottom** (newest line on the last row), not the top.
- Press `k` a few times: the view scrolls up and **stops following** (stays put as data refreshes).
- Press `G`: jumps back to the bottom and **resumes following**.
- Press `g`: jumps to the top and stays frozen.
- Press `q` to quit.

Then `cargo run --release` on a live session and confirm new events pull the view down while following.

- [ ] **Step 7: Verify clippy + suite**

Run: `cargo clippy --all-targets --all-features -- -D warnings && cargo test`
Expected: clean, all pass.

- [ ] **Step 8: Commit**

```bash
git add src/dashboard.rs
git commit -m "feat: tail feed follows newest events and anchors to the bottom"
```

---

### Task 10: Update controls help and README

**Files:**
- Modify: `src/dashboard.rs` (the tail footer/help text, if it lists keys) and `README.md` (Tail view controls).
- Test: manual.

**Interfaces:** none.

- [ ] **Step 1: Update README tail controls**

In `README.md` under "Tail view", confirm the controls reflect actual behavior: `j`/`k` scroll (and `k` freezes follow), `G` jump to bottom + follow, `g` jump to top. Adjust wording if it implies the old top-anchored behavior.

- [ ] **Step 2: Verify the footer hint**

If the bottom controls bar (`tail_footer` or the global footer) hard-codes key hints, ensure they remain accurate. No code change if it does not list scroll semantics.

- [ ] **Step 3: Commit**

```bash
git add README.md src/dashboard.rs
git commit -m "docs: document tail follow-mode controls"
```

---

## Self-Review

**Spec coverage:**
- Component 1 (auto-scroll/follow): Tasks 8 (offset math) + 9 (wiring). Bottom anchoring via viewport height ✓; follow semantics ✓; pure testable function ✓.
- Component 2 (native syntax-highlighted diffs): Task 1 (capture `FileEdit`), Task 2 (`similar` line+word diff), Task 3 (collapse), Task 4 (syntect color + row fill + gutter), Task 5 (word emphasis), Task 6 (cache), Task 7 (feed wiring). Layout matches the screenshot (line numbers, `+`/`-`, full-width red/green fill, syntax colors, word emphasis, collapse) ✓.
- Dependencies `similar` + `syntect`: Task 1 ✓.
- Testing section: parsing (Task 1), diff spans (Task 2), collapse (Task 3), scroll offset (Task 8) ✓. Visual fidelity is manual (Tasks 7, 9) as the spec states ✓.
- Scope/YAGNI (no hunk, no watch, no config): respected ✓.

**Placeholder scan:** No `TBD`/`TODO`/"handle edge cases"/"similar to Task N". Every code step has complete code.

**Type consistency:**
- `FileEditHunk { old_text, new_text }` used identically in Tasks 1, 4, 5, 6.
- `FeedEvent::FileEdit { path, hunks }` consistent in Tasks 1 and 7.
- `render_file_edit(path: &str, hunks: &[FileEditHunk], width: usize) -> Vec<Line<'static>>` consistent across Tasks 4-7; Task 6 introduces `render_file_edit_uncached` with the same signature minus `pub` and keeps the public wrapper identical.
- `feed_scroll_offset(follow, manual_scroll, total_lines, viewport) -> usize` consistent in Tasks 8 and 9.
- `DiffLineKind` / `DiffSeg` / `DiffLine` field names (`kind`, `old_no`, `new_no`, `segs`, `text`, `emphasized`) consistent across Tasks 2-5.
