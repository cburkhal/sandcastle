# CLAUDE.md: sandcastle

Context for Claude Code working in this repo. Read this file fully before making changes.

- Repo: https://github.com/cburkhal/sandcastle
- Design doc (source of the decisions below): https://claude.ai/artifact/Fede71J8PTPMntET8ztjJs
- This file is newer than the design doc where they differ (clone-and-fetch instead of a read-write repo mount, OpenCode 2.x, pf rule, order of `up`).
- Host setup (installing Apple `container` and the Bitwarden CLI, storing the LiteLLM key): see `README.md`.
- Status as of 2026-11-12: **design verified by a shell spike (`spike/NOTES.md`); build steps 1 and 2 of 4 done** (box and relay work; Bitwarden, pf and `fetch` are missing). See "Current status and next steps".

---

## 1. What sandcastle is

sandcastle runs one OpenCode instance per Git repo inside its own Apple `container` VM on macOS. The user chats with it in a browser on the Mac. The LLM key and Git credentials never enter the VM. All outbound traffic leaves through a small host-side relay that is part of the sandcastle CLI.

Owner: a single user on one Mac (Apple silicon, macOS 27). Self-hosted, personal use. Not a multi-user or team product (yet).

### Goals

- OpenCode runs with full autonomy (`"permission": {"*": "allow"}`). Safety comes from the box, not from permission prompts.
- Chat in the browser via OpenCode's web UI (`opencode serve`). **Frictionless:** `sandcastle up` asks for what it needs once at the start (Bitwarden master password, `sudo`), then opens the browser already logged in. Nothing prompts after that.
- Secrets live in Bitwarden. Never written to disk, never mounted into the VM, never committed.
- Reset is one command and leaves nothing on macOS except repo changes the user keeps.
- A small Rust CLI owns the whole lifecycle, built from replaceable modules, so a UI can sit on top later.

### Non-goals (v1)

- Docker or Docker Compose inside the box (explicitly dropped; documentation builds run elsewhere).
- Other users, other roles, other Macs.
- Headless/unattended GitHub automation (the coordinator pipeline from provision-machines).
- Several agents (Claude Code, Codex, Gemini) in one image. OpenCode only.

### Threat model

In scope:
- Prompt injection (README, issue, web page tells the agent to exfiltrate secrets).
- Malicious packages (plugins, MCP servers, `npx` deps running code at install/start).
- A compromised or buggy OpenCode release.
- Poisoned repo output (agent plants a Git hook or editor task that later runs on macOS). Mitigated in part.
- Websites in the user's browser driving the local OpenCode server (see CVE-2026-22812 below).

Out of scope / accepted:
- Hypervisor escape from Apple's Virtualization framework.
- The LLM provider seeing repo content (inherent to the use case).
- Malware already running on the Mac.

Assets, most valuable first: Bitwarden vault, SSH and GitHub credentials, the LiteLLM key, other repos and files in the home folder, the company network.

---

## 2. Architecture

```
macOS host                                   Apple container VM (one per repo)
+--------------------------------+           +----------------------------------+
| Browser  (127.0.0.1, logged in)|           | OpenCode web (agent module)      |
|        | one-time link         |  internal |   dummy LLM key, no DNS,         |
|        v                       |  network  |   no Git creds, read-only root   |
| sandcastle relay (Rust) <------+-----------+-> port 4096 (NOT published)      |
|   UI gateway: cookie + Origin  |           | /workspace  (own clone, volume)  |
|   LLM relay: swaps in real key |           | OpenCode config (read-only)      |
|   Egress proxy: CONNECT allow  |           | home volume (sessions, caches)   |
|   logs every blocked host      |           +----------------------------------+
|        ^ key, in memory        |
| Bitwarden CLI (secrets module) |
+---|----------------|-----------+
    | HTTPS + real key           | CONNECT (allowlist only)
    v                            v
 LiteLLM (company gateway)    Allowlisted hosts (npm registry, GitHub)
```

The host repo is never writable from the box. The agent works on its own clone; the host pulls commits out with `sandcastle fetch` (see 2.5).

The box has exactly one exit: the relay on macOS.

### Components

