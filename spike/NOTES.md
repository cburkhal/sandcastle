# Spike notes

Results of the manual spike from CLAUDE.md section 9. One script per checklist item.

Environment: macOS 27.0.1 (26A434), arm64, Apple `container` 1.7.1 (commit a15b460). Test images: `debian:stable-slim`, and from item 5 on the spike image with OpenCode 2.2.7 from the GitHub release.

| # | Item | Script | Status |
| --- | --- | --- | --- |
| 1 | Internal network: box reaches host listener, not the internet | `01-internal-network.sh` | **pass** (2026-11-12) |
| 2 | Host listener binds the gateway IP only; survives sleep/wake | `02-gateway-bind.sh` | **pass**, including sleep/wake (2026-11-12) |
| 3 | `--no-dns` + proxy: npm install via tinyproxy, LiteLLM via HTTP forwarder | `03-proxy-install-llm.sh` | **pass**, including an `opencode run` round trip (2026-11-12) |
| 4 | Host reaches box port 4096; LAN device cannot | `04-ui-port-reachability.sh` | host to box: **pass**. LAN isolation: **open**, IP forwarding is on (2026-11-12) |
| 5 | OpenCode with `--read-only`, tmpfs `/tmp`, home + workspace volumes | `05-readonly-root.sh` | **pass**, after a volume ownership fix (2026-11-12) |
| 6 | Proxy variables: Node fetch, npm, Git, Bun | `06-proxy-variables.sh` | **pass** (2026-11-12) |
| 7 | Clone from read-only `.git` mount; fetch via `container exec ... git upload-pack` | `07-clone-and-fetch.sh` | **pass** (2026-11-12) |
| 8 | Non-root user reads the `.git` mount and writes the volumes | `08-uid-mapping.sh` | **pass**, with the volume ownership fix (2026-11-12) |
| 9 | pf rule: box reaches only the relay on the host (added after item 1) | `09-pf-host-isolation.sh` | **pass** (2026-11-12, run by the owner, needs sudo) |

All nine scripts have run. Still open: the LAN-device half of item 4, with the pf rules loaded.

## Verdict on the fallback (items 1 and 2)

The host-listener design works. The two-network proxy container is not needed.

## Item 1: internal network

All checks passed.

- `container network create --internal sc-spike` gives mode `hostOnly`, here `192.168.128.0/24` with gateway `192.168.128.1`. The subnet is picked by `container`; read it from `network inspect`, do not hard-code it.
- The box (`192.168.128.2`) fetched a token from a host listener at `http://192.168.128.1:18081`. The host sees the box's own address as the source (no NAT), so the relay can check the peer address.
- From the box, TCP to `1.1.1.1:443`, `8.8.8.8:53` and a github.com address all failed, and so did name resolution. Control: the same probe from a box on the default network reached `1.1.1.1:443`, so the blocks are real.
- The LAN router (`192.168.1.1`, ports 80 and 443) is not reachable from the box.

Findings that affect the design:

1. **The box can reach the host's LAN address.** With the listener on `0.0.0.0`, the box connected to `192.168.1.248:18081`. So "internal" isolates the box from the internet and the LAN, but not from the Mac itself: every service on the Mac that listens on `0.0.0.0` (dev servers, AirPlay receiver, file sharing, anything else) is reachable from the box. **Decision (owner, 2026-11-12): close it with a pf rule.** Item 9 tests whether that works.
2. Without `--no-dns`, the box gets `nameserver 192.168.128.1` in `/etc/resolv.conf`. Lookups failed in this run, but `--no-dns` stays necessary so nothing depends on that.
3. The box has a default route via the gateway even on an internal network. Blocked traffic is dropped at the host, so probes time out (5 s here) instead of failing at once. Relevant for item 6 ("fail cleanly").

## Item 2: bind to the gateway address only

All automated checks passed.

- A listener bound to `192.168.128.1:18082` only. `lsof` shows that single socket; it is not reachable on `127.0.0.1` or on the LAN address, and the box reaches it.

Findings that affect the design:

