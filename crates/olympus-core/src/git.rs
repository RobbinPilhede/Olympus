use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use tokio::process::Command;

async fn git_raw(cwd: &Path, args: &[&str]) -> Result<(bool, String)> {
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(cwd).kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
    let output = cmd.output().await.context("running git")?;
    let mut text = String::from_utf8_lossy(&output.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    Ok((output.status.success(), text.trim().to_string()))
}

async fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let (ok, text) = git_raw(cwd, args).await?;
    if ok {
        Ok(text)
    } else {
        Err(anyhow!("git {}: {text}", args.first().copied().unwrap_or("")))
    }
}

/// A short stable key for a directory's repo, used to scope learning.
pub fn repo_key(cwd: &Path) -> String {
    let mut dir = Some(cwd);
    while let Some(current) = dir {
        if current.join(".git").exists() {
            return current.display().to_string();
        }
        dir = current.parent();
    }
    cwd.display().to_string()
}

/// The repo root containing `cwd`, or an error explaining git is required.
pub async fn root(cwd: &Path) -> Result<PathBuf> {
    let text = git(cwd, &["rev-parse", "--show-toplevel"]).await.map_err(|_| anyhow!("{} isn't a git repository. Run `git init` first.", cwd.display()))?;
    Ok(PathBuf::from(text))
}

/// The checked-out branch, or None when HEAD is detached.
pub async fn current_branch(cwd: &Path) -> Result<Option<String>> {
    let (ok, text) = git_raw(cwd, &["symbolic-ref", "--short", "-q", "HEAD"]).await?;
    Ok(ok.then_some(text))
}

/// True when the repo has at least one commit.
pub async fn has_commits(cwd: &Path) -> bool {
    git_raw(cwd, &["rev-parse", "--verify", "HEAD"]).await.map(|(ok, _)| ok).unwrap_or(false)
}

/// True when tracked or untracked changes exist.
pub async fn is_dirty(cwd: &Path) -> Result<bool> {
    Ok(!git(cwd, &["status", "--porcelain"]).await?.is_empty())
}

/// Creates a new branch and a worktree for it.
pub async fn add_worktree(root: &Path, path: &Path, branch: &str, base: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.with_context(|| format!("creating {}", parent.display()))?;
    }
    let location = path.display().to_string();
    git(root, &["worktree", "add", "-b", branch, &location, base]).await.map(drop)
}

/// Removes a worktree without force, so unsaved work makes this fail rather than vanish.
pub async fn remove_worktree(root: &Path, path: &Path) -> Result<()> {
    let location = path.display().to_string();
    git(root, &["worktree", "remove", &location]).await.map(drop)
}

/// Deletes a branch only if it is fully merged.
pub async fn delete_merged_branch(root: &Path, branch: &str) -> Result<()> {
    git(root, &["branch", "-d", branch]).await.map(drop)
}

async fn committer_args(cwd: &Path) -> Vec<&'static str> {
    let configured = git_raw(cwd, &["config", "user.email"]).await.map(|(ok, text)| ok && !text.is_empty()).unwrap_or(false);
    if configured { Vec::new() } else { vec!["-c", "user.name=Olympus", "-c", "user.email=olympus@localhost"] }
}

/// Stages and commits everything; returns false when nothing changed.
pub async fn commit_all(cwd: &Path, message: &str) -> Result<bool> {
    git(cwd, &["add", "-A"]).await?;
    if git_raw(cwd, &["diff", "--cached", "--quiet"]).await?.0 {
        return Ok(false);
    }
    let mut args = committer_args(cwd).await;
    args.extend(["commit", "-q", "--no-verify", "-m", message]);
    git(cwd, &args).await?;
    Ok(true)
}

/// Result of a merge attempt.
#[derive(Debug, PartialEq, Eq)]
pub enum Merge {
    Clean,
    Conflict(Vec<String>),
}

/// Merges a branch into the checked-out branch of `cwd`.
pub async fn merge(cwd: &Path, branch: &str) -> Result<Merge> {
    let mut args = committer_args(cwd).await;
    args.extend(["merge", "--no-ff", "--no-edit", branch]);
    let (ok, text) = git_raw(cwd, &args).await?;
    if ok {
        return Ok(Merge::Clean);
    }
    let conflicts = conflicted_files(cwd).await?;
    if conflicts.is_empty() {
        return Err(anyhow!("git merge failed: {text}"));
    }
    Ok(Merge::Conflict(conflicts))
}

