use olympus_core::git;
use std::collections::HashSet;
use std::path::Path;

const MAX_LINES_PER_FILE: usize = 20_000;
const MAX_NEW_FILE_BYTES: u64 = 256 * 1024;

/// What a diff line is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Hunk,
    Context,
    Add,
    Remove,
}

/// One line of a diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old: Option<u32>,
    pub new: Option<u32>,
    pub text: String,
}

/// One changed file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileDiff {
    pub path: String,
    pub added: usize,
    pub removed: usize,
    pub binary: bool,
    pub lines: Vec<DiffLine>,
}

/// Which changes the review panel shows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DiffSource {
    #[default]
    Working,
    Branch(String),
}

/// The review panel's state for one chat.
#[derive(Debug, Clone, Default)]
pub struct DiffState {
    pub source: DiffSource,
    pub files: Vec<FileDiff>,
    pub collapsed: HashSet<String>,
    pub loading: bool,
    pub stale: bool,
    pub loaded: bool,
    pub error: Option<String>,
}

fn hunk_starts(header: &str) -> (u32, u32) {
    let mut parts = header.split_whitespace().skip(1);
    let mut start = |prefix: char| parts.next().and_then(|p| p.trim_start_matches(prefix).split(',').next()).and_then(|n| n.parse().ok()).unwrap_or(1);
    let old = start('-');
    (old, start('+'))
}

fn path_from_header(line: &str) -> String {
    let rest = line.trim_start_matches("diff --git ").trim();
    match rest.split_once(" b/") {
        Some((_, path)) => path.trim_matches('"').to_string(),
        None => rest.to_string(),
    }
}

/// Parses `git diff` output.
pub fn parse(text: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let (mut old, mut new) = (0, 0);
    for line in text.lines() {
        if line.starts_with("diff --git ") {
            files.push(FileDiff { path: path_from_header(line), ..FileDiff::default() });
            continue;
        }
        let Some(file) = files.last_mut() else { continue };
        if line.starts_with("Binary files ") {
            file.binary = true;
        } else if line.starts_with("@@") {
            (old, new) = hunk_starts(line);
            file.lines.push(DiffLine { kind: LineKind::Hunk, old: None, new: None, text: line.to_string() });
        } else if file.lines.is_empty() {
            if let Some(path) = line.strip_prefix("rename to ") {
                file.path = path.to_string();
            }
        } else if let Some(body) = line.strip_prefix('+') {
            file.added += 1;
            if file.lines.len() < MAX_LINES_PER_FILE {
                file.lines.push(DiffLine { kind: LineKind::Add, old: None, new: Some(new), text: body.to_string() });
            }
            new += 1;
        } else if let Some(body) = line.strip_prefix('-') {
            file.removed += 1;
            if file.lines.len() < MAX_LINES_PER_FILE {
                file.lines.push(DiffLine { kind: LineKind::Remove, old: Some(old), new: None, text: body.to_string() });
            }
            old += 1;
        } else if !line.starts_with('\\') && file.lines.len() < MAX_LINES_PER_FILE {
            let body = line.strip_prefix(' ').unwrap_or(line);
            file.lines.push(DiffLine { kind: LineKind::Context, old: Some(old), new: Some(new), text: body.to_string() });
            old += 1;
            new += 1;
        }
    }
    files
}

/// Shows a new, untracked file as all additions; None means it can't be shown as text.
pub fn new_file(path: &str, content: Option<&str>) -> FileDiff {
    let Some(content) = content else {
        return FileDiff { path: path.to_string(), binary: true, ..FileDiff::default() };
    };
    let lines: Vec<DiffLine> = content.lines().take(MAX_LINES_PER_FILE).enumerate().map(|(i, text)| DiffLine { kind: LineKind::Add, old: None, new: Some(i as u32 + 1), text: text.to_string() }).collect();
    FileDiff { path: path.to_string(), added: lines.len(), removed: 0, binary: false, lines }
}

