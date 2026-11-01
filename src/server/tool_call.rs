//! Central, privacy-safe observation of every `tools/call` request.
//!
//! Each call gets a per-process monotonic `call_id` that never derives from client input (the
//! JSON-RPC request id is an arbitrary client string), a tool name only when the router knows
//! it, and exactly one start and one finish event at debug level. The finish event carries a
//! closed outcome category and the elapsed time; arguments, results and error messages are
//! never logged.

use std::{
    borrow::Cow,
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

use rmcp::{
    ErrorData,
    model::{CallToolResponse, ErrorCode},
};

/// The logged tool name of a call whose name the router does not know.
pub(super) const UNKNOWN_TOOL: &str = "<unknown>";

static NEXT_CALL_ID: AtomicU64 = AtomicU64::new(1);

/// The next process-wide call identifier; strictly increasing and never reused.
pub(super) fn next_call_id() -> u64 {
    NEXT_CALL_ID.fetch_add(1, Ordering::Relaxed)
}

/// The diagnostic final state of one tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CallOutcome {
    /// The tool returned a result.
    Success,
    /// The tool returned a result flagged with `isError`.
    ToolError,
    /// A known tool rejected its arguments.
    InvalidParams,
    /// The router does not know the requested tool name.
    UnknownTool,
    /// The tool failed with an internal error.
    InternalError,
    /// Any other MCP error code.
    OtherError,
    /// The client cancelled the request before the tool finished.
    Cancelled,
    /// The call future was dropped before it produced a result, for example on shutdown.
    Dropped,
}

impl CallOutcome {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::ToolError => "tool_error",
            Self::InvalidParams => "invalid_params",
            Self::UnknownTool => "unknown_tool",
            Self::InternalError => "internal_error",
            Self::OtherError => "error",
            Self::Cancelled => "cancelled",
            Self::Dropped => "dropped",
        }
    }

    /// Classifies a finished router call; only the error code is inspected, never the message.
    pub(super) fn of(result: &Result<CallToolResponse, ErrorData>, known_tool: bool) -> Self {
        match result {
            Ok(CallToolResponse::Complete(result)) if result.is_error == Some(true) => {
                Self::ToolError
            }
            Ok(_) => Self::Success,
            Err(_) if !known_tool => Self::UnknownTool,
            Err(error) if error.code == ErrorCode::INVALID_PARAMS => Self::InvalidParams,
            Err(error) if error.code == ErrorCode::INTERNAL_ERROR => Self::InternalError,
            Err(_) => Self::OtherError,
        }
    }
}

/// The start-to-finish record of one call. Dropping it without [`Self::finish`] logs the
/// call as [`CallOutcome::Dropped`], so every start event has exactly one finish event.
pub(super) struct ToolCallTrace {
    call_id: u64,
    tool: Option<Cow<'static, str>>,
    started: Instant,
    finished: bool,
}

impl ToolCallTrace {
    /// Starts the record of a call to `tool`, the router's own name of the tool or `None`.
    pub(super) fn start(tool: Option<Cow<'static, str>>) -> Self {
        let trace = Self {
            call_id: next_call_id(),
            tool,
            started: Instant::now(),
            finished: false,
        };
        tracing::debug!(
            call_id = trace.call_id,
            tool = %trace.tool_name(),
            "tool call started"
        );
        trace
    }

    pub(super) fn is_known_tool(&self) -> bool {
        self.tool.is_some()
    }

    /// The span that correlates every internal event of this call with its `call_id`.
    pub(super) fn span(&self) -> tracing::Span {
        tracing::debug_span!("tool_call", call_id = self.call_id, tool = %self.tool_name())
    }

    pub(super) fn finish(mut self, outcome: CallOutcome) {
        self.finished = true;
        self.log_finish(outcome);
    }

    fn tool_name(&self) -> &str {
        self.tool.as_deref().unwrap_or(UNKNOWN_TOOL)
    }

    fn log_finish(&self, outcome: CallOutcome) {
        tracing::debug!(
            call_id = self.call_id,
            tool = %self.tool_name(),
            outcome = %outcome.as_str(),
            duration_ms = crate::log_safety::elapsed_ms(self.started),
            "tool call finished"
        );
    }
}

impl Drop for ToolCallTrace {
    fn drop(&mut self) {
        if !self.finished {
            self.log_finish(CallOutcome::Dropped);
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/server_tool_call.rs"]
mod tests;
