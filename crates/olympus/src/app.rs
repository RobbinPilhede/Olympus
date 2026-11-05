use crate::commands::{self, COMMANDS};
use crate::composer::Composer;
use crate::diff::{self, DiffSource, DiffState, FileDiff};
use crate::input::{KeyCode, KeyEvent};
use crate::pty::{key_bytes, PtyPane};
use crate::session::{self, Activity, Block, Chat, Choice, Pane, TaskRow, TermPane, Window};
use anyhow::Result;
use olympus_core::agent::{record_outcome, run_turn, TurnRequest};
use olympus_core::config::{home_dir, user_home};
use olympus_core::event::{AgentEvent, Stop, TurnResult};
use olympus_core::proc::cancel_pair;
use olympus_core::runtime::{Runtime, Tier};
use olympus_core::target::{Provider, Target};
use olympus_core::workflow::{AutoOutcome, WorkflowEvent};
use olympus_learn::{classify_task, is_correction, Store};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;

/// This file's existence turns notifications off across restarts.
pub const NOTIFICATIONS_OFF: &str = "notifications-off";
/// Marker file: the Shift+V and Shift+C chords are off.
pub const HOTKEYS_OFF: &str = "hotkeys-off";

/// What choosing a picker row does.
#[derive(Debug, Clone)]
pub enum Action {
    Switch(u64),
    Run(String),
    Prefill(String),
    Resume { vendor: String, id: String, cwd: PathBuf, title: String },
    SelectNode { name: String, address: String },
    HfRepo(String),
    HfQuant { repo: String, quant: String },
    PfRepo { repo: String, magnet: String },
    PfQuant { repo: String, quant: String, magnet: String },
    LoadModel(String),
    UseAccount(Provider, Option<PathBuf>),
    NewChatOn(Provider, Option<PathBuf>),
    NewChatIn { provider: Provider, account: Option<PathBuf>, cwd: PathBuf },
    BrowseFolder(Provider, Option<PathBuf>),
    AddAccount(Provider),
}

/// One picker row.
#[derive(Debug, Clone)]
pub struct PickItem {
    pub label: String,
    pub detail: String,
    pub action: Action,
}

/// A filterable overlay list.
#[derive(Debug, Clone)]
pub struct Picker {
    pub title: String,
    pub query: String,
    pub items: Vec<PickItem>,
    pub selected: usize,
    pub loading: bool,
    pub empty: String,
}

impl Picker {
    /// A ready picker with an empty-state message.
    pub fn new(title: &str, items: Vec<PickItem>, empty: &str) -> Self {
        Self { title: title.into(), query: String::new(), items, selected: 0, loading: false, empty: empty.into() }
    }

    /// A picker waiting on background work.
    pub fn loading(title: &str) -> Self {
        Self { loading: true, ..Self::new(title, Vec::new(), "") }
    }

    /// Indices of rows matching the query.
    pub fn visible(&self) -> Vec<usize> {
        let words: Vec<String> = self.query.split_whitespace().map(str::to_lowercase).collect();
        self.items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                let haystack = format!("{} {}", item.label, item.detail).to_lowercase();
                words.iter().all(|w| haystack.contains(w))
            })
            .map(|(index, _)| index)
            .collect()
    }
}

/// Something finished that you should hear about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub window: u64,
    pub title: String,
    pub body: String,
    pub ok: bool,
}

/// Everything background work reports to the interface.
pub enum AppEvent {
    Agent { window: u64, event: AgentEvent },
    TurnDone { window: u64, target: Target, result: TurnResult },
    Flow { window: u64, event: WorkflowEvent },
    AutoDone { window: u64, outcome: Result<AutoOutcome, String> },
    Note { window: u64, text: String },
    Fail { window: u64, text: String },
    Answer { window: u64, label: String, text: String },
    Done { window: u64 },
    Picker(Picker),
    Status(String),
    UseTarget { window: u64, target: Target },
    LoopTick { window: u64, generation: u64 },
    Attach(PathBuf),
    Merged { window: u64 },
    AccountLabels(HashMap<PathBuf, String>),
    AccountAdded,
    Notify { window: u64, ok: bool, body: String },
    DiffLoaded { window: u64, source: DiffSource, outcome: Result<Vec<FileDiff>, String> },
    /// Time to see whether a schedule is due.
    ScheduleTick,
    /// A message from your linked Telegram chat, or a pairing attempt.
    Phone { chat: i64, text: String },
    /// A bot token checked out, so the connection can start.
    PhoneChecked { token: String, bot: Option<String> },
    Wake,
}

/// All interface state.
pub struct App {
    pub rt: Arc<Runtime>,
    pub windows: Vec<Window>,
    pub active: usize,
    pub grid: bool,
    pub composer: Composer,
    pub picker: Option<Picker>,
    pub hint_selected: usize,
    pub tx: UnboundedSender<AppEvent>,
    pub status: String,
    /// Lines of scrolling the interface hasn't applied yet; positive moves back in time.
    pub pending_scroll: isize,
    /// Finished work the interface hasn't announced yet.
    pub notices: Vec<Notice>,
    pub notifications_on: bool,
    /// A brief message the interface should show.
    pub pending_flash: Option<String>,
    /// Text the interface should put on the clipboard.
    pub pending_copy: Option<String>,
    /// Set when the interface should open or close the latest tool's details.
    pub pending_details: bool,
    /// The account waiting on the native folder dialog for its new chat.
    pub pending_folder: Option<(Provider, Option<PathBuf>)>,
    pub diff_open: bool,
    /// Claude account folders besides the default, for the usage panel.
    pub account_folders: Vec<PathBuf>,
    /// Files waiting to go with the next message.
    pub attachments: Vec<PathBuf>,
    pub skill_hints: Vec<commands::Hint>,
    /// Who each signed-in Claude folder belongs to, as the CLI reports it.
    pub account_labels: HashMap<PathBuf, String>,
    pub diffs: HashMap<u64, DiffState>,
    pub reduce_motion: bool,
    pub provider_lines: Vec<(String, bool)>,
    pub node: Option<(String, String)>,
    pub sessions_dir: PathBuf,
    pub schedules: Vec<crate::schedule::Schedule>,
    pub phone: Option<crate::telegram::Phone>,
    /// Lets the phone connection transcribe voice memos; set once the interface starts it.
    pub voice: Option<crate::voice::Voice>,
    pub quit: bool,
    pub quit_armed: bool,
    next_id: u64,
}

impl App {
    /// Builds the interface state and restores saved chats.
    pub fn new(rt: Arc<Runtime>, tx: UnboundedSender<AppEvent>, sessions_dir: PathBuf, cwd: PathBuf) -> Self {
        let provider_lines = vec![
            (rt.claude.as_ref().map_or("Claude Code not found".to_string(), |b| format!("Claude Code {}", b.version)), rt.claude.is_some()),
            (rt.codex.as_ref().map_or("Codex not found".to_string(), |b| format!("Codex {}", b.version)), rt.codex.is_some()),
            (if rt.config.openrouter_key().is_some() { "OpenRouter key set".to_string() } else { "OpenRouter: add a key to use it".to_string() }, rt.config.openrouter_key().is_some()),
        ];
        let mut app = Self {
            rt,
            windows: Vec::new(),
            active: 0,
            grid: false,
            composer: Composer::default(),
            picker: None,
            hint_selected: 0,
            tx,
            status: String::new(),
            pending_scroll: 0,
            notices: Vec::new(),
            notifications_on: !home_dir().join(NOTIFICATIONS_OFF).exists(),
            pending_flash: None,
            pending_copy: None,
            pending_details: false,
            pending_folder: None,
            diff_open: false,
            account_folders: commands::extra_account_folders(),
            attachments: Vec::new(),
            account_labels: HashMap::new(),
            skill_hints: commands::skill_hints(&std::env::current_dir().unwrap_or_default()),
            diffs: HashMap::new(),
            reduce_motion: std::env::var_os("OLYMPUS_REDUCE_MOTION").is_some(),
            provider_lines,
            node: None,
            schedules: crate::schedule::load(&sessions_dir.with_file_name(commands::SCHEDULES_FILE)),
            phone: None,
            voice: None,
            sessions_dir,
            quit: false,
            quit_armed: false,
            next_id: 1,
        };
        let recent = session::load_all(&app.sessions_dir).into_iter().rev().take(20).rev().collect();
        for chat in session::in_saved_order(&app.sessions_dir, recent) {
            app.add_window(Pane::Chat(Box::new(chat)));
        }
        let same_dir = app.windows.iter().rposition(|w| matches!(&w.pane, Pane::Chat(chat) if chat.state.cwd == cwd));
        match same_dir {
            Some(index) => app.active = index,
            None => {
                app.new_chat(cwd);
            }
        }
        app
    }

