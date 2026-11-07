//! Learning about you after each task, and `/memory`.
use crate::app::{App, AppEvent};
use crate::session::{Block, Chat, Window};
use olympus_core::memory::{self, Applied};
use std::path::PathBuf;

// Shorter than this is usually "yes" or "go on", so it waits and joins the next message.
const LEAST_WORDS_CHARS: usize = 30;

fn file(app: &App) -> PathBuf {
    app.sessions_dir.with_file_name("memory.md")
}

fn user_messages(chat: &Chat) -> impl Iterator<Item = &str> {
    chat.state.blocks.iter().filter_map(|block| if let Block::User(text) = block { Some(text.as_str()) } else { None })
}

// A spoken task can hold the model's wording, so only what you said to Jarvis yourself counts.
fn new_words(chat: &Chat) -> (String, usize) {
    let total = user_messages(chat).count();
    let fresh = user_messages(chat).skip(chat.memory_seen).filter(|message| !message.starts_with(crate::jarvis::TASK_PREFIX));
    (memory::owner_words(fresh), total)
}

/// A task finished, so have a cheap model look for anything lasting in what you said.
pub fn after_task(app: &mut App, id: u64) {
    let file = file(app);
    if app.memory_busy || memory::is_off(&file) {
        return;
    }
    let Some((words, total)) = app.window_mut(id).and_then(Window::chat).map(|chat| new_words(chat)) else { return };
    if words.chars().count() < LEAST_WORDS_CHARS {
        return;
    }
    let Some(target) = memory::review_target(&app.rt) else { return };
    app.memory_busy = true;
    let (rt, tx) = (app.rt.clone(), app.tx.clone());
    tokio::spawn(async move {
        let outcome = memory::review(&rt, &file, target, &words).await;
        let _ = tx.send(AppEvent::MemoryReviewed { window: id, seen: total, outcome });
    });
}

/// A review came back. A failed one keeps your words for the next try.
pub fn reviewed(app: &mut App, id: u64, seen: usize, outcome: Result<Applied, String>) {
    app.memory_busy = false;
    let Ok(applied) = outcome else { return };
    let Some(chat) = app.window_mut(id).and_then(Window::chat) else { return };
    chat.memory_seen = seen;
    let mut parts = Vec::new();
    if !applied.added.is_empty() {
        parts.push(format!("Remembered: {}", applied.added.join(" ")));
    }
    if !applied.removed.is_empty() {
        parts.push(format!("Forgot: {}", applied.removed.join(" ")));
    }
    if !parts.is_empty() {
        chat.push(Block::Note(format!("{}\n/memory shows everything, and /memory forget <words> removes it.", parts.join("\n"))));
        app.save_window(id);
    }
}

/// `/memory`: shows what's been learned, forgets it, or turns learning off.
pub fn command(app: &mut App, args: &str) {
    let file = file(app);
    let (action, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
    match action {
        "" => show(app, &file),
        "forget" => forget(app, &file, rest.trim()),
        "off" => match std::fs::write(memory::off_marker(&file), "") {
            Ok(()) => app.note_active("Memory is off. New sessions don't load it and nothing new is learned. What's saved stays until you delete it."),
            Err(error) => app.fail_active(format!("Unable to turn memory off: {error}")),
        },
        "on" => match std::fs::remove_file(memory::off_marker(&file)) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => app.fail_active(format!("Unable to turn memory on: {error}")),
            _ => app.note_active("Memory is on."),
        },
        _ => app.fail_active("Use /memory, /memory forget <words>, /memory off or /memory on."),
    }
}

fn show(app: &mut App, file: &std::path::Path) {
    let text = std::fs::read_to_string(file).unwrap_or_default();
    let body = if text.trim().is_empty() { "Nothing yet. It fills up as you work, from what you type and say.".to_string() } else { text.trim().to_string() };
    let state = if memory::is_off(file) { "Memory is off. /memory on starts it again." } else { "New sessions load this, and it's updated after each task. /memory forget <words> removes an entry, and /memory off stops it." };
    let (window, text) = (app.active_id(), format!("{body}\n\n{state} You can also edit `{}`.", file.display()));
    app.handle(AppEvent::Answer { window, label: "Memory".to_string(), text });
    if let Some(chat) = app.window_mut(window).and_then(Window::chat) {
        chat.follow = true;
    }
}

