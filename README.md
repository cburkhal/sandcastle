# sandcastle

One OpenCode instance per Git repo, each in its own Apple `container` VM on macOS. The LLM key and Git credentials never enter the VM.

Status: design plus a shell spike. No Rust code yet. The design lives in [CLAUDE.md](CLAUDE.md), the spike results in [spike/NOTES.md](spike/NOTES.md).

## Requirements

- Apple silicon Mac, macOS 26 or newer (tested on macOS 27.0.1).
- Apple `container` (tested with 1.7.1).
- Bitwarden CLI `bw` (tested with 2026.10.1).
- Homebrew, for `bw` and for the spike's proxy.
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

Log in once. Only if your vault is not on bitwarden.com (EU cloud or self-hosted), set the server first:

```
bw config server https://<bitwarden-host>     # only for EU cloud or self-hosted
bw login
```

`bw login` stores an encrypted copy of the vault under `~/Library/Application Support/Bitwarden CLI/`. The vault stays locked until `bw unlock`, and the unlock session key is only ever held in an environment variable.

## Put the LiteLLM key into Bitwarden

1. In LiteLLM, create a virtual key for sandcastle with all three limits, so a leaked key stays cheap:
   - a monthly budget,
   - a model allowlist,
   - an expiry date.
2. In Bitwarden, create a login item (for example "sandcastle LiteLLM") and put the key into the **password** field.
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

OpenCode is pinned to an exact version. The spike uses `opencode-ai` **1.19.5**, which was the `latest` release on npm on 2026-11-12. There is no 2.x release of the npm package: the highest published version is 1.19.5, and the other tags (`next`, `beta`, `dev`, `tui-v2`) are `0.0.0-...` snapshot builds, not releases. Check with:

```
npm view opencode-ai dist-tags.latest
```

Do not use versions before 1.0.216 (CVE-2026-22812).

## Running the spike

The spike checks the assumptions the design rests on. See [spike/NOTES.md](spike/NOTES.md) for what each script tests, how to run it and the results so far.

Extra requirement for the spike only: `brew install tinyproxy` (items 3 and 6). Remove it afterwards with `brew uninstall tinyproxy`.

Item 9 loads pf rules with `sudo` into the anchor `com.apple/sandcastle-spike` and removes them on exit. It does not edit `/etc/pf.conf`.
