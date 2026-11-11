# PHV Security Audit Report

English | [日本語](security-audit.ja.md)

This report consolidates twenty-one audits. §1–§6 cover the first audit (and the resolution of its residual risks), §7 covers the second vulnerability assessment,
§8 covers the parts excluded from the second assessment (`phv lsp`, `phv migrate`, bytecode cache), §9 covers what was added up to v0.4.0, §10 the follow-up on the residual risks, §11 what was added for v0.5.0, §12 the follow-up on its residual risks, §13 what was added for v0.5.2, §14 what was added for v0.6.0, §15 what was added for v0.7.0, §16 the follow-up on compressing proxies (v0.7.1), §17 what was added for v0.8.0, §18 what was added for v0.9.0, §19 and §20 the follow-up on two outside reviews of v0.9.0 (v0.9.1), §21 an assessment of the whole implementation before the work on 1.0, §22 an assessment of the whole implementation before 1.0.0, §23 an assessment of the whole implementation after the processing work, §24 an assessment of the whole implementation before 1.1.0, §25 an assessment of the whole implementation before 1.1.1, §26 an assessment of the whole implementation before 1.2.0, §27 an assessment of the whole implementation of the 1.2.0 release candidate, §28 an assessment of the whole implementation before 1.2.1, §29 an assessment of the whole implementation before 1.3.0, §30 an independent review of the 1.3.2 candidate, and §31 an assessment of the whole implementation before 1.3.2.

Date: 2026-11-02 (§9–§13: 2026-11-03, §14–§19: 2026-11-04, §20–§22: 2026-11-05, §23–§25: 2026-11-06, §26–§27: 2026-11-07, §28: 2026-11-08, §29: 2026-11-09, §30: 2026-11-10, §31: 2026-11-11)
Scope: All crates of the PHV implementation (parser, type checker, compiler, VM, JIT, standard library, web server, CLI tools)

## 1. Method

1. **Three independent reviews.** To avoid inheriting the implementer's assumptions, three reviewers with separate areas read the source
   and confirmed each issue by running actual attack code (PoC).
   - A: HTTP layer (routing, static files, sessions, CSRF, multipart, headers)
   - B: Runtime (VM, JIT, memory and time limits, standard library, file permissions, SQLite)
   - C: Language guarantees and tools (escaping, type-system soundness, `Html`/`Sql` types, module loading, `phv install`/`migrate`/`lsp`)
2. **Fixes and re-verification.** All findings were fixed, and the reviewers' PoCs were rerun against the fixed build.
3. **Automated tests.** Regression tests (`tests/cases/err_security.phv`, `security.phv`) were added to the golden tests.
   In addition, HTTP integration tests against a real server and fuzzing with mutated programs (about 22,000 cases locally)
   run on every CI build.

## 2. Summary of Results

| Severity | Count | Fix |
|---|---:|---|
| Critical | 3 | All fixed |
| High | 9 | All fixed |
| Medium | 10 | All fixed |
| Low / Info | 18 | All fixed (2 initially accepted, then fixed in the follow-up in §4) |
| Found during follow-up | 1 | Fixed (N1 in §4, Medium) |

All Critical and High findings were fixed, and PoCs verified that they no longer reproduce. The items listed as "residual risks" in the initial report
were also resolved in the follow-up in §4, except those accepted by design (§5).

## 3. Findings and Fixes

### Critical

| # | Finding | Fix | Verified |
|---|---|---|---|
| C1 | On Windows, `.phv` sources, templates, and dotfiles were served verbatim via aliases such as `/INDEX.PHV`, `/index.phv::$DATA`, and 8.3 short names (`/DASHBO~1.PHV`, `/ENV~1`) | Serve only when every component of the normalized path has **exactly the same spelling** as the requested path. Reject paths containing `:`. Never treat `.phv` / `.phv.html` / files starting with `.` as static files | All patterns return 404 on a real server; added to unit tests |
| C2 | Dropping a list nested 300,000 levels deep overflowed the stack during recursive deallocation and crashed the whole server process (`catch_unwind` cannot stop it) | Made `Value` deallocation and `==` comparison iterative | Completes normally at 1,000,000 levels (test `deep_values_do_not_overflow_the_stack`) |
| C3 | A failed huge allocation, such as `"a"->padStart(10^13, "b")`, aborted the process | Built-ins whose output can exceed their input check the limit with `reserve()` before allocating (pad, repeat, replace, chars, split, join, concat, range, file reads, string concatenation) | Raises an uncatchable fatal error (memory limit); the process stays alive |

### High

| # | Finding | Fix | Verified |
|---|---|---|---|
| H1 | Narrowing a generic type argument with `is` produced `never`, letting any value pass as `Sql` or `Html` (SQL injection, XSS without `raw()`) | Narrowing a type argument or `any` yields the tested type. Narrowing accounts for interfaces and `Sql`, and is not applied when the result would be `never`. `Html` concatenation also escapes plain strings at runtime | PoC fails to compile |
| H2 | Property narrowing persisted even when a call occurred on the right side of `&&` / `\|\|` or inside `render()` | Advance a generation number on every call and discard property facts across calls | All 4 PoCs fail to compile |
| H3 | `.php` and similar files under `public/` were served raw | Serve only known static extensions (css, js, images, fonts, etc.) | `/config.php` returns 404 |
| H4 | Anonymous GETs created server-side sessions for CSRF tokens; at 100,000 sessions the store filled up, and new visitors could not log in or submit forms | Make the CSRF token an HMAC of a random per-browser cookie, keeping no server-side state. The session store has count and byte limits and evicts the oldest entries first | Unit test (new visitors still get a session past the limit) |
| H5 | No timeout for receiving headers and body (slowloris), and no connection limit | Headers 10 s (including keep-alive idle time), body 30 s, 1024 concurrent connections | Partial headers are cut off after 10 s |
| H6 | The memory limit was mostly ineffective inside built-ins | Check on every built-in call, `reserve()` before allocating, SQLite string-length and heap limits | With a 16 MB limit, a PoC allocating 1 GB is stopped |
| H7 | SQLite queries ignored the execution time limit | Abort at the deadline via SQLite's progress handler | An infinite recursive CTE stops after 2 s |
| H8 | The execution time limit counted instructions, so repeating slow built-ins stretched it many times over | Check the clock on every built-in call | |
| H9 | A dangling symbolic link (target does not exist) placed in a writable directory allowed writing files outside the sandbox | Reject symbolic links and reparse points (junctions), and operate on the resolved path | Both links and junctions are rejected |

### Medium

| # | Finding | Fix |
|---|---|---|
| M1 | Escaping ignored context (unquoted attributes, `href="javascript:..."`, `<script>`, `<style>`, event attributes) | Determine the HTML context of each `<?= ?>` at parse time, and make inserting a string into an unsafe location a compile error. Added `safe_url()` and `json_html()` |
| M2 | `response()->type("image/svg+xml")` or `"text/plain, text/html"` disabled escaping | Escaping is disabled only for allowlisted types (`text/plain`, `application/json`, `text/csv`, etc.). Reject Content-Type values containing commas |
| M3 | `render()` could read any file in the project, such as `.env`. Junctions also allowed reading outside the project | Templates must be `.phv.html`. Verify that the normalized path is inside the project or a package |
| M4 | `phv install` ran git inside an existing `vendor/<name>/.git`, executing bundled hooks | Always clone fresh into a temporary directory, disable hooks and fsmonitor, and restrict protocols with an allowlist. Lock-file commits accept only 40-digit hex |
| M5 | `phv migrate` commented out `require 'auth.php'` and reported "passes type check" (auth checks silently disappear) | Emit an uncompilable `TODO_migrate(...)` call instead. Overwriting existing files requires `--force` |
| M6 | Scripts could set `Content-Length` and similar headers, desynchronizing keep-alive responses (response splitting) | Reject framing-related headers in both built-ins and the server |
| M7 | Development mode (detailed error display) was the default | Development mode is the default only when listening on loopback. Can be set explicitly with `--dev` / `--prod` |
| M8 | SQLite `PRAGMA temp_store_directory` and similar could change process-wide settings or write to other directories | Reject `ATTACH`/`DETACH` and dangerous pragmas via an authorizer |
| M9 | Out-of-range `slice()` caused an internal panic | Use saturating arithmetic. Check integer overflow in release builds too |
| M10 | Uploads could be saved as `.phv`, and `write = ["."]` allowed rewriting source | `.phv` cannot be written. Paths under `public/` and the project root cannot be listed in `[permissions].write` |

### Low / Info (main items)

- **Upload filenames**: Strip `:`, Windows reserved names, and trailing dots (fixed).
- **Sessions**: Login (`regenerate()`) also rotates the CSRF cookie; 24-hour absolute expiry; `__Host-` prefix and HSTS over HTTPS.
  Responses with a session get `Cache-Control: private, no-store` (fixed).
- **Redirects**: `redirect()` accepts only same-site paths. External redirects use `redirectExternal()` (open-redirect defense).
- **Path dependencies**: Absolute and UNC paths are not allowed. This prevents SMB connections triggered just by opening a project (fixed).
- **Bytecode cache**: The JIT validates register, jump, and call ranges in loaded bytecode (fixed).
  The cache itself is treated as having the same trust level as source code (accepted by design).
- **`password_verify`**: Added a limit on the cost parameters of stored hashes (fixed).
- **Other**: Files starting with `_` are not exposed as pages. Status 1xx is rejected. JSON `\u` escapes are parsed strictly.
  LSP header length is limited, and UNC URIs are rejected. All fixed.

## 4. Follow-up Fixes (Resolving Residual Risks)

Items accepted in the initial report were revisited and fixed.

| # | Previous risk | Fix | Verified |
|---|---|---|---|
| R1 | A hard link to an external file placed in a writable directory allowed reading and writing that file | Check the link count on the opened file handle, and reject unless it is 1 (for both reads and writes). Also reject non-regular files (directories, FIFOs, devices) | Reads and writes via a hard link to a real file raise `PermissionError`; the outside file is unchanged |
| R2 | A directory could be swapped for a link between the path check and the open (TOCTOU) | New `safefs` module. Opens one component at a time from the allowed directory's handle, rejecting links. Unix uses `openat` + `O_NOFOLLOW`; Windows opens with `FILE_FLAG_OPEN_REPARSE_POINT` and keeps each directory open without `FILE_SHARE_DELETE`, preventing rename, deletion, or swapping while in use. Writes open without truncation, check, then truncate | Windows: all access via junctions and symbolic links rejected; a unit test confirms a directory cannot be renamed while its handle is held. Linux (Docker): tests reject symbolic links, FIFOs, and `o+w` directories |
| R2' | SQLite opens files by name, so the R2 approach cannot be used as is | Pin the directory by keeping it open, and before and after opening verify that the database and `-journal` / `-wal` / `-shm` are "absent, or regular files without links". Pass `SQLITE_OPEN_NOFOLLOW`. On Unix, reject directories writable by other users (`o+w`) | `Database::open` via a junction is rejected |
| N1 | (Found in this round, Medium) `file_put_contents("data/a.phv::$DATA", ...)` and `"data/b.phv."` bypassed the `.phv` write ban and created `a.phv`. Device names such as `CON` were also usable | For each path component, reject `:`, trailing dots and spaces, device names such as `CON` `NUL` `COM1` `LPT1`, and control characters (same rules on all OSes) | All 3 PoCs raise `PermissionError` |
| R3 | CSP was off by default | Enabled by default. The parser records the position of each `<script>` tag written in a template, and a per-response nonce is added automatically. A `<script>` injected via `raw()` or similar gets no nonce, so it does not run | HTTP integration test confirms the header nonce matches the template's two `<script>` tags and is absent from the `raw()` `<script>`. Sample site works in a browser |
| R4 | No rate limiting | Added `rate_limit($key, $max, $seconds)` / `rate_limit_reset($key)` and `request()->ip`. Site-wide limit `[security] requests_per_minute` (429 with `Retry-After` when exceeded). The counter table holds up to 100,000 keys and discards the oldest windows first. Used in the sample guestbook | Unit tests and HTTP integration tests |
| R5 | Sessions were memory-only | `[security] session_store = "dir/"` stores them in files. Filenames are the SHA-256 of the ID (IDs cannot be learned from a listing); writes are atomic via rename from a temp file; reads and writes go through `safefs`. Count and size limits, plus cleanup of expired sessions | HTTP integration test confirms another server process can read the same session |

## 5. Residual Risks

| Item | Description | Decision |
|---|---|---|
| Context detection is approximate | Context detection for `<?= ?>` is based on source order. HTML built dynamically by concatenating `Html` values is not analyzed | `Html` is the type for "trusted HTML", and only `raw()`, `escape()`, and templates can create it. The spec states that `raw()` content is the user's responsibility. The default CSP adds a second layer against execution of injected scripts |
| SQLite auxiliary files (Windows) | A very small window remains in which a symbolic link could be created under a journal file's name between the pre-open and post-open checks | On Windows, creating file symbolic links requires admin rights or Developer Mode, and the directory itself cannot be swapped. Dedicate the `[permissions]` directories to script data |
| Rate limiting is per process | Counters are not shared across multiple server processes | Future work. The spec notes that behind a reverse proxy, `request()->ip` is the proxy's address |

## 6. Ongoing Verification

- `cargo test --workspace`: golden tests (both interpreter and JIT), security regression tests,
  HTTP integration tests against a real server (aliases, traversal, CSRF, cookie attributes, time limits), and fuzzing.
- CI: Linux / Windows / macOS, `clippy -D warnings`, long-running fuzzing, `cargo audit` (known vulnerabilities in dependency crates).
- Unix-specific code (the `openat` path in `safefs`) was also verified by running all tests and clippy in a Linux Docker container.
- `unsafe` is used in only three places: the memory-accounting allocator (`phv_vm/src/alloc.rs`),
  calls into JIT-generated code (`phv_jit`, `Vm::call_jit`), and calls to OS file APIs
  (`phv_std/src/safefs.rs`).

## 7. Second Vulnerability Assessment

Date: 2026-11-02 (after the first round of fixes and the resolution of residual risks)

### Method

To avoid anchoring on the first audit's conclusions, four new reviewers were given only the source, and each verified issues by actually attacking.

- A: Code written after the first audit (`safefs`, file-backed sessions, rate limiting, CSP nonces)
- B: Web server and sample site (external attacks)
- C: Template context detection and the type system
- D: SQL, runtime limits, JIT, bytecode cache, tools

