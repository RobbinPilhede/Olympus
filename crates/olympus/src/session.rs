use crate::pty::PtyPane;
use olympus_core::agent::{Profile, TurnRequest};
use olympus_core::event::{AgentEvent, Usage};
pub use olympus_core::limits::account_name;
use olympus_core::target::{Provider, Target};
use olympus_core::workflow::TaskState;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::sync::watch;

const MAX_BLOCKS: usize = 3000;
/// The title of a chat that hasn't got a topic yet.
pub const NEW_CHAT_TITLE: &str = "New chat";
const CLOSED_FOLDER: &str = "closed";
const ORDER_FILE: &str = "order.txt";
const MAX_CLOSED: usize = 50;

/// How a running tool call finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolStatus {
    Running,
    Ok,
    Failed,
}

/// One row of a workflow's task board.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskRow {
    pub id: String,
    pub title: String,
    pub state: TaskState,
    pub target: Option<String>,
    pub note: String,
}

/// One item in a chat transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Block {
    User(String),
    Assistant { label: Option<String>, text: String },
    Tool { id: String, name: String, detail: String, status: ToolStatus, summary: String },
    Note(String),
    Error(String),
    Tasks { phase: String, rows: Vec<TaskRow> },
}

/// What the session is doing, as shown in the rail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Activity {
    Idle,
    Running,
    /// Finished while in the background.
    Unseen,
    Failed,
}

/// Model choice for a chat.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Choice {
    Auto,
    Fixed(Target),
}

impl Choice {
    /// The label shown in the header.
    pub fn label(&self) -> String {
        match self {
            Choice::Auto => "auto".into(),
            Choice::Fixed(target) => target.to_string(),
        }
    }
}

/// The saved part of a chat.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatState {
    pub title: String,
    pub cwd: PathBuf,
    pub choice: Choice,
    pub last_target: Option<Target>,
    pub vendor_session: Option<String>,
    pub full_profile: bool,
    pub usage: Usage,
    pub last_branch: Option<String>,
    /// The Claude config folder this chat signs in with; None is the normal `~/.claude`.
    #[serde(default)]
    pub account_dir: Option<PathBuf>,
    /// The Codex home folder this chat signs in with; None is the normal `~/.codex`.
    #[serde(default)]
    pub codex_account_dir: Option<PathBuf>,
    pub blocks: Vec<Block>,
}

impl ChatState {
    /// The provider this chat is on, once it has one.
    pub fn provider(&self) -> Option<&Provider> {
        match &self.choice {
            Choice::Fixed(target) => Some(&target.provider),
            Choice::Auto => self.last_target.as_ref().map(|target| &target.provider),
        }
    }

    /// The account folder this chat uses on a provider.
    pub fn account_on(&self, provider: &Provider) -> Option<&Path> {
        match provider {
            Provider::Claude => self.account_dir.as_deref(),
            Provider::Codex => self.codex_account_dir.as_deref(),
            _ => None,
        }
    }
}

/// A finished turn whose verdict depends on the user's next message.
pub struct PendingRun {
    pub run_id: String,
    pub request: TurnRequest,
    pub usage: Usage,
}

/// A message a chat repeats until stopped; lives only while Olympus is open.
pub struct Loop {
    pub prompt: String,
    /// None lets the model choose how long to wait each time.
    pub every: Option<Duration>,
    pub started: Option<Instant>,
    /// Bumped per scheduled run, so a timer that was replaced does nothing.
    pub generation: u64,
}

impl Loop {
    /// What the thread list says about this loop.
    pub fn label(&self) -> String {
        match self.every {
            Some(every) => format!("looping every {}", short_duration(every)),
            None => "looping".to_string(),
        }
    }
}

