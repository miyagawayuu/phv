# Phenix

PHP の書き味をそのままに、**型安全・安全な既定値・高速**を言語の側で保証する Web 向けプログラミング言語です。
処理系は Rust で書かれています。

```phx
<?phx
import std.http.request;
import std.db.Database;

function greet(string $name, int $times = 1): string {
    $out = "";
    foreach (0..$times as $i) {
        $out .= "Hello, {$name}!\n";
    }
    return $out;
}

$name = request()->query("name") ?? "world";      // ?string を ?? で string に
$db = Database::open("data/app.db");
$rows = $db->query(sql"SELECT * FROM users WHERE name = {$name}");   // 値は必ずプレースホルダ
echo greet($name);                                 // Web ページでは自動で HTML エスケープ
```

## 特長

- **PHP から引き継いだもの**: `$変数`、`[1, 2]` / `["k" => v]`、`echo`、`foreach`、`match`、HTML への埋め込み、
  ファイル単位のルーティング、保存したらすぐ反映、リクエストごとに何も共有しない実行モデル
- **型安全**: 変数の型は最初の代入で固定。暗黙の型変換なし、`==` は常に厳密、truthiness なし、null 安全(`?T`)、
  interface・abstract・enum・ジェネリック関数、`finally`
- **セキュリティ**:
  - **XSS 対策**: 出力は自動でエスケープされる。加えて、エスケープでは守れない場所(引用符なしの属性、URL、
    `<script>`)への挿入はコンパイルエラーになる。URL には `safe_url()`、`<script>` には `json_html()` の値
    (型 `SafeUrl` / `SafeJs`)だけを置ける。場所の判定はブラウザの HTML 構文解析に合わせている。
  - **SQL インジェクション対策**: SQL は `sql"..."` でしか書けない。
  - **CSRF 対策**: フォームの POST をサーバーが自動で検査する。
  - **CSP**: Content-Security-Policy が既定で有効。テンプレートの `<script>` には自動で nonce が付くので、
    注入されたスクリプトだけが止まる。
  - **セッション**: サーバー側に保存し、固定攻撃を防ぐ(メモリ、またはファイルで再起動後も保持)。
  - **レート制限**: `rate_limit()` と、接続元ごとの上限 `requests_per_minute`。
  - **権限**: ファイルや環境変数には `phenix.toml` で許可したものしか触れない。リンクやハードリンクはたどらず、
    パスの検査と使用の間にすり替えられることもない。
  - **上限**: メモリ・時間・再帰の深さに上限がある。
  - **動的な実行がない**: `eval` や可変変数はない。
  - 3 回の監査と脆弱性診断の結果は [docs/security-audit.md](docs/security-audit.md) にある。
- **速度**: 型に特化したレジスタ型バイトコード VM、Cranelift による JIT、バイトコードのディスクキャッシュ
- **ツール**: テストランナー、フォーマッタ、Language Server、パッケージマネージャ、PHP からの移行ツール

詳しくは [言語仕様 (docs/spec.md)](docs/spec.md) を参照してください。

## ビルド

最近の安定版 Rust が必要です(1.97 で確認)。SQLite を同梱してビルドするため、C コンパイラ
(Windows では Visual Studio Build Tools)も必要です。

```bash
cargo build --release
```

実行ファイルは `target/release/phenix`(Windows では `phenix.exe`)です。

## 使い方

```bash
phenix run examples/hello/hello.phx         # 型検査して実行
phenix check examples/site/public/index.phx # 型検査だけ
phenix serve examples/site                  # http://127.0.0.1:8080 でサイトを配信
phenix test examples                        # *_test.phx の test_* 関数を実行
phenix fmt examples                         # インデントと空白を整える(--check で確認だけ)
phenix lsp                                  # エディタ用の Language Server
phenix install                              # phenix.toml の [dependencies] を取得
phenix migrate legacy/page.php              # PHP を変換し、手作業が必要な箇所を一覧にする
phenix disasm examples/bench/bench.phx      # バイトコードを表示
```

## サンプルサイト