1. **The gateway address exists on the host only while a box is running on that network.** With the network created but no box, the bind fails with `EADDRNOTAVAIL` ("Can't assign requested address"). The `up` order in CLAUDE.md section 4 (step 4 start relay, step 5 `container run`) cannot work as written: the box has to start first, then the relay binds the box-side address. Consequence: for a moment the box runs with no relay, so OpenCode must tolerate its proxy and LLM endpoint being absent at start, or the entrypoint must wait for the relay.
2. When the box stops, the gateway address disappears from the host. The listener process and its socket survived a stop/start of the box and served the box again without rebinding. Tested once, with a stop of about 2 seconds; the relay should still be written to rebind on error.
3. The gateway and subnet were the same (`192.168.128.1`) across two create/delete cycles of the network. Not a guarantee.

The owner re-ran the default mode on 2026-11-12 with the same result (0 failed checks). That run does not cover sleep/wake.

### Sleep/wake

Passed. The owner ran `hold`, slept and woke the Mac, then `check` on 2026-11-12: the listener was still bound to the gateway address and the box reached it. Result reported by the owner; the output is not recorded here.

## Item 3: install through the proxy, LiteLLM through a forwarder

Install half passed, with tinyproxy 1.11.3 (Homebrew) and `opencode-ai` 1.19.5.

- Box started with `--no-dns` and the proxy variables; tinyproxy bound to the gateway address with `FilterDefaultDeny` and one allowed host, `registry.npmjs.org`.
- `npm install -g opencode-ai@1.19.5` worked through the proxy in 5 s, using only `CONNECT registry.npmjs.org:443`. No other host was requested, so the OpenCode binary comes from npm, not from a separate download host.
- `opencode --version` runs.
- A request to `example.com` was refused by the proxy ("Proxying refused on filtered domain").

Findings:

1. `--no-dns` does not leave the box without a resolver. `/etc/resolv.conf` then holds `nameserver 1.1.1.1` and `1.0.0.1`. They are unreachable on the internal network, so nothing leaks, but lookups by tools that ignore the proxy will time out instead of failing at once. Item 6 measures that. An empty resolver file or `--dns` pointing at an address that refuses quickly may be better.
2. Node prints an `EnvHttpProxyAgent is experimental` warning with `NODE_USE_ENV_PROXY=1` (Node 22). Harmless, but it is noise in every npm run.

Since then OpenCode moved to 2.2.7 from the GitHub release, baked into the spike image (see "OpenCode version" below). The script no longer installs OpenCode from npm; it installs a small package instead, because npm through the proxy still matters for plugins and project dependencies. The changed script ran on 2026-11-12: the npm install through the proxy and the refusal of `example.com` pass with the 2.2.7 image.

### Gateway access through the forwarder

Passed, run by the owner on 2026-11-12 with `BW_ITEM=<item name>`.

- The script read the gateway URL and the key from the Bitwarden login item (website and password fields) after a master password prompt.
- The forwarder bound to the gateway address, swapped the box's dummy key for the real one and forwarded over HTTPS.
- The box, with `--no-dns` and no route to the internet, listed the gateway's models through `http://<gateway>:<port>/v1/models`: 47 model ids came back.

### `opencode run` round trip

```
BW_ITEM=<item name> OPENCODE_MODEL="<id> <id>" ./03-proxy-install-llm.sh
```

The owner's first attempt failed with `Model unavailable: relay/<id>`. Cause: a bug in the script, not the model or the gateway. `container exec` without `-i` does not forward stdin, so the OpenCode config written through a heredoc arrived as an empty file and the `relay` provider did not exist. Fixed with `-i`.

Dry run against a fake gateway after the fix (2026-11-12): OpenCode 2.2.7 lists `relay/<id>` and sends `POST /v1/chat/completions` to the forwarder. So the config shape (`provider.<name>` with `npm: @ai-sdk/openai-compatible`, `options.baseURL`, `options.apiKey`, `models`) and `OPENCODE_CONFIG` both work in 2.x. 

Real run by the owner (2026-11-12): **passed.** OpenCode 2.2.7 in the box answered `pong` through the forwarder with two models. The box had a dummy key, no DNS and no internet route; the proxy log shows no host other than the refused ones.

Findings from that run:

