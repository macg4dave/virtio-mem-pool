# Releasing

How a release is built and published, and the one-time setup it needs.
CI does the work ([ci.yml](../.github/workflows/ci.yml)); a person sets up
the accounts and keys once, then tags each release.

## What a tag publishes

Pushing a tag `vX.Y.Z` that matches the `rampire` crate's version:

| Where | What |
|---|---|
| GitHub release | Host and guest RPMs and `.deb`s, the static tarballs, the source RPM, `SHA256SUMS` and its cosign (keyless) signature; the notes are `CHANGELOG.md` |
| apt repository | `https://macg4dave.github.io/RAMpire/apt`, suite `stable`, holding this release's `.deb`s; its `Release` file is signed with the project key |
| COPR | `macg4dave/rampire` builds the host and guest RPMs from the source RPM, for each chroot the project enables; COPR signs them with its own key |

The RPMs on the release are signed with the project key. CI runs only on
a tag or when started by hand (the Actions tab, or `gh workflow run CI`),
not on every commit. A run by hand signs with a throwaway key, so it still
tests the signing, the apt repository and an `apt-get install` from it,
and checks that the RPMs built from the source RPM hold the same files as
the nfpm RPMs. Run it by hand before tagging.

## One-time setup

1. **The project signing key.** On a trusted machine:

   ```bash
   gpg --quick-gen-key 'RAMpire release signing' rsa4096 sign 3y
   gpg --armor --export-secret-keys 'RAMpire release signing' > rampire-signing.asc
   ```

   Leave the passphrase empty: CI has no way to type one, and the key
   lives only in the repository secret. Keep an offline copy of the key
   and a revocation certificate (`gpg --gen-revoke`).
2. **The secret.** Repository settings → Secrets and variables → Actions:
   `RAMPIRE_SIGNING_KEY`, the contents of `rampire-signing.asc`.
3. **GitHub Pages.** Repository settings → Pages → Source: **GitHub
   Actions**.
4. **COPR.** At <https://copr.fedorainfracloud.org>, sign in and create
   the project `rampire`:
   - chroots: `epel-10-x86_64` and the current `fedora-*-x86_64`;
   - **Enable internet access during builds** on: the build fetches the
     crates `Cargo.lock` names.

   Then copy the configuration from <https://copr.fedorainfracloud.org/api/>
   into the secret `COPR_CONFIG`. The token expires after 180 days; renew
   the secret when it does.

A tag fails, before publishing anything, if either secret is missing.

## Each release

1. Set the version in `rampire/Cargo.toml` (and the other crates), and
   add its entry to `CHANGELOG.md`.
2. Commit and push, run CI by hand (`gh workflow run CI`) and wait for it
   to pass, then tag and push the tag:

   ```bash
   git tag v0.1.0
   git push origin v0.1.0
   ```
3. Check the CI run, the release, the Pages deployment and the COPR
   build.

## Installing from the repositories

Debian and Ubuntu:

```bash
curl -fsSL https://macg4dave.github.io/RAMpire/apt/rampire.asc \
  | sudo install -D -m 0644 /dev/stdin /etc/apt/keyrings/rampire.asc
sudo curl -fsSL -o /etc/apt/sources.list.d/rampire.sources \
  https://macg4dave.github.io/RAMpire/apt/rampire.sources
sudo apt update && sudo apt install rampire      # or rampire-guest
```

RHEL, AlmaLinux, Rocky Linux and Fedora:

```bash
sudo dnf copr enable macg4dave/rampire
sudo dnf install rampire                         # or rampire-guest
```

On RHEL, AlmaLinux and Rocky Linux, `dnf copr` comes from
`dnf-plugins-core`, and EPEL is not needed.
