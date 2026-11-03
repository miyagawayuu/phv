# PHV Security Audit Report

English | [日本語](security-audit.ja.md)

This report consolidates eight audits. §1–§6 cover the first audit (and the resolution of its residual risks), §7 covers the second vulnerability assessment,
§8 covers the parts excluded from the second assessment (`phv lsp`, `phv migrate`, bytecode cache), §9 covers what was added up to v0.4.0, §10 the follow-up on the residual risks, §11 what was added for v0.5.0, §12 the follow-up on its residual risks, §13 what was added for v0.5.2, §14 what was added for v0.6.0, and §15 what was added for v0.7.0.

Date: 2026-11-02 (§9–§13: 2026-11-03, §14–§15: 2026-11-04)
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
