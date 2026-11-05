use crate::config::{home_dir, user_home, Config};
use crate::discovery::{self, Bin};
use crate::limits::Limits;
use crate::target::{Provider, Target};
use anyhow::Result;
use olympus_learn::{classify_task, Store};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Cost tier of a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Cheap,
    Standard,
    Strong,
}

impl Tier {
    /// The next tier up, if any.
    pub fn escalate(self) -> Option<Tier> {
        match self {
            Tier::Cheap => Some(Tier::Standard),
            Tier::Standard => Some(Tier::Strong),
            Tier::Strong => None,
        }
    }

    /// The tier a task class starts on when nothing else says otherwise.
    pub fn for_task(class: &str) -> Tier {
        match class {
            "trivial" | "question" | "docs" => Tier::Cheap,
            "test" | "refactor" | "review" => Tier::Standard,
            _ => Tier::Strong,
        }
    }
}

/// Shared state for every session and workflow.
pub struct Runtime {
    pub config: Config,
    pub claude: Option<Bin>,
    pub codex: Option<Bin>,
    pub limits: Arc<Limits>,
    pub learn: Option<Arc<Mutex<Store>>>,
    pub self_exe: PathBuf,
    pub rules: String,
}

impl Runtime {
    /// Loads config, finds CLIs and opens the learning store.
    pub fn load() -> Result<Runtime> {
        let home = home_dir();
        std::fs::create_dir_all(&home)?;
        let config = Config::load(&home.join("config.toml"))?;
        let rules = if config.share_user_rules {
            let text = std::fs::read_to_string(user_home().join(".claude").join("CLAUDE.md")).unwrap_or_default();
            text.chars().take(12_000).collect()
        } else {
            String::new()
        };
        let limits = Arc::new(Limits::persistent(home.join("usage.json")));
        for (dir, account) in discovery::codex_homes(&user_home()) {
            for info in discovery::codex_rate_limits(&dir) {
                limits.update(crate::limits::for_account(&info, account.as_deref()));
            }
        }
        let learn = Store::open(&home.join("learn.db")).ok().map(|s| Arc::new(Mutex::new(s)));
        Ok(Runtime {
            config,
            claude: discovery::find_claude(),
            codex: discovery::find_codex(),
            limits,
            learn,
            self_exe: std::env::current_exe()?,
            rules,
        })
    }

    /// True when the CLI or key behind a provider is present.
    pub fn provider_ready(&self, provider: &Provider) -> bool {
        match provider {
            Provider::Claude => self.claude.is_some(),
            Provider::Codex => self.codex.is_some(),
            Provider::OpenRouter => self.config.openrouter_key().is_some(),
            Provider::Node(_) => true,
        }
    }

    fn tier_candidates(&self, tier: Tier) -> Vec<String> {
        let list = match tier {
            Tier::Cheap => &self.config.tiers.cheap,
            Tier::Standard => &self.config.tiers.standard,
            Tier::Strong => &self.config.tiers.strong,
        };
        list.iter()
            .filter(|spec| match Target::parse(spec) {
                Ok(target) => self.provider_ready(&target.provider) && self.limits.available(target.provider.vendor(), self.config.quota_ceiling),
                Err(_) => false,
            })
            .cloned()
            .collect()
    }

    /// The target an unrouted chat would most likely use next, ignoring the prompt and learned outcomes.
    pub fn likely_target(&self) -> Option<Target> {
        [Tier::Standard, Tier::Strong, Tier::Cheap].iter().find_map(|tier| self.tier_candidates(*tier).first().and_then(|spec| Target::parse(spec).ok()))
    }

    /// Chooses a target for a tier using learned outcomes; falls back up then down when a tier is exhausted.
    pub async fn pick(&self, tier: Tier, prompt: &str, cwd: &Path) -> Result<Target, String> {
        let order = match tier {
            Tier::Cheap => [Tier::Cheap, Tier::Standard, Tier::Strong],
            Tier::Standard => [Tier::Standard, Tier::Strong, Tier::Cheap],
            Tier::Strong => [Tier::Strong, Tier::Standard, Tier::Cheap],
        };
        let Some(candidates) = order.iter().map(|t| self.tier_candidates(*t)).find(|c| !c.is_empty()) else {
            return Err("No model is available: every provider is missing or over its quota.".into());
        };
        let mut chosen = candidates[0].clone();
        if let Some(store) = self.learn.clone() {
            let (class, repo, list) = (classify_task(prompt).to_string(), crate::git::repo_key(cwd), candidates.clone());
            let routed = tokio::task::spawn_blocking(move || store.lock().ok().and_then(|s| s.route(&class, &repo, &list).ok())).await;
            if let Ok(Some(model)) = routed {
                chosen = model;
            }
        }
        Target::parse(&chosen)
    }

