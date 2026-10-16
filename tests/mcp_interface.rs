mod support;

use std::collections::BTreeSet;

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
