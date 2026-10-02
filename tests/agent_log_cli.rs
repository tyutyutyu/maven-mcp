//! Secret-free fixtures exercise the real standalone process, never the MCP server.
use maven_mcp::log_analysis::{LogSource, read_event_log, read_events};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::TempDir;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/agent_logs")
        .join(name)
}
fn run(paths: &[&Path], args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_maven-agent-log"))
        .args(paths)
        .args(args)
        .env("HOME", "/synthetic-home")
        .env("USER", "synthetic-local-user")
        .env_remove("USERNAME")
        .env_remove("USERPROFILE")
        .env_remove("MAVEN_TRUSTED_PROJECT_DIRECTORIES")
        .output()
        .unwrap()
}
fn json_output(paths: &[&Path], args: &[&str]) -> Value {
    let mut options = vec!["--format", "json"];
    options.extend(args);
    let output = run(paths, &options);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn normalizes_codex_context_and_distinguishes_calls_from_examples() {
    let path = fixture("codex.jsonl");
    let events = json_output(&[&path], &["--unsafe-no-redact"]);
    let events = events.as_array().unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["session"], "codex-session");
    assert_eq!(events[0]["agent"], "codex");
    assert_eq!(events[0]["project"], "/work/synthetic-project");
    assert_eq!(events[0]["cwd"], "/work/synthetic-project");
    assert_eq!(events[0]["timestamp"], "2026-01-01T10:00:00.000Z");
    assert_eq!(events[0]["maven"]["tests"], json!(["FooTest#works"]));
    let raw = json_output(&[&path], &["--keep-duplicates"]);
    assert_eq!(raw.as_array().unwrap().len(), 3);
    let records = read_event_log(&path, LogSource::Codex).unwrap();
    assert_eq!(records.skipped_records, 2);
}

#[test]
fn reads_vscode_insiders_and_intellij_rotations_from_directories() {
    let root = TempDir::new().unwrap();
    let insiders = root
        .path()
        .join("Code - Insiders/User/workspaceStorage/s/chatSessions");
    std::fs::create_dir_all(&insiders).unwrap();
    std::fs::copy(fixture("vscode.json"), insiders.join("session.json")).unwrap();
    std::fs::copy(fixture("idea.log.1"), root.path().join("idea.log.1")).unwrap();
    let value = json_output(&[root.path()], &[]);
    let events = value.as_array().unwrap();
    assert_eq!(events.len(), 4);
    assert_eq!(events[0]["ide"], "vscode-insiders");
    assert_eq!(events[0]["agent"], "github.copilot");
    assert_eq!(events[0]["category"], "resource_inspection");
    assert_eq!(events[0]["timestamp"], "2026-01-01T00:00:00.000Z");
    assert_eq!(events[1]["session"], "idea-session");
    assert_eq!(events[1]["agent"], "synthetic-agent");
    assert_eq!(events[1]["category"], "class_inspection");
    assert_eq!(events[2]["category"], "pom_inspection");
    assert_eq!(events[3]["category"], "jar_inspection");
    assert_eq!(
        json_output(&[&fixture("vscode.json")], &["--source", "vs-code"])[0]["ide"],
        "vscode"
    );
}

