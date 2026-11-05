use crate::event::LimitInfo;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Latest quota window per vendor or account, shared across sessions.
#[derive(Debug, Default)]
pub struct Limits {
    windows: Mutex<HashMap<(String, String), LimitInfo>>,
    file: Option<PathBuf>,
}

/// The short name of an account folder, like `work` for `.claude-work` or `.codex-work`.
pub fn account_name(dir: Option<&Path>) -> String {
    let name = dir.and_then(|d| d.file_name()).map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    match name.strip_prefix(".claude-").or_else(|| name.strip_prefix(".codex-")) {
        Some(short) => short.to_string(),
        None if name.is_empty() || name == ".claude" || name == ".codex" => "default".to_string(),
        None => name,
    }
}

/// Files a quota under its own account, so one account's limit never blocks another.
pub fn for_account(info: &LimitInfo, dir: Option<&Path>) -> LimitInfo {
    let mut info = info.clone();
    if dir.is_some() {
        info.vendor = format!("{}:{}", info.vendor, account_name(dir));
    }
    info
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// A quota window's short name, like `7d`.
pub fn window_word(window: &str) -> String {
    match window {
        "seven_day" => "7d".into(),
        "five_hour" => "5h".into(),
        other => match other.strip_prefix("seven_day_") {
            Some(model) => format!("7d {model}"),
            None => other.to_string(),
        },
    }
}

/// One account's quota, ready to show.
#[derive(Debug, PartialEq)]
pub struct UsageSummary {
    pub detail: String,
    pub used: f32,
    pub alert: bool,
}

/// Summarizes one vendor or account's windows, or None before anything has been reported.
pub fn summarize(limits: &[LimitInfo], vendor: &str) -> Option<UsageSummary> {
    let windows: Vec<&LimitInfo> = limits.iter().filter(|l| l.vendor == vendor).collect();
    if windows.is_empty() {
        return None;
    }
    let blocked = windows.iter().any(|l| l.blocked);
    let used = windows.iter().map(|l| l.used as f32).fold(0.0, f32::max);
    let parts: Vec<String> = windows.iter().map(|l| format!("{} {:.0}%", window_word(&l.window), l.used * 100.0)).collect();
    Some(UsageSummary { detail: format!("{}{}", parts.join("  "), if blocked { "  limit hit" } else { "" }), used: if blocked { 1.0 } else { used.clamp(0.0, 1.0) }, alert: blocked || used >= 0.9 })
}

impl Limits {
    /// Limits remembered in a file, because Claude only reports them during a turn.
    pub fn persistent(file: PathBuf) -> Self {
        let saved: Vec<LimitInfo> = std::fs::read(&file).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default();
        let windows = saved.into_iter().map(|info| ((info.vendor.clone(), info.window.clone()), info)).collect();
        Self { windows: Mutex::new(windows), file: Some(file) }
    }

    /// Records a window reported by a backend.
    pub fn update(&self, info: LimitInfo) {
        let Ok(mut windows) = self.windows.lock() else { return };
        windows.insert((info.vendor.clone(), info.window.clone()), info);
        let Some(file) = &self.file else { return };
        let all: Vec<&LimitInfo> = windows.values().collect();
        // Losing this only costs the numbers shown until the next turn reports them again.
        if let Ok(bytes) = serde_json::to_vec(&all) {
            let temp = file.with_extension("tmp");
            if std::fs::write(&temp, bytes).is_ok() {
                let _ = std::fs::rename(temp, file);
            }
        }
    }

    /// Current windows, sorted by vendor then window; expired windows read as unused.
    pub fn snapshot(&self) -> Vec<LimitInfo> {
        let mut out: Vec<LimitInfo> = match self.windows.lock() {
            Ok(windows) => windows.values().cloned().collect(),
            Err(_) => Vec::new(),
        };
        let now = now();
        for info in &mut out {
            if info.resets_at.is_some_and(|reset| reset <= now) {
                info.used = 0.0;
                info.blocked = false;
            }
        }
        out.sort_by(|a, b| (&a.vendor, &a.window).cmp(&(&b.vendor, &b.window)));
        out
    }

    /// Whether the router may choose this vendor.
    pub fn available(&self, vendor: &str, ceiling: f64) -> bool {
        self.snapshot().iter().filter(|w| w.vendor == vendor).all(|w| !w.blocked && w.used < ceiling)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(used: f64, resets_at: Option<i64>, blocked: bool) -> LimitInfo {
        LimitInfo { vendor: "claude".into(), window: "seven_day".into(), used, resets_at, blocked }
    }

    #[test]
    fn another_accounts_limit_never_blocks_the_default_account() {
        let limits = Limits::default();
        limits.update(for_account(&window(0.95, Some(now() + 100), true), Some(Path::new("C:/u/.claude-work"))));
        assert!(limits.available("claude", 0.9));
        assert_eq!(limits.snapshot()[0].vendor, "claude:work");
        assert_eq!(for_account(&window(0.1, None, false), None).vendor, "claude");
    }

    #[test]
    fn remembered_usage_survives_a_restart_and_a_bad_file_is_ignored() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("usage.json");
        Limits::persistent(file.clone()).update(window(0.25, Some(now() + 100), false));
        assert!((Limits::persistent(file.clone()).snapshot()[0].used - 0.25).abs() < 1e-9);
        std::fs::write(&file, "not json").expect("write");
        assert!(Limits::persistent(file).snapshot().is_empty());
    }

    #[test]
    fn summaries_show_each_window_and_say_when_an_account_is_stuck() {
        let mut work = window(0.61, None, false);
        work.vendor = "claude:work".into();
        let mut five_hour = window(0.12, None, false);
        five_hour.window = "five_hour".into();
        let limits = [five_hour, window(0.23, None, false), work];
        let default = summarize(&limits, "claude").expect("default");
        assert_eq!(default.detail, "5h 12%  7d 23%");
        assert!((default.used - 0.23).abs() < 1e-6, "the bar follows the fullest window");
        assert_eq!(summarize(&limits, "claude:work").expect("work").detail, "7d 61%");
        assert_eq!(summarize(&limits, "codex"), None);
        assert!(summarize(&[window(0.95, None, false)], "claude").expect("near").alert);
        let stuck = summarize(&[window(0.4, None, true)], "claude").expect("stuck");
        assert!(stuck.alert && stuck.detail.ends_with("limit hit") && (stuck.used - 1.0).abs() < 1e-6);
    }

    #[test]
    fn unknown_vendor_is_available() {
        assert!(Limits::default().available("codex", 0.9));
    }

    #[test]
    fn blocked_or_over_ceiling_is_unavailable_until_reset() {
        let limits = Limits::default();
        limits.update(window(0.2, Some(now() + 100), true));
        assert!(!limits.available("claude", 0.9));
        limits.update(window(0.95, Some(now() + 100), false));
        assert!(!limits.available("claude", 0.9));
        limits.update(window(0.95, Some(now() - 1), true));
        assert!(limits.available("claude", 0.9));
    }
}
