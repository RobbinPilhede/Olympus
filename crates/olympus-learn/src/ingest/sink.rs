use anyhow::Result;
use rusqlite::{params, Connection};

use crate::math::row_id;
use crate::text::{correction_key, is_correction, is_wrapper, redact, strip_reminders, truncate};
use crate::types::IngestStats;

pub(super) const FAILING_COMMAND: &str = "failing_command";
pub(super) const TOOL_ERROR: &str = "tool_error";
pub(super) const ERROR_FIX: &str = "error_fix";
pub(super) const PERMISSION_DENIAL: &str = "permission_denial";
pub(super) const INTERRUPTED: &str = "interrupted";
pub(super) const CORRECTION: &str = "correction";

const MAX_PROMPT_CHARS: usize = 2000;
const MAX_DETAIL_CHARS: usize = 200;

#[derive(Clone, Copy)]
pub(super) struct Ctx<'a> {
    pub(super) session: &'a str,
    pub(super) cwd: &'a str,
    pub(super) timestamp: &'a str,
}

#[derive(Clone, Copy, Default)]
pub(super) struct Tokens {
    pub(super) input: u64,
    pub(super) cache_read: u64,
    pub(super) cache_write: u64,
    pub(super) output: u64,
}

pub(super) struct Sink<'a> {
    conn: &'a Connection,
    stats: &'a mut IngestStats,
}

impl<'a> Sink<'a> {
    pub(super) fn new(conn: &'a Connection, stats: &'a mut IngestStats) -> Sink<'a> {
        Sink { conn, stats }
    }

    // `uid` is stable across file copies, so resumed and forked transcripts dedupe.
    pub(super) fn signal(
        &mut self,
        ctx: Ctx,
        kind: &str,
        uid: &str,
        key: &str,
        detail: &str,
    ) -> Result<()> {
        let inserted = self
            .conn
            .prepare_cached(
                "INSERT OR IGNORE INTO signals(id, kind, session, cwd, timestamp, sig_key, detail) \
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?
            .execute(params![
                row_id(&[kind, uid]),
                kind,
                ctx.session,
                ctx.cwd,
                ctx.timestamp,
                key,
                truncate(detail, MAX_DETAIL_CHARS)
            ])?;
        self.stats.signals += inserted as u64;
        Ok(())
    }

    // Returns whether the text was a real prompt, i.e. not harness-generated.
    pub(super) fn prompt(
        &mut self,
        ctx: Ctx,
        uid: &str,
        raw: &str,
        after_assistant: bool,
    ) -> Result<bool> {
        let stripped = strip_reminders(raw);
        let text = stripped.trim();
        if text.is_empty() || is_wrapper(text) {
            return Ok(false);
        }
        let stored = redact(truncate(text, MAX_PROMPT_CHARS));
        let id = row_id(&["prompt", uid]);
        let inserted = self
            .conn
            .prepare_cached(
                "INSERT OR IGNORE INTO prompts(id, session, cwd, timestamp, text) \
                 VALUES(?1, ?2, ?3, ?4, ?5)",
            )?
            .execute(params![id, ctx.session, ctx.cwd, ctx.timestamp, stored])?;
        if inserted == 1 {
            self.conn
                .prepare_cached("INSERT INTO prompts_fts(rowid, text) VALUES(?1, ?2)")?
                .execute(params![id, stored])?;
            self.stats.prompts += 1;
        }
        if after_assistant && is_correction(text) {
            self.signal(ctx, CORRECTION, uid, &correction_key(text), &stored)?;
        }
        Ok(true)
    }

    // Lines of one request repeat its usage while output grows, so keep each field's maximum.
    pub(super) fn usage_max(
        &mut self,
        request_key: &str,
        provider: &str,
        model: &str,
        t: Tokens,
    ) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO usage(id, provider, model, input, cache_read, cache_write, output) \
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7) ON CONFLICT(id) DO UPDATE SET \
                 input = max(input, excluded.input), \
                 cache_read = max(cache_read, excluded.cache_read), \
                 cache_write = max(cache_write, excluded.cache_write), \
                 output = max(output, excluded.output)",
            )?
            .execute(params![
                row_id(&["usage", provider, request_key]),
                provider,
                model,
                t.input as i64,
                t.cache_read as i64,
                t.cache_write as i64,
                t.output as i64
            ])?;
        Ok(())
    }

    pub(super) fn usage_add(
        &mut self,
        thread: &str,
        provider: &str,
        model: &str,
        t: Tokens,
    ) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO usage(id, provider, model, input, cache_read, cache_write, output) \
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7) ON CONFLICT(id) DO UPDATE SET \
                 input = input + excluded.input, \
                 cache_read = cache_read + excluded.cache_read, \
                 cache_write = cache_write + excluded.cache_write, \
                 output = output + excluded.output",
            )?
            .execute(params![
                row_id(&["usage", provider, thread, model]),
                provider,
                model,
                t.input as i64,
                t.cache_read as i64,
                t.cache_write as i64,
                t.output as i64
            ])?;
        Ok(())
    }

    pub(super) fn rate_limit(
        &mut self,
        provider: &str,
        used_percent: f64,
        window_minutes: u64,
        resets_at: i64,
        timestamp: &str,
    ) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO rate_limits(provider, used_percent, window_minutes, resets_at, timestamp) \
                 VALUES(?1, ?2, ?3, ?4, ?5) ON CONFLICT(provider) DO UPDATE SET \
                 used_percent = excluded.used_percent, window_minutes = excluded.window_minutes, \
                 resets_at = excluded.resets_at, timestamp = excluded.timestamp \
                 WHERE excluded.timestamp >= rate_limits.timestamp",
            )?
            .execute(params![
                provider,
                used_percent,
                window_minutes as i64,
                resets_at,
                timestamp
            ])?;
        Ok(())
    }
}