| Component | What it is | Source / license | Why trusted enough |
| --- | --- | --- | --- |
| Apple `container` | One lightweight VM per container | github.com/apple/container, Apache-2.0 | Apple-maintained; VM boundary per container |
| OpenCode | The agent, run as `opencode serve` | github.com/anomalyco/opencode, MIT | Version pinned by SHA-256 of the GitHub release, built into our own image |
| LiteLLM | Existing company model gateway | Already operated | Issues virtual keys with budget, model allowlist, expiry |
| Bitwarden CLI (`bw`) | Source of truth for secrets | bitwarden.com/help/cli, official | User unlocks it; session never stored |
| macOS `pf` | Packet filter that limits the box to the relay | Part of macOS | Rules are few and loaded per box |
| sandcastle (Rust) | CLI plus host-side relay | This repo | Small, single purpose, reviewable |

### 2.1 sandcastle CLI (Rust)

One binary on macOS. Drives `container` through its public CLI (no private APIs). Fetches secrets through the secrets module. Runs the relay while a box is up.

- Config: `~/.config/sandcastle/config.toml`. Secret *references* only, never secret values.
- Per-box state: `~/Library/Application Support/sandcastle/` (port, network name, image digest).
- No password to manage: the relay creates a random login token and a random internal OpenCode password per run, both in memory only.

### 2.2 Host relay (part of the CLI): the security core

Listens on `127.0.0.1` for the browser and on the host side of each box's internal network (the network's gateway address) for the box. The gateway address exists on the host only while a box is running on that network, so the box-side listeners bind after the box has started and must rebind on error. Three jobs:

1. **UI gateway.** Serves the OpenCode web UI on `127.0.0.1:<port>`. `sandcastle up` opens a one-time link that sets a session cookie. Every request must then carry the cookie, a `Host` of `127.0.0.1:<port>` (blocks DNS rebinding) and no foreign `Origin` (blocks other websites). When forwarding to the box, the relay adds OpenCode's internal Basic Auth credentials (user `opencode`, password `OPENCODE_SERVER_PASSWORD`). OpenCode 2.x has its own one-time links (`opencode pair`); they are not used.
2. **LLM relay.** OpenCode talks plain HTTP to `http://<gateway>:<port>/v1` with a dummy key. The relay swaps in the real LiteLLM key and forwards over HTTPS. The key exists only in relay memory. No TLS interception needed because the relay is the endpoint.
3. **Egress proxy.** All other outbound traffic goes via `HTTP_PROXY` / `HTTPS_PROXY`. The relay allows `CONNECT` only to allowlisted hosts and logs every blocked attempt. OpenCode asks for `models.opencode.ai` at start; it works without it and the host stays off the allowlist.

The relay checks the peer address of box-side connections: the host sees the box's own address, not a NAT address. Connections from any other address are dropped.

As built: the LLM relay listens on `<gateway>:4180` and forwards only paths under `/v1/`; the egress proxy listens on `<gateway>:4181`, accepts only `CONNECT <host>:443` for exact allowlisted names, and refuses names that resolve to private or local addresses. The UI gateway listens on a free port on `127.0.0.1`. `up` asks the gateway for its models (`/v1/models`) before it creates anything, which also proves the key works, and writes them into the agent's config. The gateway URL must be `https://`; plain `http://` is accepted only for `127.0.0.1`. The relay log is `~/Library/Application Support/sandcastle/boxes/<box>/relay.log`; refused hosts are also printed once on the terminal.

Why the login stays (even though "all is local"): an unauthenticated OpenCode server on localhost is reachable from any website open in the browser. CVE-2026-22812 (OpenCode < 1.0.216) showed permissive CORS let any web page execute commands through OpenCode's server. In 2.2.7 the API answers 401 without credentials and 2.2.6 added origin checks, but the relay does not rely on either. OpenCode's SECURITY.md states it does not sandbox the agent and its permission system is a UX feature, not isolation. The user asked for no password; the decision is an invisible login (one-time link + cookie), which is zero friction. Confirmed by the owner (question 6).

### 2.3 The box (Apple container)