1. `container exec` needs `-i` for anything piped in. Relevant for `sandcastle fetch` (item 7 already uses `-i`).
2. OpenCode asks for `models.opencode.ai` repeatedly at start (its model catalogue). The proxy refuses it and OpenCode works without it. Keep it off the allowlist.
3. The openai-compatible provider needed no download from npm at run time.
4. OpenCode retries a failing provider 11 times before giving up (about 90 s per model here).

## OpenCode version: 2.2.7 from the GitHub release

Owner decision (2026-11-12): take OpenCode from the GitHub release, at least 2.0.

- OpenCode 2.x is not on npm (`opencode-ai` stops at 1.19.5) and GitHub still marks v1.19.5 as "latest". The 2.x line exists as GitHub releases only for 2.2.0 to 2.2.7; 2.0.x and 2.1.x are tags without release assets.
- Picked **v2.2.7** (2026-11-10), the newest release. Nothing upstream marks one 2.2.x as more stable than another: every release mixes features and fixes. 2.2.6 or newer is wanted because 2.2.6 added origin checks on `/api/*` and stricter fetch rules. The alternative is 2.2.5, which went 7 days without a follow-up release, but it lacks those.
- The Containerfile downloads `opencode-linux-arm64.tar.gz` with `ADD --checksum`. The SHA-256 matches both the release's `SHA256SUMS` file and the digest GitHub reports for the asset.

What is different in 2.x, found by running it (not from documentation):

1. **There is no `opencode web`.** The web UI and API come from `opencode serve --hostname .. --port ..`. CLAUDE.md 2.3 still names `opencode web`.
2. **The API requires authentication.** `/api/*` answers 401 without credentials and with a wrong password, and accepts Basic Auth with user `opencode` and `OPENCODE_SERVER_PASSWORD`. The UI shell on `/` is a static page served without authentication.
3. **OpenCode has its own one-time login links.** `opencode pair` prints `http://127.0.0.1:<port>/auth/connect/<token>`, valid once and for 5 minutes. This overlaps with the relay's planned one-time link. Owner decision (2026-11-12): the relay keeps its own one-time link and injects Basic Auth; OpenCode's pairing is not used.
4. **There is a background service.** `opencode pair` and `opencode run` start or use a background server on a random port unless `--standalone` or `--server` is given. In the box only `opencode serve` should run; `pair --remote` can expose a server to the internet through a tunnel, which the egress allowlist must keep blocked.

## Item 5: read-only root, tmpfs, volumes

Passed with OpenCode 2.2.7, after one fix.

- `opencode serve` runs with `--read-only`, `--tmpfs /tmp`, `--cap-drop ALL`, `--no-dns`, 2 CPUs, 2 GB, as uid 1001, with a home volume and a workspace volume. All capability sets are empty.
- `/usr/local` and `/etc` are not writable; `/tmp` and both volumes are.
- OpenCode writes only below home: `.local/share/opencode`, `.local/state/opencode`, `.cache/opencode`, `.config/opencode`.
- With no proxy and no network it starts cleanly: no failed downloads in the log.
- Auth results as in the version section above.

Finding (answers most of item 8): **a fresh named volume is owned by root (0:0, mode 755), so the non-root user cannot write it.** OpenCode then exits at start with `EACCES: permission denied, mkdir '/home/agent/.local'`. `container volume create` has no ownership option. Fix used in the spike (`volume_init` in `lib.sh`): once per volume, a one-off root container with no network and only `CAP_CHOWN` runs `chown 1001:1001` on the volume root. sandcastle needs the same step when it creates volumes.

## Item 4: UI port reachability

Host to box passed. LAN isolation is not shown yet.

- The host fetched a page from the box at `<box-ip>:4096` over the internal network, with no port published.
- Nothing listens on port 4096 on `127.0.0.1` or on the Mac's LAN address.

Finding: **`net.inet.ip.forwarding` is 1 on this Mac**, so it routes between interfaces. A device on the LAN that adds a route to the box subnet through the Mac could then reach the box directly. The pf rules from item 9 did not cover this, because they only filter traffic coming in from the box. Mitigations:

