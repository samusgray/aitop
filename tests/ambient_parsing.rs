use std::{fs, path::Path};

use aitop::{
    app::{
        AmbientSnapshot, SessionFilter, demo_snapshot, merge_sessions,
        policy_for_missing_processes, visible_sessions,
    },
    codex::{read_process_manager, read_threads_from_db},
    git::{project_name, repo_root_for_path},
    model::{AgentKind, AgentSession, SessionStatus},
    sources::claude::{decode_project_dir, read_claude_project_journals, read_claude_sessions},
};

#[test]
fn claude_sessions_map_busy_alive_pid_files_to_live_sessions() {
    let temp = tempfile::tempdir().unwrap();
    let sessions = temp.path().join("sessions");
    fs::create_dir(&sessions).unwrap();
    fs::write(
        sessions.join("24775.json"),
        format!(
            r#"{{
          "pid": {},
          "sessionId": "35212a4f-4c3f-498c-8c34-123fe82f7434",
          "cwd": "/Users/sg/code/atail",
          "startedAt": 1782020000000,
          "procStart": "2026-06-21T00:00:00Z",
          "version": "1.0.0",
          "kind": "cli",
          "entrypoint": "claude",
          "status": "busy",
          "updatedAt": 1782020300000,
          "statusUpdatedAt": 1782020300000
        }}"#,
            std::process::id()
        ),
    )
    .unwrap();

    let sessions = read_claude_sessions(&sessions).unwrap();

    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].agent, AgentKind::Claude);
    assert_eq!(sessions[0].pid, Some(std::process::id()));
    assert_eq!(
        sessions[0].native_id.as_deref(),
        Some("35212a4f-4c3f-498c-8c34-123fe82f7434")
    );
    assert_eq!(sessions[0].cwd, Path::new("/Users/sg/code/atail"));
    assert_eq!(sessions[0].status, SessionStatus::Running);
    assert_eq!(
        sessions[0].started_at,
        Some(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_782_020_000))
    );
    assert_eq!(
        sessions[0].updated_at,
        Some(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_782_020_300))
    );
}

#[test]
fn claude_project_dir_decodes_to_cwd() {
    assert_eq!(
        decode_project_dir("-Users-sg-code-atail"),
        Some("/Users/sg/code/atail".into())
    );
}

#[test]
fn claude_only_busy_alive_pid_is_running() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(
        temp.path().join("24775.json"),
        r#"{
          "pid": 24775,
          "sessionId": "session-1",
          "cwd": "/repo",
          "status": "idle"
        }"#,
    )
    .unwrap();
    fs::write(
        temp.path().join("24776.json"),
        r#"{
          "pid": 24776,
          "sessionId": "session-2",
          "cwd": "/repo",
          "status": "needs_input"
        }"#,
    )
    .unwrap();
    fs::write(
        temp.path().join("24777.json"),
        r#"{
          "pid": 24777,
          "sessionId": "session-3",
          "cwd": "/repo",
          "status": "busy"
        }"#,
    )
    .unwrap();
    fs::write(
        temp.path().join("current.json"),
        format!(
            r#"{{
          "pid": {},
          "sessionId": "session-4",
          "cwd": "/repo",
          "status": "busy"
        }}"#,
            std::process::id()
        ),
    )
    .unwrap();

    let mut sessions = read_claude_sessions(temp.path()).unwrap();
    sessions.sort_by_key(|session| session.native_id.clone());

    assert_eq!(sessions[0].native_id.as_deref(), Some("session-1"));
    assert_eq!(sessions[0].status, SessionStatus::Recent);
    assert_eq!(sessions[1].native_id.as_deref(), Some("session-2"));
    assert_eq!(sessions[1].status, SessionStatus::Recent);
    assert_eq!(sessions[2].native_id.as_deref(), Some("session-3"));
    assert_eq!(sessions[2].status, SessionStatus::Recent);
    assert_eq!(sessions[3].native_id.as_deref(), Some("session-4"));
    assert_eq!(sessions[3].status, SessionStatus::Running);
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
            "osPid": PID,
            "processId": "proc-1",
            "startedAtMs": 1782013170000,
            "turnId": "turn-1",
            "id": "row-1",
            "updatedAtMs": 1782022194000
          }
        ]"#
        .replace("PID", &std::process::id().to_string()),
    )
    .unwrap();

    let sessions = read_process_manager(&path).unwrap();

    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].agent, AgentKind::Codex);
    assert_eq!(sessions[0].pid, Some(std::process::id()));
    assert_eq!(
        sessions[0].native_id.as_deref(),
        Some("019ee843-12e9-7ea0-8721-4cfc30c978ef")
    );
    assert_eq!(sessions[0].status, SessionStatus::Running);
}

