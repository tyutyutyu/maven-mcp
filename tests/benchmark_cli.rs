mod support;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use support::{BenchmarkProject, TestServer};

#[tokio::test]
async fn benchmark_cli_writes_paired_measurements_and_preserves_failures() -> Result<()> {
    let server = TestServer::start_with_project().await?;
    let project_path = server.project_path().unwrap().display().to_string();
    let directory = tempfile::tempdir()?;
    let spec_path = directory.path().join("benchmark.json");
    let output_path = directory.path().join("results.json");
    std::fs::write(
        &spec_path,
        serde_json::to_vec_pretty(&json!({
            "schema_version": 1,
            "name": "fixture comparison",
            "cases": [
                {
                    "id": "class-search",
                    "description": "Inspect a request-scoped Maven project",
                    "shell": { "command": "printf fixture-benchmark" },
                    "mcp": {
                        "tool": "inspect_maven_project",
                        "arguments": { "project_path": project_path }
                    }
                },
                {
                    "id": "failure-recording",
                    "shell": { "command": "exit 7" },
                    "mcp": { "tool": "unknown_benchmark_tool" }
                }
            ]
        }))?,
    )?;

    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_maven-benchmark"))
        .arg("--spec")
        .arg(&spec_path)
        .arg("--mcp-command")
        .arg(env!("CARGO_BIN_EXE_maven-mcp"))
        .arg("--output")
        .arg(&output_path)
        .arg("--iterations")
        .arg("2")
        .arg("--warmup")
        .arg("1")
        .env(
            "MAVEN_TRUSTED_PROJECT_DIRECTORIES",
            server.trusted_project_directory().unwrap(),
        )
        .output()
        .await?;
    assert!(
        output.status.success(),
        "benchmark CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let report: Value = serde_json::from_slice(
        &std::fs::read(&output_path)
            .with_context(|| format!("missing report {}", output_path.display()))?,
    )?;
    assert_eq!(report["schema_version"], "1.0");
    assert_eq!(report["benchmark"]["iterations"], 2);
    assert_eq!(report["benchmark"]["warmup_iterations"], 1);
    assert_eq!(report["token_measurement"]["kind"], "estimate");
    assert_eq!(report["cases"].as_array().unwrap().len(), 2);

    let successful = &report["cases"][0];
    assert_eq!(successful["id"], "class-search");
    assert_eq!(successful["shell"]["summary"]["successful_runs"], 2);
    assert_eq!(successful["mcp"]["summary"]["successful_runs"], 2);
    assert!(
        successful["shell"]["request"]["estimated_tokens"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(
        successful["shell"]["runs"][0]["request"]["bytes"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(
        successful["mcp"]["runs"][0]["request"]["estimated_tokens"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(
        successful["mcp"]["runs"][0]["response"]["bytes"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(successful["comparison"]["shell_to_mcp_duration_ratio"].is_number());

    let failed = &report["cases"][1];
    assert_eq!(failed["shell"]["runs"][0]["exit_code"], 7);
    assert_eq!(failed["shell"]["runs"][0]["success"], false);
    assert_eq!(failed["mcp"]["runs"][0]["success"], false);
    assert!(failed["mcp"]["runs"][0]["error"].is_string());

    Ok(())
}

#[tokio::test]
async fn benchmark_cli_preserves_shell_results_when_mcp_is_unreachable() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let spec_path = directory.path().join("benchmark.json");
    let output_path = directory.path().join("results.json");
    std::fs::write(
        &spec_path,
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "name": "unavailable MCP",
            "cases": [{
                "id": "still-run-shell",
                "shell": { "command": "printf shell-survives" },
                "mcp": { "tool": "index_stats" }
            }]
        }))?,
    )?;

    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_maven-benchmark"))
        .arg("--spec")
        .arg(&spec_path)
        .arg("--mcp-command")
        .arg(directory.path().join("missing-maven-mcp"))
        .arg("--output")
        .arg(&output_path)
        .arg("--iterations")
        .arg("1")
        .arg("--warmup")
        .arg("0")
        .arg("--timeout-seconds")
        .arg("1")
        .output()
        .await?;
    assert!(output.status.success());

    let report: Value = serde_json::from_slice(&std::fs::read(output_path)?)?;
    assert_eq!(report["cases"][0]["shell"]["runs"][0]["success"], true);
    assert_eq!(report["cases"][0]["mcp"]["runs"][0]["success"], false);
    assert!(
        report["cases"][0]["mcp"]["runs"][0]["error"]
            .as_str()
            .unwrap()
            .contains("cannot start MCP command")
    );
    Ok(())
}

#[tokio::test]
async fn benchmark_cli_bounds_shell_output_and_continues_after_timeout() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let spec_path = directory.path().join("benchmark.json");
    let output_path = directory.path().join("results.json");
    let pid_path = directory.path().join("descendant.pid");
    std::fs::write(
        &spec_path,
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "name": "bounded shell",
            "cases": [
                {
                    "id": "infinite-output",
                    "shell": { "command": "yes" },
                    "mcp": { "tool": "index_stats" }
                },
                {
                    "id": "inherited-pipe",
                    "shell": {
                        "command": format!(
                            "sleep 60 & echo $! > '{}'; echo parent-done",
                            pid_path.display()
                        )
                    },
                    "mcp": { "tool": "index_stats" }
                },
                {
                    "id": "large-finite-output",
                    "shell": { "command": "head -c 3000000 /dev/zero" },
                    "mcp": { "tool": "index_stats" }
                },
                {
                    "id": "next-case",
                    "shell": { "command": "printf next-case" },
                    "mcp": { "tool": "index_stats" }
                }
            ]
        }))?,
    )?;

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_maven-benchmark"))
            .arg("--spec")
            .arg(&spec_path)
            .arg("--mcp-command")
            .arg(directory.path().join("missing-maven-mcp"))
            .arg("--output")
            .arg(&output_path)
            .arg("--iterations")
            .arg("1")
            .arg("--warmup")
            .arg("0")
            .arg("--timeout-seconds")
            .arg("1")
            .arg("--max-output-bytes")
            .arg("4096")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("benchmark CLI did not finish within its budgets")??;
    assert!(
        output.status.success(),
        "benchmark CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let report: Value = serde_json::from_slice(&std::fs::read(&output_path)?)?;
    let runs = |index: usize| &report["cases"][index]["shell"]["runs"][0];

    let infinite = runs(0);
    assert_eq!(infinite["success"], false);
    assert!(infinite["error"].as_str().unwrap().contains("timed out"));
    assert_eq!(infinite["response"]["bytes"], 4096);
    assert_eq!(infinite["output_truncated"], true);

    let inherited = runs(1);
    assert_eq!(inherited["success"], true);
    assert_eq!(inherited["response"]["bytes"], 12);
    assert_eq!(inherited["output_truncated"], false);
    let descendant = std::fs::read_to_string(&pid_path)?.trim().to_owned();
    for _ in 0..50 {
        let state = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &descendant])
            .output()?;
        let state = String::from_utf8_lossy(&state.stdout);
        if state.trim().is_empty() || state.trim().starts_with('Z') {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let state = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &descendant])
        .output()?;
    let state = String::from_utf8_lossy(&state.stdout);
    assert!(
        state.trim().is_empty() || state.trim().starts_with('Z'),
        "descendant {descendant} is still running"
    );

    let large = runs(2);
    assert_eq!(large["success"], true);
    assert_eq!(large["response"]["bytes"], 4096);
    assert_eq!(large["output_truncated"], true);

    let next = runs(3);
    assert_eq!(next["success"], true);
    assert_eq!(next["response"]["bytes"], 9);
    assert_eq!(next["output_truncated"], false);
    assert_eq!(report["cases"].as_array().unwrap().len(), 4);
    Ok(())
}

