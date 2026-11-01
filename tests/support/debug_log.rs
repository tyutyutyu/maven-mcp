//! Reading and parsing the server's stderr diagnostics in debug-logging tests. The helpers
//! check stable `key=value` fields and their relationships, never complete formatter lines or
//! exact times.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

/// Continuously drains a server's stderr so the pipe never fills, keeping every line.
pub struct StderrLog {
    lines: Arc<Mutex<Vec<String>>>,
    reader: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl StderrLog {
    pub fn capture(stderr: impl AsyncRead + Unpin + Send + 'static) -> Self {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let collected = Arc::clone(&lines);
        let reader = tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Some(line) = reader.next_line().await? {
                collected.lock().expect("stderr lines").push(line);
            }
            Ok(())
        });
        Self { lines, reader }
    }

    pub fn snapshot(&self) -> Vec<String> {
        self.lines.lock().expect("stderr lines").clone()
    }

    /// Waits until some line satisfies `found`.
    pub async fn wait_for(&self, found: impl Fn(&str) -> bool) -> Result<()> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if self.snapshot().iter().any(|line| found(line)) {
                return Ok(());
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "expected stderr event never appeared:\n{}",
                self.snapshot().join("\n")
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Every line, once the server closed stderr.
    pub async fn finish(self) -> Result<Vec<String>> {
        tokio::time::timeout(Duration::from_secs(10), self.reader)
            .await
            .context("server kept stderr open")???;
        Ok(Arc::try_unwrap(self.lines)
            .map_err(|_| anyhow::anyhow!("stderr lines still shared"))?
            .into_inner()
            .expect("stderr lines"))
    }
}

/// The value of an event's own `name=value` field. Span context fields (`span{name=value}:`)
/// are not event fields and never match.
pub fn field<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let fields = line.rsplit_once("}: ").map_or(line, |(_, fields)| fields);
    fields
        .split_whitespace()
        .find_map(|token| token.strip_prefix(name)?.strip_prefix('='))
}

/// The `call_id` of the tool-call span an inner event was emitted in.
pub fn span_call_id(line: &str) -> Option<u64> {
    let (_, rest) = line.split_once("tool_call{call_id=")?;
    rest.split(|character: char| !character.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

pub fn has_message(line: &str, message: &str) -> bool {
    line.contains(&format!(": {message}"))
}

/// One parsed `tool call started` or `tool call finished` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallEvent {
    pub call_id: u64,
    pub tool: String,
    pub outcome: Option<String>,
    pub duration_ms: Option<u64>,
}

pub fn call_events(lines: &[String], message: &str) -> Vec<CallEvent> {
    lines
        .iter()
        .filter(|line| has_message(line, message))
        .map(|line| CallEvent {
            call_id: field(line, "call_id")
                .and_then(|value| value.parse().ok())
                .unwrap_or_else(|| panic!("event without numeric call_id: {line}")),
            tool: field(line, "tool")
                .unwrap_or_else(|| panic!("event without tool: {line}"))
                .to_owned(),
            outcome: field(line, "outcome").map(str::to_owned),
            duration_ms: field(line, "duration_ms").and_then(|value| value.parse().ok()),
        })
        .collect()
}

/// Pairs every start with its single finish by `call_id`; fails on a missing, duplicated or
/// inconsistent finish.
pub fn correlated_calls(lines: &[String]) -> Result<Vec<CallEvent>> {
    let starts = call_events(lines, "tool call started");
    let finishes = call_events(lines, "tool call finished");
    anyhow::ensure!(
        starts.len() == finishes.len(),
        "{} starts but {} finishes",
        starts.len(),
        finishes.len()
    );
    let mut calls = Vec::new();
    for start in &starts {
        let matching = finishes
            .iter()
            .filter(|finish| finish.call_id == start.call_id)
            .collect::<Vec<_>>();
        anyhow::ensure!(
            matching.len() == 1,
            "call {} has {} finish events",
            start.call_id,
            matching.len()
        );
        let finish = matching[0];
        anyhow::ensure!(finish.tool == start.tool, "tool changed for {start:?}");
        anyhow::ensure!(
            finish.outcome.is_some() && finish.duration_ms.is_some(),
            "incomplete finish {finish:?}"
        );
        calls.push(finish.clone());
    }
    calls.sort_by_key(|call| call.call_id);
    let mut ids = calls.iter().map(|call| call.call_id).collect::<Vec<_>>();
    ids.dedup();
    anyhow::ensure!(ids.len() == calls.len(), "a call_id was reused: {calls:?}");
    Ok(calls)
}
