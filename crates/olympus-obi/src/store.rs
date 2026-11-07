use crate::capture::{simhash, Kind, Observation, Span};
use anyhow::{anyhow, bail, Result};
use chrono::{Local, NaiveDate, TimeZone, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::Serialize;
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

/// How many days of raw captures to keep unless told otherwise.
pub const DEFAULT_RAW_DAYS: u32 = 14;
// Longer than the 60s sweep, so background windows keep one session between sweeps.
const ATTACH_WINDOW: i64 = 90;
const FOREGROUND_FRESH: i64 = 300;
const DIGEST_ROWS: i64 = 2000;
const MAX_ROWS: i64 = 1000;
const PREVIEW_CHARS: i64 = 200;
const RECENT_MAIL_CHAT: i64 = 10;
const COUNTED_WINDOW: i64 = 3600;

const MIGRATIONS: &[&str] = &[r#"
CREATE TABLE sessions(
    id INTEGER PRIMARY KEY,
    start INTEGER NOT NULL,
    "end" INTEGER NOT NULL,
    exe TEXT NOT NULL,
    title TEXT NOT NULL,
    url TEXT,
    kind TEXT NOT NULL,
    source TEXT,
    foreground INTEGER NOT NULL
);
CREATE INDEX sessions_end ON sessions("end");
CREATE INDEX sessions_identity ON sessions(exe, title, foreground, "end");
CREATE INDEX sessions_start ON sessions(start);
CREATE INDEX sessions_source ON sessions(source);
CREATE TABLE captures(
    id INTEGER PRIMARY KEY,
    session_id INTEGER NOT NULL REFERENCES sessions(id),
    ts INTEGER NOT NULL,
    how TEXT NOT NULL,
    text TEXT NOT NULL,
    simhash INTEGER NOT NULL
);
CREATE INDEX captures_ts ON captures(ts);
CREATE INDEX captures_session ON captures(session_id);
CREATE VIRTUAL TABLE captures_fts USING fts5(
    text, content='captures', content_rowid='id', tokenize='unicode61 remove_diacritics 2'
);
CREATE TRIGGER captures_ai AFTER INSERT ON captures BEGIN
    INSERT INTO captures_fts(rowid, text) VALUES (new.id, new.text);
END;
CREATE TRIGGER captures_ad AFTER DELETE ON captures BEGIN
    INSERT INTO captures_fts(captures_fts, rowid, text) VALUES ('delete', old.id, old.text);
END;
CREATE TRIGGER captures_au AFTER UPDATE OF text ON captures BEGIN
    INSERT INTO captures_fts(captures_fts, rowid, text) VALUES ('delete', old.id, old.text);
    INSERT INTO captures_fts(rowid, text) VALUES (new.id, new.text);
END;
CREATE TABLE processes(
    id INTEGER PRIMARY KEY,
    ts INTEGER NOT NULL,
    exe TEXT NOT NULL,
    change TEXT NOT NULL
);
CREATE INDEX processes_exe ON processes(exe, id);
CREATE TABLE notes(
    id INTEGER PRIMARY KEY,
    start INTEGER NOT NULL,
    "end" INTEGER NOT NULL,
    app TEXT NOT NULL,
    summary TEXT NOT NULL,
    model TEXT NOT NULL
);
CREATE INDEX notes_start ON notes(start);
"#, r#"
CREATE TABLE file_events(
    id INTEGER PRIMARY KEY,
    ts INTEGER NOT NULL,
    path TEXT NOT NULL,
    action TEXT NOT NULL,
    app TEXT
);
CREATE INDEX file_events_ts ON file_events(ts);
CREATE INDEX file_events_path ON file_events(path, ts);
"#];

/// A process appearing or going away.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProcessChange {
    Start,
    Exit,
}

impl ProcessChange {
    fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Exit => "exit",
        }
    }
}

/// One matching capture, or a recent one with the start of its text as the snippet.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Hit {
    pub ts: i64,
    pub exe: String,
    pub title: String,
    pub source: Option<String>,
    pub kind: Kind,
    pub snippet: String,
}

/// A stretch of time spent in one window.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Session {
    pub id: i64,
    pub start: i64,
    pub end: i64,
    pub exe: String,
    pub title: String,
    pub url: Option<String>,
    pub kind: Kind,
    pub source: Option<String>,
}

/// What the owner is doing right now, as far as the store knows.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Now {
    pub foreground: Option<Session>,
    pub running_apps: Vec<String>,
    pub last_hour_by_app: Vec<(String, f64)>,
    pub recent_mail_chat: Vec<Hit>,
}

/// One row of a day's foreground timeline.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TimelineRow {
    pub start: i64,
    pub end: i64,
    pub exe: String,
    pub app: String,
    pub title: String,
    pub kind: Kind,
    pub source: Option<String>,
}

/// How much has been seen from one source.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SourceStat {
    pub source: String,
    pub kind: Kind,
    pub last_seen: i64,
    pub captures: i64,
}

