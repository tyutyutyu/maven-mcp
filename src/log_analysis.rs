use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, types::ValueRef};
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    pub maven: Option<MavenCommand>,
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

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MavenCommand {
    pub executable: String,
    pub lifecycle_goals: Vec<String>,
    pub plugin_goals: Vec<String>,
    pub modules: Vec<String>,
    pub also_make: bool,
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

pub fn read_events(path: &Path, source: LogSource) -> Result<Vec<CommandEvent>> {
    let source = if source == LogSource::Auto {
        infer_source(path)?
    } else {
        source
    };
    let events = match source {
        LogSource::Kilo => read_kilo(path)?,
        LogSource::IntelliJ => read_intellij(path)?,
        LogSource::Codex => read_json_lines(path, "codex")?,
        LogSource::VsCode => read_json_document(path, "vscode")?,
        LogSource::VsCodeInsiders => read_json_document(path, "vscode-insiders")?,
        LogSource::Auto => unreachable!("auto source is resolved above"),
    };
    Ok(deduplicate(events))
}

pub fn filter_events(events: Vec<CommandEvent>, filter: &EventFilter) -> Vec<CommandEvent> {
    events
        .into_iter()
        .filter(|event| {
            filter
                .since
                .as_ref()
                .is_none_or(|since| event.timestamp.as_ref().is_some_and(|value| value >= since))
                && filter.until.as_ref().is_none_or(|until| {
                    event.timestamp.as_ref().is_some_and(|value| value <= until)
                })
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

pub fn redact_events(
    events: &mut [CommandEvent],
    workspace: Option<&Path>,
    extra_patterns: &[String],
) {
    let mut replacements = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        replacements.push((home.to_string_lossy().into_owned(), "<HOME>"));
    }
    if let Some(workspace) = workspace {
        replacements.push((workspace.to_string_lossy().into_owned(), "<WORKSPACE>"));
    }
    if let Ok(user) = std::env::var("USER")
        && !user.is_empty()
    {
        replacements.push((format!("/home/{user}"), "<HOME>"));
        replacements.push((format!("/Users/{user}"), "<HOME>"));
    }
    for pattern in extra_patterns {
        if !pattern.is_empty() {
            replacements.push((pattern.clone(), "<REDACTED>"));
        }
    }
    for event in events {
        redact_text(&mut event.command, &replacements);
        redact_optional(&mut event.cwd, &replacements);
        redact_optional(&mut event.project, &replacements);
        if let Some(maven) = &mut event.maven {
            for (key, value) in &mut maven.properties {
                if ["password", "token", "secret", "authorization", "api_key"]
                    .iter()
                    .any(|sensitive| key.to_ascii_lowercase().contains(sensitive))
                {
                    *value = "<REDACTED>".to_owned();
                } else {
                    redact_text(value, &replacements);
                }
            }
        }
    }
}

fn infer_source(path: &Path) -> Result<LogSource> {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(
        path.extension().and_then(|value| value.to_str()),
        Some("db" | "sqlite" | "sqlite3")
    ) {
        return Ok(LogSource::Kilo);
    }
    if name.contains("codex") || path.extension().and_then(|value| value.to_str()) == Some("jsonl")
    {
        return Ok(LogSource::Codex);
    }
    if name.contains("insider") {
        return Ok(LogSource::VsCodeInsiders);
    }
    if path.extension().and_then(|value| value.to_str()) == Some("json") {
        return Ok(LogSource::VsCode);
    }
    if matches!(
        path.extension().and_then(|value| value.to_str()),
        Some("log" | "txt")
    ) {
        return Ok(LogSource::IntelliJ);
    }
    bail!("cannot infer log source; pass --source explicitly")
}

fn read_json_document(path: &Path, ide: &str) -> Result<Vec<CommandEvent>> {
    let value: Value = serde_json::from_slice(&std::fs::read(path)?)
        .with_context(|| format!("invalid JSON log: {}", path.display()))?;
    let mut events = Vec::new();
    extract_json_events(&value, ide, &mut events);
    Ok(events)
}

fn read_json_lines(path: &Path, ide: &str) -> Result<Vec<CommandEvent>> {
    let text = std::fs::read_to_string(path)?;
    let mut events = Vec::new();
    let mut invalid_records = 0;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(_) => {
                invalid_records += 1;
                continue;
            }
        };
        extract_json_events(&value, ide, &mut events);
    }
    if events.is_empty() && invalid_records > 0 {
        bail!("JSONL log contains no readable command records");
    }
    Ok(events)
}

fn extract_json_events(value: &Value, ide: &str, events: &mut Vec<CommandEvent>) {
    extract_json_events_with_metadata(value, ide, events, &JsonMetadata::default());
}

#[derive(Clone, Default)]
struct JsonMetadata {
    timestamp: Option<String>,
    agent: Option<String>,
    project: Option<String>,
    session: Option<String>,
    cwd: Option<String>,
}

fn extract_json_events_with_metadata(
    value: &Value,
    ide: &str,
    events: &mut Vec<CommandEvent>,
    inherited: &JsonMetadata,
) {
    match value {
        Value::Array(values) => {
            for value in values {
                extract_json_events_with_metadata(value, ide, events, inherited);
            }
        }
        Value::Object(object) => {
            let metadata = JsonMetadata {
                timestamp: string_field(object, &["timestamp", "time", "created_at", "createdAt"])
                    .map(str::to_owned)
                    .or_else(|| inherited.timestamp.clone()),
                agent: string_field(object, &["agent", "model"])
                    .map(str::to_owned)
                    .or_else(|| inherited.agent.clone()),
                project: string_field(object, &["project", "workspace", "workspaceFolder"])
                    .map(str::to_owned)
                    .or_else(|| inherited.project.clone()),
                session: string_field(object, &["session", "session_id", "sessionId"])
                    .map(str::to_owned)
                    .or_else(|| inherited.session.clone()),
                cwd: string_field(object, &["cwd", "working_directory", "workingDirectory"])
                    .map(str::to_owned)
                    .or_else(|| inherited.cwd.clone()),
            };
            let tool = string_field(object, &["tool", "tool_name", "toolName", "toolId", "name"])
                .or_else(|| object.get("function")?.get("name")?.as_str());
            if let Some(tool) = tool.filter(|tool| is_shell_tool(tool))
                && let Some(command) = command_from_object(object)
            {
                events.push(event_from_parts(
                    ide,
                    tool,
                    &command,
                    EventMetadata {
                        timestamp: metadata.timestamp.as_deref(),
                        agent: metadata.agent.as_deref(),
                        project: metadata.project.as_deref(),
                        session: metadata.session.as_deref(),
                        cwd: metadata.cwd.as_deref(),
                    },
                ));
                return;
            }
            for nested in object.values() {
                extract_json_events_with_metadata(nested, ide, events, &metadata);
            }
        }
        _ => {}
    }
}

fn command_from_object(object: &serde_json::Map<String, Value>) -> Option<String> {
    string_field(object, &["command", "cmd"])
        .map(str::to_owned)
        .or_else(|| {
            for field in ["arguments", "input", "params"] {
                let Some(value) = object.get(field) else {
                    continue;
                };
                if let Some(text) = value.as_str() {
                    if let Ok(parsed) = serde_json::from_str::<Value>(text)
                        && let Some(command) = parsed
                            .get("command")
                            .or_else(|| parsed.get("cmd"))
                            .and_then(Value::as_str)
                    {
                        return Some(command.to_owned());
                    }
                } else if let Some(command) = value
                    .get("command")
                    .or_else(|| value.get("cmd"))
                    .and_then(Value::as_str)
                {
                    return Some(command.to_owned());
                }
            }
            None
        })
}

fn string_field<'a>(object: &'a serde_json::Map<String, Value>, names: &[&str]) -> Option<&'a str> {
    names
        .iter()
        .find_map(|name| object.get(*name).and_then(Value::as_str))
}