1. Added to rule set B in item 9: `block return out quick on <bridge> all`, after the rule that lets the host's gateway address reach port 4096. Forwarded LAN traffic leaves through the bridge with a LAN source address and is dropped. Run by the owner on 2026-11-12: item 9 passes with this rule, and host to box still works.
2. OpenCode's API needs the per-run password anyway (item 5), so this is a second layer, not the only one.

Not known: who turned forwarding on. `container` or vmnet NAT for the default network is the likely cause; sandcastle should not rely on it being off.

Manual check, with a second device on the LAN:

```
./04-ui-port-reachability.sh hold-pf   # box running, pf rules loaded (sudo); prints the commands for the other device
./04-ui-port-reachability.sh clean
```

The owner ran the baseline (`hold`, no pf rules) on 2026-11-12: the request to the Mac's LAN address failed, as it should. The request to the box address returned the page. It is not recorded which machine that second request came from: from the Mac itself it is expected, from another device it would confirm the hole. The run with pf rules loaded is still to do.

## Item 6: proxy variables

All checks passed (Node 22, npm 10.9, Git from Debian bookworm, Bun 1.4.4).

| Tool | No proxy variables | With `HTTP(S)_PROXY` |
| --- | --- | --- |
| Node built-in `fetch` | fails after 20 s | still fails; works only with `NODE_USE_ENV_PROXY=1` (1 s) |
| npm | fails after 20 s | works (1 s) |
| Git over HTTPS | fails after 20 s | works (1 s); a non-allowlisted host fails at once |
| Bun `fetch` and `bun add` | fails after 20 s | works (1 s) |

Findings:

1. Set all of `HTTP_PROXY`, `HTTPS_PROXY`, `http_proxy`, `https_proxy`, `NO_PROXY`, `no_proxy` and `NODE_USE_ENV_PROXY=1` in the box.
2. Tools that ignore the proxy do fail, but only after 20 s: with `--no-dns` the box still has `1.1.1.1` as resolver (item 3) and waits for it. It is a timeout, not a hang. To make it fail fast, give the box a resolver address that refuses at once (for example `--dns <gateway>`, where the pf rule answers with a reset). Not tested.
3. Installing the standalone `bun` package needed only `registry.npmjs.org`.

## Item 7: clone-and-fetch

All checks passed. The mechanism proposed in CLAUDE.md 2.5 works as written.

- **Seed:** the box cloned from the host `.git`, mounted read-only at `/source.git`. The untracked `.env` in the host working tree did not reach the box, and the box cannot write to the mount.
- **Fetch:** the host ran `git fetch "ext::container exec -i <box> git upload-pack /workspace/repo" '+refs/heads/*:refs/remotes/sandcastle/*'`. The box's commit arrived under `refs/remotes/sandcastle/agent/work`, objects pass `git fsck`, and the host's branch and working tree were untouched.
- **Nothing executable crossed over:** the box had planted a `post-checkout` hook and `core.fsmonitor` in its own clone; neither exists in the host repo after the fetch.
- **Host to box:** a new host commit was visible to `git fetch origin` in the box straight away.

Findings:

1. The mount root appears in the box as owned by root, so Git refuses it ("dubious ownership") until `safe.directory` is set for `/source.git`. sandcastle sets that in the box's Git config.
2. `ext::` transport is off by default in Git. The host enables it for that one command with `-c protocol.ext.allow=always`; nothing is written to the host's Git config.
3. `container exec` needs `-i` here, as in item 3.
4. Not covered: a repo whose `.git/config` has credentials in a remote URL. The clone copies only `remote.origin.url` of the box's own clone (the mount path), but the mounted `config` file itself is readable in the box. The check in CLAUDE.md 2.5 ("no credentials in remote URLs before mounting") is still needed.

## Item 8: non-root user, mount and volumes

Passed with the ownership fix.

- **Read-only `.git` mount:** readable by uid 1001 and by an arbitrary uid (4242), even with the host directory at mode 700. Files show up as owned by whichever user looks at them; the mount root shows as root. Nobody in the box can write it. So host file permissions do not restrict who in the box can read a mount: whatever is mounted is readable by every box user.
- **Fresh named volumes:** owned by root (0:0, mode 755); uid 1001 cannot write them. After the one-off `chown` (see item 5) uid 1001 can, another uid cannot, and ownership persists across boxes.

