use std::{fs, io::Write};

use tempfile::TempDir;

use super::*;

#[test]
fn report_discovery_enforces_aggregate_file_boundaries() {
    let root = TempDir::new().unwrap();
    let surefire = root.path().join("target/surefire-reports");
    fs::create_dir_all(&surefire).unwrap();
    for i in 0..crate::config::MAX_XML_FILES {
        let directory = root.path().join(format!("target/report-{i}"));
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("jacoco.xml"), "<report/>").unwrap();
        fs::write(surefire.join(format!("TEST-{i}.xml")), "<testsuite/>").unwrap();
    }
    assert_eq!(
        discover_jacoco_reports(root.path()).unwrap().len(),
        crate::config::MAX_XML_FILES
    );
    let paths = discover_report_files(root.path()).unwrap();
    assert_eq!(paths.len(), crate::config::MAX_XML_FILES);
    parse_test_reports(&paths).unwrap();
    let mut too_many = paths;
    too_many.push(too_many[0].clone());
    assert!(
        parse_test_reports(&too_many)
            .unwrap_err()
            .to_string()
            .contains("MAX_XML_FILES")
    );
    fs::write(root.path().join("target/jacoco.xml"), "<report/>").unwrap();
    fs::write(surefire.join("TEST-over.xml"), "<testsuite/>").unwrap();
    assert!(
        discover_jacoco_reports(root.path())
            .unwrap_err()
            .to_string()
            .contains("MAX_XML_FILES")
    );
    assert!(
        discover_report_files(root.path())
            .unwrap_err()
            .to_string()
            .contains("MAX_XML_FILES")
    );
}

#[test]
fn xml_limits_reach_pom_effective_pom_jacoco_and_surefire_parsers() {
    let (root, config) = wrapper_project("#!/bin/sh\nexit 0\n");
    let runner = MavenRunner::discover(&config).unwrap();
    let jacoco = root.path().join("core/target/site/jacoco/jacoco.xml");
    fs::create_dir_all(jacoco.parent().unwrap()).unwrap();
    let effective = root.path().join("effective.xml");
    let surefire = root.path().join("TEST-limit.xml");
    let inputs = [
        (
            " ".repeat(crate::config::DEFAULT_MAX_XML_BYTES + 1),
            "MAX_XML_BYTES",
        ),
        (
            format!(
                "{}{}",
                "<a>".repeat(crate::config::MAX_XML_DEPTH + 1),
                "</a>".repeat(crate::config::MAX_XML_DEPTH + 1)
            ),
            "MAX_XML_DEPTH",
        ),
        (
            format!("<a>{}</a>", "<b/>".repeat(crate::config::MAX_XML_ELEMENTS)),
            "MAX_XML_ELEMENTS",
        ),
    ];
    for (xml, limit) in inputs {
        for path in [&root.path().join("pom.xml"), &effective, &jacoco, &surefire] {
            fs::write(path, &xml).unwrap();
        }
        assert!(format!("{:#}", discover_project(root.path(), true).unwrap_err()).contains(limit));
        assert!(format!("{:#}", parse_effective_pom(&effective).unwrap_err()).contains(limit));
        assert!(format!("{:#}", runner.read_jacoco_reports().err().unwrap()).contains(limit));
        assert!(
            format!(
                "{:#}",
                parse_test_reports(std::slice::from_ref(&surefire)).unwrap_err()
            )
            .contains(limit)
        );
    }
}

fn write_pom(path: &Path, artifact_id: &str, packaging: &str, modules: &[&str]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let modules = modules
        .iter()
        .map(|module| format!("<module>{module}</module>"))
        .collect::<String>();
    fs::write(
            path,
            format!(
                "<project><modelVersion>4.0.0</modelVersion><artifactId>{artifact_id}</artifactId><packaging>{packaging}</packaging><modules>{modules}</modules></project>"
            ),
        )
        .unwrap();
}

fn write_executable(path: &Path, script: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let staged = path.with_extension("tmp");
    let mut file = fs::File::create(&staged).unwrap();
    file.write_all(script.as_bytes()).unwrap();
    file.sync_all().unwrap();
    drop(file);
    let mut permissions = staged.metadata().unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&staged, permissions).unwrap();
    fs::rename(staged, path).unwrap();
}

fn wrapper_project(script: &str) -> (TempDir, ProjectExecutionConfig) {
    let root = TempDir::new().unwrap();
    write_pom(
        &root.path().join("pom.xml"),
        "root",
        "pom",
        &["core", "apps"],
    );
    write_pom(&root.path().join("core/pom.xml"), "core", "jar", &[]);
    write_pom(&root.path().join("apps/pom.xml"), "apps", "pom", &["web"]);
    write_pom(&root.path().join("apps/web/pom.xml"), "web", "war", &[]);
    fs::create_dir_all(root.path().join(".mvn/wrapper")).unwrap();
    fs::write(
        root.path().join(".mvn/wrapper/maven-wrapper.properties"),
        "distributionUrl=https://example.invalid/maven.zip",
    )
    .unwrap();
    write_executable(&root.path().join("mvnw"), script);
    let repository = root.path().join("execution-repository");
    fs::create_dir(&repository).unwrap();
    let config = ProjectExecutionConfig {
        project_root: root.path().to_owned(),
        maven_executable: None,
        execution_repository: Some(repository),
        timeout: Duration::from_secs(2),
        max_output_bytes: 4096,
        max_results: 100,
        network_enabled: false,
        java_environment: JavaEnvironment::Inherit,
    };
    (root, config)
}