fn read_new_file(root: &Path, path: &str) -> Option<String> {
    let full = root.join(path);
    if std::fs::metadata(&full).ok()?.len() > MAX_NEW_FILE_BYTES {
        return None;
    }
    String::from_utf8(std::fs::read(full).ok()?).ok()
}

/// Loads the changes for a folder.
pub async fn load(cwd: &Path, source: &DiffSource) -> Result<Vec<FileDiff>, String> {
    let root = git::root(cwd).await.map_err(|error| error.to_string())?;
    let range = match source {
        DiffSource::Working => None,
        DiffSource::Branch(branch) => Some(format!("HEAD...{branch}")),
    };
    let text = git::raw_diff(&root, range.as_deref()).await.map_err(|error| format!("{error:#}"))?;
    let mut files = parse(&text);
    if *source == DiffSource::Working {
        for path in git::untracked(&root).await.map_err(|error| format!("{error:#}"))? {
            let content = read_new_file(&root, &path);
            files.push(new_file(&path, content.as_deref()));
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "diff --git a/src/a.rs b/src/a.rs\nindex 1..2 100644\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -3,3 +3,4 @@ fn x()\n keep\n-old\n+new\n+more\n tail\n\\ No newline at end of file\ndiff --git a/logo.png b/logo.png\nBinary files a/logo.png and b/logo.png differ\ndiff --git a/old.rs b/new.rs\nsimilarity index 100%\nrename from old.rs\nrename to new.rs\n";

    #[test]
    fn parses_files_counts_and_line_numbers() {
        let files = parse(SAMPLE);
        assert_eq!(files.len(), 3);
        let first = &files[0];
        assert_eq!((first.path.as_str(), first.added, first.removed), ("src/a.rs", 2, 1));
        assert_eq!(first.lines[0].kind, LineKind::Hunk);
        assert_eq!((first.lines[1].old, first.lines[1].new), (Some(3), Some(3)));
        assert_eq!(first.lines[2].old, Some(4));
        assert_eq!(first.lines[3].new, Some(4));
        assert_eq!((first.lines[5].old, first.lines[5].new), (Some(5), Some(6)));
        assert_eq!(first.lines.len(), 6, "the no-newline marker isn't a line");
    }

    #[test]
    fn binary_and_renamed_files_are_marked() {
        let files = parse(SAMPLE);
        assert!(files[1].binary);
        assert_eq!(files[2].path, "new.rs");
    }

    #[test]
    fn header_lines_never_count_as_changes() {
        let files = parse("diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n");
        assert_eq!((files[0].added, files[0].removed), (1, 1));
    }

    #[test]
    fn new_files_are_all_additions_and_binaries_say_so() {
        let file = new_file("n.txt", Some("a\nb\n"));
        assert_eq!((file.added, file.lines[1].new), (2, Some(2)));
        assert!(new_file("b.bin", None).binary);
    }

    #[tokio::test]
    async fn loads_tracked_and_new_files_from_a_real_repo_and_explains_non_repos() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(load(dir.path(), &DiffSource::Working).await.expect_err("not a repo").contains("git init"));
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git").args(args).current_dir(dir.path()).output().expect("git");
            assert!(output.status.success(), "{args:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(dir.path().join("new.txt"), "fresh\n").expect("write");
        let before_commit = load(dir.path(), &DiffSource::Working).await.expect("no commits yet");
        assert_eq!(before_commit[0].path, "new.txt");
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "T"]);
        git(&["config", "core.autocrlf", "false"]);
        git(&["add", "-A"]);
        git(&["commit", "-qm", "init"]);
        assert!(load(dir.path(), &DiffSource::Working).await.expect("clean").is_empty());
        std::fs::write(dir.path().join("new.txt"), "changed\n").expect("write");
        std::fs::write(dir.path().join("other.txt"), "x\n").expect("write");
        let files = load(dir.path(), &DiffSource::Working).await.expect("dirty");
        assert_eq!(files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), ["new.txt", "other.txt"]);
        assert_eq!((files[0].added, files[0].removed), (1, 1));
    }
}
