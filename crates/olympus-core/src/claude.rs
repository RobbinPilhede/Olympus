use crate::agent::{Access, Profile, TurnRequest};
use crate::event::{classify_error, AgentEvent, LimitInfo, Stop, TurnResult, Usage};
use crate::proc::{scrub_env, stagger, stream_lines, Cancel, Exit};
use anyhow::Result;
use serde_json::Value;
use std::path::Path;
use std::time::Duration;
use tokio::process::Command;

const LEAN_TOOLS: &str = "Bash,Read,Edit,Write,Grep,Glob,WebFetch,WebSearch,ToolSearch";
const READ_ONLY_TOOLS: &str = "Read,Grep,Glob";

/// Builds the `claude` argument list for a turn.
pub fn build_args(req: &TurnRequest, system: &str, mcp_config: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = ["-p", "--output-format", "stream-json", "--verbose", "--include-partial-messages"].map(String::from).to_vec();
    args.extend(["--model".into(), req.target.model.clone()]);
    if let Some(effort) = &req.target.effort {
        args.extend(["--effort".into(), effort.clone()]);
    }
    args.extend(["--permission-mode".into(), "bypassPermissions".into()]);
    match req.access {
        Access::ReadOnly => args.extend(["--tools".into(), READ_ONLY_TOOLS.into()]),
        Access::Full if req.profile == Profile::Lean => args.extend(["--tools".into(), LEAN_TOOLS.into()]),
        Access::Full => {}
    }
    if req.profile == Profile::Lean {
        args.extend(["--setting-sources".into(), "project,local".into(), "--disable-slash-commands".into(), "--strict-mcp-config".into()]);
    }
    if let Some(config) = mcp_config {
        args.extend(["--mcp-config".into(), config.into()]);
    }
    if !system.is_empty() {
        args.extend(["--append-system-prompt".into(), system.into()]);
    }
    if let Some(schema) = &req.schema {
        args.extend(["--json-schema".into(), schema.to_string()]);
    }
    if let Some(session) = &req.resume {
        args.extend(["--resume".into(), session.clone()]);
    }
    if req.ephemeral {
        args.push("--no-session-persistence".into());
    }
    args
}

/// Incremental parser for the `stream-json` protocol.
#[derive(Debug, Default)]
pub struct Parser {
    pub session: Option<String>,
    pub text: String,
    pub usage: Usage,
    pub structured: Option<Value>,
    pub stop: Option<Stop>,
    pub error: Option<String>,
    pub api_key_source: Option<String>,
}

fn tool_detail(name: &str, input: &Value) -> String {
    let key = match name {
        "Bash" => "command",
        "Read" | "Edit" | "Write" => "file_path",
        "Grep" | "Glob" => "pattern",
        "WebFetch" => "url",
        "WebSearch" => "query",
        _ => "description",
    };
    input.get(key).and_then(Value::as_str).unwrap_or("").chars().take(200).collect()
}

