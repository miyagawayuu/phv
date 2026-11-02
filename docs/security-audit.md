# PHV Security Audit Report (v0.2)

English | [日本語](security-audit.ja.md)

This report consolidates four audits. §1–§6 cover the first audit (and the resolution of its residual risks), §7 covers the second vulnerability assessment,
§8 covers the parts excluded from the second assessment (`phv lsp`, `phv migrate`, bytecode cache), and §9 covers what was added up to v0.4.0.

Date: 2026-11-02
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
