#!/bin/bash
# Item 3: with --no-dns and HTTPS_PROXY, npm installs through a stock
# allowlisting proxy (tinyproxy) and OpenCode reaches LiteLLM through a plain
# HTTP forwarder.
#
# OpenCode itself is no longer installed from npm: 2.x is only published as a
# GitHub release and is baked into the spike image (see Containerfile). The npm
# check stays because plugins, MCP servers and project dependencies use it.
# The first run of this item (2026-11-12) installed opencode-ai 1.19.5 from npm
# through the proxy; see NOTES.md.
#
# Needs on the host: tinyproxy (brew install tinyproxy).
# LLM part, pick one (never commit hostnames, keys or item IDs):
#   BW_ITEM=<item name or id>   read from Bitwarden: URL from the item's website
#                               field, key from its password field. Unlocks the
#                               vault for this run only if it is locked.
#   LITELLM_BASE_URL=https://<litellm-host> LITELLM_KEY=...   pass them directly
# Without either, the LLM part is skipped.
#   OPENCODE_MODEL=<id>         optional: adds an `opencode run` round trip.
#                               Without it the script lists the model ids.
source "$(dirname "$0")/lib.sh"

BOX=sc-spike-03
PROXY_PORT=18883
LLM_PORT=18084
PROXY_PID=""; LLM_PID=""

cleanup() {
  [ -n "$PROXY_PID" ] && kill "$PROXY_PID" 2>/dev/null || true
  [ -n "$LLM_PID" ] && kill "$LLM_PID" 2>/dev/null || true
  box_rm "$BOX"
  net_down
}
trap cleanup EXIT
cleanup

step "Box with --no-dns on the internal network"
ensure_image
net_up
GW="$(net_field ipv4Gateway)"; SUBNET="$(net_field ipv4Subnet)"
# Items 1 and 2: the gateway address only exists once a box is running, so the
# box starts first and the proxy binds afterwards.
container run -d --name "$BOX" --network "$NET" --no-dns --tmpfs /tmp \
  -e "HTTP_PROXY=http://$GW:$PROXY_PORT" -e "HTTPS_PROXY=http://$GW:$PROXY_PORT" \
  -e "http_proxy=http://$GW:$PROXY_PORT" -e "https_proxy=http://$GW:$PROXY_PORT" \
  -e "NO_PROXY=$GW" -e "no_proxy=$GW" \
  -e NODE_USE_ENV_PROXY=1 -e HOME=/tmp/home -e npm_config_cache=/tmp/npm-cache \
  "$SPIKE_IMAGE" sleep 3600 >/dev/null
note "box resolv.conf with --no-dns:"; container exec "$BOX" cat /etc/resolv.conf 2>&1 || true

step "tinyproxy on the gateway address, allowlist: registry.npmjs.org"
if start_proxy "$GW" "$PROXY_PORT" "$SUBNET" '^registry\.npmjs\.org$'; then
  pass "tinyproxy bound to $GW:$PROXY_PORT"
else
  fail "tinyproxy did not start"; finish; exit
fi

step "npm installs a package through the proxy"
if container exec "$BOX" npm install --prefix /tmp/pkg is-number; then
  pass "npm install through the proxy"
else
  fail "npm install through the proxy"
fi
note "OpenCode in the image: $(container exec "$BOX" opencode --version 2>&1)"
expect_fail "non-allowlisted host (example.com) is refused by the proxy" \
  container exec "$BOX" node -e 'fetch("https://example.com").then(()=>process.exit(0),()=>process.exit(1))'
note "proxy log:"; grep -E 'CONNECT|Established|refused|filtered' "$STATE_DIR/tinyproxy.log" | tail -20 || true

