use std::{
    env,
    ffi::OsString,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};

pub const DEFAULT_MAX_RESULTS: usize = 100;
pub const DEFAULT_MAX_SOURCE_BYTES: usize = 1_048_576;
pub const DEFAULT_MAVEN_TIMEOUT_SECONDS: usize = 300;
pub const DEFAULT_MAX_MAVEN_OUTPUT_BYTES: usize = 1_048_576;
pub const DEFAULT_MAX_PROJECT_INDEXES: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JavaEnvironment {
    Inherit,
    Jenv { root: PathBuf },
}

#[derive(Debug, Clone)]
pub struct MavenExecutionConfig {
    pub trusted_project_directories: Vec<PathBuf>,
    pub maven_executable: Option<PathBuf>,
    pub execution_repository: Option<PathBuf>,
    pub timeout: Duration,
    pub max_output_bytes: usize,
    pub max_results: usize,
    pub network_enabled: bool,
    pub java_environment: JavaEnvironment,
}

#[derive(Debug, Clone)]
pub struct ProjectExecutionConfig {
    pub project_root: PathBuf,
    pub maven_executable: Option<PathBuf>,
    pub execution_repository: Option<PathBuf>,
    pub timeout: Duration,
    pub max_output_bytes: usize,
    pub max_results: usize,
    pub network_enabled: bool,
    pub java_environment: JavaEnvironment,
}

impl ProjectExecutionConfig {
    pub fn from_root(project_root: PathBuf, config: &MavenExecutionConfig) -> Self {
        Self {
            project_root,
            maven_executable: config.maven_executable.clone(),
            execution_repository: config.execution_repository.clone(),
            timeout: config.timeout,
            max_output_bytes: config.max_output_bytes,
            max_results: config.max_results,
            network_enabled: config.network_enabled,
            java_environment: config.java_environment.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub max_results: usize,
    pub max_source_bytes: usize,
    pub max_project_indexes: usize,
    pub execution: MavenExecutionConfig,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Self::from_env_with_jenv(false)
    }

    pub fn from_env_with_jenv(use_jenv: bool) -> Result<Self> {
        Ok(Self {
            max_results: positive_env("MAX_RESULTS", DEFAULT_MAX_RESULTS)?,
            max_source_bytes: positive_env("MAX_SOURCE_BYTES", DEFAULT_MAX_SOURCE_BYTES)?,
            max_project_indexes: positive_env("MAX_PROJECT_INDEXES", DEFAULT_MAX_PROJECT_INDEXES)?,
            execution: maven_execution_from_env(use_jenv)?,
        })
    }
}

fn maven_execution_from_env(use_jenv: bool) -> Result<MavenExecutionConfig> {
    let timeout_seconds = positive_env("MAVEN_TIMEOUT_SECONDS", DEFAULT_MAVEN_TIMEOUT_SECONDS)?;
    let timeout_seconds =
        u64::try_from(timeout_seconds).context("MAVEN_TIMEOUT_SECONDS is too large")?;
    Ok(MavenExecutionConfig {
        trusted_project_directories: trusted_project_directories_from_env()?,
        maven_executable: env::var_os("MAVEN_EXECUTABLE").map(PathBuf::from),
        execution_repository: env::var_os("MAVEN_EXECUTION_REPO_PATH").map(PathBuf::from),
        timeout: Duration::from_secs(timeout_seconds),
        max_output_bytes: positive_env("MAX_MAVEN_OUTPUT_BYTES", DEFAULT_MAX_MAVEN_OUTPUT_BYTES)?,
        max_results: positive_env("MAX_RESULTS", DEFAULT_MAX_RESULTS)?,
        network_enabled: boolean_env("MAVEN_EXECUTION_NETWORK")?,
        java_environment: java_environment_from_env(use_jenv)?,
    })
}

fn java_environment_from_env(use_jenv: bool) -> Result<JavaEnvironment> {
    if !use_jenv {
        return Ok(JavaEnvironment::Inherit);
    }
    let root = env::var_os("JENV_ROOT")
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".jenv").into()))
        .map(PathBuf::from)
        .context("--jenv requires JENV_ROOT or HOME")?;
    if !root.is_absolute() {
        bail!("JENV_ROOT must be an absolute path when --jenv is enabled");
    }
    Ok(JavaEnvironment::Jenv { root })
}

fn trusted_project_directories_from_env() -> Result<Vec<PathBuf>> {
    let Some(value) = env::var_os("MAVEN_TRUSTED_PROJECT_DIRECTORIES") else {
        return Ok(Vec::new());
    };
    parse_trusted_project_directories(value)
}

fn parse_trusted_project_directories(value: OsString) -> Result<Vec<PathBuf>> {
    let mut directories = env::split_paths(&value)
        .map(|path| canonicalize_absolute_directory(&path))
        .collect::<Result<Vec<_>>>()?;
    if directories.is_empty() {
        bail!("MAVEN_TRUSTED_PROJECT_DIRECTORIES must contain at least one directory");
    }
    directories.sort();
    directories.dedup();
    Ok(directories)
}

fn canonicalize_absolute_directory(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("MAVEN_TRUSTED_PROJECT_DIRECTORIES entries must be absolute paths");
    }
    let canonical = path
        .canonicalize()
        .with_context(|| "MAVEN_TRUSTED_PROJECT_DIRECTORIES contains an unreadable directory")?;
    if !canonical.is_dir() {
        bail!("MAVEN_TRUSTED_PROJECT_DIRECTORIES entries must be directories");
    }
    Ok(canonical)
}

fn boolean_env(name: &str) -> Result<bool> {
    match env::var(name) {
        Ok(value) if value.eq_ignore_ascii_case("true") || value == "1" => Ok(true),
        Ok(value) if value.eq_ignore_ascii_case("false") || value == "0" => Ok(false),
        Ok(_) => bail!("{name} must be true, false, 1, or 0"),
        Err(env::VarError::NotPresent) => Ok(false),
        Err(error) => Err(error).with_context(|| format!("cannot read {name}")),
    }
}

fn positive_env(name: &str, default: usize) -> Result<usize> {
    let Some(value) = env::var_os(name) else {
        return Ok(default);
    };
    let parsed = value
        .to_string_lossy()
        .parse::<usize>()
        .with_context(|| format!("{name} must be a positive integer"))?;
    if parsed == 0 {
        bail!("{name} must be greater than zero");
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
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
        let error =
            parse_trusted_project_directories(OsString::from("relative-projects")).unwrap_err();

        assert!(error.to_string().contains("entries must be absolute paths"));
    }
}
