//! Versioned local installation (`scripts/local-install.sh`): identified installs,
//! immutable versions, and deterministic candidate promotion and rollback. Fake
//! binaries report a build identity through `--version`; no server is started. The
//! `candidate` flow runs from a copy of the script in a temporary Git repository with a
//! stub gate and a stub `cargo` on `PATH`.

use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use anyhow::Result;
use tempfile::TempDir;

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/local-install.sh");
const FIRST_COMMIT: &str = "1111111111111111111111111111111111111111";
const SECOND_COMMIT: &str = "2222222222222222222222222222222222222222";
const FIRST_ID: &str = "0.1.0+111111111111";
const SECOND_ID: &str = "0.1.0+222222222222";

struct Installation {
    root: TempDir,
    builds: TempDir,
}

impl Installation {
    fn new() -> Result<Self> {
        Ok(Self {
            root: TempDir::new()?,
            builds: TempDir::new()?,
        })
    }

    fn run(&self, arguments: &[&str]) -> Result<Output> {
        Ok(Command::new(SCRIPT)
            .args(arguments)
            .env("MAVEN_MCP_INSTALL_ROOT", self.root.path())
            .output()?)
    }

    fn succeed(&self, arguments: &[&str]) -> Result<String> {
        let output = self.run(arguments)?;
        assert!(
            output.status.success(),
            "{arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8(output.stdout)?)
    }

    fn fail(&self, arguments: &[&str]) -> Result<String> {
        let output = self.run(arguments)?;
        assert!(
            !output.status.success(),
            "{arguments:?} unexpectedly succeeded"
        );
        Ok(String::from_utf8(output.stderr)?)
    }

    /// Writes a fake binary script with the given body.
    fn script(&self, name: &str, body: &str) -> Result<PathBuf> {
        let path = self.builds.path().join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n"))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
        Ok(path)
    }

    /// Writes a fake binary whose `--version` prints `version_line`.
    fn build(&self, name: &str, version_line: &str) -> Result<PathBuf> {
        self.script(name, &format!("echo '{version_line}'"))
    }

    fn identified_build(&self, commit: &str) -> Result<PathBuf> {
        self.build(commit, &identified_version_line(commit))
    }

    fn alias(&self, name: &str) -> Option<String> {
        std::fs::read_link(self.root.path().join(name))
            .ok()
            .map(|target| target.display().to_string())
    }

    fn version_dir(&self, id: &str) -> PathBuf {
        self.root.path().join("versions").join(id)
    }
}

fn identified_version_line(commit: &str) -> String {
    format!("maven-mcp 0.1.0 (commit {commit})")
}

fn version_target(id: &str) -> Option<String> {
    Some(format!("versions/{id}"))
}

fn path(binary: &Path) -> &str {
    binary.to_str().expect("temporary paths are UTF-8")
}

#[test]
fn identified_build_is_installed_as_an_immutable_versioned_candidate() -> Result<()> {
    let installation = Installation::new()?;
    let build = installation.identified_build(FIRST_COMMIT)?;

    let output = installation.succeed(&["install", "--unverified", path(&build)])?;

    assert!(
        output.contains(&format!("candidate -> {FIRST_ID}")),
        "{output}"
    );
    assert_eq!(installation.alias("candidate"), version_target(FIRST_ID));
    let directory = installation.version_dir(FIRST_ID);
    assert_eq!(
        std::fs::read(directory.join("maven-mcp"))?,
        std::fs::read(&build)?
    );
    let build_info = std::fs::read_to_string(directory.join("BUILD-INFO"))?;
    for line in [
        format!("id={FIRST_ID}"),
        "version=0.1.0".to_owned(),
        format!("commit={FIRST_COMMIT}"),
        "verified=false".to_owned(),
    ] {
        assert!(build_info.lines().any(|actual| actual == line), "{line}");
    }
    let reported = Command::new(installation.root.path().join("candidate/maven-mcp"))
        .arg("--version")
        .output()?;
    assert_eq!(
        String::from_utf8(reported.stdout)?.trim(),
        identified_version_line(FIRST_COMMIT)
    );

    // Reinstalling identical content is idempotent; different content under the same
    // identity is rejected and leaves the installed copy untouched.
    let output = installation.succeed(&["install", "--unverified", path(&build)])?;
    assert!(output.contains("already installed"), "{output}");
    let different = installation.script(
        "different",
        &format!(
            "# different content\necho '{}'",
            identified_version_line(FIRST_COMMIT)
        ),
    )?;
    let error = installation.fail(&["install", "--unverified", path(&different)])?;
    assert!(error.contains("immutable"), "{error}");
    assert_eq!(
        std::fs::read(directory.join("maven-mcp"))?,
        std::fs::read(&build)?
    );
    Ok(())
}

#[test]
fn unidentified_builds_are_rejected() -> Result<()> {
    let installation = Installation::new()?;
    for (name, version_line) in [
        ("unknown", "maven-mcp 0.1.0 (commit unknown)"),
        ("short", "maven-mcp 0.1.0 (commit 1111111)"),
        ("plain", "maven-mcp 0.1.0"),
    ] {
        let build = installation.build(name, version_line)?;
        let error = installation.fail(&["install", "--unverified", path(&build)])?;
        assert!(error.contains("not a commit-identified build"), "{error}");
    }
    let missing = installation.builds.path().join("missing");
    let error = installation.fail(&["install", "--unverified", path(&missing)])?;
    assert!(error.contains("not an executable file"), "{error}");
    assert!(installation.alias("candidate").is_none());
    Ok(())
}

#[test]
fn promotion_and_rollback_are_deterministic() -> Result<()> {
    let installation = Installation::new()?;
    assert!(installation.fail(&["promote"])?.contains("no candidate"));
    assert!(installation.fail(&["rollback"])?.contains("no previous"));

    let first = installation.identified_build(FIRST_COMMIT)?;
    installation.succeed(&["install", "--unverified", path(&first)])?;
    installation.succeed(&["promote"])?;
    assert_eq!(installation.alias("stable"), version_target(FIRST_ID));
    assert!(installation.alias("previous").is_none());
    assert!(installation.succeed(&["promote"])?.contains("already"));

    let second = installation.identified_build(SECOND_COMMIT)?;
    installation.succeed(&["install", "--unverified", path(&second)])?;
    // A new candidate never changes stable until it is promoted.
    assert_eq!(installation.alias("stable"), version_target(FIRST_ID));
    installation.succeed(&["promote"])?;
    let promoted = (installation.alias("stable"), installation.alias("previous"));
    assert_eq!(
        promoted,
        (version_target(SECOND_ID), version_target(FIRST_ID))
    );

    installation.succeed(&["rollback"])?;
    assert_eq!(
        (installation.alias("stable"), installation.alias("previous")),
        (version_target(FIRST_ID), version_target(SECOND_ID))
    );
    installation.succeed(&["rollback"])?;
    assert_eq!(
        (installation.alias("stable"), installation.alias("previous")),
        promoted
    );

    installation.succeed(&["promote", FIRST_ID])?;
    assert_eq!(installation.alias("stable"), version_target(FIRST_ID));

    let status = installation.succeed(&["status"])?;
    for line in [
        format!("candidate: {SECOND_ID}"),
        format!("stable: {FIRST_ID}"),
        format!("previous: {SECOND_ID}"),
        format!("  {FIRST_ID} commit={FIRST_COMMIT} verified=false"),
        format!("  {SECOND_ID} commit={SECOND_COMMIT} verified=false"),
    ] {
        assert!(status.contains(&line), "{line} missing from {status}");
    }
    Ok(())
}

#[test]
fn promotion_refuses_unknown_or_modified_versions() -> Result<()> {
    let installation = Installation::new()?;
    let build = installation.identified_build(FIRST_COMMIT)?;
    installation.succeed(&["install", "--unverified", path(&build)])?;

    for unknown in ["0.1.0+000000000000", "../versions", ".hidden"] {
        let error = installation.fail(&["promote", unknown])?;
        assert!(error.contains("not installed"), "{unknown}: {error}");
    }

    std::fs::write(
        installation.version_dir(FIRST_ID).join("maven-mcp"),
        format!(
            "#!/bin/sh\necho '{}' # modified\n",
            identified_version_line(FIRST_COMMIT)
        ),
    )?;
    let error = installation.fail(&["promote"])?;
    assert!(error.contains("SHA-256"), "{error}");
    assert!(installation.alias("stable").is_none());
    Ok(())
}

#[test]
fn install_root_inside_the_source_worktree_is_rejected() -> Result<()> {
    let installation = Installation::new()?;
    let build = installation.identified_build(FIRST_COMMIT)?;
    let inside = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/local-install-rejected");
    let output = Command::new(SCRIPT)
        .args(["install", "--unverified", path(&build)])
        .env("MAVEN_MCP_INSTALL_ROOT", &inside)
        .output()?;
    assert!(
        !inside.exists(),
        "nothing may be created inside the worktree"
    );
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)?.contains("outside the source worktree"));
    Ok(())
}

