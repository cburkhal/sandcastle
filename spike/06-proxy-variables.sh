#!/bin/bash
# Item 6: tools that ignore proxy variables fail cleanly, and the ones we need
# can be made to honour them. Checks Node's fetch, npm, Git and Bun.
#
# Needs tinyproxy on the host and the spike image from item 5.
# "Fails cleanly" means: an error within a few seconds, not a hang. Each probe
# prints its exit code and duration.
source "$(dirname "$0")/lib.sh"

BOX=sc-spike-06
PROXY_PORT=18886
PROXY_PID=""

cleanup() {
  [ -n "$PROXY_PID" ] && kill "$PROXY_PID" 2>/dev/null || true
  box_rm "$BOX"
  net_down
}
trap cleanup EXIT
cleanup

net_up
GW="$(net_field ipv4Gateway)"; SUBNET="$(net_field ipv4Subnet)"
PROXY="http://$GW:$PROXY_PORT"
container run -d --name "$BOX" --network "$NET" --no-dns --read-only --tmpfs /tmp \
  -e HOME=/tmp/home -e npm_config_cache=/tmp/npm-cache \
  "$SPIKE_IMAGE" sleep 3600 >/dev/null
start_proxy "$GW" "$PROXY_PORT" "$SUBNET" '^registry\.npmjs\.org$' '^github\.com$' '^codeload\.github\.com$' ||
  { fail "tinyproxy did not start"; finish; exit; }

# probe "description" expected(ok|fail) [-e VAR=VALUE ...] -- command...
probe() {
  local d="$1" want="$2"; shift 2
  local envs=()
  while [ "$1" != -- ]; do envs+=("$1"); shift; done
  shift
  local t0 rc=0 t1
  t0="$(date +%s)"
  container exec ${envs[@]+"${envs[@]}"} "$BOX" timeout 40 "$@" >/dev/null 2>&1 || rc=$?
  t1="$(date +%s)"
  local got=ok; [ "$rc" -eq 0 ] || got=fail
  local msg="$d (exit $rc, $((t1 - t0))s)"
  if [ "$got" != "$want" ]; then fail "$msg, expected $want"
  elif [ "$rc" -eq 124 ]; then fail "$msg: hung until the timeout, not a clean failure"
  else pass "$msg"; fi
}
P=(-e "HTTP_PROXY=$PROXY" -e "HTTPS_PROXY=$PROXY" -e "http_proxy=$PROXY" -e "https_proxy=$PROXY")
FETCH='fetch("https://registry.npmjs.org/-/ping").then(r=>process.exit(r.ok?0:1),()=>process.exit(1))'

step "Node built-in fetch"
probe "node fetch, no proxy variables: fails" fail -- node -e "$FETCH"
probe "node fetch, proxy variables only: fails (ignores them)" fail "${P[@]}" -- node -e "$FETCH"
probe "node fetch, proxy variables + NODE_USE_ENV_PROXY=1: works" ok "${P[@]}" -e NODE_USE_ENV_PROXY=1 -- node -e "$FETCH"

step "npm"
probe "npm ping, no proxy variables: fails" fail -- npm ping --fetch-retries=0
probe "npm ping, proxy variables: works" ok "${P[@]}" -- npm ping --fetch-retries=0

step "Git over HTTPS"
REPO=https://github.com/apple/container.git
probe "git ls-remote, no proxy variables: fails" fail -- git ls-remote "$REPO" HEAD
probe "git ls-remote, proxy variables: works" ok "${P[@]}" -- git ls-remote "$REPO" HEAD
probe "git ls-remote to a non-allowlisted host: fails" fail "${P[@]}" -- git ls-remote https://gitlab.com/gitlab-org/gitlab.git HEAD

step "Bun (OpenCode's runtime)"
# Standalone bun from npm, installed through the proxy into /tmp.
if container exec "${P[@]}" "$BOX" npm install --prefix /tmp/bun bun >/dev/null 2>&1; then
  BUN=/tmp/bun/node_modules/.bin/bun
  note "bun version: $(container exec "$BOX" $BUN --version 2>&1)"
  BFETCH='const r = await fetch("https://registry.npmjs.org/-/ping"); process.exit(r.ok ? 0 : 1)'
  probe "bun fetch, no proxy variables: fails" fail -- $BUN -e "$BFETCH"
  probe "bun fetch, proxy variables: works" ok "${P[@]}" -- $BUN -e "$BFETCH"
  probe "bun add (package install), proxy variables: works" ok "${P[@]}" -- \
    bash -c "mkdir -p /tmp/bunproj && cd /tmp/bunproj && $BUN init -y && $BUN add is-number"
else
  fail "could not install bun through the proxy (its postinstall may need another host; see proxy log)"
fi

step "Proxy log: refused hosts"
grep -Ei 'refused|filtered|denied' "$STATE_DIR/tinyproxy.log" | tail -20 || note "none"

finish
