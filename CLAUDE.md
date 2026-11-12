# CLAUDE.md: sandcastle

Context for Claude Code working in this repo. Read this file fully before making changes.

- Repo: https://github.com/cburkhal/sandcastle
- Design doc (source of the decisions below): https://claude.ai/artifact/Fede71J8PTPMntET8ztjJs
- This file is newer than the design doc where they differ (clone-and-fetch instead of a read-write repo mount).
- Host setup (installing Apple `container` and the Bitwarden CLI, storing the LiteLLM key): see `README.md`.
- Status as of 2026-11-12: **design only, no code yet.** The next step is the manual spike (see "Current status and next steps").

---

## 1. What sandcastle is

sandcastle runs one OpenCode instance per Git repo inside its own Apple `container` VM on macOS. The user chats with it in a browser on the Mac. The LLM key and Git credentials never enter the VM. All outbound traffic leaves through a small host-side relay that is part of the sandcastle CLI.

Owner: a single user on one Mac (Apple silicon, macOS 27). Self-hosted, personal use. Not a multi-user or team product (yet).

### Goals

- OpenCode runs with full autonomy (`"permission": {"*": "allow"}`). Safety comes from the box, not from permission prompts.
- Chat in the browser via `opencode web`. **Frictionless:** nothing to type. `sandcastle up` opens the browser already logged in.
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
| OpenCode | The agent, run as `opencode web` | opencode.ai, MIT | Pinned version, built into our own image |
| LiteLLM | Existing company model gateway | Already operated | Issues virtual keys with budget, model allowlist, expiry |
| Bitwarden CLI (`bw`) | Source of truth for secrets | bitwarden.com/help/cli, official | User unlocks it; session never stored |
| sandcastle (Rust) | CLI plus host-side relay | This repo | Small, single purpose, reviewable |

### 2.1 sandcastle CLI (Rust)

One binary on macOS. Drives `container` through its public CLI (no private APIs). Fetches secrets through the secrets module. Runs the relay while a box is up.

- Config: `~/.config/sandcastle/config.toml`. Secret *references* only, never secret values.
- Per-box state: `~/Library/Application Support/sandcastle/` (port, network name, image digest).
- No password to manage: the relay creates a random login token and a random internal OpenCode password per run, both in memory only.

### 2.2 Host relay (part of the CLI): the security core

Listens on `127.0.0.1` for the browser and on the host side of each box's internal network for the box. Three jobs:

1. **UI gateway.** Serves the OpenCode web UI on `127.0.0.1:<port>`. `sandcastle up` opens a one-time link that sets a session cookie. Every request must then carry the cookie, a `Host` of `127.0.0.1:<port>` (blocks DNS rebinding) and no foreign `Origin` (blocks other websites). When forwarding to the box, the relay adds OpenCode's internal Basic Auth password (`OPENCODE_SERVER_PASSWORD`).
2. **LLM relay.** OpenCode talks plain HTTP to `http://<gateway>:<port>/v1` with a dummy key. The relay swaps in the real LiteLLM key and forwards over HTTPS. The key exists only in relay memory. No TLS interception needed because the relay is the endpoint.
3. **Egress proxy.** All other outbound traffic goes via `HTTP_PROXY` / `HTTPS_PROXY`. The relay allows `CONNECT` only to allowlisted hosts and logs every blocked attempt.

Why the login stays (even though "all is local"): an unauthenticated OpenCode server on localhost is reachable from any website open in the browser. CVE-2026-22812 (OpenCode < 1.0.216) showed permissive CORS let any web page execute commands through OpenCode's server; since v1.1.10 the server is off by default but still unauthenticated when enabled. OpenCode's SECURITY.md states it does not sandbox the agent and its permission system is a UX feature, not isolation. The user asked for no password; the decision is an invisible login (one-time link + cookie), which is zero friction. Confirmed by the owner (question 6).

### 2.3 The box (Apple container)

- **Image:** `debian:stable-slim` pinned by digest, plus Node, Git, ripgrep and a pinned OpenCode. Built locally with `container build` from a Containerfile (OCI). Non-root user.
- **Entrypoint:** `opencode web --hostname 0.0.0.0 --port 4096` with `OPENCODE_SERVER_PASSWORD` set to the random per-run value (only the relay knows it).
- **Network:** per-box network via `container network create --internal` (host-only access). The relay is the only way out.
- **DNS:** off (`--no-dns`). Names are resolved by the relay, so DNS can't be a side channel.
- **Mounts:** a named volume at `/workspace` holding the box's own clone of the repo; OpenCode config read-only; a named volume for the agent's home (sessions, caches). **The host working tree is never mounted read-write. No macOS home mount. No SSH agent (`--ssh` stays off).**
- **Hardening:** `--read-only` root filesystem, `--tmpfs /tmp`, `--cap-drop ALL`, CPU and memory limits (`-c`, `-m`).
- **Ports:** none published. The relay reaches port 4096 over the internal network.
- A "container machine" (systemd, `container machine create`) is **not** used: with Docker out of scope, one plain container per repo is simpler and smaller. If ever used, `--home-mount` must be `none` (default is `rw`).

