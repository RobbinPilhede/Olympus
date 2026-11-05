use crate::agent::{Access, Profile, TurnRequest};
use crate::event::{classify_error, AgentEvent, Stop, TurnResult, Usage};
use crate::proc::{scrub_env, stagger, stream_lines, Cancel, Exit};
use anyhow::{Context, Result};
use serde_json::Value;
use std::path::Path;
use std::time::Duration;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::process::Command;

/// Builds the `codex exec` argument list; the prompt is read from stdin.
pub fn build_args(req: &TurnRequest, schema_file: Option<&Path>, mcp_exe: Option<&Path>) -> Vec<String> {
    let mut args: Vec<String> = vec!["exec".into()];
    // On a fresh run this flag takes several values, so a following flag must end it.
    if req.resume.is_none() {
        args.extend(req.images.iter().flat_map(|image| ["--image".to_string(), image.display().to_string()]));
    }
    args.extend(["--json", "--skip-git-repo-check"].map(String::from));
    if req.profile == Profile::Lean {
        args.push("--ignore-user-config".into());
    }
    if !req.target.model.is_empty() {
        args.extend(["-m".into(), req.target.model.clone()]);
    }
    if let Some(effort) = &req.target.effort {
        args.extend(["-c".into(), format!("model_reasoning_effort=\"{effort}\"")]);
    }
    match req.access {
        Access::Full => args.push("--dangerously-bypass-approvals-and-sandbox".into()),
        Access::ReadOnly => {
            args.extend(["-s".into(), "read-only".into(), "-c".into(), "approval_policy=\"never\"".into()]);
            // Without a sandbox mode, Windows Codex rejects every command, even reads.
            #[cfg(windows)]
            args.extend(["-c".into(), "windows.sandbox=\"unelevated\"".into()]);
        }
    }
    if let Some(exe) = mcp_exe {
        args.extend(["-c".into(), format!("mcp_servers.olympus.command='{}'", exe.display())]);
        args.extend(["-c".into(), "mcp_servers.olympus.args=[\"mcp\"]".into()]);
    }
    if let Some(file) = schema_file {
        args.extend(["--output-schema".into(), file.display().to_string()]);
    }
    if req.ephemeral {
        args.push("--ephemeral".into());
    }
    if let Some(session) = &req.resume {
        args.extend(["resume".into(), session.clone()]);
        args.extend(req.images.iter().flat_map(|image| ["--image".to_string(), image.display().to_string()]));
    }
    args.push("-".into());
    args
}

/// Incremental parser for `codex exec --json`.
#[derive(Debug, Default)]
pub struct Parser {
    pub session: Option<String>,
    pub text: String,
    pub usage: Usage,
    pub stop: Option<Stop>,
    pub error: Option<String>,
}

fn item_start(item: &Value) -> Option<(String, String)> {
    match item["type"].as_str()? {
        "command_execution" => Some(("Shell".into(), item["command"].as_str().unwrap_or("").chars().take(200).collect())),
        "file_change" => {
            let paths: Vec<&str> = item["changes"].as_array().into_iter().flatten().filter_map(|c| c["path"].as_str()).collect();
            Some(("Edit".into(), paths.join(", ")))
        }
        "mcp_tool_call" => Some((format!("{}.{}", item["server"].as_str().unwrap_or("mcp"), item["tool"].as_str().unwrap_or("tool")), String::new())),
        "web_search" => Some(("WebSearch".into(), item["query"].as_str().unwrap_or("").to_string())),
        _ => None,
    }
}

impl Parser {
    /// Consumes one stdout line; returns true once the turn has ended.
    pub fn line(&mut self, line: &str, emit: &mut dyn FnMut(AgentEvent)) -> bool {
        let Ok(v) = serde_json::from_str::<Value>(line) else { return false };
        match v.get("type").and_then(Value::as_str) {
            Some("thread.started") => {
                if let Some(id) = v["thread_id"].as_str() {
                    self.session = Some(id.to_string());
                    emit(AgentEvent::Session(id.to_string()));
                }
            }
            Some("item.started") => {
                let item = &v["item"];
                if let Some((name, detail)) = item_start(item) {
                    emit(AgentEvent::ToolStart { id: item["id"].as_str().unwrap_or("").to_string(), name, detail });
                }
            }
            Some("item.completed") => self.completed(&v["item"], emit),
            Some("turn.completed") => {
                let usage = &v["usage"];
                let cached = usage["cached_input_tokens"].as_u64().unwrap_or(0);
                self.usage = Usage {
                    input: usage["input_tokens"].as_u64().unwrap_or(0).saturating_sub(cached),
                    cache_read: cached,
                    cache_write: usage["cache_write_input_tokens"].as_u64().unwrap_or(0),
                    output: usage["output_tokens"].as_u64().unwrap_or(0),
                };
                self.stop = Some(Stop::Completed);
                return true;
            }
            Some("turn.failed") => {
                self.fail(v["error"]["message"].as_str().unwrap_or("Codex reported a failed turn."));
                return true;
            }
            Some("error") => {
                let message = v["message"].as_str().unwrap_or("Codex reported an error.").to_string();
                if message.contains("Reconnecting") {
                    emit(AgentEvent::Status(message));
                } else {
                    self.error = Some(message);
                }
            }
            _ => {}
        }
        false
    }

