use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use super::sink::{Ctx, Sink, Tokens, INTERRUPTED};
use super::tracker::{ToolOutcome, Tracker};
use crate::text::command_key;

const PROVIDER: &str = "claude";
const SYNTHETIC_MODEL: &str = "<synthetic>";

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub(super) struct ClaudeState {
    tracker: Tracker,
}

pub(super) struct ClaudeParser {
    pub(super) state: ClaudeState,
    sidechain: bool,
}

// Bulky subtrees are RawValue so unrelated line types cost a scan, not an allocation.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Envelope<'a> {
    #[serde(rename = "type")]
    kind: Option<String>,
    uuid: Option<String>,
    session_id: Option<String>,
    cwd: Option<String>,
    timestamp: Option<String>,
    request_id: Option<String>,
    is_sidechain: Option<bool>,
    is_meta: Option<bool>,
    is_compact_summary: Option<bool>,
    is_visible_in_transcript_only: Option<bool>,
    prompt_source: Option<String>,
    #[serde(borrow)]
    origin: Option<&'a RawValue>,
    #[serde(borrow)]
    message: Option<&'a RawValue>,
    #[serde(borrow)]
    tool_use_result: Option<&'a RawValue>,
    #[serde(borrow)]
    permission_decision: Option<&'a RawValue>,
    tool_denial_kind: Option<String>,
}

#[derive(Deserialize)]
struct AssistantMessage<'a> {
    id: Option<String>,
    model: Option<String>,
    usage: Option<Usage>,
    #[serde(borrow)]
    content: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct Usage {
    input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct UserMessage<'a> {
    #[serde(borrow)]
    content: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct Block<'a> {
    #[serde(rename = "type")]
    kind: Option<String>,
    id: Option<String>,
    name: Option<String>,
    text: Option<String>,
    tool_use_id: Option<String>,
    is_error: Option<bool>,
    #[serde(borrow)]
    input: Option<&'a RawValue>,
    #[serde(borrow)]
    content: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct ToolInput {
    command: Option<String>,
    file_path: Option<String>,
    notebook_path: Option<String>,
}

#[derive(Deserialize)]
struct Interrupted {
    interrupted: Option<bool>,
}

#[derive(Deserialize)]
struct Decision {
    decision: Option<String>,
}

#[derive(Deserialize)]
struct Origin {
    kind: Option<String>,
}

fn blocks(raw: &RawValue) -> Option<Vec<Block<'_>>> {
    serde_json::from_str(raw.get()).ok()
}

fn tool_output(raw: Option<&RawValue>) -> String {
    let Some(raw) = raw else {
        return String::new();
    };
    if let Ok(text) = serde_json::from_str::<String>(raw.get()) {
        return text;
    }
    blocks(raw)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|b| b.text)
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_human(env: &Envelope) -> bool {
    let origin_kind = env
        .origin
        .and_then(|raw| serde_json::from_str::<Origin>(raw.get()).ok())
        .and_then(|o| o.kind);
    !env.is_meta.unwrap_or(false)
        && !env.is_compact_summary.unwrap_or(false)
        && !env.is_visible_in_transcript_only.unwrap_or(false)
        && matches!(origin_kind.as_deref(), None | Some("human"))
        && matches!(
            env.prompt_source.as_deref(),
            None | Some("typed") | Some("queued")
        )
}

impl ClaudeParser {
    pub(super) fn new(state: ClaudeState, sidechain: bool) -> ClaudeParser {
        ClaudeParser { state, sidechain }
    }

    pub(super) fn line(&mut self, raw: &[u8], sink: &mut Sink) -> Result<()> {
        // Malformed or unexpectedly shaped lines are expected in live transcripts.
        let Ok(env) = serde_json::from_slice::<Envelope>(raw) else {
            return Ok(());
        };
        match env.kind.as_deref() {
            Some("assistant") => self.assistant(&env, sink),
            Some("user") => self.user(&env, sink),
            _ => Ok(()),
        }
    }

