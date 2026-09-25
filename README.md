# Maven MCP

A native MCP server written in Rust for request-scoped Maven project inspection.
An MCP host such as Codex or GitHub Copilot starts the binary as a child process
and communicates with it exclusively over STDIO. Startup does not scan
`~/.m2/repository` and does not require a preselected project.

## Features

Every repository- or project-dependent MCP tool requires a `project_path`
argument containing an absolute Maven project root with `pom.xml`. The server
canonicalizes the path per request; Maven-backed operations accept it only when
it is inside a host-configured trusted directory tree, never because of the
client's current working directory.

The server exposes the following MCP tools:

- `index_stats` – statistics for the request-scoped project index.
- `search_classes` – returns every containing JAR for a partial or fully
  qualified class name.
- `search_jars` – searches JARs by coordinate, artifact, version, filename, or
  relative path.
- `search_jar_entries` – searches file and class entries inside JARs, optionally
  restricted to one JAR.
- `list_jar_classes` – paginated class listing for a selected JAR.
- `get_class_source` – reads Java/Kotlin source from the associated
  `-sources.jar` by fully qualified class name and optional JAR or version.
- `describe_class` – reads classfile hierarchy, modifiers, generic signatures,
  constructors, methods, fields, and annotations without a sources JAR, using a
  `public` or `all` view.
- `get_jar_entry` – reads one exact entry from one exactly selected JAR;
  distinguishes UTF-8 text from binary bytes and reports size and truncation.
- `get_artifact_pom` – returns parent, packaging, properties, dependencies,
  dependency management, and BOM imports from the local POM of an exact Maven
  coordinate, without resolving the project classpath.
- `diagnose_artifact` – summarizes the POM, binary, classifier, checksum,
  `.lastUpdated`, repository, and corrupt-JAR state of a local artifact without
  resolving the project classpath or exposing absolute paths.
- `search_class_members` – searches classfile metadata by method, field, or
  annotation name, with an optional JAR filter.
- `search_type_hierarchy` – finds direct or transitive implementations and
  descendants of interfaces and base classes, including relationship type,
  depth, and hierarchy path.
- `search_class_references` – searches inbound or outbound class, field, and
  method references from the classfile constant pool; a result is a reference,
  not proof of a runtime call site.
- `compare_artifact_api` – compares the public/protected API of two local
  versions of the same artifact at class, member, superclass, and interface
  levels.
- `search_jar_content` – searches supported UTF-8 JAR resources with bounded
  context; binary, oversized, and unsupported entries are not interpreted as
  text.
- `search_source` – searches Java/Kotlin sources from local source artifacts by
  substring or regular expression, with line numbers and bounded context.
- `get_declaration_source` – returns a focused source excerpt for a class, field,
  or method; overloads can be disambiguated with a JVM descriptor.
- `search_providers` – returns structured Java ServiceLoader, JPMS
  `uses`/`provides`, `spring.factories`, and Spring `.imports` provider
  declarations.
- `list_artifact_versions` – lists locally available versions of an artifact,
  with an optional `groupId` filter.

Project inspection and execution use the same request `project_path`:

- `inspect_maven_project` – models the root project, Maven Wrapper, and recursive
  reactor modules.
- `run_maven_lifecycle` – runs the allowlisted `compile`, `test-compile`, or
  `verify` lifecycle.
- `list_maven_test_classes`, `run_maven_test`,
  `get_last_maven_test_failures` – focused Surefire test execution and
  structured failures.
- `get_effective_pom`, `get_dependency_tree`, `get_maven_classpath` – structured
  project and dependency diagnostics resolved by Maven.
- `explain_dependency_resolution` – ties selected and omitted dependency
  versions to modules and complete paths, marking `direct`, `nearest`,
  `dependency-management`, `conflict`, or `duplicate` mediation.
- `get_jacoco_coverage`, `get_jacoco_coverage_gaps` – read-only summaries of
  existing JaCoCo XML reports and the least-covered classes; these tools do not
  start a Maven process.

List-like tools use an MCP-compatible structured root object:
`{ "results": [...] }`.

