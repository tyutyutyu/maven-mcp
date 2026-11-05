# Changelog

All user-facing changes to `maven-mcp` are recorded in this file. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions
follow the beta [versioning rules](README.md#versioning-and-compatibility) in
`README.md`. Every pull request with a user-facing change adds an entry under
`Unreleased`; a release moves those entries into a dated version section, which
becomes the GitHub Release notes.

## [Unreleased]

### Added

- Native STDIO MCP server for request-scoped Maven project inspection. Every
  repository- or project-dependent tool takes an explicit absolute
  `project_path`; Maven-backed operations require its canonical project root to
  be inside a trusted directory tree from `MAVEN_TRUSTED_PROJECT_DIRECTORIES`.
- Class, JAR, JAR entry, JAR content, source, declaration source, class member,
  type hierarchy, class reference, service provider, artifact version, artifact
  POM, artifact diagnosis, and exact local API comparison tools.
- Opt-in Maven execution tools with serialization, timeouts, output limits,
  redaction, and process-group cleanup, plus request-scoped jenv Java selection
  (`--jenv`).
- Persistent, incrementally refreshed on-disk index cache and explicit project
  refresh.
- `maven-mcp --version` and `maven-mcp stats` runtime diagnostics, and the
  `maven-agent-log` and `maven-benchmark` command-line tools.
- Versioned local installation with candidate, stable, and previous aliases.
- Native Linux release archives for `x86_64-unknown-linux-gnu` and
  `aarch64-unknown-linux-gnu` with SHA-256 checksums, and the crates.io package.
- Documented index lifecycle (startup, first request, reuse, change detection,
  refresh and restart) and the beta maturity, compatibility, deprecation, and
  support policy.
- `maven-mcp stats` reports every active project with its index `state`
  (`ready`, `building`, `not_indexed`), `classpath_entries` and
  `build_duration_ms`, plus the process start time and an `unknown_instances`
  count (additive `stats --json` fields). Cache hits update the owner-only status
  file at most once per second. SIGTERM also removes the file. A status file is
  deleted only when its process is proven gone; an access error or an unknown
  liveness keeps it.
- `RUST_LOG=maven_mcp=debug` logs one correlated start and finish event per
  `tools/call` (internal `call_id`, known tool name, `outcome`, `duration_ms`),
  project runner and index cache hits, misses, builds and evictions, runtime
  status publication, and the Maven child lifecycle (operation category, permit
  wait, run time, status, exit code, timeout, truncation and redaction count).
  The default `maven_mcp=info` events are unchanged. Warnings and debug events
  never quote paths, archive or class names, coordinates or error messages; they
  carry stable `reason` and `error_kind` categories. stderr is colored only when
  it is a terminal.
- Oversized tool results are pageable: a `tools/call` result whose complete
  serialization exceeds `MAX_MCP_RESPONSE_BYTES` (default `12288`) is kept in
  process memory and returned as an object-rooted `deferred_result` marker, and
  the new `read_tool_result_page` tool returns its JSON in cursor-linked pages
  that each fit the limit and rebuild the original byte for byte. Stored results
  are bound to the canonical project, limited to 16 results and
  `MAX_MCP_RESULT_CACHE_BYTES` (default 64 MiB), and expire after 300 seconds; a
  result above that budget is an explicit MCP error. Results within the limit
  are unchanged. Every tool's `outputSchema` now allows the optional
  `deferred_result` marker as an `anyOf` alternative to its required fields.
- Public contribution, conduct, support, and security governance:
  `CONTRIBUTING.md` (contribution process, required checks, private-data rules,
  and MIT inbound=outbound terms without a CLA or DCO), `CODE_OF_CONDUCT.md`,
  `SUPPORT.md`, and `SECURITY.md` (GitHub Private Vulnerability Reporting, the
  latest beta on the supported Linux targets, and best-effort response goals
  without an SLA). GitHub bug report and feature request issue forms, an issue
  chooser configuration with the private security reporting link, and an
  extended pull request template ask for version, platform, and minimal
  reproduction data while rejecting credentials, Maven settings, raw logs, and
  full local paths. `README.md` links every governance document.
- One English terminology across the public documentation and the client-facing
  MCP text: `README.md` gains a documentation map, an architecture and security
  model, and a terminology table (native STDIO, `project_path`, canonical
  project root, trusted directory tree, lazy project index, partial result,
  deferred result), and `AGENTS.md` records the documentation language and
  terminology rule. The server instructions, the `project_path` descriptions,
  and the `index_stats`, `inspect_maven_project`, `run_maven`,
  `run_maven_lifecycle`, `list_maven_test_classes`,
  `get_last_maven_test_failures`, `refresh_maven_project`, `search_source`, and
  `search_jar_content` descriptions use these terms.

### Fixed

- `get_maven_classpath` with `kind: "build"` now returns only the compile-scope
  classpath. It passed `-Dmdep.includeScope`, which `maven-dependency-plugin`
  ignores, so the build classpath also listed test-scope dependencies; it now
  passes the plugin's `-DincludeScope` user property for both kinds. The
  `benchmark.json` shell baselines use the same property.
