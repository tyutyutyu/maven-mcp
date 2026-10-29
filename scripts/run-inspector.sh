#!/usr/bin/env bash
set -euo pipefail

readonly SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
cd "${PROJECT_ROOT}"
# shellcheck source=scripts/node-tools.sh
source "${SCRIPT_DIR}/node-tools.sh"
node_tools_prepare "${PROJECT_ROOT}"
cargo build --locked --quiet --bin maven-mcp
readonly MCP_COMMAND="${PROJECT_ROOT}/target/debug/maven-mcp"

echo "Starting MCP Inspector with STDIO transport: ${MCP_COMMAND}" >&2
if [[ "${1:-}" == "--cli" ]]; then
    shift
    "${NODE_TOOLS_BIN}/mcp-inspector" \
        --cli "${MCP_COMMAND}" --transport stdio "$@"
else
    "${NODE_TOOLS_BIN}/mcp-inspector" \
        "${MCP_COMMAND}" --transport stdio "$@"
fi
