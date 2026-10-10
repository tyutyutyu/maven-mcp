use super::*;
use tempfile::TempDir;

#[test]
fn default_config_is_not_bound_to_repository_or_project_root() {
    let config = Config::from_env().unwrap();
    assert_eq!(config.max_results, DEFAULT_MAX_RESULTS);
    assert_eq!(config.max_source_bytes, DEFAULT_MAX_SOURCE_BYTES);
    assert_eq!(config.max_project_indexes, DEFAULT_MAX_PROJECT_INDEXES);
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
