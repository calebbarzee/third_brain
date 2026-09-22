#!/usr/bin/env bash
# smoke test for `kb mcp`: pipes an `initialize` request and a `tools/list` request (newline-
# delimited json, one message per line) into the stdio mcp server and checks that the replies
# look right. design.md §29 step 6.
#
# usage:
#   scripts/mcp_smoke.sh [path-to-kb-binary] [repo-path]
#
# defaults: kb binary at ../target/debug/kb (relative to this script), repo path a fresh temp
# kb repository created for the run.
#
# to register `kb mcp` in an mcp client config (claude desktop, or any client that speaks mcp
# over stdio), point the client at this binary with the repo as an argument, for example:
#
#   {
#     "mcpServers": {
#       "kb": {
#         "command": "/path/to/kb",
#         "args": ["--repo", "/path/to/your/kb-repo", "mcp"]
#       }
#     }
#   }

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
kb_bin="${1:-"$script_dir/../target/debug/kb"}"
repo_path="${2:-}"

if [[ ! -x "$kb_bin" ]]; then
    echo "error: kb binary not found or not executable at $kb_bin" >&2
    echo "build it first: cargo build --manifest-path \"$script_dir/../Cargo.toml\"" >&2
    exit 1
fi

cleanup_repo=""
if [[ -z "$repo_path" ]]; then
    repo_path="$(mktemp -d)"
    cleanup_repo="$repo_path"
    "$kb_bin" init "$repo_path" >/dev/null
fi
cleanup() {
    if [[ -n "$cleanup_repo" ]]; then
        rm -rf "$cleanup_repo"
    fi
}
trap cleanup EXIT

request_file="$(mktemp)"
trap 'rm -f "$request_file"; cleanup' EXIT
printf '%s\n' \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
    '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
    '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
    > "$request_file"

reply="$("$kb_bin" --repo "$repo_path" mcp < "$request_file")"

echo "$reply"

if ! grep -q '"protocolVersion":"2025-06-18"' <<<"$reply"; then
    echo "FAIL: no initialize reply with protocolVersion 2025-06-18" >&2
    exit 1
fi
if ! grep -q '"tools":\[' <<<"$reply"; then
    echo "FAIL: no tools/list reply with a tools array" >&2
    exit 1
fi
for tool in search get refs propose run answer; do
    if ! grep -q "\"name\":\"$tool\"" <<<"$reply"; then
        echo "FAIL: tools/list did not list the \`$tool\` tool" >&2
        exit 1
    fi
done

echo "OK: initialize and tools/list both replied as expected"
