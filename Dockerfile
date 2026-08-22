# syntax=docker/dockerfile:1.7
FROM rust:1.98-bookworm AS builder
WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --locked --release && \
    cp target/release/maven-mcp /tmp/maven-mcp

FROM debian:bookworm-slim AS runtime
RUN groupadd --system --gid 10001 maven-mcp && \
    useradd --system --uid 10001 --gid 10001 --no-create-home maven-mcp
COPY --from=builder /tmp/maven-mcp /usr/local/bin/maven-mcp

ENV MAVEN_REPO_PATH=/maven-repository \
    BIND_ADDRESS=0.0.0.0:8080 \
    RUST_LOG=maven_mcp=info
EXPOSE 8080
USER 10001:10001
HEALTHCHECK --interval=30s --timeout=3s --start-period=30s --retries=3 \
    CMD ["/usr/local/bin/maven-mcp", "--healthcheck"]
ENTRYPOINT ["/usr/local/bin/maven-mcp"]

FROM runtime AS execution
USER root
RUN apt-get update && \
    apt-get install --no-install-recommends --yes ca-certificates maven openjdk-17-jdk-headless && \
    rm -rf /var/lib/apt/lists/*
USER 10001:10001

FROM runtime AS final
