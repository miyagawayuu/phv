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
  - `"..."` … Interpolates `$name` and `{$expr}`. Escapes are `\n \t \r \0 \e \\ \" \$ \{ \x41 \u{1F600}`
    (`\x` takes one or two hex digits up to `\x7f`; other characters are written with `\u{...}`).
  - Heredoc and nowdoc, for text over several lines:

    ```phv
    $page = <<<HTML
        <h1>{$title}</h1>
        <p>Hello, $name</p>
        HTML;
    $raw = <<<'TEXT'
        No $interpolation or \escapes here.
        TEXT;
    ```

    `<<<NAME` (or `<<<"NAME"`) works like `"..."`; `<<<'NAME'` (nowdoc) like `'...'` without escapes. The text starts
    on the next line and ends before the line holding `NAME`. The indentation of that closing line is removed from
    every line, and a line indented less than it is an error.
- Reserved words: `function fn return if elseif else endif while endwhile do for endfor foreach endforeach as break continue
  echo class extends new public private protected readonly static true false null match default try catch finally
  throw import export use is instanceof interface implements abstract final enum case const`. `type` is a keyword only
  where it starts a type alias (§4).

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
| `set<T>` | Distinct values in the order they were added. `T` is `int` or `string` |
| `?T` | `T` or `null` (same as `T|null`) |
| `A|B` | Union type |
| `fn(A, B): R` | Function (closure) type |
| Class name | The type of an object. `self` is the class itself |

PHP's `array` is split into `list<T>` and `map<K, V>`, and `set<T>` holds distinct values. `mixed` is `any`.

### Type aliases

```phv
type UserId = int;
type Row<T> = map<string, T>;
export type Handler = fn(Request): Html;

function find(UserId $id): ?Row<string> { ... }
```

A type alias is another name for a type: `UserId` and `int` are the same type. Aliases are declared at the top level
of a file, can take type parameters, can be exported and imported like classes, and cannot refer to themselves
(use a class for recursive data).

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

### Destructuring

```phv
[$a, $b] = [1, 2];
[$a, $b] = [$b, $a];                       // swap
[$first, ...$rest] = $names;               // $rest = $names->slice(1)
["id" => $id, "name" => $name] = $row;     // a map, by key
[[$x, $y], $z] = [[1, 2], 3];
[$this->x, $list[0]] = $pair;              // any assignable place
foreach ($pairs as [$key, $value]) { ... }
foreach ($rows as $i => ["id" => $id]) { ... }
```

The value is evaluated once, then each target is assigned its element: `$v[0]`, `$v[1]`, ... for a list, `$v["key"]`
for a map. A missing element throws `IndexError` or `KeyError`, as indexing does. Each target follows the usual rules of
assignment, so an element of a `list<int|string>` is `int|string`. Only `=` can assign to a pattern.

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

A `match` on an enum or a `bool` without `default` must have an arm for every case (and for `null` when the subject
may be null); a missing one is a compile error, so adding a case to an enum shows every `match` to update.

```phv
$color = match ($suit) {
    Suit::Hearts, Suit::Diamonds => "red",
    Suit::Spades, Suit::Clubs => "black",
};
```

In `match (true)`, an arm sees what its condition proves, and the later arms what the earlier conditions disprove,
as in `if` / `elseif`:

```phv
$area = match (true) {
    $shape is Circle => 3.14 * $shape->r ** 2,
    $shape is Square => $shape->side ** 2,
    default => 0.0,
};
```

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

Nested `finally` blocks run innermost first. A `return`, `break`, `continue` or `throw` inside a `finally` block
replaces the way out it was entered for (the value returned, or the exception): a `break` there leaves the loop even
after a `return`, so a loop like `while (true)` with such a `break` can end.

Only `Exception` and its subclasses can be thrown. A `catch` that does not need the exception leaves the variable
out: `catch (ValueError) { ... }`.

An exception can carry a code and the exception it was thrown because of:

```phv
try {
    $port = parse_port($raw);
} catch (ValueError $e) {
    throw new ConfigError("bad port", previous: $e);    // or new ConfigError("bad port", 2, $e)
}
// $e->getCode(): int, $e->getPrevious(): ?Exception
// $e->getTraceAsString(): "at parse_port (lib/config.phv:12)\nat load (lib/config.phv:20)\n..."
```

`getTraceAsString()` gives the calls where the exception was first thrown, innermost first (empty for an exception that
was never thrown). A deep trace keeps its 50 innermost and 50 outermost calls, with names shortened to 200 characters.
The runtime notes where an exception was thrown and makes the text the first time `getTraceAsString()` asks for it, so
exceptions that are caught without reading it cost little. An uncaught exception shows its causes too: `Uncaught ConfigError: bad port (caused by ValueError: ...)`.

## 8. Functions

```phv
function greet(string $name, int $times = 1): string { ... }
greet("Ann");
greet(name: "Bob", times: 3);       // named arguments
```

- Parameter types are required. If the return type is omitted, it is `void`.
- A function that returns a value must `return` on every path, or it is an error.
- Default values must be constant expressions (see "Constants" in §9), `null` or `[]`.
- Generic functions with type parameters can be written (see "Generic Functions" in §9).

### Variadic parameters and spreading

```phv
function sum(int ...$xs): int { ... }    // $xs is a list<int>
sum();                     // $xs = []
sum(1, 2, 3);
sum(...$numbers);          // a list<int> spread into $xs
sum(1, ...$more, 4);
$all = [0, ...$a, ...$b];  // lists joined in a list literal
```

A variadic parameter is the last one, takes the remaining positional arguments as a list, and has no default value.
`...` passes a list's elements to a variadic parameter only (the number of a list's elements is not known when the
program is checked, so it cannot fill fixed parameters).

### Functions as values

```phv
$len = strlen(...);                    // fn(string): int
$names->map(strtoupper(...));
$greet = $greeter->greet(...);         // the method, bound to $greeter (evaluated now)
$items->map(Item::fromRow(...));       // a static method
```

`name(...)` makes a closure that calls the function, method or static method with its own arguments. Generic and
variadic functions, built-in functions with several signatures (unless the expected function type picks one),
and the methods of `string`, `list` and `map` cannot be used this way; write a closure instead.

### Closures

```phv
$double = fn(int $x): int => $x * 2;           // captures outer variables automatically
$names->map(fn($n) => $n->upper());            // parameter types are inferred from context
$counter = function () use ($start): int { return $start + 1; };
```

- All captures are **by value** (a copy taken when the closure is created). There is no capture by reference (`&$x`).
- A closure created inside a method can use `$this`.
- The body of `fn() =>` extends as far as possible: `fn($n) => $n > 0 ? "+" : "-"` returns the whole conditional.
- Closure parameters cannot have default values or be variadic, because a function type (`fn(int): int`) has neither.

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
- A method may be named like a keyword (`match`, `list`, `default`), since it is always called after `->` or `::`.
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
- A `final class` cannot be extended, and a `final` method or constructor cannot be overridden (a `private` method is
  not inherited, so a subclass can declare one of the same name). `final` cannot be combined with `abstract`, and
  interfaces, interface methods, enums, properties and constants cannot be `final` (compile errors).
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

Constant values are constant expressions: literals (int, float, string, bool, null), the constants declared before
them in the same file (and class constants: `self::X`, `Other::Y`), and operators on them, computed when the program is
checked. The same expressions are allowed as default values.

```phv
const MINUTE = 60;
const DAY = 24 * 60 * MINUTE;
const MASK = (1 << 8) - 1;
const PREFIX = "v" . 2 . "/";
function wait(int $seconds = 5 * MINUTE): void { ... }
```

An overflow, a division by zero or an operator on the wrong types is a compile error. Floats are not joined to strings
in a constant.

### Generic Functions

```phv
function first_or<T>(list<T> $xs, T $fallback): T {
    return $xs->isEmpty() ? $fallback : $xs[0];
}
$n = first_or([5, 6], 0);        // int
$s = first_or([], "none");       // string
```

Type arguments are inferred from the call's arguments (up to 4). Inside the function, `T` is treated as an opaque type,
and operators and methods cannot be used on values of type `T`, unless it has a bound:

```phv
interface Shape {
    public function area(): float;
}
function largest<T: Shape>(list<T> $shapes): ?T {
    ?T $best = null;
    foreach ($shapes as $s) {
        if ($best == null || $s->area() > $best->area()) {   // methods of Shape
            $best = $s;
        }
    }
    return $best;
}
class Registry<T: Shape> { ... }
```

With `T: Bound`, a value of type `T` has the methods and properties of `Bound` and can be passed where a `Bound` is
expected, and every type argument must be a `Bound`: `largest([new Circle()])` gives a `?Circle`, while
`largest([1, 2])` is a compile error. A type argument meets a bound without conversion (an `int` is not a `T: float`),
a bound cannot be another type parameter, and an overriding method may drop a bound but not add or change one.
A type parameter of a type alias has no bound, so an alias cannot pass its own parameter to a bounded class.

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
  and narrows to `Box<?>`, a `Box` whose type argument is not known (so does `catch (Box $e)`). What is read through
  it is `any`; nothing can be written through it (`$b->value = ...` and `$b->set(...)` are errors), and it cannot be
  used where a `Box<T>` is expected.
- A class reaches each generic class or interface with one list of type arguments: `class C implements UrlSrc,
  StrSrc` is an error when one extends `Src<SafeUrl>` and the other `Src<string>`.
- A method's own type parameters are its own: a method `m<T>` and a class parameter `T` are different types, also in
  an override, and a caller's type parameter never takes the place of a method's parameter of the same name.

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
Exception                    getMessage(), getCode(), getPrevious(), getTraceAsString()
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
import lib.admin.posts.{Post as AdminPost};     // under another name
import lib.legacy.render as render_legacy;
```

- `import a.b.{x, y};` loads `a/b.phv` relative to the project root (the directory that contains `phv.toml`, or, if there is none,
  the directory of the file being run).
- The top level of an imported module can contain only declarations (`function`, `class`, `interface`, `enum`, `const`, `type`, `import`).
  Loading a module has no side effects.
- `std.*` are the standard modules (`std.http`, `std.db`, `std.net`, `std.async`, `std.validate`, `std.mail`,
  `std.testing`, `std.regex`, `std.time`, `std.ws`, `std.jobs`). Packages listed in `[dependencies]` are loaded with `import package_name.module`.
- Only files inside the project and package directories can be loaded (symbolic links cannot be used to escape them).
  A module, template, package or `phv.toml` that is a symbolic link or junction to another machine (a network share
  such as `\\host\share`, a network drive, or an object path such as `\GLOBAL??\UNC\host\share`), or lies behind one,
  is refused without being resolved, so its host is never contacted. The project itself may be on a share. `phv lsp`
  does not resolve an open document behind such a link, and `phv serve` serves no page or file behind one.
- A program (a page or script with every module, template and package it uses, std included) may have up to
  2,000,000 tokens, 32 MB of source and 2,000 files in all. One past them is refused with "this program is too large"
  before it is checked, at the import or template that passed the limit. Split it into smaller pages.

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
- `layout()` wraps a whole page in a layout: what the page writes after it becomes the layout's `$body` (an
  `Html` parameter it must declare), and the other arguments are passed as they are. They are evaluated when the page
  has finished, so the page may set them while it runs:

  ```phv
  <?phv
  layout("views/layout.phv.html", title: $title);
  $title = "Posts";
  ?>
  <ul>...</ul>
  ```

  `layout()` is a statement at the top level of a page or template, at most once. A page that ends early (with
  `return`) or fails is not wrapped. A page that uses `layout()`, like a template, must not call it or end inside a
  tag, an attribute, a comment or a `<script>`: its output is joined with the layout's.
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
  A relative URL with no `:`, `/`, `?` or `#` (`page.html`) is returned as `./page.html`, the same URL, so no text after it can make it a scheme.
- `json_html($value)` … Returns, as `SafeJs`, a JavaScript value (JSON) that is safe to place inside `<script>` where an
  expression goes. Inside its strings, every character except ASCII letters and digits and non-ASCII characters
  other than spaces is written as `\uXXXX`, so the value cannot end a string, template literal, comment or regular
  expression it lands in by mistake (in a `"..."` written around it, its strings' contents would only be names: a
  syntax error, not code that runs).
  The `"` in strings is left as is, so the value cannot be placed inside an attribute value (a compile error; use `escape(json_encode($v))`).
- `SafeUrl` can also be used as `Html` (not the reverse). `SafeJs` cannot: its quotes would end an attribute, where
  `Html` may be placed. The result of `escape()` is `Html`, so it cannot be placed in a URL or inside `<script>`. This
  is because HTML escaping cannot stop `javascript:` or script code.
- `Html` and `string` cannot be concatenated with `.` (the error suggests `escape()`).
- Escaping is turned off only for the Content-Types `text/plain`, `application/json`, `text/csv`, `text/css`, `text/javascript`,
  `text/markdown`, `text/calendar`, `application/pdf`, `application/octet-stream`, and image, audio, and video types
  (all types that browsers do not render as HTML; `X-Content-Type-Options: nosniff` is also always sent).
  HTML, XML, SVG, and unknown types stay escaped. A JavaScript response (`text/javascript` or any type browsers run as
  a script) takes only `json_html()` values and numbers: a string or a template's output would be code, so writing one
  is an error when the page runs. Its type must be set before any output.

