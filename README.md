# Maven MCP

A Docker-ready MCP server written in Rust that indexes a local Maven repository
in memory at startup. Its MCP Streamable HTTP endpoint is
`http://localhost:8080/mcp`.

## Features

The index derives `groupId:artifactId:version[:classifier]` coordinates from the
Maven directory layout and exposes the following MCP tools:

- `index_stats` – statistics for the completed index.
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
  dependency management, and BOM imports from the POM of an exact Maven
  coordinate.
- `diagnose_artifact` – summarizes the POM, binary, classifier, checksum,
  `.lastUpdated`, repository, and corrupt-JAR state of a local artifact without
  exposing absolute paths.
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

Project execution is a separate opt-in capability. The following tools are
available only when `MAVEN_PROJECT_ROOT` is configured and startup project
validation succeeds:

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

The class index handles multi-release JARs and inner classes. Corrupt JARs and
JARs outside the Maven layout are skipped with a warning. Repository inspection
tools are read-only: they do not extract files or modify Maven metadata.

Type hierarchy, classfile reference, and provider relationships are built as
immutable facts at startup. Source JAR contents are not retained in memory:
`search_source` and `get_declaration_source` read them on demand under the
configured byte and result limits. See
[ADR-0003](docs/decisions/0003-immutable-derived-repository-facts.md) for the
detailed decision.

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

## Start with Docker Compose

```bash
export MAVEN_REPO_PATH="$HOME/.m2/repository"
docker compose up --build
```

The host repository is mounted read-only into the container. The complete index
is built before the MCP endpoint opens, so the first startup may take longer for
a large repository.

Use this Streamable HTTP URL in MCP client configuration:

```text
http://localhost:8080/mcp
```

Example with MCP Inspector:

```bash
npx @modelcontextprotocol/inspector http://localhost:8080/mcp
```

## Run Docker Directly

```bash
docker build -t maven-mcp .
docker run --rm \
  -p 127.0.0.1:8080:8080 \
  -v "$HOME/.m2/repository:/maven-repository:ro" \
  --read-only --cap-drop=ALL --security-opt=no-new-privileges \
  maven-mcp
```

## Opt-in Project Execution

The default image contains only the read-only repository indexer. The larger
`execution` target, which includes the JDK and Maven, can be built separately
and started with the additional Compose file:

```bash
export MAVEN_REPO_PATH="$HOME/.m2/repository"
export MAVEN_PROJECT_ROOT="$PWD"
mkdir -p "$HOME/.cache/maven-mcp/repository"
export MAVEN_EXECUTION_REPO_PATH="$HOME/.cache/maven-mcp/repository"
docker compose -f compose.yaml -f compose.execution.yaml up --build
```

The project mount is writable because Maven creates `target/` files; the indexed
repository remains read-only. The execution repository must be a separate,
dedicated, writable directory. Maven-level networking is offline by default
(`--offline`); set `MAVEN_EXECUTION_NETWORK=true` to enable it, but restricting
the container network remains the operator's responsibility. The server does
not accept raw Maven goals or arguments, runs one build at a time, applies time
and output limits, and redacts paths and common credential patterns. This is not
a security sandbox for malicious project code. See
[ADR-0002](docs/decisions/0002-opt-in-project-scoped-maven-execution.md) for the
detailed decision and threat model.

## Environment Variables

