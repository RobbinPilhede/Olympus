mod claude;
mod codex;
mod derive;
mod lines;
mod messages;
mod sink;
mod tracker;

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::types::{IngestProgress, IngestStats};
use crate::Store;
use claude::ClaudeParser;
use codex::CodexParser;
use lines::{scan_lines, READ_BUFFER_BYTES};
use sink::Sink;

#[derive(Clone, Copy)]
enum Source {
    Claude { sidechain: bool },
    Codex,
}

struct Discovered {
    path: PathBuf,
    source: Source,
}

fn collect_recursive(dir: &Path, accept: &dyn Fn(&str) -> bool, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() {
            collect_recursive(&path, accept, out);
        } else if kind.is_file() && entry.file_name().to_str().is_some_and(accept) {
            out.push(path);
        }
    }
}

fn discover_claude(claude_dir: &Path, out: &mut Vec<Discovered>) {
    let Ok(projects) = fs::read_dir(claude_dir.join("projects")) else {
        return;
    };
    for project in projects.flatten() {
        let Ok(entries) = fs::read_dir(project.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_file() && path.extension().is_some_and(|e| e == "jsonl") {
                let source = Source::Claude { sidechain: false };
                out.push(Discovered { path, source });
            } else if kind.is_dir() {
                let mut agents = Vec::new();
                let is_agent = |n: &str| n.starts_with("agent-") && n.ends_with(".jsonl");
                collect_recursive(&path.join("subagents"), &is_agent, &mut agents);
                out.extend(agents.into_iter().map(|path| Discovered {
                    path,
                    source: Source::Claude { sidechain: true },
                }));
            }
        }
    }
}

fn discover_codex(codex_dir: &Path, out: &mut Vec<Discovered>) {
    let mut paths = Vec::new();
    let is_rollout = |n: &str| n.starts_with("rollout-") && (n.ends_with(".jsonl") || n.ends_with(".jsonl.zst"));
    collect_recursive(&codex_dir.join("sessions"), &is_rollout, &mut paths);
    out.extend(paths.into_iter().map(|path| Discovered {
        path,
        source: Source::Codex,
    }));
}

fn load_state<T: DeserializeOwned + Default>(raw: Option<&str>) -> T {
    raw.and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default()
}

fn dump_state<T: Serialize>(state: &T) -> Result<String> {
    Ok(serde_json::to_string(state)?)
}

impl Store {
    /// Incrementally ingests transcripts: signals, truncated prompts, and redacted messages for search.
    pub fn ingest(
        &self,
        claude_dir: Option<&Path>,
        codex_dir: Option<&Path>,
        progress: &mut dyn FnMut(&IngestProgress),
    ) -> Result<IngestStats> {
        let mut found = Vec::new();
        if let Some(dir) = claude_dir {
            discover_claude(dir, &mut found);
        }
        if let Some(dir) = codex_dir {
            discover_codex(dir, &mut found);
        }
        found.sort_by(|a, b| a.path.cmp(&b.path));

        let mut stats = IngestStats {
            files_scanned: found.len() as u64,
            ..IngestStats::default()
        };
        for (index, file) in found.iter().enumerate() {
            self.ingest_file(file, &mut stats)?;
            progress(&IngestProgress {
                files_done: index as u64 + 1,
                files_total: found.len() as u64,
                current: file.path.to_string_lossy().into_owned(),
            });
        }
        if stats.files_updated > 0 {
            stats.candidates = self.derive_candidates()?;
        }
        self.index_messages(claude_dir, codex_dir)?;
        Ok(stats)
    }

    fn was_ingested(&self, path: &str) -> Result<bool> {
        Ok(self.conn.query_row("SELECT EXISTS(SELECT 1 FROM files WHERE path = ?1)", [path], |r| r.get(0))?)
    }

    fn ingest_file(&self, file: &Discovered, stats: &mut IngestStats) -> Result<()> {
        let path = file.path.to_string_lossy();
        // Unreadable or vanished transcripts must not abort a pass over thousands of others.
        let Ok(meta) = fs::metadata(&file.path) else {
            return Ok(());
        };
        let size = meta.len() as i64;
        // Sampled before reading, so an append during the read is noticed on the next pass.
        let mtime = meta
            .modified()
            .ok()
            .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos() as i64);
        let known: Option<(i64, i64, i64, String)> = self
            .conn
            .query_row(
                "SELECT byte_offset, mtime, size, state FROM files WHERE path = ?1",
                [path.as_ref()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        if let Some((_, known_mtime, known_size, _)) = &known {
            if *known_mtime == mtime && *known_size == size {
                return Ok(());
            }
        }
        let compressed = path.ends_with(".zst");
        // Codex compresses old rollouts, so one already read in plain form must not count twice.
        if compressed && self.was_ingested(path.trim_end_matches(".zst"))? {
            self.conn.execute(
                "INSERT OR REPLACE INTO files(path, byte_offset, mtime, size, state) VALUES(?1, 0, ?2, ?3, '')",
                params![path.as_ref(), mtime, size],
            )?;
            return Ok(());
        }
        // A file smaller than our offset was rewritten, so its old offset and state are void.
        // Offsets can't be seeked in compressed data, and archives aren't appended to.
        let (start, state) = match &known {
            Some((offset, _, _, state)) if *offset <= size && !compressed => (*offset, Some(state.as_str())),
            _ => (0, None),
        };
        let Ok(mut handle) = File::open(&file.path) else {
            return Ok(());
        };
        if handle.seek(SeekFrom::Start(start as u64)).is_err() {
            return Ok(());
        }
        let buffered = BufReader::with_capacity(READ_BUFFER_BYTES, handle);
        let mut reader: Box<dyn BufRead> = if compressed {
            let Ok(decoder) = ruzstd::decoding::StreamingDecoder::new(buffered) else {
                return Ok(());
            };
            Box::new(BufReader::with_capacity(READ_BUFFER_BYTES, decoder))
        } else {
            Box::new(buffered)
        };

        let tx = self.conn.unchecked_transaction()?;
        let mut sink = Sink::new(&self.conn, stats);
        let (scan, new_state) = match file.source {
            Source::Claude { sidechain } => {
                let mut parser = ClaudeParser::new(load_state(state), sidechain);
                let scan = scan_lines(&mut reader, |line| parser.line(line, &mut sink))?;
                (scan, dump_state(&parser.state)?)
            }
            Source::Codex => {
                let mut parser = CodexParser::new(load_state(state));
                let scan = scan_lines(&mut reader, |line| parser.line(line, &mut sink))?;
                parser.finish(&mut sink)?;
                (scan, dump_state(&parser.state)?)
            }
        };
        // A read error leaves size at -1 so the next pass retries from the committed offset.
        let recorded_size = if scan.read_failed { -1 } else { size };
        tx.execute(
            "INSERT INTO files(path, byte_offset, mtime, size, state) VALUES(?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(path) DO UPDATE SET byte_offset = excluded.byte_offset, \
             mtime = excluded.mtime, size = excluded.size, state = excluded.state",
            params![
                path.as_ref(),
                start + scan.consumed as i64,
                mtime,
                recorded_size,
                new_state
            ],
        )?;
        tx.commit()?;
        stats.lines += scan.lines;
        stats.skipped_large_lines += scan.skipped_large;
        stats.bytes += scan.consumed;
        if scan.consumed > 0 {
            stats.files_updated += 1;
        }
        Ok(())
    }
}
