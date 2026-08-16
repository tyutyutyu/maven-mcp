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
    let server = TestServer::start().await?;
    let client = server.connect().await?;

    let tools = client.list_all_tools().await?;
    let actual_names = tools
        .iter()
        .map(|tool| tool.name.as_ref())
        .collect::<BTreeSet<_>>();
    let expected_names = BTreeSet::from([
        "compare_artifact_api",
        "describe_class",
        "diagnose_artifact",
        "get_artifact_pom",
        "get_class_source",
        "get_declaration_source",
        "get_jar_entry",
        "index_stats",
        "list_artifact_versions",
        "list_jar_classes",
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
    assert!(tools.iter().all(|tool| {
        tool.output_schema
            .as_ref()
            .and_then(|schema| schema.get("type"))
            .and_then(Value::as_str)
            == Some("object")
    }));

    let stats = structured(
        client
            .call_tool(CallToolRequestParams::new("index_stats"))
            .await?,
    );
    assert_eq!(stats["jar_count"], 2);
    assert_eq!(stats["source_jar_count"], 2);
    assert_eq!(stats["unique_class_count"], 3);

    let classes = structured(
        client
            .call_tool(
                CallToolRequestParams::new("search_classes")
                    .with_arguments(arguments(json!({ "query": "Foo" }))),
            )
            .await?,
    );
    assert_eq!(classes["results"].as_array().unwrap().len(), 2);
    assert_eq!(classes["results"][0]["class_name"], "org.example.Foo");

    let jars = structured(
        client
            .call_tool(
                CallToolRequestParams::new("search_jars")
                    .with_arguments(arguments(json!({ "query": "demo:2.0" }))),
            )
            .await?,
    );
    assert_eq!(jars["results"].as_array().unwrap().len(), 2);
    assert!(
        jars["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|jar| jar["coordinate"] == "org.example:demo:2.0")
    );

    let entries = structured(
        client
            .call_tool(
                CallToolRequestParams::new("search_jar_entries").with_arguments(arguments(json!({
                    "query": "example.Service",
                    "jar": "demo-1.0.jar"
                }))),
            )
            .await?,
    );
    assert_eq!(
        entries["results"][0]["entry"],
        "META-INF/services/example.Service"
    );

    let entry_content = structured(
        client
            .call_tool(
                CallToolRequestParams::new("get_jar_entry").with_arguments(arguments(json!({
                    "jar": "demo-1.0.jar",
                    "entry": "META-INF/services/example.Service"
                }))),
            )
            .await?,
    );
    assert_eq!(entry_content["results"][0]["content_kind"], "text");
    assert_eq!(entry_content["results"][0]["text"], "org.example.Foo");
    assert_eq!(entry_content["results"][0]["original_size"], 15);

    let binary_entry = structured(
        client
            .call_tool(
                CallToolRequestParams::new("get_jar_entry").with_arguments(arguments(json!({
                    "jar": "demo-1.0.jar",
                    "entry": "native/image.bin"
                }))),
            )
            .await?,
    );
    assert_eq!(binary_entry["results"][0]["content_kind"], "binary");
    assert_eq!(
        binary_entry["results"][0]["bytes"],
        json!([0, 159, 146, 150])
    );

    let pom = structured(
        client
            .call_tool(
                CallToolRequestParams::new("get_artifact_pom").with_arguments(arguments(json!({
                    "coordinate": "org.example:demo:1.0"
                }))),
            )
            .await?,
    );
    assert_eq!(pom["found"], true);
    assert_eq!(pom["descriptor"]["packaging"], "jar");
    assert_eq!(pom["descriptor"]["properties"]["java.version"], "21");
    assert_eq!(pom["descriptor"]["dependencies"][0]["scope"], "runtime");

    let description = structured(
        client
            .call_tool(
                CallToolRequestParams::new("describe_class").with_arguments(arguments(json!({
                    "class_name": "org.example.Inspectable",
                    "version": "2.0",
                    "visibility": "public"
                }))),
            )
            .await?,
    );
    assert_eq!(description["results"].as_array().unwrap().len(), 1);
    assert_eq!(
        description["results"][0]["class_name"],
        "org.example.Inspectable"
    );
    assert_eq!(description["results"][0]["super_class"], "java.lang.Object");

    let health = structured(
        client
            .call_tool(
                CallToolRequestParams::new("diagnose_artifact").with_arguments(arguments(json!({
                    "coordinate": "org.example:demo:1.0"
                }))),
            )
            .await?,
    );
    assert_eq!(health["found"], true);
    assert_eq!(health["snapshot"], false);
    assert_eq!(health["repository_ids"], json!(["central"]));
    assert_eq!(health["checksums"], json!(["demo-1.0.jar.sha1"]));

    let member_matches = structured(
        client
            .call_tool(
                CallToolRequestParams::new("search_class_members").with_arguments(arguments(
                    json!({ "query": "value", "jar": "demo-2.0.jar" }),
                )),
            )
            .await?,
    );
    assert_eq!(member_matches["results"].as_array().unwrap().len(), 2);
    assert_eq!(member_matches["results"][0]["kind"], "field");
    assert_eq!(member_matches["results"][0]["name"], "value");

    let api_diff = structured(
        client
            .call_tool(
                CallToolRequestParams::new("compare_artifact_api").with_arguments(arguments(
                    json!({
                        "group_id": "org.example",
                        "artifact_id": "demo",
                        "previous_version": "1.0",
                        "current_version": "2.0"
                    }),
                )),
            )
            .await?,
    );
    assert_eq!(
        api_diff["added_classes"],
        json!(["org.example.Inspectable"])
    );
    assert_eq!(api_diff["removed_classes"], json!(["org.example.Bar"]));

    let content_matches = structured(
        client
            .call_tool(
                CallToolRequestParams::new("search_jar_content").with_arguments(arguments(json!({
                    "query": "example.foo",
                    "jar": "demo-1.0.jar"
                }))),
            )
            .await?,
    );
    let content_results = content_matches["results"].as_array().unwrap();
    assert_eq!(content_results.len(), 2);
    assert!(content_results.iter().any(|result| {
        result["entry"] == "META-INF/services/example.Service"
            && result["context"] == "org.example.Foo"
    }));

    let listed_classes = structured(
        client
            .call_tool(
                CallToolRequestParams::new("list_jar_classes").with_arguments(arguments(json!({
                    "jar": "org.example:demo:1.0",
                    "offset": 1,
                    "limit": 1
                }))),
            )
            .await?,
    );
    assert_eq!(listed_classes["results"][0]["total"], 2);
    assert_eq!(
        listed_classes["results"][0]["classes"],
        json!(["org.example.Foo"])
    );

    let source = structured(
        client
            .call_tool(
                CallToolRequestParams::new("get_class_source").with_arguments(arguments(json!({
                    "class_name": "org.example.Foo",
                    "version": "2.0"
                }))),
            )
            .await?,
    );
    assert_eq!(source["results"].as_array().unwrap().len(), 1);
    assert!(
        source["results"][0]["source"]
            .as_str()
            .unwrap()
            .contains("version = 2")
    );

    let hierarchy = structured(
        client
            .call_tool(
                CallToolRequestParams::new("search_type_hierarchy").with_arguments(arguments(
                    json!({
                        "type_name": "java.lang.Object",
                        "jar": "demo-2.0.jar"
                    }),
                )),
            )
            .await?,
    );
    assert!(
        hierarchy["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| { item["type_name"] == "org.example.Inspectable" && item["depth"] == 1 })
    );

    let source_matches = structured(
        client
            .call_tool(
                CallToolRequestParams::new("search_source").with_arguments(arguments(json!({
                    "query": "version\\s*=\\s*2",
                    "regex": true,
                    "jar": "demo-2.0-sources.jar",
                    "context_lines": 1
                }))),
            )
            .await?,
    );
    assert_eq!(
        source_matches["results"][0]["entry"],
        "org/example/Foo.java"
    );
    assert_eq!(source_matches["results"][0]["line"], 1);

    let declaration = structured(
        client
            .call_tool(
                CallToolRequestParams::new("get_declaration_source").with_arguments(arguments(
                    json!({
                        "class_name": "org.example.Foo",
                        "version": "2.0"
                    }),
                )),
            )
            .await?,
    );
    assert!(
        declaration["results"][0]["source"]
            .as_str()
            .unwrap()
            .contains("class Foo")
    );

    let references = structured(
        client
            .call_tool(
                CallToolRequestParams::new("search_class_references").with_arguments(arguments(
                    json!({
                        "class_name": "org.example.Inspectable",
                        "direction": "outbound",
                        "kind": "class",
                        "jar": "demo-2.0.jar"
                    }),
                )),
            )
            .await?,
    );
    assert!(
        references["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| { item["target_owner"] == "java.lang.Object" })
    );

    let providers = structured(
        client
            .call_tool(
                CallToolRequestParams::new("search_providers").with_arguments(arguments(json!({
                    "service": "example.Service",
                    "descriptor_kind": "service_loader"
                }))),
            )
            .await?,
    );
    assert_eq!(providers["results"][0]["provider"], "org.example.Foo");

    let spring_providers = structured(
        client
            .call_tool(
                CallToolRequestParams::new("search_providers").with_arguments(arguments(json!({
                    "service": "example.Factory",
                    "descriptor_kind": "spring_factories"
                }))),
            )
            .await?,
    );
    assert_eq!(
        spring_providers["results"][0]["provider"],
        "org.example.Bar"
    );

    let versions = structured(
        client
            .call_tool(
                CallToolRequestParams::new("list_artifact_versions").with_arguments(arguments(
                    json!({ "group_id": "org.example", "artifact_id": "demo" }),
                )),
            )
            .await?,
    );
    assert_eq!(versions["org.example:demo"], json!(["1.0", "2.0"]));

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn stdio_mcp_rejects_invalid_and_unknown_tool_calls() -> Result<()> {
    let server = TestServer::start().await?;
    let client = server.connect().await?;

    let missing_argument = client
        .call_tool(CallToolRequestParams::new("search_classes"))
        .await?;
    assert_eq!(missing_argument.is_error, Some(true));
    assert!(
        missing_argument.content[0]
            .as_text()
            .unwrap()
            .text
            .contains("query")
    );

    let invalid_regex = client
        .call_tool(
            CallToolRequestParams::new("search_source").with_arguments(arguments(json!({
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
    let client = server.connect().await?;

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
            .call_tool(CallToolRequestParams::new("inspect_maven_project"))
            .await?,
    );
    assert_eq!(project["artifact_id"], "fixture-project");
    assert_eq!(project["wrapper"], true);

    let lifecycle = structured(
        client
            .call_tool(
                CallToolRequestParams::new("run_maven_lifecycle")
                    .with_arguments(arguments(json!({ "phase": "compile" }))),
            )
            .await?,
    );
    assert_eq!(lifecycle["outcome"], "success");

    let tests = structured(
        client
            .call_tool(CallToolRequestParams::new("list_maven_test_classes"))
            .await?,
    );
    assert_eq!(tests["results"], json!(["org.example.FooTest"]));
    let focused = structured(
        client
            .call_tool(
                CallToolRequestParams::new("run_maven_test").with_arguments(arguments(json!({
                    "test_class": "org.example.FooTest"
                }))),
            )
            .await?,
    );
    assert_eq!(focused["report_status"], "available");
    assert_eq!(focused["summary"]["passed"], 1);

    let effective = structured(
        client
            .call_tool(
                CallToolRequestParams::new("get_effective_pom")
                    .with_arguments(arguments(json!({}))),
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
                CallToolRequestParams::new("get_dependency_tree")
                    .with_arguments(arguments(json!({}))),
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
                CallToolRequestParams::new("explain_dependency_resolution")
                    .with_arguments(arguments(json!({}))),
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
                CallToolRequestParams::new("get_maven_classpath")
                    .with_arguments(arguments(json!({ "kind": "test" }))),
            )
            .await?,
    );
    assert_eq!(classpath["artifacts"], json!(["org.libs:helper:2.0"]));

    let coverage = structured(
        client
            .call_tool(CallToolRequestParams::new("get_jacoco_coverage"))
            .await?,
    );
    assert_eq!(coverage["status"], "available");
    assert_eq!(coverage["reports"][0]["metrics"]["lines"]["covered"], 9);

    let gaps = structured(
        client
            .call_tool(
                CallToolRequestParams::new("get_jacoco_coverage_gaps")
                    .with_arguments(arguments(json!({ "limit": 1 }))),
            )
            .await?,
    );
    assert_eq!(gaps["gaps"][0]["class_name"], "org.example.Foo");

    client.cancel().await?;
    Ok(())
}
