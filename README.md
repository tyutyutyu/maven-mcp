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
  depth, and hierarchy path. A prebuilt parent-to-child
  index serves the walk, so deep or wide hierarchies need no repeated fact
  scans; a class reached through two parents is reported once per parent, and
  cycles end.
- `search_class_references` – searches inbound or outbound class, field, and
  method references from the classfile constant pool; a result is a reference,
  not proof of a runtime call site.
- `compare_artifact_api` – compares the public/protected API of two exact local
  versions of the same artifact at class, member, superclass, and interface
  levels. Both unclassified binary JARs are read straight from the local Maven
  repository, like `get_artifact_pom`, so neither version has to be on the
  project classpath and no Maven process or project index is needed; the
  project search is not extended by the compared versions. The request still
  needs a trusted `project_path`. Resolution is contained in the repository
  (traversal and outward symlinks are rejected), makes no network access or
  repository change, and applies the archive limits, `MAX_SOURCE_BYTES`, and
  the multi-release class selection of the other classfile tools. A version
  without a local binary JAR is an invalid-argument error; the same version on
  both sides yields an empty diff. Reactor outputs that were never installed
  into the repository are not compared.
- `search_jar_content` – searches manifests, `META-INF/services/*`, and UTF-8
  entries ending in `.conf`, `.config`, `.factories`, `.imports`, `.json`,
  `.list`, `.properties`, `.txt`, `.xml`, `.yaml`, or `.yml`. Each entry is
  capped at `MAX_SOURCE_BYTES`, and the aggregate scan at
  `MAX_SOURCE_BYTES × MAX_RESULTS`; binary and unsupported entries are skipped.
- `search_source` – searches Java/Kotlin sources from local source artifacts by
  substring or regular expression, with line numbers and bounded context. Each
  source is read up to `MAX_SOURCE_BYTES` (the aggregate scan up to
  `MAX_SOURCE_BYTES × MAX_RESULTS`); `incomplete` is `true` whenever any part of
  a source was not searched, even with no matches (see below).
