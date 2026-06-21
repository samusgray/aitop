use std::{
    collections::BTreeMap,
    time::{Duration, SystemTime},
};

use anyhow::Result;

use crate::{
    codex,
    git::status_for_cwd,
    model::{AgentKind, AgentSession, DirtyFile, GitStatus, ProcessStats, SessionStatus},
    process::ProcessSampler,
    sources::claude,
};

#[derive(Debug, Clone)]
pub struct AmbientSnapshot {
    pub sessions: Vec<AgentSession>,
    pub generated_at: SystemTime,
    pub activity: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionFilter {
    Active,
    All,
}

impl SessionFilter {
    pub fn toggle(self) -> Self {
        match self {
            Self::Active => Self::All,
            Self::All => Self::Active,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::All => "all",
        }
    }
}

impl AmbientSnapshot {
    pub fn active_count(&self) -> usize {
        self.sessions
            .iter()
            .filter(|session| session.status == SessionStatus::Running)
            .count()
    }

    pub fn text_summary(&self) -> String {
        let sessions = visible_sessions(&self.sessions, SessionFilter::Active);
        if sessions.is_empty() {
            return "aitop: no active ambient agent sessions found".to_string();
        }
        sessions
            .iter()
            .map(|session| {
                format!(
                    "{} {} pid={} repo={} status={} tokens={}",
                    session.agent,
                    session.display_title(),
                    session
                        .pid
                        .map(|pid| pid.to_string())
                        .unwrap_or_else(|| "-".to_string()),
                    session.repo_name(),
                    session.status,
                    session
                        .tokens_total
                        .map(|tokens| tokens.to_string())
                        .unwrap_or_else(|| "unknown".to_string())
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

pub fn visible_sessions(sessions: &[AgentSession], filter: SessionFilter) -> Vec<AgentSession> {
    let mut visible = Vec::new();
    let mut recent_by_project: BTreeMap<(AgentKind, String), AgentSession> = BTreeMap::new();

    for session in sessions {
        if session.status == SessionStatus::Running {
            visible.push(session.clone());
            continue;
        }

        if filter == SessionFilter::All {
            let key = (session.agent, project_key(session));
            match recent_by_project.get_mut(&key) {
                Some(existing) if session.updated_at > existing.updated_at => {
                    *existing = session.clone();
                }
                None => {
                    recent_by_project.insert(key, session.clone());
                }
                _ => {}
            }
        }
    }

    if filter == SessionFilter::All {
        visible.extend(recent_by_project.into_values());
    }

    visible.sort_by(|a, b| {
        b.status
            .eq(&SessionStatus::Running)
            .cmp(&a.status.eq(&SessionStatus::Running))
            .then_with(|| b.updated_at.cmp(&a.updated_at))
    });
    visible
}

fn project_key(session: &AgentSession) -> String {
    session
        .git
        .as_ref()
        .map(git_root_key)
        .unwrap_or_else(|| session.cwd.display().to_string())
}

fn git_root_key(git: &GitStatus) -> String {
    git.root.display().to_string()
}

pub fn snapshot() -> Result<AmbientSnapshot> {
    let mut sampler = ProcessSampler::new();
    snapshot_with_sampler(&mut sampler)
}

pub fn demo_snapshot(tick: u64) -> AmbientSnapshot {
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_782_021_000 + tick);
    let specs = [
        ("api-gateway", AgentKind::Claude, "claude", 31_u32, 620_i64),
        ("web-dashboard", AgentKind::Claude, "claude", 57, 340),
        ("migration-runner", AgentKind::Codex, "codex", 13, 90),
        ("auth-service", AgentKind::Claude, "claude", 82, 1180),
        ("billing-worker", AgentKind::Codex, "codex", 4, 740),
        ("search-index", AgentKind::Claude, "claude", 26, 410),
    ];
    let mut sessions = specs
        .iter()
        .enumerate()
        .map(|(index, (repo, agent, command, cpu_seed, token_seed))| {
            let wave = ((tick + index as u64 * 3) % 18) as u32;
            let spike = matches!((tick + index as u64) % 13, 0 | 1);
            let cpu = if spike {
                70 + wave.min(25)
            } else {
                (cpu_seed + wave * 3) % 45
            };
            let tokens = token_seed + (tick as i64 * (index as i64 + 2) * 137);
            let dirty_count = if spike {
                3
            } else {
                (tick as usize + index) % 3
            };
            let dirty_files = (0..dirty_count)
                .map(|file_index| DirtyFile {
                    code: if file_index == 0 { "M" } else { "??" }.to_string(),
                    path: format!("src/{repo}/file-{file_index}.rs"),
                })
                .collect::<Vec<_>>();
            AgentSession {
                agent: *agent,
                native_id: Some(format!("demo-{repo}")),
                title: Some(repo.to_string()),
                command: Some((*command).to_string()),
                cwd: format!("/demo/{repo}").into(),
                pid: Some(40_000 + index as u32),
                status: if tick % 23 == index as u64 {
                    SessionStatus::Recent
                } else {
                    SessionStatus::Running
                },
                started_at: Some(now - Duration::from_secs(90 + index as u64 * 41 + tick)),
                updated_at: Some(
                    now - Duration::from_secs(((index as u64 * 7 + tick) % 20).min(19)),
                ),
                model: Some(if *agent == AgentKind::Claude {
                    "claude-opus-4.8".to_string()
                } else {
                    "gpt-5.5".to_string()
                }),
                tokens_total: Some(tokens),
                git_branch: Some("demo/main".to_string()),
                journal_path: None,
                process: Some(ProcessStats {
                    cpu_percent: cpu,
                    memory_bytes: (160 + (wave as u64 * 18) + index as u64 * 31) * 1024 * 1024,
                    child_pids: (0..(1 + (wave as usize % 4)))
                        .map(|child| 50_000 + index as u32 * 10 + child as u32)
                        .collect(),
                }),
                git: Some(GitStatus {
                    root: format!("/demo/{repo}").into(),
                    branch: Some("demo/main".to_string()),
                    dirty_files,
                }),
            }
        })
        .collect::<Vec<_>>();

    sessions.sort_by(|a, b| {
        b.status
            .eq(&SessionStatus::Running)
            .cmp(&a.status.eq(&SessionStatus::Running))
            .then_with(|| b.updated_at.cmp(&a.updated_at))
    });
    let activity = demo_activity(&sessions, tick);
    AmbientSnapshot {
        sessions,
        generated_at: now,
        activity,
    }
}

fn demo_activity(sessions: &[AgentSession], tick: u64) -> Vec<String> {
    sessions
        .iter()
        .take(8)
        .enumerate()
        .map(|(index, session)| {
            let cpu = session
                .process
                .as_ref()
                .map(|process| process.cpu_percent)
                .unwrap_or(0);
            if (tick + index as u64).is_multiple_of(4) {
                format!(
                    "{} changed M src/{}/file-{}.rs",
                    crate::model::time_label(session.updated_at),
                    session.repo_name(),
                    tick % 5
                )
            } else {
                format!(
                    "{} sampled cpu {}% mem {} - {}",
                    crate::model::time_label(session.updated_at),
                    cpu,
                    session
                        .process
                        .as_ref()
                        .map(|process| format_bytes(process.memory_bytes))
                        .unwrap_or_else(|| "-".to_string()),
                    session.agent
                )
            }
        })
        .collect()
}

pub fn snapshot_with_sampler(sampler: &mut ProcessSampler) -> Result<AmbientSnapshot> {
    let mut sessions = Vec::new();

    if let Some(dir) = claude::default_sessions_dir() {
        let mut claude_sessions = claude::read_claude_sessions(&dir).unwrap_or_default();
        if let Some(projects) = claude::default_projects_dir() {
            claude::attach_recent_claude_journals(&mut claude_sessions, &projects);
            sessions
                .extend(claude::read_claude_project_journals(&projects, 20).unwrap_or_default());
        }
        sessions.extend(claude_sessions);
    }

    if let Some(path) = codex::default_process_manager_path() {
        sessions.extend(codex::read_process_manager(&path).unwrap_or_default());
    }
    if let Some(path) = codex::default_state_db_path() {
        sessions.extend(codex::read_threads_from_db(&path, 15).unwrap_or_default());
    }

    sessions = merge_sessions(sessions);
    enrich(&mut sessions, sampler);
    sessions.sort_by(|a, b| {
        b.status
            .eq(&SessionStatus::Running)
            .cmp(&a.status.eq(&SessionStatus::Running))
            .then_with(|| b.updated_at.cmp(&a.updated_at))
    });
    sessions.truncate(30);
    let activity = activity_lines(&sessions);

    Ok(AmbientSnapshot {
        sessions,
        generated_at: SystemTime::now(),
        activity,
    })
}

pub fn merge_sessions(sessions: Vec<AgentSession>) -> Vec<AgentSession> {
    let mut by_key: BTreeMap<String, AgentSession> = BTreeMap::new();
    let mut anonymous = Vec::new();

    for session in sessions {
        let Some(native_id) = session.native_id.clone() else {
            anonymous.push(session);
            continue;
        };
        let key = format!("{}:{native_id}", session.agent);
        match by_key.get_mut(&key) {
            Some(existing) => merge_into(existing, session),
            None => {
                by_key.insert(key, session);
            }
        }
    }

    anonymous.extend(by_key.into_values());
    anonymous
}

fn merge_into(existing: &mut AgentSession, incoming: AgentSession) {
    if incoming.status == SessionStatus::Running {
        existing.status = SessionStatus::Running;
    }
    if existing.pid.is_none() {
        existing.pid = incoming.pid;
    }
    if existing.title.is_none() {
        existing.title = incoming.title;
    }
    if existing.command.is_none() {
        existing.command = incoming.command;
    }
    if existing.started_at.is_none() {
        existing.started_at = incoming.started_at;
    }
    if incoming.updated_at > existing.updated_at {
        existing.updated_at = incoming.updated_at;
    }
    if existing.model.is_none() {
        existing.model = incoming.model;
    }
    if existing.tokens_total.is_none() {
        existing.tokens_total = incoming.tokens_total;
    }
    if existing.git_branch.is_none() {
        existing.git_branch = incoming.git_branch;
    }
    if existing.journal_path.is_none() {
        existing.journal_path = incoming.journal_path;
    }
    if existing.process.is_none() {
        existing.process = incoming.process;
    }
    if existing.git.is_none() {
        existing.git = incoming.git;
    }
}

fn enrich(sessions: &mut [AgentSession], sampler: &mut ProcessSampler) {
    for session in sessions {
        if let Some(pid) = session.pid {
            session.process = sampler.sample(pid);
        }
        policy_for_missing_processes(std::slice::from_mut(session));
        session.git = status_for_cwd(&session.cwd);
        if session.git_branch.is_none() {
            session.git_branch = session.git.as_ref().and_then(|git| git.branch.clone());
        }
    }
}

pub fn policy_for_missing_processes(sessions: &mut [AgentSession]) {
    for session in sessions {
        if session.agent == AgentKind::Claude
            && session.status == SessionStatus::Running
            && session.pid.is_some()
            && session.process.is_none()
        {
            session.status = SessionStatus::Recent;
        }
    }
}

fn activity_lines(sessions: &[AgentSession]) -> Vec<String> {
    let mut lines = Vec::new();
    for session in sessions.iter().take(12) {
        let agent = match session.agent {
            AgentKind::Claude => "claude",
            AgentKind::Codex => "codex",
        };
        if let Some(process) = &session.process {
            lines.push(format!(
                "{} sampled cpu {}% mem {} - {}",
                crate::model::time_label(session.updated_at),
                process.cpu_percent,
                format_bytes(process.memory_bytes),
                agent
            ));
        } else {
            lines.push(format!(
                "{} observed {} - {}",
                crate::model::time_label(session.updated_at),
                session.status,
                agent
            ));
        }
        if let Some(git) = &session.git {
            for dirty in git.dirty_files.iter().take(2) {
                lines.push(format!(
                    "{} changed {} {}",
                    crate::model::time_label(session.updated_at),
                    dirty.code,
                    dirty.path
                ));
            }
        }
    }
    lines.truncate(12);
    lines
}

pub fn format_bytes(bytes: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = MIB * 1024;
    if bytes >= GIB {
        format!("{:.1}G", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{}M", bytes / MIB)
    } else {
        format!("{}K", bytes / 1024)
    }
}
