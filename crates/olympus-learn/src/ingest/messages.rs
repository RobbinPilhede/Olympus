//! Indexes what was said in past sessions for full-text search, apart from signal extraction.
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

use super::lines::{scan_lines, READ_BUFFER_BYTES};
use super::{discover_claude, discover_codex, Discovered, Source};
use crate::math::row_id;
use crate::text::{fts_join, is_wrapper, redact, strip_reminders, terms, truncate};
use crate::types::MessageHit;
use crate::Store;

const MAX_MESSAGE_CHARS: usize = 4000;

#[derive(Default)]
struct FileState {
    session: String,
    cwd: String,
}

struct Message {
    vendor: &'static str,
    session: String,
    cwd: String,
    timestamp: String,
    role: String,
    text: String,
}

fn joined_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks.iter().filter(|block| matches!(block["type"].as_str(), Some("text" | "input_text" | "output_text"))).filter_map(|block| block["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

fn string(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_string()
}

fn claude_message(line: &Value) -> Option<Message> {
    let role = line["type"].as_str().filter(|kind| matches!(*kind, "user" | "assistant"))?;
    if line["isSidechain"].as_bool() == Some(true) || line["isMeta"].as_bool() == Some(true) {
        return None;
    }
    Some(Message { vendor: "claude", session: string(&line["sessionId"]), cwd: string(&line["cwd"]), timestamp: string(&line["timestamp"]), role: role.to_string(), text: joined_text(&line["message"]["content"]) })
}

fn codex_message(line: &Value, state: &mut FileState) -> Option<Message> {
    let payload = &line["payload"];
    match line["type"].as_str()? {
        "session_meta" => {
            state.session = string(&payload["id"]);
            state.cwd = string(&payload["cwd"]);
            None
        }
        "response_item" if payload["type"] == "message" => {
            let role = payload["role"].as_str().filter(|role| matches!(*role, "user" | "assistant"))?;
            Some(Message { vendor: "codex", session: state.session.clone(), cwd: state.cwd.clone(), timestamp: string(&line["timestamp"]), role: role.to_string(), text: joined_text(&payload["content"]) })
        }
        _ => None,
    }
}

fn store_message(conn: &Connection, message: &Message) -> Result<()> {
    let stripped = strip_reminders(&message.text);
    let text = stripped.trim();
    // Codex and Claude both inject tagged context blocks that nobody typed.
    if text.is_empty() || message.session.is_empty() || text.starts_with('<') || is_wrapper(text) {
        return Ok(());
    }
    let stored = redact(truncate(text, MAX_MESSAGE_CHARS));
    let id = row_id(&["message", &message.session, &message.timestamp, &message.role, &stored]);
    let inserted = conn.prepare_cached("INSERT OR IGNORE INTO messages(id, vendor, session, cwd, timestamp, role, text) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)")?.execute(params![id, message.vendor, message.session, message.cwd, message.timestamp, message.role, stored])?;
    if inserted == 1 {
        conn.prepare_cached("INSERT INTO messages_fts(rowid, text) VALUES(?1, ?2)")?.execute(params![id, stored])?;
    }
    Ok(())
}

impl Store {
    pub(super) fn index_messages(&self, claude_dir: Option<&Path>, codex_dir: Option<&Path>) -> Result<()> {
        let mut found = Vec::new();
        if let Some(dir) = claude_dir {
            discover_claude(dir, &mut found);
        }
        if let Some(dir) = codex_dir {
            discover_codex(dir, &mut found);
        }
        for file in &found {
            if !matches!(file.source, Source::Claude { sidechain: true }) {
                self.index_file(file)?;
            }
        }
        Ok(())
    }

    fn index_file(&self, file: &Discovered) -> Result<()> {
        let path = file.path.to_string_lossy();
        let Ok(meta) = fs::metadata(&file.path) else { return Ok(()) };
        let size = meta.len() as i64;
        let known: Option<(i64, i64, String, String)> = self.conn.query_row("SELECT byte_offset, size, session, cwd FROM message_files WHERE path = ?1", [path.as_ref()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional()?;
        if known.as_ref().is_some_and(|(_, known_size, _, _)| *known_size == size) {
            return Ok(());
        }
        let compressed = path.ends_with(".zst");
        // A compressed rollout repeats its plain form, and offsets can't be seeked in it.
        let (start, mut state) = match known {
            Some((offset, _, session, cwd)) if offset <= size && !compressed => (offset, FileState { session, cwd }),
            _ => (0, FileState::default()),
        };
        let Ok(mut handle) = File::open(&file.path) else { return Ok(()) };
        if handle.seek(SeekFrom::Start(start as u64)).is_err() {
            return Ok(());
        }
        let buffered = BufReader::with_capacity(READ_BUFFER_BYTES, handle);
        let mut reader: Box<dyn BufRead> = if compressed {
            let Ok(decoder) = ruzstd::decoding::StreamingDecoder::new(buffered) else { return Ok(()) };
            Box::new(BufReader::with_capacity(READ_BUFFER_BYTES, decoder))
        } else {
            Box::new(buffered)
        };
        let codex = matches!(file.source, Source::Codex);
        let tx = self.conn.unchecked_transaction()?;
        let scan = scan_lines(&mut reader, |raw| {
            let Ok(line) = serde_json::from_slice::<Value>(raw) else { return Ok(()) };
            let message = if codex { codex_message(&line, &mut state) } else { claude_message(&line) };
            match message {
                Some(message) => store_message(&self.conn, &message),
                None => Ok(()),
            }
        })?;
        let recorded_size = if scan.read_failed { -1 } else { size };
        tx.execute(
            "INSERT OR REPLACE INTO message_files(path, byte_offset, size, session, cwd) VALUES(?1, ?2, ?3, ?4, ?5)",
            params![path.as_ref(), start + scan.consumed as i64, recorded_size, state.session, state.cwd],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Finds past messages, best match first, keeping one hit per session.
    pub fn search_messages(&self, query: &str, limit: usize) -> Result<Vec<MessageHit>> {
        let query_terms = terms(query);
        if query_terms.is_empty() {
            return Ok(Vec::new());
        }
        let hits = self.message_query(&fts_join(&query_terms, " AND "), limit)?;
        if !hits.is_empty() {
            return Ok(hits);
        }
        self.message_query(&fts_join(&query_terms, " OR "), limit)
    }

    fn message_query(&self, fts: &str, limit: usize) -> Result<Vec<MessageHit>> {
        let mut statement = self.conn.prepare_cached(
            "SELECT m.vendor, m.session, m.cwd, m.timestamp, m.role, m.text, snippet(messages_fts, 0, '', '', '...', 24) \
             FROM messages_fts JOIN messages m ON m.id = messages_fts.rowid \
             WHERE messages_fts MATCH ?1 ORDER BY bm25(messages_fts) LIMIT ?2",
        )?;
        let rows = statement.query_map(params![fts, (limit * 8) as i64], |r| Ok(MessageHit { vendor: r.get(0)?, session: r.get(1)?, cwd: r.get(2)?, timestamp: r.get(3)?, role: r.get(4)?, text: r.get(5)?, snippet: r.get(6)? }))?;
        let mut seen = std::collections::HashSet::new();
        let mut hits = Vec::new();
        for hit in rows {
            let hit = hit?;
            if seen.insert(hit.session.clone()) {
                hits.push(hit);
                if hits.len() == limit {
                    break;
                }
            }
        }
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use crate::Store;
    use std::path::Path;

    fn claude_line(session: &str, role: &str, text: &str) -> String {
        serde_json::json!({"type": role, "sessionId": session, "cwd": "C:/work/api", "timestamp": "2026-11-05T10:00:00Z", "uuid": format!("{session}{text}"), "message": {"role": role, "content": [{"type": "text", "text": text}]}}).to_string()
    }

    fn write_session(dir: &Path, name: &str, lines: &[String]) {
        let project = dir.join("projects").join("demo");
        std::fs::create_dir_all(&project).expect("project");
        std::fs::write(project.join(format!("{name}.jsonl")), lines.join("\n") + "\n").expect("session");
    }

    fn open() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(&dir.path().join("learn.db")).expect("store");
        (dir, store)
    }

    #[test]
    fn replies_are_searchable_and_each_session_appears_once() {
        let (dir, store) = open();
        let claude = dir.path().join(".claude");
        write_session(&claude, "s1", &[claude_line("s1", "user", "why is the queue slow"), claude_line("s1", "assistant", "the rabbitmq consumer prefetch is too low"), claude_line("s1", "assistant", "raising rabbitmq prefetch fixed it")]);
        store.ingest(Some(&claude), None, &mut |_| {}).expect("ingest");
        let hits = store.search_messages("rabbitmq prefetch", 5).expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!((hits[0].session.as_str(), hits[0].role.as_str(), hits[0].vendor.as_str()), ("s1", "assistant", "claude"));
    }

    #[test]
    fn reading_a_session_again_adds_nothing_and_new_lines_are_picked_up() {
        let (dir, store) = open();
        let claude = dir.path().join(".claude");
        write_session(&claude, "s1", &[claude_line("s1", "user", "first zebra message")]);
        store.ingest(Some(&claude), None, &mut |_| {}).expect("ingest");
        store.ingest(Some(&claude), None, &mut |_| {}).expect("again");
        assert_eq!(store.search_messages("zebra", 5).expect("search").len(), 1);
        write_session(&claude, "s1", &[claude_line("s1", "user", "first zebra message"), claude_line("s1", "assistant", "second giraffe reply")]);
        store.ingest(Some(&claude), None, &mut |_| {}).expect("more");
        assert_eq!(store.search_messages("giraffe", 5).expect("search").len(), 1);
    }

    #[test]
    fn secrets_context_and_subagents_stay_out_of_the_index() {
        let (dir, store) = open();
        let claude = dir.path().join(".claude");
        let sidechain = serde_json::json!({"type": "user", "sessionId": "s1", "isSidechain": true, "timestamp": "t", "message": {"content": "hidden walrus"}}).to_string();
        write_session(&claude, "s1", &[claude_line("s1", "user", "deploy penguin with sk-supersecretvalue"), claude_line("s1", "user", "<environment_context>platypus</environment_context>"), sidechain]);
        store.ingest(Some(&claude), None, &mut |_| {}).expect("ingest");
        let hits = store.search_messages("penguin", 5).expect("search");
        assert!(!hits[0].text.contains("supersecret"), "{}", hits[0].text);
        assert!(store.search_messages("platypus", 5).expect("search").is_empty());
        assert!(store.search_messages("walrus", 5).expect("search").is_empty());
    }

    #[test]
    fn codex_rollouts_are_searchable_under_their_session() {
        let (dir, store) = open();
        let codex = dir.path().join(".codex");
        let folder = codex.join("sessions").join("2026");
        std::fs::create_dir_all(&folder).expect("folder");
        let lines = [
            serde_json::json!({"timestamp": "t0", "type": "session_meta", "payload": {"id": "c1", "cwd": "C:/work/web"}}).to_string(),
            serde_json::json!({"timestamp": "t1", "type": "response_item", "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "the capybara route works"}]}}).to_string(),
        ];
        std::fs::write(folder.join("rollout-a.jsonl"), lines.join("\n") + "\n").expect("rollout");
        store.ingest(None, Some(&codex), &mut |_| {}).expect("ingest");
        let hits = store.search_messages("capybara", 5).expect("search");
        assert_eq!((hits[0].vendor.as_str(), hits[0].session.as_str(), hits[0].cwd.as_str()), ("codex", "c1", "C:/work/web"));
    }
}
