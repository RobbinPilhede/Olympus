use anyhow::{bail, Result};
use rusqlite::Connection;

const MIGRATIONS: &[&str] = &[r#"
CREATE TABLE files(
    path TEXT PRIMARY KEY,
    byte_offset INTEGER NOT NULL,
    mtime INTEGER NOT NULL,
    size INTEGER NOT NULL,
    state TEXT NOT NULL
) WITHOUT ROWID;

CREATE TABLE prompts(
    id INTEGER PRIMARY KEY,
    session TEXT NOT NULL,
    cwd TEXT NOT NULL,
    timestamp TEXT NOT NULL,
    text TEXT NOT NULL
);
CREATE VIRTUAL TABLE prompts_fts USING fts5(
    text, content='prompts', content_rowid='id', tokenize='porter unicode61'
);

CREATE TABLE signals(
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    session TEXT NOT NULL,
    cwd TEXT NOT NULL,
    timestamp TEXT NOT NULL,
    sig_key TEXT NOT NULL,
    detail TEXT NOT NULL
);
CREATE INDEX signals_kind_key ON signals(kind, sig_key);

CREATE TABLE usage(
    id INTEGER PRIMARY KEY,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    input INTEGER NOT NULL,
    cache_read INTEGER NOT NULL,
    cache_write INTEGER NOT NULL,
    output INTEGER NOT NULL
);

CREATE TABLE rate_limits(
    provider TEXT PRIMARY KEY,
    used_percent REAL NOT NULL,
    window_minutes INTEGER NOT NULL,
    resets_at INTEGER NOT NULL,
    timestamp TEXT NOT NULL
) WITHOUT ROWID;

CREATE TABLE lessons(
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    text TEXT NOT NULL,
    kind TEXT NOT NULL,
    scope TEXT,
    provenance_file TEXT,
    provenance_hash INTEGER,
    state TEXT NOT NULL,
    alpha REAL NOT NULL DEFAULT 1,
    beta REAL NOT NULL DEFAULT 1,
    holdout_fail INTEGER NOT NULL DEFAULT 0,
    holdout_ok INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    confirmed_at INTEGER NOT NULL,
    dedupe_key TEXT NOT NULL UNIQUE
);
CREATE INDEX lessons_state ON lessons(state);
CREATE VIRTUAL TABLE lessons_fts USING fts5(text, tokenize='porter unicode61');

CREATE TABLE injections(
    run_id TEXT PRIMARY KEY,
    holdout INTEGER NOT NULL,
    eligible INTEGER NOT NULL,
    tokens INTEGER NOT NULL,
    created_at INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TABLE injection_lessons(
    run_id TEXT NOT NULL,
    lesson_id INTEGER NOT NULL,
    applied INTEGER NOT NULL,
    PRIMARY KEY(run_id, lesson_id)
) WITHOUT ROWID;

CREATE TABLE candidate_evidence(
    run_id TEXT NOT NULL,
    lesson_id INTEGER NOT NULL,
    PRIMARY KEY(run_id, lesson_id)
) WITHOUT ROWID;

CREATE TABLE runs(
    run_id TEXT PRIMARY KEY,
    task_class TEXT NOT NULL,
    repo TEXT NOT NULL,
    model TEXT NOT NULL,
    provider TEXT NOT NULL,
    input_tokens INTEGER NOT NULL,
    cache_read INTEGER NOT NULL,
    cache_write INTEGER NOT NULL,
    output_tokens INTEGER NOT NULL,
    outcome INTEGER,
    corrected INTEGER NOT NULL,
    outcome_applied INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TABLE route_stats(
    task_class TEXT NOT NULL,
    repo TEXT NOT NULL,
    model TEXT NOT NULL,
    alpha REAL NOT NULL,
    beta REAL NOT NULL,
    PRIMARY KEY(task_class, repo, model)
) WITHOUT ROWID;
"#, r#"
CREATE TABLE messages(
    id INTEGER PRIMARY KEY,
    vendor TEXT NOT NULL,
    session TEXT NOT NULL,
    cwd TEXT NOT NULL,
    timestamp TEXT NOT NULL,
    role TEXT NOT NULL,
    text TEXT NOT NULL
);
CREATE VIRTUAL TABLE messages_fts USING fts5(
    text, content='messages', content_rowid='id', tokenize='porter unicode61'
);
CREATE TABLE message_files(
    path TEXT PRIMARY KEY,
    byte_offset INTEGER NOT NULL,
    size INTEGER NOT NULL,
    session TEXT NOT NULL,
    cwd TEXT NOT NULL
) WITHOUT ROWID;
"#];

pub(crate) fn migrate(conn: &Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let version = version as usize;
    if version > MIGRATIONS.len() {
        bail!(
            "store schema version {version} is newer than supported {}",
            MIGRATIONS.len()
        );
    }
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(version) {
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(sql)?;
        tx.execute_batch(&format!("PRAGMA user_version={}", index + 1))?;
        tx.commit()?;
    }
    Ok(())
}
