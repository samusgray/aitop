# Spec A — Metrics History + Top-Panel Visuals — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the scalar activity skyline with a `MetricsHistory` time-series foundation, a btop-style braille gradient throughput graph, and a per-project heat ribbon.

**Architecture:** New `src/metrics.rs` computes per-tick deltas from `AmbientSnapshot` (token throughput, cost, per-project rollups, per-agent samples) into ring buffers. `src/dashboard.rs` renders the throughput graph (ratatui `Canvas`+Braille) and a one-line heat ribbon, replacing `ActivitySkyline`/`ActivityScorer`.

**Tech Stack:** Rust 2024, ratatui 0.29 (`widgets::canvas::Canvas`, `symbols::Marker::Braille`), existing `pricing` module.

## Global Constraints

- Edition 2024. `cargo clippy --all-targets --all-features -- -D warnings` and `cargo test` must pass after every task.
- aitop stays offline and read-only — no new dependencies, no network, no LLM.
- Timestamps for rate math are passed in / read from the snapshot's `generated_at`; never call wall-clock in pure logic (keeps tests deterministic).
- Session identity key: `format!("{}:{}", session.agent, session.native_id.clone().unwrap_or_else(|| session.cwd.display().to_string()))`.

---

### Task 1: `MetricsHistory` skeleton + token-delta computation

