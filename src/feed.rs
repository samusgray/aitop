use std::{
    collections::VecDeque,
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

use anyhow::Result;
use serde_json::Value;

use crate::{model::AgentKind, pricing};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedEvent {
    User {
        text: String,
    },
    Assistant {
        text: String,
        model: String,
    },
    Thinking {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        summary: String,
    },
    ToolResult {
        id: String,
        ok: bool,
        summary: String,
        detail: String,
    },
    Usage {
        input: u64,
        output: u64,
        cache_read: u64,
    },
    Unknown {
        kind: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Annotation {
    Error,
    TokenSpike { tokens: u64 },
    FileTouched(String),
    CommandRun(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedRecord {
    pub session_id: String,
    pub timestamp: Option<String>,
    pub event: FeedEvent,
    pub annotations: Vec<Annotation>,
}

#[derive(Debug, Clone, Default)]
pub struct SessionFeed {
    pub records: Vec<FeedRecord>,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cache_read: u64,
    pub estimated_cost: f64,
    pub context_pct: Option<u8>,
    pub model: Option<String>,
}

struct Highlighter {
    recent: VecDeque<u64>,
}

impl Highlighter {
    fn new() -> Self {
        Self {
            recent: VecDeque::with_capacity(20),
        }
    }

    fn annotate(&mut self, event: &FeedEvent) -> Vec<Annotation> {
        match event {
            FeedEvent::ToolResult { ok: false, .. } => vec![Annotation::Error],
            FeedEvent::Usage { input, output, .. } => {
                let total = input + output;
                let mut annotations = Vec::new();
                if total >= 10_000 && total as f64 > self.median() * 2.0 {
                    annotations.push(Annotation::TokenSpike { tokens: total });
                }
                self.record(total);
                annotations
            }
            _ => Vec::new(),
        }
    }

    fn median(&self) -> f64 {
        if self.recent.is_empty() {
            return 0.0;
        }
        let mut values = self.recent.iter().copied().collect::<Vec<_>>();
        values.sort_unstable();
        let mid = values.len() / 2;
        if values.len() % 2 == 0 {
            (values[mid - 1] + values[mid]) as f64 / 2.0
        } else {
            values[mid] as f64
        }
    }

    fn record(&mut self, total: u64) {
        if self.recent.len() == 20 {
            self.recent.pop_front();
        }
        self.recent.push_back(total);
    }
}

pub fn load_session_feed(
    path: &Path,
    agent: AgentKind,
    session_id: &str,
    max_lines: usize,
) -> Result<SessionFeed> {
    let text = read_tail(path, 256 * 1024)?;
    let lines = text.lines().collect::<Vec<_>>();
    let start = lines.len().saturating_sub(max_lines);
    let mut feed = SessionFeed::default();
    let mut highlighter = Highlighter::new();

    for line in lines.into_iter().skip(start) {
        let parsed = match agent {
            AgentKind::Claude => parse_claude_line(line, session_id),
            AgentKind::Codex => parse_codex_line(line, session_id),
        };
        for mut record in parsed {
            for annotation in highlighter.annotate(&record.event) {
                if !record.annotations.contains(&annotation) {
                    record.annotations.push(annotation);
                }
            }
            match &record.event {
                FeedEvent::Usage {
                    input,
                    output,
                    cache_read,
                } => {
                    feed.tokens_in += input;
                    feed.tokens_out += output;
                    feed.cache_read += cache_read;
                }
                FeedEvent::Assistant { model, .. } if !model.is_empty() => {
                    feed.model = Some(model.clone());
                }
                _ => {}
            }
            feed.records.push(record);
        }
    }

    if let Some(model) = &feed.model {
        feed.estimated_cost = pricing::estimate_cost(feed.tokens_in, feed.tokens_out, model);
        let context = pricing::lookup(model).context_window.max(1);
        feed.context_pct = Some(((feed.tokens_in * 100) / context).min(100) as u8);
    }

    Ok(feed)
}

pub fn parse_claude_line(line: &str, session_id: &str) -> Vec<FeedRecord> {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    let timestamp = value
        .get("timestamp")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "assistant" => parse_claude_assistant(&value, session_id, timestamp),
        "user" => parse_claude_user(&value, session_id, timestamp),
        "" => Vec::new(),
        other => vec![record(
            session_id,
            timestamp,
            FeedEvent::Unknown {
                kind: other.to_string(),
            },
        )],
    }
}

fn parse_claude_assistant(
    value: &Value,
    session_id: &str,
    timestamp: Option<String>,
) -> Vec<FeedRecord> {
    let message = value.get("message").unwrap_or(&Value::Null);
    let model = message
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut records = Vec::new();
    if let Some(blocks) = message.get("content").and_then(Value::as_array) {
        for block in blocks {
            if let Some(record) =
                claude_block_to_record(block, &model, session_id, timestamp.clone())
            {
                records.push(record);
            }
        }
    }
    if let Some(usage) = message.get("usage") {
        records.push(record(
            session_id,
            timestamp,
            FeedEvent::Usage {
                input: usage
                    .get("input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                output: usage
                    .get("output_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                cache_read: usage
                    .get("cache_read_input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            },
        ));
        if let Some(FeedRecord {
            event: FeedEvent::Usage { input, .. },
            ..
        }) = records.last_mut()
        {
            *input += usage
                .get("cache_creation_input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
        }
    }
    records
}

fn parse_claude_user(
    value: &Value,
    session_id: &str,
    timestamp: Option<String>,
) -> Vec<FeedRecord> {
    let content = value
        .get("message")
        .and_then(|message| message.get("content"));
    match content {
        Some(Value::String(text)) => vec![record(
            session_id,
            timestamp,
            FeedEvent::User { text: text.clone() },
        )],
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| claude_block_to_record(block, "", session_id, timestamp.clone()))
            .collect(),
        _ => Vec::new(),
    }
}

fn claude_block_to_record(
    block: &Value,
    model: &str,
    session_id: &str,
    timestamp: Option<String>,
) -> Option<FeedRecord> {
    let kind = block.get("type")?.as_str()?;
    match kind {
        "text" => Some(record(
            session_id,
            timestamp,
            FeedEvent::Assistant {
                text: block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                model: model.to_string(),
            },
        )),
        "thinking" => Some(record(
            session_id,
            timestamp,
            FeedEvent::Thinking {
                text: block
                    .get("thinking")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            },
        )),
        "tool_use" => {
            let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
            let id = block
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let input = block.get("input").unwrap_or(&Value::Null);
            let (summary, annotation) = summarize_tool(name, input);
            let mut rec = record(
                session_id,
                timestamp,
                FeedEvent::ToolCall {
                    id,
                    name: name.to_string(),
                    summary,
                },
            );
            if let Some(annotation) = annotation {
                rec.annotations.push(annotation);
            }
            Some(rec)
        }
        "tool_result" => {
            let id = block
                .get("tool_use_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let ok = !block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let detail = match block.get("content") {
                Some(Value::String(text)) => text.clone(),
                Some(other) => other.to_string(),
                None => String::new(),
            };
            let mut rec = record(
                session_id,
                timestamp,
                FeedEvent::ToolResult {
                    id,
                    ok,
                    summary: truncate_summary(&detail, 120),
                    detail: truncate_detail(&detail, 12, 2048),
                },
            );
            if !ok {
                rec.annotations.push(Annotation::Error);
            }
            Some(rec)
        }
        _ => None,
    }
}

pub fn parse_codex_line(line: &str, session_id: &str) -> Vec<FeedRecord> {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    let timestamp = value
        .get("timestamp")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let payload = value.get("payload").unwrap_or(&value);
    let payload_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
    let role = payload.get("role").and_then(Value::as_str).unwrap_or("");
    let text = payload
        .get("message")
        .or_else(|| payload.get("output"))
        .and_then(Value::as_str)
        .or_else(|| content_text(payload))
        .unwrap_or("");
    if text.is_empty() {
        return Vec::new();
    }
    let event = if role == "user" {
        FeedEvent::User {
            text: truncate_summary(text, 300),
        }
    } else if payload_type.contains("tool")
        || payload_type.contains("function_call_output")
        || payload_type.contains("function")
    {
        FeedEvent::ToolResult {
            id: payload
                .get("call_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            ok: !text.to_ascii_lowercase().contains("error"),
            summary: truncate_summary(text, 120),
            detail: truncate_detail(text, 12, 2048),
        }
    } else {
        FeedEvent::Assistant {
            text: truncate_summary(text, 500),
            model: payload
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or("codex")
                .to_string(),
        }
    };
    vec![record(session_id, timestamp, event)]
}

fn read_tail(path: &Path, max_bytes: u64) -> Result<String> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start))?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    if start > 0
        && let Some(index) = text.find('\n')
    {
        text = text[index + 1..].to_string();
    }
    Ok(text)
}

fn content_text(payload: &Value) -> Option<&str> {
    payload
        .get("content")
        .and_then(Value::as_array)
        .and_then(|items| {
            items
                .iter()
                .find_map(|item| item.get("text").and_then(Value::as_str))
        })
}

fn summarize_tool(name: &str, input: &Value) -> (String, Option<Annotation>) {
    if name == "Bash" {
        let command = input.get("command").and_then(Value::as_str).unwrap_or("");
        return (
            format!("Bash {}", truncate_summary(command, 120)),
            (!command.is_empty()).then(|| Annotation::CommandRun(command.to_string())),
        );
    }
    if matches!(name, "Edit" | "Write" | "MultiEdit" | "NotebookEdit")
        && let Some(path) = input.get("file_path").and_then(Value::as_str)
    {
        return (
            format!("{name} {path}"),
            Some(Annotation::FileTouched(path.to_string())),
        );
    }
    (name.to_string(), None)
}

fn record(session_id: &str, timestamp: Option<String>, event: FeedEvent) -> FeedRecord {
    FeedRecord {
        session_id: session_id.to_string(),
        timestamp,
        event,
        annotations: Vec::new(),
    }
}

pub fn annotation_summary(annotations: &[Annotation]) -> String {
    let mut parts = Vec::new();
    for annotation in annotations {
        match annotation {
            Annotation::Error => parts.push("ERR".to_string()),
            Annotation::TokenSpike { tokens } => {
                parts.push(format!("spike {}", pricing::compact_tokens(*tokens)))
            }
            Annotation::FileTouched(path) => parts.push(format!("file {path}")),
            Annotation::CommandRun(command) => {
                parts.push(format!("cmd {}", truncate_summary(command, 24)))
            }
        }
    }
    parts.join(" · ")
}

pub fn truncate_detail(text: &str, max_lines: usize, max_bytes: usize) -> String {
    let mut output = String::new();
    for (index, line) in text.lines().enumerate() {
        if index >= max_lines || output.len() + line.len() > max_bytes {
            output.push('…');
            break;
        }
        if index > 0 {
            output.push('\n');
        }
        output.push_str(line);
    }
    output
}

pub fn truncate_summary(text: &str, max_chars: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max_chars {
        return collapsed;
    }
    let kept = collapsed
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    format!("{kept}…")
}
