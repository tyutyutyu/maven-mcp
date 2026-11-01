use std::{borrow::Cow, collections::BTreeSet};

use rmcp::{
    ErrorData,
    model::{CallToolResponse, CallToolResult, ErrorCode},
};

use super::*;
use crate::log_safety::tests::{captured_events, field};

#[test]
fn call_ids_are_strictly_increasing_and_distinct_across_threads() {
    let first = next_call_id();
    let second = next_call_id();
    assert!(second > first);

    let ids = (0..8)
        .map(|_| std::thread::spawn(|| (0..100).map(|_| next_call_id()).collect::<Vec<_>>()))
        .flat_map(|thread| thread.join().expect("id thread"))
        .collect::<Vec<_>>();
    let distinct = ids.iter().copied().collect::<BTreeSet<_>>();
    assert_eq!(distinct.len(), ids.len());
    assert!(distinct.iter().all(|id| *id > second));
}

#[test]
fn outcome_distinguishes_success_tool_error_and_error_codes() {
    let success: Result<CallToolResponse, ErrorData> = Ok(CallToolResult::success(vec![]).into());
    let tool_error: Result<CallToolResponse, ErrorData> = Ok(CallToolResult::error(vec![]).into());
    let invalid = Err(ErrorData::invalid_params("synthetic /private/path", None));
    let internal = Err(ErrorData::internal_error("synthetic /private/path", None));
    let other = Err(ErrorData::new(ErrorCode::METHOD_NOT_FOUND, "other", None));

    assert_eq!(CallOutcome::of(&success, true), CallOutcome::Success);
    assert_eq!(CallOutcome::of(&tool_error, true), CallOutcome::ToolError);
    assert_eq!(CallOutcome::of(&invalid, true), CallOutcome::InvalidParams);
    assert_eq!(CallOutcome::of(&invalid, false), CallOutcome::UnknownTool);
    assert_eq!(CallOutcome::of(&internal, true), CallOutcome::InternalError);
    assert_eq!(CallOutcome::of(&other, true), CallOutcome::OtherError);
}

#[test]
fn outcome_labels_are_stable_and_unique() {
    let outcomes = [
        (CallOutcome::Success, "success"),
        (CallOutcome::ToolError, "tool_error"),
        (CallOutcome::InvalidParams, "invalid_params"),
        (CallOutcome::UnknownTool, "unknown_tool"),
        (CallOutcome::InternalError, "internal_error"),
        (CallOutcome::OtherError, "error"),
        (CallOutcome::Cancelled, "cancelled"),
        (CallOutcome::Dropped, "dropped"),
    ];
    for (outcome, label) in outcomes {
        assert_eq!(outcome.as_str(), label);
    }
    let labels = outcomes
        .iter()
        .map(|(outcome, _)| outcome.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(labels.len(), outcomes.len());
}

#[test]
fn a_finished_trace_logs_one_correlated_start_and_finish() {
    let logs = captured_events(|| {
        let trace = ToolCallTrace::start(Some(Cow::Borrowed("index_stats")));
        assert!(trace.is_known_tool());
        trace.finish(CallOutcome::Success);
    });
    let lines = logs.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 2, "{logs}");
    assert!(lines[0].contains("tool call started"), "{logs}");
    assert!(lines[1].contains("tool call finished"), "{logs}");
    let call_id = field(lines[0], "call_id").expect("start call_id");
    assert_eq!(field(lines[1], "call_id"), Some(call_id));
    assert_eq!(field(lines[0], "tool"), Some("index_stats"));
    assert_eq!(field(lines[1], "tool"), Some("index_stats"));
    assert_eq!(field(lines[1], "outcome"), Some("success"));
    assert!(
        field(lines[1], "duration_ms")
            .and_then(|value| value.parse::<u64>().ok())
            .is_some(),
        "{logs}"
    );
}

#[test]
fn a_dropped_trace_of_an_unknown_tool_logs_a_dropped_finish() {
    let logs = captured_events(|| {
        let trace = ToolCallTrace::start(None);
        assert!(!trace.is_known_tool());
        drop(trace);
    });
    let lines = logs.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 2, "{logs}");
    assert_eq!(field(lines[0], "tool"), Some(UNKNOWN_TOOL));
    assert_eq!(field(lines[1], "tool"), Some(UNKNOWN_TOOL));
    assert_eq!(field(lines[1], "outcome"), Some("dropped"));
}

#[test]
fn the_call_span_correlates_inner_events() {
    let logs = captured_events(|| {
        let trace = ToolCallTrace::start(Some(Cow::Borrowed("search_classes")));
        trace.span().in_scope(|| tracing::debug!("inner event"));
        trace.finish(CallOutcome::Cancelled);
    });
    let call_id = logs
        .lines()
        .next()
        .and_then(|line| field(line, "call_id"))
        .expect("start call_id");
    let inner = logs
        .lines()
        .find(|line| line.contains("inner event"))
        .expect("inner event");
    assert!(
        inner.contains(&format!(
            "tool_call{{call_id={call_id} tool=search_classes}}"
        )),
        "{logs}"
    );
    assert!(logs.contains("outcome=cancelled"), "{logs}");
}
