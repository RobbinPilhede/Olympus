use std::collections::HashMap;
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::path::{Component, Path};

use anyhow::{bail, Result};
use rusqlite::{params, OptionalExtension, Row};

use crate::math::{beta_lower, fnv1a64_start, fnv1a64_update, is_holdout};
use crate::text::{fts_join, terms, truncate};
use crate::types::{Injection, Lesson, LessonKind, LessonState, NewLesson};
use crate::{now_secs, Store};

const MAX_INJECTION_TOKENS: usize = 400;
const MAX_LESSONS: usize = 5;
const MAX_LESSON_CHARS: usize = 240;
const MAX_EVIDENCE_CANDIDATES: usize = 20;
const MAX_FTS_ROWS: i64 = 200;
const LONG_PROMPT_TERMS: usize = 4;
const SECONDS_PER_DAY: f64 = 86_400.0;
const COLUMNS: &str = "lessons.id, lessons.text, lessons.kind, lessons.scope, \
    lessons.provenance_file, lessons.provenance_hash, lessons.state, lessons.alpha, \
    lessons.beta, lessons.created_at, lessons.confirmed_at";

fn lesson_from_row(row: &Row) -> rusqlite::Result<Lesson> {
    let invalid = |index: usize, value: String| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Text,
            format!("unknown lesson value {value}").into(),
        )
    };
    let kind: String = row.get(2)?;
    let state: String = row.get(6)?;
    let hash: Option<i64> = row.get(5)?;
    Ok(Lesson {
        id: row.get(0)?,
        text: row.get(1)?,
        kind: LessonKind::from_db(&kind).ok_or_else(|| invalid(2, kind.clone()))?,
        scope: row.get(3)?,
        provenance_file: row.get(4)?,
        // SQLite integers are signed; the u64 hash round-trips through a bit cast.
        provenance_hash: hash.map(|h| h as u64),
        state: LessonState::from_db(&state).ok_or_else(|| invalid(6, state.clone()))?,
        alpha: row.get(7)?,
        beta: row.get(8)?,
        created_at: row.get(9)?,
        confirmed_at: row.get(10)?,
        score: 0.0,
    })
}

fn normalize_path(path: &str) -> String {
    let unified = path.replace('\\', "/");
    let trimmed = unified.trim_end_matches('/');
    if cfg!(windows) {
        trimmed.to_lowercase()
    } else {
        trimmed.to_owned()
    }
}

fn scope_matches(scope: Option<&str>, repo: Option<&Path>) -> bool {
    let Some(scope) = scope else {
        return true;
    };
    let Some(repo) = repo else {
        return false;
    };
    let scope = normalize_path(scope);
    let repo = normalize_path(&repo.to_string_lossy());
    repo == scope || repo.starts_with(&format!("{scope}/"))
}

enum Provenance {
    Fresh,
    Changed,
    Unverifiable,
}

fn check_provenance(lesson: &Lesson, repo: Option<&Path>) -> Provenance {
    let Some(relative) = lesson.provenance_file.as_deref() else {
        return Provenance::Fresh;
    };
    let Some(repo) = repo else {
        return Provenance::Unverifiable;
    };
    let relative = Path::new(relative);
    let inside_repo = relative
        .components()
        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
    if !inside_repo {
        return Provenance::Changed;
    }
    let mut file = match File::open(repo.join(relative)) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Provenance::Changed,
        Err(_) => return Provenance::Unverifiable,
    };
    let Some(expected) = lesson.provenance_hash else {
        return Provenance::Fresh;
    };
    let mut hash = fnv1a64_start();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => hash = fnv1a64_update(hash, &buffer[..n]),
            Err(_) => return Provenance::Unverifiable,
        }
    }
    if hash == expected {
        Provenance::Fresh
    } else {
        Provenance::Changed
    }
}

fn bullet(text: &str) -> String {
    let single_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if single_line.chars().count() > MAX_LESSON_CHARS {
        format!("- {}...", truncate(&single_line, MAX_LESSON_CHARS - 3))
    } else {
        format!("- {single_line}")
    }
}

