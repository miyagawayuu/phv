# PHV 言語仕様

[English](spec.md) | 日本語

公式サイト: <https://phv-lang.org>

PHV は、PHP の「保存してすぐ動く」「Web に特化」「HTML に埋め込める」という良さを引き継ぎながら、
型安全性・セキュリティ・実行速度・保守性を言語の側で保証する Web 向けの言語です。
既存の PHP コードとの互換性はありませんが、PHP を書いてきた人がすぐ読めて書けることを目指しています。

## 1. 設計方針

| PHP の弱点 | PHV での解決 |
|---|---|
| 暗黙の型変換 (`"1" + 1`、`"abc" == 0`) | 暗黙の変換はしない。`==` は常に厳密比較。違う型どうしの比較はコンパイルエラー |
| null による実行時エラー | `?T` 型。null チェック (`!= null`、`is`、`??`、`?->`) をしないと使えない |
| XSS | `<?= ?>` と Web ページへの `echo` は自動で HTML エスケープ。エスケープしないのは `Html` 型の値だけ |
| `eval`、可変変数、`extract`、動的な `include` | 言語に存在しない。モジュールは静的な `import`、テンプレートはリテラルのパスだけ |
| SQL インジェクション | SQL は `sql"..."` リテラル(`Sql` 型)でしか書けず、埋め込んだ値は必ずプレースホルダになる |
| 文脈を無視したエスケープ | `<?= ?>` の位置(属性・URL・`<script>`・CSS)を判定し、エスケープでは守れない場所への挿入はコンパイルエラー |
| CSRF・セッション固定 | フォームの POST はサーバーがトークンを自動で検査。セッション ID はサーバーが発行したものだけを受け付ける |
| 何でもできるスクリプト | ファイル・環境変数へのアクセスは `phv.toml` の `[permissions]` で許可したものだけ |
| 注入されたスクリプトの実行 | 既定の Content-Security-Policy で、テンプレートに書いた `<script>` 以外のインラインスクリプトを止める |
| ヘッダーインジェクション | 改行を含むヘッダー値は `ValueError` |
| パストラバーサル | サーバーは `public/` の外、ドットファイル、テンプレートを配信しない |
| リクエスト間で状態が漏れる | リクエストごとに新しいヒープ。静的プロパティもグローバル変数もない |
| 実行速度 | 静的型に特化したレジスタ型バイトコード VM |
| 大規模化すると保守しにくい | 型注釈の付いた関数、モジュールと `export`、読みやすいエラーメッセージ |

## 2. ファイル

- `.phv` … スクリプトまたはモジュール。**コードモード**で始まる。先頭の `<?phv` は書いても書かなくてもよい。
- `.phv.html` … テンプレート。**HTML モード**で始まる。
- コードモードで `?>` を書くと HTML モードに切り替わる。HTML モードでは `<?phv`(または `<? `)でコードに、
  `<?= 式 ?>` で値の出力に戻る。`?>` の直後の改行 1 つは出力されない(PHP と同じ)。
- 文字コードは UTF-8。

## 3. 字句

- 変数は `$name`。`$$name`(可変変数)はない。
- コメントは `// ...`、`# ...`、`/* ... */`。`#[` はコメントではなくアノテーションの始まり(§9)。
- 整数: `42`、`1_000_000`、`0xFF`、`0b1010`、`0o17`。64 ビット。オーバーフローは `ArithmeticError`。
- 浮動小数点数: `1.5`、`1e-3`。
- 文字列:
  - `'...'` … 展開なし。エスケープは `\'` と `\\` だけ。
  - `"..."` … `$name` と `{$式}` を展開する。エスケープは `\n \t \r \0 \e \\ \" \$ \{ \x41 \u{1F600}`
    (`\x` は 1〜2 桁の 16 進数で `\x7f` まで。ほかの文字は `\u{...}` で書く)。
  - 複数行の文字列にはヒアドキュメントと Nowdoc を使う:

    ```phv
    $page = <<<HTML
        <h1>{$title}</h1>
        <p>Hello, $name</p>
        HTML;
    $raw = <<<'TEXT'
        No $interpolation or \escapes here.
        TEXT;
    ```

    `<<<NAME`(または `<<<"NAME"`)は `"..."` と同じ、`<<<'NAME'`(Nowdoc)はエスケープもない `'...'` と同じ。
    文字列は次の行から始まり、`NAME` だけの行の前で終わる。その終わりの行のインデントがすべての行から取り除かれ、
    それより浅くインデントした行はエラー。
- 予約語: `function fn return if elseif else endif while endwhile do for endfor foreach endforeach as break continue
  echo class extends new public private protected readonly static true false null match default try catch finally
  throw import export use is instanceof interface implements abstract final enum case const`。`type` は型エイリアスの
  先頭(§4)でだけキーワードになる。

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
| `set<T>` | 重複しない値の集まり。加えた順を保つ。`T` は `int` か `string` |
| `?T` | `T` または `null`(`T|null` と同じ) |
| `A|B` | ユニオン型 |
| `fn(A, B): R` | 関数(クロージャ)の型 |
| クラス名 | オブジェクトの型。`self` はそのクラス自身 |

PHP の `array` は `list<T>` と `map<K, V>` に分かれています。重複しない値の集まりには `set<T>` を使います。`mixed` は `any` です。

### 型エイリアス

```phv
type UserId = int;
type Row<T> = map<string, T>;
export type Handler = fn(Request): Html;

function find(UserId $id): ?Row<string> { ... }
```

型エイリアスは型の別名で、`UserId` と `int` は同じ型。ファイルのトップレベルで宣言し、型パラメータを取れる。
クラスと同じように export / import でき、自分自身を含むことはできない(再帰的なデータにはクラスを使う)。

### 代入できる条件

- 同じ型、サブクラスから親クラス、`T` から `?T` やそれを含むユニオン、任意の型から `any`。
- **`int` は `float` が期待される場所では自動で `float` に変換される**(唯一の暗黙の変換。値の意味が変わらないため)。
  `list<int>` を `list<float>` に渡すことはできない(要素の型は不変)。
- それ以外の変換は明示的に書く: `(int)$float`、`(float)$int`、`(string)$x`、`$s->toInt()`、`$s->toFloat()`。
  `(int)"12"` のような文字列からのキャストはできない(`->toInt()` は数値でなければ `null` を返す)。
  `(bool)` キャストはない。

## 5. 変数

- PHP と同じく、**最初の代入で変数が宣言される**。そのとき値の型が変数の型になり、あとから別の型は代入できない。

  ```phv
  $count = 1;
  $count = "two";   // エラー: expected `int`, found `string`
  ```

- 型を推論できないとき(`null` や空の `[]`)は型を書いて宣言する。

  ```phv
  ?User $found = null;
  list<string> $names = [];
  map<string, int> $counts = [];
  ```

- 変数のスコープは関数全体(PHP と同じ)。ただし **一部の経路でしか代入されない変数は使えない**。

  ```phv
  if ($ok) { $msg = "hi"; }
  echo $msg;   // エラー: variable `$msg` might not be defined here
  ```

- `foreach` と `catch` の変数は、同じ名前の既存の変数と型が違うとき、新しい変数としてそれを隠す
  (`catch (A $e)` と `catch (B $e)` を続けて書ける)。
- `$this` は非 static メソッドの中だけで使える。代入はできない。

### 分割代入

```phv
[$a, $b] = [1, 2];
[$a, $b] = [$b, $a];                       // 入れ替え
[$first, ...$rest] = $names;               // $rest = $names->slice(1)
["id" => $id, "name" => $name] = $row;     // map はキーで取り出す
[[$x, $y], $z] = [[1, 2], 3];
[$this->x, $list[0]] = $pair;              // 代入できる場所ならどこでも
foreach ($pairs as [$key, $value]) { ... }
foreach ($rows as $i => ["id" => $id]) { ... }
```

右辺を 1 回だけ評価し、各要素を代入する。list は `$v[0]`、`$v[1]`…、map は `$v["key"]`。要素がなければ、
添字と同じく `IndexError` / `KeyError` が投げられる。それぞれの代入は通常の代入の規則に従うので、`list<int|string>`
の要素は `int|string` になる。パターンに代入できるのは `=` だけ。

### 型の絞り込み (narrowing)

条件によって、ローカル変数の型が狭められる。

```phv
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
`try` の後では、その `finally` が代入するもの、`finally` の中の呼び出しが変えたかもしれないものは、`try` の中で
わかったことでは絞り込まれない。`for` ループのステップと `do`-`while` ループの条件では、本体の終わりと各 `continue`
の両方で成り立つことだけがわかっている。

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

```phv
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

比較は `==`(厳密)。どの腕にも当てはまらず `default` もないときは `UnhandledMatchError`。

enum か `bool` に対する `match` で `default` がなければ、すべての値の腕が必要(対象が null になりうるときは `null`
の腕も)。足りなければコンパイルエラーになるので、enum にケースを加えると直すべき `match` がすべて分かる。

```phv
$color = match ($suit) {
    Suit::Hearts, Suit::Diamonds => "red",
    Suit::Spades, Suit::Clubs => "black",
};
```

`match (true)` では、`if` / `elseif` と同じく、各腕はその条件で分かったことを使え、後の腕は前の条件が偽だったことを
使える:

```phv
$area = match (true) {
    $shape is Circle => 3.14 * $shape->r ** 2,
    $shape is Square => $shape->side ** 2,
    default => 0.0,
};
```

### 例外

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
    $lock->release();      // return / break / continue / 例外のどれで抜けても実行される
}
```

入れ子の `finally` は内側から実行される。`finally` の中の `return`、`break`、`continue`、`throw` は、そのブロックに
入ったときの抜け方(返す値や例外)に取って代わる。そこでの `break` は `return` の後でもループを抜けるので、そういう
`break` のある `while (true)` のようなループは終わりうる。

投げられるのは `Exception` かそのサブクラスだけ。例外を使わない `catch` は変数を省略できる:
`catch (ValueError) { ... }`。

例外はコードと、原因になった例外を持てる:

```phv
try {
    $port = parse_port($raw);
} catch (ValueError $e) {
    throw new ConfigError("bad port", previous: $e);    // または new ConfigError("bad port", 2, $e)
}
// $e->getCode(): int、$e->getPrevious(): ?Exception
// $e->getTraceAsString(): "at parse_port (lib/config.phv:12)\nat load (lib/config.phv:20)\n..."
```

`getTraceAsString()` は、例外が最初に投げられたときの呼び出しを内側から順に返す(投げられていない例外では空)。
実行環境は投げられた場所を覚えておき、文字列は `getTraceAsString()` が初めて求めたときに作る。読まずに捕捉する例外は
安く済む。
深いトレースは内側と外側の 50 個ずつの呼び出しを残し、名前は 200 文字までにする。
捕まえられなかった例外は原因も表示する: `Uncaught ConfigError: bad port (caused by ValueError: ...)`。

## 8. 関数

```phv
function greet(string $name, int $times = 1): string { ... }
greet("Ann");
greet(name: "Bob", times: 3);       // 名前付き引数
```

- 引数の型は必須。戻り値の型を省略すると `void`。
- 値を返す関数は、すべての経路で `return` しないとエラー。
- デフォルト値は定数式(§9 の「定数」)、`null`、`[]` だけ。
- 型パラメータを持つジェネリック関数を書ける(§9 の「ジェネリック関数」)。

### 可変長引数と展開

```phv
function sum(int ...$xs): int { ... }    // $xs は list<int>
sum();                     // $xs = []
sum(1, 2, 3);
sum(...$numbers);          // list<int> を $xs に展開する
sum(1, ...$more, 4);
$all = [0, ...$a, ...$b];  // list リテラルの中でつなぐ
```

可変長引数は最後の引数で、残りの位置引数を list として受け取り、デフォルト値は持てない。`...` で list の要素を
渡せるのは可変長引数だけ(list の要素数は検査のときには分からないので、決まった数の引数は埋められない)。

### 関数を値として使う

```phv
$len = strlen(...);                    // fn(string): int
$names->map(strtoupper(...));
$greet = $greeter->greet(...);         // メソッド。$greeter はこの時点で評価される
$items->map(Item::fromRow(...));       // static メソッド
```

`name(...)` は、その関数・メソッド・static メソッドを自分の引数で呼ぶクロージャを作る。ジェネリック関数、可変長引数を
持つ関数、シグネチャが複数ある組み込み関数(期待される関数型で 1 つに決まる場合を除く)、`string`・`list`・`map`
のメソッドはこの形では使えないので、クロージャを書く。

### クロージャ

```phv
$double = fn(int $x): int => $x * 2;           // 外側の変数を自動でキャプチャ
$names->map(fn($n) => $n->upper());            // 引数の型は文脈から推論される
$counter = function () use ($start): int { return $start + 1; };
```

- キャプチャはすべて**値渡し**(作られた時点のコピー)。参照キャプチャ(`&$x`)はない。
- メソッドの中で作ったクロージャは `$this` を使える。
- `fn() =>` の本体はできるだけ長く続く: `fn($n) => $n > 0 ? "+" : "-"` は条件式全体を返す。
- 関数型(`fn(int): int`)にはデフォルト値も可変長引数もないので、クロージャの引数はどちらも持てない。

## 9. クラス

```phv
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
- メソッドはキーワードと同じ名前(`match`、`list`、`default`)にできる。常に `->` か `::` の後で呼ぶため。
### interface と abstract

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

- interface はメソッドのシグネチャと定数だけを持つ。`extends` で複数の interface を継承できる。
- `abstract` メソッドは本体を持たない。abstract メソッドを持つクラスは `abstract class` と宣言する。
- 具象クラスが interface や abstract のメソッドを実装していない、またはシグネチャが合わないときはコンパイルエラー。
- interface、abstract クラス、enum は `new` できない。
- `final class` は継承できず、`final` のメソッドとコンストラクタはオーバーライドできない(`private` メソッドは
  継承されないので、サブクラスは同じ名前のメソッドを宣言できる)。`final` は `abstract` と一緒に使えず、
  interface、interface のメソッド、enum、プロパティ、定数にも付けられない(コンパイルエラー)。
- interface 型の値へのメソッド呼び出しは名前で振り分けられる。クラス型の値へは vtable で振り分けられる。

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

```phv
const MAX_ITEMS = 50;            // ファイルの先頭レベル(export / import もできる)
class Config {
    const VERSION = "1.0";
}
echo MAX_ITEMS, Config::VERSION, self::VERSION;
```

定数の値は定数式: リテラル(int、float、string、bool、null)、同じファイルでそれより前に宣言した定数
(とクラス定数 `self::X`、`Other::Y`)、それらへの演算子で、検査のときに計算される。デフォルト値にも同じ式を書ける。

```phv
const MINUTE = 60;
const DAY = 24 * 60 * MINUTE;
const MASK = (1 << 8) - 1;
const PREFIX = "v" . 2 . "/";
function wait(int $seconds = 5 * MINUTE): void { ... }
```

オーバーフロー、0 での除算、型の合わない演算子はコンパイルエラー。定数の中では float を文字列につなげない。

### ジェネリック関数

```phv
function first_or<T>(list<T> $xs, T $fallback): T {
    return $xs->isEmpty() ? $fallback : $xs[0];
}
$n = first_or([5, 6], 0);        // int
$s = first_or([], "none");       // string
```

型引数は呼び出しの引数から推論される(最大 4 個)。関数の中では `T` は中身の分からない型として扱われ、
`T` の値に演算子やメソッドは使えない。ただし制約を付けると使える:

```phv
interface Shape {
    public function area(): float;
}
function largest<T: Shape>(list<T> $shapes): ?T {
    ?T $best = null;
    foreach ($shapes as $s) {
        if ($best == null || $s->area() > $best->area()) {   // Shape のメソッド
            $best = $s;
        }
    }
    return $best;
}
class Registry<T: Shape> { ... }
```

`T: Bound` と書くと、`T` の値は `Bound` のメソッドとプロパティを持ち、`Bound` を受け取る場所に渡せる。型引数は
すべて `Bound` でなければならない: `largest([new Circle()])` は `?Circle` になり、`largest([1, 2])` はコンパイルエラー。
型引数は変換なしで制約を満たす必要があり(`int` は `T: float` を満たさない)、制約に別の型パラメータは書けない。
オーバーライドするメソッドは制約を外せるが、加えたり変えたりはできない。型エイリアスの型パラメータには制約がないので、
エイリアスは自分の型パラメータを制約付きのクラスに渡せない。

### ジェネリッククラス

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

$b = new Box(41);                 // Box<int>。コンストラクタから推論
$s = new Box<string>("x");        // 明示
Box<float> $f = new Box(1);       // 宣言した型から: Box<float>
```

- クラスとインターフェースは型パラメーター(最大 4 個)を持てる。プロパティと、インスタンスメソッドのシグネチャと本体で
  使える。静的メソッドからは見えない(インスタンスがないため)。enum はジェネリックにできない。
- ジェネリッククラスは常に型引数を付けて使う: `Box<int>`、`Repo<User>`、`extends Base<list<T>>`。`new` は、代入先の変数の
  宣言した型か、コンストラクタの引数から推論する。明示もできる(`new Box<int>(1)`)。どちらからも決まらなければエラー。
- `Box<int>` から読むメンバーは `T` が `int` に置き換わる。継承したメンバーも同じ(`class Named extends Box<string>` の
  `get()` は `string` を返す)。オーバーライドするメソッドは、サブクラスから見た親の型に合わせる。
- 型引数は不変: `Box<int>` は `Box<any>` や `Box<float>` が要る場所には使えない(そうでないと `Box<any>` に文字列を
  入れられてしまう)。
- 型引数があるのはコンパイル時だけ。`is` はクラスを確かめる: `$x is Box`(`is Box<int>` はエラー)。型引数の分からない
  `Box<?>` に絞り込む(`catch (Box $e)` も同じ)。これを通して読んだものは `any` で、これを通して書くことはできない
  (`$b->value = ...` や `$b->set(...)` はエラー)。`Box<T>` が要る場所にも使えない。
- クラスは、ジェネリックなクラスやインターフェースのそれぞれに 1 組の型引数でしか届かない: 一方が `Src<SafeUrl>`、
  もう一方が `Src<string>` を継承するとき、`class C implements UrlSrc, StrSrc` はエラー。
- メソッド自身の型パラメーターはそのメソッドのもの: メソッド `m<T>` とクラスのパラメーター `T` は、オーバーライドでも
  別の型。呼び出し側の型パラメーターが、同じ名前のメソッドのパラメーターに置き換わることもない。

### アノテーション

```phv
#[Deprecated("use total()")]
function sum(list<int> $xs): int { return total($xs); }

class Square extends Shape {
    #[Override]
    public function area(): float { return $this->side ** 2; }
}
```

アノテーションは関数・クラス・メソッド・プロパティの前に `#[Name]` や `#[Name("理由")]` の形で書く。`#[A, B]` や
複数行で、いくつも書ける。PHV が知っているのは次のもの(ほかの名前はエラーにして、打ち間違いを見逃さない):

| アノテーション | 書ける場所 | 効果 |
|---|---|---|
| `#[Deprecated]`、`#[Deprecated("理由")]` | 関数・メソッド・クラス・プロパティ | 使った場所で警告(呼び出し、`new`、静的呼び出し、プロパティの読み書き)。そのクラスの中は除く |
| `#[Override]` | メソッド | 親クラスかインターフェースに同じ名前のメソッド(private を除く)がなければエラー |
| `#[Test]` | 関数 | 名前に関係なく `phv test` が実行する(§16) |
| `#[Skip]`、`#[Skip("理由")]` | 関数 | `phv test` は実行せず、スキップしたと報告する |

アノテーションはコンパイル時に検査され、実行時には参照できない。


## 10. 例外クラス

すべてのファイルから使える。

```
Exception                    getMessage()、getCode()、getPrevious()、getTraceAsString()
├── RuntimeException
│   ├── DatabaseError        SQL のエラー
│   └── NetworkError         WebSocket の接続エラー
├── LogicException
│   └── InvalidArgumentException
├── ArithmeticError          整数のオーバーフローなど
│   └── DivisionByZeroError  0 での割り算・剰余
├── IndexError               list の範囲外アクセス
├── KeyError                 map にないキー
├── ValueError               不正な引数(ヘッダーの改行など)
├── UnhandledMatchError
├── PermissionError          phv.toml で許可されていないファイル・環境変数へのアクセス
└── AssertionError           assert() / assert_eq() の失敗
```

実行時間の上限を超えたときなどの致命的なエラーは捕捉できない。

## 11. モジュール

```phv
// lib/posts.phv
export class Post { ... }
export function all_posts(): list<Post> { ... }
function helper(): int { ... }      // export しないものは外から見えない
```

```phv
import lib.posts.{Post, all_posts};
import lib.posts.Post;
import std.http.{request, response};
import lib.admin.posts.{Post as AdminPost};     // 別の名前で
import lib.legacy.render as render_legacy;
```

- `import a.b.{x, y};` は、プロジェクトのルート(`phv.toml` のあるディレクトリ、なければ実行するファイルの
  ディレクトリ)からの `a/b.phv` を読み込む。
- import されるモジュールのトップレベルには宣言(`function`、`class`、`interface`、`enum`、`const`、`type`、`import`)
  しか書けない。モジュールを読み込んでも副作用は起きない。
- `std.*` は標準モジュール(`std.http`、`std.db`、`std.net`、`std.async`、`std.validate`、`std.mail`、`std.testing`、`std.regex`、`std.time`、`std.ws`、`std.jobs`)。`[dependencies]` に書いたパッケージは `import パッケージ名.モジュール` で読み込む。
- 読み込めるのはプロジェクトとパッケージのディレクトリの中のファイルだけ(シンボリックリンクで外に出ることはできない)。
  別のマシン(`\\host\share` のようなネットワーク共有、ネットワークドライブ、`\GLOBAL??\UNC\host\share` のような
  オブジェクトのパス)を指すシンボリックリンクやジャンクションであるか、その先にあるモジュール、テンプレート、パッケージ、
  `phv.toml` は、リンクを解決せずに断るので、相手のホストに接続することはない。プロジェクトそのものは共有の上にあってよい。
  フォルダへのリンクを通してたどったプロジェクトは、実際にあるフォルダ(Windows がその中の相対リンクを解決する所)で調べる。
  `phv lsp` はそのようなリンクの先の開いたドキュメントを解決せず、`phv serve` はその先のページやファイルを返さない。
  動いているサイトのフォルダが後からそのようなリンクになった場合も同じ(ページをコンパイルし直し、断る)。
- 1 つのプログラム(ページかスクリプトと、それが使うすべてのモジュール、テンプレート、パッケージ。std を含む)は、
  全体でトークン 2,000,000 個、ソース 32 MB、ファイル 2,000 個まで。超えたものは型検査の前に「this program is too
  large」で拒む(超えた import かテンプレートの位置に出す)。小さいページに分けること。

## 12. テンプレート

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

- テンプレートは `params(...)` で型付きの引数を宣言する。`render()` の呼び出しはコンパイル時に型チェックされる。
- `render()` のパスは文字列リテラルで、プロジェクトのルートからの相対パス(`..` は使えない)。拡張子は `.phv.html`。
- `layout()` はページ全体をレイアウトで包む: それより後にページが書いたものがレイアウトの `$body`(レイアウトが宣言する
  `Html` の引数)になり、ほかの引数はそのまま渡される。引数はページが終わったときに評価されるので、ページは実行中に
  それらを設定できる:

  ```phv
  <?phv
  layout("views/layout.phv.html", title: $title);
  $title = "Posts";
  ?>
  <ul>...</ul>
  ```

  `layout()` はページかテンプレートのトップレベルの文で、1 回だけ書ける。途中で終わった(`return` した)ページや失敗した
  ページは包まれない。`layout()` を使うページは、テンプレートと同じく、タグ・属性・コメント・`<script>` の中で
  `layout()` を呼んだり終わったりしてはならない(出力がレイアウトの出力とつながるため)。