**Files:**
- Create: `src/metrics.rs`
- Modify: `src/lib.rs` (add `pub mod metrics;`)
- Test: `src/metrics.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Produces:
  - `pub struct AgentSample { pub key: String, pub tokens_delta: u64, pub cpu_percent: u32, pub memory_bytes: u64, pub running: bool }`
  - `pub struct GlobalSample { pub tokens_per_sec: f64, pub cost_total: f64, pub live: usize }`
  - `pub struct MetricsHistory { /* private */ }` with `pub fn new(capacity: usize) -> Self` and `pub fn push(&mut self, snapshot: &crate::app::AmbientSnapshot)`.
  - `pub fn session_key(session: &crate::model::AgentSession) -> String`

- [ ] **Step 1: Register module**

In `src/lib.rs` add (keep alphabetical, after `feed`):
```rust
pub mod metrics;
```

- [ ] **Step 2: Write the failing test**

Create `src/metrics.rs`:
```rust
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

    pub fn push(&mut self, _snapshot: &AmbientSnapshot) {
        // implemented in Step 4
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
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test --lib metrics::tests`
Expected: FAIL — `tokens_delta` is 0/absent because `push` is a stub (asserts on `growing_tokens...` and `shrinking...` fail; `agents` empty → panic on index).

- [ ] **Step 4: Implement `push` (deltas only; global/rollups in later tasks)**

Replace the stub `push`:
```rust
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
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib metrics::tests`
Expected: PASS (3 tests).

- [ ] **Step 6: Verify clippy + suite**

Run: `cargo clippy --all-targets --all-features -- -D warnings && cargo test`
Expected: clean, all pass.

- [ ] **Step 7: Commit**

```bash
git add src/lib.rs src/metrics.rs
git commit -m "feat: MetricsHistory with per-agent token deltas"
```

---

### Task 2: Global throughput series + cost + ring buffer

**Files:**
- Modify: `src/metrics.rs`
- Test: `src/metrics.rs` tests

**Interfaces:**
- Consumes: `MetricsHistory`, `AgentSample` (Task 1).
- Produces:
  - `pub fn throughput_series(&self) -> Vec<f64>` (tokens_per_sec, oldest→newest)
  - `pub fn latest_global(&self) -> Option<&GlobalSample>`
  - global ring buffer populated in `push`.

- [ ] **Step 1: Write the failing test**

Add to `src/metrics.rs` tests:
```rust
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib metrics::tests::throughput`
Expected: FAIL — `cannot find method throughput_series`.

- [ ] **Step 3: Implement global sampling**

In `push`, after building `samples` and before pushing to `self.agents`, compute the global sample:
```rust
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
```

Add accessors:
```rust
    pub fn throughput_series(&self) -> Vec<f64> {
        self.global.iter().map(|g| g.tokens_per_sec).collect()
    }

    pub fn latest_global(&self) -> Option<&GlobalSample> {
        self.global.back()
    }
```

**Pricing helper:** `pricing::estimate_cost(input, output, model)` needs an input/output split, but
the ambient snapshot only carries `tokens_total`. Add this module-level helper to `metrics.rs` and use
it here and in Task 3:
```rust
fn estimate_session_cost(model: Option<&str>, tokens_total: u64) -> f64 {
    let info = crate::pricing::lookup(model.unwrap_or(""));
    // tokens_total is not split into input/output here, so blend the two rates.
    let blended = (info.input_per_mtok + info.output_per_mtok) / 2.0;
    tokens_total as f64 * blended / 1_000_000.0
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib metrics::tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/metrics.rs
git commit -m "feat: global token-throughput series and cost in MetricsHistory"
```

---

### Task 3: Per-project rollups

**Files:**
- Modify: `src/metrics.rs`
- Test: `src/metrics.rs` tests

**Interfaces:**
- Produces:
  - `pub struct ProjectRollup { pub project: String, pub tokens_per_min: f64, pub cost_total: f64, pub live: usize, pub dirty_files: usize }`
  - `pub fn projects(&self) -> &[ProjectRollup]`

- [ ] **Step 1: Write the failing test**

Add to tests (uses `repo_name`, which derives from cwd when no git):
```rust
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib metrics::tests::projects_aggregate`
Expected: FAIL — `cannot find method projects`.

- [ ] **Step 3: Implement rollups**

Add a `projects: Vec<ProjectRollup>` field to the struct (init `Vec::new()` in `new`). The `ProjectRollup` struct goes at module top. In `push`, after the global sample, recompute rollups for this tick (per-minute rate uses the same `elapsed`):
```rust
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
```

Add accessor `pub fn projects(&self) -> &[ProjectRollup] { &self.projects }`. (Match the `estimate_cost` resolution from Task 2.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib metrics::tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/metrics.rs
git commit -m "feat: per-project rollups in MetricsHistory"
```

---

### Task 4: Replace `ActivitySkyline`/`ActivityScorer` wiring with `MetricsHistory`

**Files:**
- Modify: `src/dashboard.rs` (`run_loop` ~264, `spawn_snapshot_worker` ~329, `DrawContext` ~460, `draw` dispatch, `draw_monitor` ~/the `render_skyline` call, `draw_tail` signature which takes `skyline`, the `#[cfg(test)] skyline_rows`/`skyline_columns` helpers and their tests)
- Test: existing suite must still pass; add no new logic test (covered by Tasks 1-3).

**Interfaces:**
- Consumes: `MetricsHistory::new`, `push`, `throughput_series` (Tasks 1-2).
- Produces: `DrawContext` carries `history: &MetricsHistory` instead of `skyline`.

- [ ] **Step 1: Swap the type in `run_loop` and the worker**

In `src/dashboard.rs`, replace the `ActivitySkyline` + `ActivityScorer` locals in `run_loop` with:
```rust
    let mut history = crate::metrics::MetricsHistory::new(240);
    history.push(&snapshot);
```
Remove `let mut scorer = ...` and the `skyline.push_snapshot(&snapshot, &mut scorer)` calls; replace each with `history.push(&snapshot)`. In `spawn_snapshot_worker`, the worker does not own the skyline (it sends snapshots); confirm no `ActivityScorer` remains there.

- [ ] **Step 2: Update `DrawContext` and `draw`**

Change `DrawContext.skyline: &'a ActivitySkyline` to `history: &'a crate::metrics::MetricsHistory`. Update `draw` to pass `context.history`. In `draw_monitor`, replace the `skyline: &ActivitySkyline` parameter with `history: &MetricsHistory` (the throughput render in Task 5 consumes it). In `draw_tail`, the `skyline` parameter was only used as a tick counter (`skyline.samples.len()`); replace with `history.throughput_series().len()` — change its signature to take `history: &MetricsHistory` and update both the `KeyAction::Refresh` tick and the worker tick references accordingly.

- [ ] **Step 3: Remove dead skyline code**

Delete `struct ActivitySkyline`, `struct ActivitySample`, `struct ActivityScorer`, `impl`s, `render_skyline`, `skyline_columns`, `skyline_rows`, `SkylineColumn`, and their `#[cfg(test)]` tests. (Task 5 adds the replacement renderer.) Leave a temporary `fn render_throughput(_f: &mut Frame, _h: &MetricsHistory, _area: Rect) {}` stub so `draw_monitor` compiles; Task 5 fills it.

- [ ] **Step 4: Build + run suite**

Run: `cargo build && cargo test`
Expected: compiles; all remaining tests pass (skyline tests removed, metrics tests present).

- [ ] **Step 5: Clippy**

Run: `cargo clippy --all-targets --all-features -- -D warnings`
Expected: clean (the stub may warn unused — prefix params with `_` as shown).

- [ ] **Step 6: Commit**

```bash
git add src/dashboard.rs
git commit -m "refactor: drive the top panel from MetricsHistory"
```

---

### Task 5: #1 Braille gradient throughput graph

**Files:**
- Modify: `src/dashboard.rs` (`render_throughput`)
- Test: `src/dashboard.rs` tests (structural smoke test); color verified manually.

**Interfaces:**
- Consumes: `MetricsHistory::throughput_series`, `latest_global`.
- Produces: `fn render_throughput(frame: &mut Frame<'_>, history: &MetricsHistory, area: Rect)`.

- [ ] **Step 1: Write the failing smoke test**

Add to `src/dashboard.rs` tests (renders into a `TestBackend` buffer and asserts non-empty, no panic):
```rust
    #[test]
    fn throughput_renders_without_panicking() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut h = crate::metrics::MetricsHistory::new(64);
        for t in 0..30 {
            h.push(&super::tests_support_snapshot(t)); // helper below
        }
        let mut term = Terminal::new(TestBackend::new(80, 8)).unwrap();
        term.draw(|f| super::render_throughput(f, &h, f.area())).unwrap();
        let content: String = term.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert!(content.contains("tok/s"), "readout present");
    }
```
Add a tiny test helper near the test module:
```rust
    #[cfg(test)]
    pub(crate) fn tests_support_snapshot(at: u64) -> crate::app::AmbientSnapshot {
        use crate::model::{AgentKind, AgentSession, SessionStatus};
        let s = AgentSession {
            agent: AgentKind::Claude, native_id: Some("a".into()), title: None, command: None,
            cwd: std::path::PathBuf::from("/x"), pid: None, status: SessionStatus::Running,
            started_at: None, updated_at: None, model: None,
            tokens_total: Some((at * 100) as i64), git_branch: None, journal_path: None,
            process: None, git: None,
        };
        crate::app::AmbientSnapshot {
            sessions: vec![s],
            generated_at: std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(at),
            activity: Vec::new(),
        }
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib dashboard::tests::throughput_renders`
Expected: FAIL — the stub renders nothing, so `"tok/s"` is absent.

- [ ] **Step 3: Implement `render_throughput`**

Implement the renderer. Required behavior (exact draw code is the implementer's, verified manually):
- Draw a bordered block titled `agent throughput`.
- Top line inside: a readout `▲ {tok/s} tok/s · ${cost_total} · ${rate}/hr · {live} live` from
  `latest_global()` (format tok/s with `crate::feed`/`pricing` compact helpers or `{:.0}`/`{:.1}`).
  Burn rate `/hr`: estimate from the change in `cost_total` across the series window extrapolated to
  one hour; if the window is too short, show `-`.
- Below the readout: a `ratatui::widgets::canvas::Canvas` with `.marker(Marker::Braille)`,
  `x_bounds([0, series.len() as f64])`, `y_bounds([0, max(series).max(1.0)])`, painting the series as
  a filled area: for each sample index `i`, draw points from `y=0` up to `series[i]` (a vertical run)
  so the area fills, colored by normalized height — `Color::Rgb` interpolated cool→green→hot
  (e.g. `<0.33` cyan, `<0.66` green, else a red/orange).
- Handle empty series (draw just the readout with zeros). Never panic on width/height < 3.

Reference: ratatui `Canvas::default().marker(Marker::Braille).paint(|ctx| { ctx.draw(&Points{coords, color}) })`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib dashboard::tests::throughput_renders`
Expected: PASS.

- [ ] **Step 5: Manual visual check**

Run `cargo run` (or via tmux), open the monitor, confirm the top panel shows a gradient braille graph that rises/falls with token activity and a live readout. Resize narrow/short → no panic.

- [ ] **Step 6: Clippy + suite + commit**

Run: `cargo clippy --all-targets --all-features -- -D warnings && cargo test`
```bash
git add src/dashboard.rs
git commit -m "feat: braille gradient token-throughput graph"
```

---

### Task 6: #3 Project heat ribbon + layout row

**Files:**
- Modify: `src/dashboard.rs` (`draw_monitor` layout, new `render_heat_ribbon`)
- Test: `src/dashboard.rs` tests (smoke); color verified manually.

**Interfaces:**
- Consumes: `MetricsHistory::projects` → `&[ProjectRollup]`.
- Produces: `fn render_heat_ribbon(frame: &mut Frame<'_>, history: &MetricsHistory, area: Rect)`.

- [ ] **Step 1: Write the failing smoke test**

```rust
    #[test]
    fn heat_ribbon_lists_projects() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut h = crate::metrics::MetricsHistory::new(8);
        h.push(&super::tests_support_snapshot(0));
        h.push(&super::tests_support_snapshot(1));
        let mut term = Terminal::new(TestBackend::new(80, 1)).unwrap();
        term.draw(|f| super::render_heat_ribbon(f, &h, f.area())).unwrap();
        let content: String = term.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert!(content.contains("x"), "project name (cwd '/x' → repo 'x') present");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib dashboard::tests::heat_ribbon`
Expected: FAIL — `cannot find function render_heat_ribbon`.

- [ ] **Step 3: Implement `render_heat_ribbon`**

Behavior: render a single line of cells, one per project from `history.projects()` (already sorted by
`tokens_per_min` desc). Each cell = `"{name}"` followed by a heat glyph, with a background/foreground
color from a normalized heat score (`tokens_per_min` divided by the max across projects; idle → dim).
Truncate to width with a trailing `+N` when projects overflow. Use `Line`/`Span` with `Style::bg`.
Never panic on width 0.

- [ ] **Step 4: Insert the layout row in `draw_monitor`**

Change the vertical constraints to insert a `Constraint::Length(1)` ribbon row between the throughput
panel and the main split:
```rust
        .constraints([
            Constraint::Length(3),            // header
            Constraint::Length(skyline_height), // throughput panel
            Constraint::Length(1),            // heat ribbon
            Constraint::Min(9),               // main split
            Constraint::Length(activity_height),
            Constraint::Length(2),
        ])
```
Update the subsequent `vertical[..]` indices: throughput = `vertical[1]`, ribbon = `vertical[2]`,
main = `vertical[3]`, activity = `vertical[4]`, footer = `vertical[5]`. Call
`render_heat_ribbon(frame, history, vertical[2])`.

- [ ] **Step 5: Run smoke test + manual check**

Run: `cargo test --lib dashboard::tests::heat_ribbon`
Then `cargo run` → confirm a one-line heat strip under the graph; hottest project first.

- [ ] **Step 6: Clippy + suite + commit**

Run: `cargo clippy --all-targets --all-features -- -D warnings && cargo test`
```bash
git add src/dashboard.rs
git commit -m "feat: per-project heat ribbon under the throughput graph"
```

---

## Self-Review

**Spec coverage:** MetricsHistory (Tasks 1-3) ✓; #1 throughput graph (Task 5) ✓; #3 heat ribbon (Task 6) ✓; replace skyline/scorer wiring (Task 4) ✓; per-agent samples retained for Spec C (Task 1 `agents` buffer) ✓; offline/no-deps ✓; deterministic timestamps via `generated_at` ✓.

**Placeholder scan:** The only deferred detail is exact ratatui draw code in Tasks 5-6, which is render code verified manually (the spec states color is manual); interfaces, behavior, and structural tests are concrete. The `estimate_cost` call has an explicit fallback instruction if `pricing`'s API differs.

**Type consistency:** `MetricsHistory`, `AgentSample`, `GlobalSample`, `ProjectRollup`, `session_key`, `throughput_series`, `latest_global`, `projects`, `render_throughput`, `render_heat_ribbon` are used identically across tasks. `DrawContext.history` replaces `.skyline` consistently in Task 4. Capacity 240 chosen once.
