use std::io;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use maven_mcp::{
    config::Config,
    runtime_stats::{format_runtime_report, read_runtime_report},
    server::MavenMcpServer,
};
use rmcp::ServiceExt;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Debug, Parser)]
#[command(version, about)]
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

#[tokio::main]
async fn main() -> Result<()> {
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
                .unwrap_or_else(|_| "maven_mcp=info".into()),
        )
        .with(tracing_subscriber::fmt::layer().with_writer(io::stderr))
        .init();

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    let config = Config::from_env_with_jenv(cli.jenv)?;
    tracing::info!(
        max_results = config.max_results,
        max_source_bytes = config.max_source_bytes,
        "MCP server ready for request-scoped Maven projects"
    );

    let server = MavenMcpServer::new(config);
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