- `render()` は `Html` を返すので、別のテンプレートにそのまま渡して埋め込める(レイアウト)。
- テンプレートは開いたものをすべて閉じる。タグ、属性、コメント、`<script>` / `<style>` / `<textarea>` 要素、
  `<svg>` / `<math>` の途中で終わるテンプレートはコンパイルエラー。`Html` の値は `.` で連結できるので、`<a href="` で
  終わるテンプレートがあると、次の値が上の検査を受けずに URL の先頭に入ってしまうため。

## 13. 出力とエスケープ

出力先には「テキスト」と「HTML」の 2 種類がある。

| 状況 | 出力先 |
|---|---|
| `phv run` で実行したスクリプト | テキスト(`--html` を付けると HTML) |
| Web サーバーが実行するページ | HTML(`response()->type("application/json")` などで HTML 以外にするとテキスト) |
| テンプレートの中 | 常に HTML |

HTML の出力先に `string` を書くと `& < > " '` がエスケープされる。`Html` 型の値と、テンプレート中の
HTML 部分はそのまま出力される。`echo` と `<?= ?>` は同じ規則に従う。

- `escape($s)` / `htmlspecialchars($s)` … エスケープ済みの `Html` を返す。
- `raw($s)` … 信頼できるマークアップを `Html` として扱う(XSS に注意して使う)。
- `nl2br($s)` … エスケープしてから改行を `<br>` にした `Html`。
- `safe_url($url)` … `http(s)`、`mailto`、`tel`、相対 URL だけを通し、`javascript:` などは `#` にした `SafeUrl`。
  `:` `/` `?` `#` のない相対 URL(`page.html`)は同じ URL の `./page.html` にして返す。後ろにどんな文字が続いてもスキームにならない。
- `json_html($value)` … `<script>` の中の式を書く位置に置いても安全な JavaScript の値(JSON)を `SafeJs` で返す。
  文字列の中は、ASCII の英数字と空白以外の非 ASCII 文字を除くすべての文字を `\uXXXX` で書く。そのため、誤って置かれた
  文字列・テンプレートリテラル・コメント・正規表現を値が終わらせることはない(周りに `"..."` を書いても、値の文字列の中身は
  名前にしかならず、実行されるコードではなく構文エラーになる)。
  文字列の `"` はそのままなので、属性値の中には置けない(コンパイルエラー。`escape(json_encode($v))` を使う)。
- `SafeUrl` は `Html` としても使える(逆はできない)。`SafeJs` は使えない: `Html` を置ける属性の中では、その引用符が
  属性を終わらせてしまう。`escape()` の結果は `Html` なので、URL や `<script>` の中には置けない。HTML のエスケープでは
  `javascript:` やスクリプトのコードを止められないため。
- `Html` と `string` は `.` で連結できない(エラーで `escape()` を案内する)。
- エスケープを外す Content-Type は `text/plain`、`application/json`、`text/csv`、`text/css`、`text/javascript`、
  `text/markdown`、`text/calendar`、`application/pdf`、`application/octet-stream`、画像・音声・動画などに限る
  (どれもブラウザが HTML として表示しない型。`X-Content-Type-Options: nosniff` も常に付く)。
  HTML・XML・SVG・知らない型ではエスケープしたまま。JavaScript のレスポンス(`text/javascript` など、ブラウザがスクリプトと
  して実行する型)に書けるのは `json_html()` の値と数値だけ。文字列やテンプレートの出力はコードになってしまうので実行時
  エラー。型は何かを出力する前に設定する。同じく、空白以外を出力した後は、エスケープしない型をエスケープする型(HTML
  などすべて)に変えられない(`ValueError`): その出力はエスケープされていない。

### HTML の文脈の検査

HTML エスケープが守れるのは、要素の中身と**引用符で囲んだ**属性値だけ。`<?= ?>` と `echo` がそれ以外の場所に
あるとコンパイルエラーになる。場所の判定はブラウザの構文解析(WHATWG)に合わせている:
`<textarea>`・`<title>` などの中身、`<script>` の中の `<!--`、`<!-->` や `--!>` のコメント、`<svg>`・`<math>` の中
(そこの `<script>`・`<style>` の中のマークアップ、つまりタグ・コメント・CDATA セクションはエラー。閉じた `<svg/>` は何も開かず、
`</math>` は `<svg>` を閉じない)、`<noscript>` の中のマークアップ(マークアップとして
検査する)、値やブロックの直前の `<`(そこに入る値はタグ名になるのでエラー)、`</scripts>` のように終了タグに見えて違うもの。
`<svg>`・`<math>` の中では、要素をブラウザが組み立てるとおりに追い、ブラウザが違うように読むものはエラーになる:
`<foreignObject>`・`<desc>`・`<title>`・MathML のテキスト要素(`<mi>`、`<mtext>` など)の中で開いた HTML 要素が開いたままの
`</svg>`(や `</math>`)(ブラウザは無視して SVG として読み続ける。先にその要素を閉じる)、SVG や MathML の直下の `<p>`・`<div>`・
`<b>`・`<font>` などの HTML 要素(や `</p>`・`</br>`)(ブラウザはその前で SVG を終える)、MathML の `<annotation-xml>` の中の
`<svg>` 以外の要素、まわりの HTML 要素を閉じてしまう SVG の中の終了タグ、そして CDATA セクション(そこでは
`<![CDATA[ ... ]]>` を `]]>` まで読む)の中の、`]]>` の終わりでない `>`。開いている `<select>` の中の `<title>`・`<style>`・
`<xmp>`・`<iframe>`・`<noembed>`・`<noframes>`・`<plaintext>` もエラー: 古いブラウザはそこでそれを無視し、テキストをマークアップとして読む。
`<noscript>`・`<svg>`・`<math>` の中では、ブラウザが中身を生のテキストとして読みうる要素の終了タグ(`</textarea>`、
`</noscript>`、`</xmp>` など)がコメントや属性値の中にあるとエラーになり、そこで `</` とその名前の始まりの直後に値を入れるのも、
ほかの文を置くのもエラー(`</<?phv f(); ?>`: 関数が残りを書きうる)。ブラウザがその要素を違うように読む場所
(スクリプトが有効なときの `<noscript>`、`<foreignObject>` の中)では、その終了タグで要素が終わり、続きがマークアップになるため。
`</noscript` は、`<noscript>` の中かどうかにかかわらず、すべてのテンプレートと関数で、コメント・属性値・生のテキスト
(`<script>`、`<textarea>` など)の中をこのように検査する: どのテンプレートや関数も `<noscript>` の中に書かれうる
(`<noscript><?= render(...) ?></noscript>`、レイアウトの本文、呼び出し)が、それぞれ単独で検査されるため。
そのため `<!--</<?= $v ?>-->` や `title="</nos<?phv f(); ?>"` はどのテンプレートでもエラー。

| 場所 | 例 | 扱い |
|---|---|---|
| 要素の中身・引用符付きの属性値 | `<p><?= $s ?></p>`、`title="<?= $s ?>"` | エスケープして出力。`Html` はそのまま出力するが、属性値の中ではその `"`・`'`・`<`・`>` を `&quot;`・`&#039;`・`&lt;`・`&gt;` にする(値は同じ)ので、どの `Html` も属性を終わらせられず、ブラウザがまわりで生のテキストとして読む `<noscript>` や `<title>` も終わらせられない |
| コメント | `<!-- <?= $s ?> -->` | 要素の中身と同じ。ただし `<!--`・`<!---`・`-` の直後には値を入れられず(`<!-- a-<?= $s ?>`)、値の直後のテキストは `>`・`!>`・`->`・`-!>` で始められない(`<!-- <?= $s ?>> -->`。間に文があっても同じ: `<?= $s ?>!<?phv f(); ?>>`): `--` で終わる値がそこでコメントを終わらせるため。ループの本体は自分の終わりや `continue` の後に、ループの後は `break` の後に、`finally` ブロックは `try` ブロックの中の飛び越しの前の値の後に続きうるので、そこでも同じ: `<!-- <?phv foreach ($xs as $x) { ?>> <?= $x ?><?phv } ?> -->` はエラー。`-->` か `--!>` を含む `Html`(`<!x ...>` のような不正なコメントでは `>`)、`>`・`!>`・`->`・`-!>` で始まる `Html`(値の直後に来うる)、`</noscript` を含む `Html`(`<noscript>`・`<svg>`・`<math>` の中では `</title` などどの生テキスト要素の終了タグも)は実行時エラーでスクリプトを止める。コメントのテキストの終わりの `-` は文の後のテキストと続けて読む(`-<?phv f(); ?>->` でコメントは終わる) |
| `<textarea>`・`<title>`・`<xmp>`・`<iframe>`・`<noembed>`・`<noframes>` のテキスト | `<textarea><?= $s ?></textarea>` | 要素の中身と同じ。その要素の終了タグ(`</textarea`)か `</noscript` を含む `Html` は実行時エラーでスクリプトを止める: そこで要素(や、スクリプトが有効なとき生のテキストとして読まれるまわりの `<noscript>`)が終わってしまうため |
| `<svg>`・`<math>` の中の要素の中身・コメント | `<svg><?= render("bar.phv.html") ?></svg>` | 要素の中身と同じ。ブラウザはそこのマークアップを外部コンテンツとして読むが、テンプレートは HTML として検査している: CDATA セクション、自分が開いていない生テキスト要素の終了タグ、テキストに `<` を含むか終わらない生テキスト要素(`<title>`、`<style>`、`<textarea>` など)を含む `Html` は実行時エラーでスクリプトを止める(SVG の `<title>Bar 3</title>` や `<style>.a{fill:red}</style>` は可) |
| 引用符なしの属性値 | `href=<?= $u ?>` | エラー(引用符で囲む) |
| タグの中(属性の間) | `<div <?= $attrs ?>>` | エラー |
| URL 属性の先頭 | `href="<?= $u ?>"`、`src="<?= $u ?>"` | `SafeUrl`(`safe_url($u)`)と数値だけ。前に書いた文字列に `/` `?` `#` `:` があれば先頭ではない(`href="/post?id=<?= $id ?>"` は可)。`href="j<?= ?>"` では先頭のまま。先頭の `&` の後ろ(`href="&<?= ?>"`、`&#106;`)には `SafeUrl` を含めて数値しか入れられない(値が文字参照の続きになり、`&` と `#x6a;avascript:` で `javascript:` になる)。挿入した値の後ろも先頭のまま(`safe_url("")` は空): `href="<?= safe_url($a) ?><?= $b ?>"` はエラーで、URL 全体を `safe_url($a . $b)` で確かめる |
| `javascript:` / `data:` の URL | `href="javascript:go(<?= $v ?>)"` | エラー(数値は可) |
| `<script>` の中 | `var x = <?= $v ?>;` | 式を書く位置に `SafeJs`(`json_html($v)`)と数値だけ。JavaScript の文字列・テンプレートリテラル・コメント・正規表現の中(`` `<?= $v ?>` ``、`"<?= $v ?>"`)は数値だけ: JSON の引用符が文字列を終わらせ、テンプレートリテラルの `${...}` は実行される。`const s = <?= json_html($v) ?>;` と書く |
| `<style>`・`style="..."` | | エラー(数値は可) |
| イベント属性 | `onclick="go(<?= $v ?>)"` | エラー。`data-*` 属性に入れて JavaScript から読む |
| `<iframe srcdoc="...">` | | エラー(値が別の HTML 文書として解析される) |
| `<script>` の URL | `<script src="<?= $u ?>">`(SVG では `href`) | `SafeUrl` でもエラー。このタグには CSP の nonce が付くので、そこに入った URL は信頼されたスクリプトとして実行される。URL はテンプレートに書く |
| `<meta>` の `content` | `<meta http-equiv="refresh" content="0;url=<?= $u ?>">` | 数値だけ。ただし `content` より前に `name`、`property`、`itemprop` 属性があり、`http-equiv` がなければテキスト(`<meta property="og:title" content="<?= $t ?>">`。テキストの後に `http-equiv` を書くとエラー) |

- タグはブラウザーと同じに読む。タグの名前と属性を区切るのはタブ・改行・改ページ・復帰・半角スペースだけなので、
  `<a class=x　title="<?= $t ?>">`(全角スペース)は 1 つの引用符なしの値で、エラーになる。タグの名前はそうした
  空白か `/` か `>` まで続く。
- 関数・クロージャの本体、`if`・ループのブロックは、始まったときと同じ HTML の状態で終わらなければならない
  (`<script>` や属性をブロックの中で開いたまま終えるとエラー)。呼び出し順や繰り返しで文脈がずれるのを防ぐ。
- 同じ理由で、`break` と `continue` はそのループが始まったときの HTML の状態で、`throw` はその `try` が始まったとき
  (なければ関数の始め)の状態で、`return` は関数の始めの状態で書く: `<a href="<?phv if ($x) { break; } ?>...` は
  エラー。飛んだ先の出力が属性の中に入ってしまうため。
- 関数の本体は、どこから呼ばれても要素のテキストとして検査する。そのため、出力を書く関数(クロージャ、メソッド)を、
  その出力がほかの場所に入るところで呼ぶことはできない: `<script><?phv show($s); ?></script>`、
  `title="<?phv show($s); ?>"`、`<!-- <?phv show($s); ?> -->`、`<svg><?phv dot($p); ?></svg>`(`<svg>`・`<math>` の中。ブラウザはそのマークアップを
  外部コンテンツとして読む)、`<textarea><?phv show($s); ?></textarea>`(生テキスト要素のテキスト)は
  実行時エラーでスクリプトを止める。そこでの `dump()` も同じ。値(や `Html`)を返して
  `<?= ?>` で挿入する。レンダリング中のテンプレートなど、取り込まれる出力には関係しない。
- `<script>`・タグ・属性の中で投げられた例外を捕まえると、`try` ブロックが書いた出力は捨てる。`catch` ブロックは開いた
  ままのものの中ではなく、`try` が始まった場所に書く。それ以外の例外では、その前に書いた出力は残る。
- 式・文・型の入れ子は 500 段まで(深すぎるソースで処理系のスタックがあふれるのを防ぐ)。`if`・ループ・`else` の下に
  波かっこなしで書いた文は 1 段、連なった `elseif`(または `else if`)は 1 つごとにもう 1 段、演算子の連なり
  (`$a . $b . $c`)も演算子 1 つごとに 1 段と数える。多くの場合分けには `match` を使う。
- 1 つのソースファイルのトークンは、文字列の中のコードも含めて 1,000,000 個まで。それより多いファイルは構文解析の前に拒む
  (構文木がソースの何倍ものメモリを使うため)。

## 14. Web サーバー

```
phv serve examples/site --port 8080
```

- サイトのディレクトリには `public/` が必要。URL はファイル名で決まる:
  `/` → `public/index.phv`、`/about` → `public/about.phv` または `public/about/index.phv`、`/about.phv` も可。
- 静的ファイルとして配信するのは既知の拡張子(html、css、js、json、txt、画像、フォント、pdf、音声、動画、wasm)だけ。
- 配信しないもの: `public/` の外、`.` や `_` で始まるファイルとディレクトリ(`_partial.phv` は部品に使える)、
  `.phv.html`(テンプレート)、知らない拡張子(`.php`、`.bak`、`.sql` など)。URL はファイル名と完全に同じ綴り
  でなければならない(`INDEX.PHV` や Windows の短縮名、`::$DATA` では開けない)。
- 静的ファイルには、シンボリックリンクやジャンクションを通っては決して届かない(パスを見つけた後で途中に置かれたものも)。
  Unix では `public/` から 1 階層ずつ、リンクをたどらず、待たずに開く。FIFO やデバイス、ほかのユーザーが書き込める
  ディレクトリの下のファイルは配信しない。ほかのハードリンクを持つファイルも配信しない。
- リクエストごとに新しい VM(新しいヒープ)で実行する。コンパイル済みのバイトコードだけが共有される。
- ファイルを保存すると次のリクエストで再コンパイルされる(依存する全ファイルの更新時刻を見る)。
  本番のサーバ(`--prod`)がファイルを見るのは 1 秒に 1 回まで。見るのは、リクエストのパスが何に当たるか、
  コンパイルしたページの元のファイル、メモリに置いた小さな静的ファイル、各ディレクトリに `_middleware.phv` が
  (サイトに `_404.phv` や `_500.phv` が)あるかどうか。変更は 1 秒以内に反映される: ミドルウェアやエラーページの
  追加・削除・リンクへの置き換えは、ページと同じく最大 1 秒遅れて見える。開発用のサーバはリクエストごとに見る。
- 上限: 実行時間 30 秒(`--time-limit`)、メモリ 128 MB(`[security] memory_limit_mb`)、本文 8 MB、ヘッダーの受信 10 秒、
  本文の受信 30 秒、同時接続 1024、クエリ・フォームの項目 1000 個。クライアントが 30 秒データを受け取らなければ
  接続を閉じる。静的ファイル(と `response()->file()`)はディスクから読みながら送るので、メモリを使わない。
  ただし本番のサーバは 256 KB までの静的ファイルをメモリに置く(合わせて 32 MB まで)。
- 同時に実行するスクリプトは `[server] workers` 個まで(既定は CPU 数の 2 倍、最小 8)。空きを最大 10 秒待つ。
  時間内に空かなかったリクエストと、クライアントへの送信待ちの出力(イベントストリームの分も含む)が workers で作れる量(`workers` × `memory_limit_mb`)を
  超えているときに来たリクエストには、`503 Service Unavailable` と `Retry-After: 5` を返す。静的ファイルには空きは要らない。
  リクエスト本文の最初の 64 KB より後は `workers` × 8 MB の枠の中でだけ受信する。枠はバイトが届くたびに取り(宣言しただけの
  `Content-Length` や、まだ送られていないチャンクは枠を持たない)、スクリプトが終わるまで持つ。次のバイトの枠が 10 秒以内に
  取れなければ同じく 503。8 MB を超える本文を宣言したリクエストにはすぐ 413 を返す。クエリとフォームの項目は
  解析しながら数える。読むのが遅いクライアントのために 1 つの接続がためるのは 64 KB まで。そのため、何人が接続しても、サーバーのメモリは
  およそ `workers` × `memory_limit_mb` の 2 倍に収まる。
- `[security] requests_per_minute` を書くと、1 つの接続元アドレスから 1 分間に受け付けるリクエストをその数までにする
  (超えると `429 Too Many Requests` と `Retry-After`)。既定は無制限。`[security] rate_limit_store` を書くと、
  すべてのサーバープロセスで数を共有する(「レート制限」を参照)。接続の数はアドレスごとには制限しない(全体で最大 1,024)。
  インターネットに公開するサイトは、それを制限するリバースプロキシの後ろで動かすとよい。
- 既定で付けるヘッダー: `X-Content-Type-Options: nosniff`、`X-Frame-Options: SAMEORIGIN`、
  `Referrer-Policy: strict-origin-when-cross-origin`、Content-Security-Policy(下の「Content-Security-Policy」)。
  `secure_cookies = true` なら HSTS(HTTPS では常に)。セッションや Cookie を使うレスポンスと、CSRF トークンを含むレスポンス
  (`csrf_field()`、`csrf_token()`)には `Cache-Control: private, no-store`。
  `Server` / `X-Powered-By` のようなバージョン情報は出さない。スクリプトは `Content-Length` などの通信用ヘッダーを設定できない。
- ページはどのリクエストメソッドでも実行される(`request()->method` で読む)が、`TRACE` と `CONNECT` はページを実行せずに
  `405 Method Not Allowed` を返す。静的ファイルが答えるのは `GET` と `HEAD` だけ(ほかは 405)。
- 開発モードではコンパイルエラーと実行時エラーの詳細をブラウザに表示する。開発モードが既定になるのは
  `127.0.0.1` / `localhost` で待ち受けるときだけ(`--dev` / `--prod` で明示できる)。**`127.0.0.1` に接続してくる
  リバースプロキシの後ろでは `--prod` を付ける**こと。付けないと訪問者に詳細が見える(起動時に注意を表示する)。
- Ctrl-C か SIGTERM(`docker stop`)でサーバーは穏やかに止まる: 新しい接続を受け付けるのをやめ、実行中のリクエストが
  終わるのを最大 `[server] shutdown_timeout` 秒(既定 30)待つ。

### HTTPS と HTTP/2

```toml
[server]
tls_cert = "/etc/letsencrypt/live/example.com/fullchain.pem"
tls_key = "/etc/letsencrypt/live/example.com/privkey.pem"
```

- `tls_cert` と `tls_key`(このマシンの PEM ファイル。相対パスはプロジェクトからのパス。`\\host\share` のような
  ネットワーク共有、ネットワークドライブ、それらを指すシンボリックリンクは断る。リンクはたどらずに中身を見るので、
  相手のホストに接続することはない)を書くと、`phv serve` は HTTPS だけで待ち受ける。HTTP/2 か HTTP/1.1 かは、クライアントがハンドシェイクのときに選ぶ(ALPN)。TLS 1.2 と 1.3 を、rustls の
  安全な既定の暗号スイートで使う。0-RTT は使わない。
- ファイルはサーバーの起動時に読む。証明書と組にならない鍵、PEM でないファイル、`public/` の中のファイルは、エラーで
  起動を止める(メッセージにはファイル名だけを出し、中身は出さない)。その後は 1 分に 1 度ファイルを確かめ、更新された
  証明書(certbot など)は再起動しなくても使う。読み込めない証明書はそのことを表示し、前の証明書を使い続ける。
- HTTPS のときは `secure_cookies` が有効になる: セッションの Cookie に `Secure` を付け、`Strict-Transport-Security` を送る。
- クライアントは TLS のハンドシェイクを 10 秒以内に終えなければならない。
- HTTP/2 の接続: 1 つの接続で同時に開けるリクエストは 8 個、リクエストヘッダーは 64 KB まで。サーバーが読む前に
  クライアントが送れるのは 64 KB まで(リクエストごとと接続全体のそれぞれ)、クライアントに渡すのを待つレスポンスは
  1 つにつき 16 KB までなので、1 つの接続がためる量は HTTP/1.1 とほぼ同じ。リクエストの本文は最初のバイトから本文の
  枠で数える。クライアントが 30 秒レスポンスを何も受け取らない接続は切り、30 秒リクエストのない接続は閉じる(遅くとも
  その 10 秒後)。ワーカー、リクエストの上限、レート制限は HTTP/1.1 と共通。リクエスト先の
  authority(HTTP/2 の `:authority`、または `GET http://example.com/ HTTP/1.1` のような絶対形式の宛先)が `host` フィールドに
  置き換わる。ユーザー情報を含むもの(`user@host`)と、`Host` フィールドが 2 つあるリクエストには 400 を返す。
- TLS なしの HTTP/2 は、`[server] h2c = true` のときだけ受け付ける。HTTP/2 でサーバーに接続してくるリバースプロキシ向け
  (クライアントは最初のバイトから HTTP/2 を送る。同じポートで HTTP/1.1 もそのまま使える)。
- スクリプトからは、HTTP/2 のリクエストも HTTP/1.1 と同じに見える: `request()->header("host")` はクライアントが
  指定したホストで、いくつかのフィールドに分けて送られた Cookie はまとめて読める。

### パラメータ付きのルート

角かっこで囲んだファイル名やディレクトリ名は、パスのどれか 1 つの部分に当てはまり、`[...name]` は残り全部に当てはまる:

```
public/posts/[id].phv          /posts/42          request()->param("id") == "42"
public/users/[id]/edit.phv     /users/7/edit
public/docs/[...path].phv      /docs/guide/intro  request()->param("path") == "guide/intro"
```

