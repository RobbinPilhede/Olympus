use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use super::sink::{Ctx, Sink, Tokens, INTERRUPTED};
use super::tracker::{ToolOutcome, Tracker};
use crate::math::row_id;
use crate::text::{command_key, has_nonzero_exit};

const PROVIDER: &str = "codex";
const UNKNOWN_MODEL: &str = "unknown";
const DENIAL_MARKERS: &[&str] = &["rejected by user", "denied by", "approval denied"];

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Totals {
    input: u64,
    cached: u64,
    cache_write: u64,
    output: u64,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub(super) struct CodexState {
    session: String,
    cwd: String,
    model: String,
    subagent: bool,
    meta_seen: bool,
    totals: Totals,
    tracker: Tracker,
}

#[derive(Debug, Clone)]
struct RateReading {
    used_percent: f64,
    window_minutes: u64,
    resets_at: i64,
    timestamp: String,
}

pub(super) struct CodexParser {
    pub(super) state: CodexState,
    latest_rate: Option<RateReading>,
}

#[derive(Deserialize)]
struct Envelope<'a> {
    #[serde(rename = "type")]
    kind: Option<String>,
    timestamp: Option<String>,
    #[serde(borrow)]
    payload: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct SessionMeta<'a> {
    id: Option<String>,
    cwd: Option<String>,
    parent_thread_id: Option<String>,
    #[serde(borrow)]
    source: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct TurnContext {
    model: Option<String>,
    cwd: Option<String>,
}

#[derive(Deserialize)]
struct Item<'a> {
    #[serde(rename = "type")]
    kind: Option<String>,
    id: Option<String>,
    role: Option<String>,
    name: Option<String>,
    namespace: Option<String>,
    call_id: Option<String>,
    arguments: Option<String>,
    input: Option<String>,
    #[serde(borrow)]
    content: Option<&'a RawValue>,
    #[serde(borrow)]
    output: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct TextBlock {
    text: Option<String>,
}

#[derive(Deserialize)]
struct Event {
    #[serde(rename = "type")]
    kind: Option<String>,
    reason: Option<String>,
    turn_id: Option<String>,
    info: Option<TokenInfo>,
    rate_limits: Option<RateLimits>,
}

#[derive(Deserialize)]
struct TokenInfo {
    total_token_usage: Option<TokenUsage>,
}

#[derive(Deserialize)]
struct TokenUsage {
    input_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    cache_write_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct RateLimits {
    primary: Option<PrimaryLimit>,
}

#[derive(Deserialize)]
struct PrimaryLimit {
    used_percent: Option<f64>,
    window_minutes: Option<u64>,
    resets_at: Option<i64>,
}

fn joined_text(raw: &RawValue) -> String {
    if let Ok(text) = serde_json::from_str::<String>(raw.get()) {
        return text;
    }
    serde_json::from_str::<Vec<TextBlock>>(raw.get())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|b| b.text)
        .collect::<Vec<_>>()
        .join("\n")
}

// Exec tool input is JavaScript such as `tools.exec_command({cmd:"cargo test",...})`.
fn exec_command(script: &str) -> Option<String> {
    let at = script.find("cmd:")?;
    let rest = script[at + 4..].trim_start();
    let quote = rest
        .chars()
        .next()
        .filter(|c| matches!(c, '"' | '\'' | '`'))?;
    let body = &rest[quote.len_utf8()..];
    let mut escaped = false;
    for (index, c) in body.char_indices() {
        if escaped {
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == quote {
            let literal = &body[..index];
            return Some(
                serde_json::from_str::<String>(&format!("\"{literal}\""))
                    .unwrap_or_else(|_| literal.to_owned()),
            );
        }
    }
    None
}

fn patched_files(script: &str) -> impl Iterator<Item = &str> {
    const MARKERS: &[&str] = &["*** Update File: ", "*** Add File: "];
    script.lines().filter_map(|line| {
        MARKERS
            .iter()
            .find_map(|m| line.trim_start().strip_prefix(m))
            .map(str::trim)
    })
}

// Exec results are JSON per line with the command's stdout under `output`.
fn readable_output(text: &str) -> String {
    #[derive(Deserialize)]
    struct ExecResult {
        output: Option<String>,
    }
    let lines = text.lines().map(|line| {
        match serde_json::from_str::<ExecResult>(line)
            .ok()
            .and_then(|r| r.output)
        {
            Some(output) => output,
            None => line.to_owned(),
        }
    });
    lines.collect::<Vec<_>>().join("\n")
}

fn classify_output(text: &str) -> ToolOutcome<'_> {
    let head: String = text.chars().take(200).collect::<String>().to_lowercase();
    if DENIAL_MARKERS.iter().any(|m| head.contains(m)) {
        ToolOutcome::Denied
    } else if text.starts_with("Script failed") || has_nonzero_exit(text) {
        ToolOutcome::Failed(text)
    } else {
        ToolOutcome::Ok
    }
}

impl CodexParser {
    pub(super) fn new(state: CodexState) -> CodexParser {
        CodexParser {
            state,
            latest_rate: None,
        }
    }

    pub(super) fn line(&mut self, raw: &[u8], sink: &mut Sink) -> Result<()> {
        let Ok(env) = serde_json::from_slice::<Envelope>(raw) else {
            return Ok(());
        };
        let Some(payload) = env.payload else {
            return Ok(());
        };
        let timestamp = env.timestamp.as_deref().unwrap_or("");
        match env.kind.as_deref() {
            Some("session_meta") => self.session_meta(payload),
            Some("turn_context") => self.turn_context(payload),
            Some("response_item") => self.response_item(payload, timestamp, sink)?,
            Some("event_msg") => self.event(payload, timestamp, sink)?,
            _ => {}
        }
        Ok(())
    }

    pub(super) fn finish(&mut self, sink: &mut Sink) -> Result<()> {
        if let Some(rate) = self.latest_rate.take() {
            sink.rate_limit(
                PROVIDER,
                rate.used_percent,
                rate.window_minutes,
                rate.resets_at,
                &rate.timestamp,
            )?;
        }
        Ok(())
    }

    // Spawned threads copy the parent's meta after their own, so only the first one counts.
    fn session_meta(&mut self, payload: &RawValue) {
        if self.state.meta_seen {
            return;
        }
        let Ok(meta) = serde_json::from_str::<SessionMeta>(payload.get()) else {
            return;
        };
        self.state.meta_seen = true;
        self.state.session = meta.id.unwrap_or_default();
        self.state.cwd = meta.cwd.unwrap_or_default();
        let spawned = meta
            .source
            .is_some_and(|s| s.get().contains("\"subagent\""));
        self.state.subagent = spawned || meta.parent_thread_id.is_some();
    }

    fn turn_context(&mut self, payload: &RawValue) {
        if let Ok(turn) = serde_json::from_str::<TurnContext>(payload.get()) {
            if let Some(model) = turn.model {
                self.state.model = model;
            }
            if let Some(cwd) = turn.cwd {
                self.state.cwd = cwd;
            }
        }
    }

    fn response_item(
        &mut self,
        payload: &RawValue,
        timestamp: &str,
        sink: &mut Sink,
    ) -> Result<()> {
        let Ok(item) = serde_json::from_str::<Item>(payload.get()) else {
            return Ok(());
        };
        match item.kind.as_deref() {
            Some("message") => self.message(&item, timestamp, sink),
            Some("function_call") => {
                self.function_call(&item);
                Ok(())
            }
            Some("custom_tool_call") => {
                self.custom_tool_call(&item);
                Ok(())
            }
            Some("function_call_output") | Some("custom_tool_call_output") => {
                self.tool_output(&item, timestamp, sink)
            }
            _ => Ok(()),
        }
    }

    fn message(&mut self, item: &Item, timestamp: &str, sink: &mut Sink) -> Result<()> {
        match item.role.as_deref() {
            Some("assistant") => self.state.tracker.assistant_seen = true,
            Some("user") if !self.state.subagent => {
                let text = item.content.map(joined_text).unwrap_or_default();
                let ctx = Ctx {
                    session: &self.state.session,
                    cwd: &self.state.cwd,
                    timestamp,
                };
                // Forks replay a parent's items with their item ids, which keeps the uid stable.
                let uid = match item.id.as_deref() {
                    Some(id) => id.to_owned(),
                    None => row_id(&[timestamp, &text]).to_string(),
                };
                let after_assistant = self.state.tracker.assistant_seen;
                sink.prompt(ctx, &uid, &text, after_assistant)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn function_call(&mut self, item: &Item) {
        #[derive(Deserialize)]
        struct Args {
            cmd: Option<String>,
            command: Option<String>,
        }
        if matches!(item.namespace.as_deref(), Some("collaboration" | "clock")) {
            return;
        }
        let (Some(id), Some(name)) = (item.call_id.as_deref(), item.name.as_deref()) else {
            return;
        };
        let args = item
            .arguments
            .as_deref()
            .and_then(|a| serde_json::from_str::<Args>(a).ok());
        match args.and_then(|a| a.cmd.or(a.command)) {
            Some(command) => self.state.tracker.call(id, command_key(&command), true),
            None => self.state.tracker.call(id, name.to_owned(), false),
        }
    }

    fn custom_tool_call(&mut self, item: &Item) {
        let (Some(id), Some(name)) = (item.call_id.as_deref(), item.name.as_deref()) else {
            return;
        };
        let input = item.input.as_deref().unwrap_or("");
        for file in patched_files(input) {
            self.state.tracker.edited(file);
        }
        match exec_command(input) {
            Some(command) => self.state.tracker.call(id, command_key(&command), true),
            None => self.state.tracker.call(id, name.to_owned(), false),
        }
    }

    fn tool_output(&mut self, item: &Item, timestamp: &str, sink: &mut Sink) -> Result<()> {
        let Some(id) = item.call_id.as_deref() else {
            return Ok(());
        };
        if !self.state.tracker.is_open(id) {
            return Ok(());
        }
        let text = item.output.map(joined_text).unwrap_or_default();
        let ctx = Ctx {
            session: &self.state.session,
            cwd: &self.state.cwd,
            timestamp,
        };
        let readable;
        let outcome = match classify_output(&text) {
            ToolOutcome::Failed(_) => {
                readable = readable_output(&text);
                ToolOutcome::Failed(&readable)
            }
            other => other,
        };
        self.state.tracker.result(ctx, id, outcome, sink)
    }

    fn event(&mut self, payload: &RawValue, timestamp: &str, sink: &mut Sink) -> Result<()> {
        // task_started and item_completed dominate these lines and carry nothing we keep.
        let Ok(event) = serde_json::from_str::<Event>(payload.get()) else {
            return Ok(());
        };
        match event.kind.as_deref() {
            Some("token_count") => self.token_count(event, timestamp, sink),
            Some("turn_aborted") if event.reason.as_deref() == Some("interrupted") => {
                let ctx = Ctx {
                    session: &self.state.session,
                    cwd: &self.state.cwd,
                    timestamp,
                };
                let uid = event.turn_id.as_deref().unwrap_or(timestamp);
                sink.signal(ctx, INTERRUPTED, uid, "turn", "")
            }
            _ => Ok(()),
        }
    }

    fn token_count(&mut self, event: Event, timestamp: &str, sink: &mut Sink) -> Result<()> {
        if let Some(primary) = event.rate_limits.and_then(|r| r.primary) {
            if let Some(used_percent) = primary.used_percent {
                self.latest_rate = Some(RateReading {
                    used_percent,
                    window_minutes: primary.window_minutes.unwrap_or(0),
                    resets_at: primary.resets_at.unwrap_or(0),
                    timestamp: timestamp.to_owned(),
                });
            }
        }
        let Some(total) = event.info.and_then(|i| i.total_token_usage) else {
            return Ok(());
        };
        let next = Totals {
            input: total.input_tokens.unwrap_or(0),
            cached: total.cached_input_tokens.unwrap_or(0),
            cache_write: total.cache_write_input_tokens.unwrap_or(0),
            output: total.output_tokens.unwrap_or(0),
        };
        let prev = &self.state.totals;
        let cached = next.cached.saturating_sub(prev.cached);
        let cache_write = next.cache_write.saturating_sub(prev.cache_write);
        let input = next.input.saturating_sub(prev.input);
        let output = next.output.saturating_sub(prev.output);
        if input + cached + cache_write + output > 0 {
            let model = match self.state.model.as_str() {
                "" => UNKNOWN_MODEL,
                model => model,
            };
            // Codex input_tokens includes cached and cache-write; the weights expect disjoint.
            let tokens = Tokens {
                input: input.saturating_sub(cached).saturating_sub(cache_write),
                cache_read: cached,
                cache_write,
                output,
            };
            sink.usage_add(&self.state.session, PROVIDER, model, tokens)?;
        }
        self.state.totals = next;
        Ok(())
    }
}
