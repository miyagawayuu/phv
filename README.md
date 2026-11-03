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
  strict, no truthiness, null safety (`?T`), interfaces, abstract and `final` classes, enums, generic functions and
  classes, `finally`. Unused variables and imports and unreachable code are reported as warnings.
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
  - The results of five security audits are in [docs/security-audit.md](docs/security-audit.md).
    To report a vulnerability, see [SECURITY.md](SECURITY.md).
- **Speed**: a register-based bytecode VM specialized by type, a Cranelift JIT, and an on-disk bytecode cache.
- **Tools**: test runner, formatter, Language Server, package manager, and a migration tool from PHP.
  A VS Code extension (highlighting and the language server) is in [editors/vscode](editors/vscode); each release
  includes it as `phv-<version>.vsix`.

The full language specification is in [docs/spec.md](docs/spec.md).

## Building

You need a recent stable Rust (tested with 1.97). SQLite is built in, so a C compiler is also needed
(Visual Studio Build Tools on Windows).

```bash
cargo build --release
```

The executable is `target/release/phv` (`phv.exe` on Windows).

## Usage

```bash
phv run examples/hello/hello.phv         # type-check and run
phv check examples/site/public/index.phv # type-check only
phv serve examples/site                  # serve the site on http://127.0.0.1:8080
phv build examples/site -o site-server   # one executable that serves the site, without the sources
phv test examples                        # run the test_* functions in *_test.phv files
phv fmt examples                         # fix indentation and spacing (--check only reports)
phv lsp                                  # Language Server for editors
phv install                              # fetch the [dependencies] in phv.toml
phv migrate legacy/page.php              # convert PHP and list what needs manual work
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
| `phv_std` | Builtins (strings, JSON, dates, crypto, files, SQLite, PostgreSQL, MySQL, Redis, WebSocket, HTTP client, background work, sessions) and the prelude, `std.http`, `std.db`, `std.net` and `std.async` written in PHV |
| `phv_driver` | Loading modules and templates, `phv.toml`, bytecode cache, running programs |
| `phv_server` | Web server (hyper + tokio), routing, sessions, CSRF, uploads |
| `phv_cli` | The `phv` command (run / check / serve / test / fmt / lsp / install / migrate) |

## Tests

```bash
cargo test --workspace
```

- Golden tests: each `tests/cases/*.phv` is run and compared with its `.out` file, also with the JIT on and
  after `phv fmt`, which must not change the behavior.
- Security regression tests (`err_security.phv`, `err_context.phv`, `security.phv`, and others).
- Integration tests that start the real server and talk HTTP to it.
- Fuzzing with mutated programs (set the number of runs with `PHV_FUZZ_ITERATIONS`).

To update the expected output, run `PHV_BLESS=1 cargo test -p phv_driver --test golden golden` and review the
diff. CI (`.github/workflows/ci.yml`) runs all of this plus clippy, rustfmt and `cargo audit` on Linux, Windows
and macOS.

Releases follow [docs/releasing.md](docs/releasing.md): each one is audited and its findings fixed before it is
published.

## Benchmarks

`examples/bench/bench.phv` compared with the equivalent `bench.py` (Windows 11, release build; indicative only).

| Benchmark | PHV (JIT) | PHV (interpreter) | CPython 3.13 |
|---|---:|---:|---:|
| `fib(30)` (recursive calls) | 6.7 ms | 80 ms | 100 ms |
| 10 million loop iterations | 13 ms | 296 ms | 329 ms |
| Concatenating 1 million characters | 20 ms | 28 ms | 65 ms |
| Mandelbrot 300×300 (floats) | 4.0 ms | 249 ms | 196 ms |
| Building 1 million strings from numbers | 633 ms | 677 ms | 342 ms |
| map / filter / sum over 1 million elements | 179 ms | 179 ms | 87 ms |
| 200,000 map updates | 45 ms | 72 ms | 34 ms |

The JIT compiles functions whose values are all `int`, `bool`, `float` or `string`. Code that relies on closures,
lists or maps runs in the interpreter and, as the table shows, can be slower than CPython; so is turning many
numbers into strings, where formatting dominates (to be improved). Set `PHV_JIT=0` to turn the JIT off.

## License

[MIT](LICENSE)
