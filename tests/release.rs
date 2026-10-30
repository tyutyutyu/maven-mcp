//! Native versioned releases (`scripts/release.sh`, `.github/workflows/release.yml`):
//! one version source, the changelog-based release notes, the privacy check of the package
//! and archive manifests, and reproducible, verified archives. The script runs from a copy
//! in a temporary Git repository with stub `cargo`, `rustc` and binary-check tools; the
//! real crates.io package list is checked against the repository itself. Nothing is built,
//! tagged, uploaded or published.

use std::{
    collections::BTreeSet,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use anyhow::{Context, Result};
use serde_yaml_ng::Value as Yaml;
use tempfile::TempDir;

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/release.sh");
const TARGET: &str = "x86_64-unknown-linux-gnu";
const SUPPORTED_TARGETS: [&str; 2] = ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"];
const ARCHIVE: &str = "maven-mcp-0.1.0-x86_64-unknown-linux-gnu.tar.gz";
const PACKAGE_LIST: &str = "Cargo.lock\nCargo.toml\nCargo.toml.orig\nCHANGELOG.md\nLICENSE\nREADME.md\nsrc/lib.rs\nsrc/main.rs\n";
const UNRELEASED_ONLY: &str = "# Changelog\n\n## [Unreleased]\n\n### Added\n\n- Everything.\n";
const RELEASED: &str = "# Changelog\n\n## [Unreleased]\n\n## [0.1.0] - 2026-11-02\n\n### Added\n\n- First beta.\n- Second line.\n\n## [0.0.9] - 2026-10-01\n\n- Older.\n";

fn write_executable(path: &Path, body: &str) -> Result<()> {
    std::fs::write(path, format!("#!/bin/sh\n{body}\n"))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

fn manifest(version: &str) -> String {
    format!("[package]\nname = \"maven-mcp\"\nversion = \"{version}\"\n\n[dependencies]\n")
}

fn lock(version: &str) -> String {
    format!("version = 4\n\n[[package]]\nname = \"maven-mcp\"\nversion = \"{version}\"\n")
}

/// A committed release source tree with a copy of the script, stub tools on `PATH`, and a
/// stub `cargo` that lists `package-list` and builds a fake binary reporting
/// `maven-mcp <FAKE_REPORTED_VERSION or 0.1.0> (commit <MAVEN_MCP_BUILD_COMMIT>)`.
struct ReleaseTree {
    tree: TempDir,
    tools: TempDir,
}

impl ReleaseTree {
    fn new() -> Result<Self> {
        let release = Self {
            tree: TempDir::new()?,
            tools: TempDir::new()?,
        };
        let root = release.tree.path();
        std::fs::create_dir_all(root.join("scripts"))?;
        std::fs::create_dir_all(root.join("src"))?;
        std::fs::copy(SCRIPT, root.join("scripts/release.sh"))?;
        write_executable(&root.join("scripts/check-linux-binary.sh"), "exit 0")?;
        std::fs::write(root.join(".gitignore"), "/target/\n")?;
        std::fs::write(root.join("Cargo.toml"), manifest("0.1.0"))?;
        std::fs::write(root.join("Cargo.lock"), lock("0.1.0"))?;
        std::fs::write(root.join("CHANGELOG.md"), UNRELEASED_ONLY)?;
        std::fs::write(root.join("LICENSE"), "MIT\n")?;
        std::fs::write(root.join("README.md"), "# Fixture\n")?;
        std::fs::write(root.join("src/lib.rs"), "")?;
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n")?;
        release.write_package_list(PACKAGE_LIST)?;
        write_executable(
            &release.tools.path().join("rustc"),
            &format!("echo 'rustc 1.99.1'; echo 'host: {TARGET}'"),
        )?;
        write_executable(
            &release.tools.path().join("cargo"),
            &format!(
                r#"case "$1" in
  package) cat '{list}' ;;
  build)
    binary="$CARGO_TARGET_DIR/release/maven-mcp"
    mkdir -p "$CARGO_TARGET_DIR/release"
    printf '#!/bin/sh\necho "maven-mcp %s (commit %s)"\n' "${{FAKE_REPORTED_VERSION:-0.1.0}}" "$MAVEN_MCP_BUILD_COMMIT" >"$binary"
    chmod 0755 "$binary"
    printf 'SOURCE_DATE_EPOCH=%s\nRUSTFLAGS=%s\nARGS=%s\n' "$SOURCE_DATE_EPOCH" "$RUSTFLAGS" "$*" >'{build_env}'
    echo "{{\"reason\":\"compiler-artifact\",\"target\":{{\"kind\":[\"bin\"],\"name\":\"maven-mcp\"}},\"executable\":\"$binary\"}}"
    echo '{{"reason":"build-finished","success":true}}' ;;
  *) exit 64 ;;
esac"#,
                list = release.tools.path().join("package-list").display(),
                build_env = release.build_env_path().display(),
            ),
        )?;
        release.git(&["init", "--quiet"])?;
        release.git(&["add", "."])?;
        release.git(&["commit", "--quiet", "-m", "fixture"])?;
        Ok(release)
    }

    fn root(&self) -> &Path {
        self.tree.path()
    }

    fn build_env_path(&self) -> PathBuf {
        self.tools.path().join("build-env")
    }

    fn write_package_list(&self, list: &str) -> Result<()> {
        Ok(std::fs::write(
            self.tools.path().join("package-list"),
            list,
        )?)
    }

    fn write(&self, file: &str, content: &str) -> Result<()> {
        Ok(std::fs::write(self.root().join(file), content)?)
    }

    fn git(&self, arguments: &[&str]) -> Result<String> {
        let output = Command::new("git")
            .args([
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=fixture@example.invalid",
            ])
            .args(["-c", "commit.gpgsign=false"])
            .args(arguments)
            .current_dir(self.root())
            .output()?;
        assert!(output.status.success(), "git {arguments:?} failed");
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    }

    fn run(&self, arguments: &[&str], environment: &[(&str, &str)]) -> Result<Output> {
        let path = format!(
            "{}:{}",
            self.tools.path().display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Ok(Command::new(self.root().join("scripts/release.sh"))
            .args(arguments)
            .env("PATH", path)
            .env_remove("FAKE_REPORTED_VERSION")
            .envs(environment.iter().copied())
            .output()?)
    }

    fn succeed(&self, arguments: &[&str]) -> Result<String> {
        let output = self.run(arguments, &[])?;
        assert!(
            output.status.success(),
            "{arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8(output.stdout)?)
    }

    fn fail(&self, arguments: &[&str]) -> Result<String> {
        self.fail_with(arguments, &[])
    }

    fn fail_with(&self, arguments: &[&str], environment: &[(&str, &str)]) -> Result<String> {
        let output = self.run(arguments, environment)?;
        assert!(
            !output.status.success(),
            "{arguments:?} unexpectedly succeeded"
        );
        Ok(String::from_utf8(output.stderr)?)
    }
}

fn check_manifest(paths: &str) -> Result<Output> {
    use std::io::Write;
    let mut child = Command::new(SCRIPT)
        .arg("check-manifest")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .context("check-manifest stdin")?
        .write_all(paths.as_bytes())?;
    Ok(child.wait_with_output()?)
}

fn path(value: &Path) -> &str {
    value.to_str().expect("temporary paths are UTF-8")
}

fn archive_entries(archive: &Path) -> Result<Vec<String>> {
    let output = Command::new("tar")
        .args(["--list", "--gzip", "--file", path(archive)])
        .output()?;
    assert!(output.status.success());
    let mut entries = String::from_utf8(output.stdout)?
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    entries.sort();
    Ok(entries)
}

#[test]
fn crates_io_package_lists_only_public_sources_and_documents() -> Result<()> {
    let output = Command::new(env!("CARGO"))
        .args(["package", "--list", "--locked", "--allow-dirty"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()?;
    assert!(
        output.status.success(),
        "cargo package --list failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let listing = String::from_utf8(output.stdout)?;
    let documents = BTreeSet::from([
        ".cargo_vcs_info.json",
        "CHANGELOG.md",
        "Cargo.lock",
        "Cargo.toml",
        "Cargo.toml.orig",
        "LICENSE",
        "README.md",
    ]);
    let unexpected = listing
        .lines()
        .filter(|file| {
            !documents.contains(file) && !(file.starts_with("src/") && file.ends_with(".rs"))
        })
        .collect::<Vec<_>>();
    assert!(
        unexpected.is_empty(),
        "unexpected package files: {unexpected:?}"
    );
    for required in ["CHANGELOG.md", "LICENSE", "README.md", "src/main.rs"] {
        assert!(listing.lines().any(|file| file == required), "{required}");
    }

    let checked = check_manifest(&listing)?;
    assert!(
        checked.status.success(),
        "the real package list must pass the privacy check: {}",
        String::from_utf8_lossy(&checked.stderr)
    );
    Ok(())
}

#[test]
fn manifest_check_rejects_private_data_test_output_and_credentials() -> Result<()> {
    for (file, reason) in [
        (".backlog/tasks/task-0052.md", "private workspace data"),
        ("pkg/.brv/context.json", "private workspace data"),
        ("docs/decisions/0001-adr.md", "private workspace data"),
        ("docs/testing.md", "private workspace data"),
        ("docs/dogfooding/manifest.json", "private workspace data"),
        (".claude/settings.local.json", "local agent configuration"),
        ("CLAUDE.local.md", "local agent configuration"),
        ("target/promptfoo/results.json", "evaluation output"),
        ("tests/promptfoo/report.html", "evaluation output"),
        ("target/mcp-test-report/report.md", "evaluation output"),
        ("coverage/summary.json", "evaluation output"),
        ("benchmark-results.json", "evaluation output"),
        ("maven-mcp.log", "evaluation output"),
        ("logs/idea.log.1", "evaluation output"),
        ("default.profraw", "evaluation output"),
        (".env", "credential file"),
        ("config/.env.production", "credential file"),
        (".cargo/credentials.toml", "credential file"),
        ("home/.npmrc", "credential file"),
        (".netrc", "credential file"),
        (".m2/settings-security.xml", "credential file"),
        ("keys/release.pem", "credential file"),
        ("ssh/id_ed25519", "credential file"),
    ] {
        let output = check_manifest(&format!("src/lib.rs\n{file}\n"))?;
        assert!(!output.status.success(), "{file} must be rejected");
        let error = String::from_utf8(output.stderr)?;
        assert!(
            error.contains(file) && error.contains(reason),
            "{file}: {error}"
        );
    }

    let public = "./Cargo.toml\nsrc/redaction.rs\ndocs/dependency-maintenance.md\n.env.example\nmaven-mcp-0.1.0-x86_64-unknown-linux-gnu/README.md\n";
    assert!(check_manifest(public)?.status.success());
    assert!(!check_manifest("\n")?.status.success(), "empty manifest");
    Ok(())
}

#[test]
fn check_requires_one_version_across_manifest_lock_changelog_and_tag() -> Result<()> {
    let release = ReleaseTree::new()?;
    let ok = release.succeed(&["check"])?;
    assert!(ok.contains("maven-mcp 0.1.0 is consistent"), "{ok}");

    let error = release.fail(&["check", "--tag", "v0.1.0"])?;
    assert!(error.contains("'## [0.1.0] - YYYY-MM-DD'"), "{error}");

    release.write("CHANGELOG.md", RELEASED)?;
    let tagged = release.succeed(&["check", "--tag", "v0.1.0"])?;
    assert!(tagged.contains("with tag v0.1.0"), "{tagged}");
    for tag in ["v0.2.0", "0.1.0", "v0.1.0-beta.1"] {
        let error = release.fail(&["check", "--tag", tag])?;
        assert!(error.contains("does not match"), "{tag}: {error}");
    }

    release.write(
        "CHANGELOG.md",
        "## [Unreleased]\n\n## [0.1.0] - 2026-11-02\n\n## [0.0.9] - 2026-10-01\n\n- Older.\n",
    )?;
    let error = release.fail(&["check", "--tag", "v0.1.0"])?;
    assert!(error.contains("is empty"), "{error}");
    release.write("CHANGELOG.md", "## [0.1.0] - 2026-11-02\n\n- First beta.\n")?;
    assert!(release.fail(&["check"])?.contains("'## [Unreleased]'"));

    release.write("CHANGELOG.md", RELEASED)?;
    release.write("Cargo.lock", &lock("0.0.9"))?;
    let error = release.fail(&["check"])?;
    assert!(
        error.contains("Cargo.lock records maven-mcp 0.0.9"),
        "{error}"
    );

    release.write("Cargo.lock", &lock("0.1"))?;
    release.write("Cargo.toml", &manifest("0.1"))?;
    assert!(release.fail(&["check"])?.contains("not a SemVer version"));
    Ok(())
}

#[test]
fn check_rejects_a_private_or_incomplete_package_manifest() -> Result<()> {
    let release = ReleaseTree::new()?;
    release.write_package_list(&format!(
        "{PACKAGE_LIST}.env\ntarget/promptfoo/results.json\n"
    ))?;
    let error = release.fail(&["check"])?;
    assert!(
        error.contains(".env must not be published (credential file)"),
        "{error}"
    );
    assert!(
        error.contains("target/promptfoo/results.json must not be published"),
        "{error}"
    );

    release.write_package_list(&PACKAGE_LIST.replace("CHANGELOG.md\n", ""))?;
    let error = release.fail(&["check"])?;
    assert!(
        error.contains("the crates.io package misses CHANGELOG.md"),
        "{error}"
    );
    Ok(())
}

#[test]
fn release_notes_are_the_dated_changelog_section() -> Result<()> {
    let release = ReleaseTree::new()?;
    release.write("CHANGELOG.md", RELEASED)?;
    assert_eq!(
        release.succeed(&["notes", "0.1.0"])?,
        "### Added\n\n- First beta.\n- Second line.\n"
    );
    assert_eq!(release.succeed(&["notes", "0.0.9"])?, "- Older.\n");
    assert!(
        release
            .fail(&["notes", "0.2.0"])?
            .contains("no non-empty section")
    );
    assert!(
        release
            .fail(&["notes", "Unreleased"])?
            .contains("not a SemVer")
    );
    Ok(())
}

#[test]
fn archive_is_reproducible_exact_and_verified() -> Result<()> {
    let release = ReleaseTree::new()?;
    let commit = release.git(&["rev-parse", "HEAD"])?;
    let first = TempDir::new()?;
    let second = TempDir::new()?;

    let output = release.succeed(&["archive", TARGET, path(first.path())])?;
    assert!(
        output.contains(&format!("maven-mcp 0.1.0 (commit {commit})")),
        "{output}"
    );
    release.succeed(&["archive", TARGET, path(second.path())])?;

    let archive = first.path().join(ARCHIVE);
    assert_eq!(
        std::fs::read(&archive)?,
        std::fs::read(second.path().join(ARCHIVE))?,
        "two builds of one commit must give identical archives"
    );
    let checksum = std::fs::read_to_string(first.path().join(format!("{ARCHIVE}.sha256")))?;
    assert!(checksum.ends_with(&format!("  {ARCHIVE}\n")), "{checksum}");
    let prefix = "maven-mcp-0.1.0-x86_64-unknown-linux-gnu";
    assert_eq!(
        archive_entries(&archive)?,
        [
            format!("{prefix}/"),
            format!("{prefix}/CHANGELOG.md"),
            format!("{prefix}/LICENSE"),
            format!("{prefix}/README.md"),
            format!("{prefix}/maven-mcp"),
        ]
    );

    let build_env = std::fs::read_to_string(release.build_env_path())?;
    let epoch = release.git(&["log", "-1", "--format=%ct", "HEAD"])?;
    assert!(build_env.contains(&format!("SOURCE_DATE_EPOCH={epoch}\n")));
    assert!(build_env.contains(&format!(
        "--remap-path-prefix={}=/maven-mcp",
        release.root().canonicalize()?.display()
    )));
    assert!(build_env.contains("build --release --locked --bin maven-mcp"));

    let verified = release.succeed(&["verify-archive", path(&archive)])?;
    assert!(
        verified.contains(&format!("Verified {ARCHIVE}")),
        "{verified}"
    );
    Ok(())
}

#[test]
fn archive_requires_a_clean_native_build_of_the_package_version() -> Result<()> {
    let release = ReleaseTree::new()?;
    let output = TempDir::new()?;
    let directory = path(output.path());

    let error = release.fail(&["archive", "x86_64-apple-darwin", directory])?;
    assert!(error.contains("unsupported target"), "{error}");
    let error = release.fail(&["archive", SUPPORTED_TARGETS[1], directory])?;
    assert!(error.contains("built natively"), "{error}");
    let error = release.fail_with(
        &["archive", TARGET, directory],
        &[("FAKE_REPORTED_VERSION", "9.9.9")],
    )?;
    assert!(error.contains("does not report maven-mcp 0.1.0"), "{error}");

    release.write("README.md", "# Changed\n")?;
    let error = release.fail(&["archive", TARGET, directory])?;
    assert!(error.contains("uncommitted changes"), "{error}");
    assert!(!output.path().join(ARCHIVE).exists());
    Ok(())
}

/// Replaces `archive` with a repacked copy that also contains `extra`, with a matching
/// checksum file, so only the entry list check can reject it.
fn repack_with_extra_entry(archive: &Path, prefix: &str, extra: &str) -> Result<()> {
    let unpacked = TempDir::new()?;
    let status = Command::new("tar")
        .args([
            "--extract",
            "--gzip",
            "--file",
            path(archive),
            "--directory",
        ])
        .arg(unpacked.path())
        .status()?;
    assert!(status.success());
    std::fs::write(unpacked.path().join(prefix).join(extra), "TOKEN=fixture\n")?;
    let status = Command::new("tar")
        .args(["--create", "--gzip", "--file", path(archive), "--directory"])
        .arg(unpacked.path())
        .arg(prefix)
        .status()?;
    assert!(status.success());
    let directory = archive.parent().context("archive directory")?;
    let status = Command::new("sh")
        .args(["-c", &format!("sha256sum {ARCHIVE} > {ARCHIVE}.sha256")])
        .current_dir(directory)
        .status()?;
    assert!(status.success());
    Ok(())
}

#[test]
fn verify_archive_rejects_tampering_extra_entries_and_wrong_names() -> Result<()> {
    let release = ReleaseTree::new()?;
    let output = TempDir::new()?;
    release.succeed(&["archive", TARGET, path(output.path())])?;
    let archive = output.path().join(ARCHIVE);

    let original = std::fs::read(&archive)?;
    let mut tampered = original.clone();
    let last = tampered.len() - 1;
    tampered[last] ^= 0xff;
    std::fs::write(&archive, &tampered)?;
    let error = release.fail(&["verify-archive", path(&archive)])?;
    assert!(error.contains("does not match its SHA-256"), "{error}");
    std::fs::write(&archive, &original)?;

    repack_with_extra_entry(&archive, "maven-mcp-0.1.0-x86_64-unknown-linux-gnu", ".env")?;
    let error = release.fail(&["verify-archive", path(&archive)])?;
    assert!(
        error.contains(".env must not be published (credential file)"),
        "{error}"
    );
    std::fs::write(&archive, &original)?;
    repack_with_extra_entry(
        &archive,
        "maven-mcp-0.1.0-x86_64-unknown-linux-gnu",
        "NOTES",
    )?;
    let error = release.fail(&["verify-archive", path(&archive)])?;
    assert!(error.contains("must contain exactly"), "{error}");

    let renamed = output
        .path()
        .join("maven-mcp-0.1.0-x86_64-apple-darwin.tar.gz");
    std::fs::copy(&archive, &renamed)?;
    let error = release.fail(&["verify-archive", path(&renamed)])?;
    assert!(error.contains("is not named"), "{error}");
    std::fs::remove_file(output.path().join(format!("{ARCHIVE}.sha256")))?;
    let error = release.fail(&["verify-archive", path(&archive)])?;
    assert!(error.contains(".sha256 is missing"), "{error}");
    Ok(())
}

#[test]
fn unknown_commands_and_arguments_print_usage() -> Result<()> {
    let release = ReleaseTree::new()?;
    for arguments in [
        &[][..],
        &["publish"],
        &["check", "--tag"],
        &["check", "--version", "v0.1.0"],
        &["notes"],
        &["archive", TARGET],
        &["verify-archive"],
        &["check-manifest", "extra"],
    ] {
        let output = release.run(arguments, &[])?;
        assert_eq!(output.status.code(), Some(2), "{arguments:?}");
        assert!(String::from_utf8(output.stderr)?.contains("Usage:"));
    }
    Ok(())
}

fn workflow(name: &str) -> Result<Yaml> {
    let file = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(".github/workflows")
        .join(name);
    let text = std::fs::read_to_string(&file).with_context(|| format!("cannot read {name}"))?;
    Ok(serde_yaml_ng::from_str(&text)?)
}

fn run_commands(job: &Yaml) -> String {
    job["steps"]
        .as_sequence()
        .into_iter()
        .flatten()
        .filter_map(|step| step["run"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn release_workflow_gates_builds_and_publishes_the_selected_channels() -> Result<()> {
    let ci = workflow("ci.yml")?;
    assert!(
        ci["on"].get("workflow_call").is_some(),
        "the release must be able to run the complete CI"
    );

    let release = workflow("release.yml")?;
    let trigger = &release["on"];
    assert_eq!(
        trigger["push"]["tags"].as_sequence().map(Vec::len),
        Some(1),
        "only version tags trigger a release"
    );
    assert!(trigger["push"].get("branches").is_none());
    assert!(trigger.get("pull_request").is_none());
    assert_eq!(release["permissions"]["contents"].as_str(), Some("read"));

    let jobs = &release["jobs"];
    assert_eq!(
        jobs["ci"]["uses"].as_str(),
        Some("./.github/workflows/ci.yml")
    );
    assert_eq!(jobs["prepare"]["needs"].as_str(), Some("ci"));
    let prepare = run_commands(&jobs["prepare"]);
    for command in [
        "git merge-base --is-ancestor",
        "scripts/release.sh check --tag",
        "cargo publish --dry-run --locked",
        "scripts/release.sh notes",
        "gh release create",
        "--draft",
        "--prerelease",
    ] {
        assert!(prepare.contains(command), "prepare misses {command}");
    }

    let artifacts = &jobs["artifacts"];
    assert_eq!(artifacts["needs"].as_str(), Some("prepare"));
    let targets = artifacts["strategy"]["matrix"]["include"]
        .as_sequence()
        .context("artifact matrix")?
        .iter()
        .filter_map(|entry| entry["target"].as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(targets, BTreeSet::from(SUPPORTED_TARGETS));
    let build = run_commands(artifacts);
    assert!(build.contains("scripts/release.sh archive"));
    assert!(build.contains("gh release upload") && build.contains(".tar.gz.sha256"));

    let publish = &jobs["publish"];
    assert_eq!(publish["needs"].as_str(), Some("artifacts"));
    assert_eq!(publish["environment"].as_str(), Some("release"));
    let commands = run_commands(publish);
    assert!(commands.contains("cargo publish --locked"));
    assert!(commands.contains("gh release edit") && commands.contains("--draft=false"));
    Ok(())
}

#[test]
fn readme_documents_the_beta_channels_versioning_upgrade_and_rollback() -> Result<()> {
    let readme = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md"))?;
    for statement in [
        "## Releases",
        "### Versioning and compatibility",
        "### Upgrade and rollback",
        "GitHub Releases",
        "crates.io",
        "beta",
        "not an OCI image",
        "scripts/release.sh verify-archive",
        "sha256sum --check",
        "cargo install maven-mcp --version",
        "CHANGELOG.md",
        "### Maturity, support and deprecation",
        "**Breaking:**",
        "`Deprecated:`",
        "only the latest release is supported",
    ] {
        assert!(readme.contains(statement), "README.md misses {statement:?}");
    }
    for target in SUPPORTED_TARGETS {
        assert!(readme.contains(&format!("maven-mcp-<version>-{target}.tar.gz")));
    }
    Ok(())
}
