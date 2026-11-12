#!/bin/bash
# Item 9 (added after item 1): a pf rule keeps the box away from every host
# service except the relay.
#
# Item 1 showed that a box on an internal network reaches anything on the Mac
# that listens on 0.0.0.0. This script checks whether pf sees vmnet traffic at
# all, and which rule set blocks box -> host while keeping box -> relay and
# host -> box:4096 working.
#
# NEEDS ROOT: it calls `sudo pfctl`. It does not edit /etc/pf.conf. Rules go
# into the anchor com.apple/sandcastle-spike (the stock pf.conf only evaluates
# anchors under com.apple/*) and are flushed on exit. pf is enabled with a
# reference token (-E) and released with -X, so a pf that was already on stays on.
source "$(dirname "$0")/lib.sh"

BOX=sc-spike-09
RELAY_PORT=18091      # stand-in for the relay, bound to the gateway address
OTHER_PORT=18092      # stand-in for "some dev server" on 0.0.0.0 / ::
UI_PORT=4096
ANCHOR=com.apple/sandcastle-spike
PF_TOKEN=""; RELAY_PID=""; OTHER_PID=""

cleanup() {
  sudo -n pfctl -a "$ANCHOR" -F all >/dev/null 2>&1 || true
  [ -n "$PF_TOKEN" ] && sudo -n pfctl -X "$PF_TOKEN" >/dev/null 2>&1 || true
  [ -n "$RELAY_PID" ] && kill "$RELAY_PID" 2>/dev/null || true
  [ -n "$OTHER_PID" ] && kill "$OTHER_PID" 2>/dev/null || true
  box_rm "$BOX"
  net_down
}
trap cleanup EXIT

echo "This script needs sudo for pfctl."
sudo -v
cleanup

# TCP connect from the box via node (handles IPv6 and scoped addresses).
box_connect() {
  container exec "$BOX" timeout 5 node -e '
    require("net").connect({host: process.argv[1], port: +process.argv[2]})
      .on("connect", () => process.exit(0)).on("error", () => process.exit(1))' "$1" "$2"
}
host_gets_ui() { curl -fsS --max-time 5 "http://$BOX_IP:$UI_PORT/" | grep -q sandcastle-spike-09; }
start_box() {
  box_rm "$BOX"
  container run -d --name "$BOX" --network "$NET" --no-dns "$NODE_IMAGE" node -e "
    require('http').createServer((q, s) => s.end('sandcastle-spike-09\n')).listen($UI_PORT, '0.0.0.0')" >/dev/null
  sleep 2
  BOX_IP="$(box_ip "$BOX")"
}
load_rules() {
  printf '%s\n' "$@" | sudo pfctl -a "$ANCHOR" -f - 2>&1 | grep -v 'ALTQ\|pf.conf' || true
  note "loaded rules:"; sudo pfctl -a "$ANCHOR" -s rules 2>/dev/null
}

step "Setup: box, relay stand-in on the gateway, another service on all addresses"
net_up
GW="$(net_field ipv4Gateway)"; SUBNET="$(net_field ipv4Subnet)"
start_box
IFACE="$(ifconfig | awk -v ip="$GW" '/^[a-z0-9]+:/{i=$1; sub(":","",i)} $1=="inet" && $2==ip {print i}')"
LAN_IP="$(host_lan_ip)"
note "gateway $GW on interface $IFACE, subnet $SUBNET, box $BOX_IP, host LAN $LAN_IP"
start_listener "$GW" "$RELAY_PORT" relay || fail "relay stand-in did not start"; RELAY_PID="$LISTENER_PID"
start_listener :: "$OTHER_PORT" other || fail "other service did not start"; OTHER_PID="$LISTENER_PID"
# Host IPv6 addresses on the box interface, rewritten to the box's scope.
V6_TARGETS="$(ifconfig "$IFACE" | awk '$1=="inet6"{a=$2; if (sub(/%.*/,"",a)) a=a "%eth0"; print a}')"
note "host IPv6 addresses as seen from the box: $(echo $V6_TARGETS)"
note "box addresses: $(container exec "$BOX" hostname -I)"

# probes WANT [WANT_V6]: relay and UI must always work; the other service must
# be open/blocked as given. WANT_V6=observe records IPv6 without a verdict.
probes() {
  local want="$1" want6="${2:-$1}" t w
  expect_ok "box reaches the relay on $GW:$RELAY_PORT" box_connect "$GW" "$RELAY_PORT"
  expect_ok "host reaches the box UI on $BOX_IP:$UI_PORT" host_gets_ui
  for t in "$GW" "$LAN_IP" $V6_TARGETS; do
    [ -n "$t" ] || continue
    w="$want"; case "$t" in *:*) w="$want6" ;; esac
    if [ "$w" = blocked ]; then expect_fail "box blocked from the other service on [$t]:$OTHER_PORT" box_connect "$t" "$OTHER_PORT"
    else observe "box reaches the other service on [$t]:$OTHER_PORT" box_connect "$t" "$OTHER_PORT"; fi
  done
}

step "Baseline without pf rules (the hole from item 1)"
probes open

step "Enable pf (reference-counted)"
note "pf before: $(sudo pfctl -s info 2>/dev/null | head -1)"
PF_TOKEN="$(sudo pfctl -E 2>&1 | awk '/Token/{print $NF}')"
[ -n "$PF_TOKEN" ] && pass "pf enabled, token $PF_TOKEN" || fail "pfctl -E returned no token"

step "Rule set A: by source subnet (IPv4 only), no explicit host -> box rule"
load_rules \
  "pass in quick inet proto tcp from $SUBNET to $GW port $RELAY_PORT" \
  "block return in quick inet from $SUBNET to any"
probes blocked observe
note "A does not filter IPv6 by design; B does"

step "Rule set B: by interface (IPv4 + IPv6), stateful host -> box rule"
load_rules \
  "pass in quick on $IFACE inet proto tcp from $SUBNET to $GW port $RELAY_PORT" \
  "pass out quick on $IFACE inet proto tcp from $GW to $SUBNET port $UI_PORT keep state" \
  "block return in quick on $IFACE all"
probes blocked

step "With rule set B loaded: does a box still start and get its address?"
start_box
[ -n "$BOX_IP" ] && pass "restarted box has address $BOX_IP" || fail "restarted box has no address (rule blocks DHCP/NDP?)"
NEW_IFACE="$(ifconfig | awk -v ip="$GW" '/^[a-z0-9]+:/{i=$1; sub(":","",i)} $1=="inet" && $2==ip {print i}')"
[ "$NEW_IFACE" = "$IFACE" ] && note "interface name unchanged after box restart: $IFACE" ||
  note "interface name CHANGED after box restart: $IFACE -> $NEW_IFACE (rules by interface name would go stale)"
probes blocked

step "After flushing the anchor the baseline is back"
sudo pfctl -a "$ANCHOR" -F all >/dev/null 2>&1
observe "box reaches the other service again on $GW:$OTHER_PORT" box_connect "$GW" "$OTHER_PORT"

finish
