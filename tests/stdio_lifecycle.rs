mod support;

use std::process::Stdio;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
};

use rmcp::model::CallToolRequestParams;
use support::TestServer;

#[tokio::test]
async fn stdout_contains_only_json_rpc_and_eof_stops_the_server() -> Result<()> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_maven-mcp"))
        .env("MAVEN_MCP_INDEX_CACHE", "false")
        .env("RUST_LOG", "maven_mcp=info")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let mut stdin = child.stdin.take().context("missing child stdin")?;
    let stdout = child.stdout.take().context("missing child stdout")?;
    let mut lines = BufReader::new(stdout).lines();
    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "stdio-lifecycle-test", "version": "1.0" }
        }
    });
    stdin
        .write_all(format!("{initialize}\n").as_bytes())
        .await?;
    let response_line = tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line())
        .await
        .context("initialize response timed out")??
        .context("server closed stdout before initialize response")?;
    let response: Value =
        serde_json::from_str(&response_line).context("server stdout must contain JSON-RPC only")?;
    assert_eq!(response["id"], 1);
    assert!(response.get("result").is_some());

    drop(stdin);
    let status = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
        .await
        .context("server did not stop after stdin EOF")??;
    assert!(status.success(), "server exited with {status}");
    while let Some(line) = lines.next_line().await? {
        let message: Value = serde_json::from_str(&line)
            .with_context(|| format!("non-JSON content on server stdout: {line}"))?;
        assert_eq!(message["jsonrpc"], "2.0", "invalid JSON-RPC output: {line}");
    }

    let mut stderr = String::new();
    let mut stderr_reader = BufReader::new(child.stderr.take().context("missing child stderr")?);
    tokio::io::AsyncReadExt::read_to_string(&mut stderr_reader, &mut stderr).await?;
    assert!(stderr.contains("MCP server ready for request-scoped Maven projects"));
    assert!(!response_line.contains("Maven repository index ready"));
    Ok(())
}