`examples/site/` は小さなブログです。中身は、記事一覧、記事ページ、SQLite を使ったゲストブック
(セッション・CSRF・フラッシュメッセージを使う)、入力チェック付きのお問い合わせフォーム、JSON API です。

```
examples/site/
├── phenix.toml          セキュリティ設定と権限([permissions].write = ["data/"])
├── public/              URL に対応するページ(/ → index.phx, /guestbook → guestbook.phx)
├── lib/                 import するモジュール(posts.phx, guestbook.phx)
├── views/*.phx.html     型付きテンプレート
├── tests/posts_test.phx phenix test で実行するテスト
└── data/                SQLite のデータベース(スクリプトが書き込めるのはここだけ)
```

## エラーメッセージの例

```
error: cannot call `->upper()` on `?string` because it might be null
 --> err_null_safety.phx:6:6
  |
6 | echo $name->upper();
  |      ^^^^^
  = note: check `!= null` first, or use `?->`

error: a URL attribute cannot start with an unchecked string
 --> err_security.phx:45:16
   = note: use `safe_url($url)`, which only allows http(s), mailto, tel and relative URLs (blocks `javascript:`)
```

## 構成

| クレート | 役割 |
|---|---|
| `phenix_syntax` | ソース管理、診断メッセージ、字句解析(コード / HTML モード)、構文解析、HTML の文脈判定、フォーマッタ |
| `phenix_types` | 名前解決、型検査、絞り込み、確定代入の解析 → 型付き HIR。組み込み関数のシグネチャ |
| `phenix_compiler` | HIR → レジスタ型バイトコード |
| `phenix_vm` | 値、バイトコード形式、インタプリタ、実行時の上限 |
| `phenix_jit` | int/bool だけを扱う関数を Cranelift でネイティブコードにする |
| `phenix_std` | 組み込み関数(文字列・JSON・日付・暗号・ファイル・SQLite・セッション)と、Phenix で書かれた prelude・`std.http`・`std.db` |
| `phenix_driver` | モジュールとテンプレートの読み込み、`phenix.toml`、バイトコードキャッシュ、実行 |
| `phenix_server` | Web サーバー(hyper + tokio)、ルーティング、セッション、CSRF、アップロード |
| `phenix_cli` | `phenix` コマンド(run / check / serve / test / fmt / lsp / install / migrate) |

## テスト

```bash
cargo test --workspace
```

- `tests/cases/*.phx` を実行して `.out` と比べるゴールデンテスト。JIT を有効にした実行と、
  `phenix fmt` をかけた後の実行でも同じ結果になることを確かめる。
- セキュリティの回帰テスト(`err_security.phx`、`security.phx`)。
- 実サーバーを起動して HTTP で確かめる結合テスト。
- 変異させたプログラムによるファジング(`PHENIX_FUZZ_ITERATIONS` で回数を変えられる)。

期待値を更新するときは `PHENIX_BLESS=1 cargo test -p phenix_driver --test golden golden` を実行し、差分を確認してください。
CI(`.github/workflows/ci.yml`)は Linux / Windows / macOS でこれらと clippy・rustfmt・`cargo audit` を実行します。

## ベンチマーク

`examples/bench/bench.phx` と同じ内容の `bench.py` を比べた結果です(Windows 11、リリースビルド、参考値)。

| 項目 | Phenix (JIT) | Phenix (インタプリタ) | CPython 3.13 |
|---|---:|---:|---:|
| `fib(30)`(再帰呼び出し) | 6.7 ms | 80 ms | 94 ms |
| 1,000 万回のループ | 13 ms | 284 ms | 326 ms |
| 100 万文字の連結 | 24 ms | 25 ms | 65 ms |
| 100 万要素の map / filter / sum | 184 ms | 179 ms | 87 ms |
| map の更新 20 万回 | 44 ms | 46 ms | 34 ms |

JIT の対象は、扱う値がすべて `int` / `bool` の関数です。クロージャや文字列を多く使う処理はインタプリタで動き、
上の表のとおり CPython より遅いものがあります(今後の課題)。`PHENIX_JIT=0` で JIT を無効にできます。
