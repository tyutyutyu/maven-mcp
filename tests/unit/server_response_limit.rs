use super::*;

const RESULT_ID: &str = "r0123456789abcdef3";

fn response_len<T: Serialize>(value: &T) -> usize {
    serde_json::to_vec(&structured_result(value, usize::MAX).unwrap())
        .unwrap()
        .len()
}

/// Reads every page of `json` like a client and checks each page's size against `limit`.
fn read_all_pages(json: &str, limit: usize) -> Vec<ToolResultPage> {
    let mut pages = Vec::new();
    let mut offset = 0;
    loop {
        let page = fit_page(json, offset, RESULT_ID, limit).unwrap();
        assert!(
            response_len(&page) <= limit,
            "page at {offset} exceeds {limit}"
        );
        assert_eq!(page.offset, offset);
        assert_eq!(page.total_bytes, json.len());
        assert!(!page.fragment.is_empty());
        let next = page.next_cursor.clone();
        let complete = page.complete;
        pages.push(page);
        match next {
            None => {
                assert!(complete);
                return pages;
            }
            Some(cursor) => {
                assert!(!complete);
                offset = decode_cursor(RESULT_ID, &cursor, json).expect("issued cursor is valid");
            }
        }
    }
}

fn sample_json(unit: &str, repetitions: usize) -> String {
    serde_json::to_string(&json!({
        "results": vec![unit; repetitions],
        "incomplete": false,
    }))
    .unwrap()
}

#[test]
fn serialized_length_is_counted_without_buffering_and_stops_past_the_limit() {
    let value = json!({"results": ["a", "b"]});
    let exact = serde_json::to_vec(&value).unwrap().len();

    assert_eq!(serialized_len_within(&value, exact).unwrap(), Some(exact));
    assert_eq!(serialized_len_within(&value, exact - 1).unwrap(), None);
    assert_eq!(serialized_len_within(&value, 0).unwrap(), None);
}