step "LiteLLM through a plain HTTP forwarder on the gateway address"
if [ -n "${BW_ITEM:-}" ]; then
  # Secrets stay in shell variables of this process: never printed, never written.
  BW_STATE="$(bw status 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin)["status"])')"
  if [ "$BW_STATE" = unauthenticated ]; then
    fail "bw is not logged in: run 'bw login' first"; finish; exit
  fi
  if [ "$BW_STATE" = locked ] && [ -z "${BW_SESSION:-}" ]; then
    BW_SESSION="$(bw unlock --raw </dev/tty)" || { fail "bw unlock failed"; finish; exit; }
    export BW_SESSION
  fi
  ITEM_JSON="$(bw get item "$BW_ITEM")" || { fail "bw could not read item '$BW_ITEM'"; finish; exit; }
  LITELLM_BASE_URL="$(printf '%s' "$ITEM_JSON" | python3 -c 'import json,sys; print(json.load(sys.stdin)["login"]["uris"][0]["uri"])')"
  LITELLM_KEY="$(printf '%s' "$ITEM_JSON" | python3 -c 'import json,sys; print(json.load(sys.stdin)["login"]["password"])')"
  unset ITEM_JSON
  [ -n "$LITELLM_BASE_URL" ] && [ -n "$LITELLM_KEY" ] && pass "URL and key read from Bitwarden item '$BW_ITEM' (not shown)" ||
    { fail "Bitwarden item '$BW_ITEM' has no website or no password"; finish; exit; }
  export LITELLM_BASE_URL LITELLM_KEY
fi
if [ -z "${LITELLM_BASE_URL:-}" ] || [ -z "${LITELLM_KEY:-}" ]; then
  note "no BW_ITEM and no LITELLM_BASE_URL / LITELLM_KEY: LLM part SKIPPED"
  finish; exit
fi
python3 "$SPIKE_DIR/lib/llm_forward.py" "$GW" "$LLM_PORT" >"$STATE_DIR/llm.log" 2>&1 &
LLM_PID=$!
sleep 1
expect_ok "box lists models via http://$GW:$LLM_PORT/v1/models with a dummy key" \
  container exec "$BOX" node -e "
    fetch('http://$GW:$LLM_PORT/v1/models', {headers: {authorization: 'Bearer dummy'}})
      .then(r => r.json()).then(j => process.exit(j.data && j.data.length ? 0 : 1), () => process.exit(1))"

if [ -n "${OPENCODE_MODEL:-}" ]; then
  # Config shape taken from the OpenCode docs for OpenAI-compatible providers;
  # unverified against OpenCode 2.x. The provider package is fetched from
  # npm at first start, which is itself part of what this item tests.
  container exec "$BOX" bash -c "mkdir -p /tmp/work && cat >/tmp/opencode.json" <<EOF
{
  "\$schema": "https://opencode.ai/config.json",
  "permission": {"*": "allow"},
  "provider": {
    "relay": {
      "npm": "@ai-sdk/openai-compatible",
      "options": {"baseURL": "http://$GW:$LLM_PORT/v1", "apiKey": "dummy"},
      "models": {"$OPENCODE_MODEL": {}}
    }
  }
}
EOF
  if container exec -e OPENCODE_CONFIG=/tmp/opencode.json -w /tmp/work "$BOX" \
    timeout 120 opencode run --standalone --auto -m "relay/$OPENCODE_MODEL" "Reply with the single word: pong"; then
    pass "opencode run got an answer through the forwarder"
  else
    fail "opencode run through the forwarder"
  fi
  note "proxy log after opencode run (hosts OpenCode wanted):"
  grep -E 'CONNECT|refused|filtered' "$STATE_DIR/tinyproxy.log" | tail -30 || true
else
  note "OPENCODE_MODEL not set: 'opencode run' check SKIPPED. Model ids the gateway offers:"
  container exec "$BOX" node -e "
    fetch('http://$GW:$LLM_PORT/v1/models', {headers: {authorization: 'Bearer dummy'}})
      .then(r => r.json()).then(j => console.log(j.data.map(m => '  ' + m.id).join('\\n')))" 2>/dev/null || true
fi
note "forwarder log:"; cat "$STATE_DIR/llm.log"

finish