async fn run_project_benchmark(
    spec_path: &std::path::Path,
    output_path: &std::path::Path,
    project: &BenchmarkProject,
) -> Result<std::process::Output> {
    Ok(tokio::time::timeout(
        std::time::Duration::from_secs(120),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_maven-benchmark"))
            .arg("--spec")
            .arg(spec_path)
            .arg("--mcp-command")
            .arg(env!("CARGO_BIN_EXE_maven-mcp"))
            .arg("--project-path")
            .arg(project.project_path())
            .arg("--output")
            .arg(output_path)
            .arg("--iterations")
            .arg("2")
            .arg("--warmup")
            .arg("1")
            .env_remove("MAVEN_TRUSTED_PROJECT_DIRECTORIES")
            .env("MAVEN_EXECUTION_REPO_PATH", project.repository_path())
            .env("RUST_LOG", "maven_mcp=warn")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("benchmark CLI did not finish")??)
}

#[tokio::test]
async fn bundled_specification_compares_identical_project_scoped_results() -> Result<()> {
    let project = BenchmarkProject::new()?;
    let directory = tempfile::tempdir()?;
    let output_path = directory.path().join("results.json");
    let spec_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("benchmark.json");

    let output = run_project_benchmark(&spec_path, &output_path, &project).await?;
    assert!(
        output.status.success(),
        "benchmark CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let report: Value = serde_json::from_slice(&std::fs::read(&output_path)?)?;
    assert!(
        report["measurement_modes"]["first_call"]
            .as_str()
            .unwrap()
            .contains("Maven classpath resolution")
    );
    let cases = report["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 3);
    for case in cases {
        let id = &case["id"];
        let identity = &case["comparison"]["identity"];
        assert_eq!(identity["status"], "matched", "{id}: {identity}");
        assert!(identity.get("only_in_shell").is_none(), "{id}");
        assert!(identity.get("only_in_mcp").is_none(), "{id}");
        for side in ["shell", "mcp"] {
            assert_eq!(case[side]["first_call"]["iteration"], 0, "{id} {side}");
            assert_eq!(case[side]["first_call"]["success"], true, "{id} {side}");
            assert_eq!(case[side]["summary"]["successful_runs"], 2, "{id} {side}");
        }
        assert!(case["comparison"]["shell_to_mcp_duration_ratio"].is_number());
        assert!(case["comparison"]["first_call_shell_to_mcp_duration_ratio"].is_number());
    }

    // Only the classpath JAR is found: the outside control (slf4j-api 9.9) is in
    // the repository but not in the result set of either side. The unrelated JAR
    // listed last on the classpath (no match, no sources JAR) keeps every shell
    // run successful.
    let classes = &cases[0]["comparison"]["identity"];
    assert_eq!(classes["shell_values"], 1);
    assert_eq!(classes["mcp_values"], 1);
    let versions = &cases[1]["comparison"]["identity"];
    assert_eq!(versions["shell_values"], 1);
    assert_eq!(versions["mcp_values"], 1);
    let source = &cases[2]["comparison"]["identity"];
    assert_eq!(source["shell_values"], source["mcp_values"]);
    assert!(!std::fs::read_to_string(&output_path)?.contains(BenchmarkProject::CONTROL_JAR));
    Ok(())
}

#[tokio::test]
async fn bundled_specification_reports_a_maven_failure_as_a_failed_shell_run() -> Result<()> {
    let project = BenchmarkProject::new()?;
    project.fail_maven()?;
    let directory = tempfile::tempdir()?;
    let output_path = directory.path().join("results.json");
    let spec_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("benchmark.json");

    let output = run_project_benchmark(&spec_path, &output_path, &project).await?;
    assert!(
        output.status.success(),
        "benchmark CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let report: Value = serde_json::from_slice(&std::fs::read(&output_path)?)?;
    let cases = report["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 3);
    for case in cases {
        let id = &case["id"];
        assert_eq!(case["shell"]["first_call"]["success"], false, "{id}");
        assert_eq!(case["shell"]["summary"]["successful_runs"], 0, "{id}");
        let comparison = &case["comparison"];
        assert_eq!(comparison["identity"]["status"], "unavailable", "{id}");
        assert!(comparison["shell_to_mcp_duration_ratio"].is_null(), "{id}");
        assert!(
            comparison["first_call_shell_to_mcp_duration_ratio"].is_null(),
            "{id}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn unscoped_shell_result_is_a_failed_comparison_not_a_speedup() -> Result<()> {
    let project = BenchmarkProject::new()?;
    let directory = tempfile::tempdir()?;
    let spec_path = directory.path().join("benchmark.json");
    let output_path = directory.path().join("results.json");
    std::fs::write(
        &spec_path,
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "name": "unscoped shell",
            "cases": [{
                "id": "whole-repository",
                "shell": {
                    "command": "find \"$MAVEN_EXECUTION_REPO_PATH\" -name 'slf4j-api-*.jar' ! -name '*-sources.jar'"
                },
                "mcp": {
                    "tool": "search_classes",
                    "arguments": { "project_path": "${PROJECT_PATH}", "query": "org.slf4j.Logger" }
                },
                "identity": { "mcp_values": "/results/*/jar/path", "normalize": "file_names" }
            }]
        }))?,
    )?;

    let output = run_project_benchmark(&spec_path, &output_path, &project).await?;
    assert!(
        output.status.success(),
        "benchmark CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let report: Value = serde_json::from_slice(&std::fs::read(&output_path)?)?;
    let case = &report["cases"][0];
    let comparison = &case["comparison"];
    assert_eq!(comparison["identity"]["status"], "mismatched");
    assert_eq!(
        comparison["identity"]["only_in_shell"],
        json!([BenchmarkProject::CONTROL_JAR])
    );
    assert!(comparison["shell_to_mcp_duration_ratio"].is_null());
    assert!(comparison["first_call_shell_to_mcp_duration_ratio"].is_null());
    assert!(comparison["estimated_token_reduction_percent"].is_null());
    assert_eq!(case["shell"]["runs"].as_array().unwrap().len(), 2);
    assert_eq!(case["mcp"]["summary"]["successful_runs"], 2);
    Ok(())
}

#[tokio::test]
async fn specification_with_project_placeholder_requires_project_path() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let output_path = directory.path().join("results.json");
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_maven-benchmark"))
        .arg("--spec")
        .arg(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("benchmark.json"))
        .arg("--output")
        .arg(&output_path)
        .output()
        .await?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--project-path"));
    assert!(!output_path.exists());
    Ok(())
}
