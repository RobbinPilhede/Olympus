use crate::agent::{record_outcome, run_turn, Access, TurnRequest};
use crate::config::home_dir;
use crate::event::{AgentEvent, Stop, TurnResult, Usage};
use crate::git::{self, Merge};
use crate::proc::Cancel;
use crate::roles;
use crate::runtime::{Runtime, Tier};
use crate::target::Target;
use crate::tools::{clip, run_shell};
use anyhow::{anyhow, Context, Result};
use olympus_learn::{LessonKind, LessonState, NewLesson};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinSet;

const MAIN_SEP: char = '\\';
const VERIFY_TIMEOUT: Duration = Duration::from_secs(900);
const MAX_TASK_ATTEMPTS: u32 = 3;

/// A planned task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanTask {
    pub id: String,
    pub title: String,
    pub prompt: String,
    pub deps: Vec<String>,
    pub tier: Tier,
}

/// The planner's output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub summary: String,
    pub tasks: Vec<PlanTask>,
    pub verify: Vec<String>,
}

/// JSON schema for [`Plan`]; strict enough for both vendors' structured output.
pub fn plan_schema() -> Value {
    json!({
        "type": "object", "additionalProperties": false, "required": ["summary", "tasks", "verify"],
        "properties": {
            "summary": {"type": "string"},
            "verify": {"type": "array", "items": {"type": "string"}},
            "tasks": {"type": "array", "items": {
                "type": "object", "additionalProperties": false, "required": ["id", "title", "prompt", "deps", "tier"],
                "properties": {
                    "id": {"type": "string"}, "title": {"type": "string"}, "prompt": {"type": "string"},
                    "deps": {"type": "array", "items": {"type": "string"}},
                    "tier": {"type": "string", "enum": ["cheap", "standard", "strong"]}
                }
            }}
        }
    })
}

/// A review finding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub severity: String,
    pub file: String,
    pub summary: String,
}

/// An independent reviewer's verdict.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Review {
    pub approved: bool,
    pub findings: Vec<Finding>,
}

/// JSON schema for [`Review`].
pub fn review_schema() -> Value {
    json!({
        "type": "object", "additionalProperties": false, "required": ["approved", "findings"],
        "properties": {
            "approved": {"type": "boolean"},
            "findings": {"type": "array", "items": {
                "type": "object", "additionalProperties": false, "required": ["severity", "file", "summary"],
                "properties": {"severity": {"type": "string", "enum": ["high", "medium", "low"]}, "file": {"type": "string"}, "summary": {"type": "string"}}
            }}
        }
    })
}

/// Rejects duplicate ids, unknown dependencies and cycles.
pub fn validate_plan(plan: &Plan) -> Result<(), String> {
    if plan.tasks.is_empty() {
        return Err("The plan has no tasks.".into());
    }
    let mut ids = HashSet::new();
    for task in &plan.tasks {
        if task.id.is_empty() || !task.id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return Err(format!("Task id '{}' can't be used in a branch name.", task.id));
        }
        if !ids.insert(task.id.as_str()) {
            return Err(format!("Task id '{}' appears twice.", task.id));
        }
    }
    for task in &plan.tasks {
        if let Some(missing) = task.deps.iter().find(|d| !ids.contains(d.as_str())) {
            return Err(format!("Task '{}' depends on unknown task '{missing}'.", task.id));
        }
    }
    let mut done: HashSet<&str> = HashSet::new();
    while done.len() < plan.tasks.len() {
        let ready: Vec<&str> = plan.tasks.iter().filter(|t| !done.contains(t.id.as_str()) && t.deps.iter().all(|d| done.contains(d.as_str()))).map(|t| t.id.as_str()).collect();
        if ready.is_empty() {
            return Err("The plan's dependencies form a cycle.".into());
        }
        done.extend(ready);
    }
    Ok(())
}

fn path_spellings(dir: &Path) -> Vec<String> {
    let native = dir.display().to_string();
    let forward = native.replace(MAIN_SEP, "/");
    let mut forms = vec![native, forward.clone()];
    let mut chars = forward.chars();
    if let (Some(drive), Some(':')) = (chars.next(), chars.next()) {
        forms.push(format!("/{}{}", drive.to_ascii_lowercase(), chars.as_str()));
    }
    forms
}

/// Rewrites absolute repo paths to relative ones so a task can't escape its worktree by following its prompt.
pub fn localize_prompt(prompt: &str, dirs: &[&Path]) -> String {
    let mut text = prompt.to_string();
    for dir in dirs {
        for form in path_spellings(dir) {
            for sep in ['/', MAIN_SEP] {
                text = text.replace(&format!("{form}{sep}"), "");
            }
            text = text.replace(&form, ".");
        }
    }
    text
}

