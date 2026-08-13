use std::{env, net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};

pub const DEFAULT_MAX_RESULTS: usize = 100;
pub const DEFAULT_MAX_SOURCE_BYTES: usize = 1_048_576;
pub const DEFAULT_MAVEN_TIMEOUT_SECONDS: usize = 300;
pub const DEFAULT_MAX_MAVEN_OUTPUT_BYTES: usize = 1_048_576;

#[derive(Debug, Clone)]
pub struct ProjectExecutionConfig {
    pub project_root: PathBuf,
    pub maven_executable: Option<PathBuf>,
    pub execution_repository: Option<PathBuf>,
    pub timeout: Duration,
    pub max_output_bytes: usize,
    pub max_results: usize,
    pub network_enabled: bool,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub repository: PathBuf,
    pub bind_address: SocketAddr,
    pub max_results: usize,
    pub max_source_bytes: usize,
    pub project_execution: Option<ProjectExecutionConfig>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let repository = env::var_os("MAVEN_REPO_PATH")
            .map(PathBuf::from)
            .context("MAVEN_REPO_PATH is required (for example /maven-repository)")?;
        if !repository.is_dir() {
            bail!(
                "MAVEN_REPO_PATH is not a readable directory: {}",
                repository.display()
            );
        }

        let bind_address = env::var("BIND_ADDRESS")
            .unwrap_or_else(|_| "0.0.0.0:8080".to_owned())
            .parse()
            .context("BIND_ADDRESS must be an IP:port value")?;

        let project_execution = project_execution_from_env()?;
        validate_distinct_repositories(&repository, project_execution.as_ref())?;

        Ok(Self {
            repository,
            bind_address,
            max_results: positive_env("MAX_RESULTS", DEFAULT_MAX_RESULTS)?,
            max_source_bytes: positive_env("MAX_SOURCE_BYTES", DEFAULT_MAX_SOURCE_BYTES)?,
            project_execution,
        })
    }
}

fn validate_distinct_repositories(
    repository: &std::path::Path,
    project_execution: Option<&ProjectExecutionConfig>,
) -> Result<()> {
    if let Some(execution_repository) =
        project_execution.and_then(|execution| execution.execution_repository.as_ref())
        && repository.canonicalize()? == execution_repository.canonicalize()?
    {
        bail!("MAVEN_EXECUTION_REPO_PATH must differ from the indexed MAVEN_REPO_PATH");
    }
    Ok(())
}

fn project_execution_from_env() -> Result<Option<ProjectExecutionConfig>> {
    let Some(project_root) = env::var_os("MAVEN_PROJECT_ROOT").map(PathBuf::from) else {
        return Ok(None);
    };
    let timeout_seconds = positive_env("MAVEN_TIMEOUT_SECONDS", DEFAULT_MAVEN_TIMEOUT_SECONDS)?;
    let timeout_seconds =
        u64::try_from(timeout_seconds).context("MAVEN_TIMEOUT_SECONDS is too large")?;
    Ok(Some(ProjectExecutionConfig {
        project_root,
        maven_executable: env::var_os("MAVEN_EXECUTABLE").map(PathBuf::from),
        execution_repository: env::var_os("MAVEN_EXECUTION_REPO_PATH").map(PathBuf::from),
        timeout: Duration::from_secs(timeout_seconds),
        max_output_bytes: positive_env("MAX_MAVEN_OUTPUT_BYTES", DEFAULT_MAX_MAVEN_OUTPUT_BYTES)?,
        max_results: positive_env("MAX_RESULTS", DEFAULT_MAX_RESULTS)?,
        network_enabled: boolean_env("MAVEN_EXECUTION_NETWORK")?,
    }))
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
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn execution_repository_must_not_alias_the_indexed_repository() {
        let repository = TempDir::new().unwrap();
        let config = ProjectExecutionConfig {
            project_root: repository.path().to_owned(),
            maven_executable: None,
            execution_repository: Some(repository.path().to_owned()),
            timeout: Duration::from_secs(1),
            max_output_bytes: 1,
            max_results: 1,
            network_enabled: false,
        };
        assert!(validate_distinct_repositories(repository.path(), Some(&config)).is_err());
        assert!(validate_distinct_repositories(repository.path(), None).is_ok());
    }
}