#[test]
fn sqlite_tool_parts_inherit_session_and_message_metadata_without_writes() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("kilo.db");
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE session(id TEXT, directory TEXT); CREATE TABLE message(id TEXT, session_id TEXT, data TEXT); CREATE TABLE part(id TEXT, session_id TEXT, message_id TEXT, data TEXT); CREATE TABLE notes(command TEXT);
            INSERT INTO session VALUES ('kilo-session', '/work/kilo-project'); INSERT INTO message VALUES ('message-1','kilo-session','{\"agent\":\"build\",\"role\":\"assistant\"}'); INSERT INTO notes VALUES ('mvn deploy');").unwrap();
        let data = json!({"type":"tool","tool":"bash","callID":"kilo-call","state":{"status":"completed","input":{"command":"unzip -p /repository/a.jar config.properties","cwd":"/work/kilo-project"},"time":{"start":1767225600000i64},"output":{"type":"tool_use","name":"bash","input":{"command":"mvn deploy"}}}});
        conn.execute(
            "INSERT INTO part VALUES ('part-1','kilo-session','message-1',?1)",
            [data.to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part VALUES ('part-2','kilo-session','message-1',?1)",
            ["{incomplete"],
        )
        .unwrap();
    }
    let before = std::fs::read(&path).unwrap();
    let value = json_output(&[&path], &["--unsafe-no-redact"]);
    assert_eq!(value.as_array().unwrap().len(), 1);
    assert_eq!(value[0]["agent"], "build");
    assert_eq!(value[0]["session"], "kilo-session");
    assert_eq!(value[0]["project"], "/work/kilo-project");
    assert_eq!(value[0]["timestamp"], "2026-01-01T00:00:00.000Z");
    assert_eq!(value[0]["category"], "resource_inspection");
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(!path.with_extension("db-journal").exists());
}

#[test]
fn sqlite_explicit_shell_columns_are_recognized_without_accepting_output_rows() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("shell-columns.sqlite");
    {
        let connection = Connection::open(&path).unwrap();
        for (index, column) in ["tool", "tool_name", "toolName", "toolId"]
            .iter()
            .enumerate()
        {
            connection
                .execute_batch(&format!(
                    "CREATE TABLE calls_{index}({column} TEXT, command TEXT, role TEXT);
                 INSERT INTO calls_{index} VALUES ('execute_command', 'mvn verify', 'assistant');
                 INSERT INTO calls_{index} VALUES ('execute_command', 'mvn deploy', 'tool');"
                ))
                .unwrap();
        }
    }
    let before = std::fs::read(&path).unwrap();
    let events = json_output(&[&path], &[]);
    assert_eq!(events.as_array().unwrap().len(), 4);
    for event in events.as_array().unwrap() {
        assert_eq!(event["command"], "mvn verify");
        assert_eq!(event["ide"], "kilo");
        assert_eq!(event["category"], "maven");
    }
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn deduplicates_progressive_mirrors_but_preserves_sessions_projects_and_repetitions() {
    let root = TempDir::new().unwrap();
    let a = root.path().join("a.json");
    let b = root.path().join("b.json");
    let record = |session: &str, project: &str, command: &str, id: Option<&str>| json!({"sessionId":session,"project":project,"response":[{"kind":"toolInvocationSerialized","toolId":"run_in_terminal","toolCallId":id,"input":{"command":command}}]});
    std::fs::write(
        &a,
        serde_json::to_vec(&json!([
            record("s", "p", "mvn", Some("call")),
            record("other", "p", "mvn test", Some("call")),
            record("s", "other-project", "mvn test", Some("call")),
            record("s", "p", "mvn test", None),
            record("s", "p", "mvn test", None)
        ]))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        &b,
        serde_json::to_vec(&record("s", "p", "mvn test", Some("call"))).unwrap(),
    )
    .unwrap();
    let events = json_output(&[&a, &b], &[]);
    assert_eq!(events.as_array().unwrap().len(), 5);
    assert!(
        events
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["command"] == "mvn test")
    );
    assert_eq!(json_output(&[&a, &b], &[]), json_output(&[&b, &a], &[]));
}