    fn assistant(&mut self, env: &Envelope, sink: &mut Sink) -> Result<()> {
        let Some(message) = env
            .message
            .and_then(|raw| serde_json::from_str::<AssistantMessage>(raw.get()).ok())
        else {
            return Ok(());
        };
        let tracker = &mut self.state.tracker;
        tracker.assistant_seen = true;
        let model = message.model.as_deref().unwrap_or("unknown");
        let request = env
            .request_id
            .as_deref()
            .or(message.id.as_deref())
            .or(env.uuid.as_deref());
        if let (Some(usage), Some(request)) = (&message.usage, request) {
            if model != SYNTHETIC_MODEL {
                let tokens = Tokens {
                    input: usage.input_tokens.unwrap_or(0),
                    cache_read: usage.cache_read_input_tokens.unwrap_or(0),
                    cache_write: usage.cache_creation_input_tokens.unwrap_or(0),
                    output: usage.output_tokens.unwrap_or(0),
                };
                sink.usage_max(request, PROVIDER, model, tokens)?;
            }
        }
        for block in message.content.and_then(blocks).unwrap_or_default() {
            if block.kind.as_deref() != Some("tool_use") {
                continue;
            }
            let (Some(id), Some(name)) = (block.id.as_deref(), block.name.as_deref()) else {
                continue;
            };
            let input = block
                .input
                .and_then(|raw| serde_json::from_str::<ToolInput>(raw.get()).ok());
            let command = input.as_ref().and_then(|i| i.command.as_deref());
            match command {
                Some(command) => tracker.call(id, command_key(command), true),
                None => tracker.call(id, name.to_owned(), false),
            }
            if let Some(file) = input
                .as_ref()
                .filter(|_| matches!(name, "Edit" | "Write" | "MultiEdit" | "NotebookEdit"))
                .and_then(|i| i.file_path.as_deref().or(i.notebook_path.as_deref()))
            {
                tracker.edited(file);
            }
        }
        Ok(())
    }

    fn user(&mut self, env: &Envelope, sink: &mut Sink) -> Result<()> {
        let Some(content) = env
            .message
            .and_then(|raw| serde_json::from_str::<UserMessage>(raw.get()).ok())
            .and_then(|m| m.content)
        else {
            return Ok(());
        };
        let ctx = Ctx {
            session: env.session_id.as_deref().unwrap_or(""),
            cwd: env.cwd.as_deref().unwrap_or(""),
            timestamp: env.timestamp.as_deref().unwrap_or(""),
        };
        let uid = env.uuid.as_deref().unwrap_or("");
        let parsed = blocks(content);
        if let Some(parsed) = &parsed {
            if parsed
                .iter()
                .any(|b| b.kind.as_deref() == Some("tool_result"))
            {
                return self.tool_results(env, parsed, ctx, sink);
            }
        }
        let text = match parsed {
            Some(parsed) => parsed
                .into_iter()
                .filter_map(|b| b.text)
                .collect::<Vec<_>>()
                .join("\n"),
            None => serde_json::from_str::<String>(content.get()).unwrap_or_default(),
        };
        let sidechain = self.sidechain || env.is_sidechain.unwrap_or(false);
        if sidechain || uid.is_empty() {
            return Ok(());
        }
        if text.starts_with("[Request interrupted") {
            return sink.signal(ctx, INTERRUPTED, uid, "turn", "");
        }
        if is_human(env) {
            let after_assistant = self.state.tracker.assistant_seen;
            sink.prompt(ctx, uid, &text, after_assistant)?;
        }
        Ok(())
    }

    fn tool_results(
        &mut self,
        env: &Envelope,
        results: &[Block],
        ctx: Ctx,
        sink: &mut Sink,
    ) -> Result<()> {
        let rejected = env
            .permission_decision
            .and_then(|raw| serde_json::from_str::<Decision>(raw.get()).ok())
            .and_then(|d| d.decision)
            .is_some_and(|d| d == "reject");
        let denied = rejected || env.tool_denial_kind.is_some();
        // toolUseResult is an object, string or array depending on the tool.
        let interrupted = env
            .tool_use_result
            .and_then(|raw| serde_json::from_str::<Interrupted>(raw.get()).ok())
            .and_then(|r| r.interrupted)
            .unwrap_or(false);
        for block in results {
            let Some(id) = block.tool_use_id.as_deref() else {
                continue;
            };
            if !self.state.tracker.is_open(id) {
                continue;
            }
            let output;
            let outcome = if denied {
                ToolOutcome::Denied
            } else if interrupted {
                ToolOutcome::Interrupted
            } else if block.is_error.unwrap_or(false) {
                output = tool_output(block.content);
                ToolOutcome::Failed(&output)
            } else {
                ToolOutcome::Ok
            };
            self.state.tracker.result(ctx, id, outcome, sink)?;
        }
        Ok(())
    }
}
