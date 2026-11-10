# ベンチマークハーネス

ランタイム・サーバー・言語サーバーの数値を再現できる形で測り、`bench/results/<short-sha>.json` に
書き出します。2 つのコミットを比べるためのものです。性能に関わるプルリクエストには、変更前と変更後の
JSON(変更前のファイルはたいてい既にここにあります。新しいものを隣にコミットします)と、`--compare`
が表示する表を添えます。

[English](README.md)

## 実行方法

Windows(PowerShell 5.1 以降):

```powershell
.\bench\run.ps1 --all                 # すべて: マイクロベンチマーク、サーバー、言語サーバー
.\bench\run.ps1 --compare ae06940     # マイクロベンチマークのあと results/ae06940.json との比
.\bench\serve.ps1                     # サーバーだけ
python bench\lsp.py                   # 言語サーバーだけ
```

Linux と macOS(bash。サーバーのスクリプト、`lsp.py`、`--compare` には `python3` を使います):

```bash
bench/run.sh --all
bench/run.sh --compare ae06940
bench/serve.sh
python3 bench/lsp.py
```

各スクリプトは必要なもの(`cargo build --release -p phv_cli` と、負荷ツール
`crates/phv_server/examples/load.rs`)を `--no-build` がなければビルドします。3 つとも同じファイル
`bench/results/<short-sha>.json` に書きます。短い SHA は `HEAD` のものです。`run` がファイルを書き
(同じコミットの以前の実行の `serve` と `lsp` の節は残します)、`serve` と `lsp.py` は自分の節を
加えます。`"dirty": true` は、測ったときに `crates/` の追跡中のファイルが `HEAD` と違っていたことを表します。

先に重いプログラムを閉じてください。数値の再現性は、マシンの静かさまでです。

## 数値の意味

**`rows`**(`run.ps1` / `run.sh`): リポジトリの README の表の各行です。`examples/bench/bench.phv`
が各ベンチマークを自分で `microtime()` で計ります(コンパイル時間は含みません)。スクリプトを
JIT ありで `runs` 回(既定で 5 回)、JIT なし(`PHV_JIT=0`)で `runs` 回実行し、`jit_ms` と
`interp_ms` はその**最小値**です。マシンで他に動いていたものの影響がいちばん少ない測定です。
ばらつきが見えるように、すべての実行を `jit_runs_ms` / `interp_runs_ms` に残します。
`binary_bytes` はリリースビルドの `phv` のサイズ、`machine` は CPU 名・論理コア数・OS です。

**`serve`**(`serve.ps1` / `serve.sh`): `examples/site` のコピー(ゲストブックのデータベースが毎回
空から始まり、20 件を入れます)を 127.0.0.1 の空きポートで `phv serve --prod` します。負荷ツールは
`connections` 本(16)のキープアライブ接続を開き、それぞれで数えない `warmup_per_connection`
リクエスト(50)のあと `requests_per_connection` リクエスト(500)を送ります。対象は 3 ページです。

| ページ | 何を測るか |
|---|---|
| `/` | ホーム: テンプレート、レイアウト、投稿の一覧 |
| `/archive/2026/11` | 3 階層深いページ(`public/archive/2026/11/index.phv`): ルーティング |
| `/guestbook` | SQLite のクエリ(`SELECT ... LIMIT 20`)、セッション、CSRF フィールド |

各ページを `runs` 回(5 回)測り、`requests_per_s` は**最良**の回、`p50_ms` と `p99_ms` は
クライアントから見た 1 リクエストの中央値と 99 パーセンタイル(nearest rank)の**最小**です。
200 以外の応答があれば失敗します。

**`lsp`**(`lsp.py`): 標準入出力の `phv lsp` です。`examples/site/public` でいちばん大きいページ
(`document`)を開き、`edits` 回(20 回)、ページに 1 行を足す `didChange` と、その行末
(`$req->` の直後)での `completion` リクエストをすぐ続けて送ります。エディタが入力中に送るのと
同じ形です。編集ごとに、エディタから見た 2 つの遅延を取ります。`completion_ms` はリクエスト送信から
応答まで、`diagnostics_ms` は編集の送信からそのページの `publishDiagnostics` まで(サーバーは
補完を先に答えるので、その時間を含みます)。`median` と `p99` は 20 回の編集に対するもので、20 個の
標本では p99 はいちばん遅い 1 回です。`open_ms` は `didOpen` から最初の診断までの時間です。

`--keystrokes K`(2 以上)を付けると、編集ごとに行の先頭(`$editN = $req->`)を `K` 回の `didChange` に
分けて `--interval-ms`(50)の間隔で送り、人が打つのと同じ形にして、最後のキーの直後に補完リクエストを
送ります。このとき `diagnostics_ms` は最後のキーから、その編集で出た最後の `publishDiagnostics` までの
時間です(もう来ないことを確かめるために少し待ちます)。`checks_per_edit` はその `publishDiagnostics` の
数で、サーバーが入力の切れ目を待ってから検査すれば 1、キーごとに検査すれば最大 `K` です。これらは `lsp` の
横の `lsp_typing` に入ります。

`--pages N` を付けると、編集はせず、`examples/` の大きい順に `N` 個の `.phv` ファイルを 1 つずつ
開き(それぞれ診断を待ちます)、タブを `N` 個開いたエディタと同じ形にしてから、サーバーのメモリを読みます。
`rss_bytes` はワーキングセット(常駐セット)、`peak_rss_bytes` はその最大値(macOS では現在の値)、
`open_ms` は全部を開くまでの時間です。これらは `lsp_pages` に入ります。

## 2 つのコミットを比べる

```powershell
.\bench\run.ps1 --compare <old-sha>
```

両方のファイルにあるすべての数値について、新/旧の比を表示します。時間とサイズは 1.00 未満が速く、
毎秒リクエスト数は 1.00 超が速いです。`serve` と `lsp` の節も比べるなら、先に `serve` と
`lsp.py` を実行しておきます(または `--all`)。古いファイルは実行が自分のファイルを書く前に読むので、
同じコミットをもう一度実行すると 1 回目と比べられ、比は 1.00 前後になります。マイクロベンチマークの
数パーセント、サーバーの p99 ではそれ以上の差はノイズです。`--name <label>` は `<short-sha>.json`
の代わりに `results/<label>.json` に書きます(ブランチの未コミットの状態など)。`serve` にもあります
(`lsp.py` は `--out <file>`)。

## ファイル

`performance.py` は、同じ入力でリリースバイナリを既定 7 組交互に比較する。`runtime.phv` の各測定値と中央値を
JSON に保存する。`--load` に現行の `load` 例のバイナリを渡すと、本番 HTTP、画像混在負荷、同時初回アクセスも測る。
型が確定したプロパティの JIT 最適化に明示的な有効化が必要だった旧候補バイナリには `--scalar-properties` を指定する。

```powershell
python bench/performance.py --baseline <v1.3.0のphv> --candidate <候補のphv> --load <load> --output <結果.json>
```

| ファイル | 役割 |
|---|---|
| `run.ps1`, `run.sh` | マイクロベンチマークと `--compare` |
| `serve.ps1`, `serve.sh` | 負荷をかけたサーバー |
| `lsp.py` | 言語サーバー(Python 3.9 以降) |
| `common.ps1`, `common.sh` | 共通の処理: バイナリの場所、マシン情報、JSON |
| `results/` | 測ったコミットごとに 1 つの JSON |
| `../crates/phv_server/examples/load.rs` | 負荷ツール(`cargo run --release -p phv_server --example load -- --url http://127.0.0.1:8080 --path /`) |
