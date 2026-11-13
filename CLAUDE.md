# CLAUDE.md: sandcastle

Context for Claude Code working in this repo. Read this file fully before making changes.

- Repo: https://github.com/cburkhal/sandcastle
- Design doc (source of the decisions below): https://claude.ai/artifact/Fede71J8PTPMntET8ztjJs
- This file is newer than the design doc where they differ (clone-and-fetch instead of a read-write repo mount, OpenCode 2.x, pf rule, order of `up`).
- Host setup (installing Apple `container` and the Bitwarden CLI, storing the LiteLLM key): see `README.md`.
- Status as of 2026-11-13: **v0.3.0**: `git push` in the box, done by the Mac (2.5), and the signed app. Design verified by a shell spike (`spike/NOTES.md`); all four build steps are written and run (box, relay, Bitwarden, pf, `fetch`); `e2e/run.sh` checks the real binary end to end. See "Current status and next steps".

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

As built: a third box-side listener, the control port `<gateway>:4182`, takes the box's push requests (2.5). The LLM relay listens on `<gateway>:4180` and forwards only paths under `/v1/`; the egress proxy listens on `<gateway>:4181`, accepts only `CONNECT <host>:443` for exact allowlisted names, and refuses names that resolve to private or local addresses. The UI gateway listens on a free port on `127.0.0.1`. `up` asks the gateway for its models (`/v1/models`) before it creates anything, which also proves the key works, and writes them into the agent's config. The config sets `enabled_providers` to the relay's provider only, so OpenCode's built-in models (the free `opencode/*` ones, which the box cannot reach) are not offered. A further model source would be added to that list. The gateway URL must be `https://`; plain `http://` is accepted only for `127.0.0.1`. The relay log is `~/Library/Application Support/sandcastle/boxes/<box>/relay.log`; refused hosts are also printed once on the terminal.

Why the login stays (even though "all is local"): an unauthenticated OpenCode server on localhost is reachable from any website open in the browser. CVE-2026-22812 (OpenCode < 1.0.216) showed permissive CORS let any web page execute commands through OpenCode's server. In 2.2.7 the API answers 401 without credentials and 2.2.6 added origin checks, but the relay does not rely on either. OpenCode's SECURITY.md states it does not sandbox the agent and its permission system is a UX feature, not isolation. The user asked for no password; the decision is an invisible login (one-time link + cookie), which is zero friction. Confirmed by the owner (question 6).

### 2.3 The box (Apple container)

- **Image:** `debian:stable-slim` pinned by digest, plus Node, Git, ripgrep and a pinned OpenCode. Node comes from nodejs.org, pinned by version and SHA-256 (22.23.5): Debian's Node 20 ignores the proxy variables for `fetch`; `NODE_USE_ENV_PROXY` needs 22.21 or newer. OpenCode comes from the GitHub release tarball, checked against a pinned SHA-256 at build time (2.x is not on npm). Built locally with `container build` from a Containerfile (OCI). Non-root user (uid 1001 in the spike).
- **Runtime service:** `container system start` must have run since the last reboot. Every sandcastle command starts it when it is down (`ensure_started` in the runtime module), with `--enable-kernel-install` so the first start does not prompt.
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

1. `sandcastle up` checks `bw status`. If not logged in, it logs in: with the API key from the macOS Keychain entries `bitwarden-client-id` and `bitwarden-client-secret` if they exist (see `README.md`), otherwise interactively. If locked, it runs `bw unlock` interactively and keeps `BW_SESSION` in memory for that one command only.
2. Resolves every reference in the config (`bitwarden://<item>/<field>` with field `password`, `username`, `uri` or `notes`; `env://NAME` for an environment variable) and hands the values to the relay in memory. `[llm] api_key` must be a reference; `base_url` may be one. In the owner's vault the item is a Login with the gateway URL in the website field and the key in the password field.
3. The box gets only a dummy key and the relay's address.
4. `sandcastle down` stops the relay; the key is gone.

Rule set by the owner (2026-11-12): **all secrets are obtained at the start of the session, then everything runs unattended.** `up` begins with one secrets phase: Bitwarden login if needed, one vault unlock (master password), and resolving every secret reference the session will need, including any added later (for example a GitHub token). Nothing may prompt after that phase. One master password prompt per `up` is accepted. The same applies to other credentials `up` needs, such as `sudo` for the pf rule.