パスとちょうど同じ名前のファイルが常に優先する(`/posts/new` には `public/posts/new.phv`)。それ以外は、左から見て
決まった名前、`[param]`、`[...rest]` の順に具体的なルートが優先する。同じパスに当てはまる 2 つのルート(同じディレクトリの
`[id].phv` と `[slug].phv`)はエラーで、サーバーは起動せず、`phv build` もそのサイトを拒む。パラメータの値はデコード
済みで、空になることはなく、`.` や `_` で始まることもない。開発モードでは、新しいルートのファイルは 1 秒以内に反映される。

### エラーページとミドルウェア

- `public/_404.phv` はどのページにもファイルにも当てはまらないパスに(ステータス 404 で)答え、`public/_500.phv` は本番で
  ページのコンパイルが失敗したり例外が投げられたりしたときに答える(ステータス 500。リクエストのメソッド・パス・ヘッダーは
  受け取るが、エラーは受け取らない)。開発モードではエラーの詳細を表示する。エラーページ自体が失敗したときは組み込みの
  ページを使う。
- ディレクトリの `_middleware.phv` は、そのディレクトリとその下のすべてのページの前に、外側から順に実行される。出力を
  書いたり、ステータスを設定したり、リダイレクトしたりしたミドルウェアは、その答えでリクエストを終える(ページは実行されない)。
  そうでなければ次にページが実行され、ミドルウェアがしたこと(セッション、設定したヘッダーと Cookie、`locals()`)が
  引き継がれる:

  ```phv
  // public/admin/_middleware.phv
  import std.http.{session, response, locals};
  $user = session()->get("user_id");
  if ($user == null) {
      response()->redirect("/login");
  } else {
      locals()->set("user_id", $user);     // ページでは locals()->get("user_id") で読む
  }
  ```

- ミドルウェアはページの前に実行され、静的ファイルの前には実行されない: `public/admin/` の下のファイルは誰にでも
  配信される。保護が必要なファイルは `public/` の外に置き、ページから `response()->file()` で送る。
- `_` で始まるファイルはページとして配信されないので、`_404.phv`、`_500.phv`、`_middleware.phv` を URL で開くことは
  できない。`phv build` はこれらを実行ファイルに含める。

### 静的ファイル、圧縮、CORS

- 静的ファイルには `ETag` と `Last-Modified` が付くので、ブラウザは安く再検証できる(`304 Not Modified`)。
  `Cache-Control: no-cache` で送り、`[server] static_max_age = N` なら `public, max-age=N` で送る。メディアプレーヤーや
  ダウンロードの再開のため、1 つのバイト範囲(`Range: bytes=a-b`)を `206 Partial Content` で送る。
- 1 KB 以上のテキストのレスポンス(HTML、CSS、JavaScript、JSON、SVG など)は、クライアントが受け付けるなら gzip で
  圧縮する(`[server] compress = false` で止められる)。訪問者ごとのレスポンス(Cookie を設定する、セッションを使う、
  CSRF トークンを含む。ページが設定した `Cache-Control` によらない)と、Cookie か `Authorization` ヘッダー付きの
  リクエストに答えるページは決して圧縮しないので、圧縮後の大きさから中の秘密を推測されることはない(BREACH)。
  ただし、サーバーの前のプロキシや CDN がすべてのレスポンスを圧縮することがある(Fly.io のプロキシはそうする)。
  CSRF トークンはマスクされているので安全だが(「CSRF 対策」)、ページがリクエストの文字列と並べて表示するほかの秘密は
  そうとは限らない。プロキシが許すなら、そうしたページでは圧縮を止める。
  静的ファイルは版ごとに 1 回だけ圧縮して保持する(32 MB まで)。
- `[cors]` を書くと、ほかのオリジンのページからブラウザでこのサイトを呼べる:

  ```toml
  [cors]
  origins = ["https://app.example.com"]   # または ["*"]
  methods = ["GET", "POST"]               # 既定
  headers = ["Content-Type"]
  credentials = false                     # true なら Cookie も送られる("*" とは併用できない)
  max_age = 600
  ```

  プリフライト(`Access-Control-Request-Method` 付きの `OPTIONS`)にはサーバーが答え、ほかのレスポンスには、リクエストの
  `Origin` が許可されていれば `Access-Control-Allow-Origin` を付ける。`credentials = true` で許可したオリジンは、訪問者の
  Cookie 付きで JSON のリクエストを送れるので、信頼できるオリジンだけを書くこと。
- `[server] access_log = "logs/access.log"` を書くと、プロジェクト内のファイルにもリクエストごとに 1 行を追記する
  (`access_log_format = "json"` なら `time`、`ip`、`method`、`path`、`status`、`ms`、リクエスト ID があれば `id` を持つ
  JSON)。パスはクエリ文字列なしで記録する。

### リクエスト ID

```toml
[server]
request_ids = true          # 既定: false
trust_request_id = false    # true: 前にいるプロキシが送る ID とトレースを使う
```

- `request_ids` を有効にすると、各リクエストに ID が付く。W3C Trace Context のトレース ID(16 進数 32 桁)である。
  レスポンスの `X-Request-Id`、JSON のアクセスログの `id` に入り、そのリクエストで書いた `log_info()`・`log_warn()`・
  `log_error()` の行の末尾に `req=<id>` が付く。ページからは std.http の `request_id()` で読める(`request_ids` がなければ
  `null`)。
- ページが `HttpClient` で送るリクエストには、そのリクエストのトレースと独自のスパンを持つ `traceparent` ヘッダーが
  付くので、トレースのシステムで呼び出しをつなげられる(ページが自分で `traceparent` を設定したときは除く)。
- `trust_request_id` を有効にすると、リクエストの `X-Request-Id`(英数字・`.`・`_`・`-` の 1〜128 文字)と
  `traceparent` のトレース(バージョン `00`、小文字の 16 進数)を、形が正しければそのまま使う。正しくなければ新しく作る。
  これらのヘッダーを設定または削除するプロキシの後ろでだけ有効にすること。それ以外ではクライアントが書いた文字列である。

### トレース(OpenTelemetry)

```toml
[telemetry]
endpoint = "http://localhost:4318"              # OpenTelemetry のコレクター(OTLP/HTTP)。または endpoint_env
headers_env = { Authorization = "OTEL_AUTH" }   # 環境変数から取るヘッダー(トレースのサービスの鍵など)
service_name = "blog"                           # 既定: [project] name、なければ "phv"
sample = 1.0                                    # 新しいトレースを記録する割合、0〜1(既定 1)
# tls = false                                   # ほかのマシンへの http:// を許す(推奨しない)
```

- `[telemetry]` を書くと、`phv serve` と `phv jobs run` はトレースを OTLP over HTTP(JSON の本文)でコレクターに送る
  (`POST <endpoint>/v1/traces`。endpoint が `/v1/traces` で終わるときはそのまま)。
- スパン: リクエストごとのサーバースパン。名前はメソッドとページのルート(`GET /posts/[id]`。静的ファイルは
  `GET static`)で、要求されたパスやクエリは決して入らない(属性は `http.request.method`、`http.route`、
  `http.response.status_code`、`url.scheme`、`network.protocol.version`、`phv.kind`)。その下に、スクリプトが
  `HttpClient` で送るリクエスト(バックグラウンドのものも)と、std.oauth・std.jwt が送るリクエストごとのクライアントスパン
  (`server.address`、`server.port`、`phv.http.service`、`http.response.status_code`)。バックグラウンドジョブの実行ごとに、
  それ自身のトレースのスパン(`job send_welcome`。`phv.job.name`、`phv.job.id`、`phv.job.attempt`)。5xx のサーバースパン、
  4xx・5xx またはネットワークエラーのクライアントスパン、失敗したジョブの実行はエラーとして記録する。1 つのスクリプトが
  記録するスパンは 1,000 個まで。データベースのクエリはトレースしない。
- スクリプトが送るリクエストには、トレースとそのクライアントスパンを示す `traceparent` が付くので、呼ばれたサービスも
  同じトレースに加わる(`[telemetry]` がなければ、上の「リクエスト ID」のとおり `[server] request_ids` が `traceparent` を
  送る)。
- `request_ids` がなくても、すべてのリクエストにトレースを作る(`X-Request-Id` ヘッダー、ログの `id`、`request_id()` には
  これまでどおり `request_ids` が必要)。`trust_request_id` のときは、前にいるプロキシからの正しい `traceparent` の
  トレースを続け(そのスパンがリクエストのスパンの親になる)、その sampled フラグでトレースを記録するかを決める。
  新しいトレースは確率 `sample` で記録する。記録しないトレースも、フラグを立てない `traceparent` を送り先に渡す。
- 標準のメソッド(`GET`、`HEAD`、`POST`、`PUT`、`DELETE`、`CONNECT`、`OPTIONS`、`TRACE`、`PATCH`)以外は、OpenTelemetry の
  決まりどおり `_OTHER` と記録し、スパンの名前は `HTTP` にする(`HTTP /posts/[id]`)。スパンの名前と文字列の属性は、それぞれ
  先頭の 1,024 バイトまで。
- スパンはキューに入れ、専用のスレッドが最大 512 個ずつ 5 秒ごとに、またサーバーが止まるときに送る。キューは
  8,192 スパンかつ 16 MB まで: コレクターが遅いか止まっているときは、それ以上のスパンを捨てて `phv_telemetry_dropped_spans_total` で
  数える。送信に失敗したスパンも捨て、報告は 1 分に 1 回まで。リクエストがコレクターを待つことはない。
- リクエストのスパンは、レスポンスのヘッダーを送った時点で終わる(`flush()` でストリーミングするページはその後も続く)。
- `headers_env` は、`[http]` と同じく、スクリプトが読めない環境変数から値を取る。`headers` は固定の値。`endpoint_env` の
  変数が設定されていなければトレースは送らず、サーバーの起動時に警告する。

### ヘルスチェックとメトリクス

```toml
[admin]
listen = "127.0.0.1:9100"        # 管理用ポート。サイトとは別のリスナー
token_env = "PHV_ADMIN_TOKEN"    # listen がループバック以外なら必須
```

`[admin]` を書くと、サイトとは別のポートを開き、次の 3 つのパスに答える(サイトのパスはそこでは答えず、
これらのパスはサイトのポートでは答えない)。

| パス | 応答 |
|---|---|
| `/health` | サーバーが動いている間は 200 `ok`。止まり始めた時点(Ctrl-C、SIGTERM)から、実行中のリクエストが終わるのを待つ間は 503 |
| `/ready` | サーバーが使うストアが最後に使ったときに失敗していれば、また答えるまで 503: セッションのデータベース、レート制限のストア、`publish()` のストア(`unavailable: sessions, ...`)。probe のためにサーバーがどこかへ接続することはない |
| `/metrics` | 下の値を Prometheus のテキスト形式で返す。`token_env` があれば `Authorization: Bearer <token>` が必要(なければ 401) |

- ループバック以外のアドレス(コンテナの probe 用の `0.0.0.0:9100` など)には `token_env` が必要で、ないとサーバーは
  起動しない。`/metrics` にはサイトのルートと負荷が出るためである。トークンは起動時に読み、16 バイト以上必要。
  `/health` と `/ready` はトークンを求めない(ロードバランサーは送れないことが多い)。分かるのはサーバーが動いているか
  だけである。
- 管理用ポートは `GET` と `HEAD` に答え、1 接続 1 リクエスト、同時に 64 接続まで。クライアントは 5 秒以内にリクエストを
  送る必要がある。
- メトリクス(すべて `phv_` で始まる): `requests_total{kind, code}`(`kind`: `page`、`static`、`ws`、`sse`、`other`。`code`:
  `2xx` など)、`request_duration_seconds{route}`(ページごとのヒストグラム。ラベルはファイル名から作る `/posts/[id]` で、
  要求されたパスは決して入らない。ルートは 1,000 個までで、残りは `other`。静的ファイルは `static`)、
  `worker_wait_seconds`、`rejected_total{reason}`(`busy`、`output`、`body`、`rate_limit`、`too_large`)、`events_waiting_bytes`、
  `script_failures_total{kind}`(`exception`、`time`、`memory`、`compile`、`other`)、`ws_messages_total{dir}`、
  `publish_total{result}`、`jobs_total{result}`、`job_duration_seconds`、`store_errors_total{store}`、
  `telemetry_dropped_spans_total`、およびゲージ
  `up`、`ready`、`uptime_seconds`、`workers`、`workers_busy`、`connections`、`websockets`、`output_waiting_bytes`、
  `async_running`、`build_info{version}`。数えるときにロックを取るのはページのヒストグラムだけである。

### std.http

```phv
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
$req->param("id");         // ?string: ページのパスのパラメータ(`public/posts/[id].phv`)
$req->params;              // map<string, string>
locals()->get("user_id");  // ?string: `_middleware.phv` が locals()->set() で設定した値
request_id();              // ?string: リクエストの ID([server] request_ids のとき。「リクエスト ID」)

response()->status(404);
response()->header("Cache-Control", "no-store");
response()->type("text/plain; charset=utf-8");
response()->cookie("theme", "dark");
response()->redirect("/login");                       // サイト内のパスだけ
response()->redirectExternal("https://example.com/");  // 外部の URL
response()->json($value);
response()->file("data/photos/1.jpg");                // 出力の代わりにファイルを送る
response()->flush();                                  // それまでの出力を送る(「ストリーミングレスポンス」)
```

### ストリーミングレスポンス

```phv
import std.http.response;

response()->type("text/csv; charset=utf-8")->header("Content-Disposition", "attachment; filename=\"orders.csv\"");
foreach ($db->query(sql"SELECT id, total FROM orders") as $i => $row) {
    echo $row->int("id") ?? 0, ",", $row->float("total") ?? 0.0, "\n";
    if ($i % 1000 == 999 && !response()->flush()) {
        return;                                   // 訪問者が去った
    }
}
```

- `response()->flush()` は、ステータス・ヘッダーと、ページがそれまでに書いた出力を送り、レスポンスを開いたままにする
  (HTTP/1.1 では chunked、HTTP/2 ではストリーム)。その後に書いた出力は、次の `flush()` か、ページが終わったときに送る。
  長いエクスポートやレポート、少しずつ届く応答に向く。ページから Server-Sent Events を書くこともできる
  (`text/event-stream`)。
- 最初の `flush()` の後は、ステータス・ヘッダー・Cookie・セッションを変えられない(`ValueError`)。セッションはヘッダーと
  一緒に保存し、その Cookie を送る。セッションを読むことはできる。`csrf_token()` には訪問者の CSRF Cookie が必要だが、
  それはもう送れない: 訪問者の最初のページでは `flush()` の前に呼ぶこと。
- ストリーミングするヘッダーには、ページがセッションを使ったか Cookie を設定したか CSRF トークンを作ったとき、または
  リクエストに `Cookie` か `Authorization` ヘッダーがあるとき、`Cache-Control: private, no-store` を付ける(ページが
  `Cache-Control` を設定していなければ): ヘッダーを送った後で、ページが訪問者ごとの内容になることがあるため。ヘッダーを
  訪問者ごとのものとして送っていなければ、`flush()` の後の `csrf_token()` は拒む(`ValueError`)。
- `flush()` は前のチャンクを接続が受け取るまで待つので、ページが訪問者より 1 チャンクより先に進むことはない。送った出力は
  ページのメモリの上限ではなく、接続に書き込まれるまでサーバーの出力の合計に数える(ほかのレスポンスと同じく 64 KB ずつ
  接続に渡す)。訪問者が去ったか(`HEAD` リクエストも)、10 秒間何も受け取らないと `false` を返し、その後に書いた出力は
  捨てる。送れなかった出力は、解放されるまでページのメモリに数える。ゆっくりでも読み続ける訪問者は待つ。ページは
  終わるまでワーカーを使い、時間の上限はほかのページと同じである。
- `flush()` の後に失敗したページはエラーページを出せない: レスポンスは途中で切れる(HTTP/1.1 は最後のチャンクを送らずに
  接続を閉じ、HTTP/2 はストリームをリセットする)ので、クライアントは完全でないと分かる。エラーはいつもどおりログに出る。
- `flush()` が送った出力は `catch` で取り消せない: `flush()` より前に始まった `try` が捨てるのは、その後に書いた出力だけ。
  例外がタグ・属性・`<script>` などの HTML を書きかけのまま残し(`catch` は `try` が書いたものの代わりに書く)、その HTML の
  始まりを `flush()` が送っていたときは、ページは捕捉できないエラーで止まり、レスポンスは途中で切れる。
- ストリーミングするレスポンスは圧縮しない。出力を取り込んでいる間(`layout()` を使うページやテンプレートの中)、
  WebSocket のスクリプト、`response()->file()` の後では `flush()` を呼べない(`ValueError`)。`flush()` を呼んだ
  ミドルウェアはリクエストに答えたことになる。`phv serve` の外(`phv run`、`phv test`)では出力をそのまま持ち、`true` を
  返す。
- ストリーミングするレスポンスのアクセスログ、`phv_request_duration_seconds`、トレースのスパンは、ヘッダーを送った時点で
  終わる。

### セッション

```phv
import std.http.session;

session()->set("user_id", (string)$user->id);
$id = session()->get("user_id");      // ?string
session()->regenerate();              // ログイン直後に呼ぶ(セッション固定攻撃の対策)
session()->flash("notice");           // 読んだら消えるメッセージ
session()->destroy();                 // ログアウト
```

- 値はサーバー側に保存され、Cookie には 256 ビットの乱数の ID だけが入る
  (`PHVSESSID`、`HttpOnly`、`SameSite=Lax`、`[security] secure_cookies = true` で `Secure`)。
- サーバーが発行していない ID は受け付けない。1 セッションの値は合計 64 KB まで。
- `[security] session_ttl` 秒アクセスがないと消える。アクセスが続いても 24 時間で消える。
- 既定の保存先はメモリで、サーバーを再起動すると消える。`[security] session_store = "sessions/"` のように
  プロジェクト内のディレクトリを書くと、1 セッション 1 ファイルで保存する。再起動しても残り、同じディレクトリを使う
  複数のサーバープロセスで共有できる。ファイル名は ID のハッシュなので、ディレクトリの一覧から ID は分からない。
  `public/` の中は指定できない。新しいセッションのファイル名は、訪問者が Cookie を送り返すまでは `n-<hash>.json` で、
  送り返したときに `<hash>.json` に変わる。
- `[security] session_store = "db:<名前>"` と書くと、`[databases]` のそのデータベース(Redis、PostgreSQL、MySQL)に保存し、
  それを使うすべてのサーバープロセスとマシンで共有する。Redis は `phv:session:<hash>` に保存し、期限切れは Redis に任せる。
  訪問者が Cookie を送り返していないセッションは、長くても 1 時間で消える。サイトを下の上限に収めるための数は
  `phv:sessions:<名前空間>:` の下に置く。PostgreSQL と MySQL はサイトのセッションをそのサイト専用のテーブル
  `phv_sessions_<名前空間>`(必要になったときに作る)に保存し、30 秒ごと(新しいセッションが 1,000 できたときはその前でも)に
  期限切れを消して、下の上限に収める。データベースに届かないときは、セッションなしでリクエストを続け、警告をログに出す。
- データベースを共有するサイトどうしは、サイトの名前空間でセッションを分ける: `[security] store_namespace`、なければ
  `[project] name`(流量制限とジョブもこの下に置く)。ステージングと本番のように同じ名前のサイトを 2 つ置くときは、
  別々の `store_namespace` を設定すること(設定せずに共有の保存先を使うサーバーは、起動時にそう知らせる)。どちらもないサイトは、ほかのそうしたサイトと保存先(1.2 からはテーブル
  `phv_sessions_unnamed` とキー `phv:sessions:unnamed:*`)を共有する。名前空間のあるサイトは 1.1 より前に保存されたセッションを読まないので、
  1.1 へのアップグレードで訪問者は一度ログアウトした。`session_ttl` と上限はサイトごとに別で、データベースを共有する
  サイトどうしが共有するのはその容量だけ。
- 1.1 が共有のテーブル `phv_sessions`(PostgreSQL、MySQL)に保存したセッションや、共有のキー
  `phv:sessions:new`、`back`、`keep`、`size`、`bytes`(Redis)で数えたセッションも、保存したサイトが読み、その場所で
  更新し、ログアウトで消す。アップグレードで誰もログアウトしない。1.1.1 が名前空間のないサイトのためにそこへ保存した
  セッションも同じ。こうしたセッションは長くても 24 時間で終わり、それを過ぎると、どのサイトの掃除でも消える(Redis
  では、Redis が期限切れにしたあとでその数も消える)。どれかのサイトでまだ生きているセッションは消さない。サイトごとの
  `session_ttl` と上限は、こうしたセッションには使わない。1.1 と 1.1.1 のプロセスがそのデータベースを使わなくなったら、
  そのテーブルとキーは消してよい。名前空間のないサイトで 1.1.1 と 1.2 のプロセスを並べて動かしている間は、1.1.1 の
  プロセスは 1.2 のプロセスが保存したセッションを読まない。
- どの保存先でも、セッションを保存するのはそれがまだあるときだけ(新しい ID や作り直した ID は、まだ使われていない
  ときだけ)で、それぞれを 1 つの不可分な操作で行う。ログアウトのあとに終わったリクエストが(同じプロセスでも別の
  プロセスでも)セッションを復活させることはなく、同じセッションを作り直す 2 つのリクエストのうち、引き継げるのは 1 つだけ。
- サイトごとに、保存できるセッションは最大 50,000、データは 256 MB まで(Redis は書くたびにこの範囲に収める)。いっぱいになると、訪問者が一度も戻ってこなかったセッションから先に消し、
  その次に残りの中で一番新しいものを消す。Cookie を送り返すセッションを大量に作られても、消えるのはそのセッションで、
  それより前に作られたセッションは消えない。使われないセッションは `session_lifetime` で終わる。

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

トークンはブラウザごとの Cookie(`PHVCSRF`)の HMAC なので、攻撃者がその Cookie を書き換えられないことが前提になる。
`csrf_field()` と `csrf_token()` は、書くたびに別の乱数でマスクして書く(16 進数 128 桁)。文字列が毎回変わるので、
途中で(サーバーか、その前のプロキシや CDN で)圧縮されたページの大きさからトークンが推測されることはない(BREACH)。
サーバーはマスクした形とそのままの形の両方を受け付ける。
本番では HTTPS で配信して `secure_cookies = true` にする(Cookie が `__Host-` 付きになり、平文の通信や
サブドメインから上書きされなくなる)。
`[security] csrf = false` で無効にできる。

HMAC の鍵はサーバーが起動するたびに新しくなる。サイトに秘密鍵があればそれを使うので、再起動のあともフォームは有効で、
サイトのどのサーバープロセスでも受け付けられる。ロードバランサーの後ろで、あるプロセスが出したフォームを別のプロセスに
送れる。

```toml
[security]
secret_env = "PHV_SECRET"          # 32 バイト以上を入れた環境変数(random_token() で作れる)
secret_old_env = "PHV_SECRET_OLD"  # 任意: 前の秘密鍵。入れ替えの間だけ受け付ける
```

- サーバーは起動時に変数を読む。設定されていない、または 32 バイトに満たない変数があると、変数の名前を示したエラーで
  止まる(値は決して示さない)。サーバーの設定なので `[permissions] env` に書く必要はなく、`phv build` の実行ファイルには
  名前だけが入る。`[permissions] env` のパターンに当てはまっても、スクリプトからは読めない。`[admin] token_env` と、
  すべての `url_env`・`headers_env` も同じ。
- サーバーは用途ごとに秘密鍵から別の鍵を導く(HKDF-SHA256): CSRF トークンと、`publish()` がほかのサーバープロセスに
  送るメッセージ。スクリプトから秘密鍵や導いた鍵は読めない。
- 秘密鍵を入れ替えるときは、新しい鍵を `secret_env` に、古い鍵を `secret_old_env` に入れて全プロセスを再起動し、
  それ以前に出したフォームが使われなくなったら(数時間後)`secret_old_env` を外す。トークンは常に新しい鍵で作られる。
  プロセス間の `publish()` は `secret_env` だけを使うので、新しい鍵のプロセスと古い鍵のプロセスが混ざっている間は、
  互いのメッセージは届かない。