    /// Resolves what a person or agent types, like `codex`, `chatgpt`, `opus` or `gpt-6.1-sol`, to a target.
    pub fn resolve(&self, name: &str) -> Result<Target, String> {
        let name = name.trim();
        if name.contains(':') {
            return Target::parse(name);
        }
        let lower = name.to_ascii_lowercase();
        let vendor_top = |vendor: &str| self.config.tiers.strong.iter().find(|s| s.starts_with(vendor)).cloned();
        let spec = match lower.as_str() {
            "codex" | "chatgpt" | "gpt" | "openai" => vendor_top("codex:"),
            "claude" | "anthropic" => vendor_top("claude:"),
            "opus" | "sonnet" | "haiku" | "fable" => Some(format!("claude:{lower}")),
            m if m.starts_with("gpt-") || m.starts_with("codex-") => Some(format!("codex:{m}")),
            _ => None,
        };
        match spec {
            Some(spec) => Target::parse(&spec),
            None => Err(format!("Unknown model '{name}'. Try codex, opus, sonnet, haiku, openrouter:org/model or node:name.")),
        }
    }

    /// A strong model from a different vendor than `other`, for independent review.
    pub fn other_vendor(&self, other: &Provider) -> Option<Target> {
        self.tier_candidates(Tier::Strong)
            .into_iter()
            .filter_map(|spec| Target::parse(&spec).ok())
            .find(|t| t.provider != *other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime() -> Runtime {
        let bin = || Some(Bin { path: PathBuf::from("x"), version: "1".into() });
        Runtime {
            config: Config::default(),
            claude: bin(),
            codex: bin(),
            limits: Arc::new(Limits::default()),
            learn: None,
            self_exe: PathBuf::from("olympus"),
            rules: String::new(),
        }
    }

    fn limit(vendor: &str) -> crate::event::LimitInfo {
        crate::event::LimitInfo { vendor: vendor.into(), window: "w".into(), used: 1.0, resets_at: None, blocked: true }
    }

    #[test]
    fn resolves_what_people_actually_type() {
        let rt = runtime();
        assert_eq!(rt.resolve("chatgpt").expect("chatgpt").provider, Provider::Codex);
        assert_eq!(rt.resolve("opus").expect("opus").to_string(), "claude:opus");
        assert_eq!(rt.resolve("gpt-6.1-sol").expect("slug").to_string(), "codex:gpt-6.1-sol");
        assert_eq!(rt.resolve("openrouter:a/b").expect("full").provider, Provider::OpenRouter);
        assert!(rt.resolve("banana").is_err());
    }

    #[tokio::test]
    async fn pick_fails_over_when_a_vendor_hits_its_limit() {
        let rt = runtime();
        rt.limits.update(limit("claude"));
        let target = rt.pick(Tier::Strong, "build x", Path::new(".")).await.expect("target");
        assert_eq!(target.provider, Provider::Codex);
    }

    #[test]
    fn likely_target_moves_off_a_vendor_at_its_limit() {
        let rt = runtime();
        assert_eq!(rt.likely_target().expect("target").provider, Provider::Claude);
        rt.limits.update(limit("claude"));
        assert_eq!(rt.likely_target().expect("target").provider, Provider::Codex);
        rt.limits.update(limit("codex"));
        assert!(rt.likely_target().is_none());
    }

    #[tokio::test]
    async fn pick_reports_when_nothing_is_left() {
        let rt = runtime();
        rt.limits.update(limit("claude"));
        rt.limits.update(limit("codex"));
        assert!(rt.pick(Tier::Cheap, "x", Path::new(".")).await.is_err());
    }

    #[test]
    fn independent_review_uses_the_other_vendor() {
        let rt = runtime();
        assert_eq!(rt.other_vendor(&Provider::Claude).expect("codex").provider, Provider::Codex);
        rt.limits.update(limit("codex"));
        assert!(rt.other_vendor(&Provider::Claude).is_none());
    }

    #[test]
    fn tiers_escalate_to_a_ceiling() {
        assert_eq!(Tier::Cheap.escalate(), Some(Tier::Standard));
        assert_eq!(Tier::Strong.escalate(), None);
    }
}