fn is_shell_tool(tool: &str) -> bool {
    matches!(
        tool.to_ascii_lowercase().as_str(),
        "shell"
            | "terminal"
            | "exec_command"
            | "execute_command"
            | "run_in_terminal"
            | "runinterminal"
            | "computer_terminal"
    )
}

fn read_intellij(path: &Path) -> Result<Vec<CommandEvent>> {
    let text = std::fs::read_to_string(path)?;
    let mut events = Vec::new();
    for line in text.lines() {
        let marker = ["Executing command:", "Terminal command:", "Shell command:"]
            .iter()
            .find_map(|marker| line.find(marker).map(|index| (marker, index)));
        let Some((marker, index)) = marker else {
            continue;
        };
        let command = line[index + marker.len()..].trim();
        if command.is_empty() {
            continue;
        }
        let timestamp = line
            .split_whitespace()
            .next()
            .filter(|value| value.contains('-') && (value.contains(':') || value.len() >= 10));
        events.push(event_from_parts(
            "intellij",
            "terminal",
            command,
            EventMetadata {
                timestamp,
                ..EventMetadata::default()
            },
        ));
    }
    Ok(events)
}

fn read_kilo(path: &Path) -> Result<Vec<CommandEvent>> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let mut table_statement = connection.prepare(
        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )?;
    let tables = table_statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut events = Vec::new();
    for table in tables {
        let columns = table_columns(&connection, &table)?;
        let relevant = columns
            .iter()
            .filter(|column| {
                let lower = column.to_ascii_lowercase();
                [
                    "command",
                    "cmd",
                    "content",
                    "payload",
                    "data",
                    "json",
                    "arguments",
                ]
                .iter()
                .any(|candidate| lower == *candidate || lower.contains(candidate))
            })
            .cloned()
            .collect::<Vec<_>>();
        if relevant.is_empty() {
            continue;
        }
        let query = format!(
            "SELECT {} FROM {}",
            relevant
                .iter()
                .map(|column| quote_identifier(column))
                .collect::<Vec<_>>()
                .join(","),
            quote_identifier(&table)
        );
        let mut statement = connection.prepare(&query)?;
        let rows = statement.query_map([], |row| {
            let mut values = Vec::new();
            for index in 0..relevant.len() {
                let value = match row.get_ref(index)? {
                    ValueRef::Text(value) => Some(String::from_utf8_lossy(value).into_owned()),
                    ValueRef::Integer(value) => Some(value.to_string()),
                    ValueRef::Real(value) => Some(value.to_string()),
                    _ => None,
                };
                values.push(value);
            }
            Ok(values)
        })?;
        for row in rows {
            for (column, value) in relevant.iter().zip(row?) {
                let Some(value) = value else { continue };
                if let Ok(json) = serde_json::from_str::<Value>(&value) {
                    extract_json_events(&json, "kilo", &mut events);
                } else if matches!(column.to_ascii_lowercase().as_str(), "command" | "cmd") {
                    events.push(event_from_parts(
                        "kilo",
                        "terminal",
                        &value,
                        EventMetadata::default(),
                    ));
                }
            }
        }
    }
    Ok(events)
}

