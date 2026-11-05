use crate::event::{AgentEvent, Stop, TurnResult, Usage};
use crate::proc::Cancel;
use crate::runtime::Runtime;
use crate::target::{Provider, Target};
use crate::{claude, codex, openai, roles};
use olympus_learn::{classify_task, RunRecord};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// What an agent may do. `Full` is bypass-permissions mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Full,
    ReadOnly,
}

/// `Lean` trims startup context (about 37k to 11k tokens on Claude); `Full` keeps the user's whole setup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Lean,
    Full,
}

/// One turn for any backend.
#[derive(Debug, Clone)]
pub struct TurnRequest {
    pub target: Target,
    pub prompt: String,
    pub cwd: PathBuf,
    pub resume: Option<String>,
    pub access: Access,
    pub profile: Profile,
    pub role: Option<&'static str>,
    pub schema: Option<Value>,
    pub ephemeral: bool,
    pub depth: u32,
    pub account_dir: Option<PathBuf>,
    pub use_memory: bool,
    /// Pictures attached to the prompt, for providers that take them directly.
    pub images: Vec<PathBuf>,
    /// The caller reports success later, because finishing a turn doesn't prove the work is right.
    pub deferred_outcome: bool,
}

impl TurnRequest {
    /// A full-access lean turn with memory enabled.
    pub fn new(target: Target, prompt: impl Into<String>, cwd: PathBuf) -> Self {
        let prompt = prompt.into();
        Self {
            target,
            images: attached_images(&prompt),
            prompt,
            cwd,
            resume: None,
            access: Access::Full,
            profile: Profile::Lean,
            role: None,
            schema: None,
            ephemeral: false,
            depth: std::env::var("OLYMPUS_DEPTH").ok().and_then(|d| d.parse().ok()).unwrap_or(0),
            account_dir: None,
            use_memory: true,
            deferred_outcome: false,
        }
    }
}

/// Marks a file attached to a message; models with file tools read the path from the prompt.
pub const ATTACHED: &str = "Attached: ";

/// The image files a prompt has attached.
pub fn attached_images(prompt: &str) -> Vec<PathBuf> {
    prompt.lines().filter_map(|line| line.strip_prefix(ATTACHED)).map(PathBuf::from).filter(|path| is_image(path)).collect()
}

/// True for the picture formats models accept.
pub fn is_image(path: &std::path::Path) -> bool {
    path.extension().is_some_and(|ext| matches!(ext.to_string_lossy().to_ascii_lowercase().as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp"))
}

fn next_run_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    format!("{now:x}-{:x}-{:x}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// The MCP server definition that lets a Claude agent consult other models.
pub fn claude_mcp_config(exe: &std::path::Path) -> String {
    json!({"mcpServers": {"olympus": {"command": exe.display().to_string(), "args": ["mcp"]}}}).to_string()
}

async fn memory_text(rt: &Runtime, run_id: &str, req: &TurnRequest) -> String {
    let Some(store) = rt.learn.clone() else { return String::new() };
    let (run_id, prompt, cwd) = (run_id.to_string(), req.prompt.clone(), req.cwd.clone());
    tokio::task::spawn_blocking(move || {
        store.lock().ok().and_then(|s| s.select_injection(&run_id, &prompt, Some(&cwd), 400).ok()).map(|i| i.text).unwrap_or_default()
    })
    .await
    .unwrap_or_default()
}

fn compose_system(rt: &Runtime, req: &TurnRequest, memory: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if rt.config.share_user_rules && !rt.rules.is_empty() && req.profile == Profile::Lean {
        parts.push(&rt.rules);
    }
    if let Some(role) = req.role {
        parts.push(role);
    }
    if req.depth == 0 && req.access == Access::Full {
        parts.push(roles::BRIDGE_HINT);
    }
    if !memory.is_empty() {
        parts.push(memory);
    }
    parts.join("\n\n")
}

/// Runs one turn on its provider, updating quota state and the learning store.
pub async fn run_turn(rt: &Runtime, req: &TurnRequest, cancel: Cancel, emit: &mut (dyn FnMut(AgentEvent) + Send)) -> TurnResult {
    let run_id = next_run_id();
    let fresh = req.resume.is_none();
    let memory = if fresh && req.use_memory && req.access == Access::Full { memory_text(rt, &run_id, req).await } else { String::new() };
    let system = if fresh { compose_system(rt, req, &memory) } else { String::new() };
    let idle = Duration::from_secs(rt.config.idle_timeout_secs);
    let bridge = req.depth == 0 && req.access == Access::Full;
    let limits = rt.limits.clone();
    let mut forward = |event: AgentEvent| {
        if let AgentEvent::Limit(info) = &event {
            limits.update(crate::limits::for_account(info, req.account_dir.as_deref()));
        }
        emit(event);
    };
    let outcome = match &req.target.provider {
        Provider::Claude => match &rt.claude {
            Some(bin) => {
                let config = bridge.then(|| claude_mcp_config(&rt.self_exe));
                claude::run(&bin.path, req, &system, config.as_deref(), idle, cancel, &mut forward).await
            }
            None => Ok(TurnResult::failed(Stop::Failed, "Claude Code wasn't found. Install it, then restart Olympus.")),
        },
        Provider::Codex => match &rt.codex {
            Some(bin) => {
                let mcp = bridge.then_some(rt.self_exe.as_path());
                codex::run(&bin.path, req, &system, mcp, idle, cancel, &mut forward).await
            }
            None => Ok(TurnResult::failed(Stop::Failed, "Codex wasn't found. Install it, then restart Olympus.")),
        },
        Provider::OpenRouter | Provider::Node(_) => openai::run(rt, req, &system, idle, cancel, &mut forward).await,
    };
    let mut result = outcome.unwrap_or_else(|err| TurnResult::failed(Stop::Failed, format!("{err:#}")));
    result.run_id = run_id;
    if result.stop != Stop::Cancelled {
        let verdict = (!req.deferred_outcome || !result.ok()).then(|| result.ok());
        record_outcome(rt, &result.run_id, req, &result.usage, verdict, false).await;
    }
    result
}

/// Stores a run's usage and verdict; `None` leaves the verdict open for a later call.
pub async fn record_outcome(rt: &Runtime, run_id: &str, req: &TurnRequest, usage: &Usage, outcome: Option<bool>, corrected: bool) {
    let Some(store) = rt.learn.clone() else { return };
    let run = RunRecord {
        run_id: run_id.to_string(),
        task_class: classify_task(&req.prompt).to_string(),
        repo: crate::git::repo_key(&req.cwd),
        model: req.target.to_string(),
        provider: req.target.provider.vendor().to_string(),
        input_tokens: usage.input,
        cache_read: usage.cache_read,
        cache_write: usage.cache_write,
        output_tokens: usage.output,
        outcome,
        corrected,
    };
    let _ = tokio::task::spawn_blocking(move || store.lock().ok().map(|s| s.record_run(&run))).await;
}