### Cookie

```phv
request()->cookie("theme");                                   // ?string
response()->cookie("theme", "dark", maxAge: 86400 * 30);      // HttpOnly; SameSite=Lax が既定
response()->deleteCookie("theme");
```

名前に使えない文字、改行、`SameSite=None` で `Secure` なし、は `ValueError`。

### アップロード

```phv
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

```phv
$data = request()->json();             // any(不正な JSON は ValueError)
if ($data is map) {
    $name = $data["name"] ?? null;
    if ($name is string) { ... }
}
response()->json(["ok" => true]);
```

#### 型付きの JSON

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

Order $order = json_decode_as(request()->body);     // JSON が Order に合わなければ ValueError
echo json_encode_pretty($order);
```

`json_decode_as($text)` は、値が求められている型(宣言した変数、引数、戻り値の型。どれもなければコンパイルエラー)に
デコードし、JSON がその型に合うかを検査する。使える型は `int`、`float`(整数も受け付ける)、`string`、`bool`、`any`、
`?T`、`list<T>`、`map<string, T>`、`map<int, T>`(キーは 10 進の整数)、enum(backed enum は値で、pure enum はケース名で)、
クラス。

- クラスはコンストラクタを呼ばずに JSON のオブジェクトから作る。そのため、ジェネリックでも抽象でもなく、プロパティが
  すべて public で、コンストラクタがあれば public で引数をプロパティにするだけのクラスに限る。中に処理のある
  コンストラクタ(引数を検査しているかもしれない)は飛ばされてしまうので、そのようなクラスはコンパイルエラー。
  コンストラクタ引数でない readonly プロパティ(値は既定値にしかなりえない)があるクラスも同じ。
- JSON にないプロパティは、その既定値、コンストラクタ引数の既定値、nullable なら `null` になる。どれでもなければエラー。
  クラスにないキーは無視する。
- 合わないときの `ValueError` は場所を示す: `json_decode_as(): expected an integer, found a string at $.items[2].qty`。

`json_encode` は enum のケースを値(pure enum ではケース名)として書くので、書いたものを `json_decode_as` で読み戻せる。
`json_encode_pretty($v)` は `json_encode` の結果を行に分け、2 スペースで字下げしたもの。

### データベース(std.db)

```phv
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
- 列名やテーブル名を動的に決めたいときは `sql_ident($name)`(英数字と `_` だけを許可して引用する。セッションなど PHV 自身のテーブルである `phv_` で始まる名前は断る)。
- `ATTACH` は無効になっていて、許可されたファイル以外のデータベースには触れない(`phv db migrate` でも)。
- `LIKE` と `GLOB` もクエリのほかの部分と同じくスクリプトの制限時間で止まる。`instr()`・`replace()`・`trim(X, Y)` は
  線形時間で探す。`PRAGMA case_sensitive_like` は拒否する(SQLite 自身の `LIKE` に戻ってしまうため)。
- 長い値を扱うクエリも、スクリプトの制限時間を過ぎるとすぐに止まる。SQLite はプログラムの 100 ステップごと、1 MB
  確保するごと、1 行ごとに期限を見る。`LIKE`・`GLOB`・`instr()`・`replace()`・`trim(X, Y)` は呼ばれるたびに見る。
  これらの関数が確保するメモリもメモリ制限に数える。テキストやブロブは 16 MB まで(超えると SQLite が
  "string or blob too big" を返す)。
- 期限を見ないまま値全体を読むステップがある:テキストを数値にする(`b * 2`、`CAST`、`NOT b`)、`length()`、
  `substr()`、`NOCASE`、値の比較やコピー。文は初めて実行する前にプログラムを量る。こうしたステップをかかりうる時間で
  数え(数値にする被演算子が 2 つなら 2、コピーは 8 分の 1、比較は 16 分の 1)、PHV 自身の関数と新しい値を作る関数
  (`upper()`、`hex()`、`printf()` など)は数えない。重さが 128 を超える文は `DatabaseError`("split it into smaller
  statements")になる。ビュー、`WITH` の表、トリガーがステップを繰り返す場合も同じ。普通のプログラムの文の重さは
  10 ほどまで(30 列の算術で 120)。重さ 128 の文は、16 MB の値で制限時間を 3 秒ほど過ぎる。文のステップは
  43,008 まで。
- `transaction()` の中の `transaction()` は(同じ名前の別の `Database` からでも)セーブポイントになる。投げると自分の
  変更だけが取り消され、一番外側が終わるまで何もコミットされない。
- SQL のエラーは `DatabaseError`。
- データベースファイルへの接続は、同じファイルを開く次のスクリプトのために保持する(使われないまま 30 秒まで。
  `:memory:` は保持しない)。そのため、リクエストのたびにデータベースを開くページでもファイルを開き直さない。開くたびに
  パスはこれまでどおり確かめる。途中にリンクがないこと、ジャーナルのファイルがないか通常のファイルであること、データベースが
  ハードリンク 1 本の通常のファイルで、保持している接続が開いているファイルそのものであること(違えば新しく接続する)。
  新しいスクリプトには自分の制限時間が効き、`lastInsertId()`、`changes()`、`total_changes()` は 0 から数える。
  保持する接続はプリペアドステートメントも保持する(最後に実行した 32 個の文)。前のスクリプトと同じ文を実行する
  スクリプトは、その文をコンパイルも重さの計算もし直さない。保持している文のスキーマがその間に変わっていれば、最初の
  ステップでコンパイルし直し、そのプログラムが 200 ステップより長ければ重さも計算し直す。文に束縛した値は、その実行が
  終わると消す。スクリプトが終わると、開いたままのトランザクションはロールバックする。文が失敗したとき、`PRAGMA` を実行したとき、
  一時テーブル・ビュー・インデックス・トリガーが残っているときは、保持せずに閉じる。`unlink()` と `rename()` は、
  そのファイルの使われていない接続を先に閉じる。
- SQLite のメモリはスクリプトのメモリ制限に数える。接続ごとのページキャッシュ(2 MB まで)、スキーマと文、
  インメモリのデータベース、そして一時テーブル・インデックス・ソート(一時ファイルには書かず、メモリに置く)。保持していた
  接続も、スクリプトが持っているあいだは数える。制限を超える文は、メモリ制限の致命的エラーで止まる。スクリプトが同時に
  開いておける SQLite のデータベースは 64 個まで(使わなくなったものは `close()` する)。65 個目の `Database::open()` は
  `DatabaseError` を投げる。

#### PostgreSQL と MySQL

データベースサーバーは `phv.toml` に名前を付けて書きます。スクリプトは名前だけを使うので、パスワードがコードに出てきません。

```toml
[databases.main]
url_env = "DATABASE_URL"      # postgres://user:pass@host/db、mysql://user:pass@host/db
# url = "postgres://app@localhost/app"   # URL を直接書くこともできる(ローカル開発向け)
# tls = false                 # リモートのホストへ暗号化なしで接続する(非推奨)
# pool_size = 8               # サーバープロセスごとに保つ接続数(1〜256)
```

```phv
$db = Database::connect("main");
$row = $db->first(sql"INSERT INTO posts (title) VALUES ({$title}) RETURNING id");   // PostgreSQL
$db->execute(sql"INSERT INTO posts (title) VALUES ({$title})");
$id = $db->lastInsertId();                                                          // MySQL
```

メソッドは SQLite と同じです(`query`、`first`、`execute`、`transaction`、`close`)。

- 値は必ずバインドパラメータとして送ります(PostgreSQL は `$1`、MySQL は `?`)。SQL の本文に書いた `?`
  (PostgreSQL の `jsonb ? 'key'` など)はそのまま残ります。
- **`localhost` 以外のホストには TLS が必須**で、サーバー証明書は公開 CA(webpki-roots)で検証します。
  プライベート CA の証明書を使うサーバーにはまだ対応していません。そうしたホストでは `tls = false` で TLS を切れます。
- `url_env` の環境変数が設定されていないことは、スクリプトが接続したときにエラーになります。そのため本番の
  認証情報がなくても `phv check` は通ります。URL はスクリプトにもエラーメッセージにも出ません。
- 接続はサーバープロセスごとにプールします。スクリプトが終わる(または `close()` する)と、セッションを
  リセットしてからプールに戻します。開いたままのトランザクションはロールバックし、一時テーブルとセッション設定も
  消します(PostgreSQL はカーソル、`SET`、ロール、`LISTEN`、アドバイザリロック、一時テーブル、プリペアド文、
  MySQL は `COM_RESET_CONNECTION` と有効なロール、URL のデータベース)。1 つのスクリプトで同じ名前に
  2 回接続すると同じ接続になります。接続がすべて使用中のときは、スクリプトの制限時間(最大 30 秒)まで待ってから
  `DatabaseError` になります。
- サーバーでは、このリセットとプールへの返却(SQLite や Redis の接続も)はページのレスポンスを渡したあとに行うので、
  訪問者はそれを待ちません。ページのワーカーは終わるまで持ったままです(`workers` の上限はそうしたページも数えます)。
  アクセスログの所要時間はレスポンスを渡した時点までです。リクエストを次に渡すミドルウェアは、ページが動く前に
  接続を返します。
- PostgreSQL の接続は、最後に実行した 64 個の文をプリペアしたまま保持します(SQLite はスクリプトの接続の文)。同じ文を
  もう一度実行するときは、その手順を省きます。保持するのは `SELECT`、`INSERT`、`UPDATE`、`DELETE`、`WITH`、`VALUES`
  だけで、それ以外の文(`ALTER TABLE`、`DEALLOCATE`、`SET` など)を実行すると捨てます。表が変わったとき(別の接続が
  変えたときも)はプリペアし直します。トランザクションの中では、そのトランザクションでプリペアした文だけを使います。
  スクリプトがクライアント自身のプリペアした文を解放した接続(`DEALLOCATE ALL` や `DISCARD ALL`、型を調べる文だけの
  `DEALLOCATE`。関数の中でも)は、プールに戻さずスクリプトのあとで閉じます。リセットはそうした文を名前で探します。
  見落とした文が無いせいで文が「prepared statement ... does not exist」で失敗したときも、その接続は閉じます。
- スクリプトの制限時間を超えた文は止まります。サーバー側(PostgreSQL は `statement_timeout`、MySQL は
  `SELECT` に効く `max_execution_time`)に加えて、スクリプトがその設定を変えてもクライアント側で止めます
  (PostgreSQL の接続は期限で読み取りをやめ、MySQL の文は `KILL QUERY` で止める)。スクリプトに残っているメモリより
  大きい結果は、読んでいる途中で拒否します(その接続は閉じて再利用しない)。大きな結果やおかしなサーバーでプロセスが
  メモリ制限を超えて膨らむことはありません。PostgreSQL の通知(`LISTEN`)は届いたときに捨てます(スクリプトは読めない)。
- 1 回の呼び出しで実行できる文は 1 つだけです(MySQL の複数文は無効)。`LOAD DATA LOCAL INFILE` も拒否します。
- 値の型: PostgreSQL の `bool` → `bool`、整数型 → `int`、`real`/`double precision` → `float`。
  `numeric`、`json`/`jsonb`、`uuid`、日付と時刻(`YYYY-MM-DD HH:MM:SS[.ffffff]`。`timestamptz` は UTC)、
  `bytea`(`\x...` の 16 進)、文字列型 → `string`。それ以外の型(配列や `interval` など)は `DatabaseError`
  になるので、SQL でキャストしてください(`col::text`)。パラメータは列の型に合わせて変換するので、`date`、
  `numeric`、`uuid`、`jsonb` の列にも文字列を渡せます。MySQL の `DECIMAL`、`JSON`、日付と時刻は `string`、
  `TINYINT(1)` は `int` です(`Row::bool()` で読めます)。
- アプリケーションには必要な権限だけを持つデータベースユーザーを使ってください(`SUPERUSER` や `FILE` は不要)。

#### Redis

```toml
[databases.cache]
url_env = "REDIS_URL"         # redis://localhost。TLS なら rediss://user:pass@host
```

```phv
import std.db.Redis;

$redis = Redis::connect("cache");
$redis->set("greeting", "hello", 60);          // 60 秒後に消える(0 なら消えない)
$redis->get("greeting");                       // ?string
$redis->incr("visits");                        // int(incr($key, $by = 1))
$redis->hset("user:1", "name", "Ann");  $redis->hgetAll("user:1");   // map<string, string>
$redis->rpush("jobs", "a");  $redis->lpop("jobs");  $redis->lrange("jobs", 0, -1);
```

メソッド: `get`、`set`、`delete`、`exists`、`incr`、`expire`、`ttl`、`hget`、`hset`、`hdel`、`hgetAll`、
`rpush`、`lpop`、`lrange`、`close`。送れるのはこれらのコマンドだけで、`KEYS`、`FLUSHALL`、`CONFIG`、`EVAL`
などは使えません。`phv:` で始まるキーは PHV のもの(Redis に置くセッション・レート制限・ジョブ)で、`ValueError` になります。
リモートのサーバーには `rediss://` の URL が必要です(または `tls = false`)。エラーは `DatabaseError`。
各コマンドが待つのはスクリプトの制限時間までで、スクリプトに残っているメモリより大きい応答は拒否します。

### WebSocket サーバー (std.ws)

`public/chat.ws.phv` というファイルが `/chat` への WebSocket 接続に答える(`public/rooms/[room].ws.phv` はパラメータ
`room` 付きで `/rooms/blue` に答える)。ページではないので、`/chat` への普通のリクエストでは実行されない。

```phv
<?phv
// public/chat.ws.phv
import std.ws.{socket, publish, WsEvent};
import std.http.{request, response};

$ws = socket();
if ($ws->event == WsEvent::Open) {
    $name = request()->query("name") ?? "";
    if ($name == "") {
        response()->status(403);           // 接続を断る
    } else {
        $ws->set("name", $name);           // 次のイベントのために残す
        $ws->join("lobby");
    }
} elseif ($ws->event == WsEvent::Message) {
    publish("lobby", ($ws->get("name") ?? "?") . ": " . $ws->message());
} else {
    publish("lobby", ($ws->get("name") ?? "?") . " left");
}
```

- スクリプトは、ページがリクエストごとに実行されるのと同じく、接続の**イベント**ごとに 1 回実行される:
  `WsEvent::Open`(アップグレードのリクエストの一部として実行する。ミドルウェアが動き、`request()` と `session()` が使え、
  200 以外のステータスにするとそのレスポンスで接続を断る)、テキストメッセージごとの `WsEvent::Message`
  (`$ws->message()`)、`WsEvent::Close`。どの実行にもページと同じ時間とメモリの上限があり、ワーカーを使う。イベントの
  あいだ、接続は VM もワーカーも持たない。1 つの接続のメッセージは 1 つずつ順に処理する。
- 実行のあいだに残るのは、接続が `$ws->set($key, $value)` で残した値(`get()` で読む。256 個まで、合計 64 KB まで。
  値 1 つにつき 64 バイト多く数える)と、参加したトピックだけ。`$ws->id` はランダムな ID で、1 つの接続のすべてのイベントで同じ。
- `$ws->send($text)` はこの接続に送る。`publish($topic, $text)` は、このサーバープロセス(`publish_store` があれば
  サイトのほかのサーバープロセスも)で `$topic` に参加したすべての接続に送る(`$ws->join()`、`$ws->leave()`。1 つの接続につき 64 トピックまで)。`publish()` はどのページからも
  使える(たとえばフォームを保存したあと)。メッセージはスクリプトが終わってから送る。出力(`echo`)は捨てる。
  `$ws->close($code, $reason)` で接続を閉じる(コードは 1000 か 3000〜4999)。
- ブラウザはページのオリジンを送る。別のサイトからの接続は、`[cors] origins` がそのサイトを名前で挙げ、
  `credentials = true` でなければ 403 で断る(WebSocket には訪問者の Cookie が必ず付くので、`"*"` や `null` では許さない)。
  別のサイトが訪問者として接続を開くことはできない。
- 上限: 同時接続 1,024(超えると 503)、テキストメッセージは 64 KB まで(超えるものとバイナリのメッセージは接続を
  閉じる)、1 つの接続に送るのを待つメッセージは 1 MB まで(読まないクライアントは切断する)、1 回の実行で送る・
  publish するメッセージは 1,000 個まで、送るメッセージ 1 つは 1 MB まで。サーバーは 30 秒ごとに ping を送り、75 秒
  何も届かない接続は閉じる。`[security] requests_per_minute` があれば、メッセージ 1 つを 1 リクエストと数え、上限を
  超えた接続は閉じる。
- スクリプトのエラーは接続をコード 1011 で閉じる。サーバーを止めるときは 1001 で閉じる。
- WebSocket は HTTP/1.1(と HTTPS)で使う。HTTP/2 のクライアントは HTTP/1.1 に切り替えて接続する。

#### Server-Sent Events

同じスクリプトが、ブラウザの `EventSource`(自分でつなぎ直しもする)向けの Server-Sent Events にも答える。

```js
const events = new EventSource("/chat?name=ann");
events.onmessage = (e) => console.log(e.data);
```

- `*.ws.phv` のパスに、`text/event-stream` を受け付ける `GET` が来て、そのパスに答えるページがなければ、WebSocket と同じく
  `WsEvent::Open` のイベントを実行する(ミドルウェア、`request()`、`session()`。200 以外のステータスで断る)。そのあと
  レスポンスはストリームになり、スクリプトが送ったもの(`$ws->send()`)と、参加したトピックに publish されたものが
  イベントとして届く(本文の各行が `data:` の 1 行)。クライアントが去ると `WsEvent::Close` を実行する。
  `WsEvent::Message` のイベントはない。クライアントからは普通のリクエスト(`publish()` を呼ぶページ)で送る。
- イベントストリームは WebSocket の接続として数え(合わせて同時に 1,024 まで)、同じ 1 MB のキューを持ち(読まない
  クライアントは切る)、プロキシが閉じないよう 15 秒ごとにコメント行(`: ping`)を送る。圧縮もキャッシュもされず、
  HTTP/1.1 と HTTP/2 で使える。ほかのサイトが開けるのは WebSocket と同じ場合だけ(`[cors]`)。イベントの間は
  ワーカーを使わない。
- イベントは送り終えるまで、別の枠で数える。枠は `workers` × `memory_limit_mb` の 4 分の 1。ページには常に残りの
  4 分の 3 があるので、ストリームがページを待たせることはなく、ページとストリームの合計も `workers` × `memory_limit_mb`
  に収まる。1 つのストリームが送り待ちにできるのは 2 MB で、イベントがそれより大きくなるメッセージ(約 150,000 行を
  超える 1 MB のテキスト)はイベントとして送らない。
- 送り待ちのあるストリームが使えるのは枠の 4 分の 3 まで。残りの 4 分の 1 は、送り終えたストリームのために取っておく。
  送り待ちを 1 秒以上持っているストリームは、次のイベントで 2 MB を超えるときや、枠に入らないときに終える。最後の
  イベントを受けてから 1 秒たっていないストリームは、代わりにそのイベントを飛ばす。
- ストリームは 30 秒以内に送り待ちのない状態に戻らなければ終える。少しずつしか読まないクライアントが、出力を数えさせ
  続けることはできない。
- 送り待ちを持ったまま終えたストリームは、その接続をすぐに切る(HTTP/2 では同じ接続のほかのリクエストも)。持っていた
  出力はすぐに空き、ブラウザーはつなぎ直す。スクリプトが閉じたストリームは、同じ 30 秒の間、残りを送れる。
- 枠がいっぱいのとき、送り終えたストリームは、送り待ちを 1 秒以上持っているストリームの接続を切り、空きを最大 0.2 秒
  待つ。それでも空かなければそのイベントを飛ばす(配信は最大 1 回)。終えたストリームと飛ばしたイベントは
  `phv_rejected_total` の `output` として数える。読まないクライアントが、読むクライアントのストリームを終わらせることも、
  ページを遅らせることも、読むクライアントに約 1 秒より長くイベントを逃させることもない。

#### サーバープロセスをまたぐ publish()

ストアがなければ、`publish()` が届くのは自分のサーバープロセスの接続だけである。ロードバランサーの後ろで複数の
プロセスやマシンを動かすサイトでは、プロセスの間でメッセージを運ぶ Redis のデータベースを `[databases]` から指定する。

```toml
[databases.cache]
url_env = "REDIS_URL"

[security]
secret_env = "PHV_SECRET"     # 必須: メッセージはこれで署名する

[server]
publish_store = "cache"
```

- プロセスは、スクリプトが publish したものを、ストアがないときと同じくまず自分の接続に届け、それからほかのプロセス
  向けのキューに積む。スクリプトが Redis を待つことはない。キューは 10,000 メッセージ、32 MB まで。いっぱいのとき、
  または Redis に届かないときは、ほかのプロセス向けのメッセージを捨てる(`phv_publish_total{result="dropped"}` で
  数え、警告は 1 分に 1 回まで)。
- 各プロセスはキューを Redis に送り(PUBLISH をパイプラインで)、自分専用の購読用の接続を 1 本持つ。届いたメッセージは、
  そのトピックに参加したこのプロセスの接続に渡す。切れた接続は 1 秒後、次は 2 秒、4 秒、…最大 30 秒後に開き直す。
  その間 `/ready` は 503 を返す(「ヘルスチェックとメトリクス」)。
- **配信は最大 1 回(at-most-once)。** プロセスが Redis につながっていない間に publish されたメッセージも、購読する前に
  送られたメッセージも、そのプロセスの接続には届かない。Redis は遅れすぎた購読者を切る(`client-output-buffer-limit
  pubsub`、既定で 32 MB)。プロセスはつなぎ直し、その間に送られたものは失われる。失ってはならないものはデータベースに保存し、`publish()` は何かが変わったことを知らせるのに使う。
- メッセージは `secret_env` から導いた鍵で署名し、送ったプロセスの ID、送った時刻、メッセージごとに増える番号を持つ。
  プロセスは自分のメッセージを読み飛ばす。自分の時計と 60 秒より離れた時刻のものと、最後に購読した時(接続が切れて
  購読し直した時も)より前に送られたもの(このプロセスに届くはずのなかったもの。送り手の時計を、このプロセスから
  見た時刻で判断する)は捨て、`phv_publish_total` で `clock` として数える。送り手のプロセスごとに最初の 1 回だけ
  警告をログに出す(サーバーの時計を合わせておくこと)。
  同じプロセスから受け取ったことのあるものやそれより古いもの、サイトの鍵で署名されていないもの、形が正しくないもの、`publish()` の規則(トピックは制御文字なしの 1〜200 バイト、
  本文は 1 MB まで)に反するもの、それより長いものを捨てる(`invalid` として数える)。長すぎるメッセージは届くそばから
  読み捨て、メモリに持たない。チャネルの名前も鍵から導くので、Redis サーバーを共有するサイトどうしは互いのメッセージを
  見ない。スクリプトが自分で Redis に publish することはできない(`std.db` に PUBLISH はない)。
- Redis サーバーに書き込める者は、メッセージを落としたりストアを止めたりはできる。PHV には専用の Redis ユーザーを与え、
  別のマシンのサーバーには `rediss://` の URL を使うこと。

### WebSocket (std.net)

WebSocket のサーバーはデータベースと同じく `phv.toml` で名前を付けます。スクリプトは名前だけを使うので、パスワードを含む
URL(`ws://host:port/secret`)がコードに現れることはなく、スクリプトがそれ以外の場所に接続することもできません。

```toml
[websockets.game]
url_env = "GAME_WS_URL"       # ws:// または wss://
# url = "ws://127.0.0.1:28016/secret"   # または URL そのもの(ローカル開発用)
# tls = false                 # リモートのホストへの ws:// を許す(非推奨)
```

