use std::{io, time::Duration};

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
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
};

use crate::{
    app::{AmbientSnapshot, format_bytes, snapshot_with_sampler},
    feed::{FeedEvent, FeedRecord, SessionFeed, annotation_summary, load_session_feed},
    model::{AgentSession, SessionStatus, elapsed_label, path_home_display, time_label},
    pricing::{compact_tokens, short_model},
    process::ProcessSampler,
};

const TICK: Duration = Duration::from_millis(1000);

enum ViewMode {
    Monitor,
    Tail { scroll: usize },
}

pub fn run() -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, cursor::Hide)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let result = run_loop(&mut terminal);
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, cursor::Show)?;
    result
}

fn run_loop(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    let mut selected = 0usize;
    let mut mode = ViewMode::Monitor;
    let mut sampler = ProcessSampler::new();

    loop {
        let snapshot = snapshot_with_sampler(&mut sampler)?;
        selected = clamp_selected(selected, snapshot.sessions.len());
        terminal.draw(|frame| draw(frame, &snapshot, selected, &mode))?;

        if !event::poll(TICK)? {
            continue;
        }
        if let Event::Key(key) = event::read()? {
            match (&mut mode, key.code) {
                (_, KeyCode::Char('q')) => return Ok(()),
                (ViewMode::Tail { .. }, KeyCode::Esc) => mode = ViewMode::Monitor,
                (ViewMode::Monitor, KeyCode::Esc) => return Ok(()),
                (ViewMode::Monitor, KeyCode::Enter) => mode = ViewMode::Tail { scroll: 0 },
                (ViewMode::Monitor, KeyCode::Down | KeyCode::Char('j')) => {
                    selected = (selected + 1).min(snapshot.sessions.len().saturating_sub(1));
                }
                (ViewMode::Monitor, KeyCode::Up | KeyCode::Char('k')) => {
                    selected = selected.saturating_sub(1);
                }
                (ViewMode::Tail { scroll }, KeyCode::Down | KeyCode::Char('j')) => {
                    *scroll = scroll.saturating_add(1);
                }
                (ViewMode::Tail { scroll }, KeyCode::Up | KeyCode::Char('k')) => {
                    *scroll = scroll.saturating_sub(1);
                }
                (ViewMode::Tail { scroll }, KeyCode::Char('g')) => *scroll = 0,
                (ViewMode::Tail { scroll }, KeyCode::Char('G')) => *scroll = usize::MAX,
                (_, KeyCode::Char('r')) => {}
                _ => {}
            }
        }
    }
}

fn draw(frame: &mut Frame<'_>, snapshot: &AmbientSnapshot, selected: usize, mode: &ViewMode) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    match mode {
        ViewMode::Monitor => draw_monitor(frame, snapshot, selected),
        ViewMode::Tail { scroll } => draw_tail(frame, snapshot, selected, *scroll),
    }
}

fn draw_monitor(frame: &mut Frame<'_>, snapshot: &AmbientSnapshot, selected: usize) {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(16),
            Constraint::Length(8),
            Constraint::Length(2),
        ])
        .split(frame.area());

    frame.render_widget(header(snapshot), vertical[0]);

    let main = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(61), Constraint::Percentage(39)])
        .split(vertical[1]);

    frame.render_widget(session_table(snapshot, selected), main[0]);
    frame.render_widget(session_detail(snapshot.sessions.get(selected)), main[1]);
    frame.render_widget(activity(snapshot), vertical[2]);
    frame.render_widget(monitor_footer(), vertical[3]);

    if snapshot.sessions.is_empty() {
        draw_empty_state(frame, main[0]);
    }
}

fn header(snapshot: &AmbientSnapshot) -> Paragraph<'static> {
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
        Span::styled("live ", Style::default().fg(Color::Green)),
        Span::styled(time_label(Some(snapshot.generated_at)), strong()),
    ]))
    .block(Block::default().borders(Borders::BOTTOM))
}

