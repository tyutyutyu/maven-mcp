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
  *" help:effective-pom "*) printf '%s' '<project><groupId>org.example</groupId><artifactId>fixture-project</artifactId><version>1.0</version><packaging>jar</packaging></project>' > "$output" ;;
  *" dependency:tree "*) printf '%s\n' '[INFO] org.example:fixture-project:jar:1.0' '[INFO] \- org.libs:helper:jar:compile:2.0' ;;
  *" dependency:build-classpath "*)
    classpath="$PWD/execution-repository/org/libs/helper/2.0/helper-2.0.jar"
    if [ -f .repository-inspection-fixture ]; then
      classpath="$classpath:$PWD/execution-repository/org/libs/api-fixture/1.0/api-fixture-1.0.jar:$PWD/execution-repository/org/libs/api-fixture/2.0/api-fixture-2.0.jar:$PWD/execution-repository/org/libs/resource-fixture/1.0/resource-fixture-1.0.jar"
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

fn minimal_class(class_name: &str) -> Vec<u8> {
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
    push_utf8(&mut bytes, "java/lang/Object");
    push_class(&mut bytes, 3);
    push_utf8(&mut bytes, "value");
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
