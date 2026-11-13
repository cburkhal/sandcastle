#!/bin/bash
# End-to-end check of the real binary: starts a box for a throwaway repo and
# checks the security properties from CLAUDE.md section 5 from outside.
#
#   e2e/run.sh              without the firewall: needs no root, prompts for nothing
#   e2e/run.sh --firewall   with the pf rules: asks for sudo once at the start
#
# It uses its own HOME, so your config, your boxes and your vault are not
# touched: the LLM gateway is a local fake and the key comes from an
# environment variable. Needs the box image (`sandcastle init`) and, for the
# egress checks, internet access.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FIREWALL=0
[ "${1:-}" = "--firewall" ] && FIREWALL=1

FAILS=0
step() { printf '\n== %s\n' "$*"; }
pass() { printf 'PASS  %s\n' "$*"; }
fail() { printf 'FAIL  %s\n' "$*"; FAILS=$((FAILS + 1)); }
note() { printf 'NOTE  %s\n' "$*"; }
expect_ok() { local d="$1"; shift; if "$@" >/dev/null 2>&1; then pass "$d"; else fail "$d"; fi; }
expect_fail() { local d="$1"; shift; if "$@" >/dev/null 2>&1; then fail "$d"; else pass "$d"; fi; }
# expect_text "description" needle haystack
expect_text() { if printf '%s' "$3" | grep -qF -- "$2"; then pass "$1"; else fail "$1 (got: $(printf '%s' "$3" | head -c 300))"; fi; }
expect_no_text() { if printf '%s' "$3" | grep -qF -- "$2"; then fail "$1"; else pass "$1"; fi; }

step "Build"
(cd "$ROOT" && cargo build --release --quiet) || { echo "build failed"; exit 1; }
BIN="$ROOT/target/release/sandcastle"

WORK="$(mktemp -d)"
REAL_HOME="$HOME"
export HOME="$WORK/home"
REPO="$WORK/e2e-repo"
KEY="e2e-real-key-$RANDOM$RANDOM"
ANSWER="pineapple-$RANDOM"
SEEN="$WORK/gateway-seen.log"
UP_LOG="$WORK/up.log"
JAR="$WORK/cookies"
GATEWAY_PORT=$((20000 + RANDOM % 20000))
HOST_PORT=$((GATEWAY_PORT + 1))
UP_PID="" GATEWAY_PID="" LISTENER_PID="" BOX=""
mkdir -p "$HOME/.config/sandcastle" "$REPO"
: >"$SEEN"

cleanup() {
  [ -n "$UP_PID" ] && kill -INT "$UP_PID" 2>/dev/null && wait "$UP_PID" 2>/dev/null
  (cd "$REPO" && "$BIN" reset --yes >/dev/null 2>&1)
  [ -n "$GATEWAY_PID" ] && kill "$GATEWAY_PID" 2>/dev/null
  [ -n "$LISTENER_PID" ] && kill "$LISTENER_PID" 2>/dev/null
  wait 2>/dev/null
  rm -rf "$WORK"
}
trap cleanup EXIT

cat >"$HOME/.config/sandcastle/config.toml" <<CONFIG
[agent]
kind = "opencode"
[runtime]
kind = "apple-container"
[llm]
base_url = "http://127.0.0.1:$GATEWAY_PORT"
api_key  = "env://SANDCASTLE_E2E_KEY"
[egress]
allow = ["registry.npmjs.org"]
CONFIG

git -C "$REPO" init --quiet -b main
echo "tracked" >"$REPO/tracked.txt"
echo "SECRET=never-in-the-box" >"$REPO/.env"
git -C "$REPO" add tracked.txt
git -C "$REPO" -c user.name=e2e -c user.email=e2e@example.invalid commit --quiet -m "first"

python3 "$ROOT/e2e/fake_gateway.py" "$GATEWAY_PORT" "$ANSWER" "$SEEN" >"$WORK/gateway.log" 2>&1 &
GATEWAY_PID=$!
# A host service on all addresses: the box must not reach it once pf is loaded.
python3 -m http.server "$HOST_PORT" --bind 0.0.0.0 --directory "$WORK/home" >/dev/null 2>&1 &
LISTENER_PID=$!

step "sandcastle up"
UP_ARGS=(up --no-browser)
if [ "$FIREWALL" -eq 1 ]; then
  echo ">> macOS login password or Touch ID (sudo, to load the firewall rules for the test box)"
  sudo -p "   macOS password for %u: " -v || exit 1
