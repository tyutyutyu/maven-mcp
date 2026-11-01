mod support;

use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
};

use maven_mcp::runtime_stats::HIT_PUBLICATION_DELAY;
use rmcp::model::CallToolRequestParams;
use support::{
    TestServer,
    debug_log::{self, StderrLog},
};

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

/// A host-style STDIO instance that publishes its status into `runtime`.
struct StdioInstance {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
}

impl StdioInstance {
    async fn start(runtime: &Path) -> Result<Self> {
        let mut child = Command::new(env!("CARGO_BIN_EXE_maven-mcp"))
            .env("MAVEN_MCP_INDEX_CACHE", "false")
            .env("MAVEN_MCP_RUNTIME_DIR", runtime)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
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
                "clientInfo": { "name": "stats-test", "version": "1.0" }
            }
        });
        stdin
            .write_all(format!("{initialize}\n").as_bytes())
            .await?;
        tokio::time::timeout(Duration::from_secs(5), lines.next_line())
            .await
            .context("initialize response timed out")??
            .context("server closed before initialize response")?;
        Ok(Self { child, stdin })
    }

    fn pid(&self) -> u32 {
        self.child.id().expect("a running instance has a PID")
    }

    /// Closes stdin like a host that went away and waits for the process to exit.
    async fn stop_with_eof(mut self) -> Result<()> {
        drop(self.stdin);
        let status = tokio::time::timeout(Duration::from_secs(5), self.child.wait())
            .await
            .context("server did not stop after stdin EOF")??;
        assert!(status.success(), "server exited with {status}");
        Ok(())
    }
}

struct StatsOutput {
    stdout: String,
    stderr: String,
}

async fn run_stats(runtime: &Path, json: bool) -> Result<StatsOutput> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_maven-mcp"));
    command
        .env("MAVEN_MCP_INDEX_CACHE", "false")
        .env("MAVEN_MCP_RUNTIME_DIR", runtime)
        .arg("stats");
    if json {
        command.arg("--json");
    }
    let output = command.output().await?;
    let stats = StatsOutput {
        stdout: String::from_utf8(output.stdout)?,
        stderr: String::from_utf8(output.stderr)?,
    };
    assert!(
        output.status.success(),
        "stats CLI failed: {}",
        stats.stderr
    );
    Ok(stats)
}

async fn stats_json(runtime: &Path) -> Result<Value> {
    let output = run_stats(runtime, true).await?;
    assert_eq!(
        output.stderr, "",
        "a successful report writes nothing to stderr"
    );
    Ok(serde_json::from_str(&output.stdout)?)
}

fn reported_pids(report: &Value) -> Vec<u64> {
    report["instances"]
        .as_array()
        .expect("instances is an array")
        .iter()
        .map(|instance| instance["pid"].as_u64().expect("pid is a number"))
        .collect()
}

fn status_file(runtime: &Path, pid: u32) -> PathBuf {
    runtime.join(format!("{pid}.json"))
}

fn read_status(runtime: &Path, pid: u32) -> Result<Value> {
    Ok(serde_json::from_slice(&std::fs::read(status_file(
        runtime, pid,
    ))?)?)
}

fn mode(path: &Path) -> Result<u32> {
    Ok(std::fs::metadata(path)?.permissions().mode() & 0o777)
}

fn reaped_process_id() -> Result<u32> {
    let mut child = std::process::Command::new("true").spawn()?;
    let pid = child.id();
    child.wait()?;
    Ok(pid)
}

