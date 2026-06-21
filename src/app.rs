use std::{
    collections::BTreeMap,
    path::PathBuf,
    process::Command,
    time::{Duration, SystemTime},
};

use anyhow::Result;

use crate::{
    codex,
    feed::{Annotation, FeedEvent, FeedRecord, SessionFeed},
    git::status_for_cwd,
    model::{AgentKind, AgentSession, DirtyFile, GitStatus, ProcessStats, SessionStatus},
    pricing,
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
    Overview,
    Active,
    All,
}

impl SessionFilter {
    pub fn toggle(self) -> Self {
        match self {
            Self::Overview => Self::Active,
            Self::Active => Self::All,
            Self::All => Self::Overview,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Overview => "overview",
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
        let sessions = visible_sessions(&self.sessions, SessionFilter::Overview);
        if sessions.is_empty() {
            return "aitop: no ambient agent sessions found".to_string();
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
    let mut recent_by_project: BTreeMap<String, AgentSession> = BTreeMap::new();

    for session in sessions {
        if session.status == SessionStatus::Running {
            visible.push(session.clone());
            continue;
        }

        if matches!(filter, SessionFilter::Overview | SessionFilter::All) {
            if filter == SessionFilter::Overview && !session.cwd.exists() {
                continue;
            }
            let key = project_key(session);
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

    if matches!(filter, SessionFilter::Overview | SessionFilter::All) {
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
    let mut sessions = demo_specs()
        .iter()
        .enumerate()
        .map(|(index, spec)| {
            let wave = ((tick + index as u64 * 3) % 18) as u32;
            let live = spec.idle_seconds == 0;
            let burst = live && matches!((tick + index as u64) % 12, 0..=2);
            let cpu = if burst {
                64 + ((wave * 3) % 35)
            } else {
                (spec.cpu_seed + wave * 2) % 38
            };
            let tokens = spec.tokens_in
                + spec.tokens_out
                + if live {
                    tick as i64 * (index as i64 + 2) * 150
                } else {
                    0
                };
            let dirty_count = if burst {
                3
            } else {
                spec.dirty_files.len().min((tick as usize + index) % 3)
            };
            let dirty_files = (0..dirty_count)
                .map(|file_index| DirtyFile {
                    code: if file_index == 0 { "M" } else { "??" }.to_string(),
                    path: spec.dirty_files[file_index % spec.dirty_files.len()].to_string(),
                })
                .collect::<Vec<_>>();
            AgentSession {
                agent: spec.agent,
                native_id: Some(format!("demo-{}", spec.id)),
                title: Some(spec.id.to_string()),
                command: Some(spec.command.to_string()),
                cwd: format!("/Users/dev/{}", spec.id).into(),
                pid: Some(40_000 + index as u32),
                status: if live {
                    SessionStatus::Running
                } else if tick % 29 == index as u64 {
                    SessionStatus::Recent
                } else {
                    spec.status
                },
                started_at: Some(now - Duration::from_secs(120 + index as u64 * 53 + tick)),
                updated_at: Some(
                    now - Duration::from_secs(
                        spec.idle_seconds
                            + if live {
                                (index as u64 * 3 + tick) % 8
                            } else {
                                0
                            },
                    ),
                ),
                model: Some(spec.model.to_string()),
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
                    root: format!("/Users/dev/{}", spec.id).into(),
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

#[derive(Debug, Clone)]
struct DemoSpec {
    id: &'static str,
    agent: AgentKind,
    command: &'static str,
    model: &'static str,
    status: SessionStatus,
    idle_seconds: u64,
    tokens_in: i64,
    tokens_out: i64,
    cpu_seed: u32,
    dirty_files: &'static [&'static str],
}

fn demo_specs() -> Vec<DemoSpec> {
    vec![
        DemoSpec {
            id: "api-gateway",
            agent: AgentKind::Claude,
            command: "claude",
            model: "claude-sonnet-4-5",
            status: SessionStatus::Running,
            idle_seconds: 0,
            tokens_in: 48_200,
            tokens_out: 12_100,
            cpu_seed: 31,
            dirty_files: &[
                "src/middleware/auth.ts",
                "test/auth.spec.ts",
                "src/routes/orders.ts",
            ],
        },
        DemoSpec {
            id: "web-dashboard",
            agent: AgentKind::Claude,
            command: "claude",
            model: "claude-sonnet-4-5",
            status: SessionStatus::Running,
            idle_seconds: 0,
            tokens_in: 31_700,
            tokens_out: 7_400,
            cpu_seed: 43,
            dirty_files: &[
                "src/hooks/useDashboard.ts",
                "src/components/MetricsPanel.tsx",
                "test/dashboard.spec.ts",
            ],
        },
        DemoSpec {
            id: "migration-runner",
            agent: AgentKind::Claude,
            command: "claude",
            model: "claude-sonnet-4-5",
            status: SessionStatus::Running,
            idle_seconds: 0,
            tokens_in: 200,
            tokens_out: 40,
            cpu_seed: 12,
            dirty_files: &["migrations/0042_backfill_user_region.sql"],
        },
        DemoSpec {
            id: "auth-service",
            agent: AgentKind::Codex,
            command: "codex",
            model: "gpt-5-codex",
            status: SessionStatus::Recent,
            idle_seconds: 4 * 60,
            tokens_in: 92_100,
            tokens_out: 20_300,
            cpu_seed: 18,
            dirty_files: &["docker-compose.yml", "src/store/redis.ts"],
        },
        DemoSpec {
            id: "data-pipeline",
            agent: AgentKind::Claude,
            command: "claude",
            model: "claude-haiku-4-5",
            status: SessionStatus::Recent,
            idle_seconds: 18 * 60,
            tokens_in: 12_400,
            tokens_out: 3_100,
            cpu_seed: 8,
            dirty_files: &["src/steps/upload.ts"],
        },
        DemoSpec {
            id: "payments-api",
            agent: AgentKind::Codex,
            command: "codex",
            model: "gpt-5-codex",
            status: SessionStatus::Recent,
            idle_seconds: 60 * 60,
            tokens_in: 67_000,
            tokens_out: 9_800,
            cpu_seed: 21,
            dirty_files: &["src/payments/refund.ts", "test/refund.spec.ts"],
        },
    ]
}

pub fn demo_feed(session: &AgentSession, tick: u64) -> SessionFeed {
    let id = session
        .native_id
        .as_deref()
        .and_then(|native| native.strip_prefix("demo-"))
        .unwrap_or_else(|| session.title.as_deref().unwrap_or("api-gateway"));
    let mut feed = SessionFeed {
        model: session.model.clone(),
        ..SessionFeed::default()
    };
    let mut records = demo_script(id);
    let visible = ((tick as usize / 2) + 4).min(records.len());
    records.truncate(visible.max(1));
    for record in records {
        if let FeedEvent::Usage {
            input,
            output,
            cache_read,
        } = record.event
        {
            feed.tokens_in += input;
            feed.tokens_out += output;
            feed.cache_read += cache_read;
            feed.records.push(FeedRecord {
                event: FeedEvent::Usage {
                    input,
                    output,
                    cache_read,
                },
                ..record
            });
        } else {
            feed.records.push(record);
        }
    }
    if let Some(model) = &feed.model {
        feed.estimated_cost = pricing::estimate_cost(feed.tokens_in, feed.tokens_out, model);
        let context = pricing::lookup(model).context_window.max(1);
        feed.context_pct = Some(((feed.tokens_in * 100) / context).min(100) as u8);
    }
    feed
}

fn demo_script(id: &str) -> Vec<FeedRecord> {
    match id {
        "api-gateway" => vec![
            user(
                id,
                "The login endpoint returns 500 on expired tokens instead of 401. Fix it.",
            ),
            thinking(
                id,
                "Let me trace the token verification path before changing anything.",
            ),
            tool(id, "Grep", "\"verifyToken(\" - 3 matches", None),
            result(
                id,
                true,
                "src/middleware/auth.ts:41\nsrc/routes/orders.ts:11\ntest/auth.spec.ts:27",
            ),
            assistant(
                id,
                "claude-sonnet-4-5",
                "Found it: line 41 calls verifyToken without await, so decoded is a pending Promise. decoded.sub is undefined and loadUser(undefined) throws, which surfaces as a 500 instead of a clean 401.",
            ),
            tool(
                id,
                "Edit",
                "src/middleware/auth.ts",
                Some(Annotation::FileTouched("src/middleware/auth.ts".into())),
            ),
            result(id, true, "Applied 1 edit - +6 -3"),
            tool(
                id,
                "Bash",
                "npm run test:auth",
                Some(Annotation::CommandRun("npm run test:auth".into())),
            ),
            result(id, true, "PASS  test/auth.spec.ts\n✓ 14 passed  (2.31s)"),
            assistant(
                id,
                "claude-sonnet-4-5",
                "Fixed. verifyToken is now awaited and invalid tokens return 401 instead of crashing the request. All 14 auth tests pass.",
            ),
            usage(id, 48_200, 12_100),
        ],
        "web-dashboard" => vec![
            user(
                id,
                "Migrate the dashboard data layer from manual fetch() calls to the new useQuery hooks.",
            ),
            thinking_with(
                id,
                "I need to map every existing fetch call to a query key and a matching hook, preserve loading and error states, and make sure mutations still invalidate the right cache entries afterward.",
                Annotation::TokenSpike { tokens: 11_200 },
            ),
            tool(
                id,
                "Read",
                "src/hooks/useDashboard.ts",
                Some(Annotation::FileTouched("src/hooks/useDashboard.ts".into())),
            ),
            result(
                id,
                true,
                "6  const [data,setData] = useState(null)\n7  useEffect(() => {\n8    fetch('/api/metrics').then(r => r.json()).then(setData)\n9  }, [])",
            ),
            tool(
                id,
                "Edit",
                "src/hooks/useDashboard.ts",
                Some(Annotation::FileTouched("src/hooks/useDashboard.ts".into())),
            ),
            result(id, true, "Applied 1 edit - +24 -31"),
            assistant(
                id,
                "claude-sonnet-4-5",
                "Converted useDashboard to useQuery with key ['metrics']. Loading and error states are preserved, and mutations now invalidate ['metrics'] on success.",
            ),
            usage(id, 31_700, 7_400),
        ],
        "auth-service" => vec![
            user(id, "Run the integration suite and fix whatever is failing."),
            tool(
                id,
                "Bash",
                "npm run test:integration",
                Some(Annotation::CommandRun("npm run test:integration".into())),
            ),
            result_with(
                id,
                false,
                "FAIL  test/session.spec.ts\nError: connect ECONNREFUSED 127.0.0.1:6379\n    at RedisClient.connect (src/store/redis.ts:88)",
                Annotation::Error,
            ),
            thinking(
                id,
                "ECONNREFUSED on 6379 means Redis is not running locally. The session store needs it, so this is an environment problem, not a code bug.",
            ),
            tool(
                id,
                "Bash",
                "docker compose up -d redis",
                Some(Annotation::CommandRun("docker compose up -d redis".into())),
            ),
            result(id, true, "✓ Container auth-redis  Started  (0.9s)"),
            tool(
                id,
                "Bash",
                "npm run test:integration",
                Some(Annotation::CommandRun("npm run test:integration".into())),
            ),
            result(id, true, "PASS  test/session.spec.ts\n✓ 31 passed  (8.74s)"),
            assistant(
                id,
                "gpt-5-codex",
                "The failure was a missing Redis instance, not a code defect. Started the redis container and re-ran; all 31 integration tests pass now.",
            ),
            usage(id, 92_100, 20_300),
        ],
        "migration-runner" => vec![
            user(id, "Run the 0042 migration against staging."),
            thinking(
                id,
                "Checking the migration plan and current schema version before applying anything.",
            ),
            tool(
                id,
                "Bash",
                "migrate status --env staging",
                Some(Annotation::CommandRun(
                    "migrate status --env staging".into(),
                )),
            ),
            result(
                id,
                true,
                "current: 0041_add_index\npending: 0042_backfill_user_region",
            ),
        ],
        "data-pipeline" => vec![
            user(
                id,
                "Add retry with exponential backoff to the S3 upload step.",
            ),
            tool(
                id,
                "Edit",
                "src/steps/upload.ts",
                Some(Annotation::FileTouched("src/steps/upload.ts".into())),
            ),
            result(id, true, "Applied 1 edit - +12 -2"),
            assistant(
                id,
                "claude-haiku-4-5",
                "Added exponential backoff around the upload call. Transient 5xx responses are now retried before the step fails.",
            ),
            usage(id, 12_400, 3_100),
        ],
        "payments-api" => vec![
            user(id, "Audit the refund flow for race conditions."),
            thinking(
                id,
                "Two concurrent refunds on the same charge could double-credit if the balance check and ledger write are not in one transaction.",
            ),
            tool(id, "Grep", "\"refund(\" - 2 matches", None),
            result(id, true, "src/payments/refund.ts:34\ntest/refund.spec.ts:9"),
            assistant(
                id,
                "gpt-5-codex",
                "Found a TOCTOU window in refund(): the balance check and ledger write are not in a single transaction. Recommend wrapping both in one SERIALIZABLE transaction or taking a row lock.",
            ),
            usage(id, 67_000, 9_800),
        ],
        _ => vec![
            user(id, "Take a look at this project."),
            assistant(id, "claude-sonnet-4-5", "On it."),
        ],
    }
}

fn rec(id: &str, event: FeedEvent, annotations: Vec<Annotation>) -> FeedRecord {
    FeedRecord {
        session_id: id.to_string(),
        timestamp: None,
        event,
        annotations,
    }
}

fn user(id: &str, text: &str) -> FeedRecord {
    rec(id, FeedEvent::User { text: text.into() }, vec![])
}

fn assistant(id: &str, model: &str, text: &str) -> FeedRecord {
    rec(
        id,
        FeedEvent::Assistant {
            text: text.into(),
            model: model.into(),
        },
        vec![],
    )
}

fn thinking(id: &str, text: &str) -> FeedRecord {
    rec(id, FeedEvent::Thinking { text: text.into() }, vec![])
}

fn thinking_with(id: &str, text: &str, annotation: Annotation) -> FeedRecord {
    rec(
        id,
        FeedEvent::Thinking { text: text.into() },
        vec![annotation],
    )
}

fn tool(id: &str, name: &str, summary: &str, annotation: Option<Annotation>) -> FeedRecord {
    rec(
        id,
        FeedEvent::ToolCall {
            id: "tool".into(),
            name: name.into(),
            summary: summary.into(),
        },
        annotation.into_iter().collect(),
    )
}

fn result(id: &str, ok: bool, detail: &str) -> FeedRecord {
    result_with_annotations(id, ok, detail, vec![])
}

fn result_with(id: &str, ok: bool, detail: &str, annotation: Annotation) -> FeedRecord {
    result_with_annotations(id, ok, detail, vec![annotation])
}

fn result_with_annotations(
    id: &str,
    ok: bool,
    detail: &str,
    annotations: Vec<Annotation>,
) -> FeedRecord {
    rec(
        id,
        FeedEvent::ToolResult {
            id: "tool".into(),
            ok,
            summary: detail.lines().next().unwrap_or("").into(),
            detail: detail.into(),
        },
        annotations,
    )
}

fn usage(id: &str, input: u64, output: u64) -> FeedRecord {
    rec(
        id,
        FeedEvent::Usage {
            input,
            output,
            cache_read: 0,
        },
        vec![],
    )
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
        if session.status == SessionStatus::Running
            && let Some(pid) = session.pid
            && let Some(cwd) = live_cwd_for_pid(pid)
        {
            session.cwd = cwd;
        }
        session.git = status_for_cwd(&session.cwd);
        if session.git_branch.is_none() {
            session.git_branch = session.git.as_ref().and_then(|git| git.branch.clone());
        }
    }
}

fn live_cwd_for_pid(pid: u32) -> Option<PathBuf> {
    let output = Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    stdout
        .lines()
        .find_map(|line| line.strip_prefix('n').map(PathBuf::from))
}

pub fn policy_for_missing_processes(sessions: &mut [AgentSession]) {
    for session in sessions {
        if session.status == SessionStatus::Running
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
