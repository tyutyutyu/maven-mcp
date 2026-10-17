#![allow(dead_code)]

use std::{
    fs::File,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use rmcp::{
    RoleClient, ServiceExt,
    service::RunningService,
    transport::{ConfigureCommandExt, TokioChildProcess},
};
use tempfile::TempDir;
use zip::{ZipWriter, write::SimpleFileOptions};

pub struct TestServer {
    repository: TempDir,
    trusted_project_directory: Option<TempDir>,
    project_path: Option<PathBuf>,
    jenv_root: Option<TempDir>,
    inherited_java_home: Option<TempDir>,
}

impl TestServer {
    pub async fn start() -> Result<Self> {
        Ok(Self {
            repository: fixture_repository()?,
            trusted_project_directory: None,
            project_path: None,
            jenv_root: None,
            inherited_java_home: None,
        })
    }

    #[allow(dead_code)]
    pub async fn start_with_project() -> Result<Self> {
        Self::start_with_project_fixture(false).await
    }

    pub async fn start_with_repository_inspection_project() -> Result<Self> {
        Self::start_with_project_fixture(true).await
    }

    async fn start_with_project_fixture(include_inspection_fixtures: bool) -> Result<Self> {
        let repository = fixture_repository()?;
        let trusted_project_directory = TempDir::new()?;
        let project_path = trusted_project_directory
            .path()
            .join("nested/projects/fixture-project");
        fixture_project(&project_path, include_inspection_fixtures)?;
        let execution_repository = project_path.join("execution-repository");
        std::fs::create_dir(&execution_repository)?;
        fixture_execution_repository(&execution_repository, include_inspection_fixtures)?;
        Ok(Self {
            repository,
            trusted_project_directory: Some(trusted_project_directory),
            project_path: Some(project_path),
            jenv_root: None,
            inherited_java_home: None,
        })
    }

    /// Adds JARs whose classfile variants differ by Java version: `multi-release:1.0`
    /// (flagged, base plus 11 and 17), `multi-release:2.0` (base only) and
    /// `plain-release:1.0` (same layout as 1.0 but without the manifest flag).
    pub async fn start_with_multi_release_project() -> Result<Self> {
        let server = Self::start_with_project().await?;
        let project_path = server
            .project_path
            .as_ref()
            .context("test server has no project")?;
        std::fs::write(project_path.join(".multi-release-fixture"), "enabled\n")?;
        fixture_multi_release_jars(&project_path.join("execution-repository"))?;
        Ok(server)
    }

    pub async fn start_with_jenv_project() -> Result<Self> {
        let mut server = Self::start_with_project().await?;
        server.jenv_root = Some(fixture_jenv_root()?);
        let project_path = server
            .project_path
            .as_ref()
            .context("test server has no project")?
            .clone();
        server.set_jenv_version(&project_path, "one")?;
        Ok(server)
    }

    pub async fn start_with_inherited_java_project() -> Result<Self> {
        let mut server = Self::start_with_project().await?;
        let java_home = TempDir::new()?;
        let project_path = server
            .project_path
            .as_ref()
            .context("test server has no project")?;
        write_java_asserting_wrapper(project_path, java_home.path())?;
        server.inherited_java_home = Some(java_home);
        Ok(server)
    }

    pub async fn connect(&self) -> Result<RunningService<RoleClient, ()>> {
        self.connect_with_pid().await.map(|(client, _)| client)
    }

    pub async fn connect_with_pid(&self) -> Result<(RunningService<RoleClient, ()>, u32)> {
        self.connect_with_limits(&[]).await
    }

    pub async fn connect_with_limits(
        &self,
        limits: &[(&str, &str)],
    ) -> Result<(RunningService<RoleClient, ()>, u32)> {
        let command =
            tokio::process::Command::new(env!("CARGO_BIN_EXE_maven-mcp")).configure(|command| {
                command
                    .env("MAX_RESULTS", "50")
                    .env("MAX_SOURCE_BYTES", "1024")
                    .env("RUST_LOG", "maven_mcp=warn");
                if let (Some(trusted_directory), Some(project_path)) =
                    (&self.trusted_project_directory, &self.project_path)
                {
                    command
                        .env(
                            "MAVEN_TRUSTED_PROJECT_DIRECTORIES",
                            trusted_directory.path(),
                        )
                        .env(
                            "MAVEN_EXECUTION_REPO_PATH",
                            project_path.join("execution-repository"),
                        )
                        .env("MAVEN_TIMEOUT_SECONDS", "2")
                        .env("MAX_MAVEN_OUTPUT_BYTES", "16384");
                }
                for (name, value) in limits {
                    command.env(name, value);
                }
                if let Some(jenv_root) = &self.jenv_root {
                    command
                        .arg("--jenv")
                        .env("JENV_ROOT", jenv_root.path())
                        .env("JENV_VERSION", "must-not-override-project")
                        .env("JENV_DIR", "/must-not-override-project")
                        .env_remove("JAVA_HOME")
                        .env("PATH", "/usr/bin:/bin");
                }
                if let Some(java_home) = &self.inherited_java_home {
                    command.env("JAVA_HOME", java_home.path()).env(
                        "PATH",
                        format!("{}/bin:/usr/bin:/bin", java_home.path().display()),
                    );
                }
            });
        let transport = TokioChildProcess::new(command)?;
        let process_id = transport.id().context("missing STDIO server process id")?;
        let client = tokio::time::timeout(Duration::from_secs(300), ().serve(transport))
            .await
            .context("STDIO MCP server startup timed out")?
            .context("STDIO MCP client initialization failed")?;
        Ok((client, process_id))
    }

    #[allow(dead_code)]
    pub fn repository_path(&self) -> &Path {
        self.repository.path()
    }

    /// Replaces the source entries of the project-scoped `org.libs:helper:2.0` artifact.
    #[allow(dead_code)]
    pub fn replace_helper_sources(&self, entries: &[(&str, &[u8])]) -> Result<()> {
        let project_path = self
            .project_path
            .as_deref()
            .context("a project fixture is required")?;
        write_jar(
            &project_path.join("execution-repository/org/libs/helper/2.0/helper-2.0-sources.jar"),
            entries,
        )
    }

    pub fn project_path(&self) -> Option<&Path> {
        self.project_path.as_deref()
    }

    pub fn trusted_project_directory(&self) -> Option<&Path> {
        self.trusted_project_directory.as_ref().map(TempDir::path)
    }

    pub fn add_trusted_project(&self, name: &str) -> Result<PathBuf> {
        let trusted_directory = self
            .trusted_project_directory
            .as_ref()
            .context("test server has no trusted project directory")?;
        let project_path = trusted_directory.path().join("additional").join(name);
        fixture_project(&project_path, false)?;
        std::fs::create_dir(project_path.join("execution-repository"))?;
        fixture_execution_repository(&project_path.join("execution-repository"), false)?;
        Ok(project_path)
    }

    pub fn set_jenv_version(&self, project_path: &Path, version: &str) -> Result<()> {
        let jenv_root = self
            .jenv_root
            .as_ref()
            .context("test server has no jenv root")?;
        std::fs::write(project_path.join(".java-version"), format!("{version}\n"))?;
        write_java_asserting_wrapper(
            project_path,
            &jenv_root.path().join("versions").join(version),
        )
    }
}

fn fixture_jenv_root() -> Result<TempDir> {
    let root = TempDir::new()?;
    for version in ["one", "two"] {
        write_executable(
            &root.path().join("versions").join(version).join("bin/java"),
            "#!/bin/sh\nexit 0\n",
        )?;
    }
    write_executable(
        &root.path().join("bin/jenv"),
        r#"#!/bin/sh
[ -z "${JENV_VERSION:-}" ] || exit 21
[ -z "${JENV_DIR:-}" ] || exit 22
version=$(sed -n '1p' .java-version)
prefix="$JENV_ROOT/versions/$version"
[ -d "$prefix" ] || exit 23
printf '%s\n' "$prefix"
"#,
    )?;
    Ok(root)
}

fn write_java_asserting_wrapper(project_path: &Path, java_home: &Path) -> Result<()> {
    write_executable(
        &project_path.join("mvnw"),
        &format!(
            "#!/bin/sh\n[ \"$JAVA_HOME\" = '{}' ] || exit 31\ncase \"$PATH\" in \"$JAVA_HOME/bin:\"*) printf '%s\\n' '[INFO] BUILD SUCCESS' ;; *) exit 32 ;; esac\n",
            java_home.display()
        ),
    )
}

fn write_executable(path: &Path, contents: &str) -> Result<()> {
    std::fs::create_dir_all(path.parent().context("executable parent")?)?;
    std::fs::write(path, contents)?;
    let mut permissions = path.metadata()?.permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions)?;
    Ok(())
}