#[test]
fn discovers_wrapper_and_nested_reactor_modules() {
    let (_root, config) = wrapper_project("#!/bin/sh\nexit 0\n");
    let runner = MavenRunner::discover(&config).unwrap();

    assert!(runner.project().wrapper);
    assert_eq!(runner.project().artifact_id, "root");
    assert_eq!(runner.project().packaging, "pom");
    assert_eq!(
        runner
            .project()
            .modules
            .iter()
            .map(|module| (module.selector.clone(), module.packaging.clone()))
            .collect::<Vec<_>>(),
        vec![
            ("apps".to_owned(), "pom".to_owned()),
            ("apps/web".to_owned(), "war".to_owned()),
            ("core".to_owned(), "jar".to_owned()),
        ]
    );
}

#[test]
fn selects_an_absolute_system_maven_without_a_wrapper() {
    let root = TempDir::new().unwrap();
    write_pom(&root.path().join("pom.xml"), "single", "jar", &[]);
    let executable_dir = TempDir::new().unwrap();
    let executable = executable_dir.path().join("mvn");
    write_executable(&executable, "#!/bin/sh\nexit 0\n");
    let config = ProjectExecutionConfig {
        project_root: root.path().to_owned(),
        maven_executable: Some(executable),
        execution_repository: None,
        timeout: Duration::from_secs(1),
        max_output_bytes: 1024,
        max_results: 100,
        network_enabled: false,
        java_environment: JavaEnvironment::Inherit,
    };

    let runner = MavenRunner::discover(&config).unwrap();
    assert!(!runner.project().wrapper);
    assert_eq!(runner.project().artifact_id, "single");
}

#[test]
fn rejects_module_paths_outside_the_configured_root() {
    let parent = TempDir::new().unwrap();
    let root = parent.path().join("project");
    let outside = parent.path().join("outside");
    write_pom(&root.join("pom.xml"), "root", "pom", &["../outside"]);
    write_pom(&outside.join("pom.xml"), "outside", "jar", &[]);
    fs::create_dir_all(root.join(".mvn/wrapper")).unwrap();
    fs::write(
        root.join(".mvn/wrapper/maven-wrapper.properties"),
        "distributionUrl=unused",
    )
    .unwrap();
    write_executable(&root.join("mvnw"), "#!/bin/sh\nexit 0\n");
    let config = ProjectExecutionConfig {
        project_root: root,
        maven_executable: None,
        execution_repository: None,
        timeout: Duration::from_secs(1),
        max_output_bytes: 1024,
        max_results: 100,
        network_enabled: false,
        java_environment: JavaEnvironment::Inherit,
    };

    assert!(MavenRunner::discover(&config).is_err());
}

#[tokio::test]
async fn arbitrary_arguments_reject_nul_before_starting_maven() {
    let (root, config) = wrapper_project("#!/bin/sh\ntouch child-started\n");
    let runner = MavenRunner::discover(&config).unwrap();
    let error = runner
        .run_arguments(&["bad\0argument".to_owned()])
        .await
        .unwrap_err();
    assert!(error.to_string().contains("NUL"));
    assert!(!root.path().join("child-started").exists());
}

#[tokio::test]
async fn runs_validated_module_arguments_and_redacts_sensitive_output() {
    let (root, config) = wrapper_project(
        "#!/bin/sh\nprintf '%s\\n' \"$@\"\nprintf 'password=hunter2 root=%s url=https://user:pass@example.invalid/repo\\n' \"$PWD\" >&2\n",
    );
    let runner = MavenRunner::discover(&config).unwrap();
    let result = runner
        .run(&MavenInvocation {
            phase: LifecyclePhase::Compile,
            module: Some("apps/web".to_owned()),
            also_make: true,
        })
        .await;

    assert_eq!(result.status, MavenRunStatus::Success);
    assert!(result.stdout.contains("--offline"));
    assert!(
        result
            .stdout
            .contains("--projects\napps/web\n--also-make\ncompile")
    );
    assert!(
        !result.stdout.contains(
            config
                .execution_repository
                .as_ref()
                .unwrap()
                .to_string_lossy()
                .as_ref()
        )
    );
    assert!(!result.stderr.contains("hunter2"));
    assert!(!result.stderr.contains("user:pass"));
    assert!(
        !result
            .stderr
            .contains(root.path().to_string_lossy().as_ref())
    );
    assert!(result.redaction_count >= 3);
}

