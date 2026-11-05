//! Exercises the real signed-in CLIs on their cheapest models; run with `cargo test -p olympus-core --test live -- --ignored`.

use olympus_core::agent::{run_turn, Access, TurnRequest};
use olympus_core::config::Tiers;
use olympus_core::event::{AgentEvent, TurnResult};
use olympus_core::proc::cancel_pair;
use olympus_core::runtime::Runtime;
use olympus_core::target::Target;
use olympus_core::workflow::{self, TaskState, WorkflowEvent};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

const CHEAP_CLAUDE: &str = "claude:haiku";
const CHEAP_CODEX: &str = "codex:gpt-6-luna@low";

fn runtime() -> Arc<Runtime> {
    let mut rt = Runtime::load().expect("runtime");
    rt.learn = None;
    rt.rules = String::new();
    let exe = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/olympus.exe");
    assert!(exe.exists(), "build the olympus binary first: {}", exe.display());
    rt.self_exe = exe;
    rt.config.tiers = Tiers { cheap: vec![CHEAP_CLAUDE.into(), CHEAP_CODEX.into()], standard: vec![CHEAP_CLAUDE.into(), CHEAP_CODEX.into()], strong: vec![CHEAP_CLAUDE.into(), CHEAP_CODEX.into()] };
    Arc::new(rt)
}

async fn turn(rt: &Runtime, req: &TurnRequest) -> (TurnResult, Vec<AgentEvent>) {
    let mut events = Vec::new();
    let (_keep, cancel) = cancel_pair();
    let result = run_turn(rt, req, cancel, &mut |e| events.push(e)).await;
    (result, events)
}

fn request(spec: &str, prompt: &str, cwd: &Path) -> TurnRequest {
    let mut req = TurnRequest::new(Target::parse(spec).expect("target"), prompt, cwd.to_path_buf());
    req.use_memory = false;
    req
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git").args(args).current_dir(cwd).output().expect("git");
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    git(dir.path(), &["config", "user.email", "t@example.com"]);
    git(dir.path(), &["config", "user.name", "T"]);
    std::fs::write(dir.path().join("README.md"), "# Demo\n").expect("write");
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-q", "-m", "init"]);
    dir
}

async fn streams_and_resumes(spec: &str) {
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    let (first, events) = turn(&rt, &request(spec, "Remember the word mango. Reply with only: ok", dir.path())).await;
    assert!(first.ok(), "{spec}: {:?}", first.error);
    assert!(first.usage.output > 0, "{spec} reported no usage");
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Text(_))), "{spec} streamed no text");
    let session = first.session.clone().expect("session id");
    println!("{spec} first turn context: {:?}", first.usage);

    let mut next = request(spec, "What word did I ask you to remember? Reply with only that word.", dir.path());
    next.resume = Some(session.clone());
    let (second, _) = turn(&rt, &next).await;
    assert!(second.ok(), "{spec} resume: {:?}", second.error);
    assert!(second.text.to_lowercase().contains("mango"), "{spec} lost context: {}", second.text);
    assert_eq!(second.session.as_deref(), Some(session.as_str()));
}

#[tokio::test]
#[ignore]
async fn claude_streams_and_resumes() {
    streams_and_resumes(CHEAP_CLAUDE).await;
}

#[tokio::test]
#[ignore]
async fn codex_streams_and_resumes() {
    streams_and_resumes(CHEAP_CODEX).await;
}

#[tokio::test]
#[ignore]
async fn claude_lean_startup_uses_subscription_and_stays_small() {
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    let (result, _) = turn(&rt, &request(CHEAP_CLAUDE, "Reply with only: ok", dir.path())).await;
    assert!(result.ok(), "{:?}", result.error);
    let context = result.usage.input + result.usage.cache_read + result.usage.cache_write;
    println!("lean context tokens: {context}");
    assert!(context < 20_000, "lean startup grew to {context}");
}

async fn read_only_cannot_write(spec: &str) {
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    let mut req = request(spec, "Create a file named pwned.txt containing hi in the current directory, using any tool you have. Then say done.", dir.path());
    req.access = Access::ReadOnly;
    let (_, _) = turn(&rt, &req).await;
    assert!(!dir.path().join("pwned.txt").exists(), "{spec} wrote a file in read-only mode");
}

#[tokio::test]
#[ignore]
async fn claude_read_only_is_enforced() {
    read_only_cannot_write(CHEAP_CLAUDE).await;
}

#[tokio::test]
#[ignore]
async fn codex_read_only_is_enforced() {
    read_only_cannot_write(CHEAP_CODEX).await;
}