- **Image:** `debian:stable-slim` pinned by digest, plus Node, Git, ripgrep and a pinned OpenCode. Node comes from nodejs.org, pinned by version and SHA-256 (22.23.5): Debian's Node 20 ignores the proxy variables for `fetch`; `NODE_USE_ENV_PROXY` needs 22.21 or newer. OpenCode comes from the GitHub release tarball, checked against a pinned SHA-256 at build time (2.x is not on npm). Built locally with `container build` from a Containerfile (OCI). Non-root user (uid 1001 in the spike).
- **Entrypoint:** `opencode serve --hostname 0.0.0.0 --port 4096` with `OPENCODE_SERVER_PASSWORD` set to the random per-run value (only the relay knows it). There is no `opencode web` in 2.x. Only `serve` runs in the box; `opencode pair --remote` can open a tunnel to the internet and must stay blocked by the egress allowlist.
- **Network:** per-box network via `container network create --internal` (host-only access). The relay is the only way out. `container` picks the subnet; read gateway and subnet from `network inspect`. Box addresses change between starts.
- **Host firewall:** "internal" keeps the box off the internet and the LAN, but not off the Mac: without a filter the box reaches every host service that listens on `0.0.0.0` or `::`. A pf rule set per box, by bridge interface, allows only box to relay ports and host gateway address to box port 4096, and blocks everything else in both directions (see 2.6).
- **DNS:** off (`--no-dns`). Names are resolved by the relay, so DNS can't be a side channel. `--no-dns` still leaves `1.1.1.1` in `/etc/resolv.conf`; it is unreachable, so tools that ignore the proxy fail after a 20 s timeout. A resolver address that refuses at once would make that fast (untested).
- **Proxy variables:** `HTTP_PROXY`, `HTTPS_PROXY`, `http_proxy`, `https_proxy`, `NO_PROXY`, `no_proxy` and `NODE_USE_ENV_PROXY=1`. Verified for Node `fetch`, npm, Git and Bun.
- **Mounts:** a named volume at `/workspace` holding the box's own clone of the repo; OpenCode config read-only (passed with `OPENCODE_CONFIG`); a named volume for the agent's home (sessions, caches). New volumes are root-owned: when sandcastle creates one, a one-off root container with no network and only `CAP_CHOWN` hands it to the box user, before the box starts. A volume can be attached to only one running container. **The host working tree is never mounted read-write. No macOS home mount. No SSH agent (`--ssh` stays off).**
- **Hardening:** `--read-only` root filesystem, `--tmpfs /tmp`, `--cap-drop ALL`, CPU and memory limits (`-c`, `-m`).
- **Ports:** none published. The relay reaches port 4096 over the internal network.
- A "container machine" (systemd, `container machine create`) is **not** used: with Docker out of scope, one plain container per repo is simpler and smaller. If ever used, `--home-mount` must be `none` (default is `rw`).

### 2.4 Secrets flow

1. `sandcastle up` checks `bw status`. If not logged in, it stops and says how to log in (see `README.md`). If locked, it runs `bw unlock` interactively and keeps `BW_SESSION` in memory for that one command only.
2. Resolves the LiteLLM virtual key by Bitwarden item ID and hands it to the relay in memory. In the owner's vault the item is a Login with the gateway URL in the website field and the key in the password field.
3. The box gets only a dummy key and the relay's address.
4. `sandcastle down` stops the relay; the key is gone.

Rule set by the owner (2026-11-12): **all secrets are obtained at the start of the session, then everything runs unattended.** `up` begins with one secrets phase: Bitwarden login if needed, one vault unlock (master password), and resolving every secret reference the session will need, including any added later (for example a GitHub token). Nothing may prompt after that phase. One master password prompt per `up` is accepted. The same applies to other credentials `up` needs, such as `sudo` for the pf rule.

Every prompt must say which password it wants and why (owner request, 2026-11-12): the Bitwarden master password and the macOS login password for `sudo` look alike on a bare prompt. sandcastle prints one line before each, for example "Bitwarden master password (to read the LLM key)" and "macOS login password (sudo, to load the firewall rules for this box)", and passes its own prompt text to `sudo -p`.

The LiteLLM virtual key must carry a monthly budget, a model allowlist and an expiry, so even a leak is cheap. The user confirmed LiteLLM supports per-user virtual keys.

### 2.5 Git