#[tokio::test]
async fn stats_cli_reports_empty_runtime_as_successful_json() -> Result<()> {
    let runtime = tempfile::tempdir()?;
    let output = Command::new(env!("CARGO_BIN_EXE_maven-mcp"))
        .env("MAVEN_MCP_INDEX_CACHE", "false")
        .arg("stats")
        .arg("--json")
        .env("MAVEN_MCP_RUNTIME_DIR", runtime.path())
        .output()
        .await?;
    assert!(
        output.status.success(),
        "stats CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["instances"].as_array().unwrap().len(), 0);
    Ok(())
}

#[tokio::test]
async fn stats_cli_reports_live_stdio_instance() -> Result<()> {
    let runtime = tempfile::tempdir()?;
    let mut child = Command::new(env!("CARGO_BIN_EXE_maven-mcp"))
        .env("MAVEN_MCP_INDEX_CACHE", "false")
        .env("MAVEN_MCP_RUNTIME_DIR", runtime.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().context("missing child stdin")?;
    let stdout = child.stdout.take().context("missing child stdout")?;
    let mut lines = BufReader::new(stdout).lines();
    stdin
        .write_all(
            format!(
                "{}\n",
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-06-18",
                        "capabilities": {},
                        "clientInfo": { "name": "stats-test", "version": "1.0" }
                    }
                })
            )
            .as_bytes(),
        )
        .await?;
    tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line())
        .await
        .context("initialize response timed out")??
        .context("server closed before initialize response")?;

    let output = Command::new(env!("CARGO_BIN_EXE_maven-mcp"))
        .env("MAVEN_MCP_INDEX_CACHE", "false")
        .arg("stats")
        .arg("--json")
        .env("MAVEN_MCP_RUNTIME_DIR", runtime.path())
        .output()
        .await?;
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(report["instances"].as_array().unwrap().len(), 1);
    assert_eq!(report["instances"][0]["pid"], child.id().unwrap());
    assert_eq!(
        report["instances"][0]["binary_version"],
        env!("CARGO_PKG_VERSION")
    );
    assert_eq!(
        report["instances"][0]["build_commit"],
        json!(maven_mcp::build_info::build_commit())
    );
    let executable = std::fs::canonicalize(env!("CARGO_BIN_EXE_maven-mcp"))?;
    assert_eq!(
        report["instances"][0]["executable"],
        json!(executable.display().to_string())
    );

    drop(stdin);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn sigterm_stops_the_stdio_server_cleanly() -> Result<()> {
    use nix::{
        sys::signal::{Signal, kill},
        unistd::Pid,
    };

    let mut child = Command::new(env!("CARGO_BIN_EXE_maven-mcp"))
        .env("MAVEN_MCP_INDEX_CACHE", "false")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().context("missing child stdin")?;
    let stdout = child.stdout.take().context("missing child stdout")?;
    let _stderr = child.stderr.take().context("missing child stderr")?;
    stdin
        .write_all(
            format!(
                "{}\n",
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-06-18",
                        "capabilities": {},
                        "clientInfo": { "name": "sigterm-test", "version": "1.0" }
                    }
                })
            )
            .as_bytes(),
        )
        .await?;
    let mut stdout_lines = BufReader::new(stdout).lines();
    tokio::time::timeout(std::time::Duration::from_secs(5), stdout_lines.next_line())
        .await
        .context("initialize response timed out")??
        .context("server closed before initialize response")?;
    stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .await?;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let process_id = child.id().context("missing server process id")?;
    kill(Pid::from_raw(i32::try_from(process_id)?), Signal::SIGTERM)?;
    let status = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
        .await
        .context("server did not stop after SIGTERM")??;
    assert!(status.success(), "server exited with {status}");
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn sigterm_does_not_leave_an_active_maven_process_group() -> Result<()> {
    use nix::{
        errno::Errno,
        sys::signal::{Signal, kill},
        unistd::Pid,
    };
    use std::os::unix::fs::PermissionsExt;

    let fixture = TestServer::start_with_project().await?;
    let project = fixture.project_path().context("missing fixture project")?;
    let wrapper = project.join("mvnw");
    std::fs::write(
        &wrapper,
        "#!/bin/sh\nprintf '%s' \"$$\" > maven-child.pid\nsleep 300 &\nwait $!\n",
    )?;
    let mut permissions = wrapper.metadata()?.permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&wrapper, permissions)?;

    let (client, server_process_id) = fixture.connect_with_pid().await?;
    let request = CallToolRequestParams::new("run_maven").with_arguments(
        json!({
            "project_path": project.display().to_string(),
            "arguments": ["custom:goal"]
        })
        .as_object()
        .context("Maven arguments must be an object")?
        .clone(),
    );
    let call = tokio::spawn(async move { client.call_tool(request).await });
    let pid_path = project.join("maven-child.pid");
    let maven_process_id = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Ok(value) = std::fs::read_to_string(&pid_path) {
                break value.trim().parse::<i32>();
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .context("Maven child did not start")??;

    kill(
        Pid::from_raw(i32::try_from(server_process_id)?),
        Signal::SIGTERM,
    )?;
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), call)
        .await
        .context("MCP call did not stop with the server")?;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match kill(Pid::from_raw(maven_process_id), None) {
                Err(Errno::ESRCH) => break,
                _ => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
            }
        }
    })
    .await
    .context("Maven process group survived MCP server shutdown")?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn eof_does_not_leave_an_active_maven_process_group() -> Result<()> {
    use nix::{errno::Errno, sys::signal::kill, unistd::Pid};
    use std::os::unix::fs::PermissionsExt;

    let fixture = TestServer::start_with_project().await?;
    let project = fixture.project_path().context("missing fixture project")?;
    let wrapper = project.join("mvnw");
    std::fs::write(
        &wrapper,
        "#!/bin/sh\nprintf '%s' \"$$\" > maven-child.pid\nsleep 300 &\nwait $!\n",
    )?;
    let mut permissions = wrapper.metadata()?.permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&wrapper, permissions)?;

    let mut child = Command::new(env!("CARGO_BIN_EXE_maven-mcp"))
        .env("MAVEN_MCP_INDEX_CACHE", "false")
        .env(
            "MAVEN_TRUSTED_PROJECT_DIRECTORIES",
            fixture.trusted_project_directory().unwrap(),
        )
        .env(
            "MAVEN_EXECUTION_REPO_PATH",
            project.join("execution-repository"),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().context("missing child stdin")?;
    let stdout = child.stdout.take().context("missing child stdout")?;
    let _stderr = child.stderr.take().context("missing child stderr")?;
    let mut lines = BufReader::new(stdout).lines();
    stdin
        .write_all(
            format!(
                "{}\n",
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-06-18",
                        "capabilities": {},
                        "clientInfo": { "name": "eof-process-group-test", "version": "1.0" }
                    }
                })
            )
            .as_bytes(),
        )
        .await?;
    tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line())
        .await
        .context("initialize response timed out")??
        .context("server closed before initialize response")?;
    stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .await?;
    stdin
        .write_all(
            format!(
                "{}\n",
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "tools/call",
                    "params": {
                        "name": "run_maven_lifecycle",
                        "arguments": {
                            "project_path": project.display().to_string(),
                            "phase": "compile"
                        }
                    }
                })
            )
            .as_bytes(),
        )
        .await?;
    let pid_path = project.join("maven-child.pid");
    let maven_process_id = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Ok(value) = std::fs::read_to_string(&pid_path) {
                break value.trim().parse::<i32>();
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .context("Maven child did not start")??;

    drop(stdin);
    let status = tokio::time::timeout(std::time::Duration::from_secs(10), child.wait())
        .await
        .context("server did not stop after stdin EOF")??;
    assert!(status.success(), "server exited with {status}");
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            match kill(Pid::from_raw(maven_process_id), None) {
                Err(Errno::ESRCH) => break,
                _ => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
            }
        }
    })
    .await
    .context("Maven process group survived STDIO EOF")?;
    Ok(())
}

