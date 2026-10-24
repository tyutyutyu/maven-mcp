//! Resource limits and the retained-state budget of agent-log processing.
use super::CommandEvent;
use std::fmt;

pub const DEFAULT_MAX_INPUT_BYTES: u64 = 64 * 1024 * 1024;
pub const DEFAULT_MAX_RECORD_BYTES: usize = 8 * 1024 * 1024;
pub const DEFAULT_MAX_STATE_BYTES: usize = 1024 * 1024 * 1024;
/// A parsed JSON tree takes several times the size of its text.
pub(super) const JSON_TREE_FACTOR: usize = 8;
/// Per-entry bookkeeping of the ordered maps and vectors holding retained keys.
pub const ENTRY_OVERHEAD_BYTES: usize = 48;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceLimits {
    /// A whole-document JSON input larger than this is skipped.
    pub max_input_bytes: u64,
    /// A JSONL/IntelliJ line or SQLite row larger than this is skipped.
    pub max_record_bytes: usize,
    /// Estimated retained state (events, metadata, dedup/group keys) is capped here.
    pub max_state_bytes: usize,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: DEFAULT_MAX_INPUT_BYTES,
            max_record_bytes: DEFAULT_MAX_RECORD_BYTES,
            max_state_bytes: DEFAULT_MAX_STATE_BYTES,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitExceeded {
    /// One input is too large to process; the rest of the run can continue.
    Input,
    /// The retained-state budget is exhausted; the run cannot continue.
    State,
}

impl fmt::Display for LimitExceeded {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Input => "input exceeds the configured size limit",
            Self::State => "retained state exceeds the configured budget",
        })
    }
}

impl std::error::Error for LimitExceeded {}

/// Estimated bytes of retained state; charged before the state is stored.
#[derive(Debug)]
pub struct StateBudget {
    limit: usize,
    used: usize,
}

impl StateBudget {
    pub fn new(limit: usize) -> Self {
        Self { limit, used: 0 }
    }

    pub fn unlimited() -> Self {
        Self::new(usize::MAX)
    }

    pub fn used(&self) -> usize {
        self.used
    }

    pub fn charge(&mut self, bytes: usize) -> Result<(), LimitExceeded> {
        match self.used.checked_add(bytes) {
            Some(total) if total <= self.limit => {
                self.used = total;
                Ok(())
            }
            _ => Err(LimitExceeded::State),
        }
    }

    pub fn release(&mut self, bytes: usize) {
        self.used = self.used.saturating_sub(bytes);
    }

    pub(super) fn restore(&mut self, used: usize) {
        self.used = used;
    }
}

pub(super) fn optional_len(value: &Option<String>) -> usize {
    value.as_ref().map_or(0, String::len)
}

impl CommandEvent {
    /// Estimated bytes this event retains, excluding shared provenance text.
    pub fn retained_bytes(&self) -> usize {
        let maven = self.maven.as_ref().map_or(0, |maven| {
            std::mem::size_of_val(maven)
                + maven.executable.len()
                + optional_len(&maven.resume_from)
                + optional_len(&maven.pom_file)
                + [
                    &maven.lifecycle_goals,
                    &maven.plugin_goals,
                    &maven.modules,
                    &maven.tests,
                    &maven.profiles,
                ]
                .iter()
                .flat_map(|list| list.iter())
                .map(|text| text.len() + ENTRY_OVERHEAD_BYTES)
                .sum::<usize>()
                + maven
                    .properties
                    .iter()
                    .map(|(key, value)| key.len() + value.len() + ENTRY_OVERHEAD_BYTES)
                    .sum::<usize>()
        });
        std::mem::size_of::<Self>()
            + self.ide.len()
            + self.tool.len()
            + self.command.len()
            + [
                &self.timestamp,
                &self.agent,
                &self.project,
                &self.session,
                &self.cwd,
                &self.call_id,
            ]
            .into_iter()
            .map(optional_len)
            .sum::<usize>()
            + maven
    }
}

#[cfg(test)]
#[path = "../../tests/unit/log_analysis_limits.rs"]
mod tests;
