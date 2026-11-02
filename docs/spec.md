# PHX Language Specification (v0.2)

English | [日本語](spec.ja.md)

Website: <https://phx-lang.org>

PHX is a language for the web. It keeps what is good about PHP: you save a file and it runs, it is built for the web, and it embeds in HTML.
On top of that, the language itself guarantees type safety, security, execution speed, and maintainability.
It is not compatible with existing PHP code, but it aims to be something PHP programmers can read and write right away.

## 1. Design Principles

| PHP weakness | How PHX solves it |
|---|---|
| Implicit conversion (`"1" + 1`, `"abc" == 0`) | No implicit conversion. `==` is always a strict comparison. Comparing values of different types is a compile error |
| Runtime errors caused by null | The `?T` type. A value cannot be used until it is null-checked (`!= null`, `is`, `??`, `?->`) |
| XSS | `<?= ?>` and `echo` to a web page are HTML-escaped automatically. Only values of type `Html` are not escaped |
| `eval`, variable variables, `extract`, dynamic `include` | Not in the language. Modules use static `import`; templates use literal paths only |
| SQL injection | SQL can only be written as an `sql"..."` literal (type `Sql`), and embedded values always become placeholders |
| Escaping that ignores context | The position of `<?= ?>` (attribute, URL, `<script>`, CSS) is analyzed, and inserting into a place that escaping cannot protect is a compile error |
| CSRF and session fixation | The server checks the token on form POSTs automatically. Only session IDs issued by the server are accepted |
| Scripts that can do anything | Access to files and environment variables is limited to what `[permissions]` in `phx.toml` allows |
| Execution of injected scripts | The default Content-Security-Policy blocks inline scripts other than the `<script>` elements written in templates |
| Header injection | A header value that contains a newline raises `ValueError` |
| Path traversal | The server does not serve files outside `public/`, dotfiles, or templates |
| State leaking between requests | Each request gets a fresh heap. There are no static properties and no global variables |
| Execution speed | A register-based bytecode VM specialized for static types |
| Hard to maintain at scale | Functions with type annotations, modules and `export`, readable error messages |

## 2. Files

- `.phx` … A script or module. Starts in **code mode**. The leading `<?phx` is optional.
- `.phx.html` … A template. Starts in **HTML mode**.
- Writing `?>` in code mode switches to HTML mode. In HTML mode, `<?phx` (or `<? `) switches back to code,
  and `<?= expr ?>` outputs a value. A single newline right after `?>` is not output (same as PHP).
- The character encoding is UTF-8.

## 3. Lexical Structure

- Variables are written `$name`. There is no `$$name` (variable variables).
- Comments are `// ...`, `# ...`, and `/* ... */`.
- Integers: `42`, `1_000_000`, `0xFF`, `0b1010`, `0o17`. 64-bit. Overflow raises `ArithmeticError`.
- Floating-point numbers: `1.5`, `1e-3`.
- Strings:
  - `'...'` … No interpolation. The only escapes are `\'` and `\\`.
  - `"..."` … Interpolates `$name` and `{$expr}`. Escapes are `\n \t \r \0 \e \\ \" \$ \{ \u{1F600}`.
- Reserved words: `function fn return if elseif else endif while endwhile do for endfor foreach endforeach as break continue
  echo class extends new public private protected readonly static true false null match default try catch finally
  throw import export use is instanceof interface implements abstract final enum case const`

## 4. Types

| Type | Description |
|---|---|
| `int` | 64-bit integer |
| `float` | 64-bit floating-point number |
| `string` | UTF-8 string |
| `bool` | `true` / `false` |
| `null` | The type whose only value is null. Normally used as part of `?T` |
| `void` | Indicates no return value (only allowed as a return type) |
| `Html` | A string that may be treated as safe HTML. Created by `raw()`, `escape()`, and templates |
| `SafeUrl` | An `Html` that may be placed at the start of a URL attribute. Created by `safe_url()` |
| `SafeJs` | An `Html` that may be placed inside `<script>`. Created by `json_html()` |
| `any` | Holds any value. Must be narrowed with `is` before use |
| `list<T>` | An array whose elements have type `T` (sequential indexes starting at 0) |
| `map<K, V>` | An associative array that preserves insertion order. `K` is `int` or `string` |
| `?T` | `T` or `null` (same as `T|null`) |
| `A|B` | Union type |
| `fn(A, B): R` | Function (closure) type |
| Class name | The type of an object. `self` is the class itself |

PHP's `array` is split into `list<T>` and `map<K, V>`. `mixed` is `any`.

### Assignability

- The same type, a subclass to its parent class, `T` to `?T` or to a union that contains it, and any type to `any`.
- **`int` is automatically converted to `float` where a `float` is expected** (the only implicit conversion, because it does not change the meaning of the value).
  A `list<int>` cannot be passed as a `list<float>` (element types are invariant).
- All other conversions must be written explicitly: `(int)$float`, `(float)$int`, `(string)$x`, `$s->toInt()`, `$s->toFloat()`.
  Casting from a string, as in `(int)"12"`, is not allowed (`->toInt()` returns `null` if the string is not a number).
  There is no `(bool)` cast.

## 5. Variables

- As in PHP, **a variable is declared by its first assignment**. The type of the value becomes the type of the variable, and a value of a different type cannot be assigned later.

  ```phx
  $count = 1;
  $count = "two";   // error: expected `int`, found `string`
  ```

