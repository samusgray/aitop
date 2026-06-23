# Spec C — Per-Agent Swimlane Timeline — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A per-agent swimlane: one lane per agent, time slices colored by what it was doing each tick, shown as an alternate mode of the top panel.

**Architecture:** Extend Spec A's `MetricsHistory`/`AgentSample` with a per-tick activity class, add a `lanes()` builder, render swimlane rows in the top panel, and add a panel-mode toggle in `run_loop`.

**Tech Stack:** Rust 2024, ratatui 0.29. Depends on Spec A (`MetricsHistory`, `AgentSample`, `agent_history`) already merged.

## Global Constraints

- Edition 2024. `cargo clippy --all-targets --all-features -- -D warnings` and `cargo test` must pass after every task.
- Offline, read-only. Classification uses only data already in the ambient snapshot (token deltas + running flag) — no per-tick journal reads.
- **Scoped reduction from the spec:** the monitor snapshot has no per-session tool/error signal, so the classifier produces three states — `Idle`, `Thinking`, `Output` — not the spec's five. (Tool/Error would require journal reads the monitor avoids; out of scope.)

---

### Task 1: `AgentActivity` classification in `MetricsHistory`

**Files:**
- Modify: `src/metrics.rs` (`AgentSample`, `push`, new `AgentActivity` + `classify`)
- Test: `src/metrics.rs` tests

**Interfaces:**
- Consumes: Spec A `AgentSample`, `MetricsHistory::push`.
- Produces:
  - `pub enum AgentActivity { Idle, Thinking, Output }` (derive `Debug, Clone, Copy, PartialEq, Eq`)
  - `AgentSample` gains `pub activity: AgentActivity`
  - `pub fn classify(running: bool, tokens_delta: u64) -> AgentActivity`

- [ ] **Step 1: Write the failing test**

Add to `src/metrics.rs` tests:
```rust
    #[test]
    fn classify_precedence() {
        assert_eq!(classify(true, 100), AgentActivity::Output);
        assert_eq!(classify(true, 0), AgentActivity::Thinking);
        assert_eq!(classify(false, 0), AgentActivity::Idle);
        assert_eq!(classify(false, 50), AgentActivity::Output); // output even if not flagged running
    }

    #[test]
    fn push_records_activity_on_samples() {
        let mut h = MetricsHistory::new(8);
        h.push(&snap(0, vec![session("a", 0, true)]));
        h.push(&snap(1, vec![session("a", 500, true)]));
        assert_eq!(h.last_agents().unwrap()[0].activity, AgentActivity::Output);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib metrics::tests::classify`
Expected: FAIL — `AgentActivity`/`classify`/`activity` field absent.

- [ ] **Step 3: Implement**

Add the enum at module top:
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentActivity {
    Idle,
    Thinking,
    Output,
}

pub fn classify(running: bool, tokens_delta: u64) -> AgentActivity {
    if tokens_delta > 0 {
        AgentActivity::Output
    } else if running {
        AgentActivity::Thinking
    } else {
        AgentActivity::Idle
    }
}
```
Add `pub activity: AgentActivity` to `AgentSample`. In `push`, set it when building each sample:
`activity: classify(session.status == SessionStatus::Running, delta),`.

- [ ] **Step 4: Run tests + clippy + commit**

Run: `cargo test --lib metrics::tests && cargo clippy --all-targets --all-features -- -D warnings`
```bash
git add src/metrics.rs
git commit -m "feat: per-tick agent activity classification"
```

---

### Task 2: `lanes()` builder

**Files:**
- Modify: `src/metrics.rs`
- Test: `src/metrics.rs` tests

**Interfaces:**
- Consumes: `agent_history` (the `VecDeque<Vec<AgentSample>>` from Spec A), `AgentActivity`.
- Produces:
  - `pub struct Lane { pub key: String, pub label: String, pub slices: Vec<AgentActivity> }`
  - `pub fn lanes(&self, max_lanes: usize) -> Vec<Lane>`

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn lanes_one_per_agent_oldest_to_newest_idle_filled() {
        let mut h = MetricsHistory::new(8);
        h.push(&snap(0, vec![session("a", 0, true)]));                 // a: Thinking
        h.push(&snap(1, vec![session("a", 100, true), session("b", 0, true)])); // a: Output, b: Thinking (first sight → delta 0)
        let lanes = h.lanes(10);
        let a = lanes.iter().find(|l| l.key.ends_with("a")).expect("lane a");
        assert_eq!(a.slices.len(), 2, "one slice per tick");
        assert_eq!(a.slices[1], AgentActivity::Output);
        let b = lanes.iter().find(|l| l.key.ends_with("b")).expect("lane b");
        // b absent in tick 0 → idle-filled
        assert_eq!(b.slices.len(), 2);
        assert_eq!(b.slices[0], AgentActivity::Idle);
    }

    #[test]
    fn lanes_capped() {
        let mut h = MetricsHistory::new(8);
        h.push(&snap(0, vec![session("a",0,true), session("b",0,true), session("c",0,true)]));
        assert!(h.lanes(2).len() <= 2);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib metrics::tests::lanes`
