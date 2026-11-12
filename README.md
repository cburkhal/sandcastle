# sandcastle

One OpenCode instance per Git repo, each in its own Apple `container` VM on macOS. The LLM key and Git credentials never enter the VM.

Status: early. The design is verified by a shell spike. The CLI starts a hardened box and the relay: you can chat with OpenCode in the browser, and the box reaches only the LLM gateway and the allowlisted hosts. Not built yet: reading the key from Bitwarden by itself, the pf firewall rules, and `sandcastle fetch`. Until the pf rules exist, the box can reach other services running on your Mac. The design lives in [CLAUDE.md](CLAUDE.md), the spike results in [spike/NOTES.md](spike/NOTES.md).

## Requirements

- Apple silicon Mac, macOS 26 or newer (tested on macOS 27.0.1).
- Apple `container` (tested with 1.7.1).
- Bitwarden CLI `bw` (tested with 2026.10.1).
- Homebrew, for `bw`, Rust and the spike's proxy.
- Rust, to build sandcastle (`brew install rust`; tested with 1.99.1).
- Admin rights: the pf rule that keeps the box away from host services needs `sudo`.

## Install Apple `container`

`container` is Apple's open-source tool that runs each Linux container in its own lightweight VM. Source and releases: https://github.com/apple/container

1. Download the signed installer package (`container-<version>-installer-signed.pkg`) from the [releases page](https://github.com/apple/container/releases). Use the version above or newer.
2. Open the package and follow the installer. It asks for an admin password and installs under `/usr/local`.
3. Start the system service. On first start it offers to download a Linux kernel; accept.

   ```
   container system start
   ```

4. Check it:

   ```
   container --version
   container system status
   ```

Later:

- Update: `/usr/local/bin/update-container.sh` (latest release), or `-v <version>` for a specific one.
- Remove: `/usr/local/bin/uninstall-container.sh -d` deletes the tool and its data, `-k` keeps the data.

`container` flags change between releases. sandcastle is tested against the version listed above; after an update, re-run the spike scripts before trusting it.

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

## Build and use

```
cargo build --release
./target/release/sandcastle init      # writes ~/.config/sandcastle/config.toml, builds the box image
```

Edit `~/.config/sandcastle/config.toml`: set `[llm] base_url` to your gateway and adjust the `[egress] allow` list.

```
cd <a Git repository>
export BW_SESSION="$(bw unlock --raw)"
SANDCASTLE_LLM_KEY="$(bw get password <item-id>)" sandcastle up
```

`up` checks the gateway and the key, starts the box and the relay, and opens the browser on a one-time login link. Ctrl-C stops the box. The environment variable is a stopgap until sandcastle reads the key from Bitwarden itself.

```
sandcastle up --no-browser            # print the login link instead of opening it
sandcastle ls                         # boxes, status, address
sandcastle down                       # stop the box from another terminal; volumes are kept
sandcastle reset                      # delete the box with its network and volumes
```

While a box runs, hosts it tried to reach but was refused are printed once in the `up` terminal. The full relay log is `~/Library/Application Support/sandcastle/boxes/<box>/relay.log`.

What exists on the Mac afterwards: the config file, state under `~/Library/Application Support/sandcastle/`, the image `sandcastle-opencode:<version>`, and per repo one network and two volumes named after the box. `reset` removes the per-repo parts.

## Running the spike

The spike checks the assumptions the design rests on. See [spike/NOTES.md](spike/NOTES.md) for what each script tests, how to run it and the results so far.

Extra requirement for the spike only: `brew install tinyproxy` (items 3 and 6). Remove it afterwards with `brew uninstall tinyproxy`.

Item 9 loads pf rules with `sudo` into the anchor `com.apple/sandcastle-spike` and removes them on exit. It does not edit `/etc/pf.conf`.