Model: clone-and-fetch. The agent never writes to the host repo, so nothing it puts into `.git` (hooks, `core.fsmonitor`, `core.pager`, `diff.external`, `core.sshCommand`) can run on the Mac, whichever Git command or editor integration touches the host repo.

- The box works on its own clone in the `/workspace` volume. It can commit but has no Git credentials, so it cannot push to the user's remotes.
- `sandcastle fetch` brings the box's commits into the host repo under `refs/remotes/sandcastle/*`. Fetching from an untrusted repo is safe; checking out or merging the result is the human review gate.
- The user reviews, merges and pushes with plain Git on macOS.
- Tradeoff accepted by the owner: the agent's working tree cannot be opened live in a macOS editor. Uncommitted work in the box is invisible to the host until committed and fetched.

Mechanism (verified by spike item 7, confirmed by the owner):
- Seed: the host repo's `.git` directory is mounted read-only into the box as the clone source. Only `.git`, not the working tree, so untracked files such as `.env` stay out of the box. Check that `.git/config` holds no credentials in remote URLs before mounting.
- Seed details: the mount root shows up as root-owned in the box, so the box's Git config needs `safe.directory` for it. Whatever is mounted is readable by every user in the box, regardless of host file modes.
- Fetch: the host runs `git upload-pack` inside the box through the runtime module's exec, so Git's server side also runs in the box and the host only parses the pack stream. In the spike: `git -c protocol.ext.allow=always fetch "ext::container exec -i <box> git upload-pack /workspace/repo" '+refs/heads/*:refs/remotes/sandcastle/*'`. The `ext` transport is enabled for that one command only; exec needs `-i`.
- Sync host to box: the agent runs `git fetch` from the read-only source mount.

### 2.6 Host firewall (pf)

Needed because of spike items 1 and 4: a box on an internal network reaches host services on all addresses, and the Mac forwards IP, so a LAN device with a route could reach the box.

Rules per box, by bridge interface (verified in spike item 9):

```
pass in quick on <bridge> inet proto tcp from <subnet> to <gateway> port <relay ports>
pass out quick on <bridge> inet proto tcp from <gateway> to <subnet> port 4096 keep state
block return in quick on <bridge> all
block return out quick on <bridge> all
```

- Rules by subnet alone are not enough: they leave IPv6 link-local open.
- The bridge interface exists only while the box runs, so the rules load after the box starts, like the relay's box-side bind. Until both are in place the box must not do anything: the entrypoint waits for the relay.
- Anchor: one line `anchor "sandcastle/*"` in `/etc/pf.conf`, added by `init` (owner decision). macOS updates may remove it, so `doctor` checks that it is still there, and `up` refuses to start a box without it. Rules go into `sandcastle/<box>`.
- pf is enabled with a reference token (`pfctl -E`) and released with `pfctl -X` on `down`.
- Needs root: `up` asks for `sudo` in its secrets phase, `down` and `reset` need it to remove the rules.

---

## 3. Modules

Modularization rule set by the user (follow it strictly):

> Modular means a part can be replaced later. Do it where it costs minimal effort (e.g. putting it into a module). Where an interface can follow a standard/open format instead of a tool-specific one, use the standard. **Do not** build extra functions or adapters for a specific other tool until it is actually needed.

| Module | Interface the core calls | v1 implementation | Open standard it leans on |
| --- | --- | --- | --- |
| Secrets | `resolve(reference) -> Secret` (read only) | Bitwarden via `bw` | Secret references as URIs in config, e.g. `bitwarden://<item-id>/password` (same idea as 1Password `op://`) |
| Agent | `image()`, `config(llm_endpoint, models)`, `ui_port()`, `health()` | OpenCode | OpenAI-compatible endpoint; standard `HTTP_PROXY`/`HTTPS_PROXY`; image from a Containerfile (OCI) |
| Sandbox runtime | `network_create`, `volume_create`, `run(BoxSpec)`, `exec`, `stop`, `remove`, `inspect` | Apple `container` CLI | OCI images; `BoxSpec` (image, mounts, env, network, limits) uses OCI runtime terms, not Apple-specific flags |

Deliberately **not** modules:
- **LiteLLM:** already a standard (OpenAI-compatible URL + key in config).
- **The relay:** security core, stays one piece.
- **Config:** one TOML file; backend chosen by name (`kind = "bitwarden"`). No plugin system, no dynamic loading.

