#!/bin/bash
# Item 8: a non-root user in the box can read the read-only .git mount and
# write the named volumes (UID mapping).
#
# Tries the image's own user (uid 1001) and an arbitrary uid, and records the
# ownership the box sees, because that decides whether the image needs an
# entrypoint that fixes ownership. Needs the spike image from item 5.
source "$(dirname "$0")/lib.sh"

BOX=sc-spike-08
HOME_VOL=sc-spike-08-home
WS_VOL=sc-spike-08-ws
HOST_REPO="$STATE_DIR/08-host-repo"

cleanup() {
  box_rm "$BOX"
  container volume rm "$HOME_VOL" "$WS_VOL" >/dev/null 2>&1 || true
  net_down
  rm -rf "$HOST_REPO"
}
trap cleanup EXIT
cleanup

git init -q -b main "$HOST_REPO"
git -C "$HOST_REPO" -c user.name=host -c user.email=host@example.invalid commit -q --allow-empty -m first
chmod -R go-rwx "$HOST_REPO/.git"   # worst case: host files readable by the owner only
note "host uid: $(id -u), host .git mode: $(stat -f '%Sp' "$HOST_REPO/.git")"

net_up
ensure_image
container volume create "$HOME_VOL" >/dev/null
container volume create "$WS_VOL" >/dev/null

check_as() {
  local user="$1" want_write="$2"
  step "As user $user"
  box_rm "$BOX"
  container run -d --name "$BOX" --network "$NET" --no-dns --read-only --tmpfs /tmp --cap-drop ALL \
    -u "$user" \
    --mount "type=bind,source=$HOST_REPO/.git,target=/source.git,readonly" \
    -v "$HOME_VOL:/home/agent" -v "$WS_VOL:/workspace" \
    "$SPIKE_IMAGE" sleep 3600 >/dev/null
  bx() { container exec "$BOX" "$@"; }
  note "id: $(bx id)"
  note "ownership seen in the box:"; bx stat -c '%n  %u:%g  %A' /source.git /source.git/HEAD /home/agent /workspace /tmp
  expect_ok "reads /source.git/HEAD" bx cat /source.git/HEAD
  expect_ok "reads objects in /source.git" bx bash -c 'ls /source.git/objects >/dev/null'
  expect_fail "cannot write /source.git" bx touch /source.git/x
  if [ "$want_write" = yes ]; then
    expect_ok "writes the home volume" bx touch "/home/agent/by-$user"
    expect_ok "writes the workspace volume" bx touch "/workspace/by-$user"
  else
    observe "writes the home volume (fresh, root-owned)" bx touch "/home/agent/by-$user"
    observe "writes the workspace volume (fresh, root-owned)" bx touch "/workspace/by-$user"
  fi
}

# Fresh volumes first: item 5 found they are root-owned.
check_as "$BOX_UID" no
# Then after the one-off chown used by the other scripts.
box_rm "$BOX"   # a volume cannot be attached to two running containers
volume_init "$HOME_VOL" "$WS_VOL"
check_as "$BOX_UID" yes
check_as 4242 no

step "Do files written to a volume keep their owner across boxes?"
container exec "$BOX" stat -c '%n  %u:%g' /workspace/by-"$BOX_UID" 2>&1 || true

finish
