//! Jobs that run on a timetable, kept on disk so they survive a restart.
use chrono::{DateTime, Duration as Days, TimeZone, Timelike};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// When a schedule runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum When {
    Every { seconds: u64 },
    Daily { hour: u32, minute: u32 },
}

impl When {
    /// The timetable as you'd say it.
    pub fn label(&self) -> String {
        match self {
            When::Every { seconds } => format!("every {}", crate::session::short_duration(Duration::from_secs(*seconds))),
            When::Daily { hour, minute } => format!("daily at {hour:02}:{minute:02}"),
        }
    }
}

/// One saved job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Schedule {
    pub id: u64,
    pub when: When,
    pub prompt: String,
    /// The saved thread it reports in; it gets a new one if that thread was closed.
    pub thread: String,
    pub cwd: PathBuf,
    pub account_dir: Option<PathBuf>,
    pub codex_account_dir: Option<PathBuf>,
    pub choice: crate::session::Choice,
    /// Seconds since 1970 of the last run, or of creation before the first run.
    pub last_run: i64,
}

/// `08:30` as hour and minute.
pub fn parse_time(text: &str) -> Option<(u32, u32)> {
    let (hour, minute) = text.split_once(':')?;
    let (hour, minute) = (hour.parse().ok()?, minute.parse().ok()?);
    (hour < 24 && minute < 60).then_some((hour, minute))
}

// A run missed while Olympus was closed still happens, once, at the next launch.
/// Whether a job should run now, given when it last ran.
pub fn is_due<Tz: TimeZone>(when: &When, last_run: i64, now: &DateTime<Tz>) -> bool {
    match when {
        When::Every { seconds } => now.timestamp().saturating_sub(last_run) >= i64::try_from(*seconds).unwrap_or(i64::MAX),
        When::Daily { hour, minute } => {
            let today = now.with_hour(*hour).and_then(|time| time.with_minute(*minute)).and_then(|time| time.with_second(0));
            // A clock change can skip a time of day, and then this run waits for tomorrow.
            let Some(today) = today else { return false };
            let latest = if today.timestamp() <= now.timestamp() { today } else { today - Days::days(1) };
            last_run < latest.timestamp()
        }
    }
}

/// Loads saved jobs; a missing file means there are none.
pub fn load(path: &Path) -> Vec<Schedule> {
    std::fs::read_to_string(path).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
}

/// Saves jobs, replacing the file in one step so a crash can't leave half of it.
pub fn save(path: &Path, schedules: &[Schedule]) -> std::io::Result<()> {
    let part = path.with_extension("part");
    std::fs::write(&part, serde_json::to_vec_pretty(schedules)?)?;
    std::fs::rename(part, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    fn at(day: u32, hour: u32, minute: u32) -> DateTime<FixedOffset> {
        FixedOffset::east_opt(3600).expect("offset").with_ymd_and_hms(2026, 11, day, hour, minute, 0).single().expect("time")
    }

    #[test]
    fn times_must_be_real() {
        assert_eq!(parse_time("08:30"), Some((8, 30)));
        assert_eq!(parse_time("8:05"), Some((8, 5)));
        for bad in ["24:00", "08:60", "0830", "soon", ""] {
            assert_eq!(parse_time(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_repeating_job_waits_for_its_interval() {
        let when = When::Every { seconds: 3600 };
        let last = at(5, 8, 0).timestamp();
        assert!(!is_due(&when, last, &at(5, 8, 59)));
        assert!(is_due(&when, last, &at(5, 9, 0)));
    }

    #[test]
    fn a_daily_job_runs_at_its_time_and_only_once() {
        let when = When::Daily { hour: 8, minute: 0 };
        let made_yesterday_evening = at(4, 20, 0).timestamp();
        assert!(!is_due(&when, made_yesterday_evening, &at(5, 7, 59)));
        assert!(is_due(&when, made_yesterday_evening, &at(5, 8, 0)));
        assert!(!is_due(&when, at(5, 8, 0).timestamp(), &at(5, 8, 1)));
        assert!(is_due(&when, at(5, 8, 0).timestamp(), &at(6, 8, 0)));
    }

    #[test]
    fn a_job_created_after_todays_time_waits_for_tomorrow() {
        let when = When::Daily { hour: 8, minute: 0 };
        assert!(!is_due(&when, at(5, 9, 0).timestamp(), &at(5, 9, 1)));
    }

    #[test]
    fn a_daily_run_missed_while_closed_catches_up_once() {
        let when = When::Daily { hour: 8, minute: 0 };
        let ran = at(3, 8, 0).timestamp();
        assert!(is_due(&when, ran, &at(5, 11, 30)));
        assert!(!is_due(&when, at(5, 11, 30).timestamp(), &at(5, 11, 31)));
    }

    #[test]
    fn jobs_survive_a_save_and_load_and_a_missing_file_is_empty() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("schedules.json");
        assert!(load(&path).is_empty());
        let job = Schedule { id: 1, when: When::Daily { hour: 8, minute: 0 }, prompt: "summarize".into(), thread: "abc".into(), cwd: "C:/work".into(), account_dir: None, codex_account_dir: None, choice: crate::session::Choice::Auto, last_run: 5 };
        save(&path, std::slice::from_ref(&job)).expect("save");
        assert_eq!(load(&path), vec![job]);
    }
}
