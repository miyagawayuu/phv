# PHV Language Specification

English | [日本語](spec.ja.md)

Website: <https://phv-lang.org>

PHV is a language for the web. It keeps what is good about PHP: you save a file and it runs, it is built for the web, and it embeds in HTML.
On top of that, the language itself guarantees type safety, security, execution speed, and maintainability.
It is not compatible with existing PHP code, but it aims to be something PHP programmers can read and write right away.

## 1. Design Principles

| PHP weakness | How PHV solves it |
|---|---|
| Implicit conversion (`"1" + 1`, `"abc" == 0`) | No implicit conversion. `==` is always a strict comparison. Comparing values of different types is a compile error |
| Runtime errors caused by null | The `?T` type. A value cannot be used until it is null-checked (`!= null`, `is`, `??`, `?->`) |
| XSS | `<?= ?>` and `echo` to a web page are HTML-escaped automatically. Only values of type `Html` are not escaped |
| `eval`, variable variables, `extract`, dynamic `include` | Not in the language. Modules use static `import`; templates use literal paths only |
| SQL injection | SQL can only be written as an `sql"..."` literal (type `Sql`), and embedded values always become placeholders |
| Escaping that ignores context | The position of `<?= ?>` (attribute, URL, `<script>`, CSS) is analyzed, and inserting into a place that escaping cannot protect is a compile error |
| CSRF and session fixation | The server checks the token on form POSTs automatically. Only session IDs issued by the server are accepted |
| Scripts that can do anything | Access to files and environment variables is limited to what `[permissions]` in `phv.toml` allows |
| Execution of injected scripts | The default Content-Security-Policy blocks inline scripts other than the `<script>` elements written in templates |
| Header injection | A header value that contains a newline raises `ValueError` |
| Path traversal | The server does not serve files outside `public/`, dotfiles, or templates |
| State leaking between requests | Each request gets a fresh heap. There are no static properties and no global variables |
| Execution speed | A register-based bytecode VM specialized for static types |
| Hard to maintain at scale | Functions with type annotations, modules and `export`, readable error messages |

## 2. Files

- `.phv` … A script or module. Starts in **code mode**. The leading `<?phv` is optional.
- `.phv.html` … A template. Starts in **HTML mode**.
- Writing `?>` in code mode switches to HTML mode. In HTML mode, `<?phv` (or `<? `) switches back to code,
  and `<?= expr ?>` outputs a value. A single newline right after `?>` is not output (same as PHP).
- The character encoding is UTF-8.

## 3. Lexical Structure

- Variables are written `$name`. There is no `$$name` (variable variables).
- Comments are `// ...`, `# ...`, and `/* ... */`. `#[` starts an annotation, not a comment (§9).
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

  ```phv
  $count = 1;
  $count = "two";   // error: expected `int`, found `string`
  ```

- When the type cannot be inferred (`null` or an empty `[]`), declare the variable with a type.

  ```phv
  ?User $found = null;
  list<string> $names = [];
  map<string, int> $counts = [];
  ```

- A variable's scope is the whole function (same as PHP). However, **a variable that is assigned only on some paths cannot be used**.

  ```phv
  if ($ok) { $msg = "hi"; }
  echo $msg;   // error: variable `$msg` might not be defined here
  ```

- A `foreach` or `catch` variable whose type differs from an existing variable with the same name is a new variable that shadows it
  (so `catch (A $e)` and `catch (B $e)` can be written one after another).
- `$this` can be used only inside non-static methods. It cannot be assigned.

### Type Narrowing

Conditions narrow the types of local variables.

```phv
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

```phv
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

```phv
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

```phv
try {
    risky();
} catch (NotFound | InvalidArgumentException $e) {
    echo $e->getMessage();
}
throw new RuntimeException("boom");
$port = $config["port"] ?? throw new ValueError("port is required");
```

```phv
try {
    $db->execute(sql"...");
} finally {
    $lock->release();      // runs whether the block exits by return / break / continue / exception
}
```

Only `Exception` and its subclasses can be thrown.

## 8. Functions

```phv
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

```phv
$double = fn(int $x): int => $x * 2;           // captures outer variables automatically
$names->map(fn($n) => $n->upper());            // parameter types are inferred from context
$counter = function () use ($start): int { return $start + 1; };
```

- All captures are **by value** (a copy taken when the closure is created). There is no capture by reference (`&$x`).
- A closure created inside a method can use `$this`.

## 9. Classes

```phv
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

```phv
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

```phv
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

```phv
const MAX_ITEMS = 50;            // at the top level of a file (can also be exported / imported)
class Config {
    const VERSION = "1.0";
}
echo MAX_ITEMS, Config::VERSION, self::VERSION;
```

Constant values must be literals (int, float, string, bool, null).

### Generic Functions

```phv
function first_or<T>(list<T> $xs, T $fallback): T {
    return $xs->isEmpty() ? $fallback : $xs[0];
}
$n = first_or([5, 6], 0);        // int
$s = first_or([], "none");       // string
```

Type arguments are inferred from the call's arguments (up to 4). Inside the function, `T` is treated as an opaque type,
and operators and methods cannot be used on values of type `T`.

### Generic Classes

```phv
class Box<T> {
    public function __construct(public T $value) {}
    public function map<U>(fn(T): U $f): Box<U> {
        return new Box($f($this->value));
    }
}