    fn fail(&mut self, message: &str) {
        self.stop = Some(classify_error(message));
        self.error = Some(message.to_string());
    }

    fn completed(&mut self, item: &Value, emit: &mut dyn FnMut(AgentEvent)) {
        let id = item["id"].as_str().unwrap_or("").to_string();
        match item["type"].as_str() {
            Some("agent_message") => {
                if let Some(text) = item["text"].as_str() {
                    if !self.text.is_empty() {
                        self.text.push_str("\n\n");
                        emit(AgentEvent::Text("\n\n".into()));
                    }
                    self.text.push_str(text);
                    emit(AgentEvent::Text(text.to_string()));
                }
            }
            Some("command_execution") => {
                let code = item["exit_code"].as_i64();
                let first: String = item["aggregated_output"].as_str().unwrap_or("").lines().next().unwrap_or("").chars().take(160).collect();
                emit(AgentEvent::ToolEnd { id, ok: code == Some(0), summary: first });
            }
            Some("file_change") => emit(AgentEvent::ToolEnd { id, ok: item["status"] != "failed", summary: String::new() }),
            Some("mcp_tool_call") => emit(AgentEvent::ToolEnd { id, ok: item["status"] != "failed", summary: String::new() }),
            Some("web_search") => emit(AgentEvent::ToolEnd { id, ok: true, summary: String::new() }),
            Some("error") => {
                if let Some(message) = item["message"].as_str() {
                    emit(AgentEvent::Status(message.chars().take(200).collect()));
                }
            }
            _ => {}
        }
    }
}

