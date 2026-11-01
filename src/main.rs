use std::{
    io::{self, IsTerminal},
    time::Duration,
};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use maven_mcp::{
    build_info,
    config::Config,
    runtime_stats::{format_runtime_report, read_runtime_report},
    server::MavenMcpServer,
};
use rmcp::ServiceExt;
use tracing_subscriber::{Layer, filter::filter_fn, layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Debug, Parser)]
#[command(version = build_info::version_label(), about)]
struct Cli {
    #[arg(
        long,
        global = true,
        help = "Resolve each Maven child JDK through jenv using the request project"
    )]
    jenv: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(about = "Print live maven-mcp runtime statistics")]
    Stats {
        #[arg(long, help = "Print machine-readable JSON")]
        json: bool,
    },
}

/// How long the runtime waits for blocking index work to stop after the server finished. Work
/// inside a single system call cannot be interrupted, so it is abandoned afterwards.
const BLOCKING_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

fn main() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("cannot start the async runtime")?;
    let result = runtime.block_on(run());
    // Dropping the runtime would wait for every blocking thread without a limit.
    runtime.shutdown_timeout(BLOCKING_SHUTDOWN_GRACE);
    result
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    if let Some(Command::Stats { json }) = cli.command {
        let report = read_runtime_report()?;
        if json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            print!("{}", format_runtime_report(&report));
        }
        return Ok(());
    }

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| DEFAULT_LOG_FILTER.into()),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(io::stderr)
                .with_ansi(stderr_supports_color())
                .with_filter(filter_fn(is_minimized_event)),
        )
        .init();
    std::panic::set_hook(Box::new(log_panic_without_payload));

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    let config = Config::from_env_with_jenv(cli.jenv)?;
    tracing::info!(
        version = build_info::PACKAGE_VERSION,
        commit = build_info::build_commit().unwrap_or("unknown"),
        max_results = config.max_results,
        max_source_bytes = config.max_source_bytes,
        "MCP server ready for request-scoped Maven projects"
    );
    log_configuration_summary(&config);

    let server = MavenMcpServer::new(config);
    let lifecycle = server.clone();
    let service = tokio::select! {
        result = server.serve(rmcp::transport::stdio()) => {
            result.context("cannot start STDIO MCP server")?
        }
        _ = &mut shutdown => {
            lifecycle.shutdown();
            exit_after_signal()
        }
    };
    tracing::info!(transport = "stdio", "MCP server ready");
    tracing::debug!(transport = %"stdio", "STDIO service initialized");
    let cancellation = service.cancellation_token();
    let waiting = service.waiting();
    tokio::pin!(waiting);
    tokio::select! {
        result = &mut waiting => {
            lifecycle.shutdown();
            let reason = result.context("STDIO MCP server task failed")?;
            tracing::debug!(transport = %"stdio", reason = %quit_reason_label(&reason), "STDIO connection closed");
        }
        _ = &mut shutdown => {
            tracing::debug!(transport = %"stdio", reason = %"signal", "STDIO connection closed");
            lifecycle.shutdown();
            cancellation.cancel();
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), &mut waiting).await;
            exit_after_signal();
        }
    }
    Ok(())
}

/// The documented filter when `RUST_LOG` is unset or invalid.
const DEFAULT_LOG_FILTER: &str = "maven_mcp=info";

/// Colors only for an interactive terminal: an MCP host usually captures stderr into a log
/// file, where escape sequences would split the `key=value` fields. `NO_COLOR` still wins.
fn stderr_supports_color() -> bool {
    io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
}

/// Only counts, limits and switches: configured directories, the Maven executable and the
/// jenv root are local paths and are never logged.
fn log_configuration_summary(config: &Config) {
    tracing::debug!(
        max_results = config.max_results,
        max_source_bytes = config.max_source_bytes,
        max_project_indexes = config.max_project_indexes,
        max_project_index_bytes = config.max_project_index_bytes,
        max_heavy_jobs = config.max_heavy_jobs,
        max_queued_heavy_jobs = config.max_queued_heavy_jobs,
        trusted_directories = config.execution.trusted_project_directories.len(),
        maven_executable_configured = config.execution.maven_executable.is_some(),
        execution_repository_configured = config.execution.execution_repository.is_some(),
        maven_timeout_ms = u64::try_from(config.execution.timeout.as_millis()).unwrap_or(u64::MAX),
        max_maven_output_bytes = config.execution.max_output_bytes,
        network_enabled = config.execution.network_enabled,
        jenv = matches!(
            config.execution.java_environment,
            maven_mcp::config::JavaEnvironment::Jenv { .. }
        ),
        index_cache = config.index_cache.is_some(),
        "configuration summary"
    );
}

/// A stable category of how the STDIO session ended; never the error text.
fn quit_reason_label(reason: &rmcp::service::QuitReason) -> &'static str {
    match reason {
        rmcp::service::QuitReason::Cancelled => "cancelled",
        rmcp::service::QuitReason::Closed => "closed",
        rmcp::service::QuitReason::JoinError(_) => "join_error",
        _ => "other",
    }
}

/// `RUST_LOG` selects the verbosity, but MCP transport (`rmcp`) events are never written at
/// any level: they quote peer-supplied requests, notifications, cancellation reasons and
/// results, including absolute `project_path` values, requested source code and Maven output,
/// which the data minimization contract excludes.
fn is_minimized_event(metadata: &tracing::Metadata<'_>) -> bool {
    !metadata.target().starts_with("rmcp")
}

/// A panic payload can quote the text being processed, such as Maven output or a request
/// argument, so only the source file name and line of the panic are logged.
fn log_panic_without_payload(info: &std::panic::PanicHookInfo<'_>) {
    let location = info.location().map_or_else(
        || "unknown".to_owned(),
        |location| {
            let file = std::path::Path::new(location.file())
                .file_name()
                .map_or_else(|| "unknown".into(), |name| name.to_string_lossy());
            format!("{file}:{}", location.line())
        },
    );
    tracing::error!(%location, "internal panic; payload withheld to protect request data");
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
