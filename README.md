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
- `search_jar_content` – searches manifests, `META-INF/services/*`, and UTF-8
  entries ending in `.conf`, `.config`, `.factories`, `.imports`, `.json`,
  `.list`, `.properties`, `.txt`, `.xml`, `.yaml`, or `.yml`. Each entry is
  capped at `MAX_SOURCE_BYTES`, and the aggregate scan at
  `MAX_SOURCE_BYTES × MAX_RESULTS`; binary and unsupported entries are skipped.
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
- `run_maven` – runs any Maven phases, plugin goals and options from an exact
  argument list, returning execution status and bounded, redacted output.
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

Alternatively, run the release build through Task from the repository root:

```bash
task dev:build-release
```

`task dev:build` creates a debug binary under `target/debug/` instead.

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
Structured lifecycle, test and diagnostic tools run Maven offline by default
(`--offline`); set `MAVEN_EXECUTION_NETWORK=true` to let those tools resolve
dependencies and plugins. The general `run_maven` tool passes the requested
argument list exactly, without adding offline, repository or batch-mode flags.
All tools run at most one Maven process at a time across the entire STDIO server,
apply time and output limits, and redact paths and common credential patterns.
Native execution is not a sandbox: Maven plugins and tests run with the local
user's permissions.

### Arbitrary Maven goals and arguments

Use `run_maven` for any lifecycle phase, plugin goal, profile, property or Maven
option. Each `arguments` item is one argument, in order; do not include `mvn` or
`mvnw`, and do not add shell quotes around items containing spaces. The server
selects the project's Maven Wrapper or configured Maven executable and starts
it from the canonical trusted `project_path`.

```json
{
  "tool": "run_maven",
  "arguments": {
    "project_path": "/work/trusted-projects/team-a/service",
    "arguments": ["--batch-mode", "clean", "install", "dependency:sources", "-Pdev", "-DskipTests", "-Dmessage=hello world"]
  }
}
```

