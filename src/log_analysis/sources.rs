use super::{
    CommandCategory, CommandEvent, EventLog, LogSource, normalize_timestamp, parse_maven_command,
    shell::repository_category,
};
use anyhow::{Result, bail};
use rusqlite::{Connection, OpenFlags, types::ValueRef};
use serde_json::{Map, Value};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

type Object = Map<String, Value>;

pub(super) fn read(path: &Path, source: LogSource) -> Result<EventLog> {
    let source = if source == LogSource::Auto {
        infer_source(path)?
    } else {
        source
    };
    let mut log = match source {
        LogSource::Kilo => read_kilo(path)?,
        LogSource::IntelliJ => read_intellij(path)?,
        LogSource::Codex => read_codex(path)?,
        LogSource::VsCode | LogSource::VsCodeInsiders => {
            let value: Value = serde_json::from_reader(File::open(path)?)?;
            let mut log = EventLog::default();
            let metadata = workspace_metadata(path);
            extract(
                &value,
                if source == LogSource::VsCode {
                    "vscode"
                } else {
                    "vscode-insiders"
                },
                &metadata,
                &mut log,
                true,
            );
            log
        }
        LogSource::Auto => unreachable!("source resolved above"),
    };
    for event in &mut log.events {
        event.provenance = source_identity(path, source);
    }
    Ok(log)
}

fn source_identity(path: &Path, source: LogSource) -> String {
    // Only numeric IntelliJ rotations share an identity, in the same directory.
    // Unrelated logs and nonnumeric suffixes must remain independent.
    if source == LogSource::IntelliJ
        && let Some(name) = path.file_name().and_then(|name| name.to_str())
        && let Some((base, rotation)) = name.rsplit_once(".log.")
        && !rotation.is_empty()
        && rotation.bytes().all(|byte| byte.is_ascii_digit())
    {
        return path
            .with_file_name(format!("{base}.log"))
            .to_string_lossy()
            .into_owned();
    }
    path.to_string_lossy().into_owned()
}

fn infer_source(path: &Path) -> Result<LogSource> {
    let lower = path.to_string_lossy().to_ascii_lowercase();
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    match path.extension().and_then(|s| s.to_str()) {
        Some("db" | "sqlite" | "sqlite3" | "vscdb") => Ok(LogSource::Kilo),
        Some("jsonl") => Ok(LogSource::Codex),
        Some("json") => Ok(if lower.contains("insiders") {
            LogSource::VsCodeInsiders
        } else {
            LogSource::VsCode
        }),
        Some("log" | "txt") => Ok(LogSource::IntelliJ),
        _ if name.contains(".log.") => Ok(LogSource::IntelliJ),
        _ => bail!("cannot infer log source; pass --source explicitly"),
    }
}

pub fn supported_file(path: &Path) -> bool {
    infer_source(path).is_ok() && !path.to_string_lossy().ends_with(".gz")
}

#[derive(Clone, Default)]
struct Metadata {
    timestamp: Option<String>,
    agent: Option<String>,
    project: Option<String>,
    session: Option<String>,
    cwd: Option<String>,
}

fn field(object: &Object, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        object.get(*name).and_then(|v| match v {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            Value::Object(o) => field(o, &["fsPath", "path", "id", "name"]),
            _ => None,
        })
    })
}

impl Metadata {
    fn merge(&self, object: &Object) -> Self {
        Self {
            timestamp: field(
                object,
                &[
                    "timestamp",
                    "created_at",
                    "createdAt",
                    "time_created",
                    "ts",
                    "time",
                ],
            )
            .and_then(|v| normalize_timestamp(&v))
            .or_else(|| self.timestamp.clone()),
            agent: field(object, &["agent"]).or_else(|| self.agent.clone()),
            project: field(
                object,
                &["project", "workspace", "workspaceFolder", "directory"],
            )
            .or_else(|| self.project.clone()),
            session: field(object, &["session", "session_id", "sessionId", "sessionID"])
                .or_else(|| self.session.clone()),
            cwd: field(
                object,
                &[
                    "cwd",
                    "workdir",
                    "working_directory",
                    "workingDirectory",
                    "directory",
                ],
            )
            .or_else(|| self.cwd.clone()),
        }
    }
}