else
  UP_ARGS+=(--no-firewall)
  note "running without the firewall; use --firewall to check the pf rules too"
fi
(cd "$REPO" && SANDCASTLE_E2E_KEY="$KEY" exec "$BIN" "${UP_ARGS[@]}") >"$UP_LOG" 2>&1 &
UP_PID=$!
LINK=""
for _ in $(seq 1 240); do
  LINK="$(grep -o 'http://127\.0\.0\.1:[0-9]*/[^ ]*' "$UP_LOG" | head -1)"
  [ -n "$LINK" ] && break
  kill -0 "$UP_PID" 2>/dev/null || break
  sleep 0.5
done
if [ -z "$LINK" ]; then
  fail "up printed a login link"; cat "$UP_LOG"; exit 1
fi
pass "up printed a login link"
UI="$(printf '%s' "$LINK" | grep -o 'http://127\.0\.0\.1:[0-9]*')"
UI_PORT="${UI##*:}"
BOX="$(cd "$REPO" && "$BIN" ls | awk '$1 ~ /^sc-/ {print $1}')"
box() { container exec "$BOX" "$@"; }
status() { curl -s -o /dev/null -w '%{http_code}' --max-time 10 "$@"; }

step "UI gateway"
expect_text "no cookie: refused" 401 "$(status "$UI/")"
expect_text "login link with a foreign Host: refused" 403 "$(status -H "Host: evil.example:$UI_PORT" "$LINK")"
status -c "$JAR" "$LINK" >/dev/null
expect_ok "login link sets the session cookie" grep -q sandcastle_session "$JAR"
expect_text "login link works only once" 403 "$(status "$LINK")"
expect_text "with the cookie: UI answers" 200 "$(status -b "$JAR" "$UI/")"
expect_text "with the cookie: API answers (relay adds the agent's password)" 200 "$(status -b "$JAR" "$UI/api/session")"
expect_text "foreign Origin: refused" 403 "$(status -b "$JAR" -H 'Origin: https://evil.example' "$UI/")"
expect_text "foreign Host (DNS rebinding): refused" 403 "$(status -b "$JAR" -H "Host: evil.example:$UI_PORT" "$UI/")"
BOX_IP="$(container inspect "$BOX" | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["status"]["networks"][0]["ipv4Address"].split("/")[0])')"
expect_text "agent API without the relay: 401" 401 "$(status "http://$BOX_IP:4096/api/session")"
expect_fail "no port published on 127.0.0.1:4096" nc -z -G 2 127.0.0.1 4096

step "Box hardening"
expect_text "runs as uid 1001" 1001 "$(box id -u)"
expect_fail "root filesystem is read-only" box touch /usr/local/bin/x
expect_text "no capabilities" "0000000000000000" "$(box grep CapEff /proc/self/status)"
expect_fail "config mount is read-only" box touch /etc/sandcastle/x
ENVIRON="$(box env)"
expect_no_text "real key is not in the box environment" "$KEY" "$ENVIRON"
expect_no_text "real key is not in the agent config" "$KEY" "$(box cat /etc/sandcastle/opencode.json)"

step "Clone"
expect_ok "tracked file is in the box's clone" box test -f /workspace/repo/tracked.txt
expect_fail "untracked .env is not in the box" box test -e /workspace/repo/.env
expect_fail "source mount is read-only" box touch /source.git/x

step "Egress"
expect_ok "allowlisted host through the proxy (npm)" box npm view left-pad version
expect_fail "other host is refused" box timeout 30 node -e 'fetch("https://example.com").then(()=>process.exit(0),()=>process.exit(1))'
expect_fail "direct connection, no proxy: no route out" box timeout 8 bash -c 'exec 3<>/dev/tcp/1.1.1.1/443'
GATEWAY_IP="$(box bash -c 'echo ${NO_PROXY}')"
expect_fail "proxy refuses CONNECT to a host address" box timeout 10 bash -c \
  "exec 3<>/dev/tcp/$GATEWAY_IP/4181 && printf 'CONNECT 127.0.0.1:443 HTTP/1.1\r\nHost: 127.0.0.1:443\r\n\r\n' >&3 && head -1 <&3 | grep -q ' 200'"
