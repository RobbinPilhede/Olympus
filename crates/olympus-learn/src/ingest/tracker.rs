use std::collections::HashMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::sink::{
    Ctx, Sink, ERROR_FIX, FAILING_COMMAND, INTERRUPTED, PERMISSION_DENIAL, TOOL_ERROR,
};
use crate::text::{first_error_line, is_probe_command};

const MAX_OPEN_CALLS: usize = 256;
const MAX_FAILING: usize = 32;
const MAX_EDITS: usize = 3;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct OpenCall {
    key: String,
    is_command: bool,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Failing {
    error: String,
    edits: Vec<String>,
}

pub(super) enum ToolOutcome<'a> {
    Ok,
    Failed(&'a str),
    Denied,
    Interrupted,
}

// Serialised per file, because a call and its result can land on either side of a resume.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub(super) struct Tracker {
    calls: HashMap<String, OpenCall>,
    failing: HashMap<String, Failing>,
    pub(super) assistant_seen: bool,
}

impl Tracker {
    pub(super) fn call(&mut self, id: &str, key: String, is_command: bool) {
        if self.calls.len() >= MAX_OPEN_CALLS {
            self.calls.clear();
        }
        self.calls
            .insert(id.to_owned(), OpenCall { key, is_command });
    }

    pub(super) fn is_open(&self, id: &str) -> bool {
        self.calls.contains_key(id)
    }

    pub(super) fn edited(&mut self, file: &str) {
        let name = file.rsplit(['/', '\\']).next().unwrap_or(file);
        for failing in self.failing.values_mut() {
            if failing.edits.len() < MAX_EDITS && !failing.edits.iter().any(|e| e == name) {
                failing.edits.push(name.to_owned());
            }
        }
    }

    pub(super) fn result(
        &mut self,
        ctx: Ctx,
        id: &str,
        outcome: ToolOutcome,
        sink: &mut Sink,
    ) -> Result<()> {
        let Some(call) = self.calls.remove(id) else {
            return Ok(());
        };
        match outcome {
            ToolOutcome::Denied => sink.signal(ctx, PERMISSION_DENIAL, id, &call.key, "")?,
            ToolOutcome::Interrupted => sink.signal(ctx, INTERRUPTED, id, &call.key, "")?,
            ToolOutcome::Failed(output) => {
                let error = first_error_line(output);
                let learnable = call.is_command && !is_probe_command(&call.key);
                let kind = if learnable {
                    FAILING_COMMAND
                } else {
                    TOOL_ERROR
                };
                sink.signal(ctx, kind, id, &call.key, &error)?;
                if learnable {
                    if self.failing.len() >= MAX_FAILING {
                        self.failing.clear();
                    }
                    let edits = Vec::new();
                    self.failing.insert(call.key, Failing { error, edits });
                }
            }
            ToolOutcome::Ok => {
                if let Some(failing) = self.failing.remove(&call.key) {
                    let detail = if failing.edits.is_empty() {
                        format!("{}; passed on retry", failing.error)
                    } else {
                        format!(
                            "{}; fixed by editing {}",
                            failing.error,
                            failing.edits.join(", ")
                        )
                    };
                    sink.signal(ctx, ERROR_FIX, id, &call.key, &detail)?;
                }
            }
        }
        Ok(())
    }
}
