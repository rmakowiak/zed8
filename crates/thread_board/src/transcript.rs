//! Reads a Claude Code transcript (`~/.claude/projects/<cwd>/<session>.jsonl`)
//! a piece at a time: each read continues from where the last one stopped.

use anyhow::Result;
use chrono::{DateTime, Local, NaiveDate, Utc};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    io::{Read as _, Seek as _, SeekFrom},
    path::Path,
};

/// How many recent tool calls a thread keeps for the details panel.
const ACTIVITY_LEN: usize = 4;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TokenCounts {
    pub input: u64,
    pub cache_write: u64,
    /// The part of `cache_write` cached for an hour, which costs more.
    pub cache_write_long: u64,
    pub cache_read: u64,
    pub output: u64,
}

impl TokenCounts {
    pub fn total(&self) -> u64 {
        self.input + self.cache_write + self.cache_read + self.output
    }

    pub fn add(&mut self, other: &TokenCounts) {
        self.input += other.input;
        self.cache_write += other.cache_write;
        self.cache_write_long += other.cache_write_long;
        self.cache_read += other.cache_read;
        self.output += other.output;
    }
}

/// Tokens and their estimated price, for one day or a whole thread.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Spend {
    pub tokens: TokenCounts,
    pub cost: f64,
}

impl Spend {
    pub fn add(&mut self, other: &Spend) {
        self.tokens.add(&other.tokens);
        self.cost += other.cost;
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub summary: String,
    pub at: DateTime<Utc>,
}

#[derive(Clone, Debug, Default)]
pub struct Transcript {
    offset: u64,
    seen_messages: HashSet<String>,
    pub spend: Spend,
    pub daily: BTreeMap<NaiveDate, Spend>,
    pub model: Option<String>,
    /// The prompt size of the last API call: what the context holds now.
    pub context_tokens: u64,
    pub last_prompt: Option<(String, DateTime<Utc>)>,
    pub activity: VecDeque<ToolCall>,
    /// Tool calls that have no result yet.
    pub open_tools: Vec<ToolCall>,
    /// Tool results in a row that were errors.
    pub error_streak: u32,
    pub last_error: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub last_event_at: Option<DateTime<Utc>>,
}

impl Transcript {
    /// Reads the lines added since the last read. A line that is still being
    /// written (no newline yet) waits for the next read.
    pub fn read_new(&mut self, path: &Path) -> Result<()> {
        let mut file = std::fs::File::open(path)?;
        let length = file.metadata()?.len();
        if length < self.offset {
            *self = Self::default();
        }
        if length == self.offset {
            return Ok(());
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let Some(end) = bytes.iter().rposition(|byte| *byte == b'\n') else {
            return Ok(());
        };
        for line in bytes[..end].split(|byte| *byte == b'\n') {
            if let Ok(value) = serde_json::from_slice::<Value>(line) {
                self.apply(&value);
            }
        }
        self.offset += end as u64 + 1;
        Ok(())
    }

    pub fn apply(&mut self, line: &Value) {
        let at = line
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
            .map(|time| time.with_timezone(&Utc));
        if let Some(at) = at {
            self.started_at.get_or_insert(at);
            if self.last_event_at.is_none_or(|last| at > last) {
                self.last_event_at = Some(at);
            }
        }
        let is_meta = line.get("isMeta").and_then(Value::as_bool) == Some(true);
        match line.get("type").and_then(Value::as_str) {
            Some("assistant") => self.apply_assistant(line, at),
            Some("user") if !is_meta => self.apply_user(line, at),
            _ => {}
        }
    }

    fn apply_assistant(&mut self, line: &Value, at: Option<DateTime<Utc>>) {
        let Some(message) = line.get("message") else {
            return;
        };
        if let Some(model) = message.get("model").and_then(Value::as_str)
            && model != "<synthetic>"
        {
            self.model = Some(model.to_string());
        }
        // Claude Code writes one line per content block, each with the
        // message's full usage, so usage counts once per message ID.
        let message_id = message
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if let Some(usage) = message.get("usage")
            && (message_id.is_empty() || self.seen_messages.insert(message_id.to_string()))
        {
            let read = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
            let tokens = TokenCounts {
                input: read("input_tokens"),
                cache_write: read("cache_creation_input_tokens"),
                cache_write_long: usage
                    .get("cache_creation")
                    .and_then(|split| split.get("ephemeral_1h_input_tokens"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                cache_read: read("cache_read_input_tokens"),
                output: read("output_tokens"),
            };
            let spend = Spend {
                tokens,
                cost: self
                    .model
                    .as_deref()
                    .map_or(0.0, |model| cost(model, &tokens)),
            };
            self.spend.add(&spend);
            if let Some(at) = at {
                self.daily
                    .entry(at.with_timezone(&Local).date_naive())
                    .or_default()
                    .add(&spend);
            }
            self.context_tokens = tokens.input + tokens.cache_write + tokens.cache_read;
        }
        let Some(blocks) = message.get("content").and_then(Value::as_array) else {
            return;
        };
        for block in blocks {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            let Some(id) = block.get("id").and_then(Value::as_str) else {
                continue;
            };
            if self.open_tools.iter().any(|tool| tool.id == id)
                || self.activity.iter().any(|tool| tool.id == id)
            {
                continue;
            }
            let name = block.get("name").and_then(Value::as_str).unwrap_or("Tool");
            let call = ToolCall {
                id: id.to_string(),
                name: name.to_string(),
                summary: summarize_tool(name, block.get("input")),
                at: at.unwrap_or_else(Utc::now),
            };
            self.open_tools.push(call.clone());
            self.activity.push_back(call);
            while self.activity.len() > ACTIVITY_LEN {
                self.activity.pop_front();
            }
        }
    }

    fn apply_user(&mut self, line: &Value, at: Option<DateTime<Utc>>) {
        let Some(content) = line
            .get("message")
            .and_then(|message| message.get("content"))
        else {
            return;
        };
        if let Some(text) = content.as_str() {
            self.record_prompt(text, at);
            return;
        }
        let Some(blocks) = content.as_array() else {
            return;
        };
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("tool_result") => {
                    let id = block
                        .get("tool_use_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    self.open_tools.retain(|tool| tool.id != id);
                    if block.get("is_error").and_then(Value::as_bool) == Some(true) {
                        self.error_streak += 1;
                        self.last_error = Some(first_line(&result_text(block), 160));
                    } else {
                        self.error_streak = 0;
                        self.last_error = None;
                    }
                }
                Some("text") => {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        self.record_prompt(text, at);
                    }
                }
                _ => {}
            }
        }
    }

    /// Command output, hook notes and other wrapped text start with a tag.
    fn record_prompt(&mut self, text: &str, at: Option<DateTime<Utc>>) {
        let text = text.trim();
        if text.is_empty() || text.starts_with('<') || text.starts_with("[Request interrupted") {
            return;
        }
        self.last_prompt = Some((text.to_string(), at.unwrap_or_else(Utc::now)));
        self.error_streak = 0;
    }

    pub fn open_tool(&self) -> Option<&ToolCall> {
        self.open_tools.last()
    }
}

fn result_text(block: &Value) -> String {
    match block.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

fn first_line(text: &str, max_chars: usize) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    if line.chars().count() <= max_chars {
        return line.to_string();
    }
    let cut: String = line.chars().take(max_chars).collect();
    format!("{cut}…")
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// A short line for a tool call, like "Bash pnpm test" or "Edit panel.rs".
pub fn summarize_tool(name: &str, input: Option<&Value>) -> String {
    let field = |key: &str| {
        input
            .and_then(|input| input.get(key))
            .and_then(Value::as_str)
            .unwrap_or_default()
    };
    let detail = match name {
        "Bash" => first_line(field("command"), 80),
        "Read" | "Edit" | "Write" | "MultiEdit" => file_name(field("file_path")).to_string(),
        "NotebookEdit" => file_name(field("notebook_path")).to_string(),
        "Grep" | "Glob" => first_line(field("pattern"), 60),
        "WebSearch" => first_line(field("query"), 60),
        "WebFetch" => first_line(field("url"), 60),
        "Agent" | "Task" => first_line(field("description"), 60),
        "Skill" => field("skill").to_string(),
        "AskUserQuestion" => return "Question".to_string(),
        _ => String::new(),
    };
    let name = match name.strip_prefix("mcp__") {
        Some(rest) => rest.rsplit("__").next().unwrap_or(rest),
        None => name,
    };
    if detail.is_empty() {
        name.to_string()
    } else {
        format!("{name} {detail}")
    }
}

/// API prices in dollars per million tokens: input, output and cache read.
/// A cache write costs 1.25 times the input price, or 2 times for the
/// one-hour cache that Claude Code uses. Claude Code on a subscription does
/// not pay these, so the result is an estimate.
fn prices(model: &str) -> Option<(f64, f64, f64)> {
    let model = model.trim_start_matches("claude-");
    let model = model.split('[').next().unwrap_or(model);
    let table: &[(&str, (f64, f64, f64))] = &[
        ("fable-5-2", (8.0, 40.0, 0.2)),
        ("mythos-5-2", (8.0, 40.0, 0.2)),
        ("fable-5", (10.0, 50.0, 1.0)),
        ("mythos-5", (10.0, 50.0, 1.0)),
        ("opus-5-5", (4.0, 20.0, 0.2)),
        ("opus-5", (5.0, 25.0, 0.5)),
        ("opus-4", (5.0, 25.0, 0.5)),
        ("sonnet-5-5", (2.0, 10.0, 0.1)),
        ("sonnet-5", (2.0, 10.0, 0.2)),
        ("sonnet-4", (3.0, 15.0, 0.3)),
        ("haiku-5-5", (0.1, 0.5, 0.01)),
        ("haiku-4", (1.0, 5.0, 0.1)),
    ];
    table
        .iter()
        .find(|(prefix, _)| model.starts_with(prefix))
        .map(|(_, prices)| *prices)
}

pub fn cost(model: &str, tokens: &TokenCounts) -> f64 {
    let Some((input, output, cache_read)) = prices(model) else {
        return 0.0;
    };
    let per_token = |count: u64, price: f64| count as f64 * price / 1_000_000.0;
    let long_writes = tokens.cache_write_long.min(tokens.cache_write);
    per_token(tokens.input, input)
        + per_token(tokens.cache_write - long_writes, input * 1.25)
        + per_token(long_writes, input * 2.0)
        + per_token(tokens.cache_read, cache_read)
        + per_token(tokens.output, output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn assistant(id: &str, at: &str, content: Value, usage: Value) -> Value {
        json!({
            "type": "assistant",
            "timestamp": at,
            "message": {"id": id, "model": "claude-opus-5-5", "content": content, "usage": usage},
        })
    }

    fn usage(cache_read: u64, output: u64) -> Value {
        json!({"input_tokens": 2, "cache_creation_input_tokens": 100, "cache_read_input_tokens": cache_read, "output_tokens": output})
    }

    #[test]
    fn counts_usage_once_per_message() {
        let mut transcript = Transcript::default();
        let thinking = json!([{"type": "thinking", "thinking": ""}]);
        let text = json!([{"type": "text", "text": "hi"}]);
        transcript.apply(&assistant(
            "msg_1",
            "2026-11-14T08:00:00Z",
            thinking,
            usage(1000, 50),
        ));
        transcript.apply(&assistant(
            "msg_1",
            "2026-11-14T08:00:01Z",
            text,
            usage(1000, 50),
        ));
        transcript.apply(&assistant(
            "msg_2",
            "2026-11-14T08:01:00Z",
            json!([]),
            usage(2000, 10),
        ));

        assert_eq!(
            transcript.spend.tokens.total(),
            (2 + 100 + 1000 + 50) + (2 + 100 + 2000 + 10)
        );
        assert_eq!(transcript.context_tokens, 2 + 100 + 2000);
        assert_eq!(transcript.model.as_deref(), Some("claude-opus-5-5"));
        assert!(transcript.spend.cost > 0.0);
    }

    #[test]
    fn tracks_open_tools_errors_and_prompts() {
        let mut transcript = Transcript::default();
        transcript.apply(&json!({"type": "user", "timestamp": "2026-11-14T08:00:00Z", "message": {"content": "fix the flaky test"}}));
        transcript
            .apply(&json!({"type": "user", "isMeta": true, "message": {"content": "skill text"}}));
        transcript.apply(
            &json!({"type": "user", "message": {"content": "<command-name>/clear</command-name>"}}),
        );
        transcript.apply(&assistant(
            "msg_1",
            "2026-11-14T08:00:05Z",
            json!([{"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "pnpm test:e2e\necho done"}}]),
            usage(0, 1),
        ));
        assert_eq!(
            transcript.open_tool().map(|tool| tool.summary.as_str()),
            Some("Bash pnpm test:e2e")
        );
        assert_eq!(
            transcript
                .last_prompt
                .as_ref()
                .map(|(text, _)| text.as_str()),
            Some("fix the flaky test")
        );

        for id in ["t1", "t2", "t3"] {
            if id != "t1" {
                transcript.apply(&assistant(
                    id,
                    "2026-11-14T08:00:06Z",
                    json!([{"type": "tool_use", "id": id, "name": "Edit", "input": {"file_path": "/a/panel.rs"}}]),
                    usage(0, 1),
                ));
            }
            transcript.apply(&json!({"type": "user", "message": {"content": [
                {"type": "tool_result", "tool_use_id": id, "is_error": true, "content": "old_string not found\nmore"}
            ]}}));
        }
        assert!(transcript.open_tools.is_empty());
        assert_eq!(transcript.error_streak, 3);
        assert_eq!(
            transcript.last_error.as_deref(),
            Some("old_string not found")
        );
        assert_eq!(
            transcript.activity.back().map(|tool| tool.summary.as_str()),
            Some("Edit panel.rs")
        );
    }

    #[test]
    fn reads_only_finished_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let first = assistant("msg_1", "2026-11-14T08:00:00Z", json!([]), usage(10, 1));
        let second = assistant("msg_2", "2026-11-14T08:00:01Z", json!([]), usage(10, 1));
        std::fs::write(&path, format!("{first}\n{}", &second.to_string()[..20])).unwrap();

        let mut transcript = Transcript::default();
        transcript.read_new(&path).unwrap();
        assert_eq!(transcript.seen_messages.len(), 1);

        std::fs::write(&path, format!("{first}\n{second}\n")).unwrap();
        transcript.read_new(&path).unwrap();
        assert_eq!(transcript.seen_messages.len(), 2);
    }

    #[test]
    fn names_tools() {
        assert_eq!(summarize_tool("mcp__linear__get_issue", None), "get_issue");
        assert_eq!(
            summarize_tool("Read", Some(&json!({"file_path": "/x/SkillsService.ts"}))),
            "Read SkillsService.ts"
        );
    }
}
