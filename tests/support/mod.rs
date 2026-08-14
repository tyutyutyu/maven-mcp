#![allow(dead_code)]

use std::{fs::File, io::Write, os::unix::fs::PermissionsExt, path::Path, time::Duration};

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
    project: Option<TempDir>,
}

impl TestServer {
    pub async fn start() -> Result<Self> {
        Ok(Self {
            repository: fixture_repository()?,
            project: None,
        })
    }

    #[allow(dead_code)]
    pub async fn start_with_project() -> Result<Self> {
        let repository = fixture_repository()?;
        let project = fixture_project()?;
        let execution_repository = project.path().join("execution-repository");
        std::fs::create_dir(&execution_repository)?;
        Ok(Self {
            repository,
            project: Some(project),
        })
    }

    pub async fn connect(&self) -> Result<RunningService<RoleClient, ()>> {
        self.connect_with_pid().await.map(|(client, _)| client)
    }

    pub async fn connect_with_pid(&self) -> Result<(RunningService<RoleClient, ()>, u32)> {
        let command =
            tokio::process::Command::new(env!("CARGO_BIN_EXE_maven-mcp")).configure(|command| {
                command
                    .env("MAVEN_REPO_PATH", self.repository.path())
                    .env("MAX_RESULTS", "50")
                    .env("MAX_SOURCE_BYTES", "1024")
                    .env("RUST_LOG", "maven_mcp=warn");
                if let Some(project) = &self.project {
                    command
                        .env("MAVEN_PROJECT_ROOT", project.path())
                        .env(
                            "MAVEN_EXECUTION_REPO_PATH",
                            project.path().join("execution-repository"),
                        )
                        .env("MAVEN_TIMEOUT_SECONDS", "2")
                        .env("MAX_MAVEN_OUTPUT_BYTES", "16384");
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
        self.project.as_ref().map(TempDir::path)
    }
}

#[allow(dead_code)]
fn fixture_project() -> Result<TempDir> {
    let root = TempDir::new()?;
    std::fs::write(
        root.path().join("pom.xml"),
        "<project><modelVersion>4.0.0</modelVersion><groupId>org.example</groupId><artifactId>fixture-project</artifactId><version>1.0</version><packaging>jar</packaging></project>",
    )?;
    std::fs::create_dir_all(root.path().join(".mvn/wrapper"))?;
    std::fs::write(
        root.path().join(".mvn/wrapper/maven-wrapper.properties"),
        "distributionUrl=https://example.invalid/maven.zip",
    )?;
    let wrapper = root.path().join("mvnw");
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
  *" dependency:build-classpath "*) printf '%s\n' 'Dependencies classpath:' "$PWD/execution-repository/org/libs/helper/2.0/helper-2.0.jar" ;;
  *) printf '%s\n' '[INFO] BUILD SUCCESS' ;;
esac
"#,
    )?;
    let mut permissions = wrapper.metadata()?.permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&wrapper, permissions)?;
    let test = root.path().join("src/test/java/org/example/FooTest.java");
    std::fs::create_dir_all(test.parent().context("test source parent")?)?;
    std::fs::write(test, "package org.example; class FooTest {}")?;
    let report = root.path().join("target/site/jacoco/jacoco.xml");
    std::fs::create_dir_all(report.parent().context("coverage report parent")?)?;
    std::fs::write(
        report,
        r#"<report><package name="org/example"><class name="org/example/Foo"><counter type="LINE" missed="1" covered="9"/></class></package><counter type="LINE" missed="1" covered="9"/></report>"#,
    )?;
    Ok(root)
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