Repository inspection tools are read-only: they do not extract files or modify
Maven metadata. `get_artifact_pom` and `diagnose_artifact` read an exact coordinate
directly from the local Maven repository and work even when another reactor
module's dependency cannot be resolved. They use `MAVEN_EXECUTION_REPO_PATH` when
set, otherwise `$HOME/.m2/repository`. Set `MAVEN_EXECUTION_REPO_PATH` to the
same local repository configured in Maven settings when it differs from the
default. Other repository-search tools ask Maven for
the effective test classpath, build a lazy index only from those local JARs and
their sibling `-sources.jar` files, and cache that index by canonical
`project_path`. For a multi-module reactor, classpath sections from every module
are aggregated; empty parent or aggregator POM sections do not hide dependencies
reported by later modules. Artifacts present elsewhere in the same local Maven
repository are not searched unless Maven selected them for the requested
project. Repeated classfile fact strings are stored once per JAR and referenced
by compact numeric identifiers, so large dependency sets do not retain a
separate allocation for every constant-pool reference. The effective classpath
must be fully resolvable; for example, a reactor dependency on an unbuilt sibling
SNAPSHOT can prevent the index from being created. In that case the MCP error
includes a bounded, redacted summary of Maven's error output, regardless of
whether Maven wrote it to stdout or stderr.

`search_jar_content` examines the manifest, `META-INF/services/*` descriptors,
and UTF-8 entries with the extensions `conf`, `config`, `factories`, `imports`,
`json`, `list`, `properties`, `txt`, `xml`, `yaml`, and `yml`. Each entry is
limited by `MAX_SOURCE_BYTES`, while the aggregate read budget is
`MAX_SOURCE_BYTES × MAX_RESULTS`. The `incomplete` field reports when size or
result limits prevented a complete search.

`search_providers` interprets `META-INF/services/*`, JPMS `module-info.class`
`uses`/`provides`, `META-INF/spring.factories`, and
`META-INF/spring/*.imports`. Spring `.handlers` and `.schemas` files are not
provider descriptors and therefore are not included in this structured view.

## Native STDIO startup

```bash
cargo build --release --locked --bin maven-mcp
```

Configure the MCP host to run the resulting `target/release/maven-mcp` binary and
pass request `project_path` values in tool calls. The host owns process startup,
shutdown, and STDIO; there is no port, URL, daemon, health endpoint, Docker
image, or manual server lifecycle. Startup is lightweight because no repository
index is built before initialization finishes.

Add `--jenv` to the server command when Maven children must use each project's
jenv-selected Java instead of relying on the MCP host's inherited `JAVA_HOME`
and `PATH`.

Runtime diagnostics are available without starting an MCP transport:

```bash
maven-mcp stats
maven-mcp stats --json
```

The stats command reads user-private runtime status files for live host-managed
STDIO instances and reports PIDs, process RSS where the platform exposes it,
active project indexes, index sizes, and cache hit/miss/eviction counters. If no
server is running, it exits successfully with an empty report.

For local source-tree inspection, the bundled helper builds the binary and lets
MCP Inspector start it over STDIO:

```bash
scripts/run-inspector.sh
scripts/run-inspector.sh --cli --method tools/list
```

### Codex project configuration

In a trusted project, add `.codex/config.toml` with an absolute installed binary
path:

```toml
[mcp_servers.maven-mcp]
command = "/absolute/path/to/maven-mcp"
args = ["--jenv"]
startup_timeout_sec = 300
```

