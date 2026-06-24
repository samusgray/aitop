use std::{
    collections::BTreeSet,
    io,
    sync::mpsc,
    thread,
    time::{Duration, SystemTime},
};

use anyhow::Result;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
};

use crate::{
    app::{
        AmbientSnapshot, SessionFilter, demo_feed, demo_snapshot, format_bytes,
        snapshot_with_sampler, visible_sessions,
    },
    feed::{Annotation, FeedEvent, FeedRecord, SessionFeed, load_session_feed},
    model::{AgentSession, SessionStatus, path_home_display, session_elapsed_label, time_label},
    pricing::{compact_tokens, short_model},
    process::ProcessSampler,
};

const INPUT_TICK: Duration = Duration::from_millis(75);
const REFRESH_TICK: Duration = Duration::from_millis(1000);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DashboardSource {
    Native,
    Demo,
}

enum ViewMode {
    Monitor,
    Tail { scroll: usize, follow: bool },
    Stream {
        selected: usize,
        scroll: usize,
        follow: bool,
        expanded: BTreeSet<usize>,
        project_filter: Option<String>,
        errors_only: bool,
    },
}

enum KeyAction {
    Continue,
    Refresh,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TopPanel {
    Spectrum,
    Swimlane,
}

impl TopPanel {
    fn next(self) -> Self {
        match self {
            TopPanel::Spectrum => TopPanel::Swimlane,
            TopPanel::Swimlane => TopPanel::Spectrum,
        }
    }
}

pub fn run(source: DashboardSource) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, cursor::Hide)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let result = run_loop(&mut terminal, source);
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, cursor::Show)?;
    result
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    source: DashboardSource,
) -> Result<()> {
    let mut selected = 0usize;
    let mut mode = ViewMode::Monitor;
    let mut filter = SessionFilter::Overview;
    let mut top_panel = TopPanel::Spectrum;
    let mut spectrum = crate::spectrum::Spectrum::new(crate::spectrum::BARS);
    let (snapshot_tx, snapshot_rx) = mpsc::channel();
    spawn_snapshot_worker(source, snapshot_tx);
    let mut sampler = ProcessSampler::new();
    let mut snapshot = snapshot_for_source(source, 0, &mut sampler)?;
    let mut history = crate::metrics::MetricsHistory::new(240);
    history.push(&snapshot);
    let mut activity_index = crate::activity::ActivityIndex::build(&snapshot, 40, 500);
    let mut needs_draw = true;
    let last_feed_offset = std::cell::Cell::new(0usize);
    let last_stream_scroll = std::cell::Cell::new(0usize);

    loop {
        while let Ok(next_snapshot) = snapshot_rx.try_recv() {
            snapshot = next_snapshot;
            history.push(&snapshot);
            activity_index = crate::activity::ActivityIndex::build(&snapshot, 40, 500);
            needs_draw = true;
        }

        // Animate the spectrum every frame while on the monitor so it dances
        // between snapshot refreshes; this also keeps the monitor redrawing.
        if matches!(mode, ViewMode::Monitor) {
            spectrum.tick(spectrum_energy(&history), INPUT_TICK.as_secs_f32());
            needs_draw = true;
        }

        let sessions = visible_sessions(&snapshot.sessions, filter);
        selected = clamp_selected(selected, sessions.len());
        if needs_draw {
            terminal.draw(|frame| {
                draw(
                    frame,
                    DrawContext {
                        source,
                        snapshot: &snapshot,
                        sessions: &sessions,
                        selected,
                        mode: &mode,
                        filter,
                        history: &history,
                        last_feed_offset: &last_feed_offset,
                        last_stream_scroll: &last_stream_scroll,
                        activity_index: &activity_index,
                        top_panel,
                        spectrum: &spectrum,
                    },
                )
            })?;
            needs_draw = false;
        }

        if event::poll(INPUT_TICK)?
            && let Event::Key(key) = event::read()?
        {
            // 'v' toggles the top panel (throughput graph ↔ swimlane) in monitor mode only.
            // Handled here rather than inside handle_key to avoid adding yet another parameter
            // to an already-large signature; run_loop owns top_panel so this is the natural site.
            if matches!(mode, ViewMode::Monitor) && key.code == KeyCode::Char('v') {
                top_panel = top_panel.next();
                needs_draw = true;
                continue;
            }
            let stream_projects = distinct_projects(activity_index.events());
            let stream_filtered_len =
                if let ViewMode::Stream { ref project_filter, errors_only, .. } = mode {
                    crate::activity::filter_events(
                        activity_index.events(),
                        project_filter.as_deref(),
                        errors_only,
                    )
                    .len()
                } else {
                    0
                };
            match handle_key(
                &mut mode,
                &mut selected,
                &mut filter,
                sessions.len(),
                last_feed_offset.get(),
                key.code,
                stream_filtered_len,
                &stream_projects,
                last_stream_scroll.get(),
            ) {
                KeyAction::Quit => return Ok(()),
                KeyAction::Refresh => {
                    snapshot = snapshot_for_source(
                        source,
                        history.throughput_series().len() as u64 + 1,
                        &mut sampler,
                    )?;
                    history.push(&snapshot);
                    activity_index = crate::activity::ActivityIndex::build(&snapshot, 40, 500);
                    needs_draw = true;
                }
                KeyAction::Continue => needs_draw = true,
            }
        }
    }
}

fn spawn_snapshot_worker(source: DashboardSource, tx: mpsc::Sender<AmbientSnapshot>) {
    thread::spawn(move || {
        let mut tick = 1_u64;
        let mut sampler = ProcessSampler::new();
        loop {
            thread::sleep(REFRESH_TICK);
            if let Ok(snapshot) = snapshot_for_source(source, tick, &mut sampler)
                && tx.send(snapshot).is_err()
            {
                break;
            }
            tick += 1;
        }
    });
}

fn snapshot_for_source(
    source: DashboardSource,
    tick: u64,
    sampler: &mut ProcessSampler,
) -> Result<AmbientSnapshot> {
    match source {
        DashboardSource::Native => snapshot_with_sampler(sampler),
        DashboardSource::Demo => Ok(demo_snapshot(tick)),
    }
}

/// Pure helper: advance the project filter to the next project in the list.
/// None → first project; last project → None (wraps back to "all"); else next.
fn next_project_filter(current: Option<String>, projects: &[String]) -> Option<String> {
    if projects.is_empty() {
        return None;
    }
    match current {
        None => Some(projects[0].clone()),
        Some(ref cur) => {
            if let Some(pos) = projects.iter().position(|p| p == cur) {
                projects.get(pos + 1).cloned()
            } else {
                // Current filter not found in list → wrap to first.
                Some(projects[0].clone())
            }
        }
    }
}

