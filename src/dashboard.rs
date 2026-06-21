use std::{
    collections::{BTreeMap, VecDeque},
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
    model::{AgentSession, SessionStatus, elapsed_label, path_home_display, time_label},
    pricing::{compact_tokens, short_model},
    process::ProcessSampler,
};

const INPUT_TICK: Duration = Duration::from_millis(75);
const REFRESH_TICK: Duration = Duration::from_millis(1000);
const SKYLINE_FULL_SCALE_SCORE: usize = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DashboardSource {
    Native,
    Demo,
}

enum ViewMode {
    Monitor,
    Tail { scroll: usize },
}

enum KeyAction {
    Continue,
    Refresh,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActivityTone {
    Quiet,
    Low,
    Medium,
    High,
    Hot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ActivitySample {
    score: u32,
    tone: ActivityTone,
}

impl ActivitySample {
    fn new(score: u32, tone: ActivityTone) -> Self {
        Self { score, tone }
    }

    fn quiet() -> Self {
        Self::new(0, ActivityTone::Quiet)
    }
}

struct ActivitySkyline {
    samples: VecDeque<ActivitySample>,
    capacity: usize,
}

impl ActivitySkyline {
    fn new(capacity: usize) -> Self {
        Self {
            samples: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn push_snapshot(&mut self, snapshot: &AmbientSnapshot, scorer: &mut ActivityScorer) {
        self.push_sample(scorer.score(snapshot));
    }

    fn push_sample(&mut self, sample: ActivitySample) {
        if self.samples.len() == self.capacity {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
    }

    #[cfg(test)]
    fn push_score(&mut self, score: u32) {
        self.push_sample(ActivitySample::new(score, tone_for_score(score)));
    }

    #[cfg(test)]
    fn scores(&self) -> Vec<u32> {
        self.samples.iter().map(|sample| sample.score).collect()
    }
}

#[derive(Debug, Clone, Default)]
struct ActivityScorer {
    previous: BTreeMap<String, SessionObservation>,
    previous_activity: Vec<String>,
}

#[derive(Debug, Clone)]
struct SessionObservation {
    status: SessionStatus,
    cpu_percent: u32,
    tokens_total: i64,
    dirty_count: usize,
    updated_at: Option<SystemTime>,
}

impl ActivityScorer {
    fn score(&mut self, snapshot: &AmbientSnapshot) -> ActivitySample {
        let current = snapshot
            .sessions
            .iter()
            .map(|session| (session_key(session), observe_session(session)))
            .collect::<BTreeMap<_, _>>();
        if self.previous.is_empty() {
            self.previous = current;
            self.previous_activity = snapshot.activity.clone();
            return ActivitySample::quiet();
        }

        let mut score = 0;
        let mut hot = false;

        for (key, observation) in &current {
            match self.previous.get(key) {
                Some(previous) => {
                    let cpu_delta = observation.cpu_percent.abs_diff(previous.cpu_percent);
                    if cpu_delta >= 60 {
                        hot = true;
                    }
                    score += (cpu_delta / 10).min(8);

                    let token_delta = observation
                        .tokens_total
                        .saturating_sub(previous.tokens_total)
                        .max(0) as u32;
                    score += (token_delta / 2_000).min(10);
                    if token_delta >= 10_000 {
                        hot = true;
                    }

                    if observation.updated_at != previous.updated_at {
                        score += 3;
                    }
                    if observation.dirty_count != previous.dirty_count {
                        score += 4 + observation.dirty_count.abs_diff(previous.dirty_count) as u32;
                    }
                    if observation.status != previous.status {
                        score += 6;
                    }
                }
                None => {
                    score += if observation.status == SessionStatus::Running {
                        4
                    } else {
                        1
                    };
                }
            }
        }

        for key in self.previous.keys() {
            if !current.contains_key(key) {
                score += 6;
            }
        }

        if snapshot.activity != self.previous_activity
            && snapshot
                .activity
                .iter()
                .any(|line| line.contains("changed"))
        {
            score += 2;
        }

        self.previous = current;
        self.previous_activity = snapshot.activity.clone();
        let score = score.min(100);
        let tone = if hot {
            ActivityTone::Hot
        } else {
            tone_for_score(score)
        };
        ActivitySample::new(score, tone)
    }
}

fn observe_session(session: &AgentSession) -> SessionObservation {
    SessionObservation {
        status: session.status,
        cpu_percent: session
            .process
            .as_ref()
            .map(|process| process.cpu_percent)
            .unwrap_or(0),
        tokens_total: session.tokens_total.unwrap_or(0),
        dirty_count: session.dirty_count(),
        updated_at: session.updated_at,
    }
}

fn session_key(session: &AgentSession) -> String {
    session.native_id.clone().unwrap_or_else(|| {
        format!(
            "{}:{}:{}",
            session.agent,
            session.cwd.display(),
            session.pid.unwrap_or_default()
        )
    })
}

fn tone_for_score(score: u32) -> ActivityTone {
    match score {
        0 => ActivityTone::Quiet,
        1..=3 => ActivityTone::Low,
        4..=8 => ActivityTone::Medium,
        9..=15 => ActivityTone::High,
        _ => ActivityTone::Hot,
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
    let (snapshot_tx, snapshot_rx) = mpsc::channel();
    spawn_snapshot_worker(source, snapshot_tx);
    let mut sampler = ProcessSampler::new();
    let mut snapshot = snapshot_for_source(source, 0, &mut sampler)?;
    let mut skyline = ActivitySkyline::new(160);
    let mut scorer = ActivityScorer::default();
    skyline.push_snapshot(&snapshot, &mut scorer);
    let mut needs_draw = true;

    loop {
        while let Ok(next_snapshot) = snapshot_rx.try_recv() {
            snapshot = next_snapshot;
            skyline.push_snapshot(&snapshot, &mut scorer);
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
                        skyline: &skyline,
                    },
                )
            })?;
            needs_draw = false;
        }

        if event::poll(INPUT_TICK)?
            && let Event::Key(key) = event::read()?
        {
            match handle_key(
                &mut mode,
                &mut selected,
                &mut filter,
                sessions.len(),
                key.code,
            ) {
                KeyAction::Quit => return Ok(()),
                KeyAction::Refresh => {
                    snapshot = snapshot_for_source(
                        source,
                        skyline.samples.len() as u64 + 1,
                        &mut sampler,
                    )?;
                    skyline.push_snapshot(&snapshot, &mut scorer);
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

fn handle_key(
    mode: &mut ViewMode,
    selected: &mut usize,
    filter: &mut SessionFilter,
    session_count: usize,
    key: KeyCode,
) -> KeyAction {
    match key {
        KeyCode::Char('q') => KeyAction::Quit,
        KeyCode::Esc => match mode {
            ViewMode::Tail { .. } => {
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
        KeyCode::Enter if matches!(mode, ViewMode::Monitor) => {
            *mode = ViewMode::Tail { scroll: 0 };
            KeyAction::Continue
        }
        KeyCode::Char('j') => {
            if let ViewMode::Tail { scroll } = mode {
                *scroll = scroll.saturating_add(1);
            } else {
                *selected = (*selected + 1).min(session_count.saturating_sub(1));
            }
            KeyAction::Continue
        }
        KeyCode::Char('k') => {
            if let ViewMode::Tail { scroll } = mode {
                *scroll = scroll.saturating_sub(1);
            } else {
                *selected = selected.saturating_sub(1);
            }
            KeyAction::Continue
        }
        KeyCode::Down => {
            *selected = (*selected + 1).min(session_count.saturating_sub(1));
            if let ViewMode::Tail { scroll } = mode {
                *scroll = 0;
            }
            KeyAction::Continue
        }
        KeyCode::Up => {
            *selected = selected.saturating_sub(1);
            if let ViewMode::Tail { scroll } = mode {
                *scroll = 0;
            }
            KeyAction::Continue
        }
        KeyCode::PageDown => {
            if let ViewMode::Tail { scroll } = mode {
                *scroll = scroll.saturating_add(5);
            }
            KeyAction::Continue
        }
        KeyCode::PageUp => {
            if let ViewMode::Tail { scroll } = mode {
                *scroll = scroll.saturating_sub(5);
            }
            KeyAction::Continue
        }
        KeyCode::Char('g') => {
            if let ViewMode::Tail { scroll } = mode {
                *scroll = 0;
            }
            KeyAction::Continue
        }
        KeyCode::Char('G') => {
            if let ViewMode::Tail { scroll } = mode {
                *scroll = usize::MAX;
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
    skyline: &'a ActivitySkyline,
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
            context.skyline,
        ),
        ViewMode::Tail { scroll } => draw_tail(
            frame,
            context.source,
            context.sessions,
            context.selected,
            *scroll,
            context.filter,
            context.skyline,
        ),
    }
}

#[cfg(test)]
fn skyline_rows(skyline: &ActivitySkyline, width: usize, height: usize) -> Vec<String> {
    let columns = skyline_columns(skyline, width, height);
    (0..height)
        .map(|row| columns.iter().map(|column| column.cells[row]).collect())
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SkylineColumn {
    cells: Vec<char>,
    tone: ActivityTone,
    colors: Vec<Color>,
}

fn skyline_columns(skyline: &ActivitySkyline, width: usize, height: usize) -> Vec<SkylineColumn> {
    let mut samples = skyline
        .samples
        .iter()
        .rev()
        .take(width)
        .copied()
        .collect::<Vec<_>>();
    samples.reverse();
    let mut columns = Vec::with_capacity(width);
    if samples.len() < width {
        columns.extend((0..(width - samples.len())).map(|_| SkylineColumn {
            cells: vec![' '; height],
            tone: ActivityTone::Quiet,
            colors: vec![color_for_tone(ActivityTone::Quiet); height],
        }));
    }

    for sample in samples {
        let level = if sample.score == 0 {
            0
        } else {
            (sample.score as usize * height)
                .div_ceil(SKYLINE_FULL_SCALE_SCORE)
                .clamp(1, height)
        };
        let mut cells = Vec::with_capacity(height);
        let mut colors = Vec::with_capacity(height);
        for row in 0..height {
            if height - row <= level {
                cells.push('▪');
                colors.push(color_for_layer(height - row, height, sample.tone));
            } else {
                cells.push(' ');
                colors.push(color_for_tone(ActivityTone::Quiet));
            }
        }
        columns.push(SkylineColumn {
            cells,
            tone: sample.tone,
            colors,
        });
    }
    columns
}

fn skyline_lines(skyline: &ActivitySkyline, width: usize, height: usize) -> Vec<Line<'static>> {
    let columns = skyline_columns(skyline, width, height);
    let mut rows = Vec::with_capacity(height);

    for row in 0..height {
        let mut spans = Vec::with_capacity(columns.len());
        for column in &columns {
            spans.push(Span::styled(
                column.cells[row].to_string(),
                Style::default().fg(column.colors[row]),
            ));
        }
        rows.push(Line::from(spans));
    }
    rows
}

fn color_for_layer(layer: usize, height: usize, tone: ActivityTone) -> Color {
    let pct = (layer * 100) / height.max(1);
    match (tone, pct) {
        (ActivityTone::Hot, 76..=100) => Color::Red,
        (_, 0..=35) => Color::Rgb(90, 170, 110),
        (_, 36..=70) => Color::Rgb(190, 210, 110),
        _ => Color::Rgb(230, 170, 80),
    }
}

fn color_for_tone(tone: ActivityTone) -> Color {
    match tone {
        ActivityTone::Quiet => Color::DarkGray,
        ActivityTone::Low => Color::Rgb(80, 150, 100),
        ActivityTone::Medium => Color::Green,
        ActivityTone::High => Color::Yellow,
        ActivityTone::Hot => Color::Red,
    }
}

fn draw_monitor(
    frame: &mut Frame<'_>,
    snapshot: &AmbientSnapshot,
    sessions: &[AgentSession],
    selected: usize,
    filter: SessionFilter,
    skyline: &ActivitySkyline,
) {
    let compact = frame.area().height < 32;
    let skyline_height = if compact { 5 } else { 7 };
    let activity_height = if compact { 5 } else { 7 };
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(skyline_height),
            Constraint::Min(9),
            Constraint::Length(activity_height),
            Constraint::Length(2),
        ])
        .split(frame.area());

    frame.render_widget(Clear, vertical[0]);
    frame.render_widget(header(snapshot, filter), vertical[0]);
    render_skyline(frame, skyline, vertical[1]);

    let main = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(61), Constraint::Percentage(39)])
        .split(vertical[2]);

    frame.render_widget(Clear, main[0]);
    frame.render_widget(session_table(snapshot, sessions, selected, filter), main[0]);
    frame.render_widget(Clear, main[1]);
    frame.render_widget(session_detail(sessions.get(selected)), main[1]);
    render_activity(frame, snapshot, vertical[3]);
    frame.render_widget(Clear, vertical[4]);
    frame.render_widget(monitor_footer(), vertical[4]);

    if sessions.is_empty() {
        draw_empty_state(frame, main[0]);
    }
}

fn render_skyline(frame: &mut Frame<'_>, skyline: &ActivitySkyline, area: Rect) {
    frame.render_widget(Clear, area);
    let inner_width = area.width.saturating_sub(4) as usize;
    let rows = skyline_lines(skyline, inner_width, area.height.saturating_sub(2) as usize);
    frame.render_widget(
        Paragraph::new(rows).block(
            Block::default()
                .title(Line::from(vec![Span::styled(
                    " agent activity ",
                    title_style(),
                )]))
                .borders(Borders::ALL),
        ),
        area,
    );
}

fn header(snapshot: &AmbientSnapshot, filter: SessionFilter) -> Paragraph<'static> {
    let status = if snapshot.active_count() == 0 {
        "idle".to_string()
    } else {
        format!(
            "{} active - {} tracked",
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
        Span::styled("AI AGENT MONITOR", dim()),
        Span::raw("  "),
        Span::styled("native ambient sources", Style::default().fg(Color::Gray)),
        Span::raw("  "),
        Span::styled(status, Style::default().fg(Color::Gray)),
        Span::raw("  "),
        Span::styled(format!("filter: {} ", filter.label()), dim()),
        Span::raw("  "),
        Span::styled("live ", Style::default().fg(Color::Green)),
        Span::styled(time_label(Some(snapshot.generated_at)), strong()),
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

fn session_row(session: &AgentSession, now: std::time::SystemTime) -> Line<'static> {
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
        Span::raw(format!("{:<10}", elapsed_label(session.started_at, now))),
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

fn render_activity(frame: &mut Frame<'_>, snapshot: &AmbientSnapshot, area: Rect) {
    frame.render_widget(Clear, area);
    frame.render_widget(
        activity(snapshot, area.width.saturating_sub(4) as usize),
        area,
    );
}

fn activity(snapshot: &AmbientSnapshot, width: usize) -> Paragraph<'static> {
    Paragraph::new(activity_lines(snapshot, width)).block(
        Block::default()
            .title(Line::from(vec![
                Span::styled(" activity ", title_style()),
                Span::styled(activity_subtitle(snapshot), dim()),
            ]))
            .borders(Borders::ALL),
    )
}

fn activity_lines(snapshot: &AmbientSnapshot, width: usize) -> Vec<Line<'static>> {
    if snapshot.active_count() == 0 {
        return vec![Line::from(truncate(
            " -   watching native sources - no live agent activity",
            width,
        ))];
    }
    if snapshot.activity.is_empty() {
        return vec![Line::from(truncate(
            " -   watching native Claude/Codex activity",
            width,
        ))];
    }
    snapshot
        .activity
        .iter()
        .map(|line| Line::from(truncate(line, width)))
        .collect()
}

fn activity_subtitle(snapshot: &AmbientSnapshot) -> &'static str {
    if snapshot.active_count() == 0 {
        "idle "
    } else {
        "live project events "
    }
}

fn monitor_footer() -> Paragraph<'static> {
    Paragraph::new(Line::from(vec![
        Span::styled(" up/down ", key_style()),
        Span::raw(" select   "),
        Span::styled("enter", key_style()),
        Span::raw(" tail   "),
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

fn draw_tail(
    frame: &mut Frame<'_>,
    source: DashboardSource,
    sessions: &[AgentSession],
    selected: usize,
    scroll: usize,
    filter: SessionFilter,
    skyline: &ActivitySkyline,
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
    let feed = selected_session
        .and_then(|session| load_feed_for_session(source, session, skyline.samples.len() as u64));
    frame.render_widget(Clear, body[1]);
    frame.render_widget(
        tail_feed(
            selected_session,
            feed.as_ref(),
            scroll,
            body[1].width.saturating_sub(4) as usize,
        ),
        body[1],
    );
    frame.render_widget(Clear, vertical[1]);
    frame.render_widget(tail_footer(feed.as_ref()), vertical[1]);
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

fn tail_feed(
    session: Option<&AgentSession>,
    feed: Option<&SessionFeed>,
    scroll: usize,
    width: usize,
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

    let max_start = lines.len().saturating_sub(1);
    let scroll = if scroll == usize::MAX {
        max_start
    } else {
        scroll.min(max_start)
    };
    Paragraph::new(lines)
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Green)),
        )
        .scroll((scroll as u16, 0))
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
    if value.chars().count() <= max {
        return value.to_string();
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
mod tests {
    use super::*;
    use crate::model::{AgentKind, AgentSession, ProcessStats};

    #[test]
    fn tail_up_down_changes_selected_session_and_resets_scroll() {
        let mut mode = ViewMode::Tail { scroll: 9 };
        let mut selected = 1usize;
        let mut filter = SessionFilter::Active;

        handle_key(&mut mode, &mut selected, &mut filter, 3, KeyCode::Down);

        assert_eq!(selected, 2);
        assert_eq!(tail_scroll(&mode), Some(0));

        handle_key(&mut mode, &mut selected, &mut filter, 3, KeyCode::Up);

        assert_eq!(selected, 1);
        assert_eq!(tail_scroll(&mode), Some(0));
    }

    #[test]
    fn tail_jk_scrolls_feed_without_changing_selected_session() {
        let mut mode = ViewMode::Tail { scroll: 4 };
        let mut selected = 1usize;
        let mut filter = SessionFilter::Active;

        handle_key(&mut mode, &mut selected, &mut filter, 3, KeyCode::Char('j'));

        assert_eq!(selected, 1);
        assert_eq!(tail_scroll(&mode), Some(5));

        handle_key(&mut mode, &mut selected, &mut filter, 3, KeyCode::Char('k'));

        assert_eq!(selected, 1);
        assert_eq!(tail_scroll(&mode), Some(4));
    }

    #[test]
    fn tail_page_keys_scroll_feed_without_changing_selected_session() {
        let mut mode = ViewMode::Tail { scroll: 4 };
        let mut selected = 1usize;
        let mut filter = SessionFilter::Active;

        handle_key(&mut mode, &mut selected, &mut filter, 3, KeyCode::PageDown);

        assert_eq!(selected, 1);
        assert_eq!(tail_scroll(&mode), Some(9));

        handle_key(&mut mode, &mut selected, &mut filter, 3, KeyCode::PageUp);

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
    fn activity_panel_is_quiet_when_no_sessions_are_active() {
        let snapshot = AmbientSnapshot {
            sessions: vec![test_session("recent", SessionStatus::Recent, "/repo", 10)],
            generated_at: SystemTime::UNIX_EPOCH,
            activity: vec!["04:25:19 observed recent - codex".to_string()],
        };

        let text = lines_to_plain_text(activity_lines(&snapshot, 120));

        assert!(text.contains("watching native sources"));
        assert!(text.contains("no live agent activity"));
        assert!(!text.contains("observed recent"));
    }

    #[test]
    fn skyline_rolls_scores_and_discards_oldest_columns() {
        let mut skyline = ActivitySkyline::new(3);

        skyline.push_score(1);
        skyline.push_score(2);
        skyline.push_score(3);
        skyline.push_score(4);

        assert_eq!(skyline.scores(), vec![2, 3, 4]);
    }

    #[test]
    fn skyline_turns_activity_scores_into_dot_grid() {
        let mut skyline = ActivitySkyline::new(8);
        skyline.push_sample(ActivitySample::quiet());
        skyline.push_sample(ActivitySample::new(2, ActivityTone::Low));
        skyline.push_sample(ActivitySample::new(4, ActivityTone::Medium));

        let rows = skyline_rows(&skyline, 3, 3);

        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0], "   ");
        assert_eq!(rows[1], "   ");
        assert_eq!(rows[2], " ▪▪");
    }

    #[test]
    fn skyline_scores_deltas_not_static_load() {
        let mut session = test_session("live", SessionStatus::Running, "/repo", 40);
        session.process = Some(ProcessStats {
            cpu_percent: 10,
            memory_bytes: 128 * 1024 * 1024,
            child_pids: vec![1],
        });
        session.tokens_total = Some(10_000);

        let first = AmbientSnapshot {
            sessions: vec![session.clone()],
            generated_at: SystemTime::UNIX_EPOCH + Duration::from_secs(45),
            activity: vec![],
        };
        let unchanged = AmbientSnapshot {
            sessions: vec![session.clone()],
            generated_at: SystemTime::UNIX_EPOCH + Duration::from_secs(46),
            activity: vec![],
        };
        session.process.as_mut().unwrap().cpu_percent = 80;
        session.tokens_total = Some(16_000);
        session.updated_at = Some(SystemTime::UNIX_EPOCH + Duration::from_secs(47));
        let changed = AmbientSnapshot {
            sessions: vec![session],
            generated_at: SystemTime::UNIX_EPOCH + Duration::from_secs(47),
            activity: vec!["changed M src/lib.rs".to_string()],
        };

        let mut scorer = ActivityScorer::default();

        let warmup = scorer.score(&first);
        let quiet = scorer.score(&unchanged);
        let active = scorer.score(&changed);

        assert_eq!(warmup, ActivitySample::quiet());
        assert_eq!(quiet.score, 0);
        assert!(active.score > quiet.score);
        assert!(matches!(
            active.tone,
            ActivityTone::High | ActivityTone::Hot
        ));
    }

    #[test]
    fn skyline_ignores_repeated_stale_activity_lines() {
        let mut session = test_session("live", SessionStatus::Running, "/repo", 40);
        session.process = Some(ProcessStats {
            cpu_percent: 0,
            memory_bytes: 128 * 1024 * 1024,
            child_pids: vec![],
        });
        let first = AmbientSnapshot {
            sessions: vec![session.clone()],
            generated_at: SystemTime::UNIX_EPOCH + Duration::from_secs(45),
            activity: vec!["11:27:15 changed M src/lib.rs".to_string()],
        };
        let repeated = AmbientSnapshot {
            sessions: vec![session],
            generated_at: SystemTime::UNIX_EPOCH + Duration::from_secs(46),
            activity: vec!["11:27:15 changed M src/lib.rs".to_string()],
        };

        let mut scorer = ActivityScorer::default();

        assert_eq!(scorer.score(&first), ActivitySample::quiet());
        assert_eq!(scorer.score(&repeated), ActivitySample::quiet());
    }

    #[test]
    fn skyline_uses_absolute_scale_for_low_scores() {
        let mut skyline = ActivitySkyline::new(8);
        skyline.push_sample(ActivitySample::new(2, ActivityTone::Low));

        let rows = skyline_rows(&skyline, 1, 5);

        assert_eq!(rows, vec![" ", " ", " ", " ", "▪"]);
    }

    #[test]
    fn skyline_render_preserves_tone_for_colored_columns() {
        let mut skyline = ActivitySkyline::new(8);
        skyline.push_sample(ActivitySample::new(1, ActivityTone::Low));
        skyline.push_sample(ActivitySample::new(5, ActivityTone::Medium));
        skyline.push_sample(ActivitySample::new(9, ActivityTone::High));
        skyline.push_sample(ActivitySample::new(14, ActivityTone::Hot));

        let columns = skyline_columns(&skyline, 4, 3);

        assert_eq!(
            columns.iter().map(|column| column.tone).collect::<Vec<_>>(),
            vec![
                ActivityTone::Low,
                ActivityTone::Medium,
                ActivityTone::High,
                ActivityTone::Hot
            ]
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
    fn skyline_columns_use_one_pixel_glyph_not_punctuation_bands() {
        let mut skyline = ActivitySkyline::new(4);
        skyline.push_sample(ActivitySample::new(8, ActivityTone::Low));
        skyline.push_sample(ActivitySample::new(14, ActivityTone::Hot));

        let rows = skyline_rows(&skyline, 2, 4).join("\n");

        assert!(rows.contains('▪'));
        assert!(!rows.contains('.'));
        assert!(!rows.contains(':'));
        assert!(!rows.contains('*'));
        assert!(!rows.contains('#'));
    }

    fn tail_scroll(mode: &ViewMode) -> Option<usize> {
        match mode {
            ViewMode::Tail { scroll } => Some(*scroll),
            ViewMode::Monitor => None,
        }
    }

    fn lines_to_plain_text(lines: Vec<Line<'static>>) -> String {
        lines
            .into_iter()
            .flat_map(|line| line.spans.into_iter().map(|span| span.content.to_string()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn test_session(id: &str, status: SessionStatus, cwd: &str, updated_at: u64) -> AgentSession {
        AgentSession {
            agent: AgentKind::Codex,
            native_id: Some(id.to_string()),
            title: None,
            command: Some("codex".to_string()),
            cwd: cwd.into(),
            pid: if status == SessionStatus::Running {
                Some(123)
            } else {
                None
            },
            status,
            started_at: None,
            updated_at: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(updated_at)),
            model: None,
            tokens_total: None,
            git_branch: None,
            journal_path: None,
            process: None,
            git: None,
        }
    }
}
