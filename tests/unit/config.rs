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
