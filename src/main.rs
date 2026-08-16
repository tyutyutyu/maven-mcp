use std::{io, sync::Arc};

use anyhow::{Context, Result};
use maven_mcp::{config::Config, index::MavenIndex, project::MavenRunner, server::MavenMcpServer};
use rmcp::ServiceExt;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "maven_mcp=info".into()),
        )
        .with(tracing_subscriber::fmt::layer().with_writer(io::stderr))
        .init();

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    let config = Config::from_env()?;
    tracing::info!(repository = %config.repository.display(), "indexing Maven repository");
    let index = Arc::new(MavenIndex::build(
        &config.repository,
        config.max_results,
        config.max_source_bytes,
    )?);
    let stats = index.stats();
    tracing::info!(
        jars = stats.jar_count,
        source_jars = stats.source_jar_count,
        classes = stats.class_count,
        unique_classes = stats.unique_class_count,
        artifacts = stats.artifact_count,
        "Maven repository index ready"
    );

    let runner = config
        .project_execution
        .as_ref()
        .map(MavenRunner::discover)
        .transpose()?
        .map(Arc::new);
    let server = MavenMcpServer::with_runner(index, runner);
    let service = tokio::select! {
        result = server.serve(rmcp::transport::stdio()) => {
            result.context("cannot start STDIO MCP server")?
        }
        _ = &mut shutdown => exit_after_signal(),
    };
    tracing::info!(transport = "stdio", "MCP server ready");
    let cancellation = service.cancellation_token();
    let waiting = service.waiting();
    tokio::pin!(waiting);
    tokio::select! {
        result = &mut waiting => {
            result.context("STDIO MCP server task failed")?;
        }
        _ = &mut shutdown => {
            cancellation.cancel();
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), &mut waiting).await;
            exit_after_signal();
        }
    }
    Ok(())
}

fn exit_after_signal() -> ! {
    // Tokio's process-wide stdin reader uses a blocking thread that cannot be
    // cancelled. After the MCP service and request futures have been dropped,
    // terminate explicitly so signal-driven shutdown cannot hang on stdin.
    std::process::exit(0)
}

#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate =
        signal(SignalKind::terminate()).expect("SIGTERM handler must be installable");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => tracing::info!(signal = "SIGINT", "shutdown requested"),
        _ = terminate.recv() => tracing::info!(signal = "SIGTERM", "shutdown requested"),
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
