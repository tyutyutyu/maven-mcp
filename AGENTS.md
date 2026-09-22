# AGENTS.md

## Project Overview

`maven-mcp` is a native STDIO MCP server written in Rust 2024. The MCP host owns
the child-process lifecycle. Startup must not scan a local Maven repository or
bind the process to one project; repository- or project-dependent tool requests
must carry an explicit absolute `project_path`.

The primary user documentation is `README.md`. This file is the source of
agent-specific development rules.

## Technology and Prerequisites

- Rust `1.89` or later, using the 2024 edition.
- Cargo; always use the versioned `Cargo.lock` file and the `--locked` flag for
  reproducible commands.
- Inspector and agent evaluation require `node` and `npx`.
- The project has no database and no separate build-time code generation step.

## Repository Map

- `src/config.rs`: environment configuration and request-independent limits.
- `src/index.rs`: Maven layout recognition, JAR/ZIP processing, the in-memory
  index, and all search operations.
- `src/server.rs`: MCP request/output types, tool definitions, and handlers.
- `src/main.rs`: STDIO transport, EOF, SIGINT, and SIGTERM
  lifecycle.
- `tests/support/mod.rs`: deterministic temporary Maven fixture repository and a
  real child-process STDIO test server.
- `tests/mcp_interface.rs`: integration contract for the public MCP tool catalog
  and error semantics.
- `tests/scenarios/maven_search.yaml`: human-readable search examples.
- `tests/mcp_scenarios.rs`: scenario interpretation, semantic validation,
  snapshots, and Markdown report generation.
- `tests/snapshots/`: reviewable public MCP response contracts.
- `scripts/`: full verification, Inspector, and agent-evaluation entry points.

## Important Architectural Contracts

- Do not build a complete local repository index at startup. Project-scoped
  repository indexes are created from explicit request context only.
- Request project identity is explicit: every repository- or project-dependent
  MCP request requires an absolute `project_path`, which must canonicalize to a
  directory containing `pom.xml`.
- Multiple project contexts may be interleaved in one STDIO process. Cache,
  runner, and last-test state must be keyed by canonical project root.
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

No startup project or repository path is required. Variables and the optional
Java-selection flag are:

- `MAX_RESULTS`, default: `100`; must be a positive integer.
- `MAX_SOURCE_BYTES`, default: `1048576`; must be a positive integer.
- `MAX_PROJECT_INDEXES`, default: `4`; must be a positive integer.
- `MAVEN_TRUSTED_PROJECT_DIRECTORIES`, optional platform path-list of existing
  absolute directory trees. Maven-backed project operations are disabled when
  unset; a canonical `project_path` must be equal to or below one configured
  directory.
- `MAVEN_MCP_RUNTIME_DIR`, optional user-private directory for `maven-mcp stats`
  runtime status files.
- `--jenv`, optional STDIO-server flag that resolves Java per canonical request
  project through `JENV_ROOT/bin/jenv`; `JENV_ROOT` defaults to `$HOME/.jenv` and
  must be absolute when set explicitly.
- `RUST_LOG`, recommended default: `maven_mcp=info`.

Run locally:

```bash
cargo run --locked
```

The binary speaks MCP JSON-RPC on stdout and writes diagnostics to stderr. There
is no port or health endpoint. The process becomes ready without a Maven scan;
requests select trusted Maven projects through `project_path`.

When `--jenv` is enabled, resolve Java before every Maven child from that
request's canonical project root. Ignore inherited `JENV_VERSION` and `JENV_DIR`,
validate the absolute selected home and executable `bin/java`, and modify
`JAVA_HOME`/`PATH` only on the Maven child. When disabled, preserve inherited
Java environment behavior.

Inspect live host-managed instances without starting MCP transport:

```bash
cargo run --locked -- stats
cargo run --locked -- stats --json
```

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
6. The tool list and relevant usage guidance in `README.md`.

A change to a public response shape is an API change. Document it in
`README.md`.

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

It runs formatting, warning-as-error Clippy, and every Cargo test target. The
integration, lifecycle, scenario, and snapshot targets all start the production
binary through a real child-process STDIO MCP client.

Test modification rules:

- Cover algorithmic edge cases in the unit tests in `src/index.rs`.
- Always validate the public protocol through a real child-process STDIO MCP
  client; do not rely only on direct Rust method calls.
- Integration tests must use a temporary fixture repository. They must not
  depend on the developer's `~/.m2` contents or a network port.
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

## Native Runtime Security

- STDIO is the only supported transport; do not add a listening socket or
  daemon lifecycle without a separately approved ADR.
- stdout is protocol-only. Logging and diagnostics must remain on stderr.
- Project execution is selected per request through explicit `project_path`, but
  authorization is granted only by `MAVEN_TRUSTED_PROJECT_DIRECTORIES`. Never
  infer trust from the client working directory or the requested path itself.
- The canonical project root must be equal to or nested below a configured
  trusted directory; use component-aware path containment after canonicalizing
  both sides, so symlinks cannot escape the directory tree.
- One Maven process may run at a time across the whole STDIO server, including
  interleaved trusted project contexts.
- Native Maven execution is not a sandbox; plugins and tests run with the local
  user's permissions.
- EOF, SIGINT, and SIGTERM must stop the server and every active Maven process
  group without leaving an orphan.

## Documentation and Handoff

- Update `README.md` when user-visible behavior, tools, configuration, or run
  commands change.
- Before handoff, run checks proportionate to the change risk. The mandatory
  final gate for public MCP or broad changes is `scripts/test-pyramid.sh`.
- At the end of every task, after the required verification and before handoff,
  rebuild the release executable with `cargo build --release --locked`. This is
  mandatory even when an earlier command already built or tested another Cargo
  profile.
- Do not commit, push, or accept snapshots without a specific user request.