#[test]
fn codex_process_manager_dead_pid_is_recent_not_active() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("chat_processes.json");
    fs::write(
        &path,
        r#"[
          {
            "chatTitle": null,
            "command": "codex",
            "conversationId": "thread-1",
            "cwd": "/Users/sg/Documents/New project",
            "osPid": 4294967295,
            "startedAtMs": 1782013170000,
            "updatedAtMs": 1782022194000
          }
        ]"#,
    )
    .unwrap();

    let sessions = read_process_manager(&path).unwrap();

    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].pid, Some(4294967295));
    assert_eq!(sessions[0].status, SessionStatus::Recent);
}

#[test]
fn codex_process_manager_without_pid_is_recent_not_active() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("chat_processes.json");
    fs::write(
        &path,
        r#"[
          {
            "chatTitle": null,
            "command": "codex",
            "conversationId": "thread-1",
            "cwd": "/Users/sg/Documents/New project",
            "osPid": null,
            "startedAtMs": 1782013170000,
            "updatedAtMs": 1782022194000
          }
        ]"#,
    )
    .unwrap();

    let sessions = read_process_manager(&path).unwrap();

    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].status, SessionStatus::Recent);
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
fn codex_threads_fill_missing_db_metadata_from_rollout_head_and_tail() {
    let temp = tempfile::tempdir().unwrap();
    let rollout_path = temp.path().join("rollout.jsonl");
    fs::write(
        &rollout_path,
        concat!(
            "{\"type\":\"session_meta\",\"payload\":{\"cwd\":\"/repo/from-head\",\"model\":\"gpt-5.5\"}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[]}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"total_token_usage\":{\"total_tokens\":9876}}}}\n",
        ),
    )
    .unwrap();
    let db_path = temp.path().join("state.sqlite");
    let db = rusqlite::Connection::open(&db_path).unwrap();
    db.execute_batch(
        &format!(
            r#"
        create table threads (
          id text,
          rollout_path text,
          created_at integer,
          updated_at integer,
          cwd text,
          title text,
          tokens_used integer,
          git_branch text,
          model text
        );
        insert into threads (id, rollout_path, created_at, updated_at, cwd, title, tokens_used, git_branch, model)
        values ('thread-1', '{}', 1782013170, 1782022194, '', null, null, null, null);
        "#,
            rollout_path.display()
        ),
    )
    .unwrap();

    let sessions = read_threads_from_db(&db_path, 10).unwrap();

    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].cwd, Path::new("/repo/from-head"));
    assert_eq!(sessions[0].model.as_deref(), Some("gpt-5.5"));
    assert_eq!(sessions[0].tokens_total, Some(9876));
}

#[test]
fn project_name_prefers_last_path_component() {
    assert_eq!(project_name(Path::new("/Users/sg/code/aitop")), "aitop");
    assert_eq!(project_name(Path::new("/")), "/");
}