#[test]
fn redacts_every_exported_field_and_keeps_report_formats_deterministic() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("privacy.json");
    // Assemble synthetic auth pairs at runtime so scanners do not mistake fixtures for secrets.
    let basic_auth = ["fakeuser", "fakepass"].join(":");
    let command = format!(
        "mvn verify -Dpassword='FAKE PRIVATE VALUE' -DapiKey=FAKE_KEY -Durl=https://{basic_auth}@example.invalid -Dtest=/home/foreignuser/work/demo/Foo -Pfixture-marker -Dnote=foreignuser && curl -H 'Authorization: Bearer FAKE_BEARER' --token FAKE_FLAG"
    );
    let log = json!({"sessionId":"foreignuser-fixture-marker","agent":"fixture-marker","project":"/home/foreignuser/work/demo","response":[{"toolId":"run_in_terminal","input":{"cwd":"/home/foreignuser/work/demo","command":command}}]});
    std::fs::write(&path, serde_json::to_vec(&log).unwrap()).unwrap();
    for format in ["terminal", "json", "jsonl", "csv", "markdown"] {
        let args = ["--format", format, "--redact-pattern", "fixture-marker"];
        let first = run(&[&path], &args);
        let second = run(&[&path], &args);
        assert!(first.status.success());
        assert_eq!(first.stdout, second.stdout);
        let report = String::from_utf8(first.stdout).unwrap();
        for secret in [
            "foreignuser",
            "/home/",
            "fixture-marker",
            "FAKE PRIVATE VALUE",
            "FAKE_KEY",
            basic_auth.as_str(),
            "FAKE_BEARER",
            "FAKE_FLAG",
        ] {
            assert!(
                !report.contains(secret),
                "{format} leaked {secret}: {report}"
            );
        }
    }
    let raw = json_output(&[&path], &["--unsafe-no-redact"]);
    assert!(
        raw[0]["command"]
            .as_str()
            .unwrap()
            .contains("FAKE PRIVATE VALUE")
    );
}

#[test]
fn filters_and_groups_by_all_dimensions_and_calendar_periods() {
    let path = fixture("codex.jsonl");
    for (dimension, expected) in [
        ("day", "2026-01-01"),
        ("month", "2026-01"),
        ("ide", "codex"),
        ("agent", "codex"),
        ("project", "<WORKSPACE>"),
        ("session", "codex-session"),
        ("category", "maven"),
    ] {
        let value = json_output(&[&path], &["--group-by", dimension]);
        assert_eq!(value, json!([{"group":expected,"count":2}]));
    }
    let filtered = json_output(
        &[&path],
        &[
            "--since",
            "2026-01-01T11:00:30+01:00",
            "--until",
            "2026-01-01",
            "--ide",
            "CODEX",
            "--agent",
            "codex",
            "--project",
            "/work/synthetic-project",
            "--session",
            "codex-session",
            "--category",
            "maven",
        ],
    );
    assert_eq!(filtered.as_array().unwrap().len(), 1);
    for args in [["--since", "bad-date"], ["--until", "2026-02-30"]] {
        assert!(!run(&[&path], &args).status.success());
    }
}

#[test]
fn malformed_files_and_output_errors_never_echo_sensitive_input() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("PRIVATE-PATH.json");
    std::fs::write(&path, "{PRIVATE-CONTENT").unwrap();
    let output = run(&[&path], &["--format", "json"]);
    assert!(!output.status.success());
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(!error.contains("PRIVATE"));
    assert!(!error.contains(&root.path().display().to_string()));
    let valid = fixture("vscode.json");
    let original = std::fs::read(&valid).unwrap();
    assert!(
        !run(&[&valid], &["--output", valid.to_str().unwrap()])
            .status
            .success()
    );
    assert_eq!(std::fs::read(&valid).unwrap(), original);
    assert_eq!(
        read_events(&fixture("idea.log.1"), LogSource::Auto)
            .unwrap()
            .len(),
        3
    );
}