/// Return distinct project names in order of first appearance in `events`.
fn distinct_projects(events: &[crate::activity::StreamEvent]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();
    for e in events {
        if seen.insert(e.project.clone()) {
            result.push(e.project.clone());
        }
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn handle_key(
    mode: &mut ViewMode,
    selected: &mut usize,
    filter: &mut SessionFilter,
    session_count: usize,
    last_feed_offset: usize,
    key: KeyCode,
    stream_filtered_len: usize,
    stream_projects: &[String],
    last_stream_offset: usize,
) -> KeyAction {
    match key {
        KeyCode::Char('q') => KeyAction::Quit,
        KeyCode::Esc => match mode {
            ViewMode::Tail { .. } | ViewMode::Stream { .. } => {
                *mode = ViewMode::Monitor;
                KeyAction::Continue
            }
            ViewMode::Monitor => KeyAction::Quit,
        },
        KeyCode::Char('a') => {
            *filter = filter.toggle();
            *selected = 0;
            *mode = ViewMode::Monitor;
            KeyAction::Continue
        }
        // 's' from Monitor opens Stream view.
        KeyCode::Char('s') if matches!(mode, ViewMode::Monitor) => {
            *mode = ViewMode::Stream {
                selected: 0,
                scroll: 0,
                follow: true,
                expanded: BTreeSet::new(),
                project_filter: None,
                errors_only: false,
            };
            KeyAction::Continue
        }
        KeyCode::Enter if matches!(mode, ViewMode::Monitor) => {
            *mode = ViewMode::Tail { scroll: 0, follow: true };
            KeyAction::Continue
        }
        // Enter/Right in Stream: toggle expanded for selected event.
        KeyCode::Enter | KeyCode::Right if matches!(mode, ViewMode::Stream { .. }) => {
            if let ViewMode::Stream { selected, expanded, .. } = mode {
                let idx = *selected;
                if expanded.contains(&idx) {
                    expanded.remove(&idx);
                } else {
                    expanded.insert(idx);
                }
            }
            KeyAction::Continue
        }
        // Left in Stream: collapse selected event (remove from expanded).
        KeyCode::Left if matches!(mode, ViewMode::Stream { .. }) => {
            if let ViewMode::Stream { selected, expanded, .. } = mode {
                expanded.remove(selected);
            }
            KeyAction::Continue
        }
        // 'p' in Stream: cycle project filter.
        KeyCode::Char('p') if matches!(mode, ViewMode::Stream { .. }) => {
            if let ViewMode::Stream { project_filter, .. } = mode {
                let current = project_filter.take();
                *project_filter = next_project_filter(current, stream_projects);
            }
            KeyAction::Continue
        }
        // 'e' in Stream: toggle errors_only.
        KeyCode::Char('e') if matches!(mode, ViewMode::Stream { .. }) => {
            if let ViewMode::Stream { errors_only, .. } = mode {
                *errors_only = !*errors_only;
            }
            KeyAction::Continue
        }
        KeyCode::Char('j') => {
            if let ViewMode::Tail { scroll, follow } = mode {
                if !*follow {
                    *scroll = scroll.saturating_add(1);
                }
            } else if let ViewMode::Stream { selected: sel, .. } = mode {
                if stream_filtered_len > 0 {
                    *sel = sel.saturating_add(1).min(stream_filtered_len - 1);
                }
            } else {
                *selected = (*selected + 1).min(session_count.saturating_sub(1));
            }
            KeyAction::Continue
        }
        KeyCode::Char('k') => {
            if let ViewMode::Tail { scroll, follow } = mode {
                if *follow {
                    *scroll = last_feed_offset;
                }
                *follow = false;
                *scroll = scroll.saturating_sub(1);
            } else if let ViewMode::Stream { selected: sel, scroll, follow, .. } = mode {
                if *follow {
                    *scroll = last_stream_offset;
                }
                *follow = false;
                *sel = sel.saturating_sub(1);
            } else {
                *selected = selected.saturating_sub(1);
            }
            KeyAction::Continue
        }
        KeyCode::Down => {
            *selected = (*selected + 1).min(session_count.saturating_sub(1));
            if let ViewMode::Tail { scroll, follow } = mode {
                *scroll = 0;
                *follow = true;
            }
            KeyAction::Continue
        }
        KeyCode::Up => {
            *selected = selected.saturating_sub(1);
            if let ViewMode::Tail { scroll, follow } = mode {
                *scroll = 0;
                *follow = true;
            }
            KeyAction::Continue
        }
        KeyCode::PageDown => {
            if let ViewMode::Tail { scroll, follow } = mode {
                *follow = false;
                *scroll = scroll.saturating_add(5);
            } else if let ViewMode::Stream { scroll, follow, .. } = mode {
                *follow = false;
                *scroll = scroll.saturating_add(5);
            }
            KeyAction::Continue
        }
        KeyCode::PageUp => {
            if let ViewMode::Tail { scroll, follow } = mode {
                if *follow {
                    *scroll = last_feed_offset;
                }
                *follow = false;
                *scroll = scroll.saturating_sub(5);
            } else if let ViewMode::Stream { scroll, follow, .. } = mode {
                if *follow {
                    *scroll = last_stream_offset;
                }
                *follow = false;
                *scroll = scroll.saturating_sub(5);
            }
            KeyAction::Continue
        }
        KeyCode::Char('g') => {
            if let ViewMode::Tail { scroll, follow } = mode {
                *follow = false;
                *scroll = 0;
            } else if let ViewMode::Stream { follow, selected: sel, scroll, .. } = mode {
                *follow = false;
                *scroll = 0;
                *sel = 0;
            }
            KeyAction::Continue
        }
        KeyCode::Char('G') => {
            if let ViewMode::Tail { scroll, follow } = mode {
                *follow = true;
                *scroll = 0;
            } else if let ViewMode::Stream { follow, .. } = mode {
                *follow = true;
            }
            KeyAction::Continue
        }
        KeyCode::Char('r') => KeyAction::Refresh,
        _ => KeyAction::Continue,
    }
}

struct DrawContext<'a> {
    source: DashboardSource,
    snapshot: &'a AmbientSnapshot,
    sessions: &'a [AgentSession],
    selected: usize,
    mode: &'a ViewMode,
    filter: SessionFilter,
    history: &'a crate::metrics::MetricsHistory,
    last_feed_offset: &'a std::cell::Cell<usize>,
    last_stream_scroll: &'a std::cell::Cell<usize>,
    activity_index: &'a crate::activity::ActivityIndex,
    top_panel: TopPanel,
    spectrum: &'a crate::spectrum::Spectrum,
}

fn draw(frame: &mut Frame<'_>, context: DrawContext<'_>) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    match context.mode {
        ViewMode::Monitor => draw_monitor(
            frame,
            context.snapshot,
            context.sessions,
            context.selected,
            context.filter,
            context.history,
            context.activity_index,
            context.top_panel,
            context.spectrum,
        ),
        ViewMode::Tail { scroll, follow } => draw_tail(
            frame,
            context.source,
            context.sessions,
            context.selected,
            *scroll,
            *follow,
            context.filter,
            context.history,
            context.last_feed_offset,
        ),
        ViewMode::Stream {
            selected,
            scroll,
            follow,
            expanded,
            project_filter,
            errors_only,
        } => draw_stream(
            frame,
            context.activity_index,
            *selected,
            *scroll,
            *follow,
            expanded,
            project_filter,
            *errors_only,
            context.last_stream_scroll,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_monitor(
    frame: &mut Frame<'_>,
    snapshot: &AmbientSnapshot,
    sessions: &[AgentSession],
    selected: usize,
    filter: SessionFilter,
    history: &crate::metrics::MetricsHistory,
    activity_index: &crate::activity::ActivityIndex,
    top_panel: TopPanel,
    spectrum: &crate::spectrum::Spectrum,
) {
    let compact = frame.area().height < 32;
    let panel_height = if compact { 6 } else { 11 };
    let activity_height = if compact { 5 } else { 7 };
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),               // [0] header
            Constraint::Length(panel_height),    // [1] agent-activity panel
            Constraint::Min(9),                  // [2] main split
            Constraint::Length(activity_height), // [3] activity stream preview
            Constraint::Length(2),               // [4] footer
        ])
        .split(frame.area());

    frame.render_widget(Clear, vertical[0]);
    frame.render_widget(header(snapshot, filter), vertical[0]);
    match top_panel {
        TopPanel::Spectrum => render_spectrum(frame, spectrum, vertical[1]),
        TopPanel::Swimlane => render_swimlane(frame, history, vertical[1]),
    }

    let main = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(61), Constraint::Percentage(39)])
        .split(vertical[2]);

    frame.render_widget(Clear, main[0]);
    frame.render_widget(session_table(snapshot, sessions, selected, filter), main[0]);
    frame.render_widget(Clear, main[1]);
    frame.render_widget(session_detail(sessions.get(selected)), main[1]);
    render_activity_preview(frame, activity_index, vertical[3]);
    frame.render_widget(Clear, vertical[4]);
    frame.render_widget(monitor_footer(), vertical[4]);

    if sessions.is_empty() {
        draw_empty_state(frame, main[0]);
    }
}