- When the type cannot be inferred (`null` or an empty `[]`), declare the variable with a type.

  ```phx
  ?User $found = null;
  list<string> $names = [];
  map<string, int> $counts = [];
  ```

- A variable's scope is the whole function (same as PHP). However, **a variable that is assigned only on some paths cannot be used**.

  ```phx
  if ($ok) { $msg = "hi"; }
  echo $msg;   // error: variable `$msg` might not be defined here
  ```

- A `foreach` or `catch` variable whose type differs from an existing variable with the same name is a new variable that shadows it
  (so `catch (A $e)` and `catch (B $e)` can be written one after another).
- `$this` can be used only inside non-static methods. It cannot be assigned.

### Type Narrowing

Conditions narrow the types of local variables.

```phx
function greet(?User $u): string {
    if ($u == null) {
        return "guest";
    }
    return $u->name;          // $u is User here
}

if ($v is int) { ... }        // $v is int here
$name = $req->query("name") ?? "world";   // string
$len = $user?->email?->len();             // ?int
```

`!= null`, `== null`, `is`, `!`, `&&`, `||`, and early `return` / `throw` / `break` / `continue` are used for narrowing.
Narrowing applies to local variables and to properties of local variables (`$this->user != null`). Narrowing of a property
is reset by a function or method call or by an assignment to a property (because the call may have changed the value).

## 6. Operators

From highest to lowest precedence:

| Operator | Description |
|---|---|
| `()` `[]` `->` `?->` `::` | Call, index, member |
| `**` | Exponentiation (right-associative) |
| `is` `instanceof` | Type test |
| `!` `-` `~` `(int)` etc. | Unary operators, casts |
| `*` `/` `%` | `/` always returns `float`. Use `intdiv()` for integer division |
| `+` `-` | Numbers only. Mixing `int` and `float` gives `float` |
| `.` | String concatenation. Can concatenate `string`, `int`, and `float`. `Html . Html` is `Html` |
| `<<` `>>` | Bit shift |
| `..` `..=` | Range (`0..$n` excludes `$n`; `1..=$n` includes it) |
| `<` `<=` `>` `>=` | Between numbers, or between strings |
| `==` `!=` `<=>` | `==` is always strict. `===` is not used (writing it is an error that points you to `==`) |
| `&` `^` `\|` | Bitwise operations (`int` only) |
| `&&` | Logical AND (`bool` only) |
| `\|\|` | Logical OR (`bool` only) |
| `??` | The right operand if the left is null. `$m["k"] ?? $d` does not throw even if the key is missing |
| `? :` | Ternary operator |

- **There is no implicit conversion to boolean (truthiness)**. `if ($xs)` is an error; write `if (!$xs->isEmpty())`.
- Assignment is a statement, not an expression: `=`, `+=`, `-=`, `*=`, `/=`, `%=`, `**=`, `.=`, `??=`, `++`, `--`.
- There is no string indexing (`$s[0]`). Use `$s->slice(0, 1)`.

## 7. Control Flow

```phx
if (...) { } elseif (...) { } else if (...) { } else { }
while (...) { }
do { } while (...);
for ($i = 0; $i < 10; $i++) { }
foreach ($list as $value) { }
foreach ($list as $index => $value) { }
foreach ($map as $key => $value) { }
foreach (0..$n as $i) { }            // loops over a range without building an array
break; continue; break 2; continue 2;
```

The template-oriented syntax is also available: `if (...): ... elseif (...): ... else: ... endif;`,
`foreach (...): ... endforeach;`, `while (...): ... endwhile;`, `for (...): ... endfor;`

`foreach` iterates over a copy of the array as it was when the loop started (the same value semantics as PHP).

### match

```phx
$text = match ($code) {
    200, 201 => "ok",
    404 => "not found",
    default => "other",
};
$size = match (true) {
    $n < 10 => "small",
    default => "large",
};
```

Comparison uses `==` (strict). If no arm matches and there is no `default`, `UnhandledMatchError` is thrown.

### Exceptions

```phx
try {
    risky();
} catch (NotFound | InvalidArgumentException $e) {
    echo $e->getMessage();
}
throw new RuntimeException("boom");
$port = $config["port"] ?? throw new ValueError("port is required");
```

```phx
try {
    $db->execute(sql"...");
} finally {
    $lock->release();      // runs whether the block exits by return / break / continue / exception
}
```

Only `Exception` and its subclasses can be thrown.

## 8. Functions

```phx
function greet(string $name, int $times = 1): string { ... }
greet("Ann");
greet(name: "Bob", times: 3);       // named arguments
```

- Parameter types are required. If the return type is omitted, it is `void`.
- A function that returns a value must `return` on every path, or it is an error.
- Default values must be constants (literals, `null`, `[]`).
- Functions are not values. To pass a function, wrap it in a closure.
- Generic functions with type parameters can be written (see "Generic Functions" in §9).

### Closures

```phx
$double = fn(int $x): int => $x * 2;           // captures outer variables automatically
$names->map(fn($n) => $n->upper());            // parameter types are inferred from context
$counter = function () use ($start): int { return $start + 1; };
```

- All captures are **by value** (a copy taken when the closure is created). There is no capture by reference (`&$x`).
- A closure created inside a method can use `$this`.

