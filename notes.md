# PHV v1.3.2

## 日本語

- **JIT**。ループ内で変化しないことを確認できる非 nullable な `int`、`bool`、`float` のプロパティ読み取りを再利用します。`PHV_JIT_LOOP_PROP_CACHE=0` でこの最適化を無効にし、`PHV_JIT=0` でインタープリターに切り替えられます。
- **ジェネレーター**。キーと値をそのまま渡す `yield from` の中継処理を短縮し、停止・再開をまたいで例外ハンドラーの保存領域を再利用します。切り戻しには、それぞれ `PHV_VM_YIELD_FROM_FORWARD=0` と `PHV_VM_REUSE_GEN_HANDLERS=0` を使えます。
- **文字列追記**。追記元と追記先が別レジスタの場合、追記元の値を複製せずに処理します。添字のない代入・追記ではキー用の一時バッファを使いません。
- **リストと文字列**。JIT がリストからスカラー値を読む際の複製と、`sortBy` の比較引数の一時コピーを省きます。JIT は共有されていないリストの範囲内の要素を直接更新します。`Html` をプレーン文字列に変換する際は、型の区別を保ちながら文字列データを共有します。
- **試験設定**。`PHV_VM_COMPACT_NEW_GENERATORS=1` は、初回再開まで引数とキャプチャだけを保持します。`PHV_VM_PRUNE_GEN_REGS=1` は別の試験設定で、対象の停止中フレームに再開・終了・例外処理で必要なレジスタだけを保存します。
- **PHP 8.5 との比較**。同じ入力でプロパティ読み取りとジェネレーターを計測し、JIT 有効・無効の結果とチェックサムを記録する CLI ベンチマークを追加しました。[手順と v1.3.1 の比較結果](https://github.com/miyagawayuu/phv/blob/v1.3.2/bench/README.ja.md)

### 計測結果

Windows x64 のリリースビルドで、オブジェクトを扱うページに毎秒600件を送ったときのリクエスト p99 の中央値は、JIT の読み取り再利用なしで 0.842 ms、ありで 0.594 ms でした。例外ハンドラー保存領域の再利用では、`try/finally` 内の `yield` 10万回のウォーム中央値が 18.109 ms から 13.110 ms に短縮しました。未実行ジェネレーターを2万個保持する試験では、試験設定でピーク RSS の中央値が 34.89 MiB から 13.38 MiB に減少しました。[計測データ](https://github.com/miyagawayuu/phv/tree/v1.3.2/bench/results)

### 更新

- **セッションと OAuth**。共有ストアの使用済み state の記録を、v1.3.1 の 64 区画から state ごとの記録に変えました(1 セッションの有効な記録は 16 件まで)。v1.3.1 の記録(`phv_oauth_states` の表、`.phv-oauth-XX.claims`、`…:oauth:<n>`)は読まないので、サイトの全プロセスを一斉に切り替えてください。表は切り替えの 11 分後に削除できます。Redis の `maxmemory-policy` が `noeviction` 以外なら起動時に警告します。共有ファイルセッションを使う v1.2 の全プロセスは、新版の起動前に停止してください。
- **SQLite**。`PRAGMA busy_timeout` は拒否します。ロックを待つ書き込みは、5 秒かスクリプトの時間の上限の早いほうで諦めます。同梱の SQLite は 3.54.1 です。
- **テンプレート**。`<svg>`・`<math>` の中で、その外で開いた要素を閉じる終了タグ、表と文書の部品(`<td>`、`<table>` など)、`<select>` の中の `<svg>`・`<math>`、`/` と `>` の間の文はエラーです。`imagesrcset` は `srcset` と同じく URL のリストとして検査し、アニメーションの `values` には数値だけを入れられます。
- **応答のヘッダー**。Cookie か Authorization の付いたリクエストへの応答は、ページが `Cache-Control` を設定しない限り、ストリーミングしないページも `private, no-store` になります。ページの `Vary` はすべて残ります。
- **画像**。変換は空きの枠を到着順に待ちます(16 ワーカーで 6 枠、1 つのアドレスはその半分まで)。復号と符号化のメモリをすべて数えるので、これまで上限をすり抜けていた一部の画像はメモリのエラーになります。
- **そのほか**。トレイトの `as` の規則はメンバーの上限に数えます。MySQL の `lastInsertId()` が `int` に収まらないときは `DatabaseError` です。オブジェクトの生成と例外の送出は 4〜7% 遅くなりました(戻るコンストラクターがレジスターを手放すため)。

### セキュリティと検証

v1.3.1 と v1.3.2 の候補を含む実装全体を、10 人のレビュアーが攻撃を再現して診断しました(第 20 回、[security-audit.ja.md §31](https://github.com/miyagawayuu/phv/blob/v1.3.2/docs/security-audit.ja.md))。指摘は 35 件(Critical 2、High 2、Medium 5、Low 11、Info 15)で、すべて修正または文書化し、修正で加わったコードも別のレビュアーが攻撃しました。リリース済みの版に影響するもの:

- **画像**(Critical、1.3.0 と 1.3.1): 可逆 WebP の 1×1 のファイルが、宣言した符号の表で 128 MB の上限の下 1.3〜4.5 GB を使い、メモリを絞ったサーバーを異常終了させました。復号の前に `std.image` が表を数えます。JPEG の係数(Medium)と GIF のメタデータ(Low)も数えます。
- **サインイン**(High、1.3.1): 1 人の訪問者が使用済みの記録の区画を埋め、サイト全体のサインインを約 10 分止められました。
- **SQLite**(High、1.3.1 以前): `PRAGMA busy_timeout` で、時間の上限を超えてワーカーを持ち続けられました。
- **テンプレート**(Medium、1.3.0 と 1.3.1): `<svg>`・`<math>` の中の終了タグと表の部品が SVG を終わらせ、訪問者の値がページのノンス付きのスクリプトとして実行されました。
- **画像変換の枠**(Medium、1.3.1): 1 つのクライアントがすべての枠を持ち、ほかの訪問者の変換を失敗させられました。
- **言語**(Medium と Low、1.3.0 と 1.3.1): トレイトの別名と、ヘルパーに渡したコレクションの追記が二乗の時間になりました。
- ほかに Low として、SQL の垂直タブによる `XA PREPARE` の拒否のすり抜け、管理用ポートの空行の接続、`Vary` の消失などを修正しました。
- JIT は約 35,000、ジェネレーターは約 24,200 のランダムなプログラム、サーバーは 60 万件の混在リクエストで、1.3.2 の高速化が誤った結果を生まないことを確かめました。Windows と Linux ですべてのテストが通ります。
- VS Code 拡張は、既定の PATH 検索で、ドットで始まる名前を含むワークスペースの子ディレクトリも除外します。Web サイトのデプロイは `main` 限定の GitHub Environment を使い、GitHub リリースのドラフト作成には既存タグが必要です。

## English

- **JIT.** Eligible loops reuse checked reads of non-nullable `int`, `bool`, and `float` properties when the JIT can prove the property remains unchanged. Set `PHV_JIT_LOOP_PROP_CACHE=0` to disable this optimization or `PHV_JIT=0` to use the interpreter.
- **Generators.** The VM forwards keys and values through transparent `yield from` chains and reuses the exception-handler buffer across suspension and resumption. Set `PHV_VM_YIELD_FROM_FORWARD=0` or `PHV_VM_REUSE_GEN_HANDLERS=0` to disable the respective optimizations.
- **String appends.** The VM borrows the source value when the source and destination occupy separate registers. Assignments and appends without subscripts skip the temporary key buffer.
- **Lists and strings.** The JIT reads scalar list elements without cloning their values and updates in-bounds elements of unshared lists directly. `sortBy` avoids an intermediate copy of comparison arguments. Converting `Html` to a plain string shares the underlying text while preserving the distinction between the two types.
- **Experimental settings.** `PHV_VM_COMPACT_NEW_GENERATORS=1` retains only arguments and captures until the first resume. The separate `PHV_VM_PRUNE_GEN_REGS=1` setting keeps only the registers needed for resumption, closing, and exception handling in eligible suspended frames.
- **PHP 8.5 comparison.** A CLI benchmark measures typed property reads and generators with the same inputs, records checksums, and compares runs with and without JIT. [Instructions and v1.3.1 results](https://github.com/miyagawayuu/phv/blob/v1.3.2/bench/README.md)

### Measurements

On Windows x64 release builds, at 600 requests per second, the median request p99 of an object-processing page was 0.842 ms with loop property reuse disabled and 0.594 ms with it enabled. Reusing the exception-handler buffer cut the warm median for 100,000 yields inside `try/finally` from 18.109 ms to 13.110 ms. The experimental compact setting reduced median peak RSS from 34.89 MiB to 13.38 MiB when holding 20,000 unstarted generators. [Benchmark data](https://github.com/miyagawayuu/phv/tree/v1.3.2/bench/results)

### Upgrading

- **Sessions and OAuth.** The used-state records of shared stores are now one record per state, not v1.3.1's 64 shards (at most 16 live records per session). v1.3.1's records (the `phv_oauth_states` table, `.phv-oauth-XX.claims`, `…:oauth:<n>`) are not read, so switch every process of a site at once; the table can be dropped 11 minutes after the switch. A Redis store whose `maxmemory-policy` is not `noeviction` is warned about at startup. Stop all v1.2 processes that use a shared file session directory before starting the new version.
- **SQLite.** `PRAGMA busy_timeout` is refused. A write waiting for a lock gives up after 5 seconds or at the script's time limit, whichever is first. The bundled SQLite is 3.54.1.
- **Templates.** Inside `<svg>` and `<math>`, an end tag that closes an element opened outside them, table and document parts (`<td>`, `<table>`…), `<svg>` and `<math>` inside `<select>`, and a statement between `/` and `>` are errors. `imagesrcset` is checked as a list of URLs like `srcset`, and animation `values` take numbers only.
- **Response headers.** A response to a request with a cookie or an `Authorization` header is `private, no-store` unless the page sets `Cache-Control`, also when it is not streamed. Every `Vary` field a page sets is kept.
- **Images.** A conversion waits for a free slot in arrival order (six slots with 16 workers, at most half for one address). Decoding and encoding memory is counted in full, so a few images that slipped past the limit before end with the memory error.
- **Other.** Trait `as` rules count toward the member limit. A MySQL `lastInsertId()` that does not fit in `int` raises `DatabaseError`. Creating objects and throwing exceptions are 4–7% slower (a returning constructor lets go of its registers).

### Security and verification

The whole implementation, including v1.3.1 and the v1.3.2 candidate, was assessed by ten reviewers who reproduced every attack (the twentieth assessment, [security-audit.md §31](https://github.com/miyagawayuu/phv/blob/v1.3.2/docs/security-audit.md)): 35 findings (2 Critical, 2 High, 5 Medium, 11 Low, 15 Info), all fixed or documented, and the code the fixes added was attacked by a further reviewer. Those that affect released versions:

- **Images** (Critical, 1.3.0 and 1.3.1): a 1×1 lossless WebP made the decoder build its declared code tables, 1.3–4.5 GB under a 128 MB limit, and aborted a server with capped memory. `std.image` now counts the tables before decoding. JPEG coefficients (Medium) and GIF metadata (Low) are counted too.
- **Sign-in** (High, 1.3.1): one visitor could fill the used-state shards and block sign-in for the whole site for about 10 minutes.
- **SQLite** (High, 1.3.1 and earlier): `PRAGMA busy_timeout` let a script hold its worker past its time limit.
- **Templates** (Medium, 1.3.0 and 1.3.1): end tags and table parts inside `<svg>` or `<math>` could end the SVG, so a visitor's value ran as a script with the page's nonce.
- **Image conversion slots** (Medium, 1.3.1): one client could hold every slot and make other visitors' conversions fail.
- **The language** (Medium and Low, 1.3.0 and 1.3.1): trait aliases, and appending to a collection passed to a helper, took quadratic time.
- Low findings fixed besides: a vertical tab getting past the `XA PREPARE` refusal, empty-line connections on the admin port, lost `Vary` fields, and others.
- About 35,000 random programs for the JIT, 24,200 for generators and 600,000 mixed requests to a running server showed no wrong result from 1.3.2's speed work. All tests pass on Windows and Linux.
- The VS Code extension excludes dot-prefixed workspace subdirectories from its default executable search on PATH. The website deploys through a GitHub Environment restricted to `main`, and draft GitHub releases require an existing tag.

## インストール / Installing

| 配布経路 / Distribution | インストール方法 / Installation |
|---|---|
| Windows・Linux・macOS | [GitHub Releases](https://github.com/miyagawayuu/phv/releases/tag/v1.3.2) から対象のアーカイブを取得し、`SHA256SUMS` と照合してください。 Download the matching archive and verify it against `SHA256SUMS`. |
| Homebrew | `brew install miyagawayuu/phv/phv` |
| Scoop | `scoop bucket add phv https://github.com/miyagawayuu/scoop-phv` · `scoop install phv` |
| Docker | `docker pull ghcr.io/miyagawayuu/phv:1.3.2` |