#[test]
fn discovers_only_from_configured_roots_without_mcp_or_network() {
    let root = TempDir::new().unwrap();
    let sessions = root.path().join("custom-codex/sessions");
    let code = root
        .path()
        .join("config/Code/User/workspaceStorage/demo/chatSessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::create_dir_all(&code).unwrap();
    std::fs::copy(fixture("codex.jsonl"), sessions.join("session.jsonl")).unwrap();
    std::fs::copy(fixture("vscode.json"), code.join("session.json")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_maven-agent-log"))
        .args(["--discover", "--format", "json"])
        .env("HOME", root.path())
        .env("USERPROFILE", root.path())
        .env("CODEX_HOME", root.path().join("custom-codex"))
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("XDG_CACHE_HOME", root.path().join("cache"))
        .env("APPDATA", root.path().join("appdata"))
        .env("LOCALAPPDATA", root.path().join("localappdata"))
        .env_remove("MAVEN_TRUSTED_PROJECT_DIRECTORIES")
        .output()
        .unwrap();
    assert!(output.status.success());
    let events: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(events.len(), 3);
}

#[test]
fn inspection_categories_and_escaped_reports_follow_executed_commands() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("commands.json");
    let commands = [
        ("find ~/.m2/repository -name '*.jar'", "jar_inspection"),
        ("jar tf demo.jar|grep Foo.class", "class_inspection"),
        (
            "unzip -p demo.jar META-INF/config.xml",
            "resource_inspection",
        ),
        ("grep version ~/.m2/repository/demo.pom", "pom_inspection"),
        ("rg artifactId pom.xml", "pom_inspection"),
        ("javap -classpath demo.jar example.Foo", "class_inspection"),
        ("echo 'mvn test'", "shell"),
        ("printf '%s' 'a,b\"c|`d`<script>'", "shell"),
    ];
    let invocations: Vec<Value> = commands.iter().enumerate().map(|(i, (command, _))| json!({"toolId":"run_in_terminal","toolCallId":i.to_string(),"input":{"command":command}})).collect();
    std::fs::write(
        &path,
        serde_json::to_vec(&json!({"sessionId":"inspection-session","response":invocations}))
            .unwrap(),
    )
    .unwrap();
    let value = json_output(&[&path], &[]);
    for (command, category) in commands {
        assert!(
            value
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["command"] == command && e["category"] == category),
            "{command}"
        );
    }
    let csv = String::from_utf8(run(&[&path], &["--format", "csv"]).stdout).unwrap();
    assert!(csv.contains("a,b\"\"c"));
    let markdown = String::from_utf8(run(&[&path], &["--format", "markdown"]).stdout).unwrap();
    assert!(markdown.contains("&#124;&#96;d&#96;&lt;script&gt;"));
    for format in ["terminal", "jsonl", "csv", "markdown"] {
        let first = run(&[&path], &["--format", format, "--group-by", "category"]);
        let second = run(&[&path], &["--format", format, "--group-by", "category"]);
        assert!(first.status.success());
        assert!(!first.stdout.is_empty());
        assert_eq!(first.stdout, second.stdout);
    }
}

#[test]
fn redaction_does_not_merge_distinct_project_counts() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("projects.json");
    std::fs::write(&path, serde_json::to_vec(&json!([
        {"project":"/work/a","sessionId":"a","response":[{"toolId":"run_in_terminal","input":{"command":"mvn test"}}]},
        {"project":"/work/b","sessionId":"b","response":[{"toolId":"run_in_terminal","input":{"command":"mvn test"}}]}
    ])).unwrap()).unwrap();
    assert_eq!(
        json_output(&[&path], &["--group-by", "project"]),
        json!([
            {"group":"<WORKSPACE> [1]", "count":1}, {"group":"<WORKSPACE> [2]", "count":1}
        ])
    );
}