#[tokio::test]
async fn exited_parent_pipe_holders_stop_on_eof_sigterm_and_request_cancellation() -> Result<()> {
    use nix::{
        sys::signal::{Signal, kill},
        unistd::Pid,
    };
    use std::time::Duration;

    for action in ["eof", "sigterm", "cancel"] {
        for redirections in ["", ">/dev/null", "2>/dev/null"] {
            let fixture = TestServer::start_with_project().await?;
            let project = fixture.project_path().unwrap();
            let holder = support::PipeHolder::new(project, redirections)?;
            let other = fixture.add_trusted_project("recovery")?;
            let mut child = Command::new(env!("CARGO_BIN_EXE_maven-mcp"))
                .env("MAVEN_MCP_INDEX_CACHE", "false")
                .env(
                    "MAVEN_TRUSTED_PROJECT_DIRECTORIES",
                    fixture.trusted_project_directory().unwrap(),
                )
                .env("MAVEN_TIMEOUT_SECONDS", "30")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()?;
            let mut stdin = child.stdin.take().context("missing stdin")?;
            let mut lines = BufReader::new(child.stdout.take().context("missing stdout")?).lines();
            stdin
                .write_all(
                    format!(
                        "{}\n",
                        json!({
                            "jsonrpc": "2.0", "id": 1, "method": "initialize",
                            "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                                "clientInfo": {"name": "pipe-cleanup", "version": "1"}}
                        })
                    )
                    .as_bytes(),
                )
                .await?;
            tokio::time::timeout(Duration::from_secs(3), lines.next_line())
                .await??
                .context("missing initialize response")?;
            stdin
                .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
                .await?;
            stdin
                .write_all(
                    format!(
                        "{}\n",
                        json!({
                            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                            "params": {"name": "run_maven", "arguments": {
                                "project_path": project, "arguments": []}}
                        })
                    )
                    .as_bytes(),
                )
                .await?;
            holder.wait_started().await?;
            if action == "cancel" {
                stdin.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/cancelled\",\"params\":{\"requestId\":2,\"reason\":\"regression\"}}\n").await?;
                holder.assert_stopped().await.with_context(|| {
                    format!("descendant cleanup: {action}, pipes: {redirections}")
                })?;
                stdin
                    .write_all(
                        format!(
                            "{}\n",
                            json!({
                                "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                                "params": {"name": "run_maven", "arguments": {
                                    "project_path": other, "arguments": []}}
                            })
                        )
                        .as_bytes(),
                    )
                    .await?;
                let response = tokio::time::timeout(Duration::from_secs(3), async {
                    loop {
                        let line = lines.next_line().await?.context("server closed stdout")?;
                        let value: Value = serde_json::from_str(&line)?;
                        if value["id"] == 3 {
                            break Ok::<_, anyhow::Error>(value);
                        }
                    }
                })
                .await??;
                assert_eq!(
                    response["result"]["structuredContent"]["status"], "success",
                    "{response}"
                );
            } else if action == "sigterm" {
                kill(
                    Pid::from_raw(i32::try_from(child.id().context("missing server PID")?)?),
                    Signal::SIGTERM,
                )?;
            }
            if action != "sigterm" {
                drop(stdin);
            }
            let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
                .await
                .with_context(|| format!("server shutdown: {action}, pipes: {redirections}"))??;
            assert!(status.success(), "{action}: {status}");
            holder
                .assert_stopped()
                .await
                .with_context(|| format!("descendant cleanup: {action}, pipes: {redirections}"))?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn blocked_index_work_does_not_keep_the_server_alive_after_eof_or_sigterm() -> Result<()> {
    use std::time::Duration;

    for action in ["eof", "sigterm"] {
        let fixture = TestServer::start_with_project().await?;
        let project = fixture.project_path().unwrap();
        let _blocked = support::install_blocked_classpath(project)?;
        let mut child = Command::new(env!("CARGO_BIN_EXE_maven-mcp"))
            .env("MAVEN_MCP_INDEX_CACHE", "false")
            .env(
                "MAVEN_TRUSTED_PROJECT_DIRECTORIES",
                fixture.trusted_project_directory().unwrap(),
            )
            .env(
                "MAVEN_EXECUTION_REPO_PATH",
                project.join("execution-repository"),
            )
            .env("MAVEN_TIMEOUT_SECONDS", "30")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let mut stdin = child.stdin.take().context("missing stdin")?;
        let mut lines = BufReader::new(child.stdout.take().context("missing stdout")?).lines();
        stdin
            .write_all(
                format!(
                    "{}\n",
                    json!({
                        "jsonrpc": "2.0", "id": 1, "method": "initialize",
                        "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                            "clientInfo": {"name": "blocked-index-shutdown", "version": "1"}}
                    })
                )
                .as_bytes(),
            )
            .await?;
        tokio::time::timeout(Duration::from_secs(5), lines.next_line())
            .await??
            .context("missing initialize response")?;
        stdin
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
            .await?;
        stdin
            .write_all(
                format!(
                    "{}\n",
                    json!({
                        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                        "params": {"name": "search_classes", "arguments": {
                            "project_path": project, "query": "One"}}
                    })
                )
                .as_bytes(),
            )
            .await?;
        // The index build now blocks on the pipe JAR inside a blocking thread that no
        // checkpoint can reach; shutdown must still be bounded.
        support::wait_for_blocked_fifo_readers(child.id().context("missing server PID")?, 1)
            .await?;

        if action == "sigterm" {
            use nix::{
                sys::signal::{Signal, kill},
                unistd::Pid,
            };
            kill(
                Pid::from_raw(i32::try_from(child.id().context("missing server PID")?)?),
                Signal::SIGTERM,
            )?;
        } else {
            drop(stdin);
        }
        let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .with_context(|| format!("server kept running after {action} with blocked work"))??;
        assert!(status.success(), "{action}: {status}");
    }
    Ok(())
}