/// Files with unresolved conflicts.
pub async fn conflicted_files(cwd: &Path) -> Result<Vec<String>> {
    Ok(git(cwd, &["diff", "--name-only", "--diff-filter=U"]).await?.lines().map(String::from).collect())
}

/// Finishes a merge after conflicts were resolved.
pub async fn finish_merge(cwd: &Path) -> Result<()> {
    git(cwd, &["add", "-A"]).await?;
    let mut args = committer_args(cwd).await;
    args.extend(["commit", "-q", "--no-verify", "--no-edit"]);
    git(cwd, &args).await.map(drop)
}

/// Abandons an in-progress merge.
pub async fn abort_merge(cwd: &Path) -> Result<()> {
    git(cwd, &["merge", "--abort"]).await.map(drop)
}

/// The diff from `base` to HEAD, capped to `max` bytes keeping head and tail.
pub async fn diff(cwd: &Path, base: &str, max: usize) -> Result<String> {
    let range = format!("{base}...HEAD");
    Ok(crate::tools::clip(&git(cwd, &["diff", &range]).await?, max))
}

/// The diff of uncommitted changes, including new files, capped to `max` bytes.
pub async fn working_diff(cwd: &Path, max: usize) -> Result<String> {
    let mut text = git(cwd, &["diff", "HEAD"]).await?;
    let untracked = git(cwd, &["ls-files", "--others", "--exclude-standard"]).await?;
    if !untracked.is_empty() {
        text.push_str("\n\nNew files:\n");
        text.push_str(&untracked);
    }
    Ok(crate::tools::clip(&text, max))
}

/// Raw `git diff` text: uncommitted changes against HEAD, or the given range.
pub async fn raw_diff(cwd: &Path, range: Option<&str>) -> Result<String> {
    match range {
        Some(range) => git(cwd, &["diff", range]).await,
        None if has_commits(cwd).await => git(cwd, &["diff", "HEAD"]).await,
        None => Ok(String::new()),
    }
}

/// Paths of files git isn't tracking and doesn't ignore.
pub async fn untracked(cwd: &Path) -> Result<Vec<String>> {
    Ok(git(cwd, &["ls-files", "--others", "--exclude-standard"]).await?.lines().map(String::from).collect())
}

/// Infers verification commands from project files.
pub fn detect_verify(root: &Path) -> Vec<String> {
    let mut commands = Vec::new();
    if root.join("Cargo.toml").exists() {
        commands.push("cargo test --quiet".to_string());
    }
    if root.join("package.json").exists() {
        let scripts = std::fs::read_to_string(root.join("package.json")).ok().and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok()).map(|v| v["scripts"].clone());
        let has = |name: &str| scripts.as_ref().is_some_and(|s| s.get(name).is_some());
        let runner = if root.join("pnpm-lock.yaml").exists() { "pnpm" } else if root.join("bun.lockb").exists() || root.join("bun.lock").exists() { "bun" } else { "npm" };
        for script in ["typecheck", "test", "build"] {
            if has(script) {
                commands.push(format!("{runner} run {script}"));
            }
        }
    }
    if root.join("go.mod").exists() {
        commands.push("go test ./...".to_string());
    }
    if root.join("pyproject.toml").exists() || root.join("pytest.ini").exists() {
        commands.push("python -m pytest -q".to_string());
    }
    commands
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    pub async fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        git(dir.path(), &["init", "-q", "-b", "main"]).await.expect("init");
        git(dir.path(), &["config", "user.email", "t@example.com"]).await.expect("email");
        git(dir.path(), &["config", "user.name", "T"]).await.expect("name");
        git(dir.path(), &["config", "core.autocrlf", "false"]).await.expect("line endings");
        std::fs::write(dir.path().join("a.txt"), "one\n").expect("write");
        commit_all(dir.path(), "init").await.expect("commit");
        dir
    }
}

