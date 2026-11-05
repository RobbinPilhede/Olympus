use crate::app::{Action, App, AppEvent, PickItem, Picker};
use crate::hotkeys;
use crate::session::{short_duration, Activity, Block, Chat, Choice, Loop, Pane, TaskRow};
use olympus_core::agent::{run_turn, Access, TurnRequest, ATTACHED};
use olympus_core::config::{home_dir, user_home};
use olympus_core::discovery;
use olympus_core::node::{self, NodeClient};
use olympus_core::proc::{cancel_pair, scrub_env};
use olympus_core::roles;
use olympus_core::skills;
use olympus_core::target::{Provider, Target};
use olympus_core::workflow;
use olympus_learn::{LessonKind, LessonState, NewLesson, Store};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A slash command.
pub struct Command {
    pub name: &'static str,
    pub usage: &'static str,
    pub about: &'static str,
    pub takes_args: bool,
}

/// One row of the `/` list: an Olympus command or something passed on to the model.
#[derive(Clone)]
pub struct Hint {
    pub name: String,
    pub usage: String,
    pub about: String,
    pub takes_args: bool,
}

impl From<&Command> for Hint {
    fn from(command: &Command) -> Self {
        Self { name: command.name.to_string(), usage: command.usage.to_string(), about: command.about.to_string(), takes_args: command.takes_args }
    }
}

// Claude Code's own commands that aren't files on disk and work without a terminal.
const CLAUDE_COMMANDS: &[(&str, &str)] = &[
    ("code-review", "Review the current diff for bugs"),
    ("simplify", "Clean up the changed code"),
    ("debug", "Track down a bug"),
    ("verify", "Check that a change really works"),
    ("batch", "Make one change across many files"),
    ("run", "Launch the app to see a change working"),
    ("schedule", "Create scheduled cloud agents"),
    ("init", "Write a CLAUDE.md for this project"),
    ("compact", "Shrink this conversation's context"),
    ("context", "Show what's using the context"),
    ("insights", "Report on your Claude Code sessions"),
    ("recap", "Recap this session"),
    ("goal", "Work towards a goal"),
    ("claude-api", "Claude API reference"),
    ("update-config", "Change Claude Code's settings"),
    ("fewer-permission-prompts", "Allow your common read-only commands"),
    ("workflow-authoring", "Reference for writing workflows"),
    ("plugin-authoring", "Make a Claude Code mod"),
];

/// Skills and Claude Code commands for the `/` list; Olympus's own commands win a name clash.
pub fn skill_hints(cwd: &std::path::Path) -> Vec<Hint> {
    let skills = skills::everything(&user_home(), cwd);
    let found = skills.into_iter().map(|skill| (skill.name, skill.description));
    let built_in = CLAUDE_COMMANDS.iter().map(|(name, about)| ((*name).to_string(), (*about).to_string()));
    let mut hints: Vec<Hint> = found
        .chain(built_in)
        .map(|(name, about)| Hint { usage: format!("/{name}"), name: format!("/{name}"), about, takes_args: true })
        .filter(|hint| !COMMANDS.iter().any(|c| c.name == hint.name))
        .collect();
    hints.sort_by(|a, b| a.name.cmp(&b.name));
    hints.dedup_by(|a, b| a.name == b.name);
    hints
}

fn is_passed_on(name: &str, cwd: &std::path::Path) -> bool {
    let name = name.trim_start_matches('/');
    CLAUDE_COMMANDS.iter().any(|(known, _)| *known == name) || skills::everything(&user_home(), cwd).iter().any(|skill| skill.name == name)
}

/// Readies a `/name` message for its provider; true means it needs your full Claude setup.
pub fn slash_prompt(target: &Target, prompt: String, cwd: &std::path::Path) -> Result<(String, bool), String> {
    if !prompt.starts_with('/') {
        return Ok((prompt, false));
    }
    if target.provider == Provider::Claude {
        return Ok((prompt, true));
    }
    let (name, args) = prompt.split_once(char::is_whitespace).unwrap_or((&prompt, ""));
    let name = name.trim_start_matches('/');
    let all = skills::everything(&user_home(), cwd);
    let Some(skill) = all.iter().find(|skill| skill.name == name) else {
        return Err(format!("/{name} is a Claude Code command, so it only runs on Claude. Switch with /model claude."));
    };
    skills::prompt_for(skill, args.trim()).map(|text| (text, false)).map_err(|error| format!("Unable to read {}: {error}", skill.path.display()))
}

/// Writes a message's attached files where every provider's model can find them.
pub fn with_attachments(text: &str, files: &[PathBuf]) -> String {
    let text = if text.is_empty() { "See the attached files." } else { text };
    let list: Vec<String> = files.iter().map(|file| format!("{ATTACHED}{}", file.display())).collect();
    format!("{text}\n\n{}", list.join("\n"))
}

fn hotkeys_command(app: &mut App, args: &str) {
    let marker = home_dir().join(crate::app::HOTKEYS_OFF);
    match args {
        "on" => {
            hotkeys::set_enabled(true);
            let _ = std::fs::remove_file(marker);
            app.note_active("Hold Shift+V to dictate, or press it twice to keep listening until you press it again. Hold Shift+C or press it twice to screenshot. A single tap still types the capital.");
        }
        "off" => {
            hotkeys::set_enabled(false);
            // If this can't be written, it's still off until the next launch.
            let _ = std::fs::write(marker, "");
            app.note_active("Shift+V and Shift+C are ordinary keys again. /screenshot still works.");
        }
        _ => app.fail_active("Use /hotkeys on or /hotkeys off."),
    }
}

/// Added to a self-paced loop's message so the model says when to run again.
pub const LOOP_PACING: &str = "This message repeats on a loop. When you finish this run, end your reply with one line: `LOOP: next 20m` with how long to wait before the next run, or `LOOP: done` if there is nothing left to do.";

const DEFAULT_LOOP_PACE: Duration = Duration::from_secs(20 * 60);
const SHORTEST_LOOP: Duration = Duration::from_secs(60);
const LONGEST_PACE: Duration = Duration::from_secs(24 * 3600);

/// How long a self-paced loop waits, from the model's reply; None means it's finished.
pub fn loop_pace(reply: &str) -> Option<Duration> {
    let said = reply.lines().rev().find_map(|line| line.trim().trim_matches('`').strip_prefix("LOOP:").map(str::trim));
    match said {
        Some("done") => None,
        Some(next) => Some(next.strip_prefix("next").and_then(parse_duration).unwrap_or(DEFAULT_LOOP_PACE).clamp(SHORTEST_LOOP, LONGEST_PACE)),
        None => Some(DEFAULT_LOOP_PACE),
    }
}

fn parse_duration(text: &str) -> Option<Duration> {
    let text: String = text.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_lowercase();
    let split = text.find(|c: char| !c.is_ascii_digit())?;
    let (number, unit) = text.split_at(split);
    let number: u64 = number.parse().ok().filter(|n| *n > 0)?;
    let seconds = match unit {
        "s" | "sec" | "secs" | "second" | "seconds" => 1,
        "m" | "min" | "mins" | "minute" | "minutes" => 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3600,
        "d" | "day" | "days" => 86400,
        _ => return None,
    };
    number.checked_mul(seconds).map(Duration::from_secs)
}

// `5m message` or `message every 5 minutes`; "check every PR" has no time, so it stays a message.
fn parse_loop(args: &str) -> (Option<Duration>, String) {
    let args = args.trim();
    if let Some((first, rest)) = args.split_once(char::is_whitespace) {
        if let Some(every) = parse_duration(first) {
            return (Some(every), rest.trim().to_string());
        }
    } else if parse_duration(args).is_some() {
        return (None, String::new());
    }
    if let Some((message, tail)) = args.rsplit_once(" every ") {
        if let Some(every) = parse_duration(tail) {
            return (Some(every), message.trim().to_string());
        }
    }
    (None, args.to_string())
}

fn loop_command(app: &mut App, args: &str) {
    const USAGE: &str = "Say what to repeat, like /loop 5m check the deploy. Use /loop stop to end it.";
    if args == "stop" {
        if !app.stop_active() {
            app.note_active("No loop is running here.");
        }
        return;
    }
    let (every, prompt) = parse_loop(args);
    if prompt.is_empty() {
        app.fail_active(USAGE);
        return;
    }
    if !app.active_is_chat() {
        app.fail_active("Open a chat to start a loop.");
        return;
    }
    if prompt.starts_with('/') {
        let name = prompt.split_whitespace().next().unwrap_or_default();
        if COMMANDS.iter().any(|c| c.name == name) || !is_passed_on(name, &app.current_cwd()) {
            app.fail_active(format!("/loop repeats a message or a skill, not {name}."));
            return;
        }
    }
    let every = every.map(|every| every.max(SHORTEST_LOOP));
    let id = app.active_id();
    let Some(chat) = app.chat() else { return };
    let generation = chat.looping.as_ref().map_or(0, |previous| previous.generation + 1);
    chat.looping = Some(Loop { prompt: prompt.clone(), every, started: None, generation });
    let pace = every.map_or_else(|| "it decides how long to wait".to_string(), |every| format!("every {}", short_duration(every)));
    chat.push(Block::Note(format!("Looping, {pace}, until you run /loop stop. It runs only while Olympus is open.")));
    app.send(id, prompt);
}

const fn cmd(name: &'static str, usage: &'static str, about: &'static str, takes_args: bool) -> Command {
    Command { name, usage, about, takes_args }
}

