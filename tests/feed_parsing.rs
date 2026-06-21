use aitop::{
    feed::{
        Annotation, FeedEvent, annotation_summary, load_session_feed, parse_claude_line,
        parse_codex_line,
    },
    pricing::{compact_tokens, short_model},
};

#[test]
fn parses_claude_user_and_assistant_usage_records() {
    let user = r#"{"type":"user","timestamp":"2026-06-21T04:28:16.433Z","message":{"role":"user","content":"Run the tests"}}"#;
    let assistant = r#"{"type":"assistant","timestamp":"2026-06-21T04:28:20.000Z","message":{"role":"assistant","model":"claude-sonnet-4-5","content":[{"type":"text","text":"All tests pass"}],"usage":{"input_tokens":1200,"output_tokens":300,"cache_read_input_tokens":50}}}"#;

    let mut records = parse_claude_line(user, "s1");
    records.extend(parse_claude_line(assistant, "s1"));

    assert!(matches!(records[0].event, FeedEvent::User { .. }));
    assert!(matches!(records[1].event, FeedEvent::Assistant { .. }));
    assert!(matches!(
        records[2].event,
        FeedEvent::Usage {
            input: 1200,
            output: 300,
            cache_read: 50
        }
    ));
}

#[test]
fn annotates_failed_tool_results_and_file_edits() {
    let edit = r#"{"type":"assistant","message":{"role":"assistant","model":"claude-sonnet-4-5","content":[{"type":"tool_use","id":"t1","name":"Edit","input":{"file_path":"src/app.rs"}}]}}"#;
    let failed = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","is_error":true,"content":"boom"}]}}"#;

    let edit_records = parse_claude_line(edit, "s1");
    let fail_records = parse_claude_line(failed, "s1");

    assert!(
        edit_records[0]
            .annotations
            .contains(&Annotation::FileTouched("src/app.rs".into()))
    );
    assert!(fail_records[0].annotations.contains(&Annotation::Error));
    assert_eq!(annotation_summary(&fail_records[0].annotations), "ERR");
}

#[test]
fn load_session_feed_accumulates_tokens_and_cost() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("session.jsonl");
    std::fs::write(
        &path,
        r#"{"type":"assistant","message":{"role":"assistant","model":"claude-sonnet-4-5","content":[{"type":"text","text":"hello"}],"usage":{"input_tokens":1000,"output_tokens":500,"cache_creation_input_tokens":250,"cache_read_input_tokens":125}}}"#,
    )
    .unwrap();

    let feed = load_session_feed(&path, aitop::model::AgentKind::Claude, "s1", 100).unwrap();

    assert_eq!(feed.tokens_in, 1250);
    assert_eq!(feed.tokens_out, 500);
    assert_eq!(feed.cache_read, 125);
    assert!(feed.estimated_cost > 0.0);
    assert_eq!(feed.model.as_deref(), Some("claude-sonnet-4-5"));
}

#[test]
fn load_session_feed_does_not_duplicate_error_annotations() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("session.jsonl");
    std::fs::write(
        &path,
        r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","is_error":true,"content":"boom"}]}}"#,
    )
    .unwrap();

    let feed = load_session_feed(&path, aitop::model::AgentKind::Claude, "s1", 100).unwrap();
    let error_count = feed.records[0]
        .annotations
        .iter()
        .filter(|annotation| matches!(annotation, Annotation::Error))
        .count();

    assert_eq!(error_count, 1);
}

#[test]
fn parses_codex_rollout_message_and_tool_output_shapes() {
    let message = r#"{"timestamp":"2026-06-21T00:00:00Z","type":"event","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"summarize the repo"}]}}"#;
    let tool = r#"{"timestamp":"2026-06-21T00:00:01Z","type":"event","payload":{"type":"function_call_output","call_id":"call-1","output":"PASS tests\n15 passed"}}"#;

    let records = [
        parse_codex_line(message, "thread-1"),
        parse_codex_line(tool, "thread-1"),
    ]
    .concat();

    assert!(matches!(records[0].event, FeedEvent::User { .. }));
    assert!(matches!(
        records[1].event,
        FeedEvent::ToolResult { ok: true, .. }
    ));
}

#[test]
fn compact_display_helpers_match_tail_ui() {
    assert_eq!(compact_tokens(48_200), "48.2k");
    assert_eq!(short_model("claude-sonnet-4-5"), "sonnet-4.5");
}