## 9. Classes

```phx
class User {
    private int $loginCount = 0;
    public readonly string $email;

    public function __construct(
        public readonly int $id,      // constructor property promotion
        public string $name,
        string $email,
    ) {
        $this->email = $email;
    }

    public function login(): void {
        $this->loginCount++;
    }

    public static function guest(): User {
        return new User(0, "guest", "guest@example.com");
    }
}

class Admin extends User {
    public function __construct(int $id, string $name) {
        parent::__construct($id, $name, "admin@example.com");
    }
}
```

- Properties must have a type. A property that does not allow `null` and has no default value must be assigned at the start of the constructor
  (checked at compile time).
- A `readonly` property can be assigned only inside the constructor of its class.
- Visibility is `public` / `protected` / `private` (checked at compile time). The same applies to constructors:
  a class with `private function __construct(...)` can be instantiated with `new` only inside that class (for example, in a static method)
  (this is why a `Database` can be created only through `Database::open()`).
- Single inheritance. A method override must have the same parameter types and a compatible return type. If a class whose parent
  has a constructor has properties that need initialization, the child constructor must call `parent::__construct(...)`.
- `static` methods are called as `ClassName::method()`. **There are no static properties** (so that state is not shared between requests).
- Objects are references; `list` and `map` are values (copy-on-write).
### interface and abstract

```phx
interface Shape {
    const UNIT = "cm";
    public function area(): float;
}

interface Named extends Shape {
    public function name(): string;
}

abstract class Base implements Named {
    public function label(): string {
        return $this->name() . ": " . $this->area() . self::UNIT;
    }
}

final class Square extends Base {
    public function __construct(private float $side) {}
    public function area(): float { return $this->side ** 2; }
    public function name(): string { return "square"; }
}
```

- An interface contains only method signatures and constants. An interface can inherit from multiple interfaces with `extends`.
- An `abstract` method has no body. A class that has an abstract method must be declared `abstract class`.
- It is a compile error if a concrete class does not implement an interface or abstract method, or if the signature does not match.
- Interfaces, abstract classes, and enums cannot be instantiated with `new`.
- Method calls on a value of an interface type are dispatched by name. Method calls on a value of a class type are dispatched through a vtable.

### enum

```phx
enum Suit {
    case Hearts;
    case Spades;

    public function color(): string {
        return match ($this) {
            Suit::Hearts => "red",
            Suit::Spades => "black",
        };
    }
}

enum Status: string {        // an enum backed by int or string values
    case Active = "active";
    case Banned = "banned";
}

Suit::Hearts->name;              // "Hearts"
Status::Active->value;           // "active"
Status::from("banned");          // Status::Banned (ValueError if not found)
Status::tryFrom("x");            // null
Status::cases();                 // list<Status>
```

Each case is a single object within one execution (one request), so cases can be compared with `==`.

### Constants

```phx
const MAX_ITEMS = 50;            // at the top level of a file (can also be exported / imported)
class Config {
    const VERSION = "1.0";
}
echo MAX_ITEMS, Config::VERSION, self::VERSION;
```

Constant values must be literals (int, float, string, bool, null).

### Generic Functions

```phx
function first_or<T>(list<T> $xs, T $fallback): T {
    return $xs->isEmpty() ? $fallback : $xs[0];
}
$n = first_or([5, 6], 0);        // int
$s = first_or([], "none");       // string
```

Type arguments are inferred from the call's arguments (up to 4). Inside the function, `T` is treated as an opaque type,
and operators and methods cannot be used on values of type `T`. There are no generic classes yet.


## 10. Exception Classes

Available from every file.

```
Exception                    (getMessage(): string)
├── RuntimeException
│   └── DatabaseError        SQL error
├── LogicException
│   └── InvalidArgumentException
├── ArithmeticError          integer overflow, etc.
│   └── DivisionByZeroError  division or remainder by 0
├── IndexError               out-of-range access on a list
├── KeyError                 key not in a map
├── ValueError               invalid argument (a newline in a header, etc.)
├── UnhandledMatchError
├── PermissionError          access to a file or environment variable not allowed in phx.toml
└── AssertionError           failure of assert() / assert_eq()
```

Fatal errors, such as exceeding the execution time limit, cannot be caught.

## 11. Modules

```phx
// lib/posts.phx
export class Post { ... }
export function all_posts(): list<Post> { ... }
function helper(): int { ... }      // anything not exported is not visible from outside
```

```phx
import lib.posts.{Post, all_posts};
import lib.posts.Post;
import std.http.{request, response};
```

- `import a.b.{x, y};` loads `a/b.phx` relative to the project root (the directory that contains `phx.toml`, or, if there is none,
  the directory of the file being run).
- The top level of an imported module can contain only declarations (`function`, `class`, `interface`, `enum`, `const`, `import`).
  Loading a module has no side effects.
- `std.*` are the standard modules (`std.http`, `std.db`). Packages listed in `[dependencies]` are loaded with `import package_name.module`.
- Only files inside the project and package directories can be loaded (symbolic links cannot be used to escape them).

## 12. Templates

```html
<?phx
import lib.posts.Post;
params(string $title, list<Post> $posts);
?>
<h1><?= $title ?></h1>
<?phx foreach ($posts as $post): ?>
  <li><a href="/post?id=<?= $post->id ?>"><?= $post->title ?></a></li>
<?phx endforeach; ?>
```