#[test]
fn structured_results_keep_rmcps_shape_and_reject_results_over_the_budget() {
    let value = json!({"results": [{"z": 1, "a": "ő"}], "incomplete": false});

    let result = structured_result(&value, 1024).unwrap();

    assert_eq!(result, CallToolResult::structured(value.clone()));
    let exact = serde_json::to_vec(&value).unwrap().len();
    structured_result(&value, exact).unwrap();
    let error = structured_result(&value, exact - 1).unwrap_err();
    assert_eq!(error.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
    assert!(
        error.message.contains("MAX_MCP_RESULT_CACHE_BYTES"),
        "{error:?}"
    );
    assert!(
        error.message.len() < 300,
        "the resource-limit error stays small"
    );
}

#[test]
fn the_response_check_measures_the_complete_tool_result_including_content() {
    let value = json!({"results": ["x".repeat(100)]});
    let result = CallToolResult::structured(value.clone());
    let complete = serde_json::to_vec(&result).unwrap().len();
    // The JSON text appears twice: as structuredContent and as the escaped content text.
    assert!(complete > 2 * serde_json::to_vec(&value).unwrap().len());

    assert!(fits_response(&result, complete).unwrap());
    assert!(!fits_response(&result, complete - 1).unwrap());
}

#[test]
fn pages_rebuild_the_json_exactly_and_each_response_fits_the_limit() {
    let texts = [
        sample_json("plain", 400),
        sample_json(r#"quote " and back\slash"#, 200),
        sample_json("ő árvíztűrő 😀 漢字", 300),
        sample_json("tab\tnew\nline\u{1}control", 200),
        sample_json("😀", 2000),
    ];
    for json in &texts {
        for limit in [2_048, 4_096, 12_288] {
            let pages = read_all_pages(json, limit);
            let rebuilt = pages
                .iter()
                .map(|page| page.fragment.as_str())
                .collect::<String>();
            assert_eq!(&rebuilt, json, "limit {limit}");
            assert!(
                limit > 2_048 || pages.len() > 1,
                "the sample spans several pages at {limit}"
            );
        }
    }
}

#[test]
fn every_page_but_the_last_is_as_long_as_the_limit_allows() {
    let json = sample_json("ő \" \\ 😀 x", 500);
    let limit = 4_096;
    for page in read_all_pages(&json, limit) {
        let Some(cursor) = &page.next_cursor else {
            continue;
        };
        let end = decode_cursor(RESULT_ID, cursor, &json).unwrap();
        let next_end = end + json[end..].chars().next().unwrap().len_utf8();
        let complete = next_end == json.len();
        let longer = ToolResultPage {
            fragment: json[page.offset..next_end].to_owned(),
            next_cursor: (!complete).then(|| encode_cursor(RESULT_ID, next_end)),
            complete,
            ..page
        };
        assert!(
            response_len(&longer) > limit,
            "one more character would still fit after offset {end}"
        );
    }
}

#[test]
fn pages_never_split_a_multibyte_character() {
    let json = sample_json("😀ő", 1000);
    for page in read_all_pages(&json, 2_048) {
        let end = page.offset + page.fragment.len();
        assert!(json.is_char_boundary(page.offset));
        assert!(json.is_char_boundary(end));
    }
}

#[test]
fn a_page_is_rejected_when_not_one_character_fits() {
    let json = sample_json("😀", 10);

    assert_eq!(
        fit_page(&json, 0, RESULT_ID, 200).unwrap_err(),
        PageError::LimitTooSmall
    );
}

#[test]
fn only_canonical_cursors_at_character_boundaries_inside_the_text_are_accepted() {
    let json = r#"{"a":"ő"}"#;
    let multibyte = json.find('ő').unwrap();

    assert_eq!(
        decode_cursor(RESULT_ID, &encode_cursor(RESULT_ID, 3), json),
        Some(3)
    );
    for invalid in [
        String::new(),
        "garbage".to_owned(),
        RESULT_ID.to_owned(),
        format!("{RESULT_ID}."),
        format!("{RESULT_ID}.-1"),
        format!("{RESULT_ID}.+3"),
        format!("{RESULT_ID}.03"),
        format!("{RESULT_ID}.3 "),
        format!("{RESULT_ID}.{}", multibyte + 1),
        format!("{RESULT_ID}.{}", json.len()),
        format!("{RESULT_ID}.{}", json.len() + 5),
        format!("{RESULT_ID}.99999999999999999999999"),
        encode_cursor("r0123456789abcdef4", 3),
    ] {
        assert_eq!(
            decode_cursor(RESULT_ID, &invalid, json),
            None,
            "{invalid:?}"
        );
    }
}

#[test]
fn the_marker_names_the_pager_size_lifetime_and_instruction() {
    let marker = deferred_marker("search_classes", RESULT_ID.to_owned(), 123_456);
    let value = serde_json::to_value(&marker).unwrap();
    let deferred = &value["deferred_result"];

    assert_eq!(value.as_object().unwrap().len(), 1);
    assert_eq!(deferred["result_id"], RESULT_ID);
    assert_eq!(deferred["tool"], "search_classes");
    assert_eq!(deferred["total_bytes"], 123_456);
    assert_eq!(deferred["read_with"], READ_PAGE_TOOL);
    assert_eq!(deferred["expires_in_seconds"], 300);
    let instruction = deferred["instruction"].as_str().unwrap();
    for phrase in [
        READ_PAGE_TOOL,
        "project_path",
        "result_id",
        "next_cursor",
        "complete",
    ] {
        assert!(instruction.contains(phrase), "instruction misses {phrase}");
    }
}

#[test]
fn deferrable_schemas_keep_an_object_root_and_accept_either_shape() {
    let original = json!({
        "type": "object",
        "properties": {"results": {"type": "array"}},
        "required": ["results"]
    });

    let schema = deferrable_output_schema(original.as_object().unwrap());

    assert_eq!(schema["type"], "object");
    assert!(schema.get("required").is_none());
    assert_eq!(
        schema["anyOf"],
        json!([{"required": ["results"]}, {"required": ["deferred_result"]}])
    );
    assert_eq!(schema["properties"]["results"], json!({"type": "array"}));
    let marker = &schema["properties"]["deferred_result"];
    assert_eq!(marker["type"], "object");
    assert!(marker.get("$schema").is_none());
    assert!(
        marker["required"]
            .as_array()
            .unwrap()
            .contains(&json!("result_id"))
    );
}

#[test]
fn schemas_without_required_fields_only_gain_the_marker_property() {
    let original = json!({
        "type": "object",
        "additionalProperties": {"type": "array"}
    });

    let schema = deferrable_output_schema(original.as_object().unwrap());

    assert!(schema.get("anyOf").is_none());
    assert_eq!(schema["additionalProperties"], json!({"type": "array"}));
    assert_eq!(schema["properties"]["deferred_result"]["type"], "object");
}

#[test]
fn the_response_limit_must_hold_the_largest_marker_and_a_one_character_page() {
    let result_budget = 67_108_864;
    validate_response_limit(12_288, result_budget, 40).unwrap();

    let smallest = (1..12_288)
        .find(|limit| validate_response_limit(*limit, result_budget, 40).is_ok())
        .unwrap();
    let error = validate_response_limit(smallest - 1, result_budget, 40)
        .unwrap_err()
        .to_string();

    assert!(error.contains("MAX_MCP_RESPONSE_BYTES"), "{error}");
    assert!(error.contains("too small"), "{error}");
    let longest_marker = deferred_marker(&"t".repeat(40), longest_result_id(), result_budget);
    assert!(response_len(&longest_marker) <= smallest);
}
