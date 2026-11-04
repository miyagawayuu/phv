# Tutorial

English | [日本語](tutorial.ja.md)

This tutorial builds a small site that keeps notes in SQLite: a page with a form, a module, a template, and tests.
It takes about twenty minutes. The [specification](spec.md) has the details of everything used here.

## 1. Install PHV

Download the archive for your platform from the [releases](https://github.com/miyagawayuu/phv/releases), check it
against `SHA256SUMS`, and put `phv` (`phv.exe` on Windows) on your `PATH`.

```bash
phv --version
```

For your editor, install **PHV Language** from the Visual Studio Code Marketplace: it shows errors as you type,
completes names, renames them across files, and runs scripts under the debugger.

## 2. A first script

```bash
phv new hello
cd hello
phv run main.phv Ann
```

`phv new` writes a project: `phv.toml`, `main.phv`, a module in `lib/greet.phv`, and a test. The script prints
`Hello, Ann!`. Open `lib/greet.phv`:

```phv
export function greet(string $name): string {
    return "Hello, {$name}!";
}
```

Types are written on parameters and return values, and a variable keeps the type of its first value. Try calling
`greet(42)` in `main.phv`: `phv run` refuses to start and shows the line, because `42` is not a `string`. The
editor shows the same error as you type.

```bash
phv test
```

runs the `test_*` functions of the `*_test.phv` files, here `tests/greet_test.phv`.

## 3. A site

```bash
cd ..
phv new notes --site
cd notes
phv serve --watch
```

Open http://127.0.0.1:8080/. Pages are the `.phv` files of `public/`: `public/index.phv` answers `/`, and
`public/about.phv` would answer `/about`. With `--watch` the browser reloads the page when you save a file.

`views/layout.phv.html` is the page around every page: `layout("views/layout.phv.html", title: "Home")` in a page
puts what the page writes into it.

## 4. Store notes

Create `lib/notes.phv` (make the `lib` directory first):

```phv
import std.db.Database;

export class Note {
    public function __construct(public readonly int $id, public readonly string $text) {}
}

export function notes_db(): Database {
    $db = Database::open("data/notes.db");
    $db->execute(sql"CREATE TABLE IF NOT EXISTS notes (id INTEGER PRIMARY KEY, text TEXT NOT NULL)");
    return $db;
}

export function all_notes(Database $db): list<Note> {
    $rows = $db->query(sql"SELECT id, text FROM notes ORDER BY id DESC");
    return $rows->map(fn($r) => new Note($r->int("id") ?? 0, $r->string("text") ?? ""));
}

export function add_note(Database $db, string $text): void {
    $db->execute(sql"INSERT INTO notes (text) VALUES ({$text})");
}
```

- `export` makes a function or class visible to other files, which `import` it by path: `lib.notes`.
- Queries are `sql"..."` literals. `{$text}` in one is sent to the database as a parameter, never pasted into the
  SQL, so a note containing quotes cannot change the query. Building SQL from strings is a compile error.
- The database file is in `data/`, which `phv.toml` allows the site to write (`[permissions] write`). A script can
  touch no file and no environment variable that `phv.toml` does not list.

## 5. A form

Replace `public/index.phv`:

```phv
<?phv
import std.http.{request, response};
import std.validate.Validator;
import lib.notes.{notes_db, all_notes, add_note};

$db = notes_db();
$req = request();
list<string> $errors = [];
if ($req->isPost()) {
    $v = new Validator($req->formParams);
    $v->required("text", "Write something first.")->maxLength("text", 200);
    if ($v->fails()) {
        $errors = $v->errors()["text"] ?? [];
    } else {
        add_note($db, $v->value("text") ?? "");
        response()->redirect("/");
        return;
    }
}
layout("views/layout.phv.html", title: "Notes");
?>
<h1>Notes</h1>
<form method="post">
  <?= csrf_field() ?>
  <input name="text" placeholder="A note" maxlength="200">
  <button>Add</button>
</form>
<?phv foreach ($errors as $error): ?>
<p class="error"><?= $error ?></p>
<?phv endforeach; ?>
<ul>
<?phv foreach (all_notes($db) as $note): ?>
  <li><?= $note->text ?></li>
<?phv endforeach; ?>
</ul>
```

Add a note in the browser, then one like `<script>alert(1)</script>`: it is shown as text. Everything written with
`<?= ?>` is escaped for where it is written, and putting a value where escaping cannot protect it (an unquoted
attribute, a URL, a `<script>`) is a compile error.

- `csrf_field()` adds the token that form posts need; a post without it is refused (`[security] csrf`).
- After a successful post the page redirects, so reloading does not post again.

## 6. Test it

Replace `tests/home_test.phv`:

```phv
<?phv
import std.testing.TestClient;

#[Before]
function empty_the_notes(): void {
    file_put_contents("data/notes.db", "");
}

function test_adding_a_note(): void {
    $c = new TestClient();
    $r = $c->post("/", ["text" => "Buy <milk>"]);
    assert_eq(302, $r->status);
    assert($c->get("/")->body->contains("Buy &lt;milk&gt;"), "the note is shown, escaped");
}

function test_an_empty_note_is_refused(): void {
    $r = (new TestClient())->post("/", ["text" => "  "]);
    assert($r->body->contains("Write something first."));
}
```

```bash
phv test
```

A `TestClient` runs the site's pages in the same process, keeping cookies like a browser; no server is started.
`#[Before]` runs before each test (here it empties the database). `phv test --junit report.xml` writes a report
for CI.

## 7. Debug

The debugger runs scripts. Create `try.phv` next to `phv.toml`:

```phv
<?phv
import lib.notes.{notes_db, add_note, all_notes};

$db = notes_db();
add_note($db, "from a script");
echo all_notes($db)->len(), " notes\n";
```

In VS Code, click left of a line number in `add_note` (in `lib/notes.phv`) to set a breakpoint, open `try.phv`, and
add a `.vscode/launch.json`:

```json
{
  "configurations": [
    { "type": "phv", "request": "launch", "name": "Run the open script", "program": "${file}" }
  ]
}
```

F5 runs `try.phv` and stops at the breakpoint. F10, F11 and Shift+F11 step over, into and out; the Variables view
shows the variables of each frame and opens lists, maps and objects; hovering a variable shows its value.

## 8. Ship it

```bash
phv build . -o notes-server
./notes-server --port 8080 --prod
```

`phv build` compiles the site into one executable that serves it without the sources; the directories it writes
(`data/`) are under `--root`, the current directory by default. `--prod` hides error details from visitors: use it
behind a reverse proxy too, which reaches the server on `127.0.0.1`, where development mode is otherwise the default.
Set `[security] secure_cookies = true` once the site is served over HTTPS. The specification's §14 lists every
setting.

## Next

- [The specification](spec.md): the language, the web server, the standard library and the tools.
- [The standard library reference](stdlib.md): every class and function of the `std.*` modules (generated with
  `phv doc --std`).
- `phv doc` writes the same kind of reference for your own modules from their `/** ... */` comments.