fn block_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks.iter().filter_map(|b| b.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

impl Parser {
    /// Consumes one stdout line; returns true once the terminal `result` line arrives.
    pub fn line(&mut self, line: &str, emit: &mut dyn FnMut(AgentEvent)) -> bool {
        let Ok(v) = serde_json::from_str::<Value>(line) else { return false };
        match v.get("type").and_then(Value::as_str) {
            Some("system") => self.system(&v, emit),
            Some("stream_event") => {
                if v.get("parent_tool_use_id").is_some_and(|p| !p.is_null()) {
                    return false;
                }
                let event = &v["event"];
                if event["type"] == "content_block_delta" && event["delta"]["type"] == "text_delta" {
                    if let Some(text) = event["delta"]["text"].as_str() {
                        self.text.push_str(text);
                        emit(AgentEvent::Text(text.to_string()));
                    }
                }
            }
            Some("assistant") => {
                if v.get("parent_tool_use_id").is_some_and(|p| !p.is_null()) {
                    return false;
                }
                for block in v["message"]["content"].as_array().into_iter().flatten() {
                    if block["type"] == "tool_use" {
                        let name = block["name"].as_str().unwrap_or("tool").to_string();
                        emit(AgentEvent::ToolStart {
                            id: block["id"].as_str().unwrap_or("").to_string(),
                            detail: tool_detail(&name, &block["input"]),
                            name,
                        });
                    }
                }
            }
            Some("user") => {
                if v.get("parent_tool_use_id").is_some_and(|p| !p.is_null()) {
                    return false;
                }
                for block in v["message"]["content"].as_array().into_iter().flatten() {
                    if block["type"] == "tool_result" {
                        let summary: String = block_text(block.get("content")).lines().next().unwrap_or("").chars().take(160).collect();
                        emit(AgentEvent::ToolEnd {
                            id: block["tool_use_id"].as_str().unwrap_or("").to_string(),
                            ok: !block["is_error"].as_bool().unwrap_or(false),
                            summary,
                        });
                    }
                }
            }
            Some("rate_limit_event") => self.rate_limit(&v, emit),
            Some("result") => {
                self.result(&v);
                return true;
            }
            _ => {}
        }
        false
    }

    fn system(&mut self, v: &Value, emit: &mut dyn FnMut(AgentEvent)) {
        match v.get("subtype").and_then(Value::as_str) {
            Some("init") => {
                self.api_key_source = v["apiKeySource"].as_str().map(String::from);
                if let Some(id) = v["session_id"].as_str() {
                    self.session = Some(id.to_string());
                    emit(AgentEvent::Session(id.to_string()));
                }
            }
            Some("api_retry") => {
                let attempt = v["attempt"].as_u64().unwrap_or(0);
                emit(AgentEvent::Status(format!("Retrying the API call (attempt {attempt})")));
            }
            Some("compact_boundary") => emit(AgentEvent::Status("Context compacted".into())),
            _ => {}
        }
    }

    fn rate_limit(&mut self, v: &Value, emit: &mut dyn FnMut(AgentEvent)) {
        let info = &v["rate_limit_info"];
        let blocked = info["status"].as_str().is_some_and(|s| s == "rejected");
        let mut sent = false;
        for (window, data) in info["unifiedWindows"].as_object().into_iter().flatten() {
            emit(AgentEvent::Limit(LimitInfo {
                vendor: "claude".into(),
                window: window.clone(),
                used: data["utilization"].as_f64().unwrap_or(0.0),
                resets_at: data["resetsAt"].as_i64(),
                blocked,
            }));
            sent = true;
        }
        // Without a utilization figure, only a rejection says anything; reporting 0% would be made up.
        if !sent && blocked {
            emit(AgentEvent::Limit(LimitInfo {
                vendor: "claude".into(),
                window: info["rateLimitType"].as_str().unwrap_or("unknown").to_string(),
                used: 1.0,
                resets_at: info["resetsAt"].as_i64(),
                blocked,
            }));
        }
    }

    fn result(&mut self, v: &Value) {
        let usage = &v["usage"];
        self.usage = Usage {
            input: usage["input_tokens"].as_u64().unwrap_or(0),
            cache_read: usage["cache_read_input_tokens"].as_u64().unwrap_or(0),
            cache_write: usage["cache_creation_input_tokens"].as_u64().unwrap_or(0),
            output: usage["output_tokens"].as_u64().unwrap_or(0),
        };
        if let Some(id) = v["session_id"].as_str() {
            self.session = Some(id.to_string());
        }
        if let Some(text) = v["result"].as_str() {
            self.text = text.to_string();
        }
        self.structured = v.get("structured_output").filter(|s| !s.is_null()).cloned();
        let success = v["subtype"] == "success" && !v["is_error"].as_bool().unwrap_or(false);
        if success {
            self.stop = Some(Stop::Completed);
        } else {
            let detail = v["result"]
                .as_str()
                .map(String::from)
                .or_else(|| v["errors"].as_array().map(|e| e.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("; ")))
                .unwrap_or_else(|| v["subtype"].as_str().unwrap_or("unknown error").to_string());
            self.stop = Some(classify_error(&detail));
            self.error = Some(detail);
        }
    }
}

/// Runs one Claude Code turn through the unmodified signed-in binary.
pub async fn run(
    exe: &Path,
    req: &TurnRequest,
    system: &str,
    mcp_config: Option<&str>,
    idle: Duration,
    cancel: Cancel,
    emit: &mut (dyn FnMut(AgentEvent) + Send),
) -> Result<TurnResult> {
    stagger("claude", Duration::from_millis(400)).await;
    let mut cmd = Command::new(exe);
    cmd.args(build_args(req, system, mcp_config)).current_dir(&req.cwd);
    scrub_env(&mut cmd);
    cmd.env("OLYMPUS_DEPTH", req.depth.to_string());
    if let Some(dir) = &req.account_dir {
        cmd.env("CLAUDE_CONFIG_DIR", dir);
    }
    let mut parser = Parser::default();
    let done = stream_lines(cmd, req.prompt.clone(), idle, cancel, |line| parser.line(line, emit)).await?;
    Ok(finish(parser, done.exit, &done.stderr_tail))
}

fn finish(parser: Parser, exit: Exit, stderr: &str) -> TurnResult {
    let stop = match (&parser.stop, &exit) {
        (Some(stop), _) => *stop,
        (None, Exit::Cancelled) => Stop::Cancelled,
        (None, Exit::Idle) => Stop::IdleTimeout,
        (None, Exit::Code(_)) => classify_error(stderr),
    };
    let error = parser.error.or_else(|| match stop {
        Stop::Completed => None,
        Stop::Cancelled => Some("Stopped.".into()),
        Stop::IdleTimeout => Some("Claude produced no output before the idle timeout.".into()),
        _ if !stderr.trim().is_empty() => Some(stderr.trim().to_string()),
        _ => Some(format!("Claude exited without a result ({exit:?}).")),
    });
    TurnResult { stop, session: parser.session, text: parser.text, usage: parser.usage, structured: parser.structured, error, run_id: String::new() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::Target;

    fn request() -> TurnRequest {
        TurnRequest::new(Target::parse("claude:opus@high").expect("target"), "hi", ".".into())
    }

    fn feed(lines: &[&str]) -> (Parser, Vec<AgentEvent>) {
        let mut parser = Parser::default();
        let mut events = Vec::new();
        for line in lines {
            parser.line(line, &mut |e| events.push(e));
        }
        (parser, events)
    }

    #[test]
    fn lean_turn_trims_startup_and_keeps_bypass() {
        let args = build_args(&request(), "rules", Some("{}")).join(" ");
        for expected in ["--strict-mcp-config", "--setting-sources project,local", "--disable-slash-commands", "--permission-mode bypassPermissions", "--effort high", "--mcp-config {}", "--append-system-prompt rules"] {
            assert!(args.contains(expected), "missing {expected} in {args}");
        }
        assert!(args.contains("ToolSearch"), "MCP tools are deferred behind ToolSearch");
    }

    #[test]
    fn read_only_turn_cannot_write_or_run_commands() {
        let mut req = request();
        req.access = Access::ReadOnly;
        let args = build_args(&req, "", None);
        let at = args.iter().position(|a| a == "--tools").expect("tools flag");
        assert_eq!(args[at + 1], READ_ONLY_TOOLS);
    }

    #[test]
    fn full_profile_keeps_the_users_setup() {
        let mut req = request();
        req.profile = Profile::Full;
        let args = build_args(&req, "", None).join(" ");
        assert!(!args.contains("--strict-mcp-config") && !args.contains("--tools"));
    }

    #[test]
    fn parses_a_successful_stream() {
        let (parser, events) = feed(&[
            r#"{"type":"system","subtype":"init","session_id":"s1","apiKeySource":"none"}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"po"}}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"a\nb"}]}}"#,
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","unifiedWindows":{"seven_day":{"utilization":0.17,"resetsAt":100}}}}"#,
            r#"{"type":"result","subtype":"success","result":"pong","session_id":"s1","usage":{"input_tokens":2,"cache_read_input_tokens":3,"cache_creation_input_tokens":4,"output_tokens":5}}"#,
        ]);
        assert_eq!(parser.stop, Some(Stop::Completed));
        assert_eq!(parser.text, "pong");
        assert_eq!(parser.usage, Usage { input: 2, cache_read: 3, cache_write: 4, output: 5 });
        assert_eq!(parser.api_key_source.as_deref(), Some("none"));
        assert!(events.contains(&AgentEvent::Text("po".into())));
        assert!(events.contains(&AgentEvent::ToolStart { id: "t1".into(), name: "Bash".into(), detail: "ls".into() }));
        assert!(events.contains(&AgentEvent::ToolEnd { id: "t1".into(), ok: true, summary: "a".into() }));
        assert!(events.iter().any(|e| matches!(e, AgentEvent::Limit(l) if l.window == "seven_day" && l.used == 0.17)));
    }

    #[test]
    fn subagent_chatter_stays_out_of_the_transcript() {
        let (_, events) = feed(&[r#"{"type":"stream_event","parent_tool_use_id":"x","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"inner"}}}"#]);
        assert!(events.is_empty());
    }

    #[test]
    fn error_results_are_not_success() {
        let (parser, _) = feed(&[r#"{"type":"result","subtype":"success","is_error":true,"result":"You've hit your limit"}"#]);
        assert_eq!(parser.stop, Some(Stop::RateLimited));
        let (parser, _) = feed(&[r#"{"type":"result","subtype":"error_during_execution","errors":["boom"]}"#]);
        assert_eq!(parser.stop, Some(Stop::Failed));
        assert_eq!(parser.error.as_deref(), Some("boom"));
    }

    #[test]
    fn a_process_that_dies_without_a_result_fails() {
        let result = finish(Parser::default(), Exit::Code(0), "");
        assert_eq!(result.stop, Stop::Failed);
        assert!(!result.ok());
    }

    #[test]
    fn garbage_lines_are_ignored() {
        let (parser, events) = feed(&["not json", r#"{"type":"future_thing"}"#]);
        assert!(parser.stop.is_none() && events.is_empty());
    }
}
