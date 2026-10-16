mod support;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use support::TestServer;

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
