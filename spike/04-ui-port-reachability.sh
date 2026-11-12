#!/bin/bash
# Item 4: the relay (host) can reach port 4096 on the box over the internal
# network; another LAN device cannot.
#
# The LAN half cannot be fully automated from one machine. The script checks
# what it can (no published port, no IP forwarding) and prints the two commands
# to run from a second device on the same LAN.
#
#   ./04-ui-port-reachability.sh           automated checks, cleans up
#   ./04-ui-port-reachability.sh hold      leave the box running, no pf rules (baseline)
#   ./04-ui-port-reachability.sh hold-pf   leave the box running WITH the pf rules (needs sudo)
#   ./04-ui-port-reachability.sh clean     remove box, network and pf rules after a hold
source "$(dirname "$0")/lib.sh"

BOX=sc-spike-04
PORT=4096

ANCHOR=com.apple/sandcastle-spike
TOKENFILE="$STATE_DIR/04.pftoken"

cleanup() {
  if [ -f "$TOKENFILE" ]; then
    sudo pfctl -a "$ANCHOR" -F all >/dev/null 2>&1 || true
    sudo pfctl -X "$(cat "$TOKENFILE")" >/dev/null 2>&1 || true
    rm -f "$TOKENFILE"
  fi
  box_rm "$BOX"; net_down
}
if [ "${1:-}" = clean ]; then cleanup; echo "cleaned up"; exit 0; fi
trap cleanup EXIT
cleanup

step "Box serving on $PORT, nothing published"
net_up
container run -d --name "$BOX" --network "$NET" "$NODE_IMAGE" node -e "
  require('http').createServer((q, s) => s.end('sandcastle-spike-04\n')).listen($PORT, '0.0.0.0')" >/dev/null
sleep 2
BOX_IP="$(box_ip "$BOX")"
note "box address: $BOX_IP"

step "Host reaches the box directly over the internal network"
if curl -fsS --max-time 5 "http://$BOX_IP:$PORT/" | grep -q sandcastle-spike-04; then
  pass "host got the token from http://$BOX_IP:$PORT"
else
  fail "host cannot reach http://$BOX_IP:$PORT"
fi

step "Nothing listens for the box on the host's own addresses"
LAN_IP="$(host_lan_ip)"
expect_fail "127.0.0.1:$PORT closed" host_tcp 127.0.0.1 "$PORT"
[ -n "$LAN_IP" ] && expect_fail "LAN address $LAN_IP:$PORT closed" host_tcp "$LAN_IP" "$PORT"

step "Would the Mac route LAN traffic into the box network?"
FWD="$(sysctl -n net.inet.ip.forwarding)"
note "net.inet.ip.forwarding = $FWD (0 means the Mac does not route between LAN and vmnet)"
[ "$FWD" = 0 ] && pass "IP forwarding is off" || fail "IP forwarding is on: a LAN device with a static route could reach $BOX_IP"

case "${1:-}" in
hold | hold-pf)
  trap - EXIT
  if [ "$1" = hold-pf ]; then
    step "Load the pf rules from item 9 (sudo)"
    GW="$(net_field ipv4Gateway)"; SUBNET="$(net_field ipv4Subnet)"
    IFACE="$(ifconfig | awk -v ip="$GW" '/^[a-z0-9]+:/{i=$1; sub(":","",i)} $1=="inet" && $2==ip {print i}')"
    sudo pfctl -E 2>&1 | awk '/Token/{print $NF}' >"$TOKENFILE"
    printf '%s\n' \
      "pass out quick on $IFACE inet proto tcp from $GW to $SUBNET port $PORT keep state" \
      "block return in quick on $IFACE all" \
      "block return out quick on $IFACE all" | sudo pfctl -a "$ANCHOR" -f - 2>/dev/null
    sudo pfctl -a "$ANCHOR" -s rules 2>/dev/null
    if curl -fsS --max-time 5 "http://$BOX_IP:$PORT/" | grep -q sandcastle-spike-04; then
      pass "host still reaches the box with the rules loaded"
    else
      fail "host lost the box with the rules loaded"
    fi
  fi
  cat <<EOF

MANUAL: run these on ANOTHER device on the LAN, not on this Mac. All must fail:
  curl --max-time 5 http://$LAN_IP:$PORT/
  curl --max-time 5 http://$BOX_IP:$PORT/
  sudo route add -net ${BOX_IP%.*}.0/24 $LAN_IP     # macOS; Linux: sudo ip route add ${BOX_IP%.*}.0/24 via $LAN_IP
  curl --max-time 5 http://$BOX_IP:$PORT/           # the real test: now routed through the Mac
  sudo route delete -net ${BOX_IP%.*}.0/24          # Linux: sudo ip route del ${BOX_IP%.*}.0/24
On this Mac the second command succeeds by design (the host is allowed to reach the box).
Then clean up with: $0 clean
EOF
  ;;
*)
  note "LAN-device check is manual: run '$0 hold' (baseline) or '$0 hold-pf' (with pf rules)"
  ;;
esac

finish