Dependency crates were also checked with `cargo audit` (0 known vulnerabilities; the unmaintained `atomic-polyfill` was removed
by disabling postcard's default features). The `unsafe` code and the CI configuration were also reviewed. For fix verification,
the reviewers' PoCs were rerun against the fixed build and added to the regression tests. All tests were also run on Linux (Docker).

### Summary of Results

| Severity | Count | Fix |
|---|---:|---|
| High | 6 | All fixed |
| Medium | 9 | All fixed |
| Low / Info | 11 | 9 fixed, 2 documented |

### High

| # | Finding | Fix |
|---|---|---|
| R2-1 | Results of `escape()`, `htmlspecialchars()`, and `nl2br()` (`Html`) could be placed at the start of `href` or inside `<script>`. HTML escaping stops neither `javascript:alert(1)` nor script code, and template `<script>` tags get a nonce, so CSP did not block it either | Added context-verified types `SafeUrl` (`safe_url()`) and `SafeJs` (`json_html()`). URL starts accept only `SafeUrl` and numbers; `<script>` contents accept only `SafeJs` and numbers. `json_html()` values are rejected inside attribute values (a `"` would end the attribute) |
| R2-2 | `echo` written between template HTML (`<?phv echo $x; ?>`) was not context-checked | `echo` gets the same checks as `<?= ?>` |
| R2-3 | `file_put_contents("views/page.phv.html/", ...)` (trailing `/`, `/.`, `\`) bypassed the `.phv` write ban, allowing templates to be rewritten for code execution. `unlink` and `Database::open` did not check the ban | Check the final filename after path normalization, and apply it to all writes, deletes, saves, and databases. Reject paths ending in `/` `\` `/.` |
| R2-4 | Windows 8.3 short names (`views/GREETI~1.HTM`) allowed rewriting `.phv.html` templates | Retrieve the real name from the opened handle, and reject if it differs from the requested name (8.3 short names and other aliases). Also applied to directories |
| R2-5 | `join()`, `json_encode()`, `json_html()`, `dump()`, `echo`, and string concatenation did not check the memory limit before allocating; a single call could allocate several GB and crash the process (`serve` also stopped) | Compute the result size first and `reserve()` (`join`); once output exceeds 64 KB, proceed while checking the memory and time limits (JSON, dump); `echo` and concatenation check before allocating, including the up-to-6x growth from escaping |
| R2-6 | `==` on shared structures (`$a = [$a, $a]` 60 times) took exponential time, ignored the time limit, and tied up a worker | Remember already-compared list and map pairs, and compare each pair once (linear time). Quadratic operations such as `unique()` check the time limit as they run |

### Medium

| # | Finding | Fix |
|---|---|---|
| R2-7 | Template context detection differed from browsers: treating `</scripts>` as an end tag, `<!--<script>` inside `<script>`, `<!-->` and `--!>`, `<textarea>` and `<title>` contents, inside `<svg>`, `srcdoc`, and shifting the URL-start detection with characters, as in `href="&#x20;..."` or `href="j<?= ?>"` | Rewrote to match WHATWG tokenization (end-tag delimiters, script escaped / double escaped states, comments and bogus comments, RCDATA / RAWTEXT elements, foreign content). A URL position counts as the start until `/ ? # :` appears in the preceding literal text. Strings are rejected in `javascript:` URLs and `srcdoc` |
| R2-8 | Opening `<script>` inside a function or loop without closing it shifted the context depending on call order | Blocks and function bodies must end in the same HTML state they began in (functions start from an empty document) |
| R2-9 | `<script<?= $x ?>>` allowed insertion inside a tag name, and the attacker's `src=` received a nonce (CSP bypass) | Treat the middle of a tag name as inside the tag, making it an error |
| R2-10 | Objects with reference cycles (`$n->next = $n`) were not freed after the request, so server memory kept growing | The VM tracks the objects it creates via weak references, and clears their fields at the end of execution to break cycles |
| R2-11 | Deeply nested source (50,000 levels of `1+1+...`, `if`, `[[[...]]]`, etc.) overflowed the stack in the parser, type checker, and compiler (uncatchable, so `serve` also stopped). Error output quoted the 50,000-character line every time, producing 8.5 GB of output | Nesting is limited to 500 levels (beyond that, parsing stops with a single error). Errors are capped at 50; long lines show only the area around the error |
| R2-12 | `redirect("/<TAB>/evil.com")` passed as a same-site path; browsers stripped the tab and navigated to `//evil.com` (open redirect) | Reject paths containing control characters, whitespace, or `\`. `redirectExternal()` also rejects control characters |
| R2-13 | `session_store = "."` or `"data/"` was allowed; expiry cleanup deleted project files and overwrote `.gitignore` with `*`. Overlapping with a script permission directory allowed forging or reading sessions | Allow only a dedicated session directory (not the root, `public/`, or anything overlapping `[permissions]`). Cleanup deletes only files named like session files. `.gitignore` is created only if absent |
| R2-14 | Right after logout or `regenerate()`, a concurrent request that saved the session resurrected it | Skip saving if the original session is gone at save time (both memory and file stores) |
| R2-15 | SQLite memory was not counted toward the script limit; `:memory:` databases or `cache_size` let one request use hundreds of MB | Set the page cache to 2 MB per connection, and set `max_page_count` for `:memory:` to match the limit. Reject the `cache_size`, `temp_store`, and `max_page_count` pragmas |

### Low / Info

- **Rate limiting** (fixed): 100,000 keys could evict keys currently being limited → discard keys that have not hit the limit first. Changing the window for the same key reset the count → count per window. IPv6 is grouped by /64.
- **Configuration** (fixed): `write = ["public."]` or `["..."]` pointed to `public/` or the root on Windows → apply the same checks as for filenames to each path component. A huge `session_ttl` overflowed → range-checked.
- **5 or more type parameters** (fixed): The type checker panicked → out-of-range parameters are treated as unbound.
- **`mktime` / `checkdate`** (fixed): Out-of-range values silently wrapped around → `ValueError`.
- **`password_verify`** (fixed): Lowered the cost limits for stored hashes to 64 MiB, 8 iterations, and parallelism 4.
- **Session files** (fixed): Readable by other users on Unix → created with 0600. Mass writes between the 30-second cleanups → clean up early based on write volume.
- **`phv fmt` / `phv test`** (fixed): Directory traversal followed symbolic links and junctions, which could loop forever or rewrite outside files → links are not followed.
- **CI** (fixed): Third-party GitHub Actions were pinned by tag → use only GitHub's own actions plus rustup and `cargo audit`; checkout credentials are not persisted.
- **CSP nonce** (fixed): `<script` strings inside `<textarea>` also got a nonce → RCDATA element contents are excluded. The documented `'nonce-{nonce}'` syntax now works too.
- **Content-Type values that disable escaping** (documented): Some types (e.g. `text/markdown`) were missing from the spec. Browsers render none of them as HTML, so the spec was updated to match the implementation.
- **CSRF** (documented): The cookie-HMAC scheme assumes cookies cannot be tampered with. The spec now states that production must use HTTPS and `secure_cookies = true`.

### No Issues Found (main items)

Path traversal and source exposure (encoding, case, 8.3 short names, ADS, trailing dots), response splitting, CSRF bypasses,
cookie attributes, uploads, production-mode error display, slowloris, huge multipart, deep JSON, `sql"..."`
parameterization (no way to construct an `Sql` value through the type system was found), the SQLite authorizer, JIT/interpreter result parity and
limits, and the `phv install` git invocation (`--` separator, revision character set, protocol allowlist).

### Not Covered in This Round

`phv lsp`, `phv migrate`, and the bytecode cache were out of scope (assessed in §8).

## 8. Third Assessment (lsp, migrate, cache)

Date: 2026-11-02. One new reviewer verified, with PoCs, the three areas excluded from the second assessment. The item left over from the second
assessment, forging connection handles via `new Database(<integer>)`, was also closed.

| # | Severity | Finding | Fix |
|---|---|---|---|
| R3-1 | High | The premise that the cache has the same trust level as source did not hold. Entries were not checked against the script they belong to (always valid when the dependency list was empty), and the `stamp` (version, executable size and mtime) is public information, so a `.phv-cache/` in a project received from someone else could hold bytecode that differs from the source (code other than the source you read runs) | Sign with an HMAC using a per-user secret key (outside the project; 0600 on Unix), and refuse entries whose signature does not match. Record the canonical paths of the script and the root in each entry and check them, and verify that the script itself is among the dependencies |
| R3-2 | Medium | Dependencies were compared by mtime only, so mtime-preserving copies (`cp -p`, `rsync -t`, archive extraction) or same-length rewrites kept stale bytecode running (deployed security fixes would not take effect). There was also a race where a file changed between reading it and fetching its mtime | Record the SHA-256 of the compiled contents, the size, and the mtime of each dependency. Always verify contents when loading the cache; the server's in-memory check also verifies the contents of files modified just before recording (within the timestamp resolution) |
| R3-3 | Medium | Cache writes followed links; a project with `.phv-cache` as a junction, or a symbolic-linked `.gitignore`, allowed overwriting files outside the project. Temp file names were also predictable | Read and write via `safefs`, rejecting links and hard links. Do not use `.phv-cache` if it is itself a link. Temp files get random names; `.gitignore` is created only if absent |
| R3-4 | Low | `[permissions].write` could list `.phv-cache/` or `vendor/`, letting scripts write files that are later loaded as code | Directories starting with `.`, `vendor/`, and package directories cannot be listed in `write` |
| R3-5 | Medium | `phv migrate` did not convert `<?PHP` / `<?Php` (uppercase), so PHP containing the auth `require` and `exit` remained as a plain string, and it reported "passes type check" (auth checks silently disappear) | Convert open tags case-insensitively. Keep any `<?` that cannot be converted, with an uncompilable `TODO_migrate(...)` attached |
| R3-6 | Medium | `phv migrate` output followed dangling symbolic links and was written to arbitrary locations. Varying the spelling, as in `self.PHV`, allowed overwriting the input file itself | Write via `safefs`, never to links or hard links. Compare input and output by normalized path |
| R3-7 | Low | `phv migrate` panicked on non-ASCII characters (e.g. Japanese) near comments or strings | Compare byte-wise |
| R3-8 | Low | `phv lsp` URI checks could be bypassed with decoded UNC paths (`file:///%5C%5Chost%5Cshare`) or `\\?\` and `\\.\`, allowing reads of unopened files, triggering SMB connections, and hanging when given a named pipe | After decoding, accept only local paths that start with a drive letter; reject device names and `..`. Hover works only on open documents. Up to 1000 open documents. Return an error on malformed JSON and continue |
| R3-9 | Info | `new Database(0)` could forge a connection handle (within the same request only) | `new` now checks `private` / `protected` constructors (previously unchecked), and the `Database` constructor is now `private` |

### No Issues Found

`phv lsp` does not write files. It has limits of 64 MiB per body and 8 KiB per header line, and resists deep nesting (the parser's 500-level
limit). `phv migrate` leaves `require`, `exit`, `eval`, `$$var`, `extract`, superglobals, etc. as
uncompilable code. Decoding the cache format (postcard) is safe; even with adversarial bytecode,
the interpreter stops with a bounds-check panic (caught by `catch_unwind`), and the JIT rejects it via the `well_formed` check.

### Verification

All reviewer PoCs were rerun against the fixed build and verified to no longer reproduce. Regression tests were added for cache forgery, reuse,
same-length rewrites, junctions, constructor visibility, `lsp` URIs, and config validation.

## 9. Fourth Assessment (v0.4: network clients, database servers, web additions, editor, CI)

Date: 2026-11-03. Scope: everything added after §8, which shipped in v0.2.5–v0.4.0: the HTTP and WebSocket clients
(`std.net`), PostgreSQL / MySQL / Redis (`std.db`), `response()->file()`, `formAll()` / `queryAll()`, TOTP and QR codes,
project-root discovery, LSP completion and go to definition (and the editor analysis behind them), the release and
publishing workflows, the VS Code extension, and the example sites (`examples/demo`, `company`, `wardogs`).

### Method

Four new reviewers, each given only the source and one area, confirmed every finding with a working attack. They ran
hostile HTTP, WebSocket and TLS servers, real PostgreSQL 17, MySQL 8.4 and Redis 7 servers in Docker, the example sites
over HTTP (in production and development mode), and an LSP client with mutation fuzzing (about 400,000 requests). All
PoCs were rerun against the fixed build, and regression tests were added. Dependencies: the shipped dependencies of
the extension have no known vulnerabilities; CI's `cargo audit` passes.

### Summary of Results

| Severity | Count | Fix |
|---|---:|---|
| High | 2 | All fixed |
| Medium | 9 | All fixed |
| Low / Info | 14 | 13 fixed, 1 documented |
| Found while verifying | 1 | Fixed (R4-24) |

Repository and account settings that only the owner can change, and the remaining risks, are listed at the end.

### High

| # | Finding | Fix |
|---|---|---|
| R4-1 | Query results bypassed `memory_limit_mb` (PostgreSQL, MySQL, Redis): memory was checked every 256 rows (PostgreSQL, MySQL) or after the whole reply (Redis). With a 16 MB limit, one query grew the process to 1.9 GB; a few such requests (or one hostile server reply) could make the whole server run out of memory | Memory is checked after every row. PostgreSQL connections are read through a metered socket with a byte budget of about half the memory left to the script; MySQL gets a client-side `max_allowed_packet`; Redis uses a new client (`redisclient.rs`) with the same budget. A connection that hits the limit is closed, not drained. Peak: 1.9 GB → 23–46 MB |
| R4-2 | PostgreSQL `LISTEN` notifications piled up in the pooled connection's buffer and were never freed: the server's memory grew 26 → 1192 MB over three requests and stayed there | PostgreSQL connections are now driven by PHV (`pgmeter.rs`, over `tokio-postgres`), and notifications are dropped as they arrive. Three floods: the server stays at 23 MB |

### Medium

| # | Finding | Fix |
|---|---|---|
| R4-3 | A hostile HTTP or WebSocket server could hold a request past the script's time limit, forever: the socket timeout limits each read, not the total (one byte of headers or body every 1.5 s, endless `100 Continue`, a WebSocket handshake or ping flood one byte at a time). Through `phv serve` this pins a worker per request | Every socket read and write stops at the deadline (`netio::Socket`, also under TLS). At most 10 interim responses. `receive()` also ends when control frames keep arriving. Every attack now ends at the time limit |
| R4-4 | Database time limits could be bypassed by the script (`SET statement_timeout = 0`, `set_config`, MySQL `MAX_EXECUTION_TIME` hints, `DO SLEEP()`, which `max_execution_time` does not cover), and PostgreSQL had no client-side timeout, so a hostile server could hang a request forever and a few such requests used up `pool_size` | PostgreSQL stops reading at the deadline on the client side (R4-2's metered socket); MySQL statements are stopped with `KILL QUERY` from a second connection at the deadline, and the socket timeout is the time limit + 10 s. All four PoCs end at the limit |
| R4-5 | The MySQL pool reset (`COM_RESET_CONNECTION`) kept the active role (`SET ROLE`) and the current database (`USE`) for the next request (privileges or another tenant's data) | After the reset: `SET ROLE DEFAULT` and the URL's database again; without a database in the URL, a connection that chose one is not reused |
| R4-6 | `<script src="<?= safe_url($u) ?>">` compiled, and the tag got the CSP nonce: any URL there ran as a trusted script (also `/js/<?= $s ?>` and SVG `<script href>`) | Inserting into the URL of a `<script>` is a compile error |
| R4-7 | Static files (up to 64 MB) and `response()->file()` were read whole into memory for every request, and connections had no write timeout: 30 parallel downloads of a 60 MB file took 1.8 GB | Both are streamed from the open file. A connection whose client accepts no data for 30 s is closed (`StallGuard`). 20 parallel 40 MB downloads: peak 30 MB |
| R4-8 | Since v0.2.1, project-root discovery walked every ancestor directory, so a `phv.toml` planted by another local user (e.g. `D:\phv.toml`, writable by all users on Windows) became the project file, with its permissions, environment variables and services | A `phv.toml` is skipped at a filesystem root, in the shared temporary directory, and on Unix when it is not owned by the current user or root |
| R4-9 | Each subclass copies what it inherits, so 127 KB of source (3,000 classes extending one with 3,000 methods) took 13.7 s and 2.3 GB in `phv lsp`, repeated on every keystroke; 560 KB took 7.9 GB | All classes together may inherit at most 200,000 members (compile error beyond). The same file: 1.2 s and 69 MB. Sources larger than 8 MB are refused |
| R4-10 | The Marketplace publish job ran `npx @vscode/vsce@4.1.0` with its whole dependency tree unpinned (and install scripts) while holding the OIDC permission that can publish the extension | vsce 4.1.0 is pinned in `editors/vscode/package-lock.json`; the job checks out the tag, installs with `npm ci --ignore-scripts`, and checks the `.vsix` against the release's `SHA256SUMS` |
| R4-11 | The example sites rate-limited by the full address (an IPv6 /64 gives 2^64 keys) and the demo trusted `Fly-Client-IP` when not on Fly.io: the board, contact form, TOTP and click limits could be bypassed | New `ip_group($ip)` (an IPv6 address stands for its /64); the examples and the spec use it. The demo reads `Fly-Client-IP` only when `FLY_APP_NAME` is set |

### Low / Info

| # | Finding | Fix |
|---|---|---|
| R4-12 | A chunk size of `ffffffffffffffff` after a first chunk overflowed (`attempt to add with overflow`): any service a site calls could turn a request into an internal error | Checked arithmetic; chunk sizes are hex digits only (at most 16), the CRLF after each chunk is checked; `Content-Length` must be digits only and repeated values must agree |
| R4-13 | A script could shadow a `headers_env` credential with `X_Api_Key` (CGI servers read `_` and `-` the same) and add a second value for a query key of the base URL | `_` counts as `-` when comparing header names; query keys of the base URL cannot be given again |
| R4-14 | Paths with `..;` (path parameters) and `%252e%252e` (decoded twice by some proxies) were sent | Refused, as are `%` without two hex digits and characters outside RFC 3986 (non-ASCII is percent-encoded) |
| R4-15 | Error messages named the host of a service (`cannot resolve <host>`, rustls certificate errors, MySQL driver errors with `host:port`) | Network, TLS and connection errors are described in fixed words |
| R4-16 | `DISCARD ALL` also removed the PostgreSQL client's own prepared statements, so the first query using an unseen custom type (enum, domain) failed on that pooled connection for good | The reset runs the parts of `DISCARD ALL` except `DEALLOCATE ALL`, and deallocates the statements scripts prepared by name |
| R4-17 | `<meta http-equiv="refresh" content="0;url=<?= $s ?>">` accepted any string (open redirect) | Inserting into `<meta content>` takes only numbers unless `name`, `property` or `itemprop` comes first |
| R4-18 | 52,000 cookieless requests to a page that creates a session evicted every visitor's session (the store holds 50,000) | When full, sessions whose visitor never came back are evicted first |
| R4-19 | The TOTP replay check the spec and the demo used (last step in the session) was racy: 2 of 8 parallel requests with the same code were accepted | The demo stores the step with one SQLite statement that only succeeds for a newer step; the spec recommends this. 1 of 8 accepted |
| R4-20 | Pages with a CSRF token had no `Cache-Control` when the visitor already had the CSRF cookie: a shared cache could hand one visitor's token to another | `csrf_token()` / `csrf_field()` mark the response `private, no-store` |
| R4-21 | The website deploy step (with the Cloudflare token) ran in the same job as `npm run build`, which could plant a `wrangler` in `node_modules` that `npx` would run | Build and deploy are separate jobs; the deploy job only downloads the built site |
| R4-22 | The loader read a file when its path could not be resolved (device names), read sources of any size, and could block on a FIFO | An unresolvable path is not read; sources are read only from regular files up to 8 MB (also `phv.toml`) |
| R4-23 | The WARDOGS example returned 500 on huge numbers from the game server (`(int)round(1e308)`) | Out-of-range numbers count as missing |
| Info | `safe_url()` let `\/host` through (browsers read it as `//host`); `phv lsp` had no `catch_unwind`, checked unopened documents on save and kept diagnostics state after close; definition URIs could contain `..`; `sslmode=require` / `require_ssl` in a URL was ignored when `phv.toml` allowed plain connections; two `[databases]` entries with the same URL shared a pool; header values could hold control characters other than CR/LF | Fixed |
| Info (documented) | Development mode is the default on `127.0.0.1`, which is also where a reverse proxy connects | The spec says to use `--prod` behind a proxy, and the server prints a note at startup |

### Found while verifying

| # | Finding | Fix |
|---|---|---|
| R4-24 | On Windows, requests opening a new SQLite file at the same moment failed with "not a safe file name" (5 of 8): a journal deleted by another connection gets a final path under `\$Extend\$Deleted`, which the real-name check (R2-4) took for an alias | A file in that location counts as gone. Three runs of the same race: no failures |

### Settings for the owner (not changed)

These are repository and account settings; they were not changed in this assessment.

- The `marketplace` environment has no protection rules: allow deployments only from tags `v*`, add a required reviewer,
  and add a ruleset that restricts who can create `v*` tags.
- Turn on Dependabot security updates and secret scanning, and consider a `.github/dependabot.yml` for GitHub Actions.

### Residual Risks

| Item | Description |
|---|---|
| Deploy tooling | `wrangler` is pinned, but its own dependencies are resolved when the deploy job runs, with install scripts (the deploy job holds the Cloudflare token). `cargo install cargo-audit` takes the latest version |
| Actions pinned by tag | Only GitHub's own actions are used, referenced by tag rather than commit |
| Editor | Every change triggers a full analysis (no debouncing or cancellation); the caps above bound its cost. Dependencies listed with a `path` outside the project are read by the editor (only `.phv` files) |
| DNS | Name lookups of the network clients have no timeout of their own |
| Response bodies | A page's output is held in memory up to the memory limit of its request, for each of up to 1024 connections |
| File-backed sessions | The eviction order of R4-18 applies to the memory store |

### Verification

All PoCs were rerun against the fixed build. Regression tests were added for the slow-drip and chunk-overflow servers
(`tests/http`), memory and time limits on real servers, notifications, custom types after a reset, and the MySQL role
and database reset (`tests/db`, run in CI against PostgreSQL, MySQL and Redis), the template contexts (`tests/cases`),
the session flood, the deadline socket, URL, path, header and chunk parsing, and the fuzzer (1,500 mutations per
program, also through the editor analysis).

## 10. Follow-up on the Residual Risks (after v0.4.1)

Date: 2026-11-03. The residual risks of §5, §7 and §9 were revisited; each was fixed or kept with its reason. Reading
the code for them turned up one new problem (N2) and showed that one risk was larger than described (R2'').

| # | Risk | Fix | Verified |
|---|---|---|---|
| N2 | (New, Medium) A template could end inside a tag, attribute, comment, raw-text element or `<svg>`, and its output was still `Html`. Joining it with another `Html` value (`render(...) . escape($u)`) put that value at the start of a URL, in a `<script>`, and so on, without the context check | A template must end outside every tag, comment, raw-text element and foreign content (compile error otherwise). Every source of `Html` except `raw()` is now a complete fragment, so joining them keeps each value in the context it was checked in | `tests/cases/err_template_end.phv`; all example sites and templates still compile |
| R2'' | §4's SQLite check ran before and after `Database::open`, but SQLite opens `-journal` / `-wal` at every transaction and `-shm` outside `xOpen`, and the Windows VFS follows links (`SQLITE_OPEN_NOFOLLOW` only works on Unix, for the main file). The gap was every transaction, not a small window at open | Databases are opened through a thin VFS (`phv_std/src/db/vfs.rs`) that forwards all I/O to the default one and checks each file SQLite opens by name right after the open, through SQLite's own handle on Windows (not a reparse point, one link, final path as expected); `-shm` is checked (and held on Windows) before its first mapping. On Unix SQLite already refuses symbolic links; hard links are refused by checking the name after the open | Tests plant a symbolic and a hard link at `-journal` and `-shm`: the write fails and the target is unchanged (Windows with the symlink privilege, and Linux). The same tests fail with the default VFS. 8 threads × DELETE and WAL mode: no errors |
| R6 | Rate limits were counted per process | `[security] rate_limit_store` names a Redis database; `rate_limit()`, `rate_limit_reset()` and `requests_per_minute` then count in fixed windows on that server (`INCR` + `PEXPIRE`, hashed keys under `phv:ratelimit:`). If it cannot be reached, `rate_limit()` refuses and the per-client limit counts in the process, with a log line | Two `phv serve` processes on one Redis: 3 requests allowed across both, then 429 from either. `tests/db/redis_ratelimit.phv` (CI) and a test with an unreachable store |
| R7 | Name lookups had no deadline | Lookups (and the MySQL driver's connect, which looks the host up itself) run on a helper thread that is given up on at the deadline; at most 32 abandoned at once. PostgreSQL connects to the addresses PHV looked up | Unit test with work that never finishes: it ends at the deadline, and the cap applies |
| R8 | A page's output was held in memory up to its request's memory limit, for each of up to 1024 connections | At most `[server] workers` scripts run at once (two per CPU, at least 8; 10 seconds' wait, then 503). Output waiting to be sent is counted until the connection has written it; above `workers` × `memory_limit_mb`, new scripts get 503 with `Retry-After` | HTTP test with one worker: the second request gets 503 after 10 seconds, static files are still served. 80 parallel clients that do not read a 4 MB page: peak 35 MB with 2 workers |
| R9 | The file session store evicted by age only (R4-18 covered the memory store) | New sessions are `n-<hash>.json` until their cookie comes back, then renamed to `<hash>.json`; the sweep removes `n-` files first, from the directory listing alone | A flood past the limits keeps the returning visitor's file even when it is the oldest (the test fails with the old order) |
| R10 | `phv lsp` analysed every change as it arrived and ignored `$/cancelRequest` | Messages are read on their own thread (at most about 64 MiB unprocessed). Each batch applies the text changes, answers requests in order and checks each changed document once; cancelled requests get `RequestCancelled` | 100 back-to-back `didChange` give one `publishDiagnostics`; a cancelled completion gets -32800 |
| R11 | Actions were referenced by tag; the deploy job resolved wrangler's dependencies (with install scripts) while holding the Cloudflare token; `cargo-audit` was the latest version | Every action is pinned to the commit of its release (Dependabot updates the pins). wrangler 4.151.1 comes from a lockfile in `.github/deploy`, installed with `npm ci --ignore-scripts` in a step without the token. `cargo-audit` 0.22.2 | The YAML parses; every `uses:` is a 40-hex SHA; `npm ci --ignore-scripts` gives a working wrangler |

The owner settings listed in §9 were applied on 2026-11-03: the `marketplace` environment needs a reviewer and only
deploys from `v*` tags, a ruleset lets only admins create, move or delete `v*` tags, and Dependabot security updates,
secret scanning and push protection are on. `.github/dependabot.yml` keeps the actions and the deploy tooling current.

### Kept, with reasons

| Item | Reason |
|---|---|
| `raw()` and source-order context detection (§5) | `raw()` is the documented way to output trusted markup and is the user's responsibility; the default CSP stops injected scripts. With N2, every other `Html` value is a complete fragment |
| CSRF assumes HTTPS and `secure_cookies` in production; the Content-Type list (§7) | Documented in the spec; browsers render none of the listed types as HTML |
| Dependencies with a `path` outside the project are read by the editor (§9) | A supported feature (shared code next to the project). Only `.phv` module files are read, regular files of at most 8 MB, and nothing leaves the machine |
| Unix: a hard link swapped in and back out between SQLite's open and the name check (R2'') | Needs write access to the database directory, which other users cannot have (`o+w` directories are refused) |
| Windows: a symbolic link at `-journal` to a missing file lets Windows create that empty file before the check closes it (R2'') | No data is written to it, and file symbolic links need administrator rights or Developer Mode |
| Abandoned name lookups keep a thread until the OS gives up (R7) | At most 32 at once; beyond that, connections fail at once instead of waiting |

### Verification

All new tests pass on Windows and on Linux (Docker), with clippy (`-D warnings`) and rustfmt; `tests/db` passed against
PostgreSQL 17, MySQL 8.4 and Redis 7, and the fuzzer ran 1,500 mutations per program. Regression tests: the template
end check (`tests/cases`), the journal and `-shm` links (`phv_std` `db::vfs`), the shared rate limit and an unreachable
store (`tests/db`, run in CI against Redis), the lookup deadline (`netio`), the worker limit (`phv_server` HTTP test)
and counted output, the file session flood (`session`), and the batched and cancelled LSP messages (`lsp`).

## 11. Fifth Assessment (v0.5.0: generics, annotations, async I/O, database sessions, packages, `phv build`, JIT)

Date: 2026-11-03. Scope: everything changed after v0.4.1: generic classes and annotations, `#[` and the template end
check, asynchronous I/O (`std.async`), sessions in a database, shared rate limits, the server's worker limit, packages
of packages and the registry (`phv install`, the loader, the Packages page and the registry's own check), `phv build`,
the JIT for floats and strings, and line wrapping in `phv fmt`.

### Method

Three new reviewers, each given only the source and one area (packages and tooling; async, sessions and the server;
the language, templates, JIT and formatter), confirmed every finding with a working attack: hostile git repositories
and registries, two server processes on one session store, real PostgreSQL 17, MySQL 8.4 and Redis 7 servers in
Docker, a site served over HTTP, forged bytecode against the JIT, and a formatter harness (all 112 `.phv` files of the
repository at widths 6–80 and about 20,000 mutations). All PoCs were rerun against the fixed build, and regression
tests were added.

### Summary of Results

| Severity | Count | Fix |
|---|---:|---|
| High | 7 | All fixed |
| Medium | 10 | All fixed |
| Low | 6 | All fixed |
| Info | 4 | 2 fixed, 2 documented |

### High

| # | Finding | Fix |
|---|---|---|
| R5-1 | `phv install` wrote through a `vendor` (or `vendor/<name>`) that was a symbolic link or junction, so a cloned or planted link could make it delete and replace any directory outside the project | `vendor`, `vendor/<name>` and `phv.lock` are refused when they are links (or Windows reparse points); git runs with `core.symlinks=false` |
| R5-2 | `phv.lock` was written through a link, and git URLs could hold shell characters: with the lock linked to `.bashrc`, a dependency URL `…$(touch${IFS}PWNED)` ran when the shell started | The lock is written to a temporary file and renamed into place. Git URLs may hold only URL characters (at most 500) |
| R5-3 | A sign-out raced with a slow request: the session store read the session, then wrote it back, so a request that finished after the logout brought the session back (file, Redis and SQL stores; also across processes). Two logins regenerating one session both took it over | Stores insert a session only under a new id and update it only while it exists, each as one atomic step: the file store under an OS lock on `.phv-session.lock`, Redis with Lua scripts, SQL with `INSERT … ON CONFLICT DO NOTHING` / `INSERT IGNORE` and `UPDATE … WHERE id`. A regenerated session must be removed by this request before its new id is stored. Test: 20 rounds of 8 threads saving through two stores while a third logs out |
| R5-4 | A caller's type parameter was captured by a method's type parameter of the same name: through `Box<U>`, `map<U>` turned any value into `SafeUrl` or `SafeJs` (a `javascript:` link, a nonced script) | A method's own type parameters become inference variables before the receiver's type arguments are substituted |
| R5-5 | `is Box` and `catch (Box $e)` narrowed to `Box<any>`, which could be written: `$a->value = "…"` on a `Box<SafeJs>` put a string into a nonced `<script>` | They narrow to `Box<?>`: reads give `any`, writes accept nothing, and it is not a `Box<T>` |
| R5-6 | A generic interface reached through two paths with different type arguments (`UrlSrc extends Src<SafeUrl>`, `StrSrc extends Src<string>`) kept the last one, so a `string` came out as `SafeUrl` | A class reaching one generic class or interface with two argument lists is an error |
| R5-7 | The override check compared type parameters by name, so a method's `T` matched a subclass's class parameter `T`, and `Base<int>::m<T>` was overridden by `m<V>(T $x)` writing a string into a `SafeUrl` property | Each method's own parameters are compared by position |

### Medium

| # | Finding | Fix |
|---|---|---|
| R5-8 | Package names differing only in case were different packages but one directory on Windows and macOS: a dependency could replace another package's code | Package names are lowercase letters, digits and `_` |
| R5-9 | A package's phv.toml was read with no size or type limit, through links (`/dev/zero`, a link to a secret), and a parse error echoed its content | Read only as a regular file of at most 1 MB; messages do not quote it |
| R5-10 | Registry entries and packages' dependencies could use `file:////host/share` (UNC): Windows connected over SMB and sent NTLM credentials | Registry entries and dependencies of packages must be `https://` (a `file:///` registry may name `file:///` packages, for tests); `file:////` is refused everywhere |
| R5-11 | LSP completion details of nested generic types grew without bound: about 300 KB of source made a 1 GB detail | Details are at most 2,000 characters, and substitution is capped |
| R5-12 | Async work outlived its request: a short request could start 16 queries and end, holding DB connections and threads for the whole time limit without holding a worker | Work never awaited is waited for when the script ends (so it keeps the worker); a script may use at most half of a database's pool at once; at most 1,024 operations in the process |
| R5-13 | The Redis session store had no count or byte limit, so a flood of new sessions filled Redis and then broke the rate limits and logins | Redis keeps within 50,000 sessions and 256 MB as they are written (sorted sets of new and returning sessions, sizes, a byte counter, all in the write script), removing new sessions first. SQL stores also have the byte limit and sweep early after 1,000 new sessions |
| R5-14 | `rate_limit_reset()` scanned the whole Redis keyspace, and failed at about 3 million keys | Each key's windows are one Redis hash (`phv:ratelimit:<hash>`) updated by a script, so a reset is one `DEL` |
| R5-15 | `break`, `continue`, `return` and `throw` inside a tag moved the output that follows into the tag (`<a href="<?phv if ($id < 0) { break; } ?>…` put a later value at the start of the URL) | A jump must leave the HTML where its target (the loop, the `try`, the start of the function) began, and where every `try` it leaves began |
| R5-16 | `json_html()` (`SafeJs`) could be used as `Html`, so through an `Html` template parameter it was placed in a quoted attribute, whose quotes it ended (attribute injection) | `SafeJs` is no longer `Html` |
| R5-17 | Deep recursion of JIT code with large frames overflowed the 16 MB stack of a server worker and killed `phv serve` (only depth was bounded) | Native code also stops (as too deep) when it would use more than 8 MB of stack below its outermost call |

### Low

| # | Finding | Fix |
|---|---|---|
| R5-18 | A symbolic link inside a package could make a module of another package or of the project its own, getting around the import boundary | A package's module must be inside the package after resolving links |
| R5-19 | A package named like a directory of the project shadowed the project's own modules | An error names the clash (a `path` package that is that very directory is allowed) |
| R5-20 | The Packages page read registry entries with a regular expression and could show a different repository than `phv install` uses | Entries must be exactly the two lines `git = "https://…"` and `description = "…"`, checked the same way by the site, the registry's pull-request check (with a TOML comparison) and the installer |
| R5-21 | A MySQL server that accepts connections but never answers used up the 32 slots for blocking work, so name lookups of every other service failed | MySQL connects have their own slots |
| R5-22 | The JIT's check let `Concat` of no registers name a register out of range, aborting the process (needs forged signed bytecode) | Refused by the check |
| R5-23 | `phv fmt` changed strings and output: it trimmed spaces before a newline inside a string, rewrote line endings inside strings, and added a newline after trailing inline HTML | Lines that end inside a string or inline HTML keep their end and their line ending; a file ending in HTML ends as it did |

### Info

| Item | Result |
|---|---|
| `phv build` included every file under `public/` (for example a stray `.toml`) | Fixed: only files the server would serve |
| A conditional jump backwards never checks the clock (forged bytecode only) | Fixed in the JIT's check; the compiler only branches forward |
| phv.toml is included in `phv build` executables, with any secret written in it | Documented: use `url_env` / `headers_env` |
| Growing the register window comes before the memory check (both engines; present in v0.4.1) | Kept: the overshoot is bounded by the function's register count, and the check follows at once |

### Residual Risks

| Item | Description |
|---|---|
| `json_html()` through `any` | A `SafeJs` value passed as `any` and narrowed with `is Html` is `Html` at run time (one runtime type), and could then be placed in an attribute. It takes code that does this on purpose |
| JIT and deep recursion | A function with a large frame can stop at a smaller depth under the JIT than in the interpreter (8 MB of native stack) |

### Verification

All PoCs were rerun against the fixed build. Regression tests: the logout race across two stores and the regeneration
race (`session`, also against PostgreSQL, MySQL and Redis), the Redis count and byte limits, the rate-limit windows and
reset (`tests/db`), unawaited work (`tests/http`), separate slots for MySQL connects (`netio`), the generic soundness
cases, template jumps and `json_html()` (`tests/cases`), deep JIT recursion (`jit`), the JIT check (`phv_jit`), the
formatter (`format`), links, URLs and names in `phv install` (`install`), and the package boundary
(`tests/packages`). All tests pass on Windows and on Linux (Docker) with clippy (`-D warnings`) and rustfmt;
`tests/db` passed against PostgreSQL 17, MySQL 8.4 and Redis 7, and the fuzzer ran 1,500 mutations per program.

Dependencies: Dependabot reported eight advisories (four high) for `undici` 7.29.1 in the website's deploy tooling
(`.github/deploy`, pulled in by wrangler, which runs only in the deploy job); it is overridden to the fixed 7.30.1, and
a clean `npm ci --ignore-scripts` gives a working wrangler. `npm audit --omit=dev` finds nothing in the extension and
the website; `cargo audit` runs in CI.

## 12. Follow-up on the Residual Risks (after v0.5.0)

Date: 2026-11-03. The two residual risks of §11 and the item kept there were fixed, and the work turned up two more
problems of the same kind (compile memory of very large functions, and the time limit after long string operations).

| # | Risk | Fix | Verified |
|---|---|---|---|
| R5-24 | (§11 residual risk) A `json_html()` value passed as `any` and narrowed with `is Html` is `Html` at run time, and could be placed in an attribute, whose quotes it ended | An `Html` value placed in a quoted attribute value has its `"` and `'` encoded (`&quot;`, `&#039;`; the browser decodes them back). `escape()` output has none, so it is unchanged; a template's output or JSON in an attribute stays inside it. Element text is unchanged | `tests/cases/html_in_attributes.phv`: the laundered JSON and a template's output stay in their attributes |
| R5-25 | (§11 residual risk) Under the JIT, a function with a large frame stopped at a smaller recursion depth than in the interpreter (8 MB of native stack) | Native code that runs out of its stack stops with its own code, and the interpreter runs that call (and the calls it makes) instead. Compiled functions have no side effects (arithmetic, strings, calls to other compiled functions), so running the call again gives the same result | The depth-9,900 recursion of a 300-variable function returns its result under the JIT (`jit` test); the PoC page answers 200 instead of 500 |
| R5-26 | (§11, kept) Registers for a call were allocated before the memory check: a deep recursion of a 2,000-register function reached 517 MB under a 16 MB limit | The interpreter's register file and the JIT's string window check the memory limit before they grow | The same program under 16 MB: 6 MB with the interpreter, 12 MB with the JIT (peak working set) |
| R5-27 | (New, Low) Compiling a very large function takes memory not counted against any limit: the 2,000-register function took 159 MB to compile | Functions with more than 1,024 registers or 16,384 instructions are left to the interpreter. A 900-register function compiles within 41 MB | Peak working set measured as above |
| R5-28 | (New, Low) The time limit is checked every 10,000 loop steps or calls, and one comparison or `.` of long strings can take milliseconds: comparing two 20 MB strings 3,000 times ran 4 s under a 2 s limit, in both engines | After a comparison, `.` or `.=` over 64 KB or more, the next loop step or call checks the limits (interpreter and JIT) | The PoC stops at 2.1 s (interpreter) and 2.4 s (JIT, including compilation); `jit` test `time_limit_after_long_string_operations` |

### Verification

All tests pass on Windows and on Linux (Docker) with clippy (`-D warnings`) and rustfmt, and the fuzzer ran 1,500
mutations per program.

## 13. Sixth Assessment (v0.5.2: `final`, warnings for unused code, errors of several files at once)

Date: 2026-11-03. Scope: everything changed after v0.5.1: the compiler's check for functions too large for the
register file, `final` classes and methods, the warnings for unused variables and imports and unreachable code, and
type checking the files that loaded cleanly while another file has a syntax error. The other changes since v0.5.1 are
a test that now runs alone and the demo's Dockerfile.

### Method

A new reviewer, given only the source and the change, looked at the front end as `phv lsp` sees it (hostile source in
an untrusted workspace, within the 8 MB per file) and at what the compiler hands the VM. Every finding was confirmed
with a working PoC against a release build, and timed: generated sources with 65,537 properties, 80,000 parameters or
captures, and import chains of 2,000 modules behind an entry file of 200,000 imports. A separate fuzz run cut and
spliced the 50 test programs at 12 points each (1,800 programs through `phv check`). All PoCs were rerun against the
fixed build, and regression tests were added.

### Summary of Results

| Severity | Count | Fix |
|---|---:|---|
| High | 1 | Fixed |
| Medium | 1 | Fixed |
| Low | 2 | Fixed |
| Info | 1 | Fixed |

### High

| # | Finding | Fix |
|---|---|---|
| R6-1 | A property's slot is a 16-bit operand of the bytecode, and the compiler cut the checker's 32-bit slot to it: in a class with more than 65,536 properties, property 65,536 was property 0. `$o->f65536 = 7` stored an int in the `string` property `f0` (a panic of the VM when read as a string), and it wrote `readonly` and private properties of the class from outside. Found while auditing the register check; present since the first release | A class may have at most 65,535 properties, inherited ones included (a compile error names the class). The compiler reports a slot that does not fit instead of cutting it |

### Medium

| # | Finding | Fix |
|---|---|---|
| R6-2 | Finding the files whose type errors are held back (those using a file with a syntax error) went over every module and every `import` statement again for each step of a chain: 2,000 modules in a chain behind an entry file of 200,000 imports took 60 s in `phv check` (1.7 s without the syntax error). New in this release | One walk over the files that use each file (each import counted once): 1.5 s |

### Low

| # | Finding | Fix |
|---|---|---|
| R6-3 | The check for a repeated parameter name compared each parameter with all before it: a method of 80,000 parameters took 8.5 s to check, also in `phv lsp`. Present before this release | The names are kept in a set: 0.2 s (the method is then too large to compile) |
| R6-4 | Properties were found by name with a linear search when declared and when used: checking a class of 65,537 properties took 5 s, also in `phv lsp`. Present before this release | Each class keeps its slots by name: a class of 65,000 properties is checked and run in 0.7 s |

### Info

| Item | Result |
|---|---|
| The new warnings looked through the parameters and the captured variables for each unused variable (quadratic in principle; 1.2 s for 80,000 captures, which the compiler then refuses) | Fixed: sets |

### Residual Risks

None.

### Verification

All PoCs were rerun against the fixed build: the three property PoCs are compile errors, the import chain takes 1.5 s
and the 80,000 parameters 0.2 s. Regression tests: `oversized_functions_are_errors` (65,536 properties, 70,000
locals, a literal of 40,000 pairs) and `tests/cases/err_mixed.phv` (a chain through `lib/chain.phv`), with
`err_final`, `err_final_syntax` and `lints` for the features themselves. All tests pass on Windows, and on Linux and
macOS in CI, with clippy (`-D warnings`) and rustfmt, and the fuzzer ran 1,500 mutations per program.

Dependencies: `cargo audit` (CI) and `npm audit --omit=dev` in the extension and the website find nothing; Dependabot
has no open alerts.

## 14. Seventh Assessment (v0.6.0: destructuring, variadics, functions as values, type aliases, bounds, constant expressions)

Date: 2026-11-04. Scope: everything changed after v0.5.2 (`git diff v0.5.2..main`): heredoc and nowdoc strings and
`\x` escapes; constant expressions; `import ... as`; type aliases; variadic parameters and `...` spreading;
functions as values (`f(...)`, which builds a closure around a call); destructuring and `foreach` patterns; `match`
exhaustiveness and narrowing in `match (true)`; bounds on type parameters; and the exception's code, cause and trace
(`record_trace` and the cause chain in the VM). The only other change is the demo's Dockerfile.

### Method

The features were reviewed three times: by the author while writing them (timing generated sources against a
release build), by an independent reviewer looking for correctness bugs, and by a second independent reviewer doing
this audit with the threat model of §12–§13: hostile source in an untrusted workspace for `phv check` and `phv lsp`
(within the 8 MB per file), type soundness (a program that type-checks must not break a type at run time, which is how
private and readonly state is protected), and no VM panic for any checked program. Every finding was confirmed with a
working PoC against a release build, timed and measured (peak memory), and rerun against the fixed build. Several
findings were older slow paths that the new features made easy to reach; they are marked.

### Summary of Results

| Severity | Count | Fix |
|---|---:|---|
| High | 3 | Fixed |
| Medium | 4 | Fixed |
| Low | 1 | Fixed |
| Info | 2 | Fixed / no change needed |

### High

| # | Finding | Fix |
|---|---|---|
| R7-1 | A variable assigned through a destructuring pattern (`[$x] = ...`, `foreach (... as [$x])`) inside a loop or a `try` was not seen as assigned there, so a narrowing from before the loop survived it. With `$x` narrowed to `Foo` and `[$x] = [pick()]` in the loop, the next pass read and wrote a private property of another class through `$x->s` (the PoC printed a `Vault`'s secret), or stopped the VM with an internal error | Every variable of a pattern (nested, map values, `...$rest`) counts as assigned, like a plain assignment |
| R7-2 | Bounds on type parameters could be bypassed: through an unbounded `T` (`f<T>` calling `g<U: Shape>($x)`, also in class headers and constructors), by an override adding a bound, by an `int` for `T: float` (used as a float without conversion), by a type alias without parameters (`type Bad = Box<int>;`), and by `new Box(5)` taking its class arguments from the expected type. Each ended in a VM internal error with an `int` where an object was expected | Bounds are checked in signatures and class headers too, exactly (no conversion), on every alias use and on constructor arguments from the expected type; an override may drop a bound but not add or change one |
| R7-3 | Bounds that name each other (`<T: U, U: T>`) made the bound check loop forever: `phv check` and `phv lsp` hung | A bound cannot be another type parameter |

### Medium

| # | Finding | Fix |
|---|---|---|
| R7-4 | Constant expressions and type aliases could take exponential memory or time: constants that double a string (`const B = A . A;`), aliases that double a type (`type T2 = fn(T1): T1;`), generic aliases applied to large aliases, and many uses of a large alias (a 295 KB file took 57 s); and a chain of 100,000 aliases overflowed the stack. Copies of constant strings were not limited either: 3,000 uses of a 1 MiB constant took 3 GB (uses in function bodies did so before this release too) | A constant string is at most 1 MiB, 16 MiB for a whole program, and is shared by its uses (`Rc<str>`) instead of copied; an alias stands for at most 10,000 type nodes, all uses together for 2,000,000, measured before it is copied; aliases nest at most 64 deep |
| R7-5 | A `match` that misses cases named every missing case: an enum of 20,000 cases and 20,000 one-arm matches took 51 s and 9.5 GB | At most 5 missing values are named ("and N more cases"), and the search stops after them |
| R7-6 | Type parameters are looked up by name for each use: an alias with 700,000 parameters took 38 s, and a function with very many took 10 s before the "at most 4" error (the latter since 0.5.0) | More than 4 type parameters is refused while parsing |
| R7-7 | Each use of a union type looks at all of its members: a bound of 3,000 members used 100,000 times took 23 s, and a parameter of 3,000 members called 100,000 times 37 s (the latter since earlier releases) | A union written in the source has at most 256 members |

### Low

| # | Finding | Fix |
|---|---|---|
| R7-8 | More than 8 type parameters made the bound check index past a fixed array: `phv check` panicked | Fixed with R7-6, and the lookup no longer indexes past the end |

The author's own review fixed, before the independent reviews: many spreads in one literal nested the compiler 100,000
deep (now joined in pairs), the exhaustiveness check compared every arm with every case, and two older linear searches
made a `match` over 60,000 enum cases take 4.6 s (enum cases and class constants are now looked up by name: 0.2 s);
an unclosed `\u{` scanned the rest of the file for each one (older; now at most 8 digits are read).

### Info

| Item | Result |
|---|---|
| An exception whose cause is itself repeated its message 16 times in the uncaught-error text | Fixed: a cycle is shown once (`(caused by ...)`) |
| What the new exception data exposes | No change: in production (`--prod`) an uncaught error still shows only the generic error page; `getTraceAsString()` gives function names and file paths as the program names them, and reaches visitors only if the program echoes it |

### Residual Risks

None.

### Verification

All PoCs were rerun against the fixed build. The type-confusion and bound PoCs are compile errors; the hanging bound
check is a compile error; the 0.9 MB exhaustiveness PoC takes 0.16 s and 82 MB (was 51 s and 9.5 GB); 3,000 uses of a
1 MiB constant peak at 9 MB (was 3.0 GB); the alias, union and type-parameter PoCs end with their errors in under
0.7 s; 200,000 spreads run in 0.9 s. Regression tests: `tests/cases/err_flow_destructure.phv` (loops, `foreach` and
`try`), `err_bounds.phv`, `err_match_exhaustive.phv`, `err_destructure_syntax.phv`, and
`constants_aliases_and_spreads_stay_small` in `crates/phv_driver/tests/golden.rs` (constant strings, alias size, depth
and total, unions, `\u{`, shared constants, spreads), with golden tests for each feature. All tests pass on Windows, and
on Linux and macOS in CI, with clippy (`-D warnings`) and rustfmt, and the fuzzer mutates the new test programs too.

Dependencies: `cargo audit` (CI) and `npm audit --omit=dev` in the extension and the website find nothing; Dependabot
has no open alerts.

## 15. Eighth Assessment (v0.7.0: routes, middleware, error pages, static files, mail, migrations, test client)

Date: 2026-11-04. Scope: everything changed after v0.6.0 (`git diff v0.6.0..main`): routes with parameters
(`[id]`, `[...rest]`) and the route table, `_middleware.phv`, `_404.phv` and `_500.phv`, conditional and range
requests for static files, gzip, `[cors]`, the access log, graceful shutdown, `layout()`, `std.validate`, `std.mail`
(an SMTP client with STARTTLS), `phv db migrate`, `std.testing` and the in-process site of `phv test`, and the new
`phv.toml` settings. The only other change is the demo's Dockerfile.

### Method

A new reviewer, given the source and the change, attacked a release build as an untrusted client: `phv serve --prod`,
a site built with `phv build`, and a fake SMTP server. Requests tried every spelling of a path that could reach a
private file or skip a middleware (percent-encoding, case, Windows 8.3 names and `::$DATA`, trailing dots and spaces,
double slashes, `[id]` written literally, route parameters, the 404 page, the built site), compression with session
and CSRF secrets (BREACH), ranges and validators at their edges, CORS origins, stalled readers holding responses, and
injection through every argument of the mail API. Every finding was confirmed with a working PoC, and rerun against
the fixed build.

### Summary of Results

| Severity | Count | Fix |
|---|---:|---|
| High | 0 | – |
| Medium | 1 | Fixed |
| Low | 3 | Fixed |
| Info | 3 | Fixed / documented |

### Medium

| # | Finding | Fix |
|---|---|---|
| R8-1 | gzip skipped responses for one visitor by looking at their headers (`Set-Cookie`, `Cache-Control: private` / `no-store`), and the server adds `private, no-store` only when the page set no `Cache-Control` of its own. A page that used the session or printed a CSRF token and set `Cache-Control: no-cache` was compressed with the secret next to reflected input, and pages that read other cookies or `Authorization` were always compressed (BREACH) | Whether a response is personal is decided once, from what the page did, and carried with the response; and no page answering a request that carries cookies or `Authorization` is compressed (static files still are) |

### Low

| # | Finding | Fix |
|---|---|---|
| R8-2 | `replyTo()` accepted a display name, so a value from a form could list more addresses (`a@evil.example, "x" <b@evil.example>`); line breaks were already refused | The reply-to address is a bare address, like the recipients |
| R8-3 | Static text files of up to 1 MB were read whole on every request, even for clients that do not accept gzip, compressed again each time, and not counted in the server's limit on output waiting to be sent: 900 connections that stopped reading a 1 MB `.js` held 945 MB | Read whole only when the response will be compressed, counted in that limit; each compressed version is kept (up to 32 MB in all) instead of compressed again |
| R8-4 | `is_email()` accepted line breaks, tabs and quotes (`x@e.com\r\nBcc:...`), and `url()` accepted `"` and `<>` | Both refuse control characters, quotes and angle brackets (and `is_email` also `(),;:[]\`) |

### Info

| Item | Result |
|---|---|
| A `"` in a path was written unescaped into the text access log | Fixed: `"` and `\` are escaped |
| `_middleware.phv` runs before pages, not before static files under its directory | Documented in spec §14, with the way to serve protected files |
| Each connection could buffer about 700 KB for a client that stopped reading (hyper's default buffer), so 900 stalled connections held 640 MB even for streamed files (since earlier releases) | Fixed: a connection's buffers are capped at 64 KB; 900 stalled readers of a 1 MB file now leave the server at 8 MB |

### Residual Risks

None.

### Verification

All PoCs were rerun against the fixed build: the session and CSRF pages with their own `Cache-Control` are sent
uncompressed while static files still are compressed, the reply-to list is refused, 900 stalled readers of a 1 MB
`.js` or `.png` leave the server at about 8 MB, and the loose addresses and URLs are refused. Regression tests:
`caching_ranges_gzip_cors_and_shutdown` and `routes_middleware_and_error_pages` in `crates/phv_server/tests/http.rs`,
`test_client_runs_pages_in_process`, `mail_reaches_a_local_server` and the unit tests of `mail.rs`, `routes.rs`,
`static_files.rs` and `migrate.rs`, `tests/cases/validate.phv` and `layout.phv`, and the server migrations in the CI
database job. What held up: routing and middleware for every spelling tried (also on a built site), private `_` files,
the route scan (links are not followed; depth and page limits), production error pages, ranges and validators, CORS,
STARTTLS stripping (refused for other machines), `layout()` with exceptions and nesting, `std.testing` (its builtin
cannot be called from user code), `phv db` path checks, and shutdown. All tests pass on Windows, and on Linux and macOS
in CI, with clippy (`-D warnings`) and rustfmt.

Dependencies: `cargo audit` (CI) and `npm audit --omit=dev` in the extension and the website find nothing; Dependabot
has no open alerts. New crates: `signal-hook-registry` and `errno` (Unix only, for tokio's signal handling).

## 16. Follow-up: Compressing Proxies (v0.7.1)

Date: 2026-11-04. After v0.7.0 was deployed to the demo, its pages were checked through Fly.io: `/board`, which
carries a CSRF token and is sent uncompressed by PHV (§15, R8-1), arrived compressed. Fly.io's proxy compresses every
response the app leaves without a `Content-Encoding` header, and other proxies and CDNs can do the same, so PHV's rule
of not compressing personal responses does not protect them behind such a proxy.

| # | Finding | Fix |
|---|---|---|
| R9-1 (Medium) | Behind a compressing proxy, a page's CSRF token was compressed next to text from the request, so its size could reveal the token (BREACH) | `csrf_field()` and `csrf_token()` write the token masked with a random pad of its own each time (pad, then token XOR pad: 128 hex digits); the server unmasks it before comparing, and still accepts the plain token, which nothing writes any more |

Other secrets a page shows next to request text are the application's, and are not masked; spec §14 now says that a
compressing proxy can undo the server's rule and suggests turning its compression off for such pages.

Verification: a new reviewer checked the change against a release build with raw HTTP. Masked tokens (also replayed,
in uppercase, in `X-CSRF-Token`) and plain tokens are accepted; empty, short, long, non-hex and non-ASCII values, a pad
paired with itself, another session's token and tokens from before `regenerate()` are refused, and no input made the
server panic. No other code writes or compares the token. Regression tests: `masked_tokens_differ_and_verify` in
`crates/phv_std/src/web.rs`, `serves_pages_safely` (two loads of a form give different tokens that both work) and
`tests/cases/stdlib2.phv`. Residual risks: none.

## 17. Ninth Assessment (v0.8.0: the standard library)

Date: 2026-11-04. Scope: everything changed after v0.7.1 (`git diff v0.7.1..main`): the new list and map methods,
mathematics, number bases and encodings, UUIDs, `format()`, `std.regex`, the file and directory functions, CSV, URLs,
logging, `argv()` and standard input, `std.time`, `json_decode_as()` and `json_encode_pretty()`, `encrypt` /
`sign_token` / `jwt_*`, and the new dependencies (`regex-automata` directly, `chrono-tz`, `ring` directly).

### Method

A new reviewer, given the source and the change, attacked a release build: scripts under a 64 MB memory limit
(measuring the process's peak memory), `phv serve --prod` sites whose pages pass a visitor's input to the new
functions, and the server run in a Windows job capped at 1 GB to show what an out-of-memory machine does. The file
functions were tried against junctions, file and directory symlinks, hard links to a file outside, device and stream
names and code locations. Every finding was confirmed with a working PoC, and rerun against the fixed build.

### Summary of Results

| Severity | Count | Fix |
|---|---:|---|
| High | 4 | Fixed |
| Medium | 1 | Fixed |
| Low | 4 | Fixed |
| Info | 8 | Fixed / documented |

None of the findings escapes `[permissions]`; all are about the memory and time limits, or a panic.

### High

| # | Finding | Fix |
|---|---|---|
| R10-1 | `json_encode_pretty()` indented the compact JSON afterwards, without the memory checks of `json_encode`: each item at depth 127 grows by 256 bytes, so a 4 MB JSON body sent to a page that pretty-printed it made the server allocate about 1 GB and abort | The encoder writes the indented form itself, checking the limits as it goes, like compact output |
| R10-2 | `flatten()`, `flatMap()` and `csv_encode()` built their result before the memory limit was checked: a list that holds one shared list many times is small, but its flattened or encoded form is not (3.5 GB from a 64 MB script; a page doing it aborted the server) | The size of the result is computed first and reserved against the limit (for `flatMap`, part by part) |
| R10-3 | `std.regex` searches cannot be interrupted, and a large pattern (about 20,000 NFA states) makes the regex crate fall back to a search that costs states × text length: a visitor's pattern held a worker for 142 seconds under a 30-second limit, and longer texts for many minutes | Before each search, its worst case (states × length of the text) is compared with the time the script has left; a search that could not end in time throws `RuntimeException` without starting. Compiled patterns are limited to 2 MB (was 4 MB) |
| R10-4 | `matchAll()`, `replace()`, `replaceWith()` and `split()` built results of any size in one call (up to 2.9 GB from a 64 MB script), and the regex crate's table for groups grows with states × groups (30,000 empty groups); a 181 KB form post aborted the server | Patterns have at most 1,000 groups, and those whose table for groups would exceed 16 MB are refused; that table is reserved before each search for groups, and each match, replacement and piece is reserved before it is added |

### Medium

| # | Finding | Fix |
|---|---|---|
| R10-5 | Each worker thread kept up to 64 compiled patterns of up to 4 MB each (about 300 MB per worker), memory that outlives the request and no limit counts: 300 requests with different patterns took a 2-worker server from 4 MB to 219 MB | The cache holds at most 32 MB per thread, counted generously (the NFAs three times over, both lazy DFAs at their limit of 512 KB, and the table for groups); larger patterns are not cached |

### Low

| # | Finding | Fix |
|---|---|---|
| R10-6 | Sorting floats that include NaN (`sorted()`, `sortedByValue()`, `sortedByKey()`) panicked in Rust's sort ("does not correctly implement a total order"); the new `log`, `sqrt`, `asin` and `acos` make NaN reachable from a visitor's numbers. A page gave a 500 and a script lost its output | These sorts use a merge sort that cannot panic, with NaN after every number |
| R10-7 | `json_decode_as()` set readonly properties that are not constructor parameters (`public readonly bool $admin = false;`), which `new` can never change: a JSON body could make an `admin` the class otherwise cannot have | A class with such a property is a compile error for `json_decode_as()` |
| R10-8 | `stdin_read_all()` and `stdin_read_line()` (only in `phv run`) read any amount of input: 600 MB piped in held 1 GB under a 64 MB limit | They read at most what the memory limit leaves, then stop with the memory error |
| R10-9 | `chunk()` and `groupBy()` overshot the memory limit 6 to 8 times before it was checked | The result is reserved before it is built |

### Info

| Item | Result |
|---|---|
| The Typed JSON example of the spec used `request()->text()`, which does not exist | Fixed: `request()->body` |
| `mkdir("./data/x")` was refused, unlike the same path without `./` | Fixed |
| An error of `rename()` named the source when the destination was the link | Fixed: it names both paths |
| Log values wrote U+2028, U+2029 and the characters that reorder text (U+202E...) as they are | Fixed: escaped like control characters |
| `csv_encode()` writes cells starting with `= + - @` as they are, which spreadsheets run as formulas | Documented in spec §15 (CSV is data; prefix such cells with `'` for spreadsheets) |
| `decrypt`, `verify_token` and `jwt_decode` accepted spaces around a token, so a token had several spellings | Fixed: tokens are taken as they are |
| The JWT key is used as it is (as other JWT libraries expect), so the same key used with `hash_hmac()` on a visitor's text would sign tokens | Documented in spec §14 |
| `log_*()` used `eprintln!`, which panics when standard error is closed | Fixed: a failed write is ignored |

### Residual Risks

- The time check for regular expressions uses the worst case, so a large pattern on a long text can be refused even
  when the search would have been quick (`\w{40}` on 1 MB of text with 30 seconds left). Ordinary patterns search
  megabytes; spec §14 explains the limit. The estimate assumes about 10^8 steps a second, about 75% of what the
  reviewer's machine did, so on a machine several times slower a search can end some seconds after the time limit.

### Verification

All PoCs were rerun against the fixed build. Under a 64 MB limit the pretty-printed JSON, `flatten`, `flatMap`,
`csv_encode`, `chunk` and the regex calls now stop at the limit (peak 2 to 78 MB, the input included, instead of 0.4
to 3.5 GB); the NaN sorts work; the readonly class is refused; 600 MB on standard input stops at the limit. Against
the site: the original pattern with the 1 MB body is refused in 80 ms, and a 500 KB body with a pattern of
4,000 states ends in 14 seconds, under the 30-second limit; the pretty-printing, flattening
and form-search pages answer 500 at the limit and the server stays at about 7 MB; 300 different patterns leave it at
15 MB. Regression tests: `crates/phv_driver/tests/limits.rs` (every builder and regex call under a 16 MB limit, and a
refused search under a 2-second limit), the unit tests of `re.rs`, `textio.rs` and `crypt.rs`,
`tests/cases/collections2.phv` (NaN), `err_json_as.phv` (readonly) and `files_and_directories` (`./`). What held up:
every file function against links, junctions, hard links, stream and device names and code locations (on Windows;
the Unix code was reviewed and is tested by CI); `json_decode_as()` with every kind of class, nesting and number;
`format()`; encryption (fresh salt and nonce, constant-time checks, no algorithm confusion between the token kinds
or in JWT); `std.time` at the edges of its ranges; `argv` and standard input outside `phv run`. All tests pass on
Windows, and on Linux and macOS in CI, with clippy (`-D warnings`) and rustfmt.

Dependencies: `cargo audit` (CI) and `npm audit --omit=dev` in the extension and the website find nothing. New crates:
`chrono-tz`, `phf` and `phf_shared`; `regex`, `regex-automata`, `url` and `ring` were already in the lockfile.

## 18. Tenth Assessment (v0.9.0: editor features, debugger, test tools, distribution)

Date: 2026-11-04. Scope: everything changed after v0.8.0 (`git diff v0.8.0..main`): the new features of `phv lsp`
(references, rename, formatting, signature help, symbols, quick fixes, semantic tokens, inlay hints), `phv new`,
`phv serve --watch`, `phv test` (hooks, filters, JSON and JUnit reports, coverage), `phv run --profile`, `phv doc`,
the debugger (`phv dap`) and the VS Code extension's debug type, and the release workflows for Linux on ARM, static
builds, the Docker image, Homebrew and Scoop.

### Method

A new reviewer, given the source and the change, attacked a release build as the author of an untrusted repository
that a developer opens in the editor or runs the tools on (with an LSP and a DAP client measuring time and memory),
as a web page the developer visits while `phv serve --watch` runs, and as someone who can push to the repository or
control a release's assets (the workflows were read and their steps reproduced locally, as GitHub Actions cannot be
run here). Every finding was confirmed with a working PoC, and rerun against the fixed build.

### Summary of Results

| Severity | Count | Fix |
|---|---:|---|
| High | 0 | – |
| Medium | 4 | Fixed |
| Low | 6 | Fixed |
| Info | 7 | Fixed / documented |

None of the findings escapes `[permissions]`.

### Medium

| # | Finding | Fix |
|---|---|---|
| R11-1 | The workflows that publish a release (`docker.yml`, `package-managers.yml`, and `publish-extension.yml` since 0.5) checked out the release's tag by its bare name, and `actions/checkout` prefers a branch of the same name: whoever can push a branch `v0.9.0` could have the Dockerfile, the formula generator or vsce's lockfile come from it, with the job's token for ghcr.io, the Homebrew tap or the Marketplace | Each checks out `refs/tags/<tag>` and checks that the commit is the tag's before anything else |
| R11-2 | Semantic tokens (asked for whenever a file is opened, also in an untrusted workspace) looked every definition up by a linear search and computed each position from the line's start: a file of 100,000 uses of one variable on one line took 20 s, and 800,000 uses 160 s and 1.3 GB, with the server answering nothing else | Definitions are looked up in a map built once, and positions are found in one pass over the text; at most 100,000 tokens |
| R11-3 | Inlay hints computed each position from the start of the file (a 589 KB line of assignments: 28 s), and the quick fixes compared unknown names with every known one by edit distance whatever their length (a 60,000-character name: 2.5 s per cursor move, minutes near the 8 MB limit) | One pass for positions; names longer than 64 bytes, or of very different length, are not compared |
| R11-4 | `workspace/symbol` read every `.phv` file of the project whole, of any size (a 100 MB file: 27 s and 25 GB of memory; 2 GB: 196 GB committed), where the compiler reads at most 8 MB and only regular files | Every file the language server and `phv doc` read goes through the compiler's reader (regular files, at most 8 MB) |

### Low

| # | Finding | Fix |
|---|---|---|
| R11-5 | Find references re-read the file of each result (100,000 results: 25 s); references and rename analyzed up to 300 files with no time bound after walking the whole tree; finding the methods related by overriding was cubic (1,500 classes: 30 s) | Each file is read once; the walk stops at 300 files and the search after 10 seconds (rename then refuses rather than rename part of the uses); the related methods are found breadth first with sets |
| R11-6 | `phv doc` wrote through a link or junction planted on the output directory (`doc/` of a cloned project could point anywhere), and read source files of any size | A link anywhere on the output's directories is refused before anything is created; files are read as the compiler reads them |
| R11-7 | The debugger escaped a string in full before shortening it, and showed map keys in full: a 40 MB key gave a 240 MB message (916 MB in the adapter), and a list of 1,000 such strings did not answer in 120 s | Strings and keys are shown from their first 500 characters only |
| R11-8 | `/__phv/changes` of `phv serve --watch` counted only files toward its limit, so a tree of 60,000 empty directories took 7 s a request, asked for each second by every open tab (and any page in the browser) | Directories count toward the limit; the stamp is kept for a second, shared by all requests |
| R11-9 | `docker.yml` created a BuildKit builder from an unpinned image, which received the push credentials | The engine builds each architecture itself (the Dockerfile only copies files), and a manifest list joins them: no builder image |
| R11-10 | The publish workflows ran for any published release and on manual dispatch with any tag, so publishing an older release (or a dispatch) moved `:latest`, the Homebrew formula and the Scoop manifest back | They refuse a release that is not the latest; `docker.yml` runs in an environment of its own |

### Info

| Item | Result |
|---|---|
| `docker.yml` accepted an archive missing from `SHA256SUMS` (`--ignore-missing`) and copied `phv` even if it was a link | Fixed: each archive must be listed by its exact name with that hash, and `phv` must be a regular file |
| The environments of the publish workflows should allow deployments from `v*` tags only, and `package-managers` (which holds a token) needs a reviewer | Documented in releasing.md; settings for the owner |
| A crafted `import` with braces in a comment made the "remove the import" quick fix panic (caught; answered as an internal error) | Fixed: such an import gets no fix |
| `--junit` wrote U+FFFE and U+FFFF, which XML cannot hold | Fixed |
| Doc comments reached the website's library reference as HTML (`(?<name>...)` was taken for a tag) | Fixed: `phv doc --format md` writes `<` and `&` in prose as entities |
| The tutorial's tests empty the database the site uses | Documented in the tutorial |
| A source file of 8 MB of NUL bytes gave one error per byte (2.45 GB, also in the language server on every keystroke; older than this release) | Fixed: the lexer stops after 200 errors in a file |

Diagnostics had the same problem as R11-3: one long line with 60,000 warnings took 20 s before every other answer.
The language server now publishes at most 1,000 per file (errors first), with their positions found in one pass.

### Residual Risks

- References and rename stop searching after 10 seconds: in a project large enough to reach that limit, references
  may be incomplete (rename refuses instead).
- The publish workflows were checked by reading them and running their steps locally; they first run on GitHub
  when 0.9.0 is published.

### Verification

All PoCs were rerun against the fixed build: semantic tokens on the 800 KB line 0.18 s (was 19.75 s), inlay hints 0.39 s
(27.7 s), the quick fix with a 60,000-character name 0.01 s (2.47 s), references with 100,000 results 1.7 s (25.4 s),
the 1,500-class chain 0.19 s (30 s), opening the file with 60,000 warnings 0.24 s (20 s), `workspace/symbol` with a
100 MB file 0.00 s and 8 MB (27 s, 25.5 GB), the 40-file project stops at the time limit (33 s); `phv doc` refuses
the junctioned `doc` and skips the 30 MB file (6.4 GB); the debugger's map with a 40 MB key 3 KB in 0.00 s (240 MB)
and the list of 1,000 such strings 0.02 s (over 120 s); `/__phv/changes` over 60,000 directories 1.2 s once a second
and under 1 ms otherwise (7 s each); the crafted import gets no fix and no error. The image's two architectures were
built locally with the engine's builder. Regression tests: `positions_in_one_pass_and_bounded_suggestions` and
`workspace_symbols_skip_huge_files` (phv_cli), `related_members_of_a_long_chain` (phv_types), `junk_files_give_few_errors`
(phv_syntax), `huge_values_are_shown_short` and `junit_xml_holds_only_xml_characters` (phv_cli), and the link test of
`phv doc` (Unix). What held up: rename stays inside the project (a junctioned `lib` is refused as an import) and only
returns edits; `phv doc` escapes everything in HTML; `phv new` validates names and never writes over or through
anything; the debugger keeps stdin from the script, evaluates only variable names, and applies phv.toml's permissions
and memory limit; hooks with parameters are compile errors; coverage and profiling markers exist only when asked
for; `--watch` is refused with `--prod`; `render.py` rejects a version that could inject Ruby and a missing archive;
workflow inputs reach scripts only through the environment and are checked; the image is `FROM scratch` and runs as
an unprivileged user; `phv.path` stays machine-scoped. All tests pass on Windows, and on Linux and macOS in CI, with
clippy (`-D warnings`) and rustfmt.

Dependencies: no new crates; `cargo audit` (CI) and `npm audit --omit=dev` in the extension and the website find
nothing.

## 19. Follow-up: an Outside Review of v0.9.0 (v0.9.1)

Date: 2026-11-04. An outside review of the v0.9.0 source, made with ChatGPT, reported six items. Each was confirmed
with a PoC against a release build before it was fixed (one by reading: it needs a second user on a Unix machine),
and two new reviewers then attacked the fixes in turn, which turned up three more. All the findings are older than
v0.9.0 except R12-6 and R12-8, which were in earlier versions of a fix and never released.

| # | Finding | Fix |
|---|---|---|
| R12-1 (Medium) | `phv serve` bounded each request body (8 MB) but not the bodies being received together: each of its 1,024 connections could hold one, so clients that sent all but the last byte of 8 MB bodies made the server hold 495 MB with 60 connections and 1,617 MB with 200 | Past its first 64 KB, a body is received only within a budget of `workers` × 8 MB, taken as the bytes arrive and kept until its script ends; bytes that cannot get their share within 10 seconds get 503, and a body stated larger than 8 MB gets 413 at once |
| R12-2 (Medium) | Query and form fields were all decoded before their count was checked against the limit of 1,000: 8 MB of `a&` took 252 MB | Fields are counted while they are parsed; the 1,001st gives 400 |
| R12-3 (Medium) | `json_decode`, `json_decode_as` and `request()->json()` built the whole value before the run's memory limit was checked: 8 MB of `[[],[],…]` took 249 MB under a limit of 64 MB | The run's memory and time limits are checked every 4,096 values |
| R12-4 (Low) | `phv install` ran git with hooks from a directory of a fixed name in the shared temporary directory: on Unix another user of the machine could create it first, with hooks that git would run as the developer | Each git command gets a new directory with a random name, private to the user (0700), removed afterwards |
| R12-5 (Low) | `website/serve.mjs` (the site's local preview) stopped on a malformed escape in a URL (`/%E0%A4%A`), and would stop on a file that vanished while it was read | Such a URL gets 400, and a read error ends only its response |
| R12-6 (Medium, in the first fix) | The first fix of R12-1 reserved a body's share before reading it, from its stated length or 8 MB for a chunked body: `workers` requests that sent no body held the whole budget, and every upload over 64 KB got 503 | The share is taken as bytes arrive, so a client holds only what it has sent |
| R12-7 (Low) | `parse_query()` built its whole map before the run's limits were checked: 8 MB of distinct short fields took 251 MB under a limit of 16 MB | The limits are checked every 4,096 fields |
| R12-8 (High, in the second fix) | The second fix kept a body's chunks as they arrived: a body sent a byte at a time cost a handle per byte, about 20 times its size, and the free first 64 KB took no budget at all, so 800 connections sending 64 KB in 1-byte chunks made the server hold over 1 GB, still growing | A body is gathered in one buffer, and the budget pays for the buffer's room past its first 64 KB, not only the bytes in it |

The sixth item of the review, `rustls-pemfile` (through the MySQL client), was already known: RUSTSEC-2025-0134 says
only that it is unmaintained, and `.cargo/audit.toml` records why it is accepted.

### Residual Risks

- A client that sends a body at full speed holds its share of the budget until its script ends (at most the 30
  seconds a body may take, then the script's time limit). Clients that keep doing so make other uploads over 64 KB
  wait, then get 503, as clients that keep every worker busy already do.
- The first 64 KB of each body, and each connection's buffers, are outside the budget: about 180 KB a connection,
  at most 1,024 connections (1,000 connections sending their bodies a byte at a time: 178 MB).
- `phv serve` has no limit per client address: one address that opens 1,024 idle connections makes other clients
  wait for a free connection (up to the header timeout). A site open to the internet should run behind a reverse
  proxy that limits connections per address (spec §14).

### Verification

The PoCs were rerun against the fixed release build (`workers = 4`): with 50 requests that send no body, a 100 KB
upload gets 200 in under 0.01 s (with the first fix: 503 after 10 s with 4); 200 clients holding all but the last
byte of 8 MB bodies make the server hold 90 MB (1,617 MB), and 1,000 sending 64 KB in 1-byte chunks 178 MB (with the
second fix: over 1 GB with 800); 4 million form fields get 400 in 0.01 s at 17 MB
(252 MB); the JSON bodies and `parse_query()` stop at the run's 16 MB limit, the server peaking at 36 MB and 39 MB
(249 MB and 251 MB); hooks planted for `phv install` ran 25 times with v0.9.0 and not at all now; `serve.mjs` answers
`/%E0%A4%A` with 400. Regression tests: `request_bodies_are_bounded_as_they_are_read` (phv_server), the `json_decode`
and `parse_query` cases of `json_and_list_builders_stay_within_the_memory_limit` (phv_driver, each held 100 MB or
more without the fix), and `hooks_directories_are_private_and_new` (phv_cli). All tests pass on Windows, and on Linux
and macOS in CI, with clippy (`-D warnings`) and rustfmt. Dependencies: no new crates.

## 20. Follow-up: a Second Outside Review of v0.9.0 (v0.9.1)

Date: 2026-11-05. A second outside review of the source, made with ChatGPT before v0.9.1 was published, reported four
items from reading the code. Each was confirmed with a PoC and fixed; four new reviewers then attacked the fixes in
turn, each round's findings were fixed and attacked again, and the work turned up six more. The PoCs ran the page's
scripts in node with `alert` hooked. All the findings are older than v0.9.0, except those in earlier versions of
the fixes (R13-7), which were never released.

| # | Finding | Fix |
|---|---|---|
| R13-1 (High) | The HTML context check treated all of `<script>` as one place, so `json_html()` was accepted inside a JavaScript string, template literal, comment or regular expression. In `` `<?= json_html($q) ?>` `` a request value `${alert(1)}` ran, as did `"<?= json_html([$q]) ?>"` (its quotes end the string) and `/* <?= json_html($q) ?> */` (`*/`); the template's script carries the CSP nonce, so the policy did not stop it | The checker follows the script's JavaScript (strings, templates and `${}`, comments including `<!--` and `-->`, regular expressions, regex or division from the token before) and accepts `json_html()` only where an expression goes; elsewhere only numbers. Where the text alone does not decide (`}` before `/`, a block that may not run), every reading is followed and all must agree. Inside its strings, `json_html()` now writes every character but letters, digits and non-ASCII non-space characters as `\uXXXX`, so a value cannot end a string, template, comment or regular expression it lands in by mistake; in a `"..."` around it, its contents are names, a syntax error |
| R13-2 (Medium) | After `<?= safe_url($a) ?>` a URL attribute's start was taken as decided, so `href="<?= safe_url($a) ?><?= $b ?>"` with `java` and `script:alert(1)` made a `javascript:` URL | The start stays undecided after an inserted value (`safe_url("")` is empty), so the second value must be a `SafeUrl` too; `safe_url()` returns a relative URL with no `:`, `/`, `?` or `#` as `./x` |
| R13-3 (Low) | `csv_parse()` built up to a million fields before the script's memory limit was checked: 900 KB of line breaks held 188 MB under a 16 MB limit | The limits are checked every 4,096 fields and every 64 KB of text, and while the rows are converted |
| R13-4 (Low) | `jwt_decode()` decoded the header and claims without the limits of `json_decode()` (R12-3): a token signed with the site's key whose claims were a nested list held 81 MB under a 16 MB limit | It uses the limited decoder |
| R13-5 (High) | The body of a function, closure or method is checked as element text wherever it is called, so calling one that writes `<?= $s ?>` inside a `<script>`, an attribute, a URL or CSS put a request string there unchecked (`<script><?phv show($q); ?></script>` ran it). Older than v0.5.0 | A statement in HTML other than element text marks its output; a function it calls that writes to that output, and `dump()`, stop the script with a runtime error. Output into a template being rendered is not affected |
| R13-6 (High) | An exception thrown inside a `<script>`, a tag or an attribute and caught outside it left what the `try` wrote open, and the `catch` block, checked from where the `try` began, wrote inside it: `show("` then the catch's `json_html()` ended the string and ran the value; in a tag, the catch's text became attributes | When such an exception is caught, or the `try` began inside such HTML, what the `try` wrote is dropped, so the `catch` writes where the `try` began. Other exceptions keep the output written before them |
| R13-7 (Medium, in earlier versions of R13-1's fix) | Places where the tracker read a script differently from browsers, each letting a value in a `"..."` mistake run: postfix `++`/`--`, `)` of `if (...)` before a regular expression, `\` with CRLF, numbers inserted inside literals, `<!--` and `-->`, blocks whose `/` reads differently, `}` before `/`, properties named like keywords (`counts.in / n`, `#new`), `for await (`, comments before `(`, `break` then a line break | Each is read as browsers read it; the readings are compared in full when blocks merge; and the escaping of R13-1 makes any remaining difference a syntax error rather than code |
| R13-8 (Low) | A URL attribute's start after a block kept only the block's prefix: in `href="<?phv if ($c) { ?>x<?phv } ?>javascript:void(<?= $q ?>)"` the start looked like `xjavascript:`; many one-character blocks overflowed the set that replaced it | The start is the set of ways the text may have begun, normalised as browsers read schemes; only those that may still become `javascript:`, `vbscript:` or `data:` are kept |
| R13-9 (Low) | `<!-->` and `<!--->` inside a script end its escaped state at once in browsers, but not in the checker: after a later `"<script>"` in the script, the HTML that followed it was checked as script (an unquoted attribute accepted `json_html()`) | They end the escaped state at once, as in browsers |
| R13-10 (Low) | `<meta http-equiv="refresh" name="x" content="<?= $q ?>">` took text because a `name` came before `content`, and browsers still redirect | `content` takes text only without `http-equiv`; an `http-equiv` after inserted text is a compile error |

`1 -<?= json_html(-1) ?>` writes `1 --1`, a syntax error: a request value can make a page's own script fail, but no
code runs. It is kept.

### Residual Risks

- The checker follows a script's JavaScript as browsers read it in ordinary code; for unusual code it may read
  differently (it treats `<script type="module">` and scripts in SVG like other scripts). Because of the escaping of
  `json_html()`, such a difference can make the page's script fail with a syntax error but cannot run a value.
- Output from functions inside HTML other than element text is refused when the page runs, not when it compiles.

### Verification

The 55 PoCs of the four rounds were rerun against the fixed build: each is refused by `phv check`, stops with the
runtime error, or runs without the alert firing in node. A fuzz of `json_html()` with 13 attacking strings, as strings,
lists and maps, in 9 places in a script (code, `"..."`, `'...'`, a template literal, both comments, a regular
expression and its class, between strings) gives 351 cases: none runs the value (with v0.9.0's `json_html()`, 11 do).
Regression tests: `err_context`, `contexts`, `guarded_output` and `err_meta_refresh` in `tests/cases`;
`follows_javascript_literals_and_comments` and `blocks_merge_url_starts_and_script_states` (phv_syntax); the
`csv_parse` and `jwt_decode` cases of the limits test (188 MB and 81 MB without the fixes). All tests pass on Windows,
and on Linux and macOS in CI, with clippy (`-D warnings`) and rustfmt. Dependencies: no new crates.

## 21. Eleventh Assessment: the Whole Implementation (before 1.0)

Date: 2026-11-05. Scope: everything, at v0.9.1, before the work on 1.0 begins. Five new reviewers each took one area
(the web server; templates and XSS; the VM, its limits and the permission sandbox; the standard library's network,
database and cryptography; the tools and distribution) and attacked a release build locally, confirming every finding
with a PoC. A check made with ChatGPT added two items. After the fixes, another new reviewer attacked them, and what it
found was fixed and checked again. None of the findings escapes `[permissions]`.

### Summary of Results

| Severity | Count | Fix |
|---|---:|---|
| High | 4 | Fixed |
| Medium | 12 | Fixed |
| Low | 8 | Fixed |
| Info | 4 | Fixed / documented |

### High

| # | Finding | Fix |
|---|---|---|
| R14-1 | `phv install` wrote `phv.lock` through a temporary file of a predictable name (`.phv.lock.<pid>`): a repository that committed such names as links had the lock written to any file the developer could write | The lock (and every file the tools write, R14-22) goes to a new file with a random name, created only if nothing has that name, renamed into place |
| R14-2 | `phv db migrate --sqlite` ran a repository's migrations without the restrictions scripts have: `ATTACH` created a file anywhere | Migrations open SQLite like scripts: no `ATTACH`, and the same pragmas refused |
| R14-3 | A `<` written just before `<?= ?>` or a block was not seen as the start of a tag: the value became the tag's name and attributes (`<img src=x onerror=...>`), and the tag escaped every check (URLs, handlers, the `<script>` nonce) | What ends a piece of HTML and could start a tag or comment with what follows (`<`, `</`, `<!`, `<!-`, the start of an end tag, and in a script `<!-`, `<scr`, `-`) is read again with the next piece, and a value right there is refused |
| R14-4 | Writing to a shared list or map copied it whole, and appending grew it, without the memory limit being checked until thousands of steps later: 3 GB under a 64 MB limit | What a path assignment copies or grows is reserved first, and a large copy makes the next step check the time |

### Medium

| # | Finding | Fix |
|---|---|---|
| R14-5 | The server handed a page's output to the connection whole, and the connection copied it: the output cap and the 64 KB a slow client may hold did not apply. 600 clients that stopped reading a 4 MB page took 2.3 GB | Bodies are handed over in 64 KB slices that keep the counted output alive until sent; vectored writes reach the socket; compressed output is counted |
| R14-6 | A flood of sessions whose cookie was sent back once evicted every idle visitor's session from a full store (56,000 sessions in 20 s logged a user out) | When full, never-returned sessions go first, then the newest: a flood removes its own sessions, never those created before it (memory, files, PostgreSQL, MySQL and Redis) |
| R14-7 | A page typed `text/javascript` wrote strings unescaped, and `script-src 'self'` let markup injected anywhere load it as a script (under the default policy). The check that followed missed other JavaScript types, a type set after output, and template output | A JavaScript response takes only `json_html()` values and numbers (any JavaScript type; the type before any output); pages run only scripts with their nonce (`'strict-dynamic'`) |
| R14-8 | In SVG and MathML, a `<script>`'s text is markup: a CDATA section or comment hid `</script>` from browsers but not from the check, and a value after it ran in the nonce'd script | A CDATA section or comment in such a `<script>` or `<style>` is an error |
| R14-9 | An exception's trace copied every frame's names (500 MB for one throw from a deep recursion with long names) | At most 50 innermost and 50 outermost calls, names of 200 characters |
| R14-10 | Objects made by builtins (`json_decode_as`, built-in exceptions) were not tracked, so cycles through them outlived the request: a worker grew 481 MB in 100 requests | Every object is tracked; its fields are cleared when the run ends |
| R14-11 | Builtins whose output is larger than their input did not reserve it: `date()` with a long format, `escape()`, `nl2br()`, `json_encode()` of a long string, `json_html()` (whose v0.9.1 escaping writes six bytes for some characters: 97 MB under 16 MB), `log_*()`, `urlencode()`, `strtoupper()`, `slice()` | Each reserves its largest output first |
| R14-12 | `==`, `contains()`, `indexOf()`, `unique()`, `sorted()` and map sorts compared long strings without checking the time limit, and callbacks of builtins (`sortBy`, `map`) never reached a check: minutes under a 1-second limit | Comparisons count the bytes they read and check the time every 16 MB; equal strings are compared by identity first; every callback counts as a call |
| R14-13 | SQLite results were checked against the memory limit every 256 rows, of up to 32 MB each: 3.8 GB under a 16 MB limit | Each value is reserved before it is copied, and memory is checked every row |
| R14-14 | SQLite runs one `LIKE`, `GLOB`, `instr()`, `replace()` or `trim(X, Y)` as a single step, which the time limit could not stop (42 s, and hours for the longest values); `PRAGMA case_sensitive_like` restored the built-in `LIKE` | `LIKE` and `GLOB` are replaced by matchers that stop at the time limit (3,021 cases compared equal to SQLite's), the others by linear searches; the pragma is refused |
| R14-15 | The language server hashed `phv.toml` by reading it whole on every keystroke (a 1.5 GB file in a cloned repository: 1.5 GB each time) | It is read like every source file, 8 MB at most |
| R14-16 | Results of background work (`getAsync()` and friends) were allocated on the work's thread and so not counted for the script, and each awaited result was kept until the run ended: 80 responses of 512 KB were all held under a 16 MB limit | A result is counted on the script's thread when it arrives, and only the `Future` keeps it |

### Low

| # | Finding | Fix |
|---|---|---|
| R14-17 | Gzipped static files were cached by ETag (size and time) alone: two files of one size and time were served each other's bytes | The cache is keyed by the file as well |
| R14-18 | `<noscript>` was treated as raw text, so nothing in it was checked (a `<meta http-equiv="refresh">` with a value redirected browsers without scripting) | Its content is checked as markup |
| R14-19 | A date near the end of the range in a zone with a gap panicked (`std.time`): a 500 for a visitor's date | The arithmetic is checked |
| R14-20 | Scripts could read and write PHV's own keys in Redis (`phv:session:...`): a page that takes a key from the request could forge a session | Keys starting with `phv:` are refused |
| R14-21 | A `transaction()` inside another sent a second `BEGIN`: PostgreSQL committed the outer work with the inner `COMMIT`, MySQL committed it at the inner `BEGIN` | Inner levels are savepoints, counted per connection |
| R14-22 | `phv test --junit` and `phv build -o` wrote through a link at the output's name or on its directories (also with an absolute path written in other case on Windows) | New files renamed into place; links on the way refused, names compared regardless of case on Windows |
| R14-23 | A page using `layout()` could end, or call `layout()`, inside a tag or attribute, so the layout's HTML landed there | Both are compile errors |
| R14-24 | (In the first version of R14-3's fix) `<!` and `<!-` before a value, and `<!-`/`-` split around a value in a script, were not carried | They are |

### Info

| Item | Result |
|---|---|
| The HTTP parser accepts HTTP/1.1 without `Host`, bare LF line ends and custom methods | Kept: browsers cannot send these across sites; behind a proxy that parses differently they could matter |
| `requests_per_minute` counts the reverse proxy's address behind one | Documented as a residual risk |
| `LIKE`'s trailing escape, `GLOB`'s `[]-a]`, `ESCAPE NULL` and reals as text differed from SQLite in the first version of R14-14's fix | Fixed; 3,021 generated cases now give SQLite's answers |
| `if (x <<?= $n ?>)` in a script is refused (R14-3) | Write a space: `x < <?= $n ?>` |

### Residual Risks

- Behind a reverse proxy, `requests_per_minute` and the session store see the proxy's address (there is no trusted-proxy
  setting); limit requests per client at the proxy.
- When the session store fills with real sessions, the newest are removed first.
- The changes to PostgreSQL, MySQL and Redis (sessions, savepoints, `phv:` keys) were checked by reading and by the CI
  jobs that run those servers; no server ran here.
- A JavaScript response accepts any `Html` made only of the characters `json_html()` writes; such text cannot call
  anything.

### Verification

Every PoC was rerun against the fixed build: the lock, junit and build links leave the files outside untouched; the
migration's `ATTACH` is "not authorized"; a 300 MB `phv.toml` takes the language server 7 MB (309 MB); 600 stalled
clients of a 4 MB page 95 MB, with 503 past the cap (2.3 GB); the session flood leaves the victim logged in; the
gzip cache serves each file its own bytes; every template PoC is refused (and none of the 55 earlier script PoCs runs);
JavaScript responses with a string, another JavaScript type, a late type or a template's output answer 500, while
`json_html()` values work; copy on write stops at 64 MB (3 GB), a trace is 22 KB (1.9 GB), the cycles of 100 requests
4 MB (481 MB), string builtins stop at the limit, `==`, `contains()`, `sorted()` and `sortBy()` at the time limit
(4–60 s and more); SQLite's 200 rows of 20 MB stop at the limit (3.8 GB), `LIKE`, `instr()` and `replace()` at the time
limit or in 0.08 s (42–58 s), also after the pragma; dates near the end of the range give no panic; async results kept
past the budget are refused. Regression tests: `markup_split_by_a_value_is_refused` (phv_syntax), `javascript_responses_take_only_inert_values`
and `bodies_are_sent_in_counted_slices` (phv_server), `long_comparisons_and_callbacks_stop_at_the_time_limit` and the
`csv_parse`/`jwt_decode` cases (phv_driver), `sqlite_nested` in `tests/cases`, and the database tests (nested transactions,
`phv:` keys) that CI runs. All tests pass on Windows, and on Linux and macOS in CI, with clippy (`-D warnings`) and
rustfmt. Dependencies: no new crates (rusqlite's `functions` feature).

## 22. Twelfth Assessment: the Whole Implementation (before 1.0.0)

Date: 2026-11-05. Scope: everything at a17d29e, with the 1.0 additions (the cycle collector, `set<T>`, HTTPS and HTTP/2,
the WebSocket server, the JIT for lists, maps and nullable values, `[project] phv`), and every earlier fix, which was
attacked again. Six reviewers each took one area (the web server; templates and XSS; the VM, the JIT and the type
checker; the standard library and the databases; the tools, the editor and CI; and the tools the fifth did not reach:
the debugger, `phv migrate`, `phv db migrate`, the website and the demo) and attacked a release build locally,
confirming every finding with a PoC. Every finding was fixed and its PoC run again against the fixed build.

### Summary of Results

| Severity | Count | Fix |
|---|---:|---|
| Critical | 1 | Fixed |
| High | 3 | Fixed |
| Medium | 15 | Fixed |
| Low | 14 | Fixed |
| Info | 4 | Fixed |

### Critical

| # | Finding | Fix |
|---|---|---|
| R15-1 | A function the JIT compiled that had only numbers of its own, calling one with a `?int`, `?float` or `?bool` parameter or result (passed in the register window), had no window: the call wrote into its caller's. Six ordinary lines aborted the process (`phv serve` included: one request took the server down), or emptied a caller's string | A function that passes or receives a value in the window has one; regression test `calls_with_nullable_arguments_keep_their_windows` |

### High

| # | Finding | Fix |
|---|---|---|
| R15-2 | `phv install` ran git in the project's directory: a project laid out as a bare repository (`HEAD`, `config`, `objects/`, `refs/`) had git read its `config`, which can turn on a transport that runs commands (`ext::`) and rewrite `https://` to it | Git runs in a new empty directory of its own, never looks above it (`GIT_CEILING_DIRECTORIES`) and gets absolute paths; test `git_ignores_the_repository_it_runs_in` |
| R15-3 | HTTP/2 responses a client never took (no flow-control window, or a client that did not read) were held per stream, 64 streams of 64 KB per connection, and the socket never stalled, so nothing closed them: 200 connections held 2.5 GB, and the counted page output made every other client get 503 | 8 streams and 16 KB per stream (16 KB file buffers): a connection holds about what an HTTP/1.1 one does. A connection whose responses take no bytes for 30 seconds is dropped, and the wait after closing one is at most 10 seconds |
| R15-4 | The record of objects was searched again at every `new` when the number of live objects was just below its capacity (a shrink that never grew it): 29 seconds for 20,000 objects, past the time limit | The record always has room for as many objects again as are alive |

### Medium

| # | Finding | Fix |
|---|---|---|
| R15-5 | A TLS client that chose HTTP/2 and sent nothing was never closed; 1,024 of them shut everyone out | The wait after an idle connection is told to close is bounded: closed after 40 seconds |
| R15-6 | HTTP/2 request bodies had the 64 KB free of the body budget per stream, 64 per connection: 750 MB held | Over HTTP/2 a body counts from its first byte |
| R15-7 | Regular expressions: the time limit was checked every 1,024 matches, and each search for the next match may read to the end of the text: 97 seconds under a 2-second limit | The limits are checked after every match |
| R15-8 | Each in-memory SQLite database had the script's whole memory limit, in SQLite's memory (not the VM's): 40 of them took 567 MB for a 16 MB request and made other requests' SQLite fail | A script's in-memory databases share its memory limit |
| R15-9 | Redis array replies were counted at 3 bytes per element and became about 64 in memory: 891 MB under a 128 MB limit | 64 bytes per element are charged to the reply's budget before the list is built |
| R15-10 | `rawurlencode()`, `http_build_query()` and `bin2hex()` did not account their output (missed in R14-11): 777 MB under 128 MB | The output is reserved before it is built |
| R15-11 | WebSocket kept values counted only the bytes of keys and values: 15,000 empty values per connection, 2.2 MB each | At most 256 values, each counting 64 bytes more |
| R15-12 | Hashing a long map key was not timed: 1,000 lookups of a 32 MB key in one loop step ran past the time limit (interpreter and JIT) | A key over 64 KB checks the limits when it is looked up or stored |
| R15-13 | `.=` on a string also held elsewhere copied it untimed and unaccounted | The copy is reserved and counted as a long operation |
| R15-14 | `json_encode()` of a set did not reserve its strings' escaped output: 832 MB under 128 MB | Set elements are accounted like strings |
| R15-15 | In the JIT a builtin's argument (`count($xs)`, `strlen($s)`) stayed in the window, so the next change of the value copied it whole: a loop of `$xs[] = 1` took 77 seconds instead of 0.01 | The slot is emptied after the call, as the interpreter moves the argument out |
| R15-16 | `[server] access_log` was opened through links and junctions: a repository could make `phv serve` append to a file outside it | The log is opened inside the project with `safefs`, links refused |
| R15-17 | `[server] tls_cert` / `tls_key` took network paths (`\\host\share\...`): `phv serve` would contact that host with the user's credentials | Network and device paths are refused |
| R15-18 | The `<meta http-equiv>` check could be passed through a block: a block that closed the tag and opened `<meta name>` left the following `content` read as descriptive text (an open redirect) | After a block the tag's attributes of both ways are kept: the `content` is checked if either way makes it act |
| R15-19 | `phv db migrate --sqlite` followed links in the repository: a migration created or overwrote files outside the project (a `.bat` file whose text ran) | Migrations open SQLite as scripts do (`safefs`, no link or hard link on the way or for the journals, `NOFOLLOW`, PHV's VFS) |

### Low

| # | Finding | Fix |
|---|---|---|
| R15-20 | `[cors] origins = ["*"]` (and `Origin: null`) let any site open a WebSocket with the visitor's cookies | Another site needs to be named in `[cors]` with `credentials = true` |
| R15-21 | A request whose `host` differed from its `:authority`, or with two `Host` fields, reached scripts with another host than a proxy would read | `:authority` replaces `host`; two `Host` fields get 400 |
| R15-22 | WebSocket messages were rate limited under another key than requests (each IPv6 address of a /64, never the shared store) | Messages use the requests' key and store |
| R15-23 | `json_decode_as()` built its second tree without checking the limits: 1.7 times the memory limit | It reserves and checks the limits as it builds |
| R15-24 | `sql_ident()` let a name from a request reach PHV's own `phv_sessions` table: a visitor could read and forge sessions | Names starting with `phv_` are refused |
| R15-25 | Windows short names (`GIT~1`) passed the check that `[permissions].write` holds no code (`.git`, `.phv-cache`) | Names with `~` are refused in permission and session directories |
| R15-26 | The arguments a JIT call kept for a rerun made a function that changed its list parameter copy it whole (128 MB were not enough where the interpreter needed 40) | Such functions keep no copy; out of native stack they stop with "maximum call stack depth exceeded" |
| R15-27 | Dropping a large list moved every element onto a worklist first: twice its memory, outside the limit | Only values that hold others go on the worklist |
| R15-28 | `phv install` read `phv.lock` whole: a 293 MB lock took 330 MB | It is read with the 8 MB limit of sources |
| R15-29 | Migrations had weaker SQLite settings than scripts (no defensive mode, no length limits) | One function sets up every SQLite connection |
| R15-30 | A DAP `scopes` request with a huge frame id overflowed an addition and ended the script; later requests were not answered | Frame ids are checked; requests the script can no longer answer get an error |
| R15-31 | `phv migrate` lowered the rest of the file at each `new`: quadratic, 17 seconds for 20,000 | Only the next word is compared |
| R15-32 | Diagnostics printed source lines as they were: escape sequences in a file reached the terminal (clipboard, screen) | Control characters are shown as U+FFFD |
| R15-33 | The website build passed HTML written in the docs into the pages (a redirect, an overlay) | HTML in the docs is shown as text |

### Info

| # | Finding | Fix |
|---|---|---|
| R15-34 | Static files were served through hard links: a hard link in `public/` to the TLS key served it | Files with more than one name are not served |
| R15-35 | SQLite pragmas that can corrupt a database (`schema_version`, `cell_size_check`, `trusted_schema`) were allowed | Refused, and SQLite's defensive mode is on |
| R15-36 | `--sqlite \foo.db` (or `C:foo.db`) was not seen as outside the project | Only names inside the project are accepted |
| R15-37 | `srcset` and animation `values` were checked as a URL only at their start | Each URL of the list is checked |

### Residual Risks

- SQLite's memory has one process-wide cap (512 MB). A script's in-memory databases are bounded by its memory limit,
  but many requests at once can still reach the cap, and then SQLite fails for the others until they end.
- `tls_cert` and `tls_key` may be absolute paths on this machine (certificates usually live outside the project); a
  file is only parsed as PEM, and nothing of it is shown.
- `publish()` reaches the WebSocket connections of one server process.
- The demo's base image is not pinned by digest, and its archive is checked against the `SHA256SUMS` of the same
  release (it detects corruption, not a replaced release).
- The cycle collector runs on the number of objects, not when memory runs short.

### Verification

Every PoC was rerun against the fixed build: the `?int` call prints 42 under the JIT (it aborted); the bare repository's
command does not run; 200 stalled HTTP/2 connections take 105 MB and are dropped after 30 seconds while another client
gets 200 (2.5 GB and 503); a silent HTTP/2 connection closes at 40 seconds (never); bodies over 200 connections 127 MB
(750 MB); the regular expressions stop at 2.1 and 2.4 seconds (97 and 5); the 20,000 objects take 0.005 seconds (29);
the long key, the shared `.=` and the JIT loops stop at the time limit or run in 0.01 seconds; the JSON set stops at
61 MB (832); the dropped list peaks at 62 MB (123); the lock 6 MB (330); the access log and the migration through links
are refused and the files outside are untouched; the network TLS path is refused; the `<meta>` page and the forged
session are refused; WebSockets from `*` and `null` get 403; the hard-linked key gets 404; the DAP request gets an error
and the session continues; `phv migrate` takes 0.43 seconds (17.4) and prints no escape character. Regression tests:
`blocks_keep_the_attributes_of_a_meta`, `every_url_of_a_list_is_checked`, `control_characters_are_not_printed`
(phv_syntax); `calls_with_nullable_arguments_keep_their_windows`, `a_list_parameter_changed_in_place_is_not_copied`,
`encoders_and_sets_stay_within_the_memory_limit`, `copies_and_long_keys_stop_at_the_time_limit` and
`paths_that_leave_the_machine_or_hide_a_name_are_refused` (phv_driver); `cors_lets_other_sites_open_websockets_only_by_name_with_credentials`
and `two_host_fields_are_refused` (phv_server); `git_ignores_the_repository_it_runs_in` and the DAP session test (phv_cli);
`sql_ident_reserved` in `tests/cases`; and `every_builtin_has_one_implementation` (phv_std), which found two builtins
of one name while the WebSocket server was written. All tests pass on Windows, and on Linux and macOS in CI, with clippy
(`-D warnings`) and rustfmt. `npm audit` finds nothing in the extension and the website, and `cargo audit` runs in CI.

## 23. Thirteenth Assessment: the Whole Implementation (before 1.0.0, after the processing work)

Date: 2026-11-06. Scope: everything at 852f79d, which adds to §22 the work that made processing faster (0218333):
the page, path and static-file caches of a production server, the request log thread, SQLite connections kept
between scripts, the HTTP client's keep-alive connections, the DNS cache, the worker pool of background operations,
PostgreSQL's prepared statements and shared runtime, the process-wide regular expression cache, the parse cache, the
language server's analysis cache, in-place `.=` on properties and elements, fused `==` branches, loops tested at their
end, and the other changes of the VM, the JIT and the type checker. Every earlier fix was attacked again. Six reviewers
each took one area (the web server; the standard library and its clients; the VM, the JIT and the type checker;
templates and escaping; the tools, the editor and CI; and the debugger, migrations, WebSocket scripts, the website,
the demo and the examples), attacked a release build locally and confirmed each finding with a PoC. Every finding was
fixed, each reviewer ran its PoCs again against the fixed build and looked for ways around each fix, and what they
found was fixed in turn (three rounds).

### Summary of Results

| Severity | Count | Fix |
|---|---:|---|
| Critical | 0 | — |
| High | 3 | Fixed |
| Medium | 12 | Fixed |
| Low | 13 | Fixed |
| Info | 4 | Fixed |

### High

| # | Finding | Fix |
|---|---|---|
| R16-1 | `==` on lists and maps was neither timed nor counted against the memory limit (only strings were): two structures that share their parts differently took 3.3 GB and 32 seconds in one `==` under 128 MB and 1 second; `assert_eq()` compared without any limit | Each pair of values compared and each pair queued are counted; a large step is reserved first, and the time and memory limits are checked as the comparison goes. `assert_eq()` compares as `==` does |
| R16-2 | A static file asked for with `Range` (and gzip accepted) was read whole for each response, which held it until sent: 1,000 stalled HTTP/2 connections took 7.3 GB, and pages got 503 | A range is always streamed from the file; a file is read at once only when one copy serves many responses (a small file the server keeps, or text to compress whose copy can be kept) |
| R16-3 | A kept HTTP client connection was used again although bytes had arrived on it while it was idle: a stray answer of the server was read by the next request, another visitor's, as its own | A kept connection is used again only if nothing arrived on it (nothing TLS has decrypted, nothing on the socket, no close), checked when it is kept and when it is taken |

### Medium

| # | Finding | Fix |
|---|---|---|
| R16-4 | The production path cache kept paths of up to 64 KB: 10,000 of them took 680 MB | Paths longer than 1 KB are not kept |
| R16-5 | The request log's queue held whole paths and methods: with a slow stderr, 2 GB | Paths are cut to 2 KB and methods to 32 bytes in log lines; the queue holds 4,096 lines |
| R16-6 | Static files were compressed again by each request that missed the gzip cache, each holding its own copy (uncounted): 1.7 GB on a cold cache; with the cache full, the same for files left out | One request compresses a version of a file and the others share its copy; a file whose compressed copy cannot be kept is sent as it is; the cache is no longer emptied when full |
| R16-7 | `encrypt()`, `decrypt()`, `sign_token()` and `verify_token()` made several copies before the memory limit was checked: 705 MB under 128 MB (missed by R14-11 and R15-10) | The copies are reserved before they are made |
| R16-8 | In a `<script>` or `<style>` inside `<svg>` or `<math>`, a tag or bogus comment holding `</script>` hid the end tag from the template check: a request value ran in the script that carries the CSP nonce | Any markup there is a compile error (as CDATA sections and comments were) |
| R16-9 | The template check could be put out of step with browsers: `<noscript>` (raw text when scripting is on), a self-closed `<svg/>`, `</math>` inside `<svg>`, and `<foreignObject>`; an end tag in a comment then ended the element for the browser while the check read on (a `<meta>` redirect, a `javascript:` link) | `<svg>` and `<math>` are followed as browsers do (`<svg/>` opens nothing, an end tag closes only an open element of its name); inside `<noscript>`, `<svg>` and `<math>`, the end tag of a raw-text element in a comment or attribute value (and `</noscript` anywhere but in text) is an error, also when it is split across pieces of literal text, and a value that could complete one is refused, as is a block (`if`/`else`, a loop, `try`) that begins or ends inside one |
| R16-10 | The process-wide parse cache was bounded by source text, not by the syntax trees, which are many times larger: the language server kept 6.7 GB after documents were closed | Each module's memory is measured as it is parsed; at most 64 MB is kept, sources over 1 MB are not kept, and an empty statement makes no statement |
| R16-11 | Documents open in the editor skipped the 8 MB limit of sources: a 40 MB document took tens of GB | Text from the editor has the limit of files read from disk: a larger document is kept without its text, and while one is open nothing edits from it (formatting, code actions) and renames are refused |
| R16-12 | `phv install` let a package's `.lfsconfig` make git-lfs (on by default in Git for Windows) contact any host over any transport, around R5-10 | git-lfs's filters are turned off, and a clone's own attributes keep every filter from a package's files |
| R16-13 | Building a range (`a..b`, and `range()` in the JIT) was not timed: 20 seconds under 1 | The time limit is checked soon after |
| R16-14 | `echo` in templates was not timed: a template's output is dropped after `render()`, so its buffer did not bound the escaping: 141 seconds under 1 | Each echo counts as long work by its size |
| R16-15 | Map literals hashed long keys without a time check, around R15-12 (interpreter and JIT): 14.9 seconds under 1 | Each long key checks the limits, and the JIT's literal stops at once |

### Low

| # | Finding | Fix |
|---|---|---|
| R16-16 | The path cache trusted a decision for a second: a junction put on the way in that time served a file outside `public/` | The file opened must be the one at the path found (its handle's final path) |
| R16-17 | Two ways around R15-21: an absolute request target in HTTP/1.1, and `:authority` with user information, let scripts see another host | The authority of the request target replaces `host`; one with user information gets 400 |
| R16-18 | A built site compressed a file again for each spelling of its path | Compressed copies are kept by the file's name |
| R16-19 | A kept SQLite connection's `execute()` returned the previous script's count for a statement that changes no row | 0 when no row changed |
| R16-20 | The HTTP client sent a GET again after an answer had come (refused as too large) | Sent again only when no byte of an answer came |
| R16-21 | `safe_url()` after a literal `&` at the start of a URL made a character reference (`&` + `#x6a;avascript:`) | Only numbers may follow a `&` there |
| R16-22 | Spaces after an attribute name were scanned ahead at each one: quadratic (an 8 MB file would take hours) | Passed in one step |
| R16-23 | Escape sequences from `phv.toml`, package manifests and file names reached the terminal (R15-32 covered diagnostics only) | Every error line of the CLI shows control characters as U+FFFD |
| R16-24 | Output paths with `..`, or absolute ones, skipped the link check (around R14-22) | Resolved by name first, then checked from where they leave the current directory's ancestors; on Windows the deepest existing directory must also be the very folder its spelling names (`\\?\`, `\\.\`, a share of this machine, `C:..\`) |
| R16-25 | `phv fmt` read files of any size: 64 MB took 4.7 GB | Sources are read with the 8 MB limit |
| R16-26 | A chain of exceptions in a deep recursion went past the memory limit (each keeps its trace's text) | No trace text is made past the limit |
| R16-27 | `examples/site` rate-limited its guestbook by the full address (missed by R4-11) | `ip_group()` |
| R16-28 | WARDOGS (an example) answered 500 to huge or negative scores from the game server | Numbers are bounded, and its bars clamped |

### Info

| # | Finding | Fix |
|---|---|---|
| R16-29 | A late MySQL `KILL QUERY` could reach the next script on the connection | A connection whose kill was sent is not used again |
| R16-30 | WARDOGS printed escape sequences from the game server (names, error messages, C1 controls) | Control characters are dropped |
| R16-31 | The website kept `javascript:` link targets in the docs (also written `java&#9;script:` or with a control before it) | Only web, mail, relative and anchor links are links |
| R16-32 | A SQLite migration could end its transaction (`COMMIT;`), leaving part of a failing migration (and the demo's comment said nothing is shared) | `BEGIN`, `COMMIT` and `ROLLBACK` are refused in migrations; the comment is updated |

### Residual Risks

- An HTTP server that sends an answer nobody asked for, a few milliseconds after the real one, can have it read by the
  next request on that kept connection when that request is already on its way: HTTP/1.1 does not tie answers to
  requests. Checks when a connection is kept and taken leave only this window.
- An upstream that acts on a `GET` and closes without answering gets the `GET` again (the usual retry on a closed kept
  connection, for `GET`, `HEAD` and `OPTIONS` only).
- An idle kept SQLite connection holds its file open for up to 30 seconds: a program outside PHV cannot rename or delete
  it meanwhile on Windows.
- `==` on structures that share parts differently may go to about twice the memory limit before it stops (the table of
  compared pairs grows in one step).
- Parsing takes memory many times a source's size (a file of 8 MB of list items takes about 1.5 GB while it is checked).
- After a call, a function's registers keep its arguments: changing a list right after passing it copies it (timed and
  counted). The JIT's stack budget is measured from the outermost native call.
- In a watched state of a template (inside `<noscript>`, `<svg>` or `<math>`), a function called between `</` and the
  rest of an end tag passes `phv check`; the page stops at run time before anything reaches the browser.
- On Unix, `opened_at` compares the file opened with the path in two steps (Windows uses the handle).
- §22's residual risks remain (SQLite's process-wide heap cap, local TLS paths, `publish()` within one process, the demo's
  base image not pinned by digest, the cycle collector run by count).

### Verification

Every PoC was rerun against the fixed build: the list `==` loop stops at 1.03 seconds (16.5), the shared-structure
`==` at 1.2 seconds and 168 MB (32 seconds, 3.3 GB), `assert_eq()` on it at 1.6 seconds (46.9); stalled range requests
take 147 MB over HTTP/1.1 and 535 MB over HTTP/2 with pages answered (940 MB and 7.3 GB); a stray answer on a kept
connection is no longer read by the next visitor; 9,900 long paths take 7.4 MB (680 MB); the log queue 7–28 MB (2 GB,
517 MB with a long method); a cold gzip cache 64 MB (1.7 GB); the crypto builtins stop at the limit (705 MB); every
SVG, `<noscript>` and split-piece template is refused, and a differential fuzz of 20,000 random pages against the
previous release found only the intended differences; the language server keeps 1 MB after closing documents (6.7 GB), and with a 40 MB document open answers
`workspace/symbol` and completion at once with 154 MB at most (12.4 seconds and 23.2 GB), formats nothing in it and
refuses renames; output paths through a junction are refused in all fifteen spellings tried, while plain ones are
written;
`phv install` makes no connection for a package's `.lfsconfig`; ranges, template echoes and map literals stop at about
1 second (20, 141 and 14.9); the junction swap gets 404; `execute()` on a kept connection gives 0; the migration with
`COMMIT` leaves nothing; and the WARDOGS and website cases are handled. Regression tests:
`markup_in_foreign_raw_text_is_refused`, `foreign_content_follows_the_open_elements`,
`end_tags_hidden_in_comments_and_attributes_are_refused`, `spaces_after_an_attribute_name_are_read_in_linear_time`
(phv_syntax); `list_equal`, `range`, `map_literal`, `encrypt`, `sign_token`, `decrypt` and `verify_token` in the limits
tests, `kept_connections_carry_no_late_answer_and_answered_requests_are_not_repeated` and the kept-connection and
`execute()` checks of `sqlite_pool` (phv_driver); `hosts_long_paths_and_ranges_in_production` (phv_server);
`packages_cannot_select_filters`, the output link checks and `documents_over_the_limit_are_not_edited` (phv_cli); the migration test (phv_std); and
`tests/cases/err_context` for `&` before a URL value. All tests pass on Windows, and on Linux and macOS in CI, with
clippy (`-D warnings`) and rustfmt. `npm audit` finds nothing in the extension and the website, and `cargo audit` runs
in CI.

## 24. Fourteenth Assessment: the Whole Implementation (before 1.1.0)

Date: 2026-11-06. Scope: everything at 6f7acb3, which adds to 1.0.0 what lets a site run on several servers: a secret
shared by the server processes (`[security] secret_env`), an admin port with health checks and Prometheus metrics,
request ids and trace context, `publish()` across processes over Redis, background jobs and schedules (`std.jobs`,
`[jobs]`, `[[schedule]]`, `phv jobs`), Server-Sent Events, and faster number formatting, exception traces and object
creation. Every earlier fix was attacked again. Six reviewers each took one area: the web server; the standard library
and its clients; the VM, the JIT and the type checker; templates, the website and the examples; the tools, the editor
and CI; and background jobs. They attacked a release build locally and confirmed each finding with a PoC. Every finding
was fixed, or kept as a residual risk with its reason; each reviewer then ran its PoCs again against the fixed build
and looked for ways around each fix, and what they found was fixed in turn (four rounds for event streams, two for the
rest).

The differential check of the template tracker against a browser's parser stopped three times when reviewers ran it;
it was done in the main session instead, with harmless marker values in place of the values: 854 templates compared
with html5lib (see Verification).

### Summary of Results

| Severity | Count | Fix |
|---|---:|---|
| Critical | 0 | — |
| High | 4 | Fixed |
| Medium | 11 | Fixed |
| Low | 16 | 13 fixed, 3 kept as residual risks |
| Info | 9 | Fixed or documented |

### High

| # | Finding | Fix |
|---|---|---|
| R17-1 | Server-Sent Events were not counted in the server's output: clients that do not read held about 17 messages each, and line breaks made a message seven times larger as an event. 30 such clients held 3.9 GB while pages still answered | Events count until they are sent, in a budget of their own; a stream holds at most 2 MB, and an event larger than that is not sent |
| R17-2 | Literal HTML written in a loop was never counted: 4.1 GB under 128 MB, and the script succeeded | A large piece is reserved before the buffer grows, and the limits are checked every 64 KB written, however small the pieces |
| R17-3 | `min()` and `max()` of a list compared long strings with no time check: 23 seconds under 1 | The comparisons are metered |
| R17-4 | Two sites on one job store took and ran each other's jobs, with the other's payloads | Each site has a namespace (`[security] store_namespace`, or `[project] name`): a table and a Redis prefix of its own |

### Medium

| # | Finding | Fix |
|---|---|---|
| R17-5 | With R17-1 fixed, clients that do not read filled the shared cap, and the stream that was ended was the next to send: a reader's | Only a stream that still holds output is ended; one that sent everything goes on |
| R17-6 | Counted with pages, streams that do not read pushed the output past the cap that pages check: every page answered 503 | Event streams have a budget of their own; while it is full, a stream holding nothing skips the event |
| R17-7 | A full-width or no-break space between attributes put the template check out of step with browsers: `<a class=x　title="<?= $t ?>">` let a value add attributes | Only HTML whitespace separates names and values, and tag names run on as browsers read them |
| R17-8 | `contains()` and `indexOf()` checked the time every 1,024 elements: 18.8 seconds under 1 with maps (the existing test passed on fast machines only by finishing in time) | `==` keeps its meter across calls, and short string comparisons count too |
| R17-9 | `groupBy()` hashed long keys without counting them: 15.9 seconds under 1 | The bytes hashed count |
| R17-10 | With ten or more subscribers, a burst of cross-process `publish()` was dropped almost whole (a fixed budget for the pipelined replies) | Each reply's budget covers the replies still to come |
| R17-11 | After one failed publish, `/ready` stayed 503 until the next message was sent | The store is tried again, with a PUBLISH, when the wait ends |
| R17-12 | Sites sharing a Redis or SQL database shared sessions and rate limits: one site's cookie signed a visitor in on another | Sessions and rate limits are kept under the site's namespace |
| R17-13 | Each `[[schedule]]` entry read the jobs folder before the count of 100 was checked: `phv check` 159 seconds, 15 seconds per edit in the editor | The count is checked first; the folder is read once, 10,000 entries at most |
| R17-14 | The job cap of 100,000 was not checked inside `transaction()`, and a count a second old let each process pass it | Counted in the transaction; exact near the cap |
| R17-15 | The memory job store's bound left out jobs given up (658 MB) and keys of ended jobs, and swept everything at each `enqueue()` | Bounded (16 MB, 100,000 keys); swept at most once a second |

### Low

| # | Finding | Fix |
|---|---|---|
| R17-16 | Signed bus messages could be published again (also after a reconnect or a restart) and reordered | Messages carry a time and a sequence number given as they are sent: older than 60 seconds, seen before, or sent before the process subscribed are refused |
| R17-17 | `unique()` compared each value it cannot hash (null) with every int and string kept | Only those values are compared, and each comparison counts |
| R17-18 | `E::cases()` and `E::tryFrom()` of a huge enum were untimed steps (75.9 and 2.1 seconds under 1) | Timed by the cases they look at |
| R17-19 | Exception messages, often holding request data, reached the server's log as they were: forged lines, terminal escape sequences | Escaped as `log_*` lines are, for pages and jobs |
| R17-20 | Through a pattern of `[permissions] env`, and on Windows through any case of the name, scripts read the server's secret and the admin token | The server's own variables are hidden from scripts, compared regardless of case |
| R17-21 | `phv jobs run` and `phv jobs list` printed job errors, names and ids from the store as they were | Escaped |
| R17-22 | `phv migrate -o` wrote through a linked directory | Checked as `phv build -o` is |
| R17-23 | `phv jobs run` and `--jobs-only` ran nothing with `[jobs] run = false` | `run = false` stops only a serving process |
| R17-24 | `enqueue()` held a page 10 seconds for a store that did not answer, whatever its time limit | No longer than the script may still run |
| R17-25 | Each `enqueue()` woke every runner, and each asked the store (6.4 seconds of CPU per 1,000 jobs) | One job wakes one runner |
| R17-26 | Finished jobs without a key were kept a day in the SQL stores; a key a page gave could take a scheduled run's minute; near a full cap every `enqueue()` counted the table | Only keyed jobs are kept; schedules have keys of their own; a full count is kept a second |
| R17-27 | A built site checked its schedules and job names against the folder it ran in | Its bundle's jobs are used |
| R17-28 | Enough streams that do not read to fill the events' budget at one event each (about the budget ÷ 2 MB: 1,000 for 2 GB, 32 for 64 MB) make readers miss events until they are dropped | Kept as a residual risk: a stream that was ended keeps its unsent event until its connection is dropped (30 seconds without progress) |
| R17-29 | Concurrent `transaction()`s can each pass the job cap by what they add (8 at 95,000 reached 111,000) | Kept as a residual risk: jobs of uncommitted transactions are not counted |
| R17-30 | Sites sharing a PostgreSQL or MySQL session table share its limits and the shortest `session_ttl` | Kept as a residual risk (documented: a database per site) |
| R17-31 | The subscriber connected again every second to a Redis server that sent garbage | The wait starts over only after a connection lasted 30 seconds |

### Info

| # | Finding | Fix |
|---|---|---|
| R17-32 | `[jobs] dir`, or a directory on its way, could be a link; `Public` was accepted on Windows | Refused |
| R17-33 | A row written by someone else with a huge attempt count stopped the whole queue | The count is capped |
| R17-34 | `\r` in a metric label was not escaped (labels are file names) | Escaped |
| R17-35 | The website's build read registry files of any size, and the site sent no `Cross-Origin-Opener-Policy` | 4 KB at most; the header is sent |
| R17-36 | Copies of one project (staging, production) have the same name, so the same namespace | The server says so when it starts; documented |
| R17-37 | `/ready` reported a store that takes connections but refuses PUBLISH as ready | The probe publishes |
| R17-38 | Schedules across a change of the clock in their time zone; `BEGIN` sent as SQL with MySQL | Documented |
| R17-39 | A message of very many lines becomes an event seven times its size | Events larger than 2 MB are not sent |
| R17-40 | The demo's board refuses only lowercase link text; WARDOGS asks the game server twice per page view | Noted (examples) |

### Residual Risks

- Enough clients that do not read to fill the events' budget at one event each make readers miss events until the
  server drops them (30 seconds without progress); they must then be opened again, each with a worker for its Open
  event and within the rate limit.
- Concurrent transactions can each add jobs past the cap of 100,000 by what they add before they commit.
- Sites that share a PostgreSQL or MySQL session database share its limits and its shortest `session_ttl`; sites with
  the same name, or no name, share sessions, rate limits and jobs in shared stores. Give each site a database, or a
  `store_namespace`, of its own.
- A captured bus message can be published again within its 60 seconds to a process that was subscribed before it was
  sent and has not received it (only by someone who can write to the Redis server). Processes whose clocks differ by
  more than 60 seconds drop each other's messages; Redis drops a subscriber that falls too far behind (delivery is at
  most once).
- The server's output can reach twice `workers` × `memory_limit_mb`: pages and event streams each have that budget.
- Upgrading a site that keeps sessions in a database and has a name signs its visitors out once, and jobs it left in
  the shared table are not taken.
- §23's residual risks remain.

### Verification

Every PoC was rerun against the fixed build:
- event streams that do not read: 975 MB with 30 of them on a 32-CPU host (3.9 GB); with a 64 MB budget, pages answer
  200 throughout, the pages' output stays 0, and a reader gets every event with 10 or 60 such streams (3 of 8 with 80:
  R17-28);
- the literal HTML loop stops at the limit at 87 MB (4.1 GB);
- `min()` and `max()` stop at 1.1 seconds (23); `contains()` and `indexOf()` at 1.1–1.2 (up to 18.8); `groupBy()` at
  1.1 (15.9); `E::cases()` and `E::tryFrom()` at 1.1–1.3 (75.9 and 2.1); `unique()` with nulls takes 0.13 seconds;
- a burst with ten subscribers is sent (1,766 of 1,800, the rest dropped by the full queue; 76 before); `/ready`
  recovers without traffic and stays 503 for a user that may not PUBLISH; replays, also after a restart and after
  `CLIENT KILL`, are refused, and new messages still arrive;
- two sites with different names on one Redis, PostgreSQL or MySQL database share no session, rate limit or job;
- `phv check` with 200,000 schedules takes 0.3 seconds (159); the job cap holds in one transaction; the memory store
  keeps 32 MB after 10,000 jobs given up (658 MB); `enqueue()` on a store that does not answer returns at the page's
  limit (10 seconds); 200 refused jobs at a full cap read 1,481 pages of the table (260,746);
- the templates with a full-width or no-break space are refused; the server's log holds no raw control character;
  hidden variables cannot be read in any case; links on the way of `[jobs] dir` and of `phv migrate -o` are refused.

The differential check: 182 templates of unusual but allowed structures (attributes without spaces, `/` between
attributes, duplicate attributes, odd comments and bogus comments, raw-text, table and frameset elements, NUL and CR)
were all accepted, and html5lib put the value in text or an ordinary attribute value in each, or dropped it; 672
spellings of places that must be refused (handlers, `style`, URLs, raw text, names, unquoted values) were refused, but
for 50 of the form `<a =onclick="`, where browsers name the attribute `=onclick`, which is not a handler. The check's
classifier was first tried on 11 known placements.

Regression tests: `only_html_whitespace_separates_attributes` and `log_text_stays_on_one_line` (phv_syntax);
`events_count_in_the_output_cap` (over HTTP/2 with small windows: three clients that do not read and one that reads),
`label_values_stay_on_their_line`, `frames_are_signed_and_checked`, `replayed_frames_are_refused`, the replay in
`publish_reaches_the_other_server_processes`, and another site's namespace in `database_stores_keep_the_same_rules`
(phv_server); `literal_html_in_a_loop_stops_at_the_memory_limit`, `builtins_over_long_values_stop_at_the_time_limit`,
`unique_compares_only_what_it_cannot_hash`, `enum_cases_are_timed`, `enum_lookups_are_timed`, and the jobs of another
namespace in `databases` (phv_driver); `server_variables_are_hidden_from_scripts`,
`sites_get_a_namespace_from_their_name`, `many_schedules_are_refused_at_once` and `a_linked_jobs_folder_is_refused`
(the configuration); `namespaces_keep_sites_apart` (rate limits); `what_the_memory_store_keeps_is_bounded` (jobs); and
`phv migrate -o` in the junction test (phv_cli). All tests pass on Windows, with PostgreSQL 17, MySQL 8.4 and Redis 7,
and with clippy (`-D warnings`) and rustfmt.

## 25. Fifteenth Assessment: the Whole Implementation (before 1.1.1)

Date: 2026-11-06. Scope: everything at 4542e43. That is 1.1.0 plus the work of 1.1.1 on the residual risks of §22–§24,
listed under "The residual risks of §22–§24" below. Every earlier fix was attacked again. Six reviewers each took one
area: the web server; the standard library and the bus; the compiler, the type checker, the VM and the JIT; templates,
the website and the examples; the tools and their supply chain; and background jobs. They attacked a release build
locally, and Unix-only code in a Linux container. Each finding was confirmed with a PoC, and the main session ran most
of them again before any fix. Five groups then fixed the findings, each with its regression tests. One of those groups
found two more problems of the same kinds as it worked (R18-2, and the SVG and `<textarea>` cases of R18-1), and fixed
them too.

Three reviewers were stopped part of the way through by safety checks: templates and the website, the tools, and the
compiler and VM. The main session then covered what they had not reached, in a smaller form with harmless marker values:
- the website's headers and scripts, and the demo served locally;
- `phv install`, `phv new`, `phv migrate -o` and `phv jobs retry` with hostile arguments;
- a tampered bytecode cache;
- integer and float edge cases in code the JIT compiles.

These checks found nothing beyond a cosmetic `number_format(-INF)`, which is fixed.

### Summary of Results

| Severity | Count | Fix |
|---|---:|---|
| Critical | 0 | — |
| High | 0 | — |
| Medium | 8 | Fixed |
| Low | 6 | Fixed |
| Info | 8 | Fixed |

### The residual risks of §22–§24 (fixed in 1.1.1)

| Residual risk | Fix |
|---|---|
| Event streams that do not read made readers miss events until they were dropped (R17-28), and kept 2 MB each for hours by reading a little at a time | Each connection can be aborted. A stream that ends holding output loses its connection, and one that cannot send what it holds within 30 seconds is ended. A stream that has held output for over a second is pushed out to make room for readers. The events' budget is a quarter of the output cap, and pages check both budgets. 80 streams that do not read held 64 MB that was never given back; now it returns to 0. A reader misses events for at most about a second |
| Concurrent transactions passed the job cap (R17-29: 111,000 with a cap of 100,000) | Adding to a store near its cap takes a lock: 100,000 on PostgreSQL, 99,000 on MySQL |
| Sites on one session database shared its limits and its shortest `session_ttl` (R17-30), and on Redis its bookkeeping keys | Each namespace has a table (`phv_sessions_<ns>`) and Redis keys of its own. Sessions stored by 1.1 are still read, so the upgrade signs no one out |
| A `</` hidden in a comment or attribute could still end an element early when a statement followed (§23) | A compile error |
| `==` on deep values used about twice the memory limit | The comparison reserves its memory first. The peak went from 214–241 MB to 113–138 MB under 128 MB |
| The JIT's stack limit was measured from where it was entered | It is measured from the real end of the thread's stack |
| A static file on Unix was opened by name and checked by two more lookups, and a FIFO put in its place stopped a thread | Opened one directory at a time from `public/`, without following links and without blocking |
| TLS files could be read through a link to a share or from a network drive | Refused |
| The demo's image and release archive were fetched by name | Pinned by digest and SHA-256. Dependabot proposes only new digests of Ubuntu 24.04 |
| Cycles were collected only every so many allocations | Also at 75% of the memory limit and before a memory error |
| One 8 MB source took about 1 GB to parse | At most 1,000,000 tokens per file. The peak went from 1,068 MB to 81 MB |
| A statement prepared again inside a transaction could abort it (PostgreSQL) | It commits |
| A SQLite connection set aside was closed up to 45 seconds later | 30 seconds |
| A bus message sent again after a reconnect arrived twice; refusals for the clock were silent | Dropped; counted (`phv_publish_total{result="clock"}`), with one warning per sender |

### Medium

| # | Finding | Fix |
|---|---|---|
| R18-1 | The rule against a hidden `</` worked within one file only. A partial with `<!--</<?= $v ?>`, rendered inside `<noscript>`, passed the check, and the visitor's value came right after a literal `</` that browsers end the element with. The same held for functions, methods and closures. HTML inserted inside `<svg>` or `<math>`, where `<textarea>` text is live markup, could hold a working `javascript:` link, and HTML inserted into `<textarea>` or `<title>` could end the element | `</noscript` is watched in every template and function. HTML inserted inside SVG, MathML, raw-text elements and comments is checked when it is inserted. Functions that write output there stop the page |
| R18-2 | A value ending in `--` before a literal `>` ended an HTML comment that the check thought was still open: `<!-- <?= $v ?>> <a title="-->" href=<?= $u ?>> -->` with `$v = "--"` gave a live `javascript:` link | Text right after a value in a comment may not start with `>`, `!>`, `->` or `-!>`. A comment ending is read across statements. HTML inserted into a comment may not end it |
| R18-3 | Each `finally` was compiled twice, and again at each `return`, `break` and `continue`: 1 KB of nested `try`/`finally` took 7.7 GB and 22 seconds to compile, in `phv check` and the editor too | Each `finally` is compiled once. An exit leaving several `try` blocks pops them in one step. At most 8,388,608 instructions per function |
| R18-4 | Each class kept a copy of all its ancestors: a chain of 16,000 empty classes took 1.6 GB and wrote a 254 MB cache entry, and 75,000 passed 19 GB. Type arguments of ancestors could double at each level (`class C<T> extends B<fn(T): T>`: 23 lines, 1.7 GB) | At most 256 levels of `extends` and `implements`, 2,000,000 entries for all classes together, and 10,000 nodes per inherited type argument |
| R18-5 | The type checker copied and merged every narrowed variable at each branch: 8,000 variables × 16,000 branches took 27 seconds, and 20,000 × 40,000 over 120 | Narrowing is kept in a shared tree that a branch copies in one step. A join costs what the branches changed |
| R18-6 | Each file was limited, but not a program: 24 imported files just under the token limit took 4.7 GB in `phv check` and in the editor, which held 1.9 GB between edits (also in VS Code's Restricted Mode) | A program may have 2,000,000 tokens, 32 MB of source and 2,000 files. The editor keeps no analysis of more than 250,000 tokens |
| R18-7 | A script could open SQLite databases without limit, each with a 2 MB cache that no limit counted: 400 opens took 602 MB under 16 MB, and a few such pages filled SQLite's 512 MB for the whole process | SQLite allocates through the program's allocator, so its caches, schemas, statements, temporary tables and sorts count against the script's memory. At most 64 databases are open per script |
| R18-8 | `enqueue()` with a key that another open transaction had added waited for that transaction: 40 seconds under a 2-second limit on MySQL. On PostgreSQL the page returned, but the wait went on in the database (20 backends after 5 rounds). Four such waits took every jobs connection of the process | Every statement of the job and session stores ends at the caller's deadline (`statement_timeout`, or the lock timeouts and `KILL QUERY` on MySQL). A connection that timed out is not reused. Keyed adds have connections of their own and do not queue for the key on MySQL |

### Low

| # | Finding | Fix |
|---|---|---|
| R18-9 | The cycle collector's own memory was not counted: 321 MB under 128 MB | It works within the memory left. When even part of the walk does not fit, it frees nothing |
| R18-10 | Temporary tables and large sorts were written to disk, even for `:memory:`, which needs no permission: 2.4 GB of disk in 8 seconds under 16 MB | They are kept in memory and counted |
| R18-11 | Near the job cap, each `enqueue()` counted the table twice and one at a time, and each transaction counted it again: 5–10 times slower, and one open transaction held up every add | A transaction counts once. Near the cap a count kept in the store is used, recounted at most once a second |
| R18-12 | Sources were resolved before the check that they lie in the project: on Windows, a link to `\\host\share\m.phv` made `phv check` and the editor connect to that host | Links to other machines are refused unresolved: modules, templates, packages, `phv.toml`, cache entries, the editor's documents, and pages and static files |
| R18-13 | Found in the fixes. A `continue` in a `finally` went to the wrong loop. A `break` in a `finally` of an `int` function passed the check, and the function then returned null | Compiled correctly; "must return a value" |
| R18-14 | Found by CI. A script that ran `DEALLOCATE ALL` or `DISCARD ALL` (also inside a database function) left its PostgreSQL connection unable to look up types: every later script on it failed ("prepared statement … does not exist") for as long as the connection lived | The reset looks for a statement the connection prepared when it was made; when it is gone, the connection is closed instead of reused |

### Info

| # | Finding | Fix |
|---|---|---|
| R18-15 | `TRACE` ran the page (without echoing the request) | 405, with `CONNECT` |
| R18-16 | Sessions stored by 1.1 stayed in Redis's shared bookkeeping after a site had a namespace | A site's sweep removes those that Redis has expired |
| R18-17 | The SQL and Redis job stores capped the number of jobs but not their bytes (6.4 GB of 64 KB payloads); keys of ended jobs were kept a day without a cap; MySQL swept them in one `DELETE` | At most 1 GB of payloads and 100,000 kept keys; sweeps take 10,000 rows at a time |
| R18-18 | Link targets in the NT namespace (`\GLOBAL??\UNC\`, `\Device\Mup\`) were taken as local | Taken as remote, as any target starting with a single `\` |
| R18-19 | An error inside a Redis array reply left the rest unread on a pooled connection. A rate-limit key with a NUL character could match another site's key | The whole reply is read. Such keys get a prefix of their own |
| R18-20 | A second `:memory:` database could not be opened. A stale cache entry of another binary was decoded whole (1.9 GB) | Each in-memory database has the whole limit (counted). The entry's stamp is read first |
| R18-21 | `number_format(-INF)` printed `inf` | `-inf` |
| R18-22 | Found by CI. Processes making the same table at the same moment failed on PostgreSQL (a duplicate in its catalog) and on MySQL (a duplicate column) | Made again, which then finds it; the column counts as there |

### Residual Risks

- A program just under the per-program budget, such as two files of 1,000,000 tokens, can take about 415 MB to check.
- The type checker reads a loop or `try` body again for each enclosing one. This is bounded by the nesting limit of 500
  (5 seconds for 200 levels around 100,000 assignments).
- When even part of its walk does not fit in the memory left, the cycle collector leaves the cycles of older objects
  until the script ends. Its last try before a memory error can pass the limit by an eighth of it.
- SQLite's memory for a kept connection is an estimate, off by a few KB. Several workers with large limits can still
  reach SQLite's 512 MB for the whole process (it starts freeing caches at 384 MB).
- `enqueue()` with a key inside the script's own MySQL transaction can wait for another transaction's key, as long as the
  script may run. Near the cap, an add can be refused for up to a second while there is room. A transaction that adds
  more than 10,000 jobs while others add can pass the cap by that many.
- Inside SVG, a value that completes the end tag of a raw-text element other than `<noscript>` is caught when the page
  runs, not by `phv check`. A value right after `<!--` needs a space between them.
- A directory link to a share on the way to a document the editor opens is still looked at (the editor already
  contacted the host to open the file).
- §24's residual risks remain, apart from those fixed above.

### Verification

The PoCs were run again against the fixed build (before → after):
- the partial in `<noscript>` is refused at check (it printed `<noscript><!--</mk1 mk2-->`). The comment ended by `--`
  is refused at check (it printed a live link);
- nested `finally`: 1.75 GB and 3.7 seconds → 2 MB and 0.02 (7.7 GB → 2 MB at 24 levels);
- class chains: 16,000 classes 1.6 GB and 8.2 seconds → 52 MB and 0.19, refused for depth. 75,000 → 198 MB;
- narrowing: 30 seconds → 0.22, and over 120 → 0.5;
- 24 large modules: 4.7 GB → 314 MB, refused. In the editor, 4.7 GB peak and 1.9 GB held → 314 MB and 7–9 MB, with
  diagnostics in 0.5 seconds (11);
- SQLite: 400 opens 572 MB → a memory error at 16 MB (28 MB peak). Six pages at once: 567 MB, with other pages refused
  → 119 MB, each stopped at its own limit. A 200 MB temporary table and a 100 MB `ORDER BY` stop at the limit instead
  of using disk;
- a keyed `enqueue()` behind an open transaction: MySQL 40 seconds → 2.1, with an error the page can catch. PostgreSQL
  leaves 0 waits in the database (20). Plain adds meanwhile answer in 0.02 seconds;
- near the cap, adds are 2.2–2.3 times slower than far from it (5.6–8). In a transaction there is no difference (5–6.3);
- the cycle collector: 306 MB → 195 MB (the allocator's own floor at 128 MB counted);
- a link to `\\localhost\C$` is refused without a connection. A TLS file behind a junction to `\GLOBAL??\UNC\…` is refused;
- `TRACE` answers 405.

All 203 templates in the repository (tests, examples, website, standard library) check as before. The differential
check of §24 (854 templates against html5lib) gave the same results with the new rules. Ordinary programs compile and
run at the same speed: 120 files checked, ratio 1.008; 89 run, ratio 0.994. A loop that does almost nothing but pass
through `finally` is about 13% slower.

Regression tests:
- **phv_syntax:** `hides_noscript_outside_one`, `values_and_statements_cannot_end_a_comment`.
- **tests/cases:** the cases `err_context_noscript_partial`, `err_context_noscript_stmt`, `err_html_in_svg`,
  `err_html_in_textarea`, `err_output_in_svg`, `err_comment_end`, `err_comment_value`, `err_html_in_comment`,
  `err_output_in_comment`, `err_finally_break`, `contexts_inside`, `comment_values` and `finally_nested`.
- **phv_types:** `pmap::agrees_with_a_btreemap`, `pmap::joins_follow_the_changes`.
- **phv_driver (compile_cost):** `nested_finally_blocks_are_compiled_once`, `deep_exits_are_short`,
  `class_chains_are_limited`, `type_arguments_of_ancestors_stay_small`, `narrowing_costs_what_branches_change`.
- **phv_driver (limits):** `a_program_of_many_large_files_is_refused_before_it_is_checked`,
  `a_program_of_too_many_files_or_bytes_is_refused`.
- **phv_driver (other):** `sources_behind_a_link_to_a_share_are_refused_unresolved`;
  `the_collector_stays_within_the_memory_limit` (cycles); `page_caches_count_against_the_memory_limit`,
  `a_script_holds_at_most_64_databases`, `temporary_tables_count_against_the_memory_limit` and
  `sorts_count_against_the_memory_limit` (sqlite_limits); `keyed_jobs_wait_no_longer_than_the_time_limit`,
  `adding_jobs_near_the_cap_stays_cheap` and `a_connection_whose_statements_a_script_deallocated_is_not_reused`
  (databases).
- **phv_cli (the editor):** `programs_past_the_budget_are_not_checked_or_kept`,
  `documents_behind_a_link_to_a_share_are_not_resolved`.
- **phv_std:** `object_manager_spellings_of_shares_are_remote`, `only_the_part_below_the_base_is_looked_at`,
  `an_error_in_an_array_reply_reads_the_whole_reply`, `job_stores_cap_the_bytes_of_payloads`,
  `a_jobs_table_of_1_1_gets_its_size_column_later_when_held` and `sites_with_a_namespace_sweep_what_1_1_left`; the
  rate-limit keys in `namespaces_keep_sites_apart`.
- **phv_server:** `trace_and_connect_are_refused`.

All tests pass on Windows and on Linux, with PostgreSQL 17, MySQL 8.4 and Redis 7, and with clippy (`-D warnings`) and
rustfmt.

## 26. Sixteenth Assessment: the Whole Implementation (before 1.2.0)

Date: 2026-11-07. Scope: everything at bf7aee0. That is 1.1.1 plus the features of 1.2: sign-in with OAuth 2.0 and
OpenID Connect (`std.oauth`), JSON Web Tokens signed with RS256, ES256, EdDSA and HS256 with key sets (`std.jwt`),
streaming responses (`response()->flush()`) and OpenTelemetry traces (`[telemetry]`). Every earlier fix was attacked
again, and the fixes of 1.1.1 (§25) were assessed for the first time.

Seven reviewers each took one area: sign-in and tokens; the web server; the standard library and its clients; the
compiler, the type checker, the VM and the JIT; templates, the website and the examples; the tools and their supply
chain; and background jobs with the job and session stores. They attacked a release build locally, with fake
identity providers, key-set servers and collectors on their own ports. Each finding was confirmed with a PoC. The
template findings were also confirmed in Chrome, and the main session ran the most serious ones again. Seven groups
then fixed every finding, each with regression tests. As they worked, three of them found and fixed more of the same
kind; these are noted in the rows below.

### Summary of Results

| Severity | Count | Fix |
|---|---:|---|
| Critical | 1 | Fixed |
| High | 2 | Fixed |
| Medium | 13 | Fixed |
| Low | 15 | Fixed |
| Info | 9 | Fixed or documented |

### Critical

| # | Finding | Fix |
|---|---|---|
| R19-1 | The VS Code extension started `phv lsp` and `phv dap` by the bare name `phv`, with the opened folder as their working directory. On Windows a program found by name is looked for in the working directory before `PATH`, so opening a `.phv` file in a cloned folder ran the folder's own `phv.exe`. This held also in Restricted Mode, and in every published version of the extension | The extension finds `phv` itself: an absolute `phv.path`, or the first `.exe`/`.com` in an absolute `PATH` entry outside every workspace folder. It starts that file by its full path, from its own folder, with `NoDefaultCurrentDirectoryInExePath` set. A relative `phv.path` is refused |

### High

| # | Finding | Fix |
|---|---|---|
| R19-2 | After a `try` with `finally`, the type checker kept what the `try` had proved about variables and forgot what the `finally` assigned: a narrowed variable could hold an object of another class. A script read and overwrote another class's private property, and a typed function returned null (a VM "compiler bug"). Found in the fixes: the step of a `for` loop and the condition of `do`-`while` ignored the paths that `continue`d | After a `finally`, what it assigns (and, when it calls anything, every property fact) comes from its end. The flow of each `continue` is joined into the step and the condition |
| R19-3 | SQLite looked at the time limit only every 10,000 steps, and one step can be a function call on a 30 MB value: queries ran 12–468 seconds under a 1-second limit. Statements that read a long value over and over without allocating could run minutes more | PHV's own SQL functions check the time as they go. SQLite's allocations stop at the deadline, and the progress handler runs every 100 steps. Each statement's program is weighed when it is compiled, and one that would read long values too many times is refused. Values are limited to 16 MB |

### Medium

| # | Finding | Fix |
|---|---|---|
| R19-4 | A page that streamed to a visitor who had left, or answered a HEAD request, freed each flushed chunk twice in the count: a page under 32 MB took 400–794 MB | A chunk leaves the page's count only when it really leaves the page |
| R19-5 | Streamed chunks went to the connection whole, so the kernel held them outside the output cap: 60 clients that do not read held 470 MB, where plain pages are refused after 17 | Streamed chunks are counted in slices of 64 KB, like other output |
| R19-6 | Span names and attributes came from the request's method, which a visitor sets to any 63 KB token, and the exporter's queue was bounded by count only: 9,000 requests held 1.36 GB | Methods are named as OpenTelemetry says (`_OTHER` for others). Names and text attributes are cut to 1,024 bytes, and the queue is bounded at 16 MB too |
| R19-7 | A `catch` after `flush()` inside its `try` cut the output at an offset from before the flush, so it wrote into a tag the flush had sent: an escaped value became an event handler | Handlers follow what was flushed. A `catch` that would write into HTML that was already sent stops the page, and the response is cut |
| R19-8 | An `</svg>` or `</math>` that browsers ignore (while an HTML element in `<foreignObject>` or `<annotation-xml>` is open) put the template check out of step: a visitor's string ran in an SVG `<script>` with the page's nonce | The check follows the elements inside SVG and MathML as browsers do, and refuses what it cannot follow (such as breakout tags) |
| R19-9 | `<![CDATA[` in literal SVG or MathML was read as a bogus comment ending at `>`; browsers read it to `]]>` | Read as browsers do; a `>` inside it is refused |
| R19-10 | The rule of R18-2 (no `>` right after a value in a comment) did not see a loop's way back to its start: `<!-- <?phv foreach (…) { ?>> <a href=<?= $x ?><?phv } ?> -->` gave a live link | Loop bodies, `break`, `continue` and `finally` are checked against what may come before them |
| R19-11 | The rule of R18-1 did not cover `Html` inserted into a comment, an attribute or raw text: a partial with a `<noscript>` fallback ended an enclosing `<noscript>` | `Html` holding `</noscript` (and, where the check may be out of step, any raw-text end tag) is refused there. `<` and `>` in `Html` in attributes are written as character references |
| R19-12 | Inferred generic types had no size: `$a = dbl($a)` doubled per line, so 480 bytes took 4.5 GB. Found in the fixes: a type one level larger per line, copied at each use, took over 60 GB with 30,000 lines | A type has at most 10,000 nodes, and a program's large types 2,000,000 in all |
| R19-13 | Long `elseif` chains, statements without braces under `if` and loops, and deeply nested types overflowed the stack and killed the process | They count against the nesting limit of 500 |
| R19-14 | Constant strings were copied for each class's field defaults (inherited ones too) and at each use. A 24 KB source took 2.5 GB, and wrote a 1 GB cache entry on every run | Field defaults count against the 16 MiB constant budget, and strings are shared. Cache entries over 256 MB are not written |
| R19-15 | The editor kept one analysis per open document, with no total: 20 pages held 1 GB | One budget for all kept analyses (250,000 tokens, 16 MB of source); old ones are dropped |
| R19-16 | Keyed `enqueue()`s waiting for another transaction's key took the connections that the scheduler and adds near the cap use: a scheduled minute was lost, and other visitors' adds failed after 10 seconds | Keyed adds try briefly and hold no connection between tries. At most 8 wait per store and process, and others fail at once. The scheduler has connections of its own and tries a failed minute again |

### Low

| # | Finding | Fix |
|---|---|---|
| R19-17 | An RSA key with a modulus of zero panicked instead of being refused, and one such key in a provider's key set failed every sign-in | Refused; a key set skips it |
| R19-18 | `OAuth::callback()` after `flush()` did not use up the sign-in's state, so the callback could be replayed for 10 minutes | Refused after `flush()` |
| R19-19 | Key sets and discovery documents were decoded outside the script's memory limit | Decoded within it |
| R19-20 | A kept key set was found by its URL only, so two services that differ by a header (a tenant) shared one | Kept per service, headers and URL |
| R19-21 | `rate_limit_reset(key)` also reset other keys that start with `key` and a NUL | Exactly its own key |
| R19-22 | A fixed query parameter of an `[http]` service could be replaced by a spelling that PHP backends read as the same name (`account.id`, `account%20id`) | Names are compared as PHP and ASP.NET read them |
| R19-23 | A streaming page held its worker until its time limit when the client stopped reading | `flush()` gives up after 10 seconds without progress |
| R19-24 | A streamed form that wrote a CSRF token after `flush()` was sent without `Cache-Control: private, no-store` | A streamed head is personal when the request carries a cookie or `Authorization`; a token after `flush()` needs one |
| R19-25 | A page could write a visitor's text unescaped as `text/plain` and then switch to `text/html` | The type cannot change to an escaped one after unescaped output |
| R19-26 | Raw-text start tags inside `<select>` are ignored by older parsers but were read as raw text | Refused inside `<select>` |
| R19-27 | Two values in a row in a comment (`-` and `Html` `-> …`) could end it | `Html` in a comment may not start with `>`, `!>`, `->` or `-!>` |
| R19-28 | A running server's check for changed files went through a directory link to a share (R18-12) | Links on the way are checked too |
| R19-29 | `phv install` followed a `path` package that is a link to a share, and the link check misread relative links when the project is reached through a link | Refused before it is opened; links are resolved against the real folder |
| R19-30 | R18-14 could be passed by deallocating only the statements the client prepared to look up types | The reset checks every statement the client prepared, by name |
| R19-31 | A site without a namespace swept the shared session store by its own `session_ttl`, removing other sites' sessions stored by 1.1 | Such sites have a table and keys of their own; the shared store is swept only by its 24-hour lifetime |

### Info

| # | Finding | Fix |
|---|---|---|
| R19-32 | An ES256 signature has two valid forms (high and low S), so a token's text is not unique | Documented: check `jti` for replays |
| R19-33 | The older `jwt_decode` accepted `crit` and an `iat` in the future | Refused, as in `std.jwt` |
| R19-34 | A plain OAuth 2.0 provider's `id_token` was passed on unverified; `token_type` was not checked; the RFC 9207 `iss` was checked after an error; a failed first fetch of a key set was tried again by every request | `idToken` is set only after verification; a `token_type` other than Bearer is refused; `iss` is checked first; failures are remembered for 10 seconds |
| R19-35 | A discovery document could send the token request, with the client secret, in plain text to a loopback port of the provider's choosing | Discovered endpoints keep the issuer's scheme, and loopback hosts only when the issuer is one |
| R19-36 | PHV's SQL functions allocated outside SQLite's budget | Checked against the script's memory |
| R19-37 | `phv fmt` wrote through hard links | Refused |
| R19-38 | `phv parse` printed deep trees in cubic time; `hexdec()` and its kind quoted the whole argument in errors | Linear; 64 characters at most |
| R19-39 | With `[telemetry]` or `request_ids`, requests to third parties carry `traceparent` | Documented (as before) |
| R19-40 | §25's residual risks were checked again: the type checker's re-reading of nested bodies (240 levels around 200,000 statements: 4 seconds), the collector's budget, the program budget | Hold as written |

### Residual Risks

- A statement at the weight limit can still run about 3 seconds past the time limit on a fast machine, and more on
  a slow one. Statements heavier than the limit (more than about 30 columns of arithmetic on values) are refused and
  must be split.
- A streaming client that stops reading holds a worker for up to 10 seconds.
- The template check now refuses some markup that browsers would accept. Examples are unclosed HTML elements inside
  `<foreignObject>`, breakout tags in SVG, any element in `<annotation-xml>` other than `<svg>`, and loops in SVG
  that leave elements open. Inside SVG, a value that completes the end tag of a raw-text element other than
  `<noscript>` is still caught when the page runs.
- If a script deallocates some of the client's type lookups in the same run as the first lookup, one later statement
  on that connection can fail once before the connection is closed.
- Concurrent requests during the very first fetch of a key set all wait for it.
- While 1.1.1 and 1.2 processes of a site without a namespace run side by side, the older ones do not read sessions
  the newer ones stored.
- §25's residual risks remain, apart from those checked above.

### Verification

The PoCs were run again against the fixed build (before → after):
- the extension, with a marker `phv.exe` in the opened folder: started from the folder → started from the installed
  `phv` by its full path;
- `finally` type confusion: accepted, and a private property read and overwritten → refused at check;
- SQLite: 468, 202, 24 and 18 seconds under 1 → 1.1. Statements reading a 16 MB value 1,000 times: 66 seconds →
  refused in 0.1;
- streaming after the visitor left: 395 MB under 32 MB → 56 MB and a memory error. Stalled readers: 60 admitted → 18
  (plain pages 17). Long methods: 507 MB → 17 MB;
- the template PoCs (SVG breakout, CDATA, the loop, `</noscript` in `Html`, two values) are refused at check or stop
  when the page runs. The differential check of §24, extended to 6,048 templates around SVG, MathML, `<select>` and
  `<noscript>`, flags none (450 before). All 237 templates in the repository check as before;
- generic types: 4.5 GB → 4 MB and an error. Deep chains: a stack overflow → an error in 0.1 seconds. Constant strings:
  2.5 GB and a 1 GB cache file → 6 MB and an error;
- the editor with 20 pages: 1,064 MB held → 6 MB;
- jobs: other visitors' adds while keys are held, 3 of 28 failed and a minute was lost → 81 of 81 within 1.6 seconds,
  and every minute ran;
- sign-in: a zero-modulus key in a provider's set, 500 → 200; a replayed callback after `flush()`, accepted three
  times → refused; a token of one tenant through another's service, valid → invalid.

Ordinary programs check and run at the same speed (137 test cases and examples: 2.06 and 2.08 seconds).

Regression tests are named after what they check, in each crate's tests and in `tests/cases` (`err_finally_flow`,
`finally_flow`, `err_svg_end_ignored`, `err_svg_cdata`, `err_loop_comment_start`, `err_select_raw_text`,
`err_html_noscript_in_comment`, `err_html_noscript_in_textarea`, `err_html_raw_end_in_svg_comment`,
`err_html_comment_start`, `jwt`, `numbers`). Some of the others:
- `flush.rs`, `stream.rs`: streaming;
- `compile_cost.rs`: types, chains and constants;
- `sqlite_limits.rs`: SQLite's time and memory;
- `databases.rs`: keyed waits, the scheduler and type lookups;
- `jwks.rs`, `oauth.rs`: key sets and sign-in;
- `editors/vscode/test/phv-path.test.js`: finding `phv`.

All tests pass on Windows and on Linux, with PostgreSQL 17, MySQL 8.4 and Redis 7, and with clippy (`-D warnings`)
and rustfmt.

## 27. Seventeenth Assessment: the Whole Implementation (the 1.2.0 release candidate)

Date: 2026-11-07. Scope: everything at 6da479b: the fixes of §26 and the improvements made for 1.2.0, none of which
had been assessed before. The improvements are the cache of the lookup of `_middleware.phv`, `_404.phv` and
`_500.phv`; the teardown of a page's VM and the return of its database connections after the response has been handed
over; kept SQLite connections keeping their prepared statements; the SQLite allocator set up together with SQLite's
initialization; the test isolation (`phv_std::testing`); the benchmark harness; and the release profile with fat LTO.
Every earlier fix was attacked again.

Eight reviewers each took one area: the web server; the standard library and its clients; the compiler, the type
checker, the VM and the JIT; templates, the website and the examples; the tools and their supply chain; background
jobs with the job and session stores; sign-in and tokens; and, new this time, the running system as a whole (ordering,
timing, restarts and resource accounting across requests). They attacked a release build locally, with fake identity
providers and collectors on their own ports and the three databases in Docker. Each finding was confirmed with a PoC.
The main session ran the High again, and checked the extension in a real VS Code with marker programs in the opened
folder, behind a PATH junction into it, and named by workspace settings: none ran. The template check was confirmed
against Chromium and WebKit: of 5,812 generated templates, the 1,971 that PHV accepts rendered without a value
leaving its context, and the 6,048 templates of §26 still flag none. Four groups then fixed every finding, each with
regression tests.

### Summary of Results

| Severity | Count | Fix |
|---|---:|---|
| Critical | 0 | |
| High | 1 | Fixed |
| Medium | 0 | |
| Low | 2 | Fixed |
| Info | 8 | Fixed or documented |

### High

| # | Finding | Fix |
|---|---|---|
| R20-1 | A closure passed to a builtin with several versions of the same arity (`groupBy`) could reassign a property the type checker had narrowed (`if ($h->t is Box)`), and the read after the call was compiled for the narrowed class: a script read another class's private property, or made the VM stop with a "compiler bug" (a 500). The single-version path and user calls already forgot property facts when a closure was passed; this path did not | Every builtin call forgets property facts when the version chosen takes a closure or an argument holds one (also inside a `finally`); a call without a closure keeps them |

### Low

| # | Finding | Fix |
|---|---|---|
| R20-2 | A keyed `enqueue()` inside `transaction()` waited for a key held by another transaction until that transaction ended or the script's time limit, not the 10 seconds the spec promises (14 seconds measured against a 25-second holder, on PostgreSQL and MySQL), and held the site's job lock meanwhile, so other transactions' adds waited behind it | The keyed row is tried first, waiting at most 10 seconds (PostgreSQL: in a savepoint under a lock timeout; MySQL: a look without waiting on a store connection, tried again until then), and the site's job lock is taken only for a job that is added |
| R20-3 | A visitor who read a streamed page a few kilobytes every 9 seconds kept it from the 10-second stall cut of R19-23, so one connection held a worker for the whole time limit, and `requests_per_minute` does not count connections: `workers` such connections held every worker | `[server] streams_per_address` (a quarter of `workers`, at least 2) bounds the pages one address may stream at once; past it, `flush()` answers 429 and returns false. A proxy that limits connections per address remains the recommendation |

### Info

| # | Finding | Fix |
|---|---|---|
| R20-4 | A streamed response to an HTTP/1.0 request was delimited by closing the connection, as it must be, but carried `connection: keep-alive`; the connection was closed, so nothing was misread | It is sent as HTTP/1.0 with `Connection: close` |
| R20-5 | An idle kept SQLite connection's memory counted toward no script, and the idle pool was bounded by count only (256 connections, 32 statements each): 3 GB at the most | Idle connections hold at most 64 MB together (the largest are closed first), and a statement over 1 MB is not kept |
| R20-6 | An absolute `phv.path` was accepted with any extension, where the search of `PATH` keeps `.exe` and `.com` only (a `.cmd` would not have started: the extension uses no shell) | On Windows it must end in `.exe` or `.com` |
| R20-7 | A page's `XA PREPARE` on MySQL survived the connection's reset (MySQL detaches a prepared transaction), holding its locks until an `XA ROLLBACK` | `XA PREPARE` and PostgreSQL's `PREPARE TRANSACTION` are refused, as `LOAD DATA LOCAL INFILE` is; comments between the words are seen through |
| R20-8 | Two requests of one visitor saving the session at once: the save that ends last wins, and the other's keys are lost | Documented in the spec |
| R20-9 | Degenerate public keys (an even or a one RSA exponent, a P-256 point off the curve, an Ed25519 point of small order) were accepted as keys and failed every verification | Refused when the key is read: an RSA exponent is odd and from 3 to 2^33 − 1 with a modulus of 2048 to 8192 bits; a P-256 point is on the curve; an Ed25519 point decodes and is not of small order |
| R20-10 | The time in the access log ends when the response is handed over, so a worker held by a slow reader does not show there | Documented: `phv_workers_busy` shows workers held by streaming and by teardown |
| R20-11 | Measured, not changed: 8,192-bit RSA keys verify 1,385 tokens a second with 8 keys tried; a key set is fetched at most once in 30 seconds for an unknown key id; §26's residual risks hold as written | Holds |

### Residual Risks

- A streaming page still holds a worker for up to its time limit for a visitor who keeps reading; the per-address
  bound limits how many, and counts the proxy's address when the site runs behind one (raise it there, or set it to
  0, and let the proxy limit connections per address).
- A transactional `enqueue()` of another key waits for the store lock of a transaction that is adding a job, up to
  that transaction's time limit.
- A page's connection whose reset takes long (a database that answers slowly) holds its worker for up to 10 seconds
  after the response.
- Up to 64 MB of idle SQLite memory per process is counted toward no script, for up to 30 seconds.
- The access log's time does not include streaming or teardown.
- §26's residual risks remain.

### Verification

The PoCs were run again against the fixed build (before → after):
- `groupBy` type confusion (three shapes): accepted, and a private property read, or a VM stop → refused at check;
- a keyed add in a transaction against a 25-second holder: added after 14.1 seconds (PostgreSQL) and 14.1 (MySQL)
  → refused after 10.1 and 9.8; an unkeyed add meanwhile, 11 ms;
- two trickle readers with `workers = 2`: a plain page answered 503 after 10 seconds → with the bound at 1, 200 in
  10 ms; 8 readers against 8 workers: 2 stream, 6 get 429, a plain page in 10 ms;
- a streamed HTTP/1.0 response: `connection: keep-alive` → `connection: close`;
- 8 idle connections with `IN` statements of 2,000 values held 2.4 MB, one statement of 8,000 values 1.1 MB →
  under the caps (3 GB at the most → 64 MB);
- keys with `e = 1`, `e = 2`, an all-zero P-256 point, the identity of Ed25519: loaded → refused with a message;
- the extension: an absolute `.cmd`, `.bat` or `.ps1` resolved → an error naming `.exe`.

The benchmarks of `bench/`, run back to back with the binary of 6da479b under the same load, are within noise: the
micro-benchmarks 0.94–1.13, the server's pages 0.97–0.99 in requests per second, the editor 0.95–1.08.

Regression tests are named after what they check, in each crate's tests and in `tests/cases` and `tests/db`
(`err_groupby_prop_narrow`, `groupby_prop_narrow`, `mysql_xa`, `postgres_prepared`, `jobs_tx_hold` with
`jobs_tx_add`). Some of the others:
- `stream.rs`: `slow_readers_at_one_address_cannot_hold_every_worker`,
  `a_streamed_response_to_an_http_1_0_request_says_the_connection_closes`;
- `databases.rs`: `keyed_jobs_in_transactions_wait_no_longer_than_outside_them`;
- `sqlite_pool.rs`: `large_statements_are_not_kept`, `idle_connections_hold_a_bounded_memory`;
- `jwt.rs`: `rsa_keys_with_a_degenerate_exponent_are_refused`, `ec_and_ed25519_keys_off_the_curve_are_refused`;
- `netdb/mod.rs`: `prepared_transactions_are_refused`;
- `editors/vscode/test/phv-path.test.js`: the extension of an absolute `phv.path`.

All tests pass on Windows and on Linux, run at the same time against one PostgreSQL 17, MySQL 8.4 and Redis 7, and
with clippy (`-D warnings`) and rustfmt.

## 28. Eighteenth Assessment: the Whole Implementation (before 1.2.1)

Date: 2026-11-08. Scope: everything at 34d8272: the fixes of §27 and the three batches of the 1.2.1 improvement plan,
none of which had been assessed before. The improvements are the table-driven HTML context tracker, the SQLite cost
model with its opcode pin and the per-process weight cache; the PostgreSQL and MySQL resets, the sharded rate limiter
and route histograms, the route cache keyed by the path, exception traces built when printed, the editor's debounce,
the HTTP client taking kept connections for every method, one wording for time-outs; lazy session values, the Redis
pipeline API, `phv jobs adopt`, mimalloc under the counting allocator, the JIT compiling loop and recursive functions
as shared units, the split of `netdb.rs`, and the callback fast path. Every earlier fix was attacked again.

Eight reviewers each took one area, as in §27: the web server; the standard library and its clients; the compiler,
the type checker, the VM, the JIT and the allocator; templates and the tracker; the tools and their supply chain;
background jobs and the stores; sign-in and tokens; and the running system as a whole. They attacked a release build
locally, with fake identity providers, Redis servers and HTTP servers on their own ports, the three databases in
Docker, and Linux builds in Docker where a question depended on the platform. Each finding was confirmed with a PoC;
where a reviewer had reasoned from the source alone, the main session sent it back to run the attack, which turned
two notes into findings. The tracker was read row by row against the WHATWG tokenizer and tree-construction rules and
against Chrome 157 and html5lib: every template PHV accepts rendered the value in the context it was escaped for.
The vendored mimalloc was compared byte by byte with the upstream 3.3.2 release. Eight groups then fixed every
finding, each with regression tests.

### Summary of Results

| Severity | Count | Fix |
|---|---:|---|
| Critical | 0 | |
| High | 0 | |
| Medium | 1 | Fixed |
| Low | 10 | Fixed |
| Info | 16 | Fixed or documented |

### Medium

| # | Finding | Fix |
|---|---|---|
| R21-4 | The key under which compiled JIT units are shared between the pages of a process was not an injective encoding: a float constant was its eight raw bytes and a newline, without a tag or a length, while the other constants were their debug text. Every finite float is nameable by a 17-digit literal, so a page whose float constant's bytes spell the text of another page's string constant made the second page run the first's native code and print the first's constant (one page per site in one process). Introduced by the shared units of this cycle; not in a release | The key is a serialization in which every part is tagged and length-prefixed: the instruction words, signatures, constant count, each constant with its kind (floats by their bits), and the builtin names; a key that cannot be built makes the unit private |

### Low

| # | Finding | Fix |
|---|---|---|
| R21-1 | The rate limiter's table, when full, evicted expired windows, then windows not at their limit, then the oldest limited ones: a flood of about 100,000 keys each limited after one hit (`max = 1`) evicted a victim's long-standing limit, which the module's comment said could not happen | When the table is full, limited windows are evicted newest first (a flood's own windows), after expired and not-limited ones; the doc says what is guaranteed: a limit set before a flood survives it, one set while the table is already full of an attacker's limited windows can be lost after about 10,000 more |
| R21-2 | A request with side effects on a kept connection was sent again on a fresh connection when its write had failed before completing, so a server that read the head and part of a large body and then reset got the request twice (a 16 MB POST on Linux: three request heads over two connections, and the script saw 200); 1.2.0 never retried such a request | A request with side effects is never sent twice: on a kept connection a failed write or read gives `NetworkError`; only `GET`, `HEAD` and `OPTIONS` are retried on a fresh connection, when no byte of an answer came |
| R21-3 | The Redis client checked for stray bytes only right after reading a reply; a reply arriving a moment later on a pooled connection was read by the next command as its own (the HTTP client got that check after the 1.0.0 audit, the Redis client had not) | Before a command or pipeline is written on a kept or pooled connection, the socket is checked for pending bytes or a close; either marks the connection out of step and it is replaced, nothing having been sent; a stray reply that lands in the window between a command's write and its reply is still read as that reply (RESP has no way to tell them apart) and the next command detects the shift |
| R21-5 | `Html` written inside `<select>` (a partial rendered there, or a partial that opened a `<select>` and ended inside it) was not checked for the raw-text elements §26 refuses in the template itself, so with a parser of the old rules their text was read as markup | Element text inside an open `<select>` is its own context: `Html` written there is checked at run time for those elements, and a template may not end inside an open `<select>` |
| R21-6 | `phv jobs adopt --yes` moved jobs one at a time inside a transaction (four to five round trips each, 500 per batch) within a page's 10-second budget, so over a store four milliseconds away nothing could be moved at all, and 50,000 jobs took three minutes locally; the dry run gave no hint | A batch of up to 1,000 jobs is moved in a few set-based statements under the site's job lock (PostgreSQL: a locked select, then an insert returning the accepted ids whose rows alone are deleted; MySQL: a marked insert that tells inserted rows from duplicates, then the delete, in one transaction), with a budget of 60 seconds and a batch size that halves when a batch takes long; 50,000 jobs move in 2.3 seconds locally and in 26 seconds over a 30 ms store |
| R21-7 | A provider's discovery document saying it always sends `iss` with the authorization response was not read, so a callback without `iss` was accepted when the provider sends one (RFC 9207 says a client that knows must require it); the mix-up defence then rested on the redirect URI alone | The discovery document's flag is kept; when it is set, a callback without `iss`, for a code or an error, is refused as `invalid_issuer` |
| R21-8 | An RSA public key with an even modulus was accepted and failed every verification (the class §27 refused for the exponent) | The modulus must be odd; refused when the key is read, from a JWK or a PEM, and skipped in a key set |
| R21-9 | File session store on Windows: a read that overlapped another request's save of the same session (a rename over the file) failed the real-name check, and the request ran with no session; one in a hundred reads with four writers and eight readers of one visitor, and a page that then wrote signed the visitor out | A read that fails tries the directory lock without waiting: free, it reads once more under the lock and that result is final; busy (a save or the sweep holds it), it yields or sleeps a millisecond and tries again, forty times at most. A read that succeeds never takes the lock |
| R21-10 | A session record whose `last` was near the largest value overflowed the refresh check and panicked the request task (overflow checks are on in release builds): 500 on every page of that visitor, the record never removed; only a writer of the store can make such a record | The refresh check saturates, and a record whose `last` or `created` is more than a day in the future is damaged: refused and removed, in every store |
| R21-11 | A page calling an `[http]` service that pointed at its own server held its worker while waiting up to 10 seconds for another worker of the same server: with as many concurrent pages as workers, every worker and a bystander's page stalled for 10 seconds | At startup every `[http]` and `[websockets]` URL whose host resolves to this server's address (or to a loopback or local address when it listens on every address) and whose port is the server's is refused with a message that says to call the code directly or serve the API elsewhere |

### Info

| # | Finding | Fix |
|---|---|---|
| R21-12 | The SQLite weight cache keyed two database files alike only when the file system reports no creation time, the inode was reused and the schema cookie matched; not reproducible on Windows, overlayfs or tmpfs | When the file system reports no creation time, a per-open counter is folded into the key, so weights are per connection there |
| R21-13 | An `HGETALL` reply with an odd number of elements lost its last field | Refused as an unexpected reply |
| R21-14 | mimalloc's size-class rounding and per-thread retention are outside the counted memory limit, as the system allocator's were | Documented below |
| R21-15 | The per-thread memory counter relies on callers balancing `charge_thread`; the teardown after the response runs on the VM's thread, so no cross-thread free reaches the counters today | Documented below |
| R21-16 | A self-closed `<script/>` or `<style/>` inside SVG put the tracker into raw text, where browsers pop the element at once; everything after it was refused | A self-closing start tag read as foreign content opens no raw text; in an HTML integration point, where browsers ignore the slash, it still does |
| R21-17 | `<base href>` with `safe_url` and `<meta charset>` are accepted; the CSP's `base-uri 'self'` and the response's charset header make them harmless | Documented in the specification's table of URL attributes |
| R21-18 | On MySQL, `phv jobs adopt` reported any refused insert (a CHECK or NOT NULL constraint) as "a job with its key" and went on | The insert reports duplicates exactly (`ON DUPLICATE KEY UPDATE` with a marker) and every other refusal with the server's message, as PostgreSQL does |
| R21-19 | The Redis `phv jobs adopt`, the dry run too, ran one script over the whole shared store, stalling every client of the server for about a second at the caps | The script moves 1,000 jobs per call and is called again from where it stopped; the worst wait of another client fell from 751 to 65 ms |
| R21-20 | `phv.path` forms that passed §27's check: an alternate data stream named `.exe` on a `.cmd`, the `\\?\` and `\\.\` prefixes; a PATH entry spelling the workspace as an administrative share was searched | An absolute setting with a `:` after the drive letter or a `\\?\` or `\\.\` prefix is refused; network entries of `PATH` are not searched (the error says to set `phv.path`); a plain share path in the setting is still accepted |
| R21-21 | The dry run of `phv jobs adopt` listed every job with no count per name and no filter, so the check the spec asks for was impractical | The dry run prints a count per name, and `--name` restricts the adoption to the named scripts |
| R21-22 | While the shared rate-limit store is unreachable, `requests_per_minute` counts per process (N processes allow N times the limit); `rate_limit()` fails closed | Documented below |
| R21-23 | Two callbacks of one browser with the same sign-in state in flight at once both exchanged the code; the pending list is saved when the request ends, and providers refuse the second use of a code | Documented below |
| R21-24 | A stored pending list over 64 KB (only a writer of the store can make one) gave a 500 at the callback instead of `invalid_state`, and was decoded outside the script's limits | The list is decoded within the script's limits, and the oldest entries are dropped past the caps, so the callback answers `invalid_state` |
| R21-25 | Measured, not changed: eight 8,192-bit RSA keys verify a token without `kid` in 2.2 ms (7.0 ms with the largest exponent); a password hash at the verifier's caps takes 220 ms and 64 MiB | Holds |
| R21-26 | The compiled-page cache is bounded by the number of page files only: 600 small pages held 200 MB | Documented below |
| R21-27 | The Redis session store read a record with invalid UTF-8 lossily where the file store refuses it | Refused as damaged, like the file store |

### Residual Risks

- mimalloc's own overhead (size classes, per-thread retention) is outside the counted memory, by a small factor, as
  the system allocator's was; on Windows the committed memory grows by about 70 MB per process for mimalloc's
  arenas, while the resident memory does not.
- The per-thread memory counter is exact only while every cross-thread free is paired with `charge_thread`.
- While the shared rate-limit store is unreachable, `requests_per_minute` holds per process.
- A browser that sends two callbacks with the same sign-in state at once exchanges the code twice; the provider
  refuses the second.
- The compiled-page cache grows with the number of page files (about 330 KB per small page).
- Git finds the history of the files of `netdb/` across the split with `-M10%`.
- §27's residual risks remain.

### Verification

The PoCs were run again against the fixed build (before → after):
- a flood of 102,000 limited windows against an older limit: lifted → still limited, the flood's newest windows gone;
- a 16 MB POST on a kept connection reset after 64 KB (Linux): three request heads over two connections and 200 →
  one head and `NetworkError`;
- a Redis reply 300 ms late: the next `get` answered `EXTRA` → `fresh` on a new connection;
- two pages differing in one float constant (its bits spelling `Str("a")`): page b printed page a's float → `aa`;
- a partial holding `<title>` rendered inside `<select>`: accepted → refused at run time; a partial opening a
  `<select>`: accepted → refused at check;
- `phv jobs adopt --yes` over a 30 ms store (PostgreSQL, MySQL): an error after 12 seconds and nothing moved →
  50,003 jobs in 26 and 47 seconds; locally 2.3 and 12.4 seconds; the Redis move's worst stall of another client
  751 → 65 ms;
- a callback without `iss` from a provider that advertises it: `ok` → `invalid_issuer`; an RSA key with an even
  modulus: accepted → refused (three lines of 56 changed, the rest identical);
- 2,400 reads of a session during 1,200 saves (Windows, file store): 165–201 saw no session → 0; a record with
  `last` at the largest value: 500 on every request → no session and the file removed; a Redis record with invalid
  UTF-8: read lossily → refused;
- four pages calling their own server on four workers: every page and a bystander stalled 10 seconds → the server
  refuses to start with the service named;
- `phv.path` as `phv.cmd:s.exe` or `\\?\...`: resolved → refused; the workspace spelt as an administrative share
  in `PATH`: searched → skipped.

The differential oracle of the tracker rewrite was run again with the fixed binary: of 177,208 templates, 132
differ, every one with a self-closed `<script/>` or `<style/>` read as foreign content (84 now accepted, 48 still
refused by their other errors); the reviewer's harness of session records differs only in the three fixed cases.
The benchmarks of `bench/`, run back to back with the binary of 34d8272 under the same load (`bench/results/34d8272-loaded.json`, `audit18-loaded.json`), are within noise: the micro-benchmarks 0.93–1.04, the server's pages 0.98–1.09 in requests per second; the editor's rows moved with the load of a test suite running alongside.

### Regression tests

Regression tests are named after what they check, in each crate's tests and in `tests/cases`, `tests/db` and
`tests/http` (`err_html_in_select`, `err_template_in_select`, `post_reset`, `redis_late_reply`). Some of the others:
- `ratelimit.rs`: `a_flood_of_limited_windows_does_not_lift_an_older_limit`;
- `http.rs`: `a_post_whose_write_fails_on_a_kept_connection_is_not_repeated`;
- `redisclient.rs`: `bytes_waiting_before_a_command_break_the_connection`; `databases.rs`:
  `a_late_redis_reply_is_not_read_as_the_next_commands`;
- `jit.rs`: `programs_differing_in_a_float_constants_bits_do_not_share_code`; `phv_jit`:
  `unit_keys_tell_constants_apart`;
- `htmlctx.rs`: `select_is_a_context_and_not_the_top_level`, `self_closed_script_in_foreign_content_opens_no_text`;
- `jobs.rs` (CLI): `adopt_moves_many_jobs_through_a_slow_store_with_postgres` and `_with_mysql`; `netdb`:
  `adopt_reports_a_refused_row_with_the_servers_message`, `redis_adopt_moves_a_large_shared_store_in_pages`;
- `oauth.rs`: `answers_without_iss_are_refused_when_the_provider_always_sends_it`,
  `an_oversized_pending_list_in_the_store_finishes_the_callback`; `jwt.rs`: `rsa_keys_with_an_even_modulus_are_refused`;
- `session.rs`: `a_read_during_another_requests_save_sees_the_session`, `a_record_with_times_in_the_future_is_damaged`,
  `a_redis_record_that_is_not_utf8_is_refused`; `selfcall.rs`: `a_service_pointing_at_this_server_is_refused`;
- `editors/vscode/test/phv-path.test.js`: the alternate-data-stream, device-path and network cases.

All tests pass on Windows and on Linux, run at the same time against one PostgreSQL 17, MySQL 8.4 and Redis 7, and
with clippy (`-D warnings`) and rustfmt.

## 29. Nineteenth Assessment: the Whole Implementation (before 1.3.0)

Date: 2026-11-09. Scope: everything at 8d1a290: the fixes of §28 and all of 1.3, none of which had been assessed
before. 1.3 brought a smaller syntax tree, a parser of its own for builtin signatures, a loader that keeps the text of
unchanged files, `phv check` over several files, a pool of page threads in place of the runtime's blocking pool,
batched access-log writes, the JIT compiling objects, methods and `echo`; generators (`yield`, `yield from`,
`Generator<K, V>`), traits flattened into each class at compile time, `std.i18n` with catalogs checked at compile
time, `std.image` decoding PNG, JPEG, GIF and WebP with the `image` crate family, and `std.orm`. Every earlier fix was
attacked again.

Nine reviewers each took one area: the web server; the standard library, its clients and `std.orm`; the syntax, type
checker, compiler, VM, JIT and loader; templates and the tracker; the tools and their supply chain (with the new
image crates); background jobs and the stores; sign-in and tokens; the running system as a whole; and the two new
modules that take untrusted input, `std.image` and `std.i18n`. They attacked a release build locally, with fake
identity providers, Redis and HTTP servers on their own ports, the three databases in Docker, Chrome 157 and parse5
for templates, and Linux builds in Docker where a question depended on the platform. Each finding was confirmed with
a PoC. The image decoders were fuzzed in process (2.64 million inputs over every format, with overflow checks), the
generators by 5,500 random programs run by the interpreter and the JIT, and the formatter by comparing the syntax
trees of 298 files before and after formatting. Six groups then fixed every finding, each with regression tests.

### Summary of Results

| Severity | Count | Fix |
|---|---:|---|
| Critical | 1 | Fixed |
| High | 2 | Fixed |
| Medium | 8 | Fixed |
| Low | 13 | Fixed |
| Info | 11 | Fixed or documented |

### Critical

| # | Finding | Fix |
|---|---|---|
| R22-1 | Reading the header of a WebP (`Image::fromUpload()`, `open()`, `fromBase64()`) asked the decoder for the EXIF orientation, and image-webp allocated the size the EXIF chunk declares, with no limit, before reading it: an 82-byte upload committed 4 GB under a 128 MB limit, and a server that could not commit it aborted (on Windows, where memory is committed at once; on Linux under strict overcommit or an address-space limit) | Reading a header asks no decoder for metadata: the WebP chunks are walked in place and the orientation is read from the bytes that are there (PNG `eXIf` likewise, JPEG through the decoder's own bounded header parse); the upload is now read in about 4 ms within 13 MB |

### High

| # | Finding | Fix |
|---|---|---|
| R22-2 | `password_verify` checked the Argon2 cost caps on the first occurrence of each parameter, while the argon2 crate uses the last: a stored hash `m=19456,t=2,p=1,m=4294967295` made one login allocate 4 TiB and abort `phv serve`, and a repeated `t` ran for as long as it said (9.3 seconds under a 2-second limit); only a planted hash, the case the caps exist for | A hash that names a parameter twice or names one argon2 does not know is refused, and the caps are checked on the parameters argon2 will use; the planted hashes answer "wrong password" in about 13 ms |
| R22-3 | A trait reached through several `use` paths was flattened once per path, and neither the member limit nor the copied-code limit counted empty traits: a 1.3 KB diamond of depth 24 took about a minute in `phv check`, and depth 40 hung `phv check` and the language server, which stopped answering for the whole workspace (opening a cloned repository was enough) | The flattened members of a trait are kept per trait and substitution within one check, and every flatten counts against the member limit; the 40-level diamond checks in 0.2 seconds, and the language server answers at once |

### Medium

| # | Finding | Fix |
|---|---|---|
| R22-4 | On PostgreSQL, a `COMMIT` of a transaction aborted by a caught statement error is answered with the tag `ROLLBACK` and no error: `transaction()` returned normally, `enqueue()` inside it returned ids of jobs that never existed, and `std.orm` kept the uncommitted values, so the next `update()` skipped columns (a lost update). MySQL deadlocks caught inside a transaction left the statements after them running in autocommit | The outermost commit is sent as `SELECT 1; COMMIT`, so an aborted transaction answers with an error; a transaction the database ended (aborted, rolled back by a deadlock or a lock-wait timeout with `innodb_rollback_on_timeout`, ended by a statement) refuses further statements and `enqueue()`, and every level then raises "transaction() did not commit"; Repos forget |
| R22-5 | A failed `COMMIT` or `RELEASE SAVEPOINT` left the transaction open or its depth wrong: on SQLite a commit refused by a reader left writes the script believed committed, which were rolled back when the connection was returned (the script saw three rows, the file held none); on PostgreSQL and MySQL a nested failure left the depth at 2, so a later `transaction()` that threw kept its writes | One routine for the three databases: a commit or rollback sets the depth to the level below before it runs; a failed commit rolls its level back and rethrows; at depth 0 a connection still in a transaction is closed (SQLite) or marked broken |
| R22-6 | `#[Id]` and `#[Column]` on a property a trait brings into an entity were ignored: the key and column names fell back to the property names, and `delete()` of one entity deleted every row sharing the wrong key | Annotations of trait properties are recorded and checked as the class's own; a property declared by both must agree |
| R22-7 | A generator closed by its loop could reach `yield` again from its own code (a `continue` in a `finally` back to the loop, or a `finally` that throws into a `catch` that yields), and the VM stopped with an internal error: a page doing it answered 500 every time | The `yield` throws a catchable `LogicException` in the generator, which goes on closing (PHP raises an `Error` there) |
| R22-8 | `Html` inserted inside `<svg>` or `<math>` was not checked for markup that changes the foreign structure for what follows: a partial such as `<p>…</p>`, `</svg>` or an open `<foreignObject>` took the browser out of SVG while the tracker still read SVG, so a following self-closed `<script/>` got the page's nonce and a visitor's value after it ran as script in Chrome (and as CSS after `<style/>`) | The run-time check of `Html` in foreign content knows the element it lands in and reads the `Html` with the tracker's own rules: a breakout tag, an end tag of an element it did not open, an element or integration point it leaves open, or text ending inside a tag or comment is refused |
| R22-9 | `std.image` told progressive JPEGs from baseline ones by its own walk of the segments, which stopped at the first stray byte between them, while the decoder skips such bytes: one byte took a progressive JPEG past the scan cap and the coefficient memory reserve (477 MB under 128 MB; eight concurrent uploads, 3.8 GB; 100 scans decoded in 6.4 seconds without interruption) | The kind of JPEG and its scan count come from the decoder's own header parse, and the decoder enforces the scan cap itself; the coefficients are reserved per component |
| R22-10 | Decoders were given the memory left as their allocation limit without subtracting what was reserved for the image, and the PNG decoder inflated an `iCCP` profile even when only the header was read (296 MB under 128 MB from a 2 MB file; 4.2 GB under 2 GB); a GIF frame larger than its logical screen was not counted | Decoders get the memory left after the reservation, the header pass a fixed 1 MB; PNG chunks are read forward without allocation and metadata chunks are skipped whole; a GIF frame larger than its screen runs into the limit |
| R22-11 | A VP8 or VP8L bitstream (or an animation frame) larger than its WebP canvas was decoded at its own size before the sizes were compared: a file reading as 16×16 took 470 MB and 2.5 seconds past the time limit | The first image chunk's header is read with the canvas, and a size that differs is refused before decoding |

### Low

| # | Finding | Fix |
|---|---|---|
| R22-12 | On MySQL, `std.orm`'s `insert()` read a generated key from `lastInsertId()`, which is the AUTO_INCREMENT value of any column, or 0: the entity took another row's key, and `update()` then rewrote that row | A generated key is taken only when the key column is the table's AUTO_INCREMENT column (looked up once per Repo) and the id is not 0; otherwise `insert()` raises |
| R22-13 | A Repo's snapshot did not see a `ROLLBACK` or `ROLLBACK TO SAVEPOINT` sent as SQL, nor writes to the same entity through another Repo, so `update()` skipped a column that had changed | Rollback statements make every Repo forget, and a write through any Repo makes the other Repos of that table forget; writes with the script's own SQL are not seen, as the specification says |
| R22-14 | A provider configured without `issuer` accepted an `iss` naming another configured provider's issuer on a shared callback page, so its token endpoint received the other provider's code and PKCE verifier (an RFC 9207 mix-up) | Such an `iss` is refused with `invalid_issuer` before the code is exchanged |
| R22-15 | The refusal of `XA PREPARE` and `PREPARE TRANSACTION` (R20-7) was bypassed by comments: on MySQL a versioned comment above the server's version (`/*!99999 mk1 */ XA PREPARE`), which the server skips; on PostgreSQL `/*! … */`, which PostgreSQL reads as a plain comment. A prepared MySQL transaction outlived the script and held its row locks | On PostgreSQL `/*!` is an ordinary comment; on MySQL a statement is read once with versioned comments skipped and once per version they name, and refused if any reading is refused (at most 16 versions) |
| R22-16 | File session store: R21-9's retry polled the directory lock up to 40 times per name, so a forged or expired cookie held a worker about 180 ms while other requests saved sessions | A read that finds no session takes its session's lock once and reads both names under it; a forged cookie costs 2–3 ms under the same load, and R21-9's guarantee holds |
| R22-17 | `phv fmt` put a trailing comma into `yield from (…)` when it wrapped the line, so the formatted file no longer compiled; the parser's comment said a function named `from` kept its meaning after `yield`, which it did not | The formatter leaves the comma out; `yield from` is a keyword pair, as in PHP, and `yield (from(…))` calls such a function |
| R22-18 | The `<select>` state of R19-26 and R21-5 could be lost four more ways, so with a parser of the old rules a value read as `<title>` text became markup: a loop body read once with the state before the loop, a `</select>` inside a `<template>`, statements in a select whose functions wrote their own output (or ended inside the select), and `yield` inside a select | A block must end with a `<select>` open exactly when it began with one; a `<template>` inside a `<select>` is refused; functions called while a `<select>` is open may not write output (return `Html` instead); `yield` there is refused |
| R22-19 | R21-11's refusal of services pointing at the server itself did not check `[oauth.*]` URLs, nor the unspecified address (`0.0.0.0`, `[::]`, which Linux connects through the loopback): a sign-in against the site's own address held its worker 10 seconds and stalled a bystander 9.5 seconds | Those URLs are checked at startup, the unspecified address counts as this server, and a page's connection that would reach the server's own listening address is refused when it is made (discovered endpoints, later DNS answers) |
| R22-20 | A page whose locale `std.i18n` chose from `Accept-Language` was sent without `Vary: Accept-Language`, so a shared cache served the first visitor's language to everyone | The header is added when the locale came from the request (and to a streamed head of an `[i18n]` site whose locale is not chosen yet) |
| R22-21 | The admin port took 64 connections with no limit per address and a 5-second header timeout: one client holding them delayed `/health` 5 seconds, enough to fail a container's liveness probe | Four connections per address (an IPv6 /64 counts as one), a 1-second header timeout, and a new connection over a limit closes the oldest connection that has sent nothing; `/health` answered within 0.03 seconds under the same flood |
| R22-22 | An override could narrow visibility (a private method overriding a public one, also through a trait's `as private`), and the private body was then callable from anywhere through the parent type; older than 1.3 | An override narrower than the method it overrides is refused, trait copies after `as` included |
| R22-23 | Encoding reserved 1.25 times the pixels while the PNG and lossless WebP encoders use about four times: a 16-megapixel image was written at 341 MB under 128 MB | What the encoders use is reserved before encoding (measured: PNG 3.7×, WebP 3.9×, JPEG at quality 100 2.3×) |
| R22-24 | The file session store made every save of every session under one directory lock: about 250 saves a second per server on Windows, and pages without sessions waited for workers (480 of 40,000 requests answered 503); 1.2.1 behaved the same | 64 locks chosen by the session's hash, each with a lock file of its own; saves of one session still run one at a time; the sweep deletes each file under its own lock. The file system is now the limit, and the specification points busy sites to Redis |

### Info

| # | Finding | Fix |
|---|---|---|
| R22-25 | R21-23 was wider than documented: a sign-in started in a second tab wrote back the pending list it had read before the first tab's callback consumed its state, so that callback URL worked once more (the provider refuses the second code exchange) | Each server process remembers the digests of the states it used for their lifetime and refuses them; processes do not share that list, as the specification says |
| R22-26 | A damaged Redis session record (invalid UTF-8, a bad header) made `/ready` answer 503 and was never removed | Read as a long-expired session and removed, not counted as a store failure; PostgreSQL data that is not UTF-8 likewise |
| R22-27 | The Redis `phv jobs adopt` paged the shared store by rank, so jobs another client removed ahead of the cursor made it skip some of the site's jobs (left in place, not lost) | Pages start after a (score, id) cursor; after `--yes` what is left of the site's jobs is printed per name |
| R22-28 | On MySQL, `phv jobs adopt` matched script names with the column's case- and accent-insensitive collation | Names are compared in binary, and every store keeps only exact matches |
| R22-29 | The release archives and the Docker image carried no notices of the statically linked third-party crates (BSD-3-Clause among them since `std.image`) | `packaging/licenses.py` collects every crate's licence files from `cargo metadata`; each archive and the image carry `THIRD-PARTY-LICENSES.txt`, and CI runs the script |
| R22-30 | Bytes from an image's `send()` or `response()->file()` could be retyped as `text/html` afterwards, where echoed output is refused | They count as written output in the Content-Type rule |
| R22-31 | The server's check that a page's sources are unchanged compared write time and size only, so a same-size rewrite in place with the write time set back was served stale (by a writer of the project) | Windows now reads the NTFS change time, as Unix the inode change time; the specification names the file systems without one |
| R22-32 | `t("key", [])` passed `phv check` and failed at run time for a message with placeholders | An empty list literal is checked as an empty map |
| R22-33 | A decoder panic caught by `std.image` was still printed to standard error as raw lines | Caught panics inside the decoders are not printed; other panics are |
| R22-34 | Encoding at the caps took longer than the specification said (JPEG at quality 100 of 48 megapixels: 2.5–2.8 seconds against 2.1) | The specification gives the measured times: the uninterruptible overshoot is about 3 seconds, 4 on a busy machine |
| R22-35 | The memory session store unwrapped its lock, so a panic while it was held would have failed every later request (page panics are now caught on long-lived threads; R22-7 showed one) | The lock is taken through a poisoned state, and the byte count saturates |

### Residual Risks

- A MySQL statement that commits implicitly at the outermost level of `transaction()` is not detected (the client
  does not see the server's transaction flag); a PostgreSQL `COMMIT` that silently rolls back the script's own
  `BEGIN` is not raised, though `std.orm` forgets on it.
- While processes of 1.2 and 1.3 share a session directory (a rolling upgrade), a logout in one can be undone by a
  save in the other: they lock different files. Every process sharing a directory must be 1.3 or later.
- The list of used sign-in states is per process.
- A client opening thousands of connections a second from one address can have one of its own slow probes on the
  admin port closed; probes from other addresses are not affected.
- The decoders and encoders of `std.image` cannot be interrupted: a call at the caps runs up to about 3 seconds past
  the time limit (4 on a busy machine).
- A file system without a change time (FAT, exFAT, some network shares) cannot tell a rewrite with the old write
  time; neither can a program that sets the change time back through the native interface.
- §28's residual risks remain.

### Verification

The PoCs were run again against the fixed release build (before → after):
- the 82-byte WebP whose EXIF chunk declares 0xFFFFFF00 bytes: 4,121 MB committed under a 128 MB limit, and a server
  allowed 2 GB aborted → 13 MB, and the upload answered in 4 ms;
- a planted Argon2 hash with a second `m=4294967295`: the server aborted at the first login; a second `t=40` or
  `m=1048576`: 260 ms and 1,171 ms → "wrong password" in 0 ms (the hash is refused before hashing);
- a diamond of empty traits, depth 24 and 40: over 30 seconds (stopped) → 0.09 seconds each; the language server with
  the depth-40 file open: no answer in 89 seconds → the hover answered at once;
- a caught statement error inside a PostgreSQL `transaction()`: returned normally and the row kept the old name while
  the entity held the new one → `DatabaseError: transaction() did not commit`; a SQLite commit refused by a reader:
  the script saw three rows, the file held none → the commit throws, and the two rows the script saw are on disk;
- a trait's `#[Id]` and `#[Column("email_address")]`: `email_address` NULL and `delete()` left no row → the column
  written and two rows left;
- a generator whose `finally` continues back to its `yield`: the VM's internal error, 500 on every request → `got 1`,
  `after`; the throw-and-catch shape → an uncaught `LogicException` the script can catch;
- `Html` breaking out of `<svg>` before a self-closed `<script/>` (Chrome 157 with the CSP): the visitor's script set
  the title, and the CSS variant fetched `/mk1css` → 500 with the reason, nothing ran or was fetched; the four new
  `<select>` shapes: parse5 built a `javascript:` link in each → refused at check or at run time;
- a progressive JPEG behind one stray byte: 477 MB under 128 MB, eight uploads 3,844 MB → the memory error before
  decoding at 1 MB, eight uploads 172 MB for the whole server; 100 scans: 6.4 seconds → refused at the header;
- a 2 MB PNG with a compressed colour profile, header only: 296 MB → 1 MB; a VP8 bitstream of 16383×16383 in a 16×16
  canvas: 472 MB and 2.3 seconds → refused at the header in 0.04 seconds;
- a 16-megapixel image written as PNG and WebP under 128 MB: 341 and 338 MB → the memory error before encoding;
- MySQL `/*!99999 mk1 */ XA PREPARE`: the prepared transaction outlived the script → refused; `/*! mk1 */ PREPARE
  TRANSACTION` on PostgreSQL: reached the server → refused;
- a sign-in against the site's own `issuer`: logins 500 after 10 seconds and a bystander stalled 9.5 seconds → the
  server refuses to start; a callback naming another provider's `iss`: the code went to the plain provider →
  `invalid_issuer`; the callback URL used again after another tab's start: `ok` → `invalid_state`;
- 64 silent connections to the admin port from one client: `/health` 4.8 seconds at worst → 0.03 seconds;
- the file session store with 256 visitors saving: 598 → 775 saves a second back to back on the same machine; a forged
  cookie while two clients save: 187 ms → 1.6 ms; a damaged Redis record: `/ready` 503 and the key kept → 200 and the
  key removed;
- the Redis `phv jobs adopt` while another client took 477 jobs: three of the site's jobs left in one run of three →
  none in six runs.

The benchmarks of `bench/`, run back to back with the binary of 8d1a290 (`bench/results/8d1a290-loaded.json`,
`audit19-loaded.json`), are within noise: the micro-benchmarks 0.94–1.08 (`examples/bench` alternating seven times:
0.95–1.08), the server's pages 0.97–0.99 in requests per second at their best run and their medians alike, but for the
guestbook's best run (0.87 against one outlying run of the old binary; the medians 0.99), the editor's rows 0.77–1.06.
Appending to a string in a loop had become 11–15% slower with no change on its path: the whole-program optimization
inlined it differently after changes elsewhere (each group's changes alone moved it 2–7%, a changed message string
alone did not); the append is now always inlined, and the row is back at 1.00–1.01 (1.07 with the JIT).

All tests pass on Windows and on Linux (Docker), against PostgreSQL 17, MySQL 8.4 and Redis 7, with clippy
(`-D warnings`) and rustfmt, but one: run right after the command-line `jobs` tests (as `cargo test --workspace` runs
them) against databases reached through Docker Desktop's port forwarding, a MySQL test of keyed jobs waits past its
limits. 8d1a290 fails the same way, in another test of keyed jobs; each passes when run alone, with its own file, or
after a pause of 90 seconds.

After the fixes were pushed, CI found two more things. Its dependency audit reported RUSTSEC-2026-0459 (SQLite
CVE-2026-119375, through `libsqlite3-sys` 0.35, published 2026-10-30): rusqlite is now 0.41.0 with `libsqlite3-sys`
0.39.0 and SQLite 3.54.0, and the cost model's table of SQLite's steps was checked against 3.54.0 as its pin asks
(`IFindKey`, an index seek of `integrity_check`, weighs a comparison; `IfEmpty` is free; `IfNotOpen` is gone; the
changed operands of `IdxDelete`, `Int64`, `NullRow` and the hints change no weight), and every one of its 191 steps is
in the table. And the new session locks of R22-24 let a session saved without a pause keep a reader under the same
lock waiting for many saves (375 ms on Linux), as a plain mutex lets the thread that just let it go take it again:
each lock now hands itself over in the order it was asked for. And a MySQL connection kept
the socket timeout of the script that made it (its time limit and ten seconds), which the driver cannot change: a
connection made under a 2-second limit, taken from the pool by a script with 60 seconds, gave up on a 14-second
statement after 12 ("the time limit was reached"); an idle connection is now given only to a script whose limits it
fits, and the others make a new one.

### Regression tests

Regression tests are named after what they check, in each crate's tests and in `tests/cases` and `tests/db`
(`err_html_breakout_in_svg`, `err_select_loop`, `err_yield_in_select`, `generators_close_yield`,
`err_override_visibility`, `orm_trait_columns`, `orm_forgets_other_writes`, `sqlite_transaction_ended`,
`postgres_tx_ended`, `mysql_tx_ended`, `mysql_orm_keys`). Some of the others:
- `image.rs`: `a_webp_exif_chunk_declaring_a_gigabyte_is_not_allocated`,
  `a_stray_byte_does_not_hide_a_progressive_jpeg`, `an_icc_profile_bomb_is_not_inflated`,
  `a_webp_bitstream_of_another_size_than_its_canvas_is_refused`, `encoding_counts_what_the_encoders_take`;
- `sys.rs`: `password_verify_refuses_repeated_or_unknown_argon2_parameters`;
- `compile_cost.rs`: `diamonds_of_traits_check_at_once`; `lsp.rs`: `answers_with_a_diamond_of_traits_open`;
- `databases.rs`: `transactions_the_database_ended_are_not_reported_as_committed`; `sqlite_pool.rs`:
  `a_commit_that_fails_ends_the_transaction`; `netdb`: `prepared_transactions_are_refused`,
  `redis_adopt_skips_nothing_when_others_take_jobs_meanwhile`;
- `htmlctx.rs`: `html_inserted_in_foreign_content_keeps_its_structure`, `a_block_keeps_the_select_state`;
- `oauth.rs`: `an_iss_of_another_provider_is_refused_by_a_plain_provider`,
  `a_state_written_back_by_another_request_is_not_used_twice`; `selfcall.rs`:
  `the_unspecified_address_and_oauth_urls_pointing_at_this_server_are_refused`,
  `a_discovered_endpoint_on_this_server_is_refused_when_the_page_connects`;
- `http.rs`: `a_locale_chosen_from_accept_language_varies_by_it`, `a_sent_file_or_image_cannot_be_retyped_as_html`;
  `admin.rs`: `silent_connections_do_not_keep_health_waiting`;
- `session.rs`: `saves_of_different_sessions_do_not_wait_for_each_other`, `a_forged_cookie_does_not_wait_for_saves`,
  `a_panic_holding_the_memory_store_does_not_break_it`;
- `format.rs`: `yield_from_takes_no_trailing_comma`; `safefs.rs`: `a_rewrite_with_the_old_write_time_changes_the_stamp`.

## 30. Independent security review of the v1.3.2 candidate

Reviewed on 2026-11-10. The target was `319b9bf78ea89a8e9add064f4178ca8aa0f28328`. This review traced external
inputs, privileges, persistent state and resource limits across all nine crates and the distribution and deployment paths.

| Area | Paths reviewed |
| --- | --- |
| Syntax, types and compiler | Token and nesting limits, HTML contexts, inheritance and property slots, exception and generator instructions |
| VM and JIT | Typed property loads and reuse, native helpers, delegation frames and handlers, time, memory and stack limits |
| Standard library | OAuth and JWT, outbound HTTP and WebSocket, filesystem boundaries, SQLite and shared DB sessions, image decoding, memory estimates and conversion slots |
| Driver and HTTP server | Source loading and bundles, routing and static files, body limits, CSRF and CORS, admin port and session-store failures |
| CLI and VS Code | Git configuration for package fetches, LSP input and analysis cache, DAP startup and executable selection |
| CI, distribution, website and samples | Workflow triggers and privileges, artifacts and checksums, website HTML generation and headers, sample sessions and runtime identities |

| ID | Severity | Finding and disposition |
| --- | --- | --- |
| RC32-01 | Medium | The OAuth claim and the session values are stored separately. Atomic claims limit exchanges during normal operation, and write failures stop the callback before exchange. Recovery that preserves an older session but loses an acknowledged claim voids the one-use guarantee. Redis `noeviction` does not provide durability, and the file store replaces the claim file without syncing it first. The specification now requires operators to invalidate affected sessions before resuming callbacks if recovery may have lost claims. [OAuth](../crates/phv_std/src/oauth.rs#L474) · [Shared stores](../crates/phv_std/src/netdb/sessions.rs#L236) · [File store](../crates/phv_server/src/session.rs#L623) |
| RC32-02 | Medium | The VS Code extension treated some dot-prefixed child directories as outside the workspace. This affected the workspace exclusion in the default PATH search. The check now treats parent traversal as a path component. Windows and Unix path classification tests cover the change. [Fix](../editors/vscode/phv-path.js#L16) |
| RC32-03 | Medium | A manually dispatched website workflow could deploy from its selected ref. The deploy job requires `main` and uses the `website-production` environment, whose GitHub deployment policy also allows only `main`. Adding the environment credentials and removing the legacy repository credentials remain operational steps. [Fix](../.github/workflows/website.yml#L49) |
| RC32-04 | Low | The draft-release command could create a tag if the remote tag was missing. It now requires an existing tag. Tag creation, update and deletion restrictions and prepublication draft verification still apply. [Fix](../.github/workflows/release.yml#L195) |

Existing evidence for `319b9bf` includes eight passing CI jobs and nine passing distribution build jobs. RustSec
and the production npm dependency audits for the website and VS Code extension reported zero findings. The grammar
and path tests passed after the extension fix. The limits on interrupting a codec call and on MySQL implicit commits
remain as previously documented.

In local Redis 7, PostgreSQL 17 and MySQL 8.4 environments, a test claim written through PHV's store API survived
both restart after an unclean stop and failover to a replica confirmed to hold the claim. A separate process was
refused when registering the same marker after each recovery, and the promoted replicas accepted a new marker.
The test did not count token exchanges through OAuth callbacks or cover failover with lost, unreplicated claims.

New attack reproduction and production deployment verification remain outstanding. The full-audit gate in the
[release procedure](releasing.md) remains open.

## 31. Twentieth Assessment: the Whole Implementation (before 1.3.2)

Date: 2026-11-11. Scope: everything at 6bc5a8c, the 1.3.2 candidate: 1.3.1 (released without such an assessment:
the OAuth used-state records kept in the shared session stores, image conversion slots, changes to the page pool) and
1.3.2 (the JIT reusing checked property reads in loops and updating unshared list elements in place, `yield from`
forwarding and the exception-handler buffer reused across suspension, two experimental generator settings, string
appends borrowing their source, `Html` sharing its text with plain strings, and §30's workflow fixes), none of which
had been attacked. Every earlier fix was attacked again.

Ten reviewers each took one area: the web server; the standard library, its clients and `std.orm`; the JIT and the
VM's data operations; generators and the VM's exception handling; templates and the tracker; the tools, CI and the
supply chain; background jobs and the stores; sign-in and tokens; the running system as a whole; and `std.image` and
`std.i18n`. They attacked a release build locally as in §29, with fake identity providers and the three databases in
Docker, Chrome 157 and parse5 for templates. Each finding was confirmed with a PoC; four reviewers found R23-2
independently. The JIT was compared with the interpreter on about 35,000 random programs (three ways each), the
generators on about 24,200 programs under up to ten configurations (the off switches, the experimental settings,
and 1.3.0), the image decoders fuzzed with about 3.96 million inputs, and the running server checked over 600,000
mixed requests with every body compared: none of 1.3.2's own changes gave a wrong result, a crash or a leak between
requests. Six groups then fixed every finding, each with regression tests, and a further reviewer attacked the code
the fixes added.

### Summary of Results

| Severity | Count | Fix |
|---|---:|---|
| Critical | 2 | Fixed (one in the first fix of the other, found before the release) |
| High | 2 | Fixed |
| Medium | 5 | Fixed |
| Low | 11 | Fixed |
| Info | 15 | Fixed or documented |

### Critical

| # | Finding | Fix |
|---|---|---|
| R23-1 | A lossless WebP (VP8L) may declare up to 65,536 prefix-code groups through its 16-bit meta codes, five Huffman tables each, none depending on the canvas and none counted by image-webp or by `std.image`: a 1×1 file of 3.8 MB committed 1,315 MB under a 128 MB limit, one of 7.6 MB (inside the 8 MB upload body) 4.5 GB and 7.6 seconds past a 1-second limit, and one upload aborted a server whose memory was capped at 3 GB; also in 1.3.0 | Before image-webp decodes, `std.image` reads the lossless bitstream itself (`imaging/vp8l.rs`: the transforms, the entropy image with its backward references and color cache, every code's lengths) and counts exactly the tables and trees image-webp will build, for the main image, a lossless alpha channel and the first animation frame; past the memory left it stops with the memory error. The 7.6 MB file now ends at 13 MB in 0.06 seconds |
| R23-34 | Found by the re-attack of the fixes: R23-1's fix counted the bitstreams of an ANMF frame only in an animation, but image-webp copies the first frame's sub-chunks into its chunk map for any extended file and decodes a VP8L or lossless ALPH found there before the top-level VP8: a file that is not an animation, with a VP8 chunk and an ANMF holding the R23-1 bitstream, used 1,264 MB (the 8 MB one 4,475 MB) under a 128 MB limit with the first fix | Every VP8L and every lossless ALPH in the file is counted, top level and the first two sub-chunks of every frame, each as long as it declares, whatever the flags say; the largest bounds the decode. The three files now end with the memory error at 1–6 MB |

### High

| # | Finding | Fix |
|---|---|---|
| R23-2 | The used-state records of R22-25 (1.3.1) lived in 64 shards per site, 512 records each, shared by every session of a shard, and a full shard refused new claims; a claim was made before the code was looked at. One anonymous visitor filled a shard with about 512 pairs of `/login` and `/callback?state=…` without a code, and 64 sessions blocked sign-in for the whole site for about 10.5 minutes, repeatably (memory store 9.8 seconds, Redis 76–91, PostgreSQL, the file store; MySQL by the same code); 1.3.0 was not affected | Each state is a record of its own, keyed by a digest of the site, provider and state and expiring by the store's clock (Redis `SET NX` with an expiry, a SQL table swept with the sessions, a file per state, the memory store's session entry); one session holds at most 16 live claims, and only that session is refused past them; a callback with neither a code nor an error claims nothing. After the same fills, every other visitor signed in, in every store |
| R23-3 | `PRAGMA busy_timeout` was allowed, and SQLite waits for a lock outside the steps the progress handler sees: a script held its worker for as long as it set (up to 24.8 days); two requests on a server with two workers were never answered and every other page got 503. The default 5-second wait also outran short time limits; also in 1.3.0 | The pragma is refused; a busy handler waits with back-off and gives up after 5 seconds or at the script's deadline, whichever is first. The PoC now fails at once, and a write blocked under a 1-second limit ends in 1.1 seconds |

### Medium

| # | Finding | Fix |
|---|---|---|
| R23-4 | Each `as` rule of a `use` scanned every member of the used traits: 40,000 aliases (2.4 MB) took 13 seconds in `phv check`, 17 nested alias levels (7 MB) 44 seconds, and the language server stopped answering for the whole workspace; the deduplication of trait diagnostics was quadratic as well; also in 1.3.0 | An index per `use` makes each rule a lookup, rules count toward the member limit, and diagnostics are deduplicated through an index: 60,000 aliases check in 0.5 seconds, the 17 levels hit the member limit at once, and the language server answers in 0.24 seconds |
| R23-5 | An end tag inside `<svg>` or `<math>` that closes an HTML element opened before it (`</a>`, `</div>`, `</td>`, `</select>`…) was not refused: browsers leave the SVG while the tracker read on as SVG, so a following self-closed `<script/>` got the page's nonce and a visitor's value ran in Chrome 157; `<svg>` inside `<select>` did the same with older parsers; also in 1.3.0 | Inside SVG and MathML an end tag must close something opened inside them (or be one no browser acts on there); `<svg>` and `<math>` are refused while a `<select>` is open, also in inserted `Html` |
| R23-6 | HTML inside an integration point was read as if always "in body", so table parts (`<td>`, `<tr>`, `<table>`…) inside `<foreignObject>`, `<desc>` or `<mi>` popped the SVG in a table, or were ignored by browsers but pushed by the tracker: a way around R22-8 for inserted `Html`, and in the page | The start tags of table and document parts are refused inside SVG and MathML, in the page and in inserted `Html`; the reviewer's fuzzers flag 0 templates, against 169 |
| R23-7 | A sequential JPEG whose first scan lacks components keeps every coefficient in the decoder, unreserved: 162 bytes at 8000×5000 used 369 MB (449 MB as CMYK) under a 128 MB limit; also in 1.3.0 | The decoder's own header parse is read up to the first scan, and the coefficients are reserved when it does not hold every component: 6 MB, the memory error before decoding |
| R23-8 | The image conversion slots of 1.3.1 threw at once when none was free and had no share per client: one client keeping as many conversions as slots running (each a 146 KB upload, held about 4 seconds, also after the client left) made 259 of 260 conversions of other visitors fail, where 1.3.0 answered all; ordinary bursts lost a few | A conversion waits for a slot in arrival order within the page's limits; one client address holds at most half of the slots; a waiting conversion whose client has gone takes none. Under the same attack, 160 of 160 conversions of another visitor succeeded |

### Low

| # | Finding | Fix |
|---|---|---|
| R23-9 | The used-state digest held the session id, so a page calling `session()->regenerate()` with a stale pending list made a used callback URL work again (two code exchanges) | The digest is of the site, provider and state only |
| R23-10 | A vertical tab is whitespace to MySQL, MariaDB and PostgreSQL but not to PHV's reading of SQL words: `XA\vPREPARE` and `PREPARE\vTRANSACTION` got past R22-15 (a prepared transaction outlived its script and held its rows), and `ROLLBACK\v` past R22-4 and R22-13 (a lost update) | One definition of SQL whitespace, vertical tab included, wherever words are read |
| R23-11 | std.orm forgot other Repos' writes per exact table name, while SQLite (and MySQL on Windows and macOS) ignore case | Table names are compared without case |
| R23-12 | `lastInsertId()` saturated at the largest `int`, so an unsigned AUTO_INCREMENT key past it gave the entity another row's key | A value that does not fit raises `DatabaseError` |
| R23-13 | A SQLite statement that rolled back outside `transaction()` (`INSERT OR ROLLBACK`, `RAISE(ROLLBACK)` in a trigger) was not seen by std.orm | Any statement that leaves a transaction it found open makes the Repos forget |
| R23-14 | On the admin port a connection that sent only empty lines counted as having sent a request, so 64 of them from 16 addresses made the server close every new connection, probes from other addresses included (70 of 74 closed): a way around R22-21 | A request head counts only after a byte other than CR or LF; under the same attack 74 of 74 probes were answered |
| R23-15 | Adding the server's own `Vary` name kept only the first `Vary` field, so a page's own (`Vary: Cookie`) could be dropped when compression, CORS or i18n added theirs | Every field is merged, without duplicates |
| R23-16 | A statement between `/` and `>` (`<svg …/<?phv … ?>>`) hid a self-closed `<svg/>` from the tracker | The slash is part of the tracker's state across pieces |
| R23-17 | The GIF decoder kept XMP and ICC application blocks outside the reservation (up to about 8 MB through an upload) | GIFs are read through a view that drops those blocks |
| R23-18 | The interpreter kept a returned call's registers, so a list, map or string passed to a helper stayed shared and the caller's next append copied it: a loop calling a helper and growing a collection took quadratic time (n = 40,000: 6–22 seconds; a 70-byte request held a worker for 15.8 seconds); also in 1.3.0 | A returning call lets go of the registers that may hold such values (worked out once per function); the same request takes 0.04 seconds |
| R23-19 | Native code left a value in a register's slot when it wrote a number there, so a stale temporary kept a list shared and the next write copied it (a compiled loop 4.5–10 seconds against 0.04 interpreted), defeating 1.3.2's in-place element writes; also in 1.3.0 | The JIT empties a slot that may hold such a value when it writes a number over it; the loop takes 0.04 seconds |

### Info

| # | Finding | Fix |
|---|---|---|
| R23-20 | A buffered page answering a request with a cookie got no `Cache-Control`, where the same page streamed got `private, no-store` | Both get `private, no-store` unless the page sets `Cache-Control`, as the specification said |
| R23-21 | The MySQL pool's fit (8094110) accepted any longer socket timeout, so a page could take a connection a job made with a one-hour limit | A connection serves limits from its own down to half of it |
| R23-22 | Connections `queryAsync()` made with the time left never fitted a page afterwards | They are made with the script's full limits |
| R23-23 | `packaging/licenses.py` read a crate's `license_file` outside the crate's folder | It resolves the path, requires it inside the crate and skips links |
| R23-24 | `bench/compare_php.py` could run `php` from the current folder on Windows | A bare name is looked up in the absolute folders of `PATH` only |
| R23-25 | libsqlite3-sys 0.39.1 (SQLite 3.54.1) and rusqlite 0.41.1 were out | SQLite 3.54.1 is taken (its opcode list is unchanged); rusqlite 0.41.1, whose one change PHV does not reach, waits for the next update under the two-week rule |
| R23-26 | Expired used-state records were never removed | Every store removes them |
| R23-27 | A server whose clock ran ahead could prune other servers' claims early | Expiry is counted by the store's clock; the specification asks that a site's servers agree within 30 seconds |
| R23-28 | Nothing warned when a Redis session store may evict keys, which voids the one-use guarantee (RC32-01) | A warning at startup names the eviction policy |
| R23-29 | A value inserted into a list attribute (`srcset`, animation `values`) could start an item of its own | Every insertion into such a list is checked as an item's start; animation values take numbers |
| R23-30 | Under `PHV_VM_PRUNE_GEN_REGS=1` a waiting generator drops variables it no longer reads, where the specification said it keeps its variables | Documented next to the setting |
| R23-31 | Under that setting, a plan that did not fit in memory was worked out again at every generator creation | The failure is remembered |
| R23-32 | Smaller overshoots of `std.image`: a GIF frame larger than its screen, a VP8 partition declared past the file, very wide JPEGs | Counted |
| R23-33 | On Linux the self-call check lost the scope of a link-local IPv6 address | The address is compared with its scope |
| R23-35 | Found by the re-attack: `imagesrcset` (`<link rel=preload as=image>`), `attributionsrc` and `archive` fetch URLs but were not URL attributes to the tracker, so a visitor's string could start them (nothing runs there) | They are URL attributes, and `imagesrcset` a list of URLs like `srcset` |

### Residual Risks

- In Redis, the SQL stores and the file store, the number of used-state records is bounded by the callback rate over
  about 10.5 minutes (each record costs two page requests); the memory store's by its session limits.
- Jobs and `phv run` take no image conversion slot.
- A page answering a request that carries any cookie is `private, no-store` unless it sets `Cache-Control` itself.
- The decoders and encoders of `std.image` still cannot be interrupted (§29).
- HTTP/2 frame-level attacks were not run (no client), nor Firefox and Safari for templates.
- §29's residual risks remain, and those §30 lists (RC32-01's recovery case).

### Verification

The PoCs were run again against the fixed release build (before → after):
- a 1×1 lossless WebP with 65,536 code groups, and one of 7.6 MB: 1,315 MB, and 2,064 MB in 12.9 seconds, under a
  128 MB limit → the memory error at 13 MB within 0.14 seconds; the same bitstreams in an ANMF frame of a file that is
  not an animation (R23-34): 1,264 and 4,475 MB with the first fix → the memory error at 1–6 MB;
- the used-state fills (R23-2): after 32,748 claims, 20 of 20 real sign-ins refused (memory store, Redis) → after
  32,864 and 33,318 claims, 20 of 20 reached the provider; one session's 520 sign-ins, a victim in its shard refused
  in every store → both victims reached the provider in every store; the regenerate replay (R23-9): two exchanges →
  `invalid_state`, one exchange;
- `PRAGMA busy_timeout` under a 2-second limit: 22.5 seconds → refused in 0.13 seconds; with two workers, two
  requests never answered and `/ok` 503 → 500 in 0.03 seconds and `/ok` 200 in 7 ms;
- 40,000 trait aliases: 13 seconds in `phv check` → 0.35 seconds; the language server's hover 11.6 → 0.24 seconds;
- the template breakouts (R23-5, R23-6, R23-16) in Chrome 157: the visitor's script ran in six pages → refused at check
  or at run time; the reviewer's fuzzers 169 flagged templates → 0;
- a JPEG whose first scan lacks components: 374 MB → 6 MB; a GIF with XMP blocks: 195 → 101 MB;
- image slots held by one client: 259 of 260 conversions of another visitor failed → 160 of 160 succeeded;
- `XA\vPREPARE`: the prepared transaction outlived the script → refused; `ROLLBACK\v`: a lost update → `DatabaseError`;
- the admin port flooded with empty lines from 16 addresses: 70 of 74 probes closed → 74 of 74 answered;
- a helper growing a collection, n = 40,000: 5.8 seconds (interpreter) and 4.5–10 seconds (native) → 0.03–0.12.

A further reviewer then attacked the code the fixes added: the bitstream reader (about 1.7 million inputs, in process
with overflow checks, against image-webp 0.2.4 measured by a counting allocator: no panic, overflow or hang, never a
bitstream decoded that the reader refused, never more than 64 KB past its count), the used-state records (two servers
sharing each of the file, Redis, PostgreSQL and MySQL stores, twelve trials each of twelve concurrent callbacks with a
regenerating stale page: every code exchanged once), the template rules (52,000 generated templates and about 180
written by hand against parse5 and Chrome 157: none ran a value), and the registers let go on return (44,745 programs
and the test cases, interpreter and JIT, against the unfixed binary: no difference). It found R23-34 and R23-35, fixed
and checked against its files.

All tests pass on Windows and on Linux (Docker), against PostgreSQL 17, MySQL 8.4 and Redis 7, with clippy
(`-D warnings`) and rustfmt. The benchmarks of `bench/`, run back to back with the binary of 6bc5a8c
(`bench/results/6bc5a8c-loaded.json`, `audit20-loaded.json`): the server's pages 0.98–1.01 in requests per second,
the editor's rows 0.74–1.00, and the micro-benchmarks 0.93–1.07, where creating objects and throwing exceptions are
4–7% slower (a returning constructor lets go of its registers) and the rest within noise.

### Regression tests

Regression tests are named after what they check, in each crate's tests and in `tests/cases` and `tests/db`
(`err_svg_end_around`, `err_html_table_part_in_svg`, `err_html_svg_in_select`, `err_svg_self_closed_split`,
`err_animate_values`, `orm_forgets_other_writes`, `mysql_orm_keys`). Some of the others:
- `image.rs`: `a_lossless_webp_with_many_code_groups_stays_within_the_limit` (also in an ANMF frame of a file that is
  not an animation), `a_sequential_jpeg_whose_first_scan_lacks_components_counts_its_coefficients`;
  `phv_std`: `lossless_webp_prefix_codes_are_counted_before_image_webp_builds_them`,
  `gif_metadata_extensions_are_not_collected`; `phv_server`: `one_address_cannot_take_every_image_conversion_slot`;
- `oauth.rs` and `session.rs`: `database_claims_leave_room_for_every_other_session`,
  `claims_of_other_sessions_leave_room_for_a_new_one`,
  `a_used_state_stays_used_when_a_stale_save_regenerates_the_session`,
  `database_claims_expire_by_the_stores_clock_and_are_removed`, `a_redis_store_that_evicts_keys_is_told`;
- `sqlite_limits.rs`: `a_write_waiting_for_a_lock_stops_at_the_time_limit`; `netdb`:
  `prepared_transactions_are_refused` and `statements_that_end_or_roll_back_a_transaction_are_told_apart` (with
  vertical tabs); `databases.rs`: `a_connection_made_for_a_much_longer_time_limit_is_not_used_for_a_short_one`;
- `compile_cost.rs`: `many_trait_aliases_check_at_once`, `many_errors_in_traits_are_reported_at_once`; `lsp.rs`:
  `answers_with_many_trait_aliases_open`; `linear.rs`: `growing_a_value_passed_to_a_helper_takes_linear_time`;
  `jit.rs`: `numbers_written_over_lists_let_them_go`;
- `htmlctx.rs`: `end_tags_inside_foreign_content_close_only_what_it_opened`,
  `table_parts_inside_foreign_content_are_refused`, `every_url_of_a_list_is_checked`;
- `admin.rs`: `connections_sending_only_empty_lines_do_not_keep_probes_out`; `http.rs`:
  `a_pages_own_vary_fields_are_all_kept`, `a_buffered_page_asked_with_a_cookie_is_private`;
  `packaging/test_licenses.py`, `bench/test_compare_php.py`.
