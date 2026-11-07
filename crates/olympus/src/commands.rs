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

// A repeating job needs a real message or skill, never one of Olympus's own commands.
fn repeatable(app: &App, prompt: &str, what: &str) -> Result<(), String> {
    if prompt.starts_with('/') {
        let name = prompt.split_whitespace().next().unwrap_or_default();
        if COMMANDS.iter().any(|c| c.name == name) || !is_passed_on(name, &app.current_cwd()) {
            return Err(format!("{what} repeats a message or a skill, not {name}."));
        }
    }
    Ok(())
}

/// The saved jobs, beside the threads folder.
pub const SCHEDULES_FILE: &str = "schedules.json";
const TELEGRAM_FILE: &str = "telegram.json";
/// A task that took at least this many tools gets an offer to save it as a skill.
pub const STEPS_WORTH_SAVING: usize = 8;
const USAGE_SCHEDULE: &str = "Say when and what, like /schedule daily 08:00 summarize my calendar, or /schedule every 1h check the deploy. Use /schedule list to see them.";

fn parse_schedule(args: &str) -> Option<(crate::schedule::When, String)> {
    use crate::schedule::{parse_time, When};
    let mut parts = args.trim().splitn(3, char::is_whitespace);
    let (kind, timing, prompt) = (parts.next()?, parts.next()?, parts.next()?.trim());
    let when = match kind {
        "every" => When::Every { seconds: parse_duration(timing)?.max(SHORTEST_LOOP).as_secs() },
        "daily" => parse_time(timing).map(|(hour, minute)| When::Daily { hour, minute })?,
        _ => return None,
    };
    (!prompt.is_empty()).then(|| (when, prompt.to_string()))
}

fn save_schedules(app: &mut App) {
    let path = app.sessions_dir.with_file_name(SCHEDULES_FILE);
    if let Err(error) = crate::schedule::save(&path, &app.schedules) {
        app.fail_active(format!("Unable to save your schedules: {error}"));
    }
}

fn schedule_list(app: &App) -> String {
    if app.schedules.is_empty() {
        return "No schedules yet. Try /schedule daily 08:00 summarize my calendar.".to_string();
    }
    app.schedules.iter().map(|job| format!("{}. {}: {}", job.id, job.when.label(), job.prompt)).collect::<Vec<_>>().join("\n")
}

fn schedule_command(app: &mut App, args: &str) {
    let args = args.trim();
    if args.is_empty() || args == "list" {
        let list = schedule_list(app);
        app.note_active(list);
        return;
    }
    if let Some(number) = args.strip_prefix("stop") {
        let before = app.schedules.len();
        let number = number.trim().parse::<u64>().ok();
        app.schedules.retain(|job| Some(job.id) != number);
        if app.schedules.len() == before {
            app.fail_active("Give the schedule's number, from /schedule list.");
            return;
        }
        save_schedules(app);
        app.note_active("Schedule stopped.");
        return;
    }
    let Some((when, prompt)) = parse_schedule(args) else {
        app.fail_active(USAGE_SCHEDULE);
        return;
    };
    if let Err(problem) = repeatable(app, &prompt, "/schedule") {
        app.fail_active(problem);
        return;
    }
    let Some(chat) = app.chat() else {
        app.fail_active("Open a chat to start a schedule. It reports in that chat.");
        return;
    };
    let job = crate::schedule::Schedule { id: 0, when, prompt, thread: chat.file_id.clone(), cwd: chat.state.cwd.clone(), account_dir: chat.state.account_dir.clone(), codex_account_dir: chat.state.codex_account_dir.clone(), choice: chat.state.choice.clone(), last_run: chrono::Local::now().timestamp() };
    let label = job.when.label();
    let id = app.schedules.iter().map(|existing| existing.id).max().unwrap_or(0) + 1;
    app.schedules.push(crate::schedule::Schedule { id, ..job });
    save_schedules(app);
    let delivery = if app.phone.is_some() { " Results also go to your phone." } else { "" };
    app.note_active(format!("Scheduled {label}. It reports here while Olympus is open, and a run missed while it's closed happens at the next launch.{delivery}"));
}

/// Wakes the schedules every half minute, starting just after launch.
pub fn start_schedule_clock(tx: tokio::sync::mpsc::UnboundedSender<AppEvent>) {
    tokio::spawn(async move {
        let mut clock = tokio::time::interval(Duration::from_secs(30));
        loop {
            clock.tick().await;
            if tx.send(AppEvent::ScheduleTick).is_err() {
                return;
            }
        }
    });
}

/// Starts any schedule that's due.
pub fn run_due_schedules(app: &mut App) {
    let now = chrono::Local::now();
    let due: Vec<usize> = app.schedules.iter().enumerate().filter(|(_, job)| crate::schedule::is_due(&job.when, job.last_run, &now)).map(|(index, _)| index).collect();
    for index in &due {
        run_schedule(app, *index, now.timestamp());
    }
    if !due.is_empty() {
        save_schedules(app);
    }
}

fn run_schedule(app: &mut App, index: usize, now: i64) {
    let job = app.schedules[index].clone();
    let found = app.windows.iter().find(|window| matches!(&window.pane, Pane::Chat(chat) if chat.file_id == job.thread)).map(|window| window.id);
    let id = found.unwrap_or_else(|| {
        // Its thread was closed. It gets a new one without taking the focus from what you're doing.
        let focused = app.active;
        let id = app.new_chat(job.cwd.clone());
        app.active = focused;
        id
    });
    let Some(chat) = app.window_mut(id).and_then(crate::session::Window::chat) else { return };
    chat.state.account_dir.clone_from(&job.account_dir);
    chat.state.codex_account_dir.clone_from(&job.codex_account_dir);
    chat.state.choice = job.choice.clone();
    // Each run starts fresh, so a daily job doesn't pay for all of its earlier runs.
    chat.state.vendor_session = None;
    chat.reply_to_phone = true;
    let thread = chat.file_id.clone();
    app.schedules[index].thread = thread;
    app.schedules[index].last_run = now;
    app.send(id, job.prompt);
}