fn workspace_metadata(path: &Path) -> Metadata {
    // VS Code keeps the workspace URI beside chatSessions, outside the session.
    let workspace = path.ancestors().take(4).find_map(|parent| {
        let value: Value =
            serde_json::from_reader(File::open(parent.join("workspace.json")).ok()?).ok()?;
        value
            .get("folder")
            .and_then(Value::as_str)
            .map(|s| s.strip_prefix("file://").unwrap_or(s).to_owned())
    });
    Metadata {
        project: workspace,
        ..Metadata::default()
    }
}

fn read_codex(path: &Path) -> Result<EventLog> {
    let mut log = EventLog::default();
    let mut context = Metadata {
        agent: Some("codex".to_owned()),
        ..Metadata::default()
    };
    for line in BufReader::new(File::open(path)?).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                log.skipped_records += 1;
                continue;
            }
        };
        if matches!(
            value.get("type").and_then(Value::as_str),
            Some("session_meta" | "turn_context")
        ) {
            if let Some(payload) = value.get("payload").and_then(Value::as_object) {
                context = context.merge(payload);
                if value["type"] == "session_meta" {
                    context.session = field(payload, &["id"]).or(context.session);
                    context.project = context.project.or_else(|| context.cwd.clone());
                }
                context.timestamp = None;
            }
            continue;
        }
        extract(&value, "codex", &context, &mut log, true);
    }
    Ok(log)
}

// Apply the same exclusion to JSON envelopes and SQLite row envelopes before
// visiting payload columns; otherwise output/examples can lose their context.
fn excluded_record(object: &Object) -> bool {
    object
        .get("role")
        .and_then(Value::as_str)
        .is_some_and(|role| matches!(role, "user" | "system" | "developer" | "tool" | "function"))
        || ["type", "kind"].iter().any(|key| {
            object
                .get(*key)
                .and_then(Value::as_str)
                .is_some_and(|kind| {
                    matches!(
                        kind,
                        "function_call_output"
                            | "custom_tool_call_output"
                            | "tool_result"
                            | "tool_output"
                            | "text"
                            | "input_text"
                            | "output_text"
                            | "markdownContent"
                            | "textEdit"
                            | "patch"
                    )
                })
        })
}

fn extract(value: &Value, ide: &str, inherited: &Metadata, log: &mut EventLog, bare: bool) {
    if let Some(array) = value.as_array() {
        for child in array {
            extract(child, ide, inherited, log, bare);
        }
        return;
    }
    let Some(object) = value.as_object() else {
        return;
    };
    let kind = field(object, &["type", "kind"]).unwrap_or_default();
    if excluded_record(object) {
        return;
    }
    let mut metadata = inherited.merge(object);
    let function = object.get("function").and_then(Value::as_object);
    let tool = field(object, &["tool", "tool_name", "toolName", "toolId", "name"])
        .or_else(|| function.and_then(|f| field(f, &["name"])));
    let invocation = matches!(
        kind.as_str(),
        "function_call"
            | "tool_call"
            | "tool_use"
            | "tool"
            | "toolInvocation"
            | "toolInvocationSerialized"
            | "function"
    ) || object.contains_key("toolId")
        || object.contains_key("toolName")
        || (bare && (object.contains_key("tool") || object.contains_key("tool_name")));
    if invocation {
        // Never look inside the arguments/output of a different tool (e.g. a patch).
        let Some(tool) = tool.filter(|tool| is_shell_tool(tool)) else {
            return;
        };
        let state = object.get("state").and_then(Value::as_object);
        if object.get("isCanceled") == Some(&Value::Bool(true))
            || object.get("isCancelled") == Some(&Value::Bool(true))
            || state
                .and_then(|s| s.get("status"))
                .and_then(Value::as_str)
                .is_some_and(|s| matches!(s, "pending" | "cancelled" | "rejected"))
        {
            return;
        }
        let arguments = arguments(function.unwrap_or(object)).or_else(|| state.and_then(arguments));
        let terminal = object
            .get("toolSpecificData")
            .filter(|v| v.get("kind").and_then(Value::as_str) == Some("terminal"));
        let command = terminal
            .and_then(|v| {
                v.pointer("/commandLine/toolEdited")
                    .or_else(|| v.pointer("/commandLine/original"))
                    .or_else(|| v.get("commandLine"))
            })
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                arguments
                    .as_ref()
                    .and_then(|a| field(a, &["command", "cmd"]))
            })
            .or_else(|| field(object, &["command", "cmd"]));
        let Some(command) = command.filter(|s| !s.trim().is_empty()) else {
            log.skipped_records += 1;
            return;
        };
        if let Some(args) = &arguments {
            metadata = metadata.merge(args);
        }
        if let Some(terminal) = terminal.and_then(Value::as_object) {
            metadata = metadata.merge(terminal);
        }
        if let Some(time) = state.and_then(|s| s.get("time")).and_then(Value::as_object) {
            metadata.timestamp = field(time, &["start"])
                .and_then(|v| normalize_timestamp(&v))
                .or(metadata.timestamp);
        }
        let call_id = field(
            object,
            &["call_id", "toolCallId", "tool_call_id", "callID", "id"],
        );
        log.events
            .push(make_event(ide, &tool, command, metadata, call_id));
        return;
    }
    // Traverse only schema containers. Text, documents, patches and tool outputs
    // are deliberately absent; strings are never reparsed as embedded records.
    for key in [
        "requests",
        "messages",
        "response",
        "responses",
        "payload",
        "parts",
        "tool_calls",
        "content",
    ] {
        if let Some(child) = object.get(key) {
            extract(child, ide, &metadata, log, false);
        }
    }
}

