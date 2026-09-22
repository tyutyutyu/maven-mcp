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
    let second = client.call_tool(
        CallToolRequestParams::new("run_maven_lifecycle").with_arguments(arguments(json!({
            "project_path": second_project.display().to_string(),
            "phase": "compile"
        }))),
    );
    let (first, second) = tokio::join!(first, second);
    assert_eq!(structured(first?)["outcome"], "success");
    assert_eq!(structured(second?)["outcome"], "success");
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
    }

    server.set_jenv_version(&first_project, "missing")?;
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
    assert_eq!(
        dependency_tree["dependencies"][0]["coordinate"],
        "org.libs:helper:2.0"
    );

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
