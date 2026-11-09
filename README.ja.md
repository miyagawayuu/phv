<h1>
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo/phv-lockup-on-dark.svg">
    <img src="assets/logo/phv-lockup-on-light.svg" alt="PHV" height="64">
  </picture>
</h1>

[English](README.md) | 日本語

PHP の書き味をそのままに、**型安全・安全な既定値・高速**を言語の側で保証する Web 向けプログラミング言語です。
処理系は Rust で書かれています。

v0.2.7 までは PHX という名前でした。

公式サイト: <https://phv-lang.org>

```phv
<?phv
import std.http.request;
import std.db.Database;

function greet(string $name, int $times = 1): string {
    $out = "";
    foreach (0..$times as $_) {
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
  interface・trait・abstract・`final`・enum(すべてのケースを扱う必要がある `match`)・制約付きのジェネリック関数と
  クラス(`<T: Shape>`)・型エイリアス・`finally`。使われない変数や import、到達しないコードは警告になる
- **書きやすい構文**: 分割代入(`[$a, $b] = ...`)、可変長引数と `...` による展開、関数を値として使う
  (`strlen(...)`)、定数式、`import ... as`、ヒアドキュメント、ジェネレータ(`yield`)、原因とトレースを持つ例外
- **Web アプリケーション**: パラメータ付きのルート(`public/posts/[id].phv`)、ミドルウェア(`_middleware.phv`)、
  独自のエラーページ、レイアウト、入力の検証(`std.validate`)、メール(`std.mail`)、翻訳(`std.i18n`)、データベースのマイグレーション
  (`phv db migrate`)、静的ファイルのキャッシュと範囲リクエスト、gzip、CORS、アクセスログ、穏やかな停止、ページの実行中に
  送るレスポンス(`response()->flush()`)
- **Web サービス**: 複数のサーバープロセスやマシンが、セッション、レート制限、CSRF トークン(`secret_env`)、
  WebSocket と Server-Sent Events への `publish()`(`publish_store`)を共有する。再試行と cron 形式のスケジュールを持つ
  バックグラウンドジョブ(`std.jobs`、`phv jobs`)。管理用ポートのヘルスチェックと Prometheus のメトリクス、リクエスト ID、
  OpenTelemetry へのトレースの送信(`[telemetry]`)。OAuth 2.0 と OpenID Connect によるサインイン(`std.oauth`)
- **画像**: アップロードされた PNG・JPEG・GIF・WebP を読み、縮小・切り抜きをして、メタデータなしで書き出す(`std.image`)
- **標準ライブラリ**: 正規表現(`std.regex`)、コンパイル時に検査する `format()`、タイムゾーン付きの日時(`std.time`)、
  型付きのクラスへの JSON のデコード(`json_decode_as`)、暗号化・署名付きトークン、RS256・ES256・EdDSA・HS256 で
  署名した JSON Web Token と鍵セット(`std.jwt`)、
  テーブルの行を型付きのオブジェクトとして読み書きするクラスごとのリポジトリ(`std.orm`)、list と map のメソッド、
  数学関数、UUID、CSV、URL、ディレクトリ、ログ、スクリプトの引数と標準入力
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
  - **権限**: ファイルや環境変数には `phv.toml` で許可したものしか触れない。リンクやハードリンクはたどらず、
    パスの検査と使用の間にすり替えられることもない。
  - **上限**: メモリ・時間・再帰の深さに上限がある。
  - **動的な実行がない**: `eval` や可変変数はない。
  - 19 回の監査と脆弱性診断の結果は [docs/security-audit.ja.md](docs/security-audit.ja.md) にある。
- **速度**: 型に特化したレジスタ型バイトコード VM、Cranelift による JIT、バイトコードのディスクキャッシュ
- **ツール**: プロジェクトやサイトを作る `phv new`、テストランナー(同じプロセスで動く HTTP クライアント `std.testing`、
  前後のフック、JUnit の報告、カバレッジ付き)、ライブリロード(`phv serve --watch`)、フォーマッタ、Language Server
  (名前の変更・参照・クイックフィックス・整形など)、デバッガ(`phv dap`)、プロファイラ(`phv run --profile`)、
  ドキュメントコメントからの API リファレンス(`phv doc`)、パッケージマネージャ、データベースのマイグレーション、
  PHP からの移行ツール。
  VS Code 拡張(色分け・Language Server・デバッガ)は [editors/vscode](editors/vscode) にあり、各リリースに
  `phv-<version>.vsix` として含まれる。

まず[チュートリアル](docs/tutorial.ja.md)から始めてください。詳しくは [言語仕様 (docs/spec.ja.md)](docs/spec.ja.md)、標準ライブラリは
[リファレンス (docs/stdlib.md、英語)](docs/stdlib.md) を参照してください。次に予定している改善は、リスクと測り方とともに
[docs/improvement-plan.ja.md](docs/improvement-plan.ja.md) にあります。脆弱性の報告方法は [SECURITY.md](SECURITY.md) にあります。

## インストール

各[リリース](https://github.com/miyagawayuu/phv/releases)に、Windows・Linux(x64 と ARM64。C ライブラリの要らない静的リンクの
ビルドも)・macOS 向けのアーカイブと `SHA256SUMS` があります。0.9.0 からは、`phv` だけを入れた Docker イメージもあります:

```bash
docker run --rm -p 8080:8080 -v "$PWD:/app" --user "$(id -u):$(id -g)" ghcr.io/miyagawayuu/phv serve --host 0.0.0.0 --prod
```

パッケージマネージャーでも入れられます(0.9.0 から):

```bash
brew install miyagawayuu/phv/phv                                         # macOS と Linux
scoop bucket add phv https://github.com/miyagawayuu/scoop-phv; scoop install phv   # Windows
```

## ビルド

最近の安定版 Rust が必要です(1.97 で確認)。SQLite と割り当て器(mimalloc)を同梱してビルドするため、C コンパイラ
(Windows では Visual Studio Build Tools)も必要です。

```bash
cargo build --release
```

実行ファイルは `target/release/phv`(Windows では `phv.exe`)です。

## 使い方

```bash
phv new my-site --site                   # 新しいサイト(ページ・レイアウト・テスト)
phv run examples/hello/hello.phv         # 型検査して実行
phv check examples/site/public/index.phv # 型検査だけ
phv serve examples/site                  # http://127.0.0.1:8080 でサイトを配信(--watch: 保存で再読み込み)
phv build examples/site -o site-server   # ソースなしでサイトを配信する 1 つの実行ファイルを作る
phv test examples                        # *_test.phv の test_* 関数を実行(--coverage、--junit)
phv fmt examples                         # インデントと空白を整える(--check で確認だけ)
phv lsp                                  # エディタ用の Language Server(phv dap はデバッガ)
phv doc lib -o doc/index.html            # /** */ コメントから API リファレンスを作る
phv install                              # phv.toml の [dependencies] を取得
phv migrate legacy/page.php              # PHP を変換し、手作業が必要な箇所を一覧にする
phv db migrate --sqlite data/app.db      # migrations/ の SQL ファイルのうち未適用のものを適用
phv disasm examples/bench/bench.phv      # バイトコードを表示
```

## サンプルサイト

`examples/site/` は小さなブログです。中身は、記事一覧、記事ページ、SQLite を使ったゲストブック
(セッション・CSRF・フラッシュメッセージを使う)、入力チェック付きのお問い合わせフォーム、JSON API です。

```
examples/site/
├── phv.toml             セキュリティ設定と権限([permissions].write = ["data/"])
├── public/              URL に対応するページ(/ → index.phv, /guestbook → guestbook.phv)
├── lib/                 import するモジュール(posts.phv, guestbook.phv)
├── views/*.phv.html     型付きテンプレート
├── tests/posts_test.phv phv test で実行するテスト
└── data/                SQLite のデータベース(スクリプトが書き込めるのはここだけ)
```

## エラーメッセージの例

```
error: cannot call `->upper()` on `?string` because it might be null
 --> err_null_safety.phv:6:6
  |
6 | echo $name->upper();
  |      ^^^^^
  = note: check `!= null` first, or use `?->`

error: a URL attribute cannot start with an unchecked value
  --> err_security.phv:45:14
   |
45 | <a href="<?= $name ?>">url</a>
   |              ^^^^^
   = note: use `safe_url($url)`, which only allows http(s), mailto, tel and relative URLs (blocks `javascript:`); `escape()` does not make a URL safe
```

## 構成

| クレート | 役割 |
|---|---|
| `phv_syntax` | ソース管理、診断メッセージ、字句解析(コード / HTML モード)、構文解析、HTML の文脈判定、フォーマッタ |
| `phv_types` | 名前解決、型検査、絞り込み、確定代入の解析 → 型付き HIR。組み込み関数のシグネチャ |
| `phv_compiler` | HIR → レジスタ型バイトコード |
| `phv_vm` | 値、バイトコード形式、インタプリタ、実行時の上限 |
| `phv_jit` | int/bool/float/string を扱う関数を Cranelift でネイティブコードにする |
| `phv_std` | 組み込み関数(文字列・JSON・日付・暗号・ファイル・SQLite・PostgreSQL・MySQL・Redis・WebSocket・HTTP クライアント・裏で動く処理・セッション)と、PHV で書かれた prelude・`std.http`・`std.db`・`std.net`・`std.async`・`std.validate`・`std.mail`・`std.regex`・`std.time`・`std.ws`・`std.jobs`・`std.testing`。マイグレーション、ジョブのストア |
| `phv_driver` | モジュールとテンプレートの読み込み、`phv.toml`、バイトコードキャッシュ、実行 |
| `phv_server` | Web サーバー(hyper + tokio、HTTPS と HTTP/2)、ルーティング、セッション、CSRF、アップロード、WebSocket サーバーと Server-Sent Events、プロセス間の `publish()`、ジョブの実行、管理用ポートとメトリクス |
| `phv_cli` | `phv` コマンド(run / check / serve / test / fmt / lsp / install / migrate) |

## テスト

```bash
cargo test --workspace
```

- `tests/cases/*.phv` を実行して `.out` と比べるゴールデンテスト。JIT を有効にした実行と、
  `phv fmt` をかけた後の実行でも同じ結果になることを確かめる。
- セキュリティの回帰テスト(`err_security.phv`、`err_context.phv`、`security.phv` など)。
- 大きな結果を作る組み込み関数(`limits.rs`)、SQLite(`sqlite_limits.rs`)、JIT(`jit.rs`)のメモリと時間の制限のテスト、
  大きなプログラムのコンパイルの手間のテスト(`compile_cost.rs`)。
- 実サーバーを起動して HTTP で確かめる結合テスト。
- 変異させたプログラムによるファジング(`PHV_FUZZ_ITERATIONS` で回数を変えられる)。

期待値を更新するときは `PHV_BLESS=1 cargo test -p phv_driver --test golden golden` を実行し、差分を確認してください。
CI(`.github/workflows/ci.yml`)は Linux / Windows / macOS でこれらと clippy・rustfmt・`cargo audit` を実行します。

リリースは [docs/releasing.ja.md](docs/releasing.ja.md) の手順で行います。毎回セキュリティ監査をして、見つかった問題を
直してから公開します。

## ベンチマーク

`examples/bench/bench.phv` と同じ内容の `bench.py` を比べた結果です(Windows 11、リリースビルド、参考値)。
[bench/](bench/README.ja.md) のハーネスでこの数値を再現でき(5 回の最小値)、サーバーと言語サーバーも測れます。
性能に関わる変更には、変更前と変更後の `bench/results/*.json` を添えます。

| 項目 | PHV (JIT) | PHV (インタプリタ) | CPython 3.13 |
|---|---:|---:|---:|
| `fib(30)`(再帰呼び出し) | 7.2 ms | 84 ms | 99 ms |
| 1,000 万回のループ | 14 ms | 286 ms | 348 ms |
| 100 万文字の連結 | 22 ms | 31 ms | 66 ms |
| マンデルブロ集合 300×300(float) | 4.2 ms | 234 ms | 198 ms |
| 数値から文字列を 100 万個作る | 304 ms | 337 ms | 351 ms |
| 100 万要素の map / filter / sum | 184 ms | 180 ms | 88 ms |
| map の更新 20 万回 | 42 ms | 46 ms | 34 ms |
| 100 万までのエラトステネスの篩(`list<bool>`) | 89 ms | 155 ms | 107 ms |
| 100 万個の float の list を作って合計 | 34 ms | 75 ms | 76 ms |
| プロパティへの `.=` 20 万回 | 12 ms | 11 ms | 333 ms |
| 文字列の比較(`==`)100 万回 | 21 ms | 46 ms | 40 ms |
| 20 万個の文字列の HTML エスケープ | 89 ms | 86 ms | 93 ms |
| オブジェクトを 100 万個作る | 178 ms | 183 ms | 110 ms |
| 例外の送出と捕捉 10 万回 | 40 ms | 42 ms | 14 ms |
| 20 万要素の `sortBy` | 318 ms | 306 ms | 269 ms |

JIT の対象は、扱う値がすべて `int`・`bool`・`float`・`string` か、その null 許容型か、それらの list・map か、
オブジェクトの関数とメソッドです。list・map・オブジェクトはインタプリタと同じコードで処理するので、速くなるのは
そのまわりのループと算術です: オブジェクトの作成やプロパティの読み出しにかかる時間はインタプリタとほぼ同じです。
クロージャを多く使う処理はインタプリタで動き、上の表のとおり CPython より遅いものがあります(関数がコンパイル
されない理由は `PHV_JIT_EXPLAIN=1` で分かります)。例外(投げた場所を記録する)も遅くなります(今後の課題)。
`PHV_JIT=0` で JIT を無効にできます。

## ライセンス

[MIT](LICENSE)
