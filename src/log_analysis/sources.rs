use super::{
    CommandCategory, CommandEvent, ENTRY_OVERHEAD_BYTES, EventLog, LimitExceeded, LogSource,
    ResourceLimits, StateBudget, limits, normalize_timestamp, parse_maven_command,
    shell::repository_category,
};
use anyhow::{Result, anyhow, bail};
use rusqlite::{Connection, OpenFlags, Row, types::ValueRef};
use serde_json::{Map, Value};
use std::{
    borrow::Cow,
    collections::BTreeMap,
    fs::File,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    sync::Arc,
};

type Object = Map<String, Value>;

pub(super) fn read(
    path: &Path,
    source: LogSource,
    limits: &ResourceLimits,
    budget: &mut StateBudget,
) -> Result<EventLog> {
    let charged_before = budget.used();
    let result = read_source(path, source, limits, budget);
    if result.is_err() {
        // A failed file contributes no events, so it must not keep its charges.
        budget.restore(charged_before);
    }
    result
}

fn read_source(
    path: &Path,
    source: LogSource,
    limits: &ResourceLimits,
    budget: &mut StateBudget,
) -> Result<EventLog> {
    let source = if source == LogSource::Auto {
        infer_source(path)?
    } else {
        source
    };
    let mut log = match source {
        LogSource::Kilo => read_kilo(path, limits, budget)?,
        LogSource::IntelliJ => read_intellij(path, limits, budget)?,
        LogSource::Codex => read_codex(path, limits, budget)?,
        LogSource::VsCode | LogSource::VsCodeInsiders => {
            read_document(path, source, limits, budget)?
        }
        LogSource::Auto => unreachable!("source resolved above"),
    };
    let provenance: Arc<str> = Arc::from(source_identity(path, source));
    for event in &mut log.events {
        event.provenance = provenance.clone();
    }
    Ok(log)
}

fn read_document(
    path: &Path,
    source: LogSource,
    limits: &ResourceLimits,
    budget: &mut StateBudget,
) -> Result<EventLog> {
    let file = File::open(path)?;
    let length = file.metadata()?.len();
    if length > limits.max_input_bytes {
        return Err(LimitExceeded::Input.into());
    }
    // The parsed tree is transient but several times larger than the text.
    let tree_bytes = usize::try_from(length)
        .unwrap_or(usize::MAX)
        .saturating_mul(limits::JSON_TREE_FACTOR);
    budget.charge(tree_bytes)?;
    let value: Value = serde_json::from_reader(BufReader::new(file))?;
    let mut log = EventLog::default();
    let (metadata, oversized_workspace) = workspace_metadata(path, limits);
    log.oversized_records += usize::from(oversized_workspace);
    extract(
        &value,
        if source == LogSource::VsCode {
            "vscode"
        } else {
            "vscode-insiders"
        },
        &metadata,
        &mut log,
        budget,
        true,
    )?;
    budget.release(tree_bytes);
    Ok(log)
}

fn push_event(
    log: &mut EventLog,
    budget: &mut StateBudget,
    event: CommandEvent,
) -> Result<(), LimitExceeded> {
    budget.charge(event.retained_bytes())?;
    log.events.push(event);
    Ok(())
}

/// Visit each line without ever holding more than `max_record_bytes` of it;
/// returns the number of oversized lines that were skipped.
fn for_each_line(
    path: &Path,
    max_record_bytes: usize,
    mut visit: impl FnMut(&str) -> Result<()>,
) -> Result<usize> {
    // Room for the longest terminator ("\r\n") plus one byte proving the overrun.
    let read_cap = max_record_bytes as u64 + 3;
    let mut reader = BufReader::new(File::open(path)?);
    let mut buffer = Vec::new();
    let mut oversized = 0;
    loop {
        buffer.clear();
        let read = (&mut reader)
            .take(read_cap)
            .read_until(b'\n', &mut buffer)?;
        if read == 0 {
            return Ok(oversized);
        }
        if buffer.last() == Some(&b'\n') {
            buffer.pop();
            if buffer.last() == Some(&b'\r') {
                buffer.pop();
            }
        } else if read as u64 == read_cap {
            reader.skip_until(b'\n')?;
            oversized += 1;
            continue;
        }
        if buffer.len() > max_record_bytes {
            oversized += 1;
            continue;
        }
        let line = std::str::from_utf8(&buffer).map_err(|_| anyhow!("input is not valid UTF-8"))?;
        visit(line)?;
    }
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
    fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + [
                &self.timestamp,
                &self.agent,
                &self.project,
                &self.session,
                &self.cwd,
            ]
            .into_iter()
            .map(limits::optional_len)
            .sum::<usize>()
    }

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

