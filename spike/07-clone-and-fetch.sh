#!/bin/bash
# Item 7: clone-and-fetch works. The box clones from a read-only mount of the
# host .git; the host fetches the box's commits through
# `container exec ... git upload-pack` into refs/remotes/sandcastle/*.
#
# Uses a throwaway host repo under spike/.state, never a real one.
# Needs the spike image from item 5.
source "$(dirname "$0")/lib.sh"

BOX=sc-spike-07
WS_VOL=sc-spike-07-ws
HOST_REPO="$STATE_DIR/07-host-repo"
G=(git -C "$HOST_REPO" -c user.name=host -c user.email=host@example.invalid)

cleanup() {
  box_rm "$BOX"
  container volume rm "$WS_VOL" >/dev/null 2>&1 || true
  net_down
  rm -rf "$HOST_REPO"
}
trap cleanup EXIT
cleanup

step "Throwaway host repo with one commit and an untracked secret"
git init -q -b main "$HOST_REPO"
echo one >"$HOST_REPO/file.txt"
echo "SECRET=do-not-leak" >"$HOST_REPO/.env"
echo .env >"$HOST_REPO/.gitignore"
"${G[@]}" add file.txt .gitignore
"${G[@]}" commit -q -m "host: first commit"

step "Box with the host .git mounted read-only and a workspace volume"
net_up
container volume create "$WS_VOL" >/dev/null
container run -d --name "$BOX" --network "$NET" --no-dns --read-only --tmpfs /tmp --cap-drop ALL \
  --mount "type=bind,source=$HOST_REPO/.git,target=/source.git,readonly" \
  -v "$WS_VOL:/workspace" -e HOME=/tmp/home \
  "$SPIKE_IMAGE" sleep 3600 >/dev/null
bx() { container exec "$BOX" "$@"; }
note "box user: $(bx id); /source.git owner: $(bx stat -c '%u:%g %A' /source.git)"

step "Seed: box clones from the read-only mount"
if bx git clone -q /source.git /workspace/repo; then
  pass "plain git clone from /source.git"
else
  note "plain clone failed (probably safe.directory, the mount has a foreign owner); retrying with safe.directory"
  bx git config --global --add safe.directory /source.git
  bx git config --global --add safe.directory /source.git/.git
  expect_ok "git clone from /source.git with safe.directory set" bx git clone -q /source.git /workspace/repo
fi
expect_fail "untracked .env did not travel into the box" bx test -e /workspace/repo/.env
expect_fail "source mount is not writable from the box" bx touch /source.git/hooks/post-checkout

step "Agent commits in the box (and plants a hook and a config key)"
bx bash -c 'cd /workspace/repo && git checkout -q -b agent/work && echo two >>file.txt &&
  git -c user.name=agent -c user.email=agent@example.invalid commit -q -am "box: agent commit" &&
  printf "#!/bin/sh\ntouch /tmp/hook-ran\n" >.git/hooks/post-checkout && chmod +x .git/hooks/post-checkout &&
  git config core.fsmonitor "touch /tmp/fsmonitor-ran"'
BOX_HEAD="$(bx git -C /workspace/repo rev-parse agent/work)"

step "Fetch: host pulls through container exec, upload-pack runs in the box"
# ext:: is off by default in Git; allow it for this one command only.
if "${G[@]}" -c protocol.ext.allow=always fetch \
  "ext::container exec -i $BOX git upload-pack /workspace/repo" \
  '+refs/heads/*:refs/remotes/sandcastle/*'; then
  pass "git fetch over ext::container exec"
else
  fail "git fetch over ext::container exec"
fi
HOST_SEES="$("${G[@]}" rev-parse refs/remotes/sandcastle/agent/work 2>/dev/null || true)"
[ "$HOST_SEES" = "$BOX_HEAD" ] && pass "refs/remotes/sandcastle/agent/work matches the box commit" ||
  fail "host ref '$HOST_SEES' does not match box commit '$BOX_HEAD'"
expect_ok "fetched objects pass fsck" "${G[@]}" fsck --no-dangling
expect_fail "no hook arrived in the host repo" test -e "$HOST_REPO/.git/hooks/post-checkout"
expect_fail "no core.fsmonitor arrived in the host config" "${G[@]}" config --get core.fsmonitor
[ "$("${G[@]}" rev-parse main)" = "$("${G[@]}" rev-parse HEAD)" ] && pass "host branch and working tree untouched"

step "Sync host to box: a new host commit is visible through the read-only mount"
echo three >>"$HOST_REPO/file.txt"
"${G[@]}" commit -q -am "host: second commit"
bx git -C /workspace/repo fetch -q origin
[ "$(bx git -C /workspace/repo rev-parse origin/main)" = "$("${G[@]}" rev-parse main)" ] &&
  pass "box sees the new host commit after git fetch origin" ||
  fail "box does not see the new host commit (stale mount?)"

finish
