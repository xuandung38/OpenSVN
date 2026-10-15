# Release setup (owner checklist)

Everything in `.github/workflows/release.yml` already works without credentials: a tag push builds
unsigned installers and leaves them in a draft release of this repository. This checklist turns on
the gated environment, code signing, updater signing and the automated publish to
`xuandung38/OpenSVN-releases`. Each part is independent and optional; do them in any order.

What the workflow does once everything below exists:

1. `build` (Windows, macOS universal, Linux) waits for a reviewer to approve environment `release`,
   then builds, signs and (macOS) notarizes the installers and signs the update artifacts.
2. `publish` waits for a second approval, creates release `vX.Y.Z` in `OpenSVN-releases` as a draft,
   uploads every file, builds `latest.json` from the asset API URLs and the `.sig` files, publishes
   the release, then commits `latest.json` to `main` of `OpenSVN-releases`.

Missing credentials never fail the run; they downgrade it, and the run says so in a notice:

| Missing | Result |
|---|---|
| Apple secrets | macOS DMG ad-hoc signed (as today) |
| Windows secrets | MSI / NSIS unsigned (as today) |
| Updater key or public key | no `.sig` / `.app.tar.gz`, `latest.json` untouched |
| `OPENSVN_RELEASES_TOKEN` | no publish; draft release in this repository instead (as today) |

A *partially* configured group (for example `APPLE_CERTIFICATE` without its password) fails the
build on purpose, so a typo never ships silently unsigned.

## 1. Environment `release` with required reviewers

GitHub: **Settings → Environments → New environment**, name `release`.

- [ ] **Required reviewers**: add yourself (and any co-maintainer). Leave "Prevent self-review" off
      while you are the only maintainer, or nobody can approve.
- [ ] **Deployment branches and tags**: *Selected branches and tags*, add rule `v*` with type
      **Tag**, and branch `main` (manual "Run workflow" runs start from `main`). Runs from any other
      branch cannot enter the environment, so they never see a secret.
- [ ] Create every secret below as an **environment secret** of `release`, never as a repository
      secret. Delete repository secrets with the same names if they exist.

Why this is enough: `release.yml` has no `pull_request` trigger, and only its two jobs bound to
`release` read secrets (`scripts/release/release-scripts.test.mjs` fails if a job reads a secret
outside the environment, or if any other workflow reads one). Environment secrets are only handed
to a job after the reviewers approve it.

If you skip this section, GitHub creates `release` on the first run without protection; the build
still works, just without the approval gate.

## 2. Releases repository and publish token

- [ ] Create the repository: **New repository**, owner `xuandung38`, name `OpenSVN-releases`,
      **Private**, tick **Add a README** (the publish needs an existing `main` branch).
- [ ] Create the publish token: **Settings (your account) → Developer settings → Personal access
      tokens → Fine-grained tokens → Generate new token**:
  - Name `opensvn-release-publish`, expiration at most 90 days (calendar the renewal).
  - Resource owner `xuandung38`, **Only select repositories** → `OpenSVN-releases`.
  - Repository permissions: **Contents: Read and write**. Nothing else (GitHub adds
    Metadata: Read-only automatically).
- [ ] Environment secret `OPENSVN_RELEASES_TOKEN` = that token.

This publish token is not the token clients use. Clients get a second fine-grained token with
**Contents: Read-only** on `OpenSVN-releases`, provisioned into each machine's OS keychain
(`updater/github-token`); see `packaging/updater/README.md` and `docs/deployment-guide.md`.

## 3. Updater signing key (minisign)

On a trusted machine, from the repository root:

```sh
pnpm install --frozen-lockfile
pnpm --dir apps/opensvn tauri signer generate -w ~/.tauri/opensvn-updater.key
# asks for a password; writes opensvn-updater.key (private) and opensvn-updater.key.pub
```

- [ ] Store the private key file and its password in the password manager (losing them means
      clients can only move to a new key through the rotation in `packaging/updater/README.md`).
- [ ] Environment secret `TAURI_SIGNING_PRIVATE_KEY` = the **content** of `opensvn-updater.key`.
- [ ] Environment secret `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` = its password.
- [ ] Environment **variable** (not secret) `OPENSVN_UPDATER_PUBKEY` = the content of
      `opensvn-updater.key.pub`. The build embeds it in the app (`plugins.updater.pubkey`); the app
      refuses any update not signed by the matching private key.
