use std::collections::{BTreeMap, VecDeque};
use std::time::SystemTime;

use crate::app::AmbientSnapshot;
use crate::model::{AgentSession, SessionStatus};

fn estimate_session_cost(model: Option<&str>, tokens_total: u64) -> f64 {
    let info = crate::pricing::lookup(model.unwrap_or(""));
    // tokens_total is not split into input/output here, so blend the two rates.
    let blended = (info.input_per_mtok + info.output_per_mtok) / 2.0;
    tokens_total as f64 * blended / 1_000_000.0
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentSample {
    pub key: String,
    pub tokens_delta: u64,
    pub cpu_percent: u32,
    pub memory_bytes: u64,
    pub running: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GlobalSample {
    pub tokens_per_sec: f64,
    pub cost_total: f64,
    pub live: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectRollup {
    pub project: String,
    pub tokens_per_min: f64,
    pub cost_total: f64,
    pub live: usize,
    pub dirty_files: usize,
}

pub fn session_key(session: &AgentSession) -> String {
    format!(
        "{}:{}",
        session.agent,
        session
            .native_id
            .clone()
            .unwrap_or_else(|| session.cwd.display().to_string())
    )
}

pub struct MetricsHistory {
    capacity: usize,
    global: VecDeque<GlobalSample>,
    agents: VecDeque<Vec<AgentSample>>,
    prev_tokens: BTreeMap<String, i64>,
    prev_time: Option<SystemTime>,
    projects: Vec<ProjectRollup>,
}

impl MetricsHistory {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            global: VecDeque::with_capacity(capacity),
            agents: VecDeque::with_capacity(capacity),
            prev_tokens: BTreeMap::new(),
            prev_time: None,
            projects: Vec::new(),
        }
    }

    pub fn push(&mut self, snapshot: &AmbientSnapshot) {
        let mut samples = Vec::with_capacity(snapshot.sessions.len());
        for session in &snapshot.sessions {
            let key = session_key(session);
            let current = session.tokens_total.unwrap_or(0);
            let delta = match self.prev_tokens.get(&key) {
                Some(prev) => (current - prev).max(0) as u64,
                None => 0,
            };
            self.prev_tokens.insert(key.clone(), current);
            let (cpu_percent, memory_bytes) = session
                .process
                .as_ref()
                .map(|p| (p.cpu_percent, p.memory_bytes))
                .unwrap_or((0, 0));
            samples.push(AgentSample {
                key,
                tokens_delta: delta,
                cpu_percent,
                memory_bytes,
                running: session.status == SessionStatus::Running,
            });
        }

        let elapsed = self
            .prev_time
            .and_then(|prev| snapshot.generated_at.duration_since(prev).ok())
            .map(|d| d.as_secs_f64())
            .filter(|s| *s > 0.0)
            .unwrap_or(1.0);
        let tokens_delta_total: u64 = samples.iter().map(|s| s.tokens_delta).sum();
        let tokens_per_sec = tokens_delta_total as f64 / elapsed;
        let cost_total: f64 = snapshot
            .sessions
            .iter()
            .map(|s| estimate_session_cost(s.model.as_deref(), s.tokens_total.unwrap_or(0).max(0) as u64))
            .sum();
        let live = samples.iter().filter(|s| s.running).count();
        let global = GlobalSample { tokens_per_sec, cost_total, live };
        if self.global.len() == self.capacity {
            self.global.pop_front();
        }
        self.global.push_back(global);

        if self.agents.len() == self.capacity {
            self.agents.pop_front();
        }
        self.agents.push_back(samples.clone());

        // Compute per-project rollups
        use std::collections::BTreeMap as Map;
        let mut by_project: Map<String, ProjectRollup> = Map::new();
        for (session, sample) in snapshot.sessions.iter().zip(samples.iter()) {
            let project = session.repo_name();
            let entry = by_project.entry(project.clone()).or_insert(ProjectRollup {
                project,
                tokens_per_min: 0.0,
                cost_total: 0.0,
                live: 0,
                dirty_files: 0,
            });
            entry.tokens_per_min += sample.tokens_delta as f64 / elapsed * 60.0;
            entry.cost_total +=
                estimate_session_cost(session.model.as_deref(), session.tokens_total.unwrap_or(0).max(0) as u64);
            if sample.running {
                entry.live += 1;
            }
            entry.dirty_files += session.dirty_count();
        }
        self.projects = by_project.into_values().collect();
        self.projects.sort_by(|a, b| b.tokens_per_min.total_cmp(&a.tokens_per_min));

        self.prev_time = Some(snapshot.generated_at);
    }

    pub fn throughput_series(&self) -> Vec<f64> {
        self.global.iter().map(|g| g.tokens_per_sec).collect()
    }

    pub fn latest_global(&self) -> Option<&GlobalSample> {
        self.global.back()
    }

    pub fn projects(&self) -> &[ProjectRollup] {
        &self.projects
    }

    #[cfg(test)]
    fn last_agents(&self) -> Option<&Vec<AgentSample>> {
        self.agents.back()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AgentKind, AgentSession};
    use std::path::PathBuf;
    use std::time::Duration;

    fn session(key_id: &str, tokens: i64, running: bool) -> AgentSession {
        AgentSession {
            agent: AgentKind::Claude,
            native_id: Some(key_id.to_string()),
            title: None,
            command: None,
            cwd: PathBuf::from("/x"),
            pid: None,
            status: if running { SessionStatus::Running } else { SessionStatus::Recent },
            started_at: None,
            updated_at: None,
            model: None,
            tokens_total: Some(tokens),
            git_branch: None,
            journal_path: None,
            process: None,
            git: None,
        }
    }

    fn snap(at: u64, sessions: Vec<AgentSession>) -> AmbientSnapshot {
        AmbientSnapshot {
            sessions,
            generated_at: SystemTime::UNIX_EPOCH + Duration::from_secs(at),
            activity: Vec::new(),
        }
    }

    #[test]
    fn new_session_contributes_zero_delta_on_first_sight() {
        let mut h = MetricsHistory::new(8);
        h.push(&snap(1, vec![session("a", 1000, true)]));
        let agents = h.last_agents().expect("a tick");
        assert_eq!(agents[0].tokens_delta, 0, "first sight must not spike");
    }

    #[test]
    fn growing_tokens_yield_positive_delta_next_tick() {
        let mut h = MetricsHistory::new(8);
        h.push(&snap(1, vec![session("a", 1000, true)]));
        h.push(&snap(2, vec![session("a", 1500, true)]));
        let agents = h.last_agents().expect("a tick");
        assert_eq!(agents[0].tokens_delta, 500);
    }

    #[test]
    fn shrinking_tokens_clamp_to_zero() {
        let mut h = MetricsHistory::new(8);
        h.push(&snap(1, vec![session("a", 1000, true)]));
        h.push(&snap(2, vec![session("a", 200, true)]));
        assert_eq!(h.last_agents().unwrap()[0].tokens_delta, 0);
    }

    #[test]
    fn throughput_divides_delta_by_elapsed_seconds() {
        let mut h = MetricsHistory::new(8);
        h.push(&snap(0, vec![session("a", 0, true)]));
        // +1000 tokens over 2 seconds → 500 tok/s
        h.push(&snap(2, vec![session("a", 1000, true)]));
        let series = h.throughput_series();
        assert_eq!(series.last().copied(), Some(500.0));
    }

    #[test]
    fn latest_global_reports_live_count() {
        let mut h = MetricsHistory::new(8);
        h.push(&snap(1, vec![session("a", 0, true), session("b", 0, false)]));
        assert_eq!(h.latest_global().unwrap().live, 1);
    }

    #[test]
    fn global_ring_buffer_bounded_by_capacity() {
        let mut h = MetricsHistory::new(3);
        for t in 0..10 {
            h.push(&snap(t, vec![session("a", (t * 100) as i64, true)]));
        }
        assert!(h.throughput_series().len() <= 3);
    }

    #[test]
    fn projects_aggregate_sessions_by_repo() {
        let mut h = MetricsHistory::new(8);
        let mut a1 = session("a", 0, true);
        a1.cwd = std::path::PathBuf::from("/code/foo");
        let mut a2 = session("b", 0, false);
        a2.cwd = std::path::PathBuf::from("/code/foo");
        h.push(&snap(0, vec![a1.clone(), a2.clone()]));
        let mut a1b = session("a", 600, true);
        a1b.cwd = std::path::PathBuf::from("/code/foo");
        let mut a2b = session("b", 0, false);
        a2b.cwd = std::path::PathBuf::from("/code/foo");
        h.push(&snap(60, vec![a1b, a2b])); // +600 tokens over 60s for project "foo"
        let foo = h.projects().iter().find(|p| p.project == "foo").expect("foo rollup");
        assert_eq!(foo.live, 1);
        assert!((foo.tokens_per_min - 600.0).abs() < 1.0, "got {}", foo.tokens_per_min);
    }
}
