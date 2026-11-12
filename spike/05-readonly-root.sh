#!/bin/bash
# Item 5: OpenCode runs with --read-only, tmpfs /tmp, a home volume and a
# workspace volume.
#
# OpenCode 2.x has no `opencode web`; the web UI and API come from
# `opencode serve`. Builds the spike image if missing. The box has no proxy
# here, so anything OpenCode tries to download at start fails; the box log is
# printed so those attempts end up in the notes.
source "$(dirname "$0")/lib.sh"

BOX=sc-spike-05
HOME_VOL=sc-spike-05-home
WS_VOL=sc-spike-05-ws
PORT=4096
PASSWORD="spike-$RANDOM$RANDOM"

cleanup() {
  box_rm "$BOX"
  container volume rm "$HOME_VOL" "$WS_VOL" >/dev/null 2>&1 || true
  net_down
}
trap cleanup EXIT
cleanup

step "Build the spike image if missing"
ensure_image
note "OpenCode: $(container run --rm --tmpfs /tmp -e HOME=/tmp/h "$SPIKE_IMAGE" opencode --version 2>&1 | tail -1)"

step "Start opencode serve with the hardening flags from CLAUDE.md 2.3"
net_up
volume_init "$HOME_VOL" "$WS_VOL"   # fresh volumes are root-owned; see lib.sh
container run -d --name "$BOX" --network "$NET" --no-dns \
  --read-only --tmpfs /tmp --cap-drop ALL -c 2 -m 2G \
  -v "$HOME_VOL:/home/agent" -v "$WS_VOL:/workspace" \
  -e "OPENCODE_SERVER_PASSWORD=$PASSWORD" \
  "$SPIKE_IMAGE" opencode serve --hostname 0.0.0.0 --port "$PORT" --print-logs >/dev/null
BOX_IP="$(box_ip "$BOX")"
note "box address: $BOX_IP, user: $(container exec "$BOX" id)"

step "Web UI answers; the API needs the password"
code() { curl -s -o /dev/null -w '%{http_code}' --max-time 3 "$@" || true; }
CODE=000
for _ in $(seq 1 30); do
  CODE="$(code "http://$BOX_IP:$PORT/")"
  [ "$CODE" = 200 ] && break
  sleep 2
done
[ "$CODE" = 200 ] && pass "web UI shell answers 200 on /" || fail "web UI did not answer 200 within 60s (last code $CODE)"
note "the UI shell on / is served without authentication (static page); the API is what must be protected"
NOAUTH="$(code "http://$BOX_IP:$PORT/api/health")"
[ "$NOAUTH" = 401 ] && pass "API answers 401 without credentials" || fail "API without credentials answered $NOAUTH, expected 401"
WRONG="$(code -u "opencode:wrong" "http://$BOX_IP:$PORT/api/health")"
[ "$WRONG" = 401 ] && pass "API answers 401 with a wrong password" || fail "API with a wrong password answered $WRONG, expected 401"
AUTH="$(code -u "opencode:$PASSWORD" "http://$BOX_IP:$PORT/api/health")"
[ "$AUTH" != 401 ] && [ "$AUTH" != 000 ] && pass "API accepts Basic Auth with OPENCODE_SERVER_PASSWORD (status $AUTH)" ||
  fail "API rejected the password (status $AUTH)"

step "Filesystem: only /tmp and the two volumes are writable"
expect_fail "root filesystem is read-only (/usr/local)" container exec "$BOX" touch /usr/local/x
expect_fail "root filesystem is read-only (/etc)" container exec "$BOX" touch /etc/x
expect_ok "/tmp is writable" container exec "$BOX" touch /tmp/x
expect_ok "home volume is writable" container exec "$BOX" touch /home/agent/x
expect_ok "workspace volume is writable" container exec "$BOX" touch /workspace/x
note "what OpenCode wrote into home:"; container exec "$BOX" find /home/agent -maxdepth 3 -not -name x 2>/dev/null | head -40 || true
note "capabilities:"; container exec "$BOX" grep Cap /proc/self/status || true

step "Box log (look for failed downloads and write errors)"
container logs "$BOX" 2>&1 | grep -v "Sent HTTP response" | tail -40 || true

finish
