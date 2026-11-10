<h1>
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo/phv-lockup-on-dark.svg">
    <img src="assets/logo/phv-lockup-on-light.svg" alt="PHV" height="64">
  </picture>
</h1>

English | [日本語](README.ja.md)

PHV is a programming language for the web that keeps the feel of PHP while
the language itself guarantees **type safety, safe defaults and speed**.
The implementation is written in Rust.

PHV was called PHX until v0.2.7.

Website: <https://phv-lang.org>

```phv
<?phv
import std.http.request;
import std.db.Database;

function greet(string $name, int $times = 1): string {
    $out = "";
    foreach (0..$times as $_) {
        $out .= "Hello, {$name}!\n";
    }
    return $out;
}

$name = request()->query("name") ?? "world";      // ?string becomes string with ??
$db = Database::open("data/app.db");
$rows = $db->query(sql"SELECT * FROM users WHERE name = {$name}");   // values are always placeholders
echo greet($name);                                 // HTML-escaped automatically in web pages
```

## Features

- **What comes from PHP**: `$variables`, `[1, 2]` / `["k" => v]`, `echo`, `foreach`, `match`, embedding in HTML,
  routing by file name, changes take effect on save, and a share-nothing model where each request starts fresh.
- **Type safety**: a variable's type is fixed by its first assignment. No implicit conversions, `==` is always
  strict, no truthiness, null safety (`?T`), interfaces, traits, abstract and `final` classes, enums with `match`
  that must handle every case, generic functions and classes with bounds (`<T: Shape>`), type aliases, `finally`.
  Unused variables and imports and unreachable code are reported as warnings.
- **Convenient syntax**: destructuring (`[$a, $b] = ...`), variadic parameters and `...` spreading, functions as values
  (`strlen(...)`), constant expressions, `import ... as`, heredoc strings, generators (`yield`), and exceptions
  with a cause and a trace.
- **Web applications**: routes with parameters (`public/posts/[id].phv`), middleware (`_middleware.phv`), custom
  error pages, layouts, form validation (`std.validate`), mail (`std.mail`), translations (`std.i18n`), database migrations (`phv db migrate`),
  caching and range requests for static files, gzip, CORS, access logs, a graceful stop, and responses streamed while
  the page runs (`response()->flush()`).
- **Web services**: several server processes or machines share sessions, rate limits, CSRF tokens (`secret_env`) and
  `publish()` to WebSockets and Server-Sent Events (`publish_store`); background jobs with retries and cron-like
  schedules (`std.jobs`, `phv jobs`); health checks and Prometheus metrics on an admin port, request ids, and traces
  exported to OpenTelemetry (`[telemetry]`); sign-in with OAuth 2.0 and OpenID Connect (`std.oauth`).
- **Images**: uploads read as PNG, JPEG, GIF or WebP, resized, cropped and written without their metadata (`std.image`).
- **Standard library**: regular expressions (`std.regex`), `format()` checked at compile time, dates in time zones
  (`std.time`), JSON decoded into typed classes (`json_decode_as`), encryption, signed tokens, JSON Web Tokens signed
  with RS256, ES256, EdDSA or HS256 and key sets (`std.jwt`),
  the rows of tables as typed objects with a repository per class (`std.orm`), list and map
  methods, mathematics, UUIDs, CSV, URLs, directories, logging, and arguments and standard input for scripts.
- **Security**:
  - **XSS**: output is escaped automatically, and inserting a value where escaping cannot protect it (unquoted
    attributes, URLs, `<script>`) is a compile error. Only `safe_url()` values may start a URL and only
    `json_html()` values may go into `<script>` (types `SafeUrl` / `SafeJs`). The position is determined the way
    browsers parse HTML.
  - **SQL injection**: SQL can only be written as `sql"..."` literals.
  - **CSRF**: the server checks form POSTs automatically.
  - **CSP**: a Content-Security-Policy is on by default. `<script>` tags written in templates get a nonce
    automatically, so only injected scripts are blocked.
  - **Sessions**: stored on the server, with protection against fixation (in memory, or in files that survive
    restarts).
  - **Rate limiting**: `rate_limit()`, and a per-client limit with `requests_per_minute`.
  - **Permissions**: scripts can only touch the files and environment variables allowed in `phv.toml`. Links and
    hard links are not followed, and paths cannot be swapped between the check and the use.
  - **Limits**: memory, time and recursion depth are limited.
  - **No dynamic execution**: there is no `eval` and no variable variables.
  - The results of nineteen security audits are in [docs/security-audit.md](docs/security-audit.md).
    To report a vulnerability, see [SECURITY.md](SECURITY.md).
