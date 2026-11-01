# Phenix 言語仕様 (v0.1)

Phenix は、PHP の「保存してすぐ動く」「Web に特化」「HTML に埋め込める」という良さを引き継ぎながら、
型安全性・セキュリティ・実行速度・保守性を言語の側で保証する Web 向けの言語です。
既存の PHP コードとの互換性はありませんが、PHP を書いてきた人がすぐ読めて書けることを目指しています。

## 1. 設計方針

| PHP の弱点 | Phenix での解決 |
|---|---|
| 暗黙の型変換 (`"1" + 1`、`"abc" == 0`) | 暗黙の変換はしない。`==` は常に厳密比較。違う型どうしの比較はコンパイルエラー |
| null による実行時エラー | `?T` 型。null チェック (`!= null`、`is`、`??`、`?->`) をしないと使えない |
| XSS | `<?= ?>` と Web ページへの `echo` は自動で HTML エスケープ。エスケープしないのは `Html` 型の値だけ |
| `eval`、可変変数、`extract`、動的な `include` | 言語に存在しない。モジュールは静的な `import`、テンプレートはリテラルのパスだけ |
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
  throw import export use is instanceof`

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
絞り込みはローカル変数だけが対象(プロパティは対象外。ローカル変数に入れてから使う)。

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

投げられるのは `Exception` かそのサブクラスだけ。`finally` は未対応(今後対応)。

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
- 可視性は `public` / `protected` / `private`(コンパイル時にチェック)。
- 単一継承。メソッドのオーバーライドは引数の型が同じで、戻り値の型が互換であること。親のコンストラクタを持つ
  クラスで初期化の必要なプロパティがある場合、子のコンストラクタは `parent::__construct(...)` を呼ぶ必要がある。
- `static` メソッドは `ClassName::method()` で呼ぶ。**static プロパティはない**(リクエスト間で状態を共有させないため)。
- オブジェクトは参照、`list` と `map` は値(コピーオンライト)。
- 今後の予定: interface、abstract、enum、ジェネリクス。

## 10. 例外クラス

すべてのファイルから使える。

```
Exception                    (getMessage(): string)
├── RuntimeException
├── LogicException
│   └── InvalidArgumentException
├── ArithmeticError          整数のオーバーフローなど
│   └── DivisionByZeroError  0 での割り算・剰余
├── IndexError               list の範囲外アクセス
├── KeyError                 map にないキー
├── ValueError               不正な引数(ヘッダーの改行など)
└── UnhandledMatchError
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

- `import a.b.{x, y};` は、プロジェクトのルート(`phenix.toml` のあるディレクトリ、なければ実行するファイルの
  ディレクトリ)からの `a/b.phx` を読み込む。
- import されるモジュールのトップレベルには宣言(`function`、`class`、`import`)しか書けない。
  モジュールを読み込んでも副作用は起きない。
- `std.*` は標準モジュール(現在は `std.http`)。

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
- `render()` のパスは文字列リテラルで、プロジェクトのルートからの相対パス(`..` は使えない)。
- `render()` は `Html` を返すので、別のテンプレートにそのまま渡して埋め込める(レイアウト)。

## 13. 出力とエスケープ

出力先には「テキスト」と「HTML」の 2 種類がある。

| 状況 | 出力先 |
|---|---|
| `phenix run` で実行したスクリプト | テキスト(`--html` を付けると HTML) |
| Web サーバーが実行するページ | HTML(`response()->type("application/json")` などで HTML 以外にするとテキスト) |
| テンプレートの中 | 常に HTML |

HTML の出力先に `string` を書くと `& < > " '` がエスケープされる。`Html` 型の値と、テンプレート中の
HTML 部分はそのまま出力される。`echo` と `<?= ?>` は同じ規則に従う。

- `escape($s)` / `htmlspecialchars($s)` … エスケープ済みの `Html` を返す。
- `raw($s)` … 信頼できるマークアップを `Html` として扱う(XSS に注意して使う)。
- `nl2br($s)` … エスケープしてから改行を `<br>` にした `Html`。
- `Html` と `string` は `.` で連結できない(エラーで `escape()` を案内する)。

## 14. Web サーバー

```
phenix serve examples/site --port 8080
```

- サイトのディレクトリには `public/` が必要。URL はファイル名で決まる:
  `/` → `public/index.phx`、`/about` → `public/about.phx` または `public/about/index.phx`、`/about.phx` も可。
  `.phx` 以外のファイルは静的ファイルとして配信される。
- 配信しないもの: `public/` の外、`.` で始まるファイルやディレクトリ、`.phx.html`(テンプレート)。
- リクエストごとに新しい VM(新しいヒープ)で実行する。コンパイル済みのバイトコードだけが共有される。
- ファイルを保存すると次のリクエストで再コンパイルされる(依存する全ファイルの更新時刻を見る)。
- 1 リクエストの実行時間の上限(既定 30 秒、`--time-limit`)、リクエスト本文の上限(8 MB)。
- 既定で付けるヘッダー: `X-Content-Type-Options: nosniff`、`X-Frame-Options: SAMEORIGIN`、
  `Referrer-Policy: strict-origin-when-cross-origin`。`Server` / `X-Powered-By` のようなバージョン情報は出さない。
- 開発モード(既定)ではコンパイルエラーと実行時エラーの詳細をブラウザに表示する。`--prod` では表示せず、
  ログにだけ出す。

### std.http

```phx
import std.http.{request, response};

$req = request();
$req->method;              // "GET"
$req->path;                // "/post"
$req->query("id");         // ?string
$req->form("name");        // ?string (application/x-www-form-urlencoded)
$req->header("Accept");    // ?string(名前の大文字小文字は区別しない)
$req->body;                // string
$req->isPost();            // bool

response()->status(404);
response()->header("Cache-Control", "no-store");
response()->type("text/plain; charset=utf-8");
response()->redirect("/login");
response()->json($value);  // Content-Type を JSON にして json_encode で出力
```

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
| `rand(int, int): int` | 疑似乱数(暗号用途には使わない) |

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

## 16. 実行モデル

1. 字句解析・構文解析(手書きの再帰下降 + Pratt パーサ)
2. 型検査(名前解決、型推論、絞り込み、確定代入の解析)→ 型付き中間表現 (HIR)
3. バイトコードへのコンパイル: レジスタ型。ローカル変数は固定のレジスタ、一時値はその上に積む。
   型が分かっているので `AddI`(整数の加算)、`AddF`、`LtI` のような型ごとの命令や、
   `JumpIfNotLtIImm`(即値との比較と分岐)のような融合命令を出す。
4. VM で実行: 値は小さなタグ付き enum(`int`/`float`/`bool`/`null` は即値、文字列・配列は参照カウント +
   コピーオンライト、オブジェクトは参照)。処理系は Rust の安全なコードだけで書かれている。

`phenix disasm file.phx` でバイトコードを確認できる。

## 17. 今後の予定

- JIT / AOT コンパイル(Cranelift)、バイトコードのディスクキャッシュ
- データベース接続と、プレースホルダ以外を受け付けない `sql"..."` リテラル
- ファイル・ネットワーク・環境変数へのアクセスを `phenix.toml` で宣言する権限モデル
- セッション、Cookie、CSRF 対策、メモリ使用量の上限
- `finally`、interface、abstract、enum、ユーザー定義のジェネリクス
- 非同期 I/O
- フォーマッタ (`phenix fmt`)、テストランナー (`phenix test`)、LSP、パッケージマネージャ
- PHP からの移植支援ツール