/// A duration as you'd type it, like `5m`.
pub fn short_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    match seconds {
        s if s >= 3600 && s % 3600 == 0 => format!("{}h", s / 3600),
        s if s >= 60 && s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

/// A chat with any provider.
pub struct Chat {
    pub state: ChatState,
    pub pending_run: Option<PendingRun>,
    pub queue: VecDeque<String>,
    pub cancel: Option<watch::Sender<bool>>,
    pub last_prompt: String,
    pub failed_over: bool,
    pub looping: Option<Loop>,
    /// Set when the view should return to the newest message.
    pub follow: bool,
    /// The first block changed since the interface last looked.
    pub dirty_from: Option<usize>,
    pub file_id: String,
}

impl Chat {
    /// Creates an empty chat.
    pub fn new(title: String, cwd: PathBuf, choice: Choice) -> Self {
        let file_id = format!("{:x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
        Self::restore(ChatState { title, cwd, choice, last_target: None, vendor_session: None, full_profile: false, usage: Usage::default(), last_branch: None, account_dir: None, codex_account_dir: None, blocks: Vec::new() }, file_id)
    }

    /// Rebuilds a chat from saved state.
    pub fn restore(state: ChatState, file_id: String) -> Self {
        Self { state, pending_run: None, queue: VecDeque::new(), cancel: None, last_prompt: String::new(), failed_over: false, looping: None, follow: false, dirty_from: Some(0), file_id }
    }

    /// True until a message or `/rename` gives the chat a topic; older chats were titled after their folder.
    pub fn has_default_title(&self) -> bool {
        self.state.title == NEW_CHAT_TITLE || self.state.cwd.file_name().is_some_and(|name| name.to_string_lossy() == self.state.title)
    }

    /// The profile for the next turn.
    pub fn profile(&self) -> Profile {
        if self.state.full_profile { Profile::Full } else { Profile::Lean }
    }

    /// Notes that a block changed, so the interface redraws it and everything after it.
    pub fn touch(&mut self, index: usize) {
        self.dirty_from = Some(self.dirty_from.map_or(index, |from| from.min(index)));
    }

    /// Adds a block, dropping the oldest past the cap so long sessions keep a bounded footprint.
    pub fn push(&mut self, block: Block) {
        self.state.blocks.push(block);
        self.touch(self.state.blocks.len() - 1);
        if self.state.blocks.len() > MAX_BLOCKS {
            self.state.blocks.drain(..MAX_BLOCKS / 10);
            self.touch(0);
        }
    }

    /// Applies a backend event to the transcript.
    pub fn apply(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Session(id) => self.state.vendor_session = Some(id),
            AgentEvent::Text(text) => {
                if let Some(Block::Assistant { text: existing, label: None }) = self.state.blocks.last_mut() {
                    existing.push_str(&text);
                    self.touch(self.state.blocks.len() - 1);
                } else {
                    self.push(Block::Assistant { label: None, text });
                }
            }
            AgentEvent::ToolStart { id, name, detail } => self.push(Block::Tool { id, name, detail, status: ToolStatus::Running, summary: String::new() }),
            AgentEvent::ToolEnd { id, ok, summary } => {
                let found = self.state.blocks.iter().rposition(|b| matches!(b, Block::Tool { id: tool, .. } if *tool == id));
                if let Some(index) = found {
                    if let Block::Tool { status, summary: slot, .. } = &mut self.state.blocks[index] {
                        *status = if ok { ToolStatus::Ok } else { ToolStatus::Failed };
                        *slot = summary;
                    }
                    self.touch(index);
                }
            }
            AgentEvent::Status(text) => self.push(Block::Note(text)),
            AgentEvent::Limit(_) => {}
        }
    }

    /// Replaces the final assistant text with the authoritative result when streaming missed it.
    pub fn finish_text(&mut self, text: &str) {
        let streamed = self.state.blocks.iter().rev().take_while(|b| !matches!(b, Block::User(_))).any(|b| matches!(b, Block::Assistant { .. }));
        if !streamed && !text.is_empty() {
            self.push(Block::Assistant { label: None, text: text.to_string() });
        }
    }

    /// A recent-history digest for handing work to another model.
    pub fn handoff(&self, max: usize) -> String {
        let mut parts: Vec<String> = Vec::new();
        let mut used = 0;
        for block in self.state.blocks.iter().rev() {
            let line = match block {
                Block::User(text) => format!("User: {text}"),
                Block::Assistant { text, .. } => format!("Assistant: {text}"),
                Block::Tool { name, detail, .. } => format!("Tool {name}: {detail}"),
                _ => continue,
            };
            used += line.len();
            if used > max {
                break;
            }
            parts.push(line);
        }
        parts.reverse();
        parts.join("\n")
    }
}

/// A window title from free text, marked when cut.
pub fn short_title(text: &str) -> String {
    if text.chars().count() <= 40 {
        return text.to_string();
    }
    let mut title: String = text.chars().take(39).collect();
    title.push('…');
    title
}

/// A saved chat's file on disk.
pub fn save(chat: &Chat, dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let text = serde_json::to_string(&chat.state).map_err(std::io::Error::other)?;
    let path = dir.join(format!("{}.json", chat.file_id));
    let temp = dir.join(format!("{}.tmp", chat.file_id));
    std::fs::write(&temp, text)?;
    std::fs::rename(temp, path)
}

fn closed_files(closed: &std::path::Path) -> Vec<(std::time::SystemTime, PathBuf)> {
    let mut files: Vec<_> = std::fs::read_dir(closed)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|e| e == "json"))
        .map(|entry| (entry.metadata().and_then(|m| m.modified()).unwrap_or(std::time::SystemTime::UNIX_EPOCH), entry.path()))
        .collect();
    files.sort();
    files
}

