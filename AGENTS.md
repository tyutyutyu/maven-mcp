# AGENTS.md

## Project Overview

`maven-mcp` is a Docker-ready MCP server written in Rust 2024. At startup, it
reads the local Maven repository under `MAVEN_REPO_PATH`, builds an in-memory
index of JARs, classes, sources, and artifact versions, and exposes them through
searchable Streamable HTTP MCP tools.

The primary user documentation is `README.md`, the testing strategy is in
`docs/testing.md`, and architectural decisions are recorded under
`docs/decisions/`. This file is the source of agent-specific development rules.

## Technology and Prerequisites

- Rust `1.89` or later, using the 2024 edition.
- Cargo; always use the versioned `Cargo.lock` file and the `--locked` flag for
  reproducible commands.
- The full conformance layer requires `node`, `npx`, and `curl`.
- Container validation requires Docker, and Compose workflows require Docker
  Compose.
- The project has no database and no separate build-time code generation step.

## Repository Map

- `src/config.rs`: environment configuration and validation.
- `src/index.rs`: Maven layout recognition, JAR/ZIP processing, the in-memory
  index, and all search operations.
- `src/server.rs`: MCP request/output types, tool definitions, and handlers.
- `src/main.rs`: startup indexing, the Axum router, `/mcp`, `/healthz`, shutdown,
  and the container-internal health check.
- `tests/support/mod.rs`: deterministic temporary Maven fixture repository and a
  real Streamable HTTP test server bound to a random port.
- `tests/mcp_interface.rs`: integration contract for the public MCP tool catalog
  and error semantics.
- `tests/scenarios/maven_search.yaml`: human-readable search examples.
- `tests/mcp_scenarios.rs`: scenario interpretation, semantic validation,
  snapshots, and Markdown report generation.
- `tests/snapshots/`: reviewable public MCP response contracts.
- `scripts/`: full verification, MCP conformance, and Inspector entry points.

## Important Architectural Contracts

- The complete repository index must be built before the MCP endpoint becomes
  available. Do not expose a partially built index or turn it into background-
  mutable state without a separately approved architectural decision.
- The completed `MavenIndex` is shared through `Arc`. Queries must remain
  read-only operations.
- Maven coordinates use the format
  `groupId:artifactId:version[:classifier]`.
- Treat `-sources.jar` artifacts as source archives, not binary class JARs.
- Deduplicate classes from multi-release JARs. Source lookup for inner classes
  must resolve to the enclosing class source.
- Searches are case-insensitive, and `limit` is capped by `MAX_RESULTS`. Keep
  result ordering deterministic so snapshots do not become flaky.
- The root of an MCP `outputSchema` must be an object. The public shape of
  list-like responses is `{ "results": [...] }`; do not restore root-level
  arrays.
- No match is a successful empty structured response. Invalid arguments or an
  unknown tool are MCP errors. Do not conceal an actual source-reading failure
  as a missing result.
- `MAX_SOURCE_BYTES` limits source size, and responses must preserve whether
  content was truncated.

## Configuration and Local Execution

`MAVEN_REPO_PATH` is required and must point to an existing directory. Other
variables are:

- `BIND_ADDRESS`, default: `0.0.0.0:8080`.
- `MAX_RESULTS`, default: `100`; must be a positive integer.
- `MAX_SOURCE_BYTES`, default: `1048576`; must be a positive integer.
- `RUST_LOG`, recommended default: `maven_mcp=info`.

Run locally:

```bash
MAVEN_REPO_PATH="$HOME/.m2/repository" cargo run --locked
```

The MCP endpoint is `http://localhost:8080/mcp`, and the liveness endpoint is
`http://localhost:8080/healthz`. The index is rebuilt only at startup; restart
the server after changing a fixture or repository.

## Development Rules

- Keep logic in the appropriate layer: indexing and search in the `index`
  module, protocol adapters and schemas in the `server` module, and process
  startup in the `main` module.
- Use precise domain names, small single-purpose functions, and early
  validation.