/// Task lifecycle shown to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskState {
    Pending,
    Running,
    Verifying,
    Merging,
    Done,
    Failed,
    Skipped,
}

/// Progress from a running workflow.
#[derive(Debug, Clone)]
pub enum WorkflowEvent {
    Phase(String),
    Note(String),
    Plan(Plan),
    Task { id: String, title: String, state: TaskState, target: Option<String>, note: String },
    Agent { task: String, event: AgentEvent },
    Usage(Usage),
}

/// How a workflow ended.
#[derive(Debug, Clone)]
pub struct AutoOutcome {
    pub ok: bool,
    pub branch: String,
    pub base: String,
    pub summary: String,
    pub usage: Usage,
    pub checks_ran: Vec<String>,
}

struct Flow {
    rt: Arc<Runtime>,
    tx: UnboundedSender<WorkflowEvent>,
    cancel: Cancel,
}

impl Flow {
    fn phase(&self, text: impl Into<String>) {
        let _ = self.tx.send(WorkflowEvent::Phase(text.into()));
    }

    async fn turn(&self, task: &str, req: &TurnRequest) -> TurnResult {
        let (tx, name) = (self.tx.clone(), task.to_string());
        let result = run_turn(&self.rt, req, self.cancel.clone(), &mut move |event| {
            let _ = tx.send(WorkflowEvent::Agent { task: name.clone(), event });
        })
        .await;
        let _ = self.tx.send(WorkflowEvent::Usage(result.usage));
        result
    }

    fn cancelled(&self) -> bool {
        *self.cancel.borrow()
    }
}

async fn verify(commands: &[String], cwd: &Path) -> Result<(), String> {
    for command in commands {
        match run_shell(command, cwd, VERIFY_TIMEOUT).await {
            Ok((0, _)) => {}
            Ok((code, output)) => return Err(format!("`{command}` exited {code}:\n{}", clip(&output, 6000))),
            Err(error) => return Err(format!("`{command}` couldn't finish: {error}")),
        }
    }
    Ok(())
}

fn run_label() -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    format!("{now:x}")
}

fn worktree_base(root: &Path, run: &str) -> PathBuf {
    let name = root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "repo".into());
    home_dir().join("wt").join(name).join(run)
}

struct TaskOutcome {
    id: String,
    branch: String,
    worktree: PathBuf,
    ok: bool,
    note: String,
    usage: Usage,
}

#[allow(clippy::too_many_arguments)]
async fn run_task(ctx: Arc<Flow>, root: PathBuf, base_dir: PathBuf, run: String, integration: String, task: PlanTask, verify_commands: Vec<String>) -> TaskOutcome {
    let branch = format!("olympus/{run}/task-{}", task.id);
    let worktree = base_dir.join(&task.id);
    let mut usage = Usage::default();
    let fail = |note: String, usage: Usage, worktree: PathBuf, branch: String, id: String| TaskOutcome { id, branch, worktree, ok: false, note, usage };
    if let Err(error) = git::add_worktree(&root, &worktree, &branch, &integration).await {
        return fail(format!("Couldn't create a worktree: {error:#}"), usage, worktree, branch, task.id);
    }
    let send_state = |state: TaskState, target: Option<String>, note: String| {
        let _ = ctx.tx.send(WorkflowEvent::Task { id: task.id.clone(), title: task.title.clone(), state, target, note });
    };
    let mut tier = task.tier;
    let mut session: Option<String> = None;
    let mut prompt = task.prompt.clone();
    let mut target = match ctx.rt.pick(tier, &task.prompt, &worktree).await {
        Ok(target) => target,
        Err(message) => return fail(message, usage, worktree, branch, task.id.clone()),
    };
    for attempt in 1..=MAX_TASK_ATTEMPTS {
        send_state(TaskState::Running, Some(target.to_string()), format!("Attempt {attempt}"));
        let mut req = TurnRequest::new(target.clone(), prompt.clone(), worktree.clone());
        req.role = Some(roles::IMPLEMENTER);
        req.resume = session.clone();
        req.deferred_outcome = true;
        let result = ctx.turn(&task.id, &req).await;
        usage.add(&result.usage);
        match result.stop {
            Stop::Cancelled => return fail("Stopped.".into(), usage, worktree, branch, task.id.clone()),
            Stop::AuthError => return fail(result.error.unwrap_or_else(|| "Sign-in failed.".into()), usage, worktree, branch, task.id.clone()),
            Stop::Completed => {}
            Stop::RateLimited | Stop::Failed | Stop::IdleTimeout => {
                let error = result.error.unwrap_or_else(|| "The agent failed.".into());
                match ctx.rt.pick(tier.escalate().unwrap_or(tier), &task.prompt, &worktree).await {
                    Ok(next) if attempt < MAX_TASK_ATTEMPTS => {
                        target = next;
                        session = None;
                        prompt = format!("{}\n\nA previous attempt ended with this error, and may have left partial work in this directory:\n{}", task.prompt, clip(&error, 2000));
                        continue;
                    }
                    _ => return fail(error, usage, worktree, branch, task.id.clone()),
                }
            }
        }
        session = result.session;
        send_state(TaskState::Verifying, Some(target.to_string()), String::new());
        let checked = verify(&verify_commands, &worktree).await;
        record_outcome(&ctx.rt, &result.run_id, &req, &result.usage, Some(checked.is_ok()), false).await;
        if let Err(error) = git::commit_all(&worktree, &format!("{}\n\nOlympus task {}", task.title, task.id)).await {
            return fail(format!("Couldn't commit: {error:#}"), usage, worktree, branch, task.id.clone());
        }
        match checked {
            Ok(()) => return TaskOutcome { id: task.id.clone(), branch, worktree, ok: true, note: if verify_commands.is_empty() { "No checks configured".into() } else { "Checks passed".into() }, usage },
            Err(error) if attempt == MAX_TASK_ATTEMPTS => return fail(error, usage, worktree, branch, task.id.clone()),
            Err(error) if attempt == 1 => prompt = format!("The checks failed. Fix the cause and rerun them.\n\n{error}"),
            Err(error) => {
                tier = tier.escalate().unwrap_or(tier);
                if let Ok(next) = ctx.rt.pick(tier, &task.prompt, &worktree).await {
                    target = next;
                }
                session = None;
                prompt = format!("{}\n\nAnother agent already worked on this in this directory and its checks still fail:\n{error}\n\nFind the root cause and finish the task.", task.prompt);
            }
        }
    }
    fail("Ran out of attempts.".into(), usage, worktree, branch, task.id.clone())
}

