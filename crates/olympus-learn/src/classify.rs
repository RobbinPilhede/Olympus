const REVIEW: &[&str] = &["review", "audit", "critique", "pr", "diff"];
const QUESTION: &[&str] = &[
    "what", "why", "how", "where", "when", "which", "who", "does", "do", "is", "are", "explain",
    "should",
];
const REQUEST: &[&str] = &["can", "could", "would", "will", "please"];
const DEBUG: &[&str] = &[
    "bug",
    "bugs",
    "fix",
    "fixes",
    "debug",
    "error",
    "errors",
    "crash",
    "crashes",
    "crashing",
    "failing",
    "fails",
    "failure",
    "broken",
    "panic",
    "panics",
    "regression",
    "traceback",
];
const REFACTOR: &[&str] = &[
    "refactor",
    "refactoring",
    "cleanup",
    "simplify",
    "restructure",
    "reorganize",
    "extract",
    "dedupe",
    "deduplicate",
];
const TEST: &[&str] = &["test", "tests", "testing", "coverage", "e2e", "unittest"];
const DOCS: &[&str] = &[
    "docs",
    "documentation",
    "document",
    "readme",
    "changelog",
    "docstring",
    "docstrings",
];
const FEATURE: &[&str] = &[
    "add",
    "implement",
    "build",
    "create",
    "feature",
    "support",
    "introduce",
    "integrate",
    "make",
    "write",
];
const TRIVIAL: &[&str] = &[
    "typo",
    "typos",
    "bump",
    "whitespace",
    "hi",
    "hello",
    "hey",
    "thanks",
    "ok",
    "okay",
    "yes",
    "continue",
];

/// Buckets a prompt by deterministic keywords into one of eight task classes.
pub fn classify_task(prompt: &str) -> &'static str {
    let lower = prompt.trim().to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let Some(first) = words.first() else {
        return "trivial";
    };
    let has = |set: &[&str]| words.iter().any(|w| set.contains(w));
    if words.len() <= 6 && has(TRIVIAL) {
        return "trivial";
    }
    if has(REVIEW) {
        return "review";
    }
    let asks = lower.ends_with('?') || QUESTION.contains(first);
    if asks && !REQUEST.contains(first) {
        return "question";
    }
    if has(DEBUG) {
        return "debug";
    }
    if has(REFACTOR) {
        return "refactor";
    }
    if has(TEST) {
        return "test";
    }
    if has(DOCS) {
        return "docs";
    }
    if has(FEATURE) {
        return "feature";
    }
    if asks {
        return "question";
    }
    if words.len() <= 3 {
        return "trivial";
    }
    "feature"
}

#[cfg(test)]
mod tests {
    use super::classify_task;

    #[test]
    fn buckets_are_keyword_driven() {
        let cases = [
            ("what does this function return", "question"),
            ("why is the build slow?", "question"),
            ("review this PR for race conditions", "review"),
            ("fix the crash in the parser", "debug"),
            ("can you fix the failing login?", "debug"),
            ("refactor the session module", "refactor"),
            ("add tests for the tokenizer", "test"),
            ("update the README with install steps", "docs"),
            ("implement dark mode in settings", "feature"),
            ("fix typo", "trivial"),
            ("thanks", "trivial"),
            ("", "trivial"),
        ];
        for (prompt, expected) in cases {
            assert_eq!(classify_task(prompt), expected, "{prompt}");
        }
    }
}
