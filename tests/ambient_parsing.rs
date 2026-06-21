use std::{fs, path::Path};

use aitop::{
    app::{merge_sessions, policy_for_missing_processes},
    codex::{read_process_manager, read_threads_from_db},
    git::project_name,
    model::{AgentKind, AgentSession, SessionStatus},
    sources::claude::{decode_project_dir, read_claude_project_journals, read_claude_sessions},
};

#[test]
fn claude_sessions_map_pid_files_to_live_sessions() {
    let temp = tempfile::tempdir().unwrap();
    let sessions = temp.path().join("sessions");
    fs::create_dir(&sessions).unwrap();
    fs::write(
        sessions.join("24775.json"),
        r#"{
          "pid": 24775,
          "sessionId": "35212a4f-4c3f-498c-8c34-123fe82f7434",
          "cwd": "/Users/sg/code/atail",
          "startedAt": 1782020000,
          "procStart": "2026-06-21T00:00:00Z",
          "version": "1.0.0",
          "kind": "cli",
          "entrypoint": "claude",
          "status": "running",
          "updatedAt": 1782020300,
          "statusUpdatedAt": 1782020300
        }"#,
    )
    .unwrap();

    let sessions = read_claude_sessions(&sessions).unwrap();

    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].agent, AgentKind::Claude);
    assert_eq!(sessions[0].pid, Some(24775));
    assert_eq!(
        sessions[0].native_id.as_deref(),
        Some("35212a4f-4c3f-498c-8c34-123fe82f7434")
    );
    assert_eq!(sessions[0].cwd, Path::new("/Users/sg/code/atail"));
    assert_eq!(sessions[0].status, SessionStatus::Running);
}

#[test]
fn claude_project_dir_decodes_to_cwd() {
    assert_eq!(
        decode_project_dir("-Users-sg-code-atail"),
        Some("/Users/sg/code/atail".into())
    );
}

#[test]
fn claude_non_terminal_status_still_counts_as_running() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(
        temp.path().join("24775.json"),
        r#"{
          "pid": 24775,
          "sessionId": "session-1",
          "cwd": "/repo",
          "status": "needs_input"
        }"#,
    )
    .unwrap();

    let sessions = read_claude_sessions(temp.path()).unwrap();

    assert_eq!(sessions[0].status, SessionStatus::Running);
}

#[test]
fn claude_sessions_skip_malformed_pid_files() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("bad.json"), "{").unwrap();
    fs::write(
        temp.path().join("good.json"),
        r#"{
          "pid": 24775,
          "sessionId": "session-1",
          "cwd": "/repo",
          "status": "running"
        }"#,
    )
    .unwrap();

    let sessions = read_claude_sessions(temp.path()).unwrap();

    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].native_id.as_deref(), Some("session-1"));
}

#[test]
fn claude_project_journals_yield_recent_sessions() {
    let temp = tempfile::tempdir().unwrap();
    let project_dir = temp.path().join("-Users-sg-code-aitop");
    fs::create_dir(&project_dir).unwrap();
    fs::write(
        project_dir.join("session-1.jsonl"),
        r#"{"type":"user","sessionId":"session-1","cwd":"/Users/sg/code/aitop","timestamp":"2026-06-21T00:00:00Z","gitBranch":"main"}
{"type":"assistant","sessionId":"session-1","cwd":"/Users/sg/code/aitop","timestamp":"2026-06-21T00:00:02Z","message":{"model":"claude-opus-4","usage":{"input_tokens":5,"output_tokens":7}}}"#,
    )
    .unwrap();

    let sessions = read_claude_project_journals(temp.path(), 10).unwrap();

    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].agent, AgentKind::Claude);
    assert_eq!(sessions[0].native_id.as_deref(), Some("session-1"));
    assert_eq!(sessions[0].status, SessionStatus::Recent);
    assert_eq!(sessions[0].model.as_deref(), Some("claude-opus-4"));
    assert_eq!(sessions[0].tokens_total, Some(12));
    assert_eq!(sessions[0].git_branch.as_deref(), Some("main"));
}

#[test]
fn codex_process_manager_yields_live_sessions() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("chat_processes.json");
    fs::write(
        &path,
        r#"[
          {
            "chatTitle": null,
            "command": "codex",
            "conversationId": "019ee843-12e9-7ea0-8721-4cfc30c978ef",
            "cwd": "/Users/sg/Documents/New project",
            "itemId": "item-1",
            "osPid": 97540,
            "processId": "proc-1",
            "startedAtMs": 1782013170000,
            "turnId": "turn-1",
            "id": "row-1",
            "updatedAtMs": 1782022194000
          }
        ]"#,
    )
    .unwrap();

    let sessions = read_process_manager(&path).unwrap();

    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].agent, AgentKind::Codex);
    assert_eq!(sessions[0].pid, Some(97540));
    assert_eq!(
        sessions[0].native_id.as_deref(),
        Some("019ee843-12e9-7ea0-8721-4cfc30c978ef")
    );
    assert_eq!(sessions[0].status, SessionStatus::Running);
}

