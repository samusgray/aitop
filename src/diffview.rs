use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use std::sync::OnceLock;

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use similar::{ChangeTag, TextDiff};
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

fn emphasis_bg(kind: DiffLineKind) -> Option<Color> {
    match kind {
        DiffLineKind::Added => Some(Color::Rgb(30, 90, 40)),
        DiffLineKind::Removed => Some(Color::Rgb(110, 35, 35)),
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

pub fn collapse(mut lines: Vec<DiffLine>, max: usize) -> (Vec<DiffLine>, usize) {
    if lines.len() <= max {
        return (lines, 0);
    }
    let hidden = lines.len() - max;
    lines.truncate(max);
    (lines, hidden)
}

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

fn render_file_edit_uncached(path: &str, hunks: &[FileEditHunk], width: usize) -> Vec<Line<'static>> {
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
            let line_text = format!("{text}\n");
            let highlighted = highlighter
                .highlight_line(&line_text, syntaxes)
                .unwrap_or_default();
            // Per-character emphasis flags for this line (parallel to `text`).
            let emphasis: Vec<bool> = diff_line
                .segs
                .iter()
                .flat_map(|seg| std::iter::repeat_n(seg.emphasized, seg.text.chars().count()))
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

#[cfg(test)]
mod tests {
    use super::*;

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

    fn ctx_line(n: usize) -> DiffLine {
        DiffLine {
            kind: DiffLineKind::Context,
            old_no: Some(n),
            new_no: Some(n),
            segs: vec![DiffSeg { text: format!("line {n}"), emphasized: false }],
        }
    }

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
}
