use super::*;

#[test]
fn token_estimate_uses_unicode_scalar_values() {
    assert_eq!(payload_size("árvíz".as_bytes()).bytes, 7);
    assert_eq!(payload_size("árvíz".as_bytes()).estimated_tokens, 2);
}

#[test]
fn rejects_duplicate_case_ids() {
    let error = BenchmarkSpec::from_slice(
        br#"{"schema_version":1,"name":"x","cases":[{"id":"same","shell":{"command":"true"},"mcp":{"tool":"index_stats"}},{"id":"same","shell":{"command":"true"},"mcp":{"tool":"index_stats"}}]}"#,
    )
    .unwrap_err();
    assert!(error.to_string().contains("duplicate benchmark case id"));
}

#[test]
fn summary_uses_only_successful_runs() {
    let request = PayloadSize {
        bytes: 4,
        estimated_tokens: 1,
    };
    let runs = vec![
        RunMeasurement {
            iteration: 1,
            duration_nanoseconds: 10,
            success: true,
            request,
            response: request,
            exit_code: Some(0),
            error: None,
            output_truncated: false,
        },
        failed_run(2, 1_000, request, "failed".to_owned()),
    ];
    let summary = summarize(&runs, request);
    assert_eq!(summary.successful_runs, 1);
    assert_eq!(summary.mean_duration_nanoseconds, Some(10));
    assert_eq!(summary.mean_total_estimated_tokens, Some(2));
}

fn shell_options(timeout: Duration, max_output_bytes: usize) -> BenchmarkOptions {
    BenchmarkOptions {
        mcp_command: PathBuf::from("unused"),
        iterations: 1,
        warmup: 0,
        timeout,
        max_output_bytes,
    }
}

fn shell(command: impl Into<String>) -> ShellInvocation {
    ShellInvocation {
        command: command.into(),
        cwd: None,
    }
}

fn process_is_running(process_id: &str) -> bool {
    let output = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", process_id])
        .output()
        .expect("ps must run");
    let state = String::from_utf8_lossy(&output.stdout);
    let state = state.trim();
    !state.is_empty() && !state.starts_with('Z')
}

async fn assert_descendant_stopped(pid_file: &std::path::Path) {
    let process_id = std::fs::read_to_string(pid_file)
        .expect("descendant pid file")
        .trim()
        .to_owned();
    for _ in 0..50 {
        if !process_is_running(&process_id) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("descendant process {process_id} is still running");
}

#[tokio::test]
async fn shell_output_is_bounded_without_failing_the_run() {
    let options = shell_options(Duration::from_secs(10), 1024);
    let run = run_shell(&shell("head -c 5000000 /dev/zero"), &options, 1).await;
    assert!(run.success);
    assert_eq!(run.exit_code, Some(0));
    assert_eq!(run.response.bytes, 1024);
    assert!(run.output_truncated);
    assert!(run.error.is_none());
}

#[tokio::test]
async fn shell_stdout_and_stderr_share_one_budget() {
    let options = shell_options(Duration::from_secs(10), 100);
    let run = run_shell(
        &shell("head -c 100000 /dev/zero; head -c 100000 /dev/zero >&2"),
        &options,
        1,
    )
    .await;
    assert!(run.success);
    assert_eq!(run.response.bytes, 100);
    assert!(run.output_truncated);
}

#[tokio::test]
async fn shell_output_within_budget_is_complete_and_not_truncated() {
    let options = shell_options(Duration::from_secs(10), 1024);
    let run = run_shell(&shell("printf out; printf err >&2"), &options, 1).await;
    assert!(run.success);
    assert_eq!(run.response.bytes, 6);
    assert!(!run.output_truncated);
}

#[tokio::test]
async fn infinite_shell_output_times_out_with_bounded_storage() {
    let options = shell_options(Duration::from_millis(300), 4096);
    let started = std::time::Instant::now();
    let run = run_shell(&shell("yes"), &options, 1).await;
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(!run.success);
    assert_eq!(run.exit_code, None);
    assert!(run.error.unwrap().contains("timed out"));
    assert_eq!(run.response.bytes, 4096);
    assert!(run.output_truncated);
}

#[tokio::test]
async fn early_exit_with_descendant_holding_pipe_does_not_hang_or_orphan() {
    let directory = tempfile::tempdir().unwrap();
    let pid_file = directory.path().join("descendant.pid");
    let command = format!("sleep 30 & echo $! > '{}'; echo done", pid_file.display());
    let options = shell_options(Duration::from_secs(20), 1024);
    let started = std::time::Instant::now();
    let run = run_shell(&shell(command), &options, 1).await;
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(run.success, "{:?}", run.error);
    assert_eq!(run.response.bytes, 5);
    assert_descendant_stopped(&pid_file).await;
}

struct KillOnDrop(std::path::PathBuf);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Ok(process_id) = std::fs::read_to_string(&self.0) {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", process_id.trim()])
                .status();
        }
    }
}

#[tokio::test]
async fn supervision_failure_preserves_bounded_output_and_truncation() {
    let directory = tempfile::tempdir().unwrap();
    let pid_file = directory.path().join("escaped.pid");
    let _escaped = KillOnDrop(pid_file.clone());
    // The escaped descendant records its PID only after leaving the process
    // group; the shell waits for it so group termination cannot win the race.
    let command = format!(
        "head -c 5000 /dev/zero; setsid sh -c 'echo $$ > \"$0\"; exec sleep 30' '{0}' & \
         while [ ! -s '{0}' ]; do sleep 0.01; done",
        pid_file.display()
    );
    let options = shell_options(Duration::from_secs(20), 1024);
    let started = std::time::Instant::now();
    let run = run_shell(&shell(command), &options, 1).await;
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(!run.success);
    assert_eq!(run.exit_code, None);
    assert!(run.error.unwrap().contains("pipes stayed open"));
    assert_eq!(run.response.bytes, 1024);
    assert!(run.output_truncated);
}

#[tokio::test]
async fn timeout_terminates_descendants_of_the_shell() {
    let directory = tempfile::tempdir().unwrap();
    let pid_file = directory.path().join("descendant.pid");
    let command = format!("sleep 30 & echo $! > '{}'; wait", pid_file.display());
    let options = shell_options(Duration::from_millis(500), 1024);
    let started = std::time::Instant::now();
    let run = run_shell(&shell(command), &options, 1).await;
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(!run.success);
    assert!(run.error.unwrap().contains("timed out"));
    assert_descendant_stopped(&pid_file).await;
}

#[tokio::test]
async fn next_shell_run_proceeds_after_a_timed_out_run() {
    let options = shell_options(Duration::from_millis(300), 1024);
    let timed_out = run_shell(&shell("yes"), &options, 1).await;
    let next = run_shell(&shell("printf next"), &options, 2).await;
    assert!(!timed_out.success);
    assert!(next.success);
    assert_eq!(next.iteration, 2);
    assert_eq!(next.response.bytes, 4);
}

#[tokio::test]
async fn shell_spawn_failure_is_reported_as_a_failed_run() {
    let invocation = ShellInvocation {
        command: "true".to_owned(),
        cwd: Some(PathBuf::from("/nonexistent/benchmark-cwd")),
    };
    let run = run_shell(&invocation, &shell_options(Duration::from_secs(5), 64), 1).await;
    assert!(!run.success);
    assert!(run.error.unwrap().contains("shell execution failed"));
}