/// Files a closed chat away so `/reopen` can restore it, keeping only the newest few.
pub fn archive(chat: &Chat, dir: &std::path::Path) -> std::io::Result<()> {
    save(chat, dir)?;
    let closed = dir.join(CLOSED_FOLDER);
    std::fs::create_dir_all(&closed)?;
    let name = format!("{}.json", chat.file_id);
    std::fs::rename(dir.join(&name), closed.join(&name))?;
    // Renaming keeps the old modified time, and "newest" must mean most recently closed.
    let text = std::fs::read(closed.join(&name))?;
    std::fs::write(closed.join(&name), text)?;
    let files = closed_files(&closed);
    for (_, old) in files.iter().take(files.len().saturating_sub(MAX_CLOSED)) {
        let _ = std::fs::remove_file(old);
    }
    Ok(())
}

/// Restores the most recently closed chat that can still be read.
pub fn reopen_latest(dir: &std::path::Path) -> Option<Chat> {
    let closed = dir.join(CLOSED_FOLDER);
    for (_, path) in closed_files(&closed).into_iter().rev() {
        let id = path.file_stem()?.to_string_lossy().to_string();
        let state = std::fs::read_to_string(&path).ok().and_then(|text| serde_json::from_str::<ChatState>(&text).ok());
        if let Some(state) = state {
            std::fs::rename(&path, dir.join(format!("{id}.json"))).ok()?;
            return Some(Chat::restore(state, id));
        }
    }
    None
}

/// Loads saved chats, oldest first; unreadable files are skipped rather than blocking startup.
pub fn load_all(dir: &std::path::Path) -> Vec<Chat> {
    let mut chats: Vec<(std::time::SystemTime, Chat)> = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Some(id) = path.file_stem().map(|s| s.to_string_lossy().to_string()) else { continue };
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Ok(state) = serde_json::from_str::<ChatState>(&text) else { continue };
        let time = entry.metadata().and_then(|m| m.modified()).unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        chats.push((time, Chat::restore(state, id)));
    }
    chats.sort_by_key(|c| c.0);
    chats.into_iter().map(|c| c.1).collect()
}