interface Repo<T> {
    public function find(int $id): ?T;
}

class UserRepo implements Repo<User> {
    public function find(int $id): ?User { /* ... */ }
}

$b = new Box(41);                 // Box<int>, inferred from the constructor
$s = new Box<string>("x");        // written
Box<float> $f = new Box(1);       // from the declared type: Box<float>
```

- Classes and interfaces can take type parameters (up to 4), which can be used in their properties and in the
  signatures and bodies of their instance methods. Static methods do not see them (they have no instance). Enums
  cannot be generic.
- A generic class is always used with its type arguments: `Box<int>`, `Repo<User>`, `extends Base<list<T>>`.
  `new` infers them from the declared type of the variable it is assigned to or from the constructor's arguments, or
  they are written (`new Box<int>(1)`). If neither gives them, it is an error.
- Members read through `Box<int>` have `T` replaced by `int`, also when they are inherited (`class Named extends
  Box<string>` gives `get(): string`). An overriding method uses the parent's types as seen through the subclass.
- Type arguments are invariant: `Box<int>` cannot be used where `Box<any>` or `Box<float>` is expected (a `Box<any>`
  could otherwise be given a string).
- Type arguments exist only at compile time. `is` checks the class: `$x is Box` (writing `is Box<int>` is an error),
  and narrows to `Box<any>`.

### Annotations

```phv
#[Deprecated("use total()")]
function sum(list<int> $xs): int { return total($xs); }

class Square extends Shape {
    #[Override]
    public function area(): float { return $this->side ** 2; }
}
```

Annotations are written before a function, class, method or property, as `#[Name]` or `#[Name("reason")]`; several
can be given as `#[A, B]` or on separate lines. PHV knows these (any other name is an error, so a typo is not ignored):

| Annotation | On | Effect |
|---|---|---|
| `#[Deprecated]`, `#[Deprecated("reason")]` | function, method, class, property | A warning where it is used (calls, `new`, static calls, property reads), except inside its own class |
| `#[Override]` | method | An error unless a parent class or an interface declares a method of that name (not a private one) |
| `#[Test]` | function | `phv test` runs it, whatever its name (§16) |
| `#[Skip]`, `#[Skip("reason")]` | function | `phv test` reports it as skipped instead of running it |

Annotations are checked at compile time and are not available at run time.


## 10. Exception Classes

Available from every file.

```
Exception                    (getMessage(): string)
├── RuntimeException
│   ├── DatabaseError        SQL error
│   └── NetworkError         WebSocket connection failure
├── LogicException
│   └── InvalidArgumentException
├── ArithmeticError          integer overflow, etc.
│   └── DivisionByZeroError  division or remainder by 0
├── IndexError               out-of-range access on a list
├── KeyError                 key not in a map
├── ValueError               invalid argument (a newline in a header, etc.)
├── UnhandledMatchError
├── PermissionError          access to a file or environment variable not allowed in phv.toml
└── AssertionError           failure of assert() / assert_eq()
```

Fatal errors, such as exceeding the execution time limit, cannot be caught.

## 11. Modules

```phv
// lib/posts.phv
export class Post { ... }
export function all_posts(): list<Post> { ... }
function helper(): int { ... }      // anything not exported is not visible from outside
```

```phv
import lib.posts.{Post, all_posts};
import lib.posts.Post;
import std.http.{request, response};
```

- `import a.b.{x, y};` loads `a/b.phv` relative to the project root (the directory that contains `phv.toml`, or, if there is none,
  the directory of the file being run).
- The top level of an imported module can contain only declarations (`function`, `class`, `interface`, `enum`, `const`, `import`).
  Loading a module has no side effects.
- `std.*` are the standard modules (`std.http`, `std.db`, `std.net`, `std.async`). Packages listed in `[dependencies]` are loaded with `import package_name.module`.
- Only files inside the project and package directories can be loaded (symbolic links cannot be used to escape them).

## 12. Templates

```html
<?phv
import lib.posts.Post;
params(string $title, list<Post> $posts);
?>
<h1><?= $title ?></h1>
<?phv foreach ($posts as $post): ?>
  <li><a href="/post?id=<?= $post->id ?>"><?= $post->title ?></a></li>
<?phv endforeach; ?>
```

```phv
$html = render("views/list.phv.html", title: "Posts", posts: $posts);   // Html
echo render("views/layout.phv.html", title: "Home", body: $html);
```

- A template declares typed parameters with `params(...)`. Calls to `render()` are type-checked at compile time.
- The path given to `render()` is a string literal, relative to the project root (`..` is not allowed). The extension is `.phv.html`.
- `render()` returns `Html`, so the result can be passed directly to another template and embedded there (layouts).
- A template must close everything it opens: it cannot end inside a tag, an attribute, a comment, a `<script>` /
  `<style>` / `<textarea>` element or `<svg>` / `<math>` (compile error). `Html` values can be joined with `.`, and a
  template that ended at `<a href="` would put the next value at the start of a URL without the check above.