async fn resolve_conflicts(ctx: &Flow, integration_dir: &Path, files: &[String], usage: &mut Usage) -> Result<bool> {
    let target = ctx.rt.pick(Tier::Strong, "resolve merge conflicts", integration_dir).await.map_err(|e| anyhow!(e))?;
    let mut req = TurnRequest::new(target, format!("Resolve the merge conflicts in: {}", files.join(", ")), integration_dir.to_path_buf());
    req.role = Some(roles::RESOLVER);
    req.ephemeral = true;
    let result = ctx.turn("merge", &req).await;
    usage.add(&result.usage);
    if !result.ok() || !git::conflicted_files(integration_dir).await.map(|_| true).unwrap_or(false) {
        return Ok(false);
    }
    let markers = files.iter().any(|f| std::fs::read_to_string(integration_dir.join(f)).map(|t| t.contains("<<<<<<< ")).unwrap_or(false));
    Ok(!markers)
}

async fn make_plan(ctx: &Flow, goal: &str, cwd: &Path, usage: &mut Usage) -> Result<(Plan, Target)> {
    let mut last_error = String::new();
    let target = ctx.rt.pick(Tier::Standard, goal, cwd).await.map_err(|e| anyhow!(e))?;
    for _ in 0..2 {
        let mut req = TurnRequest::new(target.clone(), format!("Goal:\n{goal}\n{last_error}"), cwd.to_path_buf());
        req.access = Access::ReadOnly;
        req.role = Some(roles::PLANNER);
        req.schema = Some(plan_schema());
        req.ephemeral = true;
        let result = ctx.turn("plan", &req).await;
        usage.add(&result.usage);
        if result.stop == Stop::Cancelled {
            return Err(anyhow!("Stopped."));
        }
        if !result.ok() {
            return Err(anyhow!(result.error.unwrap_or_else(|| "The planner failed.".into())));
        }
        let parsed: Option<Plan> = result.structured.and_then(|v| serde_json::from_value(v).ok()).or_else(|| serde_json::from_str(crate::openai::extract_json(&result.text)).ok());
        match parsed {
            Some(plan) => {
                let mut problem = validate_plan(&plan).err();
                for command in &plan.verify {
                    if problem.is_none() {
                        problem = check_syntax(command, cwd).await.err();
                    }
                }
                match problem {
                    None => return Ok((plan, target)),
                    Some(problem) => last_error = format!("\nYour previous plan was rejected: {problem}"),
                }
            }
            None => last_error = "\nYour previous reply wasn't valid JSON for the schema.".into(),
        }
    }
    Err(anyhow!("The planner didn't produce a usable plan.{last_error}"))
}

