use std::collections::{BTreeMap, VecDeque};
use std::time::SystemTime;

use crate::app::AmbientSnapshot;
use crate::model::{AgentSession, SessionStatus};

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
    #[allow(dead_code)]
    global: VecDeque<GlobalSample>,
    agents: VecDeque<Vec<AgentSample>>,
    prev_tokens: BTreeMap<String, i64>,
    prev_time: Option<SystemTime>,
}

impl MetricsHistory {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            global: VecDeque::with_capacity(capacity),
            agents: VecDeque::with_capacity(capacity),
            prev_tokens: BTreeMap::new(),
            prev_time: None,
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

        if self.agents.len() == self.capacity {
            self.agents.pop_front();
        }
        self.agents.push_back(samples);
        self.prev_time = Some(snapshot.generated_at);
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
}
