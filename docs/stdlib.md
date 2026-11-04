# PHV standard library

- [std.http](#stdhttp)
- [std.db](#stddb)
- [std.net](#stdnet)
- [std.async](#stdasync)
- [std.validate](#stdvalidate)
- [std.mail](#stdmail)
- [std.testing](#stdtesting)
- [std.regex](#stdregex)
- [std.time](#stdtime)

## std.http

std.http — the current request, the response and the session.

```phv
import std.http.{request, response, session};
$name = request()->query("name") ?? "world";
```

### class UploadedFile

```phv
class UploadedFile
```

- `public readonly string $field`
- `public readonly string $name` — The client's file name (directory parts removed). Do not trust it as a path.
- `public readonly string $type`
- `public readonly int $size`
- `public function text(): string` — The contents as text (invalid UTF-8 is replaced).
- `public function saveTo(string $path): void` — Saves the file; the path must be inside a [permissions].write directory.

### class Request

```phv
class Request
```

- `public readonly string $method`
- `public readonly string $path`
- `public readonly map<string, string> $queryParams`
- `public readonly map<string, string> $formParams`
- `public readonly map<string, string> $headers`
- `public readonly map<string, string> $cookies`
- `public readonly list<UploadedFile> $files`
- `public readonly string $body`
- `public readonly string $ip` — The client's IP address (the proxy's, behind a reverse proxy).
- `public readonly map<string, string> $params = []` — The parameters of the page's path: `id` for `public/posts/[id].phv`.
- `public function __construct(public readonly string $method, public readonly string $path, public readonly map<string, string> $queryParams, public readonly map<string, string> $formParams, public readonly map<string, string> $headers, public readonly map<string, string> $cookies, public readonly list<UploadedFile> $files, public readonly string $body, public readonly string $ip, public readonly map<string, string> $params = [])`
- `public function param(string $name): ?string` — A parameter of the page's path (`[id]` in its file name), or null.
- `public function query(string $name): ?string` — A query string parameter (`?name=value`), or null.
- `public function form(string $name): ?string` — A form field (urlencoded or multipart POST body), or null.
- `public function queryAll(string $name): list<string>` — Every value of a query parameter that appears more than once (`?tag=a&tag=b`), in order. query() returns only one of them.
- `public function formAll(string $name): list<string>` — Every value of a form field sent more than once, such as checkboxes that share a name (`<input type="checkbox" name="ids" value="3">`), in order. form() returns only one of them.
- `public function header(string $name): ?string` — A request header (case-insensitive), or null.
- `public function cookie(string $name): ?string`
- `public function file(string $field): ?UploadedFile` — An uploaded file from a multipart form, or null.
- `public function json(): any` — The body parsed as JSON. Throws ValueError for invalid JSON.
- `public function isPost(): bool`

### class Response

```phv
class Response
```

- `public function status(int $code): Response`
- `public function header(string $name, string $value): Response` — Sets a response header. Line breaks in values are rejected.
- `public function type(string $contentType): Response` — Sets the Content-Type. Output is HTML-escaped only for `text/html`.
- `public function cookie(string $name, string $value, int $maxAge = 0, string $path = "/", bool $secure = false, bool $httpOnly = true, string $sameSite = "Lax"): Response` — Sets a cookie. Cookies are HttpOnly and SameSite=Lax unless told otherwise; `$maxAge` 0 means "until the browser closes".
- `public function deleteCookie(string $name, string $path = "/"): Response`
- `public function redirect(string $path, int $code = 302): void` — Redirects to a path on this site ("/login", "/posts?id=2"). Other URLs are rejected (open-redirect protection); use redirectExternal() for those.
- `public function redirectExternal(string $url, int $code = 302): void` — Redirects to an absolute http(s) URL on another site.
- `public function file(string $path, string $type = ""): void` — Sends a file instead of the page output, for example an uploaded image kept in `data/`. The path needs [permissions].read or write. Without `$type` the Content-Type comes from the extension; HTML, SVG and unknown files are sent as `application/octet-stream`.
- `public function json(any $value): void` — Sends `$value` as JSON.

### class Session

```phv
class Session
```

The visitor's session, stored on the server. Values are strings; use
json_encode / json_decode for structured data. Call regenerate() after login.

- `public function get(string $key): ?string`
- `public function set(string $key, string $value): void`
- `public function has(string $key): bool`
- `public function remove(string $key): void`
- `public function all(): map<string, string>`
- `public function regenerate(): void` — Issues a new session id, keeping the values.
- `public function destroy(): void` — Deletes the session and its values.
- `public function flash(string $key): ?string` — Reads and removes a one-time message (e.g. "Saved!").

### function request

```phv
function request(): Request
```

### class Locals

```phv
class Locals
```

Values that `_middleware.phv` passes on to the page of the same request,
such as the signed-in user's id. They last for one request.

- `public function get(string $key): ?string`
- `public function set(string $key, string $value): void`
- `public function has(string $key): bool`
- `public function all(): map<string, string>`

### function locals

```phv
function locals(): Locals
```

### function response

```phv
function response(): Response
```

### function session

```phv
function session(): Session
```


## std.db

std.db — SQLite databases, database servers named in phv.toml, and Redis.

```phv
import std.db.Database;
$db = Database::open("data/app.db");      // needs [permissions].write = ["data/"]
$db = Database::connect("main");          // PostgreSQL / MySQL from [databases.main]
$db->execute(sql"INSERT INTO posts (title) VALUES ({$title})");
foreach ($db->query(sql"SELECT id, title FROM posts WHERE id > {$min}") as $row) {
echo $row->int("id"), " ", $row->string("title");
}
```


Values written inside `sql"..."` are always sent as bound parameters.

### class Row

```phv
class Row
```

- `public function get(string $column): any`
- `public function has(string $column): bool`
- `public function string(string $column): ?string`
- `public function int(string $column): ?int`
- `public function float(string $column): ?float`
- `public function bool(string $column): ?bool`
- `public function toMap(): map<string, any>`

### class Database

```phv
class Database
```

- `public static function open(string $path): Database` — Opens (or creates) a database file, or ":memory:".
- `public static function connect(string $name): Database` — Connects to the PostgreSQL or MySQL server named `[databases.<name>]` in phv.toml.
- `public function query(Sql $query): list<Row>` — Runs a SELECT and returns all rows.
- `public function first(Sql $query): ?Row` — The first row of a SELECT, or null.
- `public function execute(Sql $query): int` — Runs INSERT / UPDATE / DELETE / DDL and returns the number of changed rows.
- `public function queryAsync(Sql $query): Future<list<Row>>` — `query()` in the background, on a connection of its own (so outside a transaction of this connection). Wait for the rows with `await()`. With SQLite the statement runs right away.
- `public function executeAsync(Sql $query): Future<int>` — `execute()` in the background, like `queryAsync()`.
- `public function lastInsertId(): int` — The id of the last inserted row (SQLite, MySQL; with PostgreSQL use `RETURNING id`).
- `public function transaction(fn(): void $work): void` — Runs `$work` in a transaction; rolls back if it throws.
- `public function close(): void`

### class Redis

```phv
class Redis
```

A Redis server named `[databases.<name>]` in phv.toml.

  $redis = Redis::connect("cache");
  $redis->set("greeting", "hello", 60);   // expires after 60 seconds
  echo $redis->get("greeting") ?? "(gone)";

- `public static function connect(string $name): Redis`
- `public function get(string $key): ?string` — The value of `$key`, or null when it does not exist.
- `public function getAsync(string $key): Future<?string>` — `get()` in the background: wait for the value with `await()` (std.async).
- `public function setAsync(string $key, string $value, int $ttl = 0): Future<bool>` — `set()` in the background.
- `public function set(string $key, string $value, int $ttl = 0): void` — Sets `$key`; with `$ttl` > 0 it expires after that many seconds.
- `public function delete(string $key): bool` — Deletes `$key`; true when it existed.
- `public function exists(string $key): bool`
- `public function incr(string $key, int $by = 1): int` — Adds `$by` to the integer stored at `$key` (0 when missing) and returns the new value.
- `public function expire(string $key, int $seconds): bool` — Makes `$key` expire after `$seconds`; false when it does not exist.
- `public function ttl(string $key): int` — Seconds until `$key` expires: -1 without an expiry, -2 when it does not exist.
- `public function hget(string $key, string $field): ?string`
- `public function hset(string $key, string $field, string $value): bool` — Sets a field of the hash at `$key`; true when the field is new.
- `public function hdel(string $key, string $field): bool` — Deletes a field of the hash at `$key`; true when it existed.
- `public function hgetAll(string $key): map<string, string>` — All fields of the hash at `$key`.
- `public function rpush(string $key, string $value): int` — Appends to the list at `$key` and returns its new length.
- `public function lpop(string $key): ?string` — Removes and returns the first element of the list at `$key`, or null when it is empty.
- `public function lrange(string $key, int $start, int $stop): list<string>` — Elements `$start` to `$stop` (inclusive; negative counts from the end) of the list at `$key`.
- `public function close(): void`


## std.net

std.net — WebSocket servers and HTTP services named in phv.toml.

```phv
import std.net.{HttpClient, WebSocket};
```


```phv
$api = HttpClient::connect("weather");  // [http.weather] in phv.toml
$res = $api->get("/forecast", ["city" => "Tokyo"]);
if ($res->ok()) {
$data = $res->json();
}
$res = $api->postJson("/reports", ["city" => "Tokyo", "temp" => 21]);
```


```phv
$ws = WebSocket::connect("game");       // [websockets.game] in phv.toml
$ws->send("hello");
$reply = $ws->receive(5.0);             // ?string: null when nothing arrives within 5 seconds
$ws->close();
```


Each WebSocket::connect() opens a new connection, closed by close() or when the script ends.
Each HTTP request uses its own connection; redirects are not followed.
Network failures and a server closing the connection raise NetworkError.

### class HttpResponse

```phv
class HttpResponse
```

- `public readonly int $status`
- `public readonly map<string, string> $headers`
- `public readonly string $body`
- `public function __construct(public readonly int $status, public readonly map<string, string> $headers, public readonly string $body)`
- `public function ok(): bool` — A 2xx status.
- `public function header(string $name): ?string` — A header's value (names are matched without regard to case), or null.
- `public function json(): any` — The body decoded as JSON. Throws ValueError when it is not valid JSON.

### class HttpClient

```phv
class HttpClient
```

An HTTP service named `[http.<name>]` in phv.toml. Paths are added to its base URL.

- `public static function connect(string $name): HttpClient`
- `public function get(string $path, map<string, string> $query = [], map<string, string> $headers = []): HttpResponse`
- `public function delete(string $path, map<string, string> $query = [], map<string, string> $headers = []): HttpResponse`
- `public function post(string $path, string $body = "", map<string, string> $headers = []): HttpResponse`
- `public function put(string $path, string $body = "", map<string, string> $headers = []): HttpResponse`
- `public function patch(string $path, string $body = "", map<string, string> $headers = []): HttpResponse`
- `public function postJson(string $path, any $data, map<string, string> $headers = []): HttpResponse` — Sends `$data` as JSON (`Content-Type: application/json` unless `$headers` sets it).
- `public function putJson(string $path, any $data, map<string, string> $headers = []): HttpResponse`
- `public function postForm(string $path, map<string, string> $fields, map<string, string> $headers = []): HttpResponse` — Sends `$fields` as a form (`application/x-www-form-urlencoded`).
- `public function request(string $method, string $path, string $body = "", map<string, string> $headers = [], map<string, string> $query = []): HttpResponse` — Any method (GET, HEAD, POST, PUT, PATCH, DELETE or OPTIONS).
- `public function getAsync(string $path, map<string, string> $query = [], map<string, string> $headers = []): Future<HttpResponse>` — `get()` sent in the background: wait for the response with `await()` (std.async).
- `public function postAsync(string $path, string $body = "", map<string, string> $headers = []): Future<HttpResponse>`
- `public function postJsonAsync(string $path, any $data, map<string, string> $headers = []): Future<HttpResponse>`
- `public function requestAsync(string $method, string $path, string $body = "", map<string, string> $headers = [], map<string, string> $query = []): Future<HttpResponse>` — `request()` sent in the background.

### class WebSocket

```phv
class WebSocket
```

- `public static function connect(string $name): WebSocket` — Connects to the server named `[websockets.<name>]` in phv.toml.
- `public function send(string $text): void` — Sends one text message.
- `public function receive(float $timeout = 5.0): ?string` — The next text message, or null when none arrives within `$timeout` seconds.
- `public function close(): void`


## std.async

std.async — waiting for work started by the *Async methods of std.net and std.db.

```phv
import std.async.{await, await_all};
import std.net.HttpClient;
```


```phv
$api = HttpClient::connect("weather");
$today = $api->getAsync("/today");        // sent now
$tomorrow = $api->getAsync("/tomorrow");  // sent at the same time
echo await($today)->status, " ", await($tomorrow)->status;
```


The work runs while the script goes on, and must finish within the script's
time limit. A script can have 16 operations running at once.

### class Future

```phv
class Future<T>
```

A result that may not be there yet.

- `public function await(): T` — The result: waits for it the first time.

### function await

```phv
function await<T>(Future<T> $f): T
```

Waits for `$f` and returns its result (or throws its error).

### function await_all

```phv
function await_all<T>(list<Future<T>> $fs): list<T>
```

Waits for each of `$fs` (they run at the same time) and returns their results in order.


## std.validate

std.validate — checks form fields and collects messages for each field.

```phv
import std.validate.Validator;
$v = new Validator(request()->formParams);
$v->required("name")->maxLength("name", 50);
$v->required("email")->email("email");
$v->int("age", min: 0, max: 150);
if ($v->fails()) {
$errors = $v->errors();   // map<string, list<string>>
}
```


A rule other than `required` passes when the field is missing or empty, so
optional fields only need the rules for their format. Values are trimmed.

### class Validator

```phv
class Validator
```

- `public function value(string $field): ?string` — The field's value with spaces trimmed, or null when it is missing or empty.
- `public function add(string $field, string $message): Validator` — Adds a message for a field (for checks of your own).
- `public function check(string $field, bool $ok, string $message): Validator` — Adds `$message` for the field unless `$ok` (a check of your own).
- `public function required(string $field, string $message = ""): Validator`
- `public function minLength(string $field, int $min, string $message = ""): Validator` — At least `$min` characters.
- `public function maxLength(string $field, int $max, string $message = ""): Validator` — At most `$max` characters.
- `public function email(string $field, string $message = ""): Validator` — An email address: one `@`, a name before it and a domain with a dot after it, no spaces.
- `public function url(string $field, string $message = ""): Validator` — An `http://` or `https://` URL.
- `public function int(string $field, ?int $min = null, ?int $max = null, string $message = ""): Validator` — A whole number, optionally between `$min` and `$max`.
- `public function number(string $field, ?float $min = null, ?float $max = null, string $message = ""): Validator` — A number (whole or decimal), optionally between `$min` and `$max`.
- `public function in(string $field, list<string> $options, string $message = ""): Validator` — One of `$options`.
- `public function same(string $field, string $other, string $message = ""): Validator` — The same as another field (a repeated password).
- `public function passes(): bool`
- `public function fails(): bool`
- `public function errors(): map<string, list<string>>` — The messages of each field that failed, in the order the rules ran.
- `public function first(string $field): ?string` — The first message of a field, or null.

### function is_email

```phv
function is_email(string $s): bool
```

Whether `$s` looks like an email address (a practical check, not RFC 5322).


## std.mail

std.mail — sends mail through a server named in phv.toml.

```phv
[mail.default]
url_env = "SMTP_URL"                 # smtps://user:pass@smtp.example.com
from = "Example <noreply@example.com>"
```


```phv
import std.mail.Mail;
Mail::to($email)
->subject("Welcome")
->text("Thanks for signing up.")
->html(render("views/mail/welcome.phv.html", name: $name))
->send();
```


Sending waits for the server (up to the script's time limit, at most 30
seconds) and throws NetworkError if it fails.

### class Mail

```phv
class Mail
```

- `public static function to(string $address): Mail` — A message to one address; add more with `also()`.
- `public function also(string $address): Mail` — Another recipient (each sees every address in `To`).
- `public function subject(string $subject): Mail`
- `public function text(string $body): Mail` — The plain text version (always sent, for clients that do not show HTML).
- `public function html(Html $body): Mail` — An HTML version, usually from `render()` of a template.
- `public function replyTo(string $address): Mail`
- `public function send(string $via = "default"): void` — Sends through `[mail.<via>]` of phv.toml (`default` when not given).


## std.testing

std.testing — requests to the project's own site from `phv test`, without a
network or a running server. Pages run as they do under `phv serve`
(routes, middleware, sessions, error pages), except that forms need no CSRF
token.

```phv
import std.testing.TestClient;
```


```phv
function test_home(): void {
$client = new TestClient();
$r = $client->get("/posts/1");
assert_eq($r->status, 200);
assert($r->body->contains("Hello"));
}
```

### class TestResponse

```phv
class TestResponse
```

- `public readonly int $status`
- `public readonly string $body`
- `public readonly list<list<string>> $headerList` — Every header as [name, value], in order (names in lowercase).
- `public function __construct(public readonly int $status, public readonly string $body, public readonly list<list<string>> $headerList)`
- `public function header(string $name): ?string` — The first value of a header (case-insensitive), or null.
- `public function headers(string $name): list<string>` — Every value of a header, such as each `set-cookie`.
- `public function json(): any` — The body parsed as JSON.
- `public function location(): ?string` — Where a redirect points (`Location`), or null.

### class TestClient

```phv
class TestClient
```

Sends requests to the site and keeps the cookies it sets, like a browser,
so a test can sign in and then open pages that need it.

- `public function get(string $path, map<string, string> $headers = []): TestResponse`
- `public function post(string $path, map<string, string> $form = [], map<string, string> $headers = []): TestResponse` — Submits a form (`application/x-www-form-urlencoded`).
- `public function postJson(string $path, any $data, map<string, string> $headers = []): TestResponse` — Sends a value as a JSON body.
- `public function request(string $method, string $path, string $body = "", map<string, string> $headers = []): TestResponse`
- `public function cookie(string $name): ?string` — The value of a cookie the site has set, or null.


## std.regex

std.regex — regular expressions. There is no catastrophic backtracking: a
search takes at most time proportional to the text times the size of the
pattern, and one that could not end within the time limit is refused before
it starts (RuntimeException), so patterns and texts from visitors cannot
hold a page past its limits.

```phv
import std.regex.Regex;
$date = new Regex('^(\d{4})-(\d{2})-(\d{2})$');   // single quotes keep backslashes
$parts = $date->match("2026-11-05");              // ["2026-11-05", "2026", "11", "05"]
```


The syntax is Rust's `regex` crate: (?i) for no case, (?<name>...) for a
named group, \d \w \s, Unicode classes like \p{L}. No look-around or
back-references.

### class Regex

```phv
class Regex
```

- `public readonly string $pattern`
- `public function __construct(public readonly string $pattern)` — Compiles the pattern; throws ValueError if it is not valid.
- `public function test(string $text): bool` — Whether the pattern matches somewhere in `$text`.
- `public function find(string $text): ?string` — The first match, or null.
- `public function match(string $text): ?list<string>` — The groups of the first match (0 is the whole match; a group that did not take part is ""), or null.
- `public function matchAll(string $text): list<list<string>>` — The groups of every match, in order.
- `public function replace(string $text, string $with): string` — Replaces every match; `$1` and `${name}` insert groups, `$$` a dollar sign.
- `public function replaceWith(string $text, fn(list<string>): string $with): string` — Replaces every match with what `$with` returns for its groups.
- `public function split(string $text): list<string>` — The text between the matches.
- `public static function escape(string $text): string` — `$text` with the characters that mean something in a pattern escaped.


## std.time

std.time — instants in time zones, and durations.

```phv
import std.time.{DateTime, Duration};
$meeting = DateTime::of(2026, 11, 5, 10, 0, 0, "Asia/Tokyo");
echo $meeting->inZone("Europe/Berlin")->format("Y-m-d H:i T");
$later = $meeting->plus(Duration::hours(36))->addMonths(1);
echo $later->diff($meeting)->totalHours();
```


Zones are IANA names ("Asia/Tokyo", "America/New_York", "UTC"). A local time
that happens twice when clocks go back is the earlier one; one that does not
happen when clocks go forward is moved past the gap.

### class Duration

```phv
class Duration
```

- `public readonly int $seconds`
- `public static function seconds(int $n): Duration`
- `public static function minutes(int $n): Duration`
- `public static function hours(int $n): Duration`
- `public static function days(int $n): Duration`
- `public static function weeks(int $n): Duration`
- `public function plus(Duration $other): Duration`
- `public function minus(Duration $other): Duration`
- `public function totalMinutes(): int`
- `public function totalHours(): int`
- `public function totalDays(): int`
- `public function toText(): string` — Like `1d 2h 3m 4s` (the parts that are not zero).

### class DateTime

```phv
class DateTime
```

- `public readonly int $timestamp`
- `public readonly string $zone`
- `public static function now(string $zone = "UTC"): DateTime`
- `public static function fromTimestamp(int $timestamp, string $zone = "UTC"): DateTime` — The instant `$timestamp` (seconds since 1970-01-01 UTC), seen in `$zone`.
- `public static function of(int $year, int $month, int $day, int $hour = 0, int $minute = 0, int $second = 0, string $zone = "UTC"): DateTime` — A local date and time in `$zone`; throws ValueError if there is no such date.
- `public static function parse(string $text, string $zone = "UTC"): ?DateTime` — `2026-11-05`, `2026-11-05 10:00[:30]` (local time in `$zone`), or RFC 3339 with an offset (`2026-11-05T10:00:00+09:00`, `...Z`); null if it is none of these.
- `public function inZone(string $zone): DateTime` — The same instant in another zone.
- `public function year(): int`
- `public function month(): int`
- `public function day(): int`
- `public function hour(): int`
- `public function minute(): int`
- `public function second(): int`
- `public function weekday(): int` — 1 (Monday) to 7 (Sunday).
- `public function offset(): int` — The zone's offset from UTC at this instant, in seconds.
- `public function format(string $format): string` — With the letters of `date()`: `Y-m-d H:i:s`, `D, d M Y`, `T`...
- `public function toIso(): string` — RFC 3339 with the offset: `2026-11-05T10:00:00+09:00`.
- `public function plus(Duration $d): DateTime`
- `public function minus(Duration $d): DateTime`
- `public function addDays(int $n): DateTime` — `$n` calendar days later, at the same local time (also across a change of clocks).
- `public function addMonths(int $n): DateTime` — `$n` months later; the day is kept, or is the month's last day if it has fewer.
- `public function addYears(int $n): DateTime`
- `public function startOfDay(): DateTime` — Midnight at the start of this local day.
- `public function diff(DateTime $other): Duration` — The time from `$other` to this one (negative if this one is earlier).
- `public function isBefore(DateTime $other): bool`
- `public function isAfter(DateTime $other): bool`
- `public function equals(DateTime $other): bool` — The same instant (in any zone).

