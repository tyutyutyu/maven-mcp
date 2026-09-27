mod support;

use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use rmcp::model::CallToolRequestParams;
use serde::Deserialize;
use serde_json::Value;
use support::TestServer;

#[derive(Debug, Deserialize)]
struct ScenarioCatalog {
    scenarios: Vec<Scenario>,
}

#[derive(Debug, Deserialize)]
struct Scenario {
    id: String,
    title: String,
    description: String,
    tool: String,
    arguments: Value,
    expect: Expectations,
}

#[derive(Debug, Deserialize)]
struct Expectations {
    result_count: Option<usize>,
    #[serde(default)]
    contains: Vec<Value>,
    matches: Option<Value>,
    equals: Option<Value>,
}

struct Execution {
    scenario: Scenario,
    response: Value,
    failures: Vec<String>,
}

#[tokio::test]
async fn documented_mcp_search_scenarios_match_snapshots() -> Result<()> {
    let catalog = load_catalog()?;
    ensure!(!catalog.scenarios.is_empty(), "scenario catalog is empty");
    ensure_unique_ids(&catalog.scenarios)?;

    let server = TestServer::start_with_repository_inspection_project().await?;
    let project_path = server
        .project_path()
        .context("scenario fixture must include a Maven project")?
        .display()
        .to_string();
    let client = server.connect().await?;
    let project_root = server
        .project_path()
        .context("scenario fixture must include a Maven project")?;
    let project_files_before = snapshot_file_tree(project_root)?;
    let mut executions = Vec::with_capacity(catalog.scenarios.len());

    for scenario in catalog.scenarios {
        let arguments = scenario
            .arguments
            .as_object()
            .with_context(|| format!("scenario '{}' arguments must be an object", scenario.id))?
            .clone();
        let mut arguments = arguments;
        arguments.insert(
            "project_path".to_owned(),
            Value::String(project_path.clone()),
        );
        let result = client
            .call_tool(CallToolRequestParams::new(scenario.tool.clone()).with_arguments(arguments))
            .await
            .with_context(|| format!("scenario '{}' MCP call failed", scenario.id))?;
        ensure!(
            result.is_error != Some(true),
            "scenario '{}' returned a tool error: {:?}",
            scenario.id,
            result.content
        );
        let response = result
            .structured_content
            .with_context(|| format!("scenario '{}' has no structured response", scenario.id))?;
        let failures = validate_expectations(&scenario.expect, &response);
        executions.push(Execution {
            scenario,
            response,
            failures,
        });
    }

    client.cancel().await?;
    let project_files_after = snapshot_file_tree(project_root)?;
    ensure!(
        project_files_before == project_files_after,
        "repository inspection changed a project or Maven repository file"
    );
    write_markdown_report(&executions)?;

    let semantic_failures = executions
        .iter()
        .flat_map(|execution| {
            execution
                .failures
                .iter()
                .map(|failure| format!("{}: {failure}", execution.scenario.id))
        })
        .collect::<Vec<_>>();
    assert!(
        semantic_failures.is_empty(),
        "scenario expectation failures:\n{}",
        semantic_failures.join("\n")
    );

    for execution in &executions {
        insta::assert_json_snapshot!(execution.scenario.id.as_str(), execution.response);
    }
    Ok(())
}

fn load_catalog() -> Result<ScenarioCatalog> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/scenarios/maven_search.yaml");
    let content = fs::read_to_string(&path)
        .with_context(|| format!("cannot read scenario catalog {}", path.display()))?;
    serde_yaml_ng::from_str(&content)
        .with_context(|| format!("invalid scenario catalog {}", path.display()))
}