/// Parses a verify command without running it, so a typo can't burn agent attempts later.
pub async fn check_syntax(command: &str, cwd: &Path) -> Result<(), String> {
    let quoted = command.replace('\'', "''");
    let script = format!("try {{ $null = [ScriptBlock]::Create('{quoted}'); exit 0 }} catch {{ Write-Output $_.Exception.Message; exit 1 }}");
    match run_shell(&script, cwd, Duration::from_secs(30)).await {
        Ok((0, _)) => Ok(()),
        Ok((_, output)) => Err(format!("`{command}` isn't valid PowerShell: {}", clip(output.trim(), 400))),
        Err(error) => Err(error),
    }
}

/// Asks a model from another vendor to review a diff; `None` when no other vendor is available.
pub async fn review_diff(rt: &Runtime, author: Option<&Target>, diff_text: &str, cwd: &Path, cancel: Cancel, emit: &mut (dyn FnMut(AgentEvent) + Send)) -> Result<Option<(Target, Review, Usage)>> {
    let reviewer = match author {
        Some(author) => rt.other_vendor(&author.provider),
        None => rt.pick(Tier::Strong, "review", cwd).await.ok(),
    };
    let Some(target) = reviewer else { return Ok(None) };
    let mut req = TurnRequest::new(target.clone(), format!("Review this diff.\n\n```diff\n{diff_text}\n```"), cwd.to_path_buf());
    req.access = Access::ReadOnly;
    req.role = Some(roles::REVIEWER);
    req.schema = Some(review_schema());
    req.ephemeral = true;
    let result = run_turn(rt, &req, cancel, emit).await;
    if !result.ok() {
        return Err(anyhow!(result.error.unwrap_or_else(|| "The reviewer failed.".into())));
    }
    let review = result.structured.and_then(|v| serde_json::from_value(v).ok()).or_else(|| serde_json::from_str(crate::openai::extract_json(&result.text)).ok()).context("The reviewer's reply wasn't valid JSON.")?;
    Ok(Some((target, review, result.usage)))
}