```phx
$html = render("views/list.phx.html", title: "Posts", posts: $posts);   // Html
echo render("views/layout.phx.html", title: "Home", body: $html);
```

- A template declares typed parameters with `params(...)`. Calls to `render()` are type-checked at compile time.
- The path given to `render()` is a string literal, relative to the project root (`..` is not allowed). The extension is `.phx.html`.
- `render()` returns `Html`, so the result can be passed directly to another template and embedded there (layouts).

## 13. Output and escaping

There are two kinds of output destination: text and HTML.

| Situation | Output destination |
|---|---|
| A script run with `phx run` | Text (HTML with `--html`) |
| A page run by the web server | HTML (text if set to a non-HTML type, for example with `response()->type("application/json")`) |
| Inside a template | Always HTML |

Writing a `string` to an HTML destination escapes `& < > " '`. Values of type `Html` and the HTML parts of a template
are output as is. `echo` and `<?= ?>` follow the same rules.

- `escape($s)` / `htmlspecialchars($s)` … Returns escaped `Html`.
- `raw($s)` … Treats trusted markup as `Html` (use with care to avoid XSS).
- `nl2br($s)` … Escapes the string, then returns `Html` with line breaks converted to `<br>`.
- `safe_url($url)` … Returns a `SafeUrl` that lets through only `http(s)`, `mailto`, `tel`, and relative URLs, and replaces `javascript:` and others with `#`.
- `json_html($value)` … Returns, as `SafeJs`, a JavaScript value (JSON) that is safe to place inside `<script>`.
  The `"` in strings is left as is, so the value cannot be placed inside an attribute value (a compile error; use `escape(json_encode($v))`).
- `SafeUrl` and `SafeJs` can also be used as `Html` (not the reverse). The result of `escape()` is `Html`, so it
  cannot be placed in a URL or inside `<script>`. This is because HTML escaping cannot stop `javascript:` or script code.
- `Html` and `string` cannot be concatenated with `.` (the error suggests `escape()`).
- Escaping is turned off only for the Content-Types `text/plain`, `application/json`, `text/csv`, `text/css`, `text/javascript`,
  `text/markdown`, `text/calendar`, `application/pdf`, `application/octet-stream`, and image, audio, and video types
  (all types that browsers do not render as HTML; `X-Content-Type-Options: nosniff` is also always sent).
  HTML, XML, SVG, and unknown types stay escaped.

### HTML context checks

HTML escaping protects only element content and **quoted** attribute values. `<?= ?>` and `echo` anywhere else
are a compile error. Locations are determined according to the browser's parsing rules (WHATWG), including
the content of `<textarea>`, `<title>`, and similar elements, `<!--` inside `<script>`, comments such as `<!-->` and `--!>`, the inside of `<svg>`,
and text such as `</scripts>` that looks like an end tag but is not one.

| Location | Example | Treatment |
|---|---|---|
| Element content, quoted attribute value | `<p><?= $s ?></p>`, `title="<?= $s ?>"` | Escaped and output |
| Unquoted attribute value | `href=<?= $u ?>` | Error (add quotes) |
| Inside a tag (between attributes) | `<div <?= $attrs ?>>` | Error |
| Start of a URL attribute | `href="<?= $u ?>"`, `src="<?= $u ?>"` | Only `SafeUrl` (`safe_url($u)`) and numbers. If the preceding text contains `/` `?` `#` `:`, it is not the start (`href="/post?id=<?= $id ?>"` is allowed). `href="j<?= ?>"` or only a character reference such as `&#106;` still counts as the start |
| `javascript:` / `data:` URL | `href="javascript:go(<?= $v ?>)"` | Error (numbers are allowed) |
| Inside `<script>` | `var x = <?= $v ?>;` | Only `SafeJs` (`json_html($v)`) and numbers |
| `<style>`, `style="..."` | | Error (numbers are allowed) |
| Event attribute | `onclick="go(<?= $v ?>)"` | Error. Put the value in a `data-*` attribute and read it from JavaScript |
| `<iframe srcdoc="...">` | | Error (the value is parsed as a separate HTML document) |

- The bodies of functions and closures, and `if` and loop blocks, must end in the same HTML state they started in
  (leaving a `<script>` or an attribute open at the end of a block is an error). This prevents the context from drifting with call order or repetition.