### 2.4 Secrets flow

1. `sandcastle up` checks `bw status`. If locked, it runs `bw unlock` interactively and keeps `BW_SESSION` in memory for that one command only.
2. Resolves the LiteLLM virtual key by Bitwarden item ID and hands it to the relay in memory.
3. The box gets only a dummy key and the relay's address.
4. `sandcastle down` stops the relay; the key is gone.

The LiteLLM virtual key must carry a monthly budget, a model allowlist and an expiry, so even a leak is cheap. The user confirmed LiteLLM supports per-user virtual keys.

### 2.5 Git

Model: clone-and-fetch. The agent never writes to the host repo, so nothing it puts into `.git` (hooks, `core.fsmonitor`, `core.pager`, `diff.external`, `core.sshCommand`) can run on the Mac, whichever Git command or editor integration touches the host repo.

- The box works on its own clone in the `/workspace` volume. It can commit but has no Git credentials, so it cannot push to the user's remotes.
- `sandcastle fetch` brings the box's commits into the host repo under `refs/remotes/sandcastle/*`. Fetching from an untrusted repo is safe; checking out or merging the result is the human review gate.
- The user reviews, merges and pushes with plain Git on macOS.
- Tradeoff accepted by the owner: the agent's working tree cannot be opened live in a macOS editor. Uncommitted work in the box is invisible to the host until committed and fetched.

Proposed mechanism (unverified, see question 7 and the spike):
- Seed: the host repo's `.git` directory is mounted read-only into the box as the clone source. Only `.git`, not the working tree, so untracked files such as `.env` stay out of the box. Check that `.git/config` holds no credentials in remote URLs before mounting.
- Fetch: the host runs `git upload-pack` inside the box through the runtime module's exec, so Git's server side also runs in the box and the host only parses the pack stream.
- Sync host to box: the agent runs `git fetch` from the read-only source mount.

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
  src/relay/                        UI gateway, LLM relay, egress proxy
  src/secrets/mod.rs                trait SecretStore
  src/secrets/bitwarden.rs
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
| `sandcastle doctor` | Checks macOS, Apple silicon, `container` version, `bw` login, LiteLLM reachability |
| `sandcastle init` | Writes `config.toml`, builds the image, records its digest |
| `sandcastle up [path]` | Unlocks `bw` if needed, starts relay, creates network + volumes on first run, seeds the box's clone, starts box, opens browser |
| `sandcastle down [path]` | Stops box and relay; keeps the home and workspace volumes |
| `sandcastle ls` | Lists boxes: repo, port, status, image digest |
| `sandcastle fetch [path]` | Fetches the box's commits into the host repo under `refs/remotes/sandcastle/*`; the user merges and pushes with plain Git |
| `sandcastle reset [path]` | Deletes box, network, home volume and workspace volume (warns about unfetched commits); host repo untouched |
| `sandcastle log [path]` | Relay log: allowed/blocked hosts, LLM request count |

`up`, in order:
1. Resolve repo root; derive box name from path.
2. `bw unlock` if locked; resolve LiteLLM key into memory.
3. Create internal network, home volume and workspace volume if missing; on first run the box clones the repo from the read-only source mount.
4. Start relay on the network's host address and on `127.0.0.1:<port>`.
5. `container run` with the flags from 2.3, dummy key, `HTTP_PROXY`/`HTTPS_PROXY`, and the random internal password.
6. Wait for the UI to answer, then open the one-time login link on `http://127.0.0.1:<port>`.

---

## 5. Security controls (what each layer blocks)

| Control | Blocks | Residual risk |
| --- | --- | --- |
| One VM per box, no home mount | Reading `~/.ssh`, Keychain, other repos, Bitwarden data | Hypervisor escape (accepted) |
| Internal network + relay as only exit | Arbitrary hosts, reverse shells, reaching the company network | Exfiltration via allowlisted hosts (keep list short) |
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
| Relay keeps its own one-time link; OpenCode 2.x pairing links are not used | Relay needs the cookie and Host/Origin checks anyway; no dependency on OpenCode's pairing |
| OpenCode v2.2.7 from the GitHub release, pinned by SHA-256 | Owner wants 2.x; it is not published to npm |
| Modules for secrets, agent, runtime | Replaceable later at minimal cost; no speculative adapters |
| Scope: owner only | Other roles dropped from scope |
| Name: sandcastle | Chosen by owner |