fn search(app: &mut App, args: &str) {
    if args.trim().is_empty() {
        app.fail_active("Say what to look for, like /search rate limiting.");
        return;
    }
    let (tx, id, query) = (app.tx.clone(), app.active_id(), args.trim().to_string());
    app.picker = Some(Picker::loading("Search"));
    tokio::task::spawn_blocking(move || {
        let hits = Store::open(&home_dir().join("learn.db")).and_then(|store| store.search_messages(&query, 30));
        match hits {
            Ok(hits) => {
                let items = hits
                    .into_iter()
                    .map(|hit| {
                        let cwd = PathBuf::from(&hit.cwd);
                        let folder = cwd.file_name().map(|name| name.to_string_lossy().to_string()).unwrap_or_default();
                        let date: String = hit.timestamp.chars().take(10).collect();
                        let title = crate::session::short_title(hit.text.lines().next().unwrap_or("Past chat"));
                        PickItem { label: hit.snippet.replace('\n', " "), detail: format!("{} · {folder} · {date}", hit.vendor), action: Action::Resume { vendor: hit.vendor, id: hit.session, cwd, title } }
                    })
                    .collect();
                let _ = tx.send(AppEvent::Picker(Picker::new("Search", items, "Nothing matches that. Chats since you opened Olympus are added the next time it starts.")));
            }
            Err(error) => {
                let _ = tx.send(AppEvent::Picker(Picker::new("Search", Vec::new(), "Search isn't available.")));
                let _ = tx.send(AppEvent::Fail { window: id, text: format!("{error:#}") });
            }
        }
    });
}

fn save_skill(app: &mut App, args: &str) {
    let wanted = args.trim().to_string();
    if !wanted.is_empty() && skills::clean_skill_name(&wanted).is_none() {
        app.fail_active("Use a short name with letters and numbers, like /save-skill fix-flaky-tests.");
        return;
    }
    let transcript = app.chat().map(|chat| chat.handoff(40_000)).unwrap_or_default();
    if transcript.trim().is_empty() {
        app.fail_active("There's nothing in this chat to learn from yet.");
        return;
    }
    let Some((id, cwd, cancel)) = begin_background(app) else { return };
    let (rt, tx) = (app.rt.clone(), app.tx.clone());
    tokio::spawn(async move {
        let drafted = async {
            let target = rt.pick(olympus_core::runtime::Tier::Standard, "write a skill", &cwd).await?;
            let mut req = olympus_core::agent::TurnRequest::new(target, skills::draft_prompt(&transcript), cwd);
            req.access = olympus_core::agent::Access::ReadOnly;
            req.ephemeral = true;
            req.use_memory = false;
            let result = olympus_core::agent::run_turn(&rt, &req, cancel, &mut |_| {}).await;
            if !result.ok() {
                return Err(result.error.unwrap_or_else(|| "The model failed.".to_string()));
            }
            let (name, contents) = skills::parse_draft(&result.text, (!wanted.is_empty()).then_some(wanted.as_str()))?;
            skills::save_draft(&home_dir(), &name, &contents).map_err(|error| format!("Unable to save the draft: {error}"))?;
            Ok::<_, String>((name, contents))
        };
        let event = match drafted.await {
            Ok((name, contents)) => AppEvent::Answer { window: id, label: "Skill draft".into(), text: format!("{contents}\n\nThis isn't used yet. Run `/keep-skill {name}` to approve it, or `/drop-skill {name}` to delete it.") },
            Err(text) => AppEvent::Fail { window: id, text },
        };
        let _ = tx.send(event);
        let _ = tx.send(AppEvent::Done { window: id });
    });
}

fn keep_skill(app: &mut App, args: &str) {
    match skills::keep_draft(&home_dir(), &user_home(), args) {
        Ok(path) => {
            app.skill_hints = skill_hints(&app.current_cwd());
            app.note_active(format!("Skill saved to {}. It's available from your next message.", path.display()));
        }
        Err(problem) => app.fail_active(problem),
    }
}

/// Reconnects at launch if a bot was set up before.
pub fn resume_phone(app: &mut App) {
    let path = app.sessions_dir.with_file_name(TELEGRAM_FILE);
    let Some(settings) = crate::telegram::load(&path) else { return };
    let (voice, tx) = (app.voice.clone(), app.tx.clone());
    let base = crate::telegram::api_base(&settings.token);
    app.phone = Some(crate::telegram::Phone::start(base, settings, path, voice, tx));
}

fn telegram_command(app: &mut App, args: &str) {
    let path = app.sessions_dir.with_file_name(TELEGRAM_FILE);
    let args = args.trim();
    if args == "off" {
        app.phone = None;
        match std::fs::remove_file(&path) {
            Ok(()) => app.note_active("Telegram is off, and the bot's token is deleted from this PC."),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => app.note_active("Telegram wasn't connected."),
            Err(error) => app.fail_active(format!("Unable to delete the token: {error}")),
        }
        return;
    }
    if let Some(token) = args.strip_prefix("connect").map(str::trim).filter(|token| !token.is_empty()) {
        let (tx, token) = (app.tx.clone(), token.to_string());
        app.note_active("Checking that token with Telegram.");
        tokio::spawn(async move {
            let bot = crate::telegram::bot_name(&crate::telegram::api_base(&token)).await;
            let _ = tx.send(AppEvent::PhoneChecked { token, bot });
        });
        return;
    }
    let status = match &app.phone {
        Some(phone) if phone.settings.chat_id.is_some() => "Telegram is connected. Text your bot, or send it a voice memo. /telegram off disconnects it.".to_string(),
        Some(phone) => format!("Waiting for your code. Send {} to your bot.", phone.code.as_deref().unwrap_or("nothing")),
        None => "Message @BotFather on Telegram, send /newbot, then run /telegram connect <token> with the token it gives you.".to_string(),
    };
    app.note_active(status);
}

/// Starts the connection once Telegram has accepted a token. The token never appears in a message.
pub fn finish_phone_connect(app: &mut App, token: String, bot: Option<String>) {
    let Some(bot) = bot else {
        app.fail_active("Telegram didn't accept that token. Copy it again from @BotFather.");
        return;
    };
    let path = app.sessions_dir.with_file_name(TELEGRAM_FILE);
    let settings = crate::telegram::Settings { token, chat_id: None };
    if let Err(error) = crate::telegram::save(&path, &settings) {
        app.fail_active(format!("Unable to save the connection: {error}"));
        return;
    }
    let base = crate::telegram::api_base(&settings.token);
    let (voice, tx) = (app.voice.clone(), app.tx.clone());
    let phone = crate::telegram::Phone::start(base, settings, path, voice, tx);
    let code = phone.code.clone().unwrap_or_default();
    app.phone = Some(phone);
    app.note_active(format!("Connected to @{bot}. To link your phone, send it this code: {code}. After that, only your chat can reach Olympus."));
}

fn sync_layout(app: &App) -> crate::sync::Layout {
    crate::sync::Layout { user_home: user_home(), olympus_home: app.sessions_dir.parent().map_or_else(home_dir, Path::to_path_buf) }
}

