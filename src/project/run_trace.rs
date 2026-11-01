//! Privacy-safe debug events of the Maven child-process lifecycle.
//!
//! A run is described only by a closed [`MavenOperation`] category, its timings, its status
//! and exit code, and the truncation and redaction figures of its output. The argument
//! vector, the output and the error text are never logged: `run_maven` forwards an arbitrary
//! client-supplied argument vector, so no field may be derived from the command text.

use std::time::{Duration, Instant};

use super::{MavenRunResult, MavenRunStatus};
use crate::log_safety::elapsed_ms;

/// The internal, closed category of one Maven run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MavenOperation {
    /// `run_maven_lifecycle`: a structured lifecycle phase.
    Lifecycle,
    /// `run_maven_test`: one focused test selection.
    FocusedTest,
    /// `get_effective_pom`.
    EffectivePom,
    /// `get_dependency_tree`.
    DependencyTree,
    /// `explain_dependency_resolution`.
    DependencyResolution,
    /// `get_maven_classpath` and the classpath of a project index build.
    Classpath,
    /// `run_maven`: a client-supplied argument vector, logged only as this constant.
    Arguments,
}

impl MavenOperation {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Lifecycle => "lifecycle",
            Self::FocusedTest => "focused_test",
            Self::EffectivePom => "effective_pom",
            Self::DependencyTree => "dependency_tree",
            Self::DependencyResolution => "dependency_resolution",
            Self::Classpath => "classpath",
            Self::Arguments => "arguments",
        }
    }
}

/// Why a run ended as a `runner_error` before or instead of a Maven exit status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RunFailure {
    /// The global execution permit could not be acquired.
    Permit,
    /// The selected executable or wrapper failed validation.
    Executable,
    /// The jenv Java selection failed.
    JavaSelection,
    /// The child process could not be started.
    Spawn,
    /// Waiting for the child failed or it never exited.
    Wait,
    /// Reading the child's output failed.
    OutputRead,
}

impl RunFailure {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Permit => "permit",
            Self::Executable => "executable",
            Self::JavaSelection => "java_selection",
            Self::Spawn => "spawn",
            Self::Wait => "wait",
            Self::OutputRead => "output_read",
        }
    }
}

pub(super) fn status_label(status: &MavenRunStatus) -> &'static str {
    match status {
        MavenRunStatus::Success => "success",
        MavenRunStatus::BuildFailure => "build_failure",
        MavenRunStatus::Timeout => "timeout",
        MavenRunStatus::RunnerError => "runner_error",
    }
}

/// Logs a run that failed to acquire the global permit; no child was prepared.
pub(super) fn log_permit_failure(operation: MavenOperation, wait: Duration) {
    tracing::debug!(
        operation = %operation.as_str(),
        status = %"runner_error",
        error_kind = %RunFailure::Permit.as_str(),
        wait_ms = duration_ms(wait),
        "maven run finished"
    );
}

/// The start-to-finish record of one permitted Maven run. Dropping it without a finish,
/// because the request was cancelled, logs the run as `abandoned`.
pub(super) struct MavenRunTrace {
    operation: MavenOperation,
    wait_ms: u64,
    timeout_ms: u64,
    started: Instant,
    finished: bool,
}

impl MavenRunTrace {
    /// Starts the record once the permit is held; `wait` is the time spent waiting for it.
    pub(super) fn start(operation: MavenOperation, wait: Duration, timeout: Duration) -> Self {
        let trace = Self {
            operation,
            wait_ms: duration_ms(wait),
            timeout_ms: duration_ms(timeout),
            started: Instant::now(),
            finished: false,
        };
        tracing::debug!(
            operation = %operation.as_str(),
            wait_ms = trace.wait_ms,
            timeout_ms = trace.timeout_ms,
            "maven run started"
        );
        trace
    }

    /// Logs the final state of a run that produced `result`; `failure` names the reason of a
    /// `runner_error` result.
    pub(super) fn finish(mut self, result: &MavenRunResult, failure: Option<RunFailure>) {
        self.finished = true;
        let run_ms = elapsed_ms(self.started);
        tracing::debug!(
            operation = %self.operation.as_str(),
            status = %status_label(&result.status),
            exit_code = result.exit_code,
            timed_out = result.timed_out,
            timeout_ms = self.timeout_ms,
            wait_ms = self.wait_ms,
            run_ms,
            duration_ms = self.total_ms(run_ms),
            stdout_truncated = result.stdout_truncated,
            stderr_truncated = result.stderr_truncated,
            redaction_count = result.redaction_count,
            error_kind = failure.map(|failure| tracing::field::display(failure.as_str())),
            "maven run finished"
        );
    }

    /// The logged total of a run: the permit wait plus the child run. It is derived here,
    /// not from `MavenRunResult::duration_ms`, so that it means the same for every
    /// operation, whichever instant the caller measures its public result from.
    fn total_ms(&self, run_ms: u64) -> u64 {
        self.wait_ms.saturating_add(run_ms)
    }
}

impl Drop for MavenRunTrace {
    fn drop(&mut self) {
        if !self.finished {
            let run_ms = elapsed_ms(self.started);
            tracing::debug!(
                operation = %self.operation.as_str(),
                status = %"abandoned",
                wait_ms = self.wait_ms,
                run_ms,
                duration_ms = self.total_ms(run_ms),
                "maven run finished"
            );
        }
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[path = "../../tests/unit/project_run_trace.rs"]
mod tests;
