# Security Policy

PHV is designed so that the safe way is the default way: output is escaped by
context, SQL is always parameterized, scripts only reach the files and
environment variables that `phv.toml` allows, and runaway scripts are stopped
by memory and time limits. A bug that breaks one of these guarantees is a
security vulnerability, and we want to hear about it.

## Supported versions

PHV is pre-1.0. Security fixes are made on the latest release only.

| Version | Supported |
|---|---|
| 0.8.x | Yes |
| < 0.8 | No |

## Reporting a vulnerability

Please **do not** open a public issue for a vulnerability.

Email **security@phv-lang.org** with:

- the PHV version (`phv --version`) and operating system,
- a minimal proof of concept: the `.phv` files, the `phv.toml`, and the
  request or command that triggers the problem,
- what you expected to happen and what happened instead.

We aim to acknowledge reports within 3 working days and to agree on a
disclosure date with you once the issue is understood. We will credit you in
the release notes unless you prefer otherwise.

## What counts as a vulnerability

Examples of issues we treat as security bugs:

- **Escaping bypasses**: a plain string reaching HTML output unescaped, or
  reaching a URL attribute, `<script>`, `<style>`, an event handler or
  `srcdoc` without a compile error (for example through type narrowing,
  generics, `any`, or a template the HTML context tracker misreads).
- **SQL injection**: any way to turn untrusted data into SQL text instead of
  a bound parameter.
- **Sandbox escapes**: a script reading or writing outside the directories in
  `[permissions]`, writing `.phv` / `.phv.html` source, reading environment
  variables that are not allowed, or running code that is not in the
  project's source (including through the bytecode cache).
- **Web server issues**: serving source code or hidden files, request
  smuggling or response splitting, CSRF bypasses, session fixation or
  resurrection, open redirects through `redirect()`, CSP nonce leaks.
- **Denial of service by one request**: crashing or hanging the `phv serve`
  process, or using memory or time far beyond the configured limits.
- **Tools**: `phv install`, `phv migrate`, `phv fmt` or `phv lsp` writing
  outside the paths they were given, following planted links, or running
  code from a package without being asked to.

Not vulnerabilities on their own:

- Problems that need write access to the project's source or `phv.toml`.
- Unsafe markup passed deliberately to `raw()`.
- Running with `csp = "off"`, `csrf = false` or in `--dev` mode on a public
  network, or other settings the documentation marks as unsafe.

## Known limitations

The remaining risks that are accepted by design are listed in
[docs/security-audit.md](docs/security-audit.md) (§5 and the residual risks
at the end of each later assessment), together with the results of the audits
done so far. Every release is audited before it is published; see
[docs/releasing.md](docs/releasing.md).