/// Holds the first Maven child until the test has issued its competing request.
/// Poll intervals only observe markers; elapsed time never releases the child.
pub struct MavenReportGate {
    directory: PathBuf,
}

impl MavenReportGate {
    pub fn new(project: &Path, body: &str) -> Result<Self> {
        let directory = project.join(".report-gate");
        std::fs::create_dir(&directory)?;
        write_executable(
            &project.join("mvnw"),
            &format!(
                "#!/bin/sh\nset -eu\ntouch .report-gate/started\nwhile [ ! -e .report-gate/release ]; do sleep 0.01; done\n{body}\n"
            ),
        )?;
        Ok(Self { directory })
    }

    pub async fn wait_started(&self) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !self.directory.join("started").exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .context("report-producing Maven child did not reach the marker")
    }

    pub fn release(&self) -> Result<()> {
        std::fs::write(self.directory.join("release"), "release")?;
        Ok(())
    }
}

#[allow(dead_code)]
fn fixture_project(root: &Path, include_inspection_fixtures: bool) -> Result<()> {
    std::fs::create_dir_all(root)?;
    std::fs::write(
        root.join("pom.xml"),
        "<project><modelVersion>4.0.0</modelVersion><groupId>org.example</groupId><artifactId>fixture-project</artifactId><version>1.0</version><packaging>jar</packaging></project>",
    )?;
    std::fs::create_dir_all(root.join(".mvn/wrapper"))?;
    std::fs::write(
        root.join(".mvn/wrapper/maven-wrapper.properties"),
        "distributionUrl=https://example.invalid/maven.zip",
    )?;
    // Helper-only projection of tests/fixtures/dependency_tree.txt (other branches pruned).
    // Captured using Apache Maven 3.9.9, maven-dependency-plugin 3.8.1:
    // mvn -s settings.xml -B -ntp -Dmaven.repo.local=<fixture-repository>
    //   org.apache.maven.plugins:maven-dependency-plugin:3.8.1:tree -DoutputType=text
    let wrapper = root.join("mvnw");
    std::fs::write(
        &wrapper,
        r#"#!/bin/sh
for arg in "$@"; do
  case "$arg" in
    -Doutput=*) output=${arg#-Doutput=} ;;
    -Dtest=*)
      mkdir -p target/surefire-reports
      printf '%s' '<testsuite tests="1" failures="0" errors="0" skipped="0"><testcase classname="org.example.FooTest" name="passes"/></testsuite>' > target/surefire-reports/TEST-org.example.FooTest.xml
      ;;
  esac
done
case " $* " in
  *" help:effective-pom "*)
    if [ -f .effective-pom-fixture.xml ]; then
      cat .effective-pom-fixture.xml > "$output"
    else
      printf '%s' '<project><groupId>org.example</groupId><artifactId>fixture-project</artifactId><version>1.0</version><packaging>jar</packaging></project>' > "$output"
    fi
    ;;
  *" dependency:tree "*) printf '%s\n' '[INFO] --- dependency:3.8.1:tree (default-cli) @ fixture-project ---' '[INFO] org.example:fixture-project:jar:1.0' '[INFO] \- org.libs:helper:jar:2.0:compile' ;;
  *" dependency:build-classpath "*)
    classpath="$PWD/execution-repository/org/libs/helper/2.0/helper-2.0.jar"
    if [ -f .repository-inspection-fixture ]; then
      classpath="$classpath:$PWD/execution-repository/org/libs/api-fixture/1.0/api-fixture-1.0.jar:$PWD/execution-repository/org/libs/api-fixture/2.0/api-fixture-2.0.jar:$PWD/execution-repository/org/libs/resource-fixture/1.0/resource-fixture-1.0.jar"
    fi
    if [ -f .multi-release-fixture ]; then
      for jar in multi-release/1.0/multi-release-1.0 multi-release/2.0/multi-release-2.0 plain-release/1.0/plain-release-1.0; do
        classpath="$classpath:$PWD/execution-repository/org/libs/$jar.jar"
      done
    fi
    printf '%s\n' 'Dependencies classpath:' "$classpath"
    ;;
  *) printf '%s\n' '[INFO] BUILD SUCCESS' ;;