RELAY_LOG="$(cat "$HOME/Library/Application Support/sandcastle/boxes/$BOX/relay.log")"
expect_text "refused host is in the relay log" example.com "$RELAY_LOG"
if [ "$FIREWALL" -eq 1 ]; then
  expect_fail "pf: box cannot reach another service on the Mac" box timeout 8 bash -c "exec 3<>/dev/tcp/$GATEWAY_IP/$HOST_PORT"
  expect_ok "pf: rules are loaded for the box" sudo -n pfctl -a "sandcastle/$BOX" -sr
else
  note "pf checks skipped (no --firewall)"
fi

step "LLM relay"
MODELS="$(box bash -c 'cd /workspace/repo && opencode models' 2>&1)"
expect_text "gateway model is offered" "relay/e2e-model" "$MODELS"
expect_text "only gateway models are offered" 0 "$(printf '%s\n' "$MODELS" | grep -c -v '^relay/')"
REPLY="$(box bash -c 'cd /workspace/repo && timeout 120 opencode run -m relay/e2e-model "Say the word."' 2>&1)"
expect_text "opencode answers through the relay" "$ANSWER" "$REPLY"
expect_ok "gateway saw the real key" grep -q "POST /v1/chat/completions Bearer $KEY" "$SEEN"
expect_fail "gateway never saw the placeholder key" grep -q "sandcastle-placeholder" "$SEEN"
expect_no_text "real key is not in the relay log" "$KEY" "$(cat "$HOME/Library/Application Support/sandcastle/boxes/$BOX/relay.log")"
expect_no_text "real key is not in up's output" "$KEY" "$(cat "$UP_LOG")"
expect_fail "relay forwards only /v1/" box timeout 10 node -e \
  "fetch('http://$GATEWAY_IP:4180/other').then(r=>process.exit(r.ok?0:1),()=>process.exit(1))"

step "Fetch"
box bash -c 'cd /workspace/repo && git checkout --quiet -b agent-work && echo change >agent.txt && git add agent.txt && git commit --quiet -m "agent commit" \
  && printf "#!/bin/sh\ntouch /tmp/sandcastle-e2e-hook-ran\n" >.git/hooks/post-checkout && chmod +x .git/hooks/post-checkout \
  && git config core.fsmonitor "touch /tmp/sandcastle-e2e-fsmonitor-ran"'
HEAD_BEFORE="$(git -C "$REPO" rev-parse HEAD)"
expect_ok "sandcastle fetch succeeds" bash -c "cd '$REPO' && '$BIN' fetch"
expect_text "the agent's commit arrives as sandcastle/agent-work" "agent commit" "$(git -C "$REPO" log -1 --format=%s sandcastle/agent-work 2>&1)"
expect_text "host HEAD is unchanged" "$HEAD_BEFORE" "$(git -C "$REPO" rev-parse HEAD)"
expect_fail "host working tree is untouched" test -e "$REPO/agent.txt"
expect_fail "the box's hook did not reach the host repo" test -e "$REPO/.git/hooks/post-checkout"
expect_fail "the box's core.fsmonitor did not reach the host repo" git -C "$REPO" config --get core.fsmonitor

step "Stop and reset"
kill -INT "$UP_PID"; wait "$UP_PID" 2>/dev/null; UP_PID=""
expect_fail "Ctrl-C removes the box" container inspect "$BOX"
expect_fail "UI port is closed" nc -z -G 2 127.0.0.1 "$UI_PORT"
[ "$FIREWALL" -eq 1 ] && expect_text "pf: rules are gone" 0 "$(sudo -n pfctl -a "sandcastle/$BOX" -sr 2>/dev/null | wc -l | tr -d ' ')"
expect_ok "volumes survive a stop" bash -c "container volume list --quiet | grep -q '^$BOX'"
expect_ok "reset succeeds" bash -c "cd '$REPO' && '$BIN' reset --yes"
expect_fail "reset removes the volumes" bash -c "container volume list --quiet | grep -q '^$BOX'"
expect_fail "reset removes the network" bash -c "container network list --quiet | grep -q '^$BOX'"
expect_no_text "nothing was written to the real home" "$BOX" "$(ls "$REAL_HOME/Library/Application Support/sandcastle/boxes" 2>/dev/null)"

printf '\n%d failed check(s)\n' "$FAILS"
[ "$FAILS" -eq 0 ]