#[cfg(test)]
mod tests {
    use super::testing::repo;
    use super::*;

    #[tokio::test]
    async fn commit_all_reports_no_change_then_change() {
        let repo = repo().await;
        assert!(!commit_all(repo.path(), "none").await.expect("clean"));
        std::fs::write(repo.path().join("b.txt"), "x").expect("write");
        assert!(is_dirty(repo.path()).await.expect("dirty"));
        assert!(commit_all(repo.path(), "add b").await.expect("commit"));
        assert!(!is_dirty(repo.path()).await.expect("clean"));
    }

    #[tokio::test]
    async fn worktree_merge_round_trip() {
        let repo = repo().await;
        let parent = tempfile::tempdir().expect("tempdir");
        let tree = parent.path().join("task");
        add_worktree(repo.path(), &tree, "task", "main").await.expect("add");
        std::fs::write(tree.join("b.txt"), "b").expect("write");
        commit_all(&tree, "b").await.expect("commit");
        remove_worktree(repo.path(), &tree).await.expect("remove");
        assert_eq!(merge(repo.path(), "task").await.expect("merge"), Merge::Clean);
        assert!(repo.path().join("b.txt").exists());
        delete_merged_branch(repo.path(), "task").await.expect("branch -d");
    }

    #[tokio::test]
    async fn removing_a_dirty_worktree_refuses_and_keeps_the_work() {
        let repo = repo().await;
        let parent = tempfile::tempdir().expect("tempdir");
        let tree = parent.path().join("task");
        add_worktree(repo.path(), &tree, "task", "main").await.expect("add");
        std::fs::write(tree.join("unsaved.txt"), "precious").expect("write");
        assert!(remove_worktree(repo.path(), &tree).await.is_err());
        assert!(tree.join("unsaved.txt").exists());
    }

    #[tokio::test]
    async fn unmerged_branch_is_not_deleted() {
        let repo = repo().await;
        let parent = tempfile::tempdir().expect("tempdir");
        let tree = parent.path().join("task");
        add_worktree(repo.path(), &tree, "task", "main").await.expect("add");
        std::fs::write(tree.join("b.txt"), "b").expect("write");
        commit_all(&tree, "b").await.expect("commit");
        remove_worktree(repo.path(), &tree).await.expect("remove");
        assert!(delete_merged_branch(repo.path(), "task").await.is_err());
    }

    #[tokio::test]
    async fn conflict_is_reported_resolvable_and_abortable() {
        let repo = repo().await;
        let parent = tempfile::tempdir().expect("tempdir");
        let tree = parent.path().join("task");
        add_worktree(repo.path(), &tree, "task", "main").await.expect("add");
        std::fs::write(tree.join("a.txt"), "theirs\n").expect("write");
        commit_all(&tree, "theirs").await.expect("commit");
        std::fs::write(repo.path().join("a.txt"), "ours\n").expect("write");
        commit_all(repo.path(), "ours").await.expect("commit");
        assert_eq!(merge(repo.path(), "task").await.expect("merge"), Merge::Conflict(vec!["a.txt".into()]));
        abort_merge(repo.path()).await.expect("abort");
        assert_eq!(std::fs::read_to_string(repo.path().join("a.txt")).expect("read"), "ours\n");
        assert_eq!(merge(repo.path(), "task").await.expect("merge"), Merge::Conflict(vec!["a.txt".into()]));
        std::fs::write(repo.path().join("a.txt"), "both\n").expect("write");
        finish_merge(repo.path()).await.expect("finish");
        assert!(!is_dirty(repo.path()).await.expect("clean"));
    }

    #[tokio::test]
    async fn non_repo_gets_an_actionable_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let error = root(dir.path()).await.expect_err("not a repo").to_string();
        assert!(error.contains("git init"));
    }

    #[test]
    fn detects_verify_commands_from_project_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(detect_verify(dir.path()).is_empty());
        std::fs::write(dir.path().join("Cargo.toml"), "").expect("write");
        std::fs::write(dir.path().join("package.json"), r#"{"scripts":{"test":"x","lint":"y"}}"#).expect("write");
        assert_eq!(detect_verify(dir.path()), ["cargo test --quiet", "npm run test"]);
    }
}