/// A model's summary of a stretch of time.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Note {
    pub id: i64,
    pub start: i64,
    pub end: i64,
    pub app: String,
    pub summary: String,
    pub model: String,
}

/// A file saved or deleted, and the app in front at that moment, which isn't proof that app did it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FileEvent {
    pub ts: i64,
    pub path: String,
    pub action: String,
    pub app: Option<String>,
}

/// Row counts and the size of the database file.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Stats {
    pub sessions: i64,
    pub captures: i64,
    pub processes: i64,
    pub notes: i64,
    pub oldest_capture: Option<i64>,
    pub newest_capture: Option<i64>,
    pub file_bytes: i64,
}

/// The observer's own SQLite file, safe to read from another process while it writes.
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens or creates the database, upgrading its schema.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(folder) = path.parent() {
            std::fs::create_dir_all(folder)?;
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))?;
        conn.execute_batch("PRAGMA synchronous=NORMAL;")?;
        migrate(&conn)?;
        Ok(Self { conn })
    }

    /// Stores a capture under its session, opening a session if none is recent, and returns the capture's id.
    pub fn add_observation(&self, observation: &Observation) -> Result<i64> {
        let tx = self.conn.unchecked_transaction()?;
        let found: Option<i64> = tx
            .query_row(
                r#"SELECT id FROM sessions WHERE exe = ?1 AND title = ?2 AND foreground = ?3 AND "end" >= ?4 ORDER BY "end" DESC, id DESC LIMIT 1"#,
                params![observation.exe, observation.title, observation.foreground, observation.at - ATTACH_WINDOW],
                |row| row.get(0),
            )
            .optional()?;
        let session = match found {
            Some(id) => {
                tx.execute(
                    r#"UPDATE sessions SET "end" = MAX("end", ?2), url = COALESCE(?3, url), kind = ?4, source = COALESCE(?5, source) WHERE id = ?1"#,
                    params![id, observation.at, observation.url, observation.kind.as_str(), observation.source],
                )?;
                id
            }
            None => {
                tx.execute(
                    r#"INSERT INTO sessions(start, "end", exe, title, url, kind, source, foreground) VALUES (?1, ?1, ?2, ?3, ?4, ?5, ?6, ?7)"#,
                    params![observation.at, observation.exe, observation.title, observation.url, observation.kind.as_str(), observation.source, observation.foreground],
                )?;
                tx.last_insert_rowid()
            }
        };
        tx.execute(
            "INSERT INTO captures(session_id, ts, how, text, simhash) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![session, observation.at, observation.how.as_str(), observation.text, simhash(&observation.text) as i64],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(id)
    }

    /// Records a foreground span from the `Sessionizer`; repeating a span only moves its end, and returns its id.
    pub fn session_heartbeat(&self, span: &Span) -> Result<i64> {
        let activity = &span.activity;
        let found: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM sessions WHERE start = ?1 AND exe = ?2 AND title = ?3 AND foreground = 1 LIMIT 1",
                params![span.start, activity.exe, activity.title],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(id) = found {
            self.conn.execute(r#"UPDATE sessions SET "end" = MAX("end", ?2) WHERE id = ?1"#, params![id, span.end])?;
            return Ok(id);
        }
        self.conn.execute(
            r#"INSERT INTO sessions(start, "end", exe, title, url, kind, source, foreground) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1)"#,
            params![span.start, span.end, activity.exe, activity.title, activity.url, activity.kind.as_str(), activity.source],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Every program a window was ever seen in.
    pub fn seen_exes(&self) -> Result<Vec<String>> {
        let mut statement = self.conn.prepare("SELECT DISTINCT exe FROM sessions ORDER BY exe")?;
        let rows = statement.query_map([], |row| row.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// The named sites and web apps seen inside a browser.
    pub fn web_sources(&self) -> Result<Vec<String>> {
        let mut statement = self.conn.prepare("SELECT DISTINCT source, exe FROM sessions WHERE source IS NOT NULL ORDER BY source")?;
        let rows: Vec<(String, String)> = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        let mut names: Vec<String> = rows.into_iter().filter(|(_, exe)| crate::capture::is_browser(exe)).map(|(source, _)| source).collect();
        names.dedup();
        Ok(names)
    }

    /// Records a file changing; saving the same file again within a minute only moves the time.
    pub fn add_file_event(&self, ts: i64, path: &str, action: &str, app: Option<&str>) -> Result<()> {
        let updated = self.conn.execute(
            "UPDATE file_events SET ts = ?1, action = ?3, app = COALESCE(?4, app) WHERE id = (SELECT MAX(id) FROM file_events WHERE path = ?2 AND ts >= ?1 - 60)",
            params![ts, path, action, app],
        )?;
        if updated == 0 {
            self.conn.execute("INSERT INTO file_events(ts, path, action, app) VALUES (?1, ?2, ?3, ?4)", params![ts, path, action, app])?;
        }
        Ok(())
    }

    /// The files that changed in a stretch of time, oldest first.
    pub fn file_events(&self, start: i64, end: i64) -> Result<Vec<FileEvent>> {
        let mut statement = self.conn.prepare("SELECT ts, path, action, app FROM file_events WHERE ts >= ?1 AND ts < ?2 ORDER BY ts, id")?;
        let rows = statement.query_map(params![start, end], |row| Ok(FileEvent { ts: row.get(0)?, path: row.get(1)?, action: row.get(2)?, app: row.get(3)? }))?.collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// Records a process starting or exiting.
    pub fn add_process_change(&self, ts: i64, exe: &str, change: ProcessChange) -> Result<()> {
        self.conn.execute("INSERT INTO processes(ts, exe, change) VALUES (?1, ?2, ?3)", params![ts, exe, change.as_str()])?;
        Ok(())
    }

    /// Exes whose latest recorded change is a start, by name.
    pub fn running_apps(&self) -> Result<Vec<String>> {
        let mut statement = self.conn.prepare("SELECT exe FROM processes WHERE id IN (SELECT MAX(id) FROM processes GROUP BY exe) AND change = 'start' ORDER BY exe")?;
        let rows = statement.query_map([], |row| row.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// Full-text search, best match first; `query` is plain words, never FTS syntax.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>> {
        let Some(query) = fts_query(query) else { return Ok(Vec::new()) };
        let limit = row_limit(limit);
        let mut statement = self.conn.prepare(
            "SELECT c.ts, s.exe, s.title, s.source, s.kind, snippet(captures_fts, 0, '', '', '...', 24) \
             FROM captures_fts JOIN captures c ON c.id = captures_fts.rowid JOIN sessions s ON s.id = c.session_id \
             WHERE captures_fts MATCH ?1 ORDER BY bm25(captures_fts), c.ts DESC LIMIT ?2",
        )?;
        // Near-identical captures of one page would otherwise fill the whole result.
        let mut seen = HashSet::new();
        let mut hits = Vec::new();
        for hit in statement.query_map(params![query, limit * 4], hit_from_row)? {
            let hit = hit?;
            if seen.insert((hit.title.clone(), hit.snippet.clone())) {
                hits.push(hit);
                if hits.len() as i64 == limit {
                    break;
                }
            }
        }
        Ok(hits)
    }

    /// Captures since a moment, newest first.
    pub fn recent(&self, since_ts: i64, limit: usize) -> Result<Vec<Hit>> {
        let mut statement = self.conn.prepare(
            "SELECT c.ts, s.exe, s.title, s.source, s.kind, substr(c.text, 1, ?3) \
             FROM captures c JOIN sessions s ON s.id = c.session_id WHERE c.ts >= ?1 ORDER BY c.ts DESC, c.id DESC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![since_ts, row_limit(limit), PREVIEW_CHARS], hit_from_row)?.collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// The latest captures in a stretch of time, oldest first, with up to `max_chars` of each one's text as the snippet.
    pub fn captures_between(&self, start: i64, end: i64, limit: usize, max_chars: usize) -> Result<Vec<Hit>> {
        let mut statement = self.conn.prepare(
            "SELECT c.ts, s.exe, s.title, s.source, s.kind, substr(c.text, 1, ?4) \
             FROM captures c JOIN sessions s ON s.id = c.session_id WHERE c.ts >= ?1 AND c.ts < ?2 ORDER BY c.ts DESC, c.id DESC LIMIT ?3",
        )?;
        let mut rows: Vec<Hit> = statement.query_map(params![start, end, row_limit(limit), max_chars as i64], hit_from_row)?.collect::<rusqlite::Result<_>>()?;
        rows.reverse();
        Ok(rows)
    }

    /// The owner's current situation, using the current time.
    pub fn now(&self) -> Result<Now> {
        self.now_at(Utc::now().timestamp())
    }

    /// The owner's situation as of a given moment.
    pub fn now_at(&self, at: i64) -> Result<Now> {
        let foreground = self
            .conn
            .query_row(
                r#"SELECT id, start, "end", exe, title, url, kind, source FROM sessions WHERE foreground = 1 AND "end" >= ?1 AND start <= ?2 ORDER BY "end" DESC, id DESC LIMIT 1"#,
                params![at - FOREGROUND_FRESH, at],
                session_from_row,
            )
            .optional()?;
        let mut hours = self.conn.prepare(
            r#"SELECT COALESCE(source, exe), SUM(MIN("end", ?1) - MAX(start, ?1 - ?2)) / 60.0 FROM sessions
               WHERE foreground = 1 AND "end" > ?1 - ?2 AND start < ?1 GROUP BY 1 ORDER BY 2 DESC"#,
        )?;
        let last_hour_by_app = hours.query_map(params![at, COUNTED_WINDOW], |row| Ok((row.get(0)?, row.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        // Bare columns take their values from the MAX(c.ts) row, so each session shows its newest capture.
        let mut mail_chat = self.conn.prepare(
            "SELECT MAX(c.ts), s.exe, s.title, s.source, s.kind, substr(c.text, 1, ?3) \
             FROM captures c JOIN sessions s ON s.id = c.session_id \
             WHERE s.kind IN ('mail', 'chat') AND c.ts <= ?1 GROUP BY c.session_id ORDER BY 1 DESC LIMIT ?2",
        )?;
        let recent_mail_chat = mail_chat.query_map(params![at, RECENT_MAIL_CHAT, PREVIEW_CHARS], hit_from_row)?.collect::<rusqlite::Result<_>>()?;
        Ok(Now { foreground, running_apps: self.running_apps()?, last_hour_by_app, recent_mail_chat })
    }

    /// A plain-text digest by source then app, newest first with repeated lines removed, at most `max_chars` long.
    pub fn digest(&self, start: i64, end: i64, max_chars: usize) -> Result<String> {
        self.digest_where(start, end, max_chars, &|_| true)
    }

    /// `digest` of only the captures `keep` accepts.
    pub fn digest_where(&self, start: i64, end: i64, max_chars: usize, keep: &dyn Fn(&Hit) -> bool) -> Result<String> {
        let mut statement = self.conn.prepare(
            "SELECT c.ts, s.exe, s.title, s.source, s.kind, c.text FROM captures c JOIN sessions s ON s.id = c.session_id \
             WHERE c.ts >= ?1 AND c.ts <= ?2 ORDER BY c.ts DESC, c.id DESC LIMIT ?3",
        )?;
        let mut groups: Vec<DigestGroup> = Vec::new();
        for row in statement.query_map(params![start, end, DIGEST_ROWS], hit_from_row)? {
            let hit = row?;
            if !keep(&hit) {
                continue;
            }
            let label = hit.source.clone().unwrap_or_else(|| hit.exe.clone());
            let sourced = hit.source.is_some();
            match groups.iter_mut().find(|group| group.label == label && group.sourced == sourced) {
                Some(group) => group.captures.push(hit),
                None => groups.push(DigestGroup { label, sourced, kind: hit.kind, captures: vec![hit] }),
            }
        }
        // A stable sort keeps newest-first within sources and within apps.
        groups.sort_by_key(|group| !group.sourced);
        Ok(render_digest(&groups, max_chars))
    }

    /// The foreground sessions overlapping a local calendar day, clipped to it, oldest first.
    pub fn day_timeline(&self, date: NaiveDate) -> Result<Vec<TimelineRow>> {
        let next = date.succ_opt().ok_or_else(|| anyhow!("date out of range"))?;
        let (start, end) = (local_midnight(date)?, local_midnight(next)?);
        let mut statement = self.conn.prepare(
            r#"SELECT MAX(start, ?1), MIN("end", ?2), COALESCE(source, exe), title, kind, source, exe FROM sessions
               WHERE foreground = 1 AND "end" >= ?1 AND start < ?2 ORDER BY start, id"#,
        )?;
        let rows = statement
            .query_map(params![start, end], |row| {
                Ok(TimelineRow { start: row.get(0)?, end: row.get(1)?, exe: row.get(6)?, app: row.get(2)?, title: row.get(3)?, kind: Kind::parse(&row.get::<_, String>(4)?), source: row.get(5)? })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// Every source seen, or its program when it has no label, most recent first.
    pub fn sources(&self) -> Result<Vec<SourceStat>> {
        let mut statement = self.conn.prepare(
            r#"SELECT COALESCE(s.source, s.exe), s.kind, MAX(s."end"), COUNT(c.id) FROM sessions s LEFT JOIN captures c ON c.session_id = s.id
               GROUP BY COALESCE(s.source, s.exe), s.kind ORDER BY 3 DESC"#,
        )?;
        let rows = statement
            .query_map([], |row| Ok(SourceStat { source: row.get(0)?, kind: Kind::parse(&row.get::<_, String>(1)?), last_seen: row.get(2)?, captures: row.get(3)? }))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// Saves a summary and returns its id.
    pub fn add_note(&self, start: i64, end: i64, app: &str, summary: &str, model: &str) -> Result<i64> {
        self.conn.execute(r#"INSERT INTO notes(start, "end", app, summary, model) VALUES (?1, ?2, ?3, ?4, ?5)"#, params![start, end, app, summary, model])?;
        Ok(self.conn.last_insert_rowid())
    }

    /// The summaries overlapping a stretch of time, oldest first.
    pub fn notes_between(&self, start: i64, end: i64) -> Result<Vec<Note>> {
        let mut statement = self.conn.prepare(r#"SELECT id, start, "end", app, summary, model FROM notes WHERE "end" >= ?1 AND start <= ?2 ORDER BY start, id"#)?;
        let rows = statement
            .query_map(params![start, end], |row| Ok(Note { id: row.get(0)?, start: row.get(1)?, end: row.get(2)?, app: row.get(3)?, summary: row.get(4)?, model: row.get(5)? }))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// Deletes raw captures and process history older than `raw_days`, keeping sessions and notes, and returns the captures removed.
    pub fn prune(&self, raw_days: u32) -> Result<usize> {
        self.prune_at(raw_days, Utc::now().timestamp())
    }

    /// `prune` as of a given moment.
    pub fn prune_at(&self, raw_days: u32, at: i64) -> Result<usize> {
        let cutoff = at - i64::from(raw_days) * 86_400;
        let tx = self.conn.unchecked_transaction()?;
        let removed = tx.execute("DELETE FROM captures WHERE ts < ?1", params![cutoff])?;
        tx.execute("DELETE FROM file_events WHERE ts < ?1", params![cutoff])?;
        // Each exe's latest row stays, because `running_apps` is worked out from it.
        tx.execute("DELETE FROM processes WHERE ts < ?1 AND id NOT IN (SELECT MAX(id) FROM processes GROUP BY exe)", params![cutoff])?;
        tx.commit()?;
        Ok(removed)
    }

    /// Row counts and file size.
    pub fn stats(&self) -> Result<Stats> {
        let count = |table: &str| -> rusqlite::Result<i64> { self.conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0)) };
        let (oldest_capture, newest_capture) = self.conn.query_row("SELECT MIN(ts), MAX(ts) FROM captures", [], |row| Ok((row.get(0)?, row.get(1)?)))?;
        let file_bytes = self.conn.query_row("SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()", [], |row| row.get(0))?;
        Ok(Stats { sessions: count("sessions")?, captures: count("captures")?, processes: count("processes")?, notes: count("notes")?, oldest_capture, newest_capture, file_bytes })
    }
}

struct DigestGroup {
    label: String,
    sourced: bool,
    kind: Kind,
    captures: Vec<Hit>,
}

fn migrate(conn: &Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let version = version as usize;
    if version > MIGRATIONS.len() {
        bail!("observer store schema version {version} is newer than supported {}", MIGRATIONS.len());
    }
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(version) {
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(sql)?;
        tx.execute_batch(&format!("PRAGMA user_version={}", index + 1))?;
        tx.commit()?;
    }
    Ok(())
}

fn row_limit(limit: usize) -> i64 {
    (limit as i64).clamp(0, MAX_ROWS)
}

fn hit_from_row(row: &Row) -> rusqlite::Result<Hit> {
    Ok(Hit { ts: row.get(0)?, exe: row.get(1)?, title: row.get(2)?, source: row.get(3)?, kind: Kind::parse(&row.get::<_, String>(4)?), snippet: row.get(5)? })
}

fn session_from_row(row: &Row) -> rusqlite::Result<Session> {
    Ok(Session { id: row.get(0)?, start: row.get(1)?, end: row.get(2)?, exe: row.get(3)?, title: row.get(4)?, url: row.get(5)?, kind: Kind::parse(&row.get::<_, String>(6)?), source: row.get(7)? })
}

fn local_midnight(date: NaiveDate) -> Result<i64> {
    let midnight = date.and_hms_opt(0, 0, 0).ok_or_else(|| anyhow!("no midnight on {date}"))?;
    // A clock change can skip midnight, so an hour later is the fallback.
    let moment = Local.from_local_datetime(&midnight).earliest().or_else(|| Local.from_local_datetime(&(midnight + chrono::Duration::hours(1))).earliest());
    Ok(moment.ok_or_else(|| anyhow!("no local midnight on {date}"))?.timestamp())
}

// Quoting every word turns FTS operators into plain text; words with no letters or digits match nothing.
fn fts_query(text: &str) -> Option<String> {
    let terms: Vec<String> = text
        .split_whitespace()
        .filter(|word| word.chars().any(char::is_alphanumeric))
        .map(|word| {
            let clean: String = word.chars().filter(|c| !c.is_control()).collect();
            format!("\"{}\"", clean.replace('"', "\"\""))
        })
        .collect();
    (!terms.is_empty()).then(|| terms.join(" "))
}

fn local_clock(ts: i64) -> String {
    Local.timestamp_opt(ts, 0).earliest().map_or_else(|| "--:--".to_string(), |moment| moment.format("%H:%M").to_string())
}

fn render_digest(groups: &[DigestGroup], max_chars: usize) -> String {
    let mut out = String::new();
    let mut left = max_chars;
    let mut seen = HashSet::new();
    for (index, group) in groups.iter().enumerate() {
        // Each group gets a fair share, and what one leaves unused passes to the next.
        let share = left / (groups.len() - index);
        let part = render_group(group, share, &mut seen);
        left -= part.chars().count();
        out.push_str(&part);
    }
    out
}

fn render_group(group: &DigestGroup, share: usize, seen: &mut HashSet<String>) -> String {
    let mut body = String::new();
    let heading = format!("## {} ({})\n", group.label, group.kind.as_str());
    let mut left = share.saturating_sub(heading.chars().count());
    let mut last_title: Option<&str> = None;
    'captures: for capture in &group.captures {
        for line in capture.snippet.lines().map(str::trim).filter(|line| !line.is_empty()) {
            if !seen.insert(line.to_string()) {
                continue;
            }
            if last_title != Some(capture.title.as_str()) {
                let header = format!("[{}] {}\n", local_clock(capture.ts), capture.title);
                if !push_within(&mut body, &header, &mut left) {
                    break 'captures;
                }
                last_title = Some(&capture.title);
            }
            if !push_within(&mut body, &format!("{line}\n"), &mut left) {
                break 'captures;
            }
        }
    }
    if body.is_empty() {
        return String::new();
    }
    heading + &body
}

// Adds as much as fits and returns false once the budget has run out.
fn push_within(out: &mut String, text: &str, left: &mut usize) -> bool {
    let length = text.chars().count();
    if length <= *left {
        out.push_str(text);
        *left -= length;
        return true;
    }
    out.extend(text.chars().take(*left));
    *left = 0;
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{Activity, How};
    use tempfile::TempDir;

    fn store() -> (TempDir, Store) {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = Store::open(&dir.path().join("deep").join("obi.db")).expect("open");
        (dir, store)
    }

    fn observation(at: i64, text: &str) -> Observation {
        Observation { at, exe: "ms-teams".into(), title: "Chat | Microsoft Teams".into(), url: None, kind: Kind::Chat, source: Some("Teams".into()), text: text.into(), how: How::Uia, foreground: true }
    }

    fn span(start: i64, end: i64, exe: &str, title: &str) -> Span {
        Span { start, end, activity: Activity { exe: exe.into(), title: title.into(), url: None, kind: Kind::Other, source: None } }
    }

    #[test]
    fn open_sets_schema_version_and_wal() {
        let (_dir, store) = store();
        let version: i64 = store.conn.query_row("PRAGMA user_version", [], |row| row.get(0)).expect("version");
        let mode: String = store.conn.query_row("PRAGMA journal_mode", [], |row| row.get(0)).expect("mode");
        assert_eq!(version, MIGRATIONS.len() as i64);
        assert_eq!(mode, "wal");
    }

    #[test]
    fn open_refuses_a_newer_schema() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("obi.db");
        Store::open(&path).expect("open").conn.execute_batch("PRAGMA user_version=99").expect("bump");
        assert!(Store::open(&path).is_err());
    }

    #[test]
    fn search_finds_stored_text_with_its_session() {
        let (_dir, store) = store();
        store.add_observation(&observation(100, "the quarterly budget meeting moved")).expect("add");
        let hits = store.search("budget", 10).expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].source.as_deref(), Some("Teams"));
        assert_eq!(hits[0].kind, Kind::Chat);
        assert!(hits[0].snippet.contains("budget"));
    }

    #[test]
    fn search_finds_words_regardless_of_diacritics() {
        let (_dir, store) = store();
        store.add_observation(&observation(100, "mötet flyttades")).expect("add");
        assert_eq!(store.search("motet", 10).expect("search").len(), 1);
    }

    #[test]
    fn search_treats_fts_syntax_as_plain_text() {
        let (_dir, store) = store();
        store.add_observation(&observation(100, "alpha beta gamma")).expect("add");
        for odd in ["\"", "alpha\"", "alpha OR", "NEAR(alpha beta)", "text:alpha", "alpha*", "^alpha", "-alpha", "(", "*", "a\0b", "AND", "'; DROP TABLE captures; --", "🙂"] {
            store.search(odd, 10).unwrap_or_else(|error| panic!("{odd:?}: {error}"));
        }
        assert!(store.search("zzz OR alpha", 10).expect("search").is_empty(), "OR must not act as an operator");
        assert!(store.search("alpha NOT beta", 10).expect("search").is_empty(), "NOT must not act as an operator");
        assert_eq!(store.stats().expect("stats").captures, 1);
    }

    #[test]
    fn search_of_nothing_returns_nothing() {
        let (_dir, store) = store();
        assert!(store.search("   ", 10).expect("search").is_empty());
        assert!(store.search("--- ***", 10).expect("search").is_empty());
    }

    #[test]
    fn search_drops_identical_results() {
        let (_dir, store) = store();
        store.add_observation(&observation(100, "same words here")).expect("add");
        store.add_observation(&observation(101, "same words here")).expect("add");
        assert_eq!(store.search("words", 10).expect("search").len(), 1);
    }

    #[test]
    fn observations_join_a_recent_session_and_open_a_new_one_after_a_gap() {
        let (_dir, store) = store();
        store.add_observation(&observation(100, "one")).expect("add");
        store.add_observation(&observation(150, "two")).expect("add");
        assert_eq!(store.stats().expect("stats").sessions, 1);
        store.add_observation(&observation(150 + ATTACH_WINDOW + 1, "three")).expect("add");
        assert_eq!(store.stats().expect("stats").sessions, 2);
    }

    #[test]
    fn background_and_foreground_never_share_a_session() {
        let (_dir, store) = store();
        store.add_observation(&observation(100, "one")).expect("add");
        store.add_observation(&Observation { foreground: false, ..observation(101, "two") }).expect("add");
        assert_eq!(store.stats().expect("stats").sessions, 2);
    }

    #[test]
    fn heartbeat_extends_one_session_without_duplicating() {
        let (_dir, store) = store();
        let first = store.session_heartbeat(&span(100, 100, "code", "main.rs")).expect("beat");
        let second = store.session_heartbeat(&span(100, 103, "code", "main.rs")).expect("beat");
        assert_eq!(first, second);
        let now = store.now_at(103).expect("now");
        assert_eq!(now.foreground.expect("foreground").end, 103);
    }

    #[test]
    fn observation_adds_its_url_and_label_to_the_heartbeat_session() {
        let (_dir, store) = store();
        store.session_heartbeat(&span(100, 100, "msedge", "Inbox - Gmail")).expect("beat");
        store
            .add_observation(&Observation { exe: "msedge".into(), title: "Inbox - Gmail".into(), url: Some("https://mail.google.com/".into()), kind: Kind::Mail, source: Some("Gmail".into()), ..observation(101, "hello") })
            .expect("add");
        let session = store.now_at(101).expect("now").foreground.expect("foreground");
        assert_eq!(session.url.as_deref(), Some("https://mail.google.com/"));
        assert_eq!(session.source.as_deref(), Some("Gmail"));
        assert_eq!(session.kind, Kind::Mail);
        assert_eq!(store.stats().expect("stats").sessions, 1);
    }

    #[test]
    fn prune_removes_old_captures_from_search_but_keeps_sessions_and_notes() {
        let (_dir, store) = store();
        let today = 100 * 86_400;
        store.add_observation(&observation(today - 20 * 86_400, "ancient words")).expect("add");
        store.add_observation(&observation(today, "fresh words")).expect("add");
        store.add_note(1, 2, "Teams", "summary", "model").expect("note");
        let removed = store.prune_at(DEFAULT_RAW_DAYS, today).expect("prune");
        assert_eq!(removed, 1);
        assert!(store.search("ancient", 10).expect("search").is_empty());
        assert_eq!(store.search("fresh", 10).expect("search").len(), 1);
        let stats = store.stats().expect("stats");
        assert_eq!((stats.sessions, stats.captures, stats.notes), (2, 1, 1));
    }

    #[test]
    fn prune_keeps_a_long_running_app_listed_as_running() {
        let (_dir, store) = store();
        store.add_process_change(10, "code", ProcessChange::Start).expect("change");
        store.prune_at(1, 100 * 86_400).expect("prune");
        assert_eq!(store.running_apps().expect("running"), ["code"]);
    }

    #[test]
    fn running_apps_follow_the_latest_change() {
        let (_dir, store) = store();
        store.add_process_change(1, "code", ProcessChange::Start).expect("change");
        store.add_process_change(2, "slack", ProcessChange::Start).expect("change");
        store.add_process_change(3, "code", ProcessChange::Exit).expect("change");
        assert_eq!(store.running_apps().expect("running"), ["slack"]);
    }

    #[test]
    fn now_counts_only_the_foreground_minutes_inside_the_last_hour() {
        let (_dir, store) = store();
        let at = 100_000;
        store.session_heartbeat(&span(at - 7200, at - 3000, "code", "a")).expect("beat");
        store.add_observation(&Observation { foreground: false, ..observation(at - 100, "behind") }).expect("add");
        let now = store.now_at(at).expect("now");
        assert_eq!(now.last_hour_by_app, [("code".to_string(), 10.0)]);
    }

    #[test]
    fn now_has_no_foreground_once_it_has_gone_stale() {
        let (_dir, store) = store();
        store.session_heartbeat(&span(100, 100, "code", "a")).expect("beat");
        assert!(store.now_at(100 + FOREGROUND_FRESH + 1).expect("now").foreground.is_none());
    }

    #[test]
    fn now_lists_the_newest_capture_per_mail_or_chat_session() {
        let (_dir, store) = store();
        store.add_observation(&observation(100, "older")).expect("add");
        store.add_observation(&observation(101, "newer")).expect("add");
        store.add_observation(&Observation { kind: Kind::Code, exe: "code".into(), source: None, ..observation(102, "unrelated") }).expect("add");
        let hits = store.now_at(200).expect("now").recent_mail_chat;
        assert_eq!(hits.len(), 1);
        assert_eq!((hits[0].ts, hits[0].snippet.as_str()), (101, "newer"));
    }

    #[test]
    fn recent_is_newest_first_and_respects_since() {
        let (_dir, store) = store();
        store.add_observation(&observation(100, "first")).expect("add");
        store.add_observation(&observation(110, "second")).expect("add");
        store.add_observation(&observation(120, "third")).expect("add");
        let hits = store.recent(110, 10).expect("recent");
        let order: Vec<&str> = hits.iter().map(|hit| hit.snippet.as_str()).collect();
        assert_eq!(order, ["third", "second"]);
    }

    #[test]
    fn digest_puts_sources_before_apps_and_removes_repeats() {
        let (_dir, store) = store();
        store.add_observation(&Observation { exe: "code".into(), title: "main.rs".into(), kind: Kind::Code, source: None, ..observation(130, "fn main") }).expect("add");
        store.add_observation(&observation(100, "hello there\nsecond line")).expect("add");
        store.add_observation(&observation(110, "hello there\nthird line")).expect("add");
        let digest = store.digest(0, 1000, 10_000).expect("digest");
        let (teams, code) = (digest.find("## Teams (chat)").expect("teams"), digest.find("## code (code)").expect("code"));
        assert!(teams < code);
        assert_eq!(digest.matches("hello there").count(), 1);
        assert!(digest.find("third line").expect("third") < digest.find("second line").expect("second"));
    }

    #[test]
    fn digest_never_passes_the_limit_and_shares_it_between_groups() {
        let (_dir, store) = store();
        for line in 0..200 {
            store.add_observation(&observation(100 + line, &format!("teams line number {line} with several words"))).expect("add");
        }
        store.add_observation(&Observation { exe: "code".into(), title: "main.rs".into(), kind: Kind::Code, source: None, ..observation(50, "the code line") }).expect("add");
        let digest = store.digest(0, 1000, 400).expect("digest");
        assert!(digest.chars().count() <= 400, "{}", digest.chars().count());
        assert!(digest.contains("the code line"), "a busy source must not crowd out the rest");
    }

    #[test]
    fn digest_of_an_empty_range_is_empty() {
        let (_dir, store) = store();
        assert_eq!(store.digest(0, 1000, 100).expect("digest"), "");
    }

    #[test]
    fn timeline_clips_foreground_sessions_to_the_day() {
        let (_dir, store) = store();
        let date = NaiveDate::from_ymd_opt(2026, 3, 10).expect("date");
        let start = local_midnight(date).expect("midnight");
        store.session_heartbeat(&span(start - 60, start + 60, "code", "overnight")).expect("beat");
        store.session_heartbeat(&span(start - 500, start - 400, "code", "yesterday")).expect("beat");
        store.add_observation(&Observation { foreground: false, ..observation(start + 10, "behind") }).expect("add");
        let rows = store.day_timeline(date).expect("timeline");
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].start, rows[0].end, rows[0].title.as_str()), (start, start + 60, "overnight"));
    }

    #[test]
    fn a_digest_can_leave_messages_out_and_a_file_saved_twice_is_one_event() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(&dir.path().join("o.db")).expect("open");
        let mut observation = observation(100, "x");
        observation.kind = Kind::Chat;
        observation.text = "the private chat line".into();
        store.add_observation(&observation).expect("chat");
        observation.kind = Kind::Browser;
        observation.title = "Docs".into();
        observation.text = "the public page line".into();
        store.add_observation(&observation).expect("page");
        let digest = store.digest_where(0, 1000, 10_000, &|hit| hit.kind != Kind::Chat).expect("digest");
        assert!(digest.contains("the public page line") && !digest.contains("the private chat line"), "{digest}");
        store.add_file_event(100, "a.txt", "saved", Some("code")).expect("first");
        store.add_file_event(150, "a.txt", "deleted", None).expect("second");
        store.add_file_event(400, "a.txt", "saved", None).expect("later");
        let events = store.file_events(0, 1000).expect("events");
        assert_eq!(events.iter().map(|event| (event.ts, event.action.as_str(), event.app.as_deref())).collect::<Vec<_>>(), [(150, "deleted", Some("code")), (400, "saved", None)]);
    }

    #[test]
    fn sources_count_captures_by_label_or_else_by_program() {
        let (_dir, store) = store();
        store.add_observation(&observation(100, "one")).expect("add");
        store.add_observation(&observation(101, "two")).expect("add");
        store.session_heartbeat(&span(90, 200, "code", "a")).expect("beat");
        let sources = store.sources().expect("sources");
        assert_eq!(sources.iter().find(|source| source.source == "Teams"), Some(&SourceStat { source: "Teams".into(), kind: Kind::Chat, last_seen: 101, captures: 2 }));
        assert!(sources.iter().any(|source| source.source == "code"), "a program with no label still shows up");
    }

    #[test]
    fn notes_are_found_by_overlap() {
        let (_dir, store) = store();
        let id = store.add_note(100, 200, "Teams", "a summary", "local").expect("note");
        assert_eq!(store.notes_between(150, 160).expect("notes")[0].id, id);
        assert!(store.notes_between(201, 300).expect("notes").is_empty());
    }

    #[test]
    fn a_second_connection_reads_while_the_first_holds_a_write() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("obi.db");
        let writer = Store::open(&path).expect("open writer");
        writer.add_observation(&observation(100, "already committed")).expect("add");
        let open_write = writer.conn.unchecked_transaction().expect("transaction");
        writer.conn.execute("INSERT INTO notes(start, \"end\", app, summary, model) VALUES (1, 2, 'a', 'b', 'c')", []).expect("insert");
        let reader = Store::open(&path).expect("open reader");
        assert_eq!(reader.search("committed", 10).expect("search").len(), 1);
        assert_eq!(reader.stats().expect("stats").notes, 0);
        drop(open_write);
    }
}