fn ensure_unique_ids(scenarios: &[Scenario]) -> Result<()> {
    let mut ids = std::collections::BTreeSet::new();
    for scenario in scenarios {
        ensure!(
            ids.insert(&scenario.id),
            "duplicate scenario id: {}",
            scenario.id
        );
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum TreeEntry {
    Directory(TreeMetadata),
    File {
        metadata: TreeMetadata,
        bytes: Vec<u8>,
    },
}

#[derive(Debug, PartialEq, Eq)]
struct TreeMetadata {
    mode: u32,
    modified: (i64, i64),
    changed: (i64, i64),
}

fn tree_metadata(metadata: &fs::Metadata) -> TreeMetadata {
    TreeMetadata {
        mode: metadata.mode(),
        modified: (metadata.mtime(), metadata.mtime_nsec()),
        changed: (metadata.ctime(), metadata.ctime_nsec()),
    }
}

fn snapshot_file_tree(root: &Path) -> Result<BTreeMap<PathBuf, TreeEntry>> {
    fn visit(
        root: &Path,
        directory: &Path,
        entries: &mut BTreeMap<PathBuf, TreeEntry>,
    ) -> Result<()> {
        let mut children = fs::read_dir(directory)?.collect::<std::io::Result<Vec<_>>>()?;
        children.sort_by_key(|entry| entry.file_name());
        for child in children {
            let path = child.path();
            let relative = path
                .strip_prefix(root)
                .context("project file must remain beneath the project root")?
                .to_owned();
            let metadata = child.metadata()?;
            if metadata.is_dir() {
                entries.insert(relative, TreeEntry::Directory(tree_metadata(&metadata)));
                visit(root, &path, entries)?;
            } else if metadata.is_file() {
                entries.insert(
                    relative,
                    TreeEntry::File {
                        metadata: tree_metadata(&metadata),
                        bytes: fs::read(path)?,
                    },
                );
            }
        }
        Ok(())
    }

    let mut entries = BTreeMap::new();
    entries.insert(
        PathBuf::new(),
        TreeEntry::Directory(tree_metadata(&fs::metadata(root)?)),
    );
    visit(root, root, &mut entries)?;
    Ok(entries)
}

fn validate_expectations(expect: &Expectations, actual: &Value) -> Vec<String> {
    let mut failures = Vec::new();
    if let Some(expected_count) = expect.result_count {
        match result_items(actual) {
            Some(items) if items.len() != expected_count => failures.push(format!(
                "expected {expected_count} results, received {}",
                items.len()
            )),
            None => failures.push("result_count requires an array response".to_owned()),
            _ => {}
        }
    }
    if !expect.contains.is_empty() {
        match result_items(actual) {
            Some(items) => {
                for expected_item in &expect.contains {
                    if !items.iter().any(|item| is_json_subset(expected_item, item)) {
                        failures.push(format!(
                            "response does not contain expected subset {}",
                            compact_json(expected_item)
                        ));
                    }
                }
            }
            None => failures.push("contains requires an array response".to_owned()),
        }
    }
    if let Some(expected) = &expect.matches
        && !is_json_subset(expected, actual)
    {
        failures.push(format!(
            "response does not match expected subset {}",
            compact_json(expected)
        ));
    }
    if let Some(expected) = &expect.equals
        && expected != actual
    {
        failures.push(format!(
            "expected exact response {}, received {}",
            compact_json(expected),
            compact_json(actual)
        ));
    }
    failures
}

fn result_items(actual: &Value) -> Option<&Vec<Value>> {
    actual
        .as_array()
        .or_else(|| actual.get("results").and_then(Value::as_array))
}

fn is_json_subset(expected: &Value, actual: &Value) -> bool {
    match (expected, actual) {
        (Value::Object(expected), Value::Object(actual)) => expected.iter().all(|(key, value)| {
            actual
                .get(key)
                .is_some_and(|candidate| is_json_subset(value, candidate))
        }),
        (Value::Array(expected), Value::Array(actual)) => {
            expected.len() == actual.len()
                && expected
                    .iter()
                    .zip(actual)
                    .all(|(expected, actual)| is_json_subset(expected, actual))
        }
        _ => expected == actual,
    }
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "<invalid JSON>".to_owned())
}

fn write_markdown_report(executions: &[Execution]) -> Result<()> {
    let report_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/mcp-test-report");
    fs::create_dir_all(&report_dir)?;
    let mut report = String::from(
        "# Maven MCP keresési riport\n\n\
         A riport a `tests/scenarios/maven_search.yaml` scenario-k valódi child-process STDIO MCP-hívásaiból készült.\n\n",
    );
    let passed = executions
        .iter()
        .filter(|execution| execution.failures.is_empty())
        .count();
    report.push_str(&format!(
        "**Összesítés:** {passed}/{} scenario sikeres.\n\n",
        executions.len()
    ));

    for execution in executions {
        let status = if execution.failures.is_empty() {
            "✅ Sikeres"
        } else {
            "❌ Sikertelen"
        };
        report.push_str(&format!(
            "## {status}: {}\n\n{}\n\n",
            execution.scenario.title, execution.scenario.description
        ));
        report.push_str(&format!(
            "### MCP kérés\n\nTool: `{}`\n\n```json\n{}\n```\n\n",
            execution.scenario.tool,
            serde_json::to_string_pretty(&execution.scenario.arguments)?
        ));
        report.push_str(&format!(
            "### MCP válasz\n\n```json\n{}\n```\n\n",
            serde_json::to_string_pretty(&execution.response)?
        ));
        report.push_str("### Ellenőrzések\n\n");
        if execution.failures.is_empty() {
            report.push_str("- ✅ Minden szemantikus elvárás teljesült.\n\n");
        } else {
            for failure in &execution.failures {
                report.push_str(&format!("- ❌ {failure}\n"));
            }
            report.push('\n');
        }
    }

    fs::write(report_dir.join("report.md"), report)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::is_json_subset;

    #[test]
    fn subset_matching_supports_nested_objects_and_arrays() {
        let expected = json!({"jar": {"coordinate": "g:a:1"}, "classes": ["A"]});
        let actual = json!({
            "jar": {"coordinate": "g:a:1", "path": "g/a/1/a-1.jar"},
            "classes": ["A"],
            "total": 1
        });
        assert!(is_json_subset(&expected, &actual));
        assert!(!is_json_subset(&json!({"total": 2}), &actual));
    }
}
