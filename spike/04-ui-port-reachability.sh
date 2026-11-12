#!/bin/bash
# Item 4: the relay (host) can reach port 4096 on the box over the internal
# network; another LAN device cannot.
#
# The LAN half cannot be fully automated from one machine. The script checks
# what it can (no published port, no IP forwarding) and prints the two commands
# to run from a second device on the same LAN.
source "$(dirname "$0")/lib.sh"

BOX=sc-spike-04
PORT=4096

cleanup() { box_rm "$BOX"; net_down; }
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

if [ "${1:-}" = hold ]; then
  trap - EXIT
  cat <<EOF

MANUAL: from another device on the LAN, both of these must fail:
  curl --max-time 5 http://$LAN_IP:$PORT/
  curl --max-time 5 http://$BOX_IP:$PORT/
     (optionally after: sudo route add -net ${BOX_IP%.*}.0/24 $LAN_IP)
Then clean up with: container rm -f $BOX && container network rm $NET
EOF
else
  note "LAN-device check is manual: run '$0 hold' and follow the printed commands"
fi

finish
