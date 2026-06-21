use std::cmp::Reverse;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::Deserialize;

use crate::model::{AgentKind, AgentSession, SessionStatus, unix_millis, unix_seconds};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodexProcess {
    chat_title: Option<String>,
    command: String,
    conversation_id: String,
    cwd: PathBuf,
    os_pid: Option<u32>,
    started_at_ms: Option<i64>,
    updated_at_ms: Option<i64>,
}

pub fn default_process_manager_path() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|dirs| {
        dirs.home_dir()
            .join(".codex/process_manager/chat_processes.json")
    })
}

pub fn default_state_db_path() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|dirs| dirs.home_dir().join(".codex/state_5.sqlite"))
}

pub fn read_process_manager(path: &Path) -> Result<Vec<AgentSession>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let processes: Vec<CodexProcess> =
        serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;

    let mut sessions = processes
        .into_iter()
        .map(|process| {
            let status = if process.os_pid.is_some_and(pid_is_alive) {
                SessionStatus::Running
            } else {
                SessionStatus::Recent
            };
            AgentSession {
                agent: AgentKind::Codex,
                native_id: Some(process.conversation_id),
                title: process.chat_title,
                command: Some(process.command),
                cwd: process.cwd,
                pid: process.os_pid,
                status,
                started_at: process.started_at_ms.and_then(unix_millis),
                updated_at: process.updated_at_ms.and_then(unix_millis),
                model: None,
                tokens_total: None,
                git_branch: None,
                journal_path: None,
                process: None,
                git: None,
            }
        })
        .collect::<Vec<_>>();
    sessions.sort_by_key(|session| Reverse(session.updated_at));
    Ok(sessions)
}

pub fn read_threads_from_db(path: &Path, limit: usize) -> Result<Vec<AgentSession>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let connection = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open {}", path.display()))?;
    let mut statement = connection.prepare(
        "select id, rollout_path, created_at, updated_at, cwd, title, tokens_used, git_branch, model \
         from threads order by updated_at desc limit ?1",
    )?;
    let rows = statement.query_map([limit as i64], |row| {
        let id: String = row.get(0)?;
        let rollout_path: Option<String> = row.get(1)?;
        let created_at: Option<i64> = row.get(2)?;
        let updated_at: Option<i64> = row.get(3)?;
        let cwd: String = row.get(4)?;
        let title: Option<String> = row.get(5)?;
        let tokens_total: Option<i64> = row.get(6)?;
        let git_branch: Option<String> = row.get(7)?;
        let model: Option<String> = row.get(8)?;
        let rollout_metadata = rollout_path
            .as_deref()
            .and_then(|path| read_rollout_metadata(Path::new(path)).ok());
        let cwd = if cwd.is_empty() {
            rollout_metadata
                .as_ref()
                .and_then(|metadata| metadata.cwd.clone())
                .unwrap_or_else(|| ".".to_string())
        } else {
            cwd
        };
        let model = model.or_else(|| {
            rollout_metadata
                .as_ref()
                .and_then(|metadata| metadata.model.clone())
        });
        let tokens_total = tokens_total.or_else(|| {
            rollout_metadata
                .as_ref()
                .and_then(|metadata| metadata.tokens_total)
        });
        Ok(AgentSession {
            agent: AgentKind::Codex,
            native_id: Some(id),
            title,
            command: Some("codex".to_string()),
            cwd: cwd.into(),
            pid: None,
            status: SessionStatus::Recent,
            started_at: created_at.and_then(unix_seconds),
            updated_at: updated_at.and_then(unix_seconds),
            model,
            tokens_total,
            git_branch,
            journal_path: rollout_path.map(PathBuf::from),
            process: None,
            git: None,
        })
    })?;

    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

#[derive(Debug, Default)]
struct RolloutMetadata {
    cwd: Option<String>,
    model: Option<String>,
    tokens_total: Option<i64>,
}

fn read_rollout_metadata(path: &Path) -> Result<RolloutMetadata> {
    let mut file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let size = file.metadata()?.len();
    let head_len = size.min(64 * 1024) as usize;
    let mut head = vec![0; head_len];
    file.read_exact(&mut head)?;

    let tail_len = size.min(64 * 1024) as usize;
    let mut tail = vec![0; tail_len];
    file.seek(SeekFrom::End(-(tail_len as i64)))?;
    file.read_exact(&mut tail)?;

    let mut metadata = RolloutMetadata::default();
    parse_rollout_chunk(&String::from_utf8_lossy(&head), &mut metadata);
    parse_rollout_chunk(&String::from_utf8_lossy(&tail), &mut metadata);
    Ok(metadata)
}

fn parse_rollout_chunk(chunk: &str, metadata: &mut RolloutMetadata) {
    for line in chunk.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if metadata.cwd.is_none() {
            metadata.cwd = value
                .pointer("/payload/cwd")
                .or_else(|| value.pointer("/payload/turn_context/cwd"))
                .or_else(|| value.get("cwd"))
                .and_then(|cwd| cwd.as_str())
                .map(ToOwned::to_owned);
        }
        if metadata.model.is_none() {
            metadata.model = value
                .pointer("/payload/model")
                .or_else(|| value.pointer("/payload/model_slug"))
                .or_else(|| value.pointer("/payload/turn_context/model"))
                .or_else(|| value.pointer("/payload/turn_context/model_slug"))
                .or_else(|| value.get("model"))
                .and_then(|model| model.as_str())
                .map(ToOwned::to_owned);
        }
        if let Some(tokens) = value
            .pointer("/payload/token_count")
            .or_else(|| value.pointer("/payload/info/total_token_usage/total_tokens"))
            .or_else(|| value.pointer("/token_count"))
            .and_then(|tokens| tokens.as_i64())
        {
            metadata.tokens_total = Some(tokens);
        }
    }
}

fn pid_is_alive(pid: u32) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}