/// Runs a goal end to end: plan, parallel workers in worktrees, merge, verify, independent review.
pub async fn run_auto(rt: Arc<Runtime>, goal: String, cwd: PathBuf, tx: UnboundedSender<WorkflowEvent>, cancel: Cancel) -> Result<AutoOutcome> {
    let root = git::root(&cwd).await?;
    if !git::has_commits(&root).await {
        return Err(anyhow!("This repository has no commits yet. Make one first so Olympus has a base to branch from."));
    }
    let base = git::current_branch(&root).await?.ok_or_else(|| anyhow!("HEAD is detached. Check out a branch first."))?;
    if git::is_dirty(&root).await? {
        let _ = tx.send(WorkflowEvent::Note("Uncommitted changes in your working tree aren't included in this run.".into()));
    }
    let run = run_label();
    let integration = format!("olympus/{run}/main");
    let base_dir = worktree_base(&root, &run);
    let integration_dir = base_dir.join("_integration");
    git::add_worktree(&root, &integration_dir, &integration, &base).await?;

    let ctx = Arc::new(Flow { rt: rt.clone(), tx: tx.clone(), cancel: cancel.clone() });
    let mut usage = Usage::default();
    ctx.phase("Planning");
    let (plan, planner) = make_plan(&ctx, &goal, &integration_dir, &mut usage).await?;
    let mut plan = plan;
    for task in &mut plan.tasks {
        task.prompt = localize_prompt(&task.prompt, &[&integration_dir, &root]);
    }
    let _ = tx.send(WorkflowEvent::Plan(plan.clone()));
    let mut checks = plan.verify.clone();
    if checks.is_empty() {
        checks = git::detect_verify(&integration_dir);
    }
    for task in &plan.tasks {
        let _ = tx.send(WorkflowEvent::Task { id: task.id.clone(), title: task.title.clone(), state: TaskState::Pending, target: None, note: String::new() });
    }

    ctx.phase("Building");
    let tasks: HashMap<String, PlanTask> = plan.tasks.iter().map(|t| (t.id.clone(), t.clone())).collect();
    let mut merged: HashSet<String> = HashSet::new();
    let mut started: HashSet<String> = HashSet::new();
    let mut failed: Vec<(String, String)> = Vec::new();
    let mut running: JoinSet<TaskOutcome> = JoinSet::new();
    let limit = rt.config.max_parallel_agents.max(1);
    loop {
        if !ctx.cancelled() {
            for task in &plan.tasks {
                if running.len() >= limit {
                    break;
                }
                if started.contains(&task.id) || !task.deps.iter().all(|d| merged.contains(d)) {
                    continue;
                }
                started.insert(task.id.clone());
                running.spawn(run_task(ctx.clone(), root.clone(), base_dir.clone(), run.clone(), integration.clone(), task.clone(), checks.clone()));
            }
        }
        let Some(joined) = running.join_next().await else { break };
        let outcome = joined.context("a task crashed")?;
        usage.add(&outcome.usage);
        let title = tasks.get(&outcome.id).map(|t| t.title.clone()).unwrap_or_default();
        let state = |state: TaskState, note: String| {
            let _ = tx.send(WorkflowEvent::Task { id: outcome.id.clone(), title: title.clone(), state, target: None, note });
        };
        if !outcome.ok {
            state(TaskState::Failed, outcome.note.clone());
            failed.push((outcome.id.clone(), outcome.note.clone()));
            continue;
        }
        state(TaskState::Merging, String::new());
        match merge_task(&ctx, &integration_dir, &outcome, &mut usage).await {
            Ok(()) => {
                merged.insert(outcome.id.clone());
                state(TaskState::Done, outcome.note.clone());
                if git::remove_worktree(&root, &outcome.worktree).await.is_ok() {
                    let _ = git::delete_merged_branch(&root, &outcome.branch).await;
                }
            }
            Err(error) => {
                state(TaskState::Failed, format!("{error:#}"));
                failed.push((outcome.id.clone(), format!("{error:#}")));
            }
        }
    }
    for task in &plan.tasks {
        if !started.contains(&task.id) {
            let _ = tx.send(WorkflowEvent::Task { id: task.id.clone(), title: task.title.clone(), state: TaskState::Skipped, target: None, note: "A task it depends on didn't finish".into() });
        }
    }
    let outcome = |ok: bool, summary: String, usage: Usage, checks: &[String]| AutoOutcome { ok, branch: integration.clone(), base: base.clone(), summary, usage, checks_ran: checks.to_vec() };
    if ctx.cancelled() {
        return Ok(outcome(false, format!("Stopped. Finished work is on {integration}; worktrees are kept in {}.", base_dir.display()), usage, &[]));
    }
    if !failed.is_empty() {
        let lines: Vec<String> = failed.iter().map(|(id, why)| format!("- {id}: {}", clip(why, 400))).collect();
        return Ok(outcome(false, format!("{} of {} tasks failed:\n{}\nWork that finished is on {integration}; failed worktrees are kept in {}.", failed.len(), plan.tasks.len(), lines.join("\n"), base_dir.display()), usage, &[]));
    }

    let verdict = harden(&ctx, &integration_dir, &base, &planner, &checks, &mut usage).await;
    let final_ok = verdict.as_ref().map(|v| v.0).unwrap_or(false);
    let review_note = match verdict {
        Ok((_, note)) => note,
        Err(error) => format!("{error:#}"),
    };
    if final_ok {
        let _ = git::remove_worktree(&root, &integration_dir).await;
    }
    let checked = if checks.is_empty() { "No checks were configured, so nothing was run automatically.".to_string() } else { format!("Checks passed: {}", checks.join(", ")) };
    let summary = if final_ok {
        format!("{checked}\n\n{review_note}\n\nReady on {integration}. Run /merge to bring it into {base}.")
    } else {
        format!("{}\n{review_note}\nThe work is on {integration}, in {}.", plan.summary, integration_dir.display())
    };
    Ok(outcome(final_ok, summary, usage, &checks))
}

async fn merge_task(ctx: &Flow, integration_dir: &Path, outcome: &TaskOutcome, usage: &mut Usage) -> Result<()> {
    if git::commit_all(integration_dir, "Keep changes made outside a task's worktree").await? {
        let _ = ctx.tx.send(WorkflowEvent::Note("A task edited the integration branch directly. Those changes were kept.".into()));
    }
    match git::merge(integration_dir, &outcome.branch).await? {
        Merge::Clean => Ok(()),
        Merge::Conflict(files) => {
            ctx.phase(format!("Resolving conflicts in {}", files.join(", ")));
            if resolve_conflicts(ctx, integration_dir, &files, usage).await? {
                git::finish_merge(integration_dir).await
            } else {
                git::abort_merge(integration_dir).await?;
                Err(anyhow!("Couldn't resolve conflicts in {}; the task's branch {} is intact.", files.join(", "), outcome.branch))
            }
        }
    }
}