Expected: FAIL — `cannot find method lanes`.

- [ ] **Step 3: Implement `lanes`**

Add the `Lane` struct at module top. Implement:
```rust
    pub fn lanes(&self, max_lanes: usize) -> Vec<Lane> {
        // Collect the set of agent keys seen across the window, preserving a stable order
        // by most-recent activity. Build a per-tick lookup, idle-filling missing ticks.
        use std::collections::BTreeMap;
        let ticks: Vec<&Vec<AgentSample>> = self.agents.iter().collect();
        let mut keys: Vec<String> = Vec::new();
        for tick in &ticks {
            for s in tick.iter() {
                if !keys.contains(&s.key) {
                    keys.push(s.key.clone());
                }
            }
        }
        let mut lanes: Vec<Lane> = keys
            .into_iter()
            .map(|key| {
                let slices = ticks
                    .iter()
                    .map(|tick| {
                        tick.iter()
                            .find(|s| s.key == key)
                            .map(|s| s.activity)
                            .unwrap_or(AgentActivity::Idle)
                    })
                    .collect::<Vec<_>>();
                let label = key.rsplit(':').next().unwrap_or(&key).to_string();
                let _ = BTreeMap::<u8, u8>::new(); // (no-op; keep imports honest if unused — remove)
                Lane { key, label, slices }
            })
            .collect();
        // Order by recent activity (non-idle in the latest slices first), then cap.
        lanes.sort_by_key(|l| {
            let recent_active = l.slices.iter().rev().take(8)
                .filter(|a| **a != AgentActivity::Idle).count();
            std::cmp::Reverse(recent_active)
        });
        lanes.truncate(max_lanes);
        lanes
    }
```
(Remove the `BTreeMap` no-op line; it is only a reminder not to leave unused imports.)

- [ ] **Step 4: Run tests + clippy + commit**

Run: `cargo test --lib metrics::tests && cargo clippy --all-targets --all-features -- -D warnings`
```bash
git add src/metrics.rs
git commit -m "feat: per-agent swimlane lanes from MetricsHistory"
```

---

### Task 3: `render_swimlane`

**Files:**
- Modify: `src/dashboard.rs` (new `render_swimlane`)
- Test: smoke via TestBackend; color verified manually.

**Interfaces:**
- Consumes: `MetricsHistory::lanes`, `AgentActivity`.
- Produces: `fn render_swimlane(frame: &mut Frame<'_>, history: &MetricsHistory, area: Rect)`.

- [ ] **Step 1: Write the failing smoke test**

