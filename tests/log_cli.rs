use std::process::Command;

use anyhow::Result;
use tempfile::TempDir;

fn run(arguments: &[&str]) -> Result<std::process::Output> {
    Ok(Command::new(env!("CARGO_BIN_EXE_maven-agent-log"))
        .args(arguments)
        .output()?)
}

#[test]
fn cli_emits_deterministic_redacted_reports_in_every_supported_format() -> Result<()> {
    let root = TempDir::new()?;
    let log = root.path().join("session.json");
    std::fs::write(
        &log,
        format!(
            r#"{{"tool":"run_in_terminal","timestamp":"2026-01-01T00:00:00Z","cwd":"{}","command":"./mvnw test -Dtoken=hunter2"}}"#,
            root.path().display()
        ),
    )?;
    let path = log.to_string_lossy();
    for format in ["terminal", "json", "jsonl", "csv", "markdown"] {
        let output = run(&[
            path.as_ref(),
            "--source",
            "vs-code",
            "--format",
            format,
            "--workspace",
            root.path().to_string_lossy().as_ref(),
        ])?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report = String::from_utf8(output.stdout)?;
        assert!(report.contains("mvnw"));
        assert!(report.contains("<REDACTED>"));
        assert!(!report.contains("hunter2"));
        assert!(!report.contains(root.path().to_string_lossy().as_ref()));
    }

    let first = run(&[path.as_ref(), "--source", "vs-code", "--format", "json"])?;
    let second = run(&[path.as_ref(), "--source", "vs-code", "--format", "json"])?;
    assert_eq!(first.stdout, second.stdout);

    let grouped = run(&[
        path.as_ref(),
        "--source",
        "vs-code",
        "--format",
        "markdown",
        "--group-by",
        "category",
    ])?;
    assert!(String::from_utf8(grouped.stdout)?.contains("maven"));
    Ok(())
}
