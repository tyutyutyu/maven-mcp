use super::*;
use crate::log_analysis::{
    CommandEvent, LogSource, deduplicate_events, deduplicate_events_within, read_event_log,
    read_event_log_limited, redact_events_within, retained_bytes,
};
use rusqlite::Connection;
use std::{io::Write, path::Path};
use tempfile::TempDir;

const SAFE_STATE: usize = 64 * 1024 * 1024;

fn limits(max_record_bytes: usize, max_state_bytes: usize) -> ResourceLimits {
    ResourceLimits {
        max_record_bytes,
        max_state_bytes,
        ..ResourceLimits::default()
    }
}

fn call(id: usize, command: &str) -> String {
    format!(
        r#"{{"type":"function_call","name":"shell","call_id":"call-{id}","arguments":"{{\"command\":\"{command}\"}}"}}"#
    )
}

fn write_lines(path: &Path, lines: &[String]) {
    std::fs::write(path, lines.join("\n") + "\n").unwrap();
}

fn read_limited(
    path: &Path,
    limits: &ResourceLimits,
) -> anyhow::Result<crate::log_analysis::EventLog> {
    read_event_log_limited(
        path,
        LogSource::Codex,
        limits,
        &mut StateBudget::new(limits.max_state_bytes),
    )
}

#[test]
fn budget_rejects_a_charge_before_exceeding_the_limit_and_releases() {
    let mut budget = StateBudget::new(100);
    budget.charge(60).unwrap();
    assert_eq!(budget.charge(41), Err(LimitExceeded::State));
    assert_eq!(budget.used(), 60);
    budget.charge(40).unwrap();
    budget.release(70);
    assert_eq!(budget.used(), 30);
    assert_eq!(
        StateBudget::unlimited().charge(usize::MAX),
        Ok(()),
        "an unlimited budget accepts the largest charge"
    );
    assert_eq!(
        StateBudget::new(10).charge(usize::MAX),
        Err(LimitExceeded::State)
    );
}

#[test]
fn oversized_jsonl_lines_are_skipped_and_counted_without_losing_neighbours() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("session.jsonl");
    let giant = format!(r#"{{"type":"message","text":"{}"}}"#, "x".repeat(10_000));
    write_lines(&path, &[call(1, "mvn test"), giant, call(2, "mvn verify")]);
    let log = read_limited(&path, &limits(2_048, SAFE_STATE)).unwrap();
    assert_eq!(log.events.len(), 2);
    assert_eq!(log.oversized_records, 1);
    assert_eq!(log.skipped_records, 0);
    let unbounded = read_event_log(&path, LogSource::Codex).unwrap();
    assert_eq!(unbounded.events.len(), 2);
    assert_eq!(unbounded.oversized_records, 0);
}

#[test]
fn a_line_of_exactly_the_record_limit_is_kept_and_crlf_is_stripped() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("session.jsonl");
    let line = call(1, "mvn test");
    let mut file = std::fs::File::create(&path).unwrap();
    write!(file, "{line}\r\n{line}").unwrap();
    drop(file);
    let log = read_limited(&path, &limits(line.len(), SAFE_STATE)).unwrap();
    assert_eq!(log.events.len(), 2);
    assert_eq!(log.oversized_records, 0);
    let smaller = read_limited(&path, &limits(line.len() - 1, SAFE_STATE)).unwrap();
    assert_eq!(smaller.events.len(), 0);
    assert_eq!(smaller.oversized_records, 2);
}

#[test]
fn many_unique_events_exhaust_the_state_budget_with_a_typed_error() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("session.jsonl");
    let lines: Vec<String> = (0..500).map(|id| call(id, "mvn test")).collect();
    write_lines(&path, &lines);
    let error = read_limited(&path, &limits(8_192, 20_000)).unwrap_err();
    assert_eq!(
        error.downcast_ref::<LimitExceeded>(),
        Some(&LimitExceeded::State)
    );
    assert!(read_limited(&path, &limits(8_192, SAFE_STATE)).is_ok());
}

#[test]
fn a_failed_file_returns_its_charges_to_the_shared_budget() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("session.jsonl");
    let lines: Vec<String> = (0..10).map(|id| call(id, "mvn test")).collect();
    let mut bytes = lines.join("\n").into_bytes();
    bytes.extend_from_slice(b"\n\xff\xfe\n");
    std::fs::write(&path, bytes).unwrap();
    let mut budget = StateBudget::new(SAFE_STATE);
    let result = read_event_log_limited(
        &path,
        LogSource::Codex,
        &ResourceLimits::default(),
        &mut budget,
    );
    assert!(result.is_err());
    assert_eq!(budget.used(), 0);
}