fn render_spectrum(frame: &mut Frame<'_>, spectrum: &crate::spectrum::Spectrum, area: Rect) {
    let block = Block::default()
        .title(Line::from(Span::styled(" agent activity ", accent())))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Green));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    const BLOCKS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let w = inner.width as usize;
    let h = inner.height as usize;
    let heights = spectrum.heights();
    let peaks = spectrum.peaks();

    let mut lines: Vec<Line> = Vec::with_capacity(h);
    for row in 0..h {
        // Rows render top (row 0) to bottom; count cells up from the bottom.
        let cell_from_bottom = (h - 1 - row) as i32;
        let mut spans: Vec<Span> = Vec::with_capacity(w);
        for col in 0..w {
            let val = heights.get(col).copied().unwrap_or(0.0).clamp(0.0, 1.0);
            let peak = peaks.get(col).copied().unwrap_or(0.0).clamp(0.0, 1.0);
            let total_eighths = (val * h as f32 * 8.0).round() as i32;
            let fill = (total_eighths - cell_from_bottom * 8).clamp(0, 8);
            let peak_cell = ((peak * h as f32).ceil() as i32 - 1).max(0);

            if fill == 0 {
                if peak > 0.03 && cell_from_bottom == peak_cell {
                    spans.push(Span::styled("▔", Style::default().fg(Color::Rgb(180, 220, 255))));
                } else {
                    spans.push(Span::raw(" "));
                }
                continue;
            }
            let frac = (cell_from_bottom as f32 + 1.0) / h as f32;
            spans.push(Span::styled(
                BLOCKS[fill as usize].to_string(),
                Style::default().fg(spectrum_level_color(frac)),
            ));
        }
        lines.push(Line::from(spans));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn spectrum_level_color(frac: f32) -> Color {
    // Classic EQ gradient: green low, yellow mid, red peaks.
    if frac < 0.5 {
        Color::Rgb(40, 200, 90)
    } else if frac < 0.78 {
        Color::Rgb(225, 205, 60)
    } else {
        Color::Rgb(235, 75, 60)
    }
}

/// Overall agent activity in 0..=1, driving the spectrum amplitude. Combines
/// token throughput, live-agent CPU, and live count, with a small idle baseline
/// so the spectrum always shimmers.
fn spectrum_energy(history: &crate::metrics::MetricsHistory) -> f32 {
    let global = history.latest_global();
    let tps = global.map(|g| g.tokens_per_sec).unwrap_or(0.0) as f32;
    let live = global.map(|g| g.live).unwrap_or(0) as f32;
    let cpu = history.latest_cpu_total() as f32;

    let tps_term = (tps / 350.0).min(1.0);
    let cpu_term = (cpu / 110.0).min(1.0);
    let live_term = (live / 3.0).min(1.0);
    const BASELINE: f32 = 0.55;
    (BASELINE + 0.45 * tps_term + 0.40 * cpu_term + 0.20 * live_term).min(1.0)
}

fn header(snapshot: &AmbientSnapshot, filter: SessionFilter) -> Paragraph<'static> {
    let status = if snapshot.active_count() == 0 {
        "idle".to_string()
    } else {
        format!(
            "{} active · {} tracked",
            snapshot.active_count(),
            snapshot.sessions.len()
        )
    };
    Paragraph::new(Line::from(vec![
        Span::styled(
            " aitop ",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(status, Style::default().fg(Color::Gray)),
        Span::raw("   "),
        Span::styled(filter.label().to_string(), dim()),
        Span::raw("   "),
        Span::styled(time_label(Some(snapshot.generated_at)), dim()),
    ]))
    .block(Block::default().borders(Borders::BOTTOM))
}

fn session_table(
    snapshot: &AmbientSnapshot,
    sessions: &[AgentSession],
    selected: usize,
    filter: SessionFilter,
) -> List<'static> {
    let mut rows = vec![ListItem::new(Line::from(vec![
        Span::styled("  AGENT     ", dim()),
        Span::styled("REPO        ", dim()),
        Span::styled("ELAPSED   ", dim()),
        Span::styled("PID      ", dim()),
        Span::styled("CPU          ", dim()),
        Span::styled("MEM        ", dim()),
        Span::styled("TOK", dim()),
    ]))];

    for (index, session) in sessions.iter().enumerate() {
        let style = if index == selected {
            Style::default().bg(Color::Rgb(16, 48, 32)).fg(Color::White)
        } else {
            Style::default().fg(Color::Gray)
        };
        rows.push(ListItem::new(session_row(session, snapshot.generated_at)).style(style));
    }

    List::new(rows).block(
        Block::default()
            .title(Line::from(vec![
                Span::styled(" sessions ", title_style()),
                Span::styled(
                    format!(
                        "{} active / {} total ",
                        snapshot.active_count(),
                        snapshot.sessions.len()
                    ),
                    dim(),
                ),
                Span::styled(format!("{} view ", filter.label()), dim()),
            ]))
            .borders(Borders::ALL),
    )
}

fn session_row(session: &AgentSession, now: SystemTime) -> Line<'static> {
    let marker = match session.status {
        SessionStatus::Running => "*",
        SessionStatus::Recent | SessionStatus::Done => "+",
        SessionStatus::Unknown => "?",
    };
    let pid = session
        .pid
        .map(|pid| pid.to_string())
        .unwrap_or_else(|| "-".to_string());
    let cpu = session
        .process
        .as_ref()
        .map(|process| {
            format!(
                "{:>3}% {}",
                process.cpu_percent,
                meter(process.cpu_percent, 10)
            )
        })
        .unwrap_or_else(|| "  - ----------".to_string());
    let mem = session
        .process
        .as_ref()
        .map(|process| format_bytes(process.memory_bytes))
        .unwrap_or_else(|| "-".to_string());
    let tokens = session
        .tokens_total
        .and_then(|tokens| u64::try_from(tokens).ok())
        .map(compact_tokens)
        .unwrap_or_else(|| "-".to_string());

    Line::from(vec![
        Span::styled(format!("  {marker} "), status_style(session.status)),
        Span::styled(format!("{:<9}", session.agent.to_string()), strong()),
        Span::styled(
            format!("{:<12}", truncate(&session.repo_name(), 11)),
            accent(),
        ),
        Span::raw(format!(
            "{:<10}",
            session_elapsed_label(session.status, session.started_at, session.updated_at, now)
        )),
        Span::raw(format!("{:<9}", pid)),
        Span::styled(format!("{:<13}", cpu), Style::default().fg(Color::Green)),
        Span::raw(format!("{:<11}", mem)),
        Span::raw(tokens),
    ])
}

fn session_detail(session: Option<&AgentSession>) -> Paragraph<'static> {
    let lines = match session {
        Some(session) => {
            let mut lines = vec![
                Line::from(vec![
                    Span::styled(
                        format!("{} - {}", session.agent, session.repo_name()),
                        strong(),
                    ),
                    Span::raw("  "),
                    Span::styled(
                        session.status.to_string().to_uppercase(),
                        status_style(session.status),
                    ),
                ]),
                blank(),
                kv("COMMAND", session.command.as_deref().unwrap_or("-")),
                kv(
                    "PID",
                    &session
                        .pid
                        .map(|pid| pid.to_string())
                        .unwrap_or_else(|| "-".into()),
                ),
                kv(
                    "GIT ROOT",
                    &session
                        .git
                        .as_ref()
                        .map(|git| {
                            let branch = git.branch.as_deref().unwrap_or("-");
                            format!("{} - {}", path_home_display(&git.root), branch)
                        })
                        .unwrap_or_else(|| "unknown".into()),
                ),
                kv("CWD", &path_home_display(&session.cwd)),
                kv("STARTED", &time_label(session.started_at)),
                kv("UPDATED", &time_label(session.updated_at)),
                kv(
                    "MODEL",
                    &session
                        .model
                        .as_deref()
                        .map(short_model)
                        .unwrap_or_else(|| "unknown".into()),
                ),
                kv(
                    "TOKENS",
                    &session
                        .tokens_total
                        .and_then(|tokens| u64::try_from(tokens).ok())
                        .map(compact_tokens)
                        .unwrap_or_else(|| "unknown".into()),
                ),
            ];
            if let Some(process) = &session.process {
                lines.push(kv(
                    "CPU / MEM",
                    &format!(
                        "{}% - {} - {} child pids",
                        process.cpu_percent,
                        format_bytes(process.memory_bytes),
                        process.child_pids.len()
                    ),
                ));
            }
            lines.push(blank());
            lines.push(Line::from(vec![
                Span::styled("GIT STATUS", title_style()),
                Span::raw(format!("  {} dirty files", session.dirty_count())),
            ]));
            if let Some(git) = &session.git {
                for dirty in git.dirty_files.iter().take(6) {
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("{:<3}", dirty.code),
                            Style::default().fg(Color::Yellow),
                        ),
                        Span::raw(dirty.path.clone()),
                    ]));
                }
            }
            lines
        }
        None => vec![
            blank(),
            centered("no session selected"),
            centered("run claude or codex normally"),
        ],
    };
    Paragraph::new(lines).block(
        Block::default()
            .title(Line::from(vec![
                Span::styled(" session detail ", title_style()),
                Span::styled("enter opens tail ", dim()),
            ]))
            .borders(Borders::ALL),
    )
}