Every prompt must say which password it wants and why (owner request, 2026-11-12): the Bitwarden master password and the macOS login password for `sudo` look alike on a bare prompt. sandcastle prints what it is needed for and one line on why (for `sudo`: what the firewall rules allow and block) before each, for example "Bitwarden master password (to read the LLM key)" and "macOS login password (sudo, to load the firewall rules for this box)", and passes its own prompt text to `sudo -p`.

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
- Sync host to box: the agent runs `git fetch` from the read-only source mount. `up` does this once at every start; the agent's own branches are left alone.
- No host repo (owner decision, 2026-11-13): a box belongs to a folder. If the folder is not inside a Git repo, nothing is mounted from it and `/workspace/repo` starts empty; the agent can `git init` or clone there. `fetch` then needs a host repo to fetch into and says so; after `git init` in the host folder it works. A later `up` mounts the new `.git` and prints a note if the box's repo has no matching `origin`.
- As built: the clone is `/workspace/repo` and the agent starts in it. `fetch` needs the box to be running, because the workspace volume cannot be attached anywhere else. `up` refuses a repo whose `.git` is a file (linked worktree, submodule) or whose `.git/config` has an HTTP(S) URL with a user name or token, or an `extraheader`.

#### Push: the Mac publishes for the box (v0.3, owner decisions 2026-11-13)

Fetch stays the default way out. On top of it the agent can ask for a branch to be published; the box still has no credentials and never talks to the owner's remote.

