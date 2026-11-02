# PHX 言語仕様 (v0.2)

[English](spec.md) | 日本語

公式サイト: <https://phx-lang.org>

PHX は、PHP の「保存してすぐ動く」「Web に特化」「HTML に埋め込める」という良さを引き継ぎながら、
型安全性・セキュリティ・実行速度・保守性を言語の側で保証する Web 向けの言語です。
既存の PHP コードとの互換性はありませんが、PHP を書いてきた人がすぐ読めて書けることを目指しています。

## 1. 設計方針

| PHP の弱点 | PHX での解決 |
|---|---|
| 暗黙の型変換 (`"1" + 1`、`"abc" == 0`) | 暗黙の変換はしない。`==` は常に厳密比較。違う型どうしの比較はコンパイルエラー |
| null による実行時エラー | `?T` 型。null チェック (`!= null`、`is`、`??`、`?->`) をしないと使えない |
| XSS | `<?= ?>` と Web ページへの `echo` は自動で HTML エスケープ。エスケープしないのは `Html` 型の値だけ |
| `eval`、可変変数、`extract`、動的な `include` | 言語に存在しない。モジュールは静的な `import`、テンプレートはリテラルのパスだけ |
| SQL インジェクション | SQL は `sql"..."` リテラル(`Sql` 型)でしか書けず、埋め込んだ値は必ずプレースホルダになる |
| 文脈を無視したエスケープ | `<?= ?>` の位置(属性・URL・`<script>`・CSS)を判定し、エスケープでは守れない場所への挿入はコンパイルエラー |
| CSRF・セッション固定 | フォームの POST はサーバーがトークンを自動で検査。セッション ID はサーバーが発行したものだけを受け付ける |
| 何でもできるスクリプト | ファイル・環境変数へのアクセスは `phx.toml` の `[permissions]` で許可したものだけ |
| 注入されたスクリプトの実行 | 既定の Content-Security-Policy で、テンプレートに書いた `<script>` 以外のインラインスクリプトを止める |
| ヘッダーインジェクション | 改行を含むヘッダー値は `ValueError` |
| パストラバーサル | サーバーは `public/` の外、ドットファイル、テンプレートを配信しない |
| リクエスト間で状態が漏れる | リクエストごとに新しいヒープ。静的プロパティもグローバル変数もない |
| 実行速度 | 静的型に特化したレジスタ型バイトコード VM |
| 大規模化すると保守しにくい | 型注釈の付いた関数、モジュールと `export`、読みやすいエラーメッセージ |

## 2. ファイル

- `.phx` … スクリプトまたはモジュール。**コードモード**で始まる。先頭の `<?phx` は書いても書かなくてもよい。
- `.phx.html` … テンプレート。**HTML モード**で始まる。
- コードモードで `?>` を書くと HTML モードに切り替わる。HTML モードでは `<?phx`(または `<? `)でコードに、
  `<?= 式 ?>` で値の出力に戻る。`?>` の直後の改行 1 つは出力されない(PHP と同じ)。
- 文字コードは UTF-8。

## 3. 字句

- 変数は `$name`。`$$name`(可変変数)はない。
- コメントは `// ...`、`# ...`、`/* ... */`。
- 整数: `42`、`1_000_000`、`0xFF`、`0b1010`、`0o17`。64 ビット。オーバーフローは `ArithmeticError`。
- 浮動小数点数: `1.5`、`1e-3`。
- 文字列:
  - `'...'` … 展開なし。エスケープは `\'` と `\\` だけ。
  - `"..."` … `$name` と `{$式}` を展開する。エスケープは `\n \t \r \0 \e \\ \" \$ \{ \u{1F600}`。
- 予約語: `function fn return if elseif else endif while endwhile do for endfor foreach endforeach as break continue
  echo class extends new public private protected readonly static true false null match default try catch finally
  throw import export use is instanceof interface implements abstract final enum case const`

## 4. 型

| 型 | 説明 |
|---|---|
| `int` | 64 ビット整数 |
| `float` | 64 ビット浮動小数点数 |
| `string` | UTF-8 文字列 |
| `bool` | `true` / `false` |
| `null` | null だけの型。普通は `?T` の一部として使う |
| `void` | 戻り値がないことを表す(戻り値の型にだけ使える) |
| `Html` | 安全な HTML として扱ってよい文字列。`raw()`、`escape()`、テンプレートが作る |
| `SafeUrl` | URL 属性の先頭に置いてよい `Html`。`safe_url()` が作る |
| `SafeJs` | `<script>` の中に置いてよい `Html`。`json_html()` が作る |
| `any` | どんな値でも入る。使う前に `is` で絞り込む必要がある |
| `list<T>` | 要素の型が `T` の配列(0 から始まる連番) |
| `map<K, V>` | 挿入順を保つ連想配列。`K` は `int` か `string` |
| `?T` | `T` または `null`(`T|null` と同じ) |
| `A|B` | ユニオン型 |
| `fn(A, B): R` | 関数(クロージャ)の型 |
| クラス名 | オブジェクトの型。`self` はそのクラス自身 |

PHP の `array` は `list<T>` と `map<K, V>` に分かれています。`mixed` は `any` です。

### 代入できる条件

- 同じ型、サブクラスから親クラス、`T` から `?T` やそれを含むユニオン、任意の型から `any`。
- **`int` は `float` が期待される場所では自動で `float` に変換される**(唯一の暗黙の変換。値の意味が変わらないため)。
  `list<int>` を `list<float>` に渡すことはできない(要素の型は不変)。
- それ以外の変換は明示的に書く: `(int)$float`、`(float)$int`、`(string)$x`、`$s->toInt()`、`$s->toFloat()`。
  `(int)"12"` のような文字列からのキャストはできない(`->toInt()` は数値でなければ `null` を返す)。
  `(bool)` キャストはない。