pub(crate) fn render_activity_preview(
    frame: &mut Frame<'_>,
    index: &crate::activity::ActivityIndex,
    area: Rect,
) {
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(Line::from(vec![
            Span::styled(" activity ", title_style()),
            Span::styled("s: stream ", dim()),
        ]))
        .borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 {
        return;
    }

    let k = inner.height as usize;
    let events = index.events();
    let start = events.len().saturating_sub(k);
    let visible = &events[start..];

    let max_summary = inner.width.saturating_sub(20) as usize;

    let lines: Vec<Line<'static>> = if visible.is_empty() {
        vec![Line::from(Span::styled(
            " -   watching for cross-project activity",
            dim(),
        ))]
    } else {
        visible
            .iter()
            .map(|event| {
                let time_str = event
                    .timestamp
                    .map(|t| {
                        let dt: chrono::DateTime<chrono::Local> = t.into();
                        dt.format("%H:%M").to_string()
                    })
                    .unwrap_or_else(|| "--:--".to_string());

                let glyph = kind_glyph(&event.kind);

                let project_col = truncate(&event.project, 10);
                let summary_col = truncate(&event.summary, max_summary);

                if event.is_error {
                    Line::from(vec![
                        Span::styled(format!("{time_str} "), Style::default().fg(Color::DarkGray)),
                        Span::styled(
                            format!("{project_col:<10} "),
                            Style::default().fg(Color::Red),
                        ),
                        Span::styled(
                            format!("{glyph} "),
                            Style::default().fg(Color::Red),
                        ),
                        Span::styled(summary_col, Style::default().fg(Color::Red)),
                    ])
                } else {
                    let proj_style = project_color(&event.project);
                    Line::from(vec![
                        Span::styled(format!("{time_str} "), Style::default().fg(Color::DarkGray)),
                        Span::styled(format!("{project_col:<10} "), proj_style),
                        Span::styled(format!("{glyph} "), dim()),
                        Span::styled(summary_col, Style::default().fg(Color::Gray)),
                    ])
                }
            })
            .collect()
    };

    frame.render_widget(ratatui::widgets::Paragraph::new(lines), inner);
}

pub fn render_swimlane(
    frame: &mut Frame<'_>,
    history: &crate::metrics::MetricsHistory,
    area: Rect,
) {
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(Line::from(vec![
            Span::styled(" agent timeline", title_style()),
            Span::styled("  ▎thinking", Style::default().fg(Color::Cyan)),
            Span::styled(" ▎output", Style::default().fg(Color::Green)),
            Span::styled(" ▎idle ", Style::default().fg(Color::DarkGray)),
        ]))
        .borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let inner_height = inner.height as usize;
    let inner_width = inner.width as usize;

    const LABEL_WIDTH: usize = 14;
    // label column + 1 space separator; bar gets the rest
    let bar_width = inner_width.saturating_sub(LABEL_WIDTH + 1);

    let lanes = history.lanes(inner_height);
    let total = lanes.len();

    // Reserve a row for "+N more" only when lanes overflow
    let render_count = if total > inner_height {
        inner_height.saturating_sub(1)
    } else {
        total.min(inner_height)
    };

    let mut lines: Vec<Line<'static>> = Vec::new();

    for lane in lanes.iter().take(render_count) {
        let label = truncate(&lane.label, LABEL_WIDTH);
        let label_padded = format!("{label:<LABEL_WIDTH$} ");

        let mut spans: Vec<Span<'static>> = vec![Span::styled(label_padded, dim())];

        if bar_width > 0 {
            let slices = &lane.slices;
            let start = slices.len().saturating_sub(bar_width);
            for activity in &slices[start..] {
                let (glyph, style) = match activity {
                    crate::metrics::AgentActivity::Output => {
                        ("█", Style::default().fg(Color::Green))
                    }
                    crate::metrics::AgentActivity::Thinking => {
                        ("█", Style::default().fg(Color::Cyan))
                    }
                    crate::metrics::AgentActivity::Idle => {
                        (" ", Style::default().fg(Color::DarkGray))
                    }
                };
                spans.push(Span::styled(glyph, style));
            }
        }

        lines.push(Line::from(spans));
    }

    if total > render_count {
        let more = total - render_count;
        lines.push(Line::from(Span::styled(format!(" +{more} more"), dim())));
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled("  no agent data yet", dim())));
    }

    frame.render_widget(Paragraph::new(lines), inner);
}

/// Glyph for each stream event kind.
fn kind_glyph(kind: &crate::activity::StreamKind) -> &'static str {
    use crate::activity::StreamKind;
    match kind {
        StreamKind::User => "›",
        StreamKind::Assistant => "✦",
        StreamKind::Thinking => "✻",
        StreamKind::Tool => "⚙",
        StreamKind::Result => "↳",
        StreamKind::FileEdit => "✎",
        StreamKind::Usage => "$",
    }
}

/// Foreground color for a stream event kind (non-error rows).
fn kind_color(kind: &crate::activity::StreamKind) -> Color {
    use crate::activity::StreamKind;
    match kind {
        StreamKind::User => Color::Cyan,
        StreamKind::Assistant => Color::Green,
        StreamKind::Thinking => Color::Magenta,
        StreamKind::Tool => Color::Yellow,
        StreamKind::Result => Color::Gray,
        StreamKind::FileEdit => Color::LightMagenta,
        StreamKind::Usage => Color::DarkGray,
    }
}

fn project_color(project: &str) -> Style {
    let palette = [
        Color::Cyan,
        Color::Green,
        Color::Yellow,
        Color::Magenta,
        Color::LightBlue,
        Color::LightCyan,
        Color::LightGreen,
    ];
    let hash: usize = project
        .bytes()
        .fold(0usize, |acc, b| acc.wrapping_add(b as usize));
    Style::default().fg(palette[hash % palette.len()])
}

fn monitor_footer() -> Paragraph<'static> {
    Paragraph::new(Line::from(vec![
        Span::styled(" up/down ", key_style()),
        Span::raw(" select   "),
        Span::styled("enter", key_style()),
        Span::raw(" tail   "),
        Span::styled("s", key_style()),
        Span::raw(" stream   "),
        Span::styled("v", key_style()),
        Span::raw(" panel   "),
        Span::styled("r", key_style()),
        Span::raw(" refresh   "),
        Span::styled("a", key_style()),
        Span::raw(" view   "),
        Span::styled("q", key_style()),
        Span::raw(" quit   "),
        Span::styled("future", dim()),
        Span::raw(" ask-repl"),
    ]))
    .block(Block::default().borders(Borders::TOP))
}

#[allow(clippy::too_many_arguments)]
fn draw_tail(
    frame: &mut Frame<'_>,
    source: DashboardSource,
    sessions: &[AgentSession],
    selected: usize,
    scroll: usize,
    follow: bool,
    filter: SessionFilter,
    history: &crate::metrics::MetricsHistory,
    last_feed_offset: &std::cell::Cell<usize>,
) {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(12), Constraint::Length(2)])
        .split(frame.area());
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(36), Constraint::Min(40)])
        .split(vertical[0]);

    frame.render_widget(Clear, body[0]);
    frame.render_widget(tail_sidebar(sessions, selected, filter), body[0]);
    let selected_session = sessions.get(selected);
    let feed = selected_session.and_then(|session| {
        load_feed_for_session(source, session, history.throughput_series().len() as u64)
    });
    let viewport = body[1].height.saturating_sub(2) as usize;
    frame.render_widget(Clear, body[1]);
    frame.render_widget(
        tail_feed(
            selected_session,
            feed.as_ref(),
            scroll,
            follow,
            viewport,
            body[1].width.saturating_sub(4) as usize,
            last_feed_offset,
        ),
        body[1],
    );
    frame.render_widget(Clear, vertical[1]);
    frame.render_widget(tail_footer(feed.as_ref()), vertical[1]);
}