async fn full_access_edits(spec: &str) {
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    let (result, events) = turn(&rt, &request(spec, "Create a file named made.txt containing exactly the word olympus. Then say done.", dir.path())).await;
    assert!(result.ok(), "{spec}: {:?}", result.error);
    let text = std::fs::read_to_string(dir.path().join("made.txt")).expect("file created");
    assert!(text.contains("olympus"));
    assert!(events.iter().any(|e| matches!(e, AgentEvent::ToolStart { .. })), "{spec} reported no tool use");
    assert!(events.iter().any(|e| matches!(e, AgentEvent::ToolEnd { .. })), "{spec} never finished a tool");
}

#[tokio::test]
#[ignore]
async fn claude_full_access_edits_without_prompting() {
    full_access_edits(CHEAP_CLAUDE).await;
}

#[tokio::test]
#[ignore]
async fn codex_full_access_edits_without_prompting() {
    full_access_edits(CHEAP_CODEX).await;
}

async fn returns_structured(spec: &str) {
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    let mut req = request(spec, "Return a=7.", dir.path());
    req.access = Access::ReadOnly;
    req.schema = Some(json!({"type": "object", "additionalProperties": false, "required": ["a"], "properties": {"a": {"type": "integer"}}}));
    let (result, _) = turn(&rt, &req).await;
    assert!(result.ok(), "{spec}: {:?}", result.error);
    assert_eq!(result.structured.expect("structured output")["a"], 7);
}

#[tokio::test]
#[ignore]
async fn claude_returns_structured_output() {
    returns_structured(CHEAP_CLAUDE).await;
}

#[tokio::test]
#[ignore]
async fn codex_returns_structured_output() {
    returns_structured(CHEAP_CODEX).await;
}

async fn agent_consults_other_vendor(spec: &str, other: &str) {
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    let prompt = format!("Use the olympus ask_model tool with model \"{other}\" and prompt \"Reply with only the word zebra\". Then tell me exactly what it answered.");
    let (result, events) = turn(&rt, &request(spec, &prompt, dir.path())).await;
    assert!(result.ok(), "{spec}: {:?}", result.error);
    let used = events.iter().any(|e| matches!(e, AgentEvent::ToolStart { name, .. } if name.contains("ask_model")));
    assert!(used, "{spec} never called the bridge: {events:?}");
    assert!(result.text.to_lowercase().contains("zebra"), "{spec} answer: {}", result.text);
}

#[tokio::test]
#[ignore]
async fn claude_takes_help_from_codex_through_the_bridge() {
    agent_consults_other_vendor(CHEAP_CLAUDE, CHEAP_CODEX).await;
}

#[tokio::test]
#[ignore]
async fn codex_takes_help_from_claude_through_the_bridge() {
    agent_consults_other_vendor(CHEAP_CODEX, CHEAP_CLAUDE).await;
}

#[tokio::test]
#[ignore]
async fn council_combines_both_vendors() {
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    let (_keep, cancel) = cancel_pair();
    let outcome = workflow::council(rt, "Is 17 prime? One sentence.", &[], dir.path(), cancel, &mut |_| {}).await.expect("council");
    assert_eq!(outcome.answers.iter().filter(|a| a.text.is_some()).count(), 2, "{:?}", outcome.answers);
    assert!(!outcome.synthesis.is_empty());
    println!("council usage: {:?}\n{}", outcome.usage, outcome.synthesis);
}

#[tokio::test]
#[ignore]
async fn auto_plans_builds_verifies_reviews_and_lands() {
    let rt = runtime();
    let dir = repo();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (_keep, cancel) = cancel_pair();
    let goal = "Create two files: hello.txt containing the word hello, and world.txt containing the word world. Use a single task. Verify with this exact PowerShell command: Test-Path hello.txt; if (-not (Test-Path world.txt)) { exit 1 }";
    let run = tokio::spawn(workflow::run_auto(rt, goal.to_string(), dir.path().to_path_buf(), tx, cancel));
    let mut states = Vec::new();
    let mut phases = Vec::new();
    while let Some(event) = rx.recv().await {
        match event {
            WorkflowEvent::Task { state, .. } => states.push(state),
            WorkflowEvent::Phase(phase) => phases.push(phase),
            WorkflowEvent::Plan(plan) => println!("plan: {plan:#?}"),
            WorkflowEvent::Agent { task, event: AgentEvent::ToolStart { name, detail, .. } } => println!("[{task}] {name} {detail}"),
            _ => {}
        }
    }
    let outcome = run.await.expect("join").expect("auto");
    println!("phases: {phases:?}\nstates: {states:?}\nsummary: {}\nusage: {:?}", outcome.summary, outcome.usage);
    assert!(outcome.ok, "{}", outcome.summary);
    assert!(states.contains(&TaskState::Done));
    assert!(phases.iter().any(|p| p == "Independent review"), "{phases:?}");
    assert!(!dir.path().join("hello.txt").exists(), "work must stay on its branch until landed");
    workflow::land(dir.path(), &outcome.branch).await.expect("land");
    assert!(dir.path().join("hello.txt").exists() && dir.path().join("world.txt").exists());
    assert!(git(dir.path(), &["status", "--porcelain"]).is_empty());
}

