//! Opt-in smoke test against a real Apache Maven; run it through
//! `scripts/real-maven-smoke.sh`, which pins Maven, checks the JDK, and prepares
//! the plugin repository. It is ignored by the deterministic gate.

mod support;

use std::{
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use rmcp::{
    RoleClient, ServiceExt,
    model::CallToolRequestParams,
    service::RunningService,
    transport::{ConfigureCommandExt, TokioChildProcess},
};
use serde_json::{Value, json};
use tempfile::TempDir;

const CAPTURED_TREE: &str = include_str!("fixtures/dependency_tree.txt");
const MAVEN_TIMEOUT_SECONDS: &str = "120";
const IGNORED_MAVEN_ENVIRONMENT: [&str; 3] = ["MAVEN_ARGS", "MAVEN_OPTS", "MAVEN_CONFIG"];

/// The dependency nodes of the captured tree, written by hand from the Maven output
/// and the fixture POMs below rather than produced by the parser under test.
const EXPECTED_TREE: [(&str, &str, u64); 4] = [
    ("org.libs:helper:2.0", "compile", 1),
    ("org.example:lib:1.2.3", "compile", 1),
    ("org.libs:transitive:2.0", "runtime", 2),
    ("org.libs:native:3.0:linux", "runtime", 1),
];

struct SmokeEnvironment {
    maven: PathBuf,
    plugin_repository: PathBuf,
    settings: PathBuf,
}

impl SmokeEnvironment {
    fn from_env() -> Result<Self> {
        let path = |name: &str| -> Result<PathBuf> {
            let value = std::env::var_os(name)
                .map(PathBuf::from)
                .with_context(|| format!("{name} is not set; run scripts/real-maven-smoke.sh"))?;
            ensure!(value.is_absolute(), "{name} must be absolute");
            Ok(value)
        };
        Ok(Self {
            maven: path("MAVEN_MCP_SMOKE_MAVEN_HOME")?.join("bin/mvn"),
            plugin_repository: path("MAVEN_MCP_SMOKE_PLUGIN_REPOSITORY")?,
            settings: path("MAVEN_MCP_SMOKE_SETTINGS")?,
        })
    }
}

/// A trusted project and a private local repository: a copy of the prepared plugin
/// repository plus the synthetic artifacts of `tests/fixtures/dependency_tree.txt`.
struct SmokeProject {
    _root: TempDir,
    trusted_directory: PathBuf,
    project: PathBuf,
    repository: PathBuf,
}

impl SmokeProject {
    fn new(environment: &SmokeEnvironment) -> Result<Self> {
        let root = TempDir::new()?;
        let trusted_directory = root.path().join("trusted");
        let project = trusted_directory.join("fixture-project");
        let repository = root.path().join("repository");
        copy_tree(&environment.plugin_repository, &repository)?;
        write_artifacts(&repository)?;
        std::fs::create_dir_all(project.join(".mvn"))?;
        std::fs::write(project.join("pom.xml"), PROJECT_POM)?;
        // Maven 3.9 reads one argument per line; the server's Maven runs must not
        // fall back to the user's settings.
        std::fs::write(
            project.join(".mvn/maven.config"),
            format!("-s\n{}\n", environment.settings.display()),
        )?;
        Ok(Self {
            _root: root,
            trusted_directory,
            project,
            repository,
        })
    }

    fn project_path(&self) -> String {
        self.project.display().to_string()
    }
}

const PROJECT_POM: &str = r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>org.example</groupId>
  <artifactId>fixture-project</artifactId>
  <version>1.0</version>
  <dependencies>
    <dependency><groupId>org.libs</groupId><artifactId>helper</artifactId><version>2.0</version></dependency>
    <dependency><groupId>org.example</groupId><artifactId>lib</artifactId><version>1.2.3</version></dependency>
    <dependency><groupId>org.libs</groupId><artifactId>native</artifactId><version>3.0</version><classifier>linux</classifier><scope>runtime</scope></dependency>
  </dependencies>
  <build>
    <pluginManagement>
      <plugins>
        <plugin><groupId>org.apache.maven.plugins</groupId><artifactId>maven-dependency-plugin</artifactId><version>3.8.1</version></plugin>
        <plugin><groupId>org.apache.maven.plugins</groupId><artifactId>maven-help-plugin</artifactId><version>3.5.1</version></plugin>
      </plugins>
    </pluginManagement>
  </build>
</project>
"#;

/// Writes the POM and a JAR of `classes` for one artifact; `jar_classifier` names the JAR
/// `<id>-<version>-<classifier>.jar`.
fn write_artifact(
    repository: &Path,
    coordinate: (&str, &str, &str),
    jar_classifier: Option<&str>,
    dependencies: &str,
    classes: &[&str],
) -> Result<()> {
    let (group, id, version) = coordinate;
    let directory = repository
        .join(group.replace('.', "/"))
        .join(id)
        .join(version);
    std::fs::create_dir_all(&directory)?;
    std::fs::write(
        directory.join(format!("{id}-{version}.pom")),
        format!(
            "<project xmlns=\"http://maven.apache.org/POM/4.0.0\"><modelVersion>4.0.0</modelVersion><groupId>{group}</groupId><artifactId>{id}</artifactId><version>{version}</version><dependencies>{dependencies}</dependencies></project>\n"
        ),
    )?;
    let jar_name = jar_classifier.map_or_else(
        || format!("{id}-{version}.jar"),
        |classifier| format!("{id}-{version}-{classifier}.jar"),
    );
    let classes = classes
        .iter()
        .map(|class| (*class).to_owned())
        .collect::<Vec<_>>();
    support::write_class_jar(&directory.join(jar_name), &classes)
}

fn write_artifacts(repository: &Path) -> Result<()> {
    write_artifact(
        repository,
        ("org.libs", "helper", "2.0"),
        None,
        "",
        &["org/example/Foo"],
    )?;
    write_artifact(
        repository,
        ("org.libs", "transitive", "2.0"),
        None,
        "",
        &["org/example/Transitive"],
    )?;
    write_artifact(
        repository,
        ("org.example", "lib", "1.2.3"),
        None,
        "<dependency><groupId>org.libs</groupId><artifactId>transitive</artifactId><version>2.0</version><scope>runtime</scope></dependency>",
        &["org/example/Lib"],
    )?;
    write_artifact(
        repository,
        ("org.libs", "native", "3.0"),
        Some("linux"),
        "",
        &["org/example/NativeBinding"],
    )
}

fn copy_tree(source: &Path, target: &Path) -> Result<()> {
    std::fs::create_dir_all(target)?;
    let entries =
        std::fs::read_dir(source).with_context(|| format!("cannot read {}", source.display()))?;
    for entry in entries {
        let entry = entry?;
        let destination = target.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &destination)?;
        } else {
            std::fs::copy(entry.path(), destination)?;
        }
    }
    Ok(())
}