/// Returns the workspace metadata and whether a workspace.json was skipped
/// because it exceeded `max_record_bytes` (its project is then unknown).
fn workspace_metadata(path: &Path, limits: &ResourceLimits) -> (Metadata, bool) {
    // VS Code keeps the workspace URI beside chatSessions, outside the session.
    let mut oversized = false;
    let workspace = path.ancestors().take(4).find_map(|parent| {
        let file = File::open(parent.join("workspace.json")).ok()?;
        if file.metadata().ok()?.len() > limits.max_record_bytes as u64 {
            oversized = true;
            return None;
        }
        let value: Value = serde_json::from_reader(BufReader::new(file)).ok()?;
        value
            .get("folder")
            .and_then(Value::as_str)
            .map(|s| s.strip_prefix("file://").unwrap_or(s).to_owned())
    });
    let metadata = Metadata {
        project: workspace,
        ..Metadata::default()
    };
    let skipped = oversized && metadata.project.is_none();
    (metadata, skipped)
}

fn read_codex(path: &Path, limits: &ResourceLimits, budget: &mut StateBudget) -> Result<EventLog> {
    let mut log = EventLog::default();
    let mut context = Metadata {
        agent: Some("codex".to_owned()),
        ..Metadata::default()
    };
    let oversized = for_each_line(path, limits.max_record_bytes, |line| {
        if line.trim().is_empty() {
            return Ok(());
        }
        let value: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                log.skipped_records += 1;
                return Ok(());
            }
        };
        if matches!(
            value.get("type").and_then(Value::as_str),
            Some("session_meta" | "turn_context")
        ) {
            if let Some(payload) = value.get("payload").and_then(Value::as_object) {
                context = context.merge(payload);
                if value["type"] == "session_meta" {
                    context.session = field(payload, &["id"]).or(context.session.take());
                    context.project = context.project.take().or_else(|| context.cwd.clone());
                }
                context.timestamp = None;
            }
            return Ok(());
        }
        extract(&value, "codex", &context, &mut log, budget, true)
    })?;
    log.oversized_records = oversized;
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

fn extract(
    value: &Value,
    ide: &str,
    inherited: &Metadata,
    log: &mut EventLog,
    budget: &mut StateBudget,
    bare: bool,
) -> Result<()> {
    if let Some(array) = value.as_array() {
        for child in array {
            extract(child, ide, inherited, log, budget, bare)?;
        }
        return Ok(());
    }
    let Some(object) = value.as_object() else {
        return Ok(());
    };
    let kind = field(object, &["type", "kind"]).unwrap_or_default();
    if excluded_record(object) {
        return Ok(());
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
            return Ok(());
        };
        let state = object.get("state").and_then(Value::as_object);
        if object.get("isCanceled") == Some(&Value::Bool(true))
            || object.get("isCancelled") == Some(&Value::Bool(true))
            || state
                .and_then(|s| s.get("status"))
                .and_then(Value::as_str)
                .is_some_and(|s| matches!(s, "pending" | "cancelled" | "rejected"))
        {
            return Ok(());
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
                    .as_deref()
                    .and_then(|a| field(a, &["command", "cmd"]))
            })
            .or_else(|| field(object, &["command", "cmd"]));
        let Some(command) = command.filter(|s| !s.trim().is_empty()) else {
            log.skipped_records += 1;
            return Ok(());
        };
        if let Some(args) = arguments.as_deref() {
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
        push_event(
            log,
            budget,
            make_event(ide, &tool, command, metadata, call_id),
        )?;
        return Ok(());
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
            extract(child, ide, &metadata, log, budget, false)?;
        }
    }
    Ok(())
}

