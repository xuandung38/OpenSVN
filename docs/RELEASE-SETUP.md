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

- [ ] Create the client read token the same way: name `opensvn-updater-read`, **Only select
      repositories** → `OpenSVN-releases`, **Contents: Read-only**, nothing else.
- [ ] Check its scope before handing it to any machine:

  ```sh
  read -rs OPENSVN_UPDATER_TOKEN && export OPENSVN_UPDATER_TOKEN   # paste, Enter; not echoed
  scripts/release/check-token-scope.sh
  unset OPENSVN_UPDATER_TOKEN
  ```

  The script never prints the token and passes it to `curl`/`git` without putting it on a command
  line. It checks five things and ends with `RESULT: PASS` (exit 0) or `RESULT: FAIL` (exit 1):

  | Check | Wanted |
  |---|---|
  | `GET repos/xuandung38/OpenSVN-releases` | 200 |
  | `GET repos/xuandung38/OpenSVN-releases/contents/latest.json` | 200; 404 is accepted with a note while no stable release exists yet |
  | `GET repos/xuandung38/OpenSVN` | 404 |
  | `git ls-remote https://github.com/xuandung38/OpenSVN.git` | fails (the same command on `OpenSVN-releases` must work, as a control) |
  | `PUT repos/xuandung38/OpenSVN-releases/contents/…` | 403 |

  The write probe targets a branch that does not exist, so even a token that can write gets an
  error instead of a commit. Run it again after every token rotation.

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
      `SHA256SUMS-*.txt`; the release notes list the commits since the previous stable tag,
      grouped into Features / Bug fixes / Performance / Other changes; `latest.json` on its `main` announces `0.1.0` and lists `darwin-*`,
      `windows-x86_64*` and `linux-x86_64*` with `https://api.github.com/.../releases/assets/<id>`
      URLs.
- [ ] From the next version on, run §7 (signatures) and §8 (N-1 → N) before announcing.

A published version is never overwritten: re-running a tag that already has a release (or a draft
left by a failed run) stops with an error. Delete the leftover draft in `OpenSVN-releases`, or bump
the version. `latest.json` never moves backwards.

## 7. Confirm the signatures (manual, per release)

Do this on the downloaded files from `OpenSVN-releases`, not on the build output, so you check
what users get. First compare every file with `SHA256SUMS-<os>.txt` of the same release
(`shasum -a 256 -c SHA256SUMS-macos.txt` on macOS, `Get-FileHash <file> -Algorithm SHA256` on
Windows).

**macOS** (Developer ID + notarization configured):

- [ ] Mount the DMG, copy `OpenSVN.app` to `/Applications`, then:

  ```sh
  codesign --verify --deep --strict --verbose=2 /Applications/OpenSVN.app
  # wanted: "valid on disk" and "satisfies its Designated Requirement"
  codesign -dvv /Applications/OpenSVN.app 2>&1 | grep -E 'Identifier|Authority|TeamIdentifier|Timestamp'
  # wanted: Identifier=com.opensvn.desktop, Authority=Developer ID Application: <you> (<TEAMID>),
  #         the Apple chain below it, a secure Timestamp, TeamIdentifier=<TEAMID>
  spctl --assess --type execute --verbose=4 /Applications/OpenSVN.app
  # wanted: "accepted", "source=Notarized Developer ID"
  xcrun stapler validate /Applications/OpenSVN.app
  # wanted: "The validate action worked!"
  spctl --assess --type open --context context:primary-signature --verbose=4 OpenSVN_<version>_universal.dmg
  ```

- [ ] Double-click the app from Finder on a machine that never ran it: no "unidentified
      developer" or "cannot check for malicious software" dialog, only the normal first-launch
      confirmation.
- [ ] Without Apple secrets, `codesign -dvv` shows `Signature=adhoc` and `spctl` rejects it: that
      is the expected unsigned build, not a failure of this check.

**Windows** (Authenticode configured), in PowerShell:

- [ ] For the MSI and the NSIS setup:

  ```powershell
  Get-AuthenticodeSignature .\OpenSVN_<version>_x64_en-US.msi, .\OpenSVN_<version>_x64-setup.exe |
    Format-List Path, Status, StatusMessage, SignerCertificate, TimeStamperCertificate
  # wanted: Status = Valid, SignerCertificate.Subject = your certificate,
  #         TimeStamperCertificate not empty (the signature outlives the certificate)
  ```