fn arguments(object: &Object) -> Option<Object> {
    ["arguments", "input", "params", "parameters"]
        .iter()
        .find_map(|key| {
            let value = object.get(*key)?;
            value.as_object().cloned().or_else(|| {
                serde_json::from_str::<Value>(value.as_str()?)
                    .ok()?
                    .as_object()
                    .cloned()
            })
        })
}

fn is_shell_tool(tool: &str) -> bool {
    matches!(
        tool.rsplit('.')
            .next()
            .unwrap_or(tool)
            .to_ascii_lowercase()
            .as_str(),
        "shell"
            | "bash"
            | "terminal"
            | "exec_command"
            | "shell_command"
            | "execute_command"
            | "run_in_terminal"
            | "runinterminal"
            | "computer_terminal"
    )
}

fn make_event(
    ide: &str,
    tool: &str,
    command: String,
    metadata: Metadata,
    call_id: Option<String>,
) -> CommandEvent {
    let maven = parse_maven_command(&command);
    let category = if maven.is_some() {
        CommandCategory::Maven
    } else {
        repository_category(&command)
    };
    CommandEvent {
        timestamp: metadata.timestamp,
        ide: ide.to_owned(),
        agent: metadata.agent,
        project: metadata.project,
        session: metadata.session,
        cwd: metadata.cwd,
        tool: tool.to_owned(),
        command,
        category,
        maven,
        call_id,
        provenance: String::new(),
    }
}

fn read_intellij(path: &Path) -> Result<EventLog> {
    let mut log = EventLog::default();
    // Anchor at the log header: a marker quoted in chat/tool output is not a call.
    let header = regex::Regex::new(
        r"^(\d{4}-\d{2}-\d{2}[T ][0-9:.,]+(?:Z|[+-]\d{2}:\d{2})?)\s+(?:\[[^\]]*\]\s+)?(?:INFO|DEBUG|TRACE)\s+(?:-\s+[^\s]+\s+-\s+)?(.*)$",
    )?;
    for line in BufReader::new(File::open(path)?).lines() {
        let line = line?;
        let Some(captures) = header.captures(&line) else {
            continue;
        };
        let Some(mut rest) = captures.get(2).map(|m| m.as_str()) else {
            continue;
        };
        let mut metadata = Metadata {
            timestamp: captures
                .get(1)
                .and_then(|v| normalize_timestamp(v.as_str())),
            ..Metadata::default()
        };
        // Optional machine metadata: [agent=... project=... session=... cwd=...].
        if rest.starts_with('[')
            && let Some(end) = rest.find(']')
        {
            let mut fields = Map::new();
            for part in rest[1..end].split_whitespace() {
                if let Some((k, v)) = part.split_once('=') {
                    fields.insert(k.to_owned(), Value::String(v.to_owned()));
                }
            }
            metadata = metadata.merge(&fields);
            rest = rest[end + 1..].trim_start();
        }
        let command = ["Executing command:", "Terminal command:", "Shell command:"]
            .iter()
            .find_map(|marker| rest.strip_prefix(marker));
        if let Some(command) = command.filter(|s| !s.trim().is_empty()) {
            log.events.push(make_event(
                "intellij",
                "terminal",
                command.trim().to_owned(),
                metadata,
                None,
            ));
        }
    }
    Ok(log)
}

