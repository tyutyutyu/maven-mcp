use std::{
    fs::File, io::Write, os::unix::fs::PermissionsExt, path::Path, sync::Arc, time::Duration,
};

use anyhow::{Context, Result};
use axum::Router;
use maven_mcp::{
    config::ProjectExecutionConfig, index::MavenIndex, project::MavenRunner, server::MavenMcpServer,
};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use tempfile::TempDir;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zip::{ZipWriter, write::SimpleFileOptions};

pub struct TestServer {
    _repository: TempDir,
    _project: Option<TempDir>,
    endpoint: String,
    cancellation: CancellationToken,
    task: JoinHandle<()>,
}

impl TestServer {
    pub async fn start() -> Result<Self> {
        let repository = fixture_repository()?;
        let index = Arc::new(MavenIndex::build(repository.path(), 50, 1024)?);
        let handler = MavenMcpServer::new(index);
        Self::start_handler(repository, None, handler).await
    }

    #[allow(dead_code)]
    pub async fn start_with_project() -> Result<Self> {
        let repository = fixture_repository()?;
        let project = fixture_project()?;
        let execution_repository = project.path().join("execution-repository");
        std::fs::create_dir(&execution_repository)?;
        let runner = MavenRunner::discover(&ProjectExecutionConfig {
            project_root: project.path().to_owned(),
            maven_executable: None,
            execution_repository: Some(execution_repository),
            timeout: Duration::from_secs(2),
            max_output_bytes: 16_384,
            max_results: 50,
            network_enabled: false,
        })?;
        let index = Arc::new(MavenIndex::build(repository.path(), 50, 1024)?);
        let handler = MavenMcpServer::with_runner(index, Some(Arc::new(runner)));
        Self::start_handler(repository, Some(project), handler).await
    }

    async fn start_handler(
        repository: TempDir,
        project: Option<TempDir>,
        handler: MavenMcpServer,
    ) -> Result<Self> {
        let cancellation = CancellationToken::new();
        let service = StreamableHttpService::new(
            move || Ok(handler.clone()),
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default()
                .with_json_response(true)
                .with_cancellation_token(cancellation.child_token()),
        );
        let router = Router::new().nest_service("/mcp", service);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server_cancellation = cancellation.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(server_cancellation.cancelled_owned())
                .await
                .expect("test HTTP server should run");
        });

        Ok(Self {
            _repository: repository,
            _project: project,
            endpoint: format!("http://{address}/mcp"),
            cancellation,
            task,
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub async fn stop(self) -> Result<()> {
        self.cancellation.cancel();
        self.task.await.context("test server task panicked")?;
        Ok(())
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
