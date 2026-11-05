use crate::config::user_home;
use crate::event::LimitInfo;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

/// A located CLI binary.
#[derive(Debug, Clone)]
pub struct Bin {
    pub path: PathBuf,
    pub version: String,
}

fn path_dirs() -> Vec<PathBuf> {
    std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default()
}

fn appdata() -> PathBuf {
    std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| user_home().join("AppData").join("Roaming"))
}

fn local_appdata() -> PathBuf {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| user_home().join("AppData").join("Local"))
}

fn version_of(path: &Path) -> Option<String> {
    let mut cmd = Command::new(path);
    cmd.arg("--version");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).split_whitespace().find(|w| w.chars().next().is_some_and(|c| c.is_ascii_digit())).map(String::from)
}

/// Compares dotted versions numerically; prerelease tails rank by their leading numbers.
pub fn version_key(version: &str) -> Vec<u64> {
    version.split(['.', '-']).filter_map(|p| p.parse().ok()).take(3).collect()
}

fn newest(candidates: Vec<PathBuf>) -> Option<Bin> {
    let mut seen = Vec::new();
    candidates
        .into_iter()
        .filter(|p| p.is_file())
        .filter(|p| {
            let fresh = !seen.contains(p);
            seen.push(p.clone());
            fresh
        })
        .filter_map(|path| version_of(&path).map(|version| Bin { path, version }))
        .max_by_key(|bin| version_key(&bin.version))
}

/// Finds the native Claude Code binary so no shell shim sits between Olympus and it.
pub fn find_claude() -> Option<Bin> {
    let mut candidates: Vec<PathBuf> = path_dirs().into_iter().map(|d| d.join("claude.exe")).collect();
    candidates.push(appdata().join("npm/node_modules/@anthropic-ai/claude-code/bin/claude.exe"));
    candidates.push(user_home().join(".local/bin/claude.exe"));
    newest(candidates)
}

fn find_named(dir: &Path, name: &str, depth: u32, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if depth > 0 {
                find_named(&path, name, depth - 1, out);
            }
        } else if path.file_name().is_some_and(|f| f == name) {
            out.push(path);
        }
    }
}

/// Finds every official Codex binary and takes the newest, since an old npm install can reject a newer config.
pub fn find_codex() -> Option<Bin> {
    let mut candidates: Vec<PathBuf> = path_dirs().into_iter().map(|d| d.join("codex.exe")).collect();
    find_named(&local_appdata().join("OpenAI/Codex/bin"), "codex.exe", 1, &mut candidates);
    find_named(&appdata().join("npm/node_modules/@openai/codex"), "codex.exe", 8, &mut candidates);
    newest(candidates)
}

/// A signed-in account directory for a vendor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub vendor: String,
    pub dir: PathBuf,
    pub signed_in: bool,
    pub label: String,
}

fn output_text(path: &Path, args: &[&str], env: Option<(&str, &Path)>) -> Option<(bool, String)> {
    let mut cmd = Command::new(path);
    cmd.args(args);
    if let Some((key, value)) = env {
        cmd.env(key, value);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let output = cmd.output().ok()?;
    let mut text = String::from_utf8_lossy(&output.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    Some((output.status.success(), text))
}

/// Reads who a Codex folder is signed in as; `login status` doesn't say, and tokens stay untouched.
fn codex_identity(dir: &Path) -> Option<String> {
    let auth: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("auth.json")).ok()?).ok()?;
    identity_from_id_token(auth["tokens"]["id_token"].as_str()?)
}

fn identity_from_id_token(token: &str) -> Option<String> {
    let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(token.split('.').nth(1)?.trim_end_matches('=')).ok()?;
    let claims: Value = serde_json::from_slice(&claims).ok()?;
    let email = claims["email"].as_str()?;
    Some(match claims["https://api.openai.com/auth"]["chatgpt_plan_type"].as_str() {
        Some(plan) => format!("{email} ({plan})"),
        None => email.to_string(),
    })
}

/// Every Codex home folder with its account folder, or None for the normal `~/.codex`.
pub fn codex_homes(home: &Path) -> Vec<(PathBuf, Option<PathBuf>)> {
    let mut homes = vec![(home.join(".codex"), None)];
    let extra = std::fs::read_dir(home).into_iter().flatten().flatten().map(|e| e.path());
    homes.extend(extra.filter(|p| p.is_dir() && p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(".codex-"))).map(|p| (p.clone(), Some(p))));
    homes
}

