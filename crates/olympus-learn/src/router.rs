use anyhow::{bail, Result};
use rusqlite::{params, OptionalExtension};

use crate::math::{beta_lower, beta_upper, weighted_tokens, Rng};
use crate::types::RunRecord;
use crate::{now_secs, Store};

const TOKEN_PENALTY_LAMBDA: f64 = 0.05;
const MIN_OBSERVATIONS: f64 = 5.0;
const RETIRE_BELOW: f64 = 0.4;
const MIN_HOLDOUT_EVIDENCE: i64 = 5;

impl Store {
    /// Upserts a run by id; outcome effects on lessons and the router apply exactly once.
    pub fn record_run(&self, run: &RunRecord) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO runs(run_id, task_class, repo, model, provider, input_tokens, cache_read, \
             cache_write, output_tokens, outcome, corrected, updated_at) \
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) \
             ON CONFLICT(run_id) DO UPDATE SET task_class = excluded.task_class, \
             repo = excluded.repo, model = excluded.model, provider = excluded.provider, \
             input_tokens = excluded.input_tokens, cache_read = excluded.cache_read, \
             cache_write = excluded.cache_write, output_tokens = excluded.output_tokens, \
             outcome = COALESCE(excluded.outcome, outcome), corrected = excluded.corrected, \
             updated_at = excluded.updated_at",
            params![
                run.run_id,
                run.task_class,
                run.repo,
                run.model,
                run.provider,
                run.input_tokens as i64,
                run.cache_read as i64,
                run.cache_write as i64,
                run.output_tokens as i64,
                run.outcome,
                run.corrected,
                now_secs()
            ],
        )?;
        let applied: bool = tx.query_row(
            "SELECT outcome_applied FROM runs WHERE run_id = ?1",
            [&run.run_id],
            |r| r.get(0),
        )?;
        if let (Some(outcome), false) = (run.outcome, applied) {
            let success = outcome && !run.corrected;
            self.apply_to_lessons(&run.run_id, success)?;
            self.apply_to_candidates(&run.run_id, success)?;
            self.apply_to_router(run, success)?;
            tx.execute(
                "UPDATE runs SET outcome_applied = 1 WHERE run_id = ?1",
                [&run.run_id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn apply_to_lessons(&self, run_id: &str, success: bool) -> Result<()> {
        let ids = self.lesson_ids(
            "SELECT lesson_id FROM injection_lessons WHERE run_id = ?1 AND applied = 1",
            run_id,
        )?;
        let (won, lost) = if success { (1.0, 0.0) } else { (0.0, 1.0) };
        let confirmed = success.then(now_secs);
        for id in ids {
            let (alpha, beta): (f64, f64) = self.conn.query_row(
                "UPDATE lessons SET alpha = alpha + ?2, beta = beta + ?3, \
                 confirmed_at = COALESCE(?4, confirmed_at) WHERE id = ?1 RETURNING alpha, beta",
                params![id, won, lost, confirmed],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            // The Beta(1, 1) prior contributes 2 to alpha + beta but no observations.
            if alpha + beta - 2.0 >= MIN_OBSERVATIONS && beta_upper(alpha, beta) < RETIRE_BELOW {
                self.conn.execute(
                    "UPDATE lessons SET state = 'retired' WHERE id = ?1 AND state = 'active'",
                    [id],
                )?;
            }
        }
        Ok(())
    }

    // A candidate is promoted when memory-free runs on its topic fail more than other
    // memory-free runs, which is the evidence that the warning would have been useful.
    fn apply_to_candidates(&self, run_id: &str, success: bool) -> Result<()> {
        let ids = self.lesson_ids(
            "SELECT lesson_id FROM candidate_evidence WHERE run_id = ?1",
            run_id,
        )?;
        let (fail, ok) = if success { (0, 1) } else { (1, 0) };
        for id in ids {
            let counts: Option<(i64, i64)> = self
                .conn
                .query_row(
                    "UPDATE lessons SET holdout_fail = holdout_fail + ?2, \
                     holdout_ok = holdout_ok + ?3 WHERE id = ?1 AND state = 'candidate' \
                     RETURNING holdout_fail, holdout_ok",
                    params![id, fail, ok],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let Some((fails, oks)) = counts else {
                continue;
            };
            if fails + oks < MIN_HOLDOUT_EVIDENCE {
                continue;
            }
            let (other_runs, other_fails): (i64, i64) = self.conn.query_row(
                "SELECT COUNT(*), COALESCE(SUM(NOT (r.outcome = 1 AND r.corrected = 0)), 0) \
                 FROM runs r JOIN injections i ON i.run_id = r.run_id \
                 WHERE i.holdout = 1 AND r.outcome IS NOT NULL AND r.run_id NOT IN \
                 (SELECT run_id FROM candidate_evidence WHERE lesson_id = ?1)",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let baseline = (other_fails as f64 + 1.0) / (other_runs as f64 + 2.0);
            if beta_lower(1.0 + fails as f64, 1.0 + oks as f64) > baseline {
                self.conn.execute(
                    "UPDATE lessons SET state = 'active', confirmed_at = ?2 WHERE id = ?1",
                    params![id, now_secs()],
                )?;
            }
        }
        Ok(())
    }

    fn lesson_ids(&self, sql: &str, run_id: &str) -> Result<Vec<i64>> {
        let mut stmt = self.conn.prepare_cached(sql)?;
        let rows = stmt.query_map([run_id], |r| r.get::<_, i64>(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn apply_to_router(&self, run: &RunRecord, success: bool) -> Result<()> {
        let weighted = weighted_tokens(
            run.input_tokens,
            run.cache_read,
            run.cache_write,
            run.output_tokens,
        );
        let reward = f64::from(u8::from(success)) - TOKEN_PENALTY_LAMBDA * weighted / 1_000_000.0;
        // Beta updates need a reward in [0, 1]; a costly failure cannot count below zero.
        let reward = reward.clamp(0.0, 1.0);
        self.conn.execute(
            "INSERT INTO route_stats(task_class, repo, model, alpha, beta) \
             VALUES(?1, ?2, ?3, 1 + ?4, 2 - ?4) \
             ON CONFLICT(task_class, repo, model) DO UPDATE SET \
             alpha = alpha + ?4, beta = beta + 1 - ?4",
            params![run.task_class, run.repo, run.model, reward],
        )?;
        Ok(())
    }

    /// Thompson-samples a model; a candidate with no data is always tried first.
    pub fn route(&self, task_class: &str, repo: &str, candidates: &[String]) -> Result<String> {
        if candidates.is_empty() {
            bail!("route needs at least one candidate model");
        }
        let mut seen = Vec::with_capacity(candidates.len());
        let mut unseen = Vec::new();
        {
            let mut stmt = self.conn.prepare_cached(
                "SELECT alpha, beta FROM route_stats \
                 WHERE task_class = ?1 AND repo = ?2 AND model = ?3",
            )?;
            for model in candidates {
                let stats: Option<(f64, f64)> = stmt
                    .query_row(params![task_class, repo, model], |r| {
                        Ok((r.get(0)?, r.get(1)?))
                    })
                    .optional()?;
                match stats {
                    Some((alpha, beta)) => seen.push((model, alpha, beta)),
                    None => unseen.push(model),
                }
            }
        }
        let mut rng = Rng(self.rng.get());
        let choice = if unseen.is_empty() {
            seen.iter()
                .map(|(model, alpha, beta)| (*model, rng.beta(*alpha, *beta)))
                .fold(None::<(&String, f64)>, |best, next| match best {
                    Some(b) if b.1 >= next.1 => Some(b),
                    _ => Some(next),
                })
                .map(|(model, _)| model)
        } else {
            let index = (rng.uniform() * unseen.len() as f64) as usize;
            unseen.get(index).copied()
        };
        self.rng.set(rng.0);
        match choice {
            Some(model) => Ok(model.clone()),
            None => bail!("route found no candidate"),
        }
    }
}
