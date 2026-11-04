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

/// Public Markdown documents whose language, terminology and links are checked.
const PUBLIC_MARKDOWN: [&str; 9] = [
    "README.md",
    "AGENTS.md",
    "CONTRIBUTING.md",
    "CODE_OF_CONDUCT.md",
    "SUPPORT.md",
    "SECURITY.md",
    "CHANGELOG.md",
    "docs/dependency-maintenance.md",
    PULL_REQUEST_TEMPLATE,
];
/// The shared terms that `README.md` defines and the maintenance rule names.
const SHARED_TERMS: [&str; 7] = [
    "native STDIO",
    "`project_path`",
    "canonical project root",
    "trusted directory tree",
    "lazy project index",
    "partial result",
    "deferred result",
];
/// Synonyms replaced by the shared terms. `AGENTS.md` names them in its rule, so it is
/// the only document allowed to contain them.
const RETIRED_TERMS: [&str; 5] = [
    "trusted tree",
    "startup index",
    "host-trusted",
    "the opt-in project",
    "the configured project",
];
/// The `README.md` paragraph that lists what the release privacy check rejects; naming
/// private paths there documents the check and is not a reference to them.
const RELEASE_PRIVACY_PARAGRAPH: &str = "`scripts/release.sh check` also checks";

/// Lines outside fenced code blocks.
fn prose_lines(text: &str) -> Vec<&str> {
    let mut fenced = false;
    text.lines()
        .filter(|line| {
            if line.trim_start().starts_with("```") {
                fenced = !fenced;
                return false;
            }
            !fenced
        })
        .collect()
}

/// GitHub heading anchors of a Markdown document, with `-1`, `-2` suffixes for repeats.
fn heading_anchors(text: &str) -> Vec<String> {
    let mut anchors: Vec<String> = Vec::new();
    for line in prose_lines(text) {
        let Some(title) = line
            .strip_prefix('#')
            .map(|rest| rest.trim_start_matches('#'))
            .and_then(|rest| rest.strip_prefix(' '))
        else {
            continue;
        };
        let slug = title
            .trim()
            .to_lowercase()
            .chars()
            .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_'))
            .map(|c| if c == ' ' { '-' } else { c })
            .collect::<String>();
        let repeats = anchors
            .iter()
            .filter(|anchor| {
                anchor.as_str() == slug
                    || anchor
                        .strip_prefix(&slug)
                        .and_then(|suffix| suffix.strip_prefix('-'))
                        .is_some_and(|n| n.parse::<usize>().is_ok())
            })
            .count();
        anchors.push(if repeats == 0 {
            slug
        } else {
            format!("{slug}-{repeats}")
        });
    }
    anchors
}

/// Targets of inline Markdown links outside fenced code blocks.
fn link_targets(text: &str) -> Result<Vec<String>> {
    let link = regex::Regex::new(r"\]\(([^)\s]+)\)")?;
    Ok(prose_lines(text)
        .into_iter()
        .flat_map(|line| link.captures_iter(line).map(|c| c[1].to_owned()))
        .collect::<Vec<_>>())
}

fn without_paragraph(text: &str, start: &str) -> String {
    match text.find(start) {
        Some(begin) => {
            let end = text[begin..]
                .find("\n\n")
                .map_or(text.len(), |offset| begin + offset);
            format!("{}{}", &text[..begin], &text[end..])
        }
        None => text.to_owned(),
    }
}

#[test]
fn readme_defines_the_shared_terminology_and_documentation_language() -> Result<()> {
    let readme = read("README.md")?;
    let terminology = readme
        .split("\n## Terminology\n")
        .nth(1)
        .and_then(|rest| rest.split("\n## ").next())
        .context("README.md misses ## Terminology")?;
    for term in SHARED_TERMS {
        assert!(
            terminology.contains(&format!("\n| {term} |")),
            "the README terminology does not define {term:?}"
        );
    }
    assert_contains_all(
        "README.md",
        terminology,
        &[
            "### Documentation language",
            "English is the primary and only maintained language",
            "Translations are not maintained in this repository.",
            "](AGENTS.md#documentation-language-and-terminology)",
        ],
    );
    Ok(())
}

