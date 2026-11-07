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
- [std.ws](#stdws)
- [std.jobs](#stdjobs)
- [std.jwt](#stdjwt)
- [std.oauth](#stdoauth)

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
- `public function flush(): bool` — Sends the status, the headers and what the page has written so far, and keeps the response open: what it writes later is sent at the next flush() or when the page ends. Afterwards the status, headers, cookies and session can no longer change. Returns false once the visitor has gone away (or has taken nothing for 10 seconds), or when the visitor's address has `[server] streams_per_address` pages streaming already (the request is answered with 429): stop the work then. Outside `phv serve` (scripts, `phv test`) it keeps the output and returns true.

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

### function request_id

```phv
function request_id(): ?string
```

The id of this request, with `[server] request_ids = true` in phv.toml
(otherwise null). The server sends it in the `X-Request-Id` header, writes
it in the JSON access log and adds it to the lines of `log_info()`,
`log_warn()` and `log_error()`, so the lines of one request can be found.


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
- `public function pipeline(): RedisPipeline` — Commands to send together, in one round trip (see `RedisPipeline`).
- `public function close(): void`

### class RedisPipeline

```phv
class RedisPipeline
```

Commands queued to be sent to Redis together, from `Redis::pipeline()`:
one round trip instead of one per command.

  $p = $redis->pipeline();
  $p->set("greeting", "hello", 60)->incr("visits")->get("name");
  [$set, $visits, $name] = $p->exec();   // [true, 3, "Ann"]

The methods take what the methods of `Redis` of the same name take and
queue the command; `exec()` sends them and returns the replies in order, as
`any`: `get`, `hget` and `lpop` give `?string`, `incr`, `rpush` and `ttl`
`int`, `set` `true`, `delete`, `exists`, `expire`, `hset` and `hdel` the
server's count (`int`), `hgetAll` `map<string, string>` and `lrange`
`list<string>`. A command the server refused is a `DatabaseError` value in
its place; the others are still answered. A pipeline is not a transaction:
other clients' commands may run between its commands.

- `public function get(string $key): RedisPipeline`
- `public function set(string $key, string $value, int $ttl = 0): RedisPipeline`
- `public function delete(string $key): RedisPipeline`
- `public function exists(string $key): RedisPipeline`
- `public function incr(string $key, int $by = 1): RedisPipeline`
- `public function expire(string $key, int $seconds): RedisPipeline`
- `public function ttl(string $key): RedisPipeline`
- `public function hget(string $key, string $field): RedisPipeline`
- `public function hset(string $key, string $field, string $value): RedisPipeline`
- `public function hdel(string $key, string $field): RedisPipeline`
- `public function hgetAll(string $key): RedisPipeline`
- `public function rpush(string $key, string $value): RedisPipeline`
- `public function lpop(string $key): RedisPipeline`
- `public function lrange(string $key, int $start, int $stop): RedisPipeline`
- `public function exec(): list<any>` — Sends the queued commands and returns their replies in order; the pipeline is empty again afterwards. At most 1,024 commands and 16 MB are sent at once (`ValueError`, nothing sent). The whole exchange waits until the script's time limit at most, and the replies together stay within the memory left to the script.


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


The syntax is Rust's `regex` crate: (?i) for no case, (?&lt;name>...) for a
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


## std.ws

std.ws — WebSocket connections served by `phv serve`.

A file `public/chat.ws.phv` answers WebSocket connections to `/chat`. It runs
once for each event: when a connection opens, for each text message, and when
it closes. Nothing is kept between runs except the connection's values
(`get()` / `set()`) and the topics it joined.

```phv
import std.ws.{socket, publish, WsEvent};
$ws = socket();
if ($ws->event == WsEvent::Open) {
$ws->join("lobby");
} elseif ($ws->event == WsEvent::Message) {
publish("lobby", $ws->message());
}
```

### enum WsEvent

```phv
enum WsEvent
```

- `case Open` — The connection is being opened. A status other than 200 refuses it.
- `case Message` — A text message arrived (`message()`).
- `case Close` — The connection was closed. Messages sent now are not delivered.

### class Socket

```phv
class Socket
```

- `public readonly WsEvent $event`
- `public readonly string $id` — The connection's id: random, the same for all its events.
- `public function __construct(public readonly WsEvent $event, public readonly string $id)`
- `public function message(): string` — The text message received (empty for Open and Close).
- `public function get(string $key): ?string` — A value kept for this connection, or null.
- `public function set(string $key, string $value): void` — Keeps a value for the next events of this connection (64 KB in all).
- `public function remove(string $key): void`
- `public function send(string $text): void` — Sends a text message to this connection.
- `public function join(string $topic): void` — Receives the messages published to `topic` from now on.
- `public function leave(string $topic): void`
- `public function close(int $code = 1000, string $reason = ""): void` — Closes the connection after the messages sent before.

### function socket

```phv
function socket(): Socket
```

The connection this script runs for. Only in `public/*.ws.phv`.

### function publish

```phv
function publish(string $topic, string $text): void
```

Sends `text` to every connection that joined `topic`, after the script
ends: those of this server process, and with `[server] publish_store` those
of the site's other server processes. Any page may publish.


## std.jobs

std.jobs — background jobs (`[jobs]` in phv.toml).

A job is a script in the jobs directory (`jobs/send_welcome.phv`). Any page
or job adds one with `enqueue()`; `phv serve` (and `phv jobs run`) take the
jobs and run them, outside any request, each with its own time and memory
limits. A job that throws is run again later, up to `[jobs] max_attempts`
times.

```phv
import std.jobs.enqueue;
enqueue("send_welcome", json_encode(["user" => $id]));
```


```phv
// jobs/send_welcome.phv
import std.jobs.job;
$data = json_decode(job()->payload);
```

### class Job

```phv
class Job
```

The job a script of the jobs directory runs for.

- `public readonly string $name` — The job's name: its file in the jobs directory, without `.phv`.
- `public readonly string $id` — A random id, the same for every run of the job.
- `public readonly string $payload` — What `enqueue()` was given.
- `public readonly int $attempt` — 1 for the first run, 2 for the first retry, ...
- `public function __construct(public readonly string $name, public readonly string $id, public readonly string $payload, public readonly int $attempt)`

### function enqueue

```phv
function enqueue(string $job, string $payload = "", int $delay = 0, ?string $key = null): ?string
```

Adds a job: the script `<name>.phv` of `[jobs] dir` runs with `payload`
(at most 64 KB; JSON is handy), `delay` seconds from now at the earliest
(at most 30 days). With a `key`, a job of the same name and key is added
only once (while it waits, runs or failed, and for a day after it ended):
the result is then null. Otherwise the result is the job's id.

Inside a `transaction()` on the database of `[jobs] store` (PostgreSQL or
MySQL), the job is added only if the transaction commits.

### function job

```phv
function job(): Job
```

The job this script runs for. Only in a job.


## std.jwt

std.jwt — JSON Web Tokens signed with RS256, ES256, EdDSA or HS256, and key sets (JWKS).

```phv
import std.jwt.{JwtKey, KeySet};
```


```phv
$key = JwtKey::fromPem(getenv("JWT_PRIVATE_KEY") ?? "", "2026-11");   // signs and verifies
$token = $key->sign(["sub" => "7", "exp" => time() + 3600]);
$claims = $key->verify($token);            // ?map<string, any>
echo $key->publicJwk();                    // the public key, for others to verify with
```


```phv
$keys = KeySet::fetch("auth", "/.well-known/jwks.json");   // [http.auth] in phv.toml
$claims = $keys->verify($token, issuer: "https://auth.example.com/", audience: "my-api");
```


A key's type decides its algorithm: RSA keys sign with RS256, P-256 keys with ES256,
Ed25519 keys with EdDSA, secrets with HS256. A token is checked only with keys of the
algorithm its header names, so a token cannot choose another kind of key, or `none`.

### abstract class Verifier

```phv
abstract class Verifier
```

Something that verifies tokens: a `JwtKey` or a `KeySet`.

- `public function verify(string $token, ?string $issuer = null, ?string $audience = null, int $leeway = 0): ?map<string, any>` — The claims of `$token` if one of the keys signed it and it is valid now, or null. `exp` and `nbf` are checked, with `$leeway` seconds (0 to 300) for clocks that differ. With `$issuer`, `iss` must be it; with `$audience`, `aud` must be it or a list that holds it. Without `$audience`, a token that has `aud` is refused: it was made for someone else. One token can be written in more than one way that verifies (an ES256 signature has two forms), so to refuse a token used twice, remember its `jti` claim, not the token's text.

### class JwtKey

```phv
final class JwtKey extends Verifier
```

One key: private (signs and verifies), public (verifies) or a secret (HS256).

- `public readonly string $alg` — "RS256", "ES256", "EdDSA" or "HS256".
- `public readonly ?string $kid` — The key id (`kid`) put in the header of the tokens it signs, or null.
- `public readonly bool $canSign` — Whether it can sign: a private key or a secret.
- `public static function fromPem(string $pem, ?string $kid = null): JwtKey` — A key in PEM form: a private key (`PRIVATE KEY`, `RSA PRIVATE KEY` or `EC PRIVATE KEY`) or a public one (`PUBLIC KEY`). RSA keys of 2048 to 4096 bits sign (up to 8192 verify), P-256 and Ed25519 keys both. Throws ValueError for other keys, and for a public key that could never verify a token (an RSA public exponent that is even, below 3 or above 2^33 - 1; a point off the curve; an Ed25519 point of small order). Keep private keys in an environment variable.
- `public static function fromJwk(string $jwk): JwtKey` — The public key of a JWK (JSON). Private members are not read. Throws ValueError for the keys `fromPem()` refuses.
- `public static function secret(string $secret, ?string $kid = null): JwtKey` — An HS256 key: a secret of at least 32 bytes (`random_token()` makes one).
- `public function sign(map<string, any> $claims): string` — A token with these claims. Throws ValueError for a public key.
- `public function publicJwk(): string` — The public key as a JWK (JSON). Throws ValueError for a secret.

### class KeySet

```phv
final class KeySet extends Verifier
```

Several keys, such as the JWKS an identity provider publishes.

- `public static function of(list<JwtKey> $keys): KeySet` — The keys given (to accept tokens signed by an old key while a new one takes over).
- `public static function fromJson(string $jwks): KeySet` — The signing keys of a JWKS document; keys of other kinds are skipped.
- `public static function fetch(string $service, string $path): KeySet` — The JWKS at `$path` of the HTTP service `[http.<name>]`. Key sets are kept for the whole server, fetched again when they are older than their `Cache-Control: max-age` (an hour without one) and when a token names a key id the set does not have (at most every 30 seconds). Throws NetworkError when it cannot be fetched and none is kept; it is then tried again at most every 10 seconds. Sets are kept per service, so services that share a URL and differ in their headers get their own sets.
- `public function toJson(): string` — The public keys as a JWKS document, to publish (secrets are left out).


## std.oauth

std.oauth — sign-in with OAuth 2.0 and OpenID Connect (`[oauth.<name>]` in phv.toml).

```phv
// public/login/google.phv: send the visitor to the provider
import std.oauth.OAuth;
OAuth::provider("google")->redirect();
```


```phv
// public/auth/google.phv: the redirect URI, where the provider sends the visitor back
import std.http.{response, session};
import std.oauth.{OAuth, OAuthError};
try {
$signIn = OAuth::provider("google")->callback();
} catch (OAuthError $e) {
response()->status(400);
echo "Sign-in failed: ", $e->getMessage();
return;
}
session()->regenerate();
session()->set("user", $signIn->subject() ?? "");
response()->redirect("/");
```


The authorization code flow with PKCE: the state, the nonce and the code verifier are
kept in the visitor's session for 10 minutes and one use, so a sign-in cannot be
finished in another browser. With OpenID Connect (`issuer`), the ID token's signature,
issuer, audience, nonce and expiry are checked before callback() returns.

### class OAuthError

```phv
class OAuthError extends Exception
```

A sign-in that failed: the visitor refused it, it expired, or the provider's answer was not valid.

- `public readonly string $error` — The OAuth error code: `access_denied`, `invalid_state`, `invalid_id_token`, ...
- `public function __construct(string $message, public readonly string $error)`

### class SignIn

```phv
class SignIn
```

A finished sign-in.

- `public readonly string $provider` — The name of the provider in phv.toml.
- `public readonly string $accessToken` — For calls to the provider's API (`Authorization: Bearer ...`). Keep it on the server.
- `public readonly ?string $refreshToken`
- `public readonly ?int $expiresIn` — Seconds the access token lasts, if the provider said.
- `public readonly ?string $scope` — The scopes granted, if the provider said (space-separated).
- `public readonly ?string $idToken` — The ID token (OpenID Connect), already verified. Null without OpenID Connect: an ID token that was not verified is not passed on.
- `public readonly map<string, any> $claims` — The claims of the ID token; empty without OpenID Connect.
- `public function __construct(public readonly string $provider, public readonly string $accessToken, public readonly ?string $refreshToken, public readonly ?int $expiresIn, public readonly ?string $scope, public readonly ?string $idToken, public readonly map<string, any> $claims)`
- `public function subject(): ?string` — The visitor's id at the provider (`sub`): stable, unlike the e-mail address. Null without OpenID Connect.
- `public function claim(string $name): ?string` — A claim of the ID token that is a string (`name`, `picture`, ...), or null.
- `public function verifiedEmail(): ?string` — The e-mail address, only when the provider says it verified it (`email_verified` is true). Null otherwise.

### class OAuth

```phv
class OAuth
```

A sign-in provider named `[oauth.<name>]` in phv.toml.

- `public static function provider(string $name): OAuth`
- `public function authorizationUrl(map<string, string> $params = []): string` — The provider's URL that starts a sign-in, with extra parameters such as `["prompt" => "select_account"]` or `["login_hint" => $email]` (the ones of the flow itself are set by PHV). Each call starts a new sign-in in the session.
- `public function redirect(map<string, string> $params = []): void` — Sends the visitor to the provider (a 302 response).
- `public function callback(): SignIn` — Finishes the sign-in on the redirect URI: checks the answer, exchanges the code for tokens and verifies the ID token. Throws OAuthError when the sign-in failed, NetworkError when the provider cannot be reached. Call `session()->regenerate()` before you store who signed in. It must come before `response()->flush()` (the sign-in is kept in the session); after it, it throws ValueError.

