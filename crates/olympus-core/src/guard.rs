//! A small block list for shell commands that can't be undone. It catches accidents, not determined evasion.
use serde_json::{json, Value};
use std::io::Read;
use std::path::Path;

/// A harmless phrase the list also blocks, so live tests can prove a hook works without any risk.
pub const TEST_PHRASE: &str = "olympus-guard-test";

const DELETE_VERBS: &[&str] = &["rm", "del", "erase", "rd", "rmdir", "remove-item", "ri", "rimraf"];
const DISK_WIPES: &[&str] = &["format-volume", "clear-disk", "initialize-disk", "remove-partition", "diskpart", "mkfs", "cipher /w:", "sdelete"];
const SYSTEM_DAMAGE: &[&str] = &["vssadmin delete", "wbadmin delete", "bcdedit", "reg delete hklm", "reg delete hkey_local_machine", "remove-item hklm:", ":(){"];
const HOME_NAMES: &[&str] = &["~", "$home", "${home}", "$env:userprofile", "%userprofile%"];
const SYSTEM_NAMES: &[&str] = &["$env:systemdrive", "%systemdrive%", "$env:windir", "%windir%", "$env:systemroot", "%systemroot%", "$env:programfiles", "%programfiles%"];
const UNIX_SYSTEM: &[&str] = &["/users", "/home", "/etc", "/usr", "/bin", "/sbin", "/var", "/boot", "/lib", "/opt", "/root", "/mnt", "/windows"];
const WINDOWS_SYSTEM: &[&str] = &["users", "windows", "program-files", "program-files-(x86)", "programdata"];
const PRIVATE_FOLDERS: &[&str] = &[".ssh", ".claude", ".codex", ".olympus", ".gnupg", ".aws"];

/// The reason a command must not run, or None if it's fine.
pub fn blocked(command: &str) -> Option<&'static str> {
    // Quotes are gone by the time segments split on spaces, so this folder's space has to go too.
    let text = command.to_lowercase().replace('\\', "/").replace(['"', '\'', '`'], "").replace("program files (x86)", "program-files-(x86)").replace("program files", "program-files");
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.contains(TEST_PHRASE) {
        return Some("it contains the block list's test phrase");
    }
    if DISK_WIPES.iter().any(|wipe| flat.contains(wipe)) || (flat.contains("dd ") && flat.contains("of=/dev/")) || flat.contains("> /dev/sd") {
        return Some("it would wipe a disk");
    }
    if SYSTEM_DAMAGE.iter().any(|damage| flat.contains(damage)) {
        return Some("it would damage Windows or its backups");
    }
    text.split(['\n', ';', '|', '&']).find_map(check_segment)
}

fn check_segment(segment: &str) -> Option<&'static str> {
    let tokens: Vec<&str> = segment.split_whitespace().collect();
    if tokens.windows(2).any(|pair| pair[0].trim_end_matches(".com").trim_end_matches(".exe") == "format" && is_drive(pair[1].trim_end_matches('/'))) {
        return Some("it would format a drive");
    }
    if is_forced_push_to_main(&tokens) {
        return Some("it would overwrite the history of the main branch");
    }
    let verb = tokens.iter().position(|token| DELETE_VERBS.contains(&token.trim_end_matches(".exe")))?;
    let arguments = &tokens[verb + 1..];
    let recursive = arguments.iter().any(|argument| is_recursive(argument));
    let single_file_delete = matches!(tokens[verb], "del" | "erase");
    if !recursive && !single_file_delete {
        return None;
    }
    arguments.iter().filter(|argument| !is_flag(argument)).any(|target| is_critical(target)).then_some("it would recursively delete a system, home or credentials folder")
}

fn is_forced_push_to_main(tokens: &[&str]) -> bool {
    let Some(git) = tokens.iter().position(|token| token.trim_end_matches(".exe") == "git") else { return false };
    let arguments = &tokens[git + 1..];
    let forced = arguments.iter().any(|argument| *argument == "--force" || *argument == "-f" || argument.starts_with('+'));
    let branch = arguments.iter().any(|argument| {
        let name = argument.trim_start_matches('+').rsplit(':').next().unwrap_or(argument);
        name.trim_start_matches("refs/heads/") == "main" || name.trim_start_matches("refs/heads/") == "master"
    });
    arguments.contains(&"push") && forced && branch
}

