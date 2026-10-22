mod support;

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use anyhow::Result;
use rmcp::model::CallToolRequestParams;
use serde_json::{Map, Value, json};
use support::TestServer;

fn arguments(value: Value) -> Map<String, Value> {
    value
        .as_object()
        .expect("tool arguments must be a JSON object")
        .clone()
}

fn structured(result: rmcp::model::CallToolResult) -> Value {
    assert_eq!(result.is_error, Some(false));
    result
        .structured_content
        .expect("tool should return structured content")
}

async fn overlapping_report_calls(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    gate: &support::MavenReportGate,
    first: CallToolRequestParams,
    second: CallToolRequestParams,
) -> Result<(Value, Value)> {
    let first_call = client.call_tool(first);
    let second_call = async {
        gate.wait_started().await?;
        let call = client.call_tool(second);
        tokio::pin!(call);
        // A round trip establishes that competing requests have been sent while
        // the first child remains held at its marker. No elapsed delay releases it.
        tokio::select! {
            biased;
            result = &mut call => panic!("second report request completed before release: {result:?}"),
            result = client.list_all_tools() => { result?; }
        }
        gate.release()?;
        call.await.map_err(anyhow::Error::from)
    };
    let (first, second) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::join!(first_call, second_call)
    })
    .await?;
    Ok((structured(first?), structured(second?)))
}

#[tokio::test]
async fn parallel_focused_tests_keep_results_and_last_failures_request_scoped() -> Result<()> {
    for failing in [false, true] {
        let server = TestServer::start_with_project().await?;
        let project = server.project_path().unwrap();
        let body = r#"
for arg in "$@"; do case "$arg" in -Dtest=*) test_class=${arg#-Dtest=} ;; esac; done
mkdir -p target/surefire-reports
failures=0
failure=''
if [ -e .fail-tests ]; then failures=1; failure='<failure message="fixture failure"/>'; fi
printf '<testsuite tests="1" failures="%s" errors="0" skipped="0"><testcase classname="%s" name="runs">%s</testcase></testsuite>' "$failures" "$test_class" "$failure" > "target/surefire-reports/TEST-$test_class.xml"
if [ "$failures" = 1 ]; then exit 1; fi
"#;
        let gate = support::MavenReportGate::new(project, body)?;
        if failing {
            std::fs::write(project.join(".fail-tests"), "fail")?;
        }
        let client = server.connect().await?;
        let request = |test_class: &str| {
            CallToolRequestParams::new("run_maven_test").with_arguments(arguments(
                json!({"project_path": project, "test_class": test_class}),
            ))
        };
        let (first, second) =
            overlapping_report_calls(&client, &gate, request("FirstTest"), request("SecondTest"))
                .await?;
        for result in [&first, &second] {
            assert_eq!(result["report_status"], "available", "{result}");
            assert_eq!(
                result["summary"],
                json!({"passed": usize::from(!failing), "failed": usize::from(failing), "errors": 0, "skipped": 0})
            );
        }
        if failing {
            assert_eq!(first["failures"][0]["class_name"], "FirstTest");
            assert_eq!(second["failures"][0]["class_name"], "SecondTest");
        }
        let last_request = |root: &std::path::Path| {
            CallToolRequestParams::new("get_last_maven_test_failures")
                .with_arguments(arguments(json!({"project_path": root})))
        };
        let last = structured(client.call_tool(last_request(project)).await?);
        assert_eq!(last["available"], true);
        assert_eq!(last["summary"], second["summary"]);
        assert_eq!(last["failures"], second["failures"]);

        let other = server.add_trusted_project("other-test-project")?;
        assert_eq!(
            structured(client.call_tool(last_request(&other)).await?)["available"],
            false
        );
        let other_gate = support::MavenReportGate::new(&other, body)?;
        other_gate.release()?;
        std::fs::write(other.join(".fail-tests"), "fail")?;
        let other_result =
            structured(
                client
                    .call_tool(CallToolRequestParams::new("run_maven_test").with_arguments(
                        arguments(json!({"project_path": other, "test_class": "OtherTest"})),
                    ))
                    .await?,
            );
        assert_eq!(other_result["failures"][0]["class_name"], "OtherTest");
        let other_last = structured(client.call_tool(last_request(&other)).await?);
        assert_eq!(other_last["failures"], other_result["failures"]);
        assert_eq!(
            structured(client.call_tool(last_request(project)).await?),
            last
        );
        client.cancel().await?;
    }
    Ok(())
}

#[tokio::test]
async fn parallel_effective_pom_requests_keep_modules_and_projects_isolated() -> Result<()> {
    for same_project in [true, false] {
        let server = TestServer::start_with_project().await?;
        let first_project = server.project_path().unwrap();
        std::fs::write(first_project.join(".identity"), "first-project")?;
        std::fs::write(
            first_project.join("pom.xml"),
            "<project><artifactId>first-project</artifactId><packaging>pom</packaging><modules><module>alpha</module></modules></project>",
        )?;
        std::fs::create_dir(first_project.join("alpha"))?;
        std::fs::write(
            first_project.join("alpha/pom.xml"),
            "<project><artifactId>alpha</artifactId></project>",
        )?;
        let body = r#"
identity=$(cat .identity)
next_module=false
for arg in "$@"; do
  if [ "$next_module" = true ]; then identity=$arg; next_module=false; fi
  case "$arg" in
    -Doutput=*) output=${arg#-Doutput=} ;;
    --projects) next_module=true ;;
  esac
done
printf '<project><groupId>org.fixture</groupId><artifactId>%s</artifactId><version>1</version></project>' "$identity" > "$output"
"#;
        let gate = support::MavenReportGate::new(first_project, body)?;
        let second_project = if same_project {
            first_project.to_owned()
        } else {
            let other = server.add_trusted_project("second-pom-project")?;
            std::fs::write(other.join(".identity"), "second-project")?;
            support::MavenReportGate::new(&other, body)?.release()?;
            other
        };
        let client = server.connect().await?;
        let first = CallToolRequestParams::new("get_effective_pom")
            .with_arguments(arguments(json!({"project_path": first_project})));
        let second = CallToolRequestParams::new("get_effective_pom").with_arguments(arguments(json!({"project_path": second_project, "module": if same_project { Some("alpha") } else { None }})));
        let (first, second) = overlapping_report_calls(&client, &gate, first, second).await?;
        for result in [&first, &second] {
            assert_eq!(result["status"], "available", "{result}");
            assert_eq!(result["projects"].as_array().unwrap().len(), 1);
        }
        assert_eq!(
            first["projects"][0]["coordinate"]["artifact_id"],
            "first-project"
        );
        assert_eq!(
            second["projects"][0]["coordinate"]["artifact_id"],
            if same_project {
                "alpha"
            } else {
                "second-project"
            }
        );
        for project in [first_project, second_project.as_path()] {
            assert!(!project.join("target/maven-mcp/effective-pom.xml").exists());
        }
        client.cancel().await?;
    }
    Ok(())
}

