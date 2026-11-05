const STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "from", "into", "you", "your", "are", "was",
    "were", "but", "not", "have", "has", "had", "can", "could", "would", "should", "will", "what",
    "when", "where", "which", "why", "how", "who", "there", "here", "then", "than", "them", "they",
    "its", "our", "out", "all", "any", "use", "using", "please", "just", "don", "doesn", "didn",
    "isn", "aren", "wasn", "won", "also", "some", "does", "did", "been", "being", "about", "need",
    "want", "like", "get", "let", "make", "now", "one", "very",
];
const MAX_TERMS: usize = 32;
const TOKEN_LEN: usize = 32;
const SECRET_PREFIXES: &[&str] = &[
    "sk-",
    "ghp_",
    "gho_",
    "github_pat_",
    "xoxb-",
    "xoxp-",
    "AKIA",
];

// Terms hold only alphanumerics, which is what makes quoting them for FTS5 injection-safe.
pub(crate) fn terms(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let lower = text.to_lowercase();
    for word in lower.split(|c: char| !c.is_alphanumeric()) {
        if word.chars().count() < 3 || STOPWORDS.contains(&word) {
            continue;
        }
        if !out.iter().any(|seen| seen == word) {
            out.push(word.to_owned());
            if out.len() == MAX_TERMS {
                break;
            }
        }
    }
    out
}

pub(crate) fn fts_join(terms: &[String], operator: &str) -> String {
    let quoted: Vec<String> = terms.iter().map(|t| format!("\"{t}\"")).collect();
    quoted.join(operator)
}

pub(crate) fn truncate(text: &str, max: usize) -> &str {
    match text.char_indices().nth(max) {
        Some((end, _)) => &text[..end],
        None => text,
    }
}

