use serde::{Deserialize, Serialize};

/// What a lesson asserts, which sets its decay half-life and slot eligibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LessonKind {
    Fact,
    Preference,
    Constraint,
}

impl LessonKind {
    pub(crate) fn as_db(self) -> &'static str {
        match self {
            LessonKind::Fact => "fact",
            LessonKind::Preference => "preference",
            LessonKind::Constraint => "constraint",
        }
    }

    pub(crate) fn from_db(value: &str) -> Option<LessonKind> {
        match value {
            "fact" => Some(LessonKind::Fact),
            "preference" => Some(LessonKind::Preference),
            "constraint" => Some(LessonKind::Constraint),
            _ => None,
        }
    }

    pub(crate) fn half_life_days(self) -> f64 {
        match self {
            LessonKind::Fact => 90.0,
            LessonKind::Preference | LessonKind::Constraint => 365.0,
        }
    }
}

/// Lifecycle state of a lesson; only `Active` lessons are ever injected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LessonState {
    Candidate,
    Active,
    Stale,
    Retired,
}

impl LessonState {
    pub(crate) fn as_db(self) -> &'static str {
        match self {
            LessonState::Candidate => "candidate",
            LessonState::Active => "active",
            LessonState::Stale => "stale",
            LessonState::Retired => "retired",
        }
    }

    pub(crate) fn from_db(value: &str) -> Option<LessonState> {
        match value {
            "candidate" => Some(LessonState::Candidate),
            "active" => Some(LessonState::Active),
            "stale" => Some(LessonState::Stale),
            "retired" => Some(LessonState::Retired),
            _ => None,
        }
    }
}

/// A stored lesson; `score` is non-zero only when returned from a ranked query.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lesson {
    pub id: i64,
    pub text: String,
    pub kind: LessonKind,
    pub scope: Option<String>,
    pub provenance_file: Option<String>,
    pub provenance_hash: Option<u64>,
    pub state: LessonState,
    pub alpha: f64,
    pub beta: f64,
    pub created_at: i64,
    pub confirmed_at: i64,
    pub score: f64,
}

/// Input for `Store::add_lesson`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewLesson {
    pub text: String,
    pub kind: LessonKind,
    pub scope: Option<String>,
    pub provenance_file: Option<String>,
    pub provenance_hash: Option<u64>,
    pub state: LessonState,
}

/// Lessons chosen for a run; `text` is empty when nothing is injected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Injection {
    pub lessons: Vec<Lesson>,
    pub holdout: bool,
    pub text: String,
}

/// One orchestrator run's usage and outcome.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRecord {
    pub run_id: String,
    pub task_class: String,
    pub repo: String,
    pub model: String,
    pub provider: String,
    pub input_tokens: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output_tokens: u64,
    pub outcome: Option<bool>,
    pub corrected: bool,
}

/// Per-file ingest progress.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestProgress {
    pub files_done: u64,
    pub files_total: u64,
    pub current: String,
}

/// Totals for one ingest pass; `bytes` counts only newly consumed bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestStats {
    pub files_scanned: u64,
    pub files_updated: u64,
    pub lines: u64,
    pub prompts: u64,
    pub signals: u64,
    pub candidates: u64,
    pub skipped_large_lines: u64,
    pub bytes: u64,
}

/// One prompt found by `Store::search_history`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryHit {
    pub session: String,
    pub cwd: String,
    pub timestamp: String,
    pub text: String,
    pub snippet: String,
}

/// Outcome comparison between eligible runs with memory and held-out eligible runs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HoldoutReport {
    pub with_memory_runs: u64,
    pub with_memory_success_rate: f64,
    pub with_memory_mean_weighted_tokens: f64,
    pub holdout_runs: u64,
    pub holdout_success_rate: f64,
    pub holdout_mean_weighted_tokens: f64,
}

/// Token totals for one provider and model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelTokens {
    pub provider: String,
    pub model: String,
    pub input_tokens: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output_tokens: u64,
    pub weighted_tokens: f64,
}

/// Number of stored signals of one kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignalCount {
    pub kind: String,
    pub count: u64,
}

/// Latest rate-limit reading seen in a provider's transcripts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateLimit {
    pub provider: String,
    pub used_percent: f64,
    pub window_minutes: u64,
    pub resets_at: i64,
    pub timestamp: String,
}

/// Store counts; `per_model` comes from transcripts and `run_per_model` from `record_run`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LearnStats {
    pub files: u64,
    pub prompts: u64,
    pub signals: u64,
    pub runs: u64,
    pub injections: u64,
    pub lessons_candidate: u64,
    pub lessons_active: u64,
    pub lessons_stale: u64,
    pub lessons_retired: u64,
    pub signal_counts: Vec<SignalCount>,
    pub per_model: Vec<ModelTokens>,
    pub run_per_model: Vec<ModelTokens>,
    pub rate_limits: Vec<RateLimit>,
}