#[test]
fn repo_root_for_worktree_git_file_names_main_repo() {
    let temp = tempfile::tempdir().unwrap();
    let main = temp.path().join("stream");
    let worktree = main.join(".worktrees/torrent-streamer");
    let cwd = worktree.join("src-tauri");
    fs::create_dir_all(&cwd).unwrap();
    fs::create_dir_all(main.join(".git/worktrees/torrent-streamer")).unwrap();
    fs::write(
        worktree.join(".git"),
        "gitdir: ../../.git/worktrees/torrent-streamer\n",
    )
    .unwrap();

    assert_eq!(repo_root_for_path(&cwd), Some(main.canonicalize().unwrap()));
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
fn missing_process_demotes_running_session_to_recent() {
    let mut sessions = vec![AgentSession {
        agent: AgentKind::Codex,
        native_id: Some("session-1".to_string()),
        title: None,
        command: Some("codex".to_string()),
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

#[test]
fn visible_sessions_overview_shows_recent_when_no_running_sessions() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("recent");
    fs::create_dir(&repo).unwrap();
    let older = test_session("older", SessionStatus::Recent, repo.to_str().unwrap(), 10);
    let newer = test_session("newer", SessionStatus::Recent, repo.to_str().unwrap(), 20);

    let visible = visible_sessions(&[older, newer], SessionFilter::Overview);

    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].native_id.as_deref(), Some("newer"));
    assert_eq!(visible[0].status, SessionStatus::Recent);
}

#[test]
fn active_filter_keeps_strict_running_only_view() {
    let running = test_session("live", SessionStatus::Running, "/Users/sg/code/live", 10);
    let recent = test_session("recent", SessionStatus::Recent, "/Users/sg/code/recent", 20);

    let visible = visible_sessions(&[recent, running], SessionFilter::Active);

    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].native_id.as_deref(), Some("live"));
    assert_eq!(visible[0].status, SessionStatus::Running);
}

#[test]
fn overview_orders_running_before_recent_sessions() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("recent");
    fs::create_dir(&repo).unwrap();
    let recent = test_session("recent", SessionStatus::Recent, repo.to_str().unwrap(), 20);
    let running = test_session("live", SessionStatus::Running, "/Users/sg/code/live", 10);

    let visible = visible_sessions(&[recent, running], SessionFilter::Overview);

    assert_eq!(visible.len(), 2);
    assert_eq!(visible[0].native_id.as_deref(), Some("live"));
    assert_eq!(visible[1].native_id.as_deref(), Some("recent"));
}

#[test]
fn active_view_keeps_distinct_running_rows_for_same_project() {
    let older = test_session(
        "older",
        SessionStatus::Running,
        "/Users/sg/code/src-tauri",
        10,
    );
    let newer = test_session(
        "newer",
        SessionStatus::Running,
        "/Users/sg/code/src-tauri",
        20,
    );
    let other = test_session("other", SessionStatus::Running, "/Users/sg/code/aitop", 15);

    let visible = visible_sessions(&[older, newer, other], SessionFilter::Active);

    assert_eq!(visible.len(), 3);
    assert!(
        visible
            .iter()
            .any(|session| session.native_id.as_deref() == Some("newer"))
    );
    assert!(
        visible
            .iter()
            .any(|session| session.native_id.as_deref() == Some("older"))
    );
}

#[test]
fn overview_hides_missing_inactive_cwd_but_all_keeps_it() {
    let temp = tempfile::tempdir().unwrap();
    let existing = temp.path().join("existing");
    fs::create_dir(&existing).unwrap();
    let stale = test_session(
        "stale",
        SessionStatus::Recent,
        temp.path().join("deleted").to_str().unwrap(),
        20,
    );
    let live_stale = test_session(
        "live-stale",
        SessionStatus::Running,
        temp.path().join("deleted").to_str().unwrap(),
        30,
    );
    let recent = test_session(
        "recent",
        SessionStatus::Recent,
        existing.to_str().unwrap(),
        10,
    );

    let overview = visible_sessions(
        &[stale.clone(), live_stale.clone(), recent.clone()],
        SessionFilter::Overview,
    );
    let all = visible_sessions(&[stale, live_stale, recent], SessionFilter::All);

    assert!(
        !overview
            .iter()
            .any(|session| session.native_id.as_deref() == Some("stale"))
    );
    assert!(
        overview
            .iter()
            .any(|session| session.native_id.as_deref() == Some("live-stale"))
    );
    assert!(
        all.iter()
            .any(|session| session.native_id.as_deref() == Some("stale"))
    );
}

#[test]
fn overview_dedupes_recent_rows_by_project_across_agents() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    fs::create_dir(&repo).unwrap();
    let mut claude = test_session("claude", SessionStatus::Recent, repo.to_str().unwrap(), 10);
    claude.agent = AgentKind::Claude;
    let codex = test_session("codex", SessionStatus::Recent, repo.to_str().unwrap(), 20);

    let visible = visible_sessions(&[claude, codex], SessionFilter::Overview);

    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].native_id.as_deref(), Some("codex"));
}