Hard rule: **core code never calls `bw`, `opencode` or `container` directly.** Only the module implementations do.

Honest caveat: the agent interface is the leakiest. Swapping OpenCode would still be real work inside its module; the interface only keeps that work out of the core.

### Planned layout

```
sandcastle/
  Cargo.toml
  CLAUDE.md
  src/main.rs                       CLI entry (clap)
  src/core/                         box lifecycle, state, config loading
  src/relay/                        UI gateway, LLM relay, egress proxy   (build step 2)
  src/secrets/mod.rs                trait SecretStore            (build step 3)
  src/secrets/bitwarden.rs                                       (build step 3)
  src/agent/mod.rs                  trait Agent
  src/agent/opencode/               impl, Containerfile, config template
  src/runtime/mod.rs                trait SandboxRuntime + BoxSpec
  src/runtime/apple_container.rs
  spike/                            shell scripts + notes for the manual spike (step 1)
```

### Example config (`~/.config/sandcastle/config.toml`, no secret values)

```toml
[secrets]
kind = "bitwarden"

[llm]
base_url = "https://<litellm-host>"
api_key  = "bitwarden://<item-id>/password"

[agent]
kind    = "opencode"
version = "<pinned>"

[runtime]
kind = "apple-container"

[egress]
allow = ["registry.npmjs.org", "github.com", "codeload.github.com"]
```

`github.com` and `codeload.github.com` are in the default list because npm dependencies with Git URLs and tools that download release binaries need them. **Known and accepted risk:** a CONNECT tunnel is opaque. The relay sees the host, not the method or credentials, so "read-only GitHub" cannot be enforced. An injected prompt could push repo content to an attacker's repo with an attacker-supplied token. The same applies to `registry.npmjs.org` (`npm publish` with a foreign token). Closing this would need a method-aware relay path (TLS termination for these hosts, GET and `git-upload-pack` only); not planned for v1.

Allowlist changes come only from host config. A file inside the repo must never be able to widen its own network access.

### Rust building blocks

`clap` (CLI), `tokio` + `hyper` (relay), `serde` + `toml` (config). No Keychain dependency in v1 (question 1 answered no). All widely used, permissive licenses.

---

## 4. CLI commands (v1)

In v1, `up` runs in the foreground: the relay lives as long as that terminal; Ctrl-C stops the box. A background daemon comes only with the UI.

| Command | What it does |
| --- | --- |
| `sandcastle doctor` | Checks macOS, Apple silicon, `container` version, `bw` login, LiteLLM reachability, the pf anchor line in `/etc/pf.conf` |
| `sandcastle init` | Writes `config.toml`, builds the image, records its digest, adds the pf anchor line to `/etc/pf.conf` (sudo) |
| `sandcastle up [path]` | Gets all secrets first, creates network + volumes on first run, starts box, starts relay, loads pf rules, seeds the box's clone, opens browser |
| `sandcastle down [path]` | Stops box and relay, removes the pf rules; keeps the home and workspace volumes |
| `sandcastle ls` | Lists boxes: repo, port, status, image digest |
| `sandcastle fetch [path]` | Fetches the box's commits into the host repo under `refs/remotes/sandcastle/*`; the user merges and pushes with plain Git |
| `sandcastle reset [path]` | Deletes box, network, home volume and workspace volume (warns about unfetched commits); host repo untouched |
| `sandcastle log [path]` | Relay log: allowed/blocked hosts, LLM request count |

`up`, in order:
1. Resolve repo root; derive box name from path.
2. Secrets phase, the only step that may prompt: `sudo` for pf, `bw unlock` if locked, resolve every secret into memory (2.4).
3. Create the internal network if missing. Create home and workspace volumes if missing and hand them to the box user (2.3).
4. `container run` with the flags from 2.3, dummy key, proxy variables, and the random internal password. The entrypoint waits until the relay answers before it starts OpenCode.
5. Now that the network's gateway address and bridge interface exist: bind the relay's box-side listeners on the gateway address, load the pf rules (2.6), and bind the UI gateway on `127.0.0.1:<port>`.
6. On first run the box clones the repo from the read-only source mount.
7. Wait for the UI to answer, then open the one-time login link on `http://127.0.0.1:<port>`.