| Variable | Default | Meaning |
| --- | --- | --- |
| `MAVEN_REPO_PATH` | required; `/maven-repository` in the image | Root of the Maven repository. |
| `BIND_ADDRESS` | `0.0.0.0:8080` | HTTP listen address. |
| `MAX_RESULTS` | `100` | Maximum number of items in one tool response. |
| `MAX_SOURCE_BYTES` | `1048576` | Maximum size of one source, exact JAR entry, or processed class/resource; returned content is truncated, while oversized inspection input is skipped or reported as an error. |
| `MAVEN_PROJECT_ROOT` | none | Root for opt-in project execution; a valid `pom.xml` is required. |
| `MAVEN_EXECUTABLE` | none | Absolute Maven binary path; required only when no valid executable Maven Wrapper is available. |
| `MAVEN_EXECUTION_REPO_PATH` | none | Optional existing writable Maven local repository that is separate from the indexed repository. |
| `MAVEN_EXECUTION_NETWORK` | `false` | When `true`, `--offline` is not added to Maven commands. |
| `MAVEN_TIMEOUT_SECONDS` | `300` | Maximum runtime of one Maven child process. |
| `MAX_MAVEN_OUTPUT_BYTES` | `1048576` | Separate upper limit for stdout and stderr. |
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
It connects to an already running Maven MCP server and writes measurements to a
versioned JSON file from which a separate report can be generated later.

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
        "arguments": { "query": "org.example.Foo" }
      }
    }
  ]
}
```

`shell.command` is passed to `/bin/sh -c`, so run only specifications you own or
have reviewed. `cwd` is optional. MCP `arguments` can be omitted when the tool
expects no arguments. Case IDs must be unique.

```bash
cargo run --locked --bin maven-benchmark -- \
  --spec benchmark.json \
  --mcp-url http://127.0.0.1:8080/mcp \
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
MAVEN_REPO_PATH="$HOME/.m2/repository" task mcp:serve

task agent-log:analyze INPUT="$HOME/.codex/sessions" -- \
  --format markdown --category maven
task agent-log:discover -- --format terminal

task benchmark:run
task benchmark:run SPEC=my-benchmark.json OUTPUT=target/custom-results.json \
  ITERATIONS=10 WARMUP=2
```

The repository includes a default `benchmark.json` specification for common
SLF4J class, version, and source lookup commands. The shell side inspects
`MAVEN_REPO_PATH`, or `$HOME/.m2/repository` when the variable is unset.

Every benchmark variable can be overridden. Defaults are `SPEC=benchmark.json`,
`OUTPUT=target/benchmark-results.json`,
`MCP_URL=http://127.0.0.1:8080/mcp`, `ITERATIONS=5`, `WARMUP=1`,
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
task dev:conformance -- --scenario server-initialize
task dev:inspector -- --cli --method tools/list
PROMPTFOO_PROVIDER="openai:responses:gpt-5-mini" task dev:agent-eval
```

`dev:verify` runs the complete existing `scripts/test-pyramid.sh` gate. The
conformance, Inspector, and agent-eval tasks require `npx`; HTTP readiness checks
require `curl`. The Taskfile checks these prerequisites before starting.

## Local Development

```bash
cargo test
MAVEN_REPO_PATH="$HOME/.m2/repository" cargo run
```

Unit tests cover Maven coordinate recognition, search, pagination, classifier
and multi-release JAR handling, source filtering, truncation, and malformed
JARs. Integration tests use a real Streamable HTTP MCP server bound to a random
local port and a real MCP client. They can also be run separately:

```bash
cargo test --test mcp_interface
```

The integration contract separately verifies that project tools are hidden by
default and executable through a real HTTP MCP client in opt-in mode.

Run the complete test pyramid—unit tests, MCP integration, YAML scenarios, JSON
snapshots, Markdown report, and official MCP conformance—with one command:

```bash
scripts/test-pyramid.sh
```

The human-readable report is written to:

```text
target/mcp-test-report/report.md
```

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

The script pins Promptfoo `0.121.19`, regenerates the
`target/promptfoo/maven-repository` fixture from Rust, starts a local MCP server,
runs the evaluation with caching disabled, and exits non-zero on a failed test.
The reviewable HTML is written to `target/promptfoo/report.html`, and the complete
machine-readable JSON to `target/promptfoo/results.json`. The provider/model is
configured only through `PROMPTFOO_PROVIDER`, while authentication comes from
the provider's standard environment variable; never put secrets in YAML. The
report may contain complete model output and configuration, so treat it as an
artifact and do not commit it. See [docs/testing.md](docs/testing.md) for the
detailed review workflow.

Start MCP Inspector with the fixture repository:

```bash
scripts/run-inspector.sh
```

The scenario format, snapshot review, and conformance process are documented in
[docs/testing.md](docs/testing.md). The rationale is recorded in
[ADR-0001](docs/decisions/0001-scenario-snapshot-test-pyramid.md).

The simple liveness endpoint is `GET /healthz`; it is also used by the Docker
health check.