## 13. Output and escaping

There are two kinds of output destination: text and HTML.

| Situation | Output destination |
|---|---|
| A script run with `phv run` | Text (HTML with `--html`) |
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
| URL of a `<script>` | `<script src="<?= $u ?>">` (and `href` in SVG) | Error, also for `SafeUrl`: the tag gets the CSP nonce, so any URL there would run as a trusted script. Write the URL in the template |
| `content` of a `<meta>` | `<meta http-equiv="refresh" content="0;url=<?= $u ?>">` | Only numbers, unless a `name`, `property` or `itemprop` attribute comes before `content` (`<meta property="og:title" content="<?= $t ?>">` is text) |

- The bodies of functions and closures, and `if` and loop blocks, must end in the same HTML state they started in
  (leaving a `<script>` or an attribute open at the end of a block is an error). This prevents the context from drifting with call order or repetition.
- Expressions and statements can be nested up to 500 levels (this prevents deeply nested source from overflowing the implementation's stack).

## 14. Web server

```
phv serve examples/site --port 8080
```

- The site directory must contain `public/`. URLs are determined by file names:
  `/` → `public/index.phv`, `/about` → `public/about.phv` or `public/about/index.phv`. `/about.phv` also works.
- Only files with known extensions (html, css, js, json, txt, images, fonts, pdf, audio, video, wasm) are served as static files.
- Not served: anything outside `public/`, files and directories whose names start with `.` or `_` (`_partial.phv` can be used as a partial),
  `.phv.html` (templates), and unknown extensions (`.php`, `.bak`, `.sql`, etc.). The URL must match the file name's spelling
  exactly (`INDEX.PHV`, Windows short names, and `::$DATA` do not open the file).
- Each request runs in a new VM (a new heap). Only the compiled bytecode is shared.
- When a file is saved, it is recompiled on the next request (the modification times of all the files it depends on are checked).
- Limits: execution time 30 seconds (`--time-limit`), memory 128 MB (`[security] memory_limit_mb`), body 8 MB, receiving headers 10 seconds,
  receiving the body 30 seconds, 1024 concurrent connections, 1000 query and form fields. A connection is closed when the
  client accepts no data for 30 seconds. Static files (and `response()->file()`) are sent from disk as they are read,
  so they do not take memory.
- At most `[server] workers` scripts run at once (default: two per CPU, at least 8); a request waits up to 10 seconds
  for one. A request that cannot get one in time, or that arrives while the output waiting to be sent to clients is
  more than the workers could produce (`workers` × `memory_limit_mb`), gets `503 Service Unavailable` with
  `Retry-After: 5`. Static files do not need a worker. So the memory a server uses stays around twice
  `workers` × `memory_limit_mb`, however many clients connect.
- If `[security] requests_per_minute` is set, the number of requests accepted per minute from a single client address is limited to that number
  (beyond it, the response is `429 Too Many Requests` with `Retry-After`). The default is unlimited. With
  `[security] rate_limit_store`, all server processes share the count (see "Rate limiting").
- Headers sent by default: `X-Content-Type-Options: nosniff`, `X-Frame-Options: SAMEORIGIN`,
  `Referrer-Policy: strict-origin-when-cross-origin`, and Content-Security-Policy (see "Content-Security-Policy" below).
  HSTS if `secure_cookies = true`. `Cache-Control: private, no-store` on responses that use sessions or cookies, or
  that carry a CSRF token (`csrf_field()`, `csrf_token()`).
  Version information such as `Server` / `X-Powered-By` is not sent. Scripts cannot set transport headers such as `Content-Length`.
- In development mode, the details of compile errors and runtime errors are shown in the browser. Development mode is the default only
  when listening on `127.0.0.1` / `localhost` (it can be set explicitly with `--dev` / `--prod`). **Behind a reverse proxy, which
  reaches the server on `127.0.0.1`, use `--prod`**: otherwise visitors see the details (the server prints a note at startup).

### std.http

```phv
import std.http.{request, response, session};

$req = request();
$req->method;              // "GET"
$req->path;                // "/post"
$req->query("id");         // ?string
$req->form("name");        // ?string (urlencoded / multipart; the last one if repeated)
$req->formAll("ids");      // list<string>: every value of a repeated field, such as checkboxes
$req->queryAll("tag");     // list<string>: every value of ?tag=a&tag=b
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

```phv
import std.http.session;

session()->set("user_id", (string)$user->id);
$id = session()->get("user_id");      // ?string
session()->regenerate();              // call right after login (protects against session fixation)
session()->flash("notice");           // a message that is removed once read
session()->destroy();                 // log out
```

- Values are stored on the server. The cookie holds only a 256-bit random ID
  (`PHVSESSID`, `HttpOnly`, `SameSite=Lax`, and `Secure` with `[security] secure_cookies = true`).
- IDs that the server did not issue are rejected. The values in one session can total at most 64 KB.
- A session is removed after `[security] session_ttl` seconds without access. Even with continued access, it is removed after 24 hours.
- By default, sessions are stored in memory and are lost when the server restarts. If you set a directory in the project,
  as in `[security] session_store = "sessions/"`, each session is stored in its own file. Sessions then survive restarts and can be shared by
  multiple server processes that use the same directory. File names are hashes of the IDs, so the IDs cannot be learned from a directory listing.
  A directory inside `public/` cannot be specified. A new session's file is named `n-<hash>.json` until its visitor
  sends the cookie back, and is then renamed to `<hash>.json`.
- The store holds at most 50,000 sessions (the file store also at most 256 MB). When it is full, sessions whose visitor
  never came back are removed first, then the least recently used: a flood of requests without a cookie (each creating
  a session) removes its own sessions, not those of returning visitors.

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

The token is an HMAC of a per-browser cookie (`PHVCSRF`), so this relies on attackers being unable to overwrite that cookie.
In production, serve over HTTPS and set `secure_cookies = true` (the cookie gets the `__Host-` prefix and can no longer be overwritten
over plaintext connections or from subdomains).
It can be disabled with `[security] csrf = false`.

### Cookie

```phv
request()->cookie("theme");                                   // ?string
response()->cookie("theme", "dark", maxAge: 86400 * 30);      // HttpOnly; SameSite=Lax by default
response()->deleteCookie("theme");
```

Characters not allowed in names, line breaks, and `SameSite=None` without `Secure` raise `ValueError`.

### Uploads

```phv
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

```phv
$data = request()->json();             // any (invalid JSON raises ValueError)
if ($data is map) {
    $name = $data["name"] ?? null;
    if ($name is string) { ... }
}
response()->json(["ok" => true]);
```

### Database (std.db)

```phv
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

#### PostgreSQL and MySQL

Database servers are named in `phv.toml`; scripts only use the name, so passwords never appear in the code.

```toml
[databases.main]
url_env = "DATABASE_URL"      # postgres://user:pass@host/db, mysql://user:pass@host/db
# url = "postgres://app@localhost/app"   # or the URL itself (for local development)
# tls = false                 # allow a plain connection to a remote host (not recommended)
# pool_size = 8               # connections kept per server process (1 to 256)
```

```phv
$db = Database::connect("main");
$row = $db->first(sql"INSERT INTO posts (title) VALUES ({$title}) RETURNING id");   // PostgreSQL
$db->execute(sql"INSERT INTO posts (title) VALUES ({$title})");
$id = $db->lastInsertId();                                                          // MySQL
```

The methods are the same as for SQLite (`query`, `first`, `execute`, `transaction`, `close`).

- Values are always sent as bound parameters (`$1` in PostgreSQL, `?` in MySQL). A `?` written in the SQL text,
  such as PostgreSQL's `jsonb ? 'key'`, stays as written.
- **TLS is required for every host except `localhost`**, and the server certificate is checked against the
  public CAs (webpki-roots). Servers with a certificate from a private CA are not supported yet; `tls = false`
  turns TLS off for such hosts.
- With `url_env`, an unset variable is reported when a script connects, so `phv check` works without the
  production credentials. The URL is never shown to scripts or in error messages.
- Connections are pooled per server process. When a script ends (or calls `close()`), its connection goes back
  to the pool after the session is reset: an open transaction is rolled back, and temporary tables and session
  settings are cleared (PostgreSQL: cursors, `SET`s, roles, `LISTEN`, advisory locks, temporary tables and prepared
  statements; MySQL `COM_RESET_CONNECTION`, the active role and the database of the URL). Connecting to the same name
  twice in one script gives the same connection. When every connection is in use, `connect()` waits up to the
  script's time limit (30 seconds at most) and then raises `DatabaseError`.
- A statement running past the script's time limit is stopped: by the server (PostgreSQL `statement_timeout`,
  MySQL `max_execution_time`, which applies to `SELECT`) and, also when a script changes those settings, on the client
  side (PostgreSQL connections stop reading at the deadline; MySQL statements are stopped with `KILL QUERY`). A result
  larger than the memory left to the script is refused while it is read (the connection is then closed, not reused), so
  a large result or a misbehaving server cannot grow the process past the memory limit. PostgreSQL notifications
  (`LISTEN`) are dropped as they arrive; scripts cannot read them.
- Each call runs a single statement (MySQL multi-statements are off), and `LOAD DATA LOCAL INFILE` is refused.
- Value types: PostgreSQL `bool` → `bool`; integer types → `int`; `real`/`double precision` → `float`;
  `numeric`, `json`/`jsonb`, `uuid`, dates and times (`YYYY-MM-DD HH:MM:SS[.ffffff]`, `timestamptz` in UTC),
  `bytea` (`\x...` hex) and text types → `string`. Other types (arrays, `interval`, ...) raise `DatabaseError`:
  cast them in the SQL (`col::text`). Parameters are converted to the column type, so a string can be passed for
  a `date`, `numeric`, `uuid` or `jsonb` column. MySQL `DECIMAL`, `JSON`, dates and times are `string`, and
  `TINYINT(1)` is `int` (`Row::bool()` accepts it).
- Give the application a database user with only the privileges it needs (no `SUPERUSER`, no `FILE`).

#### Redis

```toml
[databases.cache]
url_env = "REDIS_URL"         # redis://localhost, or rediss://user:pass@host for TLS
```

```phv
import std.db.Redis;

$redis = Redis::connect("cache");
$redis->set("greeting", "hello", 60);          // expires after 60 seconds (0: never)
$redis->get("greeting");                       // ?string
$redis->incr("visits");                        // int (incr($key, $by = 1))
$redis->hset("user:1", "name", "Ann");  $redis->hgetAll("user:1");   // map<string, string>
$redis->rpush("jobs", "a");  $redis->lpop("jobs");  $redis->lrange("jobs", 0, -1);
```

Methods: `get`, `set`, `delete`, `exists`, `incr`, `expire`, `ttl`, `hget`, `hset`, `hdel`, `hgetAll`, `rpush`,
`lpop`, `lrange`, `close`. Each command waits at most until the script's time limit, and a reply larger than the
memory left to the script is refused. Only these commands can be sent: commands such as `KEYS`, `FLUSHALL`, `CONFIG` or
`EVAL` are not available. A remote server needs a `rediss://` URL (or `tls = false`). Errors raise `DatabaseError`.

### WebSocket (std.net)

WebSocket servers are named in `phv.toml` like database servers. Scripts only use the name, so a URL with a
password in it (`ws://host:port/secret`) never appears in the code, and scripts cannot connect anywhere else.

```toml
[websockets.game]
url_env = "GAME_WS_URL"       # ws:// or wss://
# url = "ws://127.0.0.1:28016/secret"   # or the URL itself (for local development)
# tls = false                 # allow ws:// to a remote host (not recommended)
```

```phv
import std.net.WebSocket;

$ws = WebSocket::connect("game");
$ws->send(json_encode(["Identifier" => 1, "Message" => "status"]));
$reply = $ws->receive(5.0);       // ?string: null when no message arrives within 5 seconds
$ws->close();
```

- Each `connect()` opens a new connection, which is closed by `close()` or when the script ends. Nothing is shared
  between requests.
- Only text messages are sent and received. A binary message from the server raises `NetworkError`. Pings are
  answered while reading.
- `receive()` waits at most until the script's time limit, also when the server keeps sending pings or a message one
  byte at a time (every read and write stops at the deadline). A message is at most 16 MB, and a script can hold
  16 connections at once.
- **TLS (`wss://`) is required for every host except `localhost`**, with the server certificate checked against the
  public CAs. `tls = false` allows `ws://` to such a host.
- Connection failures, a server that closes the connection, and network errors raise `NetworkError`. The URL is
  never shown in messages.

### HTTP client (std.net)

HTTP services are named in `phv.toml` too. Scripts give the name and a path; the base URL and the API keys stay in
the configuration, so they never appear in the code, and scripts cannot send requests to any other host.

```toml
[http.weather]
url = "https://api.example.com/v1"                 # or url_env = "WEATHER_URL"
headers = { "User-Agent" = "my-site" }             # sent with every request
headers_env = { Authorization = "WEATHER_AUTH" }   # values from environment variables (API keys)
# tls = false                                      # allow http:// to a remote host (not recommended)
```

```phv
import std.net.HttpClient;

$api = HttpClient::connect("weather");
$res = $api->get("/forecast", ["city" => "Tokyo"]);   // GET https://api.example.com/v1/forecast?city=Tokyo
if ($res->ok()) {
    $data = $res->json();                              // any
}
$res = $api->postJson("/reports", ["city" => "Tokyo", "temp" => 21]);
echo $res->status, " ", $res->header("content-type") ?? "", "\n", $res->body;
```

| Method | Sends |
|---|---|
| `get($path, $query = [], $headers = [])`, `delete(...)` | A request without a body; `$query` is added as `?k=v&...` |
| `post($path, $body = "", $headers = [])`, `put(...)`, `patch(...)` | `$body` as it is |
| `postJson($path, $data, $headers = [])`, `putJson(...)` | `json_encode($data)` with `Content-Type: application/json` |
| `postForm($path, $fields, $headers = [])` | A form (`application/x-www-form-urlencoded`) |
| `request($method, $path, $body = "", $headers = [], $query = [])` | GET, HEAD, POST, PUT, PATCH, DELETE or OPTIONS |

The result is an `HttpResponse`: `status` (int), `headers` (`map<string, string>`, lowercase names; repeated headers
joined with `, `), `body` (string), `ok()` (2xx), `header($name)` (`?string`, any case) and `json()` (`ValueError`
when the body is not JSON).

- The path must start with `/` and is added to the base URL's path. Paths that could leave it are refused with
  `ValueError` before anything is sent: `//host`, `.` and `..` segments (also percent-encoded, encoded twice as
  `%252e`, or followed by `;` as in `..;`), encoded slashes, `%` not followed by two hex digits, and `?` or `#` (give
  the query as `$query`). Characters outside ASCII are percent-encoded.
- Header names and values are checked (no line breaks). `Host`, `Content-Length`, `Transfer-Encoding`,
  `Connection` and the other headers that frame the message are set by PHV. Scripts can replace the `headers` of
  `phv.toml`, but not those of `headers_env` (also not under another spelling: `_` counts as `-`, as CGI servers read
  both the same way). A query parameter of the base URL cannot be given again in `$query`.
- Errors come back as responses: a 4xx or 5xx status does not throw. **Redirects are not followed**: a 3xx response
  is returned with its `Location` header.
- **TLS (`https://`) is required for every host except `localhost`**, with the server certificate checked against the
  public CAs. `tls = false` allows `http://` to such a host.
- Each request uses its own connection and waits at most until the script's time limit (30 seconds when it has none),
  whatever the server sends: the name lookup, every read and every write stop at that deadline, and at most 10
  interim (`1xx`) responses are skipped. Request and response bodies are at most 16 MB. The body is read as UTF-8 text.
- Connection failures and network errors raise `NetworkError`, naming the service but never its URL, host or headers
  (errors are described in fixed words, such as "the server's TLS certificate is not valid"). An
  unset `url_env` or `headers_env` variable is reported when a request is sent, so `phv check` works without the
  production keys.

### Asynchronous I/O (std.async)

```phv
import std.async.{await, await_all};
import std.net.HttpClient;
import std.db.Database;

$api = HttpClient::connect("weather");
$today = $api->getAsync("/today");          // sent now
$tomorrow = $api->getAsync("/tomorrow");    // sent at the same time
$db = Database::connect("main");
$posts = $db->queryAsync(sql"SELECT id, title FROM posts");

$t = await($today);                         // HttpResponse
foreach (await($posts) as $row) { /* ... */ }
$all = await_all([$api->getAsync("/a"), $api->getAsync("/b")]);   // list<HttpResponse>
```

- Methods ending in `Async` start their work in the background and return a `Future<T>` at once: `HttpClient`
  `getAsync`, `postAsync`, `postJsonAsync`, `requestAsync`; `Database` `queryAsync`, `executeAsync`; `Redis`
  `getAsync`, `setAsync`. The script goes on while the work runs.
- `await($f)` waits for the result (`T`) or throws the work's error (`NetworkError`, `DatabaseError`); awaiting again
  gives the same result. `await_all($fs)` waits for a list of them. A mistake in the request itself (an unset
  environment variable, a path that is not allowed) throws when the work is started.
- The work stops at the script's time limit (without one: 30 seconds for HTTP and Redis, 300 for SQL), and so does
  waiting for it. A script can have 16 operations running at once; starting more throws `RuntimeException`.
- The work's result may take a sixteenth of the memory left to the script when it starts (and at most 16 MB for an
  HTTP response); a larger result is an error. It is counted against the script's limit when it is awaited.