Steps 4 and 5 are in this order because the gateway address cannot be bound before a box runs (spike item 2).

---

## 5. Security controls (what each layer blocks)

| Control | Blocks | Residual risk |
| --- | --- | --- |
| One VM per box, no home mount | Reading `~/.ssh`, Keychain, other repos, Bitwarden data | Hypervisor escape (accepted) |
| Internal network + relay as only exit | Arbitrary hosts, reverse shells, reaching the company network | Exfiltration via allowlisted hosts (keep list short) |
| pf rules per box | Box reaching other services on the Mac; LAN devices reaching the box through the Mac | Window between box start and rule load (entrypoint waits); needs the anchor line in `/etc/pf.conf` |
| No DNS in the box | DNS tunnelling | None known |
| LLM key only in relay | Stealing the LiteLLM key | Spending budget while box runs (budget caps it) |
| No Git credentials in box | Pushing to or deleting the user's remote repos | Exfiltrating repo content to GitHub or npm with an attacker-supplied token (accepted, see section 3) |
| Clone-and-fetch: host repo never writable from the box | Agent-written hooks and `.git/config` keys running on macOS | Fetched content once checked out (editor tasks, Makefiles, build scripts) |
| Read-only config mount | Agent rewriting its own config, prompts, MCP servers | None |
| Read-only root, `cap-drop ALL`, non-root | Persisting outside the volumes | Persistence in home and workspace volumes until `reset` |
| UI via relay: one-time link, cookie, Host + Origin checks | Websites in the browser or other machines driving the agent | Local malware (out of scope) |
| Allowlist only from host config | Repo widening its own network access | None |
| Pinned image digest + OpenCode version | Surprise upstream changes | Pins need updating (Renovate-style comments) |

Biggest remaining hole: content the agent commits. Treat fetched commits like a PR from a stranger, and review before checking them out.

---

## 6. Reuse from provision-machines

Reference: https://github.com/BacLuc/provision-machines/tree/293a0077319925e73f162450118a8422fd91e58a/deploys/development_tools/ai_agent_devcontainer (a colleague's Linux Docker devcontainer for AI agents). Good at configuring OpenCode, weak at isolating it.