esac
"#,
    )?;
    if include_inspection_fixtures {
        std::fs::write(root.join(".repository-inspection-fixture"), "enabled\n")?;
    }
    let mut permissions = wrapper.metadata()?.permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&wrapper, permissions)?;
    let test = root.join("src/test/java/org/example/FooTest.java");
    std::fs::create_dir_all(test.parent().context("test source parent")?)?;
    std::fs::write(test, "package org.example; class FooTest {}")?;
    let report = root.join("target/site/jacoco/jacoco.xml");
    std::fs::create_dir_all(report.parent().context("coverage report parent")?)?;
    std::fs::write(
        report,
        r#"<report><package name="org/example"><class name="org/example/Foo"><counter type="LINE" missed="1" covered="9"/></class></package><counter type="LINE" missed="1" covered="9"/></report>"#,
    )?;
    Ok(())
}

fn fixture_repository() -> Result<TempDir> {
    let root = TempDir::new()?;
    let version_one = root.path().join("org/example/demo/1.0");
    let version_two = root.path().join("org/example/demo/2.0");
    let foo_v1 = minimal_class("org/example/Foo");
    let bar = minimal_class("org/example/Bar");
    write_jar(
        &version_one.join("demo-1.0.jar"),
        &[
            ("org/example/Foo.class", &foo_v1),
            ("org/example/Bar.class", &bar),
            ("META-INF/services/example.Service", b"org.example.Foo"),
            (
                "META-INF/spring/org.example.AutoConfiguration.imports",
                b"org.example.Foo",
            ),
            (
                "META-INF/spring.factories",
                b"example.Factory=org.example.Bar",
            ),
            ("native/image.bin", &[0, 159, 146, 150]),
        ],
    )?;
    write_jar(
        &version_one.join("demo-1.0-sources.jar"),
        &[
            (
                "org/example/Foo.java",
                b"package org.example; public class Foo {}",
            ),
            (
                "org/example/Bar.java",
                b"package org.example; public class Bar {}",
            ),
        ],
    )?;
    std::fs::write(
        version_one.join("demo-1.0.pom"),
        r#"<project>
            <modelVersion>4.0.0</modelVersion>
            <groupId>org.example</groupId><artifactId>demo</artifactId><version>1.0</version>
            <properties><java.version>21</java.version></properties>
            <dependencies><dependency>
                <groupId>org.example</groupId><artifactId>helper</artifactId>
                <version>${helper.version}</version><scope>runtime</scope>
            </dependency></dependencies>
        </project>"#,
    )?;
    std::fs::write(version_one.join("demo-1.0.jar.sha1"), "fixture-checksum")?;
    std::fs::write(
        version_one.join("_remote.repositories"),
        "demo-1.0.jar>central=\n",
    )?;
    let foo_v2 = minimal_class("org/example/Foo");
    let inspectable = minimal_class("org/example/Inspectable");
    write_jar(
        &version_two.join("demo-2.0.jar"),
        &[
            ("org/example/Foo.class", &foo_v2),
            ("org/example/Inspectable.class", &inspectable),
        ],
    )?;
    write_jar(
        &version_two.join("demo-2.0-sources.jar"),
        &[(
            "org/example/Foo.java",
            b"package org.example; public class Foo { int version = 2; }",
        )],
    )?;
    Ok(root)
}

