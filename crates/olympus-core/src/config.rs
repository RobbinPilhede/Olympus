use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The user-level Olympus directory, `~/.olympus`.
pub fn home_dir() -> PathBuf {
    user_home().join(".olympus")
}

/// The user's home directory.
pub fn user_home() -> PathBuf {
    std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

/// Models per cost tier; each tier lists candidates the router chooses between.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tiers {
    pub cheap: Vec<String>,
    pub standard: Vec<String>,
    pub strong: Vec<String>,
}

/// `~/.olympus/config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Target for new chats; `auto` routes each turn by learned outcomes.
    pub default_target: String,
    pub tiers: Tiers,
    pub openrouter_key: Option<String>,
    pub node_token: String,
    /// Node name to `host:port`, for machines UDP discovery can't reach.
    pub nodes: BTreeMap<String, String>,
    pub max_parallel_agents: usize,
    pub idle_timeout_secs: u64,
    /// Carry `~/.claude/CLAUDE.md` into every provider's system prompt.
    pub share_user_rules: bool,
    /// Fraction of quota used at which the router stops choosing a vendor.
    pub quota_ceiling: f64,
    pub max_fix_rounds: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            default_target: "claude:opus".into(),
            tiers: Tiers {
                cheap: vec!["claude:haiku".into(), "codex:gpt-6-luna@low".into()],
                standard: vec!["claude:sonnet".into(), "codex:gpt-6.1-terra@medium".into()],
                strong: vec!["claude:opus".into(), "codex:gpt-6.1-sol@high".into()],
            },
            openrouter_key: None,
            node_token: "olympus".into(),
            nodes: BTreeMap::new(),
            max_parallel_agents: 4,
            idle_timeout_secs: 600,
            share_user_rules: true,
            quota_ceiling: 0.95,
            max_fix_rounds: 2,
        }
    }
}

impl Config {
    /// Loads the config, creating it with defaults on first run.
    pub fn load(path: &Path) -> Result<Config> {
        if !path.exists() {
            let config = Config::default();
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
            }
            std::fs::write(path, toml::to_string_pretty(&config)?).with_context(|| format!("writing {}", path.display()))?;
            return Ok(config);
        }
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// The OpenRouter key from config, then the environment.
    pub fn openrouter_key(&self) -> Option<String> {
        self.openrouter_key.clone().or_else(|| std::env::var("OPENROUTER_API_KEY").ok()).filter(|k| !k.is_empty())
    }
}

/// Saves or removes the OpenRouter key in `config.toml`, leaving the rest of the file as it is.
pub fn save_openrouter_key(path: &Path, key: Option<&str>) -> Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let updated = with_openrouter_key(&text, key);
    toml::from_str::<Config>(&updated).context("the updated settings wouldn't load")?;
    // A half-written file would stop Olympus from starting.
    let temporary = path.with_extension("toml.tmp");
    std::fs::write(&temporary, updated).with_context(|| format!("writing {}", temporary.display()))?;
    std::fs::rename(&temporary, path).with_context(|| format!("writing {}", path.display()))
}

// Top-level TOML keys must come before the first table, so the key goes first.
fn with_openrouter_key(text: &str, key: Option<&str>) -> String {
    let mut top_level = true;
    let rest: String = text
        .split_inclusive('\n')
        .filter(|line| {
            top_level = top_level && !line.trim_start().starts_with('[');
            !(top_level && line.split('=').next().is_some_and(|name| name.trim() == "openrouter_key"))
        })
        .collect();
    match key {
        Some(key) => format!("openrouter_key = {}\n{rest}", toml::Value::String(key.to_string())),
        None => rest,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saving_the_openrouter_key_keeps_everything_else_in_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let original = "# my notes\r\ndefault_target = \"auto\"\nopenrouter_key = \"old\"\n\n[nodes]\nopenrouter_key = \"host:1\"\n";
        std::fs::write(&path, original).expect("write");
        save_openrouter_key(&path, Some("sk-or-new")).expect("save");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("# my notes\r\n"), "{text}");
        assert!(!text.contains("\"old\""), "{text}");
        let config = Config::load(&path).expect("load");
        assert_eq!(config.openrouter_key.as_deref(), Some("sk-or-new"));
        assert_eq!(config.default_target, "auto");
        assert_eq!(config.nodes["openrouter_key"], "host:1", "a node's name isn't the key");
        save_openrouter_key(&path, None).expect("remove");
        let config = Config::load(&path).expect("load");
        assert_eq!(config.openrouter_key, None);
        assert_eq!(config.nodes["openrouter_key"], "host:1");
    }

    #[test]
    fn a_broken_settings_file_is_left_alone_when_saving_the_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "default_target = ").expect("write");
        assert!(save_openrouter_key(&path, Some("sk-or-new")).is_err());
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "default_target = ");
    }

    #[test]
    fn creates_defaults_then_keeps_user_edits() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("config.toml");
        assert_eq!(Config::load(&path).expect("create").default_target, "claude:opus");
        std::fs::write(&path, "default_target = \"auto\"\n").expect("edit");
        let config = Config::load(&path).expect("reload");
        assert_eq!(config.default_target, "auto");
        assert_eq!(config.node_token, "olympus");
    }

    #[test]
    fn invalid_toml_is_an_error_not_a_silent_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "default_target = ").expect("write");
        assert!(Config::load(&path).is_err());
    }
}