Arguments are passed directly to the executable without shell interpretation.
An empty list (for a POM's default goal), empty items and repeated options are
accepted. NUL characters and non-string items are invalid MCP parameters. Maven
itself validates its options; unsupported options or failed goals return a
structured execution result rather than an MCP argument error.

A `run_maven` call drops the cached project index, so index-backed tools such as
`get_class_source` rebuild it on the next request. Those tools read
`MAVEN_EXECUTION_REPO_PATH` when it is set (otherwise `$HOME/.m2/repository`), so
when it is configured, pass the same directory as `-Dmaven.repo.local=...` in
goals such as `dependency:sources`.

This tool does not inject `--offline` from `MAVEN_EXECUTION_NETWORK` or
`-Dmaven.repo.local` from `MAVEN_EXECUTION_REPO_PATH`. Pass `--offline` or
`-Dmaven.repo.local=/absolute/cache/path` explicitly when needed. Maven's own
environment, settings and `.mvn` configuration still apply. Options such as
`-f` and `--settings` may point outside the selected project, and goals such as
`deploy` may publish artifacts. The trusted-directory check authorizes the
starting project; it does not confine Maven's filesystem or network access.

The response is an object with `status` (`success`, `build_failure`, `timeout`
or `runner_error`), optional `exit_code`, `duration_ms`, `timed_out`, `stdout`,
`stderr`, `stdout_truncated`, `stderr_truncated` and `redaction_count`. It does
not infer lifecycle-specific diagnostics or `policy_notice`. The existing
`run_maven_lifecycle` and `run_maven_test` tools retain their structured results
and automatic flags.

For structured tools, when an offline Maven run fails because a required remote
plugin or artifact is not cached locally, `build.policy_notice` explains that
the result may reflect an intentional security policy or missing MCP server configuration,
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

Archive limit violations fail the requesting tool call with an MCP error; no partial
index is cached or returned. Malformed JARs may still be skipped with a diagnostic.
Indexes are built on demand for explicit project requests, never at server startup.
XML limit violations are errors, never truncated data: project/POM reads fail,
effective-POM and coverage diagnostics report invalid status with an error, and
Surefire failures follow the existing report-error contract.

| Variable | Default | Meaning |
| --- | --- | --- |
| `MAX_RESULTS` | `100` | Maximum number of items in one tool response. |
| `MAX_SOURCE_BYTES` | `1048576` | Maximum size of one source, exact JAR entry, or processed class/resource; returned content is truncated, while oversized inspection input is skipped or reported as an error. |
| `MAX_XML_BYTES` | `16777216` | Range 1–67108864. Maximum size of one POM, effective POM, JaCoCo or Surefire XML file. Larger files fail the request with an explicit error; they are never truncated or partially parsed. |
| `MAX_JAR_ENTRIES` | `200000` | Maximum entries in one archive. Range 1–1000000. Checked before ZIP metadata allocation; exceeding it fails the requesting tool call with an MCP error. |
| `MAX_INDEX_ENTRIES` | `20000000` | Range 1–20000000. Counts every central-directory entry, including directories. Reserved before loading records; exceeding it fails the requesting tool call with an MCP error during project-index construction. |
| `MAX_INDEX_NAME_BYTES` | `2147483648` | Range 1–2147483648. Counts names of files and directories before ZIP metadata allocation; exceeding it fails project-index construction with an MCP error. |
| Entry name bytes | `4096` (fixed) | Maximum bytes in each archive entry name, including directories; checked before ZIP metadata allocation. |
| Central directory bytes | `67108864` (fixed) | Maximum central-directory size per JAR, including extra fields and comments; checked before ZIP metadata allocation. |
| XML nesting / elements | `128` / `200000` (fixed) | Structural preflight before XML deserialization. |
| XML aggregate bytes / files | `67108864` / `4096` (fixed) | One shared budget per module-discovery, JaCoCo-report-set or Surefire-report-set operation. Module traversal also has a depth limit of 128. |
| `MAX_PROJECT_INDEXES` | `4` | Maximum number of canonical project roots retained in the in-process project-index cache. |
| `MAVEN_TRUSTED_PROJECT_DIRECTORIES` | none | Required for Maven-backed project operations. Platform path-list of existing absolute directory trees; a canonical `project_path` must be equal to or nested below one entry. |
| `MAVEN_EXECUTABLE` | none | Absolute Maven binary path; required only when no valid executable Maven Wrapper is available. |
| `MAVEN_EXECUTION_REPO_PATH` | none | Optional existing writable Maven local repository for structured Maven execution and exact artifact inspection. Exact artifact inspection otherwise reads `$HOME/.m2/repository`. `run_maven` requires an explicit `-Dmaven.repo.local` argument to select this repository. |
| `MAVEN_EXECUTION_NETWORK` | `false` | When `true`, structured lifecycle, test and diagnostic tools omit `--offline`. `run_maven` never injects this flag. |
| `MAVEN_TIMEOUT_SECONDS` | `300` | Maximum runtime of one Maven child process. |
| `MAX_MAVEN_OUTPUT_BYTES` | `1048576` | Separate upper limit for stdout and stderr. |
| `MAVEN_MCP_RUNTIME_DIR` | `$XDG_RUNTIME_DIR/maven-mcp` or `$HOME/.cache/maven-mcp/runtime` | User-private directory for live runtime status files consumed by `maven-mcp stats`. |
| `JENV_ROOT` | `$HOME/.jenv` | jenv installation root used only when the server starts with `--jenv`; must be an absolute path when explicitly set. |
| `RUST_LOG` | `maven_mcp=info` | Logging level/filter. |

`get_class_source` returns a result only when the corresponding sources artifact
is present in the local repository. For Maven projects, these artifacts can be
downloaded with commands such as `mvn dependency:sources`. Missing source entries
produce empty results; unreadable source archives or entries are reported as MCP
errors.

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
`kilo`, `intelli-j`, and `auto`. Files and directories can be mixed; directories
are searched recursively without following symlinks. Inputs are read locally;
the analyzer never executes an extracted command or sends logs to a service.

| Source | Recognized records and discovery roots |
| --- | --- |
| VS Code / Insiders / GitHub Copilot | JSON chat sessions with `requests[].response[]` terminal tool invocations, `toolSpecificData.commandLine`, or tool input arguments. Parent session/request metadata and adjacent `workspace.json` provide context. Discovery checks `Code` and `Code - Insiders` under Linux configuration, macOS Application Support, and Windows APPDATA, including workspace storage and empty-window chat sessions. |
| Codex | JSONL `session_meta`, `turn_context`, and `response_item` function calls; direct function calls and assistant tool-call arrays are also recognized. Discovery checks `$CODEX_HOME/sessions`, defaulting to `~/.codex/sessions`. |
| Kilo Code | SQLite tool parts (`type: tool`, `tool`, `state.input`, `callID`), assistant tool-use payloads, and explicit shell-call records in JSON payload columns. Session/message rows supply project/session/agent metadata where available. Discovery checks `$XDG_DATA_HOME/kilo` (default `~/.local/share/kilo`) and the VS Code Kilo global-storage directory. Connections are read-only with SQLite `query_only` enabled; a bare database `command` column is never treated as execution evidence. |
| IntelliJ | Timestamped INFO/DEBUG/TRACE lines whose message starts with `Executing command:`, `Terminal command:`, or `Shell command:`; the usual logger prefix is accepted. Plain `idea.log` and uncompressed rotations such as `idea.log.1` are supported. Discovery checks JetBrains cache/log roots under Linux XDG cache, macOS Library/Logs, and Windows LOCALAPPDATA. |

`auto` uses `.json` for VS Code (the parent path identifies Insiders), `.jsonl`
for Codex, `.db`/`.sqlite`/`.sqlite3`/`.vscdb` for Kilo, and `.log`, `.txt`, or
`.log.N` for IntelliJ. Use `--source` for a differently named file. Discovery is
opt-in with `--discover`; custom/portable locations require explicit paths.
Linux discovery honors `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, and `XDG_CACHE_HOME`.

A normalized event contains `timestamp`, `ide`, `agent`, `project`, `session`,
`tool`, `cwd`, `command`, `category`, and an optional `call_id` and `maven` object.
Unknown metadata is `null`; it is never inferred from the analyzer's current
project. Timestamps are normalized to UTC with millisecond precision. Epoch
seconds/milliseconds and ISO timestamps with offsets are supported; timezone-free
IDE timestamps are interpreted as UTC. IntelliJ metadata is optional; a prefix
such as `[agent=copilot project=/work/demo session=s1 cwd=/work/demo]` supplies it.

Maven classification recognizes `mvn`, `mvnw`, `mvnd`, and their `.cmd` variants
at shell command positions, including assignments, `env`, quoted arguments,
command lists, descriptor/file redirections (such as `2>&1`), and pipelines. The first Maven invocation in each recorded call
supplies lifecycle/plugin goals, `-pl`/`--projects`, `-am`, `-amd`, `-rf`, `-f`,
`-P`/`--activate-profiles`, `-D`/`--define`, and `test`/`it.test` filters. Long,
separate, and attached option values are supported. Repository classification
recognizes `find`, `jar`, `unzip`, `javap`, `grep`, and `rg` with JAR, class, POM,
or resource arguments. A plain source-code grep remains `shell`. Each call has
one category, with Maven taking precedence.

Deduplication spans all input files. Within an IDE/agent/project/session/tool,
a call ID identifies progressive or mirrored records; the longest command and
earliest known timestamp are retained. Without a call ID, only records with an
identical timestamp, command, and working directory are merged. Without a
session, identity is scoped to the input file, except that IntelliJ numeric
rotations (`idea.log`, `idea.log.1`, etc.) in the same directory share an identity.
Different directories, log basenames, and nonnumeric suffixes remain separate.
Untimed calls without IDs remain separate. `--keep-duplicates` disables merging. Output order is deterministic.

Filters are exact, case-insensitive matches for `--ide`, `--agent`, `--project`,
and `--session`, plus `--category` and inclusive `--since`/`--until` time bounds.
Filter values refer to original metadata, before redaction. Dates use
`YYYY-MM-DD`; a date-only `--until` includes the entire day. A timestamp requires
ISO 8601 syntax. Unknown timestamps are excluded when a time filter is active.
Categories are `maven`, `jar-inspection`, `class-inspection`, `pom-inspection`,
`resource-inspection`, and `shell` (serialized category names use underscores).

```bash
cargo run --locked --bin maven-agent-log -- logs/ --source codex \
  --since 2026-08-01 --until 2026-08-31 --agent codex --format csv --output report.csv
cargo run --locked --bin maven-agent-log -- logs/ \
  --category maven --group-by day --format markdown
```

`--group-by day|month|ide|agent|project|session|category` produces counts instead
of individual events. Grouping uses original identities; redaction cannot merge
different projects or sessions. Identical redacted labels receive numeric
suffixes. Formats are `terminal`, `json` (array), `jsonl` (one object per line),
`csv`, and `markdown`. JSON and CSV include Maven details. `--output` writes a
report; it cannot overwrite a selected input log.

Default privacy redaction covers home paths (including foreign Linux/macOS and
Windows usernames), the current username, event workspace/working directories,
`--workspace`, URL credentials, common token formats, and password/token/secret/
credential/authorization/API-key assignments and flags, including `-u`/`--user`
and `-U`/`--proxy-user` credentials. Sensitive shell argument values are consumed
with quoting, escapes, and adjacent quoted fragments preserved as one value;
an incomplete sensitive argument is redacted through the end of the text.
Redaction applies to every exported string, including identifiers and structured Maven fields. Repeat
`--redact-pattern VALUE` for additional sensitive **literal** values. Terminal
control characters and line breaks are escaped. Neither reports nor diagnostics
copy raw logs; diagnostics report only counts of skipped files/records. Use
`--unsafe-no-redact` only when an unredacted local report is intended.

Support limits: formats vary by client version. Unknown record containers,
compressed logs, shell scripts embedded in JavaScript orchestration calls,
heredocs, command substitutions, shell aliases/functions, and PowerShell/cmd.exe
syntax are not interpreted. Incomplete shell quoting retains the recorded call
as `shell`. The analyzer identifies recorded invocations, not proof that a child
process succeeded; explicitly canceled/pending calls are excluded when marked.
Only recognized IntelliJ execution messages are accepted, not arbitrary command
text. Missing IntelliJ metadata stays unknown. Valid JSONL records can be
recovered around malformed lines; truncated JSON documents cannot. Malformed
SQLite JSON payloads are skipped. The CLI reports partial input on stderr and
fails if damaged inputs yield no commands. Unsupported but valid documents may
yield an empty report. Redaction is pattern-based: domain-specific sensitive
values require `--redact-pattern`. No LLM integration is enabled or required;
extraction, classification, counts, and every report format operate offline.

Adapter references: [VS Code terminal invocation data](https://github.com/microsoft/vscode/blob/main/src/vs/workbench/contrib/terminalContrib/chatAgentTools/browser/tools/runInTerminalTool.ts),
[Kilo shell tool documentation](https://github.com/Kilo-Org/kilocode-legacy/blob/main/docs/legacy-ides/automate/tools/execute-command.md).

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

### Dependency Maintenance

Renovate opens grouped update pull requests. The required `CI` job runs
`cargo deny` for advisories, licenses and sources on every pull request and push
to `main`; the `Dependency policy` workflow also scans daily. See
[docs/dependency-maintenance.md](docs/dependency-maintenance.md) for the policy,
exception register and alert handling.

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

## Repository, Copyright and Contributions

The public repository is <https://github.com/tyutyutyu/maven-mcp>; its default
branch is `main`.

Copyright (c) 2026 István Földházi. The project is licensed under the
[MIT License](LICENSE).

Contributions are accepted under the same MIT License: by submitting a pull
request you confirm that you have the right to contribute the code and license
it under those terms. No CLA or DCO sign-off is required, and signed commits or
tags are not required.

## License

This project is licensed under the [MIT License](LICENSE).