fn fixture_execution_repository(root: &Path, include_inspection_fixtures: bool) -> Result<()> {
    let version = root.join("org/libs/helper/2.0");
    let fixture_class = minimal_class("org/example/Foo");
    let scoped_only = minimal_class("org/libs/ScopedOnly");
    write_jar(
        &version.join("helper-2.0.jar"),
        &[
            ("org/example/Foo.class", &fixture_class),
            ("org/libs/ScopedOnly.class", &scoped_only),
        ],
    )?;
    write_jar(
        &version.join("helper-2.0-sources.jar"),
        &[(
            "org/example/Foo.java",
            b"package org.example; public class Foo { int scoped = 2; }",
        )],
    )?;
    std::fs::write(
        version.join("helper-2.0.pom"),
        r#"<project>
            <modelVersion>4.0.0</modelVersion>
            <groupId>org.libs</groupId><artifactId>helper</artifactId><version>2.0</version>
        </project>"#,
    )?;
    if !include_inspection_fixtures {
        return Ok(());
    }

    let resource_version = root.join("org/libs/resource-fixture/1.0");
    write_jar(
        &resource_version.join("resource-fixture-1.0.jar"),
        &[
            (
                "META-INF/MANIFEST.MF",
                b"Manifest-Version: 1.0\nImplementation-Title: resource-fixture\n",
            ),
            (
                "META-INF/services/org.example.Service",
                b"org.example.Provider\n",
            ),
            (
                "config/inspection.properties",
                b"feature.provider=org.example.Provider\n",
            ),
        ],
    )?;
    std::fs::write(
        resource_version.join("resource-fixture-1.0.pom"),
        "<project><modelVersion>4.0.0</modelVersion><groupId>org.libs</groupId><artifactId>resource-fixture</artifactId><version>1.0</version><packaging>jar</packaging></project>",
    )?;
    std::fs::write(
        resource_version.join("resource-fixture-1.0.jar.sha1"),
        "fixture-checksum",
    )?;
    std::fs::write(
        resource_version.join("resource-fixture-1.0.jar.lastUpdated"),
        "lookup=fixture\n",
    )?;
    std::fs::write(
        resource_version.join("_remote.repositories"),
        "resource-fixture-1.0.jar>central=\n",
    )?;

    for (artifact_version, class_path, class_name) in [
        ("1.0", "org/example/api/LegacyApi.class", "LegacyApi"),
        ("2.0", "org/example/api/CurrentApi.class", "CurrentApi"),
    ] {
        let artifact_directory = root.join(format!("org/libs/api-fixture/{artifact_version}"));
        let class_file = minimal_class(&format!("org/example/api/{class_name}"));
        write_jar(
            &artifact_directory.join(format!("api-fixture-{artifact_version}.jar")),
            &[(class_path, &class_file)],
        )?;
        std::fs::write(
            artifact_directory.join(format!("api-fixture-{artifact_version}.pom")),
            format!(
                "<project><modelVersion>4.0.0</modelVersion><groupId>org.libs</groupId><artifactId>api-fixture</artifactId><version>{artifact_version}</version><packaging>jar</packaging></project>"
            ),
        )?;
    }
    Ok(())
}

