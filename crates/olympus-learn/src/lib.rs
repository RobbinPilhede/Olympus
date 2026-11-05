//! Self-learning store for Olympus: transcript signals, gated lessons and model routing.

mod classify;
mod ingest;
mod lessons;
mod math;
mod report;
mod router;
mod schema;
mod text;
mod types;

use std::cell::Cell;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::Connection;

pub use classify::classify_task;
pub use text::is_correction;
pub use math::{fnv1a64, weighted_tokens};
pub use types::{
    HistoryHit, HoldoutReport, IngestProgress, IngestStats, Injection, LearnStats, Lesson,
    LessonKind, LessonState, ModelTokens, NewLesson, RateLimit, RunRecord, SignalCount,
};

/// SQLite-backed learning store in WAL mode.
pub struct Store {
    conn: Connection,
    rng: Cell<u64>,
}

impl Store {
    /// Opens or creates the store at `path`, applying schema migrations.
    pub fn open(path: &Path) -> Result<Store> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))?;
        conn.execute_batch("PRAGMA synchronous=NORMAL;")?;
        schema::migrate(&conn)?;
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(1, |d| d.as_nanos() as u64);
        let seed = fnv1a64(&nanos.to_le_bytes()) ^ u64::from(std::process::id());
        Ok(Store {
            conn,
            // Xorshift has a fixed point at zero, so the seed must stay non-zero.
            rng: Cell::new(seed | 1),
        })
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}