- `git push origin <branch>` in the box reaches a Git remote helper (`git-remote-sandcastle`, a bash script in the read-only config mount, `bin/` on the agent's `PATH`). It sends the branch name to the relay's control port (`<gateway>:4182`, `POST /push`). Only that name crosses; it is checked against a strict pattern.
- The host then decides by the box's push mode, stored in the box record on the Mac (never in the repo): `off` (refused), `ask` (default: queued until the owner approves) or `always` (published at once).
- Publishing means: the Mac fetches from the box (as `sandcastle fetch`) and runs `git push <remote> refs/remotes/sandcastle/<branch>:refs/heads/sandcastle/<branch>` in the host repo with the owner's own Git setup. Always under the `sandcastle/` prefix, never forced, never a deletion. The remote is `origin`, or the only remote.
- The agent's `git push` returns at once. Git reports success only when the branch was published; "waiting for approval" and "off" show as a rejected push with a sentence that says what to do.
- Requests wait in `boxes/<box>/push-requests.json`, so `sandcastle push` in another terminal and the app both see them. `sandcastle push` lists them, `sandcastle push <branch> --review` fetches and shows commits and a diffstat, `sandcastle push <branch>` publishes (also without a request), `--deny` drops. `sandcastle push-mode [off|ask|always]` shows or sets the mode; it applies to a running box at once.
- **Known and accepted risk:** once a branch is on the remote, CI that runs on pushes runs the agent's code with the secrets those workflows have. `always` therefore queues any push whose new commits touch CI configuration (`CI_PATHS` in `src/core/push.rs`), and the mode carries a warning. It cannot tell what an existing workflow executes. A token in the relay with a method-aware Git path was considered and dropped: pushing from the Mac needs no new secret, works with any remote and adds no Git protocol code to the relay. Its cost: the push uses the owner's full Git identity, so the limit is sandcastle's code, not a narrow token.
- No pull request is opened. The remote's answer (often a link to open one) is shown to the owner.

**Instructions for the agent:** every chat starts with a text generated by sandcastle (`push::instructions`): what the box is, that there are no credentials, the allowed hosts, how `origin`, commits and `git push` work. It lives in the read-only config mount and is loaded through OpenCode's `instructions` setting, so it does not depend on the repo's `AGENTS.md` and the agent cannot change it. It does not depend on the push mode: the answer to `git push` says what happened. Checked by `e2e/run.sh` (the fake gateway sees the text).

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
- Anchor: one line `anchor "sandcastle/*"` in `/etc/pf.conf`, added by `init` (owner decision). `init` keeps a backup (`/etc/pf.conf.sandcastle-backup`), checks the new file with `pfctl -n` and reloads it with `pfctl -f`. Caveat, unverified: reloading the main ruleset may drop rules that system services added at run time (pfctl warns about this). macOS updates may remove it, so `doctor` checks that it is still there, and `up` refuses to start a box without it. Rules go into `sandcastle/<box>`.
- pf is enabled with a reference token (`pfctl -E`) and released with `pfctl -X` on `down`.
- Needs root, once per `up`: in the secrets phase `up` starts a small root helper through `sudo` (`src/firewall.rs`). The helper waits, loads the rules when `up` sends them and removes them once the box has stopped (Ctrl-C, or `down` from another terminal), then releases the pf token when its input closes, so no second prompt is needed. If its input closes while a box still has rules, it fails closed (see section 12). The helper ignores terminal signals so Ctrl-C cannot kill it before the cleanup.
- `up --no-firewall` skips all of this with a warning. It exists for testing without root.
- The prompt per `up` stays (owner decision, 2026-11-13). A root helper installed by `init` with a passwordless sudoers rule was offered and not chosen. Instead the owner enables Touch ID for `sudo` (`/etc/pam.d/sudo_local`, see `README.md`), so the prompt is a fingerprint. No code depends on it.

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
  e2e/                              end-to-end check of the real binary, with a fake gateway
  src/lib.rs                        the library; src/main.rs is the CLI on top of it
  src/core/session.rs               a session: secrets and the root helper, obtained once
  src/core/push.rs                  push modes, the request queue, publishing from the Mac, the agent's instructions
  src/relay/control.rs              the box's requests to sandcastle itself (publish a branch)
  ui/                               the app (Tauri 2): Rust commands in src/, the window in web/
  packaging/                        bundle script, Homebrew cask template
  .github/workflows/release.yml     signed release build (not run yet)
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
| `sandcastle push [branch] [--review\|--deny] [--path p]` | Lists the agent's push requests, or fetches a branch and pushes it from the Mac as `sandcastle/<branch>` |
| `sandcastle push-mode [off\|ask\|always] [--path p]` | Shows or sets what `git push` in the box leads to (default `ask`) |
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
| Push only through the Mac: one branch name crosses, always published under `sandcastle/`, never forced, mode set on the host | The agent changing `main` or other branches, or publishing without the owner's setting allowing it | Published code runs in CI with its secrets (mode `always`, or an approval without review) |
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
| Touch ID for `sudo` instead of a passwordless root helper (2026-11-13) | Less typing without a new passwordless path to root |
| Only the gateway's models are offered (`enabled_providers`) (2026-11-13) | OpenCode's built-in free models cannot work in the box and one was the default |
| sandcastle starts the `container` service itself (2026-11-13) | `init` and `up` failed after a reboot |
| v0.1 is released by a PR from `spike` to `main` (2026-11-13) | Owner request |
| No secrets agent and no Keychain cache for the CLI (2026-11-13) | One prompt per `up` is good enough; the app is the long-lived session instead |
| App with Tauri 2, chat stays in the browser (2026-11-13) | The app stays Rust and reuses the library; no web view that shows agent output |
| App collects the Bitwarden master password in its own field and passes it to `bw unlock --passwordenv` | `bw` can only prompt on a terminal. Cost: the password passes through sandcastle's memory once |
| App asks for `sudo` with `sudo -A` and an askpass dialog; Touch ID still comes first | Reuses the existing root helper unchanged. A registered privileged helper needs the Developer ID and comes with signing |
| A box does not need a Git repository (2026-11-13) | The agent can get or create a repo in the chat; requiring one blocked starting a box for a plain folder |
| Distribution: signed app as a Homebrew cask in a tap, CLI inside the app bundle | One install for both; owner is setting up a Developer ID |
| The admin prompt per session stays, as a fingerprint (2026-11-13) | A registered privileged helper (`SMAppService`) is the app-like answer but days of work; a passwordless sudo rule was declined again |
| A box keeps its folder (2026-11-13) | It is the source (read-only `.git`), the destination of fetch and push, and the box's name; a box without one could hand nothing over |
| Push is done by the Mac with the owner's Git setup, not by a token in the relay (2026-11-13) | No new secret, any remote, no Git protocol in the relay |
| Push modes per box: `off`, `ask` (default), `always`; published as `sandcastle/<branch>`; the agent's push returns at once (2026-11-13) | Owner decisions |

---

## 8. Open questions (numbered as in the design doc)

Answered on 2026-11-12 (see decisions log): 1 (no Keychain in v1), 3 (default allowlist as proposed, GitHub included), 4 (shell spike first), 5 (bundled), 6 (yes), 7 (clone-and-fetch as described in 2.5, confirmed after the spike).

Still open:

2. Should the agent open PRs itself? Publishing branches is answered (push through the Mac, 2.5). Opening the PR would still need a GitHub token or the `gh` CLI on the Mac; not built.
Do not silently decide these. Ask the owner, or implement the proposal and flag it.

---

## 9. Current status and next steps

**Status:** v0.1. Design verified by the shell spike; build steps 1 to 4 are written and run; `e2e/run.sh` passes without the firewall, and the owner ran it with `--firewall` and Touch ID for `sudo` (2026-11-13, reported as passed). Work happened on the `spike` branch until v0.1; after that, branch from `main`.

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
3. **Secrets phase and pf. Done 2026-11-12.** The owner ran `init` and `up` with the real vault, root and the real gateway: both prompts appeared and the browser opened on the OpenCode UI. A chat message was answered through the real gateway (owner, 2026-11-13). Verified without them: the secrets phase with an `env://` reference, `up --no-firewall`, `doctor`, and the unit tests for reference parsing, rule generation and interface lookup. Bitwarden module, the single upfront secrets phase, pf rules loaded after box start and removed on `down`/`reset`. Done when `up` prompts once at the start and then runs unattended.
4. **Git and release. `fetch` done 2026-11-12, end-to-end check and release 2026-11-13.** Verified in a throwaway repo: first `up` clones into the box without untracked files; a commit made in the box arrives under `sandcastle/<branch>`; a hook and `core.fsmonitor` planted in the box do not reach the host repo; the host working tree is untouched; a later `up` keeps the clone and sees new host commits as `origin/*`; a repo with a credential in a remote URL is refused. The end-to-end check is `e2e/run.sh` (see `README.md`): it drives the real binary with its own home folder, a fake gateway and an `env://` key. Run it before every release and after every `container` or OpenCode update.

**The app (branch `ui`, started 2026-11-13):** see 12.

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

---

## 12. The app

A second front end in `ui/` (Tauri 2), on the same library as the CLI. UI draft: https://claude.ai/artifact/GSFfGeBAmegXVPHKsgUD8Z

- **Session model:** the app is one session (`src/core/session.rs`). Unlock runs the secrets phase once: `sudo` for one root helper that serves all boxes, then the vault. Boxes start and stop inside the session (`up::plan`, `up::start`, `RunningBox::stop`). Lock or quit stops every box and drops the session, which drops the secrets and makes the helper remove all rules. The CLI uses the same code with one session per `up`.
- **Window:** static HTML, CSS and JS in `ui/web/`, no bundler and no npm dependencies. Strict CSP, no remote content, no inline script. Its only powers are the commands in `ui/src/main.rs` and the folder picker (`ui/capabilities/default.json`). Text from the system is set as text, never as HTML.
- **Chat:** opened in the default browser through a fresh one-time link (`UiGateway::new_login_url`), which replaces any unused earlier link. The link lands in the newest chat session, or a new one (`Agent::landing_path`); the login path is under `/auth/` because OpenCode's service worker answers other navigations from its cache. A box keeps its chat port across starts when the port is still free (`ui_port` in the box record).
- **Unlock and Touch ID:** `sudo -A` goes straight to the askpass program and never offers Touch ID; plain `sudo` offers it even without a terminal (checked 2026-11-13). So `Firewall::start` tries plain `sudo` first and falls back to `sudo -A` with the dialog. `Session::open` reports its steps through a callback; the window shows them next to a spinner while a command runs (`progress` command).
- **Refused pushes:** when the agent pushes while the box's mode is `off`, the app shows a notice once per branch and run of the box (`push::refused`, a list in `boxes/<box>/push-refused.json`), so the owner knows there is work to fetch.
- **Errors:** every command failure is shown as a banner with what was attempted and the cause, until dismissed or the next command. While locked, `status` reports what blocks unlocking (config, `[llm]`, pf anchor) and the window shows that instead of the form. A box that stops by itself, and boxes that could not be stopped at lock, are reported, not swallowed.
- **Box list:** each box has its own Start or Stop button in the list; the detail pane has none.
- **Design (2026-11-13, canvas: https://claude.ai/artifact/VQ8cAeHXA6UFCcWrYDoU2i):** colour says what kind of thing a block is, position says how urgent it is. Neutral: structure and settings (the selected option is inverted). Blue: primary action, running, progress. Amber: needs the owner's decision. Red: destructive and errors. The detail pane runs top to bottom: notices and errors, the box header, "Waiting for you" (the agent's push requests, each with Review, Publish and Drop), the agent's work, settings (the push mode as a three-part switch), delete (red outline at rest, filled on confirm). Unlock lists its three steps. The version is next to the name in the sidebar. Tokens are in `ui/web/style.css`, for light and dark.
- **Delete a box:** in the detail pane, with a confirmation in the window. Same effect as `sandcastle reset` (`lifecycle::delete`): the box, its network and both volumes go; a running box is stopped first. The host folder is never touched or deleted (owner question, 2026-11-13: the box's content goes, the folder stays).
- **Search path:** an app started from Finder gets only the system folders in `PATH`. The app adds `/opt/homebrew/bin` and `/usr/local/bin` at start (`search_path` in `ui/src/main.rs`), or `container` and `bw` are not found (v0.2.0 failed this way). Test the app by starting it with `open`, not from a shell.
- **Testing without root:** `sandcastle-ui --no-firewall`, same meaning and warning as the CLI flag.
- **Packaging:** `packaging/bundle.sh` builds `sandcastle.app` with the CLI at `Contents/MacOS/sandcastle`. The script sets `CI` for the bundler so the `.dmg` window is not arranged through Finder (that step needs Automation permission for the terminal and failed without it). The release workflow signs and notarizes on a version tag and attaches the `.dmg` to the GitHub release; a manual run does the same without a release. Steps: `packaging/RELEASE.md`. The tap is https://github.com/cburkhal/homebrew-tap, with a cask for the app (`packaging/homebrew/sandcastle.rb` is its template) and a formula for the CLI alone, built from source from the release tag. Both install a `sandcastle` command, so the cask declares a conflict with the formula. Each release needs the new version and checksums in both.

Verified 2026-11-13: the library refactor passes the unit tests and `e2e/run.sh`; the firewall helper's multi-box protocol passes a test with a fake `pfctl`; the app builds, starts and bundles (unsigned). v0.2.0 was released by the workflow: the app in the published `.dmg` is notarized and stapled, and Gatekeeper accepts it. The `.dmg` itself is signed but not notarized. **Not verified:** anything that needs a click (unlock, start, stop, chat, fetch in the window), the askpass dialog, the multi-box helper with real `pfctl`, installing the cask.

Not built yet: menu bar item, network view (allowed and refused hosts), unfetched-commit counts, diff review, notifications, `init` and `doctor` in the app. The Touch ID prompt for `sudo` does not get keyboard focus when the app starts it (owner had to click it once). Also open: the macOS password is asked in a separate dialog when Touch ID is not available, while the Bitwarden password has a field in the window.

**Fail closed (2026-11-13, owner decision):** if sandcastle goes away while a box still has rules (a crash, or a box that could not be stopped), the root helper does not remove the rules. It replaces them with "block everything on the box's interface", keeps pf enabled, and records the box name and its pf token in `/var/run/sandcastle-pf-blocked` and `/var/run/sandcastle-pf-tokens` (root-only, cleared at boot like pf itself). Cleanup is needed because a bridge interface name is reused: stale block rules would otherwise hit whatever gets that name next. It happens at the next session start: the helper reports the blocked boxes, sandcastle unloads every one that no longer runs, and when none is left the helper releases the kept tokens. A blocked box that still runs stays blocked until it is stopped (`sandcastle down`) and a session starts. Tested against a fake `pfctl` only.