type JarEntries<'a> = Vec<(&'a str, &'a [u8])>;

fn fixture_multi_release_jars(root: &Path) -> Result<()> {
    let shape = |super_class, field| variant_class("org/mr/Shape", super_class, field);
    let base = shape("java/lang/Object", "baseval");
    let java11 = shape("java/lang/Number", "java11x");
    let java17 = shape("java/lang/Number", "java17x");
    let version_only = minimal_class("org/mr/VersionOnly");
    let manifest: (&str, &[u8]) = (
        "META-INF/MANIFEST.MF",
        b"Manifest-Version: 1.0\r\nMulti-Release: true\r\n\r\n",
    );
    let java17_entry = ("META-INF/versions/17/org/mr/Shape.class", java17.as_slice());
    let layouts: [(&str, &str, JarEntries); 3] = [
        (
            "multi-release",
            "1.0",
            vec![
                manifest,
                ("org/mr/Shape.class", &base),
                ("META-INF/versions/11/org/mr/Shape.class", &java11),
                java17_entry,
                (
                    "META-INF/versions/17/org/mr/VersionOnly.class",
                    &version_only,
                ),
            ],
        ),
        ("multi-release", "2.0", vec![("org/mr/Shape.class", &base)]),
        (
            "plain-release",
            "1.0",
            vec![
                ("org/mr/Shape.class", &base),
                ("META-INF/versions/11/org/mr/Shape.class", &java11),
                java17_entry,
                (
                    "META-INF/versions/17/org/mr/VersionOnly.class",
                    &version_only,
                ),
            ],
        ),
    ];
    for (artifact, version, entries) in layouts {
        let directory = root.join(format!("org/libs/{artifact}/{version}"));
        write_jar(
            &directory.join(format!("{artifact}-{version}.jar")),
            &entries,
        )?;
        std::fs::write(
            directory.join(format!("{artifact}-{version}.pom")),
            format!(
                "<project><modelVersion>4.0.0</modelVersion><groupId>org.libs</groupId><artifactId>{artifact}</artifactId><version>{version}</version><packaging>jar</packaging></project>"
            ),
        )?;
    }
    Ok(())
}

