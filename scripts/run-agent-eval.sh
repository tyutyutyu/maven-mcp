#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
cd "$PROJECT_ROOT"

PROMPTFOO_VERSION="0.121.19"
EVAL_ROOT="target/promptfoo"
FIXTURE_ROOT="$PROJECT_ROOT/$EVAL_ROOT/fixture"
FIXTURE_REPOSITORY="$FIXTURE_ROOT/maven-repository"
FIXTURE_PROJECT="$FIXTURE_ROOT/project"
REPORT_HTML="$EVAL_ROOT/report.html"
REPORT_JSON="$EVAL_ROOT/results.json"

if ! command -v npx >/dev/null 2>&1; then
  echo "npx is required (Promptfoo 0.121.19 requires a supported Node.js runtime)" >&2
  exit 2
fi
if [[ -z "${PROMPTFOO_PROVIDER:-}" ]]; then
  echo "Set PROMPTFOO_PROVIDER to an MCP-capable Promptfoo provider id, for example openai:responses:gpt-5-mini" >&2
  exit 2
fi

mkdir -p "$EVAL_ROOT"
cargo run --quiet --locked --bin maven-eval-fixture -- "$FIXTURE_ROOT" >/dev/null
cargo build --quiet --locked --bin maven-mcp

export MAVEN_TRUSTED_PROJECT_DIRECTORIES="$FIXTURE_ROOT"
export MAVEN_EXECUTION_REPO_PATH="$FIXTURE_REPOSITORY"
export RUST_LOG="maven_mcp=warn"
export PROMPTFOO_MCP_COMMAND="$PROJECT_ROOT/target/debug/maven-mcp"
export PROMPTFOO_PROJECT_PATH="$FIXTURE_PROJECT"
export PROMPTFOO_PASS_RATE_THRESHOLD="1"
npx --yes "promptfoo@$PROMPTFOO_VERSION" eval \
  --config tests/promptfoo/promptfooconfig.yaml \
  --no-cache \
  --output "$REPORT_HTML" \
  --output "$REPORT_JSON"

echo "Promptfoo review report: $REPORT_HTML"
echo "Promptfoo automation result: $REPORT_JSON"