- `queryAsync` / `executeAsync` on a database server use a connection of their own from the pool, so they run
  outside the script's transaction; give `pool_size` room for them. With SQLite the statement runs right away.
- `new Future(fn(): T => ...)` defers any work: it runs once, when the future is first awaited. Work that is never
  awaited is dropped when the script ends.

### Dates, cryptography, and encoding

| Function | Description |
|---|---|
| `date($format, ?$ts)` / `gmdate(...)` | Same format as PHP (`Y-m-d H:i:s`, `D, d M Y`, `c`, `U`, etc.). `date` uses the server's local time |
| `mktime($h, $i, $s, $m, $d, $y)`, `strtotime($s): ?int`, `checkdate($m, $d, $y)` | `strtotime` accepts ISO formats and `now`/`today`/`tomorrow`/`yesterday` |
| `random_int($min, $max)`, `random_token($bytes = 32)` | The OS's cryptographic random number generator (do not use `rand()` for cryptography) |
| `password_hash($pw)`, `password_verify($pw, $hash)` | Argon2id |
| `hash("sha256" \| "sha384" \| "sha512", $data)`, `hash_hmac($algo, $data, $key)`, `hash_equals($a, $b)` | `hash_equals` takes a comparison time that does not depend on the contents |
| `totp_secret()`, `totp_code($secret, ?$time)`, `totp_verify($secret, $code): ?int` | Two-factor codes of authenticator apps (TOTP, RFC 6238: HMAC-SHA1, 6 digits, 30 seconds). The secret is 160 random bits as Base32. `totp_verify` accepts one step of clock drift either way, compares in constant time, and returns the time step it matched (or `null`): store it and refuse a step that is not newer, so a code cannot be used twice. Store it in the database with one statement that only succeeds for a newer step (`UPDATE ... SET step = {$step} WHERE id = {$id} AND step < {$step}`, then check the affected rows): with the session, two requests sending the same code at the same time could both pass |
| `qr_svg($text): Html` | A QR code as an `<svg>` element (for example of an `otpauth://` URI). The markup holds only the modules, not the text, so it is safe to output |
| `base64_encode`, `base64_decode: ?string`, `urlencode`, `rawurlencode`, `urldecode`, `http_build_query` | |
| `json_decode($s): any` | Depth up to 128 |