/// Reconnects at launch if sync was set up before; a store that's away shows up on the first round.
pub fn resume_sync(app: &mut App) {
    let Some(settings) = crate::sync::Settings::load(&app.sessions_dir.with_file_name(crate::sync::SETTINGS_FILE)) else { return };
    let syncer = crate::sync::Syncer::new(crate::sync::open_store(&settings.place), sync_layout(app), &settings);
    app.sync = Some(crate::sync::Link::new(syncer, settings));
}

/// Wakes syncing every few seconds, which then decides whether a round is due.
pub fn start_sync_clock(tx: tokio::sync::mpsc::UnboundedSender<AppEvent>) {
    tokio::spawn(async move {
        let mut clock = tokio::time::interval(Duration::from_secs(5));
        loop {
            clock.tick().await;
            if tx.send(AppEvent::SyncTick).is_err() {
                return;
            }
        }
    });
}

/// Starts a sync round if one is due and none is running.
pub fn sync_tick(app: &mut App) {
    let Some(link) = app.sync.as_mut() else { return };
    if link.last_round.is_some_and(|last| last.elapsed() < link.settings.every()) {
        return;
    }
    let Some(mut syncer) = link.syncer.take() else { return };
    link.last_round = Some(std::time::Instant::now());
    let mut open = Vec::new();
    for window in &app.windows {
        let Pane::Chat(chat) = &window.pane else { continue };
        if chat.state.blocks.is_empty() || crate::obiview::is_obiview(chat) {
            continue;
        }
        let key = chat.sync_key();
        if link.keys.get(&chat.file_id) != Some(&key) && crate::session::save(chat, &app.sessions_dir).is_ok() {
            link.keys.insert(chat.file_id.clone(), key);
        }
        open.push(crate::sync::Open { id: chat.file_id.clone(), running: window.activity == Activity::Running });
    }
    let closed = std::mem::take(&mut app.sync_closed);
    let tx = app.tx.clone();
    tokio::task::spawn_blocking(move || {
        let outcome = syncer.cycle(&open, closed);
        let _ = tx.send(AppEvent::SyncDone(syncer, outcome));
    });
}

const SYNC_PAUSED: &str = "Sync paused: ";

fn add_synced_chat(app: &mut App, id: String, state: crate::session::ChatState) {
    let _ = std::fs::remove_file(app.sessions_dir.join("closed").join(format!("{id}.json")));
    let chat = Chat::restore(state, id);
    // If this fails, the next sync round still has the chat to save.
    let _ = crate::session::save(&chat, &app.sessions_dir);
    let focused = app.active;
    let window = app.add_window(Pane::Chat(Box::new(chat)));
    app.active = focused;
    if let Some(window) = app.window_mut(window) {
        window.activity = Activity::Unseen;
    }
}

fn apply_incoming(app: &mut App, incoming: crate::sync::Incoming) {
    use crate::sync::Incoming;
    let focused = app.active_id();
    match incoming {
        Incoming::Copy { id, state } => add_synced_chat(app, id, *state),
        Incoming::Apply { id, state, expected, running_on, copy, copy_id } => {
            let found = app.windows.iter_mut().find(|window| matches!(&window.pane, Pane::Chat(chat) if chat.file_id == id));
            let Some(window) = found else {
                add_synced_chat(app, id.clone(), *state);
                if let Some(window) = app.windows.last_mut() {
                    if running_on.is_some() {
                        window.activity = Activity::Idle;
                    }
                    if let Some(chat) = window.chat() {
                        chat.elsewhere = running_on.map(|device| (device, std::time::Instant::now()));
                    }
                }
                return;
            };
            let (window_id, running) = (window.id, window.activity == Activity::Running);
            let Some(chat) = window.chat() else { return };
            // You typed or started something during the round, so neither version may be lost.
            if running || crate::sync::state_sha(&chat.state) != expected {
                add_synced_chat(app, copy_id, *copy);
                return;
            }
            // It's only news once it has stopped; until then the card says where it's running.
            let news = running_on.is_none() && (chat.elsewhere.is_some() || state.blocks.len() != chat.state.blocks.len());
            chat.state = *state;
            chat.pending_run = None;
            chat.elsewhere = running_on.map(|device| (device, std::time::Instant::now()));
            chat.touch(0);
            chat.follow = window_id == focused;
            let _ = crate::session::save(chat, &app.sessions_dir);
            let started_elsewhere = chat.elsewhere.is_some();
            if let Some(window) = app.window_mut(window_id) {
                if started_elsewhere {
                    window.activity = Activity::Idle;
                } else if news && window_id != focused {
                    window.activity = Activity::Unseen;
                }
            }
        }
        Incoming::Close { id, expected } => {
            let found = app.windows.iter_mut().position(|window| {
                let idle = window.activity != Activity::Running;
                matches!(&window.pane, Pane::Chat(chat) if idle && chat.file_id == id && crate::sync::state_sha(&chat.state) == expected)
            });
            if let Some(index) = found {
                let flash = app.pending_flash.take();
                app.close_window(index);
                app.pending_flash = flash;
                // Other PCs already know, and telling them again would count as a new close.
                app.sync_closed.retain(|closed| *closed != id);
                if let Some(index) = app.windows.iter().position(|window| window.id == focused) {
                    app.active = index;
                }
            }
        }
    }
}

/// Applies a finished round. The chats are saved before the next round can read them.
pub fn sync_done(app: &mut App, syncer: Box<crate::sync::Syncer>, outcome: crate::sync::Outcome) {
    // Sync was turned off or reconnected during the round, so this worker has nothing left to do.
    if !app.sync.as_ref().is_some_and(|link| link.syncer.is_none()) {
        return;
    }
    for incoming in outcome.incoming {
        apply_incoming(app, incoming);
    }
    let Some(link) = app.sync.as_mut() else { return };
    link.syncer = Some(syncer);
    link.unreadable = outcome.unreadable;
    link.devices = outcome.devices;
    match outcome.error {
        Some(problem) => {
            app.status = format!("{SYNC_PAUSED}{problem}");
            link.problem = Some(problem);
        }
        None => {
            link.last_ok = Some(std::time::Instant::now());
            link.problem = None;
            if app.status.starts_with(SYNC_PAUSED) {
                app.status.clear();
            }
        }
    }
}

