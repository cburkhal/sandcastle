#!/bin/bash
# Shared helpers for the spike scripts. Source it, do not run it.
# Every `container` flag used by the spike lives in these scripts on purpose:
# what works here becomes src/runtime/apple_container.rs later.
set -euo pipefail

SPIKE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
STATE_DIR="$SPIKE_DIR/.state"
mkdir -p "$STATE_DIR"
# Keep a copy of every run in .state/<script>.log so results can be read afterwards.
exec > >(tee "$STATE_DIR/$(basename "$0" .sh).log") 2>&1

NET="${SPIKE_NET:-sc-spike}"
# Spike only: tags, not digests. The real image is pinned by digest (CLAUDE.md 2.3).
BASE_IMAGE="${SPIKE_BASE_IMAGE:-docker.io/library/debian:stable-slim}"
NODE_IMAGE="${SPIKE_NODE_IMAGE:-docker.io/library/node:22-bookworm-slim}"
SPIKE_IMAGE="${SPIKE_IMAGE:-sc-spike-opencode:dev}"
BOX_UID=1001

FAILS=0
step() { printf '\n== %s\n' "$*"; }
pass() { printf 'PASS  %s\n' "$*"; }
fail() { printf 'FAIL  %s\n' "$*"; FAILS=$((FAILS + 1)); }
note() { printf 'NOTE  %s\n' "$*"; }
finish() {
  printf '\n%s: %d failed check(s)\n' "$(basename "$0")" "$FAILS"
  [ "$FAILS" -eq 0 ]
}

# expect_ok "description" cmd...   /   expect_fail "description" cmd...
expect_ok() {
  local d="$1"; shift
  if "$@" >/dev/null 2>&1; then pass "$d"; else fail "$d"; fi
}
expect_fail() {
  local d="$1"; shift
  if "$@" >/dev/null 2>&1; then fail "$d"; else pass "$d"; fi
}
# observe "description" cmd...  : no verdict, just records the outcome.
observe() {
  local d="$1"; shift
  if "$@" >/dev/null 2>&1; then note "$d: yes"; else note "$d: no"; fi
}

# --- password prompts ----------------------------------------------------
# Two different passwords are asked for in the spike. Each prompt says which
# one it is and what it is needed for.
ask() { printf '\n>> %s\n   Needed to: %s\n' "$1" "$2" >/dev/tty; }

# need_sudo "reason": make sure sudo is usable, asking for the macOS password if not.
need_sudo() {
  sudo -n true 2>/dev/null && return 0
  ask "macOS login password (sudo, admin rights on this Mac)" "$1"
  sudo -p "   macOS password for %u: " -v
}

# bw_unlock "reason": prints a Bitwarden session key; asks for the master password.
bw_unlock() {
  ask "Bitwarden MASTER password (unlocks your vault for this run only)" "$1"
  bw unlock --raw </dev/tty
}

# --- network -----------------------------------------------------------
net_exists() { container network ls 2>/dev/null | awk 'NR>1{print $1}' | grep -qx "$NET"; }
net_up() { net_exists || container network create --internal "$NET" >/dev/null; }
net_down() { net_exists && container network rm "$NET" >/dev/null 2>&1 || true; }
net_field() {
  container network inspect "$NET" |
    python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["status"][sys.argv[1]])' "$1"
}
host_has_ip() { ifconfig | grep -q "inet $1 "; }
host_lan_ip() {
  local ifc
  ifc="$(route -n get default 2>/dev/null | awk '/interface:/{print $2}')"
  [ -n "$ifc" ] && ipconfig getifaddr "$ifc" 2>/dev/null || true
}
host_lan_router() { route -n get default 2>/dev/null | awk '/gateway:/{print $2}'; }

# --- boxes -------------------------------------------------------------
box_rm() { container rm -f "$1" >/dev/null 2>&1 || true; }
box_ip() { container exec "$1" hostname -I | awk '{print $1}'; }
# TCP connect from inside a box (needs bash + coreutils in the image).
box_tcp() { container exec "$1" timeout 5 bash -c "exec 3<>/dev/tcp/$2/$3"; }
# Minimal HTTP GET from inside a box, prints the whole response.
box_http() {
  container exec "$1" timeout 5 bash -c \
    "exec 3<>/dev/tcp/$2/$3 && printf 'GET / HTTP/1.0\r\nHost: x\r\n\r\n' >&3 && cat <&3"
}
host_tcp() { nc -z -G 3 "$1" "$2"; }

# Build the spike image (OpenCode from the GitHub release, see Containerfile) if missing.
ensure_image() {
  container image inspect "$SPIKE_IMAGE" >/dev/null 2>&1 && return 0
  container build -t "$SPIKE_IMAGE" -f "$SPIKE_DIR/Containerfile" "$SPIKE_DIR"
}

# New named volumes are owned by root (0:0, mode 755), so the non-root box user
# cannot write them. Hand them to the box user once, with a one-off root
# container that has no network and only CAP_CHOWN.
volume_init() {
  local v
  for v in "$@"; do
    container volume create "$v" >/dev/null 2>&1 || true   # may exist already
    container run --rm -u 0 --cap-drop ALL --cap-add CAP_CHOWN --network none -v "$v:/v" "$SPIKE_IMAGE" \
      chown "$BOX_UID:$BOX_UID" /v >/dev/null
  done
}

# --- host listener -----------------------------------------------------
# start_listener BIND PORT TOKEN -> sets LISTENER_PID; returns 1 if it cannot bind.
start_listener() {
  local log="$STATE_DIR/listener-$2.log"
  python3 "$SPIKE_DIR/lib/listener.py" "$1" "$2" "$3" >"$log" 2>&1 &
  LISTENER_PID=$!
  local i
  for i in 1 2 3 4 5 6 7 8 9 10; do
    kill -0 "$LISTENER_PID" 2>/dev/null || { cat "$log"; return 1; }
    grep -q '^listening' "$log" && return 0
    sleep 0.3
  done
  return 1
}

# --- stock allowlisting proxy (items 3 and 6) ----------------------------
# start_proxy BIND PORT SUBNET HOST_REGEX... -> sets PROXY_PID
start_proxy() {
  command -v tinyproxy >/dev/null || { echo "tinyproxy missing: brew install tinyproxy" >&2; return 1; }
  local bind="$1" port="$2" subnet="$3"; shift 3
  local conf="$STATE_DIR/tinyproxy.conf" filter="$STATE_DIR/tinyproxy.filter"
  printf '%s\n' "$@" >"$filter"
  cat >"$conf" <<EOF
Port $port
Listen $bind
Timeout 60
Allow $subnet
ConnectPort 443
Filter "$filter"
FilterDefaultDeny Yes
FilterExtended Yes
LogLevel Connect
LogFile "$STATE_DIR/tinyproxy.log"
EOF
  : >"$STATE_DIR/tinyproxy.log"
  tinyproxy -d -c "$conf" >/dev/null 2>&1 &
  PROXY_PID=$!
  sleep 1
  kill -0 "$PROXY_PID" 2>/dev/null
}