#[tokio::test]
async fn applies_jenv_selected_java_only_to_the_maven_child() {
    let jenv_root = TempDir::new().unwrap();
    let java_home = jenv_root.path().join("jdks/selected");
    write_executable(&java_home.join("bin/java"), "#!/bin/sh\nexit 0\n");
    write_executable(
        &jenv_root.path().join("bin/jenv"),
        "#!/bin/sh\nversion=$(sed -n '1p' .java-version)\nprintf '%s\\n' \"$JENV_ROOT/jdks/$version\"\n",
    );
    let wrapper = format!(
        "#!/bin/sh\n[ \"$JAVA_HOME\" = '{}' ] || exit 8\ncase \"$PATH\" in \"$JAVA_HOME/bin:\"*) exit 0 ;; *) exit 9 ;; esac\n",
        java_home.display()
    );
    let (project, mut config) = wrapper_project(&wrapper);
    fs::write(project.path().join(".java-version"), "selected\n").unwrap();
    config.java_environment = JavaEnvironment::Jenv {
        root: jenv_root.path().to_owned(),
    };

    let result = MavenRunner::discover(&config)
        .unwrap()
        .run(&MavenInvocation {
            phase: LifecyclePhase::Compile,
            module: None,
            also_make: false,
        })
        .await;

    assert_eq!(result.status, MavenRunStatus::Success, "{result:#?}");
}

#[tokio::test]
async fn reports_jenv_resolution_failure_without_starting_maven() {
    let jenv_root = TempDir::new().unwrap();
    write_executable(&jenv_root.path().join("bin/jenv"), "#!/bin/sh\nexit 1\n");
    let marker = jenv_root.path().join("maven-started");
    let wrapper = format!("#!/bin/sh\ntouch '{}'\n", marker.display());
    let (project, mut config) = wrapper_project(&wrapper);
    fs::write(project.path().join(".java-version"), "missing\n").unwrap();
    config.java_environment = JavaEnvironment::Jenv {
        root: jenv_root.path().to_owned(),
    };

    let result = MavenRunner::discover(&config)
        .unwrap()
        .run(&MavenInvocation {
            phase: LifecyclePhase::Compile,
            module: None,
            also_make: false,
        })
        .await;

    assert_eq!(result.status, MavenRunStatus::RunnerError);
    assert!(result.stderr.contains("verify .java-version"));
    assert!(!marker.exists());
}

#[tokio::test]
async fn reports_missing_jenv_without_starting_maven() {
    let jenv_root = TempDir::new().unwrap();
    let marker = jenv_root.path().join("maven-started");
    let wrapper = format!("#!/bin/sh\ntouch '{}'\n", marker.display());
    let (_project, mut config) = wrapper_project(&wrapper);
    config.java_environment = JavaEnvironment::Jenv {
        root: jenv_root.path().to_owned(),
    };

    let result = MavenRunner::discover(&config)
        .unwrap()
        .run(&MavenInvocation {
            phase: LifecyclePhase::Compile,
            module: None,
            also_make: false,
        })
        .await;

    assert_eq!(result.status, MavenRunStatus::RunnerError);
    assert!(result.stderr.contains("jenv executable is missing"));
    assert!(!marker.exists());
}

#[tokio::test]
async fn rejects_a_jenv_home_without_executable_java() {
    let jenv_root = TempDir::new().unwrap();
    let invalid_java_home = jenv_root.path().join("invalid-java");
    fs::create_dir(&invalid_java_home).unwrap();
    write_executable(
        &jenv_root.path().join("bin/jenv"),
        &format!(
            "#!/bin/sh\nprintf '%s\\n' '{}'\n",
            invalid_java_home.display()
        ),
    );
    let marker = jenv_root.path().join("maven-started");
    let wrapper = format!("#!/bin/sh\ntouch '{}'\n", marker.display());
    let (_project, mut config) = wrapper_project(&wrapper);
    config.java_environment = JavaEnvironment::Jenv {
        root: jenv_root.path().to_owned(),
    };

    let result = MavenRunner::discover(&config)
        .unwrap()
        .run(&MavenInvocation {
            phase: LifecyclePhase::Compile,
            module: None,
            also_make: false,
        })
        .await;

    assert_eq!(result.status, MavenRunStatus::RunnerError);
    assert!(result.stderr.contains("executable bin/java"));
    assert!(!marker.exists());
}

#[tokio::test]
async fn returns_structured_errors_for_unknown_modules() {
    let (_root, config) = wrapper_project("#!/bin/sh\nexit 0\n");
    let runner = MavenRunner::discover(&config).unwrap();
    let result = runner
        .run(&MavenInvocation {
            phase: LifecyclePhase::Compile,
            module: Some("../outside".to_owned()),
            also_make: false,
        })
        .await;

    assert_eq!(result.status, MavenRunStatus::RunnerError);
    assert!(result.stderr.contains("unknown reactor module"));
}

#[tokio::test]
async fn enforces_timeout_and_output_limits() {
    let (_root, mut timeout_config) = wrapper_project("#!/bin/sh\nsleep 5\n");
    timeout_config.timeout = Duration::from_millis(50);
    let timeout_runner = MavenRunner::discover(&timeout_config).unwrap();
    let timed_out = timeout_runner
        .run(&MavenInvocation {
            phase: LifecyclePhase::Verify,
            module: None,
            also_make: false,
        })
        .await;
    assert_eq!(
        timed_out.status,
        MavenRunStatus::Timeout,
        "unexpected Maven result: {timed_out:#?}"
    );
    assert!(timed_out.timed_out);

    let (_root, mut output_config) = wrapper_project(
        "#!/bin/sh\nprintf 'abcdefghijklmnopqrstuvwxyz'\nprintf 'ABCDEFGHIJKLMNOPQRSTUVWXYZ' >&2\nexit 1\n",
    );
    output_config.max_output_bytes = 8;
    let output_runner = MavenRunner::discover(&output_config).unwrap();
    let limited = output_runner
        .run(&MavenInvocation {
            phase: LifecyclePhase::Compile,
            module: None,
            also_make: false,
        })
        .await;
    assert_eq!(limited.status, MavenRunStatus::BuildFailure);
    assert_eq!(limited.exit_code, Some(1));
    assert_eq!(limited.stdout.len(), 8);
    assert_eq!(limited.stderr.len(), 8);
    assert_eq!(limited.stdout, "stuvwxyz");
    assert_eq!(limited.stderr, "STUVWXYZ");
    assert!(limited.stdout_truncated);
    assert!(limited.stderr_truncated);
}

