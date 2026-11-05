use crate::agent::{Access, TurnRequest};
use crate::config::home_dir;
use crate::event::{classify_error, AgentEvent, Stop, TurnResult, Usage};
use crate::proc::Cancel;
use crate::runtime::Runtime;
use crate::target::Provider;
use crate::{node, tools};
use anyhow::{Context, Result};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

const MAX_STEPS: usize = 60;

/// Installs the ring TLS provider once; reqwest is built without a default.
pub fn ensure_tls() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// A client shared by every OpenAI-compatible call.
pub fn client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    ensure_tls();
    CLIENT.get_or_init(|| reqwest::Client::builder().connect_timeout(Duration::from_secs(15)).build().unwrap_or_default()).clone()
}

/// Accumulates one streamed assistant message.
#[derive(Debug, Default)]
pub struct Assembler {
    pub text: String,
    pub calls: Vec<ToolCall>,
    pub usage: Option<Usage>,
    pub finish: Option<String>,
}

/// A tool call assembled from stream fragments.
#[derive(Debug, Default, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

impl Assembler {
    /// Applies one SSE `data:` payload, calling `on_text` for visible text.
    pub fn apply(&mut self, data: &str, on_text: &mut (dyn FnMut(&str) + Send)) -> Result<(), String> {
        let Ok(v) = serde_json::from_str::<Value>(data) else { return Ok(()) };
        if let Some(message) = v["error"]["message"].as_str() {
            return Err(message.to_string());
        }
        if let Some(usage) = v.get("usage").filter(|u| u.is_object()) {
            let cached = usage["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0);
            self.usage = Some(Usage {
                input: usage["prompt_tokens"].as_u64().unwrap_or(0).saturating_sub(cached),
                cache_read: cached,
                cache_write: 0,
                output: usage["completion_tokens"].as_u64().unwrap_or(0),
            });
        }
        let choice = &v["choices"][0];
        if let Some(reason) = choice["finish_reason"].as_str() {
            self.finish = Some(reason.to_string());
        }
        let delta = &choice["delta"];
        if let Some(text) = delta["content"].as_str() {
            if !text.is_empty() {
                self.text.push_str(text);
                on_text(text);
            }
        }
        for call in delta["tool_calls"].as_array().into_iter().flatten() {
            let index = call["index"].as_u64().unwrap_or(0) as usize;
            if self.calls.len() <= index {
                self.calls.resize(index + 1, ToolCall::default());
            }
            let slot = &mut self.calls[index];
            if let Some(id) = call["id"].as_str() {
                slot.id = id.to_string();
            }
            if let Some(name) = call["function"]["name"].as_str() {
                slot.name.push_str(name);
            }
            if let Some(arguments) = call["function"]["arguments"].as_str() {
                slot.arguments.push_str(arguments);
            }
        }
        Ok(())
    }
}

/// Splits complete `data:` lines out of an SSE byte buffer.
pub fn drain_sse(buffer: &mut Vec<u8>) -> Vec<String> {
    let mut out = Vec::new();
    while let Some(end) = buffer.iter().position(|b| *b == b'\n') {
        let line: Vec<u8> = buffer.drain(..=end).collect();
        let text = String::from_utf8_lossy(&line);
        if let Some(data) = text.trim().strip_prefix("data:") {
            let data = data.trim();
            if !data.is_empty() && data != "[DONE]" {
                out.push(data.to_string());
            }
        }
    }
    out
}

struct Endpoint {
    url: String,
    key: String,
    model: String,
}

async fn endpoint(rt: &Runtime, req: &TurnRequest) -> Result<Endpoint, String> {
    match &req.target.provider {
        Provider::OpenRouter => {
            let key = rt.config.openrouter_key().ok_or("No OpenRouter key. Set OPENROUTER_API_KEY or openrouter_key in ~/.olympus/config.toml.")?;
            Ok(Endpoint { url: "https://openrouter.ai/api/v1/chat/completions".into(), key, model: req.target.model.clone() })
        }
        Provider::Node(name) => {
            let base = node::address(&rt.config, name).await.ok_or_else(|| format!("Node '{name}' didn't answer. Check it's running on this network."))?;
            Ok(Endpoint { url: format!("http://{base}/v1/chat/completions"), key: rt.config.node_token.clone(), model: req.target.model.clone() })
        }
        _ => Err("This provider doesn't use the OpenAI protocol.".into()),
    }
}

fn history_path(session: &str) -> PathBuf {
    home_dir().join("oa-sessions").join(format!("{session}.json"))
}

/// Reads a session's history; a turn that failed before saving leaves none, so that starts fresh.
async fn load_history(path: &std::path::Path) -> Result<Option<Vec<Value>>> {
    match tokio::fs::read_to_string(path).await {
        Ok(text) => Ok(Some(serde_json::from_str(&text).context("parsing session history")?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

// Pictures go inline, since these models can't open a file by path.
fn user_content(text: &str, images: &[PathBuf]) -> Value {
    use base64::Engine as _;
    let mut parts = vec![json!({"type": "text", "text": text})];
    for image in images {
        let Ok(bytes) = std::fs::read(image) else { continue };
        let kind = image.extension().map_or("png".to_string(), |ext| ext.to_string_lossy().to_ascii_lowercase().replace("jpg", "jpeg"));
        let data = base64::engine::general_purpose::STANDARD.encode(bytes);
        parts.push(json!({"type": "image_url", "image_url": {"url": format!("data:image/{kind};base64,{data}")}}));
    }
    if parts.len() == 1 {
        return json!(text);
    }
    json!(parts)
}

fn new_session() -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("oa-{now:x}")
}

fn http_error(status: &str, body: &str) -> String {
    if body.contains("model_not_ready") {
        return "No model is loaded on this node. Run /load to choose one.".to_string();
    }
    format!("{status}: {}", body.chars().take(500).collect::<String>())
}

async fn stream_once(endpoint: &Endpoint, body: &Value, idle: Duration, cancel: &mut Cancel, on_text: &mut (dyn FnMut(&str) + Send)) -> Result<Assembler, (Stop, String)> {
    let request = client().post(&endpoint.url).bearer_auth(&endpoint.key).json(body).send();
    let response = tokio::select! {
        r = request => r.map_err(|e| (Stop::Failed, format!("Request failed: {e}")))?,
        _ = cancel.changed() => return Err((Stop::Cancelled, "Stopped.".into())),
    };
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        let detail = http_error(status.as_str(), &text);
        return Err((classify_error(&detail), detail));
    }
    let mut stream = response.bytes_stream();
    let mut buffer = Vec::new();
    let mut assembler = Assembler::default();
    loop {
        tokio::select! {
            _ = cancel.changed() => return Err((Stop::Cancelled, "Stopped.".into())),
            chunk = tokio::time::timeout(idle, stream.next()) => match chunk {
                Err(_) => return Err((Stop::IdleTimeout, "The model stopped responding.".into())),
                Ok(None) => return Ok(assembler),
                Ok(Some(Err(err))) => return Err((Stop::Failed, format!("Stream failed: {err}"))),
                Ok(Some(Ok(bytes))) => {
                    buffer.extend_from_slice(&bytes);
                    for data in drain_sse(&mut buffer) {
                        assembler.apply(&data, on_text).map_err(|m| (classify_error(&m), m))?;
                    }
                }
            }
        }
    }
}

/// Runs a full agent loop against an OpenAI-compatible endpoint.
pub async fn run(rt: &Runtime, req: &TurnRequest, system: &str, idle: Duration, mut cancel: Cancel, emit: &mut (dyn FnMut(AgentEvent) + Send)) -> Result<TurnResult> {
    let endpoint = match endpoint(rt, req).await {
        Ok(endpoint) => endpoint,
        Err(message) => return Ok(TurnResult::failed(Stop::Failed, message)),
    };
    let read_only = req.access == Access::ReadOnly;
    let session = req.resume.clone().unwrap_or_else(new_session);
    emit(AgentEvent::Session(session.clone()));
    let path = history_path(&session);
    let saved = if req.resume.is_some() { load_history(&path).await? } else { None };
    let mut messages: Vec<Value> = match saved {
        Some(saved) => saved,
        None if system.is_empty() => Vec::new(),
        None => vec![json!({"role": "system", "content": system})],
    };
    let schema_note = req.schema.as_ref().map(|s| format!("\n\nReply with only JSON matching this schema:\n{s}")).unwrap_or_default();
    messages.push(json!({"role": "user", "content": user_content(&format!("{}{schema_note}", req.prompt), &req.images)}));

    let mut usage = Usage::default();
    let mut transcript = String::new();
    let mut last_text = String::new();
    for _ in 0..MAX_STEPS {
        let mut body = json!({"model": endpoint.model, "messages": messages, "stream": true, "stream_options": {"include_usage": true}, "tools": tools::definitions(read_only)});
        if let Some(effort) = &req.target.effort {
            body["reasoning_effort"] = json!(effort);
        }
        let mut on_text = |text: &str| emit(AgentEvent::Text(text.to_string()));
        let step = match stream_once(&endpoint, &body, idle, &mut cancel, &mut on_text).await {
            Ok(step) => step,
            Err((stop, message)) => {
                return Ok(TurnResult { stop, session: Some(session), text: transcript, usage, structured: None, error: Some(message), run_id: String::new() });
            }
        };
        if let Some(step_usage) = &step.usage {
            usage.add(step_usage);
        }
        if !step.text.is_empty() {
            if !transcript.is_empty() {
                transcript.push('\n');
            }
            transcript.push_str(step.text.trim());
            last_text = step.text.clone();
        }
        let calls: Vec<ToolCall> = step.calls.into_iter().filter(|c| !c.name.is_empty()).collect();
        if calls.is_empty() {
            messages.push(json!({"role": "assistant", "content": step.text}));
            persist(&path, &messages, req.ephemeral).await;
            let structured = req.schema.as_ref().and_then(|_| serde_json::from_str(extract_json(&last_text)).ok());
            return Ok(TurnResult { stop: Stop::Completed, session: Some(session), text: transcript, usage, structured, error: None, run_id: String::new() });
        }
        let wire: Vec<Value> = calls.iter().map(|c| json!({"id": c.id, "type": "function", "function": {"name": c.name, "arguments": c.arguments}})).collect();
        messages.push(json!({"role": "assistant", "content": step.text, "tool_calls": wire}));
        for call in &calls {
            let args: Value = serde_json::from_str(&call.arguments).unwrap_or(Value::Null);
            let detail = args.get("path").or_else(|| args.get("command")).or_else(|| args.get("pattern")).and_then(Value::as_str).unwrap_or("").chars().take(200).collect();
            emit(AgentEvent::ToolStart { id: call.id.clone(), name: call.name.clone(), detail });
            let outcome = tools::execute(&call.name, &args, &req.cwd, read_only).await;
            let (ok, output) = match outcome {
                Ok(output) => (true, output),
                Err(error) => (false, error),
            };
            emit(AgentEvent::ToolEnd { id: call.id.clone(), ok, summary: output.lines().next().unwrap_or("").chars().take(160).collect() });
            messages.push(json!({"role": "tool", "tool_call_id": call.id, "content": output}));
        }
        persist(&path, &messages, req.ephemeral).await;
    }
    Ok(TurnResult { stop: Stop::Failed, session: Some(session), text: transcript, usage, structured: None, error: Some(format!("Stopped after {MAX_STEPS} tool steps without finishing.")), run_id: String::new() })
}

/// Finds the JSON object inside a reply that may wrap it in prose or a code fence.
pub fn extract_json(text: &str) -> &str {
    match (text.find('{'), text.rfind('}')) {
        (Some(start), Some(end)) if end > start => &text[start..=end],
        _ => text,
    }
}

async fn persist(path: &PathBuf, messages: &[Value], ephemeral: bool) {
    if ephemeral {
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = tokio::fs::create_dir_all(dir).await;
    }
    if let Ok(text) = serde_json::to_string(messages) {
        let _ = tokio::fs::write(path, text).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_node_with_no_model_says_what_to_do_and_other_errors_keep_their_detail() {
        let body = r#"{"error":{"message":"No model is ready on this node. Load one with POST /api/load and try again.","type":"model_not_ready"}}"#;
        assert_eq!(http_error("503", body), "No model is loaded on this node. Run /load to choose one.");
        assert_eq!(http_error("500", "boom"), "500: boom");
    }

    fn apply_all(lines: &[&str]) -> (Assembler, String) {
        let mut assembler = Assembler::default();
        let mut seen = String::new();
        for line in lines {
            assembler.apply(line, &mut |t| seen.push_str(t)).expect("apply");
        }
        (assembler, seen)
    }

    #[test]
    fn pictures_go_inline_and_plain_messages_stay_plain() {
        let dir = tempfile::tempdir().expect("dir");
        let image = dir.path().join("a.JPG");
        std::fs::write(&image, [1, 2, 3]).expect("write");
        assert_eq!(user_content("hi", &[]), json!("hi"));
        assert_eq!(user_content("hi", &[dir.path().join("gone.png")]), json!("hi"), "a missing file isn't fatal");
        let parts = user_content("hi", &[image]);
        assert_eq!(parts[0]["text"], "hi");
        assert_eq!(parts[1]["image_url"]["url"], "data:image/jpeg;base64,AQID");
    }

    #[tokio::test]
    async fn a_session_that_never_saved_starts_fresh_and_a_saved_one_resumes() {
        let path = std::env::temp_dir().join(format!("{}.json", new_session()));
        assert!(load_history(&path).await.expect("missing is fine").is_none());
        std::fs::write(&path, r#"[{"role":"user","content":"hi"}]"#).expect("write");
        let saved = load_history(&path).await.expect("read").expect("history");
        std::fs::remove_file(&path).expect("clean up");
        assert_eq!(saved[0]["content"], "hi");
    }

    #[test]
    fn sse_lines_survive_split_chunks() {
        let mut buffer = b"data: {\"a\"".to_vec();
        assert!(drain_sse(&mut buffer).is_empty());
        buffer.extend_from_slice(b":1}\n\ndata: [DONE]\n\n: comment\n");
        assert_eq!(drain_sse(&mut buffer), ["{\"a\":1}"]);
        assert!(buffer.is_empty());
    }

    #[test]
    fn assembles_fragmented_tool_calls() {
        let (assembler, _) = apply_all(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"shell","arguments":"{\"com"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"mand\":\"ls\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"c2","function":{"name":"read_file","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
        ]);
        assert_eq!(assembler.calls.len(), 2);
        assert_eq!(assembler.calls[0].arguments, r#"{"command":"ls"}"#);
        assert_eq!(assembler.finish.as_deref(), Some("tool_calls"));
    }

    #[test]
    fn collects_text_and_usage() {
        let (assembler, seen) = apply_all(&[
            r#"{"choices":[{"delta":{"content":"he"}}]}"#,
            r#"{"choices":[{"delta":{"content":"y"}}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":2,"prompt_tokens_details":{"cached_tokens":4}}}"#,
        ]);
        assert_eq!(seen, "hey");
        assert_eq!(assembler.usage, Some(Usage { input: 6, cache_read: 4, cache_write: 0, output: 2 }));
    }

    #[test]
    fn mid_stream_errors_surface() {
        let mut assembler = Assembler::default();
        let err = assembler.apply(r#"{"error":{"message":"out of credits"}}"#, &mut |_| {}).expect_err("error");
        assert_eq!(err, "out of credits");
    }

    #[test]
    fn extracts_json_from_a_fenced_reply() {
        assert_eq!(extract_json("Sure:\n```json\n{\"a\":{\"b\":1}}\n```"), "{\"a\":{\"b\":1}}");
        assert_eq!(extract_json("nothing"), "nothing");
    }
}
