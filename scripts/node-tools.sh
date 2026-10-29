#!/usr/bin/env bash
# Sourced by the optional Node.js-based scripts (MCP Inspector, Promptfoo agent eval).
# The tools come only from tools/package-lock.json through `npm ci`, which verifies every
# package against its recorded integrity hash; nothing is resolved at run time through npx.
# Node.js must match the major version pinned in .node-version (CI uses the exact version);
# npm must be exactly the packageManager version of tools/package.json, which CI installs.

# Usage: node_tools_prepare <project-root>; exports NODE_TOOLS_BIN.
node_tools_prepare() {
  local project_root="$1"
  local tools_dir="${project_root}/tools"
  local pinned_node
  pinned_node="$(tr -d '[:space:]' <"${project_root}/.node-version")"

  for command in node npm; do
    if ! command -v "${command}" >/dev/null 2>&1; then
      echo "${command} is required: install Node.js ${pinned_node} (pinned in .node-version)" >&2
      return 2
    fi
  done
  local active_node
  active_node="$(node --version)"
  active_node="${active_node#v}"
  if [[ "${active_node%%.*}" != "${pinned_node%%.*}" ]]; then
    echo "Node.js ${active_node} is not supported: install Node.js ${pinned_node} (pinned in .node-version)" >&2
    return 2
  fi
  if [[ "${active_node}" != "${pinned_node}" ]]; then
    echo "Note: Node.js ${active_node} differs from the pinned ${pinned_node}; CI uses the pinned version." >&2
  fi
  local pinned_npm active_npm
  pinned_npm="$(sed -n 's/^ *"packageManager": "npm@\([^"]*\)".*/\1/p' "${tools_dir}/package.json")"
  active_npm="$(npm --version)"
  if [[ -z "${pinned_npm}" || "${active_npm}" != "${pinned_npm}" ]]; then
    echo "npm ${active_npm} is not supported: install npm ${pinned_npm:-unknown} (packageManager in tools/package.json), for example with npm install -g npm@${pinned_npm:-unknown}" >&2
    return 2
  fi

  # npm records the installed tree in node_modules/.package-lock.json; reinstall whenever
  # the committed lockfile is newer or the tree is missing.
  local installed_lock="${tools_dir}/node_modules/.package-lock.json"
  if [[ ! -f "${installed_lock}" || "${tools_dir}/package-lock.json" -nt "${installed_lock}" ]]; then
    npm ci --prefix "${tools_dir}" --no-audit --no-fund >&2
  fi
  NODE_TOOLS_BIN="${tools_dir}/node_modules/.bin"
  export NODE_TOOLS_BIN
}