/// Finds Claude and Codex config directories and asks each official CLI whether it is signed in.
pub fn accounts(claude: Option<&Bin>, codex: Option<&Bin>) -> Vec<Account> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(user_home()) else { return out };
    let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    dirs.sort();
    for dir in dirs {
        let name = dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if name.starts_with(".claude") && dir.join(".credentials.json").exists() {
            if let Some(bin) = claude {
                // The default account's profile is in `~/.claude.json`; naming its folder would hide it.
                let env = (name != ".claude").then_some(("CLAUDE_CONFIG_DIR", dir.as_path()));
                if let Some((_, text)) = output_text(&bin.path, &["auth", "status"], env) {
                    let status: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                    let plan = status["subscriptionType"].as_str().unwrap_or("signed out");
                    let email = status["email"].as_str().unwrap_or("");
                    out.push(Account {
                        vendor: "claude".into(),
                        dir: dir.clone(),
                        signed_in: status["loggedIn"].as_bool().unwrap_or(false),
                        label: format!("{email} ({plan})").trim().to_string(),
                    });
                }
            }
        }
        if name.starts_with(".codex") && dir.join("auth.json").exists() {
            if let Some(bin) = codex {
                if let Some((ok, text)) = output_text(&bin.path, &["login", "status"], Some(("CODEX_HOME", &dir))) {
                    let label = codex_identity(&dir).unwrap_or_else(|| text.lines().last().unwrap_or("").trim().to_string());
                    out.push(Account { vendor: "codex".into(), dir: dir.clone(), signed_in: ok, label });
                }
            }
        }
    }
    out
}

/// A past CLI session that can be reopened.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PastSession {
    pub vendor: String,
    pub id: String,
    pub cwd: PathBuf,
    pub title: String,
    pub modified: u64,
}

fn modified_secs(path: &Path) -> u64 {
    std::fs::metadata(path).and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs())
}

fn first_text(content: &Value) -> Option<String> {
    let text = match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks.iter().find_map(|b| b["text"].as_str().map(String::from))?,
        _ => return None,
    };
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.starts_with('<') {
        return None;
    }
    Some(trimmed.lines().next().unwrap_or("").chars().take(90).collect())
}

fn read_lines_capped(path: &Path, max_lines: usize, mut on: impl FnMut(&Value) -> bool) {
    let Ok(file) = std::fs::File::open(path) else { return };
    let mut reader = BufReader::with_capacity(1 << 16, file.take(8 << 20));
    let mut line = String::new();
    for _ in 0..max_lines {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                if let Ok(value) = serde_json::from_str::<Value>(&line) {
                    if on(&value) {
                        return;
                    }
                }
            }
        }
    }
}

fn claude_session(path: &Path) -> Option<PastSession> {
    let id = path.file_stem()?.to_string_lossy().to_string();
    let mut cwd = None;
    let mut title = None;
    read_lines_capped(path, 60, |v| {
        if cwd.is_none() {
            cwd = v["cwd"].as_str().map(PathBuf::from);
        }
        if v["type"] == "ai-title" {
            title = v["aiTitle"].as_str().map(String::from);
        } else if title.is_none() && v["type"] == "user" && v["isMeta"] != true {
            title = first_text(&v["message"]["content"]);
        }
        cwd.is_some() && title.is_some()
    });
    Some(PastSession { vendor: "claude".into(), id, cwd: cwd?, title: title.unwrap_or_else(|| "Untitled session".into()), modified: modified_secs(path) })
}

fn codex_session(path: &Path) -> Option<PastSession> {
    let mut cwd = None;
    let mut id = None;
    let mut title = None;
    read_lines_capped(path, 40, |v| {
        if v["type"] == "session_meta" {
            cwd = v["payload"]["cwd"].as_str().map(PathBuf::from);
            id = v["payload"]["id"].as_str().map(String::from);
        } else if v["type"] == "event_msg" && v["payload"]["type"] == "user_message" {
            title = first_text(&v["payload"]["message"]);
        }
        id.is_some() && title.is_some()
    });
    Some(PastSession { vendor: "codex".into(), id: id?, cwd: cwd?, title: title.unwrap_or_else(|| "Untitled session".into()), modified: modified_secs(path) })
}

fn newest_files(mut files: Vec<PathBuf>, limit: usize) -> Vec<PathBuf> {
    files.sort_by_cached_key(|p| std::cmp::Reverse(modified_secs(p)));
    files.truncate(limit);
    files
}

/// Lists recent Claude and Codex sessions, newest first.
pub fn past_sessions(claude_dir: &Path, codex_dir: &Path, limit: usize) -> Vec<PastSession> {
    let mut claude_files = Vec::new();
    for project in std::fs::read_dir(claude_dir.join("projects")).into_iter().flatten().flatten() {
        for file in std::fs::read_dir(project.path()).into_iter().flatten().flatten() {
            let path = file.path();
            if path.extension().is_some_and(|e| e == "jsonl") {
                claude_files.push(path);
            }
        }
    }
    let mut codex_files = Vec::new();
    collect_rollouts(&codex_dir.join("sessions"), 3, &mut codex_files);
    let mut sessions: Vec<PastSession> = newest_files(claude_files, limit).iter().filter_map(|p| claude_session(p)).collect();
    sessions.extend(newest_files(codex_files, limit).iter().filter_map(|p| codex_session(p)));
    sessions.sort_by_key(|s| std::cmp::Reverse(s.modified));
    sessions.truncate(limit);
    sessions
}

fn collect_rollouts(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() && depth > 0 {
            collect_rollouts(&path, depth - 1, out);
        } else if path.extension().is_some_and(|e| e == "jsonl") {
            out.push(path);
        }
    }
}