fn is_flag(argument: &str) -> bool {
    argument.starts_with('-') || (argument.len() == 2 && argument.starts_with('/') && argument.ends_with(|c: char| c.is_ascii_alphabetic()) && !is_unix_drive(argument))
}

// `/c` could be a Git Bash drive or cmd's switch, and `rd /s /q` only uses `/s`, `/q`, `/f` and `/a`.
fn is_unix_drive(argument: &str) -> bool {
    !matches!(argument, "/s" | "/q" | "/f" | "/a" | "/p" | "/r")
}

fn is_recursive(argument: &str) -> bool {
    matches!(argument, "--recursive" | "-recurse" | "/s" | "-r") || (argument.starts_with('-') && !argument.starts_with("--") && argument.len() <= 4 && argument.contains('r') && !argument.starts_with("-re"))
}

fn is_drive(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

fn is_critical(target: &str) -> bool {
    let mut path = target.trim_start_matches("-path=");
    loop {
        let trimmed = path.trim_end_matches('*').trim_end_matches('.').trim_end_matches('/');
        if trimmed == path {
            break;
        }
        path = trimmed;
    }
    if path.is_empty() || is_drive(path) || HOME_NAMES.contains(&path) || SYSTEM_NAMES.contains(&path) || UNIX_SYSTEM.contains(&path) {
        return true;
    }
    let parts: Vec<&str> = path.split('/').collect();
    match parts.as_slice() {
        [drive] if drive.len() == 1 => false,
        [home, folder] if HOME_NAMES.contains(home) => PRIVATE_FOLDERS.contains(folder),
        [drive, folder] if is_drive(drive) => WINDOWS_SYSTEM.contains(folder),
        [drive, "users", _] if is_drive(drive) => true,
        [drive, "users", _, folder] if is_drive(drive) => PRIVATE_FOLDERS.contains(folder),
        ["", letter] => letter.len() == 1,
        ["", "mnt", letter] | ["", "cygdrive", letter] => letter.len() == 1,
        ["", "home" | "users", _] | ["", _, "users", _] | ["", "mnt", _, "users", _] => true,
        ["", "home" | "users", _, folder] | ["", _, "users", _, folder] => PRIVATE_FOLDERS.contains(folder),
        _ => false,
    }
}

/// The value for Claude Code's `--settings`, which runs the guard before each shell command.
pub fn claude_settings(exe: &Path) -> String {
    let command = format!("\"{}\" guard", exe.display().to_string().replace('\\', "/"));
    json!({"hooks": {"PreToolUse": [{"matcher": "Bash|PowerShell", "hooks": [{"type": "command", "command": command}]}]}}).to_string()
}

/// The Codex `-c` setting that runs the guard before each shell command.
pub fn codex_setting(exe: &Path) -> String {
    // Codex runs hooks in PowerShell, where a quoted path is only text unless `&` calls it.
    let command = format!("& '{}' guard", exe.display().to_string().replace('\\', "/").replace('\'', "''"));
    format!("hooks.PreToolUse=[{{matcher=\"Bash\",hooks=[{{type=\"command\",command=\"{command}\"}}]}}]")
}

// Codex ignores hook exit codes when approvals are bypassed, while both CLIs honor this reply.
fn hook_reply(input: &str) -> Option<String> {
    let call: Value = serde_json::from_str(input).ok()?;
    let command = match &call["tool_input"]["command"] {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "),
        _ => return None,
    };
    let reason = blocked(&command)?;
    let message = format!("Olympus blocked this command because {reason}. Don't try another way around it. Tell the user what you wanted to run, so they can decide and run it themselves.");
    Some(json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": message}}).to_string())
}

/// Reads a tool call from a CLI's hook on stdin, and prints a refusal if it must not run.
pub fn run_hook() -> i32 {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_ok() {
        if let Some(reply) = hook_reply(&input) {
            println!("{reply}");
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catastrophic_deletes_are_blocked_in_every_shell() {
        for command in [
            "rm -rf /", "rm -rf /*", "sudo rm -fr ~", "rm -rf ~/", "rm -r $HOME", "rm -rf /c/Users/me", "rm -rf /mnt/c", "rm --recursive --force /etc",
            r"rd /s /q C:\", r"rmdir /s /q C:\Users\me", r"cmd /c rd /s /q C:\Windows", r"del /s /q C:\*", "Remove-Item -Recurse -Force C:\\", "Remove-Item C:\\Users\\me -Recurse -Force",
            "Remove-Item -Recurse -Force $env:USERPROFILE", r#"powershell -Command "Remove-Item -Recurse 'C:\Program Files'""#, "cd x && rm -rf ~/.ssh", "ri -r -fo $HOME/.claude", r"rm -rf C:\Users\me\.olympus",
        ] {
            assert!(blocked(command).is_some(), "{command}");
        }
    }

    #[test]
    fn everyday_deletes_are_allowed() {
        for command in [
            "rm file.txt", "rm -rf node_modules", "rm -rf ./target", "rm -rf /tmp/build", "rm -rf ~/projects/old-thing", "rd /s /q build", r"rmdir /s /q C:\ot\rel", "del notes.txt", "Remove-Item -Recurse -Force dist", r"Remove-Item -Recurse C:\Users\me\Desktop\old", "rm -f /", "git rm -r src/old",
        ] {
            assert_eq!(blocked(command), None, "{command}");
        }
    }

    #[test]
    fn disks_and_windows_itself_are_protected() {
        for command in ["format c:", "format.com D: /q", "Format-Volume -DriveLetter C", "Clear-Disk -Number 0", "diskpart /s wipe.txt", "mkfs.ext4 /dev/sda1", "dd if=/dev/zero of=/dev/sda", "vssadmin delete shadows /all", "bcdedit /delete {current}", r"reg delete HKLM\SOFTWARE\x /f", ":(){ :|:& };:"] {
            assert!(blocked(command).is_some(), "{command}");
        }
        for command in ["format the report", "cargo fmt", "dd if=a of=b", r"reg delete HKCU\Software\Mine /f", "grep format src/main.rs"] {
            assert_eq!(blocked(command), None, "{command}");
        }
    }

    #[test]
    fn only_a_plain_force_push_to_main_is_blocked() {
        for command in ["git push --force origin main", "git push -f origin master", "git push origin +main", "git push -f origin HEAD:refs/heads/main"] {
            assert!(blocked(command).is_some(), "{command}");
        }
        for command in ["git push origin main", "git push --force origin feature", "git push --force-with-lease origin main", "git log -f main"] {
            assert_eq!(blocked(command), None, "{command}");
        }
    }

    #[test]
    fn a_clean_command_passes_and_the_test_phrase_is_refused() {
        assert_eq!(blocked("cargo test --workspace && git status"), None);
        assert!(blocked("echo olympus-guard-test").is_some());
    }

    #[test]
    fn the_codex_hook_calls_a_path_with_spaces_the_way_powershell_needs() {
        let setting = codex_setting(Path::new(r"C:\Users\A B\olympus.exe"));
        assert!(setting.contains("& 'C:/Users/A B/olympus.exe' guard"), "{setting}");
    }

    #[test]
    fn the_hook_only_answers_for_a_blocked_command() {
        let deny = hook_reply(r#"{"tool_input":{"command":"rm -rf /"}}"#).expect("a refusal");
        assert!(deny.contains(r#""permissionDecision":"deny""#));
        assert_eq!(hook_reply(r#"{"tool_input":{"command":"cargo test"}}"#), None);
        assert_eq!(hook_reply(r#"{"tool_input":{"file_path":"a"}}"#), None);
        assert_eq!(hook_reply("not json"), None);
    }

    #[test]
    fn the_claude_hook_names_the_shell_tools_with_a_forward_slash_path() {
        let settings = claude_settings(Path::new(r"C:\Program Files\Olympus\olympus.exe"));
        assert!(settings.contains("Bash|PowerShell"));
        assert!(settings.contains("C:/Program Files/Olympus/olympus.exe"));
    }
}