/// Drops the lines whose content depends on the time of the run. Maven ends an empty
/// `[INFO] ` line with a space that the versioned fixture does not keep.
fn without_timing(output: &str) -> Vec<&str> {
    output
        .lines()
        .map(str::trim_end)
        .filter(|line| {
            !line.starts_with("[INFO] Total time:") && !line.starts_with("[INFO] Finished at:")
        })
        .collect()
}

async fn connect(
    environment: &SmokeEnvironment,
    project: &SmokeProject,
) -> Result<RunningService<RoleClient, ()>> {
    let command =
        tokio::process::Command::new(env!("CARGO_BIN_EXE_maven-mcp")).configure(|command| {
            command
                .env(
                    "MAVEN_TRUSTED_PROJECT_DIRECTORIES",
                    &project.trusted_directory,
                )
                .env("MAVEN_EXECUTION_REPO_PATH", &project.repository)
                .env("MAVEN_EXECUTABLE", &environment.maven)
                .env("MAVEN_TIMEOUT_SECONDS", MAVEN_TIMEOUT_SECONDS)
                .env("MAVEN_SKIP_RC", "1")
                .env("RUST_LOG", "maven_mcp=warn");
            for name in IGNORED_MAVEN_ENVIRONMENT {
                command.env_remove(name);
            }
        });
    tokio::time::timeout(
        Duration::from_secs(60),
        ().serve(TokioChildProcess::new(command)?),
    )
    .await
    .context("STDIO MCP server startup timed out")?
    .context("STDIO MCP client initialization failed")
}

