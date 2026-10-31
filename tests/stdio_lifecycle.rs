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
    let (client, pid) = server
        .connect_with_limits(&[
            ("MAVEN_MCP_RUNTIME_DIR", runtime_dir),
            ("MAVEN_TIMEOUT_SECONDS", "30"),
        ])
        .await?;
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