/// Reads Codex quota from the tail of the newest rollout that reports it.
pub fn codex_rate_limits(codex_dir: &Path) -> Vec<LimitInfo> {
    let mut files = Vec::new();
    collect_rollouts(&codex_dir.join("sessions"), 3, &mut files);
    for path in newest_files(files, 5) {
        if let Some(found) = rollout_limits(&path) {
            return found;
        }
    }
    Vec::new()
}

fn rollout_limits(path: &Path) -> Option<Vec<LimitInfo>> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(256 * 1024))).ok()?;
    let mut text = Vec::new();
    file.read_to_end(&mut text).ok()?;
    let text = String::from_utf8_lossy(&text);
    for line in text.lines().rev() {
        if !line.contains("\"rate_limits\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let limits = &v["payload"]["rate_limits"];
        let mut out = Vec::new();
        for key in ["primary", "secondary"] {
            let window = &limits[key];
            if let Some(used) = window["used_percent"].as_f64() {
                let minutes = window["window_minutes"].as_i64().unwrap_or(0);
                out.push(LimitInfo {
                    vendor: "codex".into(),
                    window: if minutes >= 10_000 { "seven_day".into() } else if minutes > 0 { format!("{}h", minutes / 60) } else { key.to_string() },
                    used: used / 100.0,
                    resets_at: window["resets_at"].as_i64(),
                    blocked: used >= 100.0,
                });
            }
        }
        if !out.is_empty() {
            return Some(out);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prerelease_versions_outrank_older_stable() {
        assert!(version_key("0.164.0-alpha.11.1") > version_key("0.134.0"));
        assert!(version_key("2.1.309") > version_key("2.1.31"));
    }

    #[test]
    fn lists_sessions_from_both_formats_newest_last_modified_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = dir.path().join("claude/projects/C--x");
        std::fs::create_dir_all(&project).expect("project");
        std::fs::write(project.join("abc.jsonl"), concat!(
            "{\"type\":\"user\",\"cwd\":\"C:\\\\x\",\"isMeta\":true,\"message\":{\"content\":\"hidden\"}}\n",
            "{\"type\":\"user\",\"cwd\":\"C:\\\\x\",\"message\":{\"content\":\"Fix the login bug\\nmore\"}}\n",
        )).expect("write");
        let rollouts = dir.path().join("codex/sessions/2026/11/04");
        std::fs::create_dir_all(&rollouts).expect("rollouts");
        std::fs::write(rollouts.join("rollout-1.jsonl"), concat!(
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"cid\",\"cwd\":\"C:\\\\y\"}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"Ship it\"}}\n",
        )).expect("write");
        let sessions = past_sessions(&dir.path().join("claude"), &dir.path().join("codex"), 10);
        assert_eq!(sessions.len(), 2);
        assert!(sessions.iter().any(|s| s.vendor == "claude" && s.id == "abc" && s.title == "Fix the login bug"));
        assert!(sessions.iter().any(|s| s.vendor == "codex" && s.id == "cid" && s.title == "Ship it"));
    }

    #[test]
    fn reads_codex_quota_from_the_latest_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let rollouts = dir.path().join("sessions/2026/11/04");
        std::fs::create_dir_all(&rollouts).expect("rollouts");
        std::fs::write(rollouts.join("r.jsonl"), concat!(
            "{\"payload\":{\"rate_limits\":{\"primary\":{\"used_percent\":10.0,\"window_minutes\":10080,\"resets_at\":5}}}}\n",
            "{\"payload\":{\"rate_limits\":{\"primary\":{\"used_percent\":42.0,\"window_minutes\":10080,\"resets_at\":9}}}}\n",
        )).expect("write");
        let limits = codex_rate_limits(dir.path());
        assert_eq!(limits.len(), 1);
        assert_eq!(limits[0].window, "seven_day");
        assert!((limits[0].used - 0.42).abs() < 1e-9);
    }

    #[test]
    fn no_sessions_directory_means_no_limits() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(codex_rate_limits(dir.path()).is_empty());
    }

    #[test]
    fn a_codex_identity_is_the_email_and_plan_from_the_id_token() {
        let encode = |json: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json);
        let token = format!("h.{}.s", encode(r#"{"email":"me@example.com","https://api.openai.com/auth":{"chatgpt_plan_type":"pro"}}"#));
        assert_eq!(identity_from_id_token(&token).as_deref(), Some("me@example.com (pro)"));
        let no_plan = format!("h.{}.s", encode(r#"{"email":"me@example.com"}"#));
        assert_eq!(identity_from_id_token(&no_plan).as_deref(), Some("me@example.com"));
        assert_eq!(identity_from_id_token("not a token"), None);
    }

    #[test]
    fn codex_homes_lists_the_default_and_each_account_folder() {
        let home = tempfile::tempdir().expect("home");
        std::fs::create_dir(home.path().join(".codex-work")).expect("folder");
        std::fs::create_dir(home.path().join(".claude-work")).expect("folder");
        let work = home.path().join(".codex-work");
        assert_eq!(codex_homes(home.path()), [(home.path().join(".codex"), None), (work.clone(), Some(work))]);
    }
}