```phv
import std.net.WebSocket;

$ws = WebSocket::connect("game");
$ws->send(json_encode(["Identifier" => 1, "Message" => "status"]));
$reply = $ws->receive(5.0);       // ?string: 5 秒以内にメッセージが来なければ null
$ws->close();
```

- `connect()` のたびに新しい接続を開き、`close()` かスクリプトの終了で閉じる。リクエストの間で共有されるものはない。
- 送受信できるのはテキストメッセージだけ。サーバーからのバイナリメッセージは `NetworkError`。ping には受信中に応答する。
- `receive()` が待つのはスクリプトの時間制限まで。サーバーが ping を送り続けたり、メッセージを 1 バイトずつ送ったりしても
  同じ(読み書きはすべて期限で止まる)。メッセージは最大 16 MB、1 つのスクリプトが同時に持てる接続は 16 まで。
- **`localhost` 以外のホストには TLS(`wss://`)が必要**で、サーバー証明書は公開 CA で検証する。`tls = false` でそのホストへの
  `ws://` を許す。
- 接続の失敗、サーバーによる切断、ネットワークのエラーは `NetworkError`。メッセージに URL は出ない。

### HTTP クライアント (std.net)

HTTP のサービスも `phv.toml` で名前を付けます。スクリプトが書くのは名前とパスだけで、ベース URL と API キーは設定に残るので
コードに現れることはなく、スクリプトがそれ以外のホストにリクエストを送ることもできません。

```toml
[http.weather]
url = "https://api.example.com/v1"                 # または url_env = "WEATHER_URL"
headers = { "User-Agent" = "my-site" }             # すべてのリクエストに付けるヘッダー
headers_env = { Authorization = "WEATHER_AUTH" }   # 値を環境変数から取るヘッダー(API キー)
# tls = false                                      # リモートのホストへの http:// を許す(非推奨)
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

| メソッド | 送るもの |
|---|---|
| `get($path, $query = [], $headers = [])`、`delete(...)` | 本文のないリクエスト。`$query` は `?k=v&...` として付く |
| `post($path, $body = "", $headers = [])`、`put(...)`、`patch(...)` | `$body` をそのまま |
| `postJson($path, $data, $headers = [])`、`putJson(...)` | `json_encode($data)` を `Content-Type: application/json` で |
| `postForm($path, $fields, $headers = [])` | フォーム(`application/x-www-form-urlencoded`) |
| `request($method, $path, $body = "", $headers = [], $query = [])` | GET、HEAD、POST、PUT、PATCH、DELETE、OPTIONS |

結果は `HttpResponse` で、`status`(int)、`headers`(`map<string, string>`。名前は小文字、同じヘッダーが複数あれば `, ` で
つなぐ)、`body`(string)、`ok()`(2xx)、`header($name)`(`?string`、大文字小文字は区別しない)、`json()`(本文が JSON で
なければ `ValueError`)を持ちます。

- パスは `/` で始め、ベース URL のパスの後ろに付く。そこから出られるパスは、何も送る前に `ValueError` にする:`//host`、
  `.` と `..` のセグメント(パーセントエンコードしたもの、`%252e` のように二重にエンコードしたもの、`..;` のように `;` が
  続くものも)、エンコードしたスラッシュ、16 進 2 桁が続かない `%`、`?` と `#`(クエリは `$query` で渡す)。ASCII 以外の文字は
  パーセントエンコードして送る。
- ヘッダーの名前と値を検査する(改行は不可)。`Host`、`Content-Length`、`Transfer-Encoding`、`Connection` など
  メッセージの区切りに関わるヘッダーは PHV が付ける。`phv.toml` の `headers` はスクリプトで置き換えられるが、`headers_env` の
  ものは置き換えられない(別の書き方でも不可。CGI のサーバーは `_` と `-` を同じに読むので、`_` は `-` とみなす)。
  ベース URL のクエリにあるパラメーターは、`$query` でもう一度渡せない。サーバーが同じ名前と読む別の書き方でも渡せない:
  PHP は `.`、空白、後ろに `]` のない `[` を `_` と読み、先頭の空白と NUL から後ろを捨て、`name[...]` を `name` と読む。
  ASP.NET は大文字と小文字を区別しない。名前はもう一度パーセントデコードしても比べる。ベース URL に `account_id` が
  あれば、`account.id`、`account id`、`account_id[]`、`Account_ID` も `ValueError` になる。
- エラーもレスポンスとして返る(4xx や 5xx で例外にはならない)。**リダイレクトはたどらない**:3xx はその `Location`
  ヘッダーとともにそのまま返す。
- **`localhost` 以外のホストには TLS(`https://`)が必要**で、サーバー証明書は公開 CA で検証する。`tls = false` でそのホストへの
  `http://` を許す。
- 同じホストへのリクエストの間で接続を開いたままにする(ホストごとに 8 本、全体で 256 本まで、使われずに 30 秒たつと閉じる)。
  残すのは、レスポンスが枠組みどおりに終わり(`Content-Length`、またはあとに何もない chunked の本文)、サーバーが閉じるよう
  求めなかった接続だけ。使われていない間に何かが届いた接続は使わない。残した接続を使うのは
  `GET`・`HEAD`・`OPTIONS` だけで、その間にサーバーが閉じていた接続では、答えが 1 バイトも来ていないときだけ新しい接続で
  1 回だけ送り直す。
- 待つのはスクリプトの時間制限まで(制限がなければ 30 秒)。サーバーが何を送っても
  同じで、名前解決と読み書きはすべてその期限で止まり、途中の応答(`1xx`)は 10 個までしか読み飛ばさない。リクエストとレスポンスの
  本文は最大 16 MB。本文は UTF-8 のテキストとして読む。ホスト名を引いたアドレスは 30 秒間保持し(引けなかったことは
  保持しない)、同じ名前を同時に引くリクエストは 1 回の名前解決を共有する。
- 接続の失敗やネットワークのエラーは `NetworkError`。メッセージにはサービスの名前が出るが、URL、ホスト名、ヘッダーは出ない
  (エラーは「the server's TLS certificate is not valid」のような決まった言葉で表す)。
  `url_env` や `headers_env` の環境変数が設定されていないことはリクエストを送るときに報告するので、`phv check` は本番の
  キーなしで使える。

### OAuth 2.0 と OpenID Connect によるサインイン(std.oauth)

```toml
[oauth.google]
issuer = "https://accounts.google.com"            # OpenID Connect: エンドポイントは issuer のディスカバリー文書から
client_id_env = "GOOGLE_CLIENT_ID"                # または client_id
client_secret_env = "GOOGLE_CLIENT_SECRET"        # パブリッククライアント(PKCE だけ)なら書かない
redirect_uri = "https://example.com/auth/google"  # または redirect_uri_env。プロバイダーに登録したもの
scopes = ["openid", "email", "profile"]           # 既定 ["openid"]

[oauth.github]                                    # OpenID Connect のない OAuth 2.0
authorize_url = "https://github.com/login/oauth/authorize"
token_url = "https://github.com/login/oauth/access_token"
client_id_env = "GITHUB_CLIENT_ID"
client_secret_env = "GITHUB_CLIENT_SECRET"
client_auth = "post"                              # シークレットをリクエストの本文で送る("basic": HTTP Basic)
redirect_uri = "https://example.com/auth/github"
scopes = ["read:user"]
```

```phv
// public/login/google.phv
import std.oauth.OAuth;
OAuth::provider("google")->redirect();               // プロバイダーへ(302)

// public/auth/google.phv: リダイレクト URI
import std.http.{response, session};
import std.oauth.{OAuth, OAuthError};
try {
    $signIn = OAuth::provider("google")->callback();
} catch (OAuthError $e) {
    response()->status(400);
    echo "サインインできませんでした: ", $e->getMessage();   // $e->error: "access_denied"、"invalid_state" など
    return;
}
session()->regenerate();
session()->set("user", "google:" . ($signIn->subject() ?? ""));   // プロバイダーでの訪問者の変わらない ID
$email = $signIn->verifiedEmail();                                // ?string: プロバイダーが確認したときだけ
response()->redirect("/");
```

- PKCE(S256)付きの認可コードフロー。`redirect()`(または URL を返す `authorizationUrl()`)は、ランダムな `state`、
  OpenID Connect の `nonce`、PKCE の verifier を訪問者のセッションに 10 分間・1 回限りで保存する。1 つのセッションで
  同時に始められるサインインは 4 つまで(タブ)。`["prompt" => "select_account"]` のような追加のパラメータを渡せるが、
  フローそのもののパラメータは渡せない。
- `callback()` は、このセッションがこのプロバイダーに対して始めた `state` だけを受け付ける。そのため別のブラウザで
  サインインを終えることはできず、ほかのサイトが訪問者をそのサイト自身のアカウントでサインインさせることもできない。
  別の issuer を名乗る `iss` パラメータは(エラーの応答でも)拒否し(RFC 9207)、プロバイダーのエラー
  (`?error=access_denied`)は `OAuthError` として渡し、コードをトークンエンドポイントで verifier とクライアント
  シークレットとともにトークンに交換する(HTTP Basic。`client_auth = "post"` のとき、またはディスカバリー文書が
  `client_secret_post` だけを許すときは本文)。アクセストークンは Bearer トークンでなければならない(`token_type` の
  大文字小文字は問わない。`token_type` のない応答は Bearer とみなす)。ほかの種類は `unsupported_token_type` で拒否する。
  サインインはセッションに保存するので、`redirect()`、`authorizationUrl()`、`callback()` は `response()->flush()` より
  前に呼ぶ。後で呼ぶと `ValueError` を投げる(flush の後の callback は state を使い切れない)。
- OpenID Connect では、ID トークンは、プロバイダーの JWKS の鍵(RS256・ES256・EdDSA。std.jwt の `KeySet::fetch()` と同じく
  保持する)で署名され、issuer をそのまま、このクライアントを audience として(audience が複数なら `azp` としても)名乗り、
  サインインの nonce・`sub`・`exp`・`iat` を持ち、いま有効(60 秒の猶予)でなければならない。そうでなければ `callback()` は
  `invalid_id_token` の `OAuthError` を投げる。`SignIn` には `claims`、`subject()`、`claim($name)`、`verifiedEmail()`、
  `accessToken`、`refreshToken`、`expiresIn`、`scope`、`idToken` がある。OpenID Connect がなければ `claims` は空で
  `idToken` は null(検証していない ID トークンは渡さない): アクセストークンでプロバイダーの API に問い合わせる
  (`Authorization: Bearer` ヘッダーを付けた `HttpClient`)。
- 利用者はメールアドレスではなく、プロバイダーと `subject()` で識別すること: アドレスは別の人に渡ることがあり、
  プロバイダーが確認したと分かるのは `verifiedEmail()` だけである。ほかのログインと同じく、サインインの後には
  `session()->regenerate()` を呼ぶ。
- ディスカバリー文書(`<issuer>/.well-known/openid-configuration`)は、設定した issuer をそのまま名乗らなければならない。
  1 時間保持する(取り直せないときは最大 1 日、30 秒ごとに再試行)。取得できず、保持しているものもないときは、
  再試行は 10 秒に 1 回までで、その間のサインインはすぐに `NetworkError` を投げる。エンドポイントは、このマシンにあるか
  `tls = false` でなければ TLS が必要。リダイレクト URI は、このマシンになければ TLS が必要。ディスカバリー文書が
  名乗るエンドポイントは、issuer が `https://` なら `https://` でなければならず、このマシンにあってよいのは issuer も
  このマシンにあるときだけ(リモートのプロバイダーが、コードとシークレットをこのマシンのポートへ平文で送らせることは
  できない)。クライアントシークレットは環境変数(スクリプトからは読めない)から読み、決して表示しない。変数が
  設定されていなければ、サインインを始めたときに報告する。
- プロバイダーや訪問者による失敗は `OAuthError`(`$e->error` は OAuth のエラーコード。プロバイダーからの文字列は
  表示できる ASCII に切り詰める)、プロバイダーに届かないときは `NetworkError`、phv.toml やスクリプトの誤りは
  `ValueError` を投げる。`redirect()` と `callback()` はページの中でだけ使える。

### 入力の検証(std.validate)

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
    $errors = $v->errors();          // map<string, list<string>>: フィールドごとのメッセージ
    $first = $v->first("email");     // ?string
}
$name = $v->value("name");           // ?string。前後の空白を除く(空なら null)
```

規則: `required`、`minLength`、`maxLength`(文字数)、`email`、`url`(http と https)、`int` と `number`(`min` / `max`
付き)、`in`、`same`、独自の検査には `check`。どの規則も最後の引数 `$message` で英語の既定のメッセージを置き換えられる。
`required` 以外の規則は、フィールドがないか空なら通る。規則は前後の空白を除いた値を検査するので、保存するのは検査した
値である `$v->value("name")` にする。`is_email($s)` はメールアドレスの検査だけを行う。

### メール(std.mail)

```toml
[mail.default]
url_env = "SMTP_URL"     # smtps://user:password@smtp.example.com(TLS)、または smtp://...:587(STARTTLS)
from = "Example <noreply@example.com>"
# tls = false            # ほかのマシンのサーバーに TLS なしで送る(推奨しない)
```

```phv
import std.mail.Mail;

Mail::to($email)
    ->subject("Welcome")
    ->text("Thanks for signing up.")
    ->html(render("views/mail/welcome.phv.html", name: $name))
    ->send();                        // [mail.default] で送る。[mail.alerts] なら ->send("alerts")
```

- スクリプトが送れるのは `[mail]` に書いたサーバーだけ。`smtps://` は最初から TLS、`smtp://` は `STARTTLS` で切り替え、
  ほかのマシンのサーバーはそれを提供しなければならない(`tls = false` のときを除く)。ユーザー名とパスワードは、
  ほかのマシンへは TLS なしでは決して送らない。
- アドレスは単純な `name@domain`(表示名なし、改行なし)で、件名にも改行は入れられない(`ValueError`)ので、フォームの
  値でヘッダーや宛先を増やすことはできない。宛先は 1〜50 件、メッセージは 10 MB まで。テキストの部分は常に送られ、
  `html()` で HTML の版を加える。
- 送信はサーバーを待ち(スクリプトの時間制限か 30 秒まで)、失敗すると `NetworkError` を投げる。

### 正規表現(std.regex)

```phv
import std.regex.Regex;

$date = new Regex('^(\d{4})-(\d{2})-(\d{2})$');       // 単一引用符ならバックスラッシュがそのまま
$date->test("2026-11-05");                                // bool
$date->match("2026-11-05");                               // ?list<string>: ["2026-11-05", "2026", "11", "05"]
(new Regex('\d+'))->matchAll("1 22 333");                 // list<list<string>>
(new Regex('(?<y>\d{4})-(?<m>\d{2})'))->replace($s, '${m}/${y}');
$words->replaceWith($s, fn(list<string> $m): string => $m[1]->upper());
(new Regex('\s*,\s*'))->split("a , b,c");                 // ["a", "b", "c"]
Regex::escape("1+1=2?");                                   // その文字列そのものに当てはまるパターン
```

文法は Rust の `regex` クレートのもの: `(?i)` で大文字小文字を区別しない、`(?<name>...)` で名前付きグループ、`\d \w \s`、
`\p{L}` などの Unicode のクラス。先読み・後読みと後方参照はない。その代わり壊滅的なバックトラックはなく、検索にかかる
時間は最大でもテキストの長さとコンパイルしたパターンの大きさの積に比例する(たいていはずっと短い)。

- 検索そのものは途中で止められないので、最悪の場合(パターンの状態数 × テキストの長さ)にスクリプトの残り時間で
  終わらない検索は、始める前に `RuntimeException` にする。普通のパターンなら数 MB を検索できる(状態が約 1,000 の
  メールアドレスのパターンで、30 秒あれば約 3 MB)。Unicode のクラスの長い繰り返し(`\w{40}`)のような非常に大きな
  パターンでは、より短いテキストまで。そのため、訪問者からのパターンでワーカーが制限時間を超えて止まることはない。
- 不正なパターンは `ValueError`。パターンは 64 KB まで、コンパイル後は 2 MB まで、グループは 1,000 個まで。グループの
  検索に 16 MB を超えるメモリ(状態数 × グループ数)が要るパターンも拒否する。
- 結果は作りながらメモリ制限に数える。
- サーバープロセスは最近使ったパターンを、すべてのリクエストのためにコンパイル済みのまま保持する(64 個、32 MB まで。
  いちばん長く使われていないものから捨てる)。

`match()` ではグループ 0 がマッチ全体で、関わらなかったグループは `""`。

### 日付と時刻(std.time)

```phv
import std.time.{DateTime, Duration};

$meeting = DateTime::of(2026, 11, 5, 10, 0, 0, "Asia/Tokyo");
$meeting->toIso();                                    // "2026-11-05T10:00:00+09:00"
$meeting->inZone("Europe/Berlin")->format("H:i T");   // "02:00 CET"
$meeting->plus(Duration::hours(36));                  // 正確な時間の長さ
$meeting->addDays(1)->addMonths(1);                   // 暦の上で進める(現地時刻はそのまま)
$meeting->diff(DateTime::now())->totalHours();
DateTime::parse("2026-11-05 09:30", "Asia/Tokyo");    // ?DateTime
```

`DateTime` は、ある瞬間(`timestamp`、1970-01-01 UTC からの秒数)を IANA のタイムゾーン(`zone`。`"Asia/Tokyo"`、
`"America/New_York"`、`"UTC"` など)で見たもの。値は変わらず、メソッドは新しい値を返す。

| メンバー | 説明 |
|---|---|
| `DateTime::now($zone = "UTC")`、`DateTime::fromTimestamp($ts, $zone = "UTC")` | |
| `DateTime::of($y, $m, $d, $h = 0, $i = 0, $s = 0, $zone = "UTC")` | 存在しない日付(2 月 30 日)は `ValueError` |
| `DateTime::parse($text, $zone = "UTC"): ?DateTime` | `2026-11-05`、`2026-11-05 10:00[:30]`(`$zone` の現地時刻)、またはオフセットか `Z` 付きの RFC 3339 |
| `year()`、`month()`、`day()`、`hour()`、`minute()`、`second()`、`weekday()`(1 = 月曜)、`offset()` | そのタイムゾーンでの値。`offset()` は UTC から東向きの秒数 |
| `format($f)`、`toIso()` | `date()` と同じ文字。`T` はタイムゾーンの略称(`JST`、`EDT`) |
| `inZone($zone)`、`plus(Duration)`、`minus(Duration)`、`startOfDay()` | |
| `addDays($n)`、`addMonths($n)`、`addYears($n)` | 現地時刻を保ったまま暦の上で進める。`addMonths` は日を保ち、短い月では月末にする(1 月 31 日 + 1 か月 = 2 月 28 日) |
| `diff($other): Duration`、`isBefore($other)`、`isAfter($other)`、`equals($other)` | 瞬間として比べる(タイムゾーンは問わない) |
| `Duration::seconds/minutes/hours/days/weeks($n)`、`plus`、`minus`、`totalMinutes()`、`totalHours()`、`totalDays()`、`toText()` | `toText()` は `1d 2h 3m 4s` のような形 |

- 時計を戻すときに 2 回ある現地時刻は早いほう、時計を進めるときに存在しない現地時刻は飛ばした後(02:30 は 03:30)になる。
- `plus(Duration::days(1))` はちょうど 24 時間後、`addDays(1)` は翌日の同じ現地時刻で、時計の切り替えをまたぐと両者は異なる。
- 不明なタイムゾーンは `ValueError`。

### バックグラウンドジョブ(std.jobs)

ページを待たせたくない仕事(メールの送信、レポートの作成、遅いサービスの呼び出し)は**ジョブ**にする。`jobs/` に置いた
スクリプトで、`phv serve` がリクエストの外で実行する。

```toml
[jobs]
store = "db:main"     # "memory"(既定)、または [databases] の PostgreSQL・MySQL・Redis
# dir = "jobs"        # ジョブのスクリプトのディレクトリ(public/ の外)
# workers = 2         # 1 つのサーバープロセスが同時に実行するジョブ(1〜64)
# time_limit = 300    # 1 回の実行にかけてよい秒数(1〜3600)
# max_attempts = 5    # 失敗し続けるジョブをあきらめるまでの実行回数(1〜100)
# retry_delay = 30    # 失敗したジョブを再び実行するまでの秒数。毎回 2 倍(最大 1 時間)
# run = true          # false: phv serve はジョブを追加するだけ。実行は `phv jobs run`

[[schedule]]
job = "cleanup"
cron = "0 3 * * *"        # 分、時、日、月、曜日
timezone = "Asia/Tokyo"   # 既定: UTC
```

```phv
// public/signup.phv
import std.jobs.enqueue;
enqueue("send_welcome", json_encode(["user" => $id]));      // ?string: ジョブの ID
enqueue("report", "", delay: 3600);                          // 早くても 1 時間後
enqueue("sync", (string)$id, key: "sync:{$id}");             // 待っているものがあれば追加しない(null)
```

```phv
<?phv
// jobs/send_welcome.phv
import std.jobs.job;
$j = job();                 // Job: name、id、payload、attempt(1 回目は 1)
$data = json_decode($j->payload);
```

- ジョブはスクリプト `<dir>/<name>.phv` で、名前は英小文字・数字・`_`。`enqueue()` は、存在しないジョブ、64 KB を超える
  payload、30 日を超える delay、200 バイトを超える key で `ValueError` を投げ、ストアに待ち・実行中のジョブが 100,000 件
  かその payload が 1 GB(メモリでは 10,000 件か 64 MB)あれば、`transaction()` の中でも `RuntimeException` を投げる。
  `[jobs]` のないプロジェクトでも `RuntimeException`。ストアを待つのは、スクリプトがまだ動ける間だけ(長くても 10 秒)で、
  データベースサーバーの上でもそこで止める: ストアの文は、ロックを待つ間も含めて、そこで止まる(PostgreSQL は
  `statement_timeout`、MySQL は `innodb_lock_wait_timeout` と `KILL QUERY`)。`dir` はプロジェクトの
  ディレクトリで、リンクは使えない。メモリのストアがあきらめたジョブとして残すのは 16 MB まで、終わったジョブの key は
  100,000 個まで。
- `phv serve` は、各プロセスで `[jobs] workers` 個のジョブを専用のスレッドで同時に実行する。ジョブがページのワーカーを
  使うことはない。実行ごとに新しい VM、`time_limit`、`[security] memory_limit_mb`、phv.toml の許可を、ページと同じく
  持つ。`request()` と `session()` は空で、`job()` はジョブの中でだけ使える。ジョブは `publish()`(WebSocket の接続へ)や
  ほかのジョブの `enqueue()` ができる。実行中に出力したものは捨てる。
- **ジョブは少なくとも 1 回実行される。** 実行はリース(`time_limit` + 30 秒)付きでジョブを取る。例外を投げたり上限に
  達したりしたジョブは `retry_delay` 秒後、次はその 2 倍後、…にもう一度実行し、`max_attempts` 回実行したらあきらめて、
  エラーとともに 1 週間(最大 10,000 件)残す。実行中にプロセスが止まったジョブは、リースが切れたら取り直す。
  2 回実行されても害のないジョブを書くこと(または `key` を付ける)。
- `key` を付けると 1 回だけ追加する。同じ名前と key のジョブが待っている・実行中・あきらめられた間と、終わってから 1 日の
  間、`enqueue()` は `null` を返す。ストアが覚えておく終わったジョブの key は 100,000 個まで(新しいもの)。同じ key の
  ジョブをほかの終わっていないトランザクションが追加しているときは、`enqueue()` はその終わりを待つが、スクリプトが
  まだ動ける間(長くても 10 秒)だけ待ち、そのあとは `DatabaseError`(「the key is held by a transaction that has not
  ended」)を投げる。10〜200 ミリ秒ごとに key を見直し、その間は接続を持たないので、こうして待つ間も、ほかのジョブを
  追加する接続、ジョブを実行する接続、スケジュールのジョブを追加する接続を使わない。サイトの持たれた key を同時に
  待てるのは 1 プロセスにつき 8 つのスクリプトまでで、それを超えたものはすぐに同じエラーを投げる。
