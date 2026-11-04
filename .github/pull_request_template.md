## Summary

<!-- What changes and why. Link the related issue, for example "Fixes #123". -->

## How it was tested

<!--
The commands you ran and any manual check, for example a tool call against a
small synthetic Maven project. Quote only relevant, redacted output: no
credentials, Maven settings files, private source code, complete raw logs or
complete local paths (use placeholders such as <PROJECT_ROOT> or <HOME>).
-->

## Checklist

- [ ] `scripts/test-pyramid.sh` and `cargo build --release --locked` pass
      locally (see "Making a change" in `CONTRIBUTING.md`).
- [ ] New or changed behavior has tests; snapshot changes were reviewed, not
      accepted blindly.
- [ ] A changed MCP tool or public response shape follows the "Checklist for
      MCP Tool Changes" in `AGENTS.md`, or this pull request changes no tool.
- [ ] `CHANGELOG.md` has an entry under `Unreleased` for every user-facing change
      (see "Changelog and release notes" in `README.md`), or this pull request has
      no user-facing change.
- [ ] `README.md` is updated when user-visible behavior, tools, configuration, or
      commands change.
- [ ] The diff, tests, fixtures and this description contain no credentials,
      private source code, Maven settings, raw logs or complete local paths.
- [ ] I have the right to contribute this change under the project's MIT
      License (no CLA or DCO sign-off is required).