async fn wait_until_removed(path: &Path) -> Result<()> {
    for _ in 0..200 {
        if !path.exists() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    anyhow::bail!("{} was not removed", path.display())
}

#[tokio::test]
async fn stats_cli_reports_live_stdio_instance() -> Result<()> {
    let runtime = tempfile::tempdir()?;
    let instance = StdioInstance::start(runtime.path()).await?;

    let report = stats_json(runtime.path()).await?;

    assert_eq!(reported_pids(&report), [u64::from(instance.pid())]);
    let reported = &report["instances"][0];
    assert_eq!(reported["schema_version"], 1);
    assert_eq!(reported["binary_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(
        reported["build_commit"],
        json!(maven_mcp::build_info::build_commit())
    );
    let executable = std::fs::canonicalize(env!("CARGO_BIN_EXE_maven-mcp"))?;
    assert_eq!(
        reported["executable"],
        json!(executable.display().to_string())
    );
    assert!(reported["process_start_ticks"].as_u64().is_some());
    assert!(
        reported["memory"]["rss_bytes"]
            .as_u64()
            .is_some_and(|bytes| bytes > 0)
    );
    assert!(
        reported["config"]["max_project_index_bytes"]
            .as_u64()
            .is_some()
    );
    assert_eq!(reported["projects"], json!([]));
    instance.stop_with_eof().await
}

#[tokio::test]
async fn stats_cli_reports_every_live_instance_and_cleans_only_stale_state() -> Result<()> {
    let runtime = tempfile::tempdir()?;
    let first = StdioInstance::start(runtime.path()).await?;
    let second = StdioInstance::start(runtime.path()).await?;
    let (first_pid, second_pid) = (first.pid(), second.pid());
    // A crashed instance leaves a well-formed status whose process is gone.
    let stale_pid = reaped_process_id()?;
    let mut stale = read_status(runtime.path(), first_pid)?;
    stale["pid"] = json!(stale_pid);
    let stale_file = status_file(runtime.path(), stale_pid);
    std::fs::write(&stale_file, serde_json::to_vec(&stale)?)?;
    let invalid_file = runtime.path().join("invalid.json");
    std::fs::write(&invalid_file, "{ not a status")?;

    let report = stats_json(runtime.path()).await?;

    let mut expected = vec![u64::from(first_pid), u64::from(second_pid)];
    expected.sort_unstable();
    assert_eq!(reported_pids(&report), expected, "sorted by PID");
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["unknown_instances"], 0);
    assert!(
        !stale_file.exists(),
        "the status of a gone process is deleted"
    );
    assert!(
        invalid_file.exists(),
        "an invalid status is skipped, not deleted"
    );
    assert_eq!(mode(runtime.path())?, 0o700);
    for pid in [first_pid, second_pid] {
        assert_eq!(mode(&status_file(runtime.path(), pid))?, 0o600);
    }
    let human = run_stats(runtime.path(), false).await?;
    assert_eq!(human.stderr, "");
    for pid in [first_pid, second_pid] {
        assert!(
            human.stdout.contains(&format!(
                "maven-mcp pid={pid} version={}",
                env!("CARGO_PKG_VERSION")
            )),
            "{}",
            human.stdout
        );
    }

    first.stop_with_eof().await?;
    assert!(
        !status_file(runtime.path(), first_pid).exists(),
        "EOF removes the instance's own status"
    );
    assert_eq!(
        reported_pids(&stats_json(runtime.path()).await?),
        [u64::from(second_pid)]
    );

    second.stop_with_eof().await?;
    assert_eq!(
        run_stats(runtime.path(), false).await?.stdout,
        "No live maven-mcp instances.\n"
    );
    assert_eq!(
        reported_pids(&stats_json(runtime.path()).await?),
        Vec::<u64>::new()
    );
    Ok(())
}

#[tokio::test]
async fn stats_report_index_figures_and_cache_hits_do_not_write_the_status_per_hit() -> Result<()> {
    const HITS: u64 = 25;
    let server = TestServer::start_with_project().await?;
    let project = server
        .project_path()
        .context("missing fixture project")?
        .display()
        .to_string();
    let runtime = tempfile::tempdir()?;
    let runtime_dir = runtime.path().to_str().context("UTF-8 runtime path")?;
    let (client, pid) = server
        .connect_with_limits(&[("MAVEN_MCP_RUNTIME_DIR", runtime_dir)])
        .await?;
    let search = || {
        client.call_tool(
            CallToolRequestParams::new("search_classes").with_arguments(
                json!({ "project_path": project, "query": "Foo" })
                    .as_object()
                    .cloned()
                    .unwrap(),
            ),
        )
    };

    search().await?;
    // A build is a structural change, so it is published before the response.
    let built = read_status(runtime.path(), pid)?;
    assert_eq!(built["cache"]["misses"], 1);
    assert_eq!(built["cache"]["hits"], 0);
    let indexed = &built["projects"][0];
    assert_eq!(indexed["state"], "ready", "{built:#}");
    assert!(
        indexed["classpath_entries"]
            .as_u64()
            .is_some_and(|entries| entries >= 1)
    );
    assert!(indexed["build_duration_ms"].as_u64().is_some());
    assert!(
        indexed["estimated_bytes"]
            .as_u64()
            .is_some_and(|bytes| bytes > 0)
    );
    assert!(
        indexed["index"]["class_count"]
            .as_u64()
            .is_some_and(|count| count > 0)
    );
    assert!(indexed["last_used_unix_ms"].as_u64().is_some());

    let mut publications = std::collections::BTreeSet::new();
    let started = Instant::now();
    for _ in 0..HITS {
        search().await?;
        publications.insert(read_status(runtime.path(), pid)?["updated_unix_ms"].as_u64());
    }
    let elapsed = started.elapsed();
    let allowed = elapsed.as_millis() / HIT_PUBLICATION_DELAY.as_millis() + 2;
    assert!(
        publications.len() as u128 <= allowed,
        "{} status writes for {HITS} hits in {elapsed:?}",
        publications.len()
    );

    // The last hits reach the file within the documented delay, without another request.
    let deadline = Instant::now() + HIT_PUBLICATION_DELAY * 3;
    loop {
        let status = read_status(runtime.path(), pid)?;
        if status["cache"]["hits"] == HITS {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "hits never published: {status:#}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let report = stats_json(runtime.path()).await?;
    assert_eq!(report["instances"][0]["projects"][0]["state"], "ready");
    let human = run_stats(runtime.path(), false).await?.stdout;
    assert!(human.contains(" state=ready classpath="), "{human}");
    assert!(human.contains(&format!("hits={HITS} misses=1")), "{human}");

    client.cancel().await?;
    wait_until_removed(&status_file(runtime.path(), pid)).await
}

#[tokio::test]
async fn stats_report_a_running_index_build_as_building() -> Result<()> {
    let server = TestServer::start_with_project().await?;
    let project = server.project_path().context("missing fixture project")?;
    let blocked = support::install_blocked_classpath(project)?;
    let runtime = tempfile::tempdir()?;
    let runtime_dir = runtime.path().to_str().context("UTF-8 runtime path")?;
    let (client, pid) = server
        .connect_with_limits(&[
            ("MAVEN_MCP_RUNTIME_DIR", runtime_dir),
            ("MAVEN_TIMEOUT_SECONDS", "30"),
        ])
        .await?;
    let arguments = json!({ "project_path": project.display().to_string(), "query": "One" });
    let search = client.call_tool(
        CallToolRequestParams::new("search_classes")
            .with_arguments(arguments.as_object().cloned().context("object arguments")?),
    );

    let observe = async {
        support::wait_for_blocked_fifo_readers(pid, 1).await?;
        let status = read_status(runtime.path(), pid)?;
        blocked.release().await?;
        anyhow::Ok(status)
    };
    let (searched, building) = tokio::join!(search, observe);

    let building = building?;
    assert_eq!(building["projects"][0]["state"], "building", "{building:#}");
    assert_eq!(building["projects"][0]["build_duration_ms"], Value::Null);
    // The released pipe reads as an empty archive, which the build skips.
    searched?;
    let after = read_status(runtime.path(), pid)?;
    assert_eq!(after["projects"][0]["state"], "ready", "{after:#}");
    assert!(after["projects"][0]["build_duration_ms"].as_u64().is_some());
    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn a_cancelled_index_build_stops_reporting_building_once_its_work_stops() -> Result<()> {
    let server = TestServer::start_with_project().await?;
    let project = server.project_path().context("missing fixture project")?;
    let blocked = support::install_blocked_classpath(project)?;
    let runtime = tempfile::tempdir()?;
    let runtime_dir = runtime.path().to_str().context("UTF-8 runtime path")?;
    // Debug stderr shows when the server has handled the cancellation.
    let (client, pid, stderr) = server
        .connect_with_stderr(&[
            ("MAVEN_MCP_RUNTIME_DIR", runtime_dir),
            ("MAVEN_TIMEOUT_SECONDS", "30"),
            ("RUST_LOG", "maven_mcp=debug"),
        ])
        .await?;
    let stderr = StderrLog::capture(stderr);
    let arguments = json!({ "project_path": project.display().to_string(), "query": "One" });
    let handle = client
        .send_cancellable_request(
            rmcp::model::ClientRequest::CallToolRequest(rmcp::model::CallToolRequest::new(
                CallToolRequestParams::new("search_classes")
                    .with_arguments(arguments.as_object().cloned().context("object arguments")?),
            )),
            rmcp::service::PeerRequestOptions::no_options(),
        )
        .await?;
    support::wait_for_blocked_fifo_readers(pid, 1).await?;
    let building = read_status(runtime.path(), pid)?;
    assert_eq!(building["projects"][0]["state"], "building", "{building:#}");

    handle.cancel(Some("regression".to_owned())).await?;
    // Releasing the build before the server handled the cancellation would let it finish
    // and publish a ready index.
    stderr
        .wait_for(|line| {
            debug_log::has_message(line, "tool call finished")
                && debug_log::field(line, "outcome") == Some("cancelled")
        })
        .await?;
    blocked.release().await?;

    // No further project request: the abandoned build's end alone republishes the status.
    let deadline = Instant::now() + Duration::from_secs(10);
    let after = loop {
        let status = read_status(runtime.path(), pid)?;
        if status["projects"][0]["state"] != "building" || Instant::now() >= deadline {
            break status;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(after["projects"][0]["state"], "not_indexed", "{after:#}");
    assert_eq!(after["projects"][0]["build_duration_ms"], Value::Null);
    client.cancel().await?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn sigterm_stops_the_stdio_server_cleanly() -> Result<()> {
    use nix::{
        sys::signal::{Signal, kill},
        unistd::Pid,
    };

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
    assert!(status_file(runtime.path(), process_id).exists());
    kill(Pid::from_raw(i32::try_from(process_id)?), Signal::SIGTERM)?;
    let status = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
        .await
        .context("server did not stop after SIGTERM")??;
    assert!(status.success(), "server exited with {status}");
    assert!(
        !status_file(runtime.path(), process_id).exists(),
        "SIGTERM removes the instance's status although the exit skips destructors"
    );
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

const QUERY_MARKER: &str = "SyntheticQueryMarkerZq";
const CREDENTIAL_MARKER: &str = "synthetic-credential-marker-7f3";
const UNKNOWN_TOOL_MARKER: &str = "synthetic_unknown_tool_marker";
const RELATIVE_PATH_MARKER: &str = "relative-synthetic-path-marker";

/// Wraps the fixture wrapper so a `run_maven` call with a credential-like property also
/// writes credential-like output that must be redacted and never logged.
fn install_credential_echoing_wrapper(project: &Path) -> Result<()> {
    std::fs::rename(project.join("mvnw"), project.join("mvnw-fixture"))?;
    let wrapper = project.join("mvnw");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\ncase \" $* \" in\n  *\" -Dpassword=\"*)\n    printf 'token={CREDENTIAL_MARKER}\\n'\n    printf 'Authorization: Bearer {CREDENTIAL_MARKER}\\n' >&2\n    ;;\nesac\nexec \"$PWD/mvnw-fixture\" \"$@\"\n"
        ),
    )?;
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

fn tool_call(name: &'static str, arguments: Value) -> CallToolRequestParams {
    CallToolRequestParams::new(name).with_arguments(
        arguments
            .as_object()
            .cloned()
            .expect("tool arguments are an object"),
    )
}

/// The first inner event of `call_id` with `message`.
fn inner_event<'a>(lines: &'a [String], call_id: u64, message: &str) -> Option<&'a str> {
    lines.iter().map(String::as_str).find(|line| {
        debug_log::has_message(line, message) && debug_log::span_call_id(line) == Some(call_id)
    })
}

/// Fails when stderr quotes any of `forbidden`.
fn assert_never_logged(lines: &[String], forbidden: &[&str]) {
    let stderr = lines.join("\n");
    for value in forbidden {
        assert!(!value.is_empty());
        assert!(
            !stderr.contains(value),
            "stderr quotes {value:?}:\n{stderr}"
        );
    }
}

#[tokio::test]
async fn debug_log_correlates_tool_calls_cache_and_maven_without_sensitive_data() -> Result<()> {
    let fixture = TestServer::start_with_project().await?;
    let project = fixture
        .project_path()
        .context("missing fixture project")?
        .to_owned();
    let trusted = fixture
        .trusted_project_directory()
        .context("missing trusted directory")?
        .to_owned();
    install_credential_echoing_wrapper(&project)?;
    let (client, _, stderr) = fixture
        .connect_with_stderr(&[("RUST_LOG", "maven_mcp=debug")])
        .await?;
    let stderr = StderrLog::capture(stderr);
    let project_path = project.display().to_string();

    // First and repeated index requests for the same project: miss and build, then hit.
    client
        .call_tool(tool_call(
            "search_classes",
            json!({ "project_path": project_path, "query": QUERY_MARKER }),
        ))
        .await?;
    client
        .call_tool(tool_call(
            "search_classes",
            json!({ "project_path": project_path, "query": "Foo" }),
        ))
        .await?;
    let (searched, source) = tokio::join!(
        client.call_tool(tool_call(
            "search_classes",
            json!({ "project_path": project_path, "query": "Foo" }),
        )),
        client.call_tool(tool_call(
            "get_class_source",
            json!({ "project_path": project_path, "class_name": "org.example.Foo" }),
        )),
    );
    searched?;
    let source = serde_json::to_string(&source?)?;
    assert!(source.contains("int scoped = 2"), "{source}");
    let maven = client
        .call_tool(tool_call(
            "run_maven",
            json!({
                "project_path": project_path,
                "arguments": [format!("-Dpassword={CREDENTIAL_MARKER}")]
            }),
        ))
        .await?;
    assert_eq!(
        maven
            .structured_content
            .as_ref()
            .map(|result| &result["status"]),
        Some(&json!("success"))
    );
    assert!(
        client
            .call_tool(tool_call(UNKNOWN_TOOL_MARKER, json!({})))
            .await
            .is_err()
    );
    assert!(
        client
            .call_tool(tool_call(
                "search_classes",
                json!({ "project_path": RELATIVE_PATH_MARKER, "query": QUERY_MARKER }),
            ))
            .await
            .is_err()
    );
    client.cancel().await?;
    let lines = stderr.finish().await?;
    let log = lines.join("\n");

    let calls = debug_log::correlated_calls(&lines)?;
    let summary = calls
        .iter()
        .map(|call| (call.tool.as_str(), call.outcome.as_deref().unwrap_or("")))
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 7, "{log}");
    assert_eq!(summary[0], ("search_classes", "success"), "{log}");
    assert_eq!(summary[1], ("search_classes", "success"), "{log}");
    let mut parallel = vec![summary[2], summary[3]];
    parallel.sort_unstable();
    assert_eq!(
        parallel,
        [
            ("get_class_source", "success"),
            ("search_classes", "success")
        ],
        "{log}"
    );
    assert_eq!(summary[4], ("run_maven", "success"), "{log}");
    assert_eq!(summary[5], ("<unknown>", "unknown_tool"), "{log}");
    assert_eq!(summary[6], ("search_classes", "invalid_params"), "{log}");

    let (first, repeated, run_maven) = (calls[0].call_id, calls[1].call_id, calls[4].call_id);
    assert!(
        inner_event(&lines, first, "project runner cache miss").is_some(),
        "{log}"
    );
    assert!(
        inner_event(&lines, first, "project index cache miss").is_some(),
        "{log}"
    );
    assert!(
        inner_event(&lines, first, "project index build started").is_some(),
        "{log}"
    );
    let built = inner_event(&lines, first, "project index build finished").context("build")?;
    assert_eq!(debug_log::field(built, "result"), Some("retained"), "{log}");
    assert!(debug_log::field(built, "class_count").is_some(), "{log}");
    assert!(
        debug_log::field(built, "classpath_entries").is_some(),
        "{log}"
    );
    assert!(
        inner_event(&lines, repeated, "project runner cache hit").is_some(),
        "{log}"
    );
    assert!(
        inner_event(&lines, repeated, "project index cache hit").is_some(),
        "{log}"
    );
    assert!(
        inner_event(&lines, repeated, "project index build started").is_none(),
        "{log}"
    );
    assert!(log.contains("runtime status published"), "{log}");

    let classpath = inner_event(&lines, first, "maven run finished").context("classpath run")?;
    assert_eq!(debug_log::field(classpath, "operation"), Some("classpath"));
    assert_eq!(debug_log::field(classpath, "status"), Some("success"));
    assert_eq!(debug_log::field(classpath, "exit_code"), Some("0"));
    assert_eq!(debug_log::field(classpath, "timeout_ms"), Some("2000"));
    for name in [
        "wait_ms",
        "run_ms",
        "duration_ms",
        "timed_out",
        "redaction_count",
    ] {
        assert!(debug_log::field(classpath, name).is_some(), "{name}: {log}");
    }
    assert_eq!(
        debug_log::field(classpath, "stdout_truncated"),
        Some("false")
    );
    assert_eq!(
        debug_log::field(classpath, "stderr_truncated"),
        Some("false")
    );
    assert!(
        inner_event(&lines, run_maven, "maven run started").is_some(),
        "{log}"
    );
    let arguments = inner_event(&lines, run_maven, "maven run finished").context("run_maven")?;
    assert_eq!(debug_log::field(arguments, "operation"), Some("arguments"));
    assert_eq!(debug_log::field(arguments, "exit_code"), Some("0"));
    assert!(
        debug_log::field(arguments, "redaction_count")
            .and_then(|count| count.parse::<u64>().ok())
            .is_some_and(|count| count >= 2),
        "{log}"
    );

    assert_never_logged(
        &lines,
        &[
            &project_path,
            &trusted.display().to_string(),
            QUERY_MARKER,
            CREDENTIAL_MARKER,
            UNKNOWN_TOOL_MARKER,
            RELATIVE_PATH_MARKER,
            "-Dpassword",
            "dependency:build-classpath",
            "Dependencies classpath",
            "BUILD SUCCESS",
            "org.example",
            "org/example",
            "ScopedOnly",
            "int scoped",
            "helper-2.0",
            "org.libs",
        ],
    );
    Ok(())
}

#[tokio::test]
async fn debug_log_records_a_cancelled_tool_call_with_one_cancelled_finish() -> Result<()> {
    let fixture = TestServer::start_with_project().await?;
    let project = fixture.project_path().context("missing fixture project")?;
    let blocked = support::install_blocked_classpath(project)?;
    let (client, pid, stderr) = fixture
        .connect_with_stderr(&[
            ("RUST_LOG", "maven_mcp=debug"),
            ("MAVEN_TIMEOUT_SECONDS", "30"),
        ])
        .await?;
    let stderr = StderrLog::capture(stderr);
    let arguments = json!({ "project_path": project.display().to_string(), "query": QUERY_MARKER });
    let handle = client
        .send_cancellable_request(
            rmcp::model::ClientRequest::CallToolRequest(rmcp::model::CallToolRequest::new(
                tool_call("search_classes", arguments),
            )),
            rmcp::service::PeerRequestOptions::no_options(),
        )
        .await?;
    support::wait_for_blocked_fifo_readers(pid, 1).await?;
    handle
        .cancel(Some(format!("{} {CREDENTIAL_MARKER}", project.display())))
        .await?;
    stderr
        .wait_for(|line| {
            debug_log::has_message(line, "tool call finished")
                && debug_log::field(line, "outcome") == Some("cancelled")
        })
        .await?;
    blocked.release().await?;
    client.cancel().await?;
    let lines = stderr.finish().await?;

    let calls = debug_log::correlated_calls(&lines)?;
    assert_eq!(calls.len(), 1, "{}", lines.join("\n"));
    assert_eq!(calls[0].tool, "search_classes");
    assert_eq!(calls[0].outcome.as_deref(), Some("cancelled"));
    let classpath = inner_event(&lines, calls[0].call_id, "maven run finished")
        .context("the cancelled call ran the classpath goal first")?;
    assert_eq!(debug_log::field(classpath, "operation"), Some("classpath"));
    assert_never_logged(
        &lines,
        &[
            &project.display().to_string(),
            QUERY_MARKER,
            CREDENTIAL_MARKER,
            "blocked-1.0",
            "alpha-1.0",
        ],
    );
    Ok(())
}

/// A raw STDIO session: initialize, then one unknown-tool call, then EOF or SIGTERM. Returns
/// the exit status, every stdout line and the complete stderr.
async fn raw_unknown_tool_session(
    rust_log: Option<&str>,
    stop_with_sigterm: bool,
) -> Result<(std::process::ExitStatus, Vec<String>, String)> {
    let runtime = tempfile::tempdir()?;
    let mut command = Command::new(env!("CARGO_BIN_EXE_maven-mcp"));
    command
        .env("MAVEN_MCP_INDEX_CACHE", "false")
        .env("MAVEN_MCP_RUNTIME_DIR", runtime.path())
        .env_remove("RUST_LOG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(filter) = rust_log {
        command.env("RUST_LOG", filter);
    }
    let mut child = command.spawn()?;
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
                "clientInfo": { "name": "debug-log-test", "version": "1.0" }
            }
        }),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": { "name": UNKNOWN_TOOL_MARKER, "arguments": { "query": QUERY_MARKER } }
        }),
    ] {
        stdin.write_all(format!("{message}\n").as_bytes()).await?;
    }
    let mut stdout = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let line = lines
                .next_line()
                .await?
                .context("server closed stdout before the tool response")?;
            let message: Value = serde_json::from_str(&line)
                .with_context(|| format!("non-JSON content on server stdout: {line}"))?;
            stdout.push(line);
            if message["id"] == 2 {
                anyhow::ensure!(message.get("error").is_some(), "{message}");
                return anyhow::Ok(());
            }
        }
    })
    .await
    .context("unknown tool response timed out")??;
    if stop_with_sigterm {
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(i32::try_from(child.id().context("child pid")?)?),
            nix::sys::signal::Signal::SIGTERM,
        )?;
    } else {
        drop(stdin);
    }
    let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .context("server did not stop")??;
    while let Some(line) = lines.next_line().await? {
        stdout.push(line);
    }
    Ok((status, stdout, stderr.await??))
}