### HTML context checks

HTML escaping protects only element content and **quoted** attribute values. `<?= ?>` and `echo` anywhere else
are a compile error. Locations are determined according to the browser's parsing rules (WHATWG), including
the content of `<textarea>`, `<title>`, and similar elements, `<!--` inside `<script>`, comments such as `<!-->` and `--!>`, the inside of `<svg>`
and `<math>` (where any markup, such as a tag, comment or CDATA section, in a `<script>` or `<style>` is an error; a self-closed `<svg/>`
opens nothing, and `</math>` does not close an `<svg>`), markup inside `<noscript>` (checked as markup), a `<` written just
before a value or block (a value there would name a tag: an error),
and text such as `</scripts>` that looks like an end tag but is not one. Inside `<noscript>`, `<svg>` and `<math>`, the end tag of an
element whose text a browser may read as raw text (`</textarea>`, `</noscript>`, `</xmp>`...) inside a comment or an attribute value
is an error, and so is a value right after `</` and the start of such a name there, or any other statement there
(`</<?phv f(); ?>`: a function could write the rest): where a browser reads the element differently (`<noscript>` with scripting
on, `<foreignObject>`), that end tag would end it and turn what follows into markup. `</noscript` is watched in this way in every
template and function, inside a `<noscript>` or not, in comments, attribute values and raw text (`<script>`, `<textarea>`...): any
template or function may be written inside a `<noscript>` (`<noscript><?= render(...) ?></noscript>`, a layout's body, a call),
while it is checked on its own. So `<!--</<?= $v ?>-->` and `title="</nos<?phv f(); ?>"` are errors in every template.

| Location | Example | Treatment |
|---|---|---|
| Element content, quoted attribute value | `<p><?= $s ?></p>`, `title="<?= $s ?>"` | Escaped and output. `Html` is output as it is, except that in an attribute value its `"` and `'` are written as `&quot;` and `&#039;`, so no `Html` can end the attribute |
| Comment | `<!-- <?= $s ?> -->` | As element content, but a value may not come right after `<!--`, `<!---` or a `-` (`<!-- a-<?= $s ?>`), and the text right after a value may not start with `>`, `!>`, `->` or `-!>` (`<!-- <?= $s ?>> -->`): a value ending with `--` would end the comment there. `Html` that holds `-->` or `--!>` (in a bogus comment such as `<!x ...>`, `>`) stops the script with a runtime error. A `-` at the end of a comment's text is read with the text after a statement (`-<?phv f(); ?>->` ends the comment) |
| Text of `<textarea>`, `<title>`, `<xmp>`, `<iframe>`, `<noembed>`, `<noframes>` | `<textarea><?= $s ?></textarea>` | As element content. `Html` that holds the element's end tag (`</textarea`) stops the script with a runtime error: it would end the element there |
| Element content or a comment inside `<svg>` or `<math>` | `<svg><?= render("bar.phv.html") ?></svg>` | As element content. Browsers read markup there as foreign content, while a template was checked as HTML: `Html` that holds a CDATA section, the end tag of a raw-text element it did not open, or a raw-text element (`<title>`, `<style>`, `<textarea>`...) whose text holds `<` or does not end, stops the script with a runtime error (an SVG `<title>Bar 3</title>` or `<style>.a{fill:red}</style>` is fine) |
| Unquoted attribute value | `href=<?= $u ?>` | Error (add quotes) |
| Inside a tag (between attributes) | `<div <?= $attrs ?>>` | Error |
| Start of a URL attribute | `href="<?= $u ?>"`, `src="<?= $u ?>"` | Only `SafeUrl` (`safe_url($u)`) and numbers. If the preceding text contains `/` `?` `#` `:`, it is not the start (`href="/post?id=<?= $id ?>"` is allowed). `href="j<?= ?>"` still counts as the start. After a `&` at the start (`href="&<?= ?>"`, `&#106;`) only numbers are allowed, `SafeUrl` included: the value would continue a character reference (`&` and `#x6a;avascript:` make `javascript:`). and so does the text after an inserted value (`safe_url("")` is empty): `href="<?= safe_url($a) ?><?= $b ?>"` is an error; check the whole URL with `safe_url($a . $b)` |
| `javascript:` / `data:` URL | `href="javascript:go(<?= $v ?>)"` | Error (numbers are allowed) |
| Inside `<script>` | `var x = <?= $v ?>;` | Only `SafeJs` (`json_html($v)`) and numbers, where an expression goes. Inside a JavaScript string, template literal, comment or regular expression (`` `<?= $v ?>` ``, `"<?= $v ?>"`), only numbers: a JSON value's quotes would end the string, and `${...}` in a template literal runs. Write `const s = <?= json_html($v) ?>;` |
| `<style>`, `style="..."` | | Error (numbers are allowed) |
| Event attribute | `onclick="go(<?= $v ?>)"` | Error. Put the value in a `data-*` attribute and read it from JavaScript |
| `<iframe srcdoc="...">` | | Error (the value is parsed as a separate HTML document) |
| URL of a `<script>` | `<script src="<?= $u ?>">` (and `href` in SVG) | Error, also for `SafeUrl`: the tag gets the CSP nonce, so any URL there would run as a trusted script. Write the URL in the template |
| `content` of a `<meta>` | `<meta http-equiv="refresh" content="0;url=<?= $u ?>">` | Only numbers, unless a `name`, `property` or `itemprop` attribute comes before `content` and the tag has no `http-equiv` (`<meta property="og:title" content="<?= $t ?>">` is text; `http-equiv` after such text is an error) |

- The tags are read as browsers read them: only tab, line feed, form feed, carriage return and space separate a
  tag's name and its attributes, so `<a class=x　title="<?= $t ?>">` (a full-width space) is one unquoted value,
  an error. A tag name runs up to such a space, `/` or `>`.
- The bodies of functions and closures, and `if` and loop blocks, must end in the same HTML state they started in
  (leaving a `<script>` or an attribute open at the end of a block is an error). This prevents the context from drifting with call order or repetition.
- For the same reason, `break` and `continue` must be in the HTML state where their loop began, `throw` where its
  `try` began (or at the start of the function), and `return` at the start of the function: `<a href="<?phv if ($x)
  { break; } ?>...` is an error, since the output after the jump would land inside the attribute.
- A function's body is checked as element text, wherever it is called. So a function (or closure or method) that
  writes output may not be called where its output would land elsewhere: `<script><?phv show($s); ?></script>`,
  `title="<?phv show($s); ?>"`, `<!-- <?phv show($s); ?> -->`, `<svg><?phv dot($p); ?></svg>` (inside `<svg>` or
  `<math>`, where browsers read its markup as foreign content) or `<textarea><?phv show($s); ?></textarea>` (the text
  of a raw-text element) stops the
  script with a runtime error, as does `dump()` there. Return the value (or
  `Html`) instead and insert it with `<?= ?>`. Output into a template being rendered, or other captured output, is
  not affected.
- When an exception thrown inside a `<script>`, a tag or an attribute is caught, what the `try` block wrote is
  dropped, so the `catch` block writes where the `try` began rather than inside what was left open. Other
  exceptions keep the output written before them.
- Expressions and statements can be nested up to 500 levels (this prevents deeply nested source from overflowing the implementation's stack).
- A source file may have up to 1,000,000 tokens, the code inside its strings included: a larger one is refused before it is
  parsed, since its syntax tree would take many times its size in memory.

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
- A static file is never reached through a symbolic link or junction, even one put on the way after the path was
  found. On Unix it is opened from `public/` one directory at a time without following links, and without waiting:
  a FIFO or device is not served, nor a file under a directory other users can write to. A file with another hard
  link is not served either.
- Each request runs in a new VM (a new heap). Only the compiled bytecode is shared.
- When a file is saved, it is recompiled on the next request (the modification times of all the files it depends on are checked).
  A production server (`--prod`) looks at the files at most once a second: what a request path leads to, the files a
  compiled page was built from, and the small static files it keeps in memory. A change shows within a second. A
  development server looks at every request.
- Limits: execution time 30 seconds (`--time-limit`), memory 128 MB (`[security] memory_limit_mb`), body 8 MB, receiving headers 10 seconds,
  receiving the body 30 seconds, 1024 concurrent connections, 1000 query and form fields. A connection is closed when the
  client accepts no data for 30 seconds. Static files (and `response()->file()`) are sent from disk as they are read,
  so they do not take memory; a production server keeps static files of up to 256 KB in memory, 32 MB in all.
- At most `[server] workers` scripts run at once (default: two per CPU, at least 8); a request waits up to 10 seconds
  for one. A request that cannot get one in time, or that arrives while the output waiting to be sent to clients
  (event streams included) is more than the workers could produce (`workers` × `memory_limit_mb`), gets `503 Service Unavailable` with
  `Retry-After: 5`. Static files do not need a worker. Past their first 64 KB, request bodies are received only within a
  budget of `workers` × 8 MB, taken as the bytes arrive (a stated `Content-Length` or a chunked body that has not been
  sent holds nothing) and kept until its script ends; a request whose next bytes cannot get their share within
  10 seconds gets the same 503. A body stated larger than 8 MB gets 413 at once.
  Query and form fields are counted as they are parsed. A connection buffers at most 64 KB for a client that reads
  slowly. So the memory a server uses stays around twice `workers` × `memory_limit_mb`, however many clients connect.
- If `[security] requests_per_minute` is set, the number of requests accepted per minute from a single client address is limited to that number
  (beyond it, the response is `429 Too Many Requests` with `Retry-After`). The default is unlimited. With
  `[security] rate_limit_store`, all server processes share the count (see "Rate limiting"). Connections are not
  limited per address (at most 1,024 in all): a site open to the internet should run behind a reverse proxy that limits them.
- Headers sent by default: `X-Content-Type-Options: nosniff`, `X-Frame-Options: SAMEORIGIN`,
  `Referrer-Policy: strict-origin-when-cross-origin`, and Content-Security-Policy (see "Content-Security-Policy" below).
  HSTS if `secure_cookies = true` (always over HTTPS). `Cache-Control: private, no-store` on responses that use sessions or cookies, or
  that carry a CSRF token (`csrf_field()`, `csrf_token()`).
  Version information such as `Server` / `X-Powered-By` is not sent. Scripts cannot set transport headers such as `Content-Length`.
- A page runs for any request method (it reads it from `request()->method`) but `TRACE` and `CONNECT`, which get
  `405 Method Not Allowed` without running a page. Static files answer only `GET` and `HEAD` (others get 405).
- In development mode, the details of compile errors and runtime errors are shown in the browser. Development mode is the default only
  when listening on `127.0.0.1` / `localhost` (it can be set explicitly with `--dev` / `--prod`). **Behind a reverse proxy, which
  reaches the server on `127.0.0.1`, use `--prod`**: otherwise visitors see the details (the server prints a note at startup).
- Ctrl-C or SIGTERM (`docker stop`) stops the server gracefully: it stops accepting connections and lets running requests
  finish, for at most `[server] shutdown_timeout` seconds (default 30).

### HTTPS and HTTP/2

```toml
[server]
tls_cert = "/etc/letsencrypt/live/example.com/fullchain.pem"
tls_key = "/etc/letsencrypt/live/example.com/privkey.pem"
```

- With `tls_cert` and `tls_key` (PEM files on this machine; a relative path is relative to the project; a network
  share such as `\\host\share`, a network drive, or a symbolic link to one is refused, and links are looked at without
  being followed, so the host is never contacted), `phv serve` speaks HTTPS only. Clients choose HTTP/2 or
  HTTP/1.1 during the handshake (ALPN). TLS 1.2 and 1.3 are offered, with the safe default cipher suites of rustls;
  0-RTT is not.
- The files are read when the server starts: a key that does not belong to the certificate, a file that is not
  PEM, or a file inside `public/` stops it with an error (which names the file, never its contents). After that
  they are looked at once a minute: a renewed certificate (certbot and the like) is used without a restart, and
  one that cannot be loaded is reported and the previous one kept.
- Over HTTPS, `secure_cookies` is on: the session cookie is `Secure`, and `Strict-Transport-Security` is sent.
- A client must finish the TLS handshake within 10 seconds.
- HTTP/2 connections: at most 8 requests open at once on one connection, 64 KB of request headers, 64 KB a client
  may send before the server reads it (on each request and on the connection), and 16 KB of each response waiting
  for the client, so one connection holds about what an HTTP/1.1 connection does. Request bodies count against the
  body budget from their first byte. A connection whose responses the client takes nothing of for 30 seconds is
  dropped, and one without a request for 30 seconds is closed (at most 10 seconds later). Requests share the
  workers, the request limits and the rate limit with HTTP/1.1. The authority of the request target (HTTP/2's
  `:authority`, or an absolute target such as `GET http://example.com/ HTTP/1.1`) replaces the `host` field; one with
  user information (`user@host`) and a request with two `Host` fields are answered with 400.
- HTTP/2 without TLS is refused unless `[server] h2c = true`, for a reverse proxy that speaks it to the server
  (the client then sends HTTP/2 from its first bytes; HTTP/1.1 keeps working on the same port).
- Scripts see an HTTP/2 request as an HTTP/1.1 one: `request()->header("host")` is the host the client asked for,
  and cookies sent in several fields are read together.

### Routes with parameters

A file or directory name in brackets matches any one part of the path, and `[...name]` the rest of it:

```
public/posts/[id].phv          /posts/42          request()->param("id") == "42"
public/users/[id]/edit.phv     /users/7/edit
public/docs/[...path].phv      /docs/guide/intro  request()->param("path") == "guide/intro"
```

A file whose exact name matches the path always wins (`public/posts/new.phv` answers `/posts/new`). Otherwise the most
specific route does: from the left, a fixed name before `[param]` before `[...rest]`. Two routes that would answer the
same paths (`[id].phv` and `[slug].phv` in one directory) are an error: the server does not start and `phv build`
refuses the site. Parameter values are decoded, are never empty, and never start with `.` or `_`. In development mode,
new route files are picked up within a second.

### Error pages and middleware

- `public/_404.phv` answers paths that match no page or file (with status 404), and `public/_500.phv` answers in
  production when a page fails to compile or throws (status 500; it gets the request's method, path and headers, never
  the error). In development mode the error's details are shown instead. If an error page fails itself, the built-in
  page is used.
- `_middleware.phv` in a directory runs before every page in that directory and below, the outermost first. A middleware
  that writes output, sets a status or redirects ends the request with that answer (the page does not run). Otherwise
  the page runs next and sees what the middleware did: the session, the headers and cookies it set, and `locals()`:

  ```phv
  // public/admin/_middleware.phv
  import std.http.{session, response, locals};
  $user = session()->get("user_id");
  if ($user == null) {
      response()->redirect("/login");
  } else {
      locals()->set("user_id", $user);     // read in the page with locals()->get("user_id")
  }
  ```

- Middleware runs before pages, not before static files: a file under `public/admin/` is served to anyone. Keep
  files that need protection outside `public/` and send them from a page with `response()->file()`.
- Files starting with `_` are never served as pages, so `_404.phv`, `_500.phv` and `_middleware.phv` cannot be opened by URL.
  `phv build` includes them in the executable.

### Static files, compression and CORS

- Static files carry `ETag` and `Last-Modified`, so browsers revalidate them cheaply (`304 Not Modified`); they are
  sent with `Cache-Control: no-cache`, or `public, max-age=N` with `[server] static_max_age = N`. One byte range
  (`Range: bytes=a-b`) is served with `206 Partial Content` for media players and resumed downloads.
- Text responses (HTML, CSS, JavaScript, JSON, SVG, ...) of 1 KB or more are compressed with gzip when the client
  accepts it (`[server] compress = false` turns this off). Responses for one visitor (they set a cookie, use the session
  or carry a CSRF token, whatever `Cache-Control` the page sets) and pages answering a request that carries cookies or
  an `Authorization` header are never compressed, so a secret in them cannot be guessed from the compressed size
  (BREACH). Static files are compressed once per version and kept (up to 32 MB). A proxy or CDN in front of the
  server may compress every response anyway (Fly.io's proxy does): CSRF tokens stay safe because they are masked
  (see "CSRF protection"), but other secrets a page shows next to text from the request may not be. Turn the proxy's
  compression off for such pages if it allows that.
- `[cors]` lets pages of other origins call the site from a browser:

  ```toml
  [cors]
  origins = ["https://app.example.com"]   # or ["*"]
  methods = ["GET", "POST"]               # default
  headers = ["Content-Type"]
  credentials = false                     # true sends cookies along (not with "*")
  max_age = 600
  ```

  Preflight requests (`OPTIONS` with `Access-Control-Request-Method`) are answered by the server; other responses get
  `Access-Control-Allow-Origin` when the request's `Origin` is allowed. An origin allowed with `credentials = true` can
  send JSON requests with the visitor's cookies: list only origins you trust.
- `[server] access_log = "logs/access.log"` also appends one line per request to a file inside the project
  (`access_log_format = "json"` writes JSON objects with `time`, `ip`, `method`, `path`, `status` and `ms`, and `id`
  with request ids). The path is logged without the query string.

### Request ids

```toml
[server]
request_ids = true          # default: false
trust_request_id = false    # true: take the id and the trace a proxy in front sends
```

- With `request_ids`, each request gets an id: the trace id of W3C Trace Context, 32 hex digits. The response carries it
  in `X-Request-Id`, the JSON access log in `id`, and the lines of `log_info()`, `log_warn()` and `log_error()` written
  for the request end with `req=<id>`. A page reads it with `request_id()` of std.http (`null` without
  `request_ids`).
- Requests the page sends with `HttpClient` carry a `traceparent` header with the request's trace and a span of their
  own, so a tracing system can join them (unless the page sets `traceparent` itself).
- With `trust_request_id`, the server takes the `X-Request-Id` (1 to 128 letters, digits, `.`, `_` and `-`) and the
  trace of `traceparent` (version `00`, lowercase hex) that the request carries, when they are well formed; others are
  replaced by new ones. Set it only behind a proxy that sets or removes these headers: elsewhere they are the client's
  own text.

### Health checks and metrics

```toml
[admin]
listen = "127.0.0.1:9100"        # the admin port, a listener of its own
token_env = "PHV_ADMIN_TOKEN"    # needed unless listen is a loopback address
```

`[admin]` opens a second port with three paths, apart from the site's own (the site's paths are not served there, and
these are not served on the site's port):

| Path | Answer |
|---|---|
| `/health` | 200 `ok` while the server runs; 503 from the moment it begins to stop (Ctrl-C, SIGTERM), while running requests finish |
| `/ready` | Also 503 while a store the server uses failed the last time it was used, until it answers again: the session database, the rate limit store, the `publish()` store (`unavailable: sessions, ...`). A probe makes the server connect nowhere |
| `/metrics` | The counters below in the Prometheus text format. With `token_env`, only with `Authorization: Bearer <token>` (401 otherwise) |

- An address other than a loopback one (`0.0.0.0:9100` for a container's probes) needs `token_env`, or the server does
  not start: `/metrics` shows the site's routes and load. The token is read when the server starts and must hold at
  least 16 bytes. `/health` and `/ready` need no token (load balancers often cannot send one); they tell only whether
  the server is up.
- The admin port answers `GET` and `HEAD`, one request per connection, at most 64 connections at once; a client must
  send its request within 5 seconds.
- Metrics (all start with `phv_`): `requests_total{kind, code}` (`kind`: `page`, `static`, `ws`, `sse`, `other`; `code`:
  `2xx`...), `request_duration_seconds{route}` (a histogram per page, labelled with its file: `/posts/[id]`, never the
  path asked for; at most 1,000 routes, the rest as `other`; static files as `static`), `worker_wait_seconds`,
  `rejected_total{reason}` (`busy`, `output`, `body`, `rate_limit`, `too_large`), `events_waiting_bytes`, `script_failures_total{kind}`
  (`exception`, `time`, `memory`, `compile`, `other`), `ws_messages_total{dir}`, `publish_total{result}`,
  `jobs_total{result}`, `job_duration_seconds`, `store_errors_total{store}`, and the gauges `up`, `ready`,
  `uptime_seconds`, `workers`, `workers_busy`, `connections`, `websockets`, `output_waiting_bytes`, `async_running` and
  `build_info{version}`. Counting takes no lock except a page's histogram.

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
$req->param("id");         // ?string: a parameter of the page's path (`public/posts/[id].phv`)
$req->params;              // map<string, string>
locals()->get("user_id");  // ?string: a value `_middleware.phv` set with locals()->set()
request_id();              // ?string: the request's id, with [server] request_ids (see "Request ids")

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
- With `[security] session_store = "db:<name>"`, sessions are kept in that database of `[databases]` (Redis, PostgreSQL
  or MySQL), shared by every server process and machine that uses it. Redis keeps each under `phv:session:<hash>` and
  expires it itself; a session whose visitor has not sent the cookie back expires after an hour at most. The counts
  that keep a site within the limits below are under `phv:sessions:<namespace>:`. PostgreSQL and MySQL keep a site's
  sessions in a table of its own, `phv_sessions_<namespace>`, created when first needed; every 30 seconds (sooner
  after 1,000 new sessions) a server removes expired sessions and keeps the table within the limits below. If the
  database cannot be reached, the request goes on without its session and a warning is logged.
- Sites that share a database keep their sessions apart by the site's namespace: `[security] store_namespace`, or
  else `[project] name` (rate limits and jobs are kept under it too). Two copies of a site with the same name, such as
  staging and production, must set different `store_namespace` values (a server using a shared store without it
  says so when it starts); a site with neither shares the store with every other such site (the table
  `phv_sessions` and the keys `phv:sessions:*`). Sessions stored before 1.1 are not read by a site with a namespace,
  so upgrading to 1.1 signed its visitors out once. Each site has its own `session_ttl` and its own limits: sites
  that share a database share only its capacity.
- Sessions that 1.1 stored for a site with a namespace in the shared table `phv_sessions` (PostgreSQL, MySQL), or
  counted under the shared keys `phv:sessions:new`, `back`, `keep`, `size` and `bytes` (Redis), are still read,
  updated where they are and logged out, so upgrading from 1.1 signs no one out. They last at most 24 hours: the sweep
  of every site, with a namespace or not, removes them after that (and, in Redis, their counts once Redis has expired
  them), never one that is still alive for any site. Once no 1.1 process uses the database, that table and those keys
  may be removed, unless a site without a namespace uses them.
- In every store, a session is saved only while it still exists (a new or regenerated id only if it is new), each as
  one atomic step: a request that finishes after a logout, in this process or another, cannot bring the session back,
  and of two requests that regenerate the same session only one gets it.
- A site's store holds at most 50,000 sessions and 256 MB of session data (Redis keeps within both as sessions are written).
  When it is full, sessions whose visitor never came back are removed first (the least recently used), then the newest
  of the rest: a flood of sessions, even ones whose cookie is sent back, removes its own sessions and never those
  created before it. Idle sessions still end after `session_lifetime`.

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
`csrf_field()` and `csrf_token()` write it masked with a random pad of its own each time it is written (128 hex digits), so
the text differs each time and a page compressed on the way, by the server or by a proxy or CDN in front of it, does
not give the token away through its size (BREACH). The server accepts the masked form and the plain one.
In production, serve over HTTPS and set `secure_cookies = true` (the cookie gets the `__Host-` prefix and can no longer be overwritten
over plaintext connections or from subdomains).
It can be disabled with `[security] csrf = false`.

The key of the HMAC is new each time the server starts, unless the site has a secret: then forms stay valid across
restarts and on every server process of the site, so a form served by one process can be sent to another behind a
load balancer.

```toml
[security]
secret_env = "PHV_SECRET"          # an environment variable holding at least 32 bytes (random_token() makes one)
secret_old_env = "PHV_SECRET_OLD"  # optional: the previous secret, accepted while it is replaced
```

- The server reads the variables when it starts: one that is unset or holds fewer than 32 bytes stops it with an
  error that names the variable, never its value. They are server settings, so `[permissions] env` need not list them,
  and `phv build` puts only their names in the executable. Scripts cannot read them, even when a pattern of
  `[permissions] env` matches them; the same holds for `[admin] token_env` and every `url_env` and `headers_env`.
- The server derives a key of its own for each use from the secret (HKDF-SHA256): CSRF tokens, and the messages
  `publish()` sends to other server processes. Scripts cannot read the secret or the keys.
- To replace the secret, set the new one in `secret_env` and the old one in `secret_old_env`, restart every process,
  and remove `secret_old_env` once the forms served before are no longer used (a few hours). Tokens are always made
  with the new secret. `publish()` between processes uses `secret_env` only: while some processes have the new secret
  and others the old one, their messages do not reach each other.

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

#### Typed JSON

```phv
enum Status: string {
    case Open = "open";
    case Closed = "closed";
}

class Item {
    public function __construct(public string $name, public int $qty = 1, public ?string $note = null) {}
}

class Order {
    public function __construct(public int $id, public Status $status, public list<Item> $items) {}
}

Order $order = json_decode_as(request()->body);     // ValueError if the JSON does not fit Order
echo json_encode_pretty($order);
```

`json_decode_as($text)` decodes into the type the value is expected to have (a declared variable, a parameter or a
return type; without one it is a compile error) and checks the JSON against it: `int`, `float` (an integer is
accepted), `string`, `bool`, `any`, `?T`, `list<T>`, `map<string, T>`, `map<int, T>` (keys written as decimal
integers), enums (a backed enum by its value, a pure enum by its case name) and classes.

- A class is built from a JSON object without calling its constructor, so it must be a non-generic, non-abstract
  class whose properties are all public, and whose constructor, if it has one, is public and only promotes its
  parameters. A constructor with code in it (which might check its arguments) would be skipped, so such a class is a
  compile error. So is a readonly property that is not a constructor parameter: only its default can be its value.
- A missing property takes its default, or the default of its constructor parameter, or `null` if it is nullable;
  otherwise it is an error. Keys the class does not have are ignored.
- On a mismatch the `ValueError` says where: `json_decode_as(): expected an integer, found a string at $.items[2].qty`.

`json_encode` writes an enum case as its value (its name for a pure enum), so what it writes reads back with
`json_decode_as`. `json_encode_pretty($v)` is `json_encode` laid out on lines, indented by two spaces.

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

A `transaction()` inside another (even through another `Database` of the same name) is a savepoint: if it throws,
only its own changes are undone, and nothing is committed until the outermost one ends.

- **SQL can be written only as `sql"..."` literals (type `Sql`).** Values embedded with `{$x}` are always passed
  as placeholders, so SQL injection cannot occur. A `string` cannot be passed to `query()`.
- `Sql` values can be concatenated with `.` (their parameters are carried over). `Sql` and `string` cannot be concatenated.
- To choose a column or table name dynamically, use `sql_ident($name)` (it allows only alphanumerics and `_`, and quotes the name; names starting with `phv_`, PHV's own tables such as the sessions, are refused).
- `ATTACH` is disabled, so databases other than the permitted files cannot be accessed (also in `phv db migrate`).
- `LIKE` and `GLOB` stop at the script's time limit like the rest of a query; `instr()`, `replace()` and `trim(X, Y)`
  search in linear time. `PRAGMA case_sensitive_like` is refused (it would bring back SQLite's own `LIKE`).
- SQL errors raise `DatabaseError`.
- A connection to a database file is kept for the next script that opens the same file (up to 30 seconds idle;
  `:memory:` databases are never kept), so pages that open the database on every request do not reopen the file.
  Each open checks the path as before: no link on the way, the journal files missing or plain files, and the
  database a plain file with one link that is the very file the kept connection has open (otherwise a new connection
  is opened). The new script gets its own time limit, and `lastInsertId()`, `changes()` and `total_changes()` start
  from zero. When a script is done, an open transaction is rolled back; a connection is closed instead of kept after a
  failed statement, after any `PRAGMA`, or when a temporary table, view, index or trigger exists. `unlink()` and
  `rename()` close the idle connections of the file first.
- SQLite's memory counts toward the script's memory limit: the page cache of each connection (up to 2 MB), its
  schema and statements, in-memory databases, and temporary tables, indexes and sorts, which are kept in memory
  (never in temporary files). A kept connection counts while the script holds it. A statement that would pass the
  limit stops with the memory limit's fatal error. A script may have at most 64 SQLite databases open at once
  (`close()` the ones it no longer uses); the 65th `Database::open()` raises `DatabaseError`.

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
- A PostgreSQL connection keeps the 64 statements it ran last prepared (SQLite: the statements of the script's
  connection), so a statement run again skips that step. Only `SELECT`, `INSERT`, `UPDATE`, `DELETE`, `WITH` and
  `VALUES` are kept; any other statement (`ALTER TABLE`, `DEALLOCATE`, `SET`, ...) drops them. A kept statement is
  prepared again when its table changed (also by another connection); inside a transaction only the statements
  prepared in that transaction are used. A connection on which a script deallocated every statement (`DEALLOCATE ALL`
  or `DISCARD ALL`, also inside a function) is closed after the script instead of going back to the pool.
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
`EVAL` are not available. Keys starting with `phv:` belong to PHV (sessions, rate limits and jobs kept in Redis) and raise
`ValueError`. A remote server needs a `rediss://` URL (or `tls = false`). Errors raise `DatabaseError`.

### WebSocket server (std.ws)

A file `public/chat.ws.phv` answers WebSocket connections to `/chat` (`public/rooms/[room].ws.phv` answers
`/rooms/blue`, with the parameter `room`). It is not a page: a normal request to `/chat` does not run it.

```phv
<?phv
// public/chat.ws.phv
import std.ws.{socket, publish, WsEvent};
import std.http.{request, response};

$ws = socket();
if ($ws->event == WsEvent::Open) {
    $name = request()->query("name") ?? "";
    if ($name == "") {
        response()->status(403);           // refuses the connection
    } else {
        $ws->set("name", $name);           // kept for the next events
        $ws->join("lobby");
    }
} elseif ($ws->event == WsEvent::Message) {
    publish("lobby", ($ws->get("name") ?? "?") . ": " . $ws->message());
} else {
    publish("lobby", ($ws->get("name") ?? "?") . " left");
}
```

- The script runs once for each **event** of a connection, like a page for a request: `WsEvent::Open` (as part of
  the upgrade request: middleware runs, `request()` and `session()` work, and a status other than 200 refuses the
  connection with that response), `WsEvent::Message` for each text message (`$ws->message()`), and `WsEvent::Close`.
  Each run has the time and memory limits of a page and takes a worker; between events a connection holds no VM
  and no worker. The messages of one connection are handled one at a time, in order.
- Nothing is kept between runs except what the connection keeps with `$ws->set($key, $value)` (read with `get()`;
  at most 256 values and 64 KB in all, each value counting 64 bytes more) and the topics it joined. `$ws->id` is a random id, the same for all the events of a connection.
- `$ws->send($text)` sends to this connection; `publish($topic, $text)` to every connection of this server process
  (and, with `publish_store`, of the site's other server processes) that joined `$topic` (`$ws->join()`, `$ws->leave()`; 64 topics per connection). Any page may call `publish()`,
  for example after a form is saved. Messages are sent after the script ends; output (`echo`) is discarded.
  `$ws->close($code, $reason)` closes the connection (code 1000 or 3000–4999).
- Browsers send the page's origin: a connection from another site is refused (403) unless `[cors] origins` names
  that site and `credentials = true` (a WebSocket always carries the visitor's cookies; `"*"` and `null` never
  allow one), so another site cannot open a connection as the visitor.
- Limits: 1,024 connections at once (more get 503), text messages of at most 64 KB (larger ones and binary
  messages close the connection), 1 MB of messages waiting for one connection (a client that does not read is
  disconnected), 1,000 messages sent or published per run, and 1 MB per message sent. The server sends a ping every
  30 seconds and closes a connection it has not heard from for 75 seconds. With `[security] requests_per_minute`,
  each message counts as a request; a connection over the limit is closed.
- A script error closes the connection with code 1011. When the server stops, connections are closed with 1001.
- WebSockets are served over HTTP/1.1 (and HTTPS); HTTP/2 clients fall back to it.

#### Server-Sent Events

The same script answers Server-Sent Events, for browsers' `EventSource` (which also reconnects by itself):

```js
const events = new EventSource("/chat?name=ann");
events.onmessage = (e) => console.log(e.data);
```

- A `GET` that accepts `text/event-stream` to the path of a `*.ws.phv` script, where no page answers, runs its
  `WsEvent::Open` event as for a WebSocket (middleware, `request()`, `session()`; a status other than 200 refuses it).
  The response is then a stream: what the script sends (`$ws->send()`) and what is published to the topics it joined
  arrives as events, a `data:` line for each line of the text. When the client goes away, `WsEvent::Close` runs. There
  are no `WsEvent::Message` events: a client sends with an ordinary request (a page that calls `publish()`).
- An event stream counts as a WebSocket connection (1,024 at once with them), has their queue of 1 MB (a client that
  does not read is dropped), and gets a comment line (`: ping`) every 15 seconds so proxies keep it open. It is never
  compressed or cached, works over HTTP/1.1 and HTTP/2, and other sites may open one only as for WebSockets (`[cors]`).
  A stream holds no worker between events.
- Events count until they are sent, in a budget of their own: a quarter of `workers` × `memory_limit_mb`. Pages
  always have the other three quarters, so streams never make pages wait, and pages and streams together stay within
  `workers` × `memory_limit_mb`. A stream may hold 2 MB unsent, and a message whose event is larger (1 MB of text with
  more than about 150,000 lines) is not sent as an event.
- A stream that still holds output may fill three quarters of the budget; the last quarter is kept for streams that
  have sent everything. A stream that has held output for a second is ended when its next event would pass 2 MB or
  find no room; one that took its last event less than a second ago skips the event instead.
- A stream must get back to having sent everything within 30 seconds, or it is ended. So a client that reads only a
  little now and then cannot keep its output counted.
- When a stream is ended while it still holds output, its connection is dropped at once (over HTTP/2, with the other
  requests on it), so what it held is freed; the browser connects again. A stream that the script closes may still
  send what it holds, within the same 30 seconds.
- When the budget is full, a stream that has sent everything drops the connections of the streams that have held
  output for a second, waits up to 0.2 seconds for room, and skips the event if there is still none (delivery is at
  most once). Ended streams and skipped events count as `output` in `phv_rejected_total`. So clients that do not
  read cannot end the streams of those that do, nor slow down pages, nor make readers miss events for more than
  about a second.

#### publish() across server processes

Without a store, `publish()` reaches the connections of its own server process. When a site runs several processes or
machines behind a load balancer, name a Redis database of `[databases]` that carries the messages between them:

```toml
[databases.cache]
url_env = "REDIS_URL"

[security]
secret_env = "PHV_SECRET"     # needed: the messages are signed with it

[server]
publish_store = "cache"
```

- A process delivers what its scripts publish to its own connections first, as without a store, then queues it for
  the others; a script never waits for Redis. The queue holds at most 10,000 messages and 32 MB: when it is full, or
  Redis cannot be reached, messages for the other processes are dropped (counted in `phv_publish_total{result="dropped"}`,
  and a warning is logged at most once a minute).
- Each process sends the queue to Redis (PUBLISH, in pipelines) and keeps one subscribed connection of its own, which
  hands each message to the connections of the process that joined its topic. A lost connection is opened again after
  1 second, then 2, 4, ... up to 30 seconds; meanwhile `/ready` answers 503 (see "Health checks and metrics").
- **Delivery is at most once.** A message published while a process is not connected to Redis never reaches that
  process's connections, nor does one sent before it subscribed. Redis drops a subscriber that falls too far behind
  (`client-output-buffer-limit pubsub`, 32 MB by default): the process connects again, and what was sent in between is
  lost. Keep what must not be lost in a database, and use `publish()` to say that something changed.
- Messages are signed with a key derived from `secret_env` and carry the id of the process that sent them, the time
  they were sent and a number that grows with each message. A process skips its own. It drops a message sent more
  than 60 seconds before or after its own clock, and one sent before the process last subscribed (after a lost
  connection too), judged by the sender's clock as the process saw it: such a message was never owed to it. These
  count as `clock` in `phv_publish_total`, and the first from each sending process logs a warning (keep the servers'
  clocks in step). It drops (and counts as `invalid`) a message
  it has already received or older than one it received from the same process, one that is not signed with the site's key, is malformed,
  breaks the rules of `publish()` (a topic of 1 to 200 bytes without control characters, a text of at most 1 MB), or is
  longer than that: such a message is read and thrown away as it arrives, never held. The channel's name is derived
  from the key too, so sites that share a Redis server do not see each other's messages. Scripts cannot publish to
  Redis themselves (`std.db` has no PUBLISH).
- Anyone who can write to the Redis server can still drop messages or stop the store; give PHV a Redis user of its own
  and a `rediss://` URL for a server on another machine.

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
- Connections are kept open between requests to the same host (at most 8 per host and 256 in all, closed after 30
  idle seconds): one is kept only when its response ended where its framing said (`Content-Length`, or a chunked body
  with nothing after it) and the server did not ask to close it, and it is used again only if nothing arrived on it
  while it was idle. Only `GET`, `HEAD` and `OPTIONS` requests take a kept connection; one the server closed meanwhile
  is retried once on a new connection, only when no byte of an answer came.
- Each request waits at most until the script's time limit (30 seconds when it has none),
  whatever the server sends: the name lookup, every read and every write stop at that deadline, and at most 10
  interim (`1xx`) responses are skipped. The addresses a host name was looked up to are kept for 30 seconds (a
  failed lookup is not kept), and requests for the same name at the same time share one lookup. Request and response
  bodies are at most 16 MB. The body is read as UTF-8 text.
- Connection failures and network errors raise `NetworkError`, naming the service but never its URL, host or headers
  (errors are described in fixed words, such as "the server's TLS certificate is not valid"). An
  unset `url_env` or `headers_env` variable is reported when a request is sent, so `phv check` works without the
  production keys.

### Validation (std.validate)

```phv
import std.validate.Validator;

$v = new Validator(request()->formParams);
$v->required("name")->maxLength("name", 50);
$v->required("email")->email("email");
$v->int("age", min: 0, max: 150);
$v->in("plan", ["free", "pro"]);
$v->minLength("password", 12)->same("password_again", "password", "the passwords differ");
$v->check("terms", request()->form("terms") == "yes", "accept the terms");
if ($v->fails()) {
    $errors = $v->errors();          // map<string, list<string>>: the messages of each field
    $first = $v->first("email");     // ?string
}
$name = $v->value("name");           // ?string, trimmed (null when empty)
```

Rules: `required`, `minLength`, `maxLength` (characters), `email`, `url` (http and https), `int` and `number` (with
`min` / `max`), `in`, `same`, and `check` for checks of your own. Every rule takes a last `$message` argument to
replace the English default. A rule other than `required` passes when the field is missing or empty. Rules check
the value with spaces trimmed, so store `$v->value("name")`, which is what was checked. `is_email($s)`
is the email check on its own.

### Mail (std.mail)

```toml
[mail.default]
url_env = "SMTP_URL"     # smtps://user:password@smtp.example.com (TLS), or smtp://...:587 (STARTTLS)
from = "Example <noreply@example.com>"
# tls = false            # allow a server on another machine without TLS (not recommended)
```

```phv
import std.mail.Mail;

Mail::to($email)
    ->subject("Welcome")
    ->text("Thanks for signing up.")
    ->html(render("views/mail/welcome.phv.html", name: $name))
    ->send();                        // through [mail.default]; ->send("alerts") for [mail.alerts]
```

- Scripts can send only through the servers named in `[mail]`. `smtps://` uses TLS from the start; `smtp://` upgrades
  with `STARTTLS`, which a server on another machine must offer (unless `tls = false`). A user name and password are
  never sent without TLS to another machine.
- An address is a plain `name@domain` (no display name, no line breaks), and the subject cannot contain line breaks
  (`ValueError`), so a value from a form cannot add headers or recipients. A message has 1 to 50 recipients and at most
  10 MB. The text part is always sent; `html()` adds an HTML alternative.
- Sending waits for the server, at most the script's time limit or 30 seconds, and throws `NetworkError` if it fails.

### Regular expressions (std.regex)

```phv
import std.regex.Regex;

$date = new Regex('^(\d{4})-(\d{2})-(\d{2})$');       // single quotes keep the backslashes
$date->test("2026-11-05");                                // bool
$date->match("2026-11-05");                               // ?list<string>: ["2026-11-05", "2026", "11", "05"]
(new Regex('\d+'))->matchAll("1 22 333");                 // list<list<string>>
(new Regex('(?<y>\d{4})-(?<m>\d{2})'))->replace($s, '${m}/${y}');
$words->replaceWith($s, fn(list<string> $m): string => $m[1]->upper());
(new Regex('\s*,\s*'))->split("a , b,c");                 // ["a", "b", "c"]
Regex::escape("1+1=2?");                                   // a pattern that matches the text as it is
```

The syntax is that of Rust's `regex` crate: `(?i)` for no case, `(?<name>...)` for a named group, `\d \w \s`, Unicode
classes such as `\p{L}`; there is no look-around and no back-reference. In exchange there is no catastrophic
backtracking: a search takes at most time proportional to the length of the text times the size of the compiled
pattern, and usually much less.

- The search itself cannot be interrupted, so a search whose worst case (the pattern's states × the length of the
  text) could not end in the time the script has left throws `RuntimeException` before it starts. Ordinary patterns
  search megabytes (an e-mail pattern, with about 1,000 states, about 3 MB in 30 seconds); a very large pattern, such
  as long repetitions of Unicode classes (`\w{40}`), only a shorter text. A pattern from a visitor therefore cannot
  hold a worker past the time limit.
- An invalid pattern throws `ValueError`. Patterns are at most 64 KB, 2 MB once compiled, and 1,000 groups; a pattern
  whose groups would need more than 16 MB to search (states × groups) is refused too.
- Results count toward the memory limit as they are built.
- The server process keeps the patterns used last compiled, for every request: at most 64 of them and 32 MB (the
  least recently used go first).

In `match()`, group 0 is the whole match and a group that took no part is `""`.

### Dates and times (std.time)

```phv
import std.time.{DateTime, Duration};

$meeting = DateTime::of(2026, 11, 5, 10, 0, 0, "Asia/Tokyo");
$meeting->toIso();                                    // "2026-11-05T10:00:00+09:00"
$meeting->inZone("Europe/Berlin")->format("H:i T");   // "02:00 CET"
$meeting->plus(Duration::hours(36));                  // an exact length of time
$meeting->addDays(1)->addMonths(1);                   // calendar steps, at the same local time
$meeting->diff(DateTime::now())->totalHours();
DateTime::parse("2026-11-05 09:30", "Asia/Tokyo");    // ?DateTime
```

A `DateTime` is an instant (`timestamp`, seconds since 1970-01-01 UTC) seen in an IANA time zone (`zone`, such as
`"Asia/Tokyo"`, `"America/New_York"` or `"UTC"`). It never changes; its methods return new values.

| Member | Description |
|---|---|
| `DateTime::now($zone = "UTC")`, `DateTime::fromTimestamp($ts, $zone = "UTC")` | |
| `DateTime::of($y, $m, $d, $h = 0, $i = 0, $s = 0, $zone = "UTC")` | `ValueError` for a date that does not exist (February 30) |
| `DateTime::parse($text, $zone = "UTC"): ?DateTime` | `2026-11-05`, `2026-11-05 10:00[:30]` (a local time in `$zone`), or RFC 3339 with an offset or `Z` |
| `year()`, `month()`, `day()`, `hour()`, `minute()`, `second()`, `weekday()` (1 = Monday), `offset()` | In its zone; `offset()` is seconds east of UTC |
| `format($f)`, `toIso()` | The letters of `date()`; `T` is the zone's abbreviation (`JST`, `EDT`) |
| `inZone($zone)`, `plus(Duration)`, `minus(Duration)`, `startOfDay()` | |
| `addDays($n)`, `addMonths($n)`, `addYears($n)` | Calendar steps at the same local time; `addMonths` keeps the day, or takes the last day of a shorter month (January 31 + 1 month = February 28) |
| `diff($other): Duration`, `isBefore($other)`, `isAfter($other)`, `equals($other)` | Compare instants (in any zones) |
| `Duration::seconds/minutes/hours/days/weeks($n)`, `plus`, `minus`, `totalMinutes()`, `totalHours()`, `totalDays()`, `toText()` | `toText()` is like `1d 2h 3m 4s` |

- A local time that happens twice when clocks go back is the earlier one; one that does not happen when they go
  forward is moved past the gap (02:30 becomes 03:30).
- `plus(Duration::days(1))` is exactly 24 hours later, while `addDays(1)` is the same local time on the next day: the
  two differ across a change of clocks.
- An unknown zone throws `ValueError`.

### Background jobs (std.jobs)

Work that should not hold up a page (sending mail, resizing a report, calling a slow service) goes into a **job**: a
script in `jobs/` that `phv serve` runs outside any request.

```toml
[jobs]
store = "db:main"     # "memory" (default), or a PostgreSQL, MySQL or Redis database of [databases]
# dir = "jobs"        # the directory of the job scripts (outside public/)
# workers = 2         # jobs one server process runs at once (1 to 64)
# time_limit = 300    # seconds a run may take (1 to 3600)
# max_attempts = 5    # runs before a failing job is given up (1 to 100)
# retry_delay = 30    # seconds before a failed job runs again, doubled each time (at most an hour)
# run = true          # false: phv serve only adds jobs; `phv jobs run` runs them

[[schedule]]
job = "cleanup"
cron = "0 3 * * *"        # minute, hour, day of the month, month, day of the week
timezone = "Asia/Tokyo"   # default: UTC
```

```phv
// public/signup.phv
import std.jobs.enqueue;
enqueue("send_welcome", json_encode(["user" => $id]));      // ?string: the job's id
enqueue("report", "", delay: 3600);                          // in an hour at the earliest
enqueue("sync", (string)$id, key: "sync:{$id}");             // added once while one waits (null otherwise)
```

```phv
<?phv
// jobs/send_welcome.phv
import std.jobs.job;
$j = job();                 // Job: name, id, payload, attempt (1 for the first run)
$data = json_decode($j->payload);
```

- A job is the script `<dir>/<name>.phv`; names are lowercase letters, digits and `_`. `enqueue()` throws `ValueError`
  for a job that does not exist, a payload over 64 KB, a delay over 30 days or a key over 200 bytes, and
  `RuntimeException` when the store holds 100,000 waiting and running jobs or 1 GB of their payloads (10,000 or 64 MB
  in memory), also inside a `transaction()`. It throws `RuntimeException` too in a project without `[jobs]`. It waits
  for the store no longer than the script may still run (and 10 seconds at most), on the database server too: a
  statement of the store, a wait for a lock included, is stopped there (PostgreSQL `statement_timeout`; MySQL
  `innodb_lock_wait_timeout` and `KILL QUERY`). `dir` must be a directory of the project, not a link. The memory store
  also keeps at most 16 MB of jobs given up and 100,000 keys of ended jobs.
- `phv serve` runs `[jobs] workers` jobs at once in each process, on threads of their own: jobs never take the
  workers of pages. Each run gets a fresh VM, `time_limit` and `[security] memory_limit_mb`, and the permissions of
  phv.toml, as a page does. `request()` and `session()` are empty, and `job()` works only in a job. A job may
  `publish()` (to WebSocket connections) and `enqueue()` other jobs; what a run printed is discarded.
- **A job runs at least once.** A run takes the job with a lease (`time_limit` + 30 seconds). A job that throws, or
  hits a limit, runs again after `retry_delay` seconds, then twice that, and so on; after `max_attempts` runs it is
  given up and kept for a week (at most 10,000), with its error. A job whose process stopped while it ran is taken
  again when its lease runs out. So write jobs that may run twice without harm (or give them a `key`).
- A `key` adds a job once: while a job of the same name and key waits, runs or was given up, and for a day after it
  ended, `enqueue()` returns `null`. A store remembers the keys of at most 100,000 ended jobs (the latest). While
  another open transaction has added a job with the same key, `enqueue()` waits for it to end, and no longer than the
  script may still run; such waits are kept apart from the connections that other jobs are added on and that run
  jobs.
- With PostgreSQL or MySQL, `enqueue()` inside a `transaction()` on the store's database adds the job with the
  transaction: only if it commits (with MySQL, only `transaction()` ties them: a `BEGIN` sent as SQL does not). Jobs
  are kept in a table `phv_jobs_<hash>` of the site's namespace (see "Sessions"; `phv_jobs` for a site without one),
  created when first needed and taken with `SELECT ... FOR UPDATE SKIP LOCKED`; Redis keeps them under
  `phv:jobs:<hash>:`. Every server process and machine of the site using the same store shares the jobs; other sites
  on the same database do not see them; jobs left in the shared table when a site gets a name are not taken. The
  100,000 hold for transactions running at the same time too: `enqueue()` in a transaction takes a lock of the site's
  jobs that the transaction holds until it ends (PostgreSQL an advisory lock, MySQL a row of a table
  `phv_jobs_lock`), so transactions that add jobs to the same site wait for one another, and so does an `enqueue()`
  outside a transaction when the queue is within 10,000 jobs (or 256 MB) of the caps. Farther from the caps it does
  not wait: a transaction that adds more than 10,000 jobs while others are added outside can still pass the cap by its
  jobs beyond 10,000. Near the caps, a transaction that has added a job holds up every other job added to the site
  until it ends, so call `enqueue()` last in a transaction. The store is counted when a transaction first adds to it,
  and outside transactions once a second; the count is kept in a table `phv_jobs_counts`. A jobs table made before
  1.1.1 gets a column `size` (the bytes of each payload) when a process first uses it; if another transaction holds
  the table then, the process tries again a minute later, and until then the bytes of its jobs are not counted. With
  `store = "memory"` the jobs live in the server process and are lost when it stops.
- **Schedules**: each `[[schedule]]` adds its job (with an empty payload) in the minutes its `cron` expression
  matches, in `timezone`. Fields are `*`, numbers, ranges `a-b`, steps `*/n` and lists `a,b`; days of the week are 0
  to 7 (Sunday is 0 and 7); when the day of the month and the day of the week are both given, either one matches. Each
  process adds the jobs, keyed by the minute: with a shared store a job is added once however many processes run;
  with `store = "memory"` each process runs its own. Minutes while no process was running are not made up for. When
  the clocks change in `timezone`, a time that does not exist that day does not run, and one that comes twice runs
  twice. At most 100 schedules.
- `phv jobs run [dir]` runs the jobs (and schedules) of a project without serving its pages, until Ctrl-C or
  SIGTERM, for a worker machine; with `[jobs] run = false`, `phv serve` only adds jobs (`phv jobs run` and a built
  site's `--jobs-only` run them all the same). A built site's jobs are those built into it. `phv jobs list [--failed]`
  shows the jobs of a shared store, and `phv jobs retry <id>` puts a job that was given up back in the queue.
- A job that does not compile stops `phv serve`, `phv jobs run` and `phv build`. When the server stops, running jobs
  get what is left of `shutdown_timeout`; one that does not finish runs again elsewhere when its lease runs out.
- The payload is stored as it is: encrypt (`encrypt()`) what must stay secret. A page that adds jobs for visitors
  should be rate limited (`rate_limit()`), so a visitor cannot fill the queue.

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
  waiting for it. A script can have 16 operations running at once, and on one database at most half its `pool_size`
  (at least 1); starting more throws `RuntimeException`.
- The work's result may take a sixteenth of the memory left to the script when it starts (and at most 16 MB for an
  HTTP response); a larger result is an error. It is counted against the script's limit when it is awaited.
- `queryAsync` / `executeAsync` on a database server use a connection of their own from the pool, so they run
  outside the script's transaction; give `pool_size` room for them. With SQLite the statement runs right away.
- Work that was started and never awaited is still waited for when the script ends (until its time limit at most):
  a request cannot leave work running after it, holding a thread and a connection.
- The work runs on threads the server process shares between requests: a thread is started when none is free (up to
  1,024 operations at once in the process) and ends after a few seconds without work.
- `new Future(fn(): T => ...)` defers any work: it runs once, when the future is first awaited. Deferred work that is
  never awaited does not run.

### Dates, cryptography, and encoding

| Function | Description |
|---|---|
| `date($format, ?$ts)` / `gmdate(...)` | Same format as PHP (`Y-m-d H:i:s`, `D, d M Y`, `c`, `U`, `T`, etc.). `date` uses the server's local time; for time zones, see std.time |
| `mktime($h, $i, $s, $m, $d, $y)`, `strtotime($s): ?int`, `checkdate($m, $d, $y)` | `strtotime` accepts ISO formats and `now`/`today`/`tomorrow`/`yesterday` |
| `random_int($min, $max)`, `random_token($bytes = 32)` | The OS's cryptographic random number generator (do not use `rand()` for cryptography) |
| `password_hash($pw)`, `password_verify($pw, $hash)` | Argon2id |
| `hash("sha256" \| "sha384" \| "sha512", $data)`, `hash_hmac($algo, $data, $key)`, `hash_equals($a, $b)` | `hash_equals` takes a comparison time that does not depend on the contents |
| `totp_secret()`, `totp_code($secret, ?$time)`, `totp_verify($secret, $code): ?int` | Two-factor codes of authenticator apps (TOTP, RFC 6238: HMAC-SHA1, 6 digits, 30 seconds). The secret is 160 random bits as Base32. `totp_verify` accepts one step of clock drift either way, compares in constant time, and returns the time step it matched (or `null`): store it and refuse a step that is not newer, so a code cannot be used twice. Store it in the database with one statement that only succeeds for a newer step (`UPDATE ... SET step = {$step} WHERE id = {$id} AND step < {$step}`, then check the affected rows): with the session, two requests sending the same code at the same time could both pass |
| `qr_svg($text): Html` | A QR code as an `<svg>` element (for example of an `otpauth://` URI). The markup holds only the modules, not the text, so it is safe to output |
| `base64_encode`, `base64_decode: ?string`, `urlencode`, `rawurlencode`, `urldecode`, `http_build_query` | |
| `json_decode($s): any` | Depth up to 128 |
| `encrypt($text, $key)`, `decrypt($box, $key): ?string` | ChaCha20-Poly1305, with a key derived for each message (HKDF-SHA256 with a random salt) and a random nonce; the result is base64url text. `decrypt` returns `null` if the text was changed or the key is not the one it was encrypted with |
| `sign_token($data, $key)`, `sign_token($data, $key, $seconds)`, `verify_token($token, $key): ?string` | Data with an HMAC-SHA256 signature, valid for `$seconds` when given (links in e-mails, such as password resets). The data can be read (it is only base64url): encrypt what must stay secret. `verify_token` returns the data, or `null` if the token was changed, signed with another key, or has expired |
| `jwt_encode($claims, $key)`, `jwt_decode($token, $key): ?map<string, any>` | JSON Web Tokens with HS256 only: a token that names another algorithm (`none` included) is refused. `exp` and `nbf` are checked. The key is used as it is (as other JWT libraries expect), so do not also use it with `hash_hmac()`: an HMAC of a visitor's text would be a valid signature |

Keys for `encrypt`, `sign_token` and the JWT functions are secrets of at least 32 bytes (`random_token()` makes one);
keep them in an environment variable (`getenv`), not in the code. `encrypt` and `sign_token` derive their own keys
from the one given, so one key can serve both. Tokens are compared as they are: one with spaces around it is invalid.

### Content-Security-Policy

Page responses get the following policy by default (`{nonce}` is a per-response `'nonce-...'`; writing `'nonce-{nonce}'` is equivalent).

```
default-src 'self'; script-src {nonce} 'strict-dynamic'; style-src 'self' 'unsafe-inline';
img-src 'self' data: https:; font-src 'self' data:; object-src 'none'; base-uri 'self';
form-action 'self'; frame-ancestors 'self'
```

A page runs only scripts carrying its nonce, and the scripts they load (`'strict-dynamic'`): a `<script src>` that
got past escaping cannot load even a script of the site. Static files have no nonce and get `script-src 'self'` instead.

- PHV automatically adds `nonce="..."` to `<script>` tags written in templates (`.phv.html` and the HTML parts of `.phv`).
  As a result, inline scripts you write yourself work as is, while scripts injected by getting
  past escaping (such as a `<script>` passed to `raw()`) do not run.
- Event attributes such as `onclick="..."` and `javascript:` URLs are blocked. Use `addEventListener`.
  Writing them in a template produces a compile-time warning (not when `csp` allows inline code).
- When a `<script>` built from strings needs a nonce, use `csp_nonce()`
  (`raw("<script nonce=\"" . csp_nonce() . "\">...</script>")`).
- Static files and error pages get the policy without a nonce, with `script-src 'self'` (inline scripts in static `.html` files do not run).
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
  `DatabaseError`, and `requests_per_minute` counts in the process; each failure is logged. After such a failure the
  server is not asked again for 5 seconds: meanwhile every call fails the same way at once, without waiting for it.
- The same key is counted separately for each window length (you can apply both 5 per minute and 50 per day). When there are more than 100,000 keys,
  keys that have not reached their limit are discarded first, so using a large number of keys does not release keys that are currently limited.
- `requests_per_minute` counts IPv6 addresses grouped by /64. Do the same in keys: `ip_group($ip)` returns the /64 of
  an IPv6 address (and an IPv4 address as it is), as in `rate_limit("login:" . ip_group(request()->ip), 5, 60)`, since one
  visitor can use any address of their network.
- Behind a proxy, `request()->ip` is the proxy's address. Read the visitor's address from the proxy's header (such as
  `Fly-Client-IP` on Fly.io) only when the site runs behind that proxy: anywhere else the header is the client's own text.
- The site-wide limit is `[security] requests_per_minute` (§14).

### Files and directories

| Function | Description |
|---|---|
| `file_append($path, $text)` | Adds to the end of a file, creating it if needed (`write`) |
| `filesize($path)`, `filemtime($path)`, `is_dir($path)` | Size in bytes, time of the last change (Unix time), whether it is a directory (`read`) |
| `scandir($dir): list<string>` | Names of the files and directories in `$dir`, sorted; links are left out (`read`) |
| `mkdir($dir)` | Creates the directory and any missing parents inside the permitted one; an existing directory is fine (`write`) |
| `copy($from, $to)`, `rename($from, $to)` | Of a file (`read` for the source of `copy`, `write` otherwise). The rules below apply to both paths |

### Permissions (phv.toml)

A script can touch nothing outside the heap unless it is permitted in `phv.toml`. **If nothing is written, nothing is permitted.**

```toml
[permissions]
read = ["content/"]         # directories readable with file_get_contents / file_exists / scandir ...
write = ["data/", "storage/"]  # file_put_contents / unlink / mkdir / rename / saveTo / Database::open (reading is also allowed)
env = ["APP_*"]             # environment variables readable with getenv (a trailing * is a prefix match)
```

- Paths are relative to the project. `..`, absolute paths and names with `~` (Windows short names such as `GIT~1`)
  are not allowed.
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
phv = "^1.0"            # the versions of PHV the project works with (§18)

[security]
csrf = true             # default: true
secure_cookies = false  # true when serving over HTTPS
session_ttl = 7200      # seconds
memory_limit_mb = 128   # per request (one execution)
csp = "default"         # "off" or a custom policy that may contain {nonce}
requests_per_minute = 0 # limit per client address (0 means unlimited)
session_store = "memory"  # or a directory ("sessions/"), or a database ("db:cache")
rate_limit_store = "cache"  # count rate limits on this Redis database (default: in the process)
secret_env = "PHV_SECRET"   # the server's secret, shared by its processes (see "CSRF protection")
store_namespace = "blog"    # the site's name in shared stores (default: [project] name; see "Sessions")

[server]
workers = 16            # scripts running at once (default: two per CPU, at least 8)
static_max_age = 3600   # Cache-Control max-age of static files (default: revalidate each time)
compress = true         # gzip text responses (default)
access_log = "logs/access.log"   # also append the access log to this file
access_log_format = "json"       # "text" (default) or "json"
shutdown_timeout = 30   # seconds running requests get when the server stops
tls_cert = "certs/fullchain.pem"   # serve HTTPS and HTTP/2 (see "HTTPS and HTTP/2")
tls_key = "certs/privkey.pem"
h2c = false             # also accept HTTP/2 without TLS, from a proxy that speaks it
request_ids = true      # give each request an id (see "Request ids")
publish_store = "cache" # carry publish() to the other server processes (see std.ws)

[admin]
listen = "127.0.0.1:9100"   # /health, /ready and /metrics (see "Health checks and metrics")

[cors]
origins = ["https://app.example.com"]

[mail.default]
url_env = "SMTP_URL"
from = "Example <noreply@example.com>"

[jobs]
store = "db:main"       # background jobs (see "Background jobs")

[[schedule]]
job = "cleanup"
cron = "0 3 * * *"

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
| `count(list\|map\|set): int` | Number of elements |
| `set(list<int>): set<int>` / `set(list<string>): set<string>` | The distinct elements of a list, in their first order |
| `strlen(string): int` | Number of bytes (use `->len()` for the number of characters) |
| `implode(string $sep, list<string>): string` / `explode(string $sep, string): list<string>` | Join / split |
| `str_repeat`, `str_contains`, `str_starts_with`, `str_ends_with`, `str_replace(search, replace, subject)` | Same as PHP |
| `strtoupper`, `strtolower`, `ucfirst`, `trim` | Same as PHP |
| `json_encode(any): string` | Outputs only the public properties of objects; enum cases as their value |
| `json_encode_pretty(any): string`, `json_decode_as(string)` | Typed JSON (§14, JSON) |
| `dump(any): void` | Equivalent to `var_dump` |
| `raw(string): Html`, `escape(string): Html`, `htmlspecialchars(string): Html`, `nl2br(string): Html` | §13 |
| `intdiv`, `abs`, `min`, `max`, `floor`, `ceil`, `round`, `sqrt`, `fmod` | Numeric |
| `sin`, `cos`, `tan`, `asin`, `acos`, `atan`, `atan2`, `exp`, `log(x)` / `log(x, base)`, `log10`, `log2`, `hypot`, `pi()` | Mathematics (float) |
| `clamp(x, min, max)` | int or float |
| `random_float(): float` | In `[0, 1)`, from the operating system's random source |
| `bindec`, `octdec`, `hexdec` (string → int), `decbin`, `decoct`, `dechex` (int → string), `base_convert(string, from, to)` | Number bases (2 to 36) |
| `format(string, ...)` | Formatted text (below) |
| `uuid_v4(): string`, `uuid_v7(): string` | Random UUID; v7 starts with the time, so they sort by creation |
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
| `encrypt`, `decrypt`, `sign_token`, `verify_token`, `jwt_encode`, `jwt_decode` | Encryption and signed tokens (table in §14) |
| `base64_encode`, `base64_decode`, `urlencode`, `rawurlencode`, `urldecode`, `http_build_query` | Encoding |
| `base64url_encode`, `base64url_decode`, `bin2hex`, `hex2bin` | Encoding (decoding to text that is not UTF-8 throws `ValueError`) |
| `file_get_contents`, `file_put_contents`, `file_exists`, `unlink`, `getenv` | Require `[permissions]` |
| `file_append(path, text)`, `filesize(path)`, `filemtime(path)`, `is_dir(path)`, `scandir(dir): list<string>`, `mkdir(dir)`, `copy(from, to)`, `rename(from, to)` | Files and directories; require `[permissions]` (below) |
| `csv_parse(string): list<list<string>>`, `csv_encode(list<list<string>>): string` | RFC 4180 (quoted fields, `""`, line breaks in fields); `csv_encode` ends lines with CRLF. Spreadsheets run a cell that starts with `=`, `+`, `-` or `@` as a formula: when visitors' text goes into a file people open in one, prefix such cells with `'` |
| `parse_url(string): map<string, string>`, `parse_query(string): map<string, string>` | Keys `scheme`, `host`, `port`, `user`, `pass`, `path`, `query`, `fragment` (those present); an invalid URL throws `ValueError` |
| `log_info(msg, ?fields)`, `log_warn(...)`, `log_error(...)` | One line on standard error, next to the server's own log: `2026-11-05T01:00:00.000Z INFO "signed in" user=ann n=3`. Text with spaces, `"` or `=` is quoted; line breaks, control characters and the characters that reorder text (U+202E...) are escaped, so a value cannot forge a line. With `[server] request_ids`, lines written for a request end with `req=<id>`. The server's own lines (errors of pages and jobs) are escaped the same way |
| `argv(): list<string>`, `stdin_read_line(): ?string`, `stdin_read_all(): string` | Only in scripts run with `phv run` (elsewhere `RuntimeException`): the arguments after the file, and standard input |
| `sql_ident(string): Sql` | Column and table names |
| `assert(bool, ?string)`, `assert_eq(any, any, ?string)` | For tests |

### format()

```phv
format("{} has {:,} points ({:.1}%)", $name, 1234567, 98.26);   // "Ann has 1,234,567 points (98.3%)"
format("[{:>6}] [{:<6}] [{:*^7}] [{:05}] [{:x}] [{:08b}] {{braces}}", "r", "l", "c", -42, 255, 5);
```

Each `{}` takes the next value; `{:spec}` formats it: `[[fill]align][0][width][,][.precision][type]`, where align is
`<`, `>` or `^`, `,` groups thousands, `.N` gives N decimals, and type is `x` / `X` (hex), `o`, `b` or `e`. `{{` and
`}}` are literal braces. The format string must be a literal: the number of values and each value's type are checked
when the program is (`{:.2}` needs a number, `{:x}` an `int`). Widths and precisions are at most 1000.

### string methods

`len()` (number of characters), `isEmpty()`, `upper()`, `lower()`, `ucfirst()`, `trim()`, `ltrim()`, `rtrim()`,
`contains($s)`, `startsWith($s)`, `endsWith($s)`, `replace($search, $replace)`, `split($sep)`,
`slice($start, $length?)` (in characters; negative values count from the end), `indexOf($s): ?int`, `repeat($n)`,
`padStart($len, $pad)`, `padEnd($len, $pad)`, `toInt(): ?int`, `toFloat(): ?float`, `chars()`

### list methods

`len()`, `isEmpty()`, `contains($v)`, `indexOf($v): ?int`, `first(): ?T`, `last(): ?T`,
`slice($start, $length?)`, `reverse()`, `concat($other)`, `unique()`, `sorted()`, `sortBy(fn($a, $b) => int)`,
`map(fn($x) => ...)`, `filter(fn($x) => bool)`, `find(fn($x) => bool): ?T`, `any(...)`, `all(...)`,
`join($sep)`, `sum()`, `reduce($initial, fn($acc, $x) => ...)`, `flatMap(fn($x) => list)`, `flatten()`,
`groupBy(fn($x) => string|int): map<K, list<T>>`, `chunk($size): list<list<T>>`, `zip($other, fn($a, $b) => ...)`
(as long as the shorter list), `partition(fn($x) => bool): list<list<T>>` (`[$yes, $no] = ...`),
`count(fn($x) => bool)`, `min(): ?T` / `max(): ?T` (int, float or string), `removeAt($i)`, `insertAt($i, $v)`

Methods return new lists and leave the list they are called on as it is. `sorted()` puts `NAN` after every number.

Append an element with `$xs[] = $v;` and update one with `$xs[$i] = $v;` (an index out of range raises `IndexError`).

### map methods

`len()`, `isEmpty()`, `has($k)`, `get($k): ?V`, `keys()`, `values()`, `merge($other)`, `remove($k)`,
`filter(fn($k, $v) => bool)`, `map(fn($v) => ...)` (same keys), `sortedByKey()`, `sortedByValue()` (int, float or
string values; equal values keep their order)

Add or update with `$m[$k] = $v;`. Reading a missing key with `$m[$k]` raises `KeyError`; `$m[$k] ?? $default` does not throw.

### set methods

`len()`, `isEmpty()`, `has($v)`, `add($v)`, `remove($v)`, `union($other)`, `intersect($other)`, `diff($other)`,
`isSubsetOf($other): bool`, `toList(): list<T>`, `filter(fn($x) => bool)`, `any(...)`, `all(...)`

```phv
$tags = set(["php", "web", "php"]);   // set<string> with 2 elements
set<int> $seen = [];                  // `[...]` where a set is expected is a set
$seen[] = 42;                         // adds in place; adding an element already there does nothing
foreach ($tags as $tag) { ... }       // in the order the elements were added
```

Methods return new sets and leave the set they are called on as it is. Two sets are `==` when they have the same
elements, in any order. A set cannot be indexed (`$s[0]` is an error; use `has()`). `json_encode()` writes a set as
an array, and `json_decode_as()` reads an array into a `set<int>` or `set<string>`. `$x is set` checks for a set.

When code uses a PHP function name, the error message suggests the alternative
(for example, `strpos` → `->indexOf()`, `isset` → `??`, `var_dump` → `dump()`, `eval` → not available).

## 16. Tools

| Command | Description |
|---|---|
| `phv run file.phv [args...]` | Type-check and run (`--html`, `--time-limit seconds`, `--profile`); `argv()` returns the arguments after the file |
| `phv check file.phv` | Type-check only (including imported modules and templates) |
| `phv serve dir` | Web server (`--port`, `--host`, `--prod`, `--time-limit`; `--watch` reloads the pages open in the browser when a file of the site is saved, in development only) |
| `phv build path -o file` | Compile a site or a script into one executable (see below) |
| `phv new dir [--site]` | Create a project: a script with a module and a test, or with `--site` a site with a page, a layout and a page test |
| `phv test [paths]` | Run the `test_*` functions in `*_test.phv` files (a new VM for each test; `--filter`, `--json`, `--junit`, `--coverage`, see Testing) |
| `phv fmt [paths] [--check] [--width N]` | Normalize indentation and whitespace and wrap long lines (strings, comments, and HTML are not changed) |
| `phv lsp` | Language Server for editors (see Editors below) |
| `phv dap` | Debug adapter for editors (see Debugging below) |
| `phv doc [paths] [--std] [--format html\|md] [-o file]` | An API reference from doc comments (see API reference below) |
| `phv install [--locked]` | Fetch `[dependencies]` and the packages they need, and pin them in `phv.lock` |
| `phv jobs run [dir]`, `phv jobs list [--failed] [dir]`, `phv jobs retry <id> [dir]` | Run a project's background jobs without serving its pages; list the jobs of its store; put a failed job back (see "Background jobs") |
| `phv migrate file.php` | Convert PHP mechanically and list the places that need manual work (`-o` names the output, which is not written through a link) |
| `phv disasm file.phv` | Show the bytecode |
| `phv parse file.phv` | Show the syntax tree |

### Editors

`phv lsp` is a Language Server, used by the VS Code extension (`editors/vscode`) and any editor that speaks LSP:

- errors and warnings as you type, the type of an expression on hover, completion, go to definition;
- find references and rename across the project's files. Renaming a method also renames the methods that override
  it (or that it overrides), so that overriding still works; names of the standard library cannot be renamed;
- formatting (as `phv fmt`, with `[fmt] width`), and the parameters of the function being called (signature help);
- the symbols of a file (outline) and of the project (search by name);
- quick fixes: a close name for an unknown variable, function, class, method or property, `$_name` for a variable
  that is never used, removing an unused import, `random_int()` for `mt_rand()`;
- semantic colouring of names (classes, functions, methods, properties, variables, constants), and the inferred type
  of a variable shown before the variable where it is first assigned (`$total = ...` shows `int`).

### Debugging and profiling

`phv dap` is a debug adapter (the Debug Adapter Protocol), which the VS Code extension starts for launch
configurations of type `phv`:

```json
{ "type": "phv", "request": "launch", "name": "Run the open script", "program": "${file}", "args": [] }
```

It runs a script as `phv run` does (`stopOnEntry` stops at its first statement) with breakpoints, stepping over,
into and out, pause, the call stack, the variables of each frame (lists, maps and objects can be opened), and the
value of a variable on hover. The program is compiled with a marker before each statement for this and runs without
the JIT; the standard library is not stepped into, and the script cannot read standard input (the protocol uses it).

`phv run --profile` prints, after the run, the functions that took the most time: their own time (with the builtins
they called), their total time (with the functions they called), and the number of calls. Calls are counted exactly;
time is charged to the call stack every thousand steps, so a short function may show none. The JIT is not used.

### API reference

`phv doc [paths]` writes a reference of the exported functions, classes, constants and type aliases of the modules
under `paths`, with their public members and the `/** ... */` comment before each, and the `//` comment at the top
of each file. `--format md` writes one Markdown file instead of one HTML page (`-o` chooses the file; the default is
`doc/index.html` or `doc/api.md`); `--std` documents the standard library's modules (see
[stdlib.md](stdlib.md)). A constructor whose promoted parameters are not all public is left out, as is a
declaration without `export`.

```phv
/**
 * The total of the order, with tax.
 * Throws `ValueError` for an order without items.
 */
export function total(Order $order): float { ... }
```

### Errors and warnings

All errors of a program are reported together. When a file has a syntax error, the other files are still
type-checked and their errors are shown with it; the type errors of the broken file itself, and of the files that
import or render it, appear once it is fixed (they could come from what the parser had to skip).

Code that is never used gives a warning (the program still runs):

| Warning | Not reported for |
|---|---|
| A variable that is never read (`$x = 1;` and nothing reads `$x`) | Names starting with `_` (`foreach (0..$n as $_)`); the parameters of methods, constructors, closures and templates, whose names are fixed by what overrides, calls or renders them |
| A name in an `import` that the module never uses | |
| Code after `return`, `throw`, `break` or `continue`, or after a loop that never ends | |

A `catch` that does not use the exception leaves the variable out (`catch (ValueError)`). While a program has
errors, these warnings are not shown, since the errors often leave such code behind. `phv lsp` shows them as you type.

### Building an executable

```bash
phv build mysite -o mysite-server      # a site: serves it, with the options of `phv serve`
./mysite-server --port 8080 --prod
phv build tools/report.phv -o report    # a script: runs it
```

`phv build` compiles every page of a site (`public/**/*.phv`, with the templates and modules they use) or one script
ahead of time and appends them, with the site's static files and phv.toml, to a copy of the phv executable. The result
runs without the sources and without compiling at startup. It needs no other files except the directories the project
reads and writes (`[permissions]`, a session directory), which are looked up under `--root` (default: the current
directory). Only the static files the server would serve are included (not those starting with `.` or `_`, nor
types it never serves, such as `.toml` or `.env`). phv.toml is included: keep secrets in environment variables
(`url_env`, `headers_env`), not written in phv.toml, or they are in the executable.

The bundle carries its SHA-256 and the version of phv that built it; a damaged bundle, or one from another version,
is refused at startup. Build the executable on the platform it runs on (it is a copy of that platform's phv).
The jobs of `[jobs]` are compiled into a site's executable too; `--jobs-only` runs them without serving the pages
(as `phv jobs run`).

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

Functions marked `#[Before]` run before each test of their file, and those marked `#[After]` after each one, also
when the test fails (in the order they are declared, in the same VM as the test). They take no parameters.

```phv
#[Before]
function fresh_database(): void { file_put_contents("data/test.json", "[]"); }

#[After]
function clean_up(): void { unlink("data/test.json"); }
```

| Option | |
|---|---|
| `--filter text` | Only the tests whose name contains `text` |
| `--json` | One JSON document on standard output with every result (`status`: `pass`, `fail` or `skip`), instead of the report |
| `--junit report.xml` | Also write the results as JUnit XML, which CI services display |
| `--coverage` | After the report, the share of the project's statements that the tests ran, per file, with the lines of those that did not run (test files and the standard library are left out; so are pages run through `TestClient`, which the site compiles). The tests then run without the JIT |

Requests to the project's own site (a project with `public/`) are made with `std.testing`, in-process: no server
or network is needed. Pages run as under `phv serve` in development mode (routes, middleware, sessions, error pages),
except that forms need no CSRF token. A `TestClient` keeps the cookies the site sets, like a browser:

```phv
import std.testing.TestClient;

function test_login(): void {
    $c = new TestClient();
    assert_eq($c->get("/account")->status, 302);
    $r = $c->post("/login", ["email" => "ann@example.com", "password" => "secret"]);
    assert_eq($r->location(), "/account");
    assert($c->get("/account")->body->contains("Ann"));
    $api = $c->postJson("/api/notes", ["text" => "hi"]);
    assert_eq($api->status, 201);
    assert_eq($api->header("Content-Type"), "application/json");
}
```

`TestResponse` has `status`, `body`, `header()`, `headers()` (every value), `json()` and `location()`.

### Database migrations

```
migrations/
├── 0001_create_posts.sql
└── 0002_add_tags.sql
```

```bash
phv db status --sqlite data/app.db      # each file: applied or pending
phv db migrate --sqlite data/app.db     # apply the pending ones, in order
phv db migrate --db main                # a PostgreSQL / MySQL server of [databases.main]
```

- Files are named `<number>_<name>.sql` and applied in the order of their numbers, each once. The table
  `phv_migrations` records each applied file with a checksum of its text; `migrate` refuses to run when an applied
  file has changed since (put changes in a new file instead).
- On SQLite and PostgreSQL a migration runs in a transaction with its record, so a failing one leaves nothing behind
  (on SQLite, a migration cannot end that transaction: `BEGIN`, `COMMIT` and `ROLLBACK` in it are refused).
  MySQL commits each schema change at once, so a failing migration may be left half applied there.
- `--dir` names another directory; the database is named with `--db` (from `[databases]`) or `--sqlite` (a file
  inside the project).

### Packages

```toml
[dependencies]
slugify = "^1.2"                                                            # from the registry
textutils = { git = "https://github.com/example/textutils.git", version = "~0.3" }
nightly = { git = "https://github.com/example/nightly.git", rev = "main" }
local = { path = "../shared" }
```

```phv
import textutils.strings.slugify;      // vendor/textutils/strings.phv
```

- A package is a directory of PHV modules, with a `phv.toml` if it has dependencies of its own. It comes from:
  - **the registry**: `name = "<requirement>"`. The registry is a git repository of `index/<name>.toml` files, each
    giving the package's `git` URL (`PHV_REGISTRY` names another registry). The packages are listed at
    phv-lang.org/packages/.
  - **git** with a `version`: the newest tag `vX.Y.Z` that fits the requirement. Requirements are semver ranges:
    `^1.2` (1.2 up to 2.0), `~0.3.1`, `>=1.0, <1.5`, `=2.0.0`, `*`.
  - **git** with a `rev` (a tag, branch or commit; the default branch without one).
  - **path**: a directory used in place. Only the project's own phv.toml can name one.
- `phv install` also installs the packages a package needs (its own `[dependencies]`; nothing else of its phv.toml
  is read). Each package is installed once, into `vendor/<name>`: for version requirements, at the newest version
  that every package asking for it accepts. If none fits all of them, it is an error that lists each requirement and
  who made it. A package asked for in different ways (two git URLs, two revs) is an error too.
- `phv.lock` records every package, also those installed for other packages, with the exact commit (and version).
  Later installs use it while it still fits the requirements; `phv install --locked` installs exactly what it records
  and fails if it is out of date (for CI). A checked-out commit that differs from the lock is an error.
- A module imports the modules of its own package (`import pkg.module`), of the packages it depends on, and `std`.
  The project imports only the packages in its own `[dependencies]`, not those installed for them.
- Only `https://`, `ssh://`, `git@host:...`, and `file:///` URLs are allowed, made of URL characters only. Registry
  entries and the dependencies of packages must use `https://` (a `file:///` registry may name `file:///` packages,
  for tests).
- Package names are lowercase letters, digits and `_`. A package cannot have the name of a directory or module of the
  project. `phv install` refuses a `vendor/`, `vendor/<name>` or `phv.lock` that is a link, never follows links in
  what it clones, and reads a package's phv.toml only if it is a regular file of at most 1 MB.
- Packages are PHV source code. No package code runs at install time.

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
     Objects that reach each other in a cycle (`$node->next = $node`, or an object holding a closure that captured
     it) are freed while the script runs: a cycle collector runs between instructions each time the number of live
     objects has doubled since its last run (first at 4,096), and it looks at no more than 16 values per object made
     since then. It also runs when the memory in use passes three quarters of the limit, and once more before a script
     is stopped at the limit, if objects were made since its last run (still within the 16 values per object made).
     The collector's own memory (about 120 bytes for each object it looks at) counts against the memory limit: when
     a walk of every object would not fit in what is left, it walks the newest objects that fit (those made since its
     last run, where new cycles are; older ones are kept until a walk takes them all), and a walk that outgrows that
     room stops and frees nothing. Before a script is stopped at the limit, it may use an eighth of the limit more.
     Native code cannot run it: when native code runs out of memory and the collector frees cycles made before the
     call, the interpreter runs the call again. Objects that are still reachable from a variable, a list, a map, a
     closure or another live object are never freed. When a run ends, the fields of every object left are cleared. Objects of a class whose properties all
     hold scalars, strings or enum cases cannot be part of a cycle: the collector does not look at them.
   - **JIT** (Cranelift): functions whose locals are all `int`, `bool`, `float` or `string`, nullable ones (`?int`,
     ...), or lists and maps of them (`list<int>`, `map<string, int>`, ...) are compiled to native code when every
     instruction they use is supported: arithmetic including `**`, comparisons (also with `null`), conversions,
     string constants, `.`, `.=`, `(string)`, `strlen()` and `->len()`; `null`, `??` and null checks; for
     lists and maps `[...]`, `$xs[$k]`, `$xs[$k] ?? $default`, `$xs[$k] = $v`, `$xs[] = $v`, `foreach`, `range()`,
     `count()`, `->len()` and `->has()`; and calls to other compiled functions. Numbers stay in machine registers
     (with a null flag for nullable ones); strings, lists and maps live in a register window and are handled by the
     interpreter's own code, so results, error messages (such as `IndexError` and `KeyError`) and memory checks are
     identical. `PHV_JIT_EXPLAIN=1` prints, for each function the JIT leaves to the interpreter, the instruction
     that kept it out.
     Integer overflow, division and modulo by zero, negative shifts, out-of-range float to int conversion, recursion
     depth, and the execution time and memory limits are checked in the JIT just as in the interpreter. Native code
     uses at most 8 MB of stack, and never comes within 1 MB of the end of the thread's stack (on Windows, Linux and
     macOS, which tell where it ends): a call that needs more (deep recursion of a function with a large frame) is run by
     the interpreter instead, whose frames are not on the stack. Compiled functions have no side effects, so the
     result is the same; a function that changes a string, list or map parameter in place (`$xs[] = ...`) stops
     with "maximum call stack depth exceeded" instead (its arguments are not kept for a rerun, so changing them
     never copies them). Functions with more than 1,024 registers or 16,384 instructions are left to the interpreter
     (compiling them would take much memory). Before compiling, the JIT checks every instruction and the kind of every register on every path, so bytecode from the
     cache is never trusted. It can be disabled with `PHV_JIT=0`. The stack trace of an exception raised inside the JIT
     goes back only as far as the caller.
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
| SQLite databases a script has open at once | 64 | `DatabaseError` |
| Call depth | 10,000 (callbacks of builtins inside each other: 200, each only while 512 KB of the thread's stack is left) | Fatal error |
| Request body | 8 MB | 413 |
| Scripts running at once (`serve`) | two per CPU, at least 8 (`[server] workers`) | waits up to 10 seconds, then 503 |
| Jobs running at once (`serve`, `phv jobs run`) | 2 per process (`[jobs] workers`), each with `[jobs] time_limit` (300 s) and `memory_limit_mb`, besides the scripts of pages | the job waits in the store |
| Jobs waiting and running in the store | 100,000 or 1 GB of payloads (memory: 10,000 or 64 MB); a payload 64 KB | `enqueue()` throws |
| Keys of ended jobs a store remembers | 100,000 (the latest), each a day | the oldest are forgotten |
| Output waiting to be sent (`serve`) | `workers` × `memory_limit_mb`, a quarter of it for event streams | 503; an event stream is ended or skips the event |
| Request body bytes past the first 64 KB, received or held (`serve`) | `workers` × 8 MB | waits up to 10 seconds, then 503 |
| Source file (module, template, script) | 8 MB | Compile error (`phv lsp`: a larger open document is not checked or formatted, and renames are refused while it is open) |
| Tokens of one source file (the code inside its strings included) | 1,000,000 | Compile error (the file is not parsed) |
| All the files of one program (a page or script with what it imports and renders, std included) | 2,000,000 tokens, 32 MB of source, 2,000 files | Compile error (the program is not checked; `phv lsp` keeps no analysis of a program of more than 250,000 tokens between requests) |
| Members inherited by all classes together | 200,000 | Compile error (each class holds a copy of what it inherits) |
| Levels of classes and interfaces above a class (`extends`, `implements`) | 256 | Compile error |
| Ancestors, interfaces and the type arguments given to them, all classes together | 2,000,000 (a type argument given to an ancestor: 10,000 type nodes) | Compile error (each class lists all of its ancestors) |
| Variables and temporary values of one function | 65,534 | Compile error (split the function) |
| Instructions of one function | 8,388,608 | Compile error (split the function) |
| Properties of one class (inherited ones included) | 65,535 | Compile error |
| A string built by a constant expression | 1 MiB (16 MiB for all of a program's) | Compile error |
| Type aliases resolved inside each other | 64 | Compile error |
| Type parameters of a function, class or type alias | 4 | Compile error |
| Members of a union type written in the source | 256 | Compile error |
| Size of the type an alias stands for | 10,000 type nodes (2,000,000 for all uses together) | Compile error |

`unsafe` is used only in the allocator that counts memory usage (`phv_vm/src/alloc.rs`), in the JIT
(`phv_jit`, for calling generated code, and the helpers it calls in `phv_vm/src/jit.rs`), in asking the OS where a thread's
stack ends (`phv_vm/src/stack.rs`), in calls to OS file APIs (`phv_std/src/safefs.rs`), in the SQLite VFS
that checks journal files (`phv_std/src/db/vfs.rs`) and in the allocator SQLite uses, which counts its memory
(`phv_std/src/db/mem.rs`).

## 18. Compatibility and versions

From 1.0.0, PHV follows [semantic versioning](https://semver.org): versions are MAJOR.MINOR.PATCH.

- A **patch** release (1.0.1) fixes bugs and security problems.
- A **minor** release (1.1.0) adds features. A program that worked with an earlier 1.x release keeps working, with
  the same results.
- Only a **major** release (2.0.0) removes or changes what is listed below, and only after it was deprecated in a
  minor release (see "Deprecation").

**Kept within a major version:**

- The language: its syntax, its type rules and what programs do. A program that compiles keeps compiling and gives
  the same results.
- The standard library: builtin functions, the methods of strings, lists, maps and sets, and the `std.*` modules,
  with their names, parameters, results and exceptions. New global builtin functions come only in major releases,
  because one could clash with a function of the same name in a project; minor releases add methods of the
  built-in types, functions and classes in `std.*` modules, and new modules, which a project uses only once it
  imports them.
- `phv.toml`: its sections and keys and what they mean.
- The tools: the commands of `phv`, their options and their exit codes, and the layout of a project and a site
  (`public/`, `_middleware.phv`, `_404.phv`, `_500.phv`, `*.ws.phv`, `[param]` routes, templates, `tests/`).
- What pages can rely on in the web server: `request()`, `response()`, `session()`, CSRF tokens, escaping, WebSocket
  events.

**May change in any release:**

- The text of error messages and warnings, their positions, and new warnings (a warning never stops a program).
- Speed, memory use and what the JIT compiles (results stay the same).
- The bytecode cache (`.phv-cache/`) and the executables made by `phv build`: a new version rebuilds them.
- How `phv fmt` formats code, the output of `phv doc`, `dump()` and `phv run --profile`, and the features of
  `phv lsp` and `phv dap`.
- Builtins whose names start with `__`, and the Rust crates of the implementation.

**Exceptions:** a minor or patch release may refuse code or input that was accepted before, when accepting it is a
vulnerability or the type checker missed an error (a program that could fail in a way its types say it cannot) — for
example a template check that finds a new way to inject script, a tighter default limit, or a header the server
adds. The release notes say what changed and how to update the code.

### Deprecation

What is to be removed in the next major version is deprecated at least one minor release before: functions,
methods, classes and properties of the standard library get `#[Deprecated("...")]`, which gives a warning where
they are used (§9), and everything else is marked in this specification and the release notes. It keeps working
until the next major version.

### The PHV version of a project

```toml
[project]
phv = "^1.2"   # PHV 1.2.0 or any later 1.x
```

`[project] phv` gives the versions of PHV a project works with, as a semver requirement. Any other version refuses
to run, check, test or serve the project, and says which version it needs. `phv new` writes the requirement for the
version it is (`^1.2` for PHV 1.2.x).

### Supported versions

Security fixes are released for the latest minor version of the current major version ([SECURITY.md](../SECURITY.md)).

## 19. Roadmap

Planned, in this order. Each release is audited before it is published, as before.

- **0.6, the language** (done): destructuring, variadic parameters and `...` spread, functions as values
  (`strlen(...)`), type aliases, `match` that must cover every case of an enum, bounds on type parameters,
  constant expressions, `import ... as`, heredoc strings, and exceptions that carry their cause and trace.
- **0.7, web applications** (done): routes with parameters (`public/posts/[id].phv`), middleware, custom error pages, form
  validation, layouts, CORS, compression and caching headers for static files, graceful shutdown, access
  logs, database migrations, an HTTP client for `phv test`, and sending mail.
- **0.8, the standard library** (done): regular expressions, formatted strings, more list and map methods,
  mathematics, dates with time zones, JSON decoded into classes, encryption and signed tokens, UUIDs, directories,
  logging, URLs, CSV, and arguments and standard input for scripts.
- **0.9, tools**: rename, find references, formatting, signature help, symbols, quick fixes, semantic colouring
  and inlay types in `phv lsp`, `phv new`, `phv serve --watch`, and `phv test` with hooks, filters, JSON and JUnit
  reports and coverage, `phv doc` and a reference of the standard library, a debugger (`phv dap`), `phv run
  --profile`, a tutorial, and builds for Linux on ARM, static Linux builds and a Docker image
  (`ghcr.io/miyagawayuu/phv`), Homebrew and Scoop from this release on (done).
- **1.0, speed and stability** (done): freeing reference cycles while a script runs, a JIT that covers lists, maps
  and nullable values, HTTPS and HTTP/2, a WebSocket server, a `Set` type, and the compatibility rules of §18.
- **1.1, web services** (done): a secret shared by a site's server processes (`[security] secret_env`), `publish()`
  across server processes (`[server] publish_store`), a namespace for each site in shared stores
  (`[security] store_namespace`), health checks and Prometheus metrics on an admin port (`[admin]`), request ids and
  trace context, background jobs and schedules (`std.jobs`, `[jobs]`, `[[schedule]]`, `phv jobs`), Server-Sent
  Events, and faster number formatting, exceptions and object creation.
- **1.2**: sign-in with OAuth 2.0 and OpenID Connect, JSON Web Tokens signed with RS256, ES256 and EdDSA (with JWKS),
  streaming responses, and exporting traces (OpenTelemetry).
- **Later**, in minor releases: the JIT for closures and objects, and more of the standard library. Generators
  (`yield`) and traits are being considered; they would come in a minor release too, since they only add to the
  language.

Some features are left out on purpose, because they work against the design principles (§1): `eval`, variable
variables, static properties and global variables, references (`&$x`), truthiness, `===`, `@`, `goto`, magic
methods such as `__get`, `extract` / `compact`, and `new $className`.

See [security-audit.md](security-audit.md) for the results of the security audit and the remaining risks.