- [ ] Never commit the key files (`*.key` is git-ignored).

## 4. Apple Developer ID signing and notarization (optional)

Needs an Apple Developer Program membership.

- [ ] Create a **Developer ID Application** certificate, export it with its private key from
      Keychain Access as `.p12` with a password.
- [ ] `APPLE_CERTIFICATE` = `base64 -i DeveloperID.p12` output (one line).
- [ ] `APPLE_CERTIFICATE_PASSWORD` = the `.p12` export password.
- [ ] `APPLE_SIGNING_IDENTITY` = the certificate's common name, e.g.
      `Developer ID Application: Your Name (TEAMID1234)`.
- [ ] Notarization (needs the three above): `APPLE_ID` (Apple ID e-mail), `APPLE_PASSWORD` (an
      **app-specific password** from the Apple ID account page, not the account password),
      `APPLE_TEAM_ID` (10-character team ID).

The build then runs `codesign --verify --deep --strict` and, with notarization,
`xcrun stapler validate` on the app before collecting the DMG.

## 5. Windows Authenticode (optional)

- [ ] `WINDOWS_CERTIFICATE` = base64 of the code-signing `.pfx`
      (`[Convert]::ToBase64String([IO.File]::ReadAllBytes("sign.pfx"))`), and
      `WINDOWS_CERTIFICATE_PASSWORD` = its password. The build imports it into
      `Cert:\CurrentUser\My` of the runner before building.
- [ ] `WINDOWS_SIGN_COMMAND` = the signing command, `%1` stands for the file. It must not contain
      a password; select the imported certificate by thumbprint, for example:
      `"C:\Program Files (x86)\Windows Kits\10\bin\<sdk-version>\x64\signtool.exe" sign /fd sha256 /td sha256 /tr <timestamp-url> /sha1 <THUMBPRINT> "%1"`
      (check the SDK path on the `windows-2025` runner image). A cloud signing service whose CLI
      authenticates without a password in the command works too; then leave the two certificate
      secrets unset.

The build then checks `Get-AuthenticodeSignature` is `Valid` on the MSI and the NSIS setup.

## 6. Rehearse with a pre-release, then tag v0.1.0

- [ ] Rehearsal: `git tag v0.1.0-1 && git push origin v0.1.0-1` (MSI only accepts a *numeric*
      pre-release). Approve `build`, then `publish`. Expect a **pre-release** `v0.1.0-1` in
      `OpenSVN-releases`; `latest.json` is not written for pre-releases. Check the job summaries:
      each OS reports `signing: ...`, `updater: on`. Delete the rehearsal release and its tag in
      `OpenSVN-releases` afterwards if you like.
- [ ] Bump the version through a normal PR: `[workspace.package] version = "0.1.0"` in the root
      `Cargo.toml` and `"version": "0.1.0"` in `apps/opensvn/package.json`, merge to `main`.
- [ ] Tag the merge commit and push the tag:

  ```sh
  git checkout main && git pull
  git tag -a v0.1.0 -m "OpenSVN 0.1.0"
  git push origin v0.1.0
  ```

- [ ] Approve `build`, then `publish` in the run (**Actions → release → Review deployments**).
- [ ] Verify: release `v0.1.0` in `OpenSVN-releases` with the installers, `.sig` files and
      `SHA256SUMS-*.txt`; `latest.json` on its `main` announces `0.1.0` and lists `darwin-*`,
      `windows-x86_64*` and `linux-x86_64*` with `https://api.github.com/.../releases/assets/<id>`
      URLs.
- [ ] From the next version on, test N-1 → N on a machine that has the previous build and a read
      token in its keychain before announcing.

A published version is never overwritten: re-running a tag that already has a release (or a draft
left by a failed run) stops with an error. Delete the leftover draft in `OpenSVN-releases`, or bump
the version. `latest.json` never moves backwards.

## Local checks (no credentials)

```sh
node --test scripts/release/release-scripts.test.mjs   # manifest, publish order, secret isolation
scripts/release/test-updater-local.sh                  # real updater plugin, throw-away key
actionlint .github/workflows/release.yml
```