fn table_columns(connection: &Connection, table: &str) -> Result<Vec<String>> {
    let mut statement =
        connection.prepare(&format!("PRAGMA table_info({})", quote_identifier(table)))?;
    Ok(statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

fn quote_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

#[derive(Default)]
struct EventMetadata<'a> {
    timestamp: Option<&'a str>,
    agent: Option<&'a str>,
    project: Option<&'a str>,
    session: Option<&'a str>,
    cwd: Option<&'a str>,
}

fn event_from_parts(
    ide: &str,
    tool: &str,
    command: &str,
    metadata: EventMetadata<'_>,
) -> CommandEvent {
    let maven = parse_maven_command(command);
    let category = maven
        .as_ref()
        .map(|_| CommandCategory::Maven)
        .unwrap_or_else(|| repository_category(command));
    CommandEvent {
        timestamp: metadata.timestamp.map(str::to_owned),
        ide: ide.to_owned(),
        agent: metadata.agent.map(str::to_owned),
        project: metadata.project.map(str::to_owned),
        session: metadata.session.map(str::to_owned),
        tool: tool.to_owned(),
        cwd: metadata.cwd.map(str::to_owned),
        command: command.to_owned(),
        category,
        maven,
    }
}

pub fn parse_maven_command(command: &str) -> Option<MavenCommand> {
    let tokens = shell_tokens(command);
    let executable_index = tokens.iter().position(|token| {
        let name = Path::new(token)
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or(token);
        matches!(name, "mvn" | "mvnw" | "mvnw.cmd" | "mvnd")
    })?;
    let executable = Path::new(&tokens[executable_index])
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(&tokens[executable_index])
        .to_owned();
    let mut parsed = MavenCommand {
        executable,
        ..MavenCommand::default()
    };
    let mut index = executable_index + 1;
    while index < tokens.len() {
        let token = &tokens[index];
        if matches!(token.as_str(), "|" | "&&" | ";") {
            break;
        }
        if matches!(token.as_str(), "-am" | "--also-make") {
            parsed.also_make = true;
        } else if matches!(token.as_str(), "-pl" | "--projects") {
            index += 1;
            if let Some(value) = tokens.get(index) {
                parsed.modules.extend(split_csv(value));
            }
        } else if let Some(value) = token.strip_prefix("--projects=") {
            parsed.modules.extend(split_csv(value));
        } else if token == "-P" {
            index += 1;
            if let Some(value) = tokens.get(index) {
                parsed.profiles.extend(split_csv(value));
            }
        } else if let Some(value) = token.strip_prefix("-P")
            && !value.is_empty()
        {
            parsed.profiles.extend(split_csv(value));
        } else if let Some(property) = token.strip_prefix("-D") {
            let (key, value) = property.split_once('=').unwrap_or((property, "true"));
            if key == "test" {
                parsed.tests.extend(split_csv(value));
            }
            parsed.properties.insert(key.to_owned(), value.to_owned());
        } else if !token.starts_with('-') {
            if token.contains(':') {
                parsed.plugin_goals.push(token.clone());
            } else {
                parsed.lifecycle_goals.push(token.clone());
            }
        }
        index += 1;
    }
    Some(parsed)
}

fn shell_tokens(command: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    for character in command.chars() {
        if escaped {
            current.push(character);
            escaped = false;
            continue;
        }
        if character == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if matches!(character, '\'' | '"') {
            if quote == Some(character) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(character);
            } else {
                current.push(character);
            }
            continue;
        }
        if quote.is_none() && character.is_whitespace() {
            push_token(&mut tokens, &mut current);
        } else if quote.is_none() && matches!(character, '|' | ';') {
            push_token(&mut tokens, &mut current);
            tokens.push(character.to_string());
        } else if quote.is_none() && character == '&' {
            push_token(&mut tokens, &mut current);
            if tokens.last().is_none_or(|token| token != "&&") {
                tokens.push("&&".to_owned());
            }
        } else {
            current.push(character);
        }
    }
    push_token(&mut tokens, &mut current);
    tokens
}

