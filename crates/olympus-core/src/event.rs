use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Token counts for one turn; `input` excludes cached tokens.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
}

impl Usage {
    /// Tokens weighted by their approximate relative price.
    pub fn weighted(&self) -> u64 {
        self.input + self.cache_read / 10 + self.cache_write * 5 / 4 + self.output * 5
    }

    /// Adds another turn's counts to this one.
    pub fn add(&mut self, other: &Usage) {
        self.input += other.input;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        self.output += other.output;
    }
}

/// A provider quota window as last reported.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LimitInfo {
    pub vendor: String,
    pub window: String,
    /// Fraction used, 0.0 to 1.0.
    pub used: f64,
    /// Unix seconds.
    pub resets_at: Option<i64>,
    pub blocked: bool,
}

/// One normalized event from any backend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AgentEvent {
    Session(String),
    Text(String),
    ToolStart { id: String, name: String, detail: String },
    ToolEnd { id: String, ok: bool, summary: String },
    Status(String),
    Limit(LimitInfo),
}

/// Why a turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stop {
    Completed,
    Failed,
    Cancelled,
    RateLimited,
    AuthError,
    IdleTimeout,
}

/// Final outcome of one turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnResult {
    pub stop: Stop,
    pub session: Option<String>,
    pub text: String,
    pub usage: Usage,
    pub structured: Option<Value>,
    pub error: Option<String>,
    /// Identifies this turn in the learning store.
    #[serde(default)]
    pub run_id: String,
}

impl TurnResult {
    /// Builds a failed result carrying an error message.
    pub fn failed(stop: Stop, error: impl Into<String>) -> Self {
        Self { stop, session: None, text: String::new(), usage: Usage::default(), structured: None, error: Some(error.into()), run_id: String::new() }
    }

    /// True only when the backend reported successful completion.
    pub fn ok(&self) -> bool {
        self.stop == Stop::Completed
    }
}

/// Classifies provider error text into a stop reason.
pub fn classify_error(text: &str) -> Stop {
    let t = text.to_ascii_lowercase();
    if ["rate limit", "rate_limit", "usage limit", "quota", "too many requests", "429", "hit your limit"].iter().any(|k| t.contains(k)) {
        Stop::RateLimited
    } else if ["not logged in", "please run /login", "authentication", "unauthorized", "401", "invalid api key", "oauth", "login"].iter().any(|k| t.contains(k)) {
        Stop::AuthError
    } else {
        Stop::Failed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weighted_prices_cached_and_output_tokens() {
        let usage = Usage { input: 100, cache_read: 1000, cache_write: 400, output: 10 };
        assert_eq!(usage.weighted(), 100 + 100 + 500 + 50);
    }

    #[test]
    fn classifies_limits_and_auth() {
        assert_eq!(classify_error("You've hit your limit, resets 3pm"), Stop::RateLimited);
        assert_eq!(classify_error("Not logged in, please run /login"), Stop::AuthError);
        assert_eq!(classify_error("tool crashed"), Stop::Failed);
    }
}