/// Full-screen scrollable, expandable activity stream view.
///
/// Index convention: both `selected` and the indices in `expanded` refer to
/// positions within the **filtered** events list (the result of
/// `filter_events(...)`). This keeps selection and expansion consistent — the
/// same integer means the same on-screen row regardless of which field you
/// query.
#[allow(clippy::too_many_arguments)]
fn draw_stream(
    frame: &mut Frame<'_>,
    index: &crate::activity::ActivityIndex,
    selected: usize,
    scroll: usize,
    follow: bool,
    expanded: &BTreeSet<usize>,
    project_filter: &Option<String>,
    errors_only: bool,
    last_stream_scroll: &std::cell::Cell<usize>,
) {
    let area = frame.area();

    // Guard against unusably tiny terminals.
    if area.height < 3 || area.width < 4 {
        return;
    }

    // Layout: scrollable body + footer bar.
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(2)])
        .split(area);
    let body = vertical[0];
    let footer_area = vertical[1];

    let width = body.width as usize;
    // Column widths: "HH:MM " (6) + project (10) + " " (1) + glyph (varies) + " " (1) = ~20
    let summary_width = width.saturating_sub(20);

    let events =
        crate::activity::filter_events(index.events(), project_filter.as_deref(), errors_only);
    let n_events = events.len();

    // Build all renderable lines (base row + optional detail rows).
    let mut lines: Vec<Line<'static>> = Vec::new();

    for (i, event) in events.iter().enumerate() {
        let time_str = event
            .timestamp
            .map(|t| {
                let dt: chrono::DateTime<chrono::Local> = t.into();
                dt.format("%H:%M").to_string()
            })
            .unwrap_or_else(|| "--:--".to_string());

        let glyph = kind_glyph(&event.kind);
        let project_col = truncate(&event.project, 10);
        let summary_col = truncate(&event.summary, summary_width);
        let is_selected = i == selected;

        let base = if is_selected {
            // Selected row: full reverse video.
            let sel_style = Style::default().add_modifier(Modifier::REVERSED);
            Line::from(vec![
                Span::styled(format!("{time_str} "), sel_style),
                Span::styled(format!("{project_col:<10} "), sel_style),
                Span::styled(format!("{glyph} "), sel_style),
                Span::styled(summary_col, sel_style),
            ])
        } else if event.is_error {
            Line::from(vec![
                Span::styled(format!("{time_str} "), Style::default().fg(Color::DarkGray)),
                Span::styled(format!("{project_col:<10} "), Style::default().fg(Color::Red)),
                Span::styled(format!("{glyph} "), Style::default().fg(Color::Red)),
                Span::styled(summary_col, Style::default().fg(Color::Red)),
            ])
        } else {
            let proj_style = project_color(&event.project);
            let fg = kind_color(&event.kind);
            Line::from(vec![
                Span::styled(format!("{time_str} "), Style::default().fg(Color::DarkGray)),
                Span::styled(format!("{project_col:<10} "), proj_style),
                Span::styled(format!("{glyph} "), Style::default().fg(fg)),
                Span::styled(summary_col, Style::default().fg(Color::Gray)),
            ])
        };
        lines.push(base);

        // Expanded detail rows.
        if expanded.contains(&i) && let Some(detail) = &event.detail {
            match detail {
                crate::activity::StreamDetail::Text(text) => {
                    let max_detail = width.saturating_sub(4);
                    for detail_line in text.lines().take(50) {
                        let txt = truncate(detail_line, max_detail);
                        lines.push(Line::from(vec![
                            Span::styled("  │ ", dim()),
                            Span::styled(txt, Style::default().fg(Color::Gray)),
                        ]));
                    }
                }
                crate::activity::StreamDetail::FileEdit { path, hunks } => {
                    let diff_lines =
                        crate::diffview::render_file_edit(path, hunks, width);
                    lines.extend(diff_lines);
                }
            }
        }
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            " - no events match the current filter",
            dim(),
        )));
    }

    // Compute scroll offset and store for key-handler seed.
    let viewport = body.height as usize;
    let offset = feed_scroll_offset(follow, scroll, lines.len(), viewport);
    last_stream_scroll.set(offset);

    // Render body: skip to offset, take viewport rows.
    let visible: Vec<Line<'static>> =
        lines.into_iter().skip(offset).take(viewport).collect();
    frame.render_widget(Paragraph::new(visible), body);

    // Footer.
    let filter_label = project_filter.as_deref().unwrap_or("all");
    let errors_label = if errors_only { "on" } else { "off" };
    let errors_style = if errors_only {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(format!(" {n_events} events"), Style::default().fg(Color::Cyan)),
            Span::raw(" · "),
            Span::styled(format!("filter: {filter_label}"), Style::default().fg(Color::Gray)),
            Span::raw(" · "),
            Span::styled(format!("errors:{errors_label}"), errors_style),
            Span::raw(" · "),
            Span::styled("s/esc", key_style()),
            Span::raw(" back  "),
            Span::styled("j/k", key_style()),
            Span::raw(" select  "),
            Span::styled("enter", key_style()),
            Span::raw(" expand  "),
            Span::styled("p", key_style()),
            Span::raw(" project  "),
            Span::styled("e", key_style()),
            Span::raw(" errors"),
        ]))
        .block(Block::default().borders(Borders::TOP)),
        footer_area,
    );
}

fn tail_sidebar(
    sessions: &[AgentSession],
    selected: usize,
    filter: SessionFilter,
) -> Paragraph<'static> {
    let mut lines = vec![Line::from(Span::styled(
        format!(
            "SESSIONS - {} {}",
            sessions.len(),
            filter.label().to_uppercase()
        ),
        dim(),
    ))];
    for (index, session) in sessions.iter().enumerate() {
        let selected_row = index == selected;
        let live = session.status == SessionStatus::Running;
        let prefix = if selected_row { "|" } else { " " };
        let dot = if live { "*" } else { "o" };
        let row_style = if selected_row {
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        lines.push(Line::from(vec![
            Span::styled(prefix, status_style(session.status)),
            Span::raw(" "),
            Span::styled(dot, status_style(session.status)),
            Span::raw(" "),
            Span::styled(truncate(&session.repo_name(), 20), row_style),
        ]));
        let model = session
            .model
            .as_deref()
            .map(short_model)
            .unwrap_or_else(|| "-".to_string());
        let tokens = session
            .tokens_total
            .and_then(|tokens| u64::try_from(tokens).ok())
            .map(compact_tokens)
            .unwrap_or_else(|| "-".to_string());
        lines.push(Line::from(Span::styled(
            format!("    {model} - {tokens}"),
            dim(),
        )));
        lines.push(blank());
    }
    Paragraph::new(lines).block(Block::default().borders(Borders::ALL))
}

fn load_feed_for_session(
    source: DashboardSource,
    session: &AgentSession,
    tick: u64,
) -> Option<SessionFeed> {
    match source {
        DashboardSource::Demo => Some(demo_feed(session, tick)),
        DashboardSource::Native => {
            let path = session.journal_path.as_ref()?;
            let id = session.native_id.as_deref().unwrap_or("-");
            load_session_feed(path, session.agent, id, 600).ok()
        }
    }
}

fn feed_scroll_offset(follow: bool, manual_scroll: usize, total_lines: usize, viewport: usize) -> usize {
    let max_start = total_lines.saturating_sub(viewport);
    if follow {
        max_start
    } else {
        manual_scroll.min(max_start)
    }
}

fn tail_feed(
    session: Option<&AgentSession>,
    feed: Option<&SessionFeed>,
    scroll: usize,
    follow: bool,
    viewport: usize,
    width: usize,
    last_offset: &std::cell::Cell<usize>,
) -> Paragraph<'static> {
    let title = session
        .map(|session| {
            let model = session
                .model
                .as_deref()
                .map(short_model)
                .unwrap_or_else(|| "unknown".to_string());
            Line::from(vec![
                Span::styled(format!(" {} ", session.repo_name()), accent()),
                Span::styled(format!("{model} - {}", session.agent), dim()),
            ])
        })
        .unwrap_or_else(|| Line::from(Span::styled(" no session ", dim())));

    let mut lines = Vec::new();
    match (session, feed) {
        (Some(session), Some(feed)) if feed.records.is_empty() => {
            lines.push(centered(""));
            lines.push(centered("waiting for first event..."));
            lines.push(centered(&format!(
                "{} just started - tailing session log",
                session.repo_name()
            )));
        }
        (Some(_), Some(feed)) => {
            for record in &feed.records {
                lines.extend(feed_record_lines(record, width));
            }
        }
        (Some(session), None) => {
            lines.extend(missing_journal_lines(session, width));
        }
        (None, _) => {
            lines.push(centered("no session selected"));
        }
    }

    let offset = feed_scroll_offset(follow, scroll, lines.len(), viewport);
    last_offset.set(offset);
    Paragraph::new(lines)
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Green)),
        )
        .scroll((offset as u16, 0))
}

fn missing_journal_lines(session: &AgentSession, width: usize) -> Vec<Line<'static>> {
    let pid = session
        .pid
        .map(|pid| pid.to_string())
        .unwrap_or_else(|| "-".to_string());
    let command = session.command.as_deref().unwrap_or("-");
    let cwd = path_home_display(&session.cwd);
    let full_cwd = session.cwd.display().to_string();
    let source = if session.pid.is_some() {
        format!(
            "discovered as a live {} process from native state",
            session.agent
        )
    } else {
        format!("discovered from recent {} native state", session.agent)
    };
    let inspect = if let Some(pid) = session.pid {
        format!("inspect: ps -p {pid} -o pid,ppid,etime,command")
    } else {
        "inspect: open the cwd below and check the agent terminal".to_string()
    };
    let stop = if let Some(pid) = session.pid {
        format!("stop: exit the owning terminal/session; last resort kill {pid}")
    } else {
        "stop: no live pid is known; close the owning agent session if still open".to_string()
    };

    vec![
        Line::from(Span::styled("no native journal linked", title_style())),
        Line::from(truncate(
            "aitop can see the process/session, but not a transcript file for it yet.",
            width,
        )),
        blank(),
        Line::from(Span::styled("what this is", title_style())),
        Line::from(truncate(
            &format!(
                "live {} process named {}",
                session.agent,
                session.repo_name()
            ),
            width,
        )),
        Line::from(truncate(
            "repo name comes from the working directory, not from aitop knowing the task.",
            width,
        )),
        blank(),
        kv("AGENT", &session.agent.to_string()),
        kv("PID", &pid),
        kv("COMMAND", command),
        kv("CWD", &cwd),
        kv("CWD FULL", &full_cwd),
        kv("SOURCE", &source),
        kv("THREAD", session.native_id.as_deref().unwrap_or("-")),
        blank(),
        Line::from(Span::styled("what you can do", title_style())),
        Line::from(truncate(&inspect, width)),
        Line::from(truncate(&stop, width)),
        Line::from(truncate(
            "aitop does not stop processes yet; it only shows the command to use.",
            width,
        )),
    ]
}

