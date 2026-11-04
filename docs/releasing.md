# Releasing PHV

English | [日本語](releasing.ja.md)

A release is published only after a security audit of everything it adds has been done and its findings fixed, and
after the documentation describes the release as it is. Steps 1 and 2 are not skipped, also for small releases.

## 1. Security audit and vulnerability assessment

Scope: every change since the last audited release (`git diff vX.Y.Z..main`), including CI workflows, the VS Code
extension and the example sites.

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

- [spec.md](spec.md) / [spec.ja.md](spec.ja.md): every new or changed feature, limit and error, and the roadmap (§18).
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

Check the draft before publishing it: download the archive for your platform, check it against `SHA256SUMS`, run
`phv --version`, and try the fixes of this release with the published binary.

## 5. Publish

Write the release notes (what is fixed, with a security section when there is one; what is new; the install table)
and publish the draft:

```bash
gh release edit vX.Y.Z --notes-file notes.md --draft=false --latest
```

Publishing runs `.github/workflows/publish-extension.yml`, which waits for approval in the `marketplace`
environment. Approve it under **Review deployments** in the run; it checks the `.vsix` against `SHA256SUMS` and
publishes it to the Visual Studio Marketplace.

It also runs `.github/workflows/docker.yml`, which downloads the release's static (musl) archives, checks them
against `SHA256SUMS`, and pushes `ghcr.io/miyagawayuu/phv:X.Y.Z` and `:latest` for amd64 and arm64 (the image was
already built and run from the same executables by the release workflow). Check that the run passed; the first
time, make the package public in its settings on GitHub (packages start private).

## 6. After the release

- Set `ARG PHV_VERSION` in `examples/demo/Dockerfile` to the new version, push it, and deploy the demo
  (`fly deploy -a phv-demo` from `examples/demo`). Check that every page of https://demo.phv-lang.org/ answers.
- The website is deployed by `.github/workflows/website.yml` when `docs/` or `website/` change on `main`.