#[tokio::test]
async fn report_errors_and_timeouts_release_the_permit_and_clean_effective_pom() -> Result<()> {
    for tool in ["run_maven_test", "get_effective_pom"] {
        for mode in ["invalid", "failure", "timeout"] {
            let server = TestServer::start_with_project().await?;
            let project = server.project_path().unwrap();
            let body = r#"
output=''
for arg in "$@"; do case "$arg" in -Doutput=*) output=${arg#-Doutput=} ;; -Dtest=*) test_class=${arg#-Dtest=} ;; esac; done
if [ -z "$output" ]; then
  mkdir -p target/surefire-reports
  output="target/surefire-reports/TEST-$test_class.xml"
  valid='<testsuite tests="1" failures="0" errors="0" skipped="0"><testcase classname="RecoveryTest" name="passes"/></testsuite>'
else
  valid='<project><artifactId>recovered</artifactId></project>'
fi
if [ -e .failure-mode ]; then
  mode=$(cat .failure-mode)
  if [ "$mode" = failure ]; then exit 1; fi
  printf '%s' '<broken>' > "$output"
  if [ "$mode" = timeout ]; then
    touch .report-gate/partial
    while [ ! -e .report-gate/finish ]; do sleep 0.01; done
  fi
  exit 0
fi
printf '%s' "$valid" > "$output"
"#;
            support::MavenReportGate::new(project, body)?.release()?;
            std::fs::write(project.join(".failure-mode"), mode)?;
            let (client, _) = server
                .connect_with_limits(&[("MAVEN_TIMEOUT_SECONDS", "1")])
                .await?;
            let request = |test_class: &str| {
                CallToolRequestParams::new(tool).with_arguments(arguments(
                    if tool == "run_maven_test" {
                        json!({"project_path": project, "test_class": test_class})
                    } else {
                        json!({"project_path": project})
                    },
                ))
            };
            let result = structured(
                tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    client.call_tool(request("BrokenTest")),
                )
                .await??,
            );
            let status_field = if tool == "run_maven_test" {
                "report_status"
            } else {
                "status"
            };
            assert_eq!(
                result[status_field],
                if mode == "failure" {
                    "missing"
                } else {
                    "invalid"
                },
                "{result}"
            );
            if mode == "timeout" {
                assert_eq!(result["build"]["outcome"], "timeout", "{result}");
                assert!(project.join(".report-gate/partial").exists());
            } else if mode == "failure" {
                assert_eq!(result["build"]["outcome"], "build_failure");
            }
            assert!(!project.join("target/maven-mcp/effective-pom.xml").exists());
            std::fs::remove_file(project.join(".failure-mode"))?;
            let recovered = structured(
                tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    client.call_tool(request("RecoveryTest")),
                )
                .await??,
            );
            assert_eq!(recovered[status_field], "available", "{recovered}");
            assert_eq!(recovered["build"]["outcome"], "success");
            if tool == "run_maven_test" {
                assert_eq!(recovered["summary"]["passed"], 1);
            } else {
                assert_eq!(
                    recovered["projects"][0]["coordinate"]["artifact_id"],
                    "recovered"
                );
            }
            assert!(!project.join("target/maven-mcp/effective-pom.xml").exists());
            client.cancel().await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn stdio_mcp_exposes_and_executes_all_tools() -> Result<()> {
    let server = TestServer::start_with_project().await?;
    let client = server.connect().await?;
    let project_path = server.project_path().unwrap().display().to_string();

    let tools = client.list_all_tools().await?;
    let actual_names = tools
        .iter()
        .map(|tool| tool.name.as_ref())
        .collect::<BTreeSet<_>>();
    let expected_names = BTreeSet::from([
        "compare_artifact_api",
        "describe_class",
        "diagnose_artifact",
        "explain_dependency_resolution",
        "get_artifact_pom",
        "get_class_source",
        "get_declaration_source",
        "get_dependency_tree",
        "get_effective_pom",
        "get_jacoco_coverage",
        "get_jacoco_coverage_gaps",
        "get_jar_entry",
        "get_last_maven_test_failures",
        "get_maven_classpath",
        "index_stats",
        "inspect_maven_project",
        "list_artifact_versions",
        "list_jar_classes",
        "list_maven_test_classes",
        "run_maven",
        "run_maven_lifecycle",
        "run_maven_test",
        "search_classes",
        "search_class_members",
        "search_class_references",
        "search_jar_entries",
        "search_jar_content",
        "search_jars",
        "search_providers",
        "search_source",
        "search_type_hierarchy",
    ]);
    assert_eq!(actual_names, expected_names);
    let maven_tool = tools.iter().find(|tool| tool.name == "run_maven").unwrap();
    assert_eq!(
        maven_tool.input_schema["properties"]["arguments"]["type"],
        "array"
    );
    assert_eq!(
        maven_tool.input_schema["properties"]["arguments"]["items"]["type"],
        "string"
    );
    assert!(
        maven_tool.input_schema["required"]
            .as_array()
            .unwrap()
            .contains(&json!("arguments"))
    );
    let maven_schema = maven_tool.output_schema.as_ref().unwrap();
    for field in [
        "status",
        "exit_code",
        "duration_ms",
        "timed_out",
        "stdout",
        "stderr",
        "stdout_truncated",
        "stderr_truncated",
        "redaction_count",
    ] {
        assert!(
            maven_schema["properties"].get(field).is_some(),
            "run_maven schema missing {field}"
        );
    }
    for tool in &tools {
        assert_eq!(
            tool.output_schema
                .as_ref()
                .and_then(|schema| schema.get("type"))
                .and_then(Value::as_str),
            Some("object"),
            "{} output schema must be an object",
            tool.name
        );
    }
    let lifecycle_schema = tools
        .iter()
        .find(|tool| tool.name == "run_maven_lifecycle")
        .and_then(|tool| tool.output_schema.as_ref())
        .expect("run_maven_lifecycle must publish an output schema");
    assert!(
        lifecycle_schema["properties"]
            .get("policy_notice")
            .is_some(),
        "run_maven_lifecycle schema must expose policy_notice"
    );
    for tool in &tools {
        let required = tool
            .input_schema
            .get("required")
            .and_then(Value::as_array)
            .expect("input schema must list required properties");
        assert!(
            required.iter().any(|name| name == "project_path"),
            "{} must require project_path",
            tool.name
        );
    }

    let project = structured(
        client
            .call_tool(
                CallToolRequestParams::new("inspect_maven_project")
                    .with_arguments(arguments(json!({ "project_path": project_path }))),
            )
            .await?,
    );
    assert_eq!(project["artifact_id"], "fixture-project");

    let classes = structured(
        client
            .call_tool(
                CallToolRequestParams::new("search_classes").with_arguments(arguments(json!({
                    "project_path": server.project_path().unwrap().display().to_string(),
                    "query": "Foo"
                }))),
            )
            .await?,
    );
    assert_eq!(classes["results"][0]["class_name"], "org.example.Foo");
    assert_eq!(
        classes["results"][0]["jar"]["coordinate"],
        "org.libs:helper:2.0"
    );
    let leaked = structured(
        client
            .call_tool(
                CallToolRequestParams::new("search_jars").with_arguments(arguments(json!({
                    "project_path": server.project_path().unwrap().display().to_string(),
                    "query": "demo"
                }))),
            )
            .await?,
    );
    assert_eq!(leaked["results"].as_array().unwrap().len(), 0);

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn scoped_index_accepts_a_valid_classpath_when_maven_warns_on_stderr() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let server = TestServer::start_with_project().await?;
    let project_path = server.project_path().unwrap();
    let wrapper = project_path.join("mvnw");
    std::fs::write(
        &wrapper,
        r#"#!/bin/sh
printf '%s\n' '[WARNING] benign wrapper warning' >&2
printf '%s\n' 'Dependencies classpath:' "$PWD/execution-repository/org/libs/helper/2.0/helper-2.0.jar"
"#,
    )?;
    let mut permissions = wrapper.metadata()?.permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&wrapper, permissions)?;

    let client = server.connect().await?;
    let classes = structured(
        client
            .call_tool(
                CallToolRequestParams::new("search_classes").with_arguments(arguments(json!({
                    "project_path": project_path.display().to_string(),
                    "query": "Foo"
                }))),
            )
            .await?,
    );
    assert_eq!(classes["results"][0]["class_name"], "org.example.Foo");

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn exact_artifact_lookups_work_when_reactor_classpath_fails() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let server = TestServer::start_with_project().await?;
    let project_path = server.project_path().unwrap();
    let wrapper = project_path.join("mvnw");
    std::fs::write(
        &wrapper,
        "#!/bin/sh\nprintf '%s\\n' '[ERROR] missing reactor sibling'\nexit 1\n",
    )?;
    let mut permissions = wrapper.metadata()?.permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&wrapper, permissions)?;

    let client = server.connect().await?;
    let project_path_buf = project_path.to_path_buf();
    let project_path = project_path.display().to_string();
    let pom = structured(
        client
            .call_tool(
                CallToolRequestParams::new("get_artifact_pom").with_arguments(arguments(json!({
                    "project_path": project_path,
                    "coordinate": "org.libs:helper:2.0"
                }))),
            )
            .await?,
    );
    assert_eq!(pom["found"], true);
    assert_eq!(pom["descriptor"]["packaging"], "jar");

    let health = structured(
        client
            .call_tool(
                CallToolRequestParams::new("diagnose_artifact").with_arguments(arguments(json!({
                    "project_path": project_path,
                    "coordinate": "org.libs:helper:2.0"
                }))),
            )
            .await?,
    );
    assert_eq!(health["found"], true);
    assert!(
        health["files"]
            .as_array()
            .is_some_and(|files| !files.is_empty())
    );

    let missing = structured(
        client
            .call_tool(
                CallToolRequestParams::new("diagnose_artifact").with_arguments(arguments(json!({
                    "project_path": project_path,
                    "coordinate": "org.libs:absent:2.0"
                }))),
            )
            .await?,
    );
    assert_eq!(missing["found"], false);

    let missing_pom = structured(
        client
            .call_tool(
                CallToolRequestParams::new("get_artifact_pom").with_arguments(arguments(json!({
                    "project_path": project_path,
                    "coordinate": "org.libs:absent:2.0"
                }))),
            )
            .await?,
    );
    assert_eq!(missing_pom["found"], false);

    for tool in ["get_artifact_pom", "diagnose_artifact"] {
        client
            .call_tool(
                CallToolRequestParams::new(tool).with_arguments(arguments(json!({
                    "project_path": project_path,
                    "coordinate": "org.libs:../helper:2.0"
                }))),
            )
            .await
            .expect_err("invalid coordinate must be rejected");
    }

    let outside = tempfile::TempDir::new()?;
    let outside_marker = "outside-marker-secret";
    std::fs::create_dir_all(outside.path().join("demo/1.0"))?;
    std::fs::write(
        outside.path().join("demo/1.0/demo-1.0.pom"),
        format!("<project><description>{outside_marker}</description></project>"),
    )?;
    let absolute_group = outside.path().display().to_string().replace('/', ".");
    for tool in ["get_artifact_pom", "diagnose_artifact"] {
        for coordinate in [
            format!("{absolute_group}:demo:1.0"),
            ".org.libs:helper:2.0".to_owned(),
            "org.libs.:helper:2.0".to_owned(),
            "org..libs:helper:2.0".to_owned(),
            "org/libs:helper:2.0".to_owned(),
            "org\\libs:helper:2.0".to_owned(),
            "org.libs:helper:2.0:../sources".to_owned(),
        ] {
            let error = client
                .call_tool(
                    CallToolRequestParams::new(tool).with_arguments(arguments(json!({
                        "project_path": project_path,
                        "coordinate": coordinate
                    }))),
                )
                .await
                .expect_err("traversal coordinate must be rejected");
            let message = error.to_string();
            assert!(
                message.contains("exact groupId:artifactId:version"),
                "{message}"
            );
            assert!(!message.contains(outside_marker));
            assert!(!message.contains(outside.path().to_str().unwrap()));
        }
    }

    let repository = project_path_buf.join("execution-repository");
    std::os::unix::fs::symlink(outside.path(), repository.join("org/escape"))?;
    let linked_artifact = repository.join("org/libs/linked/1.0");
    std::fs::create_dir_all(&linked_artifact)?;
    std::os::unix::fs::symlink(
        outside.path().join("demo/1.0/demo-1.0.pom"),
        linked_artifact.join("linked-1.0.pom"),
    )?;
    for tool in ["get_artifact_pom", "diagnose_artifact"] {
        for coordinate in ["org.escape:demo:1.0", "org.escape:missing:1.0"] {
            let error = client
                .call_tool(
                    CallToolRequestParams::new(tool).with_arguments(arguments(json!({
                        "project_path": project_path,
                        "coordinate": coordinate
                    }))),
                )
                .await
                .expect_err("outward symlink must be rejected");
            let message = error.to_string();
            assert!(
                message.contains("outside the configured Maven repository"),
                "{message}"
            );
            assert!(!message.contains(outside_marker));
            assert!(!message.contains(outside.path().to_str().unwrap()));
        }
    }
    let error = client
        .call_tool(
            CallToolRequestParams::new("get_artifact_pom").with_arguments(arguments(json!({
                "project_path": project_path,
                "coordinate": "org.libs:linked:1.0"
            }))),
        )
        .await
        .expect_err("outward POM symlink must be rejected");
    assert!(!error.to_string().contains(outside_marker));
    let linked_health = structured(
        client
            .call_tool(
                CallToolRequestParams::new("diagnose_artifact").with_arguments(arguments(json!({
                    "project_path": project_path,
                    "coordinate": "org.libs:linked:1.0"
                }))),
            )
            .await?,
    );
    assert_eq!(linked_health["files"], json!([]));

    std::os::unix::fs::symlink(
        outside.path().join("missing"),
        repository.join("org/dangling"),
    )?;
    let dangling_artifact = repository.join("org/libs/dangling/1.0");
    std::fs::create_dir_all(&dangling_artifact)?;
    std::os::unix::fs::symlink(
        outside.path().join("demo/missing.pom"),
        dangling_artifact.join("dangling-1.0.pom"),
    )?;
    for (tool, coordinate) in [
        ("get_artifact_pom", "org.dangling:demo:1.0"),
        ("diagnose_artifact", "org.dangling:demo:1.0"),
        ("get_artifact_pom", "org.libs:dangling:1.0"),
    ] {
        let error = client
            .call_tool(
                CallToolRequestParams::new(tool).with_arguments(arguments(json!({
                    "project_path": project_path,
                    "coordinate": coordinate
                }))),
            )
            .await
            .expect_err("dangling outward symlink must be rejected");
        let message = error.to_string();
        assert!(
            message.contains("outside the configured Maven repository"),
            "{message}"
        );
        assert!(!message.contains(outside.path().to_str().unwrap()));
    }

    let untrusted_project = tempfile::TempDir::new()?;
    std::fs::write(untrusted_project.path().join("pom.xml"), "<project/>")?;
    for tool in ["get_artifact_pom", "diagnose_artifact"] {
        let error = client
            .call_tool(
                CallToolRequestParams::new(tool).with_arguments(arguments(json!({
                    "project_path": untrusted_project.path().display().to_string(),
                    "coordinate": "org.libs:helper:2.0"
                }))),
            )
            .await
            .expect_err("untrusted project must be rejected");
        assert!(
            error
                .to_string()
                .contains("MAVEN_TRUSTED_PROJECT_DIRECTORIES")
        );
    }

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn scoped_index_aggregates_classpaths_from_later_reactor_modules() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let server = TestServer::start_with_project().await?;
    let project_path = server.project_path().unwrap();
    let wrapper = project_path.join("mvnw");
    std::fs::write(
        &wrapper,
        r#"#!/bin/sh
printf '%s\n' \
  '[INFO] Building empty-parent [pom]' \
  'Dependencies classpath:' \
  '' \
  '[INFO] Building dependency-bearing-module [jar]' \
  'Dependencies classpath:' \
  "$PWD/execution-repository/org/libs/helper/2.0/helper-2.0.jar" \
  '[INFO] Building empty-aggregator [pom]' \
  'Dependencies classpath:' \
  ''
"#,
    )?;
    let mut permissions = wrapper.metadata()?.permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&wrapper, permissions)?;

    let client = server.connect().await?;
    let versions = structured(
        client
            .call_tool(
                CallToolRequestParams::new("list_artifact_versions").with_arguments(arguments(
                    json!({
                        "project_path": project_path.display().to_string(),
                        "group_id": "org.libs",
                        "artifact_id": "helper"
                    }),
                )),
            )
            .await?,
    );
    assert_eq!(versions["org.libs:helper"], json!(["2.0"]));

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn scoped_index_reports_bounded_stdout_only_maven_failures() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let server = TestServer::start_with_project().await?;
    let project_path = server.project_path().unwrap();
    let wrapper = project_path.join("mvnw");
    std::fs::write(
        &wrapper,
        r#"#!/bin/sh
printf '%s\n' '[ERROR] Could not resolve sibling reactor artifact' '[ERROR] Install or package the required sibling modules first'
i=0
while [ "$i" -lt 100 ]; do
  printf '[ERROR] diagnostic-padding-%04d-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\n' "$i"
  i=$((i + 1))
done
exit 1
"#,
    )?;
    let mut permissions = wrapper.metadata()?.permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&wrapper, permissions)?;

    let client = server.connect().await?;
    let error = client
        .call_tool(
            CallToolRequestParams::new("search_classes").with_arguments(arguments(json!({
                "project_path": project_path.display().to_string(),
                "query": "Foo"
            }))),
        )
        .await
        .expect_err("a failed Maven classpath build must be an MCP error");
    let message = error.to_string();
    assert!(
        message.contains("Could not resolve sibling reactor artifact"),
        "{message}"
    );
    assert!(message.ends_with('…'), "{message}");
    assert!(message.chars().count() <= 2_200, "{message}");

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn scoped_index_reports_unknown_dependency_after_truncated_stdout() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let server = TestServer::start_with_project().await?;
    let project_path = server.project_path().unwrap();
    std::fs::write(
        project_path.join("pom.xml"),
        r#"<project>
  <modelVersion>4.0.0</modelVersion>
  <groupId>org.example</groupId>
  <artifactId>fixture-project</artifactId>
  <version>1.0</version>
  <dependencies>
    <dependency>
      <groupId>invalid.example</groupId>
      <artifactId>missing-library</artifactId>
      <version>99.0-does-not-exist</version>
    </dependency>
  </dependencies>
</project>"#,
    )?;
    let wrapper = project_path.join("mvnw");
    std::fs::write(
        &wrapper,
        r#"#!/bin/sh
grep -q '<artifactId>missing-library</artifactId>' pom.xml || exit 90
i=0
while [ "$i" -lt 400 ]; do
  printf '[INFO] reactor-output-before-resolution-%04d-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\n' "$i"
  i=$((i + 1))
done
printf '%s\n' \
  '[ERROR] Failed to execute goal on project fixture-project: Could not resolve dependencies' \
  '[ERROR] dependency: invalid.example:missing-library:jar:99.0-does-not-exist (compile)' \
  '[ERROR] Could not find artifact invalid.example:missing-library:jar:99.0-does-not-exist'
exit 1
"#,
    )?;
    let mut permissions = wrapper.metadata()?.permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&wrapper, permissions)?;

    let client = server.connect().await?;
    let error = client
        .call_tool(
            CallToolRequestParams::new("search_classes").with_arguments(arguments(json!({
                "project_path": project_path.display().to_string(),
                "query": "Foo"
            }))),
        )
        .await
        .expect_err("an unknown Maven dependency must prevent project indexing");
    let message = error.to_string();
    assert!(
        message.contains("invalid.example:missing-library:jar:99.0-does-not-exist"),
        "missing dependency diagnostic was lost: {message}"
    );

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn arbitrary_maven_arguments_preserve_boundaries_and_do_not_inject_flags() -> Result<()> {
    let server = TestServer::start_with_project().await?;
    let project = server.project_path().unwrap();
    std::fs::write(
        project.join("mvnw"),
        r#"#!/bin/sh
: > argv.bin
for arg do printf '%s\0' "$arg" >> argv.bin; done
printf '%s\n' '[INFO] BUILD SUCCESS'
"#,
    )?;
    let client = server.connect().await?;
    let argv = vec![
        "clean",
        "install",
        "deploy",
        "org.example:custom-plugin:1.2:goal",
        "-Pcustom,other",
        "-Dmessage=hello world",
        "-Dmessage=second",
        "--settings",
        "settings with spaces.xml",
        "-f",
        "../alternate/pom.xml",
        "-pl",
        "core",
        "-am",
        "-U",
        "-X",
        "-Dmaven.repo.local=custom-cache",
        "$(touch shell-substitution)",
        "; touch shell-semicolon",
        "*.xml",
        "",
        "line\nbreak",
        "árvíztűrő",
    ];
    for expected in [
        argv,
        vec!["--version"],
        vec!["--offline", "package"],
        vec![],
    ] {
        let result = structured(
            client
                .call_tool(
                    CallToolRequestParams::new("run_maven").with_arguments(arguments(json!({
                        "project_path": project.display().to_string(),
                        "arguments": expected
                    }))),
                )
                .await?,
        );
        assert_eq!(result["status"], "success", "{result:#}");
        assert_eq!(result["exit_code"], 0);
        let expected_bytes: Vec<u8> = expected
            .iter()
            .flat_map(|arg| arg.bytes().chain(std::iter::once(0)))
            .collect();
        assert_eq!(std::fs::read(project.join("argv.bin"))?, expected_bytes);
        assert!(!project.join("shell-substitution").exists());
        assert!(!project.join("shell-semicolon").exists());
    }
    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn arbitrary_maven_rejects_invalid_requests_and_untrusted_projects() -> Result<()> {
    let server = TestServer::start_with_project().await?;
    let other = TestServer::start_with_project().await?;
    let project = server.project_path().unwrap();
    std::fs::write(project.join("mvnw"), "#!/bin/sh\ntouch child-started\n")?;
    let client = server.connect().await?;
    let path = project.display().to_string();
    for request in [
        json!({"project_path": path}),
        json!({"project_path": path, "arguments": "clean install"}),
        json!({"project_path": path, "arguments": [7]}),
        json!({"project_path": path, "arguments": ["bad\u{0}argument"]}),
        json!({"arguments": ["clean"]}),
        json!({"project_path": "relative", "arguments": ["clean"]}),
    ] {
        let response = client
            .call_tool(CallToolRequestParams::new("run_maven").with_arguments(arguments(request)))
            .await;
        match response {
            Ok(result) => assert_eq!(result.is_error, Some(true), "{result:?}"),
            Err(error) => assert!(error.to_string().contains("-32602"), "{error}"),
        }
    }
    let error = client
        .call_tool(
            CallToolRequestParams::new("run_maven").with_arguments(arguments(json!({
                "project_path": other.project_path().unwrap().display().to_string(),
                "arguments": ["clean"]
            }))),
        )
        .await
        .expect_err("untrusted projects must be rejected");
    assert!(
        error
            .to_string()
            .contains("MAVEN_TRUSTED_PROJECT_DIRECTORIES")
    );
    assert!(!project.join("child-started").exists());
    client.cancel().await?;

    let unconfigured = TestServer::start().await?;
    let client = unconfigured.connect().await?;
    let error = client
        .call_tool(
            CallToolRequestParams::new("run_maven").with_arguments(arguments(json!({
                "project_path": path, "arguments": ["clean"]
            }))),
        )
        .await
        .expect_err("unconfigured execution must be disabled");
    assert!(
        error
            .to_string()
            .contains("MAVEN_TRUSTED_PROJECT_DIRECTORIES")
    );
    assert!(!project.join("child-started").exists());
    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn arbitrary_maven_reports_failure_timeout_and_bounded_redacted_output() -> Result<()> {
    let server = TestServer::start_with_project().await?;
    let project = server.project_path().unwrap();
    let client = server.connect().await?;
    for (script, status, exit_code, truncated) in [
        (
            "#!/bin/sh\nprintf 'root=%s password=synthetic-value\\n' \"$PWD\" >&2\nexit 23\n",
            "build_failure",
            json!(23),
            false,
        ),
        ("#!/bin/sh\nsleep 30\n", "timeout", Value::Null, false),
        (
            "#!/bin/sh\ni=0\nwhile [ \"$i\" -lt 2000 ]; do printf '%s\\n' 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'; printf '%s\\n' 'yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy' >&2; i=$((i+1)); done\n",
            "success",
            json!(0),
            true,
        ),
    ] {
        std::fs::write(project.join("mvnw"), script)?;
        let result = structured(
            client
                .call_tool(
                    CallToolRequestParams::new("run_maven").with_arguments(arguments(json!({
                        "project_path": project.display().to_string(), "arguments": ["custom:goal"]
                    }))),
                )
                .await?,
        );
        assert_eq!(result["status"], status, "{result:#}");
        assert_eq!(result["exit_code"], exit_code);
        assert_eq!(result["timed_out"], status == "timeout");
        assert_eq!(result["stdout_truncated"], truncated);
        assert_eq!(result["stderr_truncated"], truncated);
        if status == "build_failure" {
            let stderr = result["stderr"].as_str().unwrap();
            assert!(stderr.contains("<PROJECT_ROOT>"));
            assert!(!stderr.contains("synthetic-value"));
            assert!(result["redaction_count"].as_u64().unwrap() >= 2);
        }
        assert!(result["stdout"].as_str().unwrap().len() <= 16384);
        assert!(result["stderr"].as_str().unwrap().len() <= 16384);
    }
    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn trusted_directory_tree_serializes_maven_execution_across_projects() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let server = TestServer::start_with_project().await?;
    let first_project = server.project_path().unwrap().to_owned();
    let second_project = server.add_trusted_project("second-project")?;
    let marker_directory = tempfile::tempdir()?;
    let lock = marker_directory.path().join("maven-running");
    let overlap = marker_directory.path().join("overlap");
    for project in [&first_project, &second_project] {
        let wrapper = project.join("mvnw");
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nif [ -e '{lock}' ]; then touch '{overlap}'; fi\ntouch '{lock}'\nsleep 1\nrm -f '{lock}'\nprintf '%s\\n' '[INFO] BUILD SUCCESS'\n",
                lock = lock.display(),
                overlap = overlap.display(),
            ),
        )?;
        let mut permissions = wrapper.metadata()?.permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&wrapper, permissions)?;
    }

    let client = server.connect().await?;
    let first = client.call_tool(
        CallToolRequestParams::new("run_maven_lifecycle").with_arguments(arguments(json!({
            "project_path": first_project.display().to_string(),
            "phase": "compile"
        }))),
    );
    let second = client.call_tool(CallToolRequestParams::new("run_maven").with_arguments(
        arguments(json!({
            "project_path": second_project.display().to_string(),
            "arguments": ["custom:goal", "-Pcustom"]
        })),
    ));
    let (first, second) = tokio::join!(first, second);
    assert_eq!(structured(first?)["outcome"], "success");
    assert_eq!(structured(second?)["status"], "success");
    assert!(
        !overlap.exists(),
        "Maven processes overlapped across trusted projects"
    );

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn jenv_java_selection_is_request_scoped_and_reports_resolution_errors() -> Result<()> {
    let server = TestServer::start_with_jenv_project().await?;
    let first_project = server.project_path().unwrap().to_owned();
    let second_project = server.add_trusted_project("second-java-project")?;
    server.set_jenv_version(&second_project, "two")?;
    let client = server.connect().await?;

    for project_path in [&first_project, &second_project] {
        let result = structured(
            client
                .call_tool(
                    CallToolRequestParams::new("run_maven_lifecycle").with_arguments(arguments(
                        json!({
                            "project_path": project_path.display().to_string(),
                            "phase": "compile"
                        }),
                    )),
                )
                .await?,
        );
        assert_eq!(result["outcome"], "success", "{result:#}");
        let arbitrary = structured(
            client
                .call_tool(
                    CallToolRequestParams::new("run_maven").with_arguments(arguments(json!({
                        "project_path": project_path.display().to_string(),
                        "arguments": ["custom:goal"]
                    }))),
                )
                .await?,
        );
        assert_eq!(arbitrary["status"], "success", "{arbitrary:#}");
    }

    server.set_jenv_version(&first_project, "missing")?;
    let arbitrary = structured(
        client
            .call_tool(
                CallToolRequestParams::new("run_maven").with_arguments(arguments(json!({
                    "project_path": first_project.display().to_string(),
                    "arguments": ["custom:goal"]
                }))),
            )
            .await?,
    );
    assert_eq!(arbitrary["status"], "runner_error", "{arbitrary:#}");
    let failure = structured(
        client
            .call_tool(
                CallToolRequestParams::new("run_maven_lifecycle").with_arguments(arguments(
                    json!({
                        "project_path": first_project.display().to_string(),
                        "phase": "compile"
                    }),
                )),
            )
            .await?,
    );
    assert_eq!(failure["outcome"], "runner_error");
    assert_eq!(failure["run"]["status"], "runner_error");
    assert!(
        failure["run"]["stderr"]
            .as_str()
            .unwrap()
            .contains("verify .java-version")
    );

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn disabled_jenv_mode_preserves_the_inherited_java_environment() -> Result<()> {
    let server = TestServer::start_with_inherited_java_project().await?;
    let client = server.connect().await?;
    let result = structured(
        client
            .call_tool(
                CallToolRequestParams::new("run_maven_lifecycle").with_arguments(arguments(
                    json!({
                        "project_path": server.project_path().unwrap().display().to_string(),
                        "phase": "compile"
                    }),
                )),
            )
            .await?,
    );

    assert_eq!(result["outcome"], "success", "{result:#}");
    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn project_operations_require_a_configured_trusted_directory() -> Result<()> {
    let unconfigured_server = TestServer::start().await?;
    let candidate_project = TestServer::start_with_project().await?;
    let client = unconfigured_server.connect().await?;

    let error = client
        .call_tool(
            CallToolRequestParams::new("inspect_maven_project").with_arguments(arguments(json!({
                "project_path": candidate_project.project_path().unwrap().display().to_string()
            }))),
        )
        .await
        .expect_err("Maven operations must require a configured trusted directory");
    assert!(
        error
            .to_string()
            .contains("MAVEN_TRUSTED_PROJECT_DIRECTORIES")
    );

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn maven_lifecycle_reports_build_failure_and_timeout_through_stdio() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let server = TestServer::start_with_project().await?;
    let project_path = server.project_path().unwrap().to_owned();
    let wrapper = project_path.join("mvnw");
    std::fs::write(
        &wrapper,
        "#!/bin/sh\nprintf '%s\\n' '[ERROR] build failed' >&2\nexit 1\n",
    )?;
    let mut permissions = wrapper.metadata()?.permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&wrapper, permissions)?;

    let client = server.connect().await?;
    let failure = structured(
        client
            .call_tool(
                CallToolRequestParams::new("run_maven_lifecycle").with_arguments(arguments(
                    json!({
                        "project_path": project_path.display().to_string(),
                        "phase": "compile"
                    }),
                )),
            )
            .await?,
    );
    assert_eq!(failure["outcome"], "build_failure");
    assert_eq!(failure["run"]["exit_code"], 1);

    std::fs::write(&wrapper, "#!/bin/sh\nsleep 3\n")?;
    let mut permissions = wrapper.metadata()?.permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&wrapper, permissions)?;
    let timeout = structured(
        client
            .call_tool(
                CallToolRequestParams::new("run_maven_lifecycle").with_arguments(arguments(
                    json!({
                        "project_path": project_path.display().to_string(),
                        "phase": "verify"
                    }),
                )),
            )
            .await?,
    );
    assert_eq!(timeout["outcome"], "timeout");
    assert_eq!(timeout["run"]["timed_out"], true);

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn stdio_mcp_rejects_invalid_and_unknown_tool_calls() -> Result<()> {
    let server = TestServer::start_with_project().await?;
    let client = server.connect().await?;
    let project_path = server.project_path().unwrap().display().to_string();

    let missing_argument = client
        .call_tool(CallToolRequestParams::new("search_classes"))
        .await?;
    assert_eq!(missing_argument.is_error, Some(true));
    assert!(
        missing_argument.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("project_path")
    );

    let invalid_regex = client
        .call_tool(
            CallToolRequestParams::new("search_source").with_arguments(arguments(json!({
                "project_path": project_path,
                "query": "[",
                "regex": true
            }))),
        )
        .await
        .expect_err("invalid regex must fail at the protocol level");
    assert!(invalid_regex.to_string().contains("regex parse error"));

    let descriptor_without_member = client
        .call_tool(
            CallToolRequestParams::new("get_declaration_source").with_arguments(arguments(json!({
                "project_path": server.project_path().unwrap().display().to_string(),
                "class_name": "org.example.Foo",
                "descriptor": "()V"
            }))),
        )
        .await
        .expect_err("descriptor without member name must fail at the protocol level");
    assert!(
        descriptor_without_member
            .to_string()
            .contains("member_name")
    );

    let unknown_tool = client
        .call_tool(CallToolRequestParams::new("not_a_real_tool"))
        .await
        .expect_err("unknown tool must fail at the protocol level");
    assert!(
        unknown_tool
            .to_string()
            .to_lowercase()
            .contains("not found")
    );

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn effective_pom_validates_complete_xml_through_stdio() -> Result<()> {
    let server = TestServer::start_with_project().await?;
    let client = server.connect().await?;
    let project = server.project_path().unwrap();
    let fixture = project.join(".effective-pom-fixture.xml");
    for (xml, expected_ids) in [
        (
            "<?xml version=\"1.0\"?>\n<!-- generated -->\n  <projects xmlns=\"http://maven.apache.org/POM/4.0.0\"><project><artifactId>a</artifactId></project><project><artifactId>b</artifactId></project></projects>\n",
            vec!["a", "b"],
        ),
        (
            "<?xml version=\"1.0\"?><!-- generated --><m:project xmlns:m=\"http://maven.apache.org/POM/4.0.0\"><m:artifactId>solo</m:artifactId></m:project>",
            vec!["solo"],
        ),
    ] {
        std::fs::write(&fixture, xml)?;
        let result = structured(
            client
                .call_tool(
                    CallToolRequestParams::new("get_effective_pom").with_arguments(arguments(
                        json!({ "project_path": project.display().to_string() }),
                    )),
                )
                .await?,
        );
        assert_eq!(result["status"], "available");
        let ids: Vec<_> = result["projects"]
            .as_array()
            .unwrap()
            .iter()
            .map(|project| project["coordinate"]["artifact_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, expected_ids);
    }
    for xml in [
        "<other/>",
        "<projects><project><artifactId>a</artifactId>",
        "<project/><other>",
        "<project><artifactId>a</artifactId></project><other>",
        "<projects><project><artifactId>a</artifactId></project></projects><project/>",
        "<project/>",
        "<project><artifactId>a</artifactId></project>text",
    ] {
        std::fs::write(&fixture, xml)?;
        let result = structured(
            client
                .call_tool(
                    CallToolRequestParams::new("get_effective_pom").with_arguments(arguments(
                        json!({ "project_path": project.display().to_string() }),
                    )),
                )
                .await?,
        );
        assert_eq!(result["status"], "invalid", "{xml}");
        assert_eq!(result["projects"], json!([]), "{xml}");
        assert!(!result["error"].as_str().unwrap().is_empty(), "{xml}");
    }
    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn opt_in_project_mode_exposes_and_executes_project_tools() -> Result<()> {
    let server = TestServer::start_with_project().await?;
    let other_server = TestServer::start_with_project().await?;
    let client = server.connect().await?;
    let project_path = server.project_path().unwrap().display().to_string();
    let other_project_path = other_server.project_path().unwrap().display().to_string();

    let tools = client.list_all_tools().await?;
    let names = tools
        .iter()
        .map(|tool| tool.name.as_ref())
        .collect::<BTreeSet<_>>();
    for name in [
        "explain_dependency_resolution",
        "get_dependency_tree",
        "get_effective_pom",
        "get_jacoco_coverage",
        "get_jacoco_coverage_gaps",
        "get_last_maven_test_failures",
        "get_maven_classpath",
        "inspect_maven_project",
        "list_maven_test_classes",
        "run_maven",
        "run_maven_lifecycle",
        "run_maven_test",
    ] {
        assert!(names.contains(name), "missing opt-in project tool {name}");
    }

    let project = structured(
        client
            .call_tool(
                CallToolRequestParams::new("inspect_maven_project")
                    .with_arguments(arguments(json!({ "project_path": project_path }))),
            )
            .await?,
    );
    assert_eq!(project["artifact_id"], "fixture-project");
    assert_eq!(project["wrapper"], true);

    let untrusted = client
        .call_tool(
            CallToolRequestParams::new("inspect_maven_project")
                .with_arguments(arguments(json!({ "project_path": other_project_path }))),
        )
        .await
        .expect_err("a project outside the trusted directory tree must be rejected");
    assert!(
        untrusted
            .to_string()
            .contains("MAVEN_TRUSTED_PROJECT_DIRECTORIES")
    );

    let lifecycle = structured(
        client
            .call_tool(
                CallToolRequestParams::new("run_maven_lifecycle").with_arguments(arguments(
                    json!({
                        "project_path": server.project_path().unwrap().display().to_string(),
                        "phase": "compile"
                    }),
                )),
            )
            .await?,
    );
    assert_eq!(lifecycle["outcome"], "success");

    let tests = structured(
        client
            .call_tool(
                CallToolRequestParams::new("list_maven_test_classes").with_arguments(arguments(
                    json!({ "project_path": server.project_path().unwrap().display().to_string() }),
                )),
            )
            .await?,
    );
    assert_eq!(tests["results"], json!(["org.example.FooTest"]));
    let focused = structured(
        client
            .call_tool(
                CallToolRequestParams::new("run_maven_test").with_arguments(arguments(json!({
                    "project_path": server.project_path().unwrap().display().to_string(),
                    "test_class": "org.example.FooTest"
                }))),
            )
            .await?,
    );
    assert_eq!(focused["report_status"], "available");
    assert_eq!(focused["summary"]["passed"], 1);
    let first_failures = structured(
        client
            .call_tool(
                CallToolRequestParams::new("get_last_maven_test_failures").with_arguments(
                    arguments(json!({
                        "project_path": server.project_path().unwrap().display().to_string()
                    })),
                ),
            )
            .await?,
    );
    assert_eq!(first_failures["available"], true);
    let other_failures = client
        .call_tool(
            CallToolRequestParams::new("get_last_maven_test_failures")
                .with_arguments(arguments(json!({ "project_path": other_project_path }))),
        )
        .await
        .expect_err("last-test state must not be observable outside the trusted directory tree");
    assert!(
        other_failures
            .to_string()
            .contains("MAVEN_TRUSTED_PROJECT_DIRECTORIES")
    );

    let effective = structured(
        client
            .call_tool(
                CallToolRequestParams::new("get_effective_pom").with_arguments(arguments(json!({
                    "project_path": server.project_path().unwrap().display().to_string()
                }))),
            )
            .await?,
    );
    assert_eq!(effective["status"], "available");
    assert_eq!(
        effective["projects"][0]["coordinate"]["artifact_id"],
        "fixture-project"
    );

    let dependency_tree = structured(
        client
            .call_tool(
                CallToolRequestParams::new("get_dependency_tree").with_arguments(arguments(
                    json!({
                        "project_path": server.project_path().unwrap().display().to_string()
                    }),
                )),
            )
            .await?,
    );
    assert_eq!(dependency_tree["dependencies"].as_array().unwrap().len(), 1);
    assert_eq!(
        dependency_tree["dependencies"][0]["coordinate"],
        "org.libs:helper:2.0"
    );
    assert_eq!(dependency_tree["dependencies"][0]["scope"], "compile");
    assert_eq!(dependency_tree["dependencies"][0]["depth"], 1);

    let resolution = structured(
        client
            .call_tool(
                CallToolRequestParams::new("explain_dependency_resolution").with_arguments(
                    arguments(json!({
                        "project_path": server.project_path().unwrap().display().to_string()
                    })),
                ),
            )
            .await?,
    );
    assert!(matches!(
        resolution["status"].as_str(),
        Some("available" | "incomplete")
    ));
    assert_eq!(
        resolution["explanations"][0]["artifact"],
        "org.libs:helper:jar"
    );

    let classpath = structured(
        client
            .call_tool(
                CallToolRequestParams::new("get_maven_classpath").with_arguments(arguments(
                    json!({
                        "project_path": server.project_path().unwrap().display().to_string(),
                        "kind": "test"
                    }),
                )),
            )
            .await?,
    );
    assert_eq!(classpath["artifacts"], json!(["org.libs:helper:2.0"]));

    let coverage = structured(
        client
            .call_tool(
                CallToolRequestParams::new("get_jacoco_coverage").with_arguments(arguments(
                    json!({ "project_path": server.project_path().unwrap().display().to_string() }),
                )),
            )
            .await?,
    );
    assert_eq!(coverage["status"], "available");
    assert_eq!(coverage["reports"][0]["metrics"]["lines"]["covered"], 9);

    let gaps = structured(
        client
            .call_tool(
                CallToolRequestParams::new("get_jacoco_coverage_gaps").with_arguments(arguments(
                    json!({
                        "project_path": server.project_path().unwrap().display().to_string(),
                        "limit": 1
                    }),
                )),
            )
            .await?,
    );
    assert_eq!(gaps["gaps"][0]["class_name"], "org.example.Foo");

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn offline_resolution_failure_returns_a_policy_notice() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let server = TestServer::start_with_project().await?;
    let project_path = server.project_path().unwrap();
    let wrapper = project_path.join("mvnw");
    std::fs::write(
        &wrapper,
        "#!/bin/sh\nprintf '%s\\n' '[ERROR] Cannot access central in offline mode and the artifact org.example:demo:jar:1.0 has not been downloaded from it before.'\nexit 1\n",
    )?;
    let mut permissions = wrapper.metadata()?.permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&wrapper, permissions)?;

    let client = server.connect().await?;
    let result = structured(
        client
            .call_tool(
                CallToolRequestParams::new("run_maven_lifecycle").with_arguments(arguments(
                    json!({
                        "project_path": project_path.display().to_string(),
                        "phase": "compile"
                    }),
                )),
            )
            .await?,
    );

    assert_eq!(result["outcome"], "build_failure");
    assert!(
        result["policy_notice"]
            .as_str()
            .is_some_and(|notice| notice.contains("MAVEN_EXECUTION_NETWORK=true"))
    );

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn resource_limits_are_visible_to_stdio_clients() -> Result<()> {
    let server = TestServer::start_with_project().await?;
    let project_path = server.project_path().unwrap().display().to_string();
    for (name, value) in [
        ("MAX_JAR_ENTRIES", "1"),
        ("MAX_INDEX_ENTRIES", "1"),
        ("MAX_INDEX_NAME_BYTES", "1"),
    ] {
        let (client, _) = server.connect_with_limits(&[(name, value)]).await?;
        let error = client
            .call_tool(
                CallToolRequestParams::new("search_classes").with_arguments(arguments(
                    json!({"project_path": project_path, "query": "Foo"}),
                )),
            )
            .await
            .expect_err("a limit must fail the tool rather than return partial results");
        assert!(error.to_string().contains(name), "{error}");
        client.cancel().await?;
    }
    Ok(())
}

#[tokio::test]
async fn source_search_reports_truncated_sources_with_and_without_matches() -> Result<()> {
    let server = TestServer::start_with_project().await?;
    server.replace_helper_sources(&[
        (
            "org/example/Foo.java",
            b"package org.example; public class Foo { int scoped = 2; }",
        ),
        (
            "org/example/Resource.java",
            b"class Resource { int marker; }",
        ),
    ])?;
    let project_path = server.project_path().unwrap().display().to_string();
    let search = |limit: &'static str, query: &'static str| {
        let project_path = project_path.clone();
        let server = &server;
        async move {
            let (client, _) = server
                .connect_with_limits(&[("MAX_SOURCE_BYTES", limit)])
                .await?;
            let result =
                client
                    .call_tool(CallToolRequestParams::new("search_source").with_arguments(
                        arguments(json!({"project_path": project_path, "query": query})),
                    ))
                    .await?;
            client.cancel().await?;
            Ok::<_, anyhow::Error>(structured(result))
        }
    };

    // Foo.java (57 bytes) and Resource.java (30 bytes) both fit in 64 bytes.
    let complete = search("64", "no-such-text").await?;
    assert_eq!(complete["results"], json!([]));
    assert_eq!(complete["incomplete"], false);

    // At 24 bytes Resource.java ends inside "marker", so nothing matches.
    let after_limit = search("24", "marker").await?;
    assert_eq!(after_limit["results"], json!([]));
    assert_eq!(after_limit["incomplete"], true);

    // A match in a fully read source does not hide the truncated Foo.java.
    let mixed = search("40", "marker").await?;
    let matches = mixed["results"].as_array().unwrap();
    assert_eq!(matches.len(), 1, "{mixed}");
    assert_eq!(matches[0]["truncated"], false);
    assert_eq!(mixed["incomplete"], true);
    Ok(())
}

#[test]
fn resource_limit_configuration_rejects_unbounded_values() {
    for name in [
        "MAX_XML_BYTES",
        "MAX_JAR_ENTRIES",
        "MAX_INDEX_ENTRIES",
        "MAX_INDEX_NAME_BYTES",
    ] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_maven-mcp"))
            .env(name, usize::MAX.to_string())
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains(name));
    }
}

#[tokio::test]
async fn pipe_timeouts_clean_up_and_release_the_global_permit() -> Result<()> {
    use std::time::{Duration, Instant};

    for redirections in ["", ">/dev/null", "2>/dev/null"] {
        let server = TestServer::start_with_project().await?;
        let project = server.project_path().unwrap();
        let holder = support::PipeHolder::new(project, redirections)?;
        let other = server.add_trusted_project("after-timeout")?;
        std::fs::write(
            other.join("mvnw"),
            "#!/bin/sh\ntouch started\necho recovered\n",
        )?;
        let (client, _) = server
            .connect_with_limits(&[("MAVEN_TIMEOUT_SECONDS", "1")])
            .await?;
        let first = CallToolRequestParams::new("run_maven").with_arguments(arguments(json!({
            "project_path": project, "arguments": []
        })));
        let second = CallToolRequestParams::new("run_maven").with_arguments(arguments(json!({
            "project_path": other, "arguments": []
        })));
        let started = Instant::now();
        let first_call = client.call_tool(first);
        let second_call = async {
            holder.wait_started().await?;
            let call = client.call_tool(second.clone());
            tokio::pin!(call);
            tokio::select! {
                result = &mut call => panic!("second project ran before timeout: {result:?}"),
                _ = tokio::time::sleep(Duration::from_millis(150)) => {}
            }
            assert!(
                !other.join("started").exists(),
                "global permit released early"
            );
            call.await.map_err(anyhow::Error::from)
        };
        let (first, second) = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(first_call, second_call)
        })
        .await?;
        assert!(started.elapsed() < Duration::from_secs(3));
        let first = structured(first?);
        assert_eq!(first["status"], "timeout", "{first}");
        assert_eq!(first["timed_out"], true);
        holder.assert_stopped().await?;
        let second = structured(second?);
        assert_eq!(second["status"], "success", "{second}");
        assert!(second["stdout"].as_str().unwrap().contains("recovered"));
        let after = tokio::time::timeout(
            Duration::from_secs(3),
            client.call_tool(
                CallToolRequestParams::new("run_maven").with_arguments(arguments(json!({
                    "project_path": other, "arguments": ["after-timeout"]
                }))),
            ),
        )
        .await??;
        assert_eq!(structured(after)["status"], "success");
        client.cancel().await?;
    }
    Ok(())
}

#[tokio::test]
async fn classfile_tools_agree_on_the_effective_multi_release_variant() -> Result<()> {
    let server = TestServer::start_with_multi_release_project().await?;
    let project_path = server.project_path().unwrap().display().to_string();
    let client = server.connect().await?;
    let call = |tool: &'static str, extra: Value| {
        let mut arguments = arguments(extra);
        arguments.insert("project_path".to_owned(), json!(project_path));
        let client = &client;
        async move {
            Ok::<_, anyhow::Error>(structured(
                client
                    .call_tool(CallToolRequestParams::new(tool).with_arguments(arguments))
                    .await?,
            ))
        }
    };
    let count = |value: &Value| value["results"].as_array().map_or(0, Vec::len);

    let described = call(
        "describe_class",
        json!({"class_name": "org.mr.Shape", "jar": "org.libs:multi-release:1.0"}),
    )
    .await?;
    assert_eq!(count(&described), 1);
    let shape = &described["results"][0];
    assert_eq!(shape["super_class"], "java.lang.Number");
    let fields = shape["fields"].as_array().unwrap();
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0]["name"], "java17x");

    let hierarchy = |parent: &'static str, jar: &'static str| {
        call(
            "search_type_hierarchy",
            json!({"type_name": parent, "jar": jar}),
        )
    };
    assert_eq!(
        count(&hierarchy("java.lang.Number", "org.libs:multi-release:1.0").await?),
        1
    );
    let object_children = hierarchy("java.lang.Object", "org.libs:multi-release:1.0").await?;
    assert_eq!(
        object_children["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|child| child["type_name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["org.mr.VersionOnly"]
    );

    for (query, expected) in [("java17x", 1), ("java11x", 0), ("baseval", 0)] {
        let members = call(
            "search_class_members",
            json!({"query": query, "jar": "org.libs:multi-release:1.0"}),
        )
        .await?;
        assert_eq!(count(&members), expected, "member query {query}");
    }

    let references = call(
        "search_class_references",
        json!({
            "class_name": "org.mr.Shape",
            "direction": "outbound",
            "kind": "class",
            "jar": "org.libs:multi-release:1.0"
        }),
    )
    .await?;
    let targets = references["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|reference| reference["target_owner"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(targets.contains(&"java.lang.Number"), "{targets:?}");
    assert!(!targets.contains(&"java.lang.Object"), "{targets:?}");

    let listed = call(
        "list_jar_classes",
        json!({"jar": "org.libs:multi-release:1.0"}),
    )
    .await?;
    assert_eq!(
        listed["results"][0]["classes"],
        json!(["org.mr.Shape", "org.mr.VersionOnly"])
    );

    let diff = call(
        "compare_artifact_api",
        json!({
            "group_id": "org.libs",
            "artifact_id": "multi-release",
            "previous_version": "1.0",
            "current_version": "2.0"
        }),
    )
    .await?;
    assert_eq!(diff["added_classes"], json!([]));
    assert_eq!(diff["removed_classes"], json!(["org.mr.VersionOnly"]));
    let changed = &diff["changed_classes"][0];
    assert_eq!(changed["class_name"], "org.mr.Shape");
    assert_eq!(changed["previous_super_class"], "java.lang.Number");
    assert_eq!(changed["current_super_class"], "java.lang.Object");
    assert_eq!(changed["removed_members"], json!(["field:java17x:I"]));
    assert_eq!(changed["added_members"], json!(["field:baseval:I"]));

    // Control: no `Multi-Release: true` manifest, so every versioned entry is ignored.
    let plain = call(
        "describe_class",
        json!({"class_name": "org.mr.Shape", "jar": "org.libs:plain-release:1.0"}),
    )
    .await?;
    assert_eq!(plain["results"][0]["super_class"], "java.lang.Object");
    assert_eq!(plain["results"][0]["fields"][0]["name"], "baseval");
    assert_eq!(
        count(&hierarchy("java.lang.Number", "org.libs:plain-release:1.0").await?),
        0
    );
    let version_only = call(
        "describe_class",
        json!({"class_name": "org.mr.VersionOnly", "jar": "org.libs:plain-release:1.0"}),
    )
    .await?;
    assert_eq!(count(&version_only), 0);

    client.cancel().await?;
    Ok(())
}

type Client = rmcp::service::RunningService<rmcp::RoleClient, ()>;

async fn call(client: &Client, tool: &str, value: Value) -> Result<Value> {
    Ok(structured(
        client
            .call_tool(CallToolRequestParams::new(tool.to_owned()).with_arguments(arguments(value)))
            .await?,
    ))
}

async fn call_error(client: &Client, tool: &str, value: Value) -> String {
    client
        .call_tool(CallToolRequestParams::new(tool.to_owned()).with_arguments(arguments(value)))
        .await
        .expect_err("the tool call must fail")
        .to_string()
}

async fn class_names(client: &Client, project: &Path, query: &str) -> Result<BTreeSet<String>> {
    let found = call(
        client,
        "search_classes",
        json!({ "project_path": project.display().to_string(), "query": query }),
    )
    .await?;
    Ok(found["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|result| result["class_name"].as_str().unwrap().to_owned())
        .collect())
}

fn names(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn repository_jar(project: &Path, artifact: &str) -> PathBuf {
    project.join(format!(
        "execution-repository/org/t/{artifact}/1.0/{artifact}-1.0.jar"
    ))
}

fn class_list(classes: &[&str]) -> Vec<String> {
    classes.iter().map(|name| (*name).to_owned()).collect()
}

fn runtime_status(runtime_dir: &Path, pid: u32) -> Result<Value> {
    Ok(serde_json::from_slice(&std::fs::read(
        runtime_dir.join(format!("{pid}.json")),
    )?)?)
}

/// A project driven by [`support::CONTROLLED_WRAPPER`] whose classpath holds `alpha`.
async fn controlled_project() -> Result<(TestServer, PathBuf)> {
    let server = TestServer::start_with_project().await?;
    let project = server.project_path().unwrap().to_owned();
    support::write_class_jar(
        &repository_jar(&project, "alpha"),
        &class_list(&["org/t/AlphaOne"]),
    )?;
    support::write_class_jar(
        &repository_jar(&project, "beta"),
        &class_list(&["org/t/BetaOne"]),
    )?;
    support::install_controlled_wrapper(&project, &[repository_jar(&project, "alpha")])?;
    Ok((server, project))
}

/// Extra trusted projects whose classpath is the shared execution repository's `alpha` JAR.
fn extra_projects(server: &TestServer, count: usize) -> Result<Vec<PathBuf>> {
    let first = server.project_path().unwrap();
    support::write_class_jar(
        &repository_jar(first, "alpha"),
        &class_list(&["org/t/AlphaOne"]),
    )?;
    let mut projects = vec![first.to_owned()];
    for number in 1..count {
        projects.push(server.add_trusted_project(&format!("p{number}"))?);
    }
    for project in &projects {
        support::install_controlled_wrapper(project, &[repository_jar(first, "alpha")])?;
    }
    Ok(projects)
}

const LONG_MAVEN: &[(&str, &str)] = &[("MAVEN_TIMEOUT_SECONDS", "30")];

#[tokio::test]
async fn scoped_index_is_reused_until_a_relevant_file_or_maven_run_changes_it() -> Result<()> {
    let (server, project) = controlled_project().await?;
    let (client, _) = server.connect_with_limits(LONG_MAVEN).await?;

    assert_eq!(
        class_names(&client, &project, "One").await?,
        names(&["org.t.AlphaOne"])
    );
    assert_eq!(
        class_names(&client, &project, "One").await?,
        names(&["org.t.AlphaOne"])
    );
    assert_eq!(support::controlled_classpath_runs(&project), 1, "reuse");

    // A changed classpath JAR is detected without any Maven execution.
    support::write_class_jar(
        &repository_jar(&project, "alpha"),
        &class_list(&["org/t/AlphaOne", "org/t/AlphaTwo"]),
    )?;
    assert_eq!(
        class_names(&client, &project, "Alpha").await?,
        names(&["org.t.AlphaOne", "org.t.AlphaTwo"])
    );
    assert_eq!(
        support::controlled_classpath_runs(&project),
        2,
        "JAR change"
    );

    // A POM edit invalidates the index and re-reads the model.
    let pom = project.join("pom.xml");
    let edited = std::fs::read_to_string(&pom)?.replace("</project>", "<!-- edit --></project>");
    std::fs::write(&pom, edited)?;
    class_names(&client, &project, "Alpha").await?;
    assert_eq!(
        support::controlled_classpath_runs(&project),
        3,
        "POM change"
    );

    // So does a wrapper change, and the new classpath is what gets indexed.
    support::set_controlled_classpath(
        &project,
        &[
            repository_jar(&project, "alpha"),
            repository_jar(&project, "beta"),
        ],
    )?;
    let wrapper = project.join("mvnw");
    let script = std::fs::read_to_string(&wrapper)?;
    std::fs::write(&wrapper, format!("{script}# edited wrapper\n"))?;
    assert_eq!(
        class_names(&client, &project, "One").await?,
        names(&["org.t.AlphaOne", "org.t.BetaOne"])
    );
    assert_eq!(
        support::controlled_classpath_runs(&project),
        4,
        "wrapper change"
    );

    // Files Maven itself rewrites never change a stamp we track, so run_maven is explicit.
    support::set_controlled_classpath(&project, &[repository_jar(&project, "beta")])?;
    assert_eq!(class_names(&client, &project, "One").await?.len(), 2);
    assert_eq!(support::controlled_classpath_runs(&project), 4);
    call(
        &client,
        "run_maven",
        json!({ "project_path": project.display().to_string(), "arguments": ["-v"] }),
    )
    .await?;
    assert_eq!(
        class_names(&client, &project, "One").await?,
        names(&["org.t.BetaOne"])
    );
    assert_eq!(support::controlled_classpath_runs(&project), 5, "run_maven");

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn pom_reactor_changes_reach_the_next_inspection_with_and_without_run_maven() -> Result<()> {
    let (server, project) = controlled_project().await?;
    let client = server.connect().await?;
    let inspect = || {
        call(
            &client,
            "inspect_maven_project",
            json!({ "project_path": project.display().to_string() }),
        )
    };
    let modules = |inspected: &Value| -> Vec<String> {
        inspected["modules"]
            .as_array()
            .unwrap()
            .iter()
            .map(|module| module["selector"].as_str().unwrap().to_owned())
            .collect()
    };
    assert!(modules(&inspect().await?).is_empty());

    let add_module = |name: &str, existing: &str| -> Result<()> {
        std::fs::create_dir_all(project.join(name))?;
        std::fs::write(
            project.join(name).join("pom.xml"),
            format!("<project><artifactId>{name}</artifactId></project>"),
        )?;
        std::fs::write(
            project.join("pom.xml"),
            format!(
                "<project><modelVersion>4.0.0</modelVersion><groupId>org.example</groupId><artifactId>fixture-project</artifactId><version>1.0</version><packaging>pom</packaging><modules>{existing}<module>{name}</module></modules></project>"
            ),
        )?;
        Ok(())
    };
    add_module("first", "")?;
    call(
        &client,
        "run_maven",
        json!({ "project_path": project.display().to_string(), "arguments": ["-v"] }),
    )
    .await?;
    assert_eq!(modules(&inspect().await?), ["first"]);

    // A further edit is picked up on its own, without any Maven run.
    add_module("second", "<module>first</module>")?;
    assert_eq!(modules(&inspect().await?), ["first", "second"]);

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn parallel_cold_requests_share_one_build_and_one_failure() -> Result<()> {
    let (server, project) = controlled_project().await?;
    let (client, _) = server.connect_with_limits(LONG_MAVEN).await?;
    std::fs::write(project.join("slow"), "")?;

    let (first, second, third, fourth) = tokio::join!(
        class_names(&client, &project, "One"),
        class_names(&client, &project, "One"),
        class_names(&client, &project, "alpha"),
        class_names(&client, &project, "One"),
    );
    for found in [first?, second?, fourth?] {
        assert_eq!(found, names(&["org.t.AlphaOne"]));
    }
    assert_eq!(third?, names(&["org.t.AlphaOne"]));
    assert_eq!(support::controlled_classpath_runs(&project), 1);

    // A failing build is shared by every waiter and does not poison the retry.
    call(
        &client,
        "run_maven",
        json!({ "project_path": project.display().to_string(), "arguments": ["-v"] }),
    )
    .await?;
    std::fs::write(project.join("fail-classpath"), "")?;
    let project_path = project.display().to_string();
    let failures = tokio::join!(
        call_error(
            &client,
            "search_classes",
            json!({ "project_path": project_path, "query": "One" })
        ),
        call_error(
            &client,
            "search_classes",
            json!({ "project_path": project_path, "query": "One" })
        ),
        call_error(
            &client,
            "search_classes",
            json!({ "project_path": project_path, "query": "One" })
        ),
    );
    for message in [&failures.0, &failures.1, &failures.2] {
        assert!(
            message.contains("classpath resolution failed on purpose"),
            "{message}"
        );
    }
    assert_eq!(support::controlled_classpath_runs(&project), 2);

    std::fs::remove_file(project.join("fail-classpath"))?;
    assert_eq!(
        class_names(&client, &project, "One").await?,
        names(&["org.t.AlphaOne"])
    );
    assert_eq!(support::controlled_classpath_runs(&project), 3);

    client.cancel().await?;
    Ok(())
}

async fn invalidate_while_building(trigger_with_request: bool) -> Result<()> {
    let (server, project) = controlled_project().await?;
    let (client, _) = server.connect_with_limits(LONG_MAVEN).await?;
    std::fs::write(project.join("gate"), "")?;

    let building = async { class_names(&client, &project, "BetaOne").await };
    let changing = async {
        let started = project.join("classpath-started");
        for _ in 0..400 {
            if started.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(started.exists(), "the first Maven build never started");
        support::set_controlled_classpath(
            &project,
            &[
                repository_jar(&project, "alpha"),
                repository_jar(&project, "beta"),
            ],
        )?;
        let pom = project.join("pom.xml");
        let edited =
            std::fs::read_to_string(&pom)?.replace("</project>", "<!-- edit --></project>");
        std::fs::write(&pom, edited)?;
        if trigger_with_request {
            call(
                &client,
                "inspect_maven_project",
                json!({ "project_path": project.display().to_string() }),
            )
            .await?;
        }
        std::fs::remove_file(project.join("gate"))?;
        anyhow::Ok(())
    };
    let (found, changed) = tokio::join!(building, changing);
    changed?;

    // The build that started before the edit saw only `alpha`; it must not be published or
    // returned, so the request completes from the second, fresh build.
    assert_eq!(found?, names(&["org.t.BetaOne"]));
    assert_eq!(support::controlled_classpath_runs(&project), 2);
    assert_eq!(
        class_names(&client, &project, "One").await?,
        names(&["org.t.AlphaOne", "org.t.BetaOne"])
    );
    assert_eq!(support::controlled_classpath_runs(&project), 2);

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn invalidation_during_a_build_discards_the_stale_index() -> Result<()> {
    invalidate_while_building(true).await
}

#[tokio::test]
async fn project_files_changing_during_a_build_prevent_stale_publication() -> Result<()> {
    invalidate_while_building(false).await
}

#[tokio::test]
async fn incomplete_classpath_output_is_an_error_and_never_cached_as_complete() -> Result<()> {
    let (server, project) = controlled_project().await?;
    let (client, _) = server.connect_with_limits(LONG_MAVEN).await?;
    let project_path = project.display().to_string();
    std::fs::write(project.join("classpath.txt"), "relative/alpha-1.0.jar")?;

    let message = call_error(
        &client,
        "search_classes",
        json!({ "project_path": project_path, "query": "One" }),
    )
    .await;
    assert!(message.contains("truncated or malformed"), "{message}");
    let message = call_error(
        &client,
        "index_stats",
        json!({ "project_path": project_path }),
    )
    .await;
    assert!(message.contains("truncated or malformed"), "{message}");
    assert_eq!(
        support::controlled_classpath_runs(&project),
        2,
        "nothing was cached"
    );

    support::set_controlled_classpath(&project, &[repository_jar(&project, "alpha")])?;
    assert_eq!(
        class_names(&client, &project, "One").await?,
        names(&["org.t.AlphaOne"])
    );

    client.cancel().await?;
    Ok(())
}

/// Turns the controlled project into a CI-friendly reactor: the root POM declares
/// `${revision}` and the modules `core` and `api`, which inherit it.
fn write_revision_reactor(project: &Path) -> Result<()> {
    std::fs::write(
        project.join("pom.xml"),
        "<project><modelVersion>4.0.0</modelVersion><groupId>org.example</groupId><artifactId>fixture-project</artifactId><version>${revision}</version><packaging>pom</packaging><properties><revision>1.0</revision></properties><modules><module>core</module><module>api</module></modules></project>",
    )?;
    for module in ["core", "api"] {
        std::fs::create_dir_all(project.join(module))?;
        std::fs::write(
            project.join(module).join("pom.xml"),
            format!(
                "<project><parent><groupId>org.example</groupId><artifactId>fixture-project</artifactId><version>${{revision}}</version></parent><artifactId>{module}</artifactId></project>"
            ),
        )?;
    }
    Ok(())
}

#[tokio::test]
async fn reactor_outputs_are_indexed_and_unselected_artifacts_are_not() -> Result<()> {
    let (server, project) = controlled_project().await?;
    write_revision_reactor(&project)?;
    let reactor_jar = project.join("core/target/core-1.0.jar");
    support::write_class_jar(&reactor_jar, &class_list(&["org/example/CoreOne"]))?;
    // A custom finalName archive and a compiled but unpackaged module are outputs too.
    let final_name = project.join("core/target/core-app.jar");
    support::write_class_jar(&final_name, &class_list(&["org/example/RenamedOne"]))?;
    let api_classes = project.join("api/target/classes");
    support::write_class_directory(&api_classes, &class_list(&["org/example/ApiOne"]))?;
    support::write_class_jar(
        &repository_jar(&project, "unselected"),
        &class_list(&["org/t/UnselectedOne"]),
    )?;
    support::set_controlled_classpath(
        &project,
        &[
            repository_jar(&project, "alpha"),
            reactor_jar,
            final_name,
            api_classes.clone(),
        ],
    )?;
    let client = server.connect().await?;

    assert_eq!(
        class_names(&client, &project, "One").await?,
        names(&[
            "org.example.ApiOne",
            "org.example.CoreOne",
            "org.example.RenamedOne",
            "org.t.AlphaOne"
        ])
    );
    let jars = call(
        &client,
        "search_jars",
        json!({ "project_path": project.display().to_string(), "query": "org.example" }),
    )
    .await?;
    let mut outputs = jars["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|jar| {
            (
                jar["path"].as_str().unwrap().to_owned(),
                jar["coordinate"].as_str().unwrap().to_owned(),
            )
        })
        .collect::<Vec<_>>();
    outputs.sort();
    assert_eq!(
        outputs,
        [
            ("api/target/classes", "org.example:api:1.0"),
            ("core/target/core-1.0.jar", "org.example:core:1.0"),
            ("core/target/core-app.jar", "org.example:core:1.0"),
        ]
        .map(|(path, coordinate)| (path.to_owned(), coordinate.to_owned()))
    );
    let runs = support::controlled_classpath_runs(&project);

    // Editing a nested class file of an output directory invalidates the cached index.
    let nested = api_classes.join("org/example/ApiOne.class");
    let mut bytes = std::fs::read(&nested)?;
    bytes.extend_from_slice(b"changed");
    std::fs::write(&nested, bytes)?;
    class_names(&client, &project, "One").await?;
    assert_eq!(support::controlled_classpath_runs(&project), runs + 1);
    class_names(&client, &project, "One").await?;
    assert_eq!(support::controlled_classpath_runs(&project), runs + 1);

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn selected_entries_that_cannot_be_indexed_fail_without_caching() -> Result<()> {
    let (server, project) = controlled_project().await?;
    let (client, _) = server.connect_with_limits(LONG_MAVEN).await?;
    let project_path = project.display().to_string();
    let outside = tempfile::tempdir()?;
    let outside_jar = outside.path().join("system-lib.jar");
    support::write_class_jar(&outside_jar, &class_list(&["org/system/SystemOne"]))?;

    for (classpath, expected) in [
        (
            vec![
                repository_jar(&project, "alpha"),
                repository_jar(&project, "missing"),
            ],
            "1 missing: missing-1.0.jar",
        ),
        (
            vec![repository_jar(&project, "alpha"), outside_jar.clone()],
            "1 outside the local repository and the reactor build directories: system-lib.jar",
        ),
    ] {
        support::set_controlled_classpath(&project, &classpath)?;
        let runs = support::controlled_classpath_runs(&project);
        for tool in ["search_classes", "index_stats"] {
            let message = call_error(
                &client,
                tool,
                json!({ "project_path": project_path, "query": "One" }),
            )
            .await;
            assert!(message.contains(expected), "{message}");
            assert!(message.contains("would be incomplete"), "{message}");
            assert!(
                !message.contains(&outside.path().display().to_string()),
                "{message}"
            );
        }
        assert_eq!(
            support::controlled_classpath_runs(&project),
            runs + 2,
            "nothing was cached"
        );
    }

    support::set_controlled_classpath(&project, &[repository_jar(&project, "alpha")])?;
    assert_eq!(
        class_names(&client, &project, "One").await?,
        names(&["org.t.AlphaOne"])
    );

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn cancelled_cold_builds_do_not_pin_runners_beyond_the_limit() -> Result<()> {
    let server = TestServer::start_with_project().await?;
    let runtime = tempfile::tempdir()?;
    let projects = extra_projects(&server, 7)?;
    let (client, pid) = server
        .connect_with_limits(&[
            ("MAX_PROJECT_INDEXES", "1"),
            ("MAVEN_TIMEOUT_SECONDS", "30"),
            ("MAVEN_MCP_RUNTIME_DIR", runtime.path().to_str().unwrap()),
        ])
        .await?;

    for project in &projects {
        std::fs::write(project.join("gate"), "")?;
        let handle = client
            .send_cancellable_request(
                rmcp::model::ClientRequest::CallToolRequest(rmcp::model::CallToolRequest::new(
                    CallToolRequestParams::new("search_classes").with_arguments(arguments(json!({
                        "project_path": project.display().to_string(),
                        "query": "One",
                    }))),
                )),
                rmcp::service::PeerRequestOptions::no_options(),
            )
            .await?;
        let started = project.join("classpath-started");
        for _ in 0..400 {
            if started.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(started.exists(), "the cold build never started");
        handle.cancel(Some("regression".to_owned())).await?;
    }
    for project in &projects {
        std::fs::remove_file(project.join("gate"))?;
    }

    // The next request re-applies the limits; cancelled builds no longer hold their runners.
    assert_eq!(class_names(&client, &projects[0], "One").await?.len(), 1);
    let cache = runtime_status(runtime.path(), pid)?["cache"].clone();
    assert!(cache["runners"].as_u64().unwrap() <= 4, "{cache}");
    assert!(cache["entries"].as_u64().unwrap() <= 1, "{cache}");
    assert!(cache["retained_bytes"].as_u64().unwrap() <= cache["max_bytes"].as_u64().unwrap());

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn runner_and_index_caches_stay_bounded_while_switching_projects() -> Result<()> {
    let server = TestServer::start_with_project().await?;
    let runtime = tempfile::tempdir()?;
    let projects = extra_projects(&server, 7)?;
    let (client, pid) = server
        .connect_with_limits(&[
            ("MAX_PROJECT_INDEXES", "1"),
            ("MAVEN_MCP_RUNTIME_DIR", runtime.path().to_str().unwrap()),
        ])
        .await?;

    let mut plateau = Vec::new();
    for round in 0..4 {
        for project in &projects {
            assert_eq!(class_names(&client, project, "One").await?.len(), 1);
        }
        let cache = runtime_status(runtime.path(), pid)?["cache"].clone();
        assert!(cache["entries"].as_u64().unwrap() <= 1, "{cache}");
        assert!(cache["runners"].as_u64().unwrap() <= 4, "{cache}");
        assert!(cache["retained_bytes"].as_u64().unwrap() <= cache["max_bytes"].as_u64().unwrap());
        plateau.push((
            cache["entries"].clone(),
            cache["runners"].clone(),
            cache["retained_bytes"].clone(),
        ));
        assert!(
            cache["evictions"].as_u64().unwrap() >= 6 * (round + 1) - 1,
            "{cache}"
        );
    }
    assert!(
        plateau.windows(2).all(|pair| pair[0] == pair[1]),
        "{plateau:?}"
    );

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn byte_budget_evicts_indexes_and_rejects_builds_that_cannot_fit() -> Result<()> {
    let server = TestServer::start_with_project().await?;
    let projects = extra_projects(&server, 2)?;
    let (first, second) = (projects[0].clone(), projects[1].clone());
    let runtime = tempfile::tempdir()?;
    let runtime_dir = runtime.path().to_str().unwrap();

    let (client, pid) = server
        .connect_with_limits(&[("MAVEN_MCP_RUNTIME_DIR", runtime_dir)])
        .await?;
    class_names(&client, &first, "One").await?;
    let one_project = runtime_status(runtime.path(), pid)?["cache"]["retained_bytes"]
        .as_u64()
        .unwrap();
    client.cancel().await?;

    // Room for one project (runner and index) plus slack, not for two.
    let limit = (one_project + 2_000).to_string();
    let (client, pid) = server
        .connect_with_limits(&[
            ("MAVEN_MCP_RUNTIME_DIR", runtime_dir),
            ("MAX_PROJECT_INDEX_BYTES", &limit),
        ])
        .await?;
    class_names(&client, &first, "One").await?;
    class_names(&client, &second, "One").await?;
    class_names(&client, &first, "One").await?;
    let cache = runtime_status(runtime.path(), pid)?["cache"].clone();
    assert_eq!(cache["entries"], 1, "{cache}");
    assert!(cache["evictions"].as_u64().unwrap() >= 2, "{cache}");
    assert!(
        cache["retained_bytes"].as_u64().unwrap() <= one_project + 2_000,
        "{cache}"
    );
    client.cancel().await?;

    // A budget below even one runner cannot hold a build: the error names the limit.
    let (client, _) = server
        .connect_with_limits(&[("MAX_PROJECT_INDEX_BYTES", "1")])
        .await?;
    let message = call_error(
        &client,
        "search_classes",
        json!({ "project_path": first.display().to_string(), "query": "One" }),
    )
    .await;
    assert!(message.contains("MAX_PROJECT_INDEX_BYTES"), "{message}");
    client.cancel().await?;
    Ok(())
}

/// Records the cost profile of a project index on a generated fixture. Run with
/// `cargo test --test mcp_interface --locked measure_ -- --nocapture` to see the figures.
#[tokio::test]
async fn measure_cold_reuse_invalidation_and_eviction_costs() -> Result<()> {
    const JARS: usize = 3;
    const CLASSES_PER_JAR: usize = 3_000;
    const REUSE_REPETITIONS: u32 = 20;

    let server = TestServer::start_with_project().await?;
    let projects = extra_projects(&server, 2)?;
    let first = projects[0].clone();
    let classpath = (0..JARS)
        .map(|jar| {
            let path = repository_jar(&first, &format!("bulk{jar}"));
            let classes = (0..CLASSES_PER_JAR)
                .map(|class| format!("org/t/bulk{jar}/Generated{class}"))
                .collect::<Vec<_>>();
            support::write_class_jar(&path, &classes).map(|()| path)
        })
        .collect::<Result<Vec<_>>>()?;
    for project in &projects {
        support::set_controlled_classpath(project, &classpath)?;
    }
    let runtime = tempfile::tempdir()?;
    let (client, pid) = server
        .connect_with_limits(&[
            ("MAX_PROJECT_INDEXES", "1"),
            ("MAVEN_MCP_RUNTIME_DIR", runtime.path().to_str().unwrap()),
        ])
        .await?;
    let status = || runtime_status(runtime.path(), pid);
    let rss = |status: &Value, key: &str| status["memory"][key].as_u64().unwrap_or_default();
    let timed = |label: &'static str| (label, std::time::Instant::now());
    let search = |project: &Path| {
        call(
            &client,
            "search_classes",
            json!({ "project_path": project.display().to_string(), "query": "Generated2999", "limit": 50 }),
        )
    };

    call(
        &client,
        "inspect_maven_project",
        json!({ "project_path": first.display().to_string() }),
    )
    .await?;
    let baseline_rss = rss(&status()?, "rss_bytes");

    let (_, started) = timed("cold");
    assert_eq!(
        search(&first).await?["results"].as_array().unwrap().len(),
        JARS
    );
    let cold = started.elapsed();
    let built = status()?;
    let estimated = built["projects"][0]["estimated_bytes"].as_u64().unwrap();
    let steady_rss = rss(&built, "rss_bytes");
    let peak_rss = rss(&built, "peak_rss_bytes");

    let (_, started) = timed("reuse");
    for _ in 0..REUSE_REPETITIONS {
        search(&first).await?;
    }
    let reuse = started.elapsed() / REUSE_REPETITIONS;
    assert_eq!(
        support::controlled_classpath_runs(&first),
        1,
        "reuse ran Maven"
    );

    call(
        &client,
        "run_maven",
        json!({ "project_path": first.display().to_string(), "arguments": ["-v"] }),
    )
    .await?;
    let (_, started) = timed("rebuild");
    search(&first).await?;
    let rebuild = started.elapsed();
    assert_eq!(support::controlled_classpath_runs(&first), 2);

    let (_, started) = timed("evicting");
    search(&projects[1]).await?;
    let evicting = started.elapsed();
    let evicted = status()?;
    assert_eq!(evicted["cache"]["entries"], 1);
    assert!(evicted["cache"]["evictions"].as_u64().unwrap() >= 1);

    assert!(estimated > 0 && peak_rss >= steady_rss && steady_rss > 0);
    eprintln!(
        "MEASUREMENT fixture_jars={JARS} classes_per_jar={CLASSES_PER_JAR} reuse_repetitions={REUSE_REPETITIONS} \
         cold_ms={} reuse_ms={:.3} rebuild_after_run_maven_ms={} evicting_build_ms={} \
         estimated_index_bytes={estimated} baseline_rss_bytes={baseline_rss} steady_rss_bytes={steady_rss} \
         peak_rss_bytes={peak_rss} retained_bytes_after_eviction={}",
        cold.as_millis(),
        reuse.as_secs_f64() * 1000.0,
        rebuild.as_millis(),
        evicting.as_millis(),
        evicted["cache"]["retained_bytes"],
    );

    client.cancel().await?;
    Ok(())
}

/// Member search over a JAR with many classes returns the sorted, limited result and records
/// its cost. Run with `cargo test --test mcp_interface --locked measure_ -- --nocapture`; the
/// number of opens of the bulk JAR is counted outside the test (`strace -f -e trace=openat`).
#[tokio::test]
async fn measure_member_search_over_a_jar_with_many_classes() -> Result<()> {
    const CLASSES: usize = 3_000;
    const LIMIT: usize = 50;

    let server = TestServer::start_with_project().await?;
    let projects = extra_projects(&server, 1)?;
    let project = projects[0].clone();
    let bulk = repository_jar(&project, "bulkmembers");
    let classes = (0..CLASSES)
        .map(|class| format!("org/t/bulk/Generated{class:04}"))
        .collect::<Vec<_>>();
    support::write_class_jar(&bulk, &classes)?;
    support::set_controlled_classpath(&project, &[repository_jar(&project, "alpha"), bulk])?;
    let runtime = tempfile::tempdir()?;
    let (client, pid) = server
        .connect_with_limits(&[("MAVEN_MCP_RUNTIME_DIR", runtime.path().to_str().unwrap())])
        .await?;
    let project_path = project.display().to_string();
    let members = |query: &str, jar: Option<&str>| {
        let mut arguments = json!({ "project_path": project_path, "query": query, "limit": LIMIT });
        if let Some(jar) = jar {
            arguments["jar"] = json!(jar);
        }
        call(&client, "search_class_members", arguments)
    };

    // Warm the project index so that only the member search is timed.
    call(
        &client,
        "search_classes",
        json!({ "project_path": project_path, "query": "Generated0000" }),
    )
    .await?;
    let before_rss = runtime_status(runtime.path(), pid)?["memory"]["rss_bytes"]
        .as_u64()
        .unwrap_or_default();

    let started = std::time::Instant::now();
    let found = members("value", Some("org.t:bulkmembers:1.0")).await?;
    let elapsed = started.elapsed();
    let results = found["results"].as_array().unwrap();
    assert_eq!(results.len(), LIMIT);
    let class_names = results
        .iter()
        .map(|result| result["class_name"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    let expected = (0..LIMIT)
        .map(|class| format!("org.t.bulk.Generated{class:04}"))
        .collect::<Vec<_>>();
    assert_eq!(class_names, expected);
    assert!(results.iter().all(|result| result["name"] == "value"));

    // Every class of the other JAR is also visited when no JAR is selected, and the bulk JAR
    // still sorts after the single-class one by coordinate.
    let all = members("value", None).await?;
    assert_eq!(all["results"].as_array().unwrap().len(), LIMIT);
    assert_eq!(all["results"][0]["jar"]["coordinate"], "org.t:alpha:1.0");

    let after = runtime_status(runtime.path(), pid)?;
    eprintln!(
        "MEASUREMENT member_search classes={CLASSES} limit={LIMIT} search_ms={} \
         rss_before_bytes={before_rss} rss_after_bytes={} peak_rss_bytes={}",
        elapsed.as_millis(),
        after["memory"]["rss_bytes"],
        after["memory"]["peak_rss_bytes"],
    );
    client.cancel().await?;
    Ok(())
}

/// Wide, deep, diamond and cyclic hierarchies in one project index: the public
/// `search_type_hierarchy` responses over real STDIO carry the documented depth, path, relation
/// and ordering semantics, and the cycle and the diamond neither loop nor duplicate beyond the
/// distinct child-to-parent edges.
#[tokio::test]
async fn type_hierarchy_search_reports_wide_deep_diamond_and_cyclic_graphs() -> Result<()> {
    const WIDE: usize = 12;
    const DEEP: usize = 12;
    let server = TestServer::start_with_project().await?;
    let projects = extra_projects(&server, 1)?;
    let project = projects[0].clone();
    let wide = (0..WIDE)
        .map(|child| format!("org/t/Wide{child:02}"))
        .collect::<Vec<_>>();
    let deep = (0..DEEP)
        .map(|level| format!("org/t/Deep{level:02}"))
        .collect::<Vec<_>>();
    let mut classes = wide
        .iter()
        .map(|name| (name.as_str(), "org/t/WideRoot", &[][..]))
        .collect::<Vec<_>>();
    classes.extend(deep.iter().enumerate().map(|(level, name)| {
        let parent = if level == 0 {
            "org/t/DeepRoot"
        } else {
            deep[level - 1].as_str()
        };
        (name.as_str(), parent, &[][..])
    }));
    classes.extend([
        ("org/t/Left", "java/lang/Object", &["org/t/Top"][..]),
        ("org/t/Right", "java/lang/Object", &["org/t/Top"][..]),
        ("org/t/Bottom", "org/t/Left", &["org/t/Right"][..]),
        ("org/t/CycleA", "org/t/CycleB", &[][..]),
        ("org/t/CycleB", "org/t/CycleA", &[][..]),
    ]);
    let hierarchy = repository_jar(&project, "hierarchy");
    support::write_hierarchy_jar(&hierarchy, &classes)?;
    support::set_controlled_classpath(&project, &[repository_jar(&project, "alpha"), hierarchy])?;
    let client = server.connect().await?;
    let project_path = project.display().to_string();
    let search = |type_name: &str, transitive: bool, limit: Option<usize>| {
        let mut arguments = json!({
            "project_path": project_path,
            "type_name": type_name,
            "transitive": transitive,
        });
        if let Some(limit) = limit {
            arguments["limit"] = json!(limit);
        }
        call(&client, "search_type_hierarchy", arguments)
    };
    let summary = |response: &Value| {
        response["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|found| {
                (
                    found["type_name"].as_str().unwrap().to_owned(),
                    found["depth"].as_u64().unwrap(),
                    found["relation"].as_str().unwrap().to_owned(),
                    found["path"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|node| node.as_str().unwrap())
                        .collect::<Vec<_>>()
                        .join(">"),
                )
            })
            .collect::<Vec<_>>()
    };

    let wide_found = summary(&search("org.t.WideRoot", true, None).await?);
    assert_eq!(wide_found.len(), WIDE);
    assert_eq!(wide_found[0].0, "org.t.Wide00");
    assert_eq!(wide_found[WIDE - 1].0, "org.t.Wide11");
    assert!(
        wide_found
            .iter()
            .all(|found| found.1 == 1 && found.2 == "extends")
    );

    let deep_found = summary(&search("org.t.DeepRoot", true, None).await?);
    assert_eq!(deep_found.len(), DEEP);
    assert_eq!(deep_found[DEEP - 1].1, DEEP as u64);
    assert_eq!(deep_found[DEEP - 1].3.split('>').count(), DEEP + 1);
    assert_eq!(
        summary(&search("org.t.DeepRoot", false, None).await?).len(),
        1
    );
    assert_eq!(
        summary(&search("org.t.DeepRoot", true, Some(3)).await?),
        deep_found[..3]
    );

    let diamond = summary(&search("org.t.Top", true, None).await?);
    assert_eq!(
        diamond,
        vec![
            (
                "org.t.Left".to_owned(),
                1,
                "implements".to_owned(),
                "org.t.Top>org.t.Left".to_owned()
            ),
            (
                "org.t.Right".to_owned(),
                1,
                "implements".to_owned(),
                "org.t.Top>org.t.Right".to_owned()
            ),
            (
                "org.t.Bottom".to_owned(),
                2,
                "extends".to_owned(),
                "org.t.Top>org.t.Left>org.t.Bottom".to_owned()
            ),
            (
                "org.t.Bottom".to_owned(),
                2,
                "implements".to_owned(),
                "org.t.Top>org.t.Right>org.t.Bottom".to_owned()
            ),
        ]
    );

    let cycle = summary(&search("org.t.CycleA", true, None).await?);
    assert_eq!(
        cycle,
        vec![(
            "org.t.CycleB".to_owned(),
            1,
            "extends".to_owned(),
            "org.t.CycleA>org.t.CycleB".to_owned()
        )]
    );
    client.cancel().await?;
    Ok(())
}

/// Measures `search_type_hierarchy` over STDIO on one wide and deep project index. Run with
/// `cargo test --test mcp_interface --locked measure_type_hierarchy -- --nocapture`.
#[tokio::test]
async fn measure_type_hierarchy_search_over_a_wide_and_deep_graph() -> Result<()> {
    const TREES: usize = 20;
    const DEPTH: usize = 20;
    const WIDTH: usize = 10;
    const REPETITIONS: u32 = 5;
    let server = TestServer::start_with_project().await?;
    let projects = extra_projects(&server, 1)?;
    let project = projects[0].clone();
    let mut names = Vec::new();
    for tree in 0..TREES {
        for level in 0..DEPTH {
            for leaf in 0..WIDTH {
                let parent = if level == 0 {
                    format!("org/t/T{tree:02}Root")
                } else {
                    format!("org/t/T{tree:02}L{:02}N00", level - 1)
                };
                names.push((format!("org/t/T{tree:02}L{level:02}N{leaf:02}"), parent));
            }
        }
    }
    let classes = names
        .iter()
        .map(|(name, parent)| (name.as_str(), parent.as_str(), &[][..]))
        .collect::<Vec<_>>();
    let measured = repository_jar(&project, "measured");
    support::write_hierarchy_jar(&measured, &classes)?;
    support::set_controlled_classpath(&project, &[repository_jar(&project, "alpha"), measured])?;
    let runtime = tempfile::tempdir()?;
    let (client, pid) = server
        .connect_with_limits(&[
            ("MAVEN_MCP_RUNTIME_DIR", runtime.path().to_str().unwrap()),
            ("MAX_RESULTS", "100000"),
        ])
        .await?;
    let project_path = project.display().to_string();
    let search = || {
        call(
            &client,
            "search_type_hierarchy",
            json!({
                "project_path": project_path,
                "type_name": "org.t.T00Root",
                "transitive": true,
                "limit": 100_000,
            }),
        )
    };
    // The first call builds the project index and is not timed.
    let first = search().await?;
    assert_eq!(first["results"].as_array().unwrap().len(), DEPTH * WIDTH);
    let started = std::time::Instant::now();
    for _ in 0..REPETITIONS {
        assert_eq!(search().await?, first);
    }
    let elapsed = started.elapsed() / REPETITIONS;
    let status = runtime_status(runtime.path(), pid)?;
    eprintln!(
        "MEASUREMENT type_hierarchy classes={} matches={} repetitions={REPETITIONS} \
         search_ms={} index_estimated_bytes={} rss_bytes={}",
        names.len(),
        DEPTH * WIDTH,
        elapsed.as_millis(),
        status["projects"][0]["estimated_bytes"],
        status["memory"]["rss_bytes"],
    );
    client.cancel().await?;
    Ok(())
}

const LIGHT_REQUEST_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

async fn wait_for_file(path: &Path) -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !path.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("{} never appeared", path.display()))
}

/// Gives a request that has no observable marker time to reach the queue of the heavy jobs.
/// Only the absence of a response or of a build is asserted afterwards, never a deadline.
async fn settle() {
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
}

/// Sends a `search_classes` request and returns a task awaiting its result. The request is
/// sent before this returns, so later requests reach the server after it.
async fn spawn_search(
    client: &Client,
    project: &Path,
) -> Result<tokio::task::JoinHandle<Result<Value>>> {
    let handle = client
        .send_cancellable_request(
            rmcp::model::ClientRequest::CallToolRequest(rmcp::model::CallToolRequest::new(
                CallToolRequestParams::new("search_classes").with_arguments(arguments(json!({
                    "project_path": project.display().to_string(),
                    "query": "One",
                }))),
            )),
            rmcp::service::PeerRequestOptions::no_options(),
        )
        .await?;
    Ok(tokio::spawn(async move {
        match handle.await_response().await? {
            rmcp::model::ServerResult::CallToolResult(result) => result
                .structured_content
                .ok_or_else(|| anyhow::anyhow!("missing structured content")),
            other => Err(anyhow::anyhow!("unexpected response {other:?}")),
        }
    }))
}

async fn within_budget<T>(what: &str, future: impl std::future::Future<Output = T>) -> Result<T> {
    tokio::time::timeout(LIGHT_REQUEST_BUDGET, future)
        .await
        .map_err(|_| anyhow::anyhow!("{what} did not respond while heavy work was blocked"))
}

#[tokio::test]
async fn blocked_heavy_work_is_bounded_and_light_requests_stay_responsive() -> Result<()> {
    let server = TestServer::start_with_project().await?;
    let projects = extra_projects(&server, 3)?;
    let blocked = support::install_blocked_classpath(&projects[0])?;
    // One runtime worker: a blocking call on the async runtime would stall every request.
    let (client, pid) = server
        .connect_with_limits(&[
            ("TOKIO_WORKER_THREADS", "1"),
            ("MAX_HEAVY_JOBS", "1"),
            ("MAX_QUEUED_HEAVY_JOBS", "1"),
            ("MAVEN_TIMEOUT_SECONDS", "30"),
        ])
        .await?;

    // The first build blocks inside the index work and occupies the only running slot.
    let running = spawn_search(&client, &projects[0]).await?;
    support::wait_for_blocked_fifo_readers(pid, 1).await?;
    // The second build is accepted and waits in the only queue slot.
    let queued = spawn_search(&client, &projects[1]).await?;
    wait_for_file(&projects[1].join("classpath-started")).await?;
    settle().await;

    // The third heavy job exceeds running + queued and is rejected instead of growing a queue.
    let rejected = within_budget(
        "the overloaded request",
        call_error(
            &client,
            "search_classes",
            json!({ "project_path": projects[2].display().to_string(), "query": "One" }),
        ),
    )
    .await?;
    assert!(rejected.contains("overloaded"), "{rejected}");
    assert!(rejected.contains("MAX_HEAVY_JOBS"), "{rejected}");

    // Independent light protocol requests are answered while the heavy work is still blocked.
    for _ in 0..3 {
        let tools = within_budget("tools/list", client.list_tools(None)).await??;
        assert!(!tools.tools.is_empty());
        for project in &projects {
            let model = within_budget(
                "inspect_maven_project",
                call(
                    &client,
                    "inspect_maven_project",
                    json!({ "project_path": project.display().to_string() }),
                ),
            )
            .await??;
            assert!(model.is_object());
        }
    }
    assert!(!running.is_finished() && !queued.is_finished());

    // Unblocking lets the running and the queued job finish; the rejected one can be retried.
    blocked.release().await?;
    for handle in [running, queued] {
        let found = handle.await??;
        assert!(
            found["results"]
                .as_array()
                .unwrap()
                .iter()
                .any(|result| result["class_name"] == "org.t.AlphaOne")
        );
    }
    assert_eq!(
        class_names(&client, &projects[2], "One").await?,
        names(&["org.t.AlphaOne"])
    );

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn cancelled_heavy_work_is_not_published_and_keeps_its_slot_until_it_stops() -> Result<()> {
    let server = TestServer::start_with_project().await?;
    let runtime = tempfile::tempdir()?;
    let projects = extra_projects(&server, 2)?;
    let blocked = support::install_blocked_classpath(&projects[0])?;
    let (client, pid) = server
        .connect_with_limits(&[
            ("MAX_HEAVY_JOBS", "1"),
            ("MAX_QUEUED_HEAVY_JOBS", "1"),
            ("MAVEN_TIMEOUT_SECONDS", "30"),
            ("MAVEN_MCP_RUNTIME_DIR", runtime.path().to_str().unwrap()),
        ])
        .await?;

    let handle = client
        .send_cancellable_request(
            rmcp::model::ClientRequest::CallToolRequest(rmcp::model::CallToolRequest::new(
                CallToolRequestParams::new("search_classes").with_arguments(arguments(json!({
                    "project_path": projects[0].display().to_string(),
                    "query": "One",
                }))),
            )),
            rmcp::service::PeerRequestOptions::no_options(),
        )
        .await?;
    support::wait_for_blocked_fifo_readers(pid, 1).await?;
    handle.cancel(Some("regression".to_owned())).await?;

    // The abandoned blocking thread cannot be interrupted inside its system call, so it keeps
    // the only running slot: the next job waits instead of adding a second thread.
    let next = spawn_search(&client, &projects[1]).await?;
    settle().await;
    assert!(
        !next.is_finished(),
        "the cancelled job released its slot early"
    );

    blocked.release().await?;
    let found = tokio::time::timeout(std::time::Duration::from_secs(10), next).await???;
    assert_eq!(found["results"].as_array().unwrap().len(), 1);

    // The cancelled build finished without publishing: only the second project is cached.
    let cache = runtime_status(runtime.path(), pid)?["cache"].clone();
    assert_eq!(cache["entries"], 1, "{cache}");
    assert_eq!(support::controlled_classpath_runs(&projects[0]), 1);
    blocked.replace_with_regular_jar()?;
    assert_eq!(
        class_names(&client, &projects[0], "One").await?,
        names(&["org.t.AlphaOne", "org.t.BlockedOne"])
    );
    assert_eq!(
        support::controlled_classpath_runs(&projects[0]),
        2,
        "the cancelled build must not have been cached"
    );

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn a_retry_of_cancelled_heavy_work_waits_for_its_build_to_stop() -> Result<()> {
    let server = TestServer::start_with_project().await?;
    let project = extra_projects(&server, 1)?.remove(0);
    let blocked = support::install_blocked_classpath(&project)?;
    // Two running slots: the retry could start a second build beside the abandoned one.
    let (client, pid) = server
        .connect_with_limits(&[
            ("MAX_HEAVY_JOBS", "2"),
            ("MAX_QUEUED_HEAVY_JOBS", "2"),
            ("MAVEN_TIMEOUT_SECONDS", "30"),
        ])
        .await?;

    let handle = client
        .send_cancellable_request(
            rmcp::model::ClientRequest::CallToolRequest(rmcp::model::CallToolRequest::new(
                CallToolRequestParams::new("search_classes").with_arguments(arguments(json!({
                    "project_path": project.display().to_string(),
                    "query": "One",
                }))),
            )),
            rmcp::service::PeerRequestOptions::no_options(),
        )
        .await?;
    support::wait_for_blocked_fifo_readers(pid, 1).await?;
    handle.cancel(Some("regression".to_owned())).await?;

    // The retry joins the same generation and waits for the abandoned work to stop: it runs
    // neither Maven nor a second index build meanwhile.
    let retry = spawn_search(&client, &project).await?;
    within_budget("tools/list", client.list_tools(None)).await??;
    settle().await;
    assert_eq!(support::controlled_classpath_runs(&project), 1);
    assert_eq!(support::blocked_fifo_readers(pid)?, 1);
    assert!(!retry.is_finished());

    blocked.replace_with_regular_jar()?;
    blocked.release().await?;
    let found = tokio::time::timeout(std::time::Duration::from_secs(10), retry).await???;
    let found = found["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|result| result["class_name"].as_str().unwrap().to_owned())
        .collect::<BTreeSet<_>>();
    assert_eq!(found, names(&["org.t.AlphaOne", "org.t.BlockedOne"]));
    assert_eq!(
        support::controlled_classpath_runs(&project),
        2,
        "exactly one build ran after the cancelled one stopped"
    );

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn warm_index_validation_runs_as_heavy_work() -> Result<()> {
    let server = TestServer::start_with_project().await?;
    let projects = extra_projects(&server, 2)?;
    let blocked = support::install_blocked_classpath(&projects[0])?;
    let warm = &projects[1];
    let gamma = repository_jar(&projects[0], "gamma");
    support::write_class_jar(&gamma, &class_list(&["org/t/GammaOne"]))?;
    support::set_controlled_classpath(warm, std::slice::from_ref(&gamma))?;
    let (client, pid) = server
        .connect_with_limits(&[
            ("TOKIO_WORKER_THREADS", "1"),
            ("MAX_HEAVY_JOBS", "1"),
            ("MAX_QUEUED_HEAVY_JOBS", "1"),
            ("MAVEN_TIMEOUT_SECONDS", "30"),
        ])
        .await?;
    assert_eq!(
        class_names(&client, warm, "One").await?,
        names(&["org.t.GammaOne"])
    );

    let running = spawn_search(&client, &projects[0]).await?;
    support::wait_for_blocked_fifo_readers(pid, 1).await?;
    // The warm request must validate its cached index in the pool, so it waits in the queue.
    let validating = spawn_search(&client, warm).await?;
    within_budget("tools/list", client.list_tools(None)).await??;
    let rejected = within_budget(
        "the overloaded request",
        call_error(
            &client,
            "get_jacoco_coverage",
            json!({ "project_path": warm.display().to_string() }),
        ),
    )
    .await?;
    assert!(rejected.contains("overloaded"), "{rejected}");
    let tools = within_budget("tools/list", client.list_tools(None)).await??;
    assert!(!tools.tools.is_empty());

    // An artifact that changes while the validation waits is seen by that validation.
    support::write_class_jar(&gamma, &class_list(&["org/t/GammaOne", "org/t/FreshOne"]))?;
    blocked.release().await?;
    running.await??;
    let found = tokio::time::timeout(std::time::Duration::from_secs(10), validating).await???;
    let found = found["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|result| result["class_name"].as_str().unwrap().to_owned())
        .collect::<BTreeSet<_>>();
    assert_eq!(found, names(&["org.t.FreshOne", "org.t.GammaOne"]));
    assert_eq!(support::controlled_classpath_runs(warm), 2);

    client.cancel().await?;
    Ok(())
}
