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
    assert!(yaml.contains("env.PROMPTFOO_PROVIDER"));
    assert!(yaml.contains("env.PROMPTFOO_MCP_URL"));

    let script = std::fs::read_to_string("scripts/run-agent-eval.sh")?;
    assert!(script.contains("PROMPTFOO_VERSION=\"0.121.19\""));
    assert!(script.contains("--no-cache"));
    assert!(script.contains("report.html"));
    assert!(script.contains("results.json"));
    Ok(())
}