- `get_declaration_source` – returns a focused source excerpt for a class, field,
  or method; overloads are selected by their JVM descriptor, and a declaration
  whose source cannot be proven is reported in `unresolved` (see
  [Declaration source slices](#declaration-source-slices)).
- `search_providers` – returns structured Java ServiceLoader, JPMS
  `uses`/`provides`, `spring.factories`, and Spring `.imports` provider
  declarations.
- `list_artifact_versions` – lists locally available versions of an artifact,
  with an optional `groupId` filter.

### Multi-release JARs

Every classfile-based tool (`describe_class`, `search_class_members`,
`search_type_hierarchy`, `search_class_references`, `compare_artifact_api`,
`search_providers`, and `list_jar_classes`) reads the same effective class
variant, so one class never shows different superclasses or members in different
tools. The server does not target a particular Java runtime; the answer applies
to the **highest** `META-INF/versions/<N>` variant (`N >= 9`), not to the Java
version of your project:

- Only a JAR whose `META-INF/MANIFEST.MF` declares `Multi-Release: true` has its
  `META-INF/versions/<N>/` entries considered. The highest `N` wins; the base
  entry is the fallback, and a class present only in a versioned directory is
  indexed with that variant.
- Without the manifest flag, all versioned entries are ignored (as the JVM does):
  the base entry is the only variant, and a class that exists only under
  `META-INF/versions/` is not indexed.
- Version directories that are not canonical decimal numbers (for example `x`,
  `+11`, or `011`) or are below 9 are never selected.
- `search_jar_entries` still lists the raw entries, including every versioned one.

### Declaration source slices

`get_declaration_source` reads the classfile of the class to find the requested
declaration (class, field, or method with its JVM descriptor) and then locates it
in the matching `.java` or `.kt` entry of the `-sources.jar`. The source is split
into code tokens first, so comments, string and character literals, Java text
blocks, and Kotlin raw strings and `${...}` templates never move a declaration
boundary, and brackets are matched across lines. A slice consists of whole lines:
it starts at the first annotation or modifier of the declaration (Javadoc is not
included) and ends at its closing `}` or `;`, or at the end of a Kotlin
declaration without a body. `start_line` and `end_line` are 1-based lines of the
source entry.

Supported cases:

- Java and Kotlin top-level and nested types (`Outer$Inner` binary names, also
  `class`, `interface`, `enum`, `record`, `@interface`, Kotlin `object`,
  `companion object`, `data`/`enum`/`inner` classes); members of a nested type are
  searched only in that type, and anonymous or local classes are not supported.
- Fields: Java declarators (also several in one declaration), enum constants,
  record components, Kotlin `val`/`var` properties (with accessors on the
  following lines), primary-constructor properties, and companion-object
  constants.
- Methods and constructors, also with multi-line signatures, annotations,
  generics, `throws`, Kotlin expression bodies (also continued on later lines
  after a binary operator, an `if`/`while`/`for` condition, or `else`; a
  line-ending `>` that closes type arguments ends the declaration), extension
  receivers, `suspend`, `vararg`, nullable and function types, and Kotlin
  primary and secondary constructors. An overload is selected by comparing the descriptor with the
  erased source parameter types by simple name and array dimension (type
  variables erase to their first bound), independently of the source order. The
  enclosing-instance parameter of inner-class constructors and the name/ordinal
  parameters of enum constructors are accepted.

Every classfile declaration that is not returned as a slice is listed in
`unresolved` with a `reason`: `not_found` (no such declaration in the source
type), `ambiguous` (several source declarations match equally, for example
parameter types with the same simple name from different packages), or
`unproven` (same-named declarations exist, but none provably matches the
descriptor, for example a compiler-generated bridge or synthetic method, which
is recognized by its classfile access flags, or an `@JvmOverloads` overload). `candidate_lines` lists the start lines of the declarations involved.
No source fragment is guessed. Without `member_name` the class itself is
returned; with `member_name` but without `descriptor`, several classfile
overloads are still reported in `ambiguous_candidates`.

Each source is read up to `MAX_SOURCE_BYTES`. When it was cut, `truncated` is
`true` on both results and unresolved entries, a declaration that runs past the
limit ends at the last code token that was read, and an overload whose parameter
list was cut off is never returned as a match: it is reported as `ambiguous`
together with a complete match, and as `unproven` when no complete match exists. An artifact without a `-sources.jar` or
without the source entry produces neither a result nor an unresolved entry.

Project inspection and execution use the same request `project_path`:

- `inspect_maven_project` – models the root project, Maven Wrapper, and recursive
  reactor modules.
- `refresh_maven_project` – explicitly refreshes one project: drops its cached
  model and index, abandons a running index build, re-reads the POMs, wrapper and
  Maven configuration, and rebuilds the index (see [Explicit refresh](#explicit-refresh)).
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
default. The coordinate must be exactly `groupId:artifactId:version`; empty or
dot-only segments (for example a leading or trailing `.` in `groupId`), `..`, `/`,
and `\` are rejected as invalid arguments. Every existing path component is
resolved and must stay inside the canonical repository, so an artifact, POM, or
parent-directory symlink pointing outside it (even a dangling one) is rejected
without reading or returning external content; absent artifacts still report
`found: false`. Other repository-search tools ask Maven for
the effective test classpath, build a lazy index only from those local JARs and
their sibling `-sources.jar` files, and cache that index by canonical
`project_path`. For a multi-module reactor, classpath sections from every module
are aggregated; empty parent or aggregator POM sections do not hide dependencies
reported by later modules. Reactor outputs on that classpath are indexed with the
repository artifacts: packaged JARs (conventionally named or with a custom
`finalName`) and class directories such as `target/classes` and
`target/test-classes` below the `target` directory of a discovered reactor module.
They are listed under the module's coordinate, resolved from its POM, its parent
POMs inside the project and `.mvn/maven.config` `-D` properties, so CI-friendly
versions such as `${revision}` work; `test-classes` and `*-tests.jar` use the
`tests` classifier, and a class directory is addressed by its project-relative
path (for example `core/target/classes`). Artifacts present elsewhere in the same
local Maven repository are never searched. If Maven selects a classpath entry that
cannot be indexed — a missing file, a JAR outside both the local repository and the
reactor build directories (such as a `system`-scope JAR), or a reactor output
whose module coordinate cannot be resolved — the request fails with an error that
counts the entries per reason and names their files; no index is cached, so the
next request retries. Repeated classfile fact strings are stored once per JAR and referenced
by compact numeric identifiers, so large dependency sets do not retain a
separate allocation for every constant-pool reference. The effective classpath
must be fully resolvable; for example, a reactor dependency on an unbuilt sibling
SNAPSHOT can prevent the index from being created. In that case the MCP error
includes a bounded, redacted summary of Maven's error output, regardless of
whether Maven wrote it to stdout or stderr. If Maven's classpath output is
truncated or malformed (for example cut off by `MAX_MAVEN_OUTPUT_BYTES`), the
request fails with an explicit error instead of caching an index that would be
incomplete; nothing is cached, so the next request retries.

### Project index cache

The runner (reactor model) and the index of one project share a validity check
on every request. A cached index is reused while the root and module POMs, the
Maven executable and Maven Wrapper files (`mvnw`,
`.mvn/wrapper/maven-wrapper.properties`), `.mvn/maven.config`, `.mvn/jvm.config`,
`.mvn/extensions.xml` and every classpath JAR (and its `-sources.jar`) keep their
size and modification time; a classpath class directory is checked file by file,
so editing, adding or removing a nested `.class` file also counts. Any difference re-reads the reactor model — for
example a module added to the POM shows up in `inspect_maven_project` — and drops
the index, which is rebuilt on demand. `run_maven` and `run_maven_lifecycle`
always re-read the model and drop the index afterwards, because Maven may have
changed POMs, dependencies or reactor outputs. Parent POMs outside the project and
the contents of the local repository beyond the classpath JARs are not watched by
the in-process cache; use `refresh_maven_project` after changing them. The
[persistent index cache](#persistent-index-cache) adds its own, stricter checks
before a new process reuses a stored classpath.

### Persistent index cache

Every new STDIO process would otherwise pay the full cold cost of its first
project search: resolving the classpath through Maven and reading every JAR. The
server therefore keeps a versioned, user-private **on-disk index cache** of derived
data that all its processes share. It is not a startup scan: nothing is read
before the first index-dependent request of a project, and only that project's
classpath entries are looked up. The local Maven repository stays the
authoritative source and is only read, never written.

The cache holds two kinds of entries, each keyed by the writing `maven-mcp`
version:

- one **record** per indexed JAR, sources JAR or reactor class directory, keyed by
  its canonical path. A record is reused only while the file's size and
  nanosecond modification time (for a class directory: of the directory and
  every entry below it), its Maven coordinate and the result-shaping limits
  (`MAX_SOURCE_BYTES`, `MAX_JAR_ENTRIES`, `MAX_XML_BYTES`) are unchanged. Reused
  records are still charged against `MAX_INDEX_ENTRIES` and
  `MAX_INDEX_NAME_BYTES`, so a limit fails the same way as for a fresh read.
- one **classpath** per project context, keyed by the canonical project root,
  the classpath scope, `MAVEN_EXECUTABLE`, `MAVEN_EXECUTION_REPO_PATH`,
  `MAVEN_EXECUTION_NETWORK`, the Java selection (inherited or `--jenv` with its
  root) and a digest of the whole inherited environment (only the digest is
  stored), because POMs, settings, `mavenrc` files and `JAVA_TOOL_OPTIONS` may
  read any variable. Only the server's own settings (`MAX_*` limits,
  `MAVEN_TIMEOUT_SECONDS`, `MAVEN_MCP_*`, `RUST_LOG`) are left out; they are
  removed from every Maven child. It is reused instead of running Maven only
  while the project fingerprint (see [Project index cache](#project-index-cache))
  is identical, every resolution input is unchanged, and every classpath entry
  still exists. The resolution inputs are `~/.m2/settings.xml`,
  `~/.m2/toolchains.xml`, `~/.m2/extensions.xml`, `~/.mavenrc`, `/etc/mavenrc`,
  the Maven installation's `conf` and `lib/ext` directories (of `MAVEN_EXECUTABLE`,
  or of every Maven Wrapper distribution below
  `${MAVEN_USER_HOME:-~/.m2}/wrapper/dists`), the jenv version files
  (`$JENV_ROOT/version` and `.java-version` in the project and its ancestors),
  the POM beside every repository artifact on the classpath, and — followed
  transitively from the reactor POMs and those artifact POMs — every parent POM
  (a reactor POM's `relativePath` and the repository copy) and every BOM imported
  through `dependencyManagement`, including the imports of every profile. A
  parent or import whose version Maven decides (a property, a range, a SNAPSHOT)
  stamps every version of that artifact in the repository. When a parent or
  import cannot be identified (a groupId or artifactId that needs other
  properties, an unparsable POM, no known repository), the classpath is not
  stored and every process resolves it through Maven.

So a new or edited POM, parent or imported BOM, a deleted classpath artifact, a
changed setting or environment variable resolves the classpath through Maven
again, and a modified JAR or reactor output re-reads only that entry; every other
valid record is reused. The resolution inputs are stamped as Maven returns the
classpath and must all have been at least two seconds old when Maven started, and
they must still carry those stamps when the classpath is stored; otherwise the
classpath is used but not stored, because Maven may have read an older version of
a file that changed during the run or the index build. Likewise, files modified
less than two seconds before a build are indexed but not persisted, because a
coarse filesystem clock could hide a second change with the same stamp.
`run_maven` and `run_maven_lifecycle` also forget the project's stored classpath
(records stay valid by their stamps), and `refresh_maven_project` additionally
re-reads every archive of the project and replaces its stored records.

Location and lifecycle:

- Directory: `MAVEN_MCP_CACHE_DIR` when set (must be absolute), otherwise
  `$XDG_CACHE_HOME/maven-mcp/index` or `~/.cache/maven-mcp/index`. The directory
  is made `0700` and the SQLite database `index-v1.sqlite3` is `0600`; it contains
  local paths, class and entry names, never source content. The file name carries
  the cache format, so an older or newer binary with another format never reads it.
- Disable with `MAVEN_MCP_INDEX_CACHE=false`; every process then starts cold, as
  before. With no known location (neither variable nor `HOME`) the cache is off.
- Size: `MAX_INDEX_CACHE_BYTES` (default 1 GiB) bounds the stored entry bytes;
  after each write the least recently used entries are evicted. A budget smaller
  than an entry keeps nothing and only costs the cache's benefit.
- Deletion: the cache is derived data. Delete the directory at any time, also
  while servers run; the next build recreates it.
- Integrity: each build's entries are written in one SQLite transaction, so a
  crashed or killed process, or two processes building the same project at once,
  never expose a partial entry. Every payload is a zlib stream whose checksum is
  verified on read. A missing, unreadable, corrupt or incompatible database is
  deleted and rebuilt automatically, and a damaged or mismatching entry is a miss
  that is rebuilt; none of these is ever served as a result. Rebuilding a damaged
  database is serialized between processes by an exclusive lock on the private
  `index-v1.sqlite3.lock` file, which counts the rebuilds: a process deletes the
  database only when no other process rebuilt it since its own diagnosis, and
  creates the new schema before releasing the lock, so concurrent recoveries never
  delete each other's new database or its journal. A failed cache write only loses
  the cached copy: the search still answers from the index it built.

`maven-mcp stats` reports per process how many builds reused a stored classpath
(`persisted_classpath_hits`) and how many records were reused or read from their
archives (`persisted_record_hits`, `persisted_record_misses`).

### Explicit refresh

`refresh_maven_project` (input: `project_path` only) is the manual counterpart of
the automatic invalidation above, for changes the stamps cannot see (a parent POM
outside the project, `settings.xml`, a dependency re-resolved by another tool).
It requires the same trusted canonical `project_path` as every other project
operation and then, for that project only:

1. drops the cached model and index and abandons a running index build, so a build
   that started earlier can never publish (the project gets a new generation);
2. re-discovers the runner (POMs, reactor modules, Maven Wrapper or `mvn`
   selection, `.mvn` configuration);
3. rebuilds the index from every archive, without reusing persisted records,
   and replaces the project's entries in the
   [persistent index cache](#persistent-index-cache). This **starts Maven**
   (`dependency:build-classpath`) through the shared execution permit, so it waits behind a running Maven
   process like any cold request and uses the same Java selection, timeout and
   output limits. It never touches any other project's cached state.

The refresh itself never deletes, purges or rewrites local Maven repository
content. Its classpath goal follows the same network policy as a cold request:
by default it runs `--offline` and only reads artifacts already in the
repository; with `MAVEN_EXECUTION_NETWORK=true` Maven may download missing
artifacts into the configured repository (`MAVEN_EXECUTION_REPO_PATH` or
`$HOME/.m2/repository`), exactly as any cold request would.

The response is `{ "status": "refreshed", "invalidated": true,
"model_reloaded": true, "index_rebuilt": true, "index": { ...index_stats } }` and
is returned only after the new index exists. Failures come in two kinds:

- A `project_path` that fails validation (relative, missing, not a Maven project,
  or outside `MAVEN_TRUSTED_PROJECT_DIRECTORIES`) is a plain MCP error raised
  before anything is invalidated; no cache state changes.
- Any failure after the invalidation (a Maven or classpath failure, an
  unindexable classpath entry, a project that keeps changing) is an MCP error
  whose message starts with "the project cache was invalidated but the refresh
  did not complete"; the project then stays invalidated and the next request
  retries, so an invalidation is never reported as a refresh.

Concurrent cold requests for one project share a single Maven classpath run and
index build and receive the same result or the same error; after a failure the
next request retries. If the project is invalidated, or its files change, while a
build is running, that build is discarded and the request is served from a fresh
one (up to four attempts, then an error asks the client to retry).

The cache is bounded three ways: `MAX_PROJECT_INDEXES` indexes, four times that
many runners, and `MAX_PROJECT_INDEX_BYTES` of estimated memory for runners,
indexes and running builds together. The least recently used idle index (then
runner) is evicted first; the project being served is never evicted for its own
request. An evicted index that a request still holds stays counted until that
request finishes; the same holds for an evicted or replaced runner. A runner is
charged a fixed share plus the estimated size of its last focused-test result. A
cancelled request does not keep its project's build marker or runner alive past
these limits. A running build is charged the size of its classpath JARs up
front and fails with an error naming `MAX_PROJECT_INDEX_BYTES` when that cannot
fit; a finished index larger than the whole budget serves its requester but is
not retained. Evicting a runner also forgets that project's last focused-test
failures. The sizes are estimates of retained heap, not allocator measurements;
`maven-mcp stats` reports them next to the process RSS.

Index builds, the validation of a cached index against its classpath artifacts,
and every tool that reads indexes, JARs or other local files run as bounded heavy
jobs on blocking threads, so a long build or search never occupies the async
runtime that answers `initialize`, `tools/list` and the light project inspection
tools. At most `MAX_HEAVY_JOBS` jobs run and `MAX_QUEUED_HEAVY_JOBS`
more wait for a slot; a job that finds both full fails at once with an
"overloaded" MCP error naming both variables instead of growing a queue, and the
request can be retried. Cancelling a request, EOF, SIGINT or SIGTERM cancels the
job: long loops of the index build and of the source/content searches stop at
their next checkpoint, a cancelled build is never published to the cache, and a
job that cannot be interrupted (for example one blocked in a system call) keeps
its slot until it returns, but never delays shutdown by more than a couple of
seconds. A retry of a cancelled index build waits until that build's blocking
work has stopped, so one project generation never has two builds running. Maven
runs are unchanged: one Maven process at a time across the server.

`search_jar_content` examines the manifest, `META-INF/services/*` descriptors,
and UTF-8 entries with the extensions `conf`, `config`, `factories`, `imports`,
`json`, `list`, `properties`, `txt`, `xml`, `yaml`, and `yml`. Each entry is
limited by `MAX_SOURCE_BYTES`, while the aggregate read budget is
`MAX_SOURCE_BYTES × MAX_RESULTS`. The `incomplete` field reports when size or
result limits prevented a complete search.

`search_source` reads each source up to `MAX_SOURCE_BYTES` (cut at a UTF-8
character boundary), within the aggregate `MAX_SOURCE_BYTES × MAX_RESULTS` read
budget; `scanned_bytes` counts the bytes actually searched. Its `incomplete`
field describes the search, not response paging: it is `true` when any source
was only partly read (the per-match `truncated` field marks the matches in such
a source), when the aggregate budget stopped the scan, or when the result
`limit` was reached; it is `false` only when every source was read completely
and the result limit was not reached. An empty `results` list with
`incomplete: true` therefore does not prove that the query is absent. A source
read error fails the request instead of being reported as a missing match.

`search_providers` interprets `META-INF/services/*`, JPMS `module-info.class`
`uses`/`provides`, `META-INF/spring.factories`, and
`META-INF/spring/*.imports`. Spring `.handlers` and `.schemas` files are not
provider descriptors and therefore are not included in this structured view.

## Supported Platforms

The first release supports Linux only, as native binaries (a container image is
not a substitute platform). Every target below is built and tested natively in
the required `CI` check:

| Target | CI runner | Verified in CI |
| --- | --- | --- |
| `x86_64-unknown-linux-gnu` | `ubuntu-26.04` | full gate (format, Clippy, every test target, 80% line coverage), release build, binary check |
| `aarch64-unknown-linux-gnu` | `ubuntu-26.04-arm` | every test target, release build, binary check |

Runtime requirements on both targets: a Linux kernel with a mounted procfs at
`/proc`, and glibc 2.34 or newer (for example Ubuntu 22.04, Debian 12, or RHEL
9). That is the newest glibc symbol version the release binaries require; newer
functions such as `pidfd_spawnp` are weak references used only when present.
Both targets link with the `rust-lld` bundled in the pinned toolchain (on
`aarch64-unknown-linux-gnu` through `scripts/cc-rust-lld.sh`, configured in
`.cargo/config.toml`), because GNU ld would turn those weak references into a
hard glibc 2.39 requirement.
The binaries link only `libc`, `libm`, `libgcc_s`, and the dynamic loader;
SQLite is compiled in. `scripts/check-linux-binary.sh` fails CI when a release
binary needs a newer glibc symbol version or links another shared library. The
test suite itself runs on the Ubuntu 26.04 CI runners only. Maven runs in its own process group, which is killed on timeout,
cancellation, EOF, SIGINT, and SIGTERM; `maven-mcp stats` decides whether an
instance is live from `/proc/<pid>` and reads RSS from `/proc/self/status`.
Without a mounted procfs, `maven-mcp stats` exits with an error instead of
reporting any instance as live or stale.

Not supported: macOS, Windows, the BSDs, and other operating systems. The
process-group cleanup and procfs checks above are Linux-specific, so building
for any non-Linux target stops with an explicit `compile_error!` that names this
section rather than producing a binary with untested behavior. Linux targets
outside the table (for example musl/Alpine, 32-bit, or other architectures) are
untested and have no CI coverage or release artifacts. A new platform needs a
separate product decision plus its own CI job.

Toolchains:

- Rust: development and CI use the exact toolchain pinned in
  `rust-toolchain.toml` (rustup installs it, with `clippy`, `rustfmt`, and
  `llvm-tools-preview`, on first use). The minimum supported Rust version is
  `rust-version` in `Cargo.toml` (1.89). A separate `MSRV` CI job checks every
  target with that version and the unchanged `Cargo.lock`.
- Node.js (only for MCP Inspector and the Promptfoo agent eval): the version
  pinned in `.node-version`, with the npm version pinned by `packageManager` in
  `tools/package.json`. The scripts reject a different Node.js major version
  and any npm version other than the exact `packageManager` pin; the `Node tools`
  CI job installs exactly that npm version and checks it. The tools are installed only by `npm ci` from
  `tools/package-lock.json`, which verifies every package against its recorded
  integrity hash. The scripts never call `npx`.
- Renovate proposes updates to the Rust toolchain pin, the Node.js version, the
  locked npm tools, Cargo crates, CI Cargo tools, and GitHub Actions on its weekly
  schedule (see [Dependency Maintenance](#dependency-maintenance)). Every update
  must pass `CI`.

## Releases

`maven-mcp` is in **beta**. Releases are native Linux builds for the
[supported targets](#supported-platforms) and are published to two channels:

| Channel | What it contains | Install |
| --- | --- | --- |
| [GitHub Releases](https://github.com/tyutyutyu/maven-mcp/releases) | one archive per target, `maven-mcp-<version>-x86_64-unknown-linux-gnu.tar.gz` and `maven-mcp-<version>-aarch64-unknown-linux-gnu.tar.gz`, each with a `.sha256` checksum file | download, verify, extract (below) |
| [crates.io](https://crates.io/crates/maven-mcp) | the source package | `cargo install maven-mcp --version <version> --locked` |

Every beta release is marked as a GitHub pre-release. A release archive holds
the commit-identified `maven-mcp` server binary plus `LICENSE`, `README.md`,
and `CHANGELOG.md`, and nothing else. The binary is the native STDIO MCP
server; a release is not an OCI image, and no container image is published.
There is no in-app self-update: upgrades are explicit user actions.

The mandatory integrity artifact is the SHA-256 checksum of each archive.
Releases are not signed, and no SBOM or build-provenance attestation is
published, because no channel or user requirement exists for them yet. The
archives are reproducible instead: rebuilding the release tag with the pinned
toolchain (`task release:archive` on the tag checkout) produces the same
SHA-256 (see [Creating a release](#creating-a-release)).

### Versioning and compatibility

- The `version` in `Cargo.toml` is the only version source. `Cargo.lock`, the
  `CHANGELOG.md` section, the `v<version>` Git tag, the archive names,
  `maven-mcp --version`, and the MCP `initialize` `serverInfo` (name
  `maven-mcp`, version `<version>`) all derive from it. Release archives and
  [local installations](#versioned-local-installation) embed the source commit,
  so their `serverInfo.version` is `<version>+<12-hex commit>` as SemVer build
  metadata; `cargo install` builds report the plain `<version>` and
  `(commit unknown)`. `tests/mcp_interface.rs` and `scripts/release.sh check`
  fail when these sources disagree.
- Versions follow [Semantic Versioning 2.0.0](https://semver.org/). The beta
  is the `0.y.z` series, starting with `0.1.0`. While the major version is `0`,
  a minor bump (`0.1.x` to `0.2.0`) may contain breaking changes, and a patch
  bump (`0.1.0` to `0.1.1`) contains only backward-compatible fixes and
  additions. A pre-release suffix such as `0.2.0-rc.1` marks a candidate.
  `1.0.0` ends the beta; from then on, only a major bump may break the public
  interface.
- The public interface is: MCP tool names, their input schemas, `outputSchema`
  and structured response shapes, and error semantics; the `maven-mcp`,
  `maven-agent-log`, and `maven-benchmark` commands and flags, including the
  `stats --json` fields; the [environment variables](#environment-variables);
  and the supported platforms. Adding an optional input or an additive JSON
  output field is backward compatible. Not public: the Rust library API of the
  `maven_mcp` crate, human-readable text and log output, runtime status files,
  and the on-disk index cache format, which is derived data and is rebuilt when
  it changes.

### Changelog and release notes

[`CHANGELOG.md`](CHANGELOG.md) follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Every pull request
with a user-facing change adds an entry under `## [Unreleased]`, in the `Added`,
`Changed`, `Deprecated`, `Removed`, `Fixed`, or `Security` group. A user-facing
change is anything in the public interface above, a default or limit, the
platform support, the packaging, or documented behavior; a breaking change
starts with **Breaking:**. Changes with no user-visible effect (internal
refactors, tests, CI) need no entry. The pull request template asks for this
confirmation.

A release moves the `Unreleased` entries into a `## [<version>] - YYYY-MM-DD`
section and leaves an empty `Unreleased` heading. That section, printed by
`scripts/release.sh notes <version>`, is the GitHub Release body. The release
check refuses a tag without a dated, non-empty section for its version.

### Upgrade and rollback

Upgrades and rollbacks only replace the binary that the MCP host starts. A host
session that is already running keeps its binary, so restart the host session
(or its MCP server) afterwards. Configuration is environment variables only,
and no data needs migration: the [persistent index cache](#persistent-index-cache)
keeps the entries of each tool version apart and rebuilds on a format change,
so both upgrading and downgrading reuse or rebuild it safely.

From GitHub Releases, keep each version in its own directory and point the MCP
host at a `current` symlink:

```bash
version=0.1.0                      # the release to install
target=x86_64-unknown-linux-gnu    # or aarch64-unknown-linux-gnu
archive="maven-mcp-${version}-${target}.tar.gz"
url="https://github.com/tyutyutyu/maven-mcp/releases/download/v${version}"
curl -fLO "${url}/${archive}" -fLO "${url}/${archive}.sha256"
sha256sum --check "${archive}.sha256"

releases="${XDG_DATA_HOME:-$HOME/.local/share}/maven-mcp/releases"
mkdir -p "${releases}"
tar -xzf "${archive}" -C "${releases}"
"${releases}/maven-mcp-${version}-${target}/maven-mcp" --version
ln -sfn "maven-mcp-${version}-${target}" "${releases}/current"
```

Configure the host command as `<releases>/current/maven-mcp`. To upgrade, run
the same steps with the new version; the previous directory stays in place. To
roll back, point `current` at the previous directory again, for example
`ln -sfn maven-mcp-0.1.0-x86_64-unknown-linux-gnu "${releases}/current"`.
Verify either step with `"${releases}/current/maven-mcp" --version`, which must
print the selected version and its commit, and with the `serverInfo.version`
the host reports after the restart. From a source checkout,
`scripts/release.sh verify-archive <archive>` (`task release:verify-archive
ARCHIVE=<archive>`) additionally checks the exact archive contents and that the
extracted binary reports the archive's version.

From crates.io, `cargo install` builds the selected version from source with
the locked dependencies into `~/.cargo/bin`:

```bash
cargo install maven-mcp --version 0.1.0 --locked              # install or upgrade
cargo install maven-mcp --version <previous> --locked --force # roll back
maven-mcp --version
```

Add `--bin maven-mcp` to install only the server; otherwise the package's other
binaries (`maven-agent-log`, `maven-benchmark`, and the `maven-eval-fixture`
test helper) are installed too. The crates.io build needs Rust 1.89 (the
minimum supported Rust version) or newer on a
[supported platform](#supported-platforms).

### Creating a release

Maintainers create a release in three steps; publication needs the approval of
the protected `release` GitHub environment, which holds the
`CARGO_REGISTRY_TOKEN` crates.io API token.

1. On a branch, set the new `version` in `Cargo.toml`, run
   `cargo update --workspace --offline` so `Cargo.lock` records it, move the
   `Unreleased` changelog entries into the dated version section, and check the
   result with `task release:check TAG=v<version>`. Merge the pull request after
   `CI` passes.
2. Tag the merge commit on `main` and push the tag:
   `git tag -a v<version> -m "maven-mcp <version>"` and
   `git push origin v<version>`.
3. The `Release` workflow (`.github/workflows/release.yml`) then runs the
   complete `CI` on the tag, requires the tag to be on `main`, runs
   `scripts/release.sh check --tag` and `cargo publish --dry-run --locked`, and
   creates a draft GitHub pre-release with the changelog notes. On a native
   runner per target, `scripts/release.sh archive` builds the clean tag with the
   commit embedded, remapped source paths, and `SOURCE_DATE_EPOCH` set to the
   commit time, checks the binary with `scripts/check-linux-binary.sh`, writes
   a deterministic archive (sorted entries, fixed owner, mode, and mtime, and
   `gzip -n`) and its `.sha256`, verifies them, and uploads both. After the
   `release` environment is approved, the workflow runs `cargo publish --locked`
   and publishes the GitHub Release.

`scripts/release.sh check` also checks the crates.io package manifest
(`cargo package --list`), and `verify-archive` checks every archive entry: a
release must not contain private workspace data (`.backlog/`, `.brv/`, private
`docs/` such as `docs/decisions/` and `docs/testing.md`, local agent
configuration), build, test log, or evaluation output (`target/`, Promptfoo
results and reports, MCP test reports, coverage, benchmark results, `*.log`),
or credential files (`.env*`, `.npmrc`, `.netrc`, `credentials*`,
`settings-security.xml`, private keys). The package itself is an allowlist:
`Cargo.toml` `include` admits only `src/**/*.rs`, `Cargo.lock`, `LICENSE`,
`README.md`, and `CHANGELOG.md`.

If a job fails before the crates.io publication, delete the draft release and
the tag, fix the problem on `main`, and tag again. A published crates.io version
is immutable: fix a broken release with a new patch version, and `cargo yank`
the broken one if needed. Creating a local candidate installation never
publishes anything.

## Native STDIO startup

```bash
cargo build --release --locked --bin maven-mcp
```

Alternatively, run the release build through Task from the repository root:

```bash
task dev:build-release
```

`task dev:build` creates a debug binary under `target/debug/` instead.

For day-to-day use, install a verified, versioned copy instead of pointing an
MCP host at the source worktree (see
[Versioned local installation](#versioned-local-installation)). Configure the
MCP host to run the installed binary and pass request `project_path` values in
tool calls. The host owns process startup, shutdown, and STDIO; there is no port,
URL, daemon, health endpoint, Docker image, or manual server lifecycle, and there
are no start or stop tasks. Startup is lightweight because no repository index is
built before initialization finishes.

Add `--jenv` to the server command when Maven children must use each project's
jenv-selected Java instead of relying on the MCP host's inherited `JAVA_HOME`
and `PATH`.

Runtime diagnostics are available without starting an MCP transport:

```bash
maven-mcp --version
maven-mcp stats
maven-mcp stats --json
```

`--version` prints the package version and the source commit embedded at build
time, for example `maven-mcp 0.1.0 (commit <40-hex commit>)`, or
`(commit unknown)` for a build made without the installation flow. The stats
command reads user-private runtime status files for live host-managed STDIO
instances and reports PIDs, the package version, embedded commit and resolved
executable path of each instance, process RSS where the platform exposes it,
active project indexes, index sizes, estimated retained bytes, cache
hit/miss/eviction/invalidation counters, and the persistent index cache counters
(`persisted_classpath_hits`, `persisted_record_hits`, `persisted_record_misses`;
additive JSON fields). If no server is running, it exits
successfully with an empty report.

For local source-tree inspection, the bundled helper builds the binary and lets
MCP Inspector start it over STDIO:

```bash
scripts/run-inspector.sh
scripts/run-inspector.sh --cli --method tools/list
```

### Versioned local installation

`scripts/local-install.sh` (wrapped by the `install:*` tasks) installs immutable,
commit-identified release binaries outside the source worktree and selects them
through explicit aliases:

```bash
task install:candidate               # verify, build and install the clean HEAD
task install:status                  # show aliases and installed versions
task install:promote                 # stable -> candidate, previous -> old stable
task install:promote ID=0.1.0+0123456789ab
task install:rollback                # swap stable and previous
```

`install:candidate` refuses a worktree with uncommitted changes, runs the full
`scripts/test-pyramid.sh` gate, builds `cargo build --release --locked` with
`MAVEN_MCP_BUILD_COMMIT` set to the `HEAD` commit, takes the executable path from
Cargo's JSON artifact messages (so `CARGO_TARGET_DIR` or a configured target
cannot leave a stale `target/release` binary in its place), checks that `HEAD`
and the worktree did not change meanwhile and that the binary reports the `HEAD`
commit, and copies it with `verified=true` in `BUILD-INFO`. Builds without an
embedded commit (plain `cargo build`) are rejected.
`scripts/local-install.sh install --unverified <binary>` installs an already
built, commit-identified binary without the clean-tree check, the gate, or the
commit match, and records `verified=false`; `install:status` shows the flag of
every version. The install root is
`MAVEN_MCP_INSTALL_ROOT`, by default `${XDG_DATA_HOME:-$HOME/.local/share}/maven-mcp`,
and must be outside the source worktree:

```text
<root>/versions/<id>/maven-mcp    immutable copy of one identified binary
<root>/versions/<id>/BUILD-INFO   id, version, commit, verified, sha256, installed_at
<root>/candidate -> versions/<id>
<root>/stable    -> versions/<id>
<root>/previous  -> versions/<id>
```

The installation `<id>` is `<package version>+<first 12 commit characters>`, the
same value the server reports as MCP `initialize` `serverInfo.version`. An
installed version is never overwritten: reinstalling identical content is a
no-op, and different content under the same id is an error. Promotion and
rollback verify the target's SHA-256 and reported commit against `BUILD-INFO`,
then switch the symlinks atomically. Rollback swaps `stable` and `previous`, so
running it twice restores the original state. Hosts that are already running keep
their binary; a new host session starts the newly selected one. Installing a
candidate locally does not publish a release.

Point MCP hosts at `<root>/stable/maven-mcp` for everyday use, or at
`<root>/candidate/maven-mcp` while trying a candidate. Check the running version
with the host's server information, `maven-mcp stats`, or `--version` on the
alias path.

### Codex project configuration

In a trusted project, add `.codex/config.toml` with the absolute path of the
selected alias and the trusted Maven project directories:

```toml
[mcp_servers.maven-mcp]
command = "/home/<user>/.local/share/maven-mcp/stable/maven-mcp"
args = ["--jenv"]
env = { MAVEN_TRUSTED_PROJECT_DIRECTORIES = "/absolute/path/to/trusted-projects" }
startup_timeout_sec = 300
```

Alternatively, register the same STDIO command with `codex mcp add`. Verify it
with `codex mcp get maven-mcp`, `codex mcp list`, or `/mcp`. See the
[official Codex MCP documentation](https://learn.chatgpt.com/docs/extend/mcp?surface=cli).

### GitHub Copilot CLI configuration

Copilot CLI defaults local commands to STDIO. In a trusted folder, add the same
alias as a workspace server in `.github/mcp.json` (or `.mcp.json`):

```json
{
  "mcpServers": {
    "maven-mcp": {
      "type": "local",
      "command": "/home/<user>/.local/share/maven-mcp/stable/maven-mcp",
      "args": ["--jenv"],
      "env": { "MAVEN_TRUSTED_PROJECT_DIRECTORIES": "/absolute/path/to/trusted-projects" },
      "tools": ["*"],
      "timeout": 300000
    }
  }
}
```

Alternatively, register it as a user server:

```bash
copilot mcp add \
  --timeout 300000 \
  maven-mcp -- /home/<user>/.local/share/maven-mcp/stable/maven-mcp --jenv
copilot mcp get maven-mcp
```

Use the `candidate` alias instead of `stable` in either host to try a candidate.
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
For focused tests, serialization covers the report snapshot, Maven execution,
report parsing and saving the last result. Concurrent requests therefore report
only their own updated test reports; `get_last_maven_test_failures` returns the
last completed focused test for the requested project. Effective POM requests
hold the same permit through output preparation, execution, parsing and cleanup.
Previous effective POM output is cleared before execution, and temporary output
is removed on success, failure, timeout or request cancellation.
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

A `run_maven` call (like `run_maven_lifecycle`) drops the cached project index and
re-reads the reactor model, so index-backed tools such as `get_class_source`
rebuild it on the next request. Those tools read
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

### Redaction and data minimization

Maven-backed responses pass through one shared redaction step before they are
returned or retained. It protects these fields:

- `stdout` and `stderr` of every Maven run, including the build diagnostics,
  compiler diagnostics and MCP error text derived from them;
- `failures[].class_name`, `failures[].test_name`, `failures[].message` and
  `report_error` of `run_maven_test`, and the `failures` returned by
  `get_last_maven_test_failures`;
- `projects[].properties` and `error` of `get_effective_pom`;
- `descriptor.properties` of `get_artifact_pom`.

The step replaces the complete value of an `Authorization` or
`Proxy-Authorization` header (scheme and credentials through the end of the
line, with any case and spacing) with `<redacted>`. It also replaces values after a
key that contains `password`, `passwd`, `passphrase`, `secret`, `token`,
`credential`, `api key`, `access key` or `private key`. The key forms are
`key=value`, `key: value`, quoted JSON and XML elements. URL user information
becomes `<credentials>`, and self-identifying GitHub, `sk-`, AWS access-key and
JWT tokens become `<redacted>`. Property values whose names contain one of those
words are masked completely; other property values only lose embedded
credentials, so values such as `java.version` stay intact. In Maven output and
test diagnostics, the project root, execution repository and home directory
become `<PROJECT_ROOT>`, `<MAVEN_REPOSITORY>` and `<HOME>`. `redaction_count`
reports the number of actual replacements. Matching runs on the original UTF-8
text, so arbitrary Unicode output cannot crash a request. Long test failure
messages are redacted before they are truncated.

Requested content is returned verbatim: `get_class_source`,
`get_declaration_source`, `get_jar_entry` and the other artifact tools do not
rewrite source code or archive entries. Redaction is pattern-based. It does not
recognize a secret without a recognizable key, header or token format, a secret
split across lines, or custom header names. Values next to a sensitive word may
be redacted even when they are not secrets.

Diagnostics go only to stderr. Logs and MCP errors name archives by file name
instead of their absolute host path. `RUST_LOG` can raise the verbosity of
`maven_mcp`, but MCP transport (`rmcp`) events are always suppressed at every
level, because they can quote complete requests, notifications, cancellation
reasons and responses. A panic logs only
its source file and line, never its payload.

Local logs, MCP Inspector sessions, `target/promptfoo/`,
`target/mcp-test-report/`, `target/coverage/`, `target/real-maven-smoke/`,
benchmark results and
`maven-agent-log` reports can contain absolute paths, model output or
credentials. Keep them out of version control, issue reports and release
packages; a release contains only the built binaries and the public
documentation.

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
| `MAX_PROJECT_INDEXES` | `4` | Maximum number of canonical project roots whose index is retained in the in-process project-index cache; the number of retained project runners is four times this value. |
| `MAX_PROJECT_INDEX_BYTES` | `1073741824` | Budget for the estimated retained memory of cached runners, indexes and running index builds together. Exceeding it evicts least recently used idle entries; a build that cannot fit fails with an error naming this variable. |
| `MAX_HEAVY_JOBS` | `2` | Maximum number of blocking index, archive and file jobs that run at the same time. |
| `MAX_QUEUED_HEAVY_JOBS` | `16` | Maximum number of further heavy jobs waiting for a running slot. When running and queued slots are all taken, new jobs fail immediately with an "overloaded" MCP error. |
| `MAVEN_TRUSTED_PROJECT_DIRECTORIES` | none | Required for Maven-backed project operations. Platform path-list of existing absolute directory trees; a canonical `project_path` must be equal to or nested below one entry. |
| `MAVEN_EXECUTABLE` | none | Absolute Maven binary path; required only when no valid executable Maven Wrapper is available. |
| `MAVEN_EXECUTION_REPO_PATH` | none | Optional existing writable Maven local repository for structured Maven execution and exact artifact inspection. Exact artifact inspection otherwise reads `$HOME/.m2/repository`. `run_maven` requires an explicit `-Dmaven.repo.local` argument to select this repository. |
| `MAVEN_EXECUTION_NETWORK` | `false` | When `true`, structured lifecycle, test and diagnostic tools omit `--offline`. `run_maven` never injects this flag. |
| `MAVEN_TIMEOUT_SECONDS` | `300` | Maximum runtime of one Maven child process. |
| `MAX_MAVEN_OUTPUT_BYTES` | `1048576` | Separate upper limit for stdout and stderr. |
| `MAVEN_MCP_RUNTIME_DIR` | `$XDG_RUNTIME_DIR/maven-mcp` or `$HOME/.cache/maven-mcp/runtime` | User-private directory for live runtime status files consumed by `maven-mcp stats`. |
| `MAVEN_MCP_INDEX_CACHE` | `true` | `false` (or `0`) disables the [persistent index cache](#persistent-index-cache); every process then builds its first project index cold. |
| `MAVEN_MCP_CACHE_DIR` | `$XDG_CACHE_HOME/maven-mcp/index` or `$HOME/.cache/maven-mcp/index` | Absolute, user-private directory of the persistent index cache; safe to delete. |
| `MAX_INDEX_CACHE_BYTES` | `1073741824` | Positive byte budget of the persistent index cache entries; least recently used entries are evicted after each write. |
| `MAVEN_MCP_BUILD_COMMIT` | none | Build time only, not read at runtime. Full 40-character lowercase commit embedded into `--version`, `serverInfo.version` and stats; set by `install:candidate`. Any other value fails the build. |
| `MAVEN_MCP_INSTALL_ROOT` | `${XDG_DATA_HOME:-$HOME/.local/share}/maven-mcp` | Used only by `scripts/local-install.sh` and `scripts/dogfood-smoke.sh`: absolute versioned installation root outside the source worktree. |
| `JENV_ROOT` | `$HOME/.jenv` | jenv installation root used only when the server starts with `--jenv`; must be an absolute path when explicitly set. |
| `RUST_LOG` | `maven_mcp=info` | Logging level/filter for stderr diagnostics. MCP transport (`rmcp`) events are always suppressed; see [Redaction and data minimization](#redaction-and-data-minimization). |

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

Processing is bounded. SQLite tables are streamed one row at a time, JSONL and
IntelliJ logs are read line by line, and `--group-by` keeps only the original
group identities (not a second copy of every event) until redaction has run;
the reported counts, labels, and ordering are unchanged. Three limits apply:

| Option | Default | When reached |
| --- | --- | --- |
| `--max-record-bytes` | 8 MiB | A JSONL/IntelliJ line, a SQLite row (sum of its column byte lengths, taken from the record header so an oversized value is never loaded), or a VS Code `workspace.json` over the limit is skipped, never parsed, and counted. |
| `--max-input-bytes` | 64 MiB | A whole-document JSON input (VS Code/Insiders) over the limit is skipped and counted; SQLite and line-based inputs are not limited by file size. |
| `--max-state-bytes` | 1 GiB | The estimated retained state (events, including their growth by redaction escapes and placeholders; Kilo session/message metadata while its database is read; deduplication identity keys; redaction replacement tables; grouping keys, per-group counting slots, labels, and suffix tables; and a parsed VS Code document at eight times its size) would exceed the budget. Events dropped by deduplication or filtering give their share back. |

Skipped oversized records or inputs print `report is partial` with their counts on
stderr and the run still succeeds if other commands were recovered; if nothing was
recovered it fails. Exceeding `--max-state-bytes` is an error: the run fails with
a message naming the option and writes no report, stdout, or `--output` file.
The budget is an estimate of retained state, not an operating-system cap: a
single record that is being parsed (up to `--max-record-bytes`, several times
that once parsed) and the rendered report text are held in addition to it.

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
        "command": "cd \"$PROJECT_PATH\" || exit 1; cp=$(./mvnw -o dependency:build-classpath) || exit 1; printf '%s\\n' \"$cp\" | awk '/Dependencies classpath:/ {getline; print}' | tr ':' '\\n' | grep -v '^$' | while IFS= read -r jar; do entries=$(unzip -Z1 \"$jar\") || exit 1; if printf '%s\\n' \"$entries\" | grep -qx 'org/example/Foo.class'; then printf '%s\\n' \"$jar\"; fi; done",
        "cwd": "${PROJECT_PATH}"
      },
      "mcp": {
        "tool": "search_classes",
        "arguments": {
          "project_path": "${PROJECT_PATH}",
          "query": "org.example.Foo"
        }
      },
      "identity": {
        "mcp_values": "/results/*/jar/path",
        "normalize": "file_names"
      }
    }
  ]
}
```

`shell.command` is passed to `/bin/sh -c`, so run only specifications you own or
have reviewed. `/bin/sh` has no `pipefail`, so resolve the classpath into a
variable and exit on a Maven failure before parsing it, and end each loop body in
an `if`/`case` so a non-matching last JAR does not become the command's exit
status; an unreadable JAR still exits non-zero. `cwd` is optional. MCP `arguments` can be omitted when the tool
expects no arguments. Case IDs must be unique.

### Project binding

`--project-path` takes the absolute Maven project root (a directory containing
`pom.xml`; symlinks are resolved). It binds one project to the whole run:

- `${PROJECT_PATH}` in `shell.cwd` and in any string of the MCP `arguments` is
  replaced by the canonical project path. Every MCP argument object must set
  `project_path` to it, so each MCP call carries a valid `project_path`.
- Shell commands receive the canonical path in the `PROJECT_PATH` environment
  variable; use `"$PROJECT_PATH"` inside `command`.
- The MCP child process gets `MAVEN_TRUSTED_PROJECT_DIRECTORIES` set to exactly
  that project, replacing any inherited value. Passing `--project-path` is the
  trust decision; do it only for projects whose Maven build you trust.

A specification that mentions `PROJECT_PATH` without `--project-path` is
rejected before anything runs, and the bundled `benchmark.json` needs no
personal path. Without `--project-path`, the specification must carry literal
absolute `project_path` values and the host trust variable must already be set
in the environment.

### Comparable results

MCP tools search only the artifacts on the project's Maven test classpath, so a
shell command that scans all of `~/.m2` measures different work. The bundled
shell commands therefore resolve the same classpath with Maven
(`dependency:build-classpath`, test scope, offline, `MAVEN_EXECUTION_REPO_PATH`
as the local repository when set) and inspect only those JARs. Every case may
declare an `identity` block; before warmup and measurement, the first shell and
first MCP execution are normalized and compared:

- `mcp_values` is a slash-separated path into the MCP structured result; `*`
  iterates the elements of an array or the values of an object.
- Shell stdout and the selected MCP values are split into lines, trimmed, and
  compared as sorted sets (`normalize: "lines"`, the default). `file_names`
  keeps only the last path component, so absolute shell paths compare with
  repository-relative MCP paths.

The report records `comparison.identity.status` as `matched`, `mismatched`
(with `only_in_shell`/`only_in_mcp` samples), `unavailable` (a side failed, the
shell output hit `--max-output-bytes`, the path did not resolve, or both result
sets are empty), or `not_checked` (no `identity` block). Any status other than
`matched` and `not_checked` is a failed comparison: the speed and token ratios
are `null`, and a difference is never reported as a speedup. Raw runs are still
recorded.

```bash
cargo build --locked --bin maven-mcp
cargo run --locked --bin maven-benchmark -- \
  --spec benchmark.json \
  --project-path /absolute/path/to/maven-project \
  --mcp-command target/debug/maven-mcp \
  --iterations 10 \
  --warmup 2 \
  --timeout-seconds 300 \
  --output target/benchmark-results.json
```

### Cold first call and reuse

Each case on each side first runs once as the **first call** (`first_call`,
iteration 0), then `--warmup` unmeasured runs, then `--iterations` measured
runs (`runs`, summarized as the reuse measurement). The report states this in
`measurement_modes`:

- The MCP first call for a project in the freshly started server includes the
  Maven classpath resolution and project index construction. Later cases on the
  same project, and all later calls, reuse the cached index. The benchmark starts
  the MCP command with `MAVEN_MCP_INDEX_CACHE=false` unless that variable is set,
  so by default there is no persistent index cache across benchmark invocations
  and every invocation starts cold and is repeatable; set
  `MAVEN_MCP_INDEX_CACHE=true` to measure a warm first call instead. Only the first case measures the MCP cold
  cost; case order matters.
- Shell commands have no cache: the first call and every reuse run repeat all of
  their own work, including the Maven classpath resolution in the bundled
  commands. Maven preparation is therefore part of the measured time on both
  sides, once per MCP cold call and on every shell run.
- Not measured: MCP process start and initialization, warmup runs, and building
  the binaries. Keep the local Maven repository populated and run offline so the
  first runs do not include downloads. `comparison.first_call_shell_to_mcp_duration_ratio`
  compares the cold calls, `shell_to_mcp_duration_ratio` the reuse means.

No speedup is promised: report whatever the measurement shows.

The default for `--max-output-bytes` is 1 MiB. It limits the output included in
request/response metrics; `output_truncated` reports truncation. For shell
commands the limit is a single budget shared by stdout and stderr: the benchmark
never stores more than that many bytes and reads and discards the remainder, so
very large or endless output does not exhaust memory. Each shell command runs in
its own process group within the `--timeout-seconds` deadline. When the shell
exits or the deadline passes, the whole group is killed, so descendants that hold
the output pipes open cannot hang the run or outlive it. A timed-out run is
recorded with `success: false` and a `timed out` error, and the benchmark
continues with the next run and case. Under
`schema_version: "1.0"`, the JSON preserves the shell- and MCP-side request size
for each case, the first call and every raw run's duration, success state,
response size, and token estimate, as well as the mean and median of successful
runs and the speed and token ratios between the two sides. A failed command or
tool call does not stop the remaining measurements; its record retains an exit
code or error message.

The token metric is intentionally a deterministic, provider-neutral estimate:
`ceil(Unicode scalar values / 4)`. It is not a model tokenizer or API billing
value and may differ from the actual token count depending on language and
content. The report records the method and this limitation as metadata. On the
shell side, the command, stdout, and stderr are included; on the MCP side, the
tool name/arguments and tool-result JSON are included. MCP initialization,
server startup, and protocol headers are not part of the measured tool-call
duration or token value. Because the shell side counts its whole command text
(including the Maven classpath preparation) and the MCP side counts the JSON
envelope, treat the percentage as a rough relative indicator, not a billing or
context-window figure.

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

task benchmark:run PROJECT_PATH=/absolute/path/to/maven-project
task benchmark:run PROJECT_PATH=/absolute/path/to/maven-project \
  SPEC=my-benchmark.json OUTPUT=target/custom-results.json ITERATIONS=10 WARMUP=2

task install:candidate
task install:status
task install:promote
task install:rollback

task dogfood:smoke MANIFEST=/private/dogfood.json OUTPUT=/private/dogfood-results

task release:check
task release:check TAG=v0.1.0
task release:notes VERSION=0.1.0
task release:archive                       # OUTPUT=target/dist by default
task release:verify-archive ARCHIVE=target/dist/maven-mcp-0.1.0-x86_64-unknown-linux-gnu.tar.gz
```

The `install` tasks are described in
[Versioned local installation](#versioned-local-installation),
`dogfood:smoke` in [Multi-project dogfood smoke](#multi-project-dogfood-smoke),
and the `release` tasks in [Releases](#releases). They wrap
`scripts/release.sh`; none of them tags, uploads, or publishes.

The repository includes a default `benchmark.json` specification for common
Maven inspection commands, parameterized with `${PROJECT_PATH}`; the project
must declare `org.slf4j:slf4j-api` on its test classpath for the bundled cases.
`PROJECT_PATH` is required and is passed as `--project-path`
(see [Shell–MCP Benchmark CLI](#shellmcp-benchmark-cli)).

The other benchmark variables can be overridden. Defaults are `SPEC=benchmark.json`,
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
Inspector and agent-eval tasks require the Node.js and npm versions pinned in
`.node-version` and `tools/package.json`. They install the locked tools with
`npm ci`, and both launch the MCP binary directly over STDIO.

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
directory tree, and execution occurs through a real STDIO MCP client. Each
independent public contract (the tool catalog, each output schema rule, each
error class, and each project tool) is its own test, so a failure names the
broken contract. Lifecycle tests ensure EOF and SIGTERM stop the process, even
while index work is blocked, and stdout contains JSON-RPC only.

There are three separate verification levels:

| Level | Command | When | Network, credentials |
| --- | --- | --- | --- |
| Deterministic gate | `scripts/test-pyramid.sh` | before every handoff and in the required `CI` check | none; synthetic fixtures and fake Maven wrappers, no Maven, JDK or `~/.m2` |
| Real Maven smoke | `scripts/real-maven-smoke.sh` | on demand | network once to prepare a pinned Maven and plugins; then offline |
| Multi-project dogfood smoke | `scripts/dogfood-smoke.sh` | on demand, with an installed binary | the user's own trusted Maven projects, JDK and Maven repository |
| Agent eval | `scripts/run-agent-eval.sh` | only when separately decided | a model provider account; usually paid |

The gate runs neither the smokes nor the agent eval, and its result does not
cover them.

Run the complete test pyramid—unit tests, real child-process STDIO MCP
integration and lifecycle tests, YAML scenarios, JSON snapshots, and a Markdown
report—with one command:

```bash
scripts/test-pyramid.sh
```

The gate also measures Rust line coverage of the production code and fails
below 80%. The metric is covered lines divided by all measurable lines across
`src/` (including the binaries under `src/bin/`), checked on the unrounded
value. Every Cargo test target contributes, including the tests that start the
production binaries as child processes; `cargo-llvm-cov` instruments those
binaries. The gate clears previous coverage data before each test run so only
the current run contributes to the result. Dependencies and files under `tests/`
are not counted, and no production module is excluded. Unit test modules live
under `tests/unit/` and are included through `#[cfg(test)]` declarations in their
production modules, so their test bodies also stay outside the denominator.
Prerequisites (no silent
skip; the script stops with a message if they are missing):

```bash
rustup component add llvm-tools-preview
cargo install cargo-llvm-cov --version 0.8.5 --locked
```

`python3` is also required. The reports are written to:

```text
target/mcp-test-report/report.md
target/coverage/html/index.html
target/coverage/summary.json
```

Branch coverage is not measured: `cargo llvm-cov --branch` requires a nightly
Rust toolchain, and the project pins stable Rust. The `branches` totals in
`summary.json` are therefore zero; they are not a branch coverage result.

### Real Maven Smoke

The deterministic fixtures are complemented by an opt-in smoke test that runs a
real Apache Maven:

```bash
scripts/real-maven-smoke.sh            # prepare once, then run offline
scripts/real-maven-smoke.sh --refresh  # re-download and re-prepare first
```

It pins Apache Maven `3.9.9` (verified by SHA-512), `maven-dependency-plugin`
`3.8.1` and `maven-help-plugin` `3.5.1`, and accepts a JDK with major version
17, 21 or 25 from `JAVA_HOME` or `PATH` (with jenv: `jenv exec
scripts/real-maven-smoke.sh`). Prerequisites are `curl`, `sha512sum`, `tar` and
Cargo. A one-time online step resolves only the pinned plugins into
`target/real-maven-smoke/` with an empty `<settings/>` file. Each run then
copies that repository into a temporary directory, adds synthetic artifacts, and
runs Maven offline, never reading `~/.m2` or the user's Maven settings or
`~/.mavenrc`. The ignored Cargo tests in `tests/real_maven_smoke.rs` check that
real Maven reproduces the versioned `tests/fixtures/dependency_tree.txt` capture
and that the server's dependency, classpath, class search and effective-POM
tools work against real Maven over STDIO. `target/real-maven-smoke/` is a local
cache; do not commit it.

### Multi-project dogfood smoke

The opt-in dogfood smoke runs an installed binary against real Maven projects
that you choose and trust:

```bash
scripts/dogfood-smoke.sh --manifest /private/dogfood.json --output /private/dogfood-results
scripts/dogfood-smoke.sh --manifest /private/dogfood.json --output /private/dogfood-results \
  --binary "$HOME/.local/share/maven-mcp/stable/maven-mcp"
```

The binary defaults to `<install root>/candidate/maven-mcp` and must not be a
build output inside the source worktree. The manifest and the output directory
must be outside the repository; both are private, so never commit or package the
project list, paths, logs or results. Manifest format:

```json
{
  "schema_version": 1,
  "server_arguments": ["--jenv"],
  "server_environment": { "MAVEN_EXECUTION_REPO_PATH": "/private/maven-repository" },
  "reuse_repetitions": 10,
  "projects": [
    {
      "id": "single",
      "layout": "single_module",
      "path": "/absolute/path/to/project",
      "class_query": "org.slf4j.Logger",
      "source_class": "org.slf4j.Logger",
      "focused_test": { "test_class": "com.example.FooTest" }
    },
    {
      "id": "reactor",
      "layout": "multi_module",
      "path": "/absolute/path/to/reactor",
      "class_query": "org.slf4j.Logger",
      "lifecycle": { "phase": "compile", "module": "core", "also_make": true }
    }
  ]
}
```

Each project gets a freshly started STDIO server (no cache from earlier
projects; the [persistent index cache](#persistent-index-cache) is off unless the
manifest's `server_environment` sets `MAVEN_MCP_INDEX_CACHE`) whose
`MAVEN_TRUSTED_PROJECT_DIRECTORIES` is exactly that project. The
ignored Cargo test `tests/dogfood_smoke.rs` checks `initialize` (the reported
`serverInfo.version` must equal the binary's installation id), `tools/list`,
`inspect_maven_project` against the declared layout, the cold first
`search_classes` (classpath resolution plus index build), `reuse_repetitions`
cached repeats, `index_stats`, `get_class_source`, `get_dependency_tree`, the test
`get_maven_classpath`, `get_effective_pom`, the Maven and JDK versions through
`run_maven --version`, `list_maven_test_classes`, and the optional focused test
(`run_maven_test`) and lifecycle phase (`run_maven_lifecycle`). It records each
step's result and duration plus the server RSS after initialization, after the
cold query, and at the end (current and peak) in
`<output>/dogfood-<id>-<unix ms>.json`, then fails if any step failed.
Structured tools run offline unless `MAVEN_EXECUTION_NETWORK=true` is in
`server_environment`, so prepare the projects' dependencies first. Native Maven
execution is not a sandbox: plugins and tests run with your permissions.

### Hosted CI

GitHub Actions runs the same verification gate from
`.github/workflows/ci.yml` for pushes to `main` and pull requests targeting
`main`. Every job uses the committed `Cargo.lock` and installs the Rust
toolchain pinned in `rust-toolchain.toml`, except `MSRV`:

- `Gate (x86_64-unknown-linux-gnu)` runs `cargo deny` and
  `scripts/test-pyramid.sh`, including the same 80% line coverage gate. The
  `target/coverage/` report is uploaded as the `rust-coverage-report` artifact
  even when the gate fails.
- `Platform (<target>)` builds the release binary natively for every
  [supported target](#supported-platforms) and checks it with
  `scripts/check-linux-binary.sh`. On `aarch64-unknown-linux-gnu`, it also runs
  every Cargo test target.
- `MSRV` runs `cargo check --locked --all-targets --all-features` with the
  `rust-version` from `Cargo.toml`.
- `Node tools` installs the pinned Node.js, runs `npm ci` for the locked tools,
  and lists the server's tools through the MCP Inspector CLI. No model provider
  is called.
- `CI` passes only when every job above passed. It is the required status check.

The workflow has read-only repository permissions, does not persist checkout
credentials, and uses no dependency cache.

A pushed `v<version>` tag starts the separate `Release` workflow
(`.github/workflows/release.yml`), which runs this complete `CI` workflow
first and then builds and publishes the release (see
[Creating a release](#creating-a-release)). Only its `prepare`, `artifacts`, and
`publish` jobs get write access to repository contents, and only `publish`
uses the `release` environment and its crates.io token.

The active default-branch ruleset requires a pull request and a successful,
up-to-date `CI` check before merging. It requires zero approving reviews for
the single-maintainer workflow, blocks deletion and force pushes, and requires
linear history. Use a short-lived branch, open a pull request to `main`, wait
for `CI`, then squash merge. The repository deletes the branch after merging.

### Dependency Maintenance

Renovate opens grouped update pull requests, including the pinned Rust
toolchain, Node.js version, and locked npm tools. The required `CI` check runs
`cargo deny` for advisories, licenses and sources on every pull request and push
to `main`; the `Dependency policy` workflow also scans daily. See
[docs/dependency-maintenance.md](docs/dependency-maintenance.md) for the policy,
exception register and alert handling.

### Agent and LLM Evaluation

Above the deterministic Rust test pyramid, a separate, optional Promptfoo
evaluation checks whether an MCP-capable model selects the correct tool and essential
arguments for positive, ambiguous, and no-result natural-language requests, and
then avoids inventing artifacts, versions, JARs, or classes absent from the
fixture.

```bash
export PROMPTFOO_PROVIDER="openai:responses:gpt-5-mini"
export OPENAI_API_KEY="..." # or the standard environment secret for your provider
scripts/run-agent-eval.sh
```

The script runs the Promptfoo version locked in `tools/package-lock.json`, regenerates a marker-protected,
request-scoped project fixture under `target/promptfoo/fixture` from Rust,
configures its trusted project directory and local execution repository, and
passes the generated absolute `project_path` to every MCP request. Promptfoo
starts the local server over STDIO, runs the evaluation with caching disabled,
and exits non-zero on a failed test.
The reviewable HTML is written to `target/promptfoo/report.html`, and the complete
machine-readable JSON to `target/promptfoo/results.json`. The provider/model is
configured only through `PROMPTFOO_PROVIDER`, while authentication comes from
the provider's standard environment variable; never put secrets in YAML. The
report may contain complete model output, configuration and local paths, so
keep it local: do not commit it, attach it to issues or pull requests, or
upload it as a CI artifact.

Neither `scripts/test-pyramid.sh` nor the `CI` workflow runs this evaluation;
only the offline check of its configuration file is part of the gate. It calls
a real, usually paid model provider, so run it only when that cost is
intentionally accepted. Without `PROMPTFOO_PROVIDER`, or without Node.js of the
pinned major version and exactly the pinned npm version, the script stops with
exit code 2 before building anything.

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