- **Speed**: a register-based bytecode VM specialized by type, a Cranelift JIT, and an on-disk bytecode cache.
- **Tools**: `phv new` for a project or a site, a test runner (with an in-process HTTP client, `std.testing`,
  before/after hooks, JUnit reports and coverage), live reload (`phv serve --watch`), a formatter, a Language Server
  (rename, references, quick fixes, formatting and more), a debugger (`phv dap`), a profiler (`phv run --profile`),
  an API reference from doc comments (`phv doc`), a package manager, database migrations, and a migration tool from
  PHP.
  A VS Code extension (highlighting, the language server and the debugger) is in [editors/vscode](editors/vscode); each release
  includes it as `phv-<version>.vsix`.

Start with the [tutorial](docs/tutorial.md). The full language specification is in [docs/spec.md](docs/spec.md), and the
standard library's reference in [docs/stdlib.md](docs/stdlib.md). What is planned next, with its risks and measures, is in
[docs/improvement-plan.md](docs/improvement-plan.md).

## Installing

Each [release](https://github.com/miyagawayuu/phv/releases) has archives for Windows, Linux (x64 and ARM64; also
static builds that need no C library) and macOS, with `SHA256SUMS`. From 0.9.0 there is also a Docker image that
holds only `phv`:

```bash
docker run --rm -p 8080:8080 -v "$PWD:/app" --user "$(id -u):$(id -g)" ghcr.io/miyagawayuu/phv serve --host 0.0.0.0 --prod
```

Or with a package manager (from 0.9.0):

```bash
brew install miyagawayuu/phv/phv                                         # macOS and Linux
scoop bucket add phv https://github.com/miyagawayuu/scoop-phv; scoop install phv   # Windows
```

## Building

You need a recent stable Rust (tested with 1.97). SQLite and the allocator (mimalloc) are built in, so a C
compiler is also needed (Visual Studio Build Tools on Windows).

```bash
cargo build --release
```

The executable is `target/release/phv` (`phv.exe` on Windows).

## Usage

```bash
phv new my-site --site                   # a new site (a page, a layout and a test)
phv run examples/hello/hello.phv         # type-check and run
phv check examples/site/public/index.phv # type-check only
phv serve examples/site                  # serve the site on http://127.0.0.1:8080 (--watch: reload on save)
phv build examples/site -o site-server   # one executable that serves the site, without the sources
phv test examples                        # run the test_* functions in *_test.phv files (--coverage, --junit)
phv fmt examples                         # fix indentation and spacing (--check only reports)
phv lsp                                  # Language Server for editors (phv dap: the debugger)
phv doc lib -o doc/index.html            # an API reference from the /** */ comments
phv install                              # fetch the [dependencies] in phv.toml
phv migrate legacy/page.php              # convert PHP and list what needs manual work
phv db migrate --sqlite data/app.db      # apply the SQL files of migrations/ that have not run yet
phv disasm examples/bench/bench.phv      # show the bytecode
```

## Example site

`examples/site/` is a small blog: a post list, post pages, a guestbook stored in SQLite (using sessions, CSRF
protection and flash messages), a contact form with validation, and a JSON API.

```
examples/site/
├── phv.toml             security settings and permissions ([permissions].write = ["data/"])
├── public/              pages, one per URL (/ -> index.phv, /guestbook -> guestbook.phv)
├── lib/                 imported modules (posts.phv, guestbook.phv)
├── views/*.phv.html     typed templates
├── tests/posts_test.phv tests run by phv test
└── data/                the SQLite database (the only place scripts may write)
```

## Error messages

```
error: cannot call `->upper()` on `?string` because it might be null
 --> err_null_safety.phv:6:6
  |
6 | echo $name->upper();
  |      ^^^^^
  = note: check `!= null` first, or use `?->`

error: a URL attribute cannot start with an unchecked value
  --> err_security.phv:45:14
   |
45 | <a href="<?= $name ?>">url</a>
   |              ^^^^^
   = note: use `safe_url($url)`, which only allows http(s), mailto, tel and relative URLs (blocks `javascript:`); `escape()` does not make a URL safe
```

## Project layout