fn arguments(object: &Object) -> Option<Cow<'_, Object>> {
    ["arguments", "input", "params", "parameters"]
        .iter()
        .find_map(|key| {
            let value = object.get(*key)?;
            value.as_object().map(Cow::Borrowed).or_else(|| {
                match serde_json::from_str::<Value>(value.as_str()?).ok()? {
                    Value::Object(parsed) => Some(Cow::Owned(parsed)),
                    _ => None,
                }
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
        provenance: Arc::default(),
    }
}

fn read_intellij(
    path: &Path,
    limits: &ResourceLimits,
    budget: &mut StateBudget,
) -> Result<EventLog> {
    let mut log = EventLog::default();
    // Anchor at the log header: a marker quoted in chat/tool output is not a call.
    let header = regex::Regex::new(
        r"^(\d{4}-\d{2}-\d{2}[T ][0-9:.,]+(?:Z|[+-]\d{2}:\d{2})?)\s+(?:\[[^\]]*\]\s+)?(?:INFO|DEBUG|TRACE)\s+(?:-\s+[^\s]+\s+-\s+)?(.*)$",
    )?;
    let oversized = for_each_line(path, limits.max_record_bytes, |line| {
        let Some(captures) = header.captures(line) else {
            return Ok(());
        };
        let Some(mut rest) = captures.get(2).map(|m| m.as_str()) else {
            return Ok(());
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
            push_event(
                &mut log,
                budget,
                make_event(
                    "intellij",
                    "terminal",
                    command.trim().to_owned(),
                    metadata,
                    None,
                ),
            )?;
        }
        Ok(())
    })?;
    log.oversized_records = oversized;
    Ok(log)
}

fn read_kilo(path: &Path, limits: &ResourceLimits, budget: &mut StateBudget) -> Result<EventLog> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.pragma_update(None, "query_only", true)?;
    let mut tables = connection.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")?;
    let names = tables
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut log = EventLog::default();
    let mut sessions: BTreeMap<String, Metadata> = BTreeMap::new();
    // Only the message agent is consulted later, so only it is retained.
    let mut messages: BTreeMap<String, Option<String>> = BTreeMap::new();
    let charged_before_metadata = budget.used();
    for name in &names {
        if !matches!(
            name.as_str(),
            "session" | "sessions" | "message" | "messages"
        ) {
            continue;
        }
        let oversized = for_each_database_row(&connection, name, limits, |row| {
            let mut meta = Metadata {
                agent: Some("kilo".to_owned()),
                ..Metadata::default()
            }
            .merge(&row);
            if let Some(data) = row.get("data").and_then(Value::as_object) {
                meta = meta.merge(data);
            }
            let Some(id) = field(&row, &["id"]) else {
                return Ok(());
            };
            if name.starts_with("session") {
                meta.session = Some(id.clone());
                remember(&mut sessions, budget, id, meta, Metadata::retained_bytes)?;
            } else {
                remember(&mut messages, budget, id, meta.agent, |agent| {
                    std::mem::size_of::<Option<String>>() + limits::optional_len(agent)
                })?;
            }
            Ok(())
        })?;
        log.oversized_records += oversized;
    }
    // The metadata maps are dropped on return; only the events keep their charges.
    let metadata_charge = budget.used().saturating_sub(charged_before_metadata);
    for name in names {
        let oversized = for_each_database_row(&connection, &name, limits, |row| {
            if excluded_record(&row) {
                return Ok(());
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
                metadata.agent = message.clone().or(metadata.agent);
            }
            metadata.timestamp = None;
            metadata = metadata.merge(&row);
            let row = Value::Object(row);
            let Some(object) = row.as_object() else {
                return Ok(());
            };
            // A bare command column is not evidence that a tool ran.
            if ["tool", "tool_name", "toolName", "toolId"]
                .iter()
                .any(|key| object.contains_key(*key))
            {
                extract(&row, "kilo", &metadata, &mut log, budget, true)?;
            }
            for key in ["payload", "data", "json", "content", "value"] {
                if let Some(value) = object.get(key) {
                    if value.is_object() || value.is_array() {
                        extract(value, "kilo", &metadata, &mut log, budget, true)?;
                    } else if value
                        .as_str()
                        .is_some_and(|s| s.starts_with('{') || s.starts_with('['))
                    {
                        log.skipped_records += 1;
                    }
                }
            }
            Ok(())
        })?;
        log.oversized_records += oversized;
    }
    drop((sessions, messages));
    budget.release(metadata_charge);
    Ok(log)
}

/// Insert a keyed entry after charging it; a replaced entry's charge is returned.
fn remember<V>(
    map: &mut BTreeMap<String, V>,
    budget: &mut StateBudget,
    id: String,
    value: V,
    size: impl Fn(&V) -> usize,
) -> Result<(), LimitExceeded> {
    let key_length = id.len();
    budget.charge(key_length + size(&value) + ENTRY_OVERHEAD_BYTES)?;
    if let Some(previous) = map.insert(id, value) {
        budget.release(key_length + size(&previous) + ENTRY_OVERHEAD_BYTES);
    }
    Ok(())
}

/// Stream one table row at a time; returns the number of oversized rows skipped.
fn for_each_database_row(
    connection: &Connection,
    table: &str,
    limits: &ResourceLimits,
    mut visit: impl FnMut(Object) -> Result<()>,
) -> Result<usize> {
    let table = quote_identifier(table);
    let columns = connection
        .prepare(&format!("SELECT * FROM {table}"))?
        .column_names()
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();
    let mut statement = connection.prepare(&guarded_row_query(&table, &columns))?;
    let mut rows = statement.query([i64::try_from(limits.max_record_bytes).unwrap_or(i64::MAX)])?;
    let mut oversized = 0;
    while let Some(row) = rows.next()? {
        match row_object(row, &columns, limits.max_record_bytes)? {
            Some(object) => visit(object)?,
            None => oversized += 1,
        }
    }
    Ok(oversized)
}

fn quote_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Select the row size first and every column only when the row fits.
/// `octet_length` of a column reads only the record header, so an oversized
/// TEXT/BLOB value is never loaded from its overflow pages.
fn guarded_row_query(table: &str, columns: &[String]) -> String {
    let size = if columns.is_empty() {
        "0".to_owned()
    } else {
        columns
            .iter()
            .map(|column| format!("coalesce(octet_length({}), 0)", quote_identifier(column)))
            .collect::<Vec<_>>()
            .join(" + ")
    };
    let guarded = columns.iter().map(|column| {
        format!(
            ", CASE WHEN {size} <= ?1 THEN {} END",
            quote_identifier(column)
        )
    });
    format!("SELECT {size}{} FROM {table}", guarded.collect::<String>())
}

/// Convert a guarded row (size first, then the columns), or return `None`
/// without having loaded any value when it is oversized.
fn row_object(
    row: &Row<'_>,
    columns: &[String],
    max_record_bytes: usize,
) -> rusqlite::Result<Option<Object>> {
    let size = usize::try_from(row.get::<_, i64>(0)?).unwrap_or(usize::MAX);
    if size > max_record_bytes {
        return Ok(None);
    }
    let mut object = Map::new();
    for (i, column) in columns.iter().enumerate() {
        let value = match row.get_ref(i + 1)? {
            ValueRef::Text(s) => {
                let text = String::from_utf8_lossy(s);
                if matches!(
                    column.as_str(),
                    "payload" | "data" | "json" | "content" | "value"
                ) {
                    serde_json::from_str(&text).unwrap_or_else(|_| Value::String(text.into_owned()))
                } else {
                    Value::String(text.into_owned())
                }
            }
            ValueRef::Integer(n) => Value::from(n),
            _ => Value::Null,
        };
        object.insert(column.clone(), value);
    }
    Ok(Some(object))
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
