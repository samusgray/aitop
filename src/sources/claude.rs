use std::{
    cmp::Reverse,
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::model::{AgentKind, AgentSession, SessionStatus, unix_seconds};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeSessionFile {
    pid: u32,
    session_id: String,
    cwd: PathBuf,
    started_at: Option<i64>,
    updated_at: Option<i64>,
    entrypoint: Option<String>,
    status: Option<String>,
}

pub fn default_sessions_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|dirs| dirs.home_dir().join(".claude/sessions"))
}

pub fn default_projects_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|dirs| dirs.home_dir().join(".claude/projects"))
}

pub fn read_claude_sessions(dir: &Path) -> Result<Vec<AgentSession>> {
    let mut sessions = Vec::new();
    if !dir.exists() {
        return Ok(sessions);
    }

    for entry in fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(parsed) = serde_json::from_str::<ClaudeSessionFile>(&text) else {
            continue;
        };

        sessions.push(AgentSession {
            agent: AgentKind::Claude,
            native_id: Some(parsed.session_id),
            title: None,
            command: parsed.entrypoint,
            cwd: parsed.cwd,
            pid: Some(parsed.pid),
            status: claude_status(parsed.status.as_deref()),
            started_at: parsed.started_at.and_then(unix_seconds),
            updated_at: parsed.updated_at.and_then(unix_seconds),
            model: None,
            tokens_total: None,
            git_branch: None,
            journal_path: None,
            process: None,
            git: None,
        });
    }

    sessions.sort_by_key(|session| Reverse(session.updated_at));
    Ok(sessions)
}

pub fn read_claude_project_journals(dir: &Path, limit: usize) -> Result<Vec<AgentSession>> {
    let mut journals = Vec::new();
    collect_jsonl_files(dir, &mut journals)?;
    journals.sort_by_key(|path| {
        Reverse(
            path.metadata()
                .and_then(|metadata| metadata.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH),
        )
    });

    let mut sessions = Vec::new();
    for path in journals.into_iter().take(limit) {
        if let Some(session) = read_project_journal(&path) {
            sessions.push(session);
        }
    }
    Ok(sessions)
}

fn collect_jsonl_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl_files(&path, out)?;
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
            && !path
                .components()
                .any(|component| component.as_os_str() == "subagents")
        {
            out.push(path);
        }
    }
    Ok(())
}

fn read_project_journal(path: &Path) -> Option<AgentSession> {
    let text = fs::read_to_string(path).ok()?;
    let mut native_id = path.file_stem()?.to_str()?.to_string();
    let mut cwd = path
        .parent()
        .and_then(|parent| parent.file_name())
        .and_then(|name| name.to_str())
        .and_then(decode_project_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    let mut updated_at = path
        .metadata()
        .and_then(|metadata| metadata.modified())
        .ok();
    let mut model = None;
    let mut git_branch = None;
    let mut total_tokens = 0_i64;
    let mut has_tokens = false;

    for line in text.lines().rev().take(250) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(session_id) = value.get("sessionId").and_then(|id| id.as_str()) {
            native_id = session_id.to_string();
        }
        if let Some(found_cwd) = value.get("cwd").and_then(|cwd| cwd.as_str()) {
            cwd = PathBuf::from(found_cwd);
        }
        if let Some(timestamp) = value
            .get("timestamp")
            .and_then(|timestamp| timestamp.as_str())
            && let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(timestamp)
        {
            updated_at = Some(parsed.into());
        }
        if model.is_none() {
            model = value
                .get("message")
                .and_then(|message| message.get("model"))
                .and_then(|model| model.as_str())
                .map(ToOwned::to_owned);
        }
        if git_branch.is_none() {
            git_branch = value
                .get("gitBranch")
                .and_then(|branch| branch.as_str())
                .map(ToOwned::to_owned);
        }
        if let Some(usage) = value
            .get("message")
            .and_then(|message| message.get("usage"))
        {
            for key in [
                "input_tokens",
                "output_tokens",
                "cache_creation_input_tokens",
                "cache_read_input_tokens",
            ] {
                if let Some(count) = usage.get(key).and_then(|count| count.as_i64()) {
                    total_tokens += count;
                    has_tokens = true;
                }
            }
        }
    }

    Some(AgentSession {
        agent: AgentKind::Claude,
        native_id: Some(native_id),
        title: None,
        command: Some("claude".to_string()),
        cwd,
        pid: None,
        status: SessionStatus::Recent,
        started_at: None,
        updated_at,
        model,
        tokens_total: has_tokens.then_some(total_tokens),
        git_branch,
        journal_path: Some(path.to_path_buf()),
        process: None,
        git: None,
    })
}

pub fn attach_recent_claude_journals(sessions: &mut [AgentSession], projects_dir: &Path) {
    if !projects_dir.exists() {
        return;
    }

    for session in sessions {
        let Some(native_id) = session.native_id.as_deref() else {
            continue;
        };
        let encoded = encode_project_dir(&session.cwd);
        let candidate = projects_dir
            .join(encoded)
            .join(format!("{native_id}.jsonl"));
        if candidate.exists() {
            session.journal_path = Some(candidate.clone());
            attach_claude_journal_metadata(session, &candidate);
        }
    }
}

fn attach_claude_journal_metadata(session: &mut AgentSession, path: &Path) {
    let Ok(text) = fs::read_to_string(path) else {
        return;
    };
    let mut total_tokens = 0_i64;
    let mut has_tokens = false;
    for line in text.lines().rev().take(250) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if session.model.is_none() {
            session.model = value
                .get("message")
                .and_then(|message| message.get("model"))
                .and_then(|model| model.as_str())
                .map(ToOwned::to_owned);
        }
        if session.git_branch.is_none() {
            session.git_branch = value
                .get("gitBranch")
                .and_then(|branch| branch.as_str())
                .map(ToOwned::to_owned);
        }
        if let Some(usage) = value
            .get("message")
            .and_then(|message| message.get("usage"))
        {
            for key in [
                "input_tokens",
                "output_tokens",
                "cache_creation_input_tokens",
                "cache_read_input_tokens",
            ] {
                if let Some(count) = usage.get(key).and_then(|count| count.as_i64()) {
                    total_tokens += count;
                    has_tokens = true;
                }
            }
        }
    }
    if has_tokens {
        session.tokens_total = Some(total_tokens);
    }
}

fn claude_status(status: Option<&str>) -> SessionStatus {
    match status {
        Some("done") | Some("exited") | Some("complete") | Some("completed") => SessionStatus::Done,
        Some(_) => SessionStatus::Running,
        None => SessionStatus::Running,
    }
}

pub fn decode_project_dir(name: &str) -> Option<PathBuf> {
    if !name.starts_with('-') {
        return None;
    }
    Some(PathBuf::from(name.replace('-', "/")))
}

fn encode_project_dir(path: &Path) -> String {
    path.display().to_string().replace('/', "-")
}
