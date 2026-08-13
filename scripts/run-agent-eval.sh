#!/usr/bin/env bash
set -euo pipefail

PROMPTFOO_VERSION="0.121.19"
EVAL_PORT="${MAVEN_MCP_EVAL_PORT:-18083}"
EVAL_ROOT="target/promptfoo"
FIXTURE_REPOSITORY="$EVAL_ROOT/maven-repository"
REPORT_HTML="$EVAL_ROOT/report.html"
REPORT_JSON="$EVAL_ROOT/results.json"

if ! command -v npx >/dev/null 2>&1; then
  echo "npx is required (Promptfoo 0.121.19 requires a supported Node.js runtime)" >&2
  exit 2
fi
if ! command -v curl >/dev/null 2>&1; then
  echo "curl is required to wait for the fixture MCP server" >&2
  exit 2
fi
if [[ -z "${PROMPTFOO_PROVIDER:-}" ]]; then
  echo "Set PROMPTFOO_PROVIDER to an MCP-capable Promptfoo provider id, for example openai:responses:gpt-5-mini" >&2
  exit 2
fi

mkdir -p "$EVAL_ROOT"
cargo run --quiet --locked --bin maven-eval-fixture -- "$FIXTURE_REPOSITORY" >/dev/null
cargo build --quiet --locked --bin maven-mcp

MAVEN_REPO_PATH="$FIXTURE_REPOSITORY" \
BIND_ADDRESS="127.0.0.1:$EVAL_PORT" \
RUST_LOG="maven_mcp=warn" \
target/debug/maven-mcp >"$EVAL_ROOT/server.log" 2>&1 &
SERVER_PID=$!
cleanup() {
  kill "$SERVER_PID" 2>/dev/null || true
  wait "$SERVER_PID" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

for _ in {1..100}; do
  if curl --silent --fail "http://127.0.0.1:$EVAL_PORT/healthz" >/dev/null; then
    break
  fi
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    echo "fixture MCP server stopped during startup; see $EVAL_ROOT/server.log" >&2
    exit 1
  fi
  sleep 0.1
done
curl --silent --fail "http://127.0.0.1:$EVAL_PORT/healthz" >/dev/null || {
  echo "fixture MCP server did not become healthy; see $EVAL_ROOT/server.log" >&2
  exit 1
}

export PROMPTFOO_MCP_URL="http://127.0.0.1:$EVAL_PORT/mcp"
export PROMPTFOO_PASS_RATE_THRESHOLD="1"
npx --yes "promptfoo@$PROMPTFOO_VERSION" eval \
  --config tests/promptfoo/promptfooconfig.yaml \
  --no-cache \
  --output "$REPORT_HTML" \
  --output "$REPORT_JSON"

echo "Promptfoo review report: $REPORT_HTML"
echo "Promptfoo automation result: $REPORT_JSON"
