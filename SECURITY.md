# Security Policy

## Reporting a vulnerability

Report security problems privately through GitHub Private Vulnerability
Reporting:
<https://github.com/tyutyutyu/maven-mcp/security/advisories/new>
(the **Report a vulnerability** button on the repository's **Security** tab).

Do not open a public issue, pull request or discussion for a suspected
vulnerability, and do not include exploit details in public comments.

Include in the report:

- the `maven-mcp --version` output and how you installed it (GitHub Release
  archive, crates.io, or a source build);
- the Linux target and distribution;
- the affected tool, command or environment variable;
- a minimal reproduction, ideally a small synthetic Maven project with public
  dependencies only;
- the impact you expect, for example which trust boundary is crossed.

Do not send real credentials, private Maven `settings.xml` or
`settings-security.xml` files, private source code, or complete local paths.
Replace them with placeholders; the private advisory lets the maintainer ask
for more detail when needed.

## Supported versions

`maven-mcp` is beta software (`0.y.z`). Only the latest release is supported,
as described under
[Maturity, support and deprecation](README.md#maturity-support-and-deprecation).

| Version | Supported |
| --- | --- |
| Latest `0.y.z` release | Yes |
| Older releases | No |

Supported platforms are the Linux targets listed under
[Supported Platforms](README.md#supported-platforms):
`x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu`. Reproduce a problem
on the latest release and a supported target before reporting it. Security
fixes ship only as a new release of the newest `0.y` series; they are not
backported.

## Scope

In scope are problems where `maven-mcp` breaks one of its documented security
properties, for example:

- a Maven-backed operation that runs for a `project_path` outside
  `MAVEN_TRUSTED_PROJECT_DIRECTORIES`, or a path or symlink that escapes the
  trusted directory check;
- data from one project context (index, test results, stored oversized
  results) that is returned to a request for another project;
- a credential pattern that the documented
  [redaction](README.md#redaction-and-data-minimization) should remove but
  returns, or source code, secrets or tool results that reach logs, disk or
  the runtime status file against the documentation;
- a Maven child process that survives EOF, SIGINT, SIGTERM, a timeout or a
  cancellation;
- non-protocol output on stdout, or a crafted archive or request that crashes
  the server.

Out of scope are documented limits of the design:

- Maven plugins, tests and build scripts run with the local user's permissions;
  native Maven execution is not a sandbox;
- `run_maven` arguments such as `-f` or `--settings` may point outside the
  trusted project, and goals such as `deploy` may publish artifacts;
- secrets without a recognizable key, header or token format, which
  pattern-based redaction cannot detect;
- vulnerabilities in Maven, the JDK, an MCP host or a third-party dependency
  that `maven-mcp` does not make worse (report those upstream; a dependency
  advisory that affects `maven-mcp` is welcome);
- unsupported platforms and releases other than the latest one.

## Response expectations

`maven-mcp` has a single volunteer maintainer, István Földházi, and no
service-level agreement. Reports are handled on a best-effort basis:

- The maintainer aims to acknowledge a report within 14 days and to post a
  status update in the private advisory at least every 30 days until it is
  resolved. These are goals, not guarantees.
- A confirmed vulnerability is fixed in a new release of the newest `0.y`
  series. The changelog lists it in the `Security` group, and the GitHub
  security advisory is published after the release is available.
- Reporters are credited in the advisory unless they ask not to be.

Please allow time for a fix before disclosing a vulnerability publicly.
