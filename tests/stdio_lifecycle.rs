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
        .arg("stats")
        .arg("--json")
        .env("MAVEN_MCP_RUNTIME_DIR", runtime.path())
        .output()
        .await?;
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(report["instances"].as_array().unwrap().len(), 1);
    assert_eq!(report["instances"][0]["pid"], child.id().unwrap());

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
    let request = CallToolRequestParams::new("run_maven_lifecycle").with_arguments(
        json!({
            "project_path": project.display().to_string(),
            "phase": "compile"
        })
        .as_object()
        .context("lifecycle arguments must be an object")?
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
