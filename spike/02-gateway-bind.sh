#!/bin/bash
# Item 2: a host listener can bind to the vmnet gateway IP only (not 0.0.0.0)
# and survives sleep/wake.
#
#   ./02-gateway-bind.sh          automated checks, cleans up afterwards
#   ./02-gateway-bind.sh hold     set up and leave box + listener running
#   (sleep the Mac, wake it)
#   ./02-gateway-bind.sh check    re-test the surviving listener, then clean up
source "$(dirname "$0")/lib.sh"

MODE="${1:-run}"
BOX=sc-spike-02
PORT=18082
TOKEN="sandcastle-spike-02"
PIDFILE="$STATE_DIR/02.pid"
LISTENER_PID=""

cleanup() {
  [ -f "$PIDFILE" ] && kill "$(cat "$PIDFILE")" 2>/dev/null || true
  rm -f "$PIDFILE"
  [ -n "$LISTENER_PID" ] && kill "$LISTENER_PID" 2>/dev/null || true
  box_rm "$BOX"
  net_down
}

box_gets_token() { box_http "$BOX" "$GW" "$PORT" 2>/dev/null | grep -q "$TOKEN"; }

reachability_checks() {
  expect_ok "box reaches the listener on $GW:$PORT" box_gets_token
  expect_fail "listener not reachable on 127.0.0.1:$PORT" host_tcp 127.0.0.1 "$PORT"
  local lan; lan="$(host_lan_ip)"
  [ -n "$lan" ] && expect_fail "listener not reachable on LAN address $lan:$PORT" host_tcp "$lan" "$PORT"
  note "listening sockets on port $PORT:"; lsof -nP -iTCP:"$PORT" -sTCP:LISTEN || true
}

if [ "$MODE" = check ]; then
  step "After sleep/wake: is the listener from 'hold' still serving?"
  [ -f "$PIDFILE" ] || { echo "no held listener; run '$0 hold' first" >&2; exit 2; }
  GW="$(net_field ipv4Gateway)"
  expect_ok "listener process still alive" kill -0 "$(cat "$PIDFILE")"
  expect_ok "host still has the gateway address $GW" host_has_ip "$GW"
  reachability_checks
  cleanup
  finish; exit
fi

trap 'if [ "$MODE" != hold ]; then cleanup; fi' EXIT
cleanup

step "Bind before any box exists (order of steps 4 and 5 in 'sandcastle up')"
net_up
GW="$(net_field ipv4Gateway)"
note "network $NET: gateway $GW"
observe "host has $GW on an interface with the network created but no box running" host_has_ip "$GW"
if start_listener "$GW" "$PORT" "$TOKEN"; then
  note "bind to $GW:$PORT before the first box: works"
  kill "$LISTENER_PID" 2>/dev/null || true; wait "$LISTENER_PID" 2>/dev/null || true
else
  note "bind to $GW:$PORT before the first box: FAILS, so the relay must start after the box"
fi
LISTENER_PID=""

step "Bind to the gateway address only, with a box running"
container run -d --name "$BOX" --network "$NET" "$BASE_IMAGE" sleep 86400 >/dev/null
expect_ok "host has the gateway address $GW on an interface" host_has_ip "$GW"
if start_listener "$GW" "$PORT" "$TOKEN"; then
  pass "listener bound to $GW:$PORT"
else
  fail "listener could not bind to $GW:$PORT"
fi
reachability_checks

if [ "$MODE" = hold ]; then
  echo "$LISTENER_PID" >"$PIDFILE"
  disown "$LISTENER_PID" 2>/dev/null || true
  printf '\nHolding box %s and listener (pid %s).\nSleep the Mac, wake it, then run: %s check\n' \
    "$BOX" "$LISTENER_PID" "$0"
  finish; exit
fi

step "Does the listener survive the box stopping and starting again?"
container stop "$BOX" >/dev/null
sleep 2
observe "host still has $GW with the box stopped" host_has_ip "$GW"
observe "listener process alive with the box stopped" kill -0 "$LISTENER_PID"
container start "$BOX" >/dev/null
sleep 2
if box_gets_token; then
  pass "same listener serves the box again after a box restart"
else
  fail "listener is dead or unreachable after a box restart (relay would need to rebind)"
fi

note "sleep/wake not covered by this mode: run '$0 hold', sleep and wake the Mac, then '$0 check'"
finish
