# Releasing PHV

English | [日本語](releasing.ja.md)

A release is published only after a security audit of the whole implementation has been done and its findings fixed,
and after the documentation describes the release as it is. Steps 1 and 2 are not skipped, also for small releases.

## 1. Security audit and vulnerability assessment

Scope: the whole implementation, not only the changes since the last audited release: every crate, CI workflows,
the VS Code extension, the website and the example sites. Code fixed by an earlier audit is not an exception; each
earlier fix is attacked again, including ways around it.

- Review each area on its own (for example the network clients, the database drivers, the web server, templates,
  the editor, CI and publishing) and confirm every finding with a working attack (PoC) against a release build.
- Fix every Critical, High and Medium finding. A Low finding is fixed, or listed as a residual risk with the reason.
- Add a regression test for each fix, and rerun every PoC against the fixed build.
- Check dependencies: `cargo audit` (also run by CI) and `npm audit --omit=dev` in `editors/vscode` and `website`.
- Write the results as a new section of [security-audit.md](security-audit.md) and
  [security-audit.ja.md](security-audit.ja.md): scope, method, findings with their fixes, settings for the owner,
  residual risks, and how the fixes were verified.

If a finding cannot be fixed before the release, the release waits.

## 2. Documentation

Each change updates its documentation in the same commit. Before a release, check that these describe it, in both
English and Japanese:

- [spec.md](spec.md) / [spec.ja.md](spec.ja.md): every new or changed feature, limit and error, the compatibility rules (§18) and the roadmap (§19). A change
  that breaks what §18 keeps needs a major release, or must be one of its exceptions.
- [README.md](../README.md) / [README.ja.md](../README.ja.md): features, the crate table, tests, the number of audits.
- [SECURITY.md](../SECURITY.md): the supported versions table (the latest minor version).
- [editors/vscode/README.md](../editors/vscode/README.md): editor features and the `.vsix` file names.
- The website (`website/pages.mjs`): the texts that are not generated from `docs/` (such as the number of audits).
- The READMEs of the examples that changed.

## 3. Version

Set the new version in:

- `Cargo.toml` (`[workspace.package] version`)
- `editors/vscode/package.json` and both `version` fields at the top of `editors/vscode/package-lock.json`
- the `.vsix` file names in `editors/vscode/README.md`

Then build once so that `Cargo.lock` follows, commit, push `main`, and wait until CI passes on Linux, Windows and
macOS.

## 4. Tag and draft

```bash
git tag -a vX.Y.Z -m "PHV vX.Y.Z" <commit>
git push origin vX.Y.Z
```

Only repository admins can create, move or delete `v*` tags (ruleset "release tags"). The tag runs
`.github/workflows/release.yml`, which builds the four platforms and the extension and creates a **draft** release
with `SHA256SUMS`. If a macOS job is cancelled for lack of a runner, rerun it with `gh run rerun <id> --failed`.
Each archive holds, besides `phv` and `LICENSE`, `THIRD-PARTY-LICENSES.txt`: the licence notices of the crates built
into `phv` for its platform (and of the C code some of them include), which their licences ask to go with the
binary. `packaging/licenses.py` collects them from the crates' own licence files and fails when a crate has none
(CI runs it too); the Docker image has both files in `/usr/share/licenses/phv/`.

Check the draft before publishing it: download the archive for your platform, check it against `SHA256SUMS`, run
`phv --version`, and try the fixes of this release with the published binary.

## 5. Publish

Write the release notes (what is fixed, with a security section when there is one; what is new; the install table)
and publish the draft:

For the release that adds shared OAuth state claims, include the shared-session upgrade instructions in the
[specification](spec.md). Stop all 1.2 processes before starting the new release if they share a file session directory.

```bash
gh release edit vX.Y.Z --notes-file notes.md --draft=false --latest
```

Publishing runs `.github/workflows/publish-extension.yml`, which waits for approval in the `marketplace`
environment. Approve it under **Review deployments** in the run; it checks the `.vsix` against `SHA256SUMS` and
publishes it to the Visual Studio Marketplace.

Each of these workflows checks out the release's tag (never a branch of the same name) and refuses a release that is
not the latest, so users are never moved back to an older version. Their environments (`marketplace`, `docker`,
`package-managers`) should allow deployments from `v*` tags only (Settings → Environments → Deployment branches and
tags); `package-managers` holds a token, so give it a required reviewer too.
When dispatching a deployment manually, select the same release tag as the workflow ref and the `tag` input.

It also runs `.github/workflows/docker.yml`, which downloads the release's static (musl) archives, checks them
against `SHA256SUMS`, and pushes `ghcr.io/miyagawayuu/phv:X.Y.Z` and `:latest` for amd64 and arm64 (the image was
already built and run from the same executables by the release workflow). Check that the run passed; the first
time, make the package public in its settings on GitHub (packages start private).

And `.github/workflows/package-managers.yml` writes the Homebrew formula and the Scoop manifest from the release's
`SHA256SUMS` (`packaging/render.py`) and pushes them to `miyagawayuu/homebrew-phv` (`Formula/phv.rb`) and
`miyagawayuu/scoop-phv` (`bucket/phv.json`). It uses `PACKAGE_REPOS_TOKEN` of the `package-managers` environment: a
fine-grained token with read and write access to the contents of those two repositories only. Check afterwards that
`brew install miyagawayuu/phv/phv` and `scoop install phv` install the new version.

## 6. After the release

- Set `ARG PHV_VERSION` in `examples/demo/Dockerfile` to the new version and `ARG PHV_SHA256` to the hash of
  `phv-X.Y.Z-x86_64-unknown-linux-gnu.tar.gz` in the `SHA256SUMS` checked in step 4 (the build refuses any other
  archive, so a release asset replaced later is not used), push it, and deploy the demo
  (`fly deploy -a phv-demo` from `examples/demo`). Check that every page of https://demo.phv-lang.org/ answers.
- The website is deployed by `.github/workflows/website.yml` when `docs/` or `website/` change on `main`.
  Before switching to this workflow, add `PHV_WEBSITE_API_TOKEN` (Cloudflare Pages: Edit) and
  `PHV_WEBSITE_ACCOUNT_ID` to the `website-production` environment, whose only deployment branch is `main`.
  After verifying the first production deployment through the environment, remove the old repository secrets
  `CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_ACCOUNT_ID`.