Alternatively, register the same STDIO command with `codex mcp add`. Verify it
with `codex mcp list` or `/mcp`. See the
[official Codex MCP documentation](https://learn.chatgpt.com/docs/extend/mcp?surface=cli).

### GitHub Copilot CLI configuration

Copilot CLI defaults local commands to STDIO. Register the installed binary and
300-second timeout from a shell where the Maven repository path is known:

```bash
copilot mcp add \
  --timeout 300000 \
  maven-mcp -- /absolute/path/to/maven-mcp --jenv
copilot mcp get maven-mcp
```

See the
[official GitHub Copilot CLI MCP documentation](https://docs.github.com/en/copilot/how-tos/copilot-cli/customize-copilot/add-mcp-servers).

## Opt-in Project Execution

Project execution requires both a host-configured trusted directory and a request
`project_path` below that directory. Set `MAVEN_TRUSTED_PROJECT_DIRECTORIES` to a
platform path-list of existing absolute directories; the server canonicalizes the
directories at startup. A configured directory can contain Maven projects at any
depth, and may itself be a Maven project root. Without this variable, all
Maven-backed project operations are unavailable. The client-supplied
`project_path` validates the selected project; it never grants trust itself.

For example, on Unix:

```bash
export MAVEN_TRUSTED_PROJECT_DIRECTORIES="/work/trusted-projects:/srv/build-roots"
```

Then the client may select a Maven project below either directory:

```json
{
  "tool": "inspect_maven_project",
  "arguments": {
    "project_path": "/work/trusted-projects/team-a/service"
  }
}
```

The project is writable because Maven creates `target/` files. The execution
repository, when configured, must be a separate, dedicated writable directory.
Maven is offline by default (`--offline`); set `MAVEN_EXECUTION_NETWORK=true`
only for trusted projects that may resolve dependencies and plugins. The server
does not accept raw Maven goals or arguments, runs at most one Maven process at a
time across the entire STDIO server, applies time and output limits, and redacts
paths and common credential patterns. Native execution is not a sandbox: Maven
plugins and tests run with the local user's permissions.

When an offline Maven run fails because a required remote plugin or artifact is
not cached locally, the returned `build.policy_notice` explains that the result
may reflect an intentional security policy or missing MCP server configuration,
not necessarily a project error. It also names `MAVEN_EXECUTION_NETWORK=true` as
the opt-in setting for trusted projects. The optional field is omitted from
successful runs and failures unrelated to offline artifact resolution.

### Request-scoped jenv Java selection

Start the STDIO server with `maven-mcp --jenv` to resolve Java separately for
every Maven invocation. The server uses `JENV_ROOT` when set, otherwise
`$HOME/.jenv`, and runs that installation's `bin/jenv prefix` without a shell
from the request's canonical `project_path`. Inherited `JENV_VERSION` and
`JENV_DIR` values are deliberately ignored so an unrelated MCP host working
directory or shell override cannot replace the project's `.java-version`
selection.

The returned Java home must be absolute, readable, and contain executable
`bin/java`. Only the Maven child receives the resolved `JAVA_HOME` and a `PATH`
with that JDK's `bin` prepended; the MCP server environment is not mutated. A
missing jenv installation, unknown project version, or invalid JDK is returned
as a structured `runner_error`, and Maven is not started. Without `--jenv`, the
existing inherited `JAVA_HOME` and `PATH` behavior is unchanged.

## Environment Variables

| Variable | Default | Meaning |
| --- | --- | --- |
| `MAX_RESULTS` | `100` | Maximum number of items in one tool response. |
| `MAX_SOURCE_BYTES` | `1048576` | Maximum size of one source, exact JAR entry, or processed class/resource; returned content is truncated, while oversized inspection input is skipped or reported as an error. |
| `MAX_PROJECT_INDEXES` | `4` | Maximum number of canonical project roots retained in the in-process project-index cache. |
| `MAVEN_TRUSTED_PROJECT_DIRECTORIES` | none | Required for Maven-backed project operations. Platform path-list of existing absolute directory trees; a canonical `project_path` must be equal to or nested below one entry. |
| `MAVEN_EXECUTABLE` | none | Absolute Maven binary path; required only when no valid executable Maven Wrapper is available. |
| `MAVEN_EXECUTION_REPO_PATH` | none | Optional existing writable Maven local repository for request-scoped Maven execution and exact artifact inspection. Exact artifact inspection otherwise reads `$HOME/.m2/repository`. |
| `MAVEN_EXECUTION_NETWORK` | `false` | When `true`, `--offline` is not added to Maven commands. |
| `MAVEN_TIMEOUT_SECONDS` | `300` | Maximum runtime of one Maven child process. |
| `MAX_MAVEN_OUTPUT_BYTES` | `1048576` | Separate upper limit for stdout and stderr. |
| `MAVEN_MCP_RUNTIME_DIR` | `$XDG_RUNTIME_DIR/maven-mcp` or `$HOME/.cache/maven-mcp/runtime` | User-private directory for live runtime status files consumed by `maven-mcp stats`. |
| `JENV_ROOT` | `$HOME/.jenv` | jenv installation root used only when the server starts with `--jenv`; must be an absolute path when explicitly set. |
| `RUST_LOG` | `maven_mcp=info` | Logging level/filter. |

`get_class_source` returns a result only when the corresponding sources artifact
is present in the local repository. For Maven projects, these artifacts can be
downloaded with commands such as `mvn dependency:sources`.

## Agent Log Analyzer CLI

`maven-agent-log` is a standalone binary: it does not start an MCP server, use an
LLM, or establish a network connection. It considers only actual shell/terminal
tool calls; command examples appearing in messages, patches, documentation, or
tool output are not treated as executions.

```bash
cargo run --locked --bin maven-agent-log -- ~/.codex/sessions \
  --format markdown --category maven --group-by project

cargo run --locked --bin maven-agent-log -- session.json \
  --source vs-code --since 2026-08-01 --format jsonl

cargo run --locked --bin maven-agent-log -- --discover --format terminal
```

Supported explicit `--source` values are `vs-code`, `vs-code-insiders`, `codex`,
`kilo`, `intelli-j`, and `auto`. `auto` treats `.json` as a VS Code/Copilot chat
session, `.jsonl` as a Codex session, `.db`/`.sqlite` as a Kilo Code database,
and `.log`/`.txt` as an IntelliJ log. Kilo databases are opened only through a
read-only SQLite connection; tables from different Kilo versions are inspected
schema-tolerantly through textual payload columns. The IntelliJ parser accepts
only explicit `Executing command:`, `Terminal command:`, and `Shell command:` log
events.

A normalized event contains the timestamp, IDE, agent, project, session, tool
name, working directory, command, category, and—for Maven—structured goal,
module, reactor, test, profile, and property data. It recognizes `mvn`, `mvnw`,
`mvnw.cmd`, and `mvnd`, as well as repository inspection based on `find`, `jar`,
`unzip`, `javap`, `grep`, and `rg`. Repeated progressive log entries with the
same session/tool/cwd/command are deduplicated.

Filters: `--since`, `--until`, `--ide`, `--agent`, `--project`, `--session`, and
`--category`. Grouping: `--group-by ide|agent|project|session|category`. Output
formats: `terminal`, `json`, `jsonl`, `csv`, and `markdown`; `--output` writes to
a file.

By default, the home directory, `--workspace`, the user-home pattern, URL
credentials, password/token/secret/authorization/API-key values, and every
repeated `--redact-pattern` are redacted. The CLI never copies the complete raw
log into the report. Use `--unsafe-no-redact` only for deliberate local
debugging. Automatic discovery locations depend on platform and IDE version; if
a client uses a new or custom storage format, provide the file and source
directly. Optional LLM summarization is intentionally disabled so extraction,
classification, statistics, and reporting remain fully deterministic.

## Shell–MCP Benchmark CLI

`maven-benchmark` performs paired speed and data-volume measurements for
previously discovered agent commands and MCP tool calls serving the same goal.
It starts the configured Maven MCP command as a STDIO child process and writes
measurements to a versioned JSON file from which a separate report can be
generated later.

Benchmark specification format:

```json
{
  "schema_version": 1,
  "name": "class lookup comparison",
  "cases": [
    {
      "id": "find-foo",
      "description": "Find all JARs containing org.example.Foo",
      "shell": {
        "command": "find ~/.m2/repository -name '*.jar' -exec sh -c 'jar tf \"$1\" | grep -q org/example/Foo.class' _ {} \\; -print",
        "cwd": "/path/to/project"
      },
      "mcp": {
        "tool": "search_classes",
        "arguments": {
          "project_path": "/path/to/project",
          "query": "org.example.Foo"
        }
      }
    }
  ]
}
```

`shell.command` is passed to `/bin/sh -c`, so run only specifications you own or
have reviewed. `cwd` is optional. MCP `arguments` can be omitted when the tool
expects no arguments. Case IDs must be unique.

```bash
cargo build --locked --bin maven-mcp
cargo run --locked --bin maven-benchmark -- \
  --spec benchmark.json \
  --mcp-command target/debug/maven-mcp \
  --iterations 10 \
  --warmup 2 \
  --timeout-seconds 300 \
  --output target/benchmark-results.json
```

The default for `--max-output-bytes` is 1 MiB. It limits the output included in
request/response metrics; `output_truncated` reports truncation. Under
`schema_version: "1.0"`, the JSON preserves the shell- and MCP-side request size
for each case, every raw run's duration, success state, response size, and token
estimate, as well as the mean and median of successful runs and the speed and
token ratios between the two sides. A failed command or tool call does not stop
the remaining measurements; its record retains an exit code or error message.

The token metric is intentionally a deterministic, provider-neutral estimate:
`ceil(Unicode scalar values / 4)`. It is not a model tokenizer or API billing
value and may differ from the actual token count depending on language and
content. The report records the method and this limitation as metadata. On the
shell side, the command, stdout, and stderr are included; on the MCP side, the
tool name/arguments and tool-result JSON are included. MCP initialization,
server startup, and protocol headers are not part of the measured tool-call
duration or token value.

## Taskfile Command Interface

The project provides a modular [go-task](https://taskfile.dev/) command
interface. Running `task` without arguments displays the same functional menu as
`task --list`; development commands intentionally appear only in the complete
list:

```bash
task
task --list
task --list-all
```

Main user workflows:

```bash
task agent-log:analyze INPUT="$HOME/.codex/sessions" -- \
  --format markdown --category maven
task agent-log:discover -- --format terminal

task benchmark:run
task benchmark:run SPEC=my-benchmark.json OUTPUT=target/custom-results.json \
  ITERATIONS=10 WARMUP=2
```

The repository includes a default `benchmark.json` specification for common
Maven inspection commands. MCP cases that inspect project or repository state
must include an absolute `project_path` argument.

Every benchmark variable can be overridden. Defaults are `SPEC=benchmark.json`,
`OUTPUT=target/benchmark-results.json`,
`MCP_COMMAND=target/debug/maven-mcp`, `ITERATIONS=5`, `WARMUP=1`,
`TIMEOUT_SECONDS=300`, and `MAX_OUTPUT_BYTES=1048576`. Arguments following `--`
are forwarded unchanged by the `agent-log` and `benchmark` tasks.

Development tasks can be invoked directly without cluttering the default
business menu:

```bash
task dev:build
task dev:format
task dev:lint
task dev:test
task dev:verify
task dev:inspector -- --cli --method tools/list
PROMPTFOO_PROVIDER="openai:responses:gpt-5-mini" task dev:agent-eval
```

`dev:verify` runs the complete existing `scripts/test-pyramid.sh` gate. The
Inspector and agent-eval tasks require `npx`; both launch the MCP binary
directly over STDIO.

## Local Development

```bash
cargo test
cargo run --locked
```

Unit tests cover Maven coordinate recognition, search, pagination, classifier
and multi-release JAR handling, source filtering, truncation, and malformed
JARs. Integration tests start the real binary as a child-process STDIO MCP
server and use a real MCP client. They can also be run separately:

```bash
cargo test --test mcp_interface
```

The integration contract verifies that all project-dependent tools require
`project_path`, Maven-backed operations require a host-configured trusted
directory tree, and execution occurs through a real STDIO MCP client. Lifecycle
tests ensure EOF and SIGTERM stop the process and stdout contains JSON-RPC only.

Run the complete test pyramid—unit tests, real child-process STDIO MCP
integration and lifecycle tests, YAML scenarios, JSON snapshots, and a Markdown
report—with one command:

```bash
scripts/test-pyramid.sh
```

The human-readable report is written to:

```text
target/mcp-test-report/report.md
```

### Hosted CI

GitHub Actions runs the same verification gate from
`.github/workflows/ci.yml` for pushes to `main` and pull requests targeting
`main`. The workflow installs Rust `1.89.0`, uses the committed `Cargo.lock`,
and runs `scripts/test-pyramid.sh` on Ubuntu. It has read-only repository
permissions, does not persist checkout credentials, and uses no dependency
cache. Its job and status-check context are both named `CI`.

The active default-branch ruleset requires a pull request and a successful,
up-to-date `CI` check before merging. It requires zero approving reviews for
the single-maintainer workflow, blocks deletion and force pushes, and requires
linear history. Use a short-lived branch, open a pull request to `main`, wait
for `CI`, then squash merge. The repository deletes the branch after merging.

### Agent and LLM Evaluation

Above the deterministic Rust test pyramid, a separate Promptfoo evaluation
checks whether an MCP-capable model selects the correct tool and essential
arguments for positive, ambiguous, and no-result natural-language requests, and
then avoids inventing artifacts, versions, JARs, or classes absent from the
fixture.

```bash
export PROMPTFOO_PROVIDER="openai:responses:gpt-5-mini"
export OPENAI_API_KEY="..." # or the standard environment secret for your provider
scripts/run-agent-eval.sh
```

The script pins Promptfoo `0.121.19`, regenerates a marker-protected,
request-scoped project fixture under `target/promptfoo/fixture` from Rust,
configures its trusted project directory and local execution repository, and
passes the generated absolute `project_path` to every MCP request. Promptfoo
starts the local server over STDIO, runs the evaluation with caching disabled,
and exits non-zero on a failed test.
The reviewable HTML is written to `target/promptfoo/report.html`, and the complete
machine-readable JSON to `target/promptfoo/results.json`. The provider/model is
configured only through `PROMPTFOO_PROVIDER`, while authentication comes from
the provider's standard environment variable; never put secrets in YAML. The
report may contain complete model output and configuration, so treat it as an
artifact and do not commit it.

Start MCP Inspector with the fixture repository:

```bash
scripts/run-inspector.sh
```

## License

This project is licensed under the [MIT License](LICENSE).
