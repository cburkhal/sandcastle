#!/bin/bash
# Item 5: OpenCode runs with --read-only, tmpfs /tmp, a home volume and a
# workspace volume.
#
# Builds the spike image first (needs OPENCODE_VERSION). The box has no proxy
# here, so anything OpenCode tries to download at start fails; the box log is
# printed so those attempts end up in the notes.
source "$(dirname "$0")/lib.sh"
: "${OPENCODE_VERSION:?set OPENCODE_VERSION to an exact opencode-ai version}"

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

step "Build the spike image"
container build --build-arg "OPENCODE_VERSION=$OPENCODE_VERSION" \
  -t "$SPIKE_IMAGE" -f "$SPIKE_DIR/Containerfile" "$SPIKE_DIR"

step "Start OpenCode web with the hardening flags from CLAUDE.md 2.3"
net_up
container volume create "$HOME_VOL" >/dev/null
container volume create "$WS_VOL" >/dev/null
container run -d --name "$BOX" --network "$NET" --no-dns \
  --read-only --tmpfs /tmp --cap-drop ALL -c 2 -m 2G \
  -v "$HOME_VOL:/home/agent" -v "$WS_VOL:/workspace" \
  -e "OPENCODE_SERVER_PASSWORD=$PASSWORD" \
  "$SPIKE_IMAGE" opencode web --hostname 0.0.0.0 --port "$PORT" >/dev/null
BOX_IP="$(box_ip "$BOX")"
note "box address: $BOX_IP, user: $(container exec "$BOX" id)"

step "UI answers, and only with the password"
CODE=000
for _ in $(seq 1 30); do
  CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 3 -u "opencode:$PASSWORD" "http://$BOX_IP:$PORT/" || true)"
  [ "$CODE" = 200 ] && break
  sleep 2
done
[ "$CODE" = 200 ] && pass "UI answers 200 with Basic Auth" || fail "UI did not answer 200 within 60s (last code $CODE)"
NOAUTH="$(curl -s -o /dev/null -w '%{http_code}' --max-time 3 "http://$BOX_IP:$PORT/" || true)"
[ "$NOAUTH" = 401 ] && pass "UI answers 401 without the password" || fail "UI without password answered $NOAUTH, expected 401"

step "Filesystem: only /tmp and the two volumes are writable"
expect_fail "root filesystem is read-only (/usr/local)" container exec "$BOX" touch /usr/local/x
expect_fail "root filesystem is read-only (/etc)" container exec "$BOX" touch /etc/x
expect_ok "/tmp is writable" container exec "$BOX" touch /tmp/x
expect_ok "home volume is writable" container exec "$BOX" touch /home/agent/x
expect_ok "workspace volume is writable" container exec "$BOX" touch /workspace/x
note "what OpenCode wrote into home:"; container exec "$BOX" find /home/agent -maxdepth 3 -not -name x | head -40
note "capabilities:"; container exec "$BOX" grep Cap /proc/self/status

step "Box log (look for failed downloads and write errors)"
container logs "$BOX" 2>&1 | tail -40

finish
