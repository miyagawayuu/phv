# チュートリアル

[English](tutorial.md) | 日本語

このチュートリアルでは、メモを SQLite に保存する小さなサイトを作ります。フォームのあるページ、モジュール、テンプレート、
テストを書きます。所要時間は 20 分ほどです。ここで使うものの詳細は[言語仕様](spec.ja.md)にあります。

## 1. PHV を入れる

[リリース](https://github.com/miyagawayuu/phv/releases)から使っているプラットフォームのアーカイブをダウンロードし、
`SHA256SUMS` と照合してから、`phv`(Windows では `phv.exe`)を `PATH` の通った場所に置きます。

```bash
phv --version
```

エディタには、Visual Studio Code の Marketplace から **PHV Language** を入れます。入力中にエラーを表示し、名前を補完し、
ファイルをまたいで名前を変更し、デバッガでスクリプトを実行できます。

## 2. 最初のスクリプト

```bash
phv new hello
cd hello
phv run main.phv Ann
```

`phv new` はプロジェクトを作ります。`phv.toml`、`main.phv`、`lib/greet.phv` のモジュール、テストです。スクリプトは
`Hello, Ann!` と出力します。`lib/greet.phv` を開いてみましょう:

```phv
export function greet(string $name): string {
    return "Hello, {$name}!";
}
```

型は引数と戻り値に書きます。変数は最初に入れた値の型を保ちます。`main.phv` で `greet(42)` と呼んでみると、`42` は
`string` ではないので、`phv run` は実行を始めずにその行を示します。エディタも入力中に同じエラーを表示します。

```bash
phv test
```

は `*_test.phv` ファイルの `test_*` 関数を実行します。ここでは `tests/greet_test.phv` です。

## 3. サイト

```bash
cd ..
phv new notes --site
cd notes
phv serve --watch
```

http://127.0.0.1:8080/ を開きます。ページは `public/` の `.phv` ファイルです。`public/index.phv` が `/` に、
`public/about.phv` があれば `/about` に答えます。`--watch` を付けると、ファイルを保存したときにブラウザがページを
再読み込みします。

`views/layout.phv.html` はすべてのページを囲むページです。ページで `layout("views/layout.phv.html", title: "Home")` と
書くと、そのページが書く内容がレイアウトの中に入ります。

## 4. メモを保存する

`lib/notes.phv` を作ります(先に `lib` ディレクトリを作ってください):

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

- `export` を付けた関数やクラスはほかのファイルから見えます。ほかのファイルはパスで `import` します: `lib.notes`。
- クエリは `sql"..."` リテラルで書きます。その中の `{$text}` は SQL に埋め込まれず、パラメータとしてデータベースに
  送られるので、引用符を含むメモでもクエリは変わりません。文字列を連結して SQL を作るとコンパイルエラーになります。
- データベースのファイルは `data/` にあり、`phv.toml` がサイトにそこへの書き込みを許しています(`[permissions] write`)。
  スクリプトは `phv.toml` に書かれていないファイルや環境変数に触れられません。

## 5. フォーム

`public/index.phv` を次のように置き換えます:

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

ブラウザでメモを追加し、次に `<script>alert(1)</script>` のようなメモを追加してみてください。テキストとして表示されます。
`<?= ?>` で書いたものは書く場所に合わせてエスケープされ、エスケープでは守れない場所(引用符のない属性、URL、
`<script>`)に値を入れるとコンパイルエラーになります。

- `csrf_field()` はフォームの送信に必要なトークンを加えます。トークンのない送信は拒否されます(`[security] csrf`)。
- 送信に成功したらリダイレクトするので、再読み込みしても二重に送信されません。

## 6. テストする

`tests/home_test.phv` を次のように置き換えます:

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

`TestClient` はサイトのページを同じプロセスで実行し、ブラウザのように Cookie を保ちます。サーバーは起動しません。
`#[Before]` は各テストの前に実行されます(ここではデータベースを空にします)。テストは `phv serve` と同じ
`data/notes.db` を使うので、実行するとブラウザで追加したメモは消えます。実際のプロジェクトでは、テスト用に別のデータベースを
使うようにしてください。`phv test --junit report.xml` は CI 用の報告を書き出します。

## 7. デバッグする

デバッガはスクリプトを実行します。`phv.toml` の隣に `try.phv` を作ります:

```phv
<?phv
import lib.notes.{notes_db, add_note, all_notes};

$db = notes_db();
add_note($db, "from a script");
echo all_notes($db)->len(), " notes\n";
```

VS Code で `lib/notes.phv` の `add_note` の行番号の左をクリックしてブレークポイントを置き、`try.phv` を開いて、
`.vscode/launch.json` を加えます:

```json
{
  "configurations": [
    { "type": "phv", "request": "launch", "name": "Run the open script", "program": "${file}" }
  ]
}
```

F5 で `try.phv` を実行するとブレークポイントで止まります。F10・F11・Shift+F11 でステップオーバー・ステップイン・
ステップアウトします。変数ビューは各フレームの変数を表示し、list・map・オブジェクトを開けます。変数にマウスを置くと
値が表示されます。

## 8. 公開する

```bash
phv build . -o notes-server
./notes-server --port 8080 --prod
```

`phv build` はサイトを、ソースなしで配信する 1 つの実行ファイルにします。書き込むディレクトリ(`data/`)は `--root`
(既定はカレントディレクトリ)の下です。`--prod` は訪問者にエラーの詳細を見せません。リバースプロキシの後ろでも
使ってください。プロキシは `127.0.0.1` でサーバーにつなぐので、指定しないと開発モードになります。HTTPS で配信するように
なったら `[security] secure_cookies = true` にします。すべての設定は言語仕様の §14 にあります。

## 次に

- [言語仕様](spec.ja.md): 言語、Web サーバー、標準ライブラリ、ツール。
- [標準ライブラリのリファレンス](stdlib.md)(英語): `std.*` モジュールのすべてのクラスと関数(`phv doc --std` で生成)。
- `phv doc` は、自分のモジュールについても `/** ... */` コメントから同じようなリファレンスを書き出します。
