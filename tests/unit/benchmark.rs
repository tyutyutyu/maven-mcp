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
        None,
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
        project_path: None,
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

fn project_directory() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("pom.xml"), "<project/>").unwrap();
    directory
}

#[test]
fn bundled_specification_binds_the_project_in_every_case() {
    let project = project_directory();
    let project_path = canonical_project_path(project.path()).unwrap();
    let spec =
        BenchmarkSpec::from_slice(include_bytes!("../../benchmark.json"), Some(&project_path))
            .unwrap();
    assert!(!spec.cases.is_empty());
    for case in &spec.cases {
        assert_eq!(
            case.mcp.arguments["project_path"],
            project_path.to_str().unwrap(),
            "case {}",
            case.id
        );
        assert_eq!(case.shell.cwd.as_deref(), Some(project_path.as_path()));
        assert!(case.shell.command.contains(PROJECT_PATH_VARIABLE));
        assert!(case.identity.is_some(), "case {} has no identity", case.id);
    }
}

#[test]
fn bundled_specification_requires_a_project_path() {
    let error =
        BenchmarkSpec::from_slice(include_bytes!("../../benchmark.json"), None).unwrap_err();
    assert!(error.to_string().contains("--project-path"));
}

#[test]
fn bound_project_requires_matching_project_path_argument() {
    let project = project_directory();
    let project_path = canonical_project_path(project.path()).unwrap();
    for arguments in [r#"{}"#, r#"{"project_path":"/somewhere/else"}"#] {
        let spec = format!(
            r#"{{"schema_version":1,"name":"x","cases":[{{"id":"a","shell":{{"command":"true"}},"mcp":{{"tool":"index_stats","arguments":{arguments}}}}}]}}"#
        );
        let error = BenchmarkSpec::from_slice(spec.as_bytes(), Some(&project_path)).unwrap_err();
        assert!(
            error.to_string().contains("must set project_path"),
            "{arguments}: {error}"
        );
    }
}

#[test]
fn project_path_must_be_an_absolute_maven_project() {
    assert!(canonical_project_path(Path::new("relative")).is_err());
    let empty = tempfile::tempdir().unwrap();
    assert!(
        canonical_project_path(empty.path())
            .unwrap_err()
            .to_string()
            .contains("pom.xml")
    );
    let project = project_directory();
    let link = empty.path().join("link");
    std::os::unix::fs::symlink(project.path(), &link).unwrap();
    assert_eq!(
        canonical_project_path(&link).unwrap(),
        project.path().canonicalize().unwrap()
    );
}

#[tokio::test]
async fn shell_receives_the_project_path_and_returns_stdout_only() {
    let project = project_directory();
    let project_path = canonical_project_path(project.path()).unwrap();
    let mut options = shell_options(Duration::from_secs(5), 4096);
    options.project_path = Some(project_path.clone());
    let (run, stdout) = run_shell_captured(
        &shell("printf '%s' \"$PROJECT_PATH\"; printf noise >&2"),
        &options,
        0,
    )
    .await;
    assert!(run.success);
    assert_eq!(stdout, project_path.to_str().unwrap().as_bytes());
    assert!(run.response.bytes > stdout.len());
}

fn identity(pointer: &str, normalize: IdentityNormalization) -> ResultIdentity {
    ResultIdentity {
        mcp_values: pointer.to_owned(),
        normalize,
    }
}

fn successful_run() -> RunMeasurement {
    let size = payload_size(b"x");
    RunMeasurement {
        iteration: 0,
        duration_nanoseconds: 1,
        success: true,
        request: size,
        response: size,
        exit_code: Some(0),
        error: None,
        output_truncated: false,
    }
}

fn jar_result(paths: &[&str]) -> Value {
    json!({ "structuredContent": { "results": paths.iter().map(|path| json!({"jar": {"path": path}})).collect::<Vec<_>>() } })
}

#[test]
fn identity_matches_after_normalizing_paths_and_order() {
    let check = check_identity(
        Some(&identity(
            "/results/*/jar/path",
            IdentityNormalization::FileNames,
        )),
        &successful_run(),
        b"/home/u/.m2/repository/org/a/1/a-1.jar\n/home/u/.m2/repository/org/b/1/b-1.jar\n",
        &successful_run(),
        Some(&jar_result(&["org/b/1/b-1.jar", "org/a/1/a-1.jar"])),
    );
    assert_eq!(check.status, IdentityStatus::Matched);
    assert_eq!((check.shell_values, check.mcp_values), (2, 2));
    assert!(check.permits_ratios());
}

