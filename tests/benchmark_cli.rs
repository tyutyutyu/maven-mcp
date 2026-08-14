mod support;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use support::TestServer;

#[tokio::test]
async fn benchmark_cli_writes_paired_measurements_and_preserves_failures() -> Result<()> {
    let server = TestServer::start().await?;
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
                    "description": "Find an indexed class",
                    "shell": { "command": "printf fixture-benchmark" },
                    "mcp": {
                        "tool": "search_classes",
                        "arguments": { "query": "Foo" }
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
        .env("MAVEN_REPO_PATH", server.repository_path())
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
