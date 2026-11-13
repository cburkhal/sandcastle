#!/bin/bash
# Opens the app in a throwaway setup for testing by hand: its own home
# folder, a fake LLM gateway, a key from an environment variable, no
# firewall and no Bitwarden. Your config, boxes and vault are not touched.
#
#   e2e/ui-sandbox.sh
#
# In the app: Unlock (no password is asked), then + and choose the folder
# this script prints. Closing the app removes everything again.
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
(cd "$ROOT" && cargo build --quiet -p sandcastle-ui -p sandcastle) || exit 1

WORK="$(mktemp -d)"
export HOME="$WORK/home"
REPO="$WORK/demo-repo"
PORT=$((20000 + RANDOM % 20000))
mkdir -p "$HOME/.config/sandcastle" "$REPO"
cat >"$HOME/.config/sandcastle/config.toml" <<CONFIG
[agent]
kind = "opencode"
[runtime]
kind = "apple-container"
[llm]
base_url = "http://127.0.0.1:$PORT"
api_key  = "env://SANDCASTLE_E2E_KEY"
[egress]
allow = ["registry.npmjs.org"]
CONFIG
git -C "$REPO" init --quiet -b main
echo "hello" >"$REPO/README.md"
git -C "$REPO" add README.md
git -C "$REPO" -c user.name=demo -c user.email=demo@example.invalid commit --quiet -m "first"

python3 "$ROOT/e2e/fake_gateway.py" "$PORT" "Hello from the fake gateway." "$WORK/seen.log" >/dev/null 2>&1 &
GATEWAY_PID=$!
cleanup() {
  kill "$GATEWAY_PID" 2>/dev/null
  (cd "$REPO" && "$ROOT/target/debug/sandcastle" reset --yes >/dev/null 2>&1)
  rm -rf "$WORK"
}
trap cleanup EXIT

echo "Repository to add in the app (Cmd-Shift-G in the folder picker lets you paste it):"
echo "  $REPO"
SANDCASTLE_E2E_KEY="sandbox-key" "$ROOT/target/debug/sandcastle-ui" --no-firewall