#[test]
fn text_summary_uses_overview_default() {
    let running = test_session("live", SessionStatus::Running, "/Users/sg/code/live", 10);
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("recent");
    fs::create_dir(&repo).unwrap();
    let recent = test_session("recent", SessionStatus::Recent, repo.to_str().unwrap(), 20);
    let snapshot = AmbientSnapshot {
        sessions: vec![recent, running],
        generated_at: std::time::SystemTime::UNIX_EPOCH,
        activity: vec![],
    };

    let summary = snapshot.text_summary();

    assert!(summary.contains("live"));
    assert!(summary.contains("recent"));
}

#[test]
fn all_sessions_dedupes_inactive_rows_by_project() {
    let older = test_session(
        "older",
        SessionStatus::Recent,
        "/Users/sg/Documents/New project",
        10,
    );
    let newer = test_session(
        "newer",
        SessionStatus::Recent,
        "/Users/sg/Documents/New project",
        20,
    );
    let other = test_session("other", SessionStatus::Recent, "/Users/sg/code/aitop", 15);
    let running = test_session(
        "live",
        SessionStatus::Running,
        "/Users/sg/Documents/New project",
        5,
    );

    let visible = visible_sessions(&[older, newer, other, running], SessionFilter::All);

    assert_eq!(visible.len(), 3);
    assert_eq!(visible[0].native_id.as_deref(), Some("live"));
    assert!(
        visible
            .iter()
            .any(|session| session.native_id.as_deref() == Some("newer"))
    );
    assert!(
        !visible
            .iter()
            .any(|session| session.native_id.as_deref() == Some("older"))
    );
    assert!(
        visible
            .iter()
            .any(|session| session.native_id.as_deref() == Some("other"))
    );
}

#[test]
fn demo_snapshots_change_over_time() {
    let first = demo_snapshot(0);
    let later = demo_snapshot(7);

    assert!(first.sessions.len() >= 5);
    assert!(first.sessions.iter().all(|session| session.pid.is_some()));
    assert_ne!(
        first
            .sessions
            .iter()
            .filter_map(|session| session.tokens_total)
            .collect::<Vec<_>>(),
        later
            .sessions
            .iter()
            .filter_map(|session| session.tokens_total)
            .collect::<Vec<_>>()
    );
    assert_ne!(first.activity, later.activity);
}

#[test]
fn demo_snapshot_uses_authored_aitail_style_sessions() {
    let snapshot = demo_snapshot(3);
    let titles = snapshot
        .sessions
        .iter()
        .filter_map(|session| session.title.as_deref())
        .collect::<Vec<_>>();

    assert!(titles.contains(&"api-gateway"));
    assert!(titles.contains(&"web-dashboard"));
    assert!(titles.contains(&"auth-service"));
    assert!(titles.contains(&"migration-runner"));

    let web_dashboard = snapshot
        .sessions
        .iter()
        .find(|session| session.title.as_deref() == Some("web-dashboard"))
        .expect("web-dashboard demo session");
    assert_eq!(web_dashboard.model.as_deref(), Some("claude-sonnet-4-5"));
    assert!(web_dashboard.tokens_total.unwrap_or_default() > 30_000);

    let auth_service = snapshot
        .sessions
        .iter()
        .find(|session| session.title.as_deref() == Some("auth-service"))
        .expect("auth-service demo session");
    assert_eq!(auth_service.agent, AgentKind::Codex);
    assert_eq!(auth_service.status, SessionStatus::Recent);
}

fn test_session(id: &str, status: SessionStatus, cwd: &str, updated_at: u64) -> AgentSession {
    AgentSession {
        agent: AgentKind::Codex,
        native_id: Some(id.to_string()),
        title: None,
        command: Some("codex".to_string()),
        cwd: cwd.into(),
        pid: if status == SessionStatus::Running {
            Some(123)
        } else {
            None
        },
        status,
        started_at: None,
        updated_at: Some(
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(updated_at),
        ),
        model: None,
        tokens_total: None,
        git_branch: None,
        journal_path: None,
        process: None,
        git: None,
    }
}