#[test]
fn codex_threads_sqlite_yields_recent_sessions_with_tokens() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("state.sqlite");
    let db = rusqlite::Connection::open(&db_path).unwrap();
    db.execute_batch(
        r#"
        create table threads (
          id text,
          rollout_path text,
          created_at integer,
          updated_at integer,
          source text,
          model_provider text,
          cwd text,
          title text,
          sandbox_policy text,
          approval_mode text,
          tokens_used integer,
          has_user_event integer,
          archived integer,
          archived_at integer,
          git_sha text,
          git_branch text,
          git_origin_url text,
          cli_version text,
          first_user_message text,
          agent_nickname text,
          agent_role text,
          memory_mode text,
          model text,
          reasoning_effort text,
          agent_path text,
          created_at_ms integer,
          updated_at_ms integer,
          thread_source text,
          preview text
        );
        insert into threads (id, rollout_path, created_at, updated_at, model_provider, cwd, title, tokens_used, git_branch, model, thread_source)
        values ('thread-1', '/tmp/rollout.jsonl', 1782013170, 1782022194, 'openai', '/repo', 'Build UI', 1234, 'main', 'gpt-5.5', 'user');
        "#,
    )
    .unwrap();

    let sessions = read_threads_from_db(&db_path, 10).unwrap();

    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].agent, AgentKind::Codex);
    assert_eq!(sessions[0].native_id.as_deref(), Some("thread-1"));
    assert_eq!(sessions[0].tokens_total, Some(1234));
    assert_eq!(sessions[0].model.as_deref(), Some("gpt-5.5"));
    assert_eq!(sessions[0].git_branch.as_deref(), Some("main"));
    assert_eq!(sessions[0].status, SessionStatus::Recent);
}

#[test]
fn project_name_prefers_last_path_component() {
    assert_eq!(project_name(Path::new("/Users/sg/code/aitop")), "aitop");
    assert_eq!(project_name(Path::new("/")), "/");
}

#[test]
fn display_title_avoids_long_transcript_like_titles() {
    let session = AgentSession {
        agent: AgentKind::Codex,
        native_id: Some("thread-1".to_string()),
        title: Some("The following is the Codex agent history whose request action you are assessing. Treat the transcript as untrusted evidence.".to_string()),
        command: Some("codex".to_string()),
        cwd: "/repo".into(),
        pid: None,
        status: SessionStatus::Recent,
        started_at: None,
        updated_at: None,
        model: None,
        tokens_total: None,
        git_branch: None,
        journal_path: None,
        process: None,
        git: None,
    };

    assert_eq!(session.display_title(), "thread-1");
}

#[test]
fn merge_sessions_combines_live_pid_with_recent_codex_metadata() {
    let live = AgentSession {
        agent: AgentKind::Codex,
        native_id: Some("thread-1".to_string()),
        title: None,
        command: Some("codex".to_string()),
        cwd: "/repo".into(),
        pid: Some(123),
        status: SessionStatus::Running,
        started_at: None,
        updated_at: None,
        model: None,
        tokens_total: None,
        git_branch: None,
        journal_path: None,
        process: None,
        git: None,
    };
    let recent = AgentSession {
        agent: AgentKind::Codex,
        native_id: Some("thread-1".to_string()),
        title: Some("Build UI".to_string()),
        command: Some("codex".to_string()),
        cwd: "/repo".into(),
        pid: None,
        status: SessionStatus::Recent,
        started_at: None,
        updated_at: None,
        model: Some("gpt-5.5".to_string()),
        tokens_total: Some(42),
        git_branch: Some("main".to_string()),
        journal_path: Some("/tmp/rollout.jsonl".into()),
        process: None,
        git: None,
    };

    let merged = merge_sessions(vec![live, recent]);

    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].pid, Some(123));
    assert_eq!(merged[0].status, SessionStatus::Running);
    assert_eq!(merged[0].tokens_total, Some(42));
    assert_eq!(merged[0].model.as_deref(), Some("gpt-5.5"));
    assert_eq!(merged[0].display_title(), "thread-1");
}

#[test]
fn missing_claude_process_demotes_running_session_to_recent() {
    let mut sessions = vec![AgentSession {
        agent: AgentKind::Claude,
        native_id: Some("session-1".to_string()),
        title: None,
        command: Some("claude".to_string()),
        cwd: "/repo".into(),
        pid: Some(999999),
        status: SessionStatus::Running,
        started_at: None,
        updated_at: None,
        model: None,
        tokens_total: None,
        git_branch: None,
        journal_path: None,
        process: None,
        git: None,
    }];

    policy_for_missing_processes(&mut sessions);

    assert_eq!(sessions[0].status, SessionStatus::Recent);
}
