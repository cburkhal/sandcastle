#!/bin/bash
# Item 1: a container on a --internal network can reach a listener on the
# host's address for that network, but not the internet.
#
# The listener binds 0.0.0.0 here on purpose: this item only asks whether the
# path box -> host exists. Binding to the gateway address alone is item 2.
source "$(dirname "$0")/lib.sh"

BOX=sc-spike-01
PORT=18081
TOKEN="sandcastle-spike-01"
LISTENER_PID=""

cleanup() {
  [ -n "$LISTENER_PID" ] && kill "$LISTENER_PID" 2>/dev/null || true
  box_rm "$BOX"
  net_down
}
trap cleanup EXIT
cleanup

step "Create internal network and start a box on it"
net_up
container network inspect "$NET"
GW="$(net_field ipv4Gateway)"
SUBNET="$(net_field ipv4Subnet)"
note "network $NET: gateway $GW, subnet $SUBNET"
container run -d --name "$BOX" --network "$NET" "$BASE_IMAGE" sleep 3600 >/dev/null
BOX_IP="$(box_ip "$BOX")"
note "box address: $BOX_IP"
note "box routes:"; container exec "$BOX" cat /proc/net/route
note "box resolv.conf:"; container exec "$BOX" cat /etc/resolv.conf 2>&1 || true
expect_ok "host has the gateway address $GW on an interface" host_has_ip "$GW"

step "Box reaches a host listener on the network's gateway address"
start_listener 0.0.0.0 "$PORT" "$TOKEN" || fail "listener did not start"
if box_http "$BOX" "$GW" "$PORT" 2>/dev/null | grep -q "$TOKEN"; then
  pass "box got the token from http://$GW:$PORT"
else
  fail "box could not fetch http://$GW:$PORT"
fi
note "listener log (source address the host sees):"; cat "$STATE_DIR/listener-$PORT.log"

step "Box cannot reach the internet"
expect_fail "TCP 1.1.1.1:443 blocked" box_tcp "$BOX" 1.1.1.1 443
expect_fail "TCP 8.8.8.8:53 blocked" box_tcp "$BOX" 8.8.8.8 53
expect_fail "TCP 140.82.121.4:443 (github.com) blocked" box_tcp "$BOX" 140.82.121.4 443
expect_fail "name resolution for example.com fails" container exec "$BOX" timeout 5 getent hosts example.com

step "Control: the same probe succeeds on the default (NAT) network"
if container run --rm "$BASE_IMAGE" timeout 5 bash -c 'exec 3<>/dev/tcp/1.1.1.1/443' >/dev/null 2>&1; then
  pass "probe reaches 1.1.1.1:443 from the default network, so the blocks above are real"
else
  fail "probe fails on the default network too: the internet checks above prove nothing"
fi

step "Observations: what else on the host and LAN can the box reach?"
LAN_IP="$(host_lan_ip)"; ROUTER="$(host_lan_router)"
DEFAULT_GW="$(container network inspect default | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["status"]["ipv4Gateway"])')"
[ -n "$LAN_IP" ] && observe "box reaches host LAN address $LAN_IP:$PORT (listener on 0.0.0.0)" box_tcp "$BOX" "$LAN_IP" "$PORT"
observe "box reaches default-network gateway $DEFAULT_GW:$PORT" box_tcp "$BOX" "$DEFAULT_GW" "$PORT"
[ -n "$ROUTER" ] && observe "box reaches LAN router $ROUTER:80" box_tcp "$BOX" "$ROUTER" 80
[ -n "$ROUTER" ] && observe "box reaches LAN router $ROUTER:443" box_tcp "$BOX" "$ROUTER" 443

finish