async fn harden(ctx: &Flow, dir: &Path, base: &str, author: &Target, checks: &[String], usage: &mut Usage) -> Result<(bool, String)> {
    let rounds = ctx.rt.config.max_fix_rounds;
    let mut review_note = String::new();
    for round in 0..=rounds {
        ctx.phase("Verifying");
        if let Err(error) = verify(checks, dir).await {
            if round == rounds || ctx.cancelled() {
                return Ok((false, format!("Checks still fail:\n{error}")));
            }
            fix(ctx, dir, &format!("Checks fail on the integrated work:\n{error}"), usage).await?;
            continue;
        }
        ctx.phase("Independent review");
        let diff_text = git::diff(dir, base, 60_000).await?;
        if diff_text.is_empty() {
            return Ok((true, "No changes were needed.".into()));
        }
        let tx = ctx.tx.clone();
        let reviewed = review_diff(&ctx.rt, Some(author), &diff_text, dir, ctx.cancel.clone(), &mut move |event| {
            let _ = tx.send(WorkflowEvent::Agent { task: "review".into(), event });
        })
        .await;
        let (reviewer, review, review_usage) = match reviewed {
            Ok(Some(found)) => found,
            Ok(None) => return Ok((true, "Independent review was skipped: no other vendor is available.".into())),
            Err(error) => return Ok((true, format!("Independent review couldn't finish: {error:#}"))),
        };
        usage.add(&review_usage);
        let high: Vec<&Finding> = review.findings.iter().filter(|f| f.severity == "high").collect();
        let others = review.findings.len() - high.len();
        if review.approved || high.is_empty() {
            let suffix = match others {
                0 => String::new(),
                1 => ", with 1 minor note".to_string(),
                n => format!(", with {n} minor notes"),
            };
            return Ok((true, format!("Reviewed by {reviewer}: approved{suffix}.")));
        }
        let list: Vec<String> = high.iter().map(|f| format!("- {}: {}", f.file, f.summary)).collect();
        review_note = format!("Reviewed by {reviewer}: not approved.\n{}", list.join("\n"));
        if round == rounds || ctx.cancelled() {
            break;
        }
        fix(ctx, dir, &format!("An independent reviewer found these defects. Verify each one and fix the real ones:\n{}", list.join("\n")), usage).await?;
    }
    Ok((false, review_note))
}

async fn fix(ctx: &Flow, dir: &Path, problem: &str, usage: &mut Usage) -> Result<()> {
    ctx.phase("Fixing");
    let target = ctx.rt.pick(Tier::Strong, problem, dir).await.map_err(|e| anyhow!(e))?;
    let mut req = TurnRequest::new(target, problem, dir.to_path_buf());
    req.role = Some(roles::FIXER);
    let result = ctx.turn("fix", &req).await;
    usage.add(&result.usage);
    if !result.ok() {
        return Err(anyhow!(result.error.unwrap_or_else(|| "The fixer failed.".into())));
    }
    git::commit_all(dir, "Fix issues found during verification").await?;
    Ok(())
}

/// Brings a finished run's branch into the checked-out branch.
pub async fn land(root: &Path, branch: &str) -> Result<String> {
    match git::merge(root, branch).await? {
        Merge::Clean => {
            let _ = git::delete_merged_branch(root, branch).await;
            Ok(format!("Merged {branch}."))
        }
        Merge::Conflict(files) => {
            git::abort_merge(root).await?;
            Err(anyhow!("Merging {branch} conflicts in {}. The merge was undone and nothing changed.", files.join(", ")))
        }
    }
}

/// Asks a model to turn the user's corrections into candidate lessons; returns how many were added.
pub async fn distill(rt: &Runtime, cwd: &Path, cancel: Cancel) -> Result<usize, String> {
    let store = rt.learn.clone().ok_or("The learning store isn't available.")?;
    let corrections = store.lock().map_err(|_| "The learning store is unavailable.")?.recent_corrections(150).map_err(|e| e.to_string())?;
    if corrections.is_empty() {
        return Ok(0);
    }
    let bundle: Vec<String> = corrections.iter().map(|c| format!("- {}", c.lines().next().unwrap_or(""))).collect();
    let target = rt.pick(Tier::Standard, "summarize preferences", cwd).await?;
    let mut req = TurnRequest::new(target, clip(&bundle.join("\n"), 40_000), cwd.to_path_buf());
    req.access = Access::ReadOnly;
    req.role = Some(roles::DISTILLER);
    req.schema = Some(json!({
        "type": "object", "additionalProperties": false, "required": ["lessons"],
        "properties": {"lessons": {"type": "array", "items": {
            "type": "object", "additionalProperties": false, "required": ["text", "kind"],
            "properties": {"text": {"type": "string"}, "kind": {"type": "string", "enum": ["preference", "constraint"]}}
        }}}
    }));
    req.ephemeral = true;
    req.use_memory = false;
    let result = run_turn(rt, &req, cancel, &mut |_| {}).await;
    if !result.ok() {
        return Err(result.error.unwrap_or_else(|| "The model failed.".into()));
    }
    let parsed = result.structured.or_else(|| serde_json::from_str(crate::openai::extract_json(&result.text)).ok()).ok_or("The model's reply wasn't valid JSON.")?;
    let mut added = 0;
    let store = store.lock().map_err(|_| "The learning store is unavailable.")?;
    for lesson in parsed["lessons"].as_array().into_iter().flatten() {
        let Some(text) = lesson["text"].as_str().filter(|t| !t.trim().is_empty()) else { continue };
        let kind = if lesson["kind"] == "constraint" { LessonKind::Constraint } else { LessonKind::Preference };
        let new = NewLesson { text: text.trim().to_string(), kind, scope: None, provenance_file: None, provenance_hash: None, state: LessonState::Candidate };
        if store.add_lesson(new).is_ok() {
            added += 1;
        }
    }
    Ok(added)
}