## 5. 変数

- PHP と同じく、**最初の代入で変数が宣言される**。そのとき値の型が変数の型になり、あとから別の型は代入できない。

  ```phx
  $count = 1;
  $count = "two";   // エラー: expected `int`, found `string`
  ```

- 型を推論できないとき(`null` や空の `[]`)は型を書いて宣言する。

  ```phx
  ?User $found = null;
  list<string> $names = [];
  map<string, int> $counts = [];
  ```

- 変数のスコープは関数全体(PHP と同じ)。ただし **一部の経路でしか代入されない変数は使えない**。

  ```phx
  if ($ok) { $msg = "hi"; }
  echo $msg;   // エラー: variable `$msg` might not be defined here
  ```

- `foreach` と `catch` の変数は、同じ名前の既存の変数と型が違うとき、新しい変数としてそれを隠す
  (`catch (A $e)` と `catch (B $e)` を続けて書ける)。
- `$this` は非 static メソッドの中だけで使える。代入はできない。

### 型の絞り込み (narrowing)

条件によって、ローカル変数の型が狭められる。

```phx
function greet(?User $u): string {
    if ($u == null) {
        return "guest";
    }
    return $u->name;          // ここでは $u は User
}

if ($v is int) { ... }        // ここでは $v は int
$name = $req->query("name") ?? "world";   // string
$len = $user?->email?->len();             // ?int
```

`!= null`、`== null`、`is`、`!`、`&&`、`||`、早期 `return` / `throw` / `break` / `continue` が絞り込みに使われる。
絞り込みの対象はローカル変数と、ローカル変数のプロパティ(`$this->user != null`)。プロパティの絞り込みは、
関数やメソッドの呼び出し、プロパティへの代入があると解除される(呼び出しが値を変えたかもしれないため)。

## 6. 演算子

優先順位の高い順:

| 演算子 | 説明 |
|---|---|
| `()` `[]` `->` `?->` `::` | 呼び出し、添字、メンバー |
| `**` | べき乗(右結合) |
| `is` `instanceof` | 型の判定 |
| `!` `-` `~` `(int)` など | 単項演算子、キャスト |
| `*` `/` `%` | `/` は常に `float` を返す。整数の割り算は `intdiv()` |
| `+` `-` | 数値どうしだけ。`int` と `float` が混ざると `float` |
| `.` | 文字列の連結。`string`、`int`、`float` を連結できる。`Html . Html` は `Html` |
| `<<` `>>` | ビットシフト |
| `..` `..=` | 範囲(`0..$n` は `$n` を含まない、`1..=$n` は含む) |
| `<` `<=` `>` `>=` | 数値どうし、または文字列どうし |
| `==` `!=` `<=>` | `==` は常に厳密。`===` は使わない(書くとエラーで `==` を案内する) |
| `&` `^` `\|` | ビット演算(`int` だけ) |
| `&&` | 論理積(`bool` だけ) |
| `\|\|` | 論理和(`bool` だけ) |
| `??` | 左辺が null なら右辺。`$m["k"] ?? $d` はキーがなくても例外にならない |
| `? :` | 三項演算子 |

- **真偽値への暗黙変換(truthiness)はない**。`if ($xs)` はエラーで、`if (!$xs->isEmpty())` と書く。
- 代入は式ではなく文: `=`、`+=`、`-=`、`*=`、`/=`、`%=`、`**=`、`.=`、`??=`、`++`、`--`。
- 文字列の添字アクセス(`$s[0]`)はない。`$s->slice(0, 1)` を使う。

## 7. 制御構文

```phx
if (...) { } elseif (...) { } else if (...) { } else { }
while (...) { }
do { } while (...);
for ($i = 0; $i < 10; $i++) { }
foreach ($list as $value) { }
foreach ($list as $index => $value) { }
foreach ($map as $key => $value) { }
foreach (0..$n as $i) { }            // 範囲は配列を作らずにループする
break; continue; break 2; continue 2;
```

テンプレート向けの書き方も使える: `if (...): ... elseif (...): ... else: ... endif;`、
`foreach (...): ... endforeach;`、`while (...): ... endwhile;`、`for (...): ... endfor;`

`foreach` は開始時点の配列のコピーを回る(PHP と同じ値のセマンティクス)。

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

比較は `==`(厳密)。どの腕にも当てはまらず `default` もないときは `UnhandledMatchError`。

### 例外

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
    $lock->release();      // return / break / continue / 例外のどれで抜けても実行される
}
```

投げられるのは `Exception` かそのサブクラスだけ。

## 8. 関数

```phx
function greet(string $name, int $times = 1): string { ... }
greet("Ann");
greet(name: "Bob", times: 3);       // 名前付き引数
```

- 引数の型は必須。戻り値の型を省略すると `void`。
- 値を返す関数は、すべての経路で `return` しないとエラー。
- デフォルト値は定数(リテラル、`null`、`[]`)だけ。
- 関数は値ではない。関数を渡すときはクロージャで包む。
- 型パラメータを持つジェネリック関数を書ける(§9 の「ジェネリック関数」)。

### クロージャ

```phx
$double = fn(int $x): int => $x * 2;           // 外側の変数を自動でキャプチャ
$names->map(fn($n) => $n->upper());            // 引数の型は文脈から推論される
$counter = function () use ($start): int { return $start + 1; };
```

- キャプチャはすべて**値渡し**(作られた時点のコピー)。参照キャプチャ(`&$x`)はない。
- メソッドの中で作ったクロージャは `$this` を使える。

## 9. クラス

```phx
class User {
    private int $loginCount = 0;
    public readonly string $email;