#[test]
fn redacts_escaped_concatenated_and_flag_credentials_in_every_format() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("credentials.json");
    let commands = [
        r#"mvn -Dpassword="FAKE_HEAD\"FAKE_SUFFIX" verify PUBLIC_MARKER"#.to_owned(),
        r#"mvn -Dpassword='FAKE_SINGLE'"FAKE_DOUBLE"FAKE_TAIL verify PUBLIC_MARKER"#.to_owned(),
        r#"TOKEN=FAKE_ESCAPED\ SPACE mvn test PUBLIC_MARKER"#.to_owned(),
        format!(
            "curl -u {} https://example.invalid PUBLIC_MARKER",
            ["demo", "FAKE_BASIC"].join(":"),
        ),
        format!(
            r#"curl --user "{}" https://example.invalid PUBLIC_MARKER"#,
            ["demo", r#"FAKE_LONG\"FAKE_END"#].join(":"),
        ),
        format!(
            "curl --user={} https://example.invalid PUBLIC_MARKER",
            ["demo", "FAKE_EQUALS"].join(":"),
        ),
        format!(
            "curl -u{} -U {} https://example.invalid PUBLIC_MARKER",
            ["demo", "FAKE_ATTACHED"].join(":"),
            ["proxy", "FAKE_PROXY"].join(":"),
        ),
        format!(
            "curl --proxy-user={} https://example.invalid PUBLIC_MARKER",
            ["proxy", "FAKE_PROXY_LONG"].join(":"),
        ),
        r#"curl -H 'Authorization: Bearer FAKE_BEARER' https://example.invalid PUBLIC_MARKER"#
            .to_owned(),
        r#"mvn -Dpassword="FAKE_INCOMPLETE"#.to_owned(),
    ];
    let calls: Vec<_> = commands.iter().enumerate().map(|(id, cmd)| {
        json!({"toolId":"run_in_terminal","toolCallId":id.to_string(),"input":{"command":cmd}})
    }).collect();
    std::fs::write(
        &path,
        serde_json::to_vec(&json!({"sessionId":"privacy-regressions","response":calls})).unwrap(),
    )
    .unwrap();
    let events = json_output(&[&path], &[]);
    assert_eq!(events.as_array().unwrap().len(), commands.len());
    assert_eq!(
        events
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["command"].as_str().unwrap().contains("PUBLIC_MARKER"))
            .count(),
        commands.len() - 1
    );
    for format in ["terminal", "json", "jsonl", "csv", "markdown"] {
        let output = run(&[&path], &["--format", format]);
        assert!(output.status.success());
        let report = String::from_utf8(output.stdout).unwrap();
        assert!(!report.contains("FAKE_"), "{format}: {report}");
        assert!(
            !report.contains("SPACE"),
            "escaped whitespace secret leaked: {report}"
        );
        assert!(report.contains("REDACTED"));
    }
}

#[test]
fn malformed_unicode_timestamps_are_rejected_without_panicking() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("timestamps.jsonl");
    let malformed = [
        "2026-01-01T00:00:00.𝟙Z",
        "2026-01-01T00:00:00+𝟙0:00",
        "2026-01-01T00:00:00+01:𝟙0",
        "2026-01-01T00:00:00.١Z",
    ];
    let mut records: Vec<_> = malformed.iter().map(|timestamp| {
        json!({"type":"function_call","name":"exec_command","timestamp":timestamp,"arguments":{"cmd":"mvn test"}}).to_string()
    }).collect();
    records.push(json!({"type":"function_call","name":"exec_command","timestamp":"2026-01-01T00:00:00Z","arguments":{"cmd":"mvn verify"}}).to_string());
    std::fs::write(&path, records.join("\n")).unwrap();
    let events = json_output(&[&path], &[]);
    let events = events.as_array().unwrap();
    assert_eq!(events.len(), malformed.len() + 1);
    assert_eq!(
        events
            .iter()
            .filter(|event| event["timestamp"].is_null())
            .count(),
        malformed.len()
    );
    for timestamp in malformed {
        for flag in ["--since", "--until"] {
            let output = run(&[&path], &[flag, timestamp]);
            assert_eq!(output.status.code(), Some(1));
            let stderr = String::from_utf8(output.stderr).unwrap();
            assert!(stderr.contains("time filters require"));
            assert!(!stderr.contains("panicked"));
        }
    }
}

