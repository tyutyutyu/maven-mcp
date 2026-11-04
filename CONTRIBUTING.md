# Contributing to maven-mcp

Thank you for helping. This guide explains how to report problems, propose
changes and get a pull request merged. Everyone taking part follows the
[Code of Conduct](CODE_OF_CONDUCT.md).

## Before you start

- **Security problems** go through private reporting, never a public issue or
  pull request. See [SECURITY.md](SECURITY.md).
- **Questions and help** are described in [SUPPORT.md](SUPPORT.md).
- **Bugs and feature requests** use the issue forms. Search existing issues
  first, and reproduce a bug on the latest release.
- **Larger changes** (a new tool, a changed public response shape, a new
  dependency, platform or transport) start with an issue, so the direction can
  be agreed before you write the code. Small fixes and documentation
  corrections can go straight to a pull request.

`maven-mcp` has a single volunteer maintainer, István Földházi. Issues and pull
requests are reviewed on a best-effort basis; there is no guaranteed response
time.

## Development setup

- Linux `x86_64-unknown-linux-gnu` or `aarch64-unknown-linux-gnu`; other
  operating systems do not compile (see
  [Supported Platforms](README.md#supported-platforms)).
- The Rust toolchain pinned in `rust-toolchain.toml`; rustup installs it on
  first use. Code must also build with the minimum supported Rust version
  (`rust-version` in `Cargo.toml`).
- For the full gate: `cargo install cargo-llvm-cov --version 0.9.0 --locked`
  and `python3`.
- Node.js is needed only for MCP Inspector and the optional agent evaluation.

[`AGENTS.md`](AGENTS.md) describes the architecture contracts, layer
boundaries, test rules and the checklist for changing an MCP tool; it applies
to human contributors as well. [Local Development](README.md#local-development)
explains the test levels.

## Making a change

1. Fork the repository and create a short-lived branch from `main`.
2. Keep the change focused on one problem. Use `Cargo.lock` and `--locked`
   for every Cargo command; update `Cargo.lock` only for an intended dependency
   change.
3. Add or update tests. Integration tests use temporary fixtures and a real
   child-process STDIO MCP client; they must not depend on your `~/.m2`, a
   network port or a real Maven installation. Review snapshot changes with
   `cargo insta review`; never accept them blindly.
4. Update `README.md` when user-visible behavior, tools, configuration or
   commands change, and add a `CHANGELOG.md` entry under `Unreleased` for every
   user-facing change (see
   [Changelog and release notes](README.md#changelog-and-release-notes)).
5. Run the checks:

   ```bash
   cargo fmt --check
   cargo clippy --all-targets --all-features --locked -- -D warnings
   scripts/test-pyramid.sh
   cargo build --release --locked
   ```

   `scripts/test-pyramid.sh` is the same gate as the required `verify` check:
   formatting, Clippy, every test target and at least 80% Rust line coverage.
6. Open a pull request to `main` and fill in the template.

The required `verify` check must pass and be up to date before merging. The
maintainer squash-merges accepted pull requests, so the pull request title
should describe the change; [Conventional Commits](https://www.conventionalcommits.org/)
style (`fix: ...`, `feat: ...`, `docs: ...`) is preferred.

## Keep private data out

Issues, pull requests, logs, test fixtures and commits are public. Do not
include:

- credentials, tokens, private keys, or Maven `settings.xml` and
  `settings-security.xml` contents;
- private source code, internal artifact coordinates or repository URLs;
- complete local paths or user names; replace them with placeholders such as
  `<PROJECT_ROOT>`, `<MAVEN_REPOSITORY>` or `<HOME>`;
- complete raw logs, agent transcripts, evaluation reports or
  `target/` output; quote only the relevant, redacted lines.

## Contribution terms

`maven-mcp` is licensed under the [MIT License](LICENSE). Contributions are
accepted under the same license: by submitting a pull request you confirm that
you have the right to contribute the code and license it under those terms.
No CLA or DCO sign-off is required, and signed commits or tags are not
required.
