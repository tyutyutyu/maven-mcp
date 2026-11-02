use super::*;
use tempfile::TempDir;

#[test]
fn default_config_is_not_bound_to_repository_or_project_root() {
    let config = Config::from_env().unwrap();
    assert_eq!(config.max_results, DEFAULT_MAX_RESULTS);
    assert_eq!(config.max_source_bytes, DEFAULT_MAX_SOURCE_BYTES);
    assert_eq!(config.max_project_indexes, DEFAULT_MAX_PROJECT_INDEXES);
    assert_eq!(
        config.max_project_index_bytes,
        DEFAULT_MAX_PROJECT_INDEX_BYTES
    );
    assert_eq!(config.max_heavy_jobs, DEFAULT_MAX_HEAVY_JOBS);
    assert_eq!(config.max_queued_heavy_jobs, DEFAULT_MAX_QUEUED_HEAVY_JOBS);
    assert_eq!(
        config.max_mcp_response_bytes,
        DEFAULT_MAX_MCP_RESPONSE_BYTES
    );
    assert_eq!(
        config.max_mcp_result_cache_bytes,
        DEFAULT_MAX_MCP_RESULT_CACHE_BYTES
    );
    assert_eq!(
        config.execution.timeout,
        Duration::from_secs(DEFAULT_MAVEN_TIMEOUT_SECONDS as u64)
    );
    assert_eq!(config.execution.java_environment, JavaEnvironment::Inherit);
}

#[test]
fn trusted_project_directories_are_canonicalized_and_deduplicated() {
    let root = TempDir::new().unwrap();
    let nested = root.path().join("nested");
    std::fs::create_dir(&nested).unwrap();
    let value = env::join_paths([root.path(), nested.as_path(), root.path()]).unwrap();

    assert_eq!(
        parse_trusted_project_directories(value).unwrap(),
        vec![
            root.path().canonicalize().unwrap(),
            nested.canonicalize().unwrap()
        ]
    );
}

#[test]
fn trusted_project_directories_reject_relative_paths() {
    let error = parse_trusted_project_directories(OsString::from("relative-projects")).unwrap_err();

    assert!(error.to_string().contains("entries must be absolute paths"));
}

#[test]
fn index_cache_directory_prefers_the_override_then_xdg_then_home() {
    let directory = |configured: Option<&str>, xdg: Option<&str>, home: Option<&str>| {
        index_cache_directory(
            configured.map(OsString::from),
            xdg.map(OsString::from),
            home.map(OsString::from),
        )
    };

    assert_eq!(
        directory(Some("/cache"), Some("/xdg"), Some("/home/u")).unwrap(),
        Some(PathBuf::from("/cache"))
    );
    assert_eq!(
        directory(None, Some("/xdg"), Some("/home/u")).unwrap(),
        Some(PathBuf::from("/xdg/maven-mcp/index"))
    );
    assert_eq!(
        directory(None, Some("relative"), Some("/home/u")).unwrap(),
        Some(PathBuf::from("/home/u/.cache/maven-mcp/index")),
        "a relative XDG_CACHE_HOME is ignored"
    );
    assert_eq!(directory(None, None, None).unwrap(), None);
    assert!(
        directory(Some("relative"), None, None)
            .unwrap_err()
            .to_string()
            .contains("MAVEN_MCP_CACHE_DIR must be an absolute path")
    );
}

#[test]
fn mcp_response_limits_default_to_12_kib_and_64_mib() {
    assert_eq!(DEFAULT_MAX_MCP_RESPONSE_BYTES, 12_288);
    assert_eq!(DEFAULT_MAX_MCP_RESULT_CACHE_BYTES, 64 * 1024 * 1024);
    assert!(SERVER_ONLY_ENVIRONMENT.contains(&"MAX_MCP_RESPONSE_BYTES"));
    assert!(SERVER_ONLY_ENVIRONMENT.contains(&"MAX_MCP_RESULT_CACHE_BYTES"));
}

#[test]
fn positive_limits_accept_missing_and_valid_values_and_reject_zero_and_invalid_ones() {
    for name in ["MAX_MCP_RESPONSE_BYTES", "MAX_MCP_RESULT_CACHE_BYTES"] {
        let parse = |value: Option<&str>| parse_positive(name, value.map(std::ffi::OsStr::new), 7);

        assert_eq!(parse(None).unwrap(), 7, "a missing {name} uses the default");
        assert_eq!(parse(Some("4096")).unwrap(), 4096);
        let zero = parse(Some("0")).unwrap_err().to_string();
        assert!(
            zero.contains(name) && zero.contains("greater than zero"),
            "{zero}"
        );
        for invalid in ["", "-1", "12k", "1.5", "99999999999999999999999"] {
            let error = parse(Some(invalid)).unwrap_err().to_string();
            assert!(
                error.contains(name) && error.contains("positive integer"),
                "{invalid:?}: {error}"
            );
        }
    }
}
