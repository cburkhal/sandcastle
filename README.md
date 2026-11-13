# sandcastle

One OpenCode instance per Git repo, each in its own Apple `container` VM on macOS. The LLM key and Git credentials never enter the VM.

Status: v0.1. The design is verified by a shell spike and an end-to-end check. The CLI starts a hardened box and the relay: you can chat with OpenCode in the browser, and the box reaches only the LLM gateway and the allowlisted hosts. The design lives in [CLAUDE.md](CLAUDE.md), the spike results in [spike/NOTES.md](spike/NOTES.md).

## Requirements

- Apple silicon Mac, macOS 26 or newer (tested on macOS 27.0.1).
- Apple `container` (tested with 1.7.1).
- Bitwarden CLI `bw` (tested with 2026.10.1).
- Homebrew, for `bw`, Rust and the spike's proxy.
- Rust, to build sandcastle (`brew install rust`; tested with 1.99.1).
- Admin rights: the firewall (pf) rules that keep the box away from other services on the Mac need `sudo`, once in `init` and once per `up`. With Touch ID for `sudo` (below) that is a fingerprint, not a typed password.

## Install Apple `container`

`container` is Apple's open-source tool that runs each Linux container in its own lightweight VM. Source and releases: https://github.com/apple/container

1. Download the signed installer package (`container-<version>-installer-signed.pkg`) from the [releases page](https://github.com/apple/container/releases). Use the version above or newer.
2. Open the package and follow the installer. It asks for an admin password and installs under `/usr/local`.
3. Check it:

   ```
   container --version
   ```

There is no need to run `container system start`: sandcastle starts the service when it is down (after a reboot, for example) and lets it download the default Linux kernel on first start.

Later:

- Update: `/usr/local/bin/update-container.sh` (latest release), or `-v <version>` for a specific one.
- Remove: `/usr/local/bin/uninstall-container.sh -d` deletes the tool and its data, `-k` keeps the data.

`container` flags change between releases. sandcastle is tested against the version listed above; after an update, re-run the spike scripts before trusting it.

## Touch ID for sudo (optional)

`sandcastle up` needs root once, to load the firewall rules. To confirm with a fingerprint instead of typing the macOS password, enable Touch ID for `sudo` once. macOS keeps this file across updates:

```
sed 's/^#auth/auth/' /etc/pam.d/sudo_local.template | sudo tee /etc/pam.d/sudo_local
```

The file must then contain the line `auth       sufficient     pam_tid.so`. To undo it: `sudo rm /etc/pam.d/sudo_local`. This changes `sudo` for every terminal on the Mac, not only for sandcastle. With the lid closed or over SSH, `sudo` falls back to the password.

## Install the Bitwarden CLI

sandcastle reads secrets from Bitwarden through `bw`. Nothing secret is written to disk or to config files.

```
brew install bitwarden-cli
bw --version
```

Log in once. Only if your vault is not on bitwarden.com (EU cloud or self-hosted), set the server first with `bw config server https://<bitwarden-host>`.

Either log in interactively with `bw login`, or with a personal API key kept in the macOS Keychain (create the key in the Bitwarden web vault under Settings > Security > Keys, and store the two values once with `security add-generic-password -a "$USER" -s bitwarden-client-id -w` and the same for `bitwarden-client-secret`):

```
export BW_CLIENTID=$(security find-generic-password -a "$USER" -s "bitwarden-client-id" -w)
export BW_CLIENTSECRET=$(security find-generic-password -a "$USER" -s "bitwarden-client-secret" -w)
bw login --apikey
unset BW_CLIENTID BW_CLIENTSECRET
```

### Login, unlock and the master password

Bitwarden has two separate steps:

- **Login** proves who you are and downloads the vault to `~/Library/Application Support/Bitwarden CLI/`, still encrypted. The API key is enough for this, and it lasts until `bw logout`.
- **Unlock** decrypts the vault. That needs your **master password**: the one you type to open your Bitwarden vault in the browser or app. The API key cannot do this, by design, so the API key in the Keychain is not a copy of your secrets.

So a master password prompt appears whenever something needs a secret and the vault is locked. `bw unlock` returns a session key; whoever holds it can read the vault until `bw lock` or until the shell that holds it closes.

How often you type it:

- Default: once per command that needs secrets, for example once per `sandcastle up`. The session key lives only in that process.
- Once per terminal: `export BW_SESSION="$(bw unlock --raw)"`. Everything started from that terminal can then read the vault without asking. Convenient, but that includes any other program you run there.
- Never: possible only by storing the master password itself (Keychain plus `bw unlock --passwordenv`). Not recommended and not used here: it puts the key to the whole vault on disk, which the design avoids.

## Put the LiteLLM key into Bitwarden

1. In LiteLLM, create a virtual key for sandcastle with all three limits, so a leaked key stays cheap:
   - a monthly budget,
   - a model allowlist,
   - an expiry date.
2. In Bitwarden, create an item of type **Login** (for example "sandcastle LiteLLM") and put the key into the **password** field and the gateway URL into the **website** field. Leave the username empty. Not a secure note: the password field is masked in the apps and is what `bw get password` and the `bitwarden://<item-id>/password` reference read.
3. Note the item ID:

   ```
   export BW_SESSION="$(bw unlock --raw)"
   bw list items --search "sandcastle LiteLLM" | python3 -c 'import json,sys; [print(i["id"], i["name"]) for i in json.load(sys.stdin)]'
   ```

The config will reference the key as `bitwarden://<item-id>/password`. Never commit the item ID, the key or the LiteLLM hostname; the repo uses the placeholders `<item-id>` and `<litellm-host>`.

To use the key in one command without it touching disk or shell history:

```
LITELLM_KEY="$(bw get password <item-id>)" some-command
```

## OpenCode version

OpenCode is pinned to an exact version and taken from the GitHub release, not from npm: **v2.2.7** (https://github.com/anomalyco/opencode/releases/tag/v2.2.7).

- OpenCode 2.x is not published to npm. `opencode-ai` on npm stops at 1.19.5, and GitHub still marks v1.19.5 as "latest".
- The image build downloads `opencode-linux-arm64.tar.gz` and fails unless its SHA-256 matches the pinned value from the release's `SHA256SUMS` file. See `spike/Containerfile`.
- To move to another release, change `OPENCODE_VERSION` and `OPENCODE_SHA256` together and re-run the spike. Do not go below 2.2.6 (it added origin checks on the server API).

## Install with Homebrew

```
brew install cburkhal/tap/sandcastle
```

This builds the command-line tool from the latest release tag and installs the Bitwarden CLI with it. Apple `container` is still the separate installer above. Then continue with `sandcastle init` below, without the `./target/release/` prefix.

## Build and use

To build from a checkout instead, run these in the repository root:

```
cargo build --release
./target/release/sandcastle init
```

`init` writes `~/.config/sandcastle/config.toml`, adds one line (`anchor "sandcastle/*"`) to `/etc/pf.conf` and builds the box image. It asks for your macOS password for the `/etc/pf.conf` change and keeps a backup next to it.

Edit `~/.config/sandcastle/config.toml`:

```toml
[llm]
base_url = "https://<litellm-host>"
api_key  = "bitwarden://<item name or id>/password"
```

Then, in any Git repository (use the full path to the binary, or put it on your `PATH`):

```
sandcastle up
```

`up` asks for two passwords at the start and says which is which:

- your **macOS login password** or Touch ID (sudo), to load the firewall rules for the box,
- your **Bitwarden master password**, to read the key.

After that nothing prompts. It checks the gateway and the key, starts the box and the relay, and opens the browser on a one-time login link. OpenCode offers only the models your gateway lists; its built-in providers are switched off. Ctrl-C stops the box and removes the firewall rules.

```
sandcastle up --no-browser            # print the login link instead of opening it
sandcastle ls                         # boxes, status, address
sandcastle down                       # stop the box from another terminal; volumes are kept
sandcastle reset                      # delete the box with its network and volumes
sandcastle doctor                     # check config, image and the pf anchor line
```

### The app (early)

A window on top of the same code as the CLI: unlock once, then start, stop, open the chat and fetch for any number of repositories. Quitting or "Lock and stop all" stops every box, drops the secrets and removes the firewall rules.

```
cargo run -p sandcastle-ui                # run it from the source tree
packaging/bundle.sh                       # build sandcastle.app with the CLI inside
```

- Unlocking asks for the Bitwarden master password in the app's own field, because the Bitwarden CLI can only prompt on a terminal. The app hands it to `bw unlock` once and does not keep it. The Bitwarden CLI must already be logged in (`bw login`, or the API key in the Keychain).
- `sudo` asks through Touch ID if you enabled it, otherwise through a macOS password dialog.
- The chat opens in your default browser, as with the CLI.
- `packaging/bundle.sh` needs the Tauri bundler: `cargo install tauri-cli --version "^2" --locked`. Until the release is signed with a Developer ID, the built app is unsigned: open it with right-click > Open.
- Planned install of the app, once signed releases exist: `brew install --cask cburkhal/tap/sandcastle` (template in `packaging/homebrew/`).

### Getting the agent's work

The agent never writes to your repo. It works on its own clone inside the box, made from your repo's committed history; untracked files such as `.env` stay on your Mac. While the box is running, from another terminal in the repo:

```
sandcastle fetch                      # the agent's branches appear as sandcastle/<branch>
git log -p HEAD..sandcastle/<branch>  # review
git merge sandcastle/<branch>         # take the changes
```

`fetch` checks nothing out and merges nothing. Review before you merge: treat the commits like a pull request from a stranger. Work the agent has not committed stays in the box. New commits of yours reach the box at the next `up`, or when the agent runs `git fetch origin`.

`up` refuses a repo whose `.git/config` contains a token in a remote URL, because the box can read that file.

A box does not need a Git repository. In a folder that is not inside one, `up` starts a box with an empty workspace; nothing from the folder is in it. The agent can create a repository there or clone a public one. To get its commits out, run `git init` in the folder on your Mac, then `sandcastle fetch`.

While a box runs, hosts it tried to reach but was refused are printed once in the `up` terminal. The full relay log is `~/Library/Application Support/sandcastle/boxes/<box>/relay.log`.

`up --no-firewall` starts without root and without firewall rules. The box can then reach other services on your Mac; use it for testing only.

What exists on the Mac afterwards: the config file, state under `~/Library/Application Support/sandcastle/`, the image `sandcastle-opencode:<version>`, and per repo one network and two volumes named after the box. `reset` removes the per-repo parts.

## End-to-end check

```
e2e/run.sh              # no root, no prompts
e2e/run.sh --firewall   # also checks the pf rules; asks for sudo once
```

It builds the binary, starts a box for a throwaway repo and checks from outside what the design promises: the UI login and its Host and Origin checks, the hardened box, the clone without untracked files, the egress allowlist, the key swap in the LLM relay, `fetch`, and that stop and `reset` leave nothing behind. It runs with its own home folder, a local fake gateway and a key from an environment variable, so your config, boxes and vault are not touched. It needs the box image (`sandcastle init`) and internet access for the npm check. Run it after every update of `container` or OpenCode.

## Running the spike

The spike checks the assumptions the design rests on. See [spike/NOTES.md](spike/NOTES.md) for what each script tests, how to run it and the results so far.

Extra requirement for the spike only: `brew install tinyproxy` (items 3 and 6). Remove it afterwards with `brew uninstall tinyproxy`.

Item 9 loads pf rules with `sudo` into the anchor `com.apple/sandcastle-spike` and removes them on exit. It does not edit `/etc/pf.conf`.
