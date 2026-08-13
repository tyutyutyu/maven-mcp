use std::{
    env,
    io::{Read, Write},
    net::{IpAddr, SocketAddr, TcpStream},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use axum::{Router, routing::get};
use maven_mcp::{config::Config, index::MavenIndex, project::MavenRunner, server::MavenMcpServer};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> Result<()> {
    if env::args().any(|argument| argument == "--healthcheck") {
        return healthcheck();
    }

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "maven_mcp=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

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
    let cancellation = CancellationToken::new();
    let server = MavenMcpServer::with_runner(index, runner);
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_cancellation_token(cancellation.clone())
            .with_json_response(true),
    );
    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(config.bind_address)
        .await
        .with_context(|| format!("cannot bind {}", config.bind_address))?;
    tracing::info!(address = %config.bind_address, endpoint = "/mcp", "MCP server listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(cancellation))
        .await
        .context("HTTP server failed")?;
    Ok(())
}

fn healthcheck() -> Result<()> {
    let mut address: SocketAddr = env::var("BIND_ADDRESS")
        .unwrap_or_else(|_| "0.0.0.0:8080".to_owned())
        .parse()
        .context("invalid BIND_ADDRESS")?;
    if address.ip().is_unspecified() {
        address.set_ip(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    }
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;
    let mut response = [0_u8; 128];
    let count = stream.read(&mut response)?;
    if response[..count].starts_with(b"HTTP/1.1 200") {
        Ok(())
    } else {
        anyhow::bail!("health endpoint did not return HTTP 200")
    }
}

async fn shutdown_signal(cancellation: CancellationToken) {
    let _ = tokio::signal::ctrl_c().await;
    cancellation.cancel();
}