---

## 8. Open questions (numbered as in the design doc)

Answered on 2026-11-12 (see decisions log): 1 (no Keychain in v1), 3 (default allowlist as proposed, GitHub included), 4 (shell spike first), 5 (bundled), 6 (yes).

Still open:

2. Manual push acceptable, or should the agent open PRs itself? (Needs a fine-grained GitHub token, relay-injected, and `github.com` reachable through a method-aware relay path, not a CONNECT tunnel.)
7. Clone-and-fetch mechanism: is the proposal in 2.5 (read-only `.git` mount as source, `upload-pack` via exec for fetch) right, or should the relay proxy Git smart HTTP instead? Decide after the spike.

Do not silently decide these. Ask the owner, or implement the proposal and flag it.

---

## 9. Current status and next steps

**Status:** design complete, nothing implemented, repo freshly created.

**Step 1: manual spike (about one hour, shell only, in `spike/`).** These assumptions about Apple `container` are unverified and decide whether the design works:

- [x] A container on a `--internal` network can reach a listener on the host's address for that network, but not the internet. (Passed 2026-11-12, see `spike/NOTES.md`.)
- [x] A host listener can bind to that vmnet gateway IP only (not `0.0.0.0`) and survives sleep/wake. (Passed 2026-11-12.)
- [x] With `--no-dns` and `HTTPS_PROXY`, OpenCode installs from npm through a stock allowlisting proxy (e.g. tinyproxy) and reaches LiteLLM through a plain HTTP forwarder. (Passed 2026-11-12: npm through tinyproxy, model list through the forwarder with the key from Bitwarden. An `opencode run` round trip is still to do.)
- [ ] The relay can reach port 4096 on the box over the internal network; another LAN device cannot.
- [x] OpenCode runs with `--read-only`, tmpfs `/tmp`, a home volume and a workspace volume. (Passed 2026-11-12 with 2.2.7, after the volume ownership fix.)
- [ ] Tools that ignore proxy variables fail cleanly and the ones we need can be made to honour them (Node's built-in `fetch` needs `NODE_USE_ENV_PROXY=1`; check Bun, npm, Git).
- [ ] Clone-and-fetch works: the box clones from a read-only mount of the host `.git`; the host fetches the box's commits through `container exec ... git upload-pack` into `refs/remotes/sandcastle/*`.
- [ ] A non-root user in the box can read the read-only `.git` mount and write the named volumes (UID mapping).
- [x] A pf rule limits the box to the relay's ports on the host (added and passed 2026-11-12; `spike/09-pf-host-isolation.sh`, needs sudo). Rules must go by interface, not subnet, to cover IPv6.

Spike findings so far (details in `spike/NOTES.md`):
- OpenCode is pinned to v2.2.7 from the GitHub release (owner decision 2026-11-12; 2.x is not on npm). In 2.x there is no `opencode web`: the UI and API come from `opencode serve`, the API requires auth and accepts Basic Auth with `OPENCODE_SERVER_PASSWORD`, and `opencode pair` issues its own one-time login links (owner decision: not used, the relay keeps its own one-time link and injects Basic Auth). Sections 2.2 and 2.3 still describe 1.x and are not rewritten yet.
- A fresh named volume is root-owned, so the non-root box user cannot write it. Volumes need a one-off `chown` when sandcastle creates them.
- A box on an internal network can reach every service on the Mac that listens on `0.0.0.0`, through the gateway address and the Mac's LAN address. Decision: close this with a pf rule.
- The gateway address exists on the host only while a box is running on the network. The relay cannot bind the box-side address before `container run`, so steps 4 and 5 of `up` in section 4 must swap for that listener. Section 4 is not rewritten yet.

Fallback if the first two fail (not needed, both passed): a small proxy container attached to two networks (internal + default) instead of a host listener.

**Roadmap:**
1. Spike (above).
2. v0.1: Rust CLI with `up`, `down`, `ls`, `fetch`, `reset`; relay with UI gateway, LLM key swap, CONNECT allowlist.
3. v0.2: `log`, `doctor`; model list from LiteLLM; image rebuild with digest pinning.
4. v0.3: optional relay-injected secrets (Brave Search, GitHub token if Q2 = yes).
5. Later: background daemon and a small UI (list boxes, start/stop/open).

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