/// Every slash command, for hints, the palette and /help.
pub const COMMANDS: &[Command] = &[
    cmd("/auto", "/auto <goal>", "Plan, build in parallel, verify and review", true),
    cmd("/ask", "/ask <model> <question>", "Ask another model, read-only", true),
    cmd("/council", "/council <question>", "Ask several models, get one recommendation", true),
    cmd("/review", "/review", "Have another vendor review uncommitted changes", false),
    cmd("/merge", "/merge", "Bring the last /auto branch into this branch", false),
    cmd("/changes", "/changes", "Show or hide this chat's changes", false),
    cmd("/grid", "/grid", "Show several windows at once", false),
    cmd("/copy", "/copy", "Copy the latest answer", false),
    cmd("/details", "/details", "Show or hide the latest tool's details", false),
    cmd("/model", "/model [name|auto]", "Choose the model for this chat", true),
    cmd("/profile", "/profile lean|full", "Trim startup tokens, or load your whole setup", true),
    cmd("/new", "/new [folder]", "Open a new chat", true),
    cmd("/cd", "/cd <folder>", "Move this chat to another folder", true),
    cmd("/clear", "/clear", "Start this chat over with fresh context", false),
    cmd("/close", "/close", "Close this window", false),
    cmd("/reopen", "/reopen", "Bring back the last closed chat", false),
    cmd("/rename", "/rename <title>", "Rename this window", true),
    cmd("/claude", "/claude", "Open the real Claude Code, permissions bypassed", false),
    cmd("/codex", "/codex", "Open the real Codex, permissions bypassed", false),
    cmd("/term", "/term [command]", "Open a terminal window", true),
    cmd("/self-improve", "/self-improve", "Open Claude Code on Olympus's own source, permissions bypassed", false),
    cmd("/sessions", "/sessions", "Reopen a past Claude or Codex session", false),
    cmd("/account", "/account [add [chatgpt] <name>]", "Choose this chat's Claude or ChatGPT account, or sign in to another", true),
    cmd("/accounts", "/accounts", "Show which accounts are signed in", false),
    cmd("/usage", "/usage", "Show quotas, token spend and memory results", false),
    cmd("/nodes", "/nodes", "Find Olympus nodes on this network", false),
    cmd("/hf", "/hf <search>", "Find a Hugging Face model for the selected node", true),
    cmd("/pf", "/pf <search>", "Find a model torrent on Pirate Face for the selected node", true),
    cmd("/load", "/load [name]", "Load a model already on the selected node", true),
    cmd("/models", "/models", "List the models on the selected node", false),
    cmd("/unload", "/unload", "Unload the selected node's model", false),
    cmd("/skills", "/skills [search]", "List installed skills", true),
    cmd("/learn", "/learn [lessons|distill|history <q>]", "Import history, find lessons, review them", true),
    cmd("/approve", "/approve <id>", "Let a candidate lesson be used", true),
    cmd("/reject", "/reject <id>", "Retire a lesson", true),
    cmd("/remember", "/remember <fact>", "Teach Olympus something directly", true),
    cmd("/loop", "/loop [5m] <message or /skill>|stop", "Repeat a message on a timer, or let the model pace it", true),
    cmd("/screenshot", "/screenshot", "Drag a region of the screen into this message", false),
    cmd("/hotkeys", "/hotkeys on|off", "Shift+V to dictate and Shift+C to screenshot, in any app", true),
    cmd("/stop", "/stop", "Stop what this chat is doing", false),
    cmd("/notify", "/notify on|off|test", "Turn the finished sound and notifications on or off, or try them", true),
    cmd("/help", "/help", "List commands and keys", false),
    cmd("/quit", "/quit", "Leave Olympus", false),
];

fn expand_path(base: &std::path::Path, text: &str) -> PathBuf {
    let text = text.trim().trim_matches('"');
    let expanded = match text.strip_prefix("~") {
        Some(rest) => user_home().join(rest.trim_start_matches(['/', '\\'])),
        None => PathBuf::from(text),
    };
    if expanded.is_absolute() { expanded } else { base.join(expanded) }
}

/// Builds the everything palette: windows first, then commands.
pub fn palette(app: &App) -> Picker {
    let mut items: Vec<PickItem> = app
        .windows
        .iter()
        .map(|w| PickItem { label: w.title().to_string(), detail: "window".into(), action: Action::Switch(w.id) })
        .collect();
    items.extend(COMMANDS.iter().map(|c| PickItem {
        label: c.usage.to_string(),
        detail: c.about.to_string(),
        action: if c.takes_args { Action::Prefill(format!("{} ", c.name)) } else { Action::Run(c.name.to_string()) },
    }));
    Picker::new("Olympus", items, "")
}

/// Carries out a picker choice.
pub fn pick(app: &mut App, action: Action) {
    match action {
        Action::Switch(id) => {
            if let Some(index) = app.windows.iter().position(|w| w.id == id) {
                app.active = index;
            }
        }
        Action::Run(command) => run(app, &command),
        Action::Prefill(text) => {
            if !app.active_is_chat() {
                match app.windows.iter().rposition(|w| matches!(w.pane, Pane::Chat(_))) {
                    Some(index) => app.active = index,
                    None => {
                        let cwd = app.current_cwd();
                        app.new_chat(cwd);
                    }
                }
            }
            app.composer.set(&text);
        }
        Action::Resume { vendor, id, cwd, title } => {
            let target = app.rt.resolve(&vendor).ok();
            let choice = target.clone().map_or(Choice::Auto, Choice::Fixed);
            let mut chat = Chat::new(title, cwd, choice);
            chat.state.vendor_session = Some(id);
            chat.state.last_target = target;
            chat.push(Block::Note("Reopened. Your next message continues this session.".into()));
            app.add_window(Pane::Chat(Box::new(chat)));
        }
        Action::SelectNode { name, address } => select_node(app, name, address),
        Action::HfRepo(repo) => hf_files(app, repo, None),
        Action::HfQuant { repo, quant } => hf_pull(app, repo, quant, None),
        Action::PfRepo { repo, magnet } => hf_files(app, repo, Some(magnet)),
        Action::PfQuant { repo, quant, magnet } => hf_pull(app, repo, quant, Some(magnet)),
        Action::LoadModel(id) => load_model(app, id),
        Action::UseAccount(provider, dir) => use_account(app, &provider, dir),
        Action::NewChatOn(provider, dir) => folder_picker(app, provider, dir),
        Action::NewChatIn { provider, account, cwd } => {
            app.new_chat(cwd);
            set_account(app, &provider, account);
        }
        Action::BrowseFolder(provider, dir) => app.pending_folder = Some((provider, dir)),
        Action::AddAccount(provider) => {
            let name = next_account_name(&user_home(), account_kind(&provider));
            add_account(app, &provider, &name);
        }
    }
}

/// How one provider keeps its signed-in accounts.
struct AccountKind {
    word: &'static str,
    cli: &'static str,
    resolve: &'static str,
    env: &'static str,
    folder: &'static str,
    credentials: &'static str,
    instructions: &'static str,
    sign_in: &'static [&'static str],
    sign_in_title: &'static str,
    add: &'static str,
    missing: &'static str,
}

const CLAUDE_ACCOUNTS: AccountKind = AccountKind {
    word: "Claude",
    cli: "Claude Code",
    resolve: "claude",
    env: "CLAUDE_CONFIG_DIR",
    folder: ".claude",
    credentials: ".credentials.json",
    instructions: "CLAUDE.md",
    sign_in: &[],
    sign_in_title: "Sign in",
    add: "/account add",
    missing: "Claude Code wasn't found. Install it, then restart Olympus.",
};

const CODEX_ACCOUNTS: AccountKind = AccountKind {
    word: "ChatGPT",
    cli: "Codex",
    resolve: "codex",
    env: "CODEX_HOME",
    folder: ".codex",
    credentials: "auth.json",
    instructions: "AGENTS.md",
    sign_in: &["login"],
    sign_in_title: "Codex sign in",
    add: "/account add chatgpt",
    missing: "Codex wasn't found. Install it, then restart Olympus.",
};

fn account_kind(provider: &Provider) -> &'static AccountKind {
    if *provider == Provider::Codex {
        &CODEX_ACCOUNTS
    } else {
        &CLAUDE_ACCOUNTS
    }
}

/// The account folder a turn may use; each CLI reads this as its own home, so it must never get the other's.
pub fn account_for(target: &Target, claude: Option<PathBuf>, codex: Option<PathBuf>) -> Option<PathBuf> {
    match target.provider {
        Provider::Claude => claude,
        Provider::Codex => codex,
        _ => None,
    }
}

/// Claude and Codex folders besides the normal `~/.claude` and `~/.codex`.
pub fn extra_account_folders() -> Vec<PathBuf> {
    let is_account = |name: &str| name.starts_with(".claude-") || name.starts_with(".codex-");
    std::fs::read_dir(user_home()).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir() && p.file_name().is_some_and(|n| is_account(&n.to_string_lossy()))).collect()
}

// A folder without credentials is a sign-in that was never finished, so it gets reused.
fn next_account_name(home: &std::path::Path, kind: &AccountKind) -> String {
    (2..).map(|n| format!("account{n}")).find(|name| !home.join(format!("{}-{name}", kind.folder)).join(kind.credentials).exists()).unwrap_or_default()
}

// An unrouted auto chat counts as being on whatever it would most likely use.
fn thread_provider(app: &mut App) -> Option<Provider> {
    let likely = app.rt.likely_target().map(|target| target.provider);
    app.chat().and_then(|chat| chat.state.provider().cloned()).or(likely)
}

fn valid_account_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn use_account(app: &mut App, provider: &Provider, dir: Option<PathBuf>) {
    let name = crate::session::account_name(dir.as_deref());
    set_account(app, provider, dir);
    let Some(chat) = app.chat() else { return };
    chat.push(Block::Note(format!("Using the {name} {} account. The next message starts fresh on it.", account_kind(provider).word)));
    chat.follow = true;
}

fn set_account(app: &mut App, provider: &Provider, dir: Option<PathBuf>) {
    let kind = account_kind(provider);
    // Picking another provider's account means you want this thread on that provider.
    let moved = (thread_provider(app).as_ref() != Some(provider)).then(|| app.rt.resolve(kind.resolve).ok()).flatten();
    let Some(chat) = app.chat() else { return };
    if *provider == Provider::Codex {
        chat.state.codex_account_dir = dir;
    } else {
        chat.state.account_dir = dir;
    }
    if let Some(target) = moved {
        chat.state.choice = Choice::Fixed(target);
        chat.state.last_target = None;
    }
    // Each CLI keeps sessions per account folder, so the old session can't resume under the new one.
    chat.state.vendor_session = None;
}

fn add_account(app: &mut App, provider: &Provider, name: &str) {
    if !valid_account_name(name) {
        app.fail_active("Use a short name with only letters, numbers, - or _, like /account add work.");
        return;
    }
    let kind = account_kind(provider);
    let bin = if *provider == Provider::Codex { app.rt.codex.as_ref() } else { app.rt.claude.as_ref() };
    let Some(bin) = bin.map(|b| b.path.clone()) else {
        app.fail_active(kind.missing);
        return;
    };
    let dir = user_home().join(format!("{}-{name}", kind.folder));
    if let Err(error) = std::fs::create_dir_all(&dir) {
        app.fail_active(format!("Unable to create {}: {error}", dir.display()));
        return;
    }
    if !dir.join(kind.instructions).exists() {
        // Your global instructions should follow you; a missing file just means there are none.
        let _ = std::fs::copy(user_home().join(kind.folder).join(kind.instructions), dir.join(kind.instructions));
    }
    app.account_folders = extra_account_folders();
    let cwd = app.current_cwd();
    let path = bin.display().to_string();
    let credentials = dir.join(kind.credentials);
    if !credentials.exists() {
        let tx = app.tx.clone();
        tokio::spawn(async move {
            for _ in 0..900 {
                tokio::time::sleep(Duration::from_secs(2)).await;
                if credentials.exists() {
                    let _ = tx.send(AppEvent::AccountAdded);
                    return;
                }
            }
        });
    }
    let args: Vec<String> = kind.sign_in.iter().map(|arg| (*arg).to_string()).collect();
    match app.new_term_with_env(&format!("{}: {name}", kind.sign_in_title), &path, &args, cwd, Some((kind.env, &dir))) {
        Ok(_) => app.status = "Finish signing in here.".to_string(),
        Err(error) => app.fail_active(format!("{error:#}")),
    }
}

fn choose_account(app: &mut App) {
    account_picker(app, "Accounts", false);
}