/// One model's answer in a council.
#[derive(Debug, Clone)]
pub struct CouncilAnswer {
    pub target: Target,
    pub text: Option<String>,
    pub error: Option<String>,
}

/// A council's combined result.
#[derive(Debug, Clone)]
pub struct CouncilOutcome {
    pub answers: Vec<CouncilAnswer>,
    pub synthesis: String,
    pub usage: Usage,
}

/// Asks several models independently, read-only, then synthesizes with a cheap model.
pub async fn council(rt: Arc<Runtime>, prompt: &str, names: &[String], cwd: &Path, cancel: Cancel, emit: &mut (dyn FnMut(AgentEvent) + Send)) -> Result<CouncilOutcome, String> {
    let targets: Vec<Target> = if names.is_empty() {
        let mut picks: Vec<Target> = Vec::new();
        for spec in &rt.config.tiers.strong {
            if let Ok(target) = Target::parse(spec) {
                if rt.provider_ready(&target.provider) && !picks.iter().any(|p| p.provider == target.provider) {
                    picks.push(target);
                }
            }
        }
        picks
    } else {
        names.iter().map(|n| rt.resolve(n)).collect::<Result<_, _>>()?
    };
    if targets.is_empty() {
        return Err("No models are available for a council.".into());
    }
    let mut set = JoinSet::new();
    for (index, target) in targets.iter().cloned().enumerate() {
        let (rt, prompt, cwd, cancel) = (rt.clone(), prompt.to_string(), cwd.to_path_buf(), cancel.clone());
        set.spawn(async move {
            let mut req = TurnRequest::new(target.clone(), prompt, cwd);
            req.access = Access::ReadOnly;
            req.role = Some(roles::CONSULTANT);
            req.ephemeral = true;
            req.depth += 1;
            (index, target, run_turn(&rt, &req, cancel, &mut |_| {}).await)
        });
    }
    let mut ordered: Vec<Option<CouncilAnswer>> = vec![None; targets.len()];
    let mut usage = Usage::default();
    while let Some(joined) = set.join_next().await {
        let (index, target, result) = joined.map_err(|e| e.to_string())?;
        usage.add(&result.usage);
        emit(AgentEvent::Status(format!("{target} {}", if result.ok() { "answered" } else { "failed" })));
        ordered[index] = Some(if result.ok() {
            CouncilAnswer { target, text: Some(result.text), error: None }
        } else {
            CouncilAnswer { target, text: None, error: result.error }
        });
    }
    let answers: Vec<CouncilAnswer> = ordered.into_iter().flatten().collect();
    let good: Vec<&CouncilAnswer> = answers.iter().filter(|a| a.text.is_some()).collect();
    if good.is_empty() {
        return Err("Every model in the council failed.".into());
    }
    if good.len() == 1 {
        let synthesis = good[0].text.clone().unwrap_or_default();
        return Ok(CouncilOutcome { answers, synthesis, usage });
    }
    let bundle: Vec<String> = good.iter().map(|a| format!("## {}\n{}", a.target, a.text.as_deref().unwrap_or(""))).collect();
    let synth_target = rt.pick(Tier::Cheap, prompt, cwd).await?;
    let mut req = TurnRequest::new(synth_target, format!("Question:\n{prompt}\n\n{}", bundle.join("\n\n")), cwd.to_path_buf());
    req.access = Access::ReadOnly;
    req.role = Some(roles::SYNTHESIZER);
    req.ephemeral = true;
    req.depth += 1;
    req.use_memory = false;
    let result = run_turn(&rt, &req, cancel, emit).await;
    usage.add(&result.usage);
    let synthesis = if result.ok() { result.text } else { bundle.join("\n\n") };
    Ok(CouncilOutcome { answers, synthesis, usage })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, deps: &[&str]) -> PlanTask {
        PlanTask { id: id.into(), title: id.into(), prompt: "p".into(), deps: deps.iter().map(|d| d.to_string()).collect(), tier: Tier::Cheap }
    }

    fn plan(tasks: Vec<PlanTask>) -> Plan {
        Plan { summary: "s".into(), tasks, verify: vec![] }
    }

    #[test]
    fn accepts_a_valid_dag() {
        assert!(validate_plan(&plan(vec![task("a", &[]), task("b", &["a"]), task("c", &["a", "b"])])).is_ok());
    }

    #[test]
    fn rejects_bad_plans() {
        assert!(validate_plan(&plan(vec![])).is_err());
        assert!(validate_plan(&plan(vec![task("a", &[]), task("a", &[])])).expect_err("dup").contains("twice"));
        assert!(validate_plan(&plan(vec![task("a", &["ghost"])])).expect_err("unknown").contains("ghost"));
        assert!(validate_plan(&plan(vec![task("a", &["b"]), task("b", &["a"])])).expect_err("cycle").contains("cycle"));
        assert!(validate_plan(&plan(vec![task("a b", &[])])).expect_err("branch").contains("branch"));
    }

    #[test]
    fn schemas_satisfy_strict_structured_output() {
        fn check(schema: &Value) {
            if schema["type"] == "object" {
                assert_eq!(schema["additionalProperties"], false);
                let required = schema["required"].as_array().expect("required").len();
                assert_eq!(required, schema["properties"].as_object().expect("properties").len());
                for child in schema["properties"].as_object().expect("properties").values() {
                    check(child);
                }
            } else if schema["type"] == "array" {
                check(&schema["items"]);
            }
        }
        check(&plan_schema());
        check(&review_schema());
    }

    #[test]
    fn prompts_lose_absolute_repo_paths_in_every_spelling() {
        let dir = Path::new(r"C:\Users\me\wt\_integration");
        for prompt in [
            r"Edit C:\Users\me\wt\_integration\src\a.rs now",
            r"Edit C:/Users/me/wt/_integration/src\a.rs now",
            r"Edit /c/Users/me/wt/_integration/src\a.rs now",
        ] {
            assert_eq!(localize_prompt(prompt, &[dir]), r"Edit src\a.rs now", "{prompt}");
        }
        assert_eq!(localize_prompt("Work in C:/Users/me/wt/_integration.", &[dir]), "Work in ..");
        assert_eq!(localize_prompt("Touch C:/other/a.rs", &[dir]), "Touch C:/other/a.rs");
    }

    #[tokio::test]
    async fn syntax_check_rejects_what_powershell_5_cannot_parse() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(check_syntax("cargo test --quiet", dir.path()).await.is_ok());
        assert!(check_syntax("if (-not (Test-Path 'a b')) { exit 1 }", dir.path()).await.is_ok());
        assert!(check_syntax("git status || Test-Path x", dir.path()).await.expect_err("|| fails").contains("valid PowerShell"));
    }

    #[test]
    fn plan_parses_tiers_from_planner_json() {
        let parsed: Plan = serde_json::from_value(json!({"summary": "s", "verify": ["cargo test"], "tasks": [{"id": "a", "title": "t", "prompt": "p", "deps": [], "tier": "strong"}]})).expect("plan");
        assert_eq!(parsed.tasks[0].tier, Tier::Strong);
    }

    #[tokio::test]
    async fn verify_stops_at_the_first_failure_and_reports_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("ran-second");
        let commands = vec!["Write-Output boom; exit 2".to_string(), format!("Set-Content '{}' x", marker.display())];
        let error = verify(&commands, dir.path()).await.expect_err("fails");
        assert!(error.contains("exited 2") && error.contains("boom"));
        assert!(!marker.exists());
        assert!(verify(&[], dir.path()).await.is_ok());
    }

    #[tokio::test]
    async fn land_undoes_a_conflicting_merge() {
        let repo = crate::git::testing::repo().await;
        let parent = tempfile::tempdir().expect("tempdir");
        let tree = parent.path().join("t");
        git::add_worktree(repo.path(), &tree, "work", "main").await.expect("add");
        std::fs::write(tree.join("a.txt"), "theirs\n").expect("write");
        git::commit_all(&tree, "theirs").await.expect("commit");
        git::remove_worktree(repo.path(), &tree).await.expect("remove");
        std::fs::write(repo.path().join("a.txt"), "ours\n").expect("write");
        git::commit_all(repo.path(), "ours").await.expect("commit");
        assert!(land(repo.path(), "work").await.is_err());
        assert_eq!(std::fs::read_to_string(repo.path().join("a.txt")).expect("read"), "ours\n");
        assert!(!git::is_dirty(repo.path()).await.expect("clean"));
    }
}