async fn call(
    client: &RunningService<RoleClient, ()>,
    tool: &str,
    arguments: Value,
) -> Result<Value> {
    let arguments = arguments
        .as_object()
        .context("tool arguments must be an object")?
        .clone();
    let result = client
        .call_tool(CallToolRequestParams::new(tool.to_owned()).with_arguments(arguments))
        .await?;
    if result.is_error != Some(false) {
        bail!("{tool} failed: {result:?}");
    }
    result
        .structured_content
        .with_context(|| format!("{tool} returned no structured content"))
}

#[test]
#[ignore = "needs a pinned real Maven; run scripts/real-maven-smoke.sh"]
fn real_maven_reproduces_the_captured_dependency_tree_fixture() -> Result<()> {
    let environment = SmokeEnvironment::from_env()?;
    let project = SmokeProject::new(&environment)?;
    let mut command = Command::new(&environment.maven);
    command
        .current_dir(&project.project)
        .arg("-s")
        .arg(&environment.settings)
        .args(["-B", "-ntp", "--offline"])
        .arg(format!(
            "-Dmaven.repo.local={}",
            project.repository.display()
        ))
        .args([
            "org.apache.maven.plugins:maven-dependency-plugin:3.8.1:tree",
            "-DoutputType=text",
        ])
        .env("MAVEN_SKIP_RC", "1");
    for name in IGNORED_MAVEN_ENVIRONMENT {
        command.env_remove(name);
    }
    let output = command.output()?;
    let stdout = String::from_utf8(output.stdout)?;
    ensure!(output.status.success(), "real Maven failed:\n{stdout}");
    assert_eq!(without_timing(&stdout), without_timing(CAPTURED_TREE));
    Ok(())
}

#[tokio::test]
#[ignore = "needs a pinned real Maven; run scripts/real-maven-smoke.sh"]
async fn real_maven_drives_the_project_tools_through_stdio() -> Result<()> {
    let environment = SmokeEnvironment::from_env()?;
    let project = SmokeProject::new(&environment)?;
    let client = connect(&environment, &project).await?;
    let project_path = project.project_path();

    let inspected = call(
        &client,
        "inspect_maven_project",
        json!({ "project_path": project_path }),
    )
    .await?;
    assert_eq!(inspected["artifact_id"], "fixture-project");
    assert_eq!(inspected["wrapper"], false);

    let tree = call(
        &client,
        "get_dependency_tree",
        json!({ "project_path": project_path }),
    )
    .await?;
    assert_eq!(tree["build"]["outcome"], "success", "{tree:#}");
    let expected_tree = EXPECTED_TREE
        .iter()
        .map(|(coordinate, scope, depth)| {
            json!({ "coordinate": coordinate, "scope": scope, "depth": depth })
        })
        .collect::<Vec<_>>();
    assert_eq!(tree["dependencies"], json!(expected_tree));

    let classpath = call(
        &client,
        "get_maven_classpath",
        json!({ "project_path": project_path, "kind": "test" }),
    )
    .await?;
    let mut artifacts = classpath["artifacts"]
        .as_array()
        .context("classpath artifacts")?
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    artifacts.sort_unstable();
    let mut expected_artifacts = EXPECTED_TREE.map(|(coordinate, _, _)| coordinate);
    expected_artifacts.sort_unstable();
    assert_eq!(artifacts, expected_artifacts, "{classpath:#}");

    for (query, class_name, coordinate) in [
        ("Foo", "org.example.Foo", "org.libs:helper:2.0"),
        (
            "Transitive",
            "org.example.Transitive",
            "org.libs:transitive:2.0",
        ),
        (
            "NativeBinding",
            "org.example.NativeBinding",
            "org.libs:native:3.0:linux",
        ),
    ] {
        let found = call(
            &client,
            "search_classes",
            json!({ "project_path": project_path, "query": query }),
        )
        .await?;
        assert_eq!(found["results"][0]["class_name"], class_name, "{found:#}");
        assert_eq!(found["results"][0]["jar"]["coordinate"], coordinate);
    }

    let effective = call(
        &client,
        "get_effective_pom",
        json!({ "project_path": project_path }),
    )
    .await?;
    assert_eq!(effective["status"], "available", "{effective:#}");
    assert_eq!(
        effective["projects"][0]["coordinate"]["artifact_id"],
        "fixture-project"
    );

    client.cancel().await?;
    Ok(())
}