fn sync_places() -> Vec<(String, PathBuf)> {
    let home = user_home();
    let mut places = Vec::new();
    if let Some(folder) = std::env::var_os("OneDrive").map(PathBuf::from).filter(|folder| folder.is_dir()) {
        places.push(("OneDrive".to_string(), folder));
    }
    for (name, folder) in [("Dropbox", home.join("Dropbox")), ("Google Drive", home.join("Google Drive")), ("Google Drive", PathBuf::from("G:\\My Drive")), ("iCloud Drive", home.join("iCloudDrive"))] {
        if folder.is_dir() && !places.iter().any(|(known, _)| known == name) {
            places.push((name.to_string(), folder));
        }
    }
    places
}

fn sync_status(link: &crate::sync::Link) -> String {
    let when = link.last_ok.map_or_else(|| "It hasn't finished a round yet.".to_string(), |last| format!("Last synced {} ago.", short_duration(Duration::from_secs(last.elapsed().as_secs()))));
    let mut lines = vec![format!("Syncing with {}. {when}", link.settings.place)];
    if !link.devices.is_empty() {
        lines.push(format!("Also on: {}.", link.devices.iter().cloned().collect::<Vec<_>>().join(", ")));
    }
    if let Some(problem) = &link.problem {
        lines.push(format!("Paused: {problem}"));
    }
    if link.unreadable > 0 {
        lines.push(format!("{} files there can't be read with this code, so they're skipped.", link.unreadable));
    }
    lines.push("/sync code shows the code for another PC. /sync off stops syncing.".to_string());
    lines.join("\n")
}

fn show_sync_code(app: &mut App, code: String) {
    let item = PickItem { label: code.clone(), detail: "Enter copies it. Keep it private.".into(), action: Action::CopyText(code) };
    app.picker = Some(Picker::new("Sync code", vec![item], ""));
}

fn sync_command(app: &mut App, args: &str) {
    let args = args.trim();
    let settings_path = app.sessions_dir.with_file_name(crate::sync::SETTINGS_FILE);
    match args {
        "" => {
            if let Some(status) = app.sync.as_ref().map(sync_status) {
                app.note_active(status);
                return;
            }
            let mut items: Vec<PickItem> = sync_places()
                .into_iter()
                .map(|(name, folder)| {
                    let place = folder.join("Olympus Sync");
                    PickItem { label: name, detail: format!("Keeps encrypted chats in {}", place.display()), action: Action::Run(format!("/sync connect \"{}\"", place.display())) }
                })
                .collect();
            items.push(PickItem { label: "Choose a folder…".into(), detail: "Any folder a cloud app syncs, a network share, or a USB drive".into(), action: Action::BrowseSyncFolder });
            items.push(PickItem { label: "Another service…".into(), detail: "S3, R2, WebDAV, Google Drive and 70 more through rclone".into(), action: Action::Prefill("/sync connect remote:Olympus".into()) });
            app.picker = Some(Picker::new("Sync your chats to", items, ""));
        }
        "code" => match app.sync.as_ref().map(|link| link.settings.code.clone()) {
            Some(code) => show_sync_code(app, code),
            None => app.fail_active("Sync isn't on yet. Run /sync to choose where your chats go."),
        },
        "off" => {
            app.sync = None;
            app.sync_closed.clear();
            match std::fs::remove_file(&settings_path) {
                Ok(()) => app.note_active("Sync is off, and its code is deleted from this PC. Your chats stay here, and what's already in your storage stays there. To rejoin that storage, get the code from another PC with /sync code. If this was your only PC, connect to a new folder instead."),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => app.note_active("Sync wasn't on."),
                Err(error) => app.fail_active(format!("Sync is off until you restart, but its settings couldn't be deleted: {error}")),
            }
        }
        _ => {
            let Some(rest) = args.strip_prefix("connect").map(str::trim) else {
                app.fail_active("Use /sync, /sync connect <folder or rclone remote> [code], /sync code, or /sync off.");
                return;
            };
            // Two workers can't share one connection, and this one may be mid-round.
            if app.sync.is_some() {
                app.fail_active("Sync is already on. Run /sync off first to connect somewhere else.");
                return;
            }
            let Some((place, code)) = crate::sync::parse_connect(rest) else {
                app.fail_active("Say where to sync, like /sync connect D:\\Sync or /sync connect gdrive:Olympus.");
                return;
            };
            if place.starts_with(['-', ':']) {
                app.fail_active("Make a named remote with rclone config, so its keys stay out of Olympus. Then use /sync connect name:folder.");
                return;
            }
            let place = if crate::sync::is_remote(&place) { place } else { expand_path(&app.current_cwd(), &place).to_string_lossy().to_string() };
            let (layout, tx) = (sync_layout(app), app.tx.clone());
            let device = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "another PC".to_string());
            app.status = "Connecting to your storage".to_string();
            tokio::task::spawn_blocking(move || {
                let result = crate::sync::connect(crate::sync::open_store(&place), layout, &place, code, device);
                let _ = tx.send(AppEvent::SyncConnected(result.map(Box::new).map_err(|error| (place, error))));
            });
        }
    }
}

/// Starts syncing once a store has accepted this PC.
pub fn sync_connected(app: &mut App, result: Result<Box<(crate::sync::Syncer, crate::sync::Settings)>, (String, crate::sync::ConnectError)>) {
    app.status.clear();
    match result {
        Ok(connected) => {
            let (syncer, settings) = *connected;
            if let Err(error) = settings.save(&app.sessions_dir.with_file_name(crate::sync::SETTINGS_FILE)) {
                app.fail_active(format!("Unable to save the connection: {error}"));
                return;
            }
            let code = settings.code.clone();
            let place = settings.place.clone();
            app.sync = Some(crate::sync::Link::new(syncer, settings));
            app.note_active(format!("Syncing your open threads with {place}. They're encrypted on this PC before they go there. On your other PC, run /sync connect with the same place and this code. /sync code shows it again."));
            show_sync_code(app, code);
        }
        Err((place, crate::sync::ConnectError::NeedsCode)) => {
            app.composer.set(&format!("/sync connect \"{place}\" "));
            app.note_active("That storage already has synced chats. Add the code from your other PC, where /sync code shows it, and press Enter.");
        }
        Err((_, crate::sync::ConnectError::Problem(problem))) => app.fail_active(problem),
    }
}

fn chat_windows(app: &mut App) -> Vec<(u64, String)> {
    app.windows.iter_mut().filter_map(|window| { let id = window.id; window.chat().map(|chat| (id, chat.state.title.clone())) }).collect()
}

const PHONE_HELP: &str = "Text me or send a voice memo, and I'll reply when the work is done.\n\n/threads lists your threads\n/use 2 switches to thread 2\n/new starts a new thread\n/stop stops the current work\n/schedules lists your schedules";

