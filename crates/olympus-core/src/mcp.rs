use crate::agent::{run_turn, Access, TurnRequest};
use crate::config::user_home;
use crate::proc::cancel_pair;
use crate::runtime::Runtime;
use crate::tools::clip;
use crate::{roles, skills, workflow};
use anyhow::Result;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const MAX_REPLY: usize = 16_000;

fn tool_list() -> Value {
    json!([
        {"name": "ask_model", "description": "Ask another model for help and get its answer. It can read the project but not change it. Accepts codex, chatgpt, opus, sonnet, haiku, gpt-*, openrouter:org/model or node:name.",
         "inputSchema": {"type": "object", "properties": {"model": {"type": "string"}, "prompt": {"type": "string", "description": "A self-contained question; the other model hasn't seen this conversation."}}, "required": ["model", "prompt"]}},
        {"name": "council", "description": "Ask several models the same question at once and get a synthesized recommendation noting where they disagree. Costs more than ask_model; use for decisions worth it.",
         "inputSchema": {"type": "object", "properties": {"prompt": {"type": "string"}, "models": {"type": "array", "items": {"type": "string"}, "description": "Defaults to one strong model per vendor."}}, "required": ["prompt"]}},
        {"name": "recall", "description": "Search lessons learned from this user's past work.",
         "inputSchema": {"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]}},
        {"name": "search_history", "description": "Full-text search of what was said in this user's past Claude Code and Codex sessions, both their messages and the replies.",
         "inputSchema": {"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]}},
        {"name": "list_skills", "description": "Find installed skills by keyword. Returns names and descriptions.",
         "inputSchema": {"type": "object", "properties": {"query": {"type": "string"}}}},
        {"name": "get_skill", "description": "Load one installed skill's full instructions.",
         "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}}
    ])
}

async fn ask_model(rt: &Runtime, args: &Value) -> Result<String, String> {
    let model = args["model"].as_str().ok_or("ask_model needs 'model'.")?;
    let prompt = args["prompt"].as_str().ok_or("ask_model needs 'prompt'.")?;
    let target = rt.resolve(model)?;
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let mut req = TurnRequest::new(target, prompt, cwd);
    req.access = Access::ReadOnly;
    req.role = Some(roles::CONSULTANT);
    req.ephemeral = true;
    req.depth += 1;
    let (_tx, cancel) = cancel_pair();
    let result = run_turn(rt, &req, cancel, &mut |_| {}).await;
    if result.ok() {
        Ok(clip(&result.text, MAX_REPLY))
    } else {
        Err(result.error.unwrap_or_else(|| "The model failed without saying why.".into()))
    }
}

async fn call_tool(rt: Arc<Runtime>, name: &str, args: &Value) -> Result<String, String> {
    match name {
        "ask_model" => ask_model(&rt, args).await,
        "council" => {
            let prompt = args["prompt"].as_str().ok_or("council needs 'prompt'.")?.to_string();
            let names: Vec<String> = args["models"].as_array().map(|m| m.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
            let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
            let (_tx, cancel) = cancel_pair();
            let outcome = workflow::council(rt.clone(), &prompt, &names, &cwd, cancel, &mut |_| {}).await?;
            Ok(clip(&outcome.synthesis, MAX_REPLY))
        }
        "recall" => {
            let query = args["query"].as_str().ok_or("recall needs 'query'.")?;
            let store = rt.learn.clone().ok_or("The learning store isn't available.")?;
            let store = store.lock().map_err(|_| "The learning store is unavailable.")?;
            let cwd = std::env::current_dir().ok();
            let lessons = store.recall(query, cwd.as_deref(), 8).map_err(|e| e.to_string())?;
            Ok(if lessons.is_empty() { "No lessons match.".into() } else { lessons.iter().map(|l| format!("- {}", l.text)).collect::<Vec<_>>().join("\n") })
        }
        "search_history" => {
            let query = args["query"].as_str().ok_or("search_history needs 'query'.")?;
            let store = rt.learn.clone().ok_or("The learning store isn't available.")?;
            let store = store.lock().map_err(|_| "The learning store is unavailable.")?;
            let hits = store.search_messages(query, 8).map_err(|e| e.to_string())?;
            Ok(if hits.is_empty() { "Nothing in history matches.".into() } else { hits.iter().map(|h| format!("- [{} {} in {}] {}", h.timestamp, h.role, h.cwd, h.snippet)).collect::<Vec<_>>().join("\n") })
        }
        "list_skills" => {
            let all = skills::installed(&skills::default_roots(&user_home()));
            let found = skills::search(&all, args["query"].as_str().unwrap_or(""));
            Ok(clip(&found.iter().map(|s| format!("{}: {}", s.name, s.description)).collect::<Vec<_>>().join("\n"), MAX_REPLY))
        }
        "get_skill" => {
            let wanted = args["name"].as_str().ok_or("get_skill needs 'name'.")?;
            let all = skills::installed(&skills::default_roots(&user_home()));
            let skill = all.iter().find(|s| s.name.eq_ignore_ascii_case(wanted)).ok_or_else(|| format!("No skill named '{wanted}'. Use list_skills to find one."))?;
            let text = std::fs::read_to_string(&skill.path).map_err(|e| e.to_string())?;
            Ok(clip(&text, MAX_REPLY))
        }
        other => Err(format!("Unknown tool '{other}'.")),
    }
}

/// Handles one JSON-RPC message; notifications return None.
pub async fn handle(rt: Arc<Runtime>, message: &Value) -> Option<Value> {
    let id = message.get("id")?.clone();
    let method = message["method"].as_str().unwrap_or("");
    let result = match method {
        "initialize" => {
            let version = message["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18");
            json!({"protocolVersion": version, "capabilities": {"tools": {}}, "serverInfo": {"name": "olympus", "version": env!("CARGO_PKG_VERSION")}})
        }
        "ping" => json!({}),
        "tools/list" => json!({"tools": tool_list()}),
        "tools/call" => {
            let name = message["params"]["name"].as_str().unwrap_or("");
            let empty = json!({});
            let args = message["params"].get("arguments").unwrap_or(&empty);
            match call_tool(rt, name, args).await {
                Ok(text) => json!({"content": [{"type": "text", "text": text}]}),
                Err(error) => json!({"content": [{"type": "text", "text": error}], "isError": true}),
            }
        }
        other => return Some(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("Unknown method '{other}'")}})),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

/// Serves MCP over stdio until the client closes stdin; calls run concurrently.
pub async fn serve_stdio(rt: Arc<Runtime>) -> Result<()> {
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(line) = out_rx.recv().await {
            if stdout.write_all(line.as_bytes()).await.is_err() || stdout.write_all(b"\n").await.is_err() || stdout.flush().await.is_err() {
                break;
            }
        }
    });
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
        let (rt, out) = (rt.clone(), out_tx.clone());
        tokio::spawn(async move {
            if let Some(reply) = handle(rt, &message).await {
                let _ = out.send(reply.to_string());
            }
        });
    }
    drop(out_tx);
    let _ = writer.await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::limits::Limits;

    fn runtime() -> Arc<Runtime> {
        Arc::new(Runtime { config: Config::default(), claude: None, codex: None, limits: Arc::new(Limits::default()), learn: None, self_exe: "olympus".into(), rules: String::new() })
    }

    #[tokio::test]
    async fn initialize_echoes_the_clients_protocol() {
        let reply = handle(runtime(), &json!({"id": 1, "method": "initialize", "params": {"protocolVersion": "2099-01-01"}})).await.expect("reply");
        assert_eq!(reply["result"]["protocolVersion"], "2099-01-01");
        assert_eq!(reply["result"]["serverInfo"]["name"], "olympus");
    }

    #[tokio::test]
    async fn notifications_get_no_reply() {
        assert!(handle(runtime(), &json!({"method": "notifications/initialized"})).await.is_none());
    }

    #[tokio::test]
    async fn lists_every_tool_with_a_schema() {
        let reply = handle(runtime(), &json!({"id": 1, "method": "tools/list"})).await.expect("reply");
        let tools = reply["result"]["tools"].as_array().expect("tools");
        assert_eq!(tools.len(), 6);
        assert!(tools.iter().all(|t| t["inputSchema"]["type"] == "object"));
    }

    #[tokio::test]
    async fn tool_failures_are_tool_errors_not_protocol_errors() {
        let reply = handle(runtime(), &json!({"id": 1, "method": "tools/call", "params": {"name": "ask_model", "arguments": {"model": "banana", "prompt": "hi"}}})).await.expect("reply");
        assert_eq!(reply["result"]["isError"], true);
        assert!(reply["result"]["content"][0]["text"].as_str().expect("text").contains("banana"));
    }

    #[tokio::test]
    async fn unknown_methods_are_protocol_errors() {
        let reply = handle(runtime(), &json!({"id": 1, "method": "nope"})).await.expect("reply");
        assert_eq!(reply["error"]["code"], -32601);
    }
}