pub(crate) fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(is_token_char) {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let end = tail.find(|c| !is_token_char(c)).unwrap_or(tail.len());
        let word = &tail[..end];
        if word.len() >= TOKEN_LEN || SECRET_PREFIXES.iter().any(|p| word.starts_with(p)) {
            out.push_str("[redacted]");
        } else {
            out.push_str(word);
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+' | '=')
}

pub(crate) fn strip_reminders(text: &str) -> String {
    const OPEN: &str = "<system-reminder>";
    const CLOSE: &str = "</system-reminder>";
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        match rest[start..].find(CLOSE) {
            Some(end) => rest = &rest[start + end + CLOSE.len()..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

pub(crate) fn is_wrapper(text: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "Caveat:",
        "[Request interrupted",
        "This session is being continued",
        "<INSTRUCTIONS>",
        "# AGENTS.md instructions",
    ];
    if PREFIXES.iter().any(|p| text.starts_with(p)) {
        return true;
    }
    let Some(tag) = text.strip_prefix('<').and_then(|t| t.split('>').next()) else {
        return false;
    };
    (tag.contains('-') || tag.contains('_'))
        && tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// True when a message looks like the user correcting the previous answer.
pub fn is_correction(text: &str) -> bool {
    const MAX_CHARS: usize = 400;
    const STARTS: &[&str] = &["no,", "no.", "no!", "no ", "nope"];
    const PHRASES: &[&str] = &["don't", "dont ", "do not", "not what"];
    const WORDS: &[&str] = &["instead", "wrong", "stop"];
    if text.chars().count() > MAX_CHARS {
        return false;
    }
    let lower = text.trim().to_lowercase();
    STARTS.iter().any(|s| lower.starts_with(s))
        || PHRASES.iter().any(|p| lower.contains(p))
        || lower
            .split(|c: char| !c.is_alphanumeric())
            .any(|w| WORDS.contains(&w))
}

pub(crate) fn correction_key(text: &str) -> String {
    const FILLER: &[&str] = &["instead", "wrong", "stop", "nope"];
    let words: Vec<String> = terms(text)
        .into_iter()
        .filter(|t| !FILLER.contains(&t.as_str()))
        .take(5)
        .collect();
    words.join(" ")
}

// Keeps only program and subcommand: arguments are where credentials end up.
pub(crate) fn command_key(command: &str) -> String {
    const SKIP_PROGRAMS: &[&str] = &[
        "cd",
        "export",
        "set",
        "source",
        "pushd",
        "set-location",
        "for",
        "foreach",
        "if",
        "while",
        "done",
        "fi",
    ];
    const PREFIXES: &[&str] = &["sudo", "time", "env", "timeout", "do", "then", "else"];
    const WITH_SUBCOMMAND: &[&str] = &[
        "cargo", "git", "npm", "pnpm", "yarn", "bun", "npx", "docker", "go", "dotnet", "gh",
        "make", "pip", "uv", "kubectl", "rustup",
    ];
    let lines = command.lines().filter(|l| !l.trim_start().starts_with('#'));
    for segment in lines.flat_map(|l| l.split(['|', ';', '&'])).map(str::trim) {
        let mut words = segment.split_whitespace().skip_while(|w| {
            w.contains('=')
                || w.starts_with('$')
                || w.starts_with(|c: char| c.is_ascii_digit())
                || PREFIXES.contains(w)
        });
        let Some(program) = words.next() else {
            continue;
        };
        let name = program_name(program);
        let is_word = name.starts_with(char::is_alphanumeric);
        if !is_word || SKIP_PROGRAMS.contains(&name.as_str()) {
            continue;
        }
        if WITH_SUBCOMMAND.contains(&name.as_str()) {
            let sub = words.find(|w| !w.starts_with('-')).filter(|s| {
                s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':'))
            });
            if let Some(sub) = sub {
                return format!("{name} {sub}");
            }
        }
        return name;
    }
    String::new()
}

fn program_name(program: &str) -> String {
    let trimmed = program.trim_matches(|c| matches!(c, '"' | '\'' | '`' | '('));
    let base = trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed);
    let lower = base.to_lowercase();
    truncate(lower.strip_suffix(".exe").unwrap_or(&lower), 40).to_owned()
}

// These exit non-zero to mean "nothing found", which is not a failure worth learning from.
pub(crate) fn is_probe_command(key: &str) -> bool {
    const PROBES: &[&str] = &[
        "grep",
        "rg",
        "ls",
        "cat",
        "find",
        "test",
        "which",
        "where",
        "head",
        "tail",
        "sed",
        "awk",
        "echo",
        "sleep",
        "wc",
        "get-content",
        "select-string",
        "test-path",
        "get-childitem",
        "get-command",
        "dir",
        "type",
        "diff",
        "true",
        "false",
        "[",
    ];
    let program = key.split(' ').next().unwrap_or(key);
    PROBES.contains(&program)
}

pub(crate) fn first_error_line(output: &str) -> String {
    const NOISE: &[&str] = &[
        "exit code",
        "script failed",
        "script completed",
        "wall time",
        "output:",
        "script error",
        "process exited",
    ];
    const ERROR_WORDS: &[&str] = &["error", "fail", "not found", "denied", "cannot", "panic"];
    let lines: Vec<&str> = output
        .lines()
        .map(|l| {
            l.trim()
                .trim_start_matches("<tool_use_error>")
                .trim_end_matches("</tool_use_error>")
                .trim()
        })
        .filter(|l| {
            let lower = l.to_lowercase();
            l.chars().any(char::is_alphanumeric) && !NOISE.iter().any(|n| lower.starts_with(n))
        })
        .collect();
    let telling = lines.iter().find(|l| {
        let lower = l.to_lowercase();
        ERROR_WORDS.iter().any(|w| lower.contains(w))
    });
    let chosen = telling.or(lines.first()).copied().unwrap_or("");
    redact(truncate(chosen, 160))
}

// Only the earliest status counts: later matches are usually text inside the command's output.
pub(crate) fn has_nonzero_exit(output: &str) -> bool {
    const NEEDLES: &[&str] = &["exit code", "exited with code", "exit_code"];
    let head = truncate(output, 400).to_lowercase();
    let first = NEEDLES
        .iter()
        .filter_map(|needle| head.find(needle).map(|at| at + needle.len()))
        .min();
    let Some(after) = first else {
        return false;
    };
    let digits: String = head[after..]
        .trim_start_matches([' ', ':', '=', '"'])
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .collect();
    digits.parse::<i64>().is_ok_and(|code| code != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_key_drops_arguments_and_setup() {
        assert_eq!(
            command_key("cd foo && cargo test --all -- --nocapture"),
            "cargo test"
        );
        assert_eq!(command_key("FOO=1 npm run build"), "npm run");
        assert_eq!(command_key("\"C:\\tools\\Thing.exe\" --token abc"), "thing");
        assert_eq!(command_key("curl -H 'Authorization: x' http://a"), "curl");
        assert_eq!(command_key("timeout 30 cargo build"), "cargo build");
        assert_eq!(command_key("for f in a b; do cargo fmt; done"), "cargo fmt");
        assert_eq!(
            command_key("$p=Get-Item -LiteralPath x; git log"),
            "git log"
        );
        assert_eq!(command_key("@' | node"), "node");
        assert_eq!(command_key("# setup\ncd x\ncargo run"), "cargo run");
    }

    #[test]
    fn redact_masks_credentials_but_keeps_words() {
        let redacted = redact("key sk-abc and ghp_xyz plus 0123456789abcdef0123456789abcdef ok");
        assert_eq!(redacted, "key [redacted] and [redacted] plus [redacted] ok");
    }

    #[test]
    fn corrections_require_short_text_and_a_marker() {
        assert!(is_correction("No, use tabs"));
        assert!(is_correction("that's not what I asked"));
        assert!(is_correction("use pnpm instead"));
        assert!(!is_correction("nothing to see, add a login page"));
        assert!(!is_correction(&format!("don't {}", "x".repeat(500))));
    }

    #[test]
    fn wrappers_are_detected_without_catching_html_prompts() {
        assert!(is_wrapper("<command-name>/foo</command-name>"));
        assert!(is_wrapper("<environment_context>x</environment_context>"));
        assert!(!is_wrapper("<div> is misaligned"));
        assert_eq!(
            strip_reminders("a<system-reminder>b</system-reminder>c"),
            "ac"
        );
    }

    #[test]
    fn exit_status_parsing_ignores_zero() {
        assert!(has_nonzero_exit("Exit code 1\nboom"));
        assert!(has_nonzero_exit("{\"exit_code\":2}"));
        assert!(!has_nonzero_exit("exit code: 0"));
        assert!(!has_nonzero_exit("all good"));
        assert!(!has_nonzero_exit(
            "{\"exit_code\":0,\"output\":\"exit code 1\"}"
        ));
    }

    #[test]
    fn error_line_skips_boilerplate() {
        assert_eq!(
            first_error_line("Exit code 1\n\nerror: boom"),
            "error: boom"
        );
        assert_eq!(
            first_error_line("<tool_use_error>nope</tool_use_error>"),
            "nope"
        );
        assert_eq!(
            first_error_line("Compiling x\nerror: boom\nmore"),
            "error: boom"
        );
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate("åäö", 2), "åä");
        assert_eq!(truncate("ab", 5), "ab");
    }
}
