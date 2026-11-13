# Releasing a signed app

The release workflow (`.github/workflows/release.yml`) builds `sandcastle.app`, signs it with a Developer ID, has Apple notarize it and attaches the `.dmg` to the GitHub release. It needs six repository secrets. Set them up once, when the Apple Developer account is active.

Never paste these values into a chat, a file in the repo or a shell command line. The `gh secret set` commands below ask for the value and send it straight to GitHub.

## 1. Create the certificate

1. Open **Keychain Access** > menu Keychain Access > Certificate Assistant > **Request a Certificate From a Certificate Authority**. Enter your email and name, choose "Saved to disk". This writes a `.certSigningRequest` file.
2. On https://developer.apple.com/account/resources/certificates/add choose **Developer ID Application**, upload that file and download the `.cer`. Only the Account Holder of the team can create this type.
3. Double-click the `.cer`. It appears in Keychain Access under login > My Certificates as "Developer ID Application: <name> (<team id>)".
4. Right-click that entry > **Export**, format `.p12`, and set an export password. Save it outside the repo, for example `~/Desktop/developer-id.p12`.

## 2. Collect the six values

| Secret | Where it comes from |
| --- | --- |
| `APPLE_CERTIFICATE` | The `.p12` as base64: `base64 -i ~/Desktop/developer-id.p12 \| pbcopy` puts it on the clipboard |
| `APPLE_CERTIFICATE_PASSWORD` | The export password from step 1.4 |
| `APPLE_SIGNING_IDENTITY` | The full name of the certificate; `security find-identity -v -p codesigning` prints it |
| `APPLE_ID` | The email of your Apple Developer account |
| `APPLE_PASSWORD` | An app-specific password, not your Apple ID password: https://account.apple.com > Sign-In and Security > App-Specific Passwords |
| `APPLE_TEAM_ID` | The 10-character team id: https://developer.apple.com/account > Membership details |

## 3. Store them in the repository

Run each command; it asks for the value (paste, then Enter):

```
gh secret set APPLE_CERTIFICATE --repo cburkhal/sandcastle
gh secret set APPLE_CERTIFICATE_PASSWORD --repo cburkhal/sandcastle
gh secret set APPLE_SIGNING_IDENTITY --repo cburkhal/sandcastle
gh secret set APPLE_ID --repo cburkhal/sandcastle
gh secret set APPLE_PASSWORD --repo cburkhal/sandcastle
gh secret set APPLE_TEAM_ID --repo cburkhal/sandcastle
```

Or in the browser: the repository > Settings > Secrets and variables > Actions > New repository secret.

Check with `gh secret list --repo cburkhal/sandcastle`. Then delete the `.p12` from the Desktop; the certificate stays in your Keychain.

## 4. Release

1. Set the same version in `Cargo.toml`, `ui/Cargo.toml` and `ui/tauri.conf.json`, merge to `main`.
2. Tag and push: `git tag -a v<version> -m "sandcastle v<version>" && git push origin v<version>`.
3. The workflow run prints the version and the `sha256` of the `.dmg` at the end.
4. In the tap (https://github.com/cburkhal/homebrew-tap): copy `packaging/homebrew/sandcastle.rb` to `Casks/sandcastle.rb` with that version and checksum, and remove `Formula/sandcastle.rb` (both install a `sandcastle` command).

## Signing on this Mac instead

With the certificate in your Keychain, a local build is signed when the identity is set:

```
APPLE_SIGNING_IDENTITY="Developer ID Application: <name> (<team id>)" packaging/bundle.sh
```

Add `APPLE_ID`, `APPLE_PASSWORD` and `APPLE_TEAM_ID` to the environment to notarize as well.
