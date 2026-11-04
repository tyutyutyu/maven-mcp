# Support

`maven-mcp` is beta software maintained by a single volunteer, István
Földházi. Support is best-effort and happens in public GitHub issues; there is
no paid support, no service-level agreement and no guaranteed response time.

## Before asking

1. Use the latest release. During the beta only the latest release is
   supported (see
   [Maturity, support and deprecation](README.md#maturity-support-and-deprecation)).
2. Check that you are on a [supported platform](README.md#supported-platforms).
3. Work through [Troubleshooting](README.md#troubleshooting) and the relevant
   README section, for example
   [Opt-in Project Execution](README.md#opt-in-project-execution) for trusted
   directories.
4. Search the existing [issues](https://github.com/tyutyutyu/maven-mcp/issues),
   including closed ones.

## Where to ask

| Need | Channel |
| --- | --- |
| Something does not work as documented | [Bug report](https://github.com/tyutyutyu/maven-mcp/issues/new?template=bug_report.yml) |
| A new tool, option or behavior | [Feature request](https://github.com/tyutyutyu/maven-mcp/issues/new?template=feature_request.yml) |
| A suspected vulnerability | Private report, see [SECURITY.md](SECURITY.md) |
| A code or documentation change | Pull request, see [CONTRIBUTING.md](CONTRIBUTING.md) |

Questions about configuration or usage can use the bug report form when the
documented behavior does not match what you see.

## What to include

A useful report contains the `maven-mcp --version` output, the installation
channel, the Linux target, the MCP host and its version, the Maven and Java
versions, and a minimal reproduction: the tool call and arguments, ideally
against a small synthetic Maven project with public dependencies.

Never post credentials, Maven `settings.xml` or `settings-security.xml`
contents, private source code, complete raw logs or complete local paths.
Quote only the relevant lines and replace private values with placeholders
such as `<PROJECT_ROOT>` or `<HOME>`.