#[tokio::test]
async fn classifies_compile_and_verify_test_failures_from_real_child_processes() {
    let (_root, compile_config) = wrapper_project(
        "#!/bin/sh\nprintf '%s\n' '[ERROR] COMPILATION ERROR' '[ERROR] src/Foo.java:[1,1] missing symbol' >&2\nexit 1\n",
    );
    let compile = MavenBuildResult::from_run(
        MavenRunner::discover(&compile_config)
            .unwrap()
            .run(&MavenInvocation {
                phase: LifecyclePhase::Compile,
                module: None,
                also_make: false,
            })
            .await,
    );
    assert_eq!(compile.outcome, MavenBuildOutcome::CompilationError);
    assert_eq!(compile.compiler_diagnostics.len(), 2);

    let (_root, verify_config) = wrapper_project(
        "#!/bin/sh\nprintf '%s\n' '[ERROR] There are test failures' 'Tests run: 2, Failures: 1, Errors: 0' >&2\nexit 1\n",
    );
    let verify = MavenBuildResult::from_run(
        MavenRunner::discover(&verify_config)
            .unwrap()
            .run(&MavenInvocation {
                phase: LifecyclePhase::Verify,
                module: Some("core".to_owned()),
                also_make: true,
            })
            .await,
    );
    assert_eq!(verify.outcome, MavenBuildOutcome::TestFailure);
}