    fn default_choice(&self) -> Choice {
        match self.rt.config.default_target.as_str() {
            "auto" => Choice::Auto,
            spec => Target::parse(spec).map(Choice::Fixed).unwrap_or(Choice::Auto),
        }
    }

    /// Opens a new window and focuses it.
    pub fn add_window(&mut self, pane: Pane) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.windows.push(Window { pane, activity: Activity::Idle, id });
        self.active = self.windows.len() - 1;
        id
    }

    /// Opens a new chat in `cwd`.
    pub fn new_chat(&mut self, cwd: PathBuf) -> u64 {
        let mut chat = Chat::new(session::NEW_CHAT_TITLE.to_string(), cwd, self.default_choice());
        // A new thread carries on with the account you're already working on.
        if let Some(Pane::Chat(current)) = self.windows.get(self.active).map(|w| &w.pane) {
            chat.state.account_dir.clone_from(&current.state.account_dir);
            chat.state.codex_account_dir.clone_from(&current.state.codex_account_dir);
        }
        self.add_window(Pane::Chat(Box::new(chat)))
    }

    /// Opens a terminal pane running a program.
    pub fn new_term(&mut self, title: &str, program: &str, args: &[String], cwd: PathBuf) -> Result<u64> {
        self.new_term_with_env(title, program, args, cwd, None)
    }

    /// Opens a terminal pane with one extra environment variable.
    pub fn new_term_with_env(&mut self, title: &str, program: &str, args: &[String], cwd: PathBuf, env: Option<(&str, &std::path::Path)>) -> Result<u64> {
        let tx = self.tx.clone();
        let pty = PtyPane::spawn(program, args, &cwd, env, 24, 80, move || {
            let _ = tx.send(AppEvent::Wake);
        })?;
        Ok(self.add_window(Pane::Term(Box::new(TermPane { title: title.to_string(), pty }))))
    }

    /// True when the focused window takes composer input.
    pub fn active_is_chat(&self) -> bool {
        self.windows.get(self.active).is_some_and(|w| matches!(w.pane, Pane::Chat(_)))
    }

    /// The focused chat, if any.
    pub fn chat(&mut self) -> Option<&mut Chat> {
        self.windows.get_mut(self.active).and_then(Window::chat)
    }

    /// The window with this id.
    pub fn window_mut(&mut self, id: u64) -> Option<&mut Window> {
        self.windows.iter_mut().find(|w| w.id == id)
    }

    /// The focused window's id.
    pub fn active_id(&self) -> u64 {
        self.windows.get(self.active).map_or(0, |w| w.id)
    }

    /// Slash commands matching the current input, for the hint list.
    pub fn command_matches(&self) -> Vec<commands::Hint> {
        let text = &self.composer.text;
        if !text.starts_with('/') || text.contains(char::is_whitespace) {
            return Vec::new();
        }
        let own = COMMANDS.iter().filter(|c| c.name.starts_with(text.as_str())).map(commands::Hint::from);
        own.chain(self.skill_hints.iter().filter(|h| h.name.starts_with(text.as_str())).cloned()).collect()
    }

    /// Focuses a window and marks it seen.
    pub fn focus(&mut self, index: usize) {
        if index < self.windows.len() {
            self.active = index;
            if self.windows[index].activity == Activity::Unseen {
                self.windows[index].activity = Activity::Idle;
            }
            self.refresh_diff();
        }
    }

    /// Moves a thread to another place in the list; the same thread stays focused.
    pub fn move_window(&mut self, from: usize, to: usize) {
        if from == to || from >= self.windows.len() || to >= self.windows.len() {
            return;
        }
        let focused = self.active_id();
        let window = self.windows.remove(from);
        self.windows.insert(to, window);
        self.active = self.windows.iter().position(|w| w.id == focused).unwrap_or(0);
        self.save_order();
    }

    fn save_order(&self) {
        let ids: Vec<&str> = self.windows.iter().filter_map(|w| if let Pane::Chat(chat) = &w.pane { Some(chat.file_id.as_str()) } else { None }).collect();
        // Losing the order isn't worth interrupting anything; the next move or quit saves it again.
        let _ = session::save_order(&self.sessions_dir, &ids);
    }

    fn cycle(&mut self, step: isize) {
        let count = self.windows.len() as isize;
        if count > 0 {
            self.focus((self.active as isize + step).rem_euclid(count) as usize);
        }
    }

    /// Closes the focused window.
    pub fn close_active(&mut self) {
        self.close_window(self.active);
    }

    /// Closes a window; running work is stopped, chats can return with `/reopen`, and the last window is replaced by a blank chat.
    pub fn close_window(&mut self, index: usize) {
        if index >= self.windows.len() {
            return;
        }
        let mut window = self.windows.remove(index);
        self.diffs.remove(&window.id);
        if let Pane::Chat(chat) = &mut window.pane {
            if let Some(cancel) = chat.cancel.take() {
                let _ = cancel.send(true);
            }
            if chat.state.blocks.is_empty() {
                let _ = std::fs::remove_file(self.sessions_dir.join(format!("{}.json", chat.file_id)));
            } else {
                match session::archive(chat, &self.sessions_dir) {
                    Ok(()) => self.pending_flash = Some("Chat closed. /reopen brings it back.".into()),
                    Err(error) => self.status = format!("Unable to keep the closed chat: {error}"),
                }
            }
        }
        if index < self.active {
            self.active -= 1;
        }
        if self.windows.is_empty() {
            let cwd = std::env::current_dir().unwrap_or_else(|_| user_home());
            self.new_chat(cwd);
        }
        self.active = self.active.min(self.windows.len() - 1);
    }

    /// Brings back the most recently closed chat.
    pub fn reopen_chat(&mut self) {
        match session::reopen_latest(&self.sessions_dir) {
            Some(mut chat) => {
                chat.follow = true;
                self.add_window(Pane::Chat(Box::new(chat)));
            }
            None => self.fail_active("There are no closed chats to bring back."),
        }
    }

    fn save_window(&self, id: u64) {
        if let Some(Window { pane: Pane::Chat(chat), .. }) = self.windows.iter().find(|w| w.id == id) {
            // Losing one snapshot isn't worth interrupting the session; the next turn saves again.
            let _ = session::save(chat, &self.sessions_dir);
        }
    }

    /// Sends the composer's content.
    pub fn submit(&mut self) {
        let text = self.composer.take();
        let trimmed = text.trim();
        let plain_message = !trimmed.starts_with('/') && !trimmed.starts_with('@');
        if trimmed.is_empty() && self.attachments.is_empty() {
            return;
        }
        self.quit_armed = false;
        if plain_message && !self.attachments.is_empty() {
            let id = self.active_id();
            let message = commands::with_attachments(trimmed, &std::mem::take(&mut self.attachments));
            self.send(id, message);
            return;
        }
        if trimmed.starts_with('/') {
            commands::run(self, trimmed);
        } else if let Some(rest) = trimmed.strip_prefix('@') {
            match rest.split_once(char::is_whitespace) {
                Some((model, question)) if !question.trim().is_empty() => commands::consult(self, model, question.trim()),
                _ => self.fail_active("Add a question after the model, like @codex is this safe?"),
            }
        } else {
            let id = self.active_id();
            self.send(id, text);
        }
    }

    /// Adds a file to the next message, once.
    pub fn attach(&mut self, path: PathBuf) {
        if !self.attachments.contains(&path) {
            self.attachments.push(path);
        }
    }

    /// Shows a note in the focused chat.
    pub fn note_active(&mut self, text: impl Into<String>) {
        let id = self.active_id();
        self.note(id, text.into());
    }

    fn note(&mut self, id: u64, text: String) {
        match self.window_mut(id).and_then(Window::chat) {
            Some(chat) => {
                chat.push(Block::Note(text));
                chat.follow = true;
            }
            None => self.status = text,
        }
    }

    /// Shows an error in the focused chat.
    pub fn fail_active(&mut self, text: impl Into<String>) {
        let id = self.active_id();
        self.fail(id, text.into());
    }

    fn fail(&mut self, id: u64, text: String) {
        match self.window_mut(id).and_then(Window::chat) {
            Some(chat) => {
                chat.push(Block::Error(text));
                chat.follow = true;
            }
            None => self.status = text,
        }
    }

    /// Sends a message to a chat, queueing it if a turn is running.
    pub fn send(&mut self, id: u64, text: String) {
        let Some(window) = self.window_mut(id) else { return };
        let running = window.activity == Activity::Running;
        let Some(chat) = window.chat() else { return };
        if running {
            chat.queue.push_back(text);
            let count = chat.queue.len();
            chat.push(Block::Note(format!("Queued. {count} waiting.")));
            return;
        }
        let settled = chat.pending_run.take();
        let paced = chat.looping.as_mut().filter(|l| l.prompt == text).is_some_and(|l| {
            l.started = Some(Instant::now());
            l.every.is_none()
        });
        chat.push(Block::User(text.clone()));
        chat.follow = true;
        chat.failed_over = false;
        let first_message = chat.state.blocks.iter().filter(|b| matches!(b, Block::User(_))).count() == 1;
        if first_message && chat.has_default_title() {
            chat.state.title = session::short_title(text.lines().next().unwrap_or("Chat"));
        }
        if let Some(run) = settled {
            let (rt, corrected) = (self.rt.clone(), is_correction(&text));
            tokio::spawn(async move {
                record_outcome(&rt, &run.run_id, &run.request, &run.usage, Some(!corrected), corrected).await;
            });
        }
        self.start_turn(id, if paced { format!("{text}\n\n{}", commands::LOOP_PACING) } else { text });
    }

    /// Starts a turn on a chat's model.
    pub fn start_turn(&mut self, id: u64, prompt: String) {
        let (tx, rt) = (self.tx.clone(), self.rt.clone());
        let Some(window) = self.window_mut(id) else { return };
        window.activity = Activity::Running;
        let Some(chat) = window.chat() else { return };
        let (cancel_tx, cancel) = cancel_pair();
        chat.cancel = Some(cancel_tx);
        chat.last_prompt = prompt.clone();
        let choice = chat.state.choice.clone();
        let last_target = chat.state.last_target.clone();
        let session = chat.state.vendor_session.clone();
        let cwd = chat.state.cwd.clone();
        let profile = chat.profile();
        let (account, codex_account) = (chat.state.account_dir.clone(), chat.state.codex_account_dir.clone());
        tokio::spawn(async move {
            let previous = last_target.clone();
            let target = match choice {
                Choice::Fixed(target) => Ok(target),
                Choice::Auto => match last_target.filter(|t| rt.limits.available(t.provider.vendor(), rt.config.quota_ceiling)) {
                    Some(target) => Ok(target),
                    None => rt.pick(Tier::for_task(classify_task(&prompt)), &prompt, &cwd).await,
                },
            };
            let target = match target {
                Ok(target) => target,
                Err(message) => {
                    let _ = tx.send(AppEvent::Fail { window: id, text: message });
                    let _ = tx.send(AppEvent::Done { window: id });
                    return;
                }
            };
            let (prompt, needs_setup) = match commands::slash_prompt(&target, prompt, &cwd) {
                Ok(prepared) => prepared,
                Err(message) => {
                    let _ = tx.send(AppEvent::Fail { window: id, text: message });
                    let _ = tx.send(AppEvent::Done { window: id });
                    return;
                }
            };
            let mut req = TurnRequest::new(target.clone(), prompt, cwd);
            req.resume = session.filter(|_| previous.as_ref() == Some(&target));
            req.profile = if needs_setup { olympus_core::agent::Profile::Full } else { profile };
            req.deferred_outcome = true;
            req.account_dir = commands::account_for(&target, account, codex_account);
            let events = tx.clone();
            let result = run_turn(&rt, &req, cancel, &mut move |event| {
                let _ = events.send(AppEvent::Agent { window: id, event });
            })
            .await;
            let _ = tx.send(AppEvent::TurnDone { window: id, target, result });
        });
    }

    /// Stops whatever the focused chat is running.
    pub fn stop_active(&mut self) -> bool {
        let Some(window) = self.windows.get_mut(self.active) else { return false };
        let running = window.activity == Activity::Running;
        let Some(chat) = window.chat() else { return false };
        if !running {
            let waiting = chat.looping.take().is_some();
            if waiting {
                chat.push(Block::Note("Loop stopped.".into()));
                chat.follow = true;
            }
            return waiting;
        }
        chat.queue.clear();
        match &chat.cancel {
            Some(cancel) => cancel.send(true).is_ok(),
            None => false,
        }
    }

    fn finish_turn(&mut self, id: u64, target: Target, result: TurnResult) {
        let focused = self.active_id() == id;
        let rt = self.rt.clone();
        let Some(window) = self.window_mut(id) else { return };
        let Some(chat) = window.chat() else { return };
        chat.cancel = None;
        chat.state.usage.add(&result.usage);
        let changed_model = chat.state.last_target.as_ref() != Some(&target);
        chat.state.last_target = Some(target.clone());
        if changed_model && result.session.is_none() {
            chat.state.vendor_session = None;
        }
        if let Some(session) = &result.session {
            chat.state.vendor_session = Some(session.clone());
        }
        let mut failover = None;
        match result.stop {
            Stop::Completed => {
                chat.finish_text(&result.text);
                let request = TurnRequest::new(target.clone(), chat.last_prompt.clone(), chat.state.cwd.clone());
                chat.pending_run = Some(session::PendingRun { run_id: result.run_id.clone(), request, usage: result.usage });
            }
            Stop::Cancelled => chat.push(Block::Note("Stopped.".into())),
            Stop::RateLimited => {
                let alternative = rt.other_vendor(&target.provider);
                match (alternative, chat.failed_over) {
                    (Some(next), false) => {
                        chat.failed_over = true;
                        chat.push(Block::Note(format!("{target} hit its limit. Continuing on {next}.")));
                        chat.state.last_target = None;
                        chat.state.vendor_session = None;
                        let handoff = chat.handoff(6000);
                        failover = Some((next, format!("You are taking over from another model that hit its usage limit. The project state on disk is current.\n\nRecent conversation:\n{handoff}\n\nContinue with the user's latest request.")));
                    }
                    _ => chat.push(Block::Error(result.error.clone().unwrap_or_else(|| "The model hit its usage limit.".into()))),
                }
            }
            Stop::AuthError => chat.push(Block::Error(format!("{} Open a terminal window with /claude or /codex and sign in, then try again.", result.error.clone().unwrap_or_else(|| "Sign-in failed.".into())))),
            Stop::Failed | Stop::IdleTimeout => chat.push(Block::Error(result.error.clone().unwrap_or_else(|| "The turn failed.".into()))),
        }
        let queued = if failover.is_some() { None } else { chat.queue.pop_front() };
        let ok = result.stop == Stop::Completed || result.stop == Stop::Cancelled;
        window.activity = match (ok, focused) {
            (false, _) => Activity::Failed,
            (true, true) => Activity::Idle,
            (true, false) => Activity::Unseen,
        };
        self.save_window(id);
        let keeps_looping = failover.is_none() && queued.is_none() && self.schedule_loop(id, &result);
        // You stopped it yourself, or more work follows, so neither is the moment to announce.
        if failover.is_none() && queued.is_none() && result.stop != Stop::Cancelled && !keeps_looping {
            let body = match result.stop {
                Stop::Completed => format!("{target} finished"),
                _ => format!("Failed: {}", result.error.as_deref().and_then(|e| e.lines().next()).unwrap_or("the turn stopped")),
            };
            self.notify(id, result.stop == Stop::Completed, body);
            self.report_to_phone(id, &result);
            self.offer_skill(id, &result);
        }
        if let Some((next, prompt)) = failover {
            if let Some(chat) = self.window_mut(id).and_then(Window::chat) {
                if matches!(chat.state.choice, Choice::Fixed(_)) {
                    chat.state.choice = Choice::Fixed(next);
                } else {
                    chat.state.last_target = Some(next);
                }
            }
            self.start_turn(id, prompt);
        } else if let Some(text) = queued {
            self.send(id, text);
        }
    }

    fn report_to_phone(&mut self, id: u64, result: &TurnResult) {
        let Some(chat) = self.window_mut(id).and_then(Window::chat) else { return };
        if !std::mem::take(&mut chat.reply_to_phone) {
            return;
        }
        let title = chat.state.title.clone();
        let body = match result.stop {
            Stop::Completed if result.text.trim().is_empty() => "Finished.".to_string(),
            Stop::Completed => result.text.clone(),
            Stop::Cancelled => "Stopped.".to_string(),
            _ => format!("Failed: {}", result.error.as_deref().unwrap_or("the turn stopped")),
        };
        if let Some(phone) = &self.phone {
            phone.say(format!("{title}\n\n{body}"));
        }
    }

    fn offer_skill(&mut self, id: u64, result: &TurnResult) {
        let Some(chat) = self.window_mut(id).and_then(Window::chat) else { return };
        let steps = chat.steps_in_last_turn();
        if result.stop == Stop::Completed && steps >= commands::STEPS_WORTH_SAVING {
            chat.push(Block::Note(format!("That took {steps} steps. Run /save-skill to keep how it was done, and you can review it before it's used.")));
        }
    }

    // Returns whether the loop goes on; a failed or stopped run ends it instead of repeating the failure.
    fn schedule_loop(&mut self, id: u64, result: &TurnResult) -> bool {
        let tx = self.tx.clone();
        let Some(chat) = self.window_mut(id).and_then(Window::chat) else { return false };
        let Some(looping) = chat.looping.as_mut() else { return false };
        if result.stop != Stop::Completed {
            let note = if result.stop == Stop::Cancelled { "Loop stopped." } else { "Loop stopped because this run failed. Start it again with /loop." };
            chat.looping = None;
            chat.push(Block::Note(note.into()));
            return false;
        }
        let delay = match looping.every {
            Some(every) => Some(every.saturating_sub(looping.started.map_or(Duration::ZERO, |started| started.elapsed())).max(Duration::from_secs(1))),
            None => commands::loop_pace(&result.text),
        };
        let Some(delay) = delay else {
            chat.looping = None;
            chat.push(Block::Note("Loop finished: nothing left to do.".into()));
            return false;
        };
        looping.generation += 1;
        let generation = looping.generation;
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = tx.send(AppEvent::LoopTick { window: id, generation });
        });
        true
    }

    fn apply_flow(&mut self, id: u64, event: WorkflowEvent) {
        let Some(chat) = self.window_mut(id).and_then(Window::chat) else { return };
        if let WorkflowEvent::Note(text) = event {
            chat.push(Block::Note(text));
            return;
        }
        let index = chat.state.blocks.iter().rposition(|b| matches!(b, Block::Tasks { .. }));
        let Some(index) = index else { return };
        chat.touch(index);
        let Block::Tasks { phase, rows } = &mut chat.state.blocks[index] else { return };
        match event {
            WorkflowEvent::Phase(text) => *phase = text,
            WorkflowEvent::Plan(plan) => {
                *phase = plan.summary;
                rows.clear();
            }
            WorkflowEvent::Task { id: task, title, state, target, note } => match rows.iter_mut().find(|r| r.id == task) {
                Some(row) => {
                    row.state = state;
                    if target.is_some() {
                        row.target = target;
                    }
                    if !note.is_empty() || matches!(state, olympus_core::workflow::TaskState::Verifying | olympus_core::workflow::TaskState::Merging) {
                        row.note = note;
                    }
                }
                None => rows.push(TaskRow { id: task, title, state, target, note }),
            },
            WorkflowEvent::Agent { task, event } => {
                if let Some(row) = rows.iter_mut().find(|r| r.id == task) {
                    if let AgentEvent::ToolStart { name, detail, .. } = &event {
                        row.note = format!("{name} {}", detail.lines().next().unwrap_or("")).trim().chars().take(70).collect();
                    }
                }
                if let AgentEvent::Limit(info) = event {
                    self.rt.limits.update(info);
                }
            }
            WorkflowEvent::Usage(_) | WorkflowEvent::Note(_) => {}
        }
    }

    /// Applies one background event.
    pub fn handle(&mut self, event: AppEvent) {
        match event {
            AppEvent::Agent { window, event } => {
                let finished_tool = matches!(event, AgentEvent::ToolEnd { .. });
                if let Some(chat) = self.window_mut(window).and_then(Window::chat) {
                    chat.apply(event);
                }
                if finished_tool && window == self.active_id() {
                    self.refresh_diff();
                }
            }
            AppEvent::TurnDone { window, target, result } => {
                self.finish_turn(window, target, result);
                self.refresh_diff();
                if self.account_folders.iter().any(|dir| !self.account_labels.contains_key(dir)) {
                    self.refresh_account_labels();
                }
            }
            AppEvent::Flow { window, event } => self.apply_flow(window, event),
            AppEvent::AutoDone { window, outcome } => {
                self.finish_auto(window, outcome);
                self.refresh_diff();
            }
            AppEvent::Note { window, text } => self.note(window, text),
            AppEvent::Fail { window, text } => self.fail(window, text),
            AppEvent::Answer { window, label, text } => {
                if let Some(chat) = self.window_mut(window).and_then(Window::chat) {
                    chat.push(Block::Assistant { label: Some(label), text });
                }
                self.save_window(window);
            }
            AppEvent::Done { window } => self.finish_background(window),
            AppEvent::Picker(picker) => self.picker = Some(picker),
            AppEvent::Status(text) => self.status = text,
            AppEvent::Attach(path) => self.attach(path),
            AppEvent::LoopTick { window, generation } => {
                // Busy with something you sent: that turn's finish schedules the next run instead.
                let due = self.window_mut(window).and_then(|w| {
                    let idle = w.activity != Activity::Running;
                    w.chat()?.looping.as_ref().filter(|l| idle && l.generation == generation).map(|l| l.prompt.clone())
                });
                if let Some(prompt) = due {
                    self.send(window, prompt);
                }
            }
            AppEvent::UseTarget { window, target } => {
                if let Some(chat) = self.window_mut(window).and_then(Window::chat) {
                    chat.state.choice = Choice::Fixed(target);
                    chat.state.vendor_session = None;
                    chat.state.last_target = None;
                }
            }
            AppEvent::Merged { window } => {
                if let Some(chat) = self.window_mut(window).and_then(Window::chat) {
                    chat.state.last_branch = None;
                }
                self.diffs.entry(window).or_default().source = DiffSource::Working;
                self.refresh_diff();
            }
            AppEvent::DiffLoaded { window, source, outcome } => self.finish_diff(window, source, outcome),
            AppEvent::AccountLabels(labels) => self.account_labels = labels,
            AppEvent::Notify { window, ok, body } => self.notify(window, ok, body),
            AppEvent::AccountAdded => {
                self.refresh_account_labels();
                self.status.clear();
                self.pending_flash = Some("Signed in. Click the account line on any thread to use it there.".into());
            }
            AppEvent::ScheduleTick => commands::run_due_schedules(self),
            AppEvent::Phone { chat, text } => commands::phone_message(self, chat, &text),
            AppEvent::PhoneChecked { token, bot } => commands::finish_phone_connect(self, token, bot),
            AppEvent::Wake => {}
        }
    }

    /// Finds who each account folder belongs to, off the interface thread.
    pub fn refresh_account_labels(&mut self) {
        self.account_folders = commands::extra_account_folders();
        let (rt, tx) = (self.rt.clone(), self.tx.clone());
        tokio::task::spawn_blocking(move || {
            let labels = olympus_core::discovery::accounts(rt.claude.as_ref(), rt.codex.as_ref()).into_iter().map(|a| (a.dir, if a.signed_in { a.label } else { "signed out".to_string() })).collect();
            let _ = tx.send(AppEvent::AccountLabels(labels));
        });
    }

    /// Queues an announcement for a finished job in a window.
    pub fn notify(&mut self, window: u64, ok: bool, body: String) {
        if !self.notifications_on {
            return;
        }
        let title = self.windows.iter().find(|w| w.id == window).map_or_else(|| "Olympus".to_string(), |w| w.title().to_string());
        self.notices.push(Notice { window, title, body, ok });
    }

    /// Opens or closes the review panel.
    pub fn toggle_diff(&mut self) {
        self.diff_open = !self.diff_open;
        self.refresh_diff();
    }

    /// Chooses what the review panel compares for the focused chat.
    pub fn set_diff_source(&mut self, source: DiffSource) {
        let id = self.active_id();
        let state = self.diffs.entry(id).or_default();
        state.source = source;
        state.files.clear();
        state.loaded = false;
        self.refresh_diff();
    }

    /// Reloads the focused chat's changes when the panel is showing them.
    pub fn refresh_diff(&mut self) {
        if !self.diff_open {
            return;
        }
        let id = self.active_id();
        let tx = self.tx.clone();
        let Some(Pane::Chat(chat)) = self.windows.get(self.active).map(|w| &w.pane) else { return };
        let cwd = chat.state.cwd.clone();
        let state = self.diffs.entry(id).or_default();
        if state.loading {
            state.stale = true;
            return;
        }
        state.loading = true;
        state.stale = false;
        let source = state.source.clone();
        tokio::spawn(async move {
            let outcome = diff::load(&cwd, &source).await;
            let _ = tx.send(AppEvent::DiffLoaded { window: id, source, outcome });
        });
    }

    fn finish_diff(&mut self, id: u64, source: DiffSource, outcome: Result<Vec<FileDiff>, String>) {
        let state = self.diffs.entry(id).or_default();
        state.loading = false;
        if state.source == source {
            state.loaded = true;
            match outcome {
                Ok(files) => {
                    state.files = files;
                    state.error = None;
                }
                Err(error) => state.error = Some(error),
            }
        }
        if state.stale && id == self.active_id() {
            self.refresh_diff();
        }
    }

    /// Delivers typed text to whatever has the keyboard.
    pub fn text_input(&mut self, text: &str) {
        if let Some(picker) = &mut self.picker {
            picker.query.push_str(text.lines().next().unwrap_or(""));
            picker.selected = 0;
            return;
        }
        match self.windows.get_mut(self.active).map(|w| &mut w.pane) {
            Some(Pane::Term(term)) => term.pty.send(text.as_bytes()),
            _ => {
                self.composer.insert(text);
                self.hint_selected = 0;
            }
        }
    }

    /// Runs the picker row at a position in the filtered list.
    pub fn choose_picker(&mut self, position: usize) {
        let Some(picker) = &self.picker else { return };
        let chosen = picker.visible().get(position).map(|i| picker.items[*i].action.clone());
        self.picker = None;
        if let Some(action) = chosen {
            commands::pick(self, action);
        }
    }

    fn finish_background(&mut self, id: u64) {
        let focused = self.active_id() == id;
        if let Some(window) = self.window_mut(id) {
            window.activity = if focused { Activity::Idle } else { Activity::Unseen };
            if let Some(chat) = window.chat() {
                chat.cancel = None;
            }
        }
        self.save_window(id);
        self.notify(id, true, "Finished".to_string());
    }

    fn finish_auto(&mut self, id: u64, outcome: Result<AutoOutcome, String>) {
        let focused = self.active_id() == id;
        let Some(window) = self.window_mut(id) else { return };
        let ok = outcome.as_ref().map(|o| o.ok).unwrap_or(false);
        window.activity = match (ok, focused) {
            (false, _) => Activity::Failed,
            (true, true) => Activity::Idle,
            (true, false) => Activity::Unseen,
        };
        let Some(chat) = window.chat() else { return };
        let mut finished_branch = None;
        chat.cancel = None;
        if let Some(Block::Tasks { phase, .. }) = chat.state.blocks.iter_mut().rev().find(|b| matches!(b, Block::Tasks { .. })) {
            *phase = if ok { "Finished".into() } else { "Stopped before finishing".into() };
        }
        if let Some(index) = chat.state.blocks.iter().rposition(|b| matches!(b, Block::Tasks { .. })) {
            chat.touch(index);
        }
        match outcome {
            Ok(result) => {
                chat.state.usage.add(&result.usage);
                chat.state.last_branch = Some(result.branch.clone());
                finished_branch = result.ok.then(|| result.branch.clone());
                let block = if result.ok { Block::Assistant { label: Some("Summary".into()), text: result.summary } } else { Block::Error(result.summary) };
                chat.push(block);
            }
            Err(error) => chat.push(Block::Error(error)),
        }
        if let Some(branch) = finished_branch {
            self.diffs.entry(id).or_default().source = DiffSource::Branch(branch);
        }
        self.save_window(id);
        self.notify(id, ok, if ok { "Build finished. Ready to review and merge.".to_string() } else { "Build stopped before finishing".to_string() });
    }

    fn picker_key(&mut self, key: KeyEvent) {
        let Some(picker) = &mut self.picker else { return };
        match key.code {
            KeyCode::Esc => self.picker = None,
            KeyCode::Up => picker.selected = picker.selected.saturating_sub(1),
            KeyCode::Down => {
                let count = picker.visible().len();
                if picker.selected + 1 < count {
                    picker.selected += 1;
                }
            }
            KeyCode::Backspace => {
                picker.query.pop();
                picker.selected = 0;
            }
            KeyCode::Char(c) if !key.modifiers.ctrl && !key.modifiers.alt => {
                picker.query.push(c);
                picker.selected = 0;
            }
            KeyCode::Enter => {
                let position = picker.selected;
                self.choose_picker(position);
            }
            _ => {}
        }
    }

    fn global_key(&mut self, key: KeyEvent) -> bool {
        if !key.modifiers.alt {
            return false;
        }
        match key.code {
            KeyCode::Char(c @ '1'..='9') => self.focus(c as usize - '1' as usize),
            KeyCode::Left => self.cycle(-1),
            KeyCode::Right => self.cycle(1),
            KeyCode::Char('n') => {
                let cwd = self.current_cwd();
                self.new_chat(cwd);
            }
            KeyCode::Char('t') => commands::run(self, "/term"),
            KeyCode::Char('g') => self.grid = !self.grid,
            KeyCode::Char('d') => self.toggle_diff(),
            KeyCode::Char('k') => self.picker = Some(commands::palette(self)),
            KeyCode::Char('w') => self.close_active(),
            KeyCode::Char('q') => self.quit = true,
            _ => return false,
        }
        true
    }

    /// The directory new windows open in.
    pub fn current_cwd(&self) -> PathBuf {
        self.windows
            .get(self.active)
            .and_then(|w| match &w.pane {
                Pane::Chat(chat) => Some(chat.state.cwd.clone()),
                Pane::Term(_) => None,
            })
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(user_home)
    }

    /// Handles a key press.
    pub fn key(&mut self, key: KeyEvent) {
        if self.picker.is_some() {
            self.picker_key(key);
            return;
        }
        if self.global_key(key) {
            return;
        }
        let Some(window) = self.windows.get_mut(self.active) else { return };
        if let Pane::Term(term) = &mut window.pane {
            term.pty.send(&key_bytes(key));
            return;
        }
        self.composer_key(key);
    }

    fn composer_key(&mut self, key: KeyEvent) {
        let (ctrl, shift) = (key.modifiers.ctrl, key.modifiers.shift);
        let hints = self.command_matches();
        match key.code {
            KeyCode::Enter if shift => self.composer.insert("\n"),
            KeyCode::Enter => {
                if let Some(command) = hints.get(self.hint_selected) {
                    if command.takes_args && self.composer.text != command.name {
                        self.composer.set(&format!("{} ", command.name));
                        self.hint_selected = 0;
                        return;
                    }
                    self.composer.set(&command.name);
                }
                self.hint_selected = 0;
                self.submit();
            }
            KeyCode::Tab => {
                if let Some(command) = hints.get(self.hint_selected) {
                    self.composer.set(&format!("{} ", command.name));
                    self.hint_selected = 0;
                }
            }
            KeyCode::Char('j') if ctrl => self.composer.insert("\n"),
            KeyCode::Char('u') if ctrl => self.composer.set(""),
            KeyCode::Char('w') if ctrl => self.composer.delete_word(),
            KeyCode::Char('a') if ctrl => self.composer.select_all(),
            KeyCode::Char('e') if ctrl => self.composer.end(false),
            KeyCode::Char('c') if ctrl => {
                if self.stop_active() {
                    self.quit_armed = false;
                } else if !self.composer.text.is_empty() {
                    self.composer.set("");
                } else if self.quit_armed {
                    self.quit = true;
                } else {
                    self.quit_armed = true;
                    self.note_active("Press Ctrl+C again to quit.");
                }
            }
            KeyCode::Esc => {
                if !hints.is_empty() {
                    self.composer.set("");
                } else {
                    self.stop_active();
                }
            }
            KeyCode::Char(c) if !ctrl => {
                self.composer.insert(c.encode_utf8(&mut [0; 4]));
                self.hint_selected = 0;
            }
            KeyCode::Backspace => {
                if ctrl {
                    self.composer.delete_word();
                } else {
                    self.composer.backspace();
                }
                self.hint_selected = 0;
            }
            KeyCode::Delete => self.composer.delete(),
            KeyCode::Left if ctrl => self.composer.move_to(self.composer.word_left(), shift),
            KeyCode::Right if ctrl => self.composer.move_to(self.composer.word_right(), shift),
            KeyCode::Left => self.composer.left(shift),
            KeyCode::Right => self.composer.right(shift),
            KeyCode::Home => self.composer.home(shift),
            KeyCode::End => self.composer.end(shift),
            KeyCode::Up if !hints.is_empty() => self.hint_selected = self.hint_selected.saturating_sub(1),
            KeyCode::Down if !hints.is_empty() => self.hint_selected = (self.hint_selected + 1).min(hints.len() - 1),
            KeyCode::Up => {
                if !self.composer.history_up() {
                    self.scroll(3);
                }
            }
            KeyCode::Down => {
                if !self.composer.history_down() {
                    self.scroll(-3);
                }
            }
            KeyCode::PageUp => self.scroll(15),
            KeyCode::PageDown => self.scroll(-15),
            _ => {}
        }
    }

    fn scroll(&mut self, delta: isize) {
        let Some(window) = self.windows.get_mut(self.active) else { return };
        match &mut window.pane {
            Pane::Chat(_) => self.pending_scroll += delta,
            Pane::Term(term) => {
                if let Ok(mut parser) = term.pty.parser.lock() {
                    let current = parser.screen().scrollback();
                    parser.screen_mut().set_scrollback(current.saturating_add_signed(delta));
                }
            }
        }
    }

    /// Handles pasted text.
    pub fn paste(&mut self, text: &str) {
        if let Some(picker) = &mut self.picker {
            picker.query.push_str(text.lines().next().unwrap_or(""));
            picker.selected = 0;
            return;
        }
        let Some(window) = self.windows.get_mut(self.active) else { return };
        if let Pane::Term(term) = &mut window.pane {
            term.pty.paste(text);
        } else {
            self.composer.insert(text);
        }
    }

    /// Saves every chat with content.
    pub fn save_all(&self) {
        self.save_order();
        for window in &self.windows {
            if matches!(&window.pane, Pane::Chat(chat) if !chat.state.blocks.is_empty()) {
                self.save_window(window.id);
            }
        }
    }
}

