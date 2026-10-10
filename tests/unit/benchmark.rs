use super::*;

#[test]
fn token_estimate_uses_unicode_scalar_values() {
    assert_eq!(payload_size("árvíz".as_bytes()).bytes, 7);
    assert_eq!(payload_size("árvíz".as_bytes()).estimated_tokens, 2);
}

#[test]
fn rejects_duplicate_case_ids() {
    let error = BenchmarkSpec::from_slice(
        br#"{"schema_version":1,"name":"x","cases":[{"id":"same","shell":{"command":"true"},"mcp":{"tool":"index_stats"}},{"id":"same","shell":{"command":"true"},"mcp":{"tool":"index_stats"}}]}"#,
    )
    .unwrap_err();
    assert!(error.to_string().contains("duplicate benchmark case id"));
}

#[test]
fn summary_uses_only_successful_runs() {
    let request = PayloadSize {
        bytes: 4,
        estimated_tokens: 1,
    };
    let runs = vec![
        RunMeasurement {
            iteration: 1,
            duration_nanoseconds: 10,
            success: true,
            request,
            response: request,
            exit_code: Some(0),
            error: None,
            output_truncated: false,
        },
        failed_run(2, 1_000, request, "failed".to_owned()),
    ];
    let summary = summarize(&runs, request);
    assert_eq!(summary.successful_runs, 1);
    assert_eq!(summary.mean_duration_nanoseconds, Some(10));
    assert_eq!(summary.mean_total_estimated_tokens, Some(2));
}