/// Asks which account a new chat starts on; nothing opens until you choose.
pub fn choose_new_chat_account(app: &mut App) {
    account_picker(app, "Start the new chat on", true);
}

// Throwaway worktrees live under ~/.olympus, and Windows paths differ only by case or slash.
fn recent_folders(home: &std::path::Path, candidates: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let key = |path: &std::path::Path| path.to_string_lossy().to_lowercase().replace('/', "\\").trim_end_matches('\\').to_string();
    let hidden = home.join(".olympus");
    let mut seen = std::collections::HashSet::from([key(home)]);
    candidates.into_iter().filter(|folder| !folder.starts_with(&hidden) && seen.insert(key(folder)) && folder.is_dir()).take(12).collect()
}

fn folder_picker(app: &mut App, provider: Provider, account: Option<PathBuf>) {
    const TITLE: &str = "Start the new chat in";
    let open_threads = app.windows.iter().filter_map(|window| match &window.pane {
        Pane::Chat(chat) => Some(chat.state.cwd.clone()),
        _ => None,
    });
    let open: Vec<PathBuf> = std::iter::once(app.current_cwd()).chain(open_threads).collect();
    let tx = app.tx.clone();
    app.picker = Some(Picker::loading(TITLE));
    tokio::task::spawn_blocking(move || {
        let home = user_home();
        let past = discovery::past_sessions(&home.join(".claude"), &home.join(".codex"), 200).into_iter().map(|session| session.cwd);
        let start_in = |cwd: PathBuf| Action::NewChatIn { provider: provider.clone(), account: account.clone(), cwd };
        let mut items = vec![
            PickItem { label: "User folder".into(), detail: home.display().to_string(), action: start_in(home.clone()) },
            PickItem { label: "Choose a folder…".into(), detail: "Opens the Windows folder picker".into(), action: Action::BrowseFolder(provider.clone(), account.clone()) },
        ];
        items.extend(recent_folders(&home, open.into_iter().chain(past)).into_iter().map(|folder| {
            let detail = folder.display().to_string();
            let label = folder.file_name().map_or_else(|| detail.clone(), |name| name.to_string_lossy().to_string());
            PickItem { label, detail, action: start_in(folder) }
        }));
        let _ = tx.send(AppEvent::Picker(Picker::new(TITLE, items, "")));
    });
}

fn account_picker(app: &mut App, title: &'static str, new_chat: bool) {
    app.refresh_account_labels();
    let on = thread_provider(app).filter(|provider| *provider == Provider::Codex).unwrap_or(Provider::Claude);
    let current = app.chat().and_then(|chat| chat.state.account_on(&on).map(std::path::Path::to_path_buf));
    let (rt, tx) = (app.rt.clone(), app.tx.clone());
    app.picker = Some(Picker::loading(title));
    tokio::task::spawn_blocking(move || {
        let limits = rt.limits.snapshot();
        let mut found: Vec<_> = discovery::accounts(rt.claude.as_ref(), rt.codex.as_ref());
        // The accounts this thread can use right now come first.
        found.sort_by_key(|account| account.vendor != on.vendor());
        let mut items: Vec<PickItem> = found
            .into_iter()
            .map(|account| {
                let provider = if account.vendor == "codex" { Provider::Codex } else { Provider::Claude };
                let kind = account_kind(&provider);
                let name = crate::session::account_name(Some(&account.dir));
                let choice = (account.dir != user_home().join(kind.folder)).then_some(account.dir);
                let quota = if choice.is_none() { account.vendor.clone() } else { format!("{}:{name}", account.vendor) };
                let usage = olympus_core::limits::summarize(&limits, &quota).map(|usage| usage.detail);
                let in_use = !new_chat && provider == on && choice == current;
                let action = if new_chat { Action::NewChatOn(provider.clone(), choice) } else { Action::UseAccount(provider.clone(), choice) };
                PickItem { label: account_row_label(account.signed_in, &account.label, &name), detail: format!("{} · {}", kind.word, account_row_detail(account.signed_in, kind, &name, in_use, usage)), action }
            })
            .collect();
        for provider in [Provider::Claude, Provider::Codex] {
            let kind = account_kind(&provider);
            items.push(PickItem { label: format!("Sign in to another {} account", kind.word), detail: format!("Opens the real {} sign-in in a new window", kind.cli), action: Action::AddAccount(provider) });
        }
        let _ = tx.send(AppEvent::Picker(Picker::new(title, items, "")));
    });
}

// `/account add chatgpt` alone picks the next free name; a bare name follows this thread's provider.
fn parse_account_add(rest: &str, on: Option<&Provider>) -> (Provider, Option<String>) {
    let rest = rest.trim();
    let (first, name) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let name = name.trim().to_string();
    match first {
        "chatgpt" | "codex" => (Provider::Codex, Some(name).filter(|n| !n.is_empty())),
        "claude" => (Provider::Claude, Some(name).filter(|n| !n.is_empty())),
        _ => (if on == Some(&Provider::Codex) { Provider::Codex } else { Provider::Claude }, Some(rest.to_string())),
    }
}

// People recognize their email, not a folder name.
fn account_row_label(signed_in: bool, label: &str, name: &str) -> String {
    if signed_in && !label.is_empty() { label.to_string() } else { name.to_string() }
}

fn account_row_detail(signed_in: bool, kind: &AccountKind, name: &str, in_use: bool, usage: Option<String>) -> String {
    let mut parts = vec![if signed_in { name.to_string() } else { format!("signed out; run {} {name} to sign in", kind.add) }];
    if in_use {
        parts.push("this thread".to_string());
    }
    parts.extend(usage);
    parts.join(" · ")
}

fn select_node(app: &mut App, name: String, address: String) {
    app.node = Some((name.clone(), address.clone()));
    app.note_active(format!("Selected node {name}."));
    let items = vec![
        PickItem { label: format!("Use {name} in this chat"), detail: "Whatever model it has loaded".into(), action: Action::Run(format!("/model node:{name}")) },
        PickItem { label: "Find a model on Hugging Face".into(), detail: "Downloads straight to the node".into(), action: Action::Prefill("/hf ".into()) },
        PickItem { label: "Load a downloaded model".into(), detail: String::new(), action: Action::Run("/load".into()) },
        PickItem { label: "Unload the current model".into(), detail: "Frees its GPU memory".into(), action: Action::Run("/unload".into()) },
    ];
    app.picker = Some(Picker::new(&name, items, ""));
}

fn node_client(app: &mut App) -> Option<(String, NodeClient)> {
    match &app.node {
        Some((name, address)) => Some((name.clone(), NodeClient::new(address, &app.rt.config.node_token))),
        None => {
            app.fail_active("Choose a node first with /nodes.");
            None
        }
    }
}

fn format_gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
}

fn hf_search(app: &mut App, query: String) {
    let Some((_, client)) = node_client(app) else { return };
    let (tx, id) = (app.tx.clone(), app.active_id());
    app.picker = Some(Picker::loading("Hugging Face"));
    tokio::spawn(async move {
        match client.search(&query).await {
            Ok(repos) => {
                let items = repos
                    .into_iter()
                    .map(|r| {
                        let support = if r.supported { "" } else { " · not supported by this node's engine" };
                        PickItem { detail: format!("{} · {} downloads{support}", r.architecture.as_deref().unwrap_or("unknown"), r.downloads), action: Action::HfRepo(r.repo.clone()), label: r.repo }
                    })
                    .collect();
                let _ = tx.send(AppEvent::Picker(Picker::new("Hugging Face", items, "No GGUF models match that search.")));
            }
            Err(error) => {
                let _ = tx.send(AppEvent::Picker(Picker::new("Hugging Face", Vec::new(), "Search failed.")));
                let _ = tx.send(AppEvent::Fail { window: id, text: format!("{error:#}") });
            }
        }
    });
}

fn pf_search(app: &mut App, query: String) {
    let Some((_, client)) = node_client(app) else { return };
    let (tx, id) = (app.tx.clone(), app.active_id());
    app.picker = Some(Picker::loading("Pirate Face"));
    tokio::spawn(async move {
        match client.torrent_search(&query).await {
            Ok(hits) => {
                let items = hits.into_iter().map(|hit| PickItem { detail: format!("{} · {}", hit.size, if hit.seeds == 0 { "nobody sharing right now".to_string() } else { format!("{} seeds", hit.seeds) }), action: Action::PfRepo { repo: hit.repo.clone(), magnet: hit.magnet }, label: hit.repo }).collect();
                let _ = tx.send(AppEvent::Picker(Picker::new("Pirate Face", items, "No GGUF torrents match that search. Try /hf.")));
            }
            Err(error) => {
                let _ = tx.send(AppEvent::Picker(Picker::new("Pirate Face", Vec::new(), "Search failed.")));
                let _ = tx.send(AppEvent::Fail { window: id, text: format!("{error:#}") });
            }
        }
    });
}

fn hf_files(app: &mut App, repo: String, magnet: Option<String>) {
    let Some((_, client)) = node_client(app) else { return };
    let (tx, id) = (app.tx.clone(), app.active_id());
    app.picker = Some(Picker::loading(&repo));
    tokio::spawn(async move {
        let listing = match &magnet {
            Some(magnet) => client.torrent_files(magnet).await,
            None => client.files(&repo).await,
        };
        match listing {
            Ok(mut quants) => {
                quants.sort_by_key(|q| !q.recommended);
                let items = quants
                    .into_iter()
                    .map(|q| {
                        let fit = match q.fit.as_str() {
                            "gpu" => "fits GPU",
                            "cpu" => "CPU only",
                            _ => "too large",
                        };
                        let star = if q.recommended { " · recommended" } else { "" };
                        PickItem { label: q.quant.clone(), detail: format!("{} · {fit}{star}", format_gb(q.size)), action: match &magnet {
                            Some(magnet) => Action::PfQuant { repo: repo.clone(), quant: q.quant, magnet: magnet.clone() },
                            None => Action::HfQuant { repo: repo.clone(), quant: q.quant },
                        } }
                    })
                    .collect();
                let _ = tx.send(AppEvent::Picker(Picker::new(&repo, items, "This repo has no GGUF files.")));
            }
            Err(error) => {
                let _ = tx.send(AppEvent::Picker(Picker::new(&repo, Vec::new(), "Couldn't list this model's files.")));
                let _ = tx.send(AppEvent::Fail { window: id, text: format!("{error:#}") });
            }
        }
    });
}