Keep (ideas, not code):
- `opencode.jsonc` provider pattern with `{env:...}`, trimmed to **one** provider pointing at the relay.
- Model list idea (`vshn-llm-router`, `update-us-ai-models.py`): instead fetch from LiteLLM `/v1/models` at start.
- Renovate pin comments on plugins and image.
- Pairing-URL idea for `opencode serve` (now done by the relay's UI gateway).
- Per-workspace port map.
- Agents `planner`, `build`, `review`: optional, strip the GitHub issue-comment and fork rules.

Drop:
- `privileged: true`, Docker-in-Docker.
- Shared home volume across projects (one per box instead).
- Read-write config mount (read-only instead).
- `.tokens` env file / keys as env vars (Bitwarden + relay instead).
- Claude Code, Gemini, Codex, Antigravity in the image.
- `curl | bash` installs and unpinned scripts.
- `coordinator`, `model-discovery` agents (built for unattended GitHub runs).
- Plugins `ponytail`, `opencode-gemini-auth`, `auto-resume`, `completion-check` for v1. `superpowers` optional after review. Brave Search MCP later, with its key relay-injected.

Start with **zero plugins and zero MCP servers**. Each plugin runs third-party code in the box at startup and must earn its place.

---

## 7. Decisions log (2026-11-12)

| Decision | Reason |
| --- | --- |
| Isolate via Apple `container`, not Docker Desktop | User wants macOS kept clean; VM per container |
| Docker Sandboxes (`sbx`) rejected for now | Proprietary, needs Docker login; revisit only if the open-source path gets too complicated |
| Docker / Compose inside the box dropped | Was the reason for privileged mode; docs can be built elsewhere |
| Plain container, not a "container machine" | No systemd needed without Docker |
| Secrets in Bitwarden, accessed via `bw` | User's password manager |
| LiteLLM virtual key, injected by relay | Agent never sees the real key; leaks are budget-capped |
| Invisible login (one-time link + cookie) instead of no auth | User wants frictionless; CVE-2026-22812 shows unauthenticated localhost is exploitable from websites |
| No published port; UI served by relay | Lets the relay enforce Host/Origin checks |
| Clone-and-fetch instead of a read-write repo mount | Agent-writable `.git/config` runs code on any host Git command, not only on push; owner does not need live editing on macOS |
| `sandcastle fetch`, then manual merge and push (agent PRs pending Q2) | No Git creds in box; human review gate |
| `github.com` and `codeload.github.com` stay in the default allowlist | Dependencies need them; exfiltration with a foreign token is accepted (dropping them was considered and rejected) |
| No Keychain cache in v1 (Q1) | No second copy of the key on disk |
| Shell spike first, then Rust (Q4) | Unverified vmnet assumptions decide the design |
| Agent config bundled in sandcastle (Q5) | One repo, one pin |
| Invisible login confirmed (Q6) | See 2.2 |
| pf rule limits the box to the relay's ports on the host (spike item 9 passed) | Spike item 1: a box on an internal network reaches any host service on `0.0.0.0`; costs root at `up`/`down` |
| Clone-and-fetch through a read-only `.git` mount and `upload-pack` via exec (Q7) | Spike item 7 passed as proposed; no Git proxy in the relay needed |
| pf anchor as one line in `/etc/pf.conf`, checked by `doctor` | Clean namespace instead of squatting under `com.apple/*`; macOS updates may remove the line, so it is checked |
| Build order: relay before the secrets module | The risky part is built and tested first; an environment variable supplies the key in the meantime |
| Relay keeps its own one-time link; OpenCode 2.x pairing links are not used | Relay needs the cookie and Host/Origin checks anyway; no dependency on OpenCode's pairing |
| All secrets are obtained in one phase at the start of `up`; no prompts afterwards | Manual setup once per session, full automation after; one master password prompt per `up` is fine |
| OpenCode v2.2.7 from the GitHub release, pinned by SHA-256 | Owner wants 2.x; it is not published to npm |
| Modules for secrets, agent, runtime | Replaceable later at minimal cost; no speculative adapters |
| Scope: owner only | Other roles dropped from scope |
| Name: sandcastle | Chosen by owner |

---

## 8. Open questions (numbered as in the design doc)

Answered on 2026-11-12 (see decisions log): 1 (no Keychain in v1), 3 (default allowlist as proposed, GitHub included), 4 (shell spike first), 5 (bundled), 6 (yes), 7 (clone-and-fetch as described in 2.5, confirmed after the spike).

Still open:

2. Manual push acceptable, or should the agent open PRs itself? (Needs a fine-grained GitHub token, relay-injected, and `github.com` reachable through a method-aware relay path, not a CONNECT tunnel.)
Do not silently decide these. Ask the owner, or implement the proposal and flag it.

---

## 9. Current status and next steps

**Status:** design verified by the shell spike; build steps 1 and 2 are done. Work happens on the `spike` branch until v0.1.

**Step 1: manual spike. Done on 2026-11-12**, results and evidence in `spike/NOTES.md`. All nine items ran with Apple `container` 1.7.1 and OpenCode 2.2.7:

- [x] 1. A box on an `--internal` network reaches a listener on the host's gateway address, not the internet.
- [x] 2. A host listener binds the gateway address only and survives sleep/wake.
- [x] 3. With `--no-dns` and proxy variables, npm installs through an allowlisting proxy and OpenCode answers a prompt through a plain HTTP forwarder with the key from Bitwarden.
- [x] 4. The host reaches port 4096 on the box; a second device on the LAN cannot, even with a route through the Mac, once the pf rules are loaded (the Mac has IP forwarding on, see 2.6).
- [x] 5. OpenCode runs with `--read-only`, tmpfs `/tmp`, a home volume and a workspace volume.
- [x] 6. Node `fetch`, npm, Git and Bun honour the proxy variables; without them they fail after a 20 s timeout.
- [x] 7. Clone-and-fetch works as proposed in 2.5.
- [x] 8. A non-root user reads the read-only `.git` mount and writes the volumes after the ownership fix.
- [x] 9. A pf rule set by interface limits the box to the relay, including the `block out` rule added for item 4.

The fallback (a proxy container on two networks instead of a host listener) is not needed.

**Build plan (agreed 2026-11-12):**

1. **Skeleton that starts a box. Done 2026-11-12.** Checked by hand: `init` builds the image; `up` starts a box as uid 1001 with read-only root, no capabilities and no internet, UI on the box address only, API answering 401; Ctrl-C and `down` stop it; a second `up` keeps the volumes; `reset` removes everything. The secrets trait is not in the tree yet; it arrives with step 3. Cargo project, config loading, the three module traits. Runtime module wraps the `container` CLI: network, volumes with the ownership fix, run, exec, stop, remove. `init` builds the image; `up`, `down`, `ls`, `reset` work without a relay. Done when `up` starts a hardened box with OpenCode serving, reachable only from the host.
2. **Relay. Done 2026-11-12**, except that the web UI has not been tried in a real browser yet (owner to check) and it ran against a local fake gateway, not the real one. Verified end to end with OpenCode in the box: one-time link and cookie, Host and Origin refusals, Basic Auth injection, npm, Git and Node `fetch` through the egress proxy, refusal of other hosts, key swap, streamed answers arriving chunk by chunk, `opencode run` answering through the relay. The parts: egress proxy (CONNECT allowlist, log of refused hosts); LLM relay (key swap, streaming); UI gateway (one-time link, cookie, Host and Origin checks, Basic Auth injection, streaming pass-through). The key comes from an environment variable until step 3. Done when the owner can chat in the browser and tests cover allowlist matching, Host/Origin checks and "key never in logs or errors".
3. **Secrets phase and pf.** Bitwarden module, the single upfront secrets phase, pf rules loaded after box start and removed on `down`/`reset`. Done when `up` prompts once at the start and then runs unattended.
4. **Git and release.** `sandcastle fetch`; the spike scripts become an end-to-end check against the real binary; merge `spike`, tag v0.1.

**After v0.1:**
- v0.2: `log`, `doctor`; model list from the gateway's `/v1/models`; image rebuild with digest pinning.
- v0.3: optional relay-injected secrets (Brave Search, GitHub token if Q2 = yes).
- Later: background daemon and a small UI (list boxes, start/stop/open).

---

## 10. Known Apple `container` facts (verified from docs, 2026-11-12)

- One lightweight VM per container; guest kernel from Kata Containers, replaceable via `container system kernel set`.
- Requires Apple silicon; macOS 26+ is the supported baseline.
- No `--privileged` (use `--cap-add`); no `--network host`; no native Compose.
- Freed memory inside a VM is not returned to the host; long-lived boxes may need restarts.
- Relevant `container run` flags: `--mount type=bind,source=..,target=..,readonly`, `-v`, `--read-only`, `--tmpfs`, `-p [host-ip:]host-port:container-port`, `--network`, `--no-dns`, `--dns`, `-u/--user`, `-e`, `--env-file`, `--cap-add/--cap-drop`, `--init`, `--ssh` (do not use), `-c/--cpus`, `-m/--memory`.
- `container network create --internal` = host-only access; `--subnet` available; no flag to disable NAT.
- Command reference: https://github.com/apple/container/blob/main/docs/command-reference.md
- Flags change between releases: keep every `container` invocation inside `src/runtime/apple_container.rs` and pin a tested version in `doctor`.

---

## 11. Working conventions for Claude Code in this repo

- **Never commit secrets**, tokens, real hostnames of internal services, or Bitwarden item IDs. Use placeholders like `<litellm-host>`, `<item-id>`.
- Respect module boundaries: core never shells out to `bw`, `opencode` or `container`.
- No speculative adapters for other tools (1Password, Docker, Podman, other agents) until asked.
- Pin everything: base image by digest, OpenCode by exact version, Rust deps via `Cargo.lock`. Add Renovate-style comments for pins.
- No `curl | bash` anywhere.
- The relay is security-critical: keep it small, test it (allowlist matching, Host/Origin checks, key never logged, key never in error messages).
- Never log secret values. Redact `Authorization` headers in relay logs.
- Owner preferences: concise, direct, flag tradeoffs explicitly, challenge decisions that look wrong instead of implementing them politely, ask clarifying questions inline. No em dashes in prose.