#[test]
fn explains_offline_artifact_resolution_without_misclassifying_other_runs() {
    let offline_failure = MavenBuildResult::from_run(MavenRunResult {
            status: MavenRunStatus::BuildFailure,
            exit_code: Some(1),
            duration_ms: 1,
            timed_out: false,
            stdout: "[ERROR] Cannot access central in offline mode and the artifact org.example:demo:jar:1.0 has not been downloaded from it before."
                .to_owned(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            redaction_count: 0,
        });
    let notice = offline_failure
        .policy_notice
        .expect("offline repository resolution should explain the server policy");
    assert!(notice.contains("MAVEN_EXECUTION_NETWORK=true"));
    assert!(notice.contains("intentional security policy"));
    assert!(notice.contains("not necessarily a project error"));

    let unrelated_failure = MavenBuildResult::from_run(MavenRunResult {
        status: MavenRunStatus::BuildFailure,
        exit_code: Some(1),
        duration_ms: 1,
        timed_out: false,
        stdout: "[ERROR] Failed to execute goal: invalid configuration".to_owned(),
        stderr: String::new(),
        stdout_truncated: false,
        stderr_truncated: false,
        redaction_count: 0,
    });
    assert_eq!(unrelated_failure.policy_notice, None);

    let successful_run = MavenBuildResult::from_run(MavenRunResult {
        status: MavenRunStatus::Success,
        exit_code: Some(0),
        duration_ms: 1,
        timed_out: false,
        stdout: "Cannot access central in offline mode".to_owned(),
        stderr: String::new(),
        stdout_truncated: false,
        stderr_truncated: false,
        redaction_count: 0,
    });
    assert_eq!(successful_run.policy_notice, None);
}

#[tokio::test]
async fn discovers_and_runs_focused_tests_with_structured_surefire_results() {
    let report = r#"<testsuite tests="3" failures="1" errors="1" skipped="0">
            <testcase classname="org.example.FooTest" name="passes"/>
            <testcase classname="org.example.FooTest" name="fails"><failure message="expected true"/></testcase>
            <testcase classname="org.example.FooTest" name="errors"><error>boom</error></testcase>
        </testsuite>"#;
    let script = format!(
        "#!/bin/sh\nmkdir -p core/target/surefire-reports\nprintf '%s' '{}' > core/target/surefire-reports/TEST-org.example.FooTest.xml\nprintf 'Tests run: 3, Failures: 1, Errors: 1, Skipped: 0\\n'\nexit 1\n",
        report.replace('\'', "'\\''")
    );
    let (root, config) = wrapper_project(&script);
    let java = root
        .path()
        .join("core/src/test/java/org/example/FooTest.java");
    fs::create_dir_all(java.parent().unwrap()).unwrap();
    fs::write(&java, "package org.example; class FooTest {}").unwrap();
    let kotlin = root
        .path()
        .join("apps/web/src/test/kotlin/org/example/WebTest.kt");
    fs::create_dir_all(kotlin.parent().unwrap()).unwrap();
    fs::write(&kotlin, "package org.example; class WebTest").unwrap();
    let runner = MavenRunner::discover(&config).unwrap();

    assert_eq!(
        runner.test_classes().unwrap(),
        vec!["org.example.FooTest", "org.example.WebTest"]
    );
    let result = runner
        .run_focused_test(&FocusedTestInvocation {
            test_class: "org.example.FooTest".to_owned(),
            test_method: Some("fails".to_owned()),
            module: Some("core".to_owned()),
            also_make: true,
        })
        .await;
    assert_eq!(result.report_status, TestReportStatus::Available);
    assert_eq!(
        result.summary,
        Some(TestSummary {
            passed: 1,
            failed: 1,
            errors: 1,
            skipped: 0,
        })
    );
    assert_eq!(result.failures.len(), 2);
    assert_eq!(result.failures[0].test_name, "errors");
    assert_eq!(result.failures[1].message, "expected true");
    assert_eq!(result.build.outcome, MavenBuildOutcome::TestFailure);

    let last = runner.last_test_failures().await;
    assert!(last.available);
    assert_eq!(last.failures, result.failures);
}

#[tokio::test]
async fn rejects_shell_syntax_in_focused_test_selectors() {
    let (_root, config) = wrapper_project("#!/bin/sh\nexit 0\n");
    let runner = MavenRunner::discover(&config).unwrap();
    let result = runner
        .run_focused_test(&FocusedTestInvocation {
            test_class: "FooTest;touch /tmp/escape".to_owned(),
            test_method: None,
            module: None,
            also_make: false,
        })
        .await;
    assert_eq!(result.build.outcome, MavenBuildOutcome::RunnerError);
    assert_eq!(result.report_status, TestReportStatus::Missing);
}

#[tokio::test]
async fn focused_test_reports_missing_xml_and_compilation_errors_without_fabricating_counts() {
    let (_root, config) = wrapper_project(
        "#!/bin/sh\nprintf '%s\n' '[ERROR] COMPILATION ERROR' '[ERROR] src/Test.java:[2,1] broken' >&2\nexit 1\n",
    );
    let runner = MavenRunner::discover(&config).unwrap();
    let result = runner
        .run_focused_test(&FocusedTestInvocation {
            test_class: "org.example.UnknownTest".to_owned(),
            test_method: Some("missing".to_owned()),
            module: Some("apps/web".to_owned()),
            also_make: true,
        })
        .await;
    assert_eq!(result.report_status, TestReportStatus::Missing);
    assert!(result.summary.is_none());
    assert!(result.failures.is_empty());
    assert_eq!(result.build.outcome, MavenBuildOutcome::CompilationError);
}

#[test]
fn malformed_surefire_xml_is_an_explicit_error() {
    let root = TempDir::new().unwrap();
    let report = root.path().join("TEST-broken.xml");
    fs::write(&report, "<testsuite><broken></testsuite>").unwrap();
    assert!(parse_test_reports(&[report]).is_err());
}

#[tokio::test]
async fn resolves_effective_pom_into_structured_metadata() {
    let effective = r#"<project>
            <groupId>org.example</groupId><artifactId>demo</artifactId><version>1.0</version>
            <packaging>jar</packaging>
            <parent><groupId>org.parent</groupId><artifactId>parent</artifactId><version>2</version></parent>
            <properties><java.version>21</java.version></properties>
            <dependencyManagement><dependencies><dependency><groupId>org.libs</groupId><artifactId>managed</artifactId><version>9</version></dependency></dependencies></dependencyManagement>
            <dependencies><dependency><groupId>org.libs</groupId><artifactId>lib</artifactId><version>3</version><scope>runtime</scope><exclusions><exclusion><groupId>org.bad</groupId><artifactId>legacy</artifactId></exclusion></exclusions></dependency></dependencies>
            <build><plugins><plugin><groupId>org.apache.maven.plugins</groupId><artifactId>maven-compiler-plugin</artifactId><version>4</version></plugin></plugins></build>
        </project>"#;
    let script = format!(
        "#!/bin/sh\nfor arg in \"$@\"; do case \"$arg\" in -Doutput=*) output=${{arg#-Doutput=}} ;; esac; done\nprintf '%s' '{}' > \"$output\"\n",
        effective.replace('\'', "'\\''")
    );
    let (_root, config) = wrapper_project(&script);
    let runner = MavenRunner::discover(&config).unwrap();

    let result = runner.effective_pom(None).await;
    assert_eq!(result.status, DiagnosticStatus::Available);
    assert_eq!(result.projects.len(), 1);
    assert_eq!(result.projects[0].coordinate.artifact_id, "demo");
    assert_eq!(result.projects[0].properties["java.version"], "21");
    assert_eq!(
        result.projects[0].dependencies[0].scope.as_deref(),
        Some("runtime")
    );
    assert_eq!(
        result.projects[0].dependencies[0].exclusions[0].artifact_id,
        "legacy"
    );
    assert_eq!(
        result.projects[0].dependency_management[0].artifact_id,
        "managed"
    );
    assert_eq!(
        result.projects[0].plugins[0].artifact_id,
        "maven-compiler-plugin"
    );
}

#[tokio::test]
async fn parses_dependency_tree_and_normalizes_classpath_coordinates() {
    let tree_script = "#!/bin/sh\nprintf '[INFO] org.example:demo:jar:1.0\\n[INFO] +- org.libs:first:jar:compile:2.0\\n[INFO] \\- org.libs:native:jar:linux:runtime:3.0\\n'\n";
    let (_root, config) = wrapper_project(tree_script);
    let runner = MavenRunner::discover(&config).unwrap();
    let tree = runner
        .dependency_tree(None, Some(DependencyScope::Runtime), Some("org.libs:*"))
        .await;
    assert_eq!(tree.dependencies.len(), 2);
    assert_eq!(tree.dependencies[0].coordinate, "org.libs:first:2.0");
    assert_eq!(tree.dependencies[1].coordinate, "org.libs:native:3.0:linux");
    assert!(!tree.incomplete);

    let repository = config.execution_repository.as_ref().unwrap();
    let classpath_script = format!(
        "#!/bin/sh\nprintf 'Dependencies classpath:\\n\\nDependencies classpath:\\n{}/org/libs/first/2.0/first-2.0.jar:{}/org/libs/native/3.0/native-3.0-linux.jar\\nDependencies classpath:\\n\\n'\n",
        repository.display(),
        repository.display()
    );
    write_executable(&config.project_root.join("mvnw"), &classpath_script);
    let classpath_runner = MavenRunner::discover(&config).unwrap();
    let classpath = classpath_runner
        .build_classpath(None, ClasspathKind::Test)
        .await;
    assert_eq!(
        classpath.artifacts,
        vec!["org.libs:first:2.0", "org.libs:native:3.0:linux"],
        "unexpected Maven result: {:#?}",
        classpath.build.run
    );
    assert!(!classpath.incomplete);
    assert!(
        !classpath
            .build
            .run
            .stdout
            .contains(repository.to_string_lossy().as_ref())
    );
}

#[tokio::test]
async fn dependency_diagnostics_preserve_transitive_depth_and_report_failures_and_truncation() {
    let script = "#!/bin/sh\nprintf '[INFO] +- org.libs:first:jar:compile:1\\n[INFO] |  \\- org.libs:transitive:jar:runtime:2\\n[ERROR] dependency resolution failed with a deliberately long diagnostic line\\n'\nexit 1\n";
    let (_root, mut config) = wrapper_project(script);
    config.max_output_bytes = 110;
    let runner = MavenRunner::discover(&config).unwrap();
    let tree = runner
        .dependency_tree(Some("core"), Some(DependencyScope::Runtime), None)
        .await;
    assert!(
        tree.incomplete,
        "unexpected Maven result: {:#?}",
        tree.build.run
    );
    assert_eq!(tree.build.outcome, MavenBuildOutcome::BuildFailure);
    assert_eq!(tree.dependencies[0].depth, 1);
    assert!(tree.dependencies.iter().any(|node| node.depth >= 2));

    let missing = runner.effective_pom(Some("core")).await;
    assert_eq!(missing.status, DiagnosticStatus::Missing);
    assert_eq!(missing.build.outcome, MavenBuildOutcome::BuildFailure);
}

#[test]
fn explains_conflicts_management_duplicates_exclusions_and_multiple_module_paths() {
    let (_root, config) = wrapper_project("#!/bin/sh\nexit 0\n");
    let project = MavenRunner::discover(&config).unwrap().project().clone();
    let output = r#"
[INFO] --- maven-dependency-plugin:3.8.1:tree (default-cli) @ core ---
[INFO] org.example:core:jar:1.0
[INFO] +- org.direct:lib:jar:3.0:compile
[INFO] |  +- org.shared:thing:jar:2.0:compile
[INFO] |  \- (org.shared:thing:jar:1.0:compile - omitted for conflict with 2.0)
[INFO] +- org.managed:item:jar:5.0:compile (version managed from 4.0)
[INFO] +- org.dupe:item:jar:1.0:compile
[INFO] |  \- (org.dupe:item:jar:1.0:compile - omitted for duplicate)
[INFO] \- org.legacy:old:jar:1.0:compile (excluded by declared exclusion)
[INFO] --- maven-dependency-plugin:3.8.1:tree (default-cli) @ web ---
[INFO] org.example:web:war:1.0
[INFO] \- org.web:bridge:jar:1.0:runtime
[INFO]    \- org.shared:thing:jar:2.0:runtime
"#;

    let parsed = parse_dependency_resolution(output, &project);
    assert!(!parsed.malformed);
    let shared = parsed
        .explanations
        .iter()
        .find(|explanation| explanation.artifact == "org.shared:thing:jar")
        .unwrap();
    assert_eq!(shared.selected_version.as_deref(), Some("2.0"));
    assert_eq!(shared.requested_versions, vec!["1.0", "2.0"]);
    assert_eq!(shared.paths.len(), 3);
    assert_eq!(shared.paths[0].module, "core");
    assert_eq!(shared.paths[0].nodes.len(), 3);
    assert_eq!(shared.paths[2].module, "apps/web");
    assert!(
        shared
            .selection_reasons
            .contains(&DependencySelectionReason::NearestDefinition)
    );
    assert_eq!(
        shared.paths[1].status,
        DependencyPathStatus::ConflictOmitted
    );

    let managed = parsed
        .explanations
        .iter()
        .find(|explanation| explanation.artifact == "org.managed:item:jar")
        .unwrap();
    assert_eq!(managed.selected_version.as_deref(), Some("5.0"));
    assert_eq!(managed.requested_versions, vec!["4.0"]);
    assert!(
        managed
            .selection_reasons
            .contains(&DependencySelectionReason::DependencyManagement)
    );
    let duplicate = parsed
        .explanations
        .iter()
        .find(|explanation| explanation.artifact == "org.dupe:item:jar")
        .unwrap();
    assert!(
        duplicate
            .paths
            .iter()
            .any(|path| path.status == DependencyPathStatus::DuplicateOmitted)
    );
    let excluded = parsed
        .explanations
        .iter()
        .find(|explanation| explanation.artifact == "org.legacy:old:jar")
        .unwrap();
    assert_eq!(excluded.paths[0].status, DependencyPathStatus::Excluded);
    assert!(excluded.paths[0].annotations[0].contains("declared exclusion"));
}

#[tokio::test]
async fn dependency_explanation_uses_owned_verbose_arguments_and_rust_filtering() {
    let script = r#"#!/bin/sh
printf '%s\n' "$@"
printf '%s\n' '[INFO] org.example:core:jar:1.0' '[INFO] +- org.keep:first:jar:2.0:compile' '[INFO] \- org.skip:second:jar:3.0:runtime'
"#;
    let (_root, config) = wrapper_project(script);
    let runner = MavenRunner::discover(&config).unwrap();
    let result = runner
        .explain_dependency_resolution(
            Some("core"),
            Some(DependencyScope::Runtime),
            Some("org.keep:*"),
        )
        .await;

    assert_eq!(
        result.status,
        DependencyResolutionStatus::Available,
        "unexpected Maven result: {:#?}",
        result.build.run
    );
    assert_eq!(result.explanations.len(), 1);
    assert_eq!(result.explanations[0].artifact, "org.keep:first:jar");
    assert!(result.build.run.stdout.contains("-Dverbose"));
    assert!(result.build.run.stdout.contains("-Dscope=runtime"));
    assert!(!result.build.run.stdout.contains("-Dincludes="));
}

#[tokio::test]
async fn dependency_explanation_reports_empty_failure_truncation_and_invalid_output() {
    let (_root, empty_config) = wrapper_project("#!/bin/sh\nexit 0\n");
    let empty = MavenRunner::discover(&empty_config)
        .unwrap()
        .explain_dependency_resolution(None, None, None)
        .await;
    assert_eq!(empty.status, DependencyResolutionStatus::Empty);

    let (_root, failure_config) = wrapper_project(
        "#!/bin/sh\nprintf '[ERROR] Could not resolve dependencies\\n' >&2\nexit 1\n",
    );
    let failure = MavenRunner::discover(&failure_config)
        .unwrap()
        .explain_dependency_resolution(None, None, None)
        .await;
    assert_eq!(failure.status, DependencyResolutionStatus::ResolutionFailed);
    assert_eq!(failure.build.outcome, MavenBuildOutcome::BuildFailure);

    let (_root, mut truncated_config) = wrapper_project(
        "#!/bin/sh\nprintf '[INFO] org.example:core:jar:1.0\\n[INFO] +- org.libs:first:jar:2.0:compile\\n[INFO] deliberately long trailing output\\n'\n",
    );
    truncated_config.max_output_bytes = 85;
    let truncated = MavenRunner::discover(&truncated_config)
        .unwrap()
        .explain_dependency_resolution(None, None, None)
        .await;
    assert_eq!(truncated.status, DependencyResolutionStatus::Incomplete);
    assert!(truncated.incomplete);
    assert!(!truncated.explanations.is_empty());

    let (_root, invalid_config) = wrapper_project(
        "#!/bin/sh\nprintf '[INFO] org.example:core:jar:1.0\\n[INFO] +- definitely-not-a-coordinate\\n'\n",
    );
    let invalid = MavenRunner::discover(&invalid_config)
        .unwrap()
        .explain_dependency_resolution(None, None, None)
        .await;
    assert_eq!(invalid.status, DependencyResolutionStatus::Invalid);
    assert!(invalid.incomplete);
    assert!(invalid.error.is_some());

    let unsafe_filter = MavenRunner::discover(&empty_config)
        .unwrap()
        .explain_dependency_resolution(None, None, Some("g:a;touch"))
        .await;
    assert_eq!(unsafe_filter.status, DependencyResolutionStatus::Invalid);
    assert_eq!(unsafe_filter.build.outcome, MavenBuildOutcome::RunnerError);
}

#[test]
fn reads_multimodule_jacoco_coverage_and_ranks_gaps() {
    let (root, config) = wrapper_project("#!/bin/sh\nexit 0\n");
    let core_report = root.path().join("core/target/site/jacoco/jacoco.xml");
    let web_report = root.path().join("apps/web/target/site/jacoco/jacoco.xml");
    fs::create_dir_all(core_report.parent().unwrap()).unwrap();
    fs::create_dir_all(web_report.parent().unwrap()).unwrap();
    fs::write(
            &core_report,
            r#"<report><package name="org/example"><class name="org/example/Weak"><counter type="INSTRUCTION" missed="90" covered="10"/><counter type="LINE" missed="9" covered="1"/></class></package><counter type="INSTRUCTION" missed="90" covered="10"/><counter type="BRANCH" missed="4" covered="1"/><counter type="LINE" missed="9" covered="1"/><counter type="METHOD" missed="2" covered="1"/><counter type="CLASS" missed="1" covered="1"/></report>"#,
        )
        .unwrap();
    fs::write(
            &web_report,
            r#"<report><package name="org/example"><class name="org/example/Strong"><counter type="INSTRUCTION" missed="1" covered="9"/><counter type="LINE" missed="1" covered="9"/></class></package><counter type="INSTRUCTION" missed="1" covered="9"/><counter type="LINE" missed="1" covered="9"/></report>"#,
        )
        .unwrap();
    let runner = MavenRunner::discover(&config).unwrap();

    let summary = runner.jacoco_coverage();
    assert_eq!(summary.status, CoverageStatus::Available);
    assert_eq!(summary.reports.len(), 2);
    assert!(summary.missing_modules.is_empty());
    assert_eq!(summary.project_metrics.lines.as_ref().unwrap().covered, 10);
    let core = summary
        .reports
        .iter()
        .find(|report| report.module == "core")
        .unwrap();
    let web = summary
        .reports
        .iter()
        .find(|report| report.module == "apps/web")
        .unwrap();
    assert_eq!(core.metrics.branches.as_ref().unwrap().covered, 1);
    assert!(web.metrics.branches.is_none());

    let gaps = runner.jacoco_coverage_gaps(Some(1));
    assert_eq!(gaps.status, CoverageStatus::Available);
    assert_eq!(gaps.gaps.len(), 1);
    assert_eq!(gaps.gaps[0].class_name, "org.example.Weak");
    assert!(gaps.gaps[0].branch.is_none());
    assert!(gaps.incomplete);
}

#[test]
fn reports_missing_stale_and_invalid_jacoco_data_explicitly() {
    let (root, config) = wrapper_project("#!/bin/sh\nexit 0\n");
    let runner = MavenRunner::discover(&config).unwrap();
    let missing = runner.jacoco_coverage();
    assert_eq!(missing.status, CoverageStatus::Missing);
    assert_eq!(missing.missing_modules, vec!["apps/web", "core"]);

    let report = root.path().join("core/target/site/jacoco/jacoco.xml");
    fs::create_dir_all(report.parent().unwrap()).unwrap();
    fs::write(
        &report,
        r#"<report><counter type="LINE" missed="1" covered="1"/></report>"#,
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(20));
    let class = root.path().join("core/target/classes/Foo.class");
    fs::create_dir_all(class.parent().unwrap()).unwrap();
    fs::write(class, b"compiled later").unwrap();
    let stale = runner.jacoco_coverage();
    assert_eq!(stale.status, CoverageStatus::Stale);
    assert_eq!(stale.missing_modules, vec!["apps/web"]);

    fs::write(&report, "<report><broken></report>").unwrap();
    let invalid = runner.jacoco_coverage();
    assert_eq!(invalid.status, CoverageStatus::Invalid);
    assert!(invalid.error.unwrap().contains("invalid JaCoCo XML"));
}

#[test]
fn invalid_effective_pom_and_coordinate_filters_are_rejected() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("effective.xml");
    fs::write(&path, "<project><broken></project>").unwrap();
    assert!(parse_effective_pom(&path).is_err());
    assert!(validate_coordinate_filter("g:a:*").is_ok());
    assert!(validate_coordinate_filter("g:a;touch /tmp/x").is_err());
}

