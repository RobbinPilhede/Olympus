use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::process::Command;

const MAX_OUTPUT: usize = 12_000;

/// Function schemas offered to OpenRouter and node models.
pub fn definitions(read_only: bool) -> Value {
    let mut tools = vec![
        function("read_file", "Read a text file. Lines are numbered.", json!({"path": {"type": "string"}, "offset": {"type": "integer", "description": "First line, 1-based"}, "limit": {"type": "integer"}}), &["path"]),
        function("search", "Find a regex across the project with ripgrep or git grep.", json!({"pattern": {"type": "string"}, "path": {"type": "string"}}), &["pattern"]),
    ];
    if !read_only {
        tools.push(function("write_file", "Create or overwrite a file.", json!({"path": {"type": "string"}, "content": {"type": "string"}}), &["path", "content"]));
        tools.push(function("edit_file", "Replace one exact, unique string in a file.", json!({"path": {"type": "string"}, "old": {"type": "string"}, "new": {"type": "string"}}), &["path", "old", "new"]));
        tools.push(function("shell", "Run a PowerShell command in the project directory.", json!({"command": {"type": "string"}, "timeout_secs": {"type": "integer"}}), &["command"]));
    }
    Value::Array(tools)
}

fn function(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({"type": "function", "function": {"name": name, "description": description, "parameters": {"type": "object", "properties": properties, "required": required}}})
}

/// Keeps the head and tail of long output, which is where errors and results sit.
pub fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut head = max * 2 / 3;
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len() - (max - head);
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    format!("{}\n… {} bytes omitted …\n{}", &text[..head], text.len() - head - (text.len() - tail), &text[tail..])
}

fn resolve(cwd: &Path, path: &str) -> PathBuf {
    let candidate = Path::new(path);
    if candidate.is_absolute() { candidate.to_path_buf() } else { cwd.join(candidate) }
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args[key].as_str().ok_or_else(|| format!("Missing '{key}'."))
}

/// Runs one tool call; errors go back to the model as text so it can recover.
pub async fn execute(name: &str, args: &Value, cwd: &Path, read_only: bool) -> Result<String, String> {
    match name {
        "read_file" => read_file(args, cwd).await,
        "search" => search(args, cwd).await,
        "write_file" | "edit_file" | "shell" if read_only => Err(format!("{name} isn't available in a read-only consult.")),
        "write_file" => {
            let path = resolve(cwd, str_arg(args, "path")?);
            if let Some(dir) = path.parent() {
                tokio::fs::create_dir_all(dir).await.map_err(|e| e.to_string())?;
            }
            let content = str_arg(args, "content")?;
            tokio::fs::write(&path, content).await.map_err(|e| format!("{}: {e}", path.display()))?;
            Ok(format!("Wrote {} bytes to {}.", content.len(), path.display()))
        }
        "edit_file" => edit_file(args, cwd).await,
        "shell" => shell(args, cwd).await,
        other => Err(format!("Unknown tool '{other}'.")),
    }
}

async fn read_file(args: &Value, cwd: &Path) -> Result<String, String> {
    let path = resolve(cwd, str_arg(args, "path")?);
    let text = tokio::fs::read_to_string(&path).await.map_err(|e| format!("{}: {e}", path.display()))?;
    let offset = args["offset"].as_u64().unwrap_or(1).max(1) as usize;
    let limit = args["limit"].as_u64().unwrap_or(2000) as usize;
    let numbered: Vec<String> = text.lines().enumerate().skip(offset - 1).take(limit).map(|(i, l)| format!("{}\t{l}", i + 1)).collect();
    Ok(clip(&numbered.join("\n"), MAX_OUTPUT))
}

async fn edit_file(args: &Value, cwd: &Path) -> Result<String, String> {
    let path = resolve(cwd, str_arg(args, "path")?);
    let (old, new) = (str_arg(args, "old")?, str_arg(args, "new")?);
    let text = tokio::fs::read_to_string(&path).await.map_err(|e| format!("{}: {e}", path.display()))?;
    match text.matches(old).count() {
        0 => Err("'old' wasn't found. Read the file again and copy the exact text.".into()),
        1 => {
            tokio::fs::write(&path, text.replacen(old, new, 1)).await.map_err(|e| format!("{}: {e}", path.display()))?;
            Ok(format!("Edited {}.", path.display()))
        }
        n => Err(format!("'old' matches {n} places. Include more surrounding text.")),
    }
}