/// Remembers the order of the thread list by chat file.
pub fn save_order(dir: &std::path::Path, ids: &[&str]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join(ORDER_FILE), ids.join("\n"))
}

/// Sorts chats into the remembered order; chats it doesn't know keep their order, after the rest.
pub fn in_saved_order(dir: &std::path::Path, mut chats: Vec<Chat>) -> Vec<Chat> {
    let saved = std::fs::read_to_string(dir.join(ORDER_FILE)).unwrap_or_default();
    let ids: Vec<&str> = saved.lines().collect();
    chats.sort_by_key(|chat| ids.iter().position(|id| *id == chat.file_id).unwrap_or(usize::MAX));
    chats
}

/// A terminal pane running a real program.
pub struct TermPane {
    pub title: String,
    pub pty: PtyPane,
}

/// One switchable window.
pub enum Pane {
    Chat(Box<Chat>),
    Term(Box<TermPane>),
}

/// A window and its rail state.
pub struct Window {
    pub pane: Pane,
    pub activity: Activity,
    pub id: u64,
}

impl Window {
    /// The title shown in the rail.
    pub fn title(&self) -> &str {
        match &self.pane {
            Pane::Chat(chat) => &chat.state.title,
            Pane::Term(term) => &term.title,
        }
    }

    /// The chat, if this window has one.
    pub fn chat(&mut self) -> Option<&mut Chat> {
        match &mut self.pane {
            Pane::Chat(chat) => Some(chat),
            Pane::Term(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat() -> Chat {
        Chat::new("t".into(), PathBuf::from("."), Choice::Auto)
    }

    #[test]
    fn streamed_text_joins_the_same_block_until_a_tool_interrupts() {
        let mut chat = chat();
        chat.apply(AgentEvent::Text("a".into()));
        chat.apply(AgentEvent::Text("b".into()));
        chat.apply(AgentEvent::ToolStart { id: "1".into(), name: "Bash".into(), detail: "ls".into() });
        chat.apply(AgentEvent::Text("c".into()));
        assert_eq!(chat.state.blocks.len(), 3);
        assert!(matches!(&chat.state.blocks[0], Block::Assistant { text, .. } if text == "ab"));
    }

    #[test]
    fn tool_end_updates_the_matching_call() {
        let mut chat = chat();
        chat.apply(AgentEvent::ToolStart { id: "1".into(), name: "A".into(), detail: String::new() });
        chat.apply(AgentEvent::ToolStart { id: "2".into(), name: "B".into(), detail: String::new() });
        chat.apply(AgentEvent::ToolEnd { id: "1".into(), ok: false, summary: "nope".into() });
        assert!(matches!(&chat.state.blocks[0], Block::Tool { status: ToolStatus::Failed, summary, .. } if summary == "nope"));
        assert!(matches!(&chat.state.blocks[1], Block::Tool { status: ToolStatus::Running, .. }));
    }

    #[test]
    fn final_text_is_added_only_when_nothing_streamed() {
        let mut chat = chat();
        chat.push(Block::User("q".into()));
        chat.finish_text("answer");
        chat.finish_text("answer");
        assert_eq!(chat.state.blocks.iter().filter(|b| matches!(b, Block::Assistant { .. })).count(), 1);
    }

    #[test]
    fn account_folders_get_short_names() {
        assert_eq!(account_name(None), "default");
        assert_eq!(account_name(Some(std::path::Path::new("C:/u/.claude"))), "default");
        assert_eq!(account_name(Some(std::path::Path::new("C:/u/.claude-work"))), "work");
        assert_eq!(account_name(Some(std::path::Path::new("C:/u/.codex"))), "default");
        assert_eq!(account_name(Some(std::path::Path::new("C:/u/.codex-work"))), "work");
    }

    #[test]
    fn chats_saved_before_accounts_existed_still_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let original = chat();
        save(&original, dir.path()).expect("save");
        let path = dir.path().join(format!("{}.json", original.file_id));
        let mut value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
        value.as_object_mut().expect("object").remove("account_dir");
        value.as_object_mut().expect("object").remove("codex_account_dir");
        std::fs::write(&path, value.to_string()).expect("write");
        assert!(load_all(dir.path())[0].state.account_dir.is_none());
    }

    #[test]
    fn transcript_memory_is_bounded() {
        let mut chat = chat();
        for i in 0..(MAX_BLOCKS + 10) {
            chat.push(Block::Note(i.to_string()));
        }
        assert!(chat.state.blocks.len() <= MAX_BLOCKS);
        assert!(matches!(chat.state.blocks.last(), Some(Block::Note(n)) if *n == (MAX_BLOCKS + 9).to_string()));
    }

    #[test]
    fn handoff_keeps_the_most_recent_within_budget() {
        let mut chat = chat();
        chat.push(Block::User("old ".repeat(100)));
        chat.push(Block::User("newest".into()));
        chat.push(Block::Note("ignored".into()));
        let digest = chat.handoff(50);
        assert_eq!(digest, "User: newest");
    }

    #[test]
    fn the_thread_order_survives_a_restart_and_new_chats_go_last() {
        let dir = tempfile::tempdir().expect("dir");
        save_order(dir.path(), &["b", "a"]).expect("saved");
        let chat = |id: &str| {
            let mut chat = Chat::new(String::new(), PathBuf::new(), Choice::Auto);
            chat.file_id = id.to_string();
            chat
        };
        let sorted = in_saved_order(dir.path(), vec![chat("a"), chat("new"), chat("b"), chat("newer")]);
        let ids: Vec<&str> = sorted.iter().map(|chat| chat.file_id.as_str()).collect();
        assert_eq!(ids, ["b", "a", "new", "newer"]);
    }

    #[test]
    fn saved_chats_survive_a_restart_and_corrupt_files_are_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut original = chat();
        original.state.vendor_session = Some("abc".into());
        original.push(Block::Error("boom".into()));
        save(&original, dir.path()).expect("save");
        std::fs::write(dir.path().join("broken.json"), "{not json").expect("write");
        let loaded = load_all(dir.path());
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].state.vendor_session.as_deref(), Some("abc"));
        assert_eq!(loaded[0].state.blocks, original.state.blocks);
    }

    #[test]
    fn closed_chats_leave_the_list_and_reopen_newest_first_with_their_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(reopen_latest(dir.path()).is_none());
        let mut first = Chat::restore(chat().state, "first".into());
        first.state.vendor_session = Some("abc".into());
        first.push(Block::Note("one".into()));
        archive(&first, dir.path()).expect("archive");
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut second = Chat::restore(chat().state, "second".into());
        second.push(Block::Note("two".into()));
        save(&second, dir.path()).expect("save");
        archive(&second, dir.path()).expect("archive");
        assert!(load_all(dir.path()).is_empty(), "closed chats don't come back on restart");
        assert_eq!(reopen_latest(dir.path()).expect("second").file_id, "second");
        let again = reopen_latest(dir.path()).expect("first");
        assert_eq!(again.state.vendor_session.as_deref(), Some("abc"));
        assert_eq!(load_all(dir.path()).len(), 2);
        assert!(reopen_latest(dir.path()).is_none());
    }

    #[test]
    fn only_the_newest_closed_chats_are_kept_and_corrupt_ones_are_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        for i in 0..(MAX_CLOSED + 3) {
            archive(&Chat::restore(chat().state, format!("c{i}")), dir.path()).expect("archive");
        }
        assert_eq!(closed_files(&dir.path().join(CLOSED_FOLDER)).len(), MAX_CLOSED);
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(dir.path().join(CLOSED_FOLDER).join("broken.json"), "{nope").expect("write");
        assert_ne!(reopen_latest(dir.path()).expect("readable").file_id, "broken");
    }
}