fn feed_record_lines(record: &FeedRecord, width: usize) -> Vec<Line<'static>> {
    let badge = annotation_badges(&record.annotations);
    match &record.event {
        FeedEvent::User { text } => {
            let mut spans = vec![
                Span::styled("› you ", Style::default().fg(Color::Cyan)),
                Span::raw(truncate(text, width.saturating_sub(6))),
            ];
            spans.extend(badge);
            vec![Line::from(spans)]
        }
        FeedEvent::Assistant { text, .. } => {
            let mut lines = vec![Line::from(vec![Span::styled("✦ assistant", accent())])];
            lines.push(Line::from(format!(
                "  {}",
                truncate(text, width.saturating_sub(2))
            )));
            lines
        }
        FeedEvent::Thinking { text } => vec![Line::from(vec![
            Span::styled("✻ thinking ", dim()),
            Span::styled(
                truncate(text, width.saturating_sub(11)),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ),
        ])],
        FeedEvent::ToolCall { name, summary, .. } => {
            let mut spans = vec![
                Span::styled("⚙ ", Style::default().fg(Color::Yellow)),
                Span::styled(
                    name.clone(),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::styled(
                    truncate(summary, width.saturating_sub(name.len() + 4)),
                    dim(),
                ),
            ];
            spans.extend(badge);
            vec![Line::from(spans)]
        }
        FeedEvent::ToolResult { ok, detail, .. } => {
            let style = if *ok {
                Style::default().fg(Color::Gray)
            } else {
                Style::default().fg(Color::Red)
            };
            let mut header = vec![Span::styled("↳", dim()), Span::raw(" ")];
            if !ok || record.annotations.contains(&Annotation::Error) {
                header.push(Span::styled(
                    "⚠ ERR ",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ));
            }
            header.push(Span::styled("result", style));
            let mut lines = vec![Line::from(header)];
            for line in detail.lines().take(10) {
                lines.push(Line::from(vec![
                    Span::styled("  │ ", dim()),
                    Span::styled(truncate(line, width.saturating_sub(2)), style),
                ]));
            }
            lines
        }
        FeedEvent::Usage { input, output, .. } => vec![Line::from(Span::styled(
            format!(
                "  usage in {} out {}",
                compact_tokens(*input),
                compact_tokens(*output)
            ),
            dim(),
        ))],
        FeedEvent::Unknown { kind } => vec![Line::from(Span::styled(format!("? {kind}"), dim()))],
        FeedEvent::FileEdit { path, hunks } => {
            crate::diffview::render_file_edit(path, hunks, width)
        }
    }
}

fn annotation_badges(annotations: &[Annotation]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for annotation in annotations {
        match annotation {
            Annotation::Error => spans.extend([
                Span::raw(" "),
                Span::styled(
                    "⚠ ERR",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
            ]),
            Annotation::TokenSpike { tokens } => spans.extend([
                Span::raw(" "),
                Span::styled(
                    format!("⚡ {}", compact_tokens(*tokens)),
                    Style::default().fg(Color::Yellow),
                ),
            ]),
            Annotation::FileTouched(path) => {
                spans.extend([Span::raw(" "), Span::styled(format!("file {path}"), dim())])
            }
            Annotation::CommandRun(command) => spans.extend([
                Span::raw(" "),
                Span::styled(format!("cmd {}", truncate(command, 28)), dim()),
            ]),
        }
    }
    spans
}

fn tail_footer(feed: Option<&SessionFeed>) -> Paragraph<'static> {
    let (input, output, ctx, cost) = feed
        .map(|feed| {
            (
                compact_tokens(feed.tokens_in),
                compact_tokens(feed.tokens_out),
                feed.context_pct
                    .map(|pct| format!("{pct}%"))
                    .unwrap_or_else(|| "-".to_string()),
                format!("~${:.2}", feed.estimated_cost),
            )
        })
        .unwrap_or_else(|| ("-".into(), "-".into(), "-".into(), "-".into()));
    Paragraph::new(Line::from(vec![
        Span::styled(" up/down ", key_style()),
        Span::raw(" select   "),
        Span::styled("pgup/pgdn", key_style()),
        Span::raw(" scroll   "),
        Span::styled("esc", key_style()),
        Span::raw(" monitor   "),
        Span::styled("a", key_style()),
        Span::raw(" view   "),
        Span::styled("q", key_style()),
        Span::raw(" quit   "),
        Span::styled(
            format!("tokens in {input} out {output} ctx {ctx} {cost}"),
            Style::default().fg(Color::Green),
        ),
    ]))
    .block(Block::default().borders(Borders::TOP))
}

fn draw_empty_state(frame: &mut Frame<'_>, area: Rect) {
    let center = centered_rect(54, 28, area);
    frame.render_widget(
        Paragraph::new(vec![
            centered("no sessions yet"),
            blank(),
            centered("start Claude or Codex normally"),
            centered("$ claude"),
            centered("$ codex"),
        ])
        .alignment(Alignment::Center),
        center,
    );
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

fn meter(value: u32, width: usize) -> String {
    let filled = ((value.min(100) as usize * width) / 100).min(width);
    format!("{}{}", "#".repeat(filled), ".".repeat(width - filled))
}

fn kv(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<10}"), dim()),
        Span::raw(value.to_string()),
    ])
}

fn blank() -> Line<'static> {
    Line::from("")
}

fn centered(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        Style::default().fg(Color::Gray),
    ))
}

fn truncate(value: &str, max: usize) -> String {
    let value = crate::feed::sanitize_inline(value);
    if value.chars().count() <= max {
        return value;
    }
    value
        .chars()
        .take(max.saturating_sub(1))
        .collect::<String>()
        + "."
}

fn clamp_selected(selected: usize, len: usize) -> usize {
    if len == 0 { 0 } else { selected.min(len - 1) }
}

fn title_style() -> Style {
    Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

fn strong() -> Style {
    Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD)
}

fn accent() -> Style {
    Style::default()
        .fg(Color::Green)
        .add_modifier(Modifier::BOLD)
}

fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn key_style() -> Style {
    Style::default().fg(Color::White).bg(Color::DarkGray)
}

fn status_style(status: SessionStatus) -> Style {
    match status {
        SessionStatus::Running => Style::default().fg(Color::Green),
        SessionStatus::Recent | SessionStatus::Done => Style::default().fg(Color::Gray),
        SessionStatus::Unknown => Style::default().fg(Color::Yellow),
    }
}