#[tokio::test]
#[ignore]
async fn node_runs_an_agent_loop_with_tools() {
    let exe = std::env::var("OLYMPUS_NODE_EXE").expect("set OLYMPUS_NODE_EXE");
    let mut child = tokio::process::Command::new(exe).arg("--no-firewall").stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).kill_on_drop(true).spawn().expect("node starts");
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let mut rt = Runtime::load().expect("runtime");
    rt.learn = None;
    rt.rules = String::new();
    let found = olympus_core::node::discover(&rt.config, std::time::Duration::from_millis(800)).await.expect("discover");
    let node = found.first().expect("node answers discovery").clone();
    let client = olympus_core::node::NodeClient::new(&node.address(), &rt.config.node_token);
    let status = client.status().await.expect("status");
    let model = status.local.iter().find(|m| m.repo.contains("Qwen3-0.6B")).expect("Qwen3-0.6B downloaded").id.clone();
    client.load(&model).await.expect("load");
    for _ in 0..120 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if client.status().await.expect("status").state == "ready" {
            break;
        }
    }
    rt.config.nodes.insert(node.name.clone(), node.address());

    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("secret.txt"), "the code is 4417").expect("write");
    let spec = format!("node:{}", node.name);
    let mut req = request(&spec, "Use the read_file tool to read secret.txt, then tell me the code.", dir.path());
    req.access = Access::ReadOnly;
    let (result, events) = turn(&rt, &req).await;
    println!("node result: {:?} {:?}\n{}\nusage {:?}", result.stop, result.error, result.text, result.usage);
    assert!(result.ok(), "{:?}", result.error);
    assert!(events.iter().any(|e| matches!(e, AgentEvent::ToolStart { name, .. } if name == "read_file")), "{events:?}");
    assert!(result.text.contains("4417"), "{}", result.text);

    client.unload().await.expect("unload");
    child.kill().await.expect("kill");
}
#[tokio::test]
#[ignore]
async fn real_history_ingests_and_distills() {
    let rt = Runtime::load().expect("runtime");
    let store = rt.learn.clone().expect("store");
    let home = olympus_core::config::user_home();
    let started = std::time::Instant::now();
    let stats = store.lock().expect("lock").ingest(Some(&home.join(".claude")), Some(&home.join(".codex")), &mut |_| {}).expect("ingest");
    println!("ingest {:?}: {stats:?}", started.elapsed());
    let dir = tempfile::tempdir().expect("tempdir");
    let (_keep, cancel) = cancel_pair();
    let added = workflow::distill(&rt, dir.path(), cancel).await.expect("distill");
    println!("distilled {added} lessons");
    let lessons = store.lock().expect("lock").lessons(Some(olympus_learn::LessonState::Candidate), 200).expect("lessons");
    for lesson in lessons {
        println!("{:?} | {}", lesson.kind, lesson.text.chars().take(120).collect::<String>());
    }
    let totals = store.lock().expect("lock").stats().expect("stats");
    println!("files {} prompts {} signals {} candidates {}", totals.files, totals.prompts, totals.signals, totals.lessons_candidate);
    assert!(added > 0);
}
async fn read_only_can_still_read(spec: &str) {
    let rt = runtime();
    let dir = repo();
    std::fs::write(dir.path().join("secret.txt"), "the code is 4417").expect("write");
    let mut req = request(spec, "Read the file secret.txt in the current directory and reply with the code it contains. If you cannot read it, say BLOCKED.", dir.path());
    req.access = Access::ReadOnly;
    let (result, events) = turn(&rt, &req).await;
    println!("{spec}: {:?}\n{}\n{events:?}", result.stop, result.text);
    assert!(result.ok(), "{spec}: {:?}", result.error);
    assert!(result.text.contains("4417"), "{spec} couldn't read: {}", result.text);
}

