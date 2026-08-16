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
    let fixture = TestServer::start().await?;
    let mut child = Command::new(env!("CARGO_BIN_EXE_maven-mcp"))
        .env("MAVEN_REPO_PATH", fixture.repository_path())
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
    assert!(stderr.contains("Maven repository index ready"));
    assert!(!response_line.contains("Maven repository index ready"));
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn sigterm_stops_the_stdio_server_cleanly() -> Result<()> {
    use nix::{
        sys::signal::{Signal, kill},
        unistd::Pid,
    };

    let fixture = TestServer::start().await?;
    let mut child = Command::new(env!("CARGO_BIN_EXE_maven-mcp"))
        .env("MAVEN_REPO_PATH", fixture.repository_path())
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
        json!({ "phase": "compile" })
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
