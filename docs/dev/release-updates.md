# Publishing signed host updates

A release identifies its GitHub publisher at build time. Source builds without this binding do not contact a release repository. A fork that distributes its own binaries needs its own release configuration and keys.

## Configure the publisher

Set these GitHub Actions **repository variables** before pushing a release tag.

| Variable | Value |
| --- | --- |
| `ICTL_UPDATE_PUBLISHER` | Exact, case-sensitive `owner/repo` of this repository. For upstream releases use `Derrors/iphone-control`. A fork uses its own repository name. |
| `ICTL_UPDATE_PUBLIC_KEYS` | Comma-separated Ed25519 public keys. Each is the lowercase hex encoding of the raw 32-byte public key. |
| `ICTL_UPDATE_TAP` | Optional `owner/tap` Homebrew identity. For `owner/tap`, the push target is `owner/homebrew-tap`. Configure the repository-specific write deploy key in the `HOMEBREW_TAP_DEPLOY_KEY` secret. |

Create the GitHub Actions environment `release-signing`, restrict it to release tags and configure the publisher's approval rules. Store the PKCS#8 PEM signing key in its **environment secret** `ICTL_UPDATE_SIGNING_KEY`. The build jobs get only the public keys. Keep access to the private key with the designated release signer.

For example, generate the key in a private directory outside the checkout. With OpenSSL 3, this prints the public pin and leaves the private key in the chosen directory.

```sh
umask 077
openssl genpkey -algorithm ED25519 -out signing-key.pem
openssl pkey -in signing-key.pem -pubout -outform DER | tail -c 32 | od -An -v -tx1 | tr -d ' \n'
```

The stable tag must match the workspace package version, e.g. `v0.2.1` for `0.2.1`. The workflow checks the uploaded archives against the build output before signing the manifest bytes for each target. Linux and Apple Silicon macOS have independent signing jobs. During a partial release, a platform can have an archive before its signed manifest appears.

## Roll out a release

Every stable release signs one manifest per archive and, on Linux, one per Debian package (`iphone-control_X.Y.Z_<arch>.deb.manifest.json` and `.sig`): `iphone-control update` installs a .deb through `sudo apt-get` only after its signature, name, size and SHA-256 match. Any newer signed stable release can be installed from any installed version; there is no rollback command and no per-release compatibility setting (the `ICTL_ROLLBACK_*` variables are gone and can be deleted from the repository settings). An upgrade whose new daemon fails its 60-second health check still puts the old executable back. Archive manifests keep `rollback_compatible: true` and `rollback_from: "0.2.2"` only because 0.2.2's updater refuses a manifest without them (`LEGACY_UPDATER` in `packaging/release.py`).

Set `ICTL_UNATTENDED_TAG` to the same destination tag only after validating patch replacement and recovery without a connected phone, with busy clients, and on the target service managers. Each new tag requires a separate decision.

Run the signed offline installer tests with `python3 packaging/test_installer.py`. They use a temporary key, a custom prefix and isolated service manager commands. Before enabling unattended updates for a distribution, exercise a good patch and a failing daemon build through its native launchd and systemd user services. Use isolated installations and retain a working CLI for recovery.

For key rotation, publish a normally signed release with both the current and next public key embedded. After users have that release, change the environment secret to the next key. Keep previous releases' metadata immutable. Clients that missed the overlap need to acquire a newly trusted installer from their publisher. A compromised trust root requires a fresh bootstrap through a separately trusted distribution channel.

## Bootstrap and migration

Use the standalone `install.sh` **release asset** with Python 3 and OpenSSL 3 on PATH. It embeds the release publisher and its public keys. For a fork, select the fork's Releases page and check that its signing job published an installer and the target's manifest and signature. For example, after upstream publishes its first signed release, the install command is

```sh
curl -fsSL https://github.com/Derrors/iphone-control/releases/latest/download/install.sh | sh
```

A custom prefix and an explicit version can be supplied using `sh -s -- --prefix "$HOME/tools" --version v0.2.1`. A version-selected installation records a pin. `--no-service` is available for a manually started daemon. The source-tree script carries no publisher keys.

The installer stores its receipt under `$XDG_CONFIG_HOME/iphone-control/install-receipt.json`, defaulting to `~/.config/iphone-control/install-receipt.json`. It checks the canonical executable path and binary digest before uninstall or delegation to the local updater. Re-running the signed installer on an existing receipted service delegates a normal upgrade to `iphone-control update apply`.

For an older script installation, close clients and stop the daemon, then uninstall its binary and service using the original installation method. Install the signed release at the intended prefix to create the receipt. Keep the existing configuration and device data during migration. Package-managed users continue through their Homebrew tap or Debian package.

## Local acceptance coverage

`cargo test --workspace --all-features --locked` covers shared release checks, channel ownership, signatures, interrupted replacements and the CLI/Viewer contract. The transaction tests start disposable child daemons on Unix sockets. They also kill the updater between journal phases and check the executable digest and service state after recovery.

The Apple Silicon rehearsal used two builds of this workspace, temporary release keys, a local HTTPS release server and separate launchd labels, HOME, socket and installation prefix. No phone was connected. The real scheduled updater deferred for an open Viewer without downloading the archive, then installed the signed test patch. An explicit rollback (since removed) restored the original digest. A second signed candidate failed the 60-second daemon health check; the updater restored the original, answered a status request and suppressed automatic retries of that candidate. A release signed for a different source version was rejected before the archive download (the per-version rollback check, since removed). Running the generated installer again on the current version kept the same daemon PID. Both fixture services and the temporary signing keys were removed. Public GitHub release assets, the production key, the tap and native Linux user systemd still require release-environment verification.