### Content-Security-Policy

Page responses get the following policy by default (`{nonce}` is a per-response `'nonce-...'`; writing `'nonce-{nonce}'` is equivalent).

```
default-src 'self'; script-src 'self' {nonce}; style-src 'self' 'unsafe-inline';
img-src 'self' data: https:; font-src 'self' data:; object-src 'none'; base-uri 'self';
form-action 'self'; frame-ancestors 'self'
```

- PHV automatically adds `nonce="..."` to `<script>` tags written in templates (`.phv.html` and the HTML parts of `.phv`).
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

```phv
if (!rate_limit("login:" . ip_group(request()->ip), 5, 60)) {     // up to 5 times per 60 seconds
    response()->status(429);
    echo "Please wait a moment and try again.";
    return;
}
// after a successful login
rate_limit_reset("login:" . ip_group(request()->ip));
```

- `rate_limit($key, $max, $seconds)` counts one use and returns `true` if the key has not yet reached `$max` in the current window,
  and returns `false` if it has. Counters are shared within the server process and are lost on restart.
- To share the counters between server processes (or machines), name a Redis database from `[databases]` in
  `[security] rate_limit_store`. `rate_limit()`, `rate_limit_reset()` and `requests_per_minute` then count on that
  server, in keys starting with `phv:ratelimit:` that expire with their window. If the server cannot be reached,
  `rate_limit()` returns `false` (a limit that cannot be counted refuses), `rate_limit_reset()` throws
  `DatabaseError`, and `requests_per_minute` counts in the process; each failure is logged.