```rust
    #[test]
    fn swimlane_renders_lane_label() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut h = crate::metrics::MetricsHistory::new(16);
        h.push(&super::tests_support_snapshot(0));
        h.push(&super::tests_support_snapshot(1));
        let mut term = Terminal::new(TestBackend::new(80, 7)).unwrap();
        term.draw(|f| super::render_swimlane(f, &h, f.area())).unwrap();
        let content: String = term.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert!(content.contains("agent timeline"), "titled");
    }
```
(`tests_support_snapshot` was added in Spec A's plan; if absent, add the same helper.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib dashboard::tests::swimlane`
Expected: FAIL — `cannot find function render_swimlane`.

- [ ] **Step 3: Implement `render_swimlane`**

Behavior (exact draw code is the implementer's; verified manually):
- Bordered block titled `agent timeline` with a small legend in the title line: `▎thinking ▎output ▎idle`.
- For each `Lane` from `history.lanes(area_inner_height)`: render one row = a left label (truncated
  repo/agent) + the most-recent `(width - label_width)` slices as colored blocks. Color by
  `AgentActivity`: `Output`=green, `Thinking`=blue/cyan, `Idle`=dark-gray/space. Use a full block `█`
  for active, dim `·`/space for idle.
- More lanes than rows → render the top `inner_height` and a `+N more` line.
- Never panic at tiny sizes / empty history.

- [ ] **Step 4: Run smoke + manual + clippy + commit**

Run: `cargo test --lib dashboard::tests::swimlane && cargo clippy --all-targets --all-features -- -D warnings`. Manual check happens in Task 4 once the toggle exists.
```bash
git add src/dashboard.rs
git commit -m "feat: swimlane renderer for per-agent timeline"
```

---

### Task 4: Top-panel mode toggle

**Files:**
- Modify: `src/dashboard.rs` (`run_loop` state, `handle_key`, `DrawContext`, `draw_monitor`)
- Test: pure toggle test + manual.

**Interfaces:**
- Consumes: `render_throughput` (Spec A), `render_swimlane` (Task 3).
- Produces: `enum TopPanel { Throughput, Swimlane }` with `fn next(self) -> Self`; `DrawContext.top_panel: TopPanel`.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn top_panel_toggles() {
        assert_eq!(super::TopPanel::Throughput.next(), super::TopPanel::Swimlane);
        assert_eq!(super::TopPanel::Swimlane.next(), super::TopPanel::Throughput);
    }
```

- [ ] **Step 2: Run / fail / implement**

Run: `cargo test --lib dashboard::tests::top_panel_toggles` → FAIL. Implement:
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TopPanel { Throughput, Swimlane }
impl TopPanel {
    fn next(self) -> Self {
        match self {
            TopPanel::Throughput => TopPanel::Swimlane,
            TopPanel::Swimlane => TopPanel::Throughput,
        }
    }
}
```
Run → PASS.

- [ ] **Step 3: Wire the toggle**

In `run_loop`, add `let mut top_panel = TopPanel::Throughput;`. Add to `handle_key` (monitor only):
`KeyCode::Char('v') =>` set `*top_panel = top_panel.next()` (thread a `&mut TopPanel` into `handle_key`,
or handle `'v'` in `run_loop` before calling `handle_key`). Add `top_panel: TopPanel` to `DrawContext`.
In `draw_monitor`, dispatch the top panel rect: `match top_panel { Throughput => render_throughput(...),
Swimlane => render_swimlane(...) }`. The heat ribbon (Spec A) renders in both modes.

- [ ] **Step 4: Manual verify**

`cargo run` (or tmux): press `v` to flip the top panel between the throughput graph and the swimlane;
confirm lanes color by activity and the ribbon stays. Resize → no panic.

- [ ] **Step 5: Update docs + clippy + suite + commit**

Add `v: toggle top panel (graph/timeline)` to README controls. Run
`cargo clippy --all-targets --all-features -- -D warnings && cargo test`.
```bash
git add src/dashboard.rs README.md
git commit -m "feat: toggle top panel between throughput graph and swimlane"
```

---

## Self-Review

**Spec coverage:** per-tick activity class in MetricsHistory (Task 1) ✓; `lanes()` (Task 2) ✓; swimlane renderer (Task 3) ✓; top-panel mode toggle (Task 4) ✓. Deviation: 3-state classifier (Idle/Thinking/Output) instead of the spec's 5 — documented in Global Constraints (no tool/error signal in the snapshot without journal reads).

**Placeholder scan:** Logic tasks (1, 2, 4) have complete code + tests. Render task (3) gives signature + behavior + smoke test, color verified manually per the spec. The `lanes()` snippet flags and removes its own no-op import reminder.

**Type consistency:** `AgentActivity` (3 variants), `classify`, `Lane`, `lanes`, `render_swimlane`, `TopPanel`/`next` used identically across tasks. Depends on Spec A's `AgentSample`/`agent_history`/`render_throughput` — A must be merged first (enforced by the A→B→C order).