fn hf_pull(app: &mut App, repo: String, quant: String, magnet: Option<String>) {
    let Some((name, client)) = node_client(app) else { return };
    let (tx, id) = (app.tx.clone(), app.active_id());
    app.note_active(format!("Downloading {repo} {quant}."));
    tokio::spawn(async move {
        let started = match &magnet {
            Some(magnet) => client.pull_torrent(&repo, &quant, magnet).await,
            None => client.pull(&repo, &quant).await,
        };
        if let Err(error) = started {
            let _ = tx.send(AppEvent::Fail { window: id, text: format!("{error:#}") });
            return;
        }
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let Ok(status) = client.status().await else { continue };
            match status.download {
                Some(download) => {
                    let percent = (download.done * 100).checked_div(download.total).unwrap_or(0);
                    let _ = tx.send(AppEvent::Status(format!("Downloading {percent}% · {}/s", format_gb(download.bytes_per_sec))));
                }
                None => {
                    let _ = tx.send(AppEvent::Status(String::new()));
                    if let Some(error) = status.error {
                        let _ = tx.send(AppEvent::Fail { window: id, text: error });
                        let _ = tx.send(AppEvent::Notify { window: id, ok: false, body: format!("Download of {repo} failed") });
                        return;
                    }
                    let found = status.local.iter().find(|m| m.repo == repo && m.quant == quant);
                    if found.is_some_and(|m| !m.verified) {
                        let _ = tx.send(AppEvent::Note { window: id, text: "Hugging Face doesn't list this file, so its checksum couldn't be checked. It came from strangers on the torrent network.".into() });
                    }
                    let downloaded = found.map(|m| m.id.clone());
                    match (downloaded, status.model) {
                        (Some(model), None) => {
                            let _ = tx.send(AppEvent::Note { window: id, text: format!("Downloaded {repo} {quant}. Loading it now.") });
                            wait_for_load(client, name, model, id, tx).await;
                        }
                        _ => {
                            let _ = tx.send(AppEvent::Note { window: id, text: format!("Downloaded {repo} {quant}. Run /load to switch to it.") });
                            let _ = tx.send(AppEvent::Notify { window: id, ok: true, body: format!("{repo} {quant} downloaded") });
                        }
                    }
                    return;
                }
            }
        }
    });
}

fn list_models(app: &mut App, filter: &str) {
    let filter = filter.to_string();
    let Some((name, client)) = node_client(app) else { return };
    let (tx, id) = (app.tx.clone(), app.active_id());
    app.picker = Some(Picker::loading(&name));
    tokio::spawn(async move {
        match client.status().await {
            Ok(status) => {
                let items = status.local.into_iter().map(|m| PickItem { label: format!("{} {}", m.repo, m.quant), detail: format_gb(m.size), action: Action::LoadModel(m.id) }).collect();
                let mut picker = Picker::new(&name, items, "No models downloaded yet. Find one with /hf.");
                picker.query = filter;
                let _ = tx.send(AppEvent::Picker(picker));
            }
            Err(error) => {
                let _ = tx.send(AppEvent::Fail { window: id, text: format!("{error:#}") });
            }
        }
    });
}

fn load_model(app: &mut App, model: String) {
    let Some((name, client)) = node_client(app) else { return };
    let (tx, id) = (app.tx.clone(), app.active_id());
    app.note_active("Loading the model.");
    tokio::spawn(wait_for_load(client, name, model, id, tx));
}

async fn wait_for_load(client: NodeClient, name: String, model: String, id: u64, tx: tokio::sync::mpsc::UnboundedSender<AppEvent>) {
    if let Err(error) = client.load(&model).await {
        let _ = tx.send(AppEvent::Fail { window: id, text: format!("{error:#}") });
        return;
    }
    for _ in 0..600 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let Ok(status) = client.status().await else { continue };
        match status.state.as_str() {
            "ready" => {
                let _ = tx.send(AppEvent::Note { window: id, text: format!("{} is ready on {name}. This chat now uses it.", status.model.clone().unwrap_or_default()) });
                let _ = tx.send(AppEvent::Notify { window: id, ok: true, body: format!("{} is ready on {name}", status.model.unwrap_or_default()) });
                if let Ok(target) = Target::parse(&format!("node:{name}")) {
                    let _ = tx.send(AppEvent::UseTarget { window: id, target });
                }
                return;
            }
            "error" => {
                let _ = tx.send(AppEvent::Fail { window: id, text: status.error.unwrap_or_else(|| "The model failed to load.".into()) });
                let _ = tx.send(AppEvent::Notify { window: id, ok: false, body: format!("The model failed to load on {name}") });
                return;
            }
            _ => {}
        }
    }
    let _ = tx.send(AppEvent::Fail { window: id, text: "The model didn't finish loading in 5 minutes.".into() });
}

fn notify_command(app: &mut App, args: &str) {
    let marker = home_dir().join(crate::app::NOTIFICATIONS_OFF);
    match args {
        "on" => {
            app.notifications_on = true;
            let _ = std::fs::remove_file(marker);
            app.note_active("Notifications are on.");
        }
        "off" => {
            app.notifications_on = false;
            // If this can't be written, it's still off until the next launch.
            let _ = std::fs::write(marker, "");
            app.note_active("Notifications are off.");
        }
        "test" => app.notices.push(crate::app::Notice { window: 0, title: "Olympus".into(), body: "This is what a finished job sounds like.".into(), ok: true }),
        "test-failed" => app.notices.push(crate::app::Notice { window: 0, title: "Olympus".into(), body: "This is what a failed job sounds like.".into(), ok: false }),
        _ => app.fail_active("Use /notify on, /notify off or /notify test."),
    }
}

/// Consults another model read-only and posts the answer in this chat.
pub fn consult(app: &mut App, model: &str, question: &str) {
    let target = match app.rt.resolve(model) {
        Ok(target) => target,
        Err(message) => {
            app.fail_active(message);
            return;
        }
    };
    let Some(chat) = app.chat() else { return };
    chat.push(Block::User(format!("@{model} {question}")));
    chat.follow = true;
    let cwd = chat.state.cwd.clone();
    let account = account_for(&target, chat.state.account_dir.clone(), chat.state.codex_account_dir.clone());
    let (rt, tx, id) = (app.rt.clone(), app.tx.clone(), app.active_id());
    let question = question.to_string();
    tokio::spawn(async move {
        let mut req = TurnRequest::new(target.clone(), question, cwd);
        req.access = Access::ReadOnly;
        req.role = Some(roles::CONSULTANT);
        req.ephemeral = true;
        req.account_dir = account;
        let (_keep, cancel) = cancel_pair();
        let result = run_turn(&rt, &req, cancel, &mut |_| {}).await;
        if result.ok() {
            let _ = tx.send(AppEvent::Answer { window: id, label: target.to_string(), text: result.text });
            let _ = tx.send(AppEvent::Notify { window: id, ok: true, body: format!("{target} answered") });
        } else {
            let _ = tx.send(AppEvent::Fail { window: id, text: format!("{target}: {}", result.error.unwrap_or_else(|| "failed".into())) });
            let _ = tx.send(AppEvent::Notify { window: id, ok: false, body: format!("{target} couldn't answer") });
        }
    });
}

fn begin_background(app: &mut App) -> Option<(u64, PathBuf, tokio::sync::watch::Receiver<bool>)> {
    let id = app.active_id();
    let window = app.windows.get_mut(app.active)?;
    if window.activity == Activity::Running {
        app.fail_active("This chat is busy. Wait for it, press Esc to stop it, or open another with Alt+N.");
        return None;
    }
    window.activity = Activity::Running;
    let chat = window.chat()?;
    let (cancel_tx, cancel) = cancel_pair();
    chat.cancel = Some(cancel_tx);
    Some((id, chat.state.cwd.clone(), cancel))
}

fn auto(app: &mut App, goal: &str) {
    let Some((id, cwd, cancel)) = begin_background(app) else { return };
    let Some(chat) = app.chat() else { return };
    chat.push(Block::User(format!("/auto {goal}")));
    chat.push(Block::Tasks { phase: "Starting".into(), rows: Vec::<TaskRow>::new() });
    chat.follow = true;
    chat.state.title = crate::session::short_title(goal);
    let (rt, tx) = (app.rt.clone(), app.tx.clone());
    let goal = goal.to_string();
    tokio::spawn(async move {
        let (flow_tx, mut flow_rx) = tokio::sync::mpsc::unbounded_channel();
        let forward = tx.clone();
        tokio::spawn(async move {
            while let Some(event) = flow_rx.recv().await {
                let _ = forward.send(AppEvent::Flow { window: id, event });
            }
        });
        let outcome = workflow::run_auto(rt, goal, cwd, flow_tx, cancel).await.map_err(|e| format!("{e:#}"));
        let _ = tx.send(AppEvent::AutoDone { window: id, outcome });
    });
}

fn council(app: &mut App, question: &str) {
    let Some((id, cwd, cancel)) = begin_background(app) else { return };
    let Some(chat) = app.chat() else { return };
    chat.push(Block::User(format!("/council {question}")));
    chat.follow = true;
    let (rt, tx) = (app.rt.clone(), app.tx.clone());
    let question = question.to_string();
    tokio::spawn(async move {
        let events = tx.clone();
        let outcome = workflow::council(rt, &question, &[], &cwd, cancel, &mut move |event| {
            if let olympus_core::event::AgentEvent::Status(text) = event {
                let _ = events.send(AppEvent::Note { window: id, text });
            }
        })
        .await;
        match outcome {
            Ok(result) => {
                for answer in &result.answers {
                    if let Some(error) = &answer.error {
                        let _ = tx.send(AppEvent::Fail { window: id, text: format!("{}: {error}", answer.target) });
                    }
                }
                let _ = tx.send(AppEvent::Answer { window: id, label: "Council".into(), text: result.synthesis });
            }
            Err(error) => {
                let _ = tx.send(AppEvent::Fail { window: id, text: error });
            }
        }
        let _ = tx.send(AppEvent::Done { window: id });
    });
}

fn review(app: &mut App) {
    let last_target = app.chat().and_then(|c| c.state.last_target.clone());
    let Some((id, cwd, cancel)) = begin_background(app) else { return };
    let (rt, tx) = (app.rt.clone(), app.tx.clone());
    app.note_active("Reviewing uncommitted changes.");
    tokio::spawn(async move {
        let outcome = async {
            let diff = olympus_core::git::working_diff(&cwd, 60_000).await.map_err(|e| format!("{e:#}"))?;
            if diff.trim().is_empty() {
                return Ok("There are no uncommitted changes to review.".to_string());
            }
            let reviewed = workflow::review_diff(&rt, last_target.as_ref(), &diff, &cwd, cancel, &mut |_| {}).await.map_err(|e| format!("{e:#}"))?;
            let Some((reviewer, review, _)) = reviewed else {
                return Err("No other vendor is available to review this.".to_string());
            };
            let mut text = format!("{} by {reviewer}.", if review.approved { "Approved" } else { "Not approved" });
            for finding in &review.findings {
                text.push_str(&format!("\n- **{}** `{}`: {}", finding.severity, finding.file, finding.summary));
            }
            Ok(text)
        }
        .await;
        match outcome {
            Ok(text) => {
                let _ = tx.send(AppEvent::Answer { window: id, label: "Review".into(), text });
            }
            Err(text) => {
                let _ = tx.send(AppEvent::Fail { window: id, text });
            }
        }
        let _ = tx.send(AppEvent::Done { window: id });
    });
}