#[test]
fn identity_reports_an_outside_control_jar_as_a_mismatch() {
    let check = check_identity(
        Some(&identity(
            "/results/*/jar/path",
            IdentityNormalization::FileNames,
        )),
        &successful_run(),
        b"/r/a/1/a-1.jar\n/r/a/9/a-control.jar\n",
        &successful_run(),
        Some(&jar_result(&["a/1/a-1.jar"])),
    );
    assert_eq!(check.status, IdentityStatus::Mismatched);
    assert_eq!(check.only_in_shell, ["a-control.jar"]);
    assert!(check.only_in_mcp.is_empty());
    assert!(!check.permits_ratios());
}

#[test]
fn identity_is_unavailable_when_a_side_cannot_be_compared() {
    let pointer = identity("/results/*/jar/path", IdentityNormalization::Lines);
    let result = jar_result(&["a.jar"]);
    let failed = failed_run(0, 1, payload_size(b"x"), "failed".to_owned());
    let truncated = RunMeasurement {
        output_truncated: true,
        ..successful_run()
    };
    let cases = [
        (
            &failed,
            &successful_run(),
            Some(&result),
            "shell side failed",
        ),
        (&successful_run(), &failed, None, "MCP side failed"),
        (
            &successful_run(),
            &successful_run(),
            None,
            "MCP side failed",
        ),
        (
            &truncated,
            &successful_run(),
            Some(&result),
            "--max-output-bytes",
        ),
    ];
    for (shell_run, mcp_run, mcp_result, expected) in cases {
        let check = check_identity(Some(&pointer), shell_run, b"a.jar", mcp_run, mcp_result);
        assert_eq!(check.status, IdentityStatus::Unavailable);
        assert!(check.detail.unwrap().contains(expected), "{expected}");
    }

    let empty = check_identity(
        Some(&pointer),
        &successful_run(),
        b"\n",
        &successful_run(),
        Some(&jar_result(&[])),
    );
    assert_eq!(empty.status, IdentityStatus::Unavailable);
    assert!(empty.detail.unwrap().contains("empty"));

    let missing = check_identity(
        Some(&identity("/results/*/nope", IdentityNormalization::Lines)),
        &successful_run(),
        b"a.jar",
        &successful_run(),
        Some(&result),
    );
    assert_eq!(missing.status, IdentityStatus::Unavailable);
    assert!(format!("{missing:?}").contains("nope"));
}

#[test]
fn identity_is_optional_and_does_not_block_ratios() {
    let check = check_identity(None, &successful_run(), b"", &successful_run(), None);
    assert_eq!(check.status, IdentityStatus::NotChecked);
    assert!(check.permits_ratios());
}

#[test]
fn mcp_values_accept_object_values_and_text_content() {
    let versions = json!({ "content": [{ "type": "text", "text": "{\"g:a\":[\"1\",\"2\"]}" }] });
    assert_eq!(mcp_result_values(&versions, "/*/*").unwrap(), ["1", "2"]);
    assert!(mcp_result_values(&json!({ "content": [] }), "/*").is_err());
    assert!(mcp_result_values(&json!({ "structuredContent": 1 }), "/*").is_err());
}

#[test]
fn mismatched_comparison_reports_no_speedup() {
    let summary = MeasurementSummary {
        runs: 1,
        successful_runs: 1,
        mean_duration_nanoseconds: Some(100),
        median_duration_nanoseconds: Some(100),
        mean_total_estimated_tokens: Some(10),
    };
    let run = successful_run();
    let mismatch = IdentityCheck::with_status(IdentityStatus::Mismatched);
    let comparison = compare(&summary, &summary, (&run, &run), mismatch);
    assert!(comparison.shell_to_mcp_duration_ratio.is_none());
    assert!(comparison.first_call_shell_to_mcp_duration_ratio.is_none());
    assert!(comparison.estimated_token_reduction_percent.is_none());

    let matched = IdentityCheck::with_status(IdentityStatus::Matched);
    let comparison = compare(&summary, &summary, (&run, &run), matched);
    assert_eq!(comparison.shell_to_mcp_duration_ratio, Some(1.0));
    assert_eq!(comparison.first_call_shell_to_mcp_duration_ratio, Some(1.0));
}
