//! Offline extraction of recorded shell invocations. No command is executed.
mod privacy;
mod shell;
mod sources;
mod time;

use anyhow::Result;
pub use privacy::redact_events;
use serde::{Deserialize, Serialize};
pub use shell::parse_maven_command;
pub use sources::{discover_default_sources, supported_file};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
pub use time::normalize_timestamp;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogSource {
    Auto,
    VsCode,
    VsCodeInsiders,
    Codex,
    Kilo,
    IntelliJ,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandEvent {
    pub timestamp: Option<String>,
    pub ide: String,
    pub agent: Option<String>,
    pub project: Option<String>,
    pub session: Option<String>,
    pub tool: String,
    pub cwd: Option<String>,
    pub command: String,
    pub category: CommandCategory,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maven: Option<MavenCommand>,
    // Disambiguates records without a session; never appears in a report.
    #[serde(skip)]
    provenance: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum CommandCategory {
    Maven,
    JarInspection,
    ClassInspection,
    PomInspection,
    ResourceInspection,
    Shell,
}

impl CommandCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Maven => "maven",
            Self::JarInspection => "jar_inspection",
            Self::ClassInspection => "class_inspection",
            Self::PomInspection => "pom_inspection",
            Self::ResourceInspection => "resource_inspection",
            Self::Shell => "shell",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct MavenCommand {
    pub executable: String,
    pub lifecycle_goals: Vec<String>,
    pub plugin_goals: Vec<String>,
    pub modules: Vec<String>,
    pub also_make: bool,
    pub also_make_dependents: bool,
    pub resume_from: Option<String>,
    pub pom_file: Option<String>,
    pub tests: Vec<String>,
    pub profiles: Vec<String>,
    pub properties: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default)]
pub struct EventFilter {
    pub since: Option<String>,
    pub until: Option<String>,
    pub ide: Option<String>,
    pub agent: Option<String>,
    pub project: Option<String>,
    pub session: Option<String>,
    pub category: Option<CommandCategory>,
}

#[derive(Debug, Default)]
pub struct EventLog {
    pub events: Vec<CommandEvent>,
    pub skipped_records: usize,
}

/// Read without deduplication so callers can combine mirrored files first.
pub fn read_event_log(path: &Path, source: LogSource) -> Result<EventLog> {
    sources::read(path, source)
}

pub fn read_events(path: &Path, source: LogSource) -> Result<Vec<CommandEvent>> {
    Ok(deduplicate_events(read_event_log(path, source)?.events))
}

pub fn filter_events(events: Vec<CommandEvent>, filter: &EventFilter) -> Vec<CommandEvent> {
    events
        .into_iter()
        .filter(|event| {
            filter
                .since
                .as_ref()
                .is_none_or(|since| event.timestamp.as_ref().is_some_and(|t| t >= since))
                && filter
                    .until
                    .as_ref()
                    .is_none_or(|until| event.timestamp.as_ref().is_some_and(|t| t <= until))
                && matches_filter(&filter.ide, Some(&event.ide))
                && matches_filter(&filter.agent, event.agent.as_ref())
                && matches_filter(&filter.project, event.project.as_ref())
                && matches_filter(&filter.session, event.session.as_ref())
                && filter
                    .category
                    .as_ref()
                    .is_none_or(|category| &event.category == category)
        })
        .collect()
}

fn matches_filter(expected: &Option<String>, actual: Option<&String>) -> bool {
    expected
        .as_ref()
        .is_none_or(|expected| actual.is_some_and(|actual| actual.eq_ignore_ascii_case(expected)))
}

/// Call IDs identify progressive updates; without one, require a timestamp and
/// all identity fields to agree. Never collapse legitimate untimed repetitions.
pub fn deduplicate_events(events: Vec<CommandEvent>) -> Vec<CommandEvent> {
    let mut calls = BTreeMap::new();
    let mut result: Vec<CommandEvent> = Vec::new();
    for event in events {
        let identity = event.call_id.clone().map(|id| ("id", id)).or_else(|| {
            event.timestamp.as_ref().map(|time| {
                (
                    "event",
                    format!(
                        "{time}\0{}\0{}",
                        event.cwd.as_deref().unwrap_or(""),
                        event.command
                    ),
                )
            })
        });
        let Some(identity) = identity else {
            result.push(event);
            continue;
        };
        let key = (
            event.ide.clone(),
            event.agent.clone(),
            event.project.clone(),
            event
                .session
                .clone()
                .unwrap_or_else(|| event.provenance.clone()),
            event.tool.clone(),
            identity,
        );
        if let Some(&index) = calls.get(&key) {
            // Prefer the most complete progressive command, independent of input order.
            let previous: &mut CommandEvent = &mut result[index];
            let earliest = match (&previous.timestamp, &event.timestamp) {
                (Some(a), Some(b)) => Some(a.min(b).clone()),
                (a, b) => a.clone().or_else(|| b.clone()),
            };
            if (event.command.len(), &event.command) > (previous.command.len(), &previous.command) {
                *previous = event;
            }
            previous.timestamp = earliest;
        } else {
            calls.insert(key, result.len());
            result.push(event);
        }
    }
    sort_events(&mut result);
    result
}

pub fn sort_events(events: &mut [CommandEvent]) {
    events.sort_by(|a, b| {
        (
            &a.timestamp,
            &a.ide,
            &a.agent,
            &a.project,
            &a.session,
            &a.tool,
            &a.cwd,
            &a.command,
            &a.call_id,
        )
            .cmp(&(
                &b.timestamp,
                &b.ide,
                &b.agent,
                &b.project,
                &b.session,
                &b.tool,
                &b.cwd,
                &b.command,
                &b.call_id,
            ))
    });
}

pub fn source_paths(roots: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for root in roots {
        if root.is_file() {
            paths.push(root.canonicalize()?);
        } else if root.is_dir() {
            for entry in walkdir::WalkDir::new(root).follow_links(false) {
                let entry = entry?;
                if entry.file_type().is_file() && supported_file(entry.path()) {
                    paths.push(entry.path().canonicalize()?);
                }
            }
        } else {
            anyhow::bail!("input path does not exist");
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}