    public function __construct(
        public readonly int $id,      // コンストラクタでのプロパティ昇格
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

- プロパティには型が必須。`null` を許さずデフォルト値もないプロパティは、コンストラクタの先頭で代入しなければならない
  (コンパイル時にチェックされる)。
- `readonly` プロパティは、そのクラスのコンストラクタの中でしか代入できない。
- 可視性は `public` / `protected` / `private`(コンパイル時にチェック)。コンストラクタも同じで、
  `private function __construct(...)` のクラスはそのクラスの中(static メソッドなど)でしか `new` できない
  (`Database` はこのため `Database::open()` でしか作れない)。
- 単一継承。メソッドのオーバーライドは引数の型が同じで、戻り値の型が互換であること。親のコンストラクタを持つ
  クラスで初期化の必要なプロパティがある場合、子のコンストラクタは `parent::__construct(...)` を呼ぶ必要がある。
- `static` メソッドは `ClassName::method()` で呼ぶ。**static プロパティはない**(リクエスト間で状態を共有させないため)。
- オブジェクトは参照、`list` と `map` は値(コピーオンライト)。
### interface と abstract

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

- interface はメソッドのシグネチャと定数だけを持つ。`extends` で複数の interface を継承できる。
- `abstract` メソッドは本体を持たない。abstract メソッドを持つクラスは `abstract class` と宣言する。
- 具象クラスが interface や abstract のメソッドを実装していない、またはシグネチャが合わないときはコンパイルエラー。
- interface、abstract クラス、enum は `new` できない。
- interface 型の値へのメソッド呼び出しは名前で振り分けられる。クラス型の値へは vtable で振り分けられる。

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

enum Status: string {        // int か string の値を持つ enum
    case Active = "active";
    case Banned = "banned";
}

Suit::Hearts->name;              // "Hearts"
Status::Active->value;           // "active"
Status::from("banned");          // Status::Banned(見つからなければ ValueError)
Status::tryFrom("x");            // null
Status::cases();                 // list<Status>
```

各 case は 1 回の実行(1 リクエスト)の中でただ 1 つのオブジェクトなので、`==` で比べられる。

### 定数

```phx
const MAX_ITEMS = 50;            // ファイルの先頭レベル(export / import もできる)
class Config {
    const VERSION = "1.0";
}
echo MAX_ITEMS, Config::VERSION, self::VERSION;
```

定数の値はリテラル(int、float、string、bool、null)だけ。

### ジェネリック関数

```phx
function first_or<T>(list<T> $xs, T $fallback): T {
    return $xs->isEmpty() ? $fallback : $xs[0];
}
$n = first_or([5, 6], 0);        // int
$s = first_or([], "none");       // string
```

型引数は呼び出しの引数から推論される(最大 4 個)。関数の中では `T` は中身の分からない型として扱われ、
`T` の値に演算子やメソッドは使えない。ジェネリッククラスはまだない。


## 10. 例外クラス

すべてのファイルから使える。

```
Exception                    (getMessage(): string)
├── RuntimeException
│   └── DatabaseError        SQL のエラー
├── LogicException
│   └── InvalidArgumentException
├── ArithmeticError          整数のオーバーフローなど
│   └── DivisionByZeroError  0 での割り算・剰余
├── IndexError               list の範囲外アクセス
├── KeyError                 map にないキー
├── ValueError               不正な引数(ヘッダーの改行など)
├── UnhandledMatchError
├── PermissionError          phx.toml で許可されていないファイル・環境変数へのアクセス
└── AssertionError           assert() / assert_eq() の失敗
```

実行時間の上限を超えたときなどの致命的なエラーは捕捉できない。

## 11. モジュール

```phx
// lib/posts.phx
export class Post { ... }
export function all_posts(): list<Post> { ... }
function helper(): int { ... }      // export しないものは外から見えない
```

```phx
import lib.posts.{Post, all_posts};
import lib.posts.Post;
import std.http.{request, response};
```

- `import a.b.{x, y};` は、プロジェクトのルート(`phx.toml` のあるディレクトリ、なければ実行するファイルの
  ディレクトリ)からの `a/b.phx` を読み込む。
- import されるモジュールのトップレベルには宣言(`function`、`class`、`interface`、`enum`、`const`、`import`)
  しか書けない。モジュールを読み込んでも副作用は起きない。
- `std.*` は標準モジュール(`std.http`、`std.db`)。`[dependencies]` に書いたパッケージは `import パッケージ名.モジュール` で読み込む。
- 読み込めるのはプロジェクトとパッケージのディレクトリの中のファイルだけ(シンボリックリンクで外に出ることはできない)。

## 12. テンプレート

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

- テンプレートは `params(...)` で型付きの引数を宣言する。`render()` の呼び出しはコンパイル時に型チェックされる。
- `render()` のパスは文字列リテラルで、プロジェクトのルートからの相対パス(`..` は使えない)。拡張子は `.phx.html`。
- `render()` は `Html` を返すので、別のテンプレートにそのまま渡して埋め込める(レイアウト)。

## 13. 出力とエスケープ

出力先には「テキスト」と「HTML」の 2 種類がある。

| 状況 | 出力先 |
|---|---|
| `phx run` で実行したスクリプト | テキスト(`--html` を付けると HTML) |
| Web サーバーが実行するページ | HTML(`response()->type("application/json")` などで HTML 以外にするとテキスト) |
| テンプレートの中 | 常に HTML |

HTML の出力先に `string` を書くと `& < > " '` がエスケープされる。`Html` 型の値と、テンプレート中の
HTML 部分はそのまま出力される。`echo` と `<?= ?>` は同じ規則に従う。

- `escape($s)` / `htmlspecialchars($s)` … エスケープ済みの `Html` を返す。
- `raw($s)` … 信頼できるマークアップを `Html` として扱う(XSS に注意して使う)。
- `nl2br($s)` … エスケープしてから改行を `<br>` にした `Html`。
- `safe_url($url)` … `http(s)`、`mailto`、`tel`、相対 URL だけを通し、`javascript:` などは `#` にした `SafeUrl`。
- `json_html($value)` … `<script>` の中に置いても安全な JavaScript の値(JSON)を `SafeJs` で返す。
  文字列の `"` はそのままなので、属性値の中には置けない(コンパイルエラー。`escape(json_encode($v))` を使う)。
- `SafeUrl` と `SafeJs` は `Html` としても使える(逆はできない)。`escape()` の結果は `Html` なので、
  URL や `<script>` の中には置けない。HTML のエスケープでは `javascript:` やスクリプトのコードを止められないため。
- `Html` と `string` は `.` で連結できない(エラーで `escape()` を案内する)。
- エスケープを外す Content-Type は `text/plain`、`application/json`、`text/csv`、`text/css`、`text/javascript`、
  `text/markdown`、`text/calendar`、`application/pdf`、`application/octet-stream`、画像・音声・動画などに限る
  (どれもブラウザが HTML として表示しない型。`X-Content-Type-Options: nosniff` も常に付く)。
  HTML・XML・SVG・知らない型ではエスケープしたまま。

### HTML の文脈の検査

HTML エスケープが守れるのは、要素の中身と**引用符で囲んだ**属性値だけ。`<?= ?>` と `echo` がそれ以外の場所に
あるとコンパイルエラーになる。場所の判定はブラウザの構文解析(WHATWG)に合わせている:
`<textarea>`・`<title>` などの中身、`<script>` の中の `<!--`、`<!-->` や `--!>` のコメント、`<svg>` の中、
`</scripts>` のように終了タグに見えて違うもの。

| 場所 | 例 | 扱い |
|---|---|---|
| 要素の中身・引用符付きの属性値 | `<p><?= $s ?></p>`、`title="<?= $s ?>"` | エスケープして出力 |
| 引用符なしの属性値 | `href=<?= $u ?>` | エラー(引用符で囲む) |
| タグの中(属性の間) | `<div <?= $attrs ?>>` | エラー |
| URL 属性の先頭 | `href="<?= $u ?>"`、`src="<?= $u ?>"` | `SafeUrl`(`safe_url($u)`)と数値だけ。前に書いた文字列に `/` `?` `#` `:` があれば先頭ではない(`href="/post?id=<?= $id ?>"` は可)。`href="j<?= ?>"` や `&#106;` のような文字参照だけでは先頭のまま |
| `javascript:` / `data:` の URL | `href="javascript:go(<?= $v ?>)"` | エラー(数値は可) |
| `<script>` の中 | `var x = <?= $v ?>;` | `SafeJs`(`json_html($v)`)と数値だけ |
| `<style>`・`style="..."` | | エラー(数値は可) |
| イベント属性 | `onclick="go(<?= $v ?>)"` | エラー。`data-*` 属性に入れて JavaScript から読む |
| `<iframe srcdoc="...">` | | エラー(値が別の HTML 文書として解析される) |

- 関数・クロージャの本体、`if`・ループのブロックは、始まったときと同じ HTML の状態で終わらなければならない
  (`<script>` や属性をブロックの中で開いたまま終えるとエラー)。呼び出し順や繰り返しで文脈がずれるのを防ぐ。
- 式と文の入れ子は 500 段まで(深すぎるソースで処理系のスタックがあふれるのを防ぐ)。

## 14. Web サーバー

```
phx serve examples/site --port 8080
```

- サイトのディレクトリには `public/` が必要。URL はファイル名で決まる:
  `/` → `public/index.phx`、`/about` → `public/about.phx` または `public/about/index.phx`、`/about.phx` も可。
- 静的ファイルとして配信するのは既知の拡張子(html、css、js、json、txt、画像、フォント、pdf、音声、動画、wasm)だけ。
- 配信しないもの: `public/` の外、`.` や `_` で始まるファイルとディレクトリ(`_partial.phx` は部品に使える)、
  `.phx.html`(テンプレート)、知らない拡張子(`.php`、`.bak`、`.sql` など)。URL はファイル名と完全に同じ綴り
  でなければならない(`INDEX.PHX` や Windows の短縮名、`::$DATA` では開けない)。
- リクエストごとに新しい VM(新しいヒープ)で実行する。コンパイル済みのバイトコードだけが共有される。
- ファイルを保存すると次のリクエストで再コンパイルされる(依存する全ファイルの更新時刻を見る)。
- 上限: 実行時間 30 秒(`--time-limit`)、メモリ 128 MB(`[security] memory_limit_mb`)、本文 8 MB、ヘッダーの受信 10 秒、
  本文の受信 30 秒、同時接続 1024、クエリ・フォームの項目 1000 個。
- `[security] requests_per_minute` を書くと、1 つの接続元アドレスから 1 分間に受け付けるリクエストをその数までにする
  (超えると `429 Too Many Requests` と `Retry-After`)。既定は無制限。
- 既定で付けるヘッダー: `X-Content-Type-Options: nosniff`、`X-Frame-Options: SAMEORIGIN`、
  `Referrer-Policy: strict-origin-when-cross-origin`、Content-Security-Policy(下の「Content-Security-Policy」)。
  `secure_cookies = true` なら HSTS。セッションや Cookie を使うレスポンスには `Cache-Control: private, no-store`。
  `Server` / `X-Powered-By` のようなバージョン情報は出さない。スクリプトは `Content-Length` などの通信用ヘッダーを設定できない。
- 開発モードではコンパイルエラーと実行時エラーの詳細をブラウザに表示する。開発モードが既定になるのは
  `127.0.0.1` / `localhost` で待ち受けるときだけ(`--dev` / `--prod` で明示できる)。

### std.http

```phx
import std.http.{request, response, session};

$req = request();
$req->method;              // "GET"
$req->path;                // "/post"
$req->query("id");         // ?string
$req->form("name");        // ?string (urlencoded / multipart。同じ名前が複数あれば最後の値)
$req->formAll("ids");      // list<string>: 同じ名前で送られたすべての値（チェックボックスなど）
$req->queryAll("tag");     // list<string>: ?tag=a&tag=b のすべての値
$req->header("Accept");    // ?string(名前の大文字小文字は区別しない)
$req->cookie("theme");     // ?string
$req->file("avatar");      // ?UploadedFile
$req->json();              // any
$req->body;                // string
$req->ip;                  // string(接続元の IP アドレス。リバースプロキシの後ろではプロキシのアドレス)
$req->isPost();            // bool

response()->status(404);
response()->header("Cache-Control", "no-store");
response()->type("text/plain; charset=utf-8");
response()->cookie("theme", "dark");
response()->redirect("/login");                       // サイト内のパスだけ
response()->redirectExternal("https://example.com/");  // 外部の URL
response()->json($value);
response()->file("data/photos/1.jpg");                // 出力の代わりにファイルを送る
```

### セッション

```phx
import std.http.session;

session()->set("user_id", (string)$user->id);
$id = session()->get("user_id");      // ?string
session()->regenerate();              // ログイン直後に呼ぶ(セッション固定攻撃の対策)
session()->flash("notice");           // 読んだら消えるメッセージ
session()->destroy();                 // ログアウト
```

- 値はサーバー側に保存され、Cookie には 256 ビットの乱数の ID だけが入る
  (`PHXSESSID`、`HttpOnly`、`SameSite=Lax`、`[security] secure_cookies = true` で `Secure`)。
- サーバーが発行していない ID は受け付けない。1 セッションの値は合計 64 KB まで。
- `[security] session_ttl` 秒アクセスがないと消える。アクセスが続いても 24 時間で消える。
- 既定の保存先はメモリで、サーバーを再起動すると消える。`[security] session_store = "sessions/"` のように
  プロジェクト内のディレクトリを書くと、1 セッション 1 ファイルで保存する。再起動しても残り、同じディレクトリを使う
  複数のサーバープロセスで共有できる。ファイル名は ID のハッシュなので、ディレクトリの一覧から ID は分からない。
  `public/` の中は指定できない。

### CSRF 対策

`POST`・`PUT`・`PATCH`・`DELETE` のうち、ブラウザがほかのサイトから送れる形式
(`application/x-www-form-urlencoded`、`multipart/form-data`、`text/plain`、Content-Type なし)のリクエストは、
スクリプトを実行する前にサーバーがトークンを確認する。トークンがない、または違うときは 403 になる。

```html
<form method="post">
  <?= csrf_field() ?>     <!-- <input type="hidden" name="_csrf" value="..."> -->
  ...
</form>
```

JavaScript から送るときは `csrf_token()` の値を `X-CSRF-Token` ヘッダーに入れる。
JSON のリクエストは CORS のプリフライトが必要なので、ほかのサイトからは送れない。

トークンはブラウザごとの Cookie(`PHXCSRF`)の HMAC なので、攻撃者がその Cookie を書き換えられないことが前提になる。
本番では HTTPS で配信して `secure_cookies = true` にする(Cookie が `__Host-` 付きになり、平文の通信や
サブドメインから上書きされなくなる)。
`[security] csrf = false` で無効にできる。

### Cookie

```phx
request()->cookie("theme");                                   // ?string
response()->cookie("theme", "dark", maxAge: 86400 * 30);      // HttpOnly; SameSite=Lax が既定
response()->deleteCookie("theme");
```

名前に使えない文字、改行、`SameSite=None` で `Secure` なし、は `ValueError`。

### アップロード

```phx
$file = request()->file("avatar");     // ?UploadedFile
if ($file != null) {
    echo $file->name, $file->type, $file->size;
    $file->saveTo("storage/avatars/" . random_token(16) . ".png");   // [permissions].write が必要
}
```

`name` はクライアントが送ったファイル名からディレクトリ部分と制御文字を取り除いたもの。
保存先のパスには使わないこと。リクエスト本文は合計 8 MB まで。

保存したファイルは `public/` の外にあるので、`response()->file($path, $type = "")` で返す。ファイルのバイト列が
スクリプトの出力の代わりに送られるため、画像などのバイナリファイルも返せる（`echo` は UTF-8 のテキストだけ）。
パスには `[permissions].read` か `write` が必要。`$type` を省くと拡張子から Content-Type を決める（画像・音声・動画・
フォント・PDF・txt・csv・json）。HTML・SVG・XML と未知の拡張子は、アップロードされたファイルがサイト上のページとして
動かないよう `application/octet-stream` で送る。

### JSON

```phx
$data = request()->json();             // any(不正な JSON は ValueError)
if ($data is map) {
    $name = $data["name"] ?? null;
    if ($name is string) { ... }
}
response()->json(["ok" => true]);
```

### データベース(std.db)

```phx
import std.db.Database;

$db = Database::open("data/app.db");      // [permissions].write = ["data/"] が必要。":memory:" も可
$db->execute(sql"CREATE TABLE IF NOT EXISTS posts (id INTEGER PRIMARY KEY, title TEXT)");
$db->execute(sql"INSERT INTO posts (title) VALUES ({$title})");
$id = $db->lastInsertId();

foreach ($db->query(sql"SELECT id, title FROM posts WHERE id > {$min} ORDER BY id") as $row) {
    echo $row->int("id"), $row->string("title");      // ?int, ?string
}
$one = $db->first(sql"SELECT * FROM posts WHERE id = {$id}");   // ?Row

$db->transaction(function () use ($db): void {
    $db->execute(sql"UPDATE posts SET title = 'x'");
});                                         // 例外が出たら ROLLBACK
```

- **SQL は `sql"..."` リテラル(`Sql` 型)でしか書けない。** `{$x}` で埋め込んだ値は必ずプレースホルダとして
  渡されるので、SQL インジェクションは起こらない。`string` を `query()` に渡すことはできない。
- `Sql` どうしは `.` で連結できる(パラメータも引き継がれる)。`Sql` と `string` は連結できない。
- 列名やテーブル名を動的に決めたいときは `sql_ident($name)`(英数字と `_` だけを許可して引用する)。
- `ATTACH` は無効になっていて、許可されたファイル以外のデータベースには触れない。
- SQL のエラーは `DatabaseError`。

### 日付・暗号・エンコード

| 関数 | 説明 |
|---|---|
| `date($format, ?$ts)` / `gmdate(...)` | PHP と同じ書式(`Y-m-d H:i:s`、`D, d M Y`、`c`、`U` など)。`date` はサーバーのローカル時刻 |
| `mktime($h, $i, $s, $m, $d, $y)`, `strtotime($s): ?int`, `checkdate($m, $d, $y)` | `strtotime` は ISO 形式と `now`/`today`/`tomorrow`/`yesterday` |
| `random_int($min, $max)`, `random_token($bytes = 32)` | OS の暗号論的乱数(`rand()` は暗号用途に使わない) |
| `password_hash($pw)`, `password_verify($pw, $hash)` | Argon2id |
| `hash("sha256" \| "sha384" \| "sha512", $data)`, `hash_hmac($algo, $data, $key)`, `hash_equals($a, $b)` | `hash_equals` は比較時間が内容に依存しない |
| `totp_secret()`, `totp_code($secret, ?$time)`, `totp_verify($secret, $code): ?int` | 認証アプリの 2 段階認証コード(TOTP、RFC 6238: HMAC-SHA1、6 桁、30 秒)。秘密鍵は 160 ビットの乱数を Base32 にしたもの。`totp_verify` は前後 1 ステップの時刻のずれを許し、比較時間は内容に依存せず、一致した時刻ステップ(なければ `null`)を返す。これを保存して、それより新しくないステップを拒めば、同じコードを二度使えない |
| `qr_svg($text): Html` | QR コードを `<svg>` 要素で返す(`otpauth://` の URI など)。中身は模様だけで文字列そのものは含まないので、そのまま出力してよい |
| `base64_encode`, `base64_decode: ?string`, `urlencode`, `rawurlencode`, `urldecode`, `http_build_query` | |
| `json_decode($s): any` | 深さ 128 まで |

### Content-Security-Policy

ページのレスポンスには既定で次のポリシーが付く(`{nonce}` はレスポンスごとの `'nonce-...'`。`'nonce-{nonce}'` と書いても同じ)。

```
default-src 'self'; script-src 'self' {nonce}; style-src 'self' 'unsafe-inline';
img-src 'self' data: https:; font-src 'self' data:; object-src 'none'; base-uri 'self';
form-action 'self'; frame-ancestors 'self'
```

- テンプレート(`.phx.html` と `.phx` の HTML 部分)に書いた `<script>` タグには、PHX が自動で
  `nonce="..."` を付ける。そのため、自分で書いたインラインスクリプトはそのまま動き、エスケープをすり抜けて
  注入されたスクリプト(`raw()` に入った `<script>` など)は動かない。
- `onclick="..."` のようなイベント属性と `javascript:` の URL は止まる。`addEventListener` を使う。
  テンプレートに書くとコンパイル時に警告が出る(`csp` でインラインのコードを許可しているときは出ない)。
- 文字列から組み立てた `<script>` に nonce が必要なときは `csp_nonce()` を使う
  (`raw("<script nonce=\"" . csp_nonce() . "\">...</script>")`)。
- 静的ファイルとエラーページには nonce のないポリシーが付く(静的な `.html` のインラインスクリプトは動かない)。
- 変更: `[security] csp = "off"` で付けない。`csp = "default-src 'self'; script-src 'self' {nonce}"` のように
  書くとそのポリシーを使う(`{nonce}` は `'nonce-...'` になる)。

### レート制限

```phx
if (!rate_limit("login:" . request()->ip, 5, 60)) {     // 60 秒に 5 回まで
    response()->status(429);
    echo "しばらく待ってからやり直してください。";
    return;
}
// ログインに成功したら
rate_limit_reset("login:" . request()->ip);
```

- `rate_limit($key, $max, $seconds)` は、そのキーの今の時間枠でまだ `$max` 回に達していなければ 1 回数えて `true`、
  達していれば `false` を返す。カウンターはサーバープロセスの中で共有され、再起動で消える。
- 同じキーでも時間枠の長さごとに別に数える(1 分 5 回と 1 日 50 回を両方かけられる)。キーが 10 万個を超えると、
  上限に達していないものから捨てるので、大量のキーを使っても制限中のキーは外れない。
- `requests_per_minute` は IPv6 のアドレスを /64 ごとにまとめて数える。
- サイト全体の制限は `[security] requests_per_minute`(§14)。

### 権限(phx.toml)

スクリプトがヒープの外に触れるものは、すべて `phx.toml` で許可したものだけ。**何も書かなければ何も許可されない。**

```toml
[permissions]
read = ["content/"]         # file_get_contents / file_exists で読めるディレクトリ
write = ["data/", "storage/"]  # file_put_contents / unlink / saveTo / Database::open(読み取りも可)
env = ["APP_*"]             # getenv で読める環境変数(末尾 * は前方一致)
```

- パスはプロジェクトからの相対パスで、`..` と絶対パスは使えない。
- 許可したディレクトリの下にあるシンボリックリンクとジャンクションはたどらない(リンクを経由するパスは `PermissionError`)。
  パスは許可したディレクトリから 1 段ずつ、リンクを拒否しながら開くので、検査と使用の間にディレクトリを
  すり替えられることもない。
- 開けるのは通常のファイルで、ハードリンクが 1 本だけのもの。外のファイルへのハードリンクを置かれても読み書きしない。
- ファイル名に `:`(Windows の代替データストリーム)、末尾のドットや空白、`CON` `NUL` `COM1` などのデバイス名は使えない。
- Unix では、ほかのユーザーが書き込めるディレクトリ(`o+w`)は使えない。
- コードとして読み込まれる場所は `write` に指定できない: `public/`、プロジェクトのルート、`.` で始まるディレクトリ
  (`.phx-cache/`、`.git/` など)、`vendor/`、`[dependencies]` のパッケージのディレクトリ。
- 許可されていないときは `PermissionError`(メッセージに `phx.toml` に書くべき内容が出る)。

### phx.toml

```toml
[project]
name = "blog"

[security]
csrf = true             # 既定: true
secure_cookies = false  # HTTPS で配信するときは true
session_ttl = 7200      # 秒
memory_limit_mb = 128   # 1 リクエスト(1 回の実行)あたり
csp = "default"         # "off" か、{nonce} を含められる独自のポリシー
requests_per_minute = 0 # 接続元アドレスごとの上限(0 は無制限)
session_store = "memory"  # またはディレクトリ("sessions/")

[permissions]
write = ["data/"]

[dependencies]
utils = { path = "../utils" }
markdown = { git = "https://example.com/markdown.git", rev = "v1.2.0" }
```

`phx.toml` のあるディレクトリがプロジェクトのルートになる。知らないキーはエラー(書き間違いに気づける)。
`phx serve` は起動時に読み込むので、変更したらサーバーを再起動する。

## 15. 組み込み関数

### グローバル関数

| 関数 | 説明 |
|---|---|
| `count(list\|map): int` | 要素数 |
| `strlen(string): int` | バイト数(文字数は `->len()`) |
| `implode(string $sep, list<string>): string` / `explode(string $sep, string): list<string>` | 結合 / 分割 |
| `str_repeat`, `str_contains`, `str_starts_with`, `str_ends_with`, `str_replace(search, replace, subject)` | PHP と同じ |
| `strtoupper`, `strtolower`, `ucfirst`, `trim` | PHP と同じ |
| `json_encode(any): string` | オブジェクトは public プロパティだけを出力 |
| `dump(any): void` | `var_dump` に相当 |
| `raw(string): Html`, `escape(string): Html`, `htmlspecialchars(string): Html`, `nl2br(string): Html` | §13 |
| `intdiv`, `abs`, `min`, `max`, `floor`, `ceil`, `round`, `sqrt`, `fmod` | 数値 |
| `number_format(float, int $decimals = 0): string` | `1,234.50` |
| `range(int, int): list<int>` | 両端を含む |
| `time(): int`, `microtime(): float` | 現在時刻 |
| `rand(int, int): int` | 疑似乱数(暗号用途には使わない。`random_int` を使う) |
| `json_decode(string): any`, `json_html(any): SafeJs`, `safe_url(string): SafeUrl` | §13、§14 |
| `csrf_token(): string`, `csrf_field(): Html` | §14 |
| `csp_nonce(): string` | このレスポンスの CSP の nonce(§14) |
| `rate_limit(string, int, int): bool`, `rate_limit_reset(string)` | §14 |
| `date`, `gmdate`, `mktime`, `strtotime`, `checkdate` | 日付(§14 の表) |
| `random_int`, `random_token`, `password_hash`, `password_verify`, `hash`, `hash_hmac`, `hash_equals`, `totp_secret`, `totp_code`, `totp_verify`, `qr_svg` | 暗号と 2 段階認証(§14 の表) |
| `base64_encode`, `base64_decode`, `urlencode`, `rawurlencode`, `urldecode`, `http_build_query` | エンコード |
| `file_get_contents`, `file_put_contents`, `file_exists`, `unlink`, `getenv` | `[permissions]` が必要 |
| `sql_ident(string): Sql` | 列名・テーブル名 |
| `assert(bool, ?string)`, `assert_eq(any, any, ?string)` | テスト用 |

### string のメソッド

`len()`(文字数)、`isEmpty()`、`upper()`、`lower()`、`ucfirst()`、`trim()`、`ltrim()`、`rtrim()`、
`contains($s)`、`startsWith($s)`、`endsWith($s)`、`replace($search, $replace)`、`split($sep)`、
`slice($start, $length?)`(文字単位、負の値は末尾から)、`indexOf($s): ?int`、`repeat($n)`、
`padStart($len, $pad)`、`padEnd($len, $pad)`、`toInt(): ?int`、`toFloat(): ?float`、`chars()`

### list のメソッド

`len()`、`isEmpty()`、`contains($v)`、`indexOf($v): ?int`、`first(): ?T`、`last(): ?T`、
`slice($start, $length?)`、`reverse()`、`concat($other)`、`unique()`、`sorted()`、`sortBy(fn($a, $b) => int)`、
`map(fn($x) => ...)`、`filter(fn($x) => bool)`、`find(fn($x) => bool): ?T`、`any(...)`、`all(...)`、
`join($sep)`、`sum()`

要素の追加は `$xs[] = $v;`、更新は `$xs[$i] = $v;`(範囲外は `IndexError`)。

### map のメソッド

`len()`、`isEmpty()`、`has($k)`、`get($k): ?V`、`keys()`、`values()`、`merge($other)`

追加・更新は `$m[$k] = $v;`。存在しないキーの読み取り `$m[$k]` は `KeyError`、`$m[$k] ?? $default` は例外にならない。

PHP の関数名で書いたときは、エラーメッセージで代わりの書き方を案内する
(例: `strpos` → `->indexOf()`、`isset` → `??`、`var_dump` → `dump()`、`eval` → 使えない)。

## 16. ツール

| コマンド | 説明 |
|---|---|
| `phx run file.phx` | 型検査して実行(`--html`、`--time-limit 秒`) |
| `phx check file.phx` | 型検査だけ(import しているモジュールとテンプレートも) |
| `phx serve dir` | Web サーバー(`--port`、`--host`、`--prod`、`--time-limit`) |
| `phx test [paths]` | `*_test.phx` の `test_*` 関数を実行(1 テストごとに新しい VM) |
| `phx fmt [paths] [--check]` | インデントと空白を整える(文字列・コメント・HTML は変えない) |
| `phx lsp` | エディタ用の Language Server(入力中のエラー表示、ホバーで型を表示) |
| `phx install` | `[dependencies]` を取得して `phx.lock` に固定 |
| `phx migrate file.php` | PHP を機械的に変換し、手作業が必要な箇所を一覧にする |
| `phx disasm file.phx` | バイトコードを表示 |
| `phx parse file.phx` | 構文木を表示 |

### テスト

```phx
// tests/posts_test.phx
import lib.posts.find_post;

function test_find_post(): void {
    assert_eq(find_post(2)?->title, "Types without ceremony");
    assert(find_post(99) == null, "missing posts are null");
}
```

`assert(bool, ?message)` と `assert_eq(expected, actual, ?message)` が失敗すると `AssertionError`。

### パッケージ

```toml
[dependencies]
textutils = { git = "https://github.com/example/textutils.git", rev = "v1" }
local = { path = "../shared" }
```

```phx
import textutils.strings.slugify;      // vendor/textutils/strings.phx
```

- `git` のパッケージは `vendor/<名前>` に取得され、正確なコミットが `phx.lock` に記録される。
  2 回目以降は lock のコミットを取得し、違っていればエラーになる。
- 使える URL は `https://`、`ssh://`、`git@host:...`、`file://` だけ。
- パッケージは PHX のソースコードで、インストール時にパッケージのコードが実行されることはない。
- パッケージの依存はアプリの `phx.toml` に並べて書く(推移的な依存の解決はまだない)。

### 移行

```
phx migrate legacy/page.php      # page.phx を書き出す
```

`<?php`→`<?phx`、`===`→`==`、`array(...)`→`[...]`、`isset($x)`→`($x != null)`、`$_GET`/`$_POST`/`$_COOKIE`/`$_SESSION`
→ `request()`/`session()`、`var_dump`/`print_r`→`dump`、`print`→`echo` を変換し、`require` や `global` には
`TODO(migrate)` コメントを付ける。そのあと型検査を実行して、型注釈などの手作業が必要な箇所を表示する。

## 17. 実行の仕組み

1. 字句解析・構文解析(手書きの再帰下降 + Pratt パーサ)
2. 型検査(名前解決、推論、絞り込み、確定代入の解析)→ 型付き中間表現 (HIR)
3. レジスタ型バイトコードへのコンパイル(`AddI`、`LtI`、`JumpIfNotLtIImm` などの型に特化した命令)
4. 実行
   - **インタプリタ**: 安全な Rust だけで書かれている。文字列・list・map は参照カウント + コピーオンライト。
   - **JIT**(Cranelift): すべての値が `int`/`bool` の関数(数値計算のループや再帰)はネイティブコードになる。
     オーバーフロー、0 での剰余、負のシフト、再帰の深さ、実行時間・メモリの上限は JIT でも同じように検査される。
     `PHX_JIT=0` で無効にできる。JIT の中で起きた例外のスタックトレースは呼び出し元までになる。
5. **バイトコードキャッシュ**: `phx run` と `phx serve` はコンパイル結果を `.phx-cache/` に保存し、
   ソースファイルと `phx` 本体が変わっていなければ再利用する。
   - キャッシュはユーザーごとの鍵(`%LOCALAPPDATA%\phx\cache.key`、`~/.cache/phx/cache.key`)の HMAC で
     署名し、署名が合わないものは読まない。ほかの人から受け取ったプロジェクトに `.phx-cache/` が入っていても、
     ソースと違うコードが動くことはない。
   - エントリはスクリプトとプロジェクトごと。依存するすべてのファイルの内容(SHA-256)を記録し、読み込むときに
     確かめる(更新時刻を保つコピーや、同じ長さの書き換えでも古いコードは使われない)。
   - `.phx-cache/` がリンクなら使わない。中のファイルも、リンクやハードリンクなら読み書きしない。

### 実行時の上限

| 上限 | 既定 | 超えたとき |
|---|---|---|
| 実行時間 | `serve`: 30 秒(`--time-limit`)、`run`: なし | 致命的エラー(捕捉できない) |
| メモリ | 128 MB(`[security] memory_limit_mb`) | 致命的エラー |
| 呼び出しの深さ | 10,000 | 致命的エラー |
| リクエスト本文 | 8 MB | 413 |

`unsafe` を使っているのは、メモリ使用量を数えるアロケータ(`phx_vm/src/alloc.rs`)と JIT
(`phx_jit`、生成したコードの呼び出し)だけ。

## 18. 今後の予定

- ジェネリッククラス、アノテーション(属性)
- 非同期 I/O、HTTP クライアント(`[permissions].net`)
- JIT の対象拡大(float・文字列を扱う関数)、AOT コンパイル
- 推移的な依存関係の解決、パッケージレジストリ
- LSP の定義ジャンプ・補完、`phx fmt` の行の折り返し
- 複数プロセスで共有できるレート制限、データベースに保存するセッション

セキュリティ監査の結果と残っているリスクは [security-audit.ja.md](security-audit.ja.md) を参照。