#[test]
fn classifies_compiler_test_and_reactor_results() {
    let compilation = MavenBuildResult::from_run(MavenRunResult {
            status: MavenRunStatus::BuildFailure,
            exit_code: Some(1),
            duration_ms: 10,
            timed_out: false,
            stdout: "[INFO] Reactor Summary:\n[INFO] core .... SUCCESS [ 1.0 s ]\n[INFO] web ..... FAILURE [ 0.2 s ]\n[INFO] BUILD FAILURE".to_owned(),
            stderr: "[ERROR] COMPILATION ERROR\n[ERROR] <PROJECT_ROOT>/src/Foo.java:[2,3] missing symbol".to_owned(),
            stdout_truncated: false,
            stderr_truncated: false,
            redaction_count: 1,
        });
    assert_eq!(compilation.outcome, MavenBuildOutcome::CompilationError);
    assert_eq!(compilation.compiler_diagnostics.len(), 2);
    assert_eq!(
        compilation.reactor_summary,
        vec![
            ReactorModuleResult {
                module: "core".to_owned(),
                status: "success".to_owned(),
            },
            ReactorModuleResult {
                module: "web".to_owned(),
                status: "failure".to_owned(),
            },
        ]
    );

    let tests = MavenBuildResult::from_run(MavenRunResult {
        status: MavenRunStatus::BuildFailure,
        exit_code: Some(1),
        duration_ms: 1,
        timed_out: false,
        stdout: "Tests run: 2, Failures: 1, Errors: 0, Skipped: 0".to_owned(),
        stderr: String::new(),
        stdout_truncated: false,
        stderr_truncated: false,
        redaction_count: 0,
    });
    assert_eq!(tests.outcome, MavenBuildOutcome::TestFailure);
}