fn push_token(tokens: &mut Vec<String>, current: &mut String) {
    if !current.is_empty() {
        tokens.push(std::mem::take(current));
    }
}

fn split_csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect()
}

fn repository_category(command: &str) -> CommandCategory {
    let lower = command.to_ascii_lowercase();
    let executable = shell_tokens(command).into_iter().find(|token| {
        matches!(
            Path::new(token).file_name().and_then(|v| v.to_str()),
            Some("find" | "jar" | "unzip" | "javap" | "grep" | "rg")
        )
    });
    let Some(executable) = executable else {
        return CommandCategory::Shell;
    };
    let name = Path::new(&executable)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    if name == "javap" || lower.contains(".class") {
        CommandCategory::ClassInspection
    } else if lower.contains("pom.xml") || lower.contains(".pom") {
        CommandCategory::PomInspection
    } else if lower.contains("meta-inf") || lower.contains("resources") {
        CommandCategory::ResourceInspection
    } else {
        CommandCategory::JarInspection
    }
}

fn deduplicate(events: Vec<CommandEvent>) -> Vec<CommandEvent> {
    let mut seen = BTreeSet::new();
    events
        .into_iter()
        .filter(|event| {
            seen.insert((
                event.ide.clone(),
                event.session.clone(),
                event.tool.clone(),
                event.cwd.clone(),
                event.command.clone(),
            ))
        })
        .collect()
}