- Use deterministic ordering for returned collections and `BTreeMap` for maps
  when needed.
- Propagate expected runtime failures with `Result` and useful
  `anyhow::Context` messages. Do not use `unwrap`/`expect` in production code
  without a proven invariant; focused messages are acceptable in tests.
- Do not log source code, repository contents, secrets, or unnecessarily
  complete local paths. A malformed individual JAR must not stop indexing the
  entire repository when it can be skipped safely.
- Before adding a dependency, check whether the standard library or an existing
  crate already solves the problem. Update and version `Cargo.lock` after a
  dependency change.
- Do not edit generated content under `target/` manually.

Formatting and static analysis:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features --locked -- -D warnings
```

## Checklist for MCP Tool Changes

When adding a tool or changing a public tool contract, update all of the
following together:

1. The request/output type and tool description in `src/server.rs`.
2. The indexing or search logic and its unit tests.
3. The expected tool-name list, `outputSchema`, and lifecycle checks in
   `tests/mcp_interface.rs`.
4. At least one relevant YAML scenario when the behavior can be expressed as a
   user-facing search example.
5. Snapshots, but only after manually reviewing the diff.
6. The tool list in `README.md` and, when necessary, `docs/testing.md`.

A change to a public response shape is an API change. Document it, and add or
update an ADR when the decision has lasting architectural consequences.

## Testing

Fast targeted runs:

```bash
cargo test --lib --locked
cargo test --test mcp_interface --locked
cargo test --test mcp_scenarios --locked
```

The complete pre-handoff verification gate is:

```bash
scripts/test-pyramid.sh
```

It runs formatting, warning-as-error Clippy, every Cargo test target, and the
pinned MCP conformance scenarios applicable to the production capabilities.
The npm-based steps may require network access on their first run.

Test modification rules:

- Cover algorithmic edge cases in the unit tests in `src/index.rs`.
- Always validate the public protocol through a real Streamable HTTP MCP client;
  do not rely only on direct Rust method calls.
- Integration tests must use a temporary fixture repository and
  `127.0.0.1:0`. They must not depend on the developer's `~/.m2` contents or a
  fixed port.
- YAML scenario `id` values must be unique and stable because they also become
  snapshot names.
- Scenario expectations must target public results through `result_count`,
  partial recursive `contains`, or exact `equals`.
- For intentional snapshot changes, use the `cargo insta review` workflow.
  Never accept a snapshot blindly just to make a test pass.
- The generated human-readable report is
  `target/mcp-test-report/report.md`; inspect it when needed, but do not commit
  it.

Manually inspect MCP behavior with the fixture repository:

```bash
scripts/run-inspector.sh
scripts/run-inspector.sh --cli --method tools/list
```

## Docker and Runtime Security

Build and start with Compose:

```bash
docker build -t maven-mcp .
MAVEN_REPO_PATH="$HOME/.m2/repository" docker compose up --build
```

Docker runtime invariants:

- multi-stage release build;
- numeric non-root user `10001:10001`;
- read-only bind mount for the Maven repository;
- read-only root filesystem and a constrained `/tmp` tmpfs;
- all capabilities dropped and `no-new-privileges` enabled;
- a health check that uses the binary's own `--healthcheck` mode;
- the MCP port published only on `127.0.0.1` by default.

After changing the Dockerfile or Compose configuration, run at least:

```bash
MAVEN_REPO_PATH="$HOME/.m2/repository" docker compose config --quiet
docker build -t maven-mcp .
```

Do not copy a Maven repository, credentials, or an `.env` file into the image.

## Documentation and Handoff

- Update `README.md` when user-visible behavior, tools, configuration, or run
  commands change.
- Update `docs/testing.md` when the testing workflow changes.
- Record significant architectural or public API decisions in an ADR under
  `docs/decisions/`.
- Before handoff, run checks proportionate to the change risk. The mandatory
  final gate for public MCP or broad changes is `scripts/test-pyramid.sh`.
- Do not commit, push, or accept snapshots without a specific user request.