/// Handles one message from Telegram.
pub fn phone_message(app: &mut App, chat: i64, text: &str) {
    let Some(phone) = app.phone.as_mut() else { return };
    let text = text.trim();
    if phone.settings.chat_id.is_none() {
        pair_phone(phone, chat, text);
        return;
    }
    let chats = chat_windows(app);
    let current = app.phone.as_ref().and_then(|phone| phone.thread).filter(|id| chats.iter().any(|(chat, _)| chat == id)).or_else(|| chats.last().map(|(id, _)| *id));
    let say = |app: &App, message: String| {
        if let Some(phone) = &app.phone {
            phone.say(message);
        }
    };
    let (command, argument) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    match command {
        "/start" | "/help" => say(app, PHONE_HELP.to_string()),
        "/threads" => {
            let list: Vec<String> = chats.iter().enumerate().map(|(index, (id, title))| format!("{}{}. {title}", if Some(*id) == current { "→ " } else { "" }, index + 1)).collect();
            say(app, list.join("\n"));
        }
        "/use" => match argument.trim().parse::<usize>().ok().and_then(|number| number.checked_sub(1)).and_then(|index| chats.get(index)) {
            Some((id, title)) => {
                let (id, title) = (*id, title.clone());
                if let Some(phone) = app.phone.as_mut() {
                    phone.thread = Some(id);
                }
                say(app, format!("Now in: {title}"));
            }
            None => say(app, "Give a number from /threads, like /use 2.".to_string()),
        },
        "/new" => {
            let (focused, cwd) = (app.active, app.current_cwd());
            let id = app.new_chat(cwd);
            app.active = focused;
            if let Some(phone) = app.phone.as_mut() {
                phone.thread = Some(id);
            }
            say(app, "Started a new thread. Your next message goes there.".to_string());
        }
        "/stop" => {
            let stopped = current.and_then(|id| app.window_mut(id)).and_then(crate::session::Window::chat).is_some_and(|chat| {
                chat.queue.clear();
                chat.cancel.as_ref().is_some_and(|cancel| cancel.send(true).is_ok())
            });
            if !stopped {
                say(app, "Nothing is running there.".to_string());
            }
        }
        "/schedules" => {
            let list = schedule_list(app);
            say(app, list);
        }
        _ if text.starts_with('/') && !is_passed_on(command, &app.current_cwd()) => say(app, format!("I don't know {command}. Send /help to see what I can do.")),
        _ => send_from_phone(app, current, chat, text),
    }
}

// The code goes to anyone who opens the bot, so wrong guesses close the door for good.
fn pair_phone(phone: &mut crate::telegram::Phone, chat: i64, text: &str) {
    let Some(code) = phone.code.clone() else { return };
    if text.eq_ignore_ascii_case(&code) {
        let saved = phone.pair(chat);
        phone.say(if saved.is_ok() { format!("Linked. Only this chat can reach Olympus now.\n\n{PHONE_HELP}") } else { "Linked for now, but I couldn't save it, so you'll need to pair again after a restart.".to_string() });
        return;
    }
    phone.wrong_codes += 1;
    if phone.wrong_codes >= crate::telegram::MAX_PAIRING_TRIES {
        phone.code = None;
    }
}

