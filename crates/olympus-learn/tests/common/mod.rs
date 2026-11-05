#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};

use olympus_learn::{LessonKind, LessonState, NewLesson, Store};
use tempfile::TempDir;

pub struct Fixture {
    pub dir: TempDir,
    pub store: Store,
}

pub fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(&dir.path().join("learn.db")).expect("open store");
    Fixture { dir, store }
}

impl Fixture {
    pub fn db_path(&self) -> PathBuf {
        self.dir.path().join("learn.db")
    }

    pub fn raw(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.db_path()).expect("second connection")
    }

    pub fn age_lesson(&self, id: i64, days: i64) {
        self.raw()
            .execute(
                "UPDATE lessons SET confirmed_at = confirmed_at - ?1 * 86400 WHERE id = ?2",
                rusqlite::params![days, id],
            )
            .expect("age lesson");
    }

    pub fn ingest_claude(&self, root: &Path) -> olympus_learn::IngestStats {
        self.store
            .ingest(Some(root), None, &mut |_| {})
            .expect("ingest")
    }

    pub fn ingest_codex(&self, root: &Path) -> olympus_learn::IngestStats {
        self.store
            .ingest(None, Some(root), &mut |_| {})
            .expect("ingest")
    }
}

pub fn active(text: &str, kind: LessonKind) -> NewLesson {
    NewLesson {
        text: text.to_owned(),
        kind,
        scope: None,
        provenance_file: None,
        provenance_hash: None,
        state: LessonState::Active,
    }
}

pub fn candidate(text: &str) -> NewLesson {
    NewLesson {
        state: LessonState::Candidate,
        ..active(text, LessonKind::Fact)
    }
}

pub fn write_lines(path: &Path, lines: &[String]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("mkdir");
    }
    let mut body = lines.join("\n");
    body.push('\n');
    fs::write(path, body).expect("write transcript");
}

pub fn append_raw(path: &Path, text: &str) {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(path)
        .expect("open for append");
    file.write_all(text.as_bytes()).expect("append");
    file.sync_all().expect("sync");
}

/// Finds run ids on either side of the holdout hash by asking the store with a no-match prompt.
pub fn run_id_with_holdout(
    store: &Store,
    holdout: bool,
    prefix: &str,
    from: usize,
) -> (String, usize) {
    let mut n = from;
    loop {
        let id = format!("{prefix}-{n}");
        let got = store
            .select_injection(&id, "", None, 400)
            .expect("probe holdout");
        n += 1;
        if got.holdout == holdout {
            return (id, n);
        }
    }
}