fn token_estimate(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

fn pick(ranked: Vec<Lesson>, budget: usize) -> Vec<(Lesson, String)> {
    let priced = |lesson: Lesson| {
        let line = bullet(&lesson.text);
        // One extra char pays for the newline joining this bullet to the next.
        let cost = (line.chars().count() + 1).div_ceil(4);
        (lesson, line, cost)
    };
    let mut pool: Vec<_> = ranked.into_iter().map(priced).collect();
    let mut chosen: Vec<(Lesson, String)> = Vec::new();
    let mut spent = 0;
    let reserved = pool
        .iter()
        .position(|(l, _, cost)| l.kind == LessonKind::Constraint && *cost <= budget);
    if let Some(index) = reserved {
        let (lesson, line, cost) = pool.remove(index);
        spent += cost;
        chosen.push((lesson, line));
    }
    let mut others = 0;
    for (lesson, line, cost) in pool {
        if chosen.len() == MAX_LESSONS {
            break;
        }
        let is_other = lesson.kind != LessonKind::Constraint;
        if (is_other && others == MAX_LESSONS - 1) || spent + cost > budget {
            continue;
        }
        others += usize::from(is_other);
        spent += cost;
        chosen.push((lesson, line));
    }
    chosen.sort_by(|a, b| b.0.score.total_cmp(&a.0.score));
    chosen
}

impl Store {
    pub(crate) fn insert_lesson(&self, new: &NewLesson, dedupe_key: &str) -> Result<(i64, bool)> {
        let now = now_secs();
        let tx = self.conn.unchecked_transaction()?;
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO lessons(text, kind, scope, provenance_file, provenance_hash, \
             state, created_at, confirmed_at, dedupe_key) \
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8)",
            params![
                new.text,
                new.kind.as_db(),
                new.scope,
                new.provenance_file,
                new.provenance_hash.map(|h| h as i64),
                new.state.as_db(),
                now,
                dedupe_key
            ],
        )? == 1;
        let id: i64 = tx.query_row(
            "SELECT id FROM lessons WHERE dedupe_key = ?1",
            [dedupe_key],
            |r| r.get(0),
        )?;
        if inserted {
            tx.execute(
                "INSERT INTO lessons_fts(rowid, text) VALUES(?1, ?2)",
                params![id, new.text],
            )?;
        }
        tx.commit()?;
        Ok((id, inserted))
    }

    /// Stores a lesson, returning the existing id if the same text and scope is already stored.
    pub fn add_lesson(&self, new: NewLesson) -> Result<i64> {
        let text = new.text.trim();
        if text.is_empty() {
            bail!("lesson text is empty");
        }
        let key = format!(
            "manual\0{}\0{}",
            new.scope.as_deref().unwrap_or(""),
            text.to_lowercase()
        );
        let new = NewLesson {
            text: text.to_owned(),
            ..new
        };
        Ok(self.insert_lesson(&new, &key)?.0)
    }

    /// Makes a lesson eligible for injection; re-approving a retired lesson resets its evidence.
    pub fn approve_lesson(&self, id: i64) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE lessons SET \
             alpha = CASE WHEN state = 'retired' THEN 1 ELSE alpha END, \
             beta = CASE WHEN state = 'retired' THEN 1 ELSE beta END, \
             state = 'active', confirmed_at = ?2 WHERE id = ?1",
            params![id, now_secs()],
        )?;
        if changed == 0 {
            bail!("no lesson with id {id}");
        }
        Ok(())
    }

    /// Retires a lesson permanently; derived candidates with the same key are not recreated.
    pub fn reject_lesson(&self, id: i64) -> Result<()> {
        let changed = self
            .conn
            .execute("UPDATE lessons SET state = 'retired' WHERE id = ?1", [id])?;
        if changed == 0 {
            bail!("no lesson with id {id}");
        }
        Ok(())
    }

    /// Lists lessons newest first, optionally filtered by state.
    pub fn lessons(&self, state: Option<LessonState>, limit: usize) -> Result<Vec<Lesson>> {
        let sql = format!(
            "SELECT {COLUMNS} FROM lessons WHERE ?1 IS NULL OR state = ?1 \
             ORDER BY id DESC LIMIT ?2"
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(
            params![state.map(LessonState::as_db), limit as i64],
            lesson_from_row,
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // Lessons with a changed provenance file are marked stale as a side effect.
    fn ranked(
        &self,
        query: &str,
        repo: Option<&Path>,
        state: LessonState,
        gated: bool,
    ) -> Result<Vec<Lesson>> {
        let query_terms = terms(query);
        if query_terms.is_empty() {
            return Ok(Vec::new());
        }
        let required = if gated && query_terms.len() >= LONG_PROMPT_TERMS {
            2
        } else {
            1
        };
        let mut overlap: HashMap<i64, usize> = HashMap::new();
        {
            let mut stmt = self
                .conn
                .prepare_cached("SELECT rowid FROM lessons_fts WHERE lessons_fts MATCH ?1")?;
            for term in &query_terms {
                let ids = stmt.query_map([format!("\"{term}\"")], |r| r.get::<_, i64>(0))?;
                for id in ids {
                    *overlap.entry(id?).or_insert(0) += 1;
                }
            }
        }
        let sql = format!(
            "SELECT {COLUMNS}, bm25(lessons_fts) FROM lessons_fts \
             JOIN lessons ON lessons.id = lessons_fts.rowid \
             WHERE lessons_fts MATCH ?1 AND lessons.state = ?2 \
             ORDER BY bm25(lessons_fts) LIMIT ?3"
        );
        let matched: Vec<(Lesson, f64)> = {
            let mut stmt = self.conn.prepare_cached(&sql)?;
            let rows = stmt.query_map(
                params![fts_join(&query_terms, " OR "), state.as_db(), MAX_FTS_ROWS],
                |row| Ok((lesson_from_row(row)?, row.get::<_, f64>(11)?)),
            )?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut fresh = Vec::with_capacity(matched.len());
        for (lesson, rank) in matched {
            let enough = overlap.get(&lesson.id).copied().unwrap_or(0) >= required;
            if !enough || !scope_matches(lesson.scope.as_deref(), repo) {
                continue;
            }
            match check_provenance(&lesson, repo) {
                Provenance::Fresh => fresh.push((lesson, rank)),
                Provenance::Unverifiable => {}
                Provenance::Changed => {
                    self.conn.execute(
                        "UPDATE lessons SET state = 'stale' WHERE id = ?1",
                        [lesson.id],
                    )?;
                }
            }
        }
        // bm25() is negative with better matches lower, so the minimum is the best match.
        let best = fresh.iter().map(|(_, rank)| *rank).fold(0.0, f64::min);
        let now = now_secs();
        let mut scored: Vec<Lesson> = fresh
            .into_iter()
            .map(|(mut lesson, rank)| {
                let relevance = if best < 0.0 { rank / best } else { 1.0 };
                let days = (now - lesson.confirmed_at).max(0) as f64 / SECONDS_PER_DAY;
                let decay = 0.5f64.powf(days / lesson.kind.half_life_days());
                lesson.score = beta_lower(lesson.alpha, lesson.beta) * decay * relevance;
                lesson
            })
            .collect();
        scored.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.id.cmp(&b.id)));
        Ok(scored)
    }

    /// Chooses and records lessons for a run; held-out runs get none but are still recorded.
    pub fn select_injection(
        &self,
        run_id: &str,
        prompt: &str,
        repo: Option<&Path>,
        budget_tokens: usize,
    ) -> Result<Injection> {
        let holdout = is_holdout(run_id);
        let budget = budget_tokens.min(MAX_INJECTION_TOKENS);
        let ranked = self.ranked(prompt, repo, LessonState::Active, true)?;
        let chosen = pick(ranked, budget);
        let candidates = if holdout {
            let mut found = self.ranked(prompt, repo, LessonState::Candidate, true)?;
            found.truncate(MAX_EVIDENCE_CANDIDATES);
            found
        } else {
            Vec::new()
        };
        let lines: Vec<&str> = chosen.iter().map(|(_, line)| line.as_str()).collect();
        let text = if holdout {
            String::new()
        } else {
            lines.join("\n")
        };

        let tx = self.conn.unchecked_transaction()?;
        let settled: bool = tx
            .query_row(
                "SELECT outcome_applied FROM runs WHERE run_id = ?1",
                [run_id],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(false);
        if !settled {
            tx.execute(
                "INSERT OR REPLACE INTO injections(run_id, holdout, eligible, tokens, created_at) \
                 VALUES(?1, ?2, ?3, ?4, ?5)",
                params![
                    run_id,
                    holdout,
                    !chosen.is_empty(),
                    token_estimate(&text) as i64,
                    now_secs()
                ],
            )?;
            tx.execute("DELETE FROM injection_lessons WHERE run_id = ?1", [run_id])?;
            tx.execute("DELETE FROM candidate_evidence WHERE run_id = ?1", [run_id])?;
            for (lesson, _) in &chosen {
                tx.execute(
                    "INSERT INTO injection_lessons(run_id, lesson_id, applied) VALUES(?1, ?2, ?3)",
                    params![run_id, lesson.id, !holdout],
                )?;
            }
            for lesson in &candidates {
                tx.execute(
                    "INSERT INTO candidate_evidence(run_id, lesson_id) VALUES(?1, ?2)",
                    params![run_id, lesson.id],
                )?;
            }
        }
        tx.commit()?;

        let lessons = if holdout {
            Vec::new()
        } else {
            chosen.into_iter().map(|(lesson, _)| lesson).collect()
        };
        Ok(Injection {
            lessons,
            holdout,
            text,
        })
    }

    /// Searches active lessons on demand, ignoring holdout but still honouring staleness.
    pub fn recall(&self, query: &str, repo: Option<&Path>, limit: usize) -> Result<Vec<Lesson>> {
        let mut found = self.ranked(query, repo, LessonState::Active, false)?;
        found.truncate(limit);
        Ok(found)
    }
}