fn minimal_class(class_name: &str) -> Vec<u8> {
    variant_class(class_name, "java/lang/Object", "value")
}

/// Public class with one public `int` field; superclass and field name select the variant.
fn variant_class(class_name: &str, super_class: &str, field_name: &str) -> Vec<u8> {
    fn push_u16(bytes: &mut Vec<u8>, value: u16) {
        bytes.extend_from_slice(&value.to_be_bytes());
    }
    fn push_utf8(bytes: &mut Vec<u8>, value: &str) {
        bytes.push(1);
        push_u16(
            bytes,
            u16::try_from(value.len()).expect("test class name should fit in u16"),
        );
        bytes.extend_from_slice(value.as_bytes());
    }
    fn push_class(bytes: &mut Vec<u8>, name_index: u16) {
        bytes.push(7);
        push_u16(bytes, name_index);
    }

    let mut bytes = vec![0xca, 0xfe, 0xba, 0xbe];
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 61);
    push_u16(&mut bytes, 7);
    push_utf8(&mut bytes, class_name);
    push_class(&mut bytes, 1);
    push_utf8(&mut bytes, super_class);
    push_class(&mut bytes, 3);
    push_utf8(&mut bytes, field_name);
    push_utf8(&mut bytes, "I");
    push_u16(&mut bytes, 0x0021);
    push_u16(&mut bytes, 2);
    push_u16(&mut bytes, 4);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 1);
    push_u16(&mut bytes, 0x0001);
    push_u16(&mut bytes, 5);
    push_u16(&mut bytes, 6);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    bytes
}

fn write_jar(path: &Path, entries: &[(&str, &[u8])]) -> Result<()> {
    std::fs::create_dir_all(path.parent().context("JAR path must have a parent")?)?;
    let mut writer = ZipWriter::new(File::create(path)?);
    for (name, content) in entries {
        writer.start_file(*name, SimpleFileOptions::default())?;
        writer.write_all(content)?;
    }
    writer.finish()?;
    Ok(())
}