fn send_from_phone(app: &mut App, thread: Option<u64>, chat: i64, text: &str) {
    let id = thread.unwrap_or_else(|| {
        let (focused, cwd) = (app.active, app.current_cwd());
        let id = app.new_chat(cwd);
        app.active = focused;
        id
    });
    if let Some(phone) = app.phone.as_mut() {
        phone.thread = Some(id);
        let _ = phone.outbox.send(crate::telegram::Outbound::Typing(chat));
    }
    let Some(window) = app.window_mut(id) else { return };
    let busy = window.activity == Activity::Running;
    let Some(thread_chat) = window.chat() else { return };
    thread_chat.reply_to_phone = true;
    let title = thread_chat.state.title.clone();
    app.send(id, text.to_string());
    if let Some(phone) = &app.phone {
        phone.say(if busy { format!("Queued in: {title}") } else { format!("On it, in: {title}") });
    }
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
    if let Err(problem) = repeatable(app, &prompt, "/loop") {
        app.fail_active(problem);
        return;
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
    cmd("/schedule", "/schedule every 1h <message>|daily 08:00 <message>|list|stop <number>", "Run something on a timetable, even after a restart", true),
    cmd("/search", "/search <words>", "Find words in your past Claude and Codex chats", true),
    cmd("/save-skill", "/save-skill [name]", "Draft a skill from how this chat did its task", true),
    cmd("/keep-skill", "/keep-skill <name>", "Approve a skill draft so it can be used", true),
    cmd("/drop-skill", "/drop-skill <name>", "Delete a skill draft", true),
    cmd("/telegram", "/telegram connect <token>|off", "Reach this Olympus from your phone", true),
    cmd("/mirror", "/mirror on|off|forget", "Open Olympus in a browser on your network, or pair a phone", true),
    cmd("/obiview", "/obiview apps|on|off|pause|now|summaries off|<model> [messages]|exclude <app>|include <app>", "Open your vault and what Obiview has read", true),
    cmd("/note", "/note <name>", "Open a note in your vault, or start a new one", true),
    cmd("/talk", "/talk <words>", "Tell Jarvis something by typing, the same as holding Shift+B", true),
    cmd("/jarvis", "/jarvis <question>", "Ask what's going on, from anything Obiview has read", true),
    cmd("/sync", "/sync connect <folder or rclone remote> [code]|code|off", "Keep your chats and their memory the same on every PC", true),
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
    if let Some(vault) = &app.obi.vault {
        items.extend(vault.notes().iter().map(|note| PickItem { label: note.title.clone(), detail: format!("note · {}", note.rel), action: Action::OpenNote(note.rel.clone()) }));
    }
    Picker::new("Olympus", items, "")
}

/// Carries out a picker choice.
pub fn pick(app: &mut App, action: Action) {
    match action {
        Action::OpenNote(rel) => {
            crate::obiview::focus(app);
            crate::obiview::open_note(app, &rel);
        }
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
        Action::BrowseSyncFolder => app.pending_sync_folder = true,
        Action::CopyText(text) => {
            app.pending_copy = Some(text);
            app.pending_flash = Some("Sync code copied.".into());
        }
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
        "/schedule" => schedule_command(app, args),
        "/search" => search(app, args),
        "/save-skill" => save_skill(app, args),
        "/keep-skill" => keep_skill(app, args),
        "/drop-skill" => match skills::drop_draft(&home_dir(), args) {
            Ok(()) => app.note_active("Draft deleted."),
            Err(problem) => app.fail_active(problem),
        },
        "/telegram" => telegram_command(app, args),
        "/mirror" => crate::mirror::command(app, args),
        "/obiview" => crate::obiview::command(app, args),
        "/jarvis" => crate::obiview::ask_jarvis(app, args),
        "/talk" => crate::jarvis::heard(app, args),
        "/note" => crate::obiview::note_command(app, args),
        "/sync" => sync_command(app, args),
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

    fn linked_to_sync(app: &mut App, store: &Path) -> Box<crate::sync::Syncer> {
        let layout = sync_layout(app);
        std::fs::create_dir_all(&layout.olympus_home).expect("home");
        let (syncer, settings) = crate::sync::connect(Box::new(crate::sync::FolderStore(store.to_path_buf())), layout, "s", None, "PC".into()).expect("connect");
        let mut link = crate::sync::Link::new(syncer, settings);
        let syncer = link.syncer.take().expect("syncer");
        app.sync = Some(link);
        syncer
    }

    fn other_pcs_chat(app: &App) -> crate::session::ChatState {
        let mut state = Chat::new("From elsewhere".into(), app.sessions_dir.clone(), Choice::Auto).state;
        state.blocks.push(Block::User("hello from the other PC".into()));
        state
    }

    fn results(incoming: crate::sync::Incoming) -> crate::sync::Outcome {
        crate::sync::Outcome { incoming: vec![incoming], ..crate::sync::Outcome::default() }
    }

    fn arriving(app: &App, id: &str, expected: Option<String>, running_on: Option<&str>) -> crate::sync::Incoming {
        let state = other_pcs_chat(app);
        crate::sync::Incoming::Apply { id: id.into(), state: Box::new(state.clone()), expected, running_on: running_on.map(String::from), copy: Box::new(state), copy_id: format!("{id}-copy") }
    }

    fn open_chat_with_content(app: &mut App) -> (u64, String, Option<String>) {
        let id = app.active_id();
        let chat = app.chat().expect("chat");
        chat.push(Block::User("mine".into()));
        let (file, sha) = (chat.file_id.clone(), crate::sync::state_sha(&chat.state));
        (id, file, sha)
    }

    #[test]
    fn a_chat_arriving_from_another_pc_doesnt_take_the_focus() {
        let (mut app, _rx, _dir) = app(true);
        let store = tempfile::tempdir().expect("store");
        let syncer = linked_to_sync(&mut app, store.path());
        let focused = app.active_id();

        let incoming = arriving(&app, "new", None, None);
        sync_done(&mut app, syncer, results(incoming));

        assert_eq!(app.active_id(), focused);
        let window = app.windows.last().expect("window");
        assert_eq!((window.title(), window.activity), ("From elsewhere", Activity::Unseen));
        assert!(app.sessions_dir.join("new.json").exists());
        assert!(app.sync.as_ref().expect("link").syncer.is_some(), "the worker must be back for the next round");
    }

    #[test]
    fn a_chat_running_elsewhere_counts_as_finished_only_once_it_stops() {
        let (mut app, _rx, _dir) = app(true);
        let store = tempfile::tempdir().expect("store");
        let syncer = linked_to_sync(&mut app, store.path());
        let focused = app.active_id();

        let incoming = arriving(&app, "new", None, Some("LAPTOP"));
        sync_done(&mut app, syncer, results(incoming));
        let window = app.windows.last_mut().expect("window");
        assert_eq!(window.activity, Activity::Idle);
        let (id, sha) = {
            let chat = window.chat().expect("chat");
            assert_eq!(chat.running_elsewhere(), Some("LAPTOP"));
            (chat.file_id.clone(), crate::sync::state_sha(&chat.state))
        };

        let syncer = app.sync.as_mut().expect("link").syncer.take().expect("worker");
        let incoming = arriving(&app, &id, sha, None);
        sync_done(&mut app, syncer, results(incoming));

        assert_eq!(app.active_id(), focused);
        let window = app.windows.last_mut().expect("window");
        assert_eq!(window.activity, Activity::Unseen);
        assert_eq!(window.chat().expect("chat").running_elsewhere(), None);
    }

    #[test]
    fn a_finished_chat_that_starts_again_elsewhere_stops_saying_finished() {
        let (mut app, _rx, _dir) = app(true);
        let store = tempfile::tempdir().expect("store");
        let syncer = linked_to_sync(&mut app, store.path());
        let incoming = arriving(&app, "new", None, None);
        sync_done(&mut app, syncer, results(incoming));
        let window = app.windows.last_mut().expect("window");
        assert_eq!(window.activity, Activity::Unseen);
        let sha = crate::sync::state_sha(&window.chat().expect("chat").state);

        let syncer = app.sync.as_mut().expect("link").syncer.take().expect("worker");
        let incoming = arriving(&app, "new", sha, Some("LAPTOP"));
        sync_done(&mut app, syncer, results(incoming));

        assert_eq!(app.windows.last().expect("window").activity, Activity::Idle);
    }

    #[test]
    fn an_update_replaces_an_unchanged_chat_in_place() {
        let (mut app, _rx, _dir) = app(true);
        let store = tempfile::tempdir().expect("store");
        let syncer = linked_to_sync(&mut app, store.path());
        let (_, file, sha) = open_chat_with_content(&mut app);
        let count = app.windows.len();

        let incoming = arriving(&app, &file, sha, None);
        sync_done(&mut app, syncer, results(incoming));

        assert_eq!(app.windows.len(), count);
        assert_eq!(app.chat().expect("chat").state.title, "From elsewhere");
    }

    #[test]
    fn typing_during_a_round_keeps_both_versions() {
        let (mut app, _rx, _dir) = app(true);
        let store = tempfile::tempdir().expect("store");
        let syncer = linked_to_sync(&mut app, store.path());
        let (_, file, sha) = open_chat_with_content(&mut app);

        app.chat().expect("chat").push(Block::User("typed while syncing".into()));
        let incoming = arriving(&app, &file, sha, None);
        sync_done(&mut app, syncer, results(incoming));

        assert!(matches!(app.chat().expect("chat").state.blocks.last(), Some(Block::User(text)) if text == "typed while syncing"));
        let copy = app.windows.last_mut().and_then(crate::session::Window::chat).expect("copy");
        assert_eq!((copy.file_id.as_str(), copy.state.title.as_str()), (format!("{file}-copy").as_str(), "From elsewhere"));
    }

    #[test]
    fn a_message_waits_while_the_chat_runs_on_another_pc() {
        let (mut app, _rx, _dir) = app(true);
        let store = tempfile::tempdir().expect("store");
        let syncer = linked_to_sync(&mut app, store.path());
        let (id, file, sha) = open_chat_with_content(&mut app);
        let incoming = arriving(&app, &file, sha, Some("LAPTOP"));
        sync_done(&mut app, syncer, results(incoming));
        let before = app.chat().expect("chat").state.blocks.len();

        app.send(id, "carry on".into());

        assert_eq!(app.chat().expect("chat").state.blocks.len(), before);
        assert_eq!(app.composer.text, "carry on");
        assert!(app.pending_flash.as_deref().is_some_and(|text| text.contains("LAPTOP")));
        assert_ne!(app.windows.iter().find(|window| window.id == id).expect("window").activity, Activity::Running);
    }

    #[test]
    fn closing_elsewhere_closes_it_here_without_telling_everyone_again() {
        let (mut app, _rx, _dir) = app(true);
        let store = tempfile::tempdir().expect("store");
        let syncer = linked_to_sync(&mut app, store.path());
        let (id, file, sha) = open_chat_with_content(&mut app);
        let other = app.new_chat(app.sessions_dir.clone());

        sync_done(&mut app, syncer, results(crate::sync::Incoming::Close { id: file, expected: sha }));

        assert!(app.windows.iter().all(|window| window.id != id));
        assert_eq!(app.active_id(), other);
        assert!(app.sync_closed.is_empty());
    }

    #[test]
    fn closing_a_chat_here_tells_other_pcs_only_when_syncing() {
        let (mut app, _rx, _dir) = app(true);
        app.chat().expect("chat").push(Block::User("x".into()));
        app.close_active();
        assert!(app.sync_closed.is_empty());

        let store = tempfile::tempdir().expect("store");
        let _worker = linked_to_sync(&mut app, store.path());
        let (_, file, _) = open_chat_with_content(&mut app);
        app.close_active();
        assert_eq!(app.sync_closed, vec![file]);
    }

    #[test]
    fn a_round_from_before_sync_was_turned_off_changes_nothing() {
        let (mut app, _rx, _dir) = app(true);
        let store = tempfile::tempdir().expect("store");
        let syncer = linked_to_sync(&mut app, store.path());
        let count = app.windows.len();
        run(&mut app, "/sync off");

        let incoming = arriving(&app, "late", None, None);
        sync_done(&mut app, syncer, results(incoming));

        assert_eq!(app.windows.len(), count);
    }

    #[test]
    fn storage_that_already_has_chats_asks_for_the_code_and_keeps_your_place() {
        let (mut app, _rx, _dir) = app(true);
        sync_connected(&mut app, Err(("D:\\My Sync".into(), crate::sync::ConnectError::NeedsCode)));
        assert_eq!(app.composer.text, "/sync connect \"D:\\My Sync\" ");
        assert!(app.sync.is_none());
    }

    #[test]
    fn connecting_saves_the_connection_and_shows_the_code_without_writing_it_in_the_chat() {
        let (mut app, _rx, _dir) = app(true);
        let store = tempfile::tempdir().expect("store");
        let layout = sync_layout(&app);
        std::fs::create_dir_all(&layout.olympus_home).expect("home");
        let connected = crate::sync::connect(Box::new(crate::sync::FolderStore(store.path().to_path_buf())), layout, "s", None, "PC".into()).expect("connect");
        let code = connected.1.code.clone();

        sync_connected(&mut app, Ok(Box::new(connected)));

        assert!(app.sync.is_some());
        assert_eq!(crate::sync::Settings::load(&app.sessions_dir.with_file_name(crate::sync::SETTINGS_FILE)).expect("settings").code, code);
        assert_eq!(app.picker.as_ref().expect("picker").items[0].label, code);
        let saved = serde_json::to_string(&app.chat().expect("chat").state).expect("json");
        assert!(!saved.contains(&code), "the code must never reach a chat that syncs");
    }

    #[test]
    fn connecting_twice_is_refused_while_a_round_may_be_running() {
        let (mut app, _rx, _dir) = app(true);
        let store = tempfile::tempdir().expect("store");
        let _worker = linked_to_sync(&mut app, store.path());
        run(&mut app, "/sync connect D:\\Elsewhere");
        assert!(matches!(app.chat().expect("chat").state.blocks.last(), Some(Block::Error(text)) if text.contains("/sync off")));
    }

    #[test]
    fn a_chat_with_no_memory_here_sends_its_recent_history_once() {
        let mut chat = Chat::new("x".into(), PathBuf::from("."), Choice::Auto);
        chat.state.carry_context = true;
        chat.push(Block::User("the plan is pineapple".into()));

        assert_eq!(chat.with_carried_context("/skill go".into()), "/skill go", "a skill's name must stay first");
        let sent = chat.with_carried_context("carry on".into());
        assert!(sent.contains("pineapple") && !chat.state.carry_context, "{sent}");
        assert_eq!(chat.with_carried_context("next".into()), "next");
    }
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

    #[test]
    fn schedules_parse_their_timing_and_need_a_message() {
        use crate::schedule::When;
        assert_eq!(parse_schedule("every 1h check the deploy"), Some((When::Every { seconds: 3600 }, "check the deploy".to_string())));
        assert_eq!(parse_schedule("daily 08:30 summarize my day"), Some((When::Daily { hour: 8, minute: 30 }, "summarize my day".to_string())));
        for bad in ["every 1h", "daily 25:00 x", "weekly 08:00 x", "every soon x", ""] {
            assert_eq!(parse_schedule(bad), None, "{bad}");
        }
    }

    #[tokio::test]
    async fn a_schedule_is_saved_runs_when_due_and_can_be_stopped() {
        let (mut app, _rx, _dir) = app(true);
        enter(&mut app, "/schedule every 1h check the deploy");
        assert_eq!(app.schedules.len(), 1);
        let path = app.sessions_dir.with_file_name(SCHEDULES_FILE);
        assert_eq!(crate::schedule::load(&path), app.schedules, "it survives a restart");
        run_due_schedules(&mut app);
        assert!(!app.chat().expect("chat").state.blocks.iter().any(|b| matches!(b, Block::User(_))), "it isn't due yet");
        app.schedules[0].last_run = 0;
        run_due_schedules(&mut app);
        let chat = app.chat().expect("chat");
        assert!(chat.state.blocks.iter().any(|b| matches!(b, Block::User(text) if text == "check the deploy")));
        assert!(chat.reply_to_phone);
        assert!(app.schedules[0].last_run > 0);
        enter(&mut app, "/schedule stop 1");
        assert!(app.schedules.is_empty() && crate::schedule::load(&path).is_empty());
    }

    #[tokio::test]
    async fn a_schedule_whose_thread_closed_gets_a_new_one_without_taking_the_focus() {
        let (mut app, _rx, _dir) = app(true);
        enter(&mut app, "/schedule every 1h check the deploy");
        app.schedules[0].thread = "closed long ago".to_string();
        app.schedules[0].last_run = 0;
        let (windows, focused) = (app.windows.len(), app.active_id());
        run_due_schedules(&mut app);
        assert_eq!((app.windows.len(), app.active_id()), (windows + 1, focused));
        assert_ne!(app.schedules[0].thread, "closed long ago");
    }

    #[tokio::test]
    async fn schedules_refuse_olympus_commands_and_bad_input_explains_itself() {
        let (mut app, _rx, _dir) = app(true);
        for line in ["/schedule every 1h /review", "/schedule weekly x y", "/schedule stop 99", "/search", "/keep-skill nope", "/drop-skill nope"] {
            enter(&mut app, line);
            assert!(matches!(last_block(&mut app), Block::Error(_)), "{line}");
        }
        assert!(app.schedules.is_empty());
    }

    fn linked_phone(app: &mut App, code: Option<&str>) -> tokio::sync::mpsc::UnboundedReceiver<crate::telegram::Outbound> {
        let path = app.sessions_dir.with_file_name(TELEGRAM_FILE);
        let (phone, sent) = crate::telegram::Phone::detached(path, code.map(str::to_string));
        app.phone = Some(phone);
        sent
    }

    fn texts(sent: &mut tokio::sync::mpsc::UnboundedReceiver<crate::telegram::Outbound>) -> Vec<String> {
        std::iter::from_fn(|| sent.try_recv().ok()).filter_map(|message| if let crate::telegram::Outbound::Text(_, text) = message { Some(text) } else { None }).collect()
    }

    #[tokio::test]
    async fn only_the_code_links_a_phone_and_the_link_is_remembered() {
        let (mut app, _rx, _dir) = app(true);
        let mut sent = linked_phone(&mut app, Some("ABCD2345"));
        let messages = app.chat().expect("chat").state.blocks.len();
        phone_message(&mut app, 7, "hello");
        assert!(texts(&mut sent).is_empty(), "a wrong code gets silence");
        assert_eq!(app.chat().expect("chat").state.blocks.len(), messages, "and never reaches a model");
        phone_message(&mut app, 7, "abcd2345");
        assert!(texts(&mut sent)[0].starts_with("Linked."));
        let saved = crate::telegram::load(&app.sessions_dir.with_file_name(TELEGRAM_FILE)).expect("saved");
        assert_eq!(saved.chat_id, Some(7));
    }

    #[tokio::test]
    async fn pairing_closes_for_good_after_too_many_wrong_codes() {
        let (mut app, _rx, _dir) = app(true);
        let mut sent = linked_phone(&mut app, Some("ABCD2345"));
        for _ in 0..crate::telegram::MAX_PAIRING_TRIES {
            phone_message(&mut app, 99, "guess");
        }
        phone_message(&mut app, 99, "ABCD2345");
        assert!(texts(&mut sent).is_empty());
        assert_eq!(app.phone.as_ref().expect("phone").settings.chat_id, None);
    }

    #[tokio::test]
    async fn a_phone_message_runs_in_the_thread_and_the_result_comes_back() {
        let (mut app, _rx, _dir) = app(true);
        let mut sent = linked_phone(&mut app, Some("CODE"));
        phone_message(&mut app, 7, "CODE");
        texts(&mut sent);
        let id = app.active_id();
        phone_message(&mut app, 7, "fix the login");
        assert!(texts(&mut sent)[0].starts_with("On it, in:"));
        let target = Target::parse("claude:haiku").expect("target");
        app.handle(AppEvent::TurnDone { window: id, target, result: olympus_core::event::TurnResult { text: "Fixed it.".into(), ..olympus_core::event::TurnResult::failed(olympus_core::event::Stop::Completed, "") } });
        let replies = texts(&mut sent);
        assert!(replies.iter().any(|reply| reply.contains("Fixed it.")), "{replies:?}");
        assert!(!app.chat().expect("chat").reply_to_phone);
    }

    #[tokio::test]
    async fn the_phone_can_list_and_switch_threads_without_moving_your_focus() {
        let (mut app, _rx, dir) = app(true);
        app.new_chat(dir.path().to_path_buf());
        let focused = app.active_id();
        let mut sent = linked_phone(&mut app, Some("CODE"));
        phone_message(&mut app, 7, "CODE");
        texts(&mut sent);
        phone_message(&mut app, 7, "/threads");
        assert!(texts(&mut sent)[0].contains("2. "));
        phone_message(&mut app, 7, "/use 1");
        assert!(texts(&mut sent)[0].starts_with("Now in:"));
        phone_message(&mut app, 7, "/use 99");
        assert!(texts(&mut sent)[0].contains("from /threads"));
        phone_message(&mut app, 7, "/new");
        assert_eq!(app.active_id(), focused);
        phone_message(&mut app, 7, "/banana");
        assert!(texts(&mut sent).last().expect("reply").contains("/help"));
    }

    #[tokio::test]
    async fn a_long_task_offers_to_become_a_skill_and_a_short_one_does_not() {
        let (mut app, _rx, _dir) = app(true);
        let id = app.active_id();
        let target = Target::parse("claude:haiku").expect("target");
        let done = || olympus_core::event::TurnResult { text: "done".into(), ..olympus_core::event::TurnResult::failed(olympus_core::event::Stop::Completed, "") };
        app.chat().expect("chat").push(Block::User("do it".into()));
        app.handle(AppEvent::TurnDone { window: id, target: target.clone(), result: done() });
        assert!(!app.chat().expect("chat").state.blocks.iter().any(|b| matches!(b, Block::Note(text) if text.contains("/save-skill"))));
        for step in 0..STEPS_WORTH_SAVING {
            app.chat().expect("chat").push(Block::Tool { id: step.to_string(), name: "Bash".into(), detail: String::new(), status: crate::session::ToolStatus::Ok, summary: String::new() });
        }
        app.handle(AppEvent::TurnDone { window: id, target, result: done() });
        assert!(app.chat().expect("chat").state.blocks.iter().any(|b| matches!(b, Block::Note(text) if text.contains("/save-skill"))));
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