#[test]
fn whole_document_inputs_over_the_input_limit_are_rejected() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("session.json");
    std::fs::write(
        &path,
        format!(r#"{{"requests":[],"pad":"{}"}}"#, "x".repeat(4_096)),
    )
    .unwrap();
    let limited = ResourceLimits {
        max_input_bytes: 1_024,
        ..ResourceLimits::default()
    };
    let error = read_event_log_limited(
        &path,
        LogSource::VsCode,
        &limited,
        &mut StateBudget::unlimited(),
    )
    .unwrap_err();
    assert_eq!(
        error.downcast_ref::<LimitExceeded>(),
        Some(&LimitExceeded::Input)
    );
    let mut budget = StateBudget::new(1_024);
    let error = read_event_log_limited(
        &path,
        LogSource::VsCode,
        &ResourceLimits::default(),
        &mut budget,
    )
    .unwrap_err();
    assert_eq!(
        error.downcast_ref::<LimitExceeded>(),
        Some(&LimitExceeded::State)
    );
    assert_eq!(budget.used(), 0);
}

fn kilo_database(path: &Path, sessions: usize, parts: usize) {
    let connection = Connection::open(path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE session (id TEXT, data TEXT);
             CREATE TABLE part (id TEXT, session_id TEXT, data TEXT);",
        )
        .unwrap();
    let transaction = connection.unchecked_transaction().unwrap();
    for session in 0..sessions {
        transaction
            .execute(
                "INSERT INTO session VALUES (?1, ?2)",
                (format!("s{session}"), r#"{"project":"/work/demo"}"#),
            )
            .unwrap();
    }
    for part in 0..parts {
        let data = format!(
            r#"{{"type":"tool","tool":"bash","callID":"c{part}","state":{{"input":{{"command":"mvn test"}}}}}}"#
        );
        transaction
            .execute(
                "INSERT INTO part VALUES (?1, ?2, ?3)",
                (
                    format!("p{part}"),
                    format!("s{}", part % sessions.max(1)),
                    data,
                ),
            )
            .unwrap();
    }
    transaction.commit().unwrap();
}

#[test]
fn sqlite_rows_stream_and_oversized_rows_and_session_state_are_bounded() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("kilo.db");
    kilo_database(&path, 50, 200);
    {
        let connection = Connection::open(&path).unwrap();
        connection
            .execute(
                "INSERT INTO part VALUES ('big', 's1', ?1)",
                [format!(r#"{{"pad":"{}"}}"#, "x".repeat(50_000))],
            )
            .unwrap();
    }
    let log = read_event_log_limited(
        &path,
        LogSource::Kilo,
        &limits(4_096, SAFE_STATE),
        &mut StateBudget::new(SAFE_STATE),
    )
    .unwrap();
    assert_eq!(log.events.len(), 200);
    assert_eq!(log.oversized_records, 1);
    assert!(
        log.events
            .iter()
            .all(|event| event.project.as_deref() == Some("/work/demo"))
    );

    // Session metadata alone must exhaust a small budget before any event exists.
    let error = read_event_log_limited(
        &path,
        LogSource::Kilo,
        &limits(4_096, 3_000),
        &mut StateBudget::new(3_000),
    )
    .unwrap_err();
    assert_eq!(
        error.downcast_ref::<LimitExceeded>(),
        Some(&LimitExceeded::State)
    );
}

#[test]
fn bounded_deduplication_matches_the_unbounded_result_and_releases_its_keys() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("session.jsonl");
    let lines = vec![
        call(1, "mvn"),
        call(1, "mvn test"),
        call(2, "mvn verify"),
        call(2, "mvn verify"),
    ];
    write_lines(&path, &lines);
    let events = read_event_log(&path, LogSource::Codex).unwrap().events;
    let expected = deduplicate_events(events.clone());
    assert_eq!(expected.len(), 2);
    let mut budget = StateBudget::new(SAFE_STATE);
    budget.charge(retained_bytes(&events)).unwrap();
    let bounded = deduplicate_events_within(events.clone(), &mut budget).unwrap();
    assert_eq!(bounded, expected);
    assert_eq!(
        budget.used(),
        retained_bytes(&bounded),
        "identity keys and merged-away events are released after the pass"
    );
    let mut tiny = StateBudget::new(10);
    assert_eq!(
        deduplicate_events_within(events, &mut tiny),
        Err(LimitExceeded::State)
    );
    assert_eq!(tiny.used(), 0);
}

#[test]
fn event_size_estimate_covers_text_and_maven_details() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("session.jsonl");
    write_lines(&path, &[call(1, "mvn -Dkey=value -pl core test")]);
    let event = &read_event_log(&path, LogSource::Codex).unwrap().events[0];
    let floor = std::mem::size_of::<crate::log_analysis::CommandEvent>() + event.command.len();
    assert!(event.retained_bytes() > floor);
}

#[test]
fn a_giant_sqlite_value_is_skipped_without_loading_it() {
    const GIANT: i64 = 48 * 1024 * 1024;
    let root = TempDir::new().unwrap();
    let path = root.path().join("kilo.db");
    kilo_database(&path, 1, 2);
    {
        let connection = Connection::open(&path).unwrap();
        connection
            .execute(
                "INSERT INTO part VALUES ('blob', 's0', zeroblob(?1))",
                [GIANT],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO part VALUES ('text', 's0', printf('%.*c', ?1, 'x'))",
                [GIANT],
            )
            .unwrap();
    }
    // SAFETY: both calls only read and reset SQLite's global memory statistics.
    unsafe { rusqlite::ffi::sqlite3_memory_highwater(1) };
    let log = read_event_log_limited(
        &path,
        LogSource::Kilo,
        &limits(4_096, SAFE_STATE),
        &mut StateBudget::new(SAFE_STATE),
    )
    .unwrap();
    // SAFETY: as above.
    let peak = unsafe { rusqlite::ffi::sqlite3_memory_highwater(0) };
    assert_eq!(log.events.len(), 2);
    assert_eq!(log.oversized_records, 2);
    assert!(
        peak < 8 * 1024 * 1024,
        "SQLite allocated {peak} bytes; an oversized value was loaded"
    );
}

