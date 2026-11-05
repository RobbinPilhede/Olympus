use anyhow::Result;
use rusqlite::params;

use super::sink::{CORRECTION, PERMISSION_DENIAL};
use crate::text::truncate;
use crate::types::{LessonKind, LessonState, NewLesson};
use crate::Store;

const MAX_CANDIDATES: i64 = 200;
const PER_KIND_LIMIT: usize = 25;
const MAX_TEXT_CHARS: usize = 240;

struct Rule {
    signal: &'static str,
    kind: LessonKind,
    min_count: i64,
    min_sessions: i64,
    render: fn(&str, &str) -> String,
}

// Failure and correction signals proved too noisy on real history to become lessons by rule;
// they stay searchable, and `distill` is what turns corrections into lessons.
const RULES: &[Rule] = &[Rule {
    signal: PERMISSION_DENIAL,
    kind: LessonKind::Constraint,
    min_count: 3,
    min_sessions: 2,
    render: |key, _| format!("`{key}` is often denied here; ask first or take another route"),
}];

const GENERIC_KEYS: &[&str] = &[
    "python", "python3", "py", "node", "bun", "bunx", "bun x", "bun run", "npm", "npx", "pnpm", "bash", "sh", "cmd", "powershell", "pwsh",
    "from", "import", "program", "bin", "status", "mkdir", "cd", "echo", "cat", "ls", "get-date", "get-location)", "convertfrom-json", "new-item",
    "git status", "git diff", "git show", "git add", "git log", "git rev-parse",
];

// A lesson needs a specific tool and a real message; shell quoting slips and JSON payloads teach nothing.
fn is_useful_signal(key: &str, detail: &str, needs_detail: bool) -> bool {
    if GENERIC_KEYS.contains(&key) {
        return false;
    }
    if !needs_detail {
        return true;
    }
    let detail = detail.trim();
    let noise = ["{", "[", "/usr/bin/bash: -c:", "Exception:"];
    detail.len() >= 12 && !noise.iter().any(|n| detail.starts_with(n))
}

impl Store {
    pub(super) fn derive_candidates(&self) -> Result<u64> {
        let existing: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM lessons WHERE state = 'candidate'",
            [],
            |r| r.get(0),
        )?;
        let mut room = MAX_CANDIDATES - existing;
        let mut created = 0;
        let mut add = |new: NewLesson, dedupe: String| -> Result<()> {
            if room > 0 && self.insert_lesson(&new, &dedupe)?.1 {
                created += 1;
                room -= 1;
            }
            Ok(())
        };
        for rule in RULES {
            for (cwd, key, detail) in self.frequent_signals(rule)? {
                let text = (rule.render)(&key, &detail);
                let dedupe = format!("derived\0{}\0{cwd}\0{key}", rule.signal);
                add(candidate(&text, rule.kind, Some(cwd)), dedupe)?;
            }
        }
        Ok(created)
    }

    fn frequent_signals(&self, rule: &Rule) -> Result<Vec<(String, String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT cwd, sig_key, detail, MAX(timestamp) FROM signals \
             WHERE kind = ?1 AND sig_key NOT IN ('', 'turn') \
             GROUP BY cwd, sig_key \
             HAVING COUNT(*) >= ?2 AND COUNT(DISTINCT session) >= ?3 \
             ORDER BY COUNT(*) DESC, cwd, sig_key LIMIT ?4",
        )?;
        let rows = stmt.query_map(
            params![rule.signal, rule.min_count, rule.min_sessions, PER_KIND_LIMIT as i64 * 8],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let needs_detail = rule.kind == LessonKind::Fact;
        let found: Vec<(String, String, String)> = rows.collect::<rusqlite::Result<_>>()?;
        Ok(found.into_iter().filter(|(_, key, detail)| is_useful_signal(key, detail, needs_detail)).take(PER_KIND_LIMIT).collect())
    }

    /// The user's corrections, newest first, for distilling into lessons.
    pub fn recent_corrections(&self, limit: usize) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT detail FROM signals WHERE kind = ?1 ORDER BY timestamp DESC, id DESC LIMIT ?2")?;
        let rows = stmt.query_map(params![CORRECTION, limit as i64], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

fn candidate(text: &str, kind: LessonKind, scope: Option<String>) -> NewLesson {
    NewLesson {
        text: truncate(text, MAX_TEXT_CHARS).to_owned(),
        kind,
        scope,
        provenance_file: None,
        provenance_hash: None,
        state: LessonState::Candidate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_tools_and_noisy_details_are_not_lessons() {
        assert!(!is_useful_signal("python", "AssertionError: real failure", true));
        assert!(!is_useful_signal("cargo test", "{\"status\":\"fulfilled\"}", true));
        assert!(!is_useful_signal("cargo test", "/usr/bin/bash: -c: line 1: unexpected EOF", true));
        assert!(is_useful_signal("cargo test", "error[E0308]: mismatched types", true));
        assert!(is_useful_signal("terraform", "", false));
    }
}