- PostgreSQL と MySQL では、ストアのデータベースへの `transaction()` の中の `enqueue()` は、トランザクションと一緒に
  追加する(コミットされたときだけ。MySQL で結び付くのは `transaction()` だけで、SQL で送った `BEGIN` は結び付かない)。
  ジョブはサイトの名前空間のテーブル `phv_jobs_<hash>`(「セッション」を参照。名前空間のないサイトは `phv_jobs`)に
  置き、初めて要るときに作って `SELECT ... FOR UPDATE SKIP LOCKED` で取る。Redis では `phv:jobs:<hash>:` の下に置く。
  同じストアを使うサイトのサーバープロセスとマシンはジョブを共有し、同じデータベースのほかのサイトからは見えない。
  サイトに名前を付けたとき共有のテーブルに残っていたジョブは取らない: `phv_jobs`(Redis では `phv:jobs:` の下)に
  残ったままで、`phv jobs list` が待機中・実行中・失敗したジョブがそこに何件あるかを知らせる。それをサイトのテーブルに
  移すコマンドは後のリリースで加わる。同時に動くトランザクションも 100,000 件を超えない:
  トランザクションの中の `enqueue()` は、サイトのジョブのロック(PostgreSQL は advisory lock、MySQL はテーブル
  `phv_jobs_lock` の行)を取り、トランザクションが終わるまで持つので、同じサイトにジョブを追加するトランザクションは
  互いを待つ。トランザクションの外の `enqueue()` も、キューが上限まで 10,000 件(か 256 MB)以内のときは待つ。それより
  上限から遠いときは待たないので、外からジョブが追加されている間に 10,000 件より多く追加するトランザクションは、
  10,000 件を超えた分だけ上限を超えうる。上限の近くでは、ジョブを追加したトランザクションが終わるまで、そのサイトへの
  ほかのジョブの追加はすべて待つので、`enqueue()` はトランザクションの最後に呼ぶこと。ストアを数えるのは、
  トランザクションが初めて追加するときと、トランザクションの外では 1 秒に 1 回で、その数はテーブル `phv_jobs_counts` に
  置く。1.1.1 より前に作ったジョブのテーブルには、プロセスが初めて使うときに列 `size`(各 payload のバイト数)を足す。
  そのときほかのトランザクションがテーブルを持っていれば 1 分後にもう一度試し、それまでそのジョブのバイト数は数えない。
  `store = "memory"` ではジョブはサーバープロセスの中にあり、止めると消える。
- **スケジュール**: `[[schedule]]` は、`cron` の式が `timezone` で一致する分ごとに、そのジョブを(空の payload で)
  追加する。フィールドは `*`、数、範囲 `a-b`、間隔 `*/n`、列挙 `a,b`。曜日は 0〜7(日曜は 0 と 7)。日と曜日の両方を
  指定したときは、どちらかが一致すればよい。各プロセスが分をキーにして追加するので、共有のストアなら何プロセス
  動いていても 1 回だけ追加される。`store = "memory"` では各プロセスがそれぞれ実行する。スケジュールのジョブは、
  スクリプトが使わないストアの接続で追加する。ジョブを追加できなかった分(ストアが時間内に答えなかった)は、そのあとの
  分と一緒に 5 秒ごとに、長くても 1 時間もう一度試す(警告は 1 回だけ出す)。どのプロセスも動いていなかった
  分を後から実行することはない。`timezone` で時計が切り替わる日は、その日にない時刻は実行せず、2 回ある時刻は 2 回
  実行する。スケジュールは 100 個まで。
- `phv jobs run [dir]` は、ページを配信せずにプロジェクトのジョブ(とスケジュール)を Ctrl-C か SIGTERM まで実行する
  (ワーカー用のマシン向け)。`[jobs] run = false` なら `phv serve` はジョブを追加するだけになる(`phv jobs run` と、
  ビルドしたサイトの `--jobs-only` はそれでも実行する)。ビルドしたサイトのジョブは、ビルドに入ったもの。
  `phv jobs list [--failed]` は共有のストアのジョブを表示し、`phv jobs retry <id>` はあきらめたジョブをキューに戻す。
- コンパイルできないジョブがあると、`phv serve`・`phv jobs run`・`phv build` は止まる。サーバーを止めるとき、実行中の
  ジョブは `shutdown_timeout` の残りの時間を使える。終わらなかったものは、リースが切れたあとほかで実行される。
- payload はそのまま保存する。秘密にすべきものは暗号化(`encrypt()`)すること。訪問者のためにジョブを追加するページには
  レート制限(`rate_limit()`)をかけ、訪問者がキューを埋められないようにする。

### 非同期 I/O(std.async)

```phv
import std.async.{await, await_all};
import std.net.HttpClient;
import std.db.Database;

$api = HttpClient::connect("weather");
$today = $api->getAsync("/today");          // すぐ送る
$tomorrow = $api->getAsync("/tomorrow");    // 同時に送る
$db = Database::connect("main");
$posts = $db->queryAsync(sql"SELECT id, title FROM posts");

$t = await($today);                         // HttpResponse
foreach (await($posts) as $row) { /* ... */ }
$all = await_all([$api->getAsync("/a"), $api->getAsync("/b")]);   // list<HttpResponse>
```

- 名前が `Async` で終わるメソッドは処理を裏で始め、すぐに `Future<T>` を返す: `HttpClient` の `getAsync`、`postAsync`、
  `postJsonAsync`、`requestAsync`、`Database` の `queryAsync`、`executeAsync`、`Redis` の `getAsync`、`setAsync`。
  処理の間もスクリプトは先へ進む。
- `await($f)` は結果(`T`)を待つ。処理が失敗していればそのエラー(`NetworkError`、`DatabaseError`)を投げる。もう一度
  `await` しても同じ結果。`await_all($fs)` はリストのすべてを待つ。リクエスト自体の誤り(設定されていない環境変数、
  許されないパス)は、処理を始めるときに投げる。
- 処理と、それを待つのは、スクリプトの時間制限まで(制限がなければ HTTP と Redis は 30 秒、SQL は 300 秒)。1 つの
  スクリプトが同時に動かせるのは 16 個まで、1 つのデータベースではその `pool_size` の半分(最低 1)までで、それを
  超えて始めると `RuntimeException`。
- 処理の結果に使えるのは、始めたときにスクリプトに残っているメモリの 16 分の 1(HTTP の応答は最大 16 MB)。それより
  大きい結果はエラー。`await` したときに、スクリプトのメモリ制限に数える。
- データベースサーバーの `queryAsync` / `executeAsync` はプールから自分の接続を取るので、スクリプトのトランザクションの
  外で動く。`pool_size` に余裕を持たせる。SQLite ではその場で実行する。
- 始めたのに `await` されなかった処理も、スクリプトの終わりに(長くてもその時間制限まで)終わるのを待つ。リクエストが
  終わったあとに、スレッドと接続を持ったまま処理が残ることはない。
- 処理は、サーバープロセスがリクエストの間で共有するスレッドで動く。空いているスレッドがなければ新しく始め(プロセス
  全体で同時に 1,024 個まで)、数秒間仕事がなければ終わる。
- `new Future(fn(): T => ...)` で任意の処理を後回しにできる。最初に `await` したときに 1 回だけ動く。`await` されなかった
  後回しの処理は動かない。

### 日付・暗号・エンコード

| 関数 | 説明 |
|---|---|
| `date($format, ?$ts)` / `gmdate(...)` | PHP と同じ書式(`Y-m-d H:i:s`、`D, d M Y`、`c`、`U`、`T` など)。`date` はサーバーのローカル時刻。タイムゾーンは std.time を使う |
| `mktime($h, $i, $s, $m, $d, $y)`, `strtotime($s): ?int`, `checkdate($m, $d, $y)` | `strtotime` は ISO 形式と `now`/`today`/`tomorrow`/`yesterday` |
| `random_int($min, $max)`, `random_token($bytes = 32)` | OS の暗号論的乱数(`rand()` は暗号用途に使わない) |
| `password_hash($pw)`, `password_verify($pw, $hash)` | Argon2id |
| `hash("sha256" \| "sha384" \| "sha512", $data)`, `hash_hmac($algo, $data, $key)`, `hash_equals($a, $b)` | `hash_equals` は比較時間が内容に依存しない |
| `totp_secret()`, `totp_code($secret, ?$time)`, `totp_verify($secret, $code): ?int` | 認証アプリの 2 段階認証コード(TOTP、RFC 6238: HMAC-SHA1、6 桁、30 秒)。秘密鍵は 160 ビットの乱数を Base32 にしたもの。`totp_verify` は前後 1 ステップの時刻のずれを許し、比較時間は内容に依存せず、一致した時刻ステップ(なければ `null`)を返す。これを保存して、それより新しくないステップを拒めば、同じコードを二度使えない。保存は、新しいステップのときだけ成功する 1 つの文でデータベースに行う(`UPDATE ... SET step = {$step} WHERE id = {$id} AND step < {$step}` のあと影響を受けた行数を見る)。セッションに保存すると、同じコードを同時に送った 2 つのリクエストがどちらも通ることがある |
| `qr_svg($text): Html` | QR コードを `<svg>` 要素で返す(`otpauth://` の URI など)。中身は模様だけで文字列そのものは含まないので、そのまま出力してよい |
| `base64_encode`, `base64_decode: ?string`, `urlencode`, `rawurlencode`, `urldecode`, `http_build_query` | |
| `json_decode($s): any` | 深さ 128 まで |
| `encrypt($text, $key)`、`decrypt($box, $key): ?string` | ChaCha20-Poly1305。メッセージごとに鍵を導出し(ランダムな salt で HKDF-SHA256)、nonce もランダム。結果は base64url のテキスト。`decrypt` は、テキストが変えられていたり暗号化したときと鍵が違ったりすると `null` |
| `sign_token($data, $key)`、`sign_token($data, $key, $seconds)`、`verify_token($token, $key): ?string` | HMAC-SHA256 の署名付きのデータ。`$seconds` を渡すとその秒数だけ有効(パスワード再設定などメールのリンク用)。データは base64url にしただけで読めるので、秘密にするものは暗号化する。`verify_token` はデータを返し、トークンが変えられている、別の鍵で署名されている、期限切れのときは `null` |
| `jwt_encode($claims, $key)`、`jwt_decode($token, $key): ?map<string, any>` | HS256 だけの JSON Web Token(RS256・ES256・EdDSA と鍵セットは std.jwt)。ほかのアルゴリズム(`none` を含む)を名乗るトークンは拒否する。`exp` と `nbf` を検査する。std.jwt と同じく、ヘッダーに `crit` を持つトークンと、1 分より先の `iat`(数値)を持つトークンは拒否する。鍵は(ほかの JWT のライブラリと同じく)そのまま使うので、同じ鍵を `hash_hmac()` に使わない: 訪問者のテキストの HMAC が正しい署名になってしまう |

`encrypt`、`sign_token`、JWT の関数の鍵は 32 バイト以上の秘密の文字列(`random_token()` で作れる)。コードには書かず、
環境変数に入れて `getenv` で読む。`encrypt` と `sign_token` は渡された鍵からそれぞれ別の鍵を導出するので、1 つの鍵を
両方に使ってよい。トークンはそのまま比べるので、前後に空白があると無効。

### JSON Web Token(std.jwt)

```phv
import std.jwt.{JwtKey, KeySet};

$key = JwtKey::fromPem(getenv("JWT_PRIVATE_KEY") ?? "", "2026-11");   // 秘密鍵と、その鍵 ID
$token = $key->sign(["sub" => "7", "exp" => time() + 3600]);
$claims = $key->verify($token);                                      // ?map<string, any>
echo $key->publicJwk();                                              // 公開する公開鍵(JWK)

$auth = KeySet::fetch("auth", "/.well-known/jwks.json");             // [http.auth] の JWKS
$claims = $auth->verify($token, issuer: "https://auth.example.com/", audience: "my-api", leeway: 30);
```

- 鍵の種類がアルゴリズムを決める: RSA の鍵は RS256、P-256 の鍵は ES256、Ed25519 の鍵は EdDSA、
  `JwtKey::secret($secret)`(32 バイト以上)は HS256 で署名する。トークンは、そのヘッダーが名乗るアルゴリズムの鍵でだけ
  検査するので、トークンが別の種類の鍵(HMAC のシークレットとして使う公開鍵など)や `none` を選ぶことはできない。
  `crit` を持つトークンは拒否し、ほかの鍵を指すヘッダーのパラメータ(`jku`、`x5u`、`jwk`、`x5c`)は無視する。