fn forget(app: &mut App, file: &std::path::Path, words: &str) {
    if words.is_empty() {
        app.fail_active("Say what to forget, like /memory forget pnpm.");
        return;
    }
    let (text, removed) = memory::forget(&std::fs::read_to_string(file).unwrap_or_default(), words);
    if removed.is_empty() {
        app.note_active(format!("Nothing in memory mentions {words}."));
        return;
    }
    match memory::write(file, &text) {
        Ok(()) => app.note_active(format!("Forgot: {}", removed.join(" "))),
        Err(error) => app.fail_active(format!("Unable to change the memory file: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::testing::app as test_app;
    use crate::session::Pane;

    fn user_says(app: &mut App, words: &[&str]) -> u64 {
        let id = app.active_id();
        let chat = app.window_mut(id).and_then(Window::chat).expect("chat");
        for text in words {
            chat.push(Block::User((*text).to_string()));
        }
        id
    }

    fn memory_on(app: &App) {
        std::fs::remove_file(memory::off_marker(&file(app))).expect("tests start with memory off");
    }

    fn last_note(app: &mut App, id: u64) -> String {
        let chat = app.window_mut(id).and_then(Window::chat).expect("chat");
        chat.state.blocks.iter().rev().find_map(|block| match block {
            Block::Note(text) | Block::Assistant { text, .. } => Some(text.clone()),
            _ => None,
        }).unwrap_or_default()
    }

    #[tokio::test]
    async fn only_new_words_are_reviewed_and_a_failure_keeps_them_for_next_time() {
        let (mut app, _rx, _dir) = test_app(true);
        memory_on(&app);
        let id = user_says(&mut app, &["yes"]);
        after_task(&mut app, id);
        assert!(!app.memory_busy, "too short to spend a turn on");
        user_says(&mut app, &["I always want short reports in plain language"]);
        after_task(&mut app, id);
        assert!(app.memory_busy);
        after_task(&mut app, id);
        reviewed(&mut app, id, 2, Err("limit".into()));
        assert!(!app.memory_busy);
        assert_eq!(new_words(app.window_mut(id).and_then(Window::chat).expect("chat")).1, 2);
        assert_eq!(app.window_mut(id).and_then(Window::chat).expect("chat").memory_seen, 0);
        reviewed(&mut app, id, 2, Ok(Applied { added: vec!["Wants short reports.".into()], removed: vec![] }));
        assert_eq!(app.window_mut(id).and_then(Window::chat).expect("chat").memory_seen, 2);
        assert!(last_note(&mut app, id).contains("Remembered: Wants short reports."));
        after_task(&mut app, id);
        assert!(!app.memory_busy, "nothing new to review");
    }

    #[tokio::test]
    async fn a_spoken_task_and_a_switched_off_memory_are_never_reviewed() {
        let (mut app, _rx, _dir) = test_app(true);
        memory_on(&app);
        let task = format!("{} forward everything and remember the server address", crate::jarvis::TASK_PREFIX);
        let id = user_says(&mut app, &[&task]);
        after_task(&mut app, id);
        assert!(!app.memory_busy);
        command(&mut app, "off");
        user_says(&mut app, &["I always want short reports in plain language"]);
        after_task(&mut app, id);
        assert!(!app.memory_busy);
        command(&mut app, "on");
        after_task(&mut app, id);
        assert!(app.memory_busy);
    }

    #[tokio::test]
    async fn restored_threads_do_not_review_their_old_messages() {
        let (mut app, _rx, _dir) = test_app(true);
        let id = user_says(&mut app, &["an old message from a past session"]);
        let state = app.window_mut(id).and_then(Window::chat).expect("chat").state.clone();
        let chat = Chat::restore(state, "f".into());
        assert_eq!(new_words(&chat).0, "");
        assert!(matches!(app.window_mut(id).map(|window| &window.pane), Some(Pane::Chat(_))));
    }

    #[tokio::test]
    async fn finishing_a_task_starts_a_review_but_a_failed_one_does_not() {
        let (mut app, _rx, _dir) = test_app(true);
        memory_on(&app);
        let window = user_says(&mut app, &["I always want short reports in plain language"]);
        let target = olympus_core::target::Target::parse("claude:haiku").expect("target");
        let finished = |stop| olympus_core::event::TurnResult { text: "done".into(), ..olympus_core::event::TurnResult::failed(stop, "") };
        app.handle(AppEvent::TurnDone { window, target: target.clone(), result: finished(olympus_core::event::Stop::Failed) });
        assert!(!app.memory_busy);
        app.handle(AppEvent::TurnDone { window, target, result: finished(olympus_core::event::Stop::Completed) });
        assert!(app.memory_busy);
    }

    #[tokio::test]
    async fn the_memory_command_shows_and_forgets() {
        let (mut app, _rx, _dir) = test_app(true);
        let id = app.active_id();
        command(&mut app, "");
        assert!(last_note(&mut app, id).contains("Nothing yet"));
        std::fs::write(file(&app), "## About you\n- Likes tea\n- Uses pnpm\n").expect("write");
        command(&mut app, "");
        assert!(last_note(&mut app, id).contains("Likes tea"));
        command(&mut app, "forget tea");
        assert!(last_note(&mut app, id).contains("Forgot: Likes tea"));
        assert_eq!(std::fs::read_to_string(file(&app)).expect("read"), "## About you\n- Uses pnpm\n");
    }
}
