//! Offline checks that the pinned toolchains, the CI platform matrix and the documented
//! support statement stay consistent. They read repository files only.

use std::collections::BTreeSet;

use anyhow::{Context, Result};
use serde_json::Value as Json;
use serde_yaml_ng::Value as Yaml;

const SUPPORTED_TARGETS: [&str; 2] = ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"];

fn read(path: &str) -> Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("cannot read {path}"))
}

fn quoted_value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|line| {
        line.trim()
            .strip_prefix(key)?
            .trim_start()
            .strip_prefix('=')?
            .trim()
            .strip_prefix('"')?
            .strip_suffix('"')
    })
}

fn version_parts(version: &str) -> Option<Vec<u64>> {
    version
        .split('.')
        .map(|part| part.parse::<u64>().ok())
        .collect()
}

fn is_exact_semver(version: &str) -> bool {
    version_parts(version).is_some_and(|parts| parts.len() == 3)
}

fn ci_workflow() -> Result<Yaml> {
    Ok(serde_yaml_ng::from_str(&read(".github/workflows/ci.yml")?)?)
}

fn step_commands(job: &Yaml) -> String {
    job["steps"]
        .as_sequence()
        .into_iter()
        .flatten()
        .filter_map(|step| step["run"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn developer_and_ci_rust_toolchain_is_an_exact_pin_newer_than_the_msrv() -> Result<()> {
    let toolchain = read("rust-toolchain.toml")?;
    let channel = quoted_value(&toolchain, "channel").context("rust-toolchain.toml channel")?;
    assert!(
        is_exact_semver(channel),
        "channel must be X.Y.Z, got {channel:?}"
    );
    for component in ["clippy", "rustfmt", "llvm-tools-preview"] {
        assert!(
            toolchain.contains(&format!("\"{component}\"")),
            "{component} missing"
        );
    }

    let manifest = read("Cargo.toml")?;
    let msrv = quoted_value(&manifest, "rust-version").context("Cargo.toml rust-version")?;
    let msrv_parts = version_parts(msrv).context("rust-version must be numeric")?;
    let channel_parts = version_parts(channel).context("channel must be numeric")?;
    assert!(
        channel_parts > msrv_parts,
        "the pinned toolchain {channel} must be newer than the MSRV {msrv}"
    );
    Ok(())
}

#[test]
fn ci_installs_the_pinned_toolchain_and_checks_the_msrv_separately() -> Result<()> {
    let workflow = ci_workflow()?;
    let jobs = workflow["jobs"].as_mapping().context("jobs")?;
    for (name, job) in jobs {
        let commands = step_commands(job);
        let uses_cargo = commands.contains("cargo ") || commands.contains("scripts/");
        if uses_cargo && name.as_str() != Some("msrv") {
            assert!(
                commands
                    .lines()
                    .any(|line| line.trim() == "rustup toolchain install"),
                "job {name:?} must install the rust-toolchain.toml pin"
            );
        }
    }
    let msrv = step_commands(&workflow["jobs"]["msrv"]);
    assert!(
        msrv.contains("rust-version"),
        "the MSRV must come from Cargo.toml"
    );
    assert!(msrv.contains("check --locked --all-targets --all-features"));
    Ok(())
}

#[test]
fn ci_builds_and_tests_every_supported_linux_target_natively() -> Result<()> {
    let workflow = ci_workflow()?;
    let platform = &workflow["jobs"]["platform"];
    let targets = platform["strategy"]["matrix"]["include"]
        .as_sequence()
        .context("platform matrix")?
        .iter()
        .filter_map(|entry| entry["target"].as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(targets, SUPPORTED_TARGETS.into_iter().collect());

    let commands = step_commands(platform);
    assert!(commands.contains("cargo test --all-targets --locked"));
    assert!(commands.contains("cargo build --release --locked"));
    assert!(commands.contains("scripts/check-linux-binary.sh"));
    // x86_64 tests run in the coverage gate; the other targets run them in the matrix.
    assert!(step_commands(&workflow["jobs"]["gate"]).contains("scripts/test-pyramid.sh"));
    Ok(())
}

#[test]
fn aarch64_links_with_the_bundled_lld_to_keep_newer_glibc_references_weak() -> Result<()> {
    let config = read(".cargo/config.toml")?;
    let aarch64 = config
        .split("[target.aarch64-unknown-linux-gnu]")
        .nth(1)
        .context(".cargo/config.toml must configure aarch64-unknown-linux-gnu")?;
    assert_eq!(
        quoted_value(aarch64, "linker"),
        Some("scripts/cc-rust-lld.sh")
    );
    let wrapper = read("scripts/cc-rust-lld.sh")?;
    assert!(wrapper.contains("-fuse-ld=lld"));
    assert!(wrapper.contains("bin/gcc-ld"));
    Ok(())
}

#[test]
fn required_ci_check_passes_only_when_every_job_passes() -> Result<()> {
    let workflow = ci_workflow()?;
    let jobs = workflow["jobs"].as_mapping().context("jobs")?;
    let ci = &workflow["jobs"]["ci"];
    assert_eq!(ci["name"].as_str(), Some("CI"));
    assert_eq!(ci["if"].as_str(), Some("always()"));
    let needs = ci["needs"]
        .as_sequence()
        .context("CI needs")?
        .iter()
        .filter_map(Yaml::as_str)
        .collect::<BTreeSet<_>>();
    let others = jobs
        .keys()
        .filter_map(Yaml::as_str)
        .filter(|name| *name != "ci")
        .collect::<BTreeSet<_>>();
    assert_eq!(needs, others, "CI must depend on every other job");
    assert!(step_commands(ci).contains("= success"));
    Ok(())
}

#[test]
fn node_tools_are_exactly_pinned_and_locked_with_integrity() -> Result<()> {
    let node_version = read(".node-version")?;
    assert!(is_exact_semver(node_version.trim()), "{node_version:?}");

    let package: Json = serde_json::from_str(&read("tools/package.json")?)?;
    let npm = package["packageManager"]
        .as_str()
        .and_then(|value| value.strip_prefix("npm@"))
        .context("tools/package.json packageManager must pin npm")?;
    assert!(is_exact_semver(npm), "{npm:?}");
    let pinned = package["devDependencies"]
        .as_object()
        .context("devDependencies")?;
    for name in ["@modelcontextprotocol/inspector", "promptfoo"] {
        let version = pinned[name].as_str().unwrap_or_default();
        assert!(
            is_exact_semver(version),
            "{name} must be an exact pin: {version:?}"
        );
    }

    let lock: Json = serde_json::from_str(&read("tools/package-lock.json")?)?;
    assert_eq!(lock["lockfileVersion"].as_u64(), Some(3));
    assert_eq!(
        lock["packages"][""]["devDependencies"],
        package["devDependencies"]
    );
    let packages = lock["packages"].as_object().context("lock packages")?;
    let unlocked = packages
        .iter()
        .filter(|(path, entry)| !path.is_empty() && entry["link"].as_bool() != Some(true))
        .filter(|(_, entry)| {
            let has_integrity = entry["integrity"]
                .as_str()
                .is_some_and(|hash| hash.starts_with("sha512-"));
            let from_registry = entry["resolved"]
                .as_str()
                .is_some_and(|url| url.starts_with("https://registry.npmjs.org/"));
            !(has_integrity && from_registry)
        })
        .map(|(path, _)| path.as_str())
        .collect::<Vec<_>>();
    assert!(
        unlocked.is_empty(),
        "packages without registry integrity: {unlocked:?}"
    );
    Ok(())
}

#[test]
fn node_tool_scripts_use_the_locked_install_instead_of_npx() -> Result<()> {
    for script in ["scripts/run-inspector.sh", "scripts/run-agent-eval.sh"] {
        let text = read(script)?;
        assert!(text.contains("node_tools_prepare"), "{script}");
        assert!(
            !text.contains("npx"),
            "{script} must not resolve tools through npx"
        );
    }
    let helper = read("scripts/node-tools.sh")?;
    assert!(helper.contains("npm ci"));
    assert!(helper.contains(".node-version"));
    assert!(
        helper.contains(r#""${active_npm}" != "${pinned_npm}""#),
        "the helper must reject every npm version other than the exact packageManager pin"
    );
    Ok(())
}

#[test]
fn ci_node_tools_job_enforces_the_exact_package_manager_npm() -> Result<()> {
    let workflow = ci_workflow()?;
    let commands = step_commands(&workflow["jobs"]["node-tools"]);
    assert!(commands.contains("tools/package.json"));
    let install = commands
        .find(r#"npm install -g "npm@${pinned_npm}""#)
        .context("the Node tools job must install the packageManager npm version")?;
    let check = commands
        .find(r#"test "$(npm --version)" = "${pinned_npm}""#)
        .context("the Node tools job must assert the exact npm version")?;
    let locked_install = commands
        .find("scripts/run-inspector.sh")
        .context("the Node tools job must run the locked install")?;
    assert!(install < check && check < locked_install);
    Ok(())
}

#[test]
fn toolchain_pins_are_maintained_by_scheduled_renovate_updates() -> Result<()> {
    let renovate: Json = serde_json::from_str(&read("renovate.json")?)?;
    assert!(
        renovate["schedule"]
            .as_array()
            .is_some_and(|value| !value.is_empty())
    );
    let managers = renovate["enabledManagers"]
        .as_array()
        .context("enabledManagers")?
        .iter()
        .filter_map(Json::as_str)
        .collect::<BTreeSet<_>>();
    for manager in ["cargo", "github-actions", "npm", "nodenv", "custom.regex"] {
        assert!(managers.contains(manager), "{manager} must be enabled");
    }
    let custom = serde_json::to_string(&renovate["customManagers"])?;
    assert!(custom.contains("rust-toolchain"));
    assert!(custom.contains("rust-lang/rust"));
    assert!(custom.contains("cargo-(?:deny|llvm-cov)"));
    Ok(())
}

#[test]
fn readme_states_the_supported_platform_matrix_and_its_limits() -> Result<()> {
    let readme = read("README.md")?;
    let section = readme
        .split("## Supported Platforms")
        .nth(1)
        .and_then(|rest| rest.split("\n## ").next())
        .context("README must have a Supported Platforms section")?;
    for expected in SUPPORTED_TARGETS.into_iter().chain([
        "macOS",
        "Windows",
        "musl",
        "rust-toolchain.toml",
        ".node-version",
    ]) {
        assert!(
            section.contains(expected),
            "Supported Platforms must mention {expected}"
        );
    }
    Ok(())
}