- [ ] After installing, the same for the installed executable (Start menu shortcut → **Open file
      location**; NSIS installs per user under `%LOCALAPPDATA%`, the MSI under
      `C:\Program Files\`): `Get-AuthenticodeSignature <path to OpenSVN.exe>`. Wanted: `Valid`.
- [ ] Right-click the setup → **Properties → Digital Signatures**: one entry, your name,
      SHA256, with a timestamp. Running the downloaded setup shows your name as publisher, not
      "Unknown publisher" (SmartScreen reputation for a new certificate can still warn for a while).

**Updater signatures** (every OS): `latest.json` on `main` of `OpenSVN-releases` has a non-empty
`signature` for each platform, equal to the content of the matching `.sig` asset. The app checks
them itself; §8 is the end-to-end proof.

## 8. Verify the N-1 → N auto-update (manual, macOS and Windows)

Needs two published stable versions (N-1 and N, signed with the same updater key) and the read
token from §2. Run it on one macOS and one Windows machine that you can reset. The app has the
updater commands (`updater_token_set`, `update_check`, `update_install`); if the Settings screen
for them is not in N-1 yet, provision the token with the keychain commands in
`docs/deployment-guide.md` ("Provisioning the update token") and test from the first version that
ships the screen.

Prepare, once per machine:

- [ ] Uninstall any OpenSVN, then install **N-1** from its release (macOS: DMG → `/Applications`;
      Windows: run the **NSIS setup** for one pass and the **MSI** for a second pass, since each
      updates through its own artifact).
- [ ] Launch N-1, open About and note the version. Add a working copy, make one local change and
      save one server password, so you can check that data and keychain entries survive.
- [ ] Store the read token through Settings (or the keychain command). The token status reads
      "set"; the token itself is never shown again.

Update:

- [ ] Check for updates. Wanted: "version N available" with the notes of release N (the grouped
      list from `--notes-from-git`).
- [ ] Install. macOS: the app downloads the `.app.tar.gz`, replaces `/Applications/OpenSVN.app`
      and restarts. Windows: the app downloads the MSI or setup matching how it was installed, runs
      it (UAC prompt for the MSI) and restarts.
- [ ] About shows **N**. Check for updates again: "up to date".
- [ ] The working copy is still listed with its local change, the saved password still works
      (no new login prompt), settings are unchanged.
- [ ] macOS: run the §7 `codesign`/`spctl` commands on the updated `/Applications/OpenSVN.app`;
      Windows: `Get-AuthenticodeSignature` on the installed `OpenSVN.exe`. Both still `Valid`
      / `accepted`.

Negative checks (one machine is enough):

- [ ] Clear the token: the check reports updates as not configured and makes no network request.
- [ ] Store a token without access (or an expired one): the check fails with a clear error, the
      app keeps running N-1.
- [ ] Offline: the check fails cleanly and can be retried.

Record the OS versions, N-1, N and the results in the release issue.

## Bundle identifier (decided)

The owner confirmed `com.opensvn.desktop` on 2026-10-16 as the permanent release identifier
(`tauri.conf.json`, `tauri.bundle.conf.json`, `db/data_dir.rs`). The phase-14 plan's
`vn.opensvn.app` is superseded. Do not change it: after the first public release any change
breaks installed clients.

- **Updates.** The updater replaces the installed app in place, so a new identifier would start
  next to the old one's leftovers: macOS ties the designated requirement, notarization ticket,
  Gatekeeper and privacy (TCC) approvals to it, and Windows keeps the WebView2 profile under
  `%LOCALAPPDATA%\<id>`.
- **Keychain and data.** The app data directory (`~/Library/Application Support/<id>`,
  `%APPDATA%\<id>`, `~/.local/share/<id>`) is named after it, and macOS keychain access is
  granted per signed identifier, so saved passwords, settings and the updater token would be lost.

## Local checks (no credentials)

```sh
node --test scripts/release/release-scripts.test.mjs   # manifest, publish order, notes, token check, secret isolation
scripts/release/test-updater-local.sh                  # real updater plugin, throw-away key
actionlint .github/workflows/release.yml
```