- `JwtKey::fromPem()` は秘密鍵(`PRIVATE KEY`(PKCS#8)、`RSA PRIVATE KEY`、`EC PRIVATE KEY`)と公開鍵(`PUBLIC KEY`、
  `RSA PUBLIC KEY`)を読む。RSA の鍵は 2048〜4096 ビットで署名、8192 ビットまで検証できる。暗号化された鍵やほかの種類の
  鍵は拒否する(`ValueError`)。`JwtKey::fromJwk()` は JWK の公開鍵を読む。`alg`、`kid`、`canSign` で鍵の種類が分かる。
  鍵は実行環境の中にだけあり、スクリプトが持つ値にも `dump()` にも秘密鍵は現れない。
- `verify()` はクレームを返す。署名・アルゴリズム・クレームが正しくなければ `null`: `exp` と `nbf`(数値)は `$leeway` 秒
  (0〜300)の猶予付きで検査し、1 分より先の `iat`、`$issuer` を渡したときの `iss`、`aud`(文字列 `$audience`、または
  それを含むリスト)も検査する。`$audience` を渡さないと、`aud` を持つトークンは拒否する: ほかの誰かのために作られた
  トークンである。トークンの文字列は一意ではない: 正しいトークンは、同じく検証を通る別の形にも書ける(ES256 の署名には
  2 つの形がある)。同じトークンの 2 回目の使用を拒否するには、トークンではなく `jti` クレームを覚えておく。
- `KeySet::of([...])` は複数の鍵で検証する(鍵を入れ替える間の古い鍵と新しい鍵)。トークンの `kid` で鍵を選ぶ。
  `KeySet::fromJson()` は JWKS の文書を読み、`toJson()` は鍵セットの公開鍵を書く(`/.well-known/jwks.json` で配るため)。
  JWKS のほかの種類の鍵は飛ばし、`oct` の鍵は決して読まない。
- `KeySet::fetch($service, $path)` は `[http]` の HTTP サービスから(そのヘッダーを付けて)JWKS を取得する。鍵セットは
  サーバープロセス全体で保持し、`Cache-Control: max-age`(1 分〜1 日。なければ 1 時間)より古くなったとき、また
  トークンが鍵セットにない鍵 ID を名乗ったときに取り直す。後者は最初のトークンですぐ、その後は 30 秒に 1 回までなので、
  でっち上げた鍵 ID でサーバーに何度も取得させることはできない。取り直せない鍵セットは最大 1 日使い、30 秒ごとに
  再試行する。取得できず、保持しているものもない鍵セットの再試行は 10 秒に 1 回まで(その間はすぐに `NetworkError`)。
  鍵セットはサービスとその設定したヘッダーごとに保持するので、URL が同じでヘッダー(テナントなど)が違うサービスが
  鍵セットを共有することはない。JWKS は 512 KB までで、スクリプトのメモリと時間の制限の中で読む。読む鍵は 64 個まで、
  保持する鍵セットは 32 個まで。
- `jwt_encode()` と `jwt_decode()`(HS256)も使える(上の表)。

### Content-Security-Policy

ページのレスポンスには既定で次のポリシーが付く(`{nonce}` はレスポンスごとの `'nonce-...'`。`'nonce-{nonce}'` と書いても同じ)。

```
default-src 'self'; script-src {nonce} 'strict-dynamic'; style-src 'self' 'unsafe-inline';
img-src 'self' data: https:; font-src 'self' data:; object-src 'none'; base-uri 'self';
form-action 'self'; frame-ancestors 'self'
```

- テンプレート(`.phv.html` と `.phv` の HTML 部分)に書いた `<script>` タグには、PHV が自動で
  `nonce="..."` を付ける。そのため、自分で書いたインラインスクリプトはそのまま動き、エスケープをすり抜けて
  注入されたスクリプト(`raw()` に入った `<script>` など)は動かない。
- `onclick="..."` のようなイベント属性と `javascript:` の URL は止まる。`addEventListener` を使う。
  テンプレートに書くとコンパイル時に警告が出る(`csp` でインラインのコードを許可しているときは出ない)。
- 文字列から組み立てた `<script>` に nonce が必要なときは `csp_nonce()` を使う
  (`raw("<script nonce=\"" . csp_nonce() . "\">...</script>")`)。
- ページが実行するのは nonce の付いたスクリプトと、それが読み込むスクリプトだけ(`'strict-dynamic'`)。エスケープを
  すり抜けた `<script src>` は、サイトのスクリプトも読み込めない。
- 静的ファイルとエラーページには nonce のない、`script-src 'self'` のポリシーが付く(静的な `.html` のインラインスクリプトは動かない)。
- 変更: `[security] csp = "off"` で付けない。`csp = "default-src 'self'; script-src 'self' {nonce}"` のように
  書くとそのポリシーを使う(`{nonce}` は `'nonce-...'` になる)。

### レート制限

```phv
if (!rate_limit("login:" . ip_group(request()->ip), 5, 60)) {     // 60 秒に 5 回まで
    response()->status(429);
    echo "しばらく待ってからやり直してください。";
    return;
}
// ログインに成功したら
rate_limit_reset("login:" . ip_group(request()->ip));
```

- `rate_limit($key, $max, $seconds)` は、そのキーの今の時間枠でまだ `$max` 回に達していなければ 1 回数えて `true`、
  達していれば `false` を返す。カウンターはサーバープロセスの中で共有され、再起動で消える。
- サーバープロセス(やマシン)の間でカウンターを共有するには、`[databases]` の Redis の名前を
  `[security] rate_limit_store` に書く。`rate_limit()`、`rate_limit_reset()`、`requests_per_minute` はそのサーバーで数え、
  キーは `phv:ratelimit:` で始まり、時間枠とともに消える。サーバーに届かないとき、`rate_limit()` は `false` を返し
  (数えられない制限は拒否する側に倒す)、`rate_limit_reset()` は `DatabaseError` を投げ、`requests_per_minute` は
  プロセスの中で数える。失敗はそれぞれログに出る。失敗したあと 5 秒間はそのサーバーに問い合わせない。その間の呼び出しは
  サーバーを待たずに、すぐ同じように失敗する。
- 同じキーでも時間枠の長さごとに別に数える(1 分 5 回と 1 日 50 回を両方かけられる)。`rate_limit_reset($key)` は
  そのキーの時間枠をすべての長さについて忘れ、ほかのキーには触れない。キーが 10 万個を超えると、
  上限に達していないものから捨てるので、大量のキーを使っても制限中のキーは外れない。
- `requests_per_minute` は IPv6 のアドレスを /64 ごとにまとめて数える。キーも同じようにする。`ip_group($ip)` は IPv6 の
  アドレスなら /64 を、IPv4 ならそのアドレスを返す(`rate_limit("login:" . ip_group(request()->ip), 5, 60)`)。1 人の訪問者が
  自分のネットワークのどのアドレスでも使えるため。
- プロキシの後ろでは `request()->ip` はプロキシのアドレス。訪問者のアドレスをプロキシのヘッダー(Fly.io の `Fly-Client-IP`
  など)から読むのは、そのプロキシの後ろで動くときだけにする。それ以外では、そのヘッダーはクライアントが書いた文字列。
- サイト全体の制限は `[security] requests_per_minute`(§14)。

### ファイルとディレクトリ

| 関数 | 説明 |
|---|---|
| `file_append($path, $text)` | ファイルの末尾に追記する。なければ作る(`write`) |
| `filesize($path)`、`filemtime($path)`、`is_dir($path)` | バイト数、最終更新時刻(Unix 時刻)、ディレクトリかどうか(`read`) |
| `scandir($dir): list<string>` | `$dir` にあるファイルとディレクトリの名前を並べ替えて返す。リンクは含めない(`read`) |
| `mkdir($dir)` | 許可したディレクトリの中に、足りない親ごとディレクトリを作る。すでにあればそのまま(`write`) |
| `copy($from, $to)`、`rename($from, $to)` | ファイルのコピーと名前の変更(`copy` のコピー元は `read`、ほかは `write`)。下の規則は両方のパスに適用する |

### 権限(phv.toml)

スクリプトがヒープの外に触れるものは、すべて `phv.toml` で許可したものだけ。**何も書かなければ何も許可されない。**

```toml
[permissions]
read = ["content/"]         # file_get_contents / file_exists / scandir などで読めるディレクトリ
write = ["data/", "storage/"]  # file_put_contents / unlink / mkdir / rename / saveTo / Database::open(読み取りも可)
env = ["APP_*"]             # getenv で読める環境変数(末尾 * は前方一致)
```

- パスはプロジェクトからの相対パスで、`..`、絶対パス、`~` を含む名前(`GIT~1` のような Windows の短い名前)は使えない。
- 許可したディレクトリの下にあるシンボリックリンクとジャンクションはたどらない(リンクを経由するパスは `PermissionError`)。
  パスは許可したディレクトリから 1 段ずつ、リンクを拒否しながら開くので、検査と使用の間にディレクトリを
  すり替えられることもない。
- 開けるのは通常のファイルで、ハードリンクが 1 本だけのもの。外のファイルへのハードリンクを置かれても読み書きしない。
- ファイル名に `:`(Windows の代替データストリーム)、末尾のドットや空白、`CON` `NUL` `COM1` などのデバイス名は使えない。
- Unix では、ほかのユーザーが書き込めるディレクトリ(`o+w`)は使えない。
- コードとして読み込まれる場所は `write` に指定できない: `public/`、プロジェクトのルート、`.` で始まるディレクトリ
  (`.phv-cache/`、`.git/` など)、`vendor/`、`[dependencies]` のパッケージのディレクトリ。
- 許可されていないときは `PermissionError`(メッセージに `phv.toml` に書くべき内容が出る)。

### phv.toml

```toml
[project]
name = "blog"
phv = "^1.0"            # プロジェクトが動く PHV のバージョン(§18)

[security]
csrf = true             # 既定: true
secure_cookies = false  # HTTPS で配信するときは true
session_ttl = 7200      # 秒
memory_limit_mb = 128   # 1 リクエスト(1 回の実行)あたり
csp = "default"         # "off" か、{nonce} を含められる独自のポリシー
requests_per_minute = 0 # 接続元アドレスごとの上限(0 は無制限)
session_store = "memory"  # またはディレクトリ("sessions/")、データベース("db:cache")
rate_limit_store = "cache"  # この Redis のデータベースでレート制限を数える(既定: プロセスの中)
secret_env = "PHV_SECRET"   # サーバーの秘密鍵。サイトのプロセスが共有する(「CSRF 対策」)
store_namespace = "blog"    # 共有の保存先でのサイトの名前(既定: [project] name。「セッション」)

[server]
workers = 16            # 同時に実行するスクリプトの数(既定: CPU 数の 2 倍、最小 8)
static_max_age = 3600   # 静的ファイルの Cache-Control の max-age(既定: 毎回再検証)
compress = true         # テキストのレスポンスを gzip で圧縮(既定)
access_log = "logs/access.log"   # アクセスログをこのファイルにも追記
access_log_format = "json"       # "text"(既定)か "json"
shutdown_timeout = 30   # サーバーを止めるとき、実行中のリクエストに与える秒数
tls_cert = "certs/fullchain.pem"   # HTTPS と HTTP/2 で待ち受ける(「HTTPS と HTTP/2」を参照)
tls_key = "certs/privkey.pem"
h2c = false             # TLS なしの HTTP/2 も受け付ける(HTTP/2 で接続してくるプロキシ向け)
request_ids = true      # リクエストごとに ID を付ける(「リクエスト ID」)
publish_store = "cache" # publish() をほかのサーバープロセスにも届ける(std.ws)

[admin]
listen = "127.0.0.1:9100"   # /health、/ready、/metrics(「ヘルスチェックとメトリクス」)

[cors]
origins = ["https://app.example.com"]

[mail.default]
url_env = "SMTP_URL"
from = "Example <noreply@example.com>"

[jobs]
store = "db:main"       # バックグラウンドジョブ(「バックグラウンドジョブ」)

[[schedule]]
job = "cleanup"
cron = "0 3 * * *"

[oauth.google]
issuer = "https://accounts.google.com"   # OpenID Connect によるサインイン(std.oauth)
client_id_env = "GOOGLE_CLIENT_ID"
client_secret_env = "GOOGLE_CLIENT_SECRET"
redirect_uri = "https://example.com/auth/google"

[telemetry]
endpoint = "http://localhost:4318"   # OpenTelemetry のコレクターにトレースを送る(「トレース」)

[fmt]
width = 100             # phv fmt はこれより長い行を折り返す

[permissions]
write = ["data/"]

[dependencies]
utils = { path = "../utils" }
markdown = { git = "https://example.com/markdown.git", rev = "v1.2.0" }
```

`phv.toml` のあるディレクトリがプロジェクトのルートになる。知らないキーはエラー(書き間違いに気づける)。
`phv serve` は起動時に読み込むので、変更したらサーバーを再起動する。

`phv run`、`check`、`test` とエディタは、スクリプトのディレクトリとその上のディレクトリで `phv.toml` を探す。ほかのユーザーが
置けた可能性のある `phv.toml` は使わない: ファイルシステムのルート(Windows ではドライブのルートに誰でもファイルを作れる)、
共有の一時ディレクトリ、Unix では今のユーザー(または root)の持ち物でないもの。プロジェクトファイルは、スクリプトが何を
読み書きし、どこに接続できるかを決めるため。

## 15. 組み込み関数

### グローバル関数

| 関数 | 説明 |
|---|---|
| `count(list\|map\|set): int` | 要素数 |
| `set(list<int>): set<int>` / `set(list<string>): set<string>` | list の要素から重複を除いた set(最初に現れた順) |
| `strlen(string): int` | バイト数(文字数は `->len()`) |
| `implode(string $sep, list<string>): string` / `explode(string $sep, string): list<string>` | 結合 / 分割 |
| `str_repeat`, `str_contains`, `str_starts_with`, `str_ends_with`, `str_replace(search, replace, subject)` | PHP と同じ |
| `strtoupper`, `strtolower`, `ucfirst`, `trim` | PHP と同じ |
| `json_encode(any): string` | オブジェクトは public プロパティだけを、enum のケースは値を出力 |
| `json_encode_pretty(any): string`、`json_decode_as(string)` | 型付きの JSON(§14 の JSON) |
| `dump(any): void` | `var_dump` に相当 |
| `raw(string): Html`, `escape(string): Html`, `htmlspecialchars(string): Html`, `nl2br(string): Html` | §13 |
| `intdiv`、`abs`、`min`、`max`、`floor`、`ceil`、`round`、`sqrt`、`fmod` | 数値 |
| `sin`、`cos`、`tan`、`asin`、`acos`、`atan`、`atan2`、`exp`、`log(x)` / `log(x, base)`、`log10`、`log2`、`hypot`、`pi()` | 数学関数(float) |
| `clamp(x, min, max)` | int か float |
| `random_float(): float` | `[0, 1)` の値。OS の乱数源から |
| `bindec`、`octdec`、`hexdec`(string → int)、`decbin`、`decoct`、`dechex`(int → string)、`base_convert(string, from, to)` | 進数(2〜36) |
| `format(string, ...)` | 書式付きの文字列(下) |
| `uuid_v4(): string`、`uuid_v7(): string` | ランダムな UUID。v7 は時刻で始まるので作った順に並ぶ |
| `number_format(float, int $decimals = 0): string` | `1,234.50` |
| `range(int, int): list<int>` | 両端を含む |
| `time(): int`, `microtime(): float` | 現在時刻 |
| `rand(int, int): int` | 疑似乱数(暗号用途には使わない。`random_int` を使う) |
| `json_decode(string): any`, `json_html(any): SafeJs`, `safe_url(string): SafeUrl` | §13、§14 |
| `csrf_token(): string`, `csrf_field(): Html` | §14 |
| `csp_nonce(): string` | このレスポンスの CSP の nonce(§14) |
| `rate_limit(string, int, int): bool`, `rate_limit_reset(string)`, `ip_group(string): string` | §14 |
| `date`, `gmdate`, `mktime`, `strtotime`, `checkdate` | 日付(§14 の表) |
| `random_int`, `random_token`, `password_hash`, `password_verify`, `hash`, `hash_hmac`, `hash_equals`, `totp_secret`, `totp_code`, `totp_verify`, `qr_svg` | 暗号と 2 段階認証(§14 の表) |
| `base64_encode`、`base64_decode`、`urlencode`、`rawurlencode`、`urldecode`、`http_build_query` | エンコード |
| `base64url_encode`、`base64url_decode`、`bin2hex`、`hex2bin` | エンコード(UTF-8 でないテキストへのデコードは `ValueError`) |
| `file_get_contents`, `file_put_contents`, `file_exists`, `unlink`, `getenv` | `[permissions]` が必要 |
| `file_append(path, text)`、`filesize(path)`、`filemtime(path)`、`is_dir(path)`、`scandir(dir): list<string>`、`mkdir(dir)`、`copy(from, to)`、`rename(from, to)` | ファイルとディレクトリ。`[permissions]` が必要(§14) |
| `csv_parse(string): list<list<string>>`、`csv_encode(list<list<string>>): string` | RFC 4180(引用符で囲んだフィールド、`""`、フィールド内の改行)。`csv_encode` の行末は CRLF。表計算ソフトは `=`・`+`・`-`・`@` で始まるセルを数式として実行するので、訪問者のテキストを表計算ソフトで開くファイルに入れるときは、そのようなセルの前に `'` を付ける |
| `parse_url(string): map<string, string>`、`parse_query(string): map<string, string>` | キーは `scheme`、`host`、`port`、`user`、`pass`、`path`、`query`、`fragment`(あるものだけ)。不正な URL は `ValueError` |
| `log_info(msg, ?fields)`、`log_warn(...)`、`log_error(...)` | 標準エラーに 1 行(サーバー自身のログと並ぶ): `2026-11-05T01:00:00.000Z INFO "signed in" user=ann n=3`。空白・`"`・`=` を含む値は引用符で囲み、改行・制御文字・テキストの向きを変える文字(U+202E など)はエスケープするので、値で行を偽造できない。`[server] request_ids` があれば、リクエストのために書いた行の末尾に `req=<id>` が付く。サーバー自身の行(ページやジョブのエラー)も同じくエスケープする |
| `argv(): list<string>`、`stdin_read_line(): ?string`、`stdin_read_all(): string` | `phv run` で実行したスクリプトだけ(ほかでは `RuntimeException`): ファイルの後の引数と標準入力 |
| `sql_ident(string): Sql` | 列名・テーブル名 |
| `assert(bool, ?string)`, `assert_eq(any, any, ?string)` | テスト用 |

### format()

```phv
format("{} has {:,} points ({:.1}%)", $name, 1234567, 98.26);   // "Ann has 1,234,567 points (98.3%)"
format("[{:>6}] [{:<6}] [{:*^7}] [{:05}] [{:x}] [{:08b}] {{braces}}", "r", "l", "c", -42, 255, 5);
```

`{}` は次の値を取り、`{:spec}` はそれを書式化する: `[[fill]align][0][width][,][.precision][type]`。align は `<`、`>`、`^`、
`,` は 3 桁区切り、`.N` は小数 N 桁、type は `x` / `X`(16 進)、`o`、`b`、`e`。`{{` と `}}` はかっこそのもの。書式の文字列は
リテラルでなければならず、値の数とそれぞれの型を検査のときに確かめる(`{:.2}` は数値、`{:x}` は `int`)。幅と精度は 1000 まで。

### string のメソッド

`len()`(文字数)、`isEmpty()`、`upper()`、`lower()`、`ucfirst()`、`trim()`、`ltrim()`、`rtrim()`、
`contains($s)`、`startsWith($s)`、`endsWith($s)`、`replace($search, $replace)`、`split($sep)`、
`slice($start, $length?)`(文字単位、負の値は末尾から)、`indexOf($s): ?int`、`repeat($n)`、
`padStart($len, $pad)`、`padEnd($len, $pad)`、`toInt(): ?int`、`toFloat(): ?float`、`chars()`

### list のメソッド

`len()`、`isEmpty()`、`contains($v)`、`indexOf($v): ?int`、`first(): ?T`、`last(): ?T`、
`slice($start, $length?)`、`reverse()`、`concat($other)`、`unique()`、`sorted()`、`sortBy(fn($a, $b) => int)`、
`map(fn($x) => ...)`、`filter(fn($x) => bool)`、`find(fn($x) => bool): ?T`、`any(...)`、`all(...)`、
`join($sep)`、`sum()`、`reduce($initial, fn($acc, $x) => ...)`、`flatMap(fn($x) => list)`、`flatten()`、
`groupBy(fn($x) => string|int): map<K, list<T>>`、`chunk($size): list<list<T>>`、`zip($other, fn($a, $b) => ...)`
(短いほうの長さまで)、`partition(fn($x) => bool): list<list<T>>`(`[$yes, $no] = ...`)、
`count(fn($x) => bool)`、`min(): ?T` / `max(): ?T`(int、float、string)、`removeAt($i)`、`insertAt($i, $v)`

メソッドは新しい list を返し、呼び出した list はそのまま。`sorted()` は `NAN` をどの数よりも後ろに置く。

要素の追加は `$xs[] = $v;`、更新は `$xs[$i] = $v;`(範囲外は `IndexError`)。

### map のメソッド

`len()`、`isEmpty()`、`has($k)`、`get($k): ?V`、`keys()`、`values()`、`merge($other)`、`remove($k)`、
`filter(fn($k, $v) => bool)`、`map(fn($v) => ...)`(キーはそのまま)、`sortedByKey()`、`sortedByValue()`(値が int、float、
string。同じ値は順序を保つ)

追加・更新は `$m[$k] = $v;`。存在しないキーの読み取り `$m[$k]` は `KeyError`、`$m[$k] ?? $default` は例外にならない。

### set のメソッド

`len()`、`isEmpty()`、`has($v)`、`add($v)`、`remove($v)`、`union($other)`、`intersect($other)`、`diff($other)`、
`isSubsetOf($other): bool`、`toList(): list<T>`、`filter(fn($x) => bool)`、`any(...)`、`all(...)`

```phv
$tags = set(["php", "web", "php"]);   // 要素 2 個の set<string>
set<int> $seen = [];                  // set を期待する場所の `[...]` は set になる
$seen[] = 42;                         // その場で追加する。すでにある要素を加えても何も起きない
foreach ($tags as $tag) { ... }       // 要素を加えた順
```

メソッドは新しい set を返し、呼び出した set はそのまま。2 つの set は、要素が同じなら順序に関係なく `==`。set は
添字で読めない(`$s[0]` はエラー。`has()` を使う)。`json_encode()` は set を配列として書き、`json_decode_as()` は配列を
`set<int>` / `set<string>` に読み込む。`$x is set` で set かどうかを確かめられる。

PHP の関数名で書いたときは、エラーメッセージで代わりの書き方を案内する
(例: `strpos` → `->indexOf()`、`isset` → `??`、`var_dump` → `dump()`、`eval` → 使えない)。

## 16. ツール

| コマンド | 説明 |
|---|---|
| `phv run file.phv [引数...]` | 型検査して実行(`--html`、`--time-limit 秒`、`--profile`)。ファイルの後の引数は `argv()` で受け取る |
| `phv check file.phv` | 型検査だけ(import しているモジュールとテンプレートも) |
| `phv serve dir` | Web サーバー(`--port`、`--host`、`--prod`、`--time-limit`。`--watch` を付けると、サイトのファイルを保存したときにブラウザで開いているページを再読み込みする。開発時だけ) |
| `phv build path -o file` | サイトかスクリプトを 1 つの実行ファイルにする(下を参照) |
| `phv new dir [--site]` | プロジェクトを作る: モジュールとテスト付きのスクリプト、または `--site` でページ・レイアウト・ページのテスト付きのサイト |
| `phv test [paths]` | `*_test.phv` の `test_*` 関数を実行(1 テストごとに新しい VM。`--filter`、`--json`、`--junit`、`--coverage` はテストの節を参照) |
| `phv fmt [paths] [--check] [--width N]` | インデントと空白を整え、長い行を折り返す(文字列・コメント・HTML は変えない) |
| `phv lsp` | エディタ用の Language Server(下のエディタの節を参照) |
| `phv dap` | エディタ用のデバッグアダプタ(下のデバッグの節を参照) |
| `phv doc [paths] [--std] [--format html\|md] [-o file]` | ドキュメントコメントから API リファレンスを作る(下の API リファレンスの節を参照) |
| `phv install [--locked]` | `[dependencies]` とそれらが使うパッケージを取得して `phv.lock` に固定 |
| `phv jobs run [dir]`、`phv jobs list [--failed] [dir]`、`phv jobs retry <id> [dir]` | ページを配信せずにバックグラウンドジョブを実行する。ストアのジョブを表示する。失敗したジョブを戻す(「バックグラウンドジョブ」) |
| `phv migrate file.php` | PHP を機械的に変換し、手作業が必要な箇所を一覧にする(`-o` で出力先を指定。リンクを通しては書かない) |
| `phv disasm file.phv` | バイトコードを表示 |
| `phv parse file.phv` | 構文木を表示 |

### エディタ

`phv lsp` は Language Server で、VS Code 拡張(`editors/vscode`)や LSP に対応するエディタから使う:

- 入力中のエラーと警告、ホバーで式の型、補完、定義へのジャンプ。
- プロジェクトのファイル全体での参照の検索と名前の変更。メソッドの名前を変えると、それをオーバーライドしている
  (またはそれがオーバーライドしている)メソッドの名前も変えるので、オーバーライドの関係は保たれる。標準ライブラリの
  名前は変えられない。
- 整形(`phv fmt` と同じで `[fmt] width` に従う)と、呼び出し中の関数の引数の表示(シグネチャヘルプ)。
- ファイルのシンボル(アウトライン)とプロジェクトのシンボル(名前で検索)。
- クイックフィックス: 不明な変数・関数・クラス・メソッド・プロパティに近い名前、使われない変数に `$_name`、使われない
  import の削除、`mt_rand()` に `random_int()`。
- 名前の意味による色分け(クラス・関数・メソッド・プロパティ・変数・定数)と、変数を最初に代入した所の前に推論した型を
  表示(`$total = ...` に `int`)。

VS Code 拡張は `phv lsp` と `phv dap` をフルパスで起動し、名前だけでは起動しない。Windows は名前だけのプログラムを `PATH`
より先に作業ディレクトリ(ワークスペースのフォルダ)で探すので、クローンしたフォルダのファイルを開くとそのフォルダの
`phv.exe` が動いてしまうため。設定 `phv.path`(ユーザー設定だけ。ワークスペースからは変えられない)は、フルパスならそのまま
使い、名前(既定は `phv`)なら `PATH` で探す。探すのは `PATH` のうち絶対パスで、どのワークスペースフォルダの中でもない
ものだけで、Windows では `.exe` と `.com` のファイル(`PATHEXT` の順)。相対パスの `phv.path` や `PATH` の項目は使わない。
見つからなければ拡張はそう表示し、サーバーを起動しない。

### デバッグとプロファイル

`phv dap` はデバッグアダプタ(Debug Adapter Protocol)で、VS Code 拡張が種類 `phv` の起動構成で起動する:

```json
{ "type": "phv", "request": "launch", "name": "Run the open script", "program": "${file}", "args": [] }
```

スクリプトを `phv run` と同じように実行し(`stopOnEntry` で最初の文で止まる)、ブレークポイント、ステップオーバー・
ステップイン・ステップアウト、一時停止、コールスタック、各フレームの変数(list・map・オブジェクトは開ける)、
マウスを置いた変数の値を扱う。そのためにプログラムは各文の前に印を入れてコンパイルし、JIT なしで実行する。標準ライブラリ
の中にはステップインせず、スクリプトは標準入力を読めない(プロトコルが使うため)。

`phv run --profile` は実行のあとに、時間のかかった関数を表示する: 関数自身の時間(呼んだ組み込み関数を含む)、合計の
時間(呼んだ関数を含む)、呼び出し回数。呼び出しは正確に数え、時間は 1,000 ステップごとにコールスタックに割り当てるので、
短い関数は時間が 0 になることがある。JIT は使わない。

### API リファレンス

`phv doc [paths]` は、`paths` の下のモジュールが export する関数・クラス・定数・型エイリアスのリファレンスを書き出す。
public なメンバー、それぞれの前の `/** ... */` コメント、各ファイルの先頭の `//` コメントを含む。`--format md` で HTML の
1 ページの代わりに Markdown の 1 ファイルを書く(`-o` でファイルを選ぶ。既定は `doc/index.html` か `doc/api.md`)。
`--std` は標準ライブラリのモジュールを書き出す([stdlib.md](stdlib.md) を参照)。昇格した引数がすべて public では
ないコンストラクタと、`export` のない宣言は含めない。

```phv
/**
 * The total of the order, with tax.
 * Throws `ValueError` for an order without items.
 */
export function total(Order $order): float { ... }
```

### エラーと警告

プログラムのエラーはまとめて表示する。あるファイルに構文エラーがあっても、ほかのファイルは型検査され、そのエラーも
一緒に表示される。構文エラーのあるファイル自身と、それを import するファイルや render するファイルの型エラーは、
構文エラーを直してから表示される(パーサーが読み飛ばした部分のせいで出た誤りかもしれないため)。

使われないコードは警告になる(プログラムは実行できる):

| 警告 | 警告しないもの |
|---|---|
| 一度も読まれない変数(`$x = 1;` のあと `$x` をどこでも読まない) | `_` で始まる名前(`foreach (0..$n as $_)`)。メソッド、コンストラクタ、クロージャ、テンプレートの引数(名前はオーバーライドや呼び出し、render の側で決まるため) |
| モジュールが使わない `import` の名前 | |
| `return`、`throw`、`break`、`continue` や終わらないループのあとのコード | |

例外を使わない `catch` は変数を省略する(`catch (ValueError)`)。プログラムにエラーがあるあいだは、これらの警告は
表示しない(エラーのせいで使われなくなったコードが残っていることが多いため)。`phv lsp` は入力中にも表示する。

### 実行ファイルを作る

```bash
phv build mysite -o mysite-server      # サイト: 配信する(オプションは `phv serve` と同じ)
./mysite-server --port 8080 --prod
phv build tools/report.phv -o report    # スクリプト: 実行する
```

`phv build` は、サイトのすべてのページ(`public/**/*.phv`。使うテンプレートとモジュールを含む)か 1 つのスクリプトを前もって
コンパイルし、サイトの静的ファイルと phv.toml とともに、phv の実行ファイルの写しの末尾に付ける。できたものはソースなしで
動き、起動時のコンパイルもない。必要なのは、プロジェクトが読み書きするディレクトリ(`[permissions]`、セッションの
ディレクトリ)だけで、`--root`(既定はカレントディレクトリ)の下で探す。静的ファイルは、サーバーが配信するものだけを
入れる(`.` や `_` で始まるもの、`.toml` や `.env` のように配信しない種類は入れない)。phv.toml は入るので、秘密の値は
phv.toml に書かず環境変数(`url_env`、`headers_env`)で渡す。書くと実行ファイルに入ってしまう。

バンドルには SHA-256 と、作った phv のバージョンが入っている。壊れたバンドルや、別のバージョンで作ったものは起動時に
拒否する。実行ファイルは、動かすプラットフォームで作る(そのプラットフォームの phv の写しなので)。
`[jobs]` のジョブもサイトの実行ファイルにコンパイルして入れる。`--jobs-only` を付けると、ページを配信せずにジョブだけを
実行する(`phv jobs run` と同じ)。

### 整形

`phv fmt` は 1 段を空白 4 つでインデントし、100 文字(インデントを含む)を超える行を折り返す。幅は `--width N` で、
プロジェクトごとには phv.toml で変えられる(`[fmt] width = 120`)。

長い行はいちばん外側で分ける。括弧の外のカンマ(`echo a, b, c;`)では入るだけ並べ、残りを 1 段深い次の行に続ける。
そうでなければ、その行で開いて閉じるいちばん長い `(...)` か `[...]` の要素を 1 行に 1 つずつ 1 段深く置き、閉じ括弧を
独立した行にする。呼び出し・引数リスト・リストとマップのリテラルでは最後の要素のあとにもカンマを付ける。そうした括弧が
ない行は `??`・`||`・`&&`・`.`(または `->` と `?->`)の前で分け、続きの行を 1 段深くする。まだ長い要素も同じように分ける。

```phv
$body = render(
    "views/contact.phv.html",
    name: $name,
    message: $message,
    errors: $errors,
    sent: $sent,
);
```

`phv fmt` は行をつなげず、コメント・複数行の文字列・インライン HTML・`<?phv` / `?>` を含む行は変えないので、整形で
プログラムの動作は変わらない。整形済みのファイルを整形しても何も変わらない。ハードリンクが 2 本以上あるファイルは、
書くともう一方の名前の内容も変わるので、報告してそのままにする。

### テスト

```phv
// tests/posts_test.phv
import lib.posts.find_post;

function test_find_post(): void {
    assert_eq(find_post(2)?->title, "Types without ceremony");
    assert(find_post(99) == null, "missing posts are null");
}
```

`assert(bool, ?message)` と `assert_eq(expected, actual, ?message)` が失敗すると `AssertionError`。

テストは、テストファイルの引数を取らない関数のうち、名前が `test_*` のものか `#[Test]` を付けたもの。宣言した順に実行する。
`#[Skip("理由")]` を付けたテストは実行せず、スキップしたと報告する。

```phv
#[Test]
function posts_have_titles(): void { /* ... */ }

#[Skip("needs the staging database")]
function test_import(): void { /* ... */ }
```

`#[Before]` を付けた関数はそのファイルの各テストの前に、`#[After]` を付けた関数は各テストの後に(テストが失敗しても)
実行する(宣言した順に、テストと同じ VM で)。引数は取れない。

```phv
#[Before]
function fresh_database(): void { file_put_contents("data/test.json", "[]"); }

#[After]
function clean_up(): void { unlink("data/test.json"); }
```

| オプション | |
|---|---|
| `--filter 文字列` | 名前にその文字列を含むテストだけ |
| `--json` | 報告の代わりに、すべての結果を 1 つの JSON にして標準出力へ(`status` は `pass`・`fail`・`skip`) |
| `--junit report.xml` | 結果を JUnit XML にしても書き出す(CI のサービスが表示できる) |
| `--coverage` | 報告のあとに、テストが実行したプロジェクトの文の割合をファイルごとに、実行されなかった文の行と一緒に示す(テストファイルと標準ライブラリ、サイトがコンパイルする `TestClient` 経由のページは除く)。このときテストは JIT なしで実行する |

プロジェクト自身のサイト(`public/` のあるプロジェクト)へのリクエストは `std.testing` で、同じプロセスの中で送る。
サーバーもネットワークも要らない。ページは開発モードの `phv serve` と同じように実行される(ルート、ミドルウェア、
セッション、エラーページ)が、フォームに CSRF トークンは要らない。`TestClient` はブラウザと同じくサイトが設定した
Cookie を保持する:

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

`TestResponse` は `status`、`body`、`header()`、`headers()`(すべての値)、`json()`、`location()` を持つ。

### データベースのマイグレーション

```
migrations/
├── 0001_create_posts.sql
└── 0002_add_tags.sql
```

```bash
phv db status --sqlite data/app.db      # 各ファイルが適用済みか未適用か
phv db migrate --sqlite data/app.db     # 未適用のものを順に適用
phv db migrate --db main                # [databases.main] の PostgreSQL / MySQL サーバー
```

- ファイル名は `<番号>_<名前>.sql` で、番号の順に 1 回ずつ適用する。表 `phv_migrations` に、適用したファイルを本文の
  チェックサムとともに記録し、適用済みのファイルが後で変わっていたら `migrate` は実行を拒む(変更は新しいファイルに書く)。
- SQLite と PostgreSQL では、マイグレーションはその記録とともに 1 つのトランザクションで実行されるので、失敗しても何も
  残らない(SQLite ではマイグレーションがそのトランザクションを終えることはできず、中の `BEGIN`・`COMMIT`・`ROLLBACK` は拒まれる)。MySQL はスキーマの変更をそのつど確定するので、失敗したマイグレーションが途中まで適用されたままになりうる。
- `--dir` でほかのディレクトリを指定できる。データベースは `--db`(`[databases]` の名前)か `--sqlite`(プロジェクト内の
  ファイル)で指定する。

### パッケージ

```toml
[dependencies]
slugify = "^1.2"                                                            # レジストリから
textutils = { git = "https://github.com/example/textutils.git", version = "~0.3" }
nightly = { git = "https://github.com/example/nightly.git", rev = "main" }
local = { path = "../shared" }
```

```phv
import textutils.strings.slugify;      // vendor/textutils/strings.phv
```

- パッケージは PHV のモジュールを置いたディレクトリで、自分の依存があれば `phv.toml` を持つ。取得元は次のどれか:
  - **レジストリ**: `名前 = "<要件>"`。レジストリは `index/<名前>.toml`(パッケージの `git` の URL を書く)を並べた git
    リポジトリ(`PHV_REGISTRY` で別のレジストリを使える)。パッケージの一覧は phv-lang.org/packages/ にある。
  - **git** と `version`: 要件に合う最も新しい `vX.Y.Z` タグ。要件は semver の範囲: `^1.2`(1.2 以上 2.0 未満)、
    `~0.3.1`、`>=1.0, <1.5`、`=2.0.0`、`*`。
  - **git** と `rev`(タグ、ブランチ、コミット。なければ既定のブランチ)。
  - **path**: そのまま使うディレクトリ。書けるのはプロジェクト自身の phv.toml だけ。別のマシン(ネットワーク共有や
    ネットワークドライブ、またはそれを指すリンク)のパスは、そこにあるものを開く前に断る。
- `phv install` は、パッケージが使うパッケージ(その `[dependencies]`。phv.toml のほかの部分は読まない)も入れる。
  各パッケージは `vendor/<名前>` に 1 回だけ入る。バージョンの要件なら、それを求めるすべてのパッケージが受け入れる
  最も新しいバージョン。すべてに合うものがなければ、要件とそれを出したものを並べたエラーになる。同じパッケージを
  違う形で求めた場合(git の URL が 2 つ、rev が 2 つ)もエラー。
- `phv.lock` には、ほかのパッケージのために入れたものも含め、すべてのパッケージを正確なコミット(とバージョン)で記録する。
  次からは要件に合う限りそれを使う。`phv install --locked` は記録どおりに入れ、古くなっていれば失敗する(CI 向け)。
  取得したコミットが lock と違えばエラー。
- モジュールが import できるのは、自分のパッケージのモジュール(`import pkg.module`)、依存しているパッケージ、`std`。
  プロジェクトが import できるのは自分の `[dependencies]` のパッケージだけで、それらのために入れたものは使えない。
- 使える URL は `https://`、`ssh://`、`git@host:...`、`file:///` だけで、URL に使える文字だけからなるもの。レジストリの
  エントリとパッケージの依存先は `https://` でなければならない(`file:///` のレジストリは、テスト用に `file:///` の
  パッケージを指せる)。
- パッケージ名は英小文字・数字・`_`。プロジェクトのディレクトリやモジュールと同じ名前のパッケージは使えない。
  `phv install` は、リンクになっている `vendor/`、`vendor/<名前>`、`phv.lock` を拒み、clone したもののリンクはたどらず、
  パッケージの phv.toml は 1 MB までの通常のファイルのときだけ読む。
- パッケージは PHV のソースコードで、インストール時にパッケージのコードが実行されることはない。

### 移行

```
phv migrate legacy/page.php      # page.phv を書き出す
```

`<?php`→`<?phv`、`===`→`==`、`array(...)`→`[...]`、`isset($x)`→`($x != null)`、`$_GET`/`$_POST`/`$_COOKIE`/`$_SESSION`
→ `request()`/`session()`、`var_dump`/`print_r`→`dump`、`print`→`echo` を変換し、`require` や `global` には
`TODO(migrate)` コメントを付ける。そのあと型検査を実行して、型注釈などの手作業が必要な箇所を表示する。

## 17. 実行の仕組み

1. 字句解析・構文解析(手書きの再帰下降 + Pratt パーサ)
2. 型検査(名前解決、推論、絞り込み、確定代入の解析)→ 型付き中間表現 (HIR)
3. レジスタ型バイトコードへのコンパイル(`AddI`、`LtI`、`JumpIfNotLtIImm` などの型に特化した命令)
4. 実行
   - **インタプリタ**: 安全な Rust だけで書かれている。文字列・list・map は参照カウント + コピーオンライト。
     互いを参照して循環するオブジェクト(`$node->next = $node` や、自分をキャプチャしたクロージャを持つオブジェクト)は
     スクリプトの実行中に解放される。循環参照の回収は、生きているオブジェクトの数が前回の回収から 2 倍になるたびに
     (初回は 4,096 個)命令の合間に行われ、前回から作られたオブジェクト 1 個あたり 16 個までの値しか調べない。
     前回の回収の後にオブジェクトが作られていれば、使用中のメモリが上限の 4 分の 3 を超えたときと、上限でスクリプトを
     止める前にも一度回収する(このときも、作られたオブジェクト 1 個あたり 16 個の値まで)。
     回収そのものが使うメモリ(調べるオブジェクト 1 個あたり約 120 バイト)もメモリの上限に数える: すべてのオブジェクトを
     調べると残りに収まらないときは、収まるだけの新しいオブジェクト(前回の回収の後に作られたもので、新しい循環はここにある。
     古いものは、すべてを調べられる回収まで残す)を調べ、調べる途中でその範囲を超えた回収は止めて何も解放しない。
     上限でスクリプトを止める前の回収は、上限の 8 分の 1 まで多く使える。ネイティブコードは回収できない
     ので、ネイティブコードがメモリ不足になり、呼び出しの前に作られた循環を回収で解放できたときは、インタプリタがその
     呼び出しをやり直す。
     変数・list・map・クロージャ・生きている別のオブジェクトからたどれるオブジェクトは解放されない。実行が終わると、
     残ったすべてのオブジェクトのフィールドを空にする。プロパティがすべてスカラー・文字列・enum のケースを持つクラスの
     オブジェクトは循環できないので、回収の対象にしない。
   - **JIT**(Cranelift): ローカル変数がすべて `int`・`bool`・`float`・`string` か、その null 許容型(`?int` など)か、
     それらの list・map(`list<int>`、`map<string, int>` など)の関数は、使う命令がすべて対応していればネイティブコードに
     なる: `**` を含む算術、比較(`null` との比較も)、変換、文字列定数、`.`、`.=`、`(string)`、`strlen()`、`->len()`。
     `null`、`??`、null の確認。list と map では `[...]`、`$xs[$k]`、`$xs[$k] ?? $default`、`$xs[$k] = $v`、`$xs[] = $v`、
     `foreach`、`range()`、`count()`、`->len()`、`->has()`。ほかのコンパイルされた関数の呼び出し。数値はマシンのレジスタに
     置き(null 許容のものは null の印も)、文字列・list・map はレジスタウィンドウに置いてインタプリタと同じコードで処理する
     ので、結果・エラーメッセージ(`IndexError`、`KeyError` など)・メモリの検査は同じになる。`PHV_JIT_EXPLAIN=1` を付けると、
     JIT がインタプリタに任せた関数ごとに、その理由になった命令を表示する。整数のオーバーフロー、0 での除算と剰余、負のシフト、範囲外の float
     から int への変換、再帰の深さ、実行時間・メモリの上限は JIT でも同じように検査される。ネイティブコードが使う
     スタックは 8 MB までで、スレッドのスタックの端から 1 MB 以内には入らない(端がわかる Windows・Linux・macOS で):
     それより多く要る呼び出し(フレームの大きい関数の深い再帰)は、フレームをスタックに置かない
     インタプリタが代わりに実行する。コンパイルされる関数には副作用がないので、結果は同じ。ただし文字列・list・map の
     引数をその場で変える関数(`$xs[] = ...`)は「maximum call stack depth exceeded」で止まる(やり直し用に引数を
     持っておかないので、引数を変えてもコピーにならない)。レジスタが 1,024 個、
     命令が 16,384 個を超える関数はインタプリタに任せる(コンパイルに多くのメモリを使うため)。JIT はコンパイルの前に、
     すべての命令と、すべての経路での各レジスタの種類を検査するので、キャッシュのバイトコードをそのまま信用することはない。
     `PHV_JIT=0` で無効にできる。JIT の中で起きた例外のスタックトレースは呼び出し元までになる。
5. **バイトコードキャッシュ**: `phv run` と `phv serve` はコンパイル結果を `.phv-cache/` に保存し、
   ソースファイルと `phv` 本体が変わっていなければ再利用する。
   - キャッシュはユーザーごとの鍵(`%LOCALAPPDATA%\phv\cache.key`、`~/.cache/phv/cache.key`)の HMAC で
     署名し、署名が合わないものは読まない。ほかの人から受け取ったプロジェクトに `.phv-cache/` が入っていても、
     ソースと違うコードが動くことはない。
   - エントリはスクリプトとプロジェクトごと。依存するすべてのファイルの内容(SHA-256)を記録し、読み込むときに
     確かめる(更新時刻を保つコピーや、同じ長さの書き換えでも古いコードは使われない)。
   - `.phv-cache/` がリンクなら使わない。中のファイルも、リンクやハードリンクなら読み書きしない。

### 実行時の上限

| 上限 | 既定 | 超えたとき |
|---|---|---|
| 実行時間 | `serve`: 30 秒(`--time-limit`)、`run`: なし | 致命的エラー(捕捉できない) |
| メモリ | 128 MB(`[security] memory_limit_mb`) | 致命的エラー |
| スクリプトが同時に開いておける SQLite のデータベース | 64 | `DatabaseError` |
| SQLite のテキストやブロブ | 16 MB | `DatabaseError`(string or blob too big) |
| SQLite の文:値全体を読むステップの重さ(上の SQLite を参照)、ステップ数 | 128、43,008 | `DatabaseError` |
| 呼び出しの深さ | 10,000(組み込み関数のコールバックの入れ子は 200 まで。それぞれスレッドのスタックが 512 KB 残っている間だけ) | 致命的エラー |
| リクエスト本文 | 8 MB | 413 |
| 同時に実行するスクリプト(`serve`) | CPU 数の 2 倍、最小 8(`[server] workers`) | 最大 10 秒待ち、その後 503 |
| 同時に実行するジョブ(`serve`、`phv jobs run`) | プロセスごとに 2(`[jobs] workers`)。それぞれ `[jobs] time_limit`(300 秒)と `memory_limit_mb`。ページのスクリプトとは別 | ジョブはストアで待つ |
| ストアで待つ・実行中のジョブ | 100,000 か payload の 1 GB(メモリ: 10,000 か 64 MB)。payload は 64 KB | `enqueue()` が例外を投げる |
| ストアが覚えておく終わったジョブの key | 100,000(新しいもの)、それぞれ 1 日 | 古いものから忘れる |
| 終わっていないトランザクションが持つジョブの key を待つスクリプト | 1 プロセス・1 サイトにつき 8、それぞれ長くても 10 秒 | `enqueue()` が `DatabaseError` を投げる |
| 送信待ちの出力(`serve`) | `workers` × `memory_limit_mb`(その 4 分の 1 がイベントストリームの分) | 503。イベントストリームは終えるか、そのイベントを飛ばす |
| 受信中・保持中のリクエスト本文の、最初の 64 KB より後のバイト(`serve`) | `workers` × 8 MB | 最大 10 秒待ち、その後 503 |
| ストリーミングするレスポンス(`flush()`) | クライアントより 1 チャンク先まで。読まない時間は 10 秒まで | `flush()` が false を返す |
| JSON Web Token(std.jwt) | 256 KB、ヘッダーは 8 KB | `verify()` が null を返す |
| JWKS | 512 KB、読む鍵は 64 個。プロセスごとに保持する鍵セットは 32 個 | `NetworkError`(`fetch`)、`ValueError`(`fromJson`) |
| セッションが始めたサインイン(std.oauth) | 4 つ、それぞれ 10 分 | いちばん古いものを忘れる |
| スクリプトが記録するスパン(`[telemetry]`) | 1,000 | 記録しない |
| 送信を待つスパン | 8,192 個かつ 16 MB。スパンの名前・文字列の属性は 1,024 バイト | 捨てる(`phv_telemetry_dropped_spans_total`)。長い文字列は切る |
| ソースファイル(モジュール、テンプレート、スクリプト) | 8 MB | コンパイルエラー(`phv lsp`: それより大きい開いた文書は検査も整形もせず、開いている間は名前の変更を拒む) |
| 1 つのソースファイルのトークン(文字列の中のコードを含む) | 1,000,000 | コンパイルエラー(そのファイルは構文解析しない) |
| 1 つのプログラムのすべてのファイル(ページかスクリプトと、それが import・描画するもの。std を含む) | トークン 2,000,000 個、ソース 32 MB、ファイル 2,000 個 | コンパイルエラー(そのプログラムは型検査しない。`phv lsp` がリクエストの間に保持する開いたドキュメントの解析結果は、すべて合わせてトークン 250,000 個、ソース 16 MB まで。使われていない時間が最も長いものから捨て、どれも 2 秒まで) |
| 全クラスが継承するメンバーの合計 | 200,000 | コンパイルエラー(各クラスは継承したものの写しを持つ) |
| クラスの上にあるクラスとインターフェースの段数(`extends`、`implements`) | 256 | コンパイルエラー |
| 全クラスの祖先・インターフェースと、それらに渡す型引数の合計 | 2,000,000(祖先に渡す 1 つの型引数は 10,000 個の型の要素まで) | コンパイルエラー(各クラスは祖先をすべて並べて持つ) |
| 1 つの関数の変数と一時的な値 | 65,534 | コンパイルエラー(関数を分ける) |
| 1 つの関数の命令 | 8,388,608 | コンパイルエラー(関数を分ける) |
| 1 つのクラスのプロパティ(継承したものを含む) | 65,535 | コンパイルエラー |
| 定数式で作る文字列 | 1 MiB(プログラム全体で、全クラスのプロパティの文字列の既定値(継承したものを含む)と合わせて 16 MiB) | コンパイルエラー |
| 入れ子になった型エイリアス | 64 | コンパイルエラー |
| 関数・クラス・型エイリアスの型パラメータ | 4 | コンパイルエラー |
| ソースに書いた union 型のメンバー | 256 | コンパイルエラー |
| 型エイリアスが表す型の大きさ | 10,000 個の型の要素(すべての使用の合計で 2,000,000) | コンパイルエラー |
| ほかの値の型から推論される式の型の大きさ(ジェネリックな呼び出し、クロージャ、リスト) | 10,000 個の型の要素(型の要素が 100 個以上の式すべての合計で 2,000,000) | コンパイルエラー |
| バイトコードキャッシュの 1 エントリ(`.phv-cache/`) | 256 MB | 書かない(実行のたびにコンパイルする) |

`unsafe` を使っているのは、メモリ使用量を数えるアロケータ(`phv_vm/src/alloc.rs`)、JIT
(`phv_jit`、生成したコードの呼び出し、およびそれが呼ぶ `phv_vm/src/jit.rs` のヘルパー)、スレッドのスタックの端を OS に
問い合わせるところ(`phv_vm/src/stack.rs`)、OS のファイル API の呼び出し(`phv_std/src/safefs.rs`)、ジャーナルファイルを
検査する SQLite の VFS(`phv_std/src/db/vfs.rs`)、SQLite のメモリを数えるアロケータ
(`phv_std/src/db/mem.rs`)だけ。

## 18. 互換性とバージョン

1.0.0 から、PHV は[セマンティック バージョニング](https://semver.org/lang/ja/)に従う。バージョンは MAJOR.MINOR.PATCH。

- **パッチ**リリース(1.0.1)はバグとセキュリティの問題を直す。
- **マイナー**リリース(1.1.0)は機能を加える。それより前の 1.x で動いたプログラムは、同じ結果のまま動き続ける。
- 下に挙げるものを取り除いたり変えたりするのは**メジャー**リリース(2.0.0)だけで、その前のマイナーリリースで非推奨に
  したものに限る(「非推奨」を参照)。

**メジャーバージョンの中で保つもの:**

- 言語: 構文、型の規則、プログラムの動き。コンパイルできるプログラムはコンパイルでき続け、同じ結果になる。
- 標準ライブラリ: 組み込み関数、文字列・list・map・set のメソッド、`std.*` のモジュールの、名前・引数・結果・例外。
  新しいグローバルな組み込み関数はメジャーリリースでだけ加える(プロジェクトの同じ名前の関数とぶつかることがあるため)。
  マイナーリリースで加えるのは、組み込みの型のメソッド、`std.*` のモジュールの関数とクラス、新しいモジュールで、
  プロジェクトはそれを import して初めて使う。
- `phv.toml`: セクションとキー、その意味。
- ツール: `phv` のコマンド、オプション、終了コード。プロジェクトとサイトの構成(`public/`、`_middleware.phv`、`_404.phv`、
  `_500.phv`、`*.ws.phv`、`[param]` のルート、テンプレート、`tests/`)。
- Web サーバーでページが頼れるもの: `request()`、`response()`、`session()`、CSRF トークン、エスケープ、WebSocket の
  イベント。

**どのリリースでも変わりうるもの:**

- エラーメッセージと警告の文面と位置、新しい警告(警告でプログラムが止まることはない)。
- 速さ、メモリの使用量、JIT がコンパイルする範囲(結果は変わらない)。
- バイトコードのキャッシュ(`.phv-cache/`)と `phv build` で作った実行ファイル。新しいバージョンが作り直す。
- `phv fmt` の整形、`phv doc`・`dump()`・`phv run --profile` の出力、`phv lsp` と `phv dap` の機能。
- 名前が `__` で始まる組み込み関数と、実装の Rust のクレート。

**例外:** 受け付けることが脆弱性になる場合や、型検査が誤りを見逃していた場合(型の上では起きないはずの失敗をしうる
プログラム)は、マイナーやパッチのリリースでも、それまで受け付けていたコードや入力を断ることがある。たとえば、
スクリプトを埋め込む新しい方法を見つけたテンプレートの検査、厳しくした既定の上限、サーバーが加えるヘッダー。
何が変わり、コードをどう直すかはリリースノートに書く。

### 非推奨

次のメジャーバージョンで取り除くものは、少なくとも 1 つ前のマイナーリリースで非推奨にする。標準ライブラリの関数・
メソッド・クラス・プロパティには `#[Deprecated("...")]` を付け、使った場所で警告が出る(§9)。それ以外はこの仕様書と
リリースノートに書く。次のメジャーバージョンまでは動き続ける。

### プロジェクトの PHV のバージョン

```toml
[project]
phv = "^1.2"   # PHV 1.2.0 以降の 1.x
```

`[project] phv` には、プロジェクトが動く PHV のバージョンを semver の条件で書く。それに当てはまらないバージョンは、
プロジェクトの実行・検査・テスト・配信を断り、必要なバージョンを表示する。`phv new` は自分のバージョンに合う条件を
書く(PHV 1.2.x なら `^1.2`)。

### サポートするバージョン

セキュリティの修正は、今のメジャーバージョンの最新のマイナーバージョンに出す([SECURITY.md](../SECURITY.md))。

## 19. 今後の予定

次の順に予定している。これまでどおり、各リリースは監査してから公開する。

- **0.6 言語**(実装済み): 分割代入、可変長引数と `...` による展開、関数を値として使う(`strlen(...)`)、
  型エイリアス、enum のすべてのケースを書く必要がある `match`、型パラメータの制約、定数式、`import ... as`、
  ヒアドキュメント、原因とトレースを持つ例外。
- **0.7 Web アプリケーション**(実装済み): パラメータ付きのルート(`public/posts/[id].phv`)、ミドルウェア、独自のエラーページ、
  フォームの検証、レイアウト、CORS、静的ファイルの圧縮とキャッシュのヘッダー、グレースフル
  シャットダウン、アクセスログ、データベースのマイグレーション、`phv test` 用の HTTP クライアント、メール送信。
- **0.8 標準ライブラリ**(実装済み): 正規表現、書式付きの文字列、list と map のメソッドの追加、数学関数、
  タイムゾーン付きの日時、クラスへの JSON のデコード、暗号化と署名付きトークン、UUID、ディレクトリ、ログ、
  URL、CSV、スクリプトの引数と標準入力。
- **0.9 ツール**: `phv lsp` の名前の変更・参照の検索・整形・シグネチャヘルプ・シンボル・クイックフィックス・意味による
  色分け・型の表示、`phv new`、`phv serve --watch`、フック・絞り込み・JSON と JUnit の報告・カバレッジ付きの
  `phv test`、`phv doc` と標準ライブラリのリファレンス、デバッガ(`phv dap`)、`phv run --profile`、チュートリアル
  (実装済み)。
  このリリースからの ARM の Linux 向けビルド、静的リンクの Linux 向けビルド、Docker イメージ(`ghcr.io/miyagawayuu/phv`)、
  Homebrew と Scoop も実装済み。
- **1.0 速度と安定性**(実装済み): 実行中の循環参照の解放、list・map・null 許容型を扱う JIT、HTTPS と HTTP/2、
  WebSocket サーバー、`Set` 型、§18 の互換性の規則。
- **1.1 Web サービス**(完了): サイトのサーバープロセスが共有する秘密鍵(`[security] secret_env`)、サーバープロセスを
  またぐ `publish()`(`[server] publish_store`)、共有の保存先でのサイトごとの名前空間(`[security] store_namespace`)、
  管理用ポートのヘルスチェックと Prometheus のメトリクス(`[admin]`)、
  リクエスト ID とトレースコンテキスト、バックグラウンドジョブとスケジュール(`std.jobs`、`[jobs]`、`[[schedule]]`、
  `phv jobs`)、Server-Sent Events、数値の文字列化・例外・オブジェクト生成の高速化。
- **1.2**: OAuth 2.0 と OpenID Connect によるサインイン、RS256・ES256・EdDSA で署名した JSON Web Token(JWKS を含む)、
  ストリーミングレスポンス、トレースの送信(OpenTelemetry)。
- **その後**(マイナーリリースで): クロージャとオブジェクトの JIT、標準ライブラリの拡充。ジェネレータ(`yield`)と
  トレイトは検討中で、言語に加えるだけなので、入れるとしてもマイナーリリースになる。

設計方針(§1)に反するため、意図して入れないものもある: `eval`、可変変数、static プロパティとグローバル変数、
参照(`&$x`)、真偽値への暗黙の変換、`===`、`@`、`goto`、`__get` などのマジックメソッド、`extract` / `compact`、
`new $className`。

セキュリティ監査の結果と残っているリスクは [security-audit.ja.md](security-audit.ja.md) を参照。