#[tokio::test]
async fn debug_logging_and_responses_never_reveal_credentials_or_project_paths() -> Result<()> {
    const MARKER: &str = "synthetic-secret-marker";
    let fixture = TestServer::start_with_project().await?;
    let project = fixture.project_path().context("missing fixture project")?;
    std::fs::write(
        project.join("mvnw"),
        format!(
            "#!/bin/sh\nprintf '%s\\n' 'İ password=é' 'Authorization: Bearer {MARKER}' \"root=$PWD token={MARKER}\" 'secret=a&{MARKER};x'\nprintf 'Authorization: Basic {MARKER}\\n' >&2\n"
        ),
    )?;
    let mut child = Command::new(env!("CARGO_BIN_EXE_maven-mcp"))
        .env("MAVEN_MCP_INDEX_CACHE", "false")
        .env("RUST_LOG", "trace")
        .env(
            "MAVEN_TRUSTED_PROJECT_DIRECTORIES",
            fixture.trusted_project_directory().unwrap(),
        )
        .env(
            "MAVEN_EXECUTION_REPO_PATH",
            project.join("execution-repository"),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().context("missing child stdin")?;
    let mut lines = BufReader::new(child.stdout.take().context("missing child stdout")?).lines();
    let stderr = child.stderr.take().context("missing child stderr")?;
    let stderr = tokio::spawn(async move {
        let mut text = String::new();
        tokio::io::AsyncReadExt::read_to_string(&mut BufReader::new(stderr), &mut text)
            .await
            .map(|_| text)
    });
    for message in [
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "redaction-test", "version": "1.0" }
            }
        }),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {
                "name": "run_maven",
                "arguments": {
                    "project_path": project.display().to_string(),
                    "arguments": [format!("-Dpassword={MARKER}")]
                }
            }
        }),
    ] {
        stdin.write_all(format!("{message}\n").as_bytes()).await?;
    }
    let response = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let line = lines
                .next_line()
                .await?
                .context("server closed stdout before the tool response")?;
            let message: Value = serde_json::from_str(&line)?;
            if message["id"] == 2 {
                return Ok::<_, anyhow::Error>(message);
            }
        }
    })
    .await
    .context("tool call with Unicode and credential output timed out")??;
    let result = &response["result"]["structuredContent"];
    assert_eq!(result["status"], "success", "{response:#}");
    assert_eq!(
        result["stdout"],
        "İ password=<redacted>\nAuthorization: <redacted>\nroot=<PROJECT_ROOT> token=<redacted>\nsecret=<redacted>\n"
    );
    assert_eq!(result["stderr"], "Authorization: <redacted>\n");
    let cancelled = json!({
        "jsonrpc": "2.0", "method": "notifications/cancelled",
        "params": {
            "requestId": 2,
            "reason": format!("{} password={MARKER}", project.display())
        }
    });
    stdin.write_all(format!("{cancelled}\n").as_bytes()).await?;

    drop(stdin);
    let status = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
        .await
        .context("server did not stop after stdin EOF")??;
    assert!(status.success(), "server exited with {status}");
    let stderr = stderr.await??;
    let response = response.to_string();
    for text in [&stderr, &response] {
        assert!(!text.contains(MARKER), "{text}");
        assert!(!text.contains(project.to_string_lossy().as_ref()), "{text}");
        assert!(!text.contains("panicked"), "{text}");
    }
    assert!(stderr.contains("MCP server ready for request-scoped Maven projects"));
    Ok(())
}