Finding: **a named volume can be attached to only one running container.** A second container with the same volume fails to start ("The storage device attachment is invalid"). So the `chown` step must run before the box starts, and nothing else can mount the workspace volume while the box is up. That is another reason for fetching through `container exec` and not through a helper container.

## Item 9: pf rule

Passed. pf sees vmnet traffic and can limit the box to the relay.

Baseline without rules: the box reached a second host service (listening on `::`) through the gateway address, the Mac's LAN address and the host's IPv6 link-local address on the bridge.

- **Rule set A, by source subnet:** blocks the two IPv4 paths, keeps box to relay and host to box port 4096 working. Leaves the IPv6 link-local path open. Not enough on its own.
- **Rule set B, by interface (`bridge100`):** blocks all three paths, IPv6 included. Box to relay and host to box port 4096 keep working. This is the one to use:

  ```
  pass in quick on bridge100 inet proto tcp from <subnet> to <gateway> port <relay ports>
  pass out quick on bridge100 inet proto tcp from <gateway> to <subnet> port 4096 keep state
  block return in quick on bridge100 all
  ```

- With rule set B loaded, a restarted box still got an address (`.3` instead of `.2`, so box addresses are not stable) and all checks held.
- The interface name stayed `bridge100` across the box restart. One box only; with several boxes on several networks the names will differ and must be looked up per network.
- Flushing the anchor restores the baseline, so cleanup works.
- Host to box worked under rule set A without an explicit `pass out` rule. Rule set B keeps the rule anyway, because it is explicit about what is allowed.

The first run reported 1 failed check. The output was not kept, but the likely cause is the script, not pf: it judged the IPv6 path under rule set A as a failure although A does not filter IPv6 by design. With that check made observational, the same IPv6 path shows as open under A and the run has 0 failures.

pf was already enabled when the second run started ("Enabled for 14 minutes", which matches the first run). Either the first run did not release its enable token or something else holds one. Check with `sudo pfctl -s References`; an enabled pf with the stock empty ruleset changes nothing.

Costs of the pf approach:

- Needs root at `up` and at `down`/`reset` (a sudo prompt, or a small privileged helper later). This is new friction against the "nothing to type" goal.
- The stock `/etc/pf.conf` only evaluates anchors under `com.apple/*`. The spike loads into `com.apple/sandcastle-spike` to avoid editing `/etc/pf.conf`. That is squatting in Apple's namespace; the clean alternative is one added `anchor "sandcastle/*"` line in `/etc/pf.conf`, which macOS updates may overwrite.
- The rule can only be loaded once the box interface exists, the same ordering problem as the relay bind. Until it is loaded, the box can reach host services, so the box should start with an entrypoint that waits, or the rule goes by subnet and is loaded before the box starts (possible only if rule set A is enough for IPv4 and IPv6 is handled another way).

## How to run the rest

```
./01-internal-network.sh
./02-gateway-bind.sh [hold|check]
[BW_ITEM=<item name>] [OPENCODE_MODEL=<id>] ./03-proxy-install-llm.sh
./04-ui-port-reachability.sh [hold]
./05-readonly-root.sh
./06-proxy-variables.sh
./07-clone-and-fetch.sh
./08-uid-mapping.sh
./09-pf-host-isolation.sh                           # asks for sudo
```

- Items 3 and 6 need `tinyproxy` on the host. It was installed with `brew install tinyproxy` on 2026-11-12; remove it with `brew uninstall tinyproxy` after the spike.
- Items 3 and 5 to 8 use the spike image; the first script that needs it builds it from `Containerfile`.
- Item 3 reads the gateway URL and key from Bitwarden (`BW_ITEM`). Do not put them in a file in this repo.
- Every run is copied to `spike/.state/<script>.log`.
- Each script removes its own boxes, volumes and the `sc-spike` network on exit. Scratch files go to `spike/.state/` (git-ignored).
- The spike uses image tags, not digests, and the Node base image. That is for the spike only.