#[tokio::test]
#[ignore]
async fn claude_read_only_can_read_the_project() {
    read_only_can_still_read(CHEAP_CLAUDE).await;
}

#[tokio::test]
#[ignore]
async fn codex_read_only_can_read_the_project() {
    read_only_can_still_read(CHEAP_CODEX).await;
}

#[test]
#[ignore]
fn the_signed_in_default_account_is_reported_as_signed_in() {
    let claude = olympus_core::discovery::find_claude();
    let found = olympus_core::discovery::accounts(claude.as_ref(), None);
    let default = found.iter().find(|a| a.dir.ends_with(".claude")).expect("default account");
    assert!(default.signed_in, "{default:?}");
    assert!(default.label.contains('@'), "the default account should name its email");
}

#[test]
#[ignore]
fn the_signed_in_chatgpt_account_is_listed_with_its_email() {
    let codex = olympus_core::discovery::find_codex();
    let found = olympus_core::discovery::accounts(None, codex.as_ref());
    let default = found.iter().find(|a| a.dir.ends_with(".codex")).expect("default ChatGPT account");
    assert!(default.signed_in, "{default:?}");
    assert!(default.label.contains('@'), "the account should name its email");
}

#[tokio::test]
#[ignore]
async fn a_skill_runs_for_real_on_the_full_profile() {
    let rt = runtime();
    let dir = repo();
    let skill = dir.path().join(".claude/skills/secret-word");
    std::fs::create_dir_all(&skill).expect("skill folder");
    std::fs::write(skill.join("SKILL.md"), "---\nname: secret-word\ndescription: Answers with a word\n---\nReply with exactly this word and nothing else: ZEBRA7731").expect("skill");
    let mut req = request(CHEAP_CLAUDE, "/secret-word", dir.path());
    req.profile = olympus_core::agent::Profile::Full;
    let (result, events) = turn(&rt, &req).await;
    assert!(result.ok(), "{:?}", result.error);
    assert!(result.text.contains("ZEBRA7731"), "only the skill knows this word: {:?} {events:?}", result.text);
}

#[tokio::test]
#[ignore]
async fn real_models_can_see_an_attached_picture() {
    let picture = std::env::var("OLYMPUS_TEST_PNG").expect("a picture of the word ZEBRA");
    let rt = runtime();
    let dir = repo();
    let prompt = format!("What single word is written in the attached picture? Answer with only that word.\n\nAttached: {picture}");
    for spec in [CHEAP_CLAUDE, CHEAP_CODEX] {
        let (result, _) = turn(&rt, &request(spec, &prompt, dir.path())).await;
        println!("{spec}: {:?} {:?}", result.text, result.error);
        assert!(result.ok(), "{spec}: {:?}", result.error);
        assert!(result.text.to_uppercase().contains("ZEBRA"), "{spec} read the picture: {:?}", result.text);
    }
}

fn transcripts(dir: &std::path::Path) -> usize {
    fn walk(dir: &std::path::Path, count: &mut usize) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, count);
            } else if path.extension().is_some_and(|e| e == "jsonl") {
                *count += 1;
            }
        }
    }
    let mut count = 0;
    walk(&dir.join("projects"), &mut count);
    count
}

#[tokio::test]
#[ignore]
async fn two_threads_can_run_at_once_on_different_claude_accounts() {
    let home = olympus_core::config::user_home();
    let extra = std::fs::read_dir(&home).expect("home").flatten().map(|e| e.path()).find(|p| p.join(".credentials.json").exists() && p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(".claude-"))).expect("a second account");
    let default_dir = home.join(".claude");
    let (default_before, extra_before) = (transcripts(&default_dir), transcripts(&extra));
    let rt = runtime();
    let (dir_a, dir_b) = (repo(), repo());
    let first = request(CHEAP_CLAUDE, "Reply with the single word ONE.", dir_a.path());
    let mut second = request(CHEAP_CLAUDE, "Reply with the single word TWO.", dir_b.path());
    second.account_dir = Some(extra.clone());
    let ((a, _), (b, _)) = tokio::join!(turn(&rt, &first), turn(&rt, &second));
    println!("default: {:?} {:?}\nextra: {:?} {:?}\nlimits: {:?}", a.stop, a.error, b.stop, b.error, rt.limits.snapshot());
    assert!(a.ok() && b.ok(), "{:?} {:?}", a.error, b.error);
    assert_eq!(transcripts(&default_dir), default_before + 1, "the default account ran only its own turn");
    assert_eq!(transcripts(&extra), extra_before + 1, "the second account ran its own turn");
}
