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

const DRAFTS: &str = "skill-drafts";

/// A skill's folder name: lowercase letters, digits and hyphens only.
pub fn clean_skill_name(name: &str) -> Option<String> {
    let cleaned: String = name.trim().to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let cleaned = cleaned.split('-').filter(|part| !part.is_empty()).collect::<Vec<_>>().join("-");
    (!cleaned.is_empty() && cleaned.len() <= 64).then_some(cleaned)
}

/// The instruction that turns a finished conversation into a skill draft.
pub fn draft_prompt(transcript: &str) -> String {
    format!("Below is a conversation in which a task got done. Write a reusable skill that captures how to do this kind of task again: the steps that worked, the checks to run, and the mistakes to avoid. Leave out details that only apply to this one run.\n\nReply with only the file's contents, in this form:\n---\nname: short-hyphenated-name\ndescription: One sentence saying what it does and when to use it.\n---\n\nThe instructions, in markdown.\n\nConversation:\n{transcript}")
}

/// Checks a model's draft, returning its name and contents; `wanted` overrides the name it chose.
pub fn parse_draft(reply: &str, wanted: Option<&str>) -> Result<(String, String), String> {
    const BAD: &str = "The model's draft wasn't a valid skill. Try /save-skill again.";
    let text = reply.trim();
    let text = text.strip_prefix("```markdown").or_else(|| text.strip_prefix("```md")).or_else(|| text.strip_prefix("```")).map_or(text, |inner| inner.trim_end().trim_end_matches("```").trim());
    let (front, body) = text.strip_prefix("---").and_then(|rest| rest.split_once("\n---")).ok_or(BAD)?;
    let field = |key: &str| front.lines().find_map(|line| line.trim().strip_prefix(key)?.strip_prefix(':').map(|value| value.trim().trim_matches('"').to_string())).filter(|value| !value.is_empty());
    let description = field("name").and(field("description")).ok_or(BAD)?;
    if body.trim().is_empty() {
        return Err(BAD.into());
    }
    let name = wanted.map(str::to_string).or_else(|| field("name")).and_then(|name| clean_skill_name(&name)).ok_or(BAD)?;
    let contents = format!("---\nname: {name}\ndescription: {description}\n---\n{}\n", body.trim_end());
    Ok((name, contents))
}

/// Saves a draft where it isn't used yet, returning its path.
pub fn save_draft(olympus_home: &Path, name: &str, contents: &str) -> std::io::Result<PathBuf> {
    let folder = olympus_home.join(DRAFTS).join(name);
    std::fs::create_dir_all(&folder)?;
    let path = folder.join("SKILL.md");
    std::fs::write(&path, contents)?;
    Ok(path)
}

/// Moves an approved draft into your skills. It never replaces a skill you already have.
pub fn keep_draft(olympus_home: &Path, user_home: &Path, name: &str) -> Result<PathBuf, String> {
    let name = clean_skill_name(name).ok_or("Give the draft's name, from /save-skill.")?;
    let draft = olympus_home.join(DRAFTS).join(&name);
    if !draft.join("SKILL.md").is_file() {
        return Err(format!("There's no draft named {name}. Make one with /save-skill."));
    }
    let skills = user_home.join(".claude").join("skills");
    let target = skills.join(&name);
    if target.exists() {
        return Err(format!("You already have a skill named {name}. Remove it first, or use /save-skill with another name."));
    }
    std::fs::create_dir_all(&skills).map_err(|error| error.to_string())?;
    std::fs::rename(&draft, &target).map_err(|error| error.to_string())?;
    Ok(target.join("SKILL.md"))
}

/// Deletes a draft you didn't want.
pub fn drop_draft(olympus_home: &Path, name: &str) -> Result<(), String> {
    let name = clean_skill_name(name).ok_or("Give the draft's name, from /save-skill.")?;
    std::fs::remove_dir_all(olympus_home.join(DRAFTS).join(&name)).map_err(|_| format!("There's no draft named {name}."))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, dir: &str, text: &str) {
        std::fs::create_dir_all(root.join(dir)).expect("dir");
        std::fs::write(root.join(dir).join("SKILL.md"), text).expect("write");
    }

    #[test]
    fn a_good_draft_parses_and_the_fence_and_quotes_go() {
        let reply = "```markdown\n---\nname: Fix Flaky Tests!\ndescription: \"Finds flaky tests.\"\n---\n\n# Steps\nRun them twice.\n```";
        let (name, contents) = parse_draft(reply, None).expect("draft");
        assert_eq!(name, "fix-flaky-tests");
        assert!(contents.starts_with("---\nname: fix-flaky-tests\ndescription: Finds flaky tests.\n---\n"), "{contents}");
        assert!(contents.contains("Run them twice.") && !contents.contains("```"));
        assert_eq!(parse_draft(reply, Some("my-name")).expect("draft").0, "my-name");
    }

    #[test]
    fn a_draft_without_its_parts_is_refused() {
        for reply in ["Sure, here is a skill!", "---\nname: a\n---\nbody", "---\ndescription: b\n---\nbody", "---\nname: a\ndescription: b\n---\n   "] {
            assert!(parse_draft(reply, None).is_err(), "{reply}");
        }
    }

    #[test]
    fn names_cannot_escape_their_folder() {
        assert_eq!(clean_skill_name("../../evil").as_deref(), Some("evil"));
        assert_eq!(clean_skill_name(".."), None);
    }

    #[test]
    fn a_draft_is_only_used_once_kept_and_never_replaces_a_skill() {
        let root = tempfile::tempdir().expect("root");
        let (home, olympus) = (root.path().join("home"), root.path().join("home").join(".olympus"));
        let draft = save_draft(&olympus, "fix-tests", "first").expect("save");
        assert!(draft.is_file() && !home.join(".claude/skills/fix-tests").exists());
        let kept = keep_draft(&olympus, &home, "fix-tests").expect("keep");
        assert_eq!(std::fs::read_to_string(&kept).expect("kept"), "first");
        assert!(!draft.exists());
        save_draft(&olympus, "fix-tests", "second").expect("save again");
        assert!(keep_draft(&olympus, &home, "fix-tests").expect_err("refused").contains("already have"));
        assert_eq!(std::fs::read_to_string(&kept).expect("kept"), "first");
        drop_draft(&olympus, "fix-tests").expect("drop");
        assert!(keep_draft(&olympus, &home, "fix-tests").is_err());
        assert!(drop_draft(&olympus, "fix-tests").is_err());
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