/// Wrapper whose `dependency:build-classpath` output and timing are driven by marker files in
/// the project: `classpath.txt` (the printed classpath), `fail-classpath` (fail the run), `gate`
/// (block until removed) and `slow` (sleep). Every classpath run appends a line to
/// `classpath-runs` and creates `classpath-started`.
#[allow(dead_code)]
pub const CONTROLLED_WRAPPER: &str = r#"#!/bin/sh
case " $* " in
  *" dependency:build-classpath "*)
    echo run >> "$PWD/classpath-runs"
    if [ -f "$PWD/fail-classpath" ]; then
      printf '%s\n' '[ERROR] classpath resolution failed on purpose' >&2
      exit 1
    fi
    classpath=$(cat "$PWD/classpath.txt")
    : > "$PWD/classpath-started"
    while [ -f "$PWD/gate" ]; do sleep 0.05; done
    if [ -f "$PWD/slow" ]; then sleep 0.5; fi
    printf '%s\n' 'Dependencies classpath:' "$classpath"
    ;;
  *) printf '%s\n' '[INFO] BUILD SUCCESS' ;;
esac
"#;

#[allow(dead_code)]
pub fn install_controlled_wrapper(project: &Path, classpath: &[PathBuf]) -> Result<()> {
    write_executable(&project.join("mvnw"), CONTROLLED_WRAPPER)?;
    set_controlled_classpath(project, classpath)
}

#[allow(dead_code)]
pub fn set_controlled_classpath(project: &Path, classpath: &[PathBuf]) -> Result<()> {
    let classpath = classpath
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(":");
    std::fs::write(project.join("classpath.txt"), classpath)?;
    Ok(())
}

#[allow(dead_code)]
pub fn controlled_classpath_runs(project: &Path) -> usize {
    std::fs::read_to_string(project.join("classpath-runs")).map_or(0, |runs| runs.lines().count())
}

/// Writes a JAR with one public empty class per name (`org/example/Name`).
#[allow(dead_code)]
pub fn write_class_jar(path: &Path, class_names: &[String]) -> Result<()> {
    let classes = class_names
        .iter()
        .map(|name| (format!("{name}.class"), minimal_class(name)))
        .collect::<Vec<_>>();
    let entries = classes
        .iter()
        .map(|(name, bytes)| (name.as_str(), bytes.as_slice()))
        .collect::<Vec<_>>();
    write_jar(path, &entries)
}

/// Writes one public empty class file per name (`org/example/Name`) below a class directory.
#[allow(dead_code)]
pub fn write_class_directory(directory: &Path, class_names: &[String]) -> Result<()> {
    for name in class_names {
        let path = directory.join(format!("{name}.class"));
        std::fs::create_dir_all(path.parent().context("class file has a parent")?)?;
        std::fs::write(path, minimal_class(name))?;
    }
    Ok(())
}

/// A project whose classpath holds `org.slf4j:slf4j-api:2.0.9` followed by an
/// unrelated `org.example:unrelated:1.0` without a sources JAR, next to a control
/// `slf4j-api:9.9` in the same local repository but outside the classpath.
pub struct BenchmarkProject {
    _root: TempDir,
    project_path: PathBuf,
    repository_path: PathBuf,
}

impl BenchmarkProject {
    pub const CONTROL_JAR: &'static str = "slf4j-api-9.9.jar";
    const UNRELATED_JAR: &'static str = "org/example/unrelated/1.0/unrelated-1.0.jar";

