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

#[cfg(test)]
mod tests {
    use super::*;

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
}
