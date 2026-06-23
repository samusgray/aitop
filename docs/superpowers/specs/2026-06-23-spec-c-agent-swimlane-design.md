# Spec C — Per-Agent Swimlane Timeline

**Date:** 2026-06-23
**Status:** Approved (design)
**Part of:** activity-visuals (A → B → C)

## Problem

Even with the throughput graph (Spec A), there is no way to see *per-agent* rhythm over time — who
has been grinding, who stalled, who errored and recovered. The data exists (per-tick agent samples in
`MetricsHistory`, plus event types from journals) but is not visualized per agent.

## Goal

Add a **swimlane timeline**: one horizontal lane per active agent, divided into time slices colored by
what that agent was doing each slice (thinking / tool-call / output / idle / error). Surface it as an
alternate render mode of the top panel, toggled with a key, reusing `MetricsHistory`.

Constraints: offline, read-only. Builds on Spec A; no new data sources beyond what A and B already
read.

## Components

### Component 1 — Per-tick agent state in `MetricsHistory` (extend Spec A)

Extend `AgentSample` (from Spec A) with a coarse activity class for that tick:

```rust
pub enum AgentActivity {
    Idle,      // no token delta, not running busy
    Thinking,  // recent thinking event / running with no output delta
    Tool,      // recent tool-call/result
    Output,    // token output delta this tick
    Error,     // recent error annotation
}

// AgentSample gains:
pub activity: AgentActivity,
```

`MetricsHistory::push` classifies each agent's tick: `Error` if the session's latest activity carries
an error signal; else `Output` if `tokens_delta > 0`; else `Tool` if the session's most recent event
was a tool call/result; else `Thinking` if running; else `Idle`. The "most recent event kind" comes
from the snapshot's per-session activity (cheap — no extra journal reads), falling back to status.
(`MetricsHistory` already retains `agent_history: VecDeque<Vec<AgentSample>>`.)

A helper exposes lanes ready to render:

```rust
pub struct Lane { pub key: String, pub label: String, pub slices: Vec<AgentActivity> }
impl MetricsHistory {
    /// One lane per agent seen in the window, slices oldest→newest, gaps filled with Idle.
    pub fn lanes(&self, max_lanes: usize) -> Vec<Lane>;
}
```

### Component 2 — Swimlane renderer

`render_swimlane(frame, history, area)` draws, in the top panel rect:
- One row per `Lane` (label = repo/agent, truncated), the remaining width split into the most recent
  `width` slices.
- Each slice is a colored block (`▓`/`█`) keyed by `AgentActivity`: Thinking=blue, Tool=yellow,
  Output=green, Error=red, Idle=dim/space. A small legend on the title line.
- More lanes than rows → show top `max_lanes` by recent activity, with a `+N more` indicator.

### Component 3 — Top-panel mode toggle

The top panel gains a mode: `TopPanel { Throughput, Swimlane }`, toggled with **`v`** (cycle view) in
the monitor. `draw_monitor` dispatches to `render_throughput` (Spec A) or `render_swimlane` based on
the mode. The mode is owned by `run_loop` and passed via `DrawContext`. The heat ribbon (Spec A #3)
remains visible in both modes. The panel title reflects the active mode.

## Data Flow

```
AmbientSnapshot (per tick)
  → MetricsHistory::push  // now also classifies AgentActivity per agent
  → draw_monitor (TopPanel::Swimlane)
      render_swimlane(history)  // lanes(): per-agent colored time slices
```

## Dependencies

None new. Depends on Spec A's `MetricsHistory` and `AgentSample` already being in place.

## Testing (TDD)

- Activity classification precedence (Error > Output > Tool > Thinking > Idle) given a synthetic
  session state.
- `lanes()` produces one lane per agent, slices oldest→newest, idle-fills missing ticks, caps at
  `max_lanes`, orders by recent activity.
- Toggle cycles `Throughput ↔ Swimlane`.
- Rendering smoke test via `TestBackend`; color/legibility verified manually.

## Scope / YAGNI

- Coarse 5-state classification only (no sub-tool breakdown).
- No zoom/pan over history; window = ring-buffer capacity.
- Swimlane shares the top panel (no new layout row beyond the toggle).