fn merge(app: &mut App) {
    let Some(chat) = app.chat() else { return };
    let Some(branch) = chat.state.last_branch.clone() else {
        app.fail_active("There's no finished /auto run in this chat to merge.");
        return;
    };
    let cwd = chat.state.cwd.clone();
    let (tx, id) = (app.tx.clone(), app.active_id());
    tokio::spawn(async move {
        let outcome = async {
            let root = olympus_core::git::root(&cwd).await?;
            workflow::land(&root, &branch).await
        }
        .await;
        let _ = match outcome {
            Ok(text) => {
                let _ = tx.send(AppEvent::Merged { window: id });
                tx.send(AppEvent::Note { window: id, text })
            }
            Err(error) => tx.send(AppEvent::Fail { window: id, text: format!("{error:#}") }),
        };
    });
}

fn set_model(app: &mut App, spec: &str) {
    if spec.is_empty() {
        let tiers = &app.rt.config.tiers;
        let mut items = vec![PickItem { label: "auto".into(), detail: "Route by task and learned results".into(), action: Action::Run("/model auto".into()) }];
        for (tier, list) in [("strong", &tiers.strong), ("standard", &tiers.standard), ("cheap", &tiers.cheap)] {
            items.extend(list.iter().map(|spec| PickItem { label: spec.clone(), detail: tier.into(), action: Action::Run(format!("/model {spec}")) }));
        }
        app.picker = Some(Picker::new("Model", items, ""));
        return;
    }
    let choice = if spec == "auto" {
        Choice::Auto
    } else {
        match app.rt.resolve(spec) {
            Ok(target) => {
                if target.provider == Provider::OpenRouter && !app.rt.provider_ready(&target.provider) {
                    app.fail_active("Add an OpenRouter key first: set OPENROUTER_API_KEY or openrouter_key in ~/.olympus/config.toml.");
                    return;
                }
                Choice::Fixed(target)
            }
            Err(message) => {
                app.fail_active(message);
                return;
            }
        }
    };
    let Some(chat) = app.chat() else { return };
    let label = choice.label();
    chat.state.choice = choice;
    chat.state.vendor_session = None;
    chat.state.last_target = None;
    chat.push(Block::Note(format!("Using {label}. The next message starts fresh on it.")));
}

fn open_cli(app: &mut App, title: &str, path: Option<PathBuf>, flags: &[&str], kind: &AccountKind, account: Option<PathBuf>) {
    let Some(path) = path else {
        app.fail_active(kind.missing);
        return;
    };
    let cwd = app.current_cwd();
    let args: Vec<String> = flags.iter().map(|f| f.to_string()).collect();
    let env = account.as_deref().map(|dir| (kind.env, dir));
    if let Err(error) = app.new_term_with_env(title, &path.display().to_string(), &args, cwd, env) {
        app.fail_active(format!("{error:#}"));
    }
}

// A build sits in target/release inside its source, while a downloaded exe has none nearby.
fn source_folder(exe: &Path) -> Option<&Path> {
    exe.ancestors().find(|folder| folder.join("CLAUDE.md").is_file() && folder.join("crates").join("olympus").is_dir())
}

fn self_improve_command(source: &Path, claude: &Path, account: Option<&Path>) -> tokio::process::Command {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut command = tokio::process::Command::new("cmd.exe");
    // `start` gives it its own console; spawned directly with CREATE_NEW_CONSOLE it quit within seconds.
    command.current_dir(source).raw_arg(format!("/c start \"Olympus self-improve\" /d \"{}\" \"{}\" --dangerously-skip-permissions --name \"Olympus self-improve\"", source.display(), claude.display())).creation_flags(CREATE_NO_WINDOW);
    scrub_env(&mut command);
    if let Some(dir) = account {
        command.env(CLAUDE_ACCOUNTS.env, dir);
    }
    command
}

// Its own console rather than a pane, so it outlives the Olympus it restarts.
fn self_improve(app: &mut App) {
    let Some(claude) = app.rt.claude.as_ref().map(|b| b.path.clone()) else {
        app.fail_active(CLAUDE_ACCOUNTS.missing);
        return;
    };
    let exe = std::env::current_exe().unwrap_or_default();
    let Some(source) = source_folder(&exe) else {
        app.fail_active("Self-improve edits Olympus's source. Clone the repo, build it, and run that olympus.exe.");
        return;
    };
    let account = app.chat().and_then(|chat| chat.state.account_dir.clone());
    if let Err(error) = self_improve_command(source, &claude, account.as_deref()).spawn() {
        app.fail_active(format!("Couldn't open Claude Code: {error}"));
    }
}

fn sessions(app: &mut App) {
    let tx = app.tx.clone();
    app.picker = Some(Picker::loading("Past sessions"));
    tokio::task::spawn_blocking(move || {
        let home = user_home();
        let found = discovery::past_sessions(&home.join(".claude"), &home.join(".codex"), 200);
        let items = found
            .into_iter()
            .map(|s| PickItem {
                label: s.title.clone(),
                detail: format!("{} · {}", s.vendor, s.cwd.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()),
                action: Action::Resume { vendor: s.vendor, id: s.id, cwd: s.cwd, title: s.title },
            })
            .collect();
        let _ = tx.send(AppEvent::Picker(Picker::new("Past sessions", items, "No past sessions found.")));
    });
}

fn accounts(app: &mut App) {
    let (rt, tx, id) = (app.rt.clone(), app.tx.clone(), app.active_id());
    tokio::task::spawn_blocking(move || {
        let found = discovery::accounts(rt.claude.as_ref(), rt.codex.as_ref());
        let mut lines: Vec<String> = found
            .iter()
            .map(|a| format!("{} {} · {} · {}", if a.signed_in { "✓" } else { "–" }, a.vendor, a.label, a.dir.display()))
            .collect();
        if lines.is_empty() {
            lines.push("No signed-in accounts found. Open /claude or /codex to sign in.".into());
        }
        if let Some(bin) = &rt.claude {
            lines.push(format!("Claude Code {} at {}", bin.version, bin.path.display()));
        }
        if let Some(bin) = &rt.codex {
            lines.push(format!("Codex {} at {}", bin.version, bin.path.display()));
        }
        let _ = tx.send(AppEvent::Note { window: id, text: lines.join("\n") });
    });
}

fn usage(app: &mut App) {
    let (rt, tx, id) = (app.rt.clone(), app.tx.clone(), app.active_id());
    tokio::task::spawn_blocking(move || {
        let mut text = String::new();
        for limit in rt.limits.snapshot() {
            text.push_str(&format!("- {} {}: {:.0}% used\n", limit.vendor, limit.window, limit.used * 100.0));
        }
        if let Some(store) = &rt.learn {
            if let Ok(store) = store.lock() {
                if let Ok(stats) = store.stats() {
                    text.push_str(&format!("\n**History:** {} prompts and {} signals from {} files\n", stats.prompts, stats.signals, stats.files));
                    text.push_str(&format!("**Lessons:** {} in use, {} waiting for approval, {} stale, {} retired\n", stats.lessons_active, stats.lessons_candidate, stats.lessons_stale, stats.lessons_retired));
                    if !stats.run_per_model.is_empty() {
                        text.push_str("\n**Spend through Olympus:**\n");
                        for model in &stats.run_per_model {
                            text.push_str(&format!("- {}: {:.0}k weighted tokens\n", model.model, model.weighted_tokens / 1000.0));
                        }
                    }
                }
                if let Ok(report) = store.holdout_report() {
                    if report.with_memory_runs + report.holdout_runs > 0 {
                        text.push_str(&format!(
                            "\n**Does memory help?** With memory: {:.0}% success over {} runs. Without: {:.0}% over {} runs.\n",
                            report.with_memory_success_rate * 100.0, report.with_memory_runs, report.holdout_success_rate * 100.0, report.holdout_runs
                        ));
                    }
                }
            }
        }
        if text.trim().is_empty() {
            text = "Nothing to report yet. Quotas appear after the first turn.".into();
        }
        let _ = tx.send(AppEvent::Answer { window: id, label: "Usage".into(), text });
    });
}