fn assert_json_rpc_only(stdout: &[String]) -> Result<()> {
    for line in stdout {
        let message: Value = serde_json::from_str(line)
            .with_context(|| format!("non-JSON content on server stdout: {line}"))?;
        assert_eq!(message["jsonrpc"], "2.0", "invalid JSON-RPC output: {line}");
    }
    Ok(())
}

#[tokio::test]
async fn debug_logging_keeps_stdout_json_rpc_only_through_eof_and_sigterm() -> Result<()> {
    for (stop_with_sigterm, reason) in [(false, "closed"), (true, "signal")] {
        let (status, stdout, stderr) =
            raw_unknown_tool_session(Some("maven_mcp=debug"), stop_with_sigterm).await?;
        assert!(status.success(), "sigterm={stop_with_sigterm}: {status}");
        assert_json_rpc_only(&stdout)?;
        let lines = stderr.lines().map(str::to_owned).collect::<Vec<_>>();
        assert!(stderr.contains("MCP server ready for request-scoped Maven projects"));
        let summary = lines
            .iter()
            .find(|line| debug_log::has_message(line, "configuration summary"))
            .context("configuration summary")?;
        assert_eq!(debug_log::field(summary, "trusted_directories"), Some("0"));
        assert!(stderr.contains("STDIO service initialized"), "{stderr}");
        assert!(
            lines.iter().any(|line| {
                debug_log::has_message(line, "STDIO connection closed")
                    && debug_log::field(line, "reason") == Some(reason)
            }),
            "{stderr}"
        );
        let calls = debug_log::correlated_calls(&lines)?;
        assert_eq!(calls.len(), 1, "{stderr}");
        assert_eq!(calls[0].tool, "<unknown>");
        assert_eq!(calls[0].outcome.as_deref(), Some("unknown_tool"));
        assert_never_logged(&lines, &[UNKNOWN_TOOL_MARKER, QUERY_MARKER]);
        assert!(
            !stderr.contains('\u{1b}'),
            "stderr is not a terminal: {stderr}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn default_and_invalid_log_filters_keep_info_without_per_call_events() -> Result<()> {
    for filter in [None, Some("maven_mcp=info"), Some("maven_mcp=notalevel")] {
        let (status, stdout, stderr) = raw_unknown_tool_session(filter, false).await?;
        assert!(status.success(), "{filter:?}: {status}");
        assert_json_rpc_only(&stdout)?;
        assert!(
            stderr.contains("MCP server ready for request-scoped Maven projects"),
            "{filter:?}: {stderr}"
        );
        assert!(stderr.contains("MCP server ready"), "{filter:?}: {stderr}");
        for debug_only in [
            "tool call started",
            "tool call finished",
            "configuration summary",
            "STDIO connection closed",
            " DEBUG ",
        ] {
            assert!(!stderr.contains(debug_only), "{filter:?}: {stderr}");
        }
        assert!(
            !stderr.contains(UNKNOWN_TOOL_MARKER),
            "{filter:?}: {stderr}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn debug_log_counts_project_index_evictions() -> Result<()> {
    let fixture = TestServer::start_with_project().await?;
    let first = fixture
        .project_path()
        .context("missing fixture project")?
        .to_owned();
    let second = fixture.add_trusted_project("eviction-second")?;
    // Both classpaths must lie in the configured execution repository.
    support::install_controlled_wrapper(
        &second,
        &[first.join("execution-repository/org/libs/helper/2.0/helper-2.0.jar")],
    )?;
    let (client, _, stderr) = fixture
        .connect_with_stderr(&[
            ("RUST_LOG", "maven_mcp=debug"),
            ("MAX_PROJECT_INDEXES", "1"),
        ])
        .await?;
    let stderr = StderrLog::capture(stderr);
    for project in [&first, &second, &first] {
        client
            .call_tool(tool_call(
                "search_classes",
                json!({ "project_path": project.display().to_string(), "query": "Foo" }),
            ))
            .await?;
    }
    client.cancel().await?;
    let lines = stderr.finish().await?;
    let log = lines.join("\n");

    let calls = debug_log::correlated_calls(&lines)?;
    assert_eq!(calls.len(), 3, "{log}");
    assert!(inner_event(&lines, calls[0].call_id, "project index evicted").is_none());
    let evictions = [1, 2].map(|call| {
        inner_event(&lines, calls[call].call_id, "project index evicted")
            .and_then(|line| debug_log::field(line, "evictions"))
            .map(str::to_owned)
    });
    assert_eq!(
        evictions,
        [Some("1".to_owned()), Some("2".to_owned())],
        "{log}"
    );
    // The repeated first project was evicted, so it misses and builds again.
    assert!(inner_event(&lines, calls[2].call_id, "project index cache miss").is_some());
    assert!(
        lines.iter().any(|line| {
            debug_log::has_message(line, "runtime status published")
                && debug_log::field(line, "evictions") == Some("2")
        }),
        "{log}"
    );
    assert_never_logged(
        &lines,
        &[
            &first.display().to_string(),
            &second.display().to_string(),
            "eviction-second",
            "helper-2.0",
        ],
    );
    Ok(())
}

#[tokio::test]
async fn skipped_archive_warnings_carry_a_reason_but_no_archive_or_path() -> Result<()> {
    let fixture = TestServer::start_with_project().await?;
    let project = fixture.project_path().context("missing fixture project")?;
    let blocked = support::install_blocked_classpath(project)?;
    let (client, pid, stderr) = fixture
        .connect_with_stderr(&[
            ("RUST_LOG", "maven_mcp=debug"),
            ("MAVEN_TIMEOUT_SECONDS", "30"),
        ])
        .await?;
    let stderr = StderrLog::capture(stderr);
    let search = client.call_tool(tool_call(
        "search_classes",
        json!({ "project_path": project.display().to_string(), "query": "One" }),
    ));
    let release = async {
        support::wait_for_blocked_fifo_readers(pid, 1).await?;
        blocked.release().await
    };
    let (searched, released) = tokio::join!(search, release);
    released?;
    // The released pipe reads as an empty archive, which the build skips with a warning.
    searched?;
    client.cancel().await?;
    let lines = stderr.finish().await?;
    let log = lines.join("\n");

    let calls = debug_log::correlated_calls(&lines)?;
    assert_eq!(calls.len(), 1, "{log}");
    let warning = lines
        .iter()
        .find(|line| line.contains(" WARN ") && debug_log::field(line, "reason").is_some())
        .context("skipped archive warning")?;
    assert_eq!(
        debug_log::span_call_id(warning),
        Some(calls[0].call_id),
        "{log}"
    );
    assert!(debug_log::field(warning, "error_kind").is_some(), "{log}");
    assert_never_logged(
        &lines,
        &[
            &project.display().to_string(),
            "blocked-1.0",
            "alpha-1.0",
            "org.t",
            "org/t",
            "AlphaOne",
        ],
    );
    Ok(())
}
