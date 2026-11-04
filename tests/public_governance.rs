//! Public contribution, conduct, support and security governance: the documents exist and
//! are linked from `README.md`, `SECURITY.md` names the private reporting channel, supported
//! versions and response expectations, the issue forms and pull request template ask for
//! reproduction data without private data, and no document references the private workspace.

use std::path::Path;

use anyhow::{Context, Result};
use serde_yaml_ng::Value as Yaml;

const REPO: &str = env!("CARGO_MANIFEST_DIR");
const GOVERNANCE_DOCUMENTS: [&str; 4] = [
    "CONTRIBUTING.md",
    "CODE_OF_CONDUCT.md",
    "SUPPORT.md",
    "SECURITY.md",
];
const ISSUE_FORMS: [&str; 2] = [
    ".github/ISSUE_TEMPLATE/bug_report.yml",
    ".github/ISSUE_TEMPLATE/feature_request.yml",
];
const ISSUE_CONFIG: &str = ".github/ISSUE_TEMPLATE/config.yml";
const PULL_REQUEST_TEMPLATE: &str = ".github/pull_request_template.md";
const PRIVATE_VULNERABILITY_REPORTING: &str =
    "https://github.com/tyutyutyu/maven-mcp/security/advisories/new";
const PRIVATE_REFERENCES: [&str; 9] = [
    ".backlog",
    "backlog",
    "TASK-",
    "doc-0",
    "docs/decisions",
    "docs/testing.md",
    "docs/dogfooding",
    ".brv",
    "/home/",
];

fn read(relative: &str) -> Result<String> {
    std::fs::read_to_string(Path::new(REPO).join(relative))
        .with_context(|| format!("read {relative}"))
}

fn read_yaml(relative: &str) -> Result<Yaml> {
    serde_yaml_ng::from_str(&read(relative)?).with_context(|| format!("parse {relative}"))
}

/// Collapses every whitespace run to one space so checks do not depend on line wrapping.
fn normalized(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn assert_contains_all(relative: &str, text: &str, statements: &[&str]) {
    let text = normalized(text);
    for statement in statements {
        assert!(
            text.contains(&normalized(statement)),
            "{relative} misses {statement:?}"
        );
    }
}

#[test]
fn readme_links_every_governance_document() -> Result<()> {
    let readme = read("README.md")?;
    for document in GOVERNANCE_DOCUMENTS {
        let text = read(document)?;
        assert!(text.starts_with("# "), "{document} has no title");
        assert!(
            readme.contains(&format!("]({document})")),
            "README.md does not link {document}"
        );
    }
    Ok(())
}

#[test]
fn security_policy_names_private_reporting_supported_versions_and_response_goals() -> Result<()> {
    let security = read("SECURITY.md")?;
    assert_contains_all(
        "SECURITY.md",
        &security,
        &[
            "GitHub Private Vulnerability Reporting",
            PRIVATE_VULNERABILITY_REPORTING,
            "Do not open a public issue",
            "## Supported versions",
            "Only the latest release is supported",
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
            "## Response expectations",
            "no service-level agreement",
            "These are goals, not guarantees.",
            "István Földházi",
        ],
    );
    let config = read_yaml(ISSUE_CONFIG)?;
    assert_eq!(config["blank_issues_enabled"].as_bool(), Some(false));
    let links = config["contact_links"]
        .as_sequence()
        .context("contact_links")?;
    assert!(
        links
            .iter()
            .any(|link| link["url"].as_str() == Some(PRIVATE_VULNERABILITY_REPORTING)),
        "the issue chooser does not offer private vulnerability reporting"
    );
    Ok(())
}

#[test]
fn issue_forms_request_reproduction_data_and_reject_private_data() -> Result<()> {
    for form in ISSUE_FORMS {
        let parsed = read_yaml(form)?;
        assert!(parsed["name"].as_str().is_some(), "{form} has no name");
        assert!(parsed["body"].as_sequence().is_some(), "{form} has no body");
    }

    let bug = read_yaml(ISSUE_FORMS[0])?;
    let ids = bug["body"]
        .as_sequence()
        .context("bug report body")?
        .iter()
        .filter_map(|field| field["id"].as_str())
        .collect::<Vec<_>>();
    for id in [
        "version",
        "install",
        "target",
        "environment",
        "reproduction",
        "expected",
        "actual",
    ] {
        assert!(ids.contains(&id), "the bug report misses the {id:?} field");
    }
    let bug_text = read(ISSUE_FORMS[0])?;
    assert_contains_all(
        ISSUE_FORMS[0],
        &bug_text,
        &[
            "maven-mcp --version",
            "settings.xml",
            "complete raw logs",
            "complete local paths",
            "SECURITY.md",
        ],
    );
    Ok(())
}

#[test]
fn pull_request_template_requests_tests_and_safe_review_data() -> Result<()> {
    let template = read(PULL_REQUEST_TEMPLATE)?;
    assert_contains_all(
        PULL_REQUEST_TEMPLATE,
        &template,
        &[
            "## How it was tested",
            "scripts/test-pyramid.sh",
            "cargo build --release --locked",
            "CHANGELOG.md",
            "README.md",
            "Checklist for MCP Tool Changes",
            "no credentials",
            "MIT",
        ],
    );
    Ok(())
}

#[test]
fn contribution_terms_match_the_mit_license() -> Result<()> {
    let license = read("LICENSE")?;
    assert!(license.starts_with("MIT License"));
    for document in ["README.md", "CONTRIBUTING.md"] {
        let text = read(document)?;
        assert_contains_all(
            document,
            &text,
            &[
                "Contributions are accepted under the same",
                "No CLA or DCO sign-off is required",
            ],
        );
    }
    Ok(())
}

#[test]
fn governance_documents_reference_no_private_workspace_data() -> Result<()> {
    let public = GOVERNANCE_DOCUMENTS
        .iter()
        .chain(ISSUE_FORMS.iter())
        .chain([ISSUE_CONFIG, PULL_REQUEST_TEMPLATE].iter());
    for document in public {
        let text = read(document)?;
        for private in PRIVATE_REFERENCES {
            assert!(
                !text.contains(private),
                "{document} references private workspace data {private:?}"
            );
        }
    }
    Ok(())
}