- The same key is counted separately for each window length (you can apply both 5 per minute and 50 per day). When there are more than 100,000 keys,
  keys that have not reached their limit are discarded first, so using a large number of keys does not release keys that are currently limited.
- `requests_per_minute` counts IPv6 addresses grouped by /64. Do the same in keys: `ip_group($ip)` returns the /64 of
  an IPv6 address (and an IPv4 address as it is), as in `rate_limit("login:" . ip_group(request()->ip), 5, 60)`, since one
  visitor can use any address of their network.
- Behind a proxy, `request()->ip` is the proxy's address. Read the visitor's address from the proxy's header (such as
  `Fly-Client-IP` on Fly.io) only when the site runs behind that proxy: anywhere else the header is the client's own text.
- The site-wide limit is `[security] requests_per_minute` (§14).

### Permissions (phv.toml)

A script can touch nothing outside the heap unless it is permitted in `phv.toml`. **If nothing is written, nothing is permitted.**

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
  (`.phv-cache/`, `.git/`, etc.), `vendor/`, and the directories of packages in `[dependencies]`.
- Anything not permitted raises `PermissionError` (the message shows what to write in `phv.toml`).

### phv.toml

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
rate_limit_store = "cache"  # count rate limits on this Redis database (default: in the process)

[server]
workers = 16            # scripts running at once (default: two per CPU, at least 8)