#[cfg(test)]
pub(crate) fn tests_support_snapshot(at: u64) -> crate::app::AmbientSnapshot {
    use crate::model::{AgentKind, AgentSession, SessionStatus};
    let s = AgentSession {
        agent: AgentKind::Claude,
        native_id: Some("a".into()),
        title: None,
        command: None,
        cwd: std::path::PathBuf::from("/x"),
        pid: None,
        status: SessionStatus::Running,
        started_at: None,
        updated_at: None,
        model: None,
        tokens_total: Some((at * 100) as i64),
        git_branch: None,
        journal_path: None,
        process: None,
        git: None,
    };
    crate::app::AmbientSnapshot {
        sessions: vec![s],
        generated_at: std::time::SystemTime::UNIX_EPOCH
            + std::time::Duration::from_secs(at),
        activity: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AgentKind, AgentSession};

    /// Convenience: call handle_key with zeroed stream params (for Monitor/Tail tests).
    fn hk(
        mode: &mut ViewMode,
        selected: &mut usize,
        filter: &mut SessionFilter,
        session_count: usize,
        last_offset: usize,
        key: KeyCode,
    ) -> KeyAction {
        handle_key(mode, selected, filter, session_count, last_offset, key, 0, &[], 0)
    }

    #[test]
    fn top_panel_toggles() {
        assert_eq!(super::TopPanel::Spectrum.next(), super::TopPanel::Swimlane);
        assert_eq!(super::TopPanel::Swimlane.next(), super::TopPanel::Spectrum);
    }

    #[test]
    fn tail_up_down_changes_selected_session_and_resets_scroll() {
        let mut mode = ViewMode::Tail { scroll: 9, follow: false };
        let mut selected = 1usize;
        let mut filter = SessionFilter::Active;

        hk(&mut mode, &mut selected, &mut filter, 3, 0, KeyCode::Down);

        assert_eq!(selected, 2);
        assert_eq!(tail_scroll(&mode), Some(0));

        hk(&mut mode, &mut selected, &mut filter, 3, 0, KeyCode::Up);

        assert_eq!(selected, 1);
        assert_eq!(tail_scroll(&mode), Some(0));
    }

    #[test]
    fn tail_jk_scrolls_feed_without_changing_selected_session() {
        let mut mode = ViewMode::Tail { scroll: 4, follow: false };
        let mut selected = 1usize;
        let mut filter = SessionFilter::Active;

        hk(&mut mode, &mut selected, &mut filter, 3, 0, KeyCode::Char('j'));

        assert_eq!(selected, 1);
        assert_eq!(tail_scroll(&mode), Some(5));

        hk(&mut mode, &mut selected, &mut filter, 3, 0, KeyCode::Char('k'));

        assert_eq!(selected, 1);
        assert_eq!(tail_scroll(&mode), Some(4));
    }

    #[test]
    fn tail_page_keys_scroll_feed_without_changing_selected_session() {
        let mut mode = ViewMode::Tail { scroll: 4, follow: false };
        let mut selected = 1usize;
        let mut filter = SessionFilter::Active;

        hk(&mut mode, &mut selected, &mut filter, 3, 0, KeyCode::PageDown);

        assert_eq!(selected, 1);
        assert_eq!(tail_scroll(&mode), Some(9));

        hk(&mut mode, &mut selected, &mut filter, 3, 0, KeyCode::PageUp);

        assert_eq!(selected, 1);
        assert_eq!(tail_scroll(&mode), Some(4));
    }

    #[test]
    fn missing_journal_context_explains_process_location_and_stop_path() {
        let session = AgentSession {
            agent: AgentKind::Codex,
            native_id: Some("thread-1".to_string()),
            title: None,
            command: Some("codex".to_string()),
            cwd: "/Users/sg/code/example/src-tauri".into(),
            pid: Some(4242),
            status: SessionStatus::Running,
            started_at: None,
            updated_at: None,
            model: None,
            tokens_total: None,
            git_branch: None,
            journal_path: None,
            process: None,
            git: None,
        };

        let text = lines_to_plain_text(missing_journal_lines(&session, 120));

        assert!(text.contains("live codex process"));
        assert!(text.contains("repo name comes from the working directory"));
        assert!(text.contains("/Users/sg/code/example/src-tauri"));
        assert!(text.contains("ps -p 4242"));
        assert!(text.contains("kill 4242"));
    }

    #[test]
    fn activity_preview_renders_project_name() {
        use crate::activity::{ActivityIndex, StreamEvent, StreamKind};
        use crate::model::AgentKind;
        use ratatui::{backend::TestBackend, Terminal};

        let event = StreamEvent {
            timestamp: None,
            project: "my-project".to_string(),
            agent: AgentKind::Claude,
            session_key: "k".to_string(),
            kind: StreamKind::Assistant,
            summary: "hello world".to_string(),
            detail: None,
            is_error: false,
        };
        let index = ActivityIndex::from_events(vec![event]);

        let mut term = Terminal::new(TestBackend::new(80, 6)).unwrap();
        term.draw(|f| super::render_activity_preview(f, &index, f.area()))
            .unwrap();
        let content: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            content.contains("my-project"),
            "project name must appear in the activity preview buffer"
        );
    }

    #[test]
    fn feed_record_lines_use_aitail_style_badges_and_result_panels() {
        let error = FeedRecord {
            session_id: "auth-service".to_string(),
            timestamp: None,
            event: FeedEvent::ToolResult {
                id: "tool-1".to_string(),
                ok: false,
                summary: "failed".to_string(),
                detail: "FAIL test/session.spec.ts\nError: connect ECONNREFUSED 127.0.0.1:6379"
                    .to_string(),
            },
            annotations: vec![crate::feed::Annotation::Error],
        };
        let text = lines_to_plain_text(feed_record_lines(&error, 100));

        assert!(text.contains("↳"));
        assert!(text.contains("⚠ ERR"));
        assert!(text.contains("FAIL test/session.spec.ts"));
        assert!(text.contains("│"));
    }

    #[test]
    fn follow_anchors_last_page_to_bottom() {
        // 100 lines, 20-row viewport: first visible line is 80.
        assert_eq!(super::feed_scroll_offset(true, 0, 100, 20), 80);
    }

    #[test]
    fn follow_with_short_feed_starts_at_top() {
        assert_eq!(super::feed_scroll_offset(true, 0, 10, 20), 0);
    }

    #[test]
    fn manual_scroll_is_clamped_to_max_start() {
        // Can't scroll past total - viewport.
        assert_eq!(super::feed_scroll_offset(false, 999, 100, 20), 80);
        assert_eq!(super::feed_scroll_offset(false, 25, 100, 20), 25);
    }

    fn tail_scroll(mode: &ViewMode) -> Option<usize> {
        match mode {
            ViewMode::Tail { scroll, .. } => Some(*scroll),
            _ => None,
        }
    }

    fn tail_follow(mode: &ViewMode) -> Option<bool> {
        match mode {
            ViewMode::Tail { follow, .. } => Some(*follow),
            _ => None,
        }
    }

    #[test]
    fn follow_k_seeds_scroll_from_last_offset() {
        // following + last_offset=80, press k → Tail { scroll: 79, follow: false }
        let mut mode = ViewMode::Tail { scroll: 0, follow: true };
        let mut selected = 0usize;
        let mut filter = SessionFilter::Active;
        hk(&mut mode, &mut selected, &mut filter, 3, 80, KeyCode::Char('k'));
        assert_eq!(tail_scroll(&mode), Some(79));
        assert_eq!(tail_follow(&mode), Some(false));
    }

    #[test]
    fn follow_pageup_seeds_scroll_from_last_offset() {
        // following + last_offset=80, press PageUp → Tail { scroll: 75, follow: false }
        let mut mode = ViewMode::Tail { scroll: 0, follow: true };
        let mut selected = 0usize;
        let mut filter = SessionFilter::Active;
        hk(&mut mode, &mut selected, &mut filter, 3, 80, KeyCode::PageUp);
        assert_eq!(tail_scroll(&mode), Some(75));
        assert_eq!(tail_follow(&mode), Some(false));
    }

    #[test]
    fn spectrum_renders_without_panicking() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut s = crate::spectrum::Spectrum::new(crate::spectrum::BARS);
        for _ in 0..20 {
            s.tick(0.8, 0.05);
        }
        let mut term = Terminal::new(TestBackend::new(80, 9)).unwrap();
        term.draw(|f| super::render_spectrum(f, &s, f.area())).unwrap();
        let content: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(content.contains("agent activity"), "panel title present");
    }

    #[test]
    fn spectrum_renders_at_tiny_size_without_panic() {
        use ratatui::{backend::TestBackend, Terminal};
        let s = crate::spectrum::Spectrum::new(crate::spectrum::BARS);
        let mut term = Terminal::new(TestBackend::new(2, 2)).unwrap();
        term.draw(|f| super::render_spectrum(f, &s, f.area())).unwrap();
    }

    // ── Stream view tests ────────────────────────────────────────────────────

    #[test]
    fn project_cycle_wraps_through_all_then_none() {
        let projects = vec!["a".to_string(), "b".to_string()];
        assert_eq!(super::next_project_filter(None, &projects), Some("a".to_string()));
        assert_eq!(super::next_project_filter(Some("a".into()), &projects), Some("b".to_string()));
        assert_eq!(super::next_project_filter(Some("b".into()), &projects), None);
    }

    #[test]
    fn project_cycle_empty_list_returns_none() {
        assert_eq!(super::next_project_filter(None, &[]), None);
        assert_eq!(super::next_project_filter(Some("x".into()), &[]), None);
    }

    #[test]
    fn project_cycle_unknown_current_wraps_to_first() {
        let projects = vec!["a".to_string(), "b".to_string()];
        assert_eq!(
            super::next_project_filter(Some("z".into()), &projects),
            Some("a".to_string())
        );
    }

    #[test]
    fn stream_s_key_opens_stream_from_monitor() {
        let mut mode = ViewMode::Monitor;
        let mut selected = 0usize;
        let mut filter = SessionFilter::Active;
        hk(&mut mode, &mut selected, &mut filter, 0, 0, KeyCode::Char('s'));
        assert!(matches!(mode, ViewMode::Stream { follow: true, .. }));
    }

    #[test]
    fn stream_esc_returns_to_monitor() {
        let mut mode = ViewMode::Stream {
            selected: 0, scroll: 0, follow: true,
            expanded: BTreeSet::new(), project_filter: None, errors_only: false,
        };
        let mut selected = 0usize;
        let mut filter = SessionFilter::Active;
        hk(&mut mode, &mut selected, &mut filter, 0, 0, KeyCode::Esc);
        assert!(matches!(mode, ViewMode::Monitor));
    }

    #[test]
    fn stream_jk_moves_selected_event() {
        let mut mode = ViewMode::Stream {
            selected: 0, scroll: 0, follow: true,
            expanded: BTreeSet::new(), project_filter: None, errors_only: false,
        };
        let mut selected = 0usize;
        let mut filter = SessionFilter::Active;
        // j with 3 events moves selected to 1
        handle_key(&mut mode, &mut selected, &mut filter, 0, 0, KeyCode::Char('j'), 3, &[], 0);
        assert!(matches!(mode, ViewMode::Stream { selected: 1, .. }));
        // k moves back to 0 and sets follow=false
        handle_key(&mut mode, &mut selected, &mut filter, 0, 0, KeyCode::Char('k'), 3, &[], 0);
        assert!(matches!(mode, ViewMode::Stream { selected: 0, follow: false, .. }));
    }

    #[test]
    fn stream_j_clamps_to_last_event() {
        let mut mode = ViewMode::Stream {
            selected: 2, scroll: 0, follow: false,
            expanded: BTreeSet::new(), project_filter: None, errors_only: false,
        };
        let mut selected = 0usize;
        let mut filter = SessionFilter::Active;
        // 3 events, selected already at 2 — j should clamp
        handle_key(&mut mode, &mut selected, &mut filter, 0, 0, KeyCode::Char('j'), 3, &[], 0);
        assert!(matches!(mode, ViewMode::Stream { selected: 2, .. }));
    }

    #[test]
    fn stream_enter_toggles_expanded() {
        let mut mode = ViewMode::Stream {
            selected: 1, scroll: 0, follow: false,
            expanded: BTreeSet::new(), project_filter: None, errors_only: false,
        };
        let mut selected = 0usize;
        let mut filter = SessionFilter::Active;
        // Enter inserts index 1
        hk(&mut mode, &mut selected, &mut filter, 0, 0, KeyCode::Enter);
        if let ViewMode::Stream { ref expanded, .. } = mode {
            assert!(expanded.contains(&1));
        }
        // Enter again removes it
        hk(&mut mode, &mut selected, &mut filter, 0, 0, KeyCode::Enter);
        if let ViewMode::Stream { ref expanded, .. } = mode {
            assert!(!expanded.contains(&1));
        }
    }

    #[test]
    fn stream_left_collapses_selected() {
        let mut expanded = BTreeSet::new();
        expanded.insert(0usize);
        let mut mode = ViewMode::Stream {
            selected: 0, scroll: 0, follow: false,
            expanded, project_filter: None, errors_only: false,
        };
        let mut selected = 0usize;
        let mut filter = SessionFilter::Active;
        hk(&mut mode, &mut selected, &mut filter, 0, 0, KeyCode::Left);
        if let ViewMode::Stream { ref expanded, .. } = mode {
            assert!(!expanded.contains(&0));
        }
    }

    #[test]
    fn stream_e_toggles_errors_only() {
        let mut mode = ViewMode::Stream {
            selected: 0, scroll: 0, follow: false,
            expanded: BTreeSet::new(), project_filter: None, errors_only: false,
        };
        let mut selected = 0usize;
        let mut filter = SessionFilter::Active;
        hk(&mut mode, &mut selected, &mut filter, 0, 0, KeyCode::Char('e'));
        assert!(matches!(mode, ViewMode::Stream { errors_only: true, .. }));
        hk(&mut mode, &mut selected, &mut filter, 0, 0, KeyCode::Char('e'));
        assert!(matches!(mode, ViewMode::Stream { errors_only: false, .. }));
    }

    #[test]
    fn stream_p_cycles_project_filter() {
        let projects = vec!["alpha".to_string(), "beta".to_string()];
        let mut mode = ViewMode::Stream {
            selected: 0, scroll: 0, follow: false,
            expanded: BTreeSet::new(), project_filter: None, errors_only: false,
        };
        let mut selected = 0usize;
        let mut filter = SessionFilter::Active;
        handle_key(&mut mode, &mut selected, &mut filter, 0, 0, KeyCode::Char('p'), 0, &projects, 0);
        assert!(matches!(&mode, ViewMode::Stream { project_filter: Some(p), .. } if p == "alpha"));
        handle_key(&mut mode, &mut selected, &mut filter, 0, 0, KeyCode::Char('p'), 0, &projects, 0);
        assert!(matches!(&mode, ViewMode::Stream { project_filter: Some(p), .. } if p == "beta"));
        handle_key(&mut mode, &mut selected, &mut filter, 0, 0, KeyCode::Char('p'), 0, &projects, 0);
        assert!(matches!(mode, ViewMode::Stream { project_filter: None, .. }));
    }

    #[test]
    fn stream_capital_g_sets_follow() {
        let mut mode = ViewMode::Stream {
            selected: 0, scroll: 5, follow: false,
            expanded: BTreeSet::new(), project_filter: None, errors_only: false,
        };
        let mut selected = 0usize;
        let mut filter = SessionFilter::Active;
        hk(&mut mode, &mut selected, &mut filter, 0, 0, KeyCode::Char('G'));
        assert!(matches!(mode, ViewMode::Stream { follow: true, .. }));
    }

    #[test]
    fn stream_g_resets_to_top() {
        let mut mode = ViewMode::Stream {
            selected: 5, scroll: 10, follow: true,
            expanded: BTreeSet::new(), project_filter: None, errors_only: false,
        };
        let mut selected = 0usize;
        let mut filter = SessionFilter::Active;
        hk(&mut mode, &mut selected, &mut filter, 0, 0, KeyCode::Char('g'));
        assert!(matches!(mode, ViewMode::Stream { selected: 0, follow: false, scroll: 0, .. }));
    }

    #[test]
    fn stream_renders_without_panicking() {
        use crate::activity::{ActivityIndex, StreamEvent, StreamKind};
        use ratatui::{backend::TestBackend, Terminal};

        let events = vec![
            StreamEvent {
                timestamp: None,
                project: "proj-a".to_string(),
                agent: AgentKind::Claude,
                session_key: "k1".to_string(),
                kind: StreamKind::Assistant,
                summary: "hello".to_string(),
                detail: Some(crate::activity::StreamDetail::Text("detail text".to_string())),
                is_error: false,
            },
            StreamEvent {
                timestamp: None,
                project: "proj-b".to_string(),
                agent: AgentKind::Claude,
                session_key: "k2".to_string(),
                kind: StreamKind::Result,
                summary: "error occurred".to_string(),
                detail: None,
                is_error: true,
            },
        ];
        let index = ActivityIndex::from_events(events);
        let mut expanded = BTreeSet::new();
        expanded.insert(0usize); // expand first event
        let last_scroll = std::cell::Cell::new(0usize);

        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| {
            super::draw_stream(
                f,
                &index,
                0,
                0,
                true,
                &expanded,
                &None,
                false,
                &last_scroll,
            )
        })
        .unwrap();

        let content: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(content.contains("proj-a") || content.contains("proj-b"),
            "at least one project name must appear");
        assert!(content.contains("events"), "footer must contain 'events'");
    }

    #[test]
    fn stream_renders_at_tiny_size_without_panic() {
        use crate::activity::ActivityIndex;
        use ratatui::{backend::TestBackend, Terminal};

        let index = ActivityIndex::from_events(vec![]);
        let last_scroll = std::cell::Cell::new(0usize);

        // 2×2 — below the guard threshold, should return immediately without panic.
        let mut term = Terminal::new(TestBackend::new(2, 2)).unwrap();
        term.draw(|f| {
            super::draw_stream(f, &index, 0, 0, true, &BTreeSet::new(), &None, false, &last_scroll)
        })
        .unwrap();
    }

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

    fn lines_to_plain_text(lines: Vec<Line<'static>>) -> String {
        lines
            .into_iter()
            .flat_map(|line| line.spans.into_iter().map(|span| span.content.to_string()))
            .collect::<Vec<_>>()
            .join("\n")
    }
}
