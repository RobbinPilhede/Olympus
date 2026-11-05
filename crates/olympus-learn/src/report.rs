use anyhow::Result;
use rusqlite::{params, Connection};

use crate::math::weighted_tokens;
use crate::text::{fts_join, terms};
use crate::types::{HistoryHit, HoldoutReport, LearnStats, ModelTokens, RateLimit, SignalCount};
use crate::Store;

fn count(conn: &Connection, sql: &str) -> Result<u64> {
    let value: i64 = conn.query_row(sql, [], |r| r.get(0))?;
    Ok(value as u64)
}

fn model_tokens(conn: &Connection, sql: &str) -> Result<Vec<ModelTokens>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], |r| {
        let (input, cache_read, cache_write, output): (i64, i64, i64, i64) =
            (r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?);
        let (input, cache_read, cache_write, output) = (
            input as u64,
            cache_read as u64,
            cache_write as u64,
            output as u64,
        );
        Ok(ModelTokens {
            provider: r.get(0)?,
            model: r.get(1)?,
            input_tokens: input,
            cache_read,
            cache_write,
            output_tokens: output,
            weighted_tokens: weighted_tokens(input, cache_read, cache_write, output),
        })
    })?;
    let mut models: Vec<ModelTokens> = rows.collect::<rusqlite::Result<_>>()?;
    models.sort_by(|a, b| b.weighted_tokens.total_cmp(&a.weighted_tokens));
    Ok(models)
}

impl Store {
    /// Full-text searches ingested prompts: all terms first, then any term.
    pub fn search_history(&self, query: &str, limit: usize) -> Result<Vec<HistoryHit>> {
        let query_terms = terms(query);
        if query_terms.is_empty() {
            return Ok(Vec::new());
        }
        let hits = self.history_query(&fts_join(&query_terms, " AND "), limit)?;
        if !hits.is_empty() {
            return Ok(hits);
        }
        self.history_query(&fts_join(&query_terms, " OR "), limit)
    }

    fn history_query(&self, fts: &str, limit: usize) -> Result<Vec<HistoryHit>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT p.session, p.cwd, p.timestamp, p.text, \
             snippet(prompts_fts, 0, '[', ']', '...', 16) \
             FROM prompts_fts JOIN prompts p ON p.id = prompts_fts.rowid \
             WHERE prompts_fts MATCH ?1 ORDER BY bm25(prompts_fts) LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![fts, limit as i64], |r| {
            Ok(HistoryHit {
                session: r.get(0)?,
                cwd: r.get(1)?,
                timestamp: r.get(2)?,
                text: r.get(3)?,
                snippet: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Compares success rate and mean weighted tokens across eligible runs with known outcomes.
    pub fn holdout_report(&self) -> Result<HoldoutReport> {
        let mut report = HoldoutReport::default();
        let mut stmt = self.conn.prepare_cached(
            "SELECT i.holdout, COUNT(*), AVG(r.outcome = 1 AND r.corrected = 0), \
             AVG(r.input_tokens + 0.1 * r.cache_read + 1.25 * r.cache_write \
                 + 5.0 * r.output_tokens) \
             FROM injections i JOIN runs r ON r.run_id = i.run_id \
             WHERE i.eligible = 1 AND r.outcome IS NOT NULL GROUP BY i.holdout",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, bool>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, f64>(2)?,
                r.get::<_, f64>(3)?,
            ))
        })?;
        for row in rows {
            let (holdout, runs, rate, tokens) = row?;
            if holdout {
                report.holdout_runs = runs as u64;
                report.holdout_success_rate = rate;
                report.holdout_mean_weighted_tokens = tokens;
            } else {
                report.with_memory_runs = runs as u64;
                report.with_memory_success_rate = rate;
                report.with_memory_mean_weighted_tokens = tokens;
            }
        }
        Ok(report)
    }

    /// Returns store counts, signal counts, per-model tokens and latest rate limits.
    pub fn stats(&self) -> Result<LearnStats> {
        let conn = &self.conn;
        let lessons_in = |state: &str| -> Result<u64> {
            let value: i64 = conn.query_row(
                "SELECT COUNT(*) FROM lessons WHERE state = ?1",
                [state],
                |r| r.get(0),
            )?;
            Ok(value as u64)
        };
        let signal_counts = {
            let mut stmt = conn.prepare(
                "SELECT kind, COUNT(*) FROM signals GROUP BY kind ORDER BY COUNT(*) DESC",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(SignalCount {
                    kind: r.get(0)?,
                    count: r.get::<_, i64>(1)? as u64,
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let rate_limits = {
            let mut stmt = conn.prepare(
                "SELECT provider, used_percent, window_minutes, resets_at, timestamp \
                 FROM rate_limits ORDER BY provider",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(RateLimit {
                    provider: r.get(0)?,
                    used_percent: r.get(1)?,
                    window_minutes: r.get::<_, i64>(2)? as u64,
                    resets_at: r.get(3)?,
                    timestamp: r.get(4)?,
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        Ok(LearnStats {
            files: count(conn, "SELECT COUNT(*) FROM files")?,
            prompts: count(conn, "SELECT COUNT(*) FROM prompts")?,
            signals: count(conn, "SELECT COUNT(*) FROM signals")?,
            runs: count(conn, "SELECT COUNT(*) FROM runs")?,
            injections: count(conn, "SELECT COUNT(*) FROM injections")?,
            lessons_candidate: lessons_in("candidate")?,
            lessons_active: lessons_in("active")?,
            lessons_stale: lessons_in("stale")?,
            lessons_retired: lessons_in("retired")?,
            signal_counts,
            per_model: model_tokens(
                conn,
                "SELECT provider, model, SUM(input), SUM(cache_read), SUM(cache_write), \
                 SUM(output) FROM usage GROUP BY provider, model",
            )?,
            run_per_model: model_tokens(
                conn,
                "SELECT provider, model, SUM(input_tokens), SUM(cache_read), SUM(cache_write), \
                 SUM(output_tokens) FROM runs GROUP BY provider, model",
            )?,
            rate_limits,
        })
    }
}
