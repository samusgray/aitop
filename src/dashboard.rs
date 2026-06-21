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
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
};

use crate::{
    app::{AmbientSnapshot, format_bytes, snapshot_with_sampler},
    model::{AgentSession, SessionStatus, elapsed_label, path_home_display, time_label},
    process::ProcessSampler,
};

const TICK: Duration = Duration::from_millis(1000);

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
    let mut sampler = ProcessSampler::new();
    loop {
        let snapshot = snapshot_with_sampler(&mut sampler)?;
        if selected >= snapshot.sessions.len() {
            selected = snapshot.sessions.len().saturating_sub(1);
        }
        terminal.draw(|frame| draw(frame, &snapshot, selected))?;

        if !event::poll(TICK)? {
            continue;
        }
        if let Event::Key(key) = event::read()? {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Down | KeyCode::Char('j') => {
                    selected = (selected + 1).min(snapshot.sessions.len().saturating_sub(1));
                }
                KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
                KeyCode::Char('r') => {}
                _ => {}
            }
        }
    }
}

fn draw(frame: &mut Frame<'_>, snapshot: &AmbientSnapshot, selected: usize) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(12),
            Constraint::Length(8),
            Constraint::Length(2),
        ])
        .split(area);

    frame.render_widget(header(snapshot), vertical[0]);

    let main = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(61), Constraint::Percentage(39)])
        .split(vertical[1]);

    frame.render_widget(sessions_list(snapshot, selected), main[0]);
    frame.render_widget(session_detail(snapshot.sessions.get(selected)), main[1]);
    frame.render_widget(activity(snapshot), vertical[2]);
    frame.render_widget(footer(), vertical[3]);

    if snapshot.sessions.is_empty() {
        draw_empty_state(frame, main[0]);
    }
}

fn header(snapshot: &AmbientSnapshot) -> Paragraph<'static> {
    let status = if snapshot.active_count() == 0 {
        "idle".to_string()
    } else {
        format!(
            "{} active / {} tracked",
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
        Span::styled("AI AGENT MONITOR", Style::default().fg(Color::DarkGray)),
        Span::raw(" | "),
        Span::styled(status, Style::default().fg(Color::Gray)),
        Span::raw(" | "),
        Span::styled("live ", Style::default().fg(Color::Green)),
        Span::styled(
            time_label(Some(snapshot.generated_at)),
            Style::default().fg(Color::White),
        ),
    ]))
    .block(Block::default().borders(Borders::BOTTOM))
}

fn sessions_list(snapshot: &AmbientSnapshot, selected: usize) -> List<'static> {
    let mut rows = vec![ListItem::new(Line::from(vec![
        Span::styled("  AGENT     ", dim()),
        Span::styled("REPO        ", dim()),
        Span::styled("ELAPSED   ", dim()),
        Span::styled("PID      ", dim()),
        Span::styled("CPU     ", dim()),
        Span::styled("MEM      ", dim()),
        Span::styled("TOK", dim()),
    ]))];

    let now = snapshot.generated_at;
    for (index, session) in snapshot.sessions.iter().enumerate() {
        let style = if index == selected {
            Style::default().bg(Color::Rgb(20, 45, 36)).fg(Color::White)
        } else {
            Style::default().fg(Color::Gray)
        };
        rows.push(ListItem::new(session_row(session, now)).style(style));
    }

    List::new(rows).block(
        Block::default()
            .title(Span::styled(" sessions ", title_style()))
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
        .map(|process| format!("{}%", process.cpu_percent))
        .unwrap_or_else(|| "-".to_string());
    let mem = session
        .process
        .as_ref()
        .map(|process| format_bytes(process.memory_bytes))
        .unwrap_or_else(|| "-".to_string());
    let tokens = session
        .tokens_total
        .map(short_number)
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
        Span::raw(format!("{:<8}", cpu)),
        Span::raw(format!("{:<9}", mem)),
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
                        .map(|git| path_home_display(&git.root))
                        .unwrap_or_else(|| "unknown".into()),
                ),
                kv("CWD", &path_home_display(&session.cwd)),
                kv("STARTED", &time_label(session.started_at)),
                kv("UPDATED", &time_label(session.updated_at)),
                kv("MODEL", session.model.as_deref().unwrap_or("unknown")),
                kv(
                    "TOKENS",
                    &session
                        .tokens_total
                        .map(|tokens| tokens.to_string())
                        .unwrap_or_else(|| "unknown".into()),
                ),
            ];
            if let Some(process) = &session.process {
                lines.push(kv(
                    "CPU / MEM",
                    &format!(
                        "{}% - {}",
                        process.cpu_percent,
                        format_bytes(process.memory_bytes)
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
            .title(Span::styled(" session detail ", title_style()))
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
            .title(Span::styled(" activity ", title_style()))
            .borders(Borders::ALL),
    )
}

fn footer() -> Paragraph<'static> {
    Paragraph::new(Line::from(vec![
        Span::styled("  up/down ", key_style()),
        Span::raw("select   "),
        Span::styled("r", key_style()),
        Span::raw(" refresh   "),
        Span::styled("q", key_style()),
        Span::raw(" quit"),
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
        .alignment(Alignment::Center)
        .block(Block::default()),
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

fn short_number(value: i64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{}K", value / 1_000)
    } else {
        value.to_string()
    }
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
        .fg(Color::Cyan)
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