#[test]
fn descriptor_redirections_preserve_maven_arguments_and_repository_pipelines() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("redirects.json");
    let commands = [
        "mvn 2>&1 verify -Dtest=Foo",
        "2>&1 mvn verify -Dtest=Foo",
        "mvn verify 3>&2 -Dtest=Foo",
        "mvn >report.txt verify -Dtest=Foo",
        "mvn verify -Dtest=Foo 2>&1|tee report",
        "mvn &>report.txt verify -Dtest=Foo",
        "mvn 2>errors.log verify -Dtest=Foo",
        "mvn <input.txt verify -Dtest=Foo",
        "mvn 3<>channel verify -Dtest=Foo",
    ];
    let mut calls: Vec<_> = commands.iter().enumerate().map(|(id, cmd)| json!({"toolId":"run_in_terminal","toolCallId":id.to_string(),"input":{"command":cmd}})).collect();
    calls.push(json!({"toolId":"run_in_terminal","toolCallId":"jar","input":{"command":"2>/dev/null jar tf demo.jar | rg META-INF"}}));
    std::fs::write(&path, serde_json::to_vec(&calls).unwrap()).unwrap();
    let events = json_output(&[&path], &[]);
    assert_eq!(events.as_array().unwrap().len(), commands.len() + 1);
    for event in events.as_array().unwrap() {
        if event["call_id"] == "jar" {
            assert_eq!(event["category"], "resource_inspection");
        } else {
            assert_eq!(
                event["maven"]["lifecycle_goals"],
                json!(["verify"]),
                "{event}"
            );
            assert_eq!(event["maven"]["tests"], json!(["Foo"]), "{event}");
        }
    }
}

#[test]
fn sqlite_output_row_envelopes_cannot_be_reinterpreted_as_tool_calls() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("envelopes.sqlite");
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE events(type TEXT, kind TEXT, role TEXT, content TEXT);")
            .unwrap();
        let example =
            json!({"type":"tool_use","name":"bash","input":{"command":"mvn deploy"}}).to_string();
        for kind in [
            "tool_result",
            "tool_output",
            "function_call_output",
            "custom_tool_call_output",
            "text",
            "markdownContent",
            "patch",
        ] {
            conn.execute(
                "INSERT INTO events(type,content) VALUES (?1,?2)",
                [kind, &example],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO events(type,kind,content) VALUES ('message',?1,?2)",
                [kind, &example],
            )
            .unwrap();
        }
        for role in ["user", "tool", "system", "developer", "function"] {
            conn.execute(
                "INSERT INTO events(role,content) VALUES (?1,?2)",
                [role, &example],
            )
            .unwrap();
        }
        let real =
            json!({"type":"tool_use","name":"bash","input":{"command":"mvn verify"}}).to_string();
        conn.execute(
            "INSERT INTO events(type,role,content) VALUES ('message','assistant',?1)",
            [real],
        )
        .unwrap();
    }
    let events = json_output(&[&path], &[]);
    assert_eq!(events.as_array().unwrap().len(), 1);
    assert_eq!(events[0]["command"], "mvn verify");
}

#[test]
fn overlapping_intellij_rotations_merge_without_merging_unrelated_logs() {
    let root = TempDir::new().unwrap();
    let line = "2026-01-01T10:00:00Z INFO Executing command: mvn verify\n";
    for name in [
        "idea.log",
        "idea.log.1",
        "idea.log.12",
        "other.log",
        "idea.log.backup",
    ] {
        std::fs::write(root.path().join(name), line).unwrap();
    }
    let other_directory = root.path().join("another-ide");
    std::fs::create_dir(&other_directory).unwrap();
    std::fs::write(other_directory.join("idea.log"), line).unwrap();
    let events = json_output(&[root.path()], &[]);
    assert_eq!(events.as_array().unwrap().len(), 4);
    assert_eq!(
        json_output(&[root.path()], &["--keep-duplicates"])
            .as_array()
            .unwrap()
            .len(),
        6
    );
    assert_eq!(
        json_output(&[root.path()], &["--group-by", "category"]),
        json!([{"group":"maven","count":4}])
    );
}