#[test]
fn readme_reaches_the_architecture_and_security_model_in_english() -> Result<()> {
    let readme = read("README.md")?;
    let documentation = readme
        .split("\n## Documentation\n")
        .nth(1)
        .and_then(|rest| rest.split("\n## ").next())
        .context("README.md misses ## Documentation")?;
    for target in [
        "(#getting-started)",
        "(#architecture-and-security-model)",
        "(#terminology)",
        "(#opt-in-project-execution)",
        "(#redaction-and-data-minimization)",
        "(#local-development)",
        "(#releases)",
        "(AGENTS.md)",
        "(SECURITY.md)",
    ] {
        assert!(
            documentation.contains(target),
            "the README documentation map does not link {target}"
        );
    }
    let architecture = readme
        .split("\n## Architecture and security model\n")
        .nth(1)
        .and_then(|rest| rest.split("\n## ").next())
        .context("README.md misses ## Architecture and security model")?;
    assert_contains_all(
        "README.md",
        architecture,
        &[
            "**Native STDIO.**",
            "stdout carries only protocol messages",
            "**Request-scoped projects.**",
            "canonical project root",
            "**Trust boundary.**",
            "MAVEN_TRUSTED_PROJECT_DIRECTORIES",
            "trusted directory trees",
            "**Lazy project index.**",
            "**Maven execution.**",
            "is not a sandbox",
            "partial result",
            "**Data minimization.**",
            "[SECURITY.md](SECURITY.md)",
        ],
    );
    let local_development = readme
        .split("\n## Local Development\n")
        .nth(1)
        .context("README.md misses ## Local Development")?;
    assert_contains_all(
        "README.md",
        local_development,
        &[
            "cargo test --locked",
            "cargo test --test mcp_interface --locked",
            "scripts/test-pyramid.sh",
            "There are four separate verification levels:",
        ],
    );
    Ok(())
}

#[test]
fn agents_md_records_the_documentation_language_and_terminology_rule() -> Result<()> {
    let agents = read("AGENTS.md")?;
    let rule = agents
        .split("\n## Documentation Language and Terminology\n")
        .nth(1)
        .and_then(|rest| rest.split("\n## ").next())
        .context("AGENTS.md misses ## Documentation Language and Terminology")?;
    assert_contains_all(
        "AGENTS.md",
        rule,
        &[
            "English is the primary and authoritative language",
            "Translations are optional",
            "marked as non-authoritative, links to its English source",
            "updated or removed in the same pull request",
            "\"Terminology\" in `README.md`",
            "never linked or required as references",
        ],
    );
    for term in SHARED_TERMS {
        assert!(
            normalized(rule).contains(term),
            "the AGENTS.md rule does not name {term:?}"
        );
    }
    Ok(())
}

#[test]
fn public_documents_use_english_shared_terminology() -> Result<()> {
    for document in PUBLIC_MARKDOWN {
        let text = read(document)?;
        let prose = prose_lines(&text).join("\n");
        assert!(
            !prose.contains(['ő', 'ű', 'Ő', 'Ű']),
            "{document} contains Hungarian prose"
        );
        if document == "AGENTS.md" {
            continue;
        }
        let normalized_text = normalized(&text);
        for retired in RETIRED_TERMS {
            assert!(
                !normalized_text.contains(retired),
                "{document} uses the retired term {retired:?}"
            );
        }
    }
    Ok(())
}

#[test]
fn public_markdown_links_resolve_to_files_and_headings() -> Result<()> {
    for document in PUBLIC_MARKDOWN {
        let text = read(document)?;
        let directory = Path::new(document).parent().unwrap_or(Path::new(""));
        for target in link_targets(&text)? {
            if target.starts_with("https://") || target.starts_with("mailto:") {
                continue;
            }
            let (file, anchor) = target.split_once('#').unwrap_or((&target, ""));
            let linked = if file.is_empty() {
                document.to_owned()
            } else {
                directory.join(file).to_string_lossy().into_owned()
            };
            assert!(
                Path::new(REPO).join(&linked).exists(),
                "{document} links the missing file {target:?}"
            );
            if !anchor.is_empty() {
                assert!(
                    heading_anchors(&read(&linked)?)
                        .iter()
                        .any(|heading| heading == anchor),
                    "{document} links the missing heading {target:?}"
                );
            }
        }
    }
    Ok(())
}

#[test]
fn public_documents_require_no_private_workspace_references() -> Result<()> {
    for document in PUBLIC_MARKDOWN {
        let text = read(document)?;
        for target in link_targets(&text)? {
            for private in PRIVATE_REFERENCES {
                assert!(
                    !target.contains(private),
                    "{document} links private workspace data {target:?}"
                );
            }
        }
        let checked = if document == "README.md" {
            without_paragraph(&text, RELEASE_PRIVACY_PARAGRAPH)
        } else {
            text
        };
        for private in PRIVATE_REFERENCES
            .iter()
            .filter(|private| **private != "/home/")
        {
            assert!(
                !checked.contains(private),
                "{document} references private workspace data {private:?}"
            );
        }
    }
    Ok(())
}