fn learn(app: &mut App, args: &str) {
    let (tx, id, rt, cwd) = (app.tx.clone(), app.active_id(), app.rt.clone(), app.current_cwd());
    let handle = tokio::runtime::Handle::current();
    let (sub, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
    let (sub, rest) = (sub.to_string(), rest.trim().to_string());
    tokio::task::spawn_blocking(move || {
        let _guard = handle.enter();
        let send = |text: String| {
            let _ = tx.send(AppEvent::Answer { window: id, label: "Learning".into(), text });
        };
        let store = match Store::open(&home_dir().join("learn.db")) {
            Ok(store) => store,
            Err(error) => {
                let _ = tx.send(AppEvent::Fail { window: id, text: format!("{error:#}") });
                return;
            }
        };
        match sub.as_str() {
            "" | "import" => {
                let home = user_home();
                match store.ingest(Some(&home.join(".claude")), Some(&home.join(".codex")), &mut |_| {}) {
                    Ok(stats) => send(format!("Read {} new lines from {} files and found {} signals. {} candidate lessons are waiting: run /learn lessons.", stats.lines, stats.files_updated, stats.signals, stats.candidates)),
                    Err(error) => {
                        let _ = tx.send(AppEvent::Fail { window: id, text: format!("{error:#}") });
                    }
                }
            }
            "lessons" => {
                let mut text = String::new();
                for (title, state) in [("In use", LessonState::Active), ("Waiting for /approve", LessonState::Candidate)] {
                    let lessons = store.lessons(Some(state), 15).unwrap_or_default();
                    if !lessons.is_empty() {
                        text.push_str(&format!("**{title}**\n"));
                        for lesson in lessons {
                            text.push_str(&format!("- `{}` {}\n", lesson.id, lesson.text.lines().next().unwrap_or("")));
                        }
                        text.push('\n');
                    }
                }
                send(if text.is_empty() { "No lessons yet. Teach one with /remember, or run /learn to import history.".into() } else { text });
            }
            "distill" => {
                let (rt, tx, cwd) = (rt.clone(), tx.clone(), cwd.clone());
                tokio::runtime::Handle::current().spawn(async move {
                    let (_keep, cancel) = cancel_pair();
                    let text = match workflow::distill(&rt, &cwd, cancel).await {
                        Ok(0) => "Found nothing lasting in your corrections yet.".to_string(),
                        Ok(count) => format!("Found {count} possible lessons in your corrections. Review them with /learn lessons, then /approve the ones you agree with."),
                        Err(error) => {
                            let _ = tx.send(AppEvent::Fail { window: id, text: error });
                            return;
                        }
                    };
                    let _ = tx.send(AppEvent::Answer { window: id, label: "Learning".into(), text });
                });
            }
            "history" => match store.search_history(&rest, 10) {
                Ok(hits) if hits.is_empty() => send("Nothing in your history matches.".into()),
                Ok(hits) => send(hits.iter().map(|h| format!("- {}", h.snippet)).collect::<Vec<_>>().join("\n")),
                Err(error) => {
                    let _ = tx.send(AppEvent::Fail { window: id, text: format!("{error:#}") });
                }
            },
            other => {
                let _ = tx.send(AppEvent::Fail { window: id, text: format!("Unknown option '{other}'. Try /learn, /learn lessons, /learn distill or /learn history <words>.") });
            }
        }
    });
}

fn lesson_action(app: &mut App, args: &str, approve: bool) {
    let Ok(lesson) = args.trim().parse::<i64>() else {
        app.fail_active("Give the lesson's number, from /learn lessons.");
        return;
    };
    let Some(store) = app.rt.learn.clone() else {
        app.fail_active("The learning store isn't available.");
        return;
    };
    let outcome = store.lock().map_err(|_| anyhow::anyhow!("The learning store is unavailable.")).and_then(|s| if approve { s.approve_lesson(lesson) } else { s.reject_lesson(lesson) });
    match outcome {
        Ok(()) => app.note_active(format!("Lesson {lesson} {}.", if approve { "is now in use" } else { "was retired" })),
        Err(error) => app.fail_active(format!("{error:#}")),
    }
}

fn remember(app: &mut App, text: &str) {
    let Some(store) = app.rt.learn.clone() else {
        app.fail_active("The learning store isn't available.");
        return;
    };
    let lesson = NewLesson { text: text.to_string(), kind: LessonKind::Preference, scope: None, provenance_file: None, provenance_hash: None, state: LessonState::Active };
    match store.lock().map_err(|_| anyhow::anyhow!("The learning store is unavailable.")).and_then(|s| s.add_lesson(lesson)) {
        Ok(id) => app.note_active(format!("Remembered as lesson {id}.")),
        Err(error) => app.fail_active(format!("{error:#}")),
    }
}

fn nodes(app: &mut App) {
    let (rt, tx) = (app.rt.clone(), app.tx.clone());
    app.picker = Some(Picker::loading("Nodes"));
    tokio::spawn(async move {
        let found = node::discover(&rt.config, Duration::from_millis(1200)).await.unwrap_or_default();
        let items = found
            .into_iter()
            .map(|n| {
                let gpu = n.gpu.clone().unwrap_or_else(|| "no GPU".into());
                let model = n.model.clone().unwrap_or_else(|| "no model loaded".into());
                PickItem { label: n.name.clone(), detail: format!("{gpu} · {model}"), action: Action::SelectNode { address: n.address(), name: n.name } }
            })
            .collect();
        let _ = tx.send(AppEvent::Picker(Picker::new("Nodes", items, "No nodes answered. Run olympus-node.exe on the other computer, and check it's on this network.")));
    });
}

/// Runs a slash command.
pub fn run(app: &mut App, line: &str) {
    let (name, args) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let args = args.trim();
    match name {
        "/help" => {
            let mut text = String::from("**Commands**\n");
            for command in COMMANDS {
                text.push_str(&format!("- `{}` {}\n", command.usage, command.about));
            }
            text.push_str("\n**Keys**\n- `Alt+1-9` switch windows, `Alt+←/→` cycle, `Alt+G` grid view\n- `Alt+N` new chat, `Alt+T` terminal, `Alt+W` close (`/reopen` brings a chat back), `Alt+K` find anything, `Alt+Q` quit\n- Each chat can use its own Claude account: `/account`\n- Hear when work finishes: `/notify on`, `/notify off`, `/notify test`\n- `Alt+D` review changes, `Alt+PgUp/PgDn` scroll them, `Alt+B` show or hide threads\n- `Enter` send, `Ctrl+J` new line, `Esc` stop, `PgUp/PgDn` scroll\n- Drag to select text, then `Ctrl+C` to copy\n\nAsk naturally too: \"take help from ChatGPT on this\" works, because agents can call other models themselves.");
            if let Some(chat) = app.chat() {
                chat.push(Block::Assistant { label: Some("Olympus".into()), text });
            }
        }
        "/auto" if args.is_empty() => app.fail_active("Say what to build, like /auto add rate limiting to login."),
        "/auto" => auto(app, args),
        "/ask" => match args.split_once(char::is_whitespace) {
            Some((model, question)) => consult(app, model, question.trim()),
            None => app.fail_active("Use /ask <model> <question>, like /ask codex is this safe?"),
        },
        "/council" if args.is_empty() => app.fail_active("Add a question, like /council which database fits this?"),
        "/council" => council(app, args),
        "/review" => review(app),
        "/merge" => merge(app),
        "/changes" => app.toggle_diff(),
        "/grid" => app.grid = !app.grid,
        "/copy" => {
            let answer = app.chat().and_then(|chat| chat.state.blocks.iter().rev().find_map(|block| match block {
                Block::Assistant { text, .. } => Some(text.clone()),
                _ => None,
            }));
            match answer {
                Some(text) => app.pending_copy = Some(text),
                None => app.fail_active("There's no answer to copy yet."),
            }
        }
        "/details" => app.pending_details = true,
        "/model" => set_model(app, args),
        "/profile" => {
            let full = match args {
                "full" => true,
                "lean" => false,
                _ => {
                    app.fail_active("Use /profile lean or /profile full.");
                    return;
                }
            };
            if let Some(chat) = app.chat() {
                chat.state.full_profile = full;
                chat.state.vendor_session = None;
                let message = if full { "Next message loads your full setup: every MCP server, skill and hook." } else { "Next message uses the lean setup, about 70% fewer startup tokens on Claude." };
                chat.push(Block::Note(message.into()));
            }
        }
        "/new" => {
            let base = app.current_cwd();
            let cwd = if args.is_empty() { base } else { expand_path(&base, args) };
            if cwd.is_dir() {
                app.new_chat(cwd);
            } else {
                app.fail_active(format!("{} isn't a folder.", cwd.display()));
            }
        }
        "/cd" => {
            let cwd = expand_path(&app.current_cwd(), args);
            if !cwd.is_dir() {
                app.fail_active(format!("{} isn't a folder.", cwd.display()));
            } else if let Some(chat) = app.chat() {
                chat.state.cwd = cwd.clone();
                chat.state.vendor_session = None;
                chat.push(Block::Note(format!("Now working in {}. The next message starts fresh.", cwd.display())));
            }
        }
        "/clear" => {
            if let Some(chat) = app.chat() {
                chat.state.blocks.clear();
                chat.state.vendor_session = None;
                chat.state.last_target = None;
                chat.touch(0);
                chat.follow = true;
            }
        }
        "/close" => app.close_active(),
        "/reopen" => app.reopen_chat(),
        "/rename" if !args.is_empty() => {
            if let Some(chat) = app.chat() {
                chat.state.title = args.to_string();
            }
        }
        "/rename" => app.fail_active("Add the new title, like /rename login work."),
        "/claude" => {
            let path = app.rt.claude.as_ref().map(|b| b.path.clone());
            let account = app.chat().and_then(|chat| chat.state.account_dir.clone());
            open_cli(app, "Claude Code", path, &["--dangerously-skip-permissions"], &CLAUDE_ACCOUNTS, account);
        }
        "/codex" => {
            let path = app.rt.codex.as_ref().map(|b| b.path.clone());
            let account = app.chat().and_then(|chat| chat.state.codex_account_dir.clone());
            open_cli(app, "Codex", path, &["--dangerously-bypass-approvals-and-sandbox"], &CODEX_ACCOUNTS, account);
        }
        "/term" => {
            let cwd = app.current_cwd();
            let outcome = if args.is_empty() {
                app.new_term("PowerShell", "powershell", &["-NoLogo".to_string()], cwd)
            } else {
                app.new_term(args, "powershell", &["-NoLogo".to_string(), "-NoExit".to_string(), "-Command".to_string(), args.to_string()], cwd)
            };
            if let Err(error) = outcome {
                app.fail_active(format!("{error:#}"));
            }
        }
        "/self-improve" => self_improve(app),
        "/sessions" => sessions(app),
        "/accounts" => accounts(app),
        "/account" => match args.split_once(char::is_whitespace).unwrap_or((args, "")) {
            ("", _) => choose_account(app),
            ("add", rest) => {
                let on = thread_provider(app);
                let (provider, name) = parse_account_add(rest, on.as_ref());
                let name = name.unwrap_or_else(|| next_account_name(&user_home(), account_kind(&provider)));
                add_account(app, &provider, &name);
            }
            _ => app.fail_active("Use /account to choose, or /account add [chatgpt] <name> to sign in to another."),
        },
        "/usage" => usage(app),
        "/nodes" => nodes(app),
        "/hf" if args.is_empty() => app.fail_active("Add a search, like /hf qwen3."),
        "/hf" => hf_search(app, args.to_string()),
        "/pf" if args.is_empty() => app.fail_active("Add a search, like /pf qwen3."),
        "/pf" => pf_search(app, args.to_string()),
        "/load" => list_models(app, args),
        "/models" => list_models(app, ""),
        "/unload" => {
            if let Some((name, client)) = node_client(app) {
                let (tx, id) = (app.tx.clone(), app.active_id());
                tokio::spawn(async move {
                    let _ = match client.unload().await {
                        Ok(()) => tx.send(AppEvent::Note { window: id, text: format!("Unloaded the model on {name}.") }),
                        Err(error) => tx.send(AppEvent::Fail { window: id, text: format!("{error:#}") }),
                    };
                });
            }
        }
        "/skills" => {
            let all = skills::everything(&user_home(), &app.current_cwd());
            let items = skills::search(&all, args).iter().map(|s| PickItem { label: s.name.clone(), detail: s.description.clone(), action: Action::Prefill(format!("/{} ", s.name)) }).collect();
            app.picker = Some(Picker::new("Skills", items, "No skills found in ~/.claude/skills."));
        }
        "/learn" => learn(app, args),
        "/approve" => lesson_action(app, args, true),
        "/reject" => lesson_action(app, args, false),
        "/remember" if args.is_empty() => app.fail_active("Say what to remember, like /remember always use pnpm here."),
        "/remember" => remember(app, args),
        "/notify" => notify_command(app, args),
        "/loop" => loop_command(app, args),
        "/hotkeys" => hotkeys_command(app, args),
        "/screenshot" => {
            let tx = app.tx.clone();
            crate::capture::select(move |path| {
                let _ = tx.send(AppEvent::Attach(path));
            });
        }
        "/stop" => {
            if !app.stop_active() {
                app.note_active("Nothing is running here.");
            }
        }
        "/quit" => app.quit = true,
        other if app.active_is_chat() && is_passed_on(other, &app.current_cwd()) => {
            let id = app.active_id();
            app.send(id, line.to_string());
        }
        other => app.fail_active(format!("Unknown command {other}. Run /help to see them all.")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::testing::*;
    use crate::input::{KeyCode, KeyModifiers};

    fn last_block(app: &mut App) -> Block {
        app.chat().expect("chat").state.blocks.last().cloned().expect("block")
    }

    fn enter(app: &mut App, text: &str) {
        typed(app, text);
        press(app, KeyCode::Esc, KeyModifiers::NONE);
        app.composer.set(text);
        app.submit();
    }

    #[test]
    fn every_command_has_a_handler() {
        let source = include_str!("commands.rs");
        for command in COMMANDS {
            assert!(source.contains(&format!("\"{}\" ", command.name)) || source.contains(&format!("\"{}\" =>", command.name)), "{} has no handler", command.name);
        }
    }

    #[tokio::test]
    async fn choosing_a_folder_for_a_new_chat_opens_it_there_and_leaves_the_old_one_alone() {
        let (mut app, _rx, dir) = app(true);
        let first = app.active_id();
        let cwd = dir.path().to_path_buf();
        pick(&mut app, Action::NewChatIn { provider: Provider::Claude, account: Some(PathBuf::from("C:/u/.claude-work")), cwd: cwd.clone() });
        assert_ne!(app.active_id(), first);
        let chat = app.chat().expect("chat");
        assert_eq!(chat.state.account_dir, Some(PathBuf::from("C:/u/.claude-work")));
        assert_eq!(chat.state.cwd, cwd);
        assert!(chat.state.blocks.is_empty(), "a new chat still shows its welcome");
        pick(&mut app, Action::NewChatIn { provider: Provider::Codex, account: None, cwd });
        assert_eq!(thread_provider(&mut app), Some(Provider::Codex));
        let old = app.window_mut(first).and_then(crate::session::Window::chat).expect("chat");
        assert_eq!(old.state.account_dir, None);
    }

    #[tokio::test]
    async fn choosing_an_account_asks_for_a_folder_before_opening_anything() {
        let (mut app, _rx, _dir) = app(true);
        let (first, windows) = (app.active_id(), app.windows.len());
        pick(&mut app, Action::NewChatOn(Provider::Claude, None));
        assert_eq!((app.active_id(), app.windows.len()), (first, windows));
        assert_eq!(app.picker.as_ref().expect("folder picker").title, "Start the new chat in");
        pick(&mut app, Action::BrowseFolder(Provider::Codex, None));
        assert_eq!(app.pending_folder, Some((Provider::Codex, None)));
        assert_eq!(app.windows.len(), windows);
    }

    #[test]
    fn recent_folders_skip_home_duplicates_worktrees_and_missing_folders() {
        let home = tempfile::tempdir().expect("home");
        let project = home.path().join("Project");
        let worktree = home.path().join(".olympus").join("wt").join("task");
        std::fs::create_dir_all(&project).expect("project");
        std::fs::create_dir_all(&worktree).expect("worktree");
        let shouted = PathBuf::from(project.to_string_lossy().to_uppercase());
        let found = recent_folders(home.path(), [home.path().to_path_buf(), project.clone(), shouted, worktree, home.path().join("gone")]);
        assert_eq!(found, vec![project]);
    }

    #[test]
    fn self_improve_opens_bypassed_claude_where_its_briefing_is() {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).expect("the source folder");
        assert_eq!(source_folder(&source.join("target").join("release").join("olympus.exe")), Some(source));
        assert_eq!(source_folder(Path::new("C:/Downloads/olympus.exe")), None);
        let command = self_improve_command(source, Path::new("claude.exe"), Some(Path::new("work")));
        let command = command.as_std();
        assert!(command.get_current_dir().expect("folder").join("CLAUDE.md").is_file());
        assert!(command.get_args().any(|arg| arg.to_string_lossy().contains("--dangerously-skip-permissions")));
        assert!(command.get_envs().any(|(key, value)| key == "CLAUDE_CONFIG_DIR" && value == Some(std::ffi::OsStr::new("work"))));
    }

    #[tokio::test]
    async fn unknown_commands_point_to_help() {
        let (mut app, _rx, _dir) = app(true);
        enter(&mut app, "/banana");
        assert!(matches!(last_block(&mut app), Block::Error(t) if t.contains("/help")));
    }

    #[tokio::test]
    async fn commands_missing_arguments_explain_what_to_add() {
        let (mut app, _rx, _dir) = app(true);
        for line in ["/auto", "/council", "/ask codex", "/remember", "/rename", "/hf", "/pf", "/notify banana", "/profile nope", "/approve x"] {
            enter(&mut app, line);
            assert!(matches!(last_block(&mut app), Block::Error(_)), "{line} should explain itself");
        }
    }

    #[tokio::test]
    async fn model_accepts_aliases_and_rejects_unknowns() {
        let (mut app, _rx, _dir) = app(true);
        enter(&mut app, "/model chatgpt");
        assert!(matches!(&app.chat().expect("chat").state.choice, Choice::Fixed(t) if t.provider == Provider::Codex));
        enter(&mut app, "/model auto");
        assert_eq!(app.chat().expect("chat").state.choice, Choice::Auto);
        enter(&mut app, "/model banana");
        assert!(matches!(last_block(&mut app), Block::Error(_)));
        enter(&mut app, "/model openrouter:a/b");
        if std::env::var_os("OPENROUTER_API_KEY").is_none() {
            assert!(matches!(last_block(&mut app), Block::Error(t) if t.contains("OpenRouter key")));
        }
    }

    #[tokio::test]
    async fn switching_model_never_resumes_the_old_session() {
        let (mut app, _rx, _dir) = app(true);
        app.chat().expect("chat").state.vendor_session = Some("old".into());
        enter(&mut app, "/model codex");
        assert!(app.chat().expect("chat").state.vendor_session.is_none());
    }

    #[tokio::test]
    async fn copy_takes_the_latest_answer_and_explains_when_there_is_none() {
        let (mut app, _rx, _dir) = app(true);
        enter(&mut app, "/copy");
        assert!(matches!(last_block(&mut app), Block::Error(t) if t.contains("copy")));
        app.chat().expect("chat").push(Block::Assistant { label: None, text: "first".into() });
        app.chat().expect("chat").push(Block::Assistant { label: None, text: "second".into() });
        enter(&mut app, "/copy");
        assert_eq!(app.pending_copy.as_deref(), Some("second"));
    }

    #[test]
    fn each_cli_only_ever_gets_its_own_account_folder() {
        let (work, team) = (PathBuf::from("C:/u/.claude-work"), PathBuf::from("C:/u/.codex-team"));
        let accounts = || (Some(work.clone()), Some(team.clone()));
        let on = |spec: &str| {
            let (claude, codex) = accounts();
            account_for(&Target::parse(spec).expect("target"), claude, codex)
        };
        assert_eq!(on("claude:opus"), Some(work.clone()));
        assert_eq!(on("codex:gpt"), Some(team.clone()));
        assert_eq!(on("openrouter:x"), None);
    }

    #[tokio::test]
    async fn a_thread_keeps_one_account_per_provider() {
        let (mut app, _rx, _dir) = app(true);
        enter(&mut app, "/model claude:opus");
        pick(&mut app, Action::UseAccount(Provider::Claude, Some(PathBuf::from("C:/u/.claude-work"))));
        enter(&mut app, "/model codex:gpt");
        app.chat().expect("chat").state.vendor_session = Some("old".into());
        pick(&mut app, Action::UseAccount(Provider::Codex, Some(PathBuf::from("C:/u/.codex-team"))));
        let chat = app.chat().expect("chat");
        assert_eq!(chat.state.account_dir, Some(PathBuf::from("C:/u/.claude-work")));
        assert_eq!(chat.state.codex_account_dir, Some(PathBuf::from("C:/u/.codex-team")));
        assert!(chat.state.vendor_session.is_none());
        assert_eq!(chat.state.choice.label(), "codex:gpt");
    }

    #[tokio::test]
    async fn picking_another_providers_account_moves_the_thread_to_it() {
        let (mut app, _rx, _dir) = app(true);
        enter(&mut app, "/model claude:opus");
        pick(&mut app, Action::UseAccount(Provider::Codex, Some(PathBuf::from("C:/u/.codex-team"))));
        let chat = app.chat().expect("chat");
        assert!(chat.state.choice.label().starts_with("codex:"), "{}", chat.state.choice.label());
        assert_eq!(chat.state.account_dir, None);
    }

    #[test]
    fn account_add_reads_the_provider_and_the_name() {
        let codex = Some(&Provider::Codex);
        assert_eq!(parse_account_add("chatgpt work", None), (Provider::Codex, Some("work".into())));
        assert_eq!(parse_account_add("codex", None), (Provider::Codex, None));
        assert_eq!(parse_account_add("claude", codex), (Provider::Claude, None));
        assert_eq!(parse_account_add("work", codex), (Provider::Codex, Some("work".into())));
        assert_eq!(parse_account_add("work", None), (Provider::Claude, Some("work".into())));
        assert_eq!(parse_account_add("a b", None), (Provider::Claude, Some("a b".into())));
    }

    #[tokio::test]
    async fn switching_account_starts_fresh_and_keeps_the_model() {
        let (mut app, _rx, _dir) = app(true);
        enter(&mut app, "/model claude:opus");
        app.chat().expect("chat").state.vendor_session = Some("old".into());
        pick(&mut app, Action::UseAccount(Provider::Claude, Some(PathBuf::from("C:/u/.claude-work"))));
        let chat = app.chat().expect("chat");
        assert!(chat.state.vendor_session.is_none(), "a session can't resume under another account");
        assert_eq!(chat.state.choice.label(), "claude:opus");
        assert!(matches!(last_block(&mut app), Block::Note(t) if t.contains("work")));
    }

    #[test]
    fn attached_files_are_listed_so_every_model_can_find_them() {
        let files = [PathBuf::from("C:/a/shot.png"), PathBuf::from("C:/a/notes.txt")];
        assert_eq!(with_attachments("look", &files), "look\n\nAttached: C:/a/shot.png\nAttached: C:/a/notes.txt");
        assert!(with_attachments("", &files).starts_with("See the attached files."));
        assert_eq!(olympus_core::agent::attached_images(&with_attachments("look", &files)), [PathBuf::from("C:/a/shot.png")]);
    }

    #[tokio::test]
    async fn an_attachment_waits_until_you_send_and_goes_only_once() {
        let (mut app, _rx, _dir) = app(true);
        let shot = PathBuf::from("C:/a/shot.png");
        app.handle(AppEvent::Attach(shot.clone()));
        app.handle(AppEvent::Attach(shot.clone()));
        assert_eq!(app.attachments, std::slice::from_ref(&shot), "the same file twice is one attachment");
        assert!(app.chat().expect("chat").state.blocks.iter().all(|b| !matches!(b, Block::User(_))), "nothing was sent");
        enter(&mut app, "/rename Keep it");
        assert_eq!(app.attachments, [shot], "a command doesn't use it up");
        app.composer.set("what is this?");
        app.submit();
        assert!(app.attachments.is_empty());
        let sent = app.chat().expect("chat").state.blocks.iter().rev().find_map(|b| if let Block::User(text) = b { Some(text.clone()) } else { None }).expect("sent");
        assert_eq!(sent, "what is this?\n\nAttached: C:/a/shot.png");
    }

    #[tokio::test]
    async fn a_lone_attachment_can_be_sent_without_typing() {
        let (mut app, _rx, _dir) = app(true);
        app.handle(AppEvent::Attach(PathBuf::from("C:/a/shot.png")));
        app.submit();
        assert!(app.attachments.is_empty());
        assert!(app.chat().expect("chat").state.blocks.iter().any(|b| matches!(b, Block::User(t) if t.contains("Attached: C:/a/shot.png"))));
    }

    #[test]
    fn loop_finds_the_interval_at_either_end_like_claude_code() {
        let five = Some(Duration::from_secs(300));
        assert_eq!(parse_loop("5m check the deploy"), (five, "check the deploy".to_string()));
        assert_eq!(parse_loop("check the deploy every 5m"), (five, "check the deploy".to_string()));
        assert_eq!(parse_loop("run tests every 5 minutes"), (five, "run tests".to_string()));
        assert_eq!(parse_loop("check every PR"), (None, "check every PR".to_string()));
        assert_eq!(parse_loop("check the deploy"), (None, "check the deploy".to_string()));
        assert_eq!(parse_loop("5m"), (None, String::new()));
        assert_eq!(parse_loop("5m /simplify"), (five, "/simplify".to_string()));
    }

    #[test]
    fn a_self_paced_loop_reads_the_models_last_word() {
        assert_eq!(loop_pace("All good.\nLOOP: next 30m"), Some(Duration::from_secs(1800)));
        assert_eq!(loop_pace("All good.\n`LOOP: next 2h`"), Some(Duration::from_secs(7200)));
        assert_eq!(loop_pace("Nothing left.\nLOOP: done"), None);
        assert_eq!(loop_pace("It forgot to say"), Some(DEFAULT_LOOP_PACE));
        assert_eq!(loop_pace("LOOP: next 1s"), Some(SHORTEST_LOOP), "never hammers the model");
    }

    fn start_waiting_loop(app: &mut App, every: Option<Duration>) {
        let chat = app.chat().expect("chat");
        chat.looping = Some(Loop { prompt: "check".into(), every, started: Some(std::time::Instant::now()), generation: 0 });
    }

    fn finished(stop: olympus_core::event::Stop, text: &str) -> olympus_core::event::TurnResult {
        let mut result = olympus_core::event::TurnResult::failed(stop, String::new());
        result.text = text.to_string();
        result
    }

    #[tokio::test]
    async fn a_finished_run_schedules_the_next_and_stays_quiet() {
        use olympus_core::event::Stop;
        let (mut app, mut rx, _dir) = app(true);
        start_waiting_loop(&mut app, Some(Duration::from_millis(10)));
        let id = app.active_id();
        let target = Target::parse("claude:opus").expect("target");
        app.handle(AppEvent::TurnDone { window: id, target, result: finished(Stop::Completed, "ok") });
        assert!(app.notices.is_empty(), "a repeating run shouldn't chime every time");
        let tick = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(AppEvent::LoopTick { generation, .. }) = rx.recv().await {
                    return generation;
                }
            }
        });
        assert_eq!(tick.await.expect("the next run was scheduled"), 1);
    }

    #[tokio::test]
    async fn a_failed_run_ends_the_loop_and_announces_it() {
        use olympus_core::event::Stop;
        let (mut app, _rx, _dir) = app(true);
        start_waiting_loop(&mut app, Some(Duration::from_secs(300)));
        let id = app.active_id();
        let target = Target::parse("claude:opus").expect("target");
        app.handle(AppEvent::TurnDone { window: id, target, result: finished(Stop::Failed, "") });
        assert!(app.chat().expect("chat").looping.is_none());
        assert_eq!(app.notices.len(), 1);
    }

    #[tokio::test]
    async fn a_self_paced_loop_ends_when_the_model_says_done() {
        use olympus_core::event::Stop;
        let (mut app, _rx, _dir) = app(true);
        start_waiting_loop(&mut app, None);
        let id = app.active_id();
        let target = Target::parse("claude:opus").expect("target");
        app.handle(AppEvent::TurnDone { window: id, target, result: finished(Stop::Completed, "LOOP: done") });
        assert!(app.chat().expect("chat").looping.is_none());
        assert_eq!(app.notices.len(), 1, "the end of the loop is worth announcing");
    }

    #[tokio::test]
    async fn a_replaced_timer_does_nothing_and_stop_ends_a_waiting_loop() {
        let (mut app, _rx, _dir) = app(true);
        start_waiting_loop(&mut app, Some(Duration::from_secs(300)));
        let id = app.active_id();
        app.handle(AppEvent::LoopTick { window: id, generation: 99 });
        assert!(!app.chat().expect("chat").state.blocks.iter().any(|b| matches!(b, Block::User(_))));
        enter(&mut app, "/stop");
        assert!(app.chat().expect("chat").looping.is_none());
        assert!(matches!(last_block(&mut app), Block::Note(t) if t.contains("Loop stopped")));
    }

    #[tokio::test]
    async fn loop_refuses_olympus_commands_and_unknown_names_stay_errors() {
        let (mut app, _rx, _dir) = app(true);
        for line in ["/loop", "/loop 5m", "/loop 5m /review", "/loop /not-a-real-skill", "/not-a-real-skill"] {
            enter(&mut app, line);
            assert!(matches!(last_block(&mut app), Block::Error(_)), "{line}");
        }
        assert!(app.chat().expect("chat").looping.is_none());
    }

    #[test]
    fn slash_messages_use_claudes_real_command_or_explain_why_not() {
        let cwd = tempfile::tempdir().expect("cwd");
        let claude = Target::parse("claude:opus").expect("target");
        let codex = Target::parse("codex:gpt").expect("target");
        assert_eq!(slash_prompt(&claude, "/simplify".into(), cwd.path()), Ok(("/simplify".to_string(), true)));
        assert_eq!(slash_prompt(&codex, "hello".into(), cwd.path()), Ok(("hello".to_string(), false)));
        assert!(slash_prompt(&codex, "/not-a-real-skill".into(), cwd.path()).expect_err("nothing to run").contains("/model claude"));
    }

    #[test]
    fn claude_commands_appear_in_the_list_but_never_replace_olympus_commands() {
        let cwd = tempfile::tempdir().expect("cwd");
        let hints = skill_hints(cwd.path());
        assert!(hints.iter().any(|h| h.name == "/code-review"));
        assert!(!hints.iter().any(|h| COMMANDS.iter().any(|c| c.name == h.name)));
    }

    #[test]
    fn a_command_is_listed_before_any_longer_command_it_prefixes() {
        for (index, command) in COMMANDS.iter().enumerate() {
            let earlier_longer = COMMANDS[..index].iter().find(|c| c.name.starts_with(command.name));
            assert!(earlier_longer.is_none(), "{} would run instead of {}", earlier_longer.map_or("", |c| c.name), command.name);
        }
    }

    #[test]
    fn new_accounts_get_the_next_free_name_and_reuse_unfinished_sign_ins() {
        let home = tempfile::tempdir().expect("home");
        assert_eq!(next_account_name(home.path(), &CLAUDE_ACCOUNTS), "account2");
        std::fs::create_dir_all(home.path().join(".claude-account2")).expect("folder");
        assert_eq!(next_account_name(home.path(), &CLAUDE_ACCOUNTS), "account2", "nothing was signed in there yet");
        std::fs::write(home.path().join(".claude-account2").join(".credentials.json"), "{}").expect("write");
        assert_eq!(next_account_name(home.path(), &CLAUDE_ACCOUNTS), "account3");
        assert_eq!(next_account_name(home.path(), &CODEX_ACCOUNTS), "account2", "each provider counts its own");
        std::fs::create_dir_all(home.path().join(".codex-account2")).expect("folder");
        std::fs::write(home.path().join(".codex-account2").join("auth.json"), "{}").expect("write");
        assert_eq!(next_account_name(home.path(), &CODEX_ACCOUNTS), "account3");
    }

    #[test]
    fn account_rows_lead_with_the_email_and_mark_the_one_in_use() {
        assert_eq!(account_row_label(true, "me@example.com (max)", "work"), "me@example.com (max)");
        assert_eq!(account_row_label(false, "(signed out)", "work"), "work");
        assert_eq!(account_row_detail(true, &CLAUDE_ACCOUNTS, "work", true, Some("7d 12%".into())), "work · this thread · 7d 12%");
        assert_eq!(account_row_detail(true, &CLAUDE_ACCOUNTS, "work", false, None), "work");
        assert!(account_row_detail(false, &CLAUDE_ACCOUNTS, "work", false, None).contains("/account add work"));
        assert!(account_row_detail(false, &CODEX_ACCOUNTS, "work", false, None).contains("/account add chatgpt work"));
    }

    #[tokio::test]
    async fn notify_test_previews_both_sounds() {
        let (mut app, _rx, _dir) = app(true);
        enter(&mut app, "/notify test");
        enter(&mut app, "/notify test-failed");
        assert_eq!(app.notices.iter().map(|n| n.ok).collect::<Vec<_>>(), [true, false]);
    }

    #[tokio::test]
    async fn account_names_stay_inside_the_home_folder() {
        let (mut app, _rx, _dir) = app(true);
        for line in ["/account add", "/account add ../../evil", "/account add a b", "/account banana"] {
            enter(&mut app, line);
            assert!(matches!(last_block(&mut app), Block::Error(_)), "{line}");
        }
    }

    #[tokio::test]
    async fn merge_without_a_run_explains() {
        let (mut app, _rx, _dir) = app(true);
        enter(&mut app, "/merge");
        assert!(matches!(last_block(&mut app), Block::Error(t) if t.contains("/auto")));
    }

    #[tokio::test]
    async fn node_commands_need_a_selected_node() {
        let (mut app, _rx, _dir) = app(true);
        for line in ["/models", "/load", "/unload", "/hf qwen", "/pf qwen"] {
            enter(&mut app, line);
            assert!(matches!(last_block(&mut app), Block::Error(t) if t.contains("/nodes")), "{line}");
        }
    }

    #[tokio::test]
    async fn cd_rejects_missing_folders_and_clear_empties_the_chat() {
        let (mut app, _rx, _dir) = app(true);
        enter(&mut app, "/cd Z:/not/here");
        assert!(matches!(last_block(&mut app), Block::Error(_)));
        enter(&mut app, "/clear");
        assert!(app.chat().expect("chat").state.blocks.is_empty());
    }

    #[tokio::test]
    async fn a_busy_chat_refuses_background_work_without_losing_it() {
        let (mut app, _rx, _dir) = app(true);
        let id = app.active_id();
        app.window_mut(id).expect("window").activity = Activity::Running;
        enter(&mut app, "/auto do it");
        assert!(matches!(last_block(&mut app), Block::Error(t) if t.contains("busy")));
    }

    #[tokio::test]
    async fn resume_opens_a_window_bound_to_the_vendor_session() {
        let (mut app, _rx, dir) = app(true);
        pick(&mut app, Action::Resume { vendor: "codex".into(), id: "abc".into(), cwd: dir.path().to_path_buf(), title: "Old work".into() });
        let chat = app.chat().expect("chat");
        assert_eq!(chat.state.vendor_session.as_deref(), Some("abc"));
        assert_eq!(chat.state.title, "Old work");
    }

    #[test]
    fn paths_expand_home_and_relative() {
        let base = PathBuf::from("C:/work");
        assert_eq!(expand_path(&base, "sub"), base.join("sub"));
        assert_eq!(expand_path(&base, "~/x"), user_home().join("x"));
    }
}
