# Phenix

PHP の書き味をそのままに、**型安全・安全な既定値・高速**を言語の側で保証する Web 向けプログラミング言語です。
処理系は Rust で書かれています。

```phx
<?phx
import std.http.request;

function greet(string $name, int $times = 1): string {
    $out = "";
    foreach (0..$times as $i) {
        $out .= "Hello, {$name}!\n";
    }
    return $out;
}

$name = request()->query("name") ?? "world";   // ?string を ?? で string に
echo greet($name);                              // Web ページでは自動で HTML エスケープ
```

- **PHP から引き継いだもの**: `$変数`、`[1, 2]` / `["k" => v]`、`echo`、`foreach`、`match`、HTML への埋め込み、
  ファイル単位のルーティング、保存したらすぐ反映、リクエストごとに何も共有しない実行モデル
- **型安全**: 変数の型は最初の代入で固定。暗黙の型変換なし、`==` は常に厳密、truthiness なし、null 安全(`?T`)
- **セキュリティ**: 出力の自動エスケープ、`eval`・可変変数・動的 include なし、ヘッダーインジェクションと
  パストラバーサルの防止、リクエストごとに独立したヒープ
- **速度**: 型ごとに特化した命令を持つレジスタ型バイトコード VM

詳しくは [言語仕様 (docs/spec.md)](docs/spec.md) を参照してください。

## ビルド

Rust 1.80 以降が必要です。

```bash
cargo build --release
```

実行ファイルは `target/release/phenix`(Windows では `phenix.exe`)です。

## 使い方

```bash
phenix run examples/hello/hello.phx      # 型検査して実行
phenix check examples/site/public/index.phx  # 型検査だけ
phenix serve examples/site               # http://127.0.0.1:8080 でサイトを配信
phenix disasm examples/bench/bench.phx   # バイトコードを表示
```

`phenix serve` はファイルを保存すると次のリクエストで自動的に再コンパイルします。
`--prod` を付けるとエラーの詳細をブラウザに表示しません。

## サンプルサイト

`examples/site/` は、記事一覧・記事ページ・入力チェック付きのお問い合わせフォーム・JSON API からなる小さなブログです。

```
examples/site/
├── phenix.toml          プロジェクトのルートの目印
├── public/              URL に対応するページ(/ → index.phx, /contact → contact.phx)
│   ├── index.phx
│   ├── post.phx
│   ├── contact.phx
│   ├── api/posts.phx
│   └── style.css
├── lib/posts.phx        import するモジュール
└── views/*.phx.html     型付きテンプレート
```

## エラーメッセージの例

```
error: cannot call `->upper()` on `?string` because it might be null
 --> public/index.phx:6:6
  |
6 | echo $name->upper();
  |      ^^^^^
  = note: check `!= null` first, or use `?->`
```

## 構成

| クレート | 役割 |
|---|---|
| `phenix_syntax` | ソース管理、診断メッセージ、字句解析(コード / HTML モード)、構文解析、AST |
| `phenix_types` | 名前解決、型検査、絞り込み、確定代入の解析 → 型付き HIR。組み込み関数のシグネチャ |
| `phenix_compiler` | HIR → レジスタ型バイトコード |
| `phenix_vm` | 値、バイトコード形式、インタプリタ |
| `phenix_std` | 組み込み関数の実装、Phenix で書かれた prelude と `std.http` |
| `phenix_driver` | モジュールとテンプレートの読み込み、コンパイルの流れ、実行 |
| `phenix_server` | Web サーバー(hyper + tokio) |
| `phenix_cli` | `phenix` コマンド |

## テスト

```bash
cargo test --workspace
```

`tests/cases/*.phx` を実行して、隣の `.out` ファイル(出力、実行時エラー、またはコンパイルエラー)と比べます。
期待値を更新するときは `PHENIX_BLESS=1 cargo test -p phenix_driver --test golden` を実行し、差分を確認してください。

## ベンチマーク

`examples/bench/bench.phx` と同じ内容の `bench.py` の比較です(Windows 11、リリースビルド、参考値)。

| 項目 | Phenix 0.1 | CPython 3.13 |
|---|---:|---:|
| `fib(30)`(再帰呼び出し) | 71 ms | 98 ms |
| 1,000 万回のループ | 282 ms | 703 ms |
| 100 万要素の map / filter / sum | 129 ms | 98 ms |
| map の更新 20 万回 | 44 ms | 56 ms |

JIT はまだありません(今後の予定は仕様書の §17 を参照)。