async fn search(args: &Value, cwd: &Path) -> Result<String, String> {
    let pattern = str_arg(args, "pattern")?;
    let path = args["path"].as_str().unwrap_or(".");
    let mut cmd = Command::new("rg");
    cmd.args(["-n", "--max-count", "50", "--", pattern, path]).current_dir(cwd).kill_on_drop(true);
    let output = match cmd.output().await {
        Ok(output) => output,
        Err(_) => {
            let mut git = Command::new("git");
            git.args(["grep", "-n", "-E", "--", pattern, path]).current_dir(cwd).kill_on_drop(true);
            git.output().await.map_err(|e| format!("Neither rg nor git could run: {e}"))?
        }
    };
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(if text.trim().is_empty() { "No matches.".into() } else { clip(&text, MAX_OUTPUT) })
}

async fn shell(args: &Value, cwd: &Path) -> Result<String, String> {
    let command = str_arg(args, "command")?;
    if let Some(reason) = crate::guard::blocked(command) {
        return Err(format!("Olympus blocked this command because {reason}. Tell the user what you wanted to run, so they can decide and run it themselves."));
    }
    let timeout = args["timeout_secs"].as_u64().unwrap_or(120).min(1800);
    let (code, output) = run_shell(command, cwd, Duration::from_secs(timeout)).await?;
    Ok(format!("exit {code}\n{}", clip(&output, MAX_OUTPUT)))
}

/// Runs a PowerShell command and returns its exit code with combined output.
pub async fn run_shell(command: &str, cwd: &Path, timeout: Duration) -> Result<(i32, String), String> {
    let mut cmd = Command::new("powershell");
    cmd.args(["-NoProfile", "-NonInteractive", "-Command", command]).current_dir(cwd).kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
    let output = tokio::time::timeout(timeout, cmd.output())
        .await
        .map_err(|_| format!("Stopped after {} seconds.", timeout.as_secs()))?
        .map_err(|e| e.to_string())?;
    let mut text = String::from_utf8_lossy(&output.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    Ok((output.status.code().unwrap_or(-1), text))
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn the_shell_tool_refuses_a_blocked_command_without_running_it() {
        let dir = tempfile::tempdir().expect("dir");
        let phrase = crate::guard::TEST_PHRASE;
        let refused = super::execute("shell", &serde_json::json!({"command": format!("Set-Content proof.txt {phrase}")}), dir.path(), false).await.expect_err("refused");
        assert!(refused.contains("blocked"), "{refused}");
        assert!(!dir.path().join("proof.txt").exists());
    }

    use super::*;

    #[test]
    fn clip_keeps_both_ends_and_respects_utf8() {
        let text = format!("{}{}{}", "start-", "é".repeat(5000), "-end");
        let clipped = clip(&text, 100);
        assert!(clipped.starts_with("start-") && clipped.ends_with("-end") && clipped.contains("omitted"));
        assert_eq!(clip("short", 100), "short");
    }

    #[tokio::test]
    async fn edit_requires_a_unique_match() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "x x y").expect("write");
        let args = json!({"path": "a.txt", "old": "x", "new": "z"});
        assert!(execute("edit_file", &args, dir.path(), false).await.expect_err("ambiguous").contains("2 places"));
        let args = json!({"path": "a.txt", "old": "y", "new": "z"});
        execute("edit_file", &args, dir.path(), false).await.expect("edit");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "x x z");
    }

    #[tokio::test]
    async fn read_only_blocks_every_mutating_tool() {
        let dir = tempfile::tempdir().expect("tempdir");
        for (name, args) in [("write_file", json!({"path": "a", "content": "b"})), ("edit_file", json!({"path": "a", "old": "b", "new": "c"})), ("shell", json!({"command": "echo hi"}))] {
            assert!(execute(name, &args, dir.path(), true).await.is_err(), "{name}");
        }
        assert!(!dir.path().join("a").exists());
    }

    #[tokio::test]
    async fn read_only_definitions_omit_mutating_tools() {
        let names = definitions(true).to_string();
        assert!(names.contains("read_file") && !names.contains("write_file") && !names.contains("\"shell\""));
    }

    #[tokio::test]
    async fn shell_reports_exit_code() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = execute("shell", &json!({"command": "Write-Output hi; exit 7"}), dir.path(), false).await.expect("runs");
        assert!(out.starts_with("exit 7") && out.contains("hi"));
    }

    #[tokio::test]
    async fn read_file_numbers_the_requested_window() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree").expect("write");
        let out = execute("read_file", &json!({"path": "a.txt", "offset": 2, "limit": 1}), dir.path(), true).await.expect("read");
        assert_eq!(out, "2\ttwo");
    }
}
