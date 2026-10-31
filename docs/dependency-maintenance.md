# Dependency, advisory and license maintenance

## Automated updates

Renovate (`renovate.json`) opens update pull requests for:

- Cargo crates and `Cargo.lock`;
- the developer and CI Rust toolchain pinned in `rust-toolchain.toml`
  (`rust-lang/rust` releases);
- GitHub Actions (pinned by digest) and the CI Cargo tools `cargo-deny` and
  `cargo-llvm-cov` (in the workflows, `scripts/`, `README.md`, and `AGENTS.md`);
- the Node.js version in `.node-version` and the locked Node.js tools in
  `tools/package.json` and `tools/package-lock.json` (MCP Inspector, Promptfoo,
  and the npm `packageManager` pin).

The minimum supported Rust version (`rust-version` in `Cargo.toml`) is not
updated automatically. Raise it only by a deliberate decision; the `MSRV` CI job
checks it. No container image is used today; the `dockerfile` manager is enabled
so a future image is covered automatically.

- Schedule: Mondays before 06:00 Europe/Budapest; at most 5 open PRs.
- Grouping: Cargo minor/patch updates, GitHub Actions with the CI Cargo tools,
  the Rust toolchain, and the Node.js version with the npm tools are each
  grouped. Major updates get separate PRs.
- Lockfile maintenance runs monthly.
- Security vulnerability PRs are raised immediately, labelled `security`.

Every update PR must pass the required `verify` check before squash merge. That
check includes the dependency policy, the native Linux platform matrix, the MSRV
check, and the locked Node.js tool install.

## Policy gate

The `Verify` workflow (`.github/workflows/verify.yml`) runs `cargo deny`
(`deny.toml`) on every pull request and push to `main`, including every lockfile
change. `.github/workflows/dependency-policy.yml` also scans daily and on manual
dispatch so newly published RustSec advisories surface without a code change.

| Check | Policy |
| --- | --- |
| Advisories | Any vulnerability, yanked crate or unmaintained workspace dependency fails the job |
| Licenses | Allow-list only: 0BSD, MIT, Apache-2.0 (and LLVM exception), BSD-2/3-Clause, ISC, Unicode-3.0, Zlib, CC0-1.0, MPL-2.0, BSL-1.0. Anything else (e.g. GPL, AGPL) fails |
| Sources | Only crates.io; git and unknown registries are denied |
| Bans | Wildcard version requirements denied; duplicate versions warn |

Policy failures fail the required `verify` check and block merging. They also block
a release: do not tag a release while `verify` or the daily `Dependency policy` scan
is red on `main`. Triage scheduled scan failures within 7 days (critical/high
severity: 48 hours).

## Handling findings

1. Prefer upgrading or replacing the crate.
2. If no fix exists, add a time-limited exception to `deny.toml`
   (`advisories.ignore` or `licenses.exceptions`) and a row in the register below
   in the same pull request.
3. Remove the exception as soon as a fix is available or it expires.

## Exception register

Each exception needs a reason, an owner and an expiry date (at most 90 days).
Expired entries must be fixed or renewed with a fresh justification.

| ID / crate | Reason | Owner | Expires |
| --- | --- | --- | --- |
| _none_ | | | |

## Reporting

Report suspected vulnerabilities in this project privately through GitHub
security advisories on the repository rather than public issues.

## Local check

```bash
cargo install cargo-deny --locked
cargo deny --locked check
```
