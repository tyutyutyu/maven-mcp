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
  `project_path`; Maven-backed operations require a trusted project directory
  from `MAVEN_TRUSTED_PROJECT_DIRECTORIES`.
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
