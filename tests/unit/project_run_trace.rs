use std::{collections::BTreeSet, time::Duration};

use super::*;
use crate::log_safety::tests::{captured_events, field};

fn run_result(status: MavenRunStatus, exit_code: Option<i32>) -> MavenRunResult {
    MavenRunResult {
        status,
        exit_code,
        duration_ms: 42,
        timed_out: false,
        stdout: "synthetic-stdout-marker".to_owned(),
        stderr: "synthetic-stderr-marker".to_owned(),
        stdout_truncated: true,
        stderr_truncated: false,
        redaction_count: 3,
    }
}

#[test]
fn operation_categories_are_closed_and_unique() {
    let operations = [
        (MavenOperation::Lifecycle, "lifecycle"),
        (MavenOperation::FocusedTest, "focused_test"),
        (MavenOperation::EffectivePom, "effective_pom"),
        (MavenOperation::DependencyTree, "dependency_tree"),
        (
            MavenOperation::DependencyResolution,
            "dependency_resolution",
        ),
        (MavenOperation::Classpath, "classpath"),
        (MavenOperation::Arguments, "arguments"),
    ];
    for (operation, label) in operations {
        assert_eq!(operation.as_str(), label);
    }
    let labels = operations
        .iter()
        .map(|(operation, _)| operation.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(labels.len(), operations.len());
}

#[test]
fn failure_and_status_labels_are_stable() {
    let failures = [
        (RunFailure::Permit, "permit"),
        (RunFailure::Executable, "executable"),
        (RunFailure::JavaSelection, "java_selection"),
        (RunFailure::Spawn, "spawn"),
        (RunFailure::Wait, "wait"),
        (RunFailure::OutputRead, "output_read"),
    ];
    for (failure, label) in failures {
        assert_eq!(failure.as_str(), label);
    }
    assert_eq!(status_label(&MavenRunStatus::Success), "success");
    assert_eq!(status_label(&MavenRunStatus::BuildFailure), "build_failure");
    assert_eq!(status_label(&MavenRunStatus::Timeout), "timeout");
    assert_eq!(status_label(&MavenRunStatus::RunnerError), "runner_error");
}

#[test]
fn a_finished_run_logs_timings_status_and_output_figures_without_output() {
    let logs = captured_events(|| {
        let trace = MavenRunTrace::start(
            MavenOperation::Arguments,
            Duration::from_millis(7),
            Duration::from_secs(2),
        );
        trace.finish(&run_result(MavenRunStatus::BuildFailure, Some(1)), None);
    });
    let lines = logs.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 2, "{logs}");
    assert!(lines[0].contains("maven run started"), "{logs}");
    assert_eq!(field(lines[0], "operation"), Some("arguments"));
    assert_eq!(field(lines[0], "wait_ms"), Some("7"));
    assert_eq!(field(lines[0], "timeout_ms"), Some("2000"));
    let finished = lines[1];
    assert!(finished.contains("maven run finished"), "{logs}");
    assert_eq!(field(finished, "operation"), Some("arguments"));
    assert_eq!(field(finished, "status"), Some("build_failure"));
    assert_eq!(field(finished, "exit_code"), Some("1"));
    assert_eq!(field(finished, "timed_out"), Some("false"));
    assert_eq!(field(finished, "wait_ms"), Some("7"));
    assert_total_is_wait_plus_run(finished, 7);
    assert_eq!(field(finished, "stdout_truncated"), Some("true"));
    assert_eq!(field(finished, "stderr_truncated"), Some("false"));
    assert_eq!(field(finished, "redaction_count"), Some("3"));
    assert!(field(finished, "error_kind").is_none(), "{logs}");
    assert!(!logs.contains("synthetic-stdout-marker"), "{logs}");
    assert!(!logs.contains("synthetic-stderr-marker"), "{logs}");
}

#[test]
fn a_runner_error_logs_its_failure_category() {
    let logs = captured_events(|| {
        let trace = MavenRunTrace::start(
            MavenOperation::Classpath,
            Duration::ZERO,
            Duration::from_secs(1),
        );
        trace.finish(
            &run_result(MavenRunStatus::RunnerError, None),
            Some(RunFailure::Spawn),
        );
    });
    let finished = logs.lines().nth(1).expect("finish event");
    assert_eq!(field(finished, "status"), Some("runner_error"));
    assert_eq!(field(finished, "error_kind"), Some("spawn"));
    assert!(field(finished, "exit_code").is_none(), "{logs}");
}

#[test]
fn a_dropped_run_and_a_permit_failure_still_log_a_finish() {
    let logs = captured_events(|| {
        drop(MavenRunTrace::start(
            MavenOperation::Lifecycle,
            Duration::ZERO,
            Duration::from_secs(1),
        ));
        run_trace_permit_failure();
    });
    let lines = logs.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 3, "{logs}");
    assert_eq!(field(lines[1], "status"), Some("abandoned"));
    assert_eq!(field(lines[1], "operation"), Some("lifecycle"));
    assert_eq!(field(lines[2], "status"), Some("runner_error"));
    assert_eq!(field(lines[2], "error_kind"), Some("permit"));
    assert_eq!(field(lines[2], "operation"), Some("effective_pom"));
}

#[test]
fn every_operation_logs_the_permit_wait_in_its_total_duration() {
    // `run_focused_test` and `effective_pom` measure their public `duration_ms` from after
    // the permit wait, so a result shorter than the wait must not become the logged total.
    let wait = Duration::from_millis(250);
    let operations = [
        MavenOperation::Lifecycle,
        MavenOperation::FocusedTest,
        MavenOperation::EffectivePom,
        MavenOperation::DependencyTree,
        MavenOperation::DependencyResolution,
        MavenOperation::Classpath,
        MavenOperation::Arguments,
    ];
    let logs = captured_events(|| {
        for operation in operations {
            let mut result = run_result(MavenRunStatus::Success, Some(0));
            result.duration_ms = 1;
            MavenRunTrace::start(operation, wait, Duration::from_secs(2)).finish(&result, None);
        }
        drop(MavenRunTrace::start(
            MavenOperation::FocusedTest,
            wait,
            Duration::from_secs(2),
        ));
    });
    let finishes = logs
        .lines()
        .filter(|line| line.contains("maven run finished"))
        .collect::<Vec<_>>();
    assert_eq!(finishes.len(), operations.len() + 1, "{logs}");
    for (finished, operation) in finishes.iter().zip(operations) {
        assert_eq!(field(finished, "operation"), Some(operation.as_str()));
        assert_total_is_wait_plus_run(finished, 250);
    }
    let abandoned = finishes.last().expect("abandoned finish");
    assert_eq!(field(abandoned, "status"), Some("abandoned"));
    assert_total_is_wait_plus_run(abandoned, 250);
}

/// Asserts that a finish event's `duration_ms` is exactly its `wait_ms` plus its `run_ms`,
/// and therefore never smaller than either of them.
fn assert_total_is_wait_plus_run(finished: &str, expected_wait_ms: u64) {
    let number = |name| {
        field(finished, name)
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or_else(|| panic!("{name} is missing: {finished}"))
    };
    let (wait_ms, run_ms, duration_ms) =
        (number("wait_ms"), number("run_ms"), number("duration_ms"));
    assert_eq!(wait_ms, expected_wait_ms, "{finished}");
    assert_eq!(duration_ms, wait_ms + run_ms, "{finished}");
    assert!(
        duration_ms >= wait_ms && duration_ms >= run_ms,
        "{finished}"
    );
}

fn run_trace_permit_failure() {
    log_permit_failure(MavenOperation::EffectivePom, Duration::from_millis(3));
}