#[test]
fn relative_install_root_and_unknown_commands_are_rejected() -> Result<()> {
    let output = Command::new(SCRIPT)
        .arg("status")
        .env("MAVEN_MCP_INSTALL_ROOT", "relative/root")
        .output()?;
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)?.contains("absolute"));

    let installation = Installation::new()?;
    let build = installation.identified_build(FIRST_COMMIT)?;
    for arguments in [
        &["start"][..],
        &[],
        &["promote", "a", "b"],
        &["install", path(&build)],
        &["install", "--verified", path(&build)],
    ] {
        let output = installation.run(arguments)?;
        assert_eq!(output.status.code(), Some(2), "{arguments:?}");
        assert!(String::from_utf8(output.stderr)?.contains("Usage:"));
    }
    assert!(installation.alias("candidate").is_none());
    Ok(())
}

/// A committed source tree holding a copy of the script, a passing stub gate, and a
/// stub `cargo` that writes a fake binary into a separate output directory and reports
/// it through Cargo's JSON artifact messages, like a redirected `CARGO_TARGET_DIR`.
struct SourceTree {
    tree: TempDir,
    tools: TempDir,
    cargo_output: TempDir,
    installation: Installation,
}

impl SourceTree {
    fn new() -> Result<Self> {
        let source = Self {
            tree: TempDir::new()?,
            tools: TempDir::new()?,
            cargo_output: TempDir::new()?,
            installation: Installation::new()?,
        };
        let root = source.tree.path();
        std::fs::create_dir_all(root.join("scripts"))?;
        std::fs::copy(SCRIPT, root.join("scripts/local-install.sh"))?;
        write_executable(&root.join("scripts/test-pyramid.sh"), "exit 0")?;
        std::fs::write(root.join(".gitignore"), "/target/\n")?;
        write_executable(
            &source.tools.path().join("cargo"),
            &format!(
                r#"binary='{output}/maven-mcp'
printf '#!/bin/sh\necho "maven-mcp 0.1.0 (commit %s)"\n' "${{FAKE_REPORTED_COMMIT:-$MAVEN_MCP_BUILD_COMMIT}}" >"$binary"
chmod 0755 "$binary"
echo '{{"reason":"compiler-artifact","target":{{"kind":["custom-build"],"name":"build-script-build"}},"executable":null}}'
echo '{{"reason":"compiler-artifact","target":{{"kind":["lib"],"name":"maven_mcp"}},"executable":null}}'
echo "{{\"reason\":\"compiler-artifact\",\"target\":{{\"kind\":[\"bin\"],\"name\":\"maven-mcp\"}},\"executable\":\"$binary\"}}"
echo '{{"reason":"build-finished","success":true}}'"#,
                output = source.cargo_output.path().display()
            ),
        )?;
        source.git(&["init", "--quiet"])?;
        source.git(&["add", "."])?;
        source.git(&["commit", "--quiet", "-m", "fixture"])?;
        Ok(source)
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
            .current_dir(self.tree.path())
            .output()?;
        assert!(output.status.success(), "git {arguments:?} failed");
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    }