- Expressions and statements can be nested up to 500 levels (this prevents deeply nested source from overflowing the implementation's stack).

## 14. Web server

```
phx serve examples/site --port 8080
```

- The site directory must contain `public/`. URLs are determined by file names:
  `/` → `public/index.phx`, `/about` → `public/about.phx` or `public/about/index.phx`. `/about.phx` also works.
- Only files with known extensions (html, css, js, json, txt, images, fonts, pdf, audio, video, wasm) are served as static files.
- Not served: anything outside `public/`, files and directories whose names start with `.` or `_` (`_partial.phx` can be used as a partial),
  `.phx.html` (templates), and unknown extensions (`.php`, `.bak`, `.sql`, etc.). The URL must match the file name's spelling
  exactly (`INDEX.PHX`, Windows short names, and `::$DATA` do not open the file).
- Each request runs in a new VM (a new heap). Only the compiled bytecode is shared.
- When a file is saved, it is recompiled on the next request (the modification times of all the files it depends on are checked).
- Limits: execution time 30 seconds (`--time-limit`), memory 128 MB (`[security] memory_limit_mb`), body 8 MB, receiving headers 10 seconds,
  receiving the body 30 seconds, 1024 concurrent connections, 1000 query and form fields.
- If `[security] requests_per_minute` is set, the number of requests accepted per minute from a single client address is limited to that number
  (beyond it, the response is `429 Too Many Requests` with `Retry-After`). The default is unlimited.
- Headers sent by default: `X-Content-Type-Options: nosniff`, `X-Frame-Options: SAMEORIGIN`,
  `Referrer-Policy: strict-origin-when-cross-origin`, and Content-Security-Policy (see "Content-Security-Policy" below).
  HSTS if `secure_cookies = true`. `Cache-Control: private, no-store` on responses that use sessions or cookies.
  Version information such as `Server` / `X-Powered-By` is not sent. Scripts cannot set transport headers such as `Content-Length`.
- In development mode, the details of compile errors and runtime errors are shown in the browser. Development mode is the default only
  when listening on `127.0.0.1` / `localhost` (it can be set explicitly with `--dev` / `--prod`).

### std.http

```phx
import std.http.{request, response, session};

$req = request();
$req->method;              // "GET"
$req->path;                // "/post"
$req->query("id");         // ?string
$req->form("name");        // ?string (urlencoded / multipart)
$req->header("Accept");    // ?string (header names are case-insensitive)
$req->cookie("theme");     // ?string
$req->file("avatar");      // ?UploadedFile
$req->json();              // any
$req->body;                // string
$req->ip;                  // string (the client's IP address; behind a reverse proxy, the proxy's address)
$req->isPost();            // bool

response()->status(404);
response()->header("Cache-Control", "no-store");
response()->type("text/plain; charset=utf-8");
response()->cookie("theme", "dark");
response()->redirect("/login");                       // paths within the site only
response()->redirectExternal("https://example.com/");  // external URL
response()->json($value);
response()->file("data/photos/1.jpg");                // sends a file instead of the output
```

### Sessions

```phx
import std.http.session;

session()->set("user_id", (string)$user->id);
$id = session()->get("user_id");      // ?string
session()->regenerate();              // call right after login (protects against session fixation)
session()->flash("notice");           // a message that is removed once read
session()->destroy();                 // log out
```

- Values are stored on the server. The cookie holds only a 256-bit random ID
  (`PHXSESSID`, `HttpOnly`, `SameSite=Lax`, and `Secure` with `[security] secure_cookies = true`).
- IDs that the server did not issue are rejected. The values in one session can total at most 64 KB.
- A session is removed after `[security] session_ttl` seconds without access. Even with continued access, it is removed after 24 hours.
- By default, sessions are stored in memory and are lost when the server restarts. If you set a directory in the project,
  as in `[security] session_store = "sessions/"`, each session is stored in its own file. Sessions then survive restarts and can be shared by
  multiple server processes that use the same directory. File names are hashes of the IDs, so the IDs cannot be learned from a directory listing.
  A directory inside `public/` cannot be specified.

### CSRF protection

For `POST`, `PUT`, `PATCH`, and `DELETE` requests in a format that browsers can send from other sites
(`application/x-www-form-urlencoded`, `multipart/form-data`, `text/plain`, or no Content-Type),
the server checks a token before running the script. If the token is missing or wrong, the response is 403.

```html
<form method="post">
  <?= csrf_field() ?>     <!-- <input type="hidden" name="_csrf" value="..."> -->
  ...
</form>
```

When sending from JavaScript, put the value of `csrf_token()` in the `X-CSRF-Token` header.
JSON requests require a CORS preflight, so other sites cannot send them.

The token is an HMAC of a per-browser cookie (`PHXCSRF`), so this relies on attackers being unable to overwrite that cookie.
In production, serve over HTTPS and set `secure_cookies = true` (the cookie gets the `__Host-` prefix and can no longer be overwritten
over plaintext connections or from subdomains).
It can be disabled with `[security] csrf = false`.

### Cookie

```phx
request()->cookie("theme");                                   // ?string
response()->cookie("theme", "dark", maxAge: 86400 * 30);      // HttpOnly; SameSite=Lax by default
response()->deleteCookie("theme");
```

Characters not allowed in names, line breaks, and `SameSite=None` without `Secure` raise `ValueError`.

### Uploads

```phx
$file = request()->file("avatar");     // ?UploadedFile
if ($file != null) {
    echo $file->name, $file->type, $file->size;
    $file->saveTo("storage/avatars/" . random_token(16) . ".png");   // requires [permissions].write
}
```

`name` is the file name sent by the client, with the directory part and control characters removed.
Do not use it in the destination path. The request body is limited to 8 MB in total.

Saved files are outside `public/`, so send them back with `response()->file($path, $type = "")`. The bytes of the file
replace everything the script writes, so images and other binary files can be served (`echo` only sends UTF-8 text).
The path needs `[permissions].read` or `write`. Without `$type`, the Content-Type comes from the extension (images,
audio, video, fonts, PDF, txt, csv, json); HTML, SVG, XML and unknown extensions are sent as `application/octet-stream`
so that an uploaded file cannot run as a page on the site.

### JSON

```phx
$data = request()->json();             // any (invalid JSON raises ValueError)
if ($data is map) {
    $name = $data["name"] ?? null;
    if ($name is string) { ... }
}
response()->json(["ok" => true]);
```

### Database (std.db)

```phx
import std.db.Database;

$db = Database::open("data/app.db");      // requires [permissions].write = ["data/"]. ":memory:" is also allowed
$db->execute(sql"CREATE TABLE IF NOT EXISTS posts (id INTEGER PRIMARY KEY, title TEXT)");
$db->execute(sql"INSERT INTO posts (title) VALUES ({$title})");
$id = $db->lastInsertId();

foreach ($db->query(sql"SELECT id, title FROM posts WHERE id > {$min} ORDER BY id") as $row) {
    echo $row->int("id"), $row->string("title");      // ?int, ?string
}
$one = $db->first(sql"SELECT * FROM posts WHERE id = {$id}");   // ?Row

$db->transaction(function () use ($db): void {
    $db->execute(sql"UPDATE posts SET title = 'x'");
});                                         // ROLLBACK if an exception is thrown
```

- **SQL can be written only as `sql"..."` literals (type `Sql`).** Values embedded with `{$x}` are always passed
  as placeholders, so SQL injection cannot occur. A `string` cannot be passed to `query()`.
- `Sql` values can be concatenated with `.` (their parameters are carried over). `Sql` and `string` cannot be concatenated.
- To choose a column or table name dynamically, use `sql_ident($name)` (it allows only alphanumerics and `_`, and quotes the name).
- `ATTACH` is disabled, so databases other than the permitted files cannot be accessed.
- SQL errors raise `DatabaseError`.

### Dates, cryptography, and encoding

| Function | Description |
|---|---|
| `date($format, ?$ts)` / `gmdate(...)` | Same format as PHP (`Y-m-d H:i:s`, `D, d M Y`, `c`, `U`, etc.). `date` uses the server's local time |
| `mktime($h, $i, $s, $m, $d, $y)`, `strtotime($s): ?int`, `checkdate($m, $d, $y)` | `strtotime` accepts ISO formats and `now`/`today`/`tomorrow`/`yesterday` |
| `random_int($min, $max)`, `random_token($bytes = 32)` | The OS's cryptographic random number generator (do not use `rand()` for cryptography) |
| `password_hash($pw)`, `password_verify($pw, $hash)` | Argon2id |
| `hash("sha256" \| "sha384" \| "sha512", $data)`, `hash_hmac($algo, $data, $key)`, `hash_equals($a, $b)` | `hash_equals` takes a comparison time that does not depend on the contents |
| `base64_encode`, `base64_decode: ?string`, `urlencode`, `rawurlencode`, `urldecode`, `http_build_query` | |
| `json_decode($s): any` | Depth up to 128 |

### Content-Security-Policy

Page responses get the following policy by default (`{nonce}` is a per-response `'nonce-...'`; writing `'nonce-{nonce}'` is equivalent).

```
default-src 'self'; script-src 'self' {nonce}; style-src 'self' 'unsafe-inline';
img-src 'self' data: https:; font-src 'self' data:; object-src 'none'; base-uri 'self';
form-action 'self'; frame-ancestors 'self'
```

- PHX automatically adds `nonce="..."` to `<script>` tags written in templates (`.phx.html` and the HTML parts of `.phx`).
  As a result, inline scripts you write yourself work as is, while scripts injected by getting
  past escaping (such as a `<script>` passed to `raw()`) do not run.
- Event attributes such as `onclick="..."` and `javascript:` URLs are blocked. Use `addEventListener`.
  Writing them in a template produces a compile-time warning (not when `csp` allows inline code).
- When a `<script>` built from strings needs a nonce, use `csp_nonce()`
  (`raw("<script nonce=\"" . csp_nonce() . "\">...</script>")`).
- Static files and error pages get a policy without a nonce (inline scripts in static `.html` files do not run).
- Changing the policy: `[security] csp = "off"` omits it. Writing a policy, as in `csp = "default-src 'self'; script-src 'self' {nonce}"`,
  uses that policy (`{nonce}` becomes `'nonce-...'`).

### Rate limiting

```phx
if (!rate_limit("login:" . request()->ip, 5, 60)) {     // up to 5 times per 60 seconds
    response()->status(429);
    echo "Please wait a moment and try again.";
    return;
}
// after a successful login
rate_limit_reset("login:" . request()->ip);
```

- `rate_limit($key, $max, $seconds)` counts one use and returns `true` if the key has not yet reached `$max` in the current window,
  and returns `false` if it has. Counters are shared within the server process and are lost on restart.
- The same key is counted separately for each window length (you can apply both 5 per minute and 50 per day). When there are more than 100,000 keys,
  keys that have not reached their limit are discarded first, so using a large number of keys does not release keys that are currently limited.
- `requests_per_minute` counts IPv6 addresses grouped by /64.
- The site-wide limit is `[security] requests_per_minute` (§14).

### Permissions (phx.toml)

A script can touch nothing outside the heap unless it is permitted in `phx.toml`. **If nothing is written, nothing is permitted.**

```toml
[permissions]
read = ["content/"]         # directories readable with file_get_contents / file_exists
write = ["data/", "storage/"]  # file_put_contents / unlink / saveTo / Database::open (reading is also allowed)
env = ["APP_*"]             # environment variables readable with getenv (a trailing * is a prefix match)
```

- Paths are relative to the project. `..` and absolute paths are not allowed.
- Symbolic links and junctions under a permitted directory are not followed (a path through a link raises `PermissionError`).
  Paths are opened one level at a time from the permitted directory, rejecting links, so a directory cannot be swapped
  between the check and the use.
- Only regular files with exactly one hard link can be opened. If a hard link to a file outside is planted, it is neither read nor written.
- File names cannot contain `:` (Windows alternate data streams), trailing dots or spaces, or device names such as `CON` `NUL` `COM1`.
- On Unix, directories that other users can write to (`o+w`) cannot be used.
- Locations that are loaded as code cannot be specified in `write`: `public/`, the project root, directories whose names start with `.`
  (`.phx-cache/`, `.git/`, etc.), `vendor/`, and the directories of packages in `[dependencies]`.
- Anything not permitted raises `PermissionError` (the message shows what to write in `phx.toml`).

### phx.toml

```toml
[project]
name = "blog"

[security]
csrf = true             # default: true
secure_cookies = false  # true when serving over HTTPS
session_ttl = 7200      # seconds
memory_limit_mb = 128   # per request (one execution)
csp = "default"         # "off" or a custom policy that may contain {nonce}
requests_per_minute = 0 # limit per client address (0 means unlimited)
session_store = "memory"  # or a directory ("sessions/")

[permissions]
write = ["data/"]

[dependencies]
utils = { path = "../utils" }
markdown = { git = "https://example.com/markdown.git", rev = "v1.2.0" }
```

The directory that contains `phx.toml` is the project root. Unknown keys are errors (so typos are caught).
`phx serve` reads the file at startup, so restart the server after changing it.

## 15. Builtin functions

### Global functions

| Function | Description |
|---|---|
| `count(list\|map): int` | Number of elements |
| `strlen(string): int` | Number of bytes (use `->len()` for the number of characters) |
| `implode(string $sep, list<string>): string` / `explode(string $sep, string): list<string>` | Join / split |
| `str_repeat`, `str_contains`, `str_starts_with`, `str_ends_with`, `str_replace(search, replace, subject)` | Same as PHP |
| `strtoupper`, `strtolower`, `ucfirst`, `trim` | Same as PHP |
| `json_encode(any): string` | Outputs only the public properties of objects |
| `dump(any): void` | Equivalent to `var_dump` |
| `raw(string): Html`, `escape(string): Html`, `htmlspecialchars(string): Html`, `nl2br(string): Html` | §13 |
| `intdiv`, `abs`, `min`, `max`, `floor`, `ceil`, `round`, `sqrt`, `fmod` | Numeric |
| `number_format(float, int $decimals = 0): string` | `1,234.50` |
| `range(int, int): list<int>` | Includes both ends |
| `time(): int`, `microtime(): float` | Current time |
| `rand(int, int): int` | Pseudo-random number (not for cryptography; use `random_int`) |
| `json_decode(string): any`, `json_html(any): SafeJs`, `safe_url(string): SafeUrl` | §13, §14 |
| `csrf_token(): string`, `csrf_field(): Html` | §14 |
| `csp_nonce(): string` | The CSP nonce for this response (§14) |
| `rate_limit(string, int, int): bool`, `rate_limit_reset(string)` | §14 |
| `date`, `gmdate`, `mktime`, `strtotime`, `checkdate` | Dates (table in §14) |
| `random_int`, `random_token`, `password_hash`, `password_verify`, `hash`, `hash_hmac`, `hash_equals` | Cryptography (table in §14) |
| `base64_encode`, `base64_decode`, `urlencode`, `rawurlencode`, `urldecode`, `http_build_query` | Encoding |
| `file_get_contents`, `file_put_contents`, `file_exists`, `unlink`, `getenv` | Require `[permissions]` |
| `sql_ident(string): Sql` | Column and table names |
| `assert(bool, ?string)`, `assert_eq(any, any, ?string)` | For tests |

### string methods

`len()` (number of characters), `isEmpty()`, `upper()`, `lower()`, `ucfirst()`, `trim()`, `ltrim()`, `rtrim()`,
`contains($s)`, `startsWith($s)`, `endsWith($s)`, `replace($search, $replace)`, `split($sep)`,
`slice($start, $length?)` (in characters; negative values count from the end), `indexOf($s): ?int`, `repeat($n)`,
`padStart($len, $pad)`, `padEnd($len, $pad)`, `toInt(): ?int`, `toFloat(): ?float`, `chars()`

### list methods

`len()`, `isEmpty()`, `contains($v)`, `indexOf($v): ?int`, `first(): ?T`, `last(): ?T`,
`slice($start, $length?)`, `reverse()`, `concat($other)`, `unique()`, `sorted()`, `sortBy(fn($a, $b) => int)`,
`map(fn($x) => ...)`, `filter(fn($x) => bool)`, `find(fn($x) => bool): ?T`, `any(...)`, `all(...)`,
`join($sep)`, `sum()`

Append an element with `$xs[] = $v;` and update one with `$xs[$i] = $v;` (an index out of range raises `IndexError`).

### map methods

`len()`, `isEmpty()`, `has($k)`, `get($k): ?V`, `keys()`, `values()`, `merge($other)`

Add or update with `$m[$k] = $v;`. Reading a missing key with `$m[$k]` raises `KeyError`; `$m[$k] ?? $default` does not throw.

When code uses a PHP function name, the error message suggests the alternative
(for example, `strpos` → `->indexOf()`, `isset` → `??`, `var_dump` → `dump()`, `eval` → not available).

## 16. Tools

| Command | Description |
|---|---|
| `phx run file.phx` | Type-check and run (`--html`, `--time-limit seconds`) |
| `phx check file.phx` | Type-check only (including imported modules and templates) |
| `phx serve dir` | Web server (`--port`, `--host`, `--prod`, `--time-limit`) |
| `phx test [paths]` | Run the `test_*` functions in `*_test.phx` files (a new VM for each test) |
| `phx fmt [paths] [--check]` | Normalize indentation and whitespace (strings, comments, and HTML are not changed) |
| `phx lsp` | Language Server for editors (errors as you type, types on hover) |
| `phx install` | Fetch `[dependencies]` and pin them in `phx.lock` |
| `phx migrate file.php` | Convert PHP mechanically and list the places that need manual work |
| `phx disasm file.phx` | Show the bytecode |
| `phx parse file.phx` | Show the syntax tree |

### Testing

```phx
// tests/posts_test.phx
import lib.posts.find_post;

function test_find_post(): void {
    assert_eq(find_post(2)?->title, "Types without ceremony");
    assert(find_post(99) == null, "missing posts are null");
}
```

When `assert(bool, ?message)` or `assert_eq(expected, actual, ?message)` fails, it raises `AssertionError`.

### Packages

```toml
[dependencies]
textutils = { git = "https://github.com/example/textutils.git", rev = "v1" }
local = { path = "../shared" }
```

```phx
import textutils.strings.slugify;      // vendor/textutils/strings.phx
```

- `git` packages are fetched into `vendor/<name>`, and the exact commit is recorded in `phx.lock`.
  From the second install on, the commit in the lock file is fetched, and a mismatch is an error.
- Only `https://`, `ssh://`, `git@host:...`, and `file://` URLs are allowed.
- Packages are PHX source code. No package code runs at install time.
- List a package's dependencies alongside it in the app's `phx.toml` (transitive dependency resolution is not yet supported).

### Migration

```
phx migrate legacy/page.php      # writes page.phx
```

Converts `<?php`→`<?phx`, `===`→`==`, `array(...)`→`[...]`, `isset($x)`→`($x != null)`, `$_GET`/`$_POST`/`$_COOKIE`/`$_SESSION`
→ `request()`/`session()`, `var_dump`/`print_r`→`dump`, and `print`→`echo`, and adds `TODO(migrate)` comments to `require` and `global`.
It then runs the type checker and shows the places that need manual work, such as type annotations.

## 17. How execution works

1. Lexing and parsing (hand-written recursive descent + Pratt parser)
2. Type checking (name resolution, inference, narrowing, definite assignment analysis) → typed intermediate representation (HIR)
3. Compilation to register-based bytecode (type-specialized instructions such as `AddI`, `LtI`, and `JumpIfNotLtIImm`)
4. Execution
   - **Interpreter**: written entirely in safe Rust. Strings, lists, and maps use reference counting + copy-on-write.
   - **JIT** (Cranelift): functions in which every value is an `int`/`bool` (numeric loops and recursion) are compiled to native code.
     Overflow, modulo by zero, negative shifts, recursion depth, and the execution time and memory limits are checked in the JIT just as in the interpreter.
     It can be disabled with `PHX_JIT=0`. The stack trace of an exception raised inside the JIT goes back only as far as the caller.
5. **Bytecode cache**: `phx run` and `phx serve` save compiled output in `.phx-cache/` and reuse it
   if neither the source files nor the `phx` binary have changed.
   - The cache is signed with an HMAC using a per-user key (`%LOCALAPPDATA%\phx\cache.key`, `~/.cache/phx/cache.key`), and entries whose
     signature does not match are not read. Even if a project received from someone else contains `.phx-cache/`,
     code that differs from the source never runs.
   - Entries are per script and per project. Each records the contents (SHA-256) of all the files it depends on, and these are verified on load
     (old code is not used even after a copy that preserves modification times or a rewrite that keeps the same length).
   - If `.phx-cache/` is a link, it is not used. Files inside it are neither read nor written if they are links or hard links.

### Runtime limits

| Limit | Default | When exceeded |
|---|---|---|
| Execution time | `serve`: 30 seconds (`--time-limit`), `run`: none | Fatal error (cannot be caught) |
| Memory | 128 MB (`[security] memory_limit_mb`) | Fatal error |
| Call depth | 10,000 | Fatal error |
| Request body | 8 MB | 413 |

`unsafe` is used only in the allocator that counts memory usage (`phx_vm/src/alloc.rs`) and in the JIT
(`phx_jit`, for calling generated code).

## 18. Roadmap

- Generic classes, annotations (attributes)
- Asynchronous I/O, HTTP client (`[permissions].net`)
- Wider JIT coverage (functions that handle floats and strings), AOT compilation
- Transitive dependency resolution, a package registry
- Go-to-definition and completion in the LSP, line wrapping in `phx fmt`
- Rate limiting shared across multiple processes, sessions stored in a database

See [security-audit.md](security-audit.md) for the results of the security audit and the remaining risks.
