use anyhow::Result;
use serde_yaml_ng::Value;

#[test]
fn promptfoo_agent_eval_is_versioned_and_covers_required_request_classes() -> Result<()> {
    let yaml = std::fs::read_to_string("tests/promptfoo/promptfooconfig.yaml")?;
    let config: Value = serde_yaml_ng::from_str(&yaml)?;
    let tests = config["tests"]
        .as_sequence()
        .expect("Promptfoo tests must be a sequence");
    assert_eq!(tests.len(), 4);
    let descriptions = tests
        .iter()
        .filter_map(|test| test["description"].as_str())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(descriptions.contains("positive"));
    assert!(descriptions.contains("ambiguous"));
    assert!(descriptions.contains("no-match"));
    assert!(tests.iter().all(|test| {
        test["assert"]
            .as_sequence()
            .is_some_and(|assertions| assertions.len() == 2)
    }));
    assert!(yaml.contains("requireToolEvidence"));
    assert!(yaml.contains("evidenceAll"));
    assert!(yaml.contains("evidenceEmpty"));
    assert!(yaml.contains("env.PROMPTFOO_PROVIDER"));
    assert!(yaml.contains("env.PROMPTFOO_MCP_COMMAND"));
    assert!(yaml.contains("env.PROMPTFOO_PROJECT_PATH"));
    assert!(tests.iter().all(|test| {
        test["vars"]["project_path"]
            .as_str()
            .is_some_and(|path| path.contains("PROMPTFOO_PROJECT_PATH"))
    }));

    let tools: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string("tools/package.json")?)?;
    let promptfoo_version = tools["devDependencies"]["promptfoo"]
        .as_str()
        .expect("tools/package.json must pin promptfoo");
    assert!(
        is_exact_semver(promptfoo_version),
        "promptfoo must be an exact MAJOR.MINOR.PATCH pin, got {promptfoo_version:?}"
    );

    let script = std::fs::read_to_string("scripts/run-agent-eval.sh")?;
    assert!(script.contains("node_tools_prepare"));
    assert!(script.contains("\"$NODE_TOOLS_BIN/promptfoo\" eval"));
    assert!(!script.contains("npx"));
    assert!(script.contains("MAVEN_TRUSTED_PROJECT_DIRECTORIES"));
    assert!(script.contains("MAVEN_EXECUTION_REPO_PATH"));
    assert!(script.contains("PROMPTFOO_PROJECT_PATH"));
    assert!(script.contains("--no-cache"));
    assert!(script.contains("report.html"));
    assert!(script.contains("results.json"));
    Ok(())
}

fn is_exact_semver(version: &str) -> bool {
    let parts = version.split('.').collect::<Vec<_>>();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}