/// Reads new history into the learning store in the background.
pub fn start_ingest(tx: UnboundedSender<AppEvent>) {
    tokio::task::spawn_blocking(move || {
        let Ok(store) = Store::open(&home_dir().join("learn.db")) else { return };
        let home = user_home();
        let (claude, codex) = (home.join(".claude"), home.join(".codex"));
        let progress = tx.clone();
        let mut last = Instant::now();
        let mut report = move |p: &olympus_learn::IngestProgress| {
            if last.elapsed() > Duration::from_millis(500) {
                last = Instant::now();
                let _ = progress.send(AppEvent::Status(format!("Learning from history {}/{}", p.files_done, p.files_total)));
            }
        };
        let outcome = store.ingest(Some(&claude), Some(&codex), &mut report);
        // A failed extra account shouldn't hide the main import's result; the next launch tries again.
        for extra in commands::extra_account_folders() {
            if extra.file_name().is_some_and(|name| name.to_string_lossy().starts_with(".codex-")) {
                let _ = store.ingest(None, Some(&extra), &mut |_| {});
            } else {
                let _ = store.ingest(Some(&extra), None, &mut |_| {});
            }
        }
        let text = match outcome {
            Ok(stats) if stats.files_updated == 1 => "Learned from 1 session".to_string(),
            Ok(stats) if stats.files_updated > 0 => format!("Learned from {} sessions", stats.files_updated),
            Ok(_) => String::new(),
            Err(error) => format!("History import failed: {error}"),
        };
        let _ = tx.send(AppEvent::Status(text));
    });
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use crate::input::KeyModifiers;
    use olympus_core::config::Config;
    use olympus_core::discovery::Bin;
    use olympus_core::limits::Limits;
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

    pub fn runtime(with_clis: bool) -> Arc<Runtime> {
        let bin = |name: &str| with_clis.then(|| Bin { path: PathBuf::from(name), version: "1.2.3".into() });
        Arc::new(Runtime { config: Config::default(), claude: bin("claude"), codex: bin("codex"), limits: Arc::new(Limits::default()), learn: None, self_exe: PathBuf::from("olympus"), rules: String::new() })
    }

    pub fn app(with_clis: bool) -> (App, UnboundedReceiver<AppEvent>, tempfile::TempDir) {
        let (tx, rx) = unbounded_channel();
        let dir = tempfile::tempdir().expect("tempdir");
        let app = App::new(runtime(with_clis), tx, dir.path().join("sessions"), dir.path().to_path_buf());
        (app, rx, dir)
    }

    pub fn press(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
        app.key(KeyEvent::new(code, modifiers));
    }

    pub fn typed(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, KeyCode::Char(c), KeyModifiers::NONE);
        }
    }

    pub fn result(stop: Stop, text: &str, error: Option<&str>) -> TurnResult {
        TurnResult { stop, session: Some("sess".into()), text: text.into(), usage: Default::default(), structured: None, error: error.map(String::from), run_id: String::new() }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use crate::input::KeyModifiers;
    use olympus_core::event::LimitInfo;

    fn blocks(app: &mut App) -> Vec<Block> {
        app.chat().expect("chat").state.blocks.clone()
    }

    #[tokio::test]
    async fn moving_a_thread_keeps_the_same_one_focused() {
        let (mut app, _rx, _dir) = app(true);
        let cwd = app.current_cwd();
        let first = app.active_id();
        let second = app.new_chat(cwd.clone());
        let third = app.new_chat(cwd);
        app.focus(0);
        app.move_window(0, 2);
        assert_eq!(app.windows.iter().map(|w| w.id).collect::<Vec<_>>(), [second, third, first]);
        assert_eq!(app.active_id(), first);
        app.move_window(0, 1);
        assert_eq!(app.active_id(), first);
        app.move_window(2, 9);
        assert_eq!(app.windows.len(), 3);
    }

    #[tokio::test]
    async fn alt_keys_create_and_switch_windows() {
        let (mut app, _rx, _dir) = app(true);
        assert_eq!(app.windows.len(), 1);
        press(&mut app, KeyCode::Char('n'), KeyModifiers::ALT);
        assert_eq!((app.windows.len(), app.active), (2, 1));
        press(&mut app, KeyCode::Char('1'), KeyModifiers::ALT);
        assert_eq!(app.active, 0);
        press(&mut app, KeyCode::Left, KeyModifiers::ALT);
        assert_eq!(app.active, 1);
        press(&mut app, KeyCode::Char('9'), KeyModifiers::ALT);
        assert_eq!(app.active, 1);
        press(&mut app, KeyCode::Char('g'), KeyModifiers::ALT);
        assert!(app.grid);
    }

    #[tokio::test]
    async fn closing_the_last_window_leaves_a_fresh_chat() {
        let (mut app, _rx, _dir) = app(true);
        press(&mut app, KeyCode::Char('w'), KeyModifiers::ALT);
        assert_eq!(app.windows.len(), 1);
        assert!(app.active_is_chat());
    }

    #[tokio::test]
    async fn messages_during_a_turn_queue_and_send_after_it() {
        let (mut app, _rx, _dir) = app(false);
        let id = app.active_id();
        app.window_mut(id).expect("window").activity = Activity::Running;
        typed(&mut app, "next");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.chat().expect("chat").queue.len(), 1);
        let target = Target::parse("claude:opus").expect("target");
        app.handle(AppEvent::TurnDone { window: id, target, result: result(Stop::Completed, "done", None) });
        assert!(blocks(&mut app).iter().any(|b| matches!(b, Block::User(t) if t == "next")));
        assert!(app.chat().expect("chat").queue.is_empty());
    }

    #[tokio::test]
    async fn limit_hit_fails_over_to_the_other_vendor_with_a_handoff() {
        let (mut app, _rx, _dir) = app(true);
        let id = app.active_id();
        app.chat().expect("chat").push(Block::User("build the thing".into()));
        app.window_mut(id).expect("window").activity = Activity::Running;
        let target = Target::parse("claude:opus").expect("target");
        app.handle(AppEvent::TurnDone { window: id, target, result: result(Stop::RateLimited, "", Some("hit your limit")) });
        let chat = app.chat().expect("chat");
        assert!(chat.failed_over);
        assert!(chat.last_prompt.contains("build the thing"));
        assert!(matches!(&chat.state.choice, Choice::Fixed(t) if t.provider.vendor() == "codex"));
        assert!(chat.state.vendor_session.is_none(), "a new vendor can't resume the old session");
    }

    #[tokio::test]
    async fn a_second_limit_hit_reports_instead_of_looping() {
        let (mut app, _rx, _dir) = app(true);
        let id = app.active_id();
        app.chat().expect("chat").failed_over = true;
        let target = Target::parse("codex:gpt-6.1-sol").expect("target");
        app.handle(AppEvent::TurnDone { window: id, target, result: result(Stop::RateLimited, "", Some("still limited")) });
        assert!(blocks(&mut app).iter().any(|b| matches!(b, Block::Error(t) if t == "still limited")));
        assert_eq!(app.windows[0].activity, Activity::Failed);
    }

    #[tokio::test]
    async fn background_completion_is_flagged_until_focused() {
        let (mut app, _rx, _dir) = app(true);
        let first = app.active_id();
        press(&mut app, KeyCode::Char('n'), KeyModifiers::ALT);
        let target = Target::parse("claude:opus").expect("target");
        app.handle(AppEvent::TurnDone { window: first, target, result: result(Stop::Completed, "x", None) });
        assert_eq!(app.windows[0].activity, Activity::Unseen);
        press(&mut app, KeyCode::Char('1'), KeyModifiers::ALT);
        assert_eq!(app.windows[0].activity, Activity::Idle);
    }

    #[tokio::test]
    async fn at_mention_without_a_question_explains_itself() {
        let (mut app, _rx, _dir) = app(true);
        typed(&mut app, "@codex");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(blocks(&mut app).last(), Some(Block::Error(t)) if t.contains("question")));
    }

    #[tokio::test]
    async fn slash_hints_complete_with_tab_and_enter_runs_no_arg_commands() {
        let (mut app, _rx, _dir) = app(true);
        typed(&mut app, "/aut");
        assert_eq!(app.command_matches()[0].name, "/auto", "Olympus's own commands come before skills");
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.composer.text, "/auto ");
        assert!(app.command_matches().is_empty());
    }

    #[tokio::test]
    async fn ctrl_c_needs_two_presses_to_quit_an_idle_chat() {
        let (mut app, _rx, _dir) = app(true);
        press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(!app.quit);
        press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(app.quit);
    }

    #[tokio::test]
    async fn picker_filters_and_escape_closes() {
        let (mut app, _rx, _dir) = app(true);
        press(&mut app, KeyCode::Char('k'), KeyModifiers::ALT);
        typed(&mut app, "council");
        let picker = app.picker.as_ref().expect("picker");
        assert_eq!(picker.visible().len(), 1);
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.picker.is_none());
    }

    #[tokio::test]
    async fn a_finished_turn_waits_for_the_next_message_to_be_judged() {
        let (mut app, _rx, _dir) = app(false);
        let id = app.active_id();
        let target = Target::parse("claude:opus").expect("target");
        app.handle(AppEvent::TurnDone { window: id, target, result: result(Stop::Completed, "done", None) });
        assert!(app.chat().expect("chat").pending_run.is_some());
        typed(&mut app, "no, that's wrong");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.chat().expect("chat").pending_run.is_none());
    }

    #[tokio::test]
    async fn errors_in_a_terminal_window_reach_the_status_line() {
        let (mut app, _rx, dir) = app(true);
        app.new_term("sh", "cmd", &["/c".to_string(), "echo x".to_string()], dir.path().to_path_buf()).expect("term");
        app.fail_active("Choose a node first");
        assert_eq!(app.status, "Choose a node first");
    }

    #[tokio::test]
    async fn finished_runs_stop_showing_their_last_phase_and_notes_persist() {
        let (mut app, _rx, _dir) = app(true);
        let id = app.active_id();
        app.chat().expect("chat").push(Block::Tasks { phase: "Independent review".into(), rows: Vec::new() });
        app.handle(AppEvent::Flow { window: id, event: WorkflowEvent::Note("Heads up".into()) });
        app.handle(AppEvent::AutoDone { window: id, outcome: Err("no".into()) });
        let blocks = blocks(&mut app);
        assert!(blocks.iter().any(|b| matches!(b, Block::Tasks { phase, .. } if phase == "Stopped before finishing")));
        assert!(blocks.iter().any(|b| matches!(b, Block::Note(t) if t == "Heads up")));
    }

    #[tokio::test]
    async fn footer_data_tracks_reported_limits() {
        let (app, _rx, _dir) = app(true);
        app.rt.limits.update(LimitInfo { vendor: "claude".into(), window: "seven_day".into(), used: 0.17, resets_at: None, blocked: false });
        assert_eq!(app.rt.limits.snapshot().len(), 1);
    }

    #[tokio::test]
    async fn the_first_message_names_the_topic_unless_you_already_did() {
        let (mut app, _rx, dir) = app(false);
        let named = app.new_chat(dir.path().to_path_buf());
        assert_eq!(app.windows[app.active].title(), "New chat");
        app.send(named, "fix the login limiter\nand more".into());
        assert_eq!(app.windows[app.active].title(), "fix the login limiter");
        app.send(named, "another thing".into());
        assert_eq!(app.windows[app.active].title(), "fix the login limiter", "later messages don't rename it");
        let renamed = app.new_chat(dir.path().to_path_buf());
        app.chat().expect("chat").state.title = "Billing".into();
        app.send(renamed, "hello".into());
        assert_eq!(app.windows[app.active].title(), "Billing", "a name you chose stays");
    }

    #[tokio::test]
    async fn threads_keep_their_own_accounts_and_new_ones_start_on_the_current_account() {
        let (mut app, _rx, dir) = app(true);
        let work = PathBuf::from("C:/u/.claude-work");
        let first = app.active_id();
        app.chat().expect("chat").state.account_dir = Some(work.clone());
        let second = app.new_chat(dir.path().to_path_buf());
        assert_eq!(app.chat().expect("chat").state.account_dir.as_ref(), Some(&work), "a new thread inherits it");
        commands::pick(&mut app, Action::UseAccount(Provider::Claude, None));
        assert!(app.chat().expect("chat").state.account_dir.is_none(), "this thread moved to the default");
        let account = |app: &mut App, id| app.window_mut(id).and_then(Window::chat).expect("chat").state.account_dir.clone();
        assert_eq!(account(&mut app, first), Some(work), "the first thread keeps its own");
        assert_eq!(account(&mut app, second), None);
    }

    #[tokio::test]
    async fn finishing_a_sign_in_says_so() {
        let (mut app, _rx, _dir) = app(true);
        app.status = "Finish signing in here".into();
        app.handle(AppEvent::AccountAdded);
        assert!(app.status.is_empty());
        assert!(app.pending_flash.as_deref().is_some_and(|text| text.starts_with("Signed in")));
    }

    fn finish(app: &mut App, id: u64, stop: Stop, error: Option<&str>) {
        let target = Target::parse("claude:opus").expect("target");
        app.handle(AppEvent::TurnDone { window: id, target, result: result(stop, "text", error) });
    }

    #[tokio::test]
    async fn a_finished_turn_announces_itself_with_its_topic() {
        let (mut app, _rx, _dir) = app(false);
        app.notifications_on = true;
        let id = app.active_id();
        app.chat().expect("chat").state.title = "Billing work".into();
        finish(&mut app, id, Stop::Completed, None);
        assert_eq!(app.notices, [Notice { window: id, title: "Billing work".into(), body: "claude:opus finished".into(), ok: true }]);
    }

    #[tokio::test]
    async fn failures_use_the_failure_sound_and_say_why() {
        let (mut app, _rx, _dir) = app(false);
        app.notifications_on = true;
        let id = app.active_id();
        finish(&mut app, id, Stop::Failed, Some("disk full\nmore detail"));
        assert_eq!(app.notices[0].body, "Failed: disk full");
        assert!(!app.notices[0].ok);
    }

    #[tokio::test]
    async fn stopping_it_yourself_stays_quiet() {
        let (mut app, _rx, _dir) = app(false);
        app.notifications_on = true;
        let id = app.active_id();
        finish(&mut app, id, Stop::Cancelled, None);
        assert!(app.notices.is_empty());
    }

    #[tokio::test]
    async fn a_queued_message_or_a_failover_means_it_is_not_done_yet() {
        let (mut app, _rx, _dir) = app(true);
        app.notifications_on = true;
        let id = app.active_id();
        app.chat().expect("chat").queue.push_back("next".into());
        finish(&mut app, id, Stop::Completed, None);
        assert!(app.notices.is_empty(), "the queued message runs next");
        app.chat().expect("chat").queue.clear();
        app.chat().expect("chat").failed_over = false;
        finish(&mut app, id, Stop::RateLimited, Some("limit"));
        assert!(app.notices.is_empty(), "it carries on with the other vendor");
    }

    #[tokio::test]
    async fn turning_notifications_off_silences_everything() {
        let (mut app, _rx, _dir) = app(false);
        app.notifications_on = false;
        let id = app.active_id();
        finish(&mut app, id, Stop::Completed, None);
        app.handle(AppEvent::Notify { window: id, ok: true, body: "x".into() });
        assert!(app.notices.is_empty());
    }

    #[tokio::test]
    async fn closing_another_window_keeps_focus_on_the_same_chat() {
        let (mut app, _rx, dir) = app(true);
        let (second, third) = (app.new_chat(dir.path().to_path_buf()), app.new_chat(dir.path().to_path_buf()));
        app.focus(1);
        assert_eq!(app.active_id(), second);
        app.close_window(0);
        assert_eq!(app.active_id(), second, "the same chat stays focused when an earlier one closes");
        app.close_window(1);
        assert_eq!(app.windows.iter().filter(|w| w.id == third).count(), 0);
        assert_eq!(app.active_id(), second);
    }

    #[tokio::test]
    async fn a_closed_chat_stops_its_work_and_comes_back_with_reopen() {
        let (mut app, _rx, _dir) = app(true);
        let (cancel, mut stopped) = cancel_pair();
        let chat = app.chat().expect("chat");
        chat.push(Block::User("keep me".into()));
        chat.cancel = Some(cancel);
        app.close_active();
        assert!(*stopped.borrow_and_update(), "running work stops");
        assert_eq!(app.pending_flash.as_deref(), Some("Chat closed. /reopen brings it back."));
        assert!(blocks(&mut app).is_empty(), "the last window becomes a blank chat");
        app.reopen_chat();
        assert!(blocks(&mut app).iter().any(|b| matches!(b, Block::User(t) if t == "keep me")));
        app.reopen_chat();
        assert!(matches!(blocks(&mut app).last(), Some(Block::Error(t)) if t.contains("no closed chats")));
    }

    #[tokio::test]
    async fn a_late_result_for_a_closed_chat_is_ignored() {
        let (mut app, _rx, _dir) = app(true);
        let id = app.active_id();
        app.close_active();
        let target = Target::parse("claude:opus").expect("target");
        app.handle(AppEvent::TurnDone { window: id, target, result: result(Stop::Completed, "late", None) });
        assert!(blocks(&mut app).is_empty());
    }

    #[tokio::test]
    async fn typed_text_goes_to_the_palette_then_the_composer() {
        let (mut app, _rx, _dir) = app(true);
        press(&mut app, KeyCode::Char('k'), KeyModifiers::ALT);
        app.text_input("council");
        assert_eq!(app.picker.as_ref().expect("picker").visible().len(), 1);
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        app.text_input("åäö");
        assert_eq!(app.composer.text, "åäö");
    }

    #[tokio::test]
    async fn clicking_a_palette_row_runs_it_and_closes_the_palette() {
        let (mut app, _rx, _dir) = app(true);
        press(&mut app, KeyCode::Char('k'), KeyModifiers::ALT);
        app.text_input("help");
        app.choose_picker(0);
        assert!(app.picker.is_none());
        assert!(blocks(&mut app).iter().any(|b| matches!(b, Block::Assistant { .. })));
    }

    #[tokio::test]
    async fn transcript_changes_mark_the_earliest_changed_block() {
        let (mut app, _rx, _dir) = app(true);
        let chat = app.chat().expect("chat");
        chat.dirty_from = None;
        chat.push(Block::Note("a".into()));
        chat.push(Block::Note("b".into()));
        assert_eq!(chat.dirty_from, Some(0));
        chat.apply(AgentEvent::Text("hi".into()));
        chat.dirty_from = None;
        chat.apply(AgentEvent::Text(" there".into()));
        assert_eq!(chat.dirty_from, Some(2));
    }

    #[tokio::test]
    async fn merging_returns_the_review_panel_to_the_working_tree() {
        let (mut app, _rx, _dir) = app(true);
        let id = app.active_id();
        app.chat().expect("chat").state.last_branch = Some("olympus/x/main".into());
        app.diffs.entry(id).or_default().source = DiffSource::Branch("olympus/x/main".into());
        app.handle(AppEvent::Merged { window: id });
        assert!(app.chat().expect("chat").state.last_branch.is_none());
        assert_eq!(app.diffs[&id].source, DiffSource::Working);
    }

    #[tokio::test]
    async fn a_diff_for_the_old_source_never_replaces_the_new_one() {
        let (mut app, _rx, _dir) = app(true);
        let id = app.active_id();
        app.diffs.entry(id).or_default().source = DiffSource::Branch("b".into());
        app.handle(AppEvent::DiffLoaded { window: id, source: DiffSource::Working, outcome: Ok(vec![FileDiff::default()]) });
        assert!(app.diffs[&id].files.is_empty());
    }
}