fn matches_filter(expected: &Option<String>, actual: Option<&String>) -> bool {
    expected
        .as_ref()
        .is_none_or(|expected| actual.is_some_and(|actual| actual.eq_ignore_ascii_case(expected)))
}

fn redact_optional(value: &mut Option<String>, replacements: &[(String, &str)]) {
    if let Some(value) = value {
        redact_text(value, replacements);
    }
}

fn redact_text(value: &mut String, replacements: &[(String, &str)]) {
    for (pattern, replacement) in replacements {
        if !pattern.is_empty() {
            *value = value.replace(pattern, replacement);
        }
    }
    *value = redact_secret_assignments(std::mem::take(value));
    *value = redact_url_credentials(std::mem::take(value));
}

fn redact_secret_assignments(mut value: String) -> String {
    for key in [
        "password=",
        "token=",
        "secret=",
        "authorization:",
        "api_key=",
    ] {
        let mut offset = 0;
        loop {
            let lowercase = value.to_ascii_lowercase();
            let Some(relative) = lowercase[offset..].find(key) else {
                break;
            };
            let start = offset + relative + key.len();
            let end = value[start..]
                .find(|character: char| character.is_whitespace() || matches!(character, '&' | ';'))
                .map_or(value.len(), |index| start + index);
            value.replace_range(start..end, "<REDACTED>");
            offset = start + "<REDACTED>".len();
        }
    }
    value
}

fn redact_url_credentials(mut value: String) -> String {
    let mut offset = 0;
    while let Some(scheme) = value[offset..].find("://") {
        let start = offset + scheme + 3;
        let authority_end = value[start..]
            .find(['/', ' ', '\n', '\r'])
            .map_or(value.len(), |index| start + index);
        let Some(at) = value[start..authority_end].rfind('@') else {
            offset = authority_end;
            continue;
        };
        value.replace_range(start..start + at, "<CREDENTIALS>");
        offset = start + "<CREDENTIALS>".len() + 1;
    }
    value
}

