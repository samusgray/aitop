use std::{
    fmt,
    path::{Path, PathBuf},
    time::SystemTime,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AgentKind {
    Claude,
    Codex,
}

impl fmt::Display for AgentKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Claude => write!(f, "claude"),
            Self::Codex => write!(f, "codex"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    Running,
    Recent,
    Done,
    Unknown,
}

impl fmt::Display for SessionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Running => write!(f, "running"),
            Self::Recent => write!(f, "recent"),
            Self::Done => write!(f, "done"),
            Self::Unknown => write!(f, "unknown"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessStats {
    pub cpu_percent: u32,
    pub memory_bytes: u64,
    pub child_pids: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitStatus {
    pub root: PathBuf,
    pub branch: Option<String>,
    pub dirty_files: Vec<DirtyFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirtyFile {
    pub code: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSession {
    pub agent: AgentKind,
    pub native_id: Option<String>,
    pub title: Option<String>,
    pub command: Option<String>,
    pub cwd: PathBuf,
    pub pid: Option<u32>,
    pub status: SessionStatus,
    pub started_at: Option<SystemTime>,
    pub updated_at: Option<SystemTime>,
    pub model: Option<String>,
    pub tokens_total: Option<i64>,
    pub git_branch: Option<String>,
    pub journal_path: Option<PathBuf>,
    pub process: Option<ProcessStats>,
    pub git: Option<GitStatus>,
}

impl AgentSession {
    pub fn repo_name(&self) -> String {
        self.git
            .as_ref()
            .map(|git| crate::git::project_name(&git.root))
            .unwrap_or_else(|| crate::git::project_name(&self.cwd))
    }

    pub fn display_title(&self) -> String {
        self.native_id.clone().unwrap_or_else(|| "-".to_string())
    }

    pub fn dirty_count(&self) -> usize {
        self.git
            .as_ref()
            .map(|git| git.dirty_files.len())
            .unwrap_or(0)
    }
}

pub fn unix_seconds(seconds: i64) -> Option<SystemTime> {
    if seconds < 0 {
        return None;
    }
    Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(seconds as u64))
}

pub fn unix_millis(millis: i64) -> Option<SystemTime> {
    if millis < 0 {
        return None;
    }
    Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(millis as u64))
}

pub fn elapsed_label(start: Option<SystemTime>, end: SystemTime) -> String {
    let Some(start) = start else {
        return "--m --s".to_string();
    };
    let seconds = end
        .duration_since(start)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    format!("{:02}m {:02}s", seconds / 60, seconds % 60)
}

/// Elapsed time to display for a session. A running session is still working,
/// so it counts to `now`; a finished or idle session counts only the span of
/// real activity (start to last update), not wall-clock age since launch.
pub fn session_elapsed_label(
    status: SessionStatus,
    started_at: Option<SystemTime>,
    updated_at: Option<SystemTime>,
    now: SystemTime,
) -> String {
    let end = match status {
        SessionStatus::Running => now,
        _ => updated_at.unwrap_or(now),
    };
    elapsed_label(started_at, end)
}

pub fn time_label(time: Option<SystemTime>) -> String {
    let Some(time) = time else {
        return "-".to_string();
    };
    let datetime: chrono::DateTime<chrono::Local> = time.into();
    datetime.format("%H:%M:%S").to_string()
}

pub fn path_home_display(path: &Path) -> String {
    if let Some(home) = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf())
        && let Ok(stripped) = path.strip_prefix(&home)
    {
        return format!("~/{}", stripped.display());
    }
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn recent_session_elapsed_spans_activity_not_wall_clock_age() {
        // 100s of real work (started 100, last update 200), but "now" is a day later.
        let label = session_elapsed_label(
            SessionStatus::Recent,
            Some(at(100)),
            Some(at(200)),
            at(100_000),
        );
        assert_eq!(label, "01m 40s");
    }

    #[test]
    fn running_session_elapsed_counts_to_now() {
        // A live session is still working, so it measures to now.
        let label = session_elapsed_label(
            SessionStatus::Running,
            Some(at(100)),
            Some(at(120)),
            at(160),
        );
        assert_eq!(label, "01m 00s");
    }

    #[test]
    fn missing_update_falls_back_to_now() {
        let label =
            session_elapsed_label(SessionStatus::Recent, Some(at(100)), None, at(160));
        assert_eq!(label, "01m 00s");
    }
}
