use serde::Serialize;
use std::path::{Path, PathBuf};

/// An installed skill, loaded on demand so unused ones cost no tokens.
#[derive(Debug, Clone, Serialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

fn frontmatter_value(text: &str, key: &str) -> Option<String> {
    let body = text.strip_prefix("---")?;
    let end = body.find("\n---")?;
    body[..end].lines().find_map(|line| {
        let value = line.strip_prefix(key)?.strip_prefix(':')?.trim();
        Some(value.trim_matches(['"', '\'']).to_string())
    })
}

fn scan(root: &Path, depth: u32, out: &mut Vec<Skill>) {
    for entry in std::fs::read_dir(root).into_iter().flatten().flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let file = path.join("SKILL.md");
        if let Ok(text) = std::fs::read_to_string(&file) {
            let fallback = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            out.push(Skill {
                name: frontmatter_value(&text, "name").unwrap_or(fallback),
                description: frontmatter_value(&text, "description").unwrap_or_default(),
                path: file,
            });
        } else if depth > 0 {
            scan(&path, depth - 1, out);
        }
    }
}

/// Lists skills under the given roots, deduplicated by name and sorted.
pub fn installed(roots: &[PathBuf]) -> Vec<Skill> {
    let mut skills = Vec::new();
    for root in roots {
        scan(root, 1, &mut skills);
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills.dedup_by(|a, b| a.name == b.name);
    skills
}

/// The skill roots for this user.
pub fn default_roots(home: &Path) -> Vec<PathBuf> {
    vec![home.join(".claude").join("skills"), home.join(".codex").join("skills"), home.join(".agents").join("skills")]
}

// Plugins keep skills at `cache/<marketplace>/<plugin>/<version>/skills`.
fn plugin_skills(home: &Path) -> Vec<Skill> {
    let children = |dir: &Path| -> Vec<PathBuf> { std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect() };
    let mut out = Vec::new();
    for marketplace in children(&home.join(".claude").join("plugins").join("cache")) {
        for plugin in children(&marketplace) {
            let prefix = plugin.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            for version in children(&plugin) {
                out.extend(installed(&[version.join("skills")]).into_iter().map(|skill| Skill { name: format!("{prefix}:{}", skill.name), ..skill }));
            }
        }
    }
    out
}

/// Every skill reachable from a folder: yours, the project's and your plugins'.
pub fn everything(home: &Path, cwd: &Path) -> Vec<Skill> {
    let mut roots = default_roots(home);
    roots.push(cwd.join(".claude").join("skills"));
    let mut all = installed(&roots);
    all.extend(plugin_skills(home));
    all.sort_by(|a, b| a.name.cmp(&b.name));
    all.dedup_by(|a, b| a.name == b.name);
    all
}

/// Turns a skill into a prompt, for providers that have no skill system of their own.
pub fn prompt_for(skill: &Skill, args: &str) -> std::io::Result<String> {
    let text = std::fs::read_to_string(&skill.path)?;
    let body = text.strip_prefix("---").and_then(|rest| rest.find("\n---").map(|end| rest[end + 4..].trim_start())).unwrap_or(&text);
    let folder = skill.path.parent().map(|p| p.display().to_string()).unwrap_or_default();
    let body = if body.contains("$ARGUMENTS") { body.replace("$ARGUMENTS", args) } else if args.is_empty() { body.to_string() } else { format!("{body}\n\nTask: {args}") };
    Ok(format!("Follow the {} skill below. Files it mentions are relative to {folder}.\n\n{body}", skill.name))
}

/// Filters skills whose name or description mention every query word.
pub fn search<'a>(skills: &'a [Skill], query: &str) -> Vec<&'a Skill> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    skills
        .iter()
        .filter(|skill| {
            let haystack = format!("{} {}", skill.name, skill.description).to_lowercase();
            words.iter().all(|word| haystack.contains(word))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, dir: &str, text: &str) {
        std::fs::create_dir_all(root.join(dir)).expect("dir");
        std::fs::write(root.join(dir).join("SKILL.md"), text).expect("write");
    }

    #[test]
    fn reads_frontmatter_falls_back_to_folder_and_dedupes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (a, b) = (temp.path().join("a"), temp.path().join("b"));
        write_skill(&a, "tdd", "---\nname: tdd-workflow\ndescription: \"Red green refactor\"\n---\nbody");
        write_skill(&a, "plain", "no frontmatter");
        write_skill(&b, "other", "---\nname: tdd-workflow\ndescription: duplicate\n---\n");
        let skills = installed(&[a, b]);
        assert_eq!(skills.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["plain", "tdd-workflow"]);
        assert_eq!(skills[1].description, "Red green refactor");
        assert_eq!(search(&skills, "RED refactor").len(), 1);
        assert!(search(&skills, "kubernetes").is_empty());
    }

    #[test]
    fn everything_includes_project_and_plugin_skills() {
        let (home, cwd) = (tempfile::tempdir().expect("home"), tempfile::tempdir().expect("cwd"));
        write_skill(&home.path().join(".claude/skills"), "mine", "---\nname: mine\n---\n");
        write_skill(&cwd.path().join(".claude/skills"), "ours", "---\nname: ours\n---\n");
        write_skill(&home.path().join(".claude/plugins/cache/shop/marketing/1.0/skills"), "seo", "---\nname: seo\n---\n");
        write_skill(&home.path().join(".claude/plugins/cache/shop/marketing/2.0/skills"), "seo", "---\nname: seo\n---\n");
        let names: Vec<String> = everything(home.path(), cwd.path()).into_iter().map(|s| s.name).collect();
        assert_eq!(names, ["marketing:seo", "mine", "ours"]);
    }

    #[test]
    fn a_skill_prompt_drops_the_frontmatter_and_carries_the_task() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_skill(temp.path(), "a", "---\nname: a\ndescription: secret words\n---\nDo the thing.");
        write_skill(temp.path(), "b", "---\nname: b\n---\nFix $ARGUMENTS now.");
        let skills = installed(&[temp.path().to_path_buf()]);
        let first = prompt_for(&skills[0], "the login").expect("prompt");
        assert!(first.contains("Do the thing.") && first.ends_with("Task: the login") && !first.contains("secret words"), "{first}");
        assert!(prompt_for(&skills[1], "the login").expect("prompt").ends_with("Fix the login now."));
    }

    #[test]
    fn missing_roots_are_empty_not_errors() {
        assert!(installed(&[PathBuf::from("Z:/does/not/exist")]).is_empty());
    }
}