fn read_kilo(path: &Path) -> Result<EventLog> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.pragma_update(None, "query_only", true)?;
    let mut tables = connection.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")?;
    let names = tables
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut sessions = BTreeMap::new();
    let mut messages = BTreeMap::new();
    for name in &names {
        if matches!(
            name.as_str(),
            "session" | "sessions" | "message" | "messages"
        ) {
            for row in database_rows(&connection, name)? {
                let mut meta = Metadata {
                    agent: Some("kilo".to_owned()),
                    ..Metadata::default()
                }
                .merge(&row);
                if let Some(data) = row.get("data").and_then(Value::as_object) {
                    meta = meta.merge(data);
                }
                if let Some(id) = field(&row, &["id"]) {
                    if name.starts_with("session") {
                        meta.session = Some(id.clone());
                        sessions.insert(id, meta);
                    } else {
                        messages.insert(id, meta);
                    }
                }
            }
        }
    }
    let mut log = EventLog::default();
    for name in names {
        for row in database_rows(&connection, &name)? {
            if excluded_record(&row) {
                continue;
            }
            let session = field(&row, &["session_id", "sessionID"]);
            let mut metadata = session
                .as_ref()
                .and_then(|s| sessions.get(s))
                .cloned()
                .unwrap_or_else(|| Metadata {
                    agent: Some("kilo".to_owned()),
                    ..Metadata::default()
                });
            if let Some(message) =
                field(&row, &["message_id", "messageID"]).and_then(|id| messages.get(&id))
            {
                metadata.agent = message.agent.clone().or(metadata.agent);
            }
            metadata.timestamp = None;
            metadata = metadata.merge(&row);
            // A bare command column is not evidence that a tool ran.
            if ["tool", "tool_name", "toolName", "toolId"]
                .iter()
                .any(|key| row.contains_key(*key))
            {
                extract(
                    &Value::Object(row.clone()),
                    "kilo",
                    &metadata,
                    &mut log,
                    true,
                );
            }
            for key in ["payload", "data", "json", "content", "value"] {
                if let Some(value) = row.get(key) {
                    if value.is_object() || value.is_array() {
                        extract(value, "kilo", &metadata, &mut log, true);
                    } else if value
                        .as_str()
                        .is_some_and(|s| s.starts_with('{') || s.starts_with('['))
                    {
                        log.skipped_records += 1;
                    }
                }
            }
        }
    }
    Ok(log)
}

fn database_rows(connection: &Connection, table: &str) -> Result<Vec<Object>> {
    let mut statement =
        connection.prepare(&format!("SELECT * FROM \"{}\"", table.replace('"', "\"\"")))?;
    let columns = statement
        .column_names()
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();
    let rows = statement
        .query_map([], |row| {
            let mut object = Map::new();
            for (i, column) in columns.iter().enumerate() {
                let value = match row.get_ref(i)? {
                    ValueRef::Text(s) => {
                        let text = String::from_utf8_lossy(s);
                        if matches!(
                            column.as_str(),
                            "payload" | "data" | "json" | "content" | "value"
                        ) {
                            serde_json::from_str(&text)
                                .unwrap_or_else(|_| Value::String(text.into_owned()))
                        } else {
                            Value::String(text.into_owned())
                        }
                    }
                    ValueRef::Integer(n) => Value::from(n),
                    _ => Value::Null,
                };
                object.insert(column.clone(), value);
            }
            Ok(object)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn discover_default_sources() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
    else {
        return Vec::new();
    };
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"));
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".cache"));
    let mut roots = vec![
        std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".codex"))
            .join("sessions"),
        data.join("kilo"),
        cache.join("JetBrains"),
        home.join("Library/Logs/JetBrains"),
    ];
    let mut code_bases = vec![config, home.join("Library/Application Support")];
    if let Some(appdata) = std::env::var_os("APPDATA") {
        code_bases.push(PathBuf::from(appdata));
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        roots.push(PathBuf::from(local).join("JetBrains"));
    }
    for base in code_bases {
        for edition in ["Code", "Code - Insiders"] {
            let user = base.join(edition).join("User");
            roots.extend([
                user.join("workspaceStorage"),
                user.join("globalStorage/emptyWindowChatSessions"),
                user.join("globalStorage/kilocode.kilo-code"),
            ]);
        }
    }
    roots.retain(|path| path.exists());
    roots.sort();
    roots.dedup();
    roots
}