pub fn discover_default_sources() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    [
        home.join(".config/Code/User/workspaceStorage"),
        home.join(".config/Code - Insiders/User/workspaceStorage"),
        home.join(".codex/sessions"),
        home.join(".config/Code/User/globalStorage/kilocode.kilo-code"),
        home.join(".cache/JetBrains"),
    ]
    .into_iter()
    .filter(|path| path.exists())
    .collect()
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn extracts_only_real_vscode_and_codex_shell_calls_and_deduplicates_them() {
        let root = TempDir::new().unwrap();
        let vscode = root.path().join("session.json");
        std::fs::write(
            &vscode,
            r#"{"sessionId":"s1","messages":[{"role":"assistant","content":"example: mvn clean"},{"toolId":"run_in_terminal","timestamp":"2026-01-01T10:00:00Z","input":{"command":"./mvnw -pl core -am -Pci verify -Dtest='FooTest#works' -DskipITs=true"}}]}"#,
        )
        .unwrap();
        let events = read_events(&vscode, LogSource::VsCode).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].session.as_deref(), Some("s1"));
        let maven = events[0].maven.as_ref().unwrap();
        assert_eq!(maven.executable, "mvnw");
        assert_eq!(maven.modules, vec!["core"]);
        assert!(maven.also_make);
        assert_eq!(maven.profiles, vec!["ci"]);
        assert_eq!(maven.lifecycle_goals, vec!["verify"]);
        assert_eq!(maven.tests, vec!["FooTest#works"]);
        assert_eq!(maven.properties["skipITs"], "true");

        let codex = root.path().join("codex.jsonl");
        let record = r#"{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"jar tf ~/.m2/repository/demo.jar | rg META-INF\"}"}"#;
        std::fs::write(
            &codex,
            format!(
                "{record}\n{{broken\n{record}\n{{\"role\":\"user\",\"content\":\"run jar tf x.jar\"}}\n"
            ),
        )
        .unwrap();
        let events = read_events(&codex, LogSource::Codex).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].category, CommandCategory::ResourceInspection);
    }

    #[test]
    fn reads_kilo_sqlite_read_only_and_intellij_rotated_text() {
        let root = TempDir::new().unwrap();
        let database = root.path().join("kilo.sqlite");
        {
            let connection = Connection::open(&database).unwrap();
            connection
                .execute("CREATE TABLE events(payload TEXT)", [])
                .unwrap();
            connection
                .execute(
                    "INSERT INTO events(payload) VALUES (?1)",
                    [r#"{"tool":"execute_command","command":"javap -classpath demo.jar org.example.Foo"}"#],
                )
                .unwrap();
        }
        let events = read_events(&database, LogSource::Kilo).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].category, CommandCategory::ClassInspection);

        let log = root.path().join("idea.log.1.txt");
        std::fs::write(
            &log,
            "2026-01-02T10:00:00Z INFO chat text: mvn test\n2026-01-02T10:01:00Z INFO Executing command: rg artifactId ~/.m2/x.pom\n",
        )
        .unwrap();
        let events = read_events(&log, LogSource::IntelliJ).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].timestamp.as_deref(), Some("2026-01-02T10:01:00Z"));
        assert_eq!(events[0].category, CommandCategory::PomInspection);
    }

    #[test]
    fn handles_quoted_pipelines_filters_and_privacy_redaction() {
        let parsed = parse_maven_command(
            "cd '/tmp/a b' && mvnd test org.apache.maven.plugins:maven-help-plugin:help -Dtoken=secret | tee out",
        )
        .unwrap();
        assert_eq!(parsed.executable, "mvnd");
        assert_eq!(parsed.lifecycle_goals, vec!["test"]);
        assert_eq!(
            parsed.plugin_goals,
            vec!["org.apache.maven.plugins:maven-help-plugin:help"]
        );

        let mut events = vec![event_from_parts(
            "codex",
            "exec_command",
            "curl https://user:pass@example.test -Dtoken=hunter2 /work/demo",
            EventMetadata {
                timestamp: Some("2026-01-01"),
                agent: Some("agent"),
                project: Some("/work/demo"),
                session: Some("s1"),
                cwd: Some("/work/demo"),
            },
        )];
        redact_events(&mut events, Some(Path::new("/work/demo")), &[]);
        assert!(!events[0].command.contains("user:pass"));
        assert!(!events[0].command.contains("hunter2"));
        assert!(events[0].command.contains("<WORKSPACE>"));

        let filtered = filter_events(
            events,
            &EventFilter {
                ide: Some("CODEX".to_owned()),
                category: Some(CommandCategory::Shell),
                ..EventFilter::default()
            },
        );
        assert_eq!(filtered.len(), 1);
    }
}