#[test]
fn kilo_metadata_charges_are_released_after_each_database() {
    let root = TempDir::new().unwrap();
    let paths = [root.path().join("a.db"), root.path().join("b.db")];
    for path in &paths {
        kilo_database(path, 500, 1);
    }
    let budget_limit = 150_000;
    // One database's metadata alone needs more than half of the budget, so
    // leaked metadata from the first database would reject the second one.
    let half = read_event_log_limited(
        &paths[0],
        LogSource::Kilo,
        &limits(4_096, budget_limit / 2),
        &mut StateBudget::new(budget_limit / 2),
    )
    .unwrap_err();
    assert_eq!(
        half.downcast_ref::<LimitExceeded>(),
        Some(&LimitExceeded::State)
    );
    let mut budget = StateBudget::new(budget_limit);
    let mut events = Vec::new();
    for path in &paths {
        let log = read_event_log_limited(
            path,
            LogSource::Kilo,
            &limits(4_096, budget_limit),
            &mut budget,
        )
        .unwrap();
        events.extend(log.events);
    }
    assert_eq!(events.len(), 2);
    assert_eq!(budget.used(), retained_bytes(&events));
}

#[test]
fn an_oversized_workspace_json_is_counted_as_a_partial_record() {
    let root = TempDir::new().unwrap();
    let workspace = root.path().join("ws");
    let sessions = workspace.join("chatSessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(
        workspace.join("workspace.json"),
        format!(
            r#"{{"folder":"file:///work/demo","pad":"{}"}}"#,
            "x".repeat(2_000)
        ),
    )
    .unwrap();
    let path = sessions.join("session.json");
    std::fs::write(
        &path,
        r#"{"requests":[{"response":[{"kind":"toolInvocationSerialized","toolId":"run_in_terminal","toolSpecificData":{"kind":"terminal","commandLine":{"original":"mvn test"}}}]}]}"#,
    )
    .unwrap();
    let read = |max_record_bytes| {
        read_event_log_limited(
            &path,
            LogSource::VsCode,
            &limits(max_record_bytes, SAFE_STATE),
            &mut StateBudget::new(SAFE_STATE),
        )
        .unwrap()
    };
    let complete = read(DEFAULT_MAX_RECORD_BYTES);
    assert_eq!(complete.oversized_records, 0);
    assert_eq!(complete.events[0].project.as_deref(), Some("/work/demo"));
    let partial = read(1_024);
    assert_eq!(partial.oversized_records, 1);
    assert_eq!(partial.events.len(), 1);
    assert_eq!(partial.events[0].project, None);
}

#[test]
fn redaction_charges_growth_and_releases_shrinkage_per_event() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("session.jsonl");
    // Escaped twice: once for the JSONL line, once for the nested arguments.
    let control = "\\\\u0001".repeat(1_000);
    write_lines(
        &path,
        &[
            call(1, &control),
            call(2, "mvn -Dtoken=abcdefghijklmnopqrstuvwxyz test"),
        ],
    );
    let events = read_event_log(&path, LogSource::Codex).unwrap().events;
    let charged = retained_bytes(&events);

    let mut budget = StateBudget::new(SAFE_STATE);
    budget.charge(charged).unwrap();
    let mut redacted = events.clone();
    redact_events_within(&mut redacted, None, &[], &mut budget).unwrap();
    assert!(retained_bytes(&redacted) > charged + 3_000);
    assert_eq!(budget.used(), retained_bytes(&redacted));

    let mut tight = StateBudget::new(charged + 1_024);
    tight.charge(charged).unwrap();
    let mut events = events;
    assert_eq!(
        redact_events_within(&mut events, None, &[], &mut tight),
        Err(LimitExceeded::State)
    );
}

#[test]
fn filtering_releases_the_charges_of_dropped_events() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("session.jsonl");
    write_lines(&path, &[call(1, "mvn test"), call(2, "ls target")]);
    let events = read_event_log(&path, LogSource::Codex).unwrap().events;
    let mut budget = StateBudget::new(SAFE_STATE);
    budget.charge(retained_bytes(&events)).unwrap();
    let filter = crate::log_analysis::EventFilter {
        category: Some(crate::log_analysis::CommandCategory::Maven),
        ..Default::default()
    };
    let kept: Vec<CommandEvent> =
        crate::log_analysis::filter_events_within(events, &filter, &mut budget);
    assert_eq!(kept.len(), 1);
    assert_eq!(budget.used(), retained_bytes(&kept));
}
