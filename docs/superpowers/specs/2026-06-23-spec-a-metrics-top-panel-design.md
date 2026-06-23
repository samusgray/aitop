# Spec A — Metrics History + Top-Panel Visuals

**Date:** 2026-06-23
**Status:** Approved (design)
**Part of:** activity-visuals (A → B → C)

## Problem

The top "agent activity" panel (`render_skyline`, `src/dashboard.rs`) is a bar chart of a single
scalar score derived by string-diffing `snapshot.activity` between ticks (`ActivityScorer`). It does
not convey anything quantitative and does not "wow." aitop already collects rich per-tick data
(token totals, CPU/MEM, cost) but discards all of it except that one scalar.

## Goal

Replace the skyline with a btop-style, gradient **braille area-graph** of real aggregate **token
throughput**, plus a compact readout (throughput, total cost, burn rate, live count), and add a thin
**per-project heat ribbon** below it. Introduce a reusable `MetricsHistory` time-series foundation
that Spec C (swimlane) will also consume.

Constraints: aitop stays offline, read-only, no new network calls, no LLM. Pure local computation
over data already in `AmbientSnapshot`.

## Components

### Component 1 — `MetricsHistory` (new module `src/metrics.rs`)

A per-tick time-series store. On each new snapshot it computes deltas versus the previous snapshot
it saw (it owns the "previous" bookkeeping, replacing `ActivityScorer`).

```rust
pub struct AgentSample {
    pub key: String,         // agent + native_id
    pub tokens_delta: u64,   // tokens added since last tick (max(0, now - prev))
    pub cpu_percent: u32,
    pub memory_bytes: u64,
    pub running: bool,
}

pub struct GlobalSample {
    pub tokens_per_sec: f64, // tokens_delta summed / seconds since last tick
    pub cost_total: f64,     // aggregate estimated cost across sessions
    pub live: usize,         // running session count
}

pub struct ProjectRollup {
    pub project: String,
    pub tokens_per_min: f64,
    pub cost_total: f64,
    pub live: usize,
    pub dirty_files: usize,
}

pub struct MetricsHistory {
    capacity: usize,
    global: VecDeque<GlobalSample>,     // ring buffer, len <= capacity
    agents: VecDeque<Vec<AgentSample>>, // per-tick agent samples (for Spec C)
    projects: Vec<ProjectRollup>,       // current rollups (recomputed each tick)
    prev_tokens: BTreeMap<String, i64>, // last seen tokens_total per session key
    prev_time: Option<SystemTime>,
}

impl MetricsHistory {
    pub fn new(capacity: usize) -> Self;
    pub fn push(&mut self, snapshot: &AmbientSnapshot);   // compute deltas, append, evict
    pub fn throughput_series(&self) -> Vec<f64>;          // tokens_per_sec history (for #1)
    pub fn latest_global(&self) -> Option<&GlobalSample>;
    pub fn projects(&self) -> &[ProjectRollup];           // for #3
    pub fn agent_history(&self) -> &VecDeque<Vec<AgentSample>>; // for Spec C
}
```

Token delta per session: `max(0, current tokens_total − prev tokens_total)`. New sessions seed
`prev` with their current value and contribute 0 on first sight (no startup spike — mirrors the
existing skyline warmup-suppression intent). Cost uses `crate::pricing` over token totals and model.

`session key` = `format!("{}:{}", session.agent, session.native_id.unwrap_or(cwd))`, matching the
merge key used in `app::merge_sessions`.

### Component 2 — #1 Throughput graph (replaces `render_skyline`)

A new `render_throughput(frame, history, area)` renders, inside the existing top panel rect:
- A **braille area-graph** of `throughput_series()` using `ratatui::widgets::canvas::Canvas` with
  `Marker::Braille`, y-axis auto-scaled to the series max (with a sane floor), x-axis = time
  (oldest→newest, left→right).
- **Gradient fill:** color each column by its normalized height — cool (blue/cyan) low → green mid →
  yellow/red high. (Implemented by drawing per-column points with a height-derived color.)
- A one-line **readout** at the top of the panel: `▲ {tok/s} tok/s · ${cost} · ${rate}/hr · {n} live`
  where burn rate = cost delta over the window extrapolated to /hr.

The panel keeps its current height (5 compact / 7 normal). The title becomes `agent throughput`.

### Component 3 — #3 Project heat ribbon (new thin strip)

A new `render_heat_ribbon(frame, history, area)` renders a single-line strip of per-project cells:
`aitop▇ loco▇ proto░ …` where each project name is followed by a heat glyph/background colored by a
normalized "heat" score (default: `tokens_per_min`, fallback to `cost_total` ratio). Projects sorted
by heat descending; overflow truncated with `+N`. Idle projects render dim.

**Layout change** (`draw_monitor`): insert a `Constraint::Length(1)` row between the throughput panel
(`vertical[1]`) and the main split for the ribbon. Reduce nothing else; the ribbon is 1 line.

### Component 4 — Wiring

`run_loop` and `spawn_snapshot_worker` already hold an `ActivitySkyline` + `ActivityScorer`. Replace
both with a single `MetricsHistory` threaded through `DrawContext`. `draw_tail` currently also takes
`skyline` — it only used it for `samples.len()` as a tick counter; replace with
`history.throughput_series().len()`.

## Data Flow

```
AmbientSnapshot (per tick)
  → MetricsHistory::push        // deltas vs previous, ring-buffer append
  → draw_monitor
      render_throughput(history) // #1 braille gradient graph + readout
      render_heat_ribbon(history)// #3 per-project strip
```

## Dependencies

None new. `ratatui` 0.29 already provides `widgets::canvas::Canvas` and `symbols::Marker::Braille`.

## Testing (TDD)

- `MetricsHistory::push` computes correct token deltas (new session → 0; increasing total → delta;
  decreasing/reset → 0 clamp).
- Ring buffer evicts at capacity; `throughput_series` length bounded.
- `tokens_per_sec` divides by elapsed seconds (use injected timestamps, not wall clock).
- Project rollups aggregate sessions by repo (counts, dirty files, live).
- Graph/ribbon rendering: a structural smoke test via ratatui `TestBackend` (non-empty buffer, no
  panic at tiny sizes); color fidelity verified manually.

## Scope / YAGNI

- No per-tick event-type buckets yet (Spec C adds them to `MetricsHistory`).
- No interactivity on the ribbon yet (Spec B adds project filtering).
- Keep a single built-in gradient palette.
