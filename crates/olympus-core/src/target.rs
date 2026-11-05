use serde::{Deserialize, Serialize};
use std::fmt;

/// Where a turn runs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Provider {
    Claude,
    Codex,
    OpenRouter,
    Node(String),
}

impl Provider {
    /// Stable lowercase label used for limits and learning.
    pub fn vendor(&self) -> &str {
        match self {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
            Provider::OpenRouter => "openrouter",
            Provider::Node(_) => "node",
        }
    }
}

/// A provider, model and optional reasoning effort, written `provider:model@effort`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Target {
    pub provider: Provider,
    pub model: String,
    pub effort: Option<String>,
}

impl Target {
    /// Parses `claude:opus`, `codex:gpt-6.1-sol@high`, `openrouter:org/model`, `node:host` or `node:host/model`.
    pub fn parse(spec: &str) -> Result<Target, String> {
        let spec = spec.trim();
        let (provider, rest) = spec
            .split_once(':')
            .ok_or_else(|| format!("'{spec}' needs a provider, like claude:opus or codex:gpt-6.1-sol"))?;
        let (model, effort) = match rest.rsplit_once('@') {
            Some((m, e)) if !e.is_empty() => (m, Some(e.to_string())),
            _ => (rest, None),
        };
        let provider = match provider {
            "claude" => Provider::Claude,
            "codex" => Provider::Codex,
            "openrouter" => Provider::OpenRouter,
            "node" => {
                let (host, model) = model.split_once('/').unwrap_or((model, ""));
                if host.is_empty() {
                    return Err("node targets need a name, like node:gaming-pc".into());
                }
                return Ok(Target { provider: Provider::Node(host.to_string()), model: model.to_string(), effort });
            }
            other => return Err(format!("Unknown provider '{other}'. Use claude, codex, openrouter or node.")),
        };
        if model.is_empty() {
            return Err(format!("'{spec}' is missing a model name"));
        }
        Ok(Target { provider, model: model.to_string(), effort })
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.provider {
            Provider::Node(host) if self.model.is_empty() => write!(f, "node:{host}")?,
            Provider::Node(host) => write!(f, "node:{host}/{}", self.model)?,
            p => write!(f, "{}:{}", p.vendor(), self.model)?,
        }
        if let Some(effort) = &self.effort {
            write!(f, "@{effort}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_provider() {
        for spec in ["claude:opus", "codex:gpt-6.1-sol@high", "openrouter:moonshot/kimi-k2", "node:gaming-pc", "node:gaming-pc/qwen3@low"] {
            assert_eq!(Target::parse(spec).map(|t| t.to_string()).as_deref(), Ok(spec));
        }
    }

    #[test]
    fn rejects_unknown_and_incomplete_specs() {
        assert!(Target::parse("opus").is_err());
        assert!(Target::parse("foo:bar").is_err());
        assert!(Target::parse("claude:").is_err());
        assert!(Target::parse("node:").is_err());
    }
}