| Crate | Purpose |
|---|---|
| `phv_syntax` | Sources, diagnostics, lexer (code / HTML modes), parser, HTML context tracking, formatter |
| `phv_types` | Name resolution, type checking, narrowing, definite assignment → typed HIR; builtin signatures |
| `phv_compiler` | HIR → register bytecode |
| `phv_vm` | Values, bytecode format, interpreter, runtime limits |
| `phv_jit` | Native code for functions that use int/bool/float/string values (Cranelift) |
| `phv_std` | Builtins (strings, JSON, dates, crypto, files, SQLite, PostgreSQL, MySQL, Redis, WebSocket, HTTP client, background work, sessions) and the prelude, `std.http`, `std.db`, `std.net`, `std.async`, `std.validate`, `std.mail`, `std.regex`, `std.time`, `std.ws`, `std.jobs` and `std.testing` written in PHV; migrations, the job store |
| `phv_driver` | Loading modules and templates, `phv.toml`, bytecode cache, running programs |
| `phv_server` | Web server (hyper + tokio, HTTPS and HTTP/2), routing, sessions, CSRF, uploads, WebSocket server and Server-Sent Events, `publish()` between processes, job runner, admin port and metrics |
| `phv_cli` | The `phv` command (run / check / serve / test / fmt / lsp / install / migrate) |

## Tests

```bash
cargo test --workspace
```

- Golden tests: each `tests/cases/*.phv` is run and compared with its `.out` file, also with the JIT on and
  after `phv fmt`, which must not change the behavior.
- Security regression tests (`err_security.phv`, `err_context.phv`, `security.phv`, and others).
- Tests of the memory and time limits for builtins that build large results (`limits.rs`), for SQLite
  (`sqlite_limits.rs`) and for the JIT (`jit.rs`), and of the cost of compiling large programs (`compile_cost.rs`).
- Integration tests that start the real server and talk HTTP to it.
- Fuzzing with mutated programs (set the number of runs with `PHV_FUZZ_ITERATIONS`).

To update the expected output, run `PHV_BLESS=1 cargo test -p phv_driver --test golden golden` and review the
diff. CI (`.github/workflows/ci.yml`) runs all of this plus clippy, rustfmt and `cargo audit` on Linux, Windows
and macOS.

Releases follow [docs/releasing.md](docs/releasing.md): each one is audited and its findings fixed before it is
published.

## Benchmarks

`examples/bench/bench.phv` compared with the equivalent `bench.py` (Windows 11, release build; indicative only).
The harness in [bench/](bench/README.md) reproduces these numbers (min of 5 runs) and measures the server and the
language server too; a change that touches performance comes with its before/after `bench/results/*.json`.

[PHP 8.5 comparison results](bench/README.md#comparing-phv-with-php-85) cover typed property reads
and generators, with and without JIT.

| Benchmark | PHV (JIT) | PHV (interpreter) | CPython 3.13 |
|---|---:|---:|---:|
| `fib(30)` (recursive calls) | 7.2 ms | 84 ms | 99 ms |
| 10 million loop iterations | 14 ms | 286 ms | 348 ms |
| Concatenating 1 million characters | 22 ms | 31 ms | 66 ms |
| Mandelbrot 300×300 (floats) | 4.2 ms | 234 ms | 198 ms |
| Building 1 million strings from numbers | 304 ms | 337 ms | 351 ms |
| map / filter / sum over 1 million elements | 184 ms | 180 ms | 88 ms |
| 200,000 map updates | 42 ms | 46 ms | 34 ms |
| Sieve of Eratosthenes up to 1 million (`list<bool>`) | 89 ms | 155 ms | 107 ms |
| Building and summing a list of 1 million floats | 34 ms | 75 ms | 76 ms |
| Appending to a property with `.=` 200,000 times | 12 ms | 11 ms | 333 ms |
| 1 million string comparisons (`==`) | 21 ms | 46 ms | 40 ms |
| Escaping 200,000 strings for HTML | 89 ms | 86 ms | 93 ms |
| Creating 1 million objects | 178 ms | 183 ms | 110 ms |
| Throwing and catching 100,000 exceptions | 40 ms | 42 ms | 14 ms |
| Sorting 200,000 elements with `sortBy` | 318 ms | 306 ms | 269 ms |

The JIT compiles functions and methods whose values are all `int`, `bool`, `float` or `string`, nullable ones,
lists and maps of them, or objects. Lists, maps and objects are handled by the interpreter's own code, so the gain
is in the loops and arithmetic around them: creating objects and reading their properties take about as long as in
the interpreter. Code that relies on closures runs in the interpreter and, as the table shows, can be slower than
CPython (`PHV_JIT_EXPLAIN=1` shows why a function is not compiled); so are exceptions, which note where they were
thrown (to be improved). Set `PHV_JIT=0` to turn the JIT off.

## License

[MIT](LICENSE)