fn session_table(snapshot: &AmbientSnapshot, selected: usize) -> List<'static> {
    let mut rows = vec![ListItem::new(Line::from(vec![
        Span::styled("  AGENT     ", dim()),
        Span::styled("REPO        ", dim()),
        Span::styled("ELAPSED   ", dim()),
        Span::styled("PID      ", dim()),
        Span::styled("CPU          ", dim()),
        Span::styled("MEM        ", dim()),
        Span::styled("TOK", dim()),
    ]))];

    for (index, session) in snapshot.sessions.iter().enumerate() {
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

fn activity(snapshot: &AmbientSnapshot) -> Paragraph<'static> {
    let lines = if snapshot.activity.is_empty() {
        vec![Line::from(" -   watch   native Claude/Codex activity")]
    } else {
        snapshot
            .activity
            .iter()
            .map(|line| Line::from(line.clone()))
            .collect()
    };
    Paragraph::new(lines).block(
        Block::default()
            .title(Line::from(vec![
                Span::styled(" activity ", title_style()),
                Span::styled("recent project events ", dim()),
            ]))
            .borders(Borders::ALL),
    )
}

fn monitor_footer() -> Paragraph<'static> {
    Paragraph::new(Line::from(vec![
        Span::styled(" up/down ", key_style()),
        Span::raw(" select   "),
        Span::styled("enter", key_style()),
        Span::raw(" tail   "),
        Span::styled("r", key_style()),
        Span::raw(" refresh   "),
        Span::styled("q", key_style()),
        Span::raw(" quit   "),
        Span::styled("future", dim()),
        Span::raw(" ask-repl"),
    ]))
    .block(Block::default().borders(Borders::TOP))
}

fn draw_tail(frame: &mut Frame<'_>, snapshot: &AmbientSnapshot, selected: usize, scroll: usize) {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(12), Constraint::Length(2)])
        .split(frame.area());
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(36), Constraint::Min(40)])
        .split(vertical[0]);

    frame.render_widget(tail_sidebar(snapshot, selected), body[0]);
    let selected_session = snapshot.sessions.get(selected);
    let feed = selected_session.and_then(load_feed_for_session);
    frame.render_widget(tail_feed(selected_session, feed.as_ref(), scroll), body[1]);
    frame.render_widget(tail_footer(feed.as_ref()), vertical[1]);
}

fn tail_sidebar(snapshot: &AmbientSnapshot, selected: usize) -> Paragraph<'static> {
    let mut lines = vec![Line::from(Span::styled(
        format!("SESSIONS - {} SESSIONS", snapshot.sessions.len()),
        dim(),
    ))];
    for (index, session) in snapshot.sessions.iter().enumerate() {
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

fn load_feed_for_session(session: &AgentSession) -> Option<SessionFeed> {
    let path = session.journal_path.as_ref()?;
    let id = session.native_id.as_deref().unwrap_or("-");
    load_session_feed(path, session.agent, id, 600).ok()
}

fn tail_feed(
    session: Option<&AgentSession>,
    feed: Option<&SessionFeed>,
    scroll: usize,
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
                lines.extend(feed_record_lines(record));
            }
        }
        (Some(session), None) => {
            lines.push(centered("no native journal for this session"));
            lines.push(centered(&session.display_title()));
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
        .wrap(Wrap { trim: false })
}

fn feed_record_lines(record: &FeedRecord) -> Vec<Line<'static>> {
    let badge = annotation_summary(&record.annotations);
    let badge = if badge.is_empty() {
        Vec::new()
    } else {
        vec![
            Span::raw(" "),
            Span::styled(format!("[{badge}]"), Style::default().fg(Color::Yellow)),
        ]
    };
    match &record.event {
        FeedEvent::User { text } => {
            let mut spans = vec![
                Span::styled("> you ", Style::default().fg(Color::Cyan)),
                Span::raw(text.clone()),
            ];
            spans.extend(badge);
            vec![Line::from(spans)]
        }
        FeedEvent::Assistant { text, .. } => {
            let mut lines = vec![Line::from(vec![Span::styled("* assistant", accent())])];
            lines.push(Line::from(format!("  {}", text)));
            lines
        }
        FeedEvent::Thinking { text } => vec![Line::from(vec![
            Span::styled("~ thinking ", dim()),
            Span::styled(
                text.clone(),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ),
        ])],
        FeedEvent::ToolCall { name, summary, .. } => {
            let mut spans = vec![
                Span::styled("# ", Style::default().fg(Color::Yellow)),
                Span::styled(
                    name.clone(),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::styled(summary.clone(), dim()),
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
            let mut lines = vec![Line::from(vec![Span::styled("` result", style)])];
            for line in detail.lines().take(10) {
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(line.to_string(), style),
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
        Span::raw(" scroll   "),
        Span::styled("esc", key_style()),
        Span::raw(" monitor   "),
        Span::styled("q", key_style()),
        Span::raw(" quit"),
        Span::raw(format!("{:>48}", "")),
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
