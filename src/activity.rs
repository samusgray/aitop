use std::time::SystemTime;

use crate::app::AmbientSnapshot;
use crate::feed::{FeedEvent, FeedRecord, FileEditHunk, sanitize_inline, truncate_summary};
use crate::model::AgentKind;
use crate::pricing::compact_tokens;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamKind {
    User,
    Assistant,
    Thinking,
    Tool,
    Result,
    FileEdit,
    Usage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamDetail {
    Text(String),
    FileEdit { path: String, hunks: Vec<FileEditHunk> },
}

#[derive(Debug, Clone)]
pub struct StreamEvent {
    pub timestamp: Option<SystemTime>,
    pub project: String,
    pub agent: AgentKind,
    pub session_key: String,
    pub kind: StreamKind,
    pub summary: String,
    pub detail: Option<StreamDetail>,
    pub is_error: bool,
}

pub struct ActivityIndex {
    events: Vec<StreamEvent>,
}

impl ActivityIndex {
    pub fn from_events(mut events: Vec<StreamEvent>) -> Self {
        events.sort_by_key(|a| a.timestamp);
        Self { events }
    }

    pub fn events(&self) -> &[StreamEvent] {
        &self.events
    }

    pub fn build(snapshot: &AmbientSnapshot, per_session: usize, max_total: usize) -> Self {
        let mut all = Vec::new();
        for session in &snapshot.sessions {
            let Some(path) = session.journal_path.as_ref() else { continue };
            let key = crate::metrics::session_key(session);
            let project = session.repo_name();
            let id = session.native_id.clone().unwrap_or_default();
            for record in crate::feed::tail_records(path, session.agent, &id, per_session) {
                all.push(event_from_record(&record, &project, session.agent, &key));
            }
        }
        let mut idx = Self::from_events(all);
        if idx.events.len() > max_total {
            let cut = idx.events.len() - max_total;
            idx.events.drain(0..cut);
        }
        idx
    }
}

pub fn event_from_record(
    record: &FeedRecord,
    project: &str,
    agent: AgentKind,
    session_key: &str,
) -> StreamEvent {
    let timestamp = record
        .timestamp
        .as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(SystemTime::from);

    let annotations_has_error = record
        .annotations
        .contains(&crate::feed::Annotation::Error);

    let (kind, summary, detail, is_error) = match &record.event {
        FeedEvent::FileEdit { path, hunks } => (
            StreamKind::FileEdit,
            sanitize_inline(&format!("✎ {path}")),
            Some(StreamDetail::FileEdit {
                path: path.clone(),
                hunks: hunks.clone(),
            }),
            annotations_has_error,
        ),
        FeedEvent::ToolCall { name, summary, .. } => (
            StreamKind::Tool,
            sanitize_inline(&format!("{name} {summary}")),
            None,
            annotations_has_error,
        ),
        FeedEvent::ToolResult { ok, summary, detail, .. } => (
            StreamKind::Result,
            sanitize_inline(summary),
            Some(StreamDetail::Text(detail.clone())),
            !ok || annotations_has_error,
        ),
        FeedEvent::Assistant { text, .. } => (
            StreamKind::Assistant,
            truncate_summary(text, 120),
            Some(StreamDetail::Text(text.clone())),
            annotations_has_error,
        ),
        FeedEvent::Thinking { text } => (
            StreamKind::Thinking,
            truncate_summary(text, 120),
            Some(StreamDetail::Text(text.clone())),
            annotations_has_error,
        ),
        FeedEvent::User { text } => (
            StreamKind::User,
            truncate_summary(text, 120),
            None,
            annotations_has_error,
        ),
        FeedEvent::Usage { input, output, .. } => (
            StreamKind::Usage,
            sanitize_inline(&format!("{} in {} out", compact_tokens(*input), compact_tokens(*output))),
            None,
            annotations_has_error,
        ),
        FeedEvent::Unknown { kind } => (
            StreamKind::Result,
            sanitize_inline(&format!("? {kind}")),
            None,
            annotations_has_error,
        ),
    };

    StreamEvent {
        timestamp,
        project: project.to_string(),
        agent,
        session_key: session_key.to_string(),
        kind,
        summary,
        detail,
        is_error,
    }
}

pub fn filter_events<'a>(events: &'a [StreamEvent], project: Option<&str>, errors_only: bool) -> Vec<&'a StreamEvent> {
    events.iter()
        .filter(|e| project.map(|p| e.project == p).unwrap_or(true))
        .filter(|e| !errors_only || e.is_error)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed::{FeedEvent, FeedRecord, FileEditHunk};
    use crate::model::AgentKind;

    fn ev(at: Option<u64>, project: &str) -> StreamEvent {
        StreamEvent {
            timestamp: at.map(|s| std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(s)),
            project: project.into(),
            agent: AgentKind::Claude,
            session_key: "k".into(),
            kind: StreamKind::User,
            summary: format!("{project}@{at:?}"),
            detail: None,
            is_error: false,
        }
    }

    #[test]
    fn merge_sorts_ascending_and_caps() {
        let idx = ActivityIndex::from_events(vec![ev(Some(3), "a"), ev(Some(1), "b"), ev(Some(2), "a")]);
        let times: Vec<_> = idx.events().iter().map(|e| e.timestamp).collect();
        assert!(times.windows(2).all(|w| w[0] <= w[1]), "ascending by time");
    }

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

    #[test]
    fn timestamp_parses_rfc3339() {
        let mut r = rec(FeedEvent::User { text: "hi".into() });
        r.timestamp = Some("2024-01-15T12:00:00Z".into());
        let e = event_from_record(&r, "p", AgentKind::Claude, "k");
        assert!(e.timestamp.is_some());
    }

    #[test]
    fn usage_formats_token_counts() {
        let r = rec(FeedEvent::Usage { input: 1000, output: 500, cache_read: 0 });
        let e = event_from_record(&r, "p", AgentKind::Claude, "k");
        assert!(matches!(e.kind, StreamKind::Usage));
        assert!(e.summary.contains("in"));
        assert!(e.summary.contains("out"));
    }

    #[test]
    fn unknown_maps_to_result_kind() {
        let r = rec(FeedEvent::Unknown { kind: "mystery".into() });
        let e = event_from_record(&r, "p", AgentKind::Claude, "k");
        assert!(matches!(e.kind, StreamKind::Result));
        assert!(e.summary.contains("mystery"));
    }

    #[test]
    fn filters_by_project_and_errors() {
        let mut a = ev(Some(1), "a"); a.is_error = true;
        let b = ev(Some(2), "b");
        let events = vec![a, b];
        assert_eq!(filter_events(&events, Some("a"), false).len(), 1);
        assert_eq!(filter_events(&events, None, true).len(), 1);
        assert_eq!(filter_events(&events, Some("b"), true).len(), 0);
    }
}