[fmt]
width = 100             # phv fmt wraps longer lines

[permissions]
write = ["data/"]

[dependencies]
utils = { path = "../utils" }
markdown = { git = "https://example.com/markdown.git", rev = "v1.2.0" }
```

The directory that contains `phv.toml` is the project root. Unknown keys are errors (so typos are caught).
`phv serve` reads the file at startup, so restart the server after changing it.

`phv run`, `check`, `test` and the editor look for `phv.toml` in the script's directory and the ones above it. A
`phv.toml` that another user could have put there is skipped: at the root of a filesystem (on Windows any user may
create files at the root of a drive), in the shared temporary directory, and on Unix one not owned by the current user
(or root). The project file decides what scripts may read, write and connect to.

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
| `rate_limit(string, int, int): bool`, `rate_limit_reset(string)`, `ip_group(string): string` | §14 |
| `date`, `gmdate`, `mktime`, `strtotime`, `checkdate` | Dates (table in §14) |
| `random_int`, `random_token`, `password_hash`, `password_verify`, `hash`, `hash_hmac`, `hash_equals`, `totp_secret`, `totp_code`, `totp_verify`, `qr_svg` | Cryptography and two-factor authentication (table in §14) |
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
| `phv run file.phv` | Type-check and run (`--html`, `--time-limit seconds`) |
| `phv check file.phv` | Type-check only (including imported modules and templates) |
| `phv serve dir` | Web server (`--port`, `--host`, `--prod`, `--time-limit`) |
| `phv test [paths]` | Run the `test_*` functions in `*_test.phv` files (a new VM for each test) |
| `phv fmt [paths] [--check] [--width N]` | Normalize indentation and whitespace and wrap long lines (strings, comments, and HTML are not changed) |
| `phv lsp` | Language Server for editors (errors as you type, types on hover, completion, go to definition) |
| `phv install` | Fetch `[dependencies]` and pin them in `phv.lock` |
| `phv migrate file.php` | Convert PHP mechanically and list the places that need manual work |
| `phv disasm file.phv` | Show the bytecode |
| `phv parse file.phv` | Show the syntax tree |

### Formatting

`phv fmt` indents with 4 spaces per level and wraps lines longer than 100 characters (indentation included). Choose
another width with `--width N`, or for a project in phv.toml (`[fmt] width = 120`).

A long line is split at its outermost level. Commas outside any bracket (`echo a, b, c;`) keep as many items on the
line as fit, and the rest continues on the next line, one level deeper. Otherwise the longest `(...)` or `[...]` that
opens and closes on the line puts each element on its own line, one level deeper, and the closing bracket on its own
line; calls, parameter lists and list and map literals also get a comma after the last element. A line without such a
bracket is split before `??`, `||`, `&&` or `.` (or `->` and `?->`), and the continuation lines are indented one more
level. Elements that are still too long are split the same way.

```phv
$body = render(
    "views/contact.phv.html",
    name: $name,
    message: $message,
    errors: $errors,
    sent: $sent,
);
```

`phv fmt` never joins lines, and never changes a line that holds a comment, a multi-line string, inline HTML or
`<?phv` / `?>` tags, so formatting cannot change what a program does; formatting a formatted file changes nothing.

### Testing

```phv
// tests/posts_test.phv
import lib.posts.find_post;