/// Runs one Codex turn through the official signed-in binary.
pub async fn run(
    exe: &Path,
    req: &TurnRequest,
    system: &str,
    mcp_exe: Option<&Path>,
    idle: Duration,
    cancel: Cancel,
    emit: &mut (dyn FnMut(AgentEvent) + Send),
) -> Result<TurnResult> {
    stagger("codex", Duration::from_millis(400)).await;
    let schema_file = match &req.schema {
        Some(schema) => {
            let dir = crate::config::home_dir().join("tmp");
            tokio::fs::create_dir_all(&dir).await.context("creating schema directory")?;
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = dir.join(format!("schema-{}-{}.json", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            tokio::fs::write(&path, schema.to_string()).await.context("writing output schema")?;
            Some(path)
        }
        None => None,
    };
    let mut cmd = Command::new(exe);
    cmd.args(build_args(req, schema_file.as_deref(), mcp_exe)).current_dir(&req.cwd);
    scrub_env(&mut cmd);
    cmd.env("OLYMPUS_DEPTH", req.depth.to_string());
    if let Some(dir) = &req.account_dir {
        cmd.env("CODEX_HOME", dir);
    }
    let prompt = if system.is_empty() || req.resume.is_some() { req.prompt.clone() } else { format!("<instructions>\n{system}\n</instructions>\n\n{}", req.prompt) };
    let mut parser = Parser::default();
    let done = stream_lines(cmd, prompt, idle, cancel, |line| parser.line(line, emit)).await?;
    if let Some(path) = schema_file {
        let _ = tokio::fs::remove_file(path).await;
    }
    Ok(finish(parser, done.exit, &done.stderr_tail, req.schema.is_some()))
}

fn finish(parser: Parser, exit: Exit, stderr: &str, wants_structured: bool) -> TurnResult {
    let stop = match (&parser.stop, &exit) {
        (Some(stop), _) => *stop,
        (None, Exit::Cancelled) => Stop::Cancelled,
        (None, Exit::Idle) => Stop::IdleTimeout,
        (None, Exit::Code(_)) => classify_error(parser.error.as_deref().unwrap_or(stderr)),
    };
    let error = match stop {
        Stop::Completed => None,
        Stop::Cancelled => Some("Stopped.".into()),
        Stop::IdleTimeout => Some("Codex produced no output before the idle timeout.".into()),
        _ => parser.error.or_else(|| Some(stderr.trim().chars().take(600).collect::<String>())).filter(|e| !e.is_empty()).or_else(|| Some(format!("Codex exited without finishing ({exit:?})."))),
    };
    let structured = if wants_structured { serde_json::from_str(&parser.text).ok() } else { None };
    TurnResult { stop, session: parser.session, text: parser.text, usage: parser.usage, structured, error, run_id: String::new() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attached_pictures_reach_codex_without_swallowing_other_arguments() {
        let target = crate::target::Target::parse("codex:gpt").expect("target");
        let prompt = "Look\n\nAttached: C:/a/shot.png\nAttached: C:/a/notes.txt";
        let mut req = TurnRequest::new(target, prompt, std::path::PathBuf::from("."));
        let fresh = build_args(&req, None, None);
        assert_eq!(fresh[..4], ["exec", "--image", "C:/a/shot.png", "--json"], "{fresh:?}");
        assert_eq!(fresh.iter().filter(|arg| *arg == "--image").count(), 1, "text files are read by path");
        req.resume = Some("s1".into());
        let resumed = build_args(&req, None, None);
        let at = resumed.iter().position(|arg| arg == "resume").expect("resume");
        assert_eq!(resumed[at..], ["resume", "s1", "--image", "C:/a/shot.png", "-"], "{resumed:?}");
    }
    use crate::target::Target;

    fn request() -> TurnRequest {
        TurnRequest::new(Target::parse("codex:gpt-6.1-sol@high").expect("target"), "hi", ".".into())
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
    fn resume_flags_precede_the_subcommand_and_stdin_marker_is_last() {
        let mut req = request();
        req.resume = Some("abc".into());
        let args = build_args(&req, None, None);
        let resume = args.iter().position(|a| a == "resume").expect("resume");
        assert_eq!(args[resume + 1], "abc");
        assert_eq!(args.last().map(String::as_str), Some("-"));
        assert!(args.iter().position(|a| a == "--json").expect("json") < resume);
    }

    #[test]
    fn read_only_never_bypasses_the_sandbox() {
        let mut req = request();
        req.access = Access::ReadOnly;
        let args = build_args(&req, None, None).join(" ");
        assert!(args.contains("-s read-only"));
        assert!(!args.contains("--dangerously-bypass"));
        #[cfg(windows)]
        assert!(args.contains("windows.sandbox=\"unelevated\""), "without this Codex can't read either");
    }

    #[test]
    fn parses_a_successful_turn_and_subtracts_cached_input() {
        let (parser, events) = feed(&[
            r#"{"type":"thread.started","thread_id":"t1"}"#,
            r#"{"type":"item.started","item":{"id":"i1","type":"command_execution","command":"ls"}}"#,
            r#"{"type":"item.completed","item":{"id":"i1","type":"command_execution","exit_code":1,"aggregated_output":"nope"}}"#,
            r#"{"type":"item.completed","item":{"id":"i2","type":"agent_message","text":"pong"}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":80,"output_tokens":5}}"#,
        ]);
        assert_eq!(parser.stop, Some(Stop::Completed));
        assert_eq!(parser.session.as_deref(), Some("t1"));
        assert_eq!(parser.usage, Usage { input: 20, cache_read: 80, cache_write: 0, output: 5 });
        assert!(events.contains(&AgentEvent::ToolEnd { id: "i1".into(), ok: false, summary: "nope".into() }));
        assert!(events.contains(&AgentEvent::Text("pong".into())));
    }

    #[test]
    fn failed_turn_is_classified() {
        let (parser, _) = feed(&[r#"{"type":"turn.failed","error":{"message":"You've hit your usage limit"}}"#]);
        assert_eq!(parser.stop, Some(Stop::RateLimited));
    }

    #[test]
    fn exit_without_turn_completed_fails_even_with_code_zero() {
        let result = finish(Parser::default(), Exit::Code(0), "", false);
        assert_eq!(result.stop, Stop::Failed);
    }

    #[test]
    fn structured_output_comes_from_the_final_message() {
        let mut parser = Parser { text: "{\"a\":1}".into(), stop: Some(Stop::Completed), ..Parser::default() };
        parser.usage.input = 1;
        let result = finish(parser, Exit::Code(0), "", true);
        assert_eq!(result.structured.expect("json")["a"], 1);
    }
}