    pub fn new() -> Result<Self> {
        let root = TempDir::new()?;
        let base = root.path().canonicalize()?;
        let project_path = base.join("project");
        let repository_path = base.join("repository");
        std::fs::create_dir_all(project_path.join(".mvn/wrapper"))?;
        std::fs::write(
            project_path.join("pom.xml"),
            "<project><modelVersion>4.0.0</modelVersion><groupId>org.example</groupId><artifactId>benchmark-fixture</artifactId><version>1.0</version></project>",
        )?;
        std::fs::write(
            project_path.join(".mvn/wrapper/maven-wrapper.properties"),
            "distributionUrl=https://example.invalid/maven.zip",
        )?;
        let wrapper = project_path.join("mvnw");
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nprintf '%s\\n' 'Dependencies classpath:' '{}:{}'\n",
                repository_path
                    .join("org/slf4j/slf4j-api/2.0.9/slf4j-api-2.0.9.jar")
                    .display(),
                repository_path.join(Self::UNRELATED_JAR).display()
            ),
        )?;
        let mut permissions = wrapper.metadata()?.permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&wrapper, permissions)?;

        let logger = minimal_class("org/slf4j/Logger");
        for (version, source) in [
            ("2.0.9", "package org.slf4j;\npublic interface Logger {}\n"),
            (
                "9.9",
                "package org.slf4j;\npublic interface ControlLogger {}\n",
            ),
        ] {
            let directory = repository_path.join(format!("org/slf4j/slf4j-api/{version}"));
            write_jar(
                &directory.join(format!("slf4j-api-{version}.jar")),
                &[("org/slf4j/Logger.class", &logger)],
            )?;
            write_jar(
                &directory.join(format!("slf4j-api-{version}-sources.jar")),
                &[("org/slf4j/Logger.java", source.as_bytes())],
            )?;
        }
        write_jar(
            &repository_path.join(Self::UNRELATED_JAR),
            &[(
                "org/example/Unrelated.class",
                &minimal_class("org/example/Unrelated"),
            )],
        )?;
        Ok(Self {
            _root: root,
            project_path,
            repository_path,
        })
    }

    pub fn project_path(&self) -> &Path {
        &self.project_path
    }

    pub fn repository_path(&self) -> &Path {
        &self.repository_path
    }

    /// Replaces the fake `mvnw` with one that fails classpath resolution.
    pub fn fail_maven(&self) -> Result<()> {
        write_executable(
            &self.project_path.join("mvnw"),
            "#!/bin/sh\necho 'offline artifact missing' >&2\nexit 3\n",
        )
    }
}

/// Owns the recorded processes even when a regression assertion unwinds.
pub struct PipeHolder {
    project: PathBuf,
}

impl PipeHolder {
    pub fn new(project: &Path, redirections: &str) -> Result<Self> {
        let guard = Self {
            project: project.to_owned(),
        };
        write_executable(
            &project.join("mvnw"),
            &format!(
                "#!/bin/sh\nprintf '%s' \"$$\" > holder-parent.pid\nsleep 30 {redirections} &\nprintf '%s' \"$!\" > holder-child.pid\nexit 0\n"
            ),
        )?;
        Ok(guard)
    }

    pub async fn wait_started(&self) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if self.pid("child").is_some()
                    && self.pid("parent").is_some_and(|pid| !process_is_live(pid))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .context("pipe holder did not start and its parent exit")
    }

    pub async fn assert_stopped(&self) -> Result<()> {
        let pid = self.pid("child").context("missing descendant PID")?;
        tokio::time::timeout(Duration::from_secs(3), async {
            while process_is_live(pid) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .context("pipe-holding descendant survived cleanup")
    }

    fn pid(&self, role: &str) -> Option<i32> {
        std::fs::read_to_string(self.project.join(format!("holder-{role}.pid")))
            .ok()?
            .trim()
            .parse()
            .ok()
    }
}

fn process_is_live(pid: i32) -> bool {
    // An orphan may be a zombie until the host's init reaps it; it cannot run
    // or retain pipe descriptors in that state.
    #[cfg(target_os = "linux")]
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        && stat
            .rsplit_once(") ")
            .is_some_and(|(_, fields)| fields.starts_with('Z'))
    {
        return false;
    }
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}

impl Drop for PipeHolder {
    fn drop(&mut self) {
        for role in ["parent", "child"] {
            if let Some(pid) = self.pid(role) {
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(pid),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
        }
    }
}