function test_find_post(): void {
    assert_eq(find_post(2)?->title, "Types without ceremony");
    assert(find_post(99) == null, "missing posts are null");
}
```

When `assert(bool, ?message)` or `assert_eq(expected, actual, ?message)` fails, it raises `AssertionError`.

A test is a function of the test file that takes no parameters and is named `test_*` or marked `#[Test]`. Tests run in
the order they are declared. A test marked `#[Skip("reason")]` is reported as skipped and not run.

```phv
#[Test]
function posts_have_titles(): void { /* ... */ }

#[Skip("needs the staging database")]
function test_import(): void { /* ... */ }
```

### Packages

```toml
[dependencies]
textutils = { git = "https://github.com/example/textutils.git", rev = "v1" }
local = { path = "../shared" }
```

```phv
import textutils.strings.slugify;      // vendor/textutils/strings.phv
```

- `git` packages are fetched into `vendor/<name>`, and the exact commit is recorded in `phv.lock`.
  From the second install on, the commit in the lock file is fetched, and a mismatch is an error.
- Only `https://`, `ssh://`, `git@host:...`, and `file://` URLs are allowed.
- Packages are PHV source code. No package code runs at install time.
- List a package's dependencies alongside it in the app's `phv.toml` (transitive dependency resolution is not yet supported).

### Migration

```
phv migrate legacy/page.php      # writes page.phv
```

Converts `<?php`→`<?phv`, `===`→`==`, `array(...)`→`[...]`, `isset($x)`→`($x != null)`, `$_GET`/`$_POST`/`$_COOKIE`/`$_SESSION`
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
     It can be disabled with `PHV_JIT=0`. The stack trace of an exception raised inside the JIT goes back only as far as the caller.
5. **Bytecode cache**: `phv run` and `phv serve` save compiled output in `.phv-cache/` and reuse it
   if neither the source files nor the `phv` binary have changed.
   - The cache is signed with an HMAC using a per-user key (`%LOCALAPPDATA%\phv\cache.key`, `~/.cache/phv/cache.key`), and entries whose
     signature does not match are not read. Even if a project received from someone else contains `.phv-cache/`,
     code that differs from the source never runs.
   - Entries are per script and per project. Each records the contents (SHA-256) of all the files it depends on, and these are verified on load
     (old code is not used even after a copy that preserves modification times or a rewrite that keeps the same length).
   - If `.phv-cache/` is a link, it is not used. Files inside it are neither read nor written if they are links or hard links.

### Runtime limits

| Limit | Default | When exceeded |
|---|---|---|
| Execution time | `serve`: 30 seconds (`--time-limit`), `run`: none | Fatal error (cannot be caught) |
| Memory | 128 MB (`[security] memory_limit_mb`) | Fatal error |
| Call depth | 10,000 | Fatal error |
| Request body | 8 MB | 413 |
| Scripts running at once (`serve`) | two per CPU, at least 8 (`[server] workers`) | waits up to 10 seconds, then 503 |
| Output waiting to be sent (`serve`) | `workers` × `memory_limit_mb` | 503 |
| Source file (module, template, script) | 8 MB | Compile error |
| Members inherited by all classes together | 200,000 | Compile error (each class holds a copy of what it inherits) |

`unsafe` is used only in the allocator that counts memory usage (`phv_vm/src/alloc.rs`), in the JIT
(`phv_jit`, for calling generated code), in calls to OS file APIs (`phv_std/src/safefs.rs`) and in the SQLite VFS
that checks journal files (`phv_std/src/db/vfs.rs`).

## 18. Roadmap

- Wider JIT coverage (functions that handle floats and strings), AOT compilation
- Transitive dependency resolution, a package registry
- Sessions stored in a database

See [security-audit.md](security-audit.md) for the results of the security audit and the remaining risks.