    fn candidate(&self, reported_commit: Option<&str>) -> Result<Output> {
        let path = format!(
            "{}:{}",
            self.tools.path().display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut command = Command::new(self.tree.path().join("scripts/local-install.sh"));
        command
            .arg("candidate")
            .env("PATH", path)
            .env("MAVEN_MCP_INSTALL_ROOT", self.installation.root.path())
            .env_remove("FAKE_REPORTED_COMMIT");
        if let Some(commit) = reported_commit {
            command.env("FAKE_REPORTED_COMMIT", commit);
        }
        Ok(command.output()?)
    }
}

fn write_executable(path: &Path, body: &str) -> Result<()> {
    std::fs::write(path, format!("#!/bin/sh\n{body}\n"))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

#[test]
fn candidate_installs_the_binary_cargo_built_instead_of_a_stale_default_path() -> Result<()> {
    let source = SourceTree::new()?;
    let head = source.git(&["rev-parse", "HEAD"])?;
    let id = format!("0.1.0+{}", &head[..12]);
    // An older identified build left at the conventional path must not be installed.
    let stale = source.tree.path().join("target/release/maven-mcp");
    std::fs::create_dir_all(stale.parent().expect("stale binary has a parent"))?;
    write_executable(
        &stale,
        &format!("echo '{}'", identified_version_line(FIRST_COMMIT)),
    )?;

    let output = source.candidate(None)?;

    assert!(
        output.status.success(),
        "candidate failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let installation = &source.installation;
    assert_eq!(installation.alias("candidate"), version_target(&id));
    assert!(!installation.version_dir(FIRST_ID).exists());
    assert_eq!(
        std::fs::read(installation.version_dir(&id).join("maven-mcp"))?,
        std::fs::read(source.cargo_output.path().join("maven-mcp"))?
    );
    let build_info = std::fs::read_to_string(installation.version_dir(&id).join("BUILD-INFO"))?;
    for line in [format!("commit={head}"), "verified=true".to_owned()] {
        assert!(build_info.lines().any(|actual| actual == line), "{line}");
    }
    Ok(())
}

#[test]
fn candidate_rejects_a_built_binary_reporting_another_commit() -> Result<()> {
    let source = SourceTree::new()?;

    let output = source.candidate(Some(SECOND_COMMIT))?;

    assert!(!output.status.success(), "candidate unexpectedly succeeded");
    let error = String::from_utf8(output.stderr)?;
    assert!(error.contains("not the verified commit"), "{error}");
    assert!(source.installation.alias("candidate").is_none());
    assert!(!source.installation.version_dir(SECOND_ID).exists());
    Ok(())
}
