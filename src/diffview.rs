use std::collections::HashMap;
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
                // Drop interior control chars / ANSI as well as the trailing
                // newline so a diff line can never corrupt terminal rendering.
                let text = crate::feed::sanitize_inline(&value);
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

const CACHE_CAP: usize = 256;

type CacheKey = (String, usize, Vec<FileEditHunk>);

fn cache() -> &'static Mutex<HashMap<CacheKey, Vec<Line<'static>>>> {
    static CACHE: OnceLock<Mutex<HashMap<CacheKey, Vec<Line<'static>>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn render_file_edit(path: &str, hunks: &[FileEditHunk], width: usize) -> Vec<Line<'static>> {
    let key: CacheKey = (path.to_string(), width, hunks.to_vec());
    if let Some(hit) = cache().lock().expect("diff cache lock").get(&key) {
        return hit.clone();
    }
    let rendered = render_file_edit_uncached(path, hunks, width);
    let mut map = cache().lock().expect("diff cache lock");
    if map.len() >= CACHE_CAP {
        map.clear();
    }
    map.insert(key, rendered.clone());
    rendered
}

/// The 1-based line where a hunk begins in the actual file on disk, so diffs can
/// show real file line numbers. After an edit is applied the file holds the new
/// text, so we locate `new_text`; fall back to `old_text`, then to 1 (relative
/// numbering) if the file is unreadable or the snippet no longer matches.
fn locate_base(path: &str, hunk: &FileEditHunk) -> usize {
    let needle = if !hunk.new_text.is_empty() {
        &hunk.new_text
    } else {
        &hunk.old_text
    };
    if needle.is_empty() {
        return 1;
    }
    let Ok(content) = std::fs::read_to_string(path) else {
        return 1;
    };
    match content.find(needle) {
        Some(idx) => content[..idx].bytes().filter(|&b| b == b'\n').count() + 1,
        None => 1,
    }
}

/// Syntax-highlight a single line of code into colored spans (foreground only,
/// no background). `ext` is the file extension used to pick the grammar; unknown
/// extensions fall back to plain text. Used to highlight code shown in tool
/// results (e.g. a `Read` body) the same way diffs are highlighted.
pub fn highlight_spans(text: &str, ext: &str) -> Vec<Span<'static>> {
    let (syntaxes, theme) = assets();
    let syntax = syntaxes
        .find_syntax_by_extension(ext)
        .unwrap_or_else(|| syntaxes.find_syntax_plain_text());
    let mut highlighter = HighlightLines::new(syntax, theme);
    let line_text = format!("{text}\n");
    let mut spans = Vec::new();
    if let Ok(ranges) = highlighter.highlight_line(&line_text, syntaxes) {
        for (syn, piece) in ranges {
            let piece = piece.trim_end_matches('\n');
            if piece.is_empty() {
                continue;
            }
            spans.push(Span::styled(
                piece.to_string(),
                Style::default().fg(Color::Rgb(
                    syn.foreground.r,
                    syn.foreground.g,
                    syn.foreground.b,
                )),
            ));
        }
    }
    if spans.is_empty() {
        spans.push(Span::raw(text.to_string()));
    }
    spans
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
        // Number the diff from the hunk's real position in the file rather than
        // from 1, so it lines up with editor / Claude Code line numbers.
        let base = locate_base(path, hunk);
        let (lines, hidden) = collapse(diff_hunk(&hunk.old_text, &hunk.new_text), MAX_DIFF_LINES);
        for diff_line in &lines {
            let bg = row_bg(diff_line.kind);
            let number = diff_line
                .new_no
                .or(diff_line.old_no)
                .map(|n| base - 1 + n)
                .unwrap_or(0);
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
                let piece = piece.trim_end_matches(['\r', '\n']);
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

    fn cache_len() -> usize {
        super::cache().lock().unwrap().len()
    }

    #[test]
    fn highlight_spans_colors_rust_tokens() {
        let spans = highlight_spans("pub fn main() {}", "rs");
        let colors: std::collections::HashSet<(u8, u8, u8)> = spans
            .iter()
            .filter_map(|s| match s.style.fg {
                Some(Color::Rgb(r, g, b)) => Some((r, g, b)),
                _ => None,
            })
            .collect();
        assert!(
            colors.len() >= 2,
            "rust code should produce multiple token colors, got {}",
            colors.len()
        );
    }

    #[test]
    fn locate_base_finds_real_line_number() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.rs");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(f, "line1\nline2\nTARGET line\nline4\n").unwrap();
        let hunk = crate::feed::FileEditHunk {
            old_text: String::new(),
            new_text: "TARGET line".to_string(),
        };
        assert_eq!(locate_base(path.to_str().unwrap(), &hunk), 3);
    }

    #[test]
    fn locate_base_falls_back_to_one_when_missing() {
        let hunk = crate::feed::FileEditHunk {
            old_text: "x".to_string(),
            new_text: "NOT IN ANY FILE".to_string(),
        };
        assert_eq!(locate_base("/no/such/path/nope.rs", &hunk), 1);
    }

    #[test]
    fn cache_evicts_when_full() {
        // Insert CACHE_CAP+1 distinct edits (vary new_text). Without a cap the map
        // would grow to CACHE_CAP+1; with the eviction logic it must stay <= CACHE_CAP.
        for i in 0..=super::CACHE_CAP {
            let hunks = vec![FileEditHunk {
                old_text: String::new(),
                new_text: format!("cache_evict_test line {i}\n"),
            }];
            render_file_edit("src/evict_test.rs", &hunks, 80);
        }
        assert!(
            cache_len() <= super::CACHE_CAP,
            "cache grew to {} entries, expected <= {}",
            cache_len(),
            super::CACHE_CAP
        );
    }

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
