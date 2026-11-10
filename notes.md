# PHV v1.3.2

## 日本語

- **JIT**。ループ内で変化しないことを確認できる非 nullable な `int`、`bool`、`float` のプロパティ読み取りを再利用します。`PHV_JIT_LOOP_PROP_CACHE=0` でこの最適化を無効にし、`PHV_JIT=0` でインタープリターに切り替えられます。
- **ジェネレーター**。キーと値をそのまま渡す `yield from` の中継処理を短縮し、停止・再開をまたいで例外ハンドラーの保存領域を再利用します。切り戻しには、それぞれ `PHV_VM_YIELD_FROM_FORWARD=0` と `PHV_VM_REUSE_GEN_HANDLERS=0` を使えます。
- **文字列追記**。追記元と追記先が別レジスタの場合、追記元の値を複製せずに処理します。
- **リストと文字列**。JIT がリストからスカラー値を読む際の複製と、`sortBy` の比較引数の一時コピーを省きます。JIT は共有されていないリストの範囲内の要素を直接更新します。`Html` をプレーン文字列に変換する際は、型の区別を保ちながら文字列データを共有します。
- **試験設定**。`PHV_VM_COMPACT_NEW_GENERATORS=1` は、初回再開まで引数とキャプチャだけを保持します。`PHV_VM_PRUNE_GEN_REGS=1` は別の試験設定で、対象の停止中フレームに再開・終了・例外処理で必要なレジスタだけを保存します。
- **PHP 8.5 との比較**。同じ入力でプロパティ読み取りとジェネレーターを計測し、JIT 有効・無効の結果とチェックサムを記録する CLI ベンチマークを追加しました。[手順と v1.3.1 の比較結果](https://github.com/miyagawayuu/phv/blob/v1.3.2/bench/README.ja.md)

### 計測結果

Windows x64 のリリースビルドで、オブジェクトを扱うページに毎秒600件を送ったときのリクエスト p99 の中央値は、JIT の読み取り再利用なしで 0.842 ms、ありで 0.594 ms でした。例外ハンドラー保存領域の再利用では、`try/finally` 内の `yield` 10万回のウォーム中央値が 18.109 ms から 13.110 ms に短縮しました。未実行ジェネレーターを2万個保持する試験では、試験設定でピーク RSS の中央値が 34.89 MiB から 13.38 MiB に減少しました。[計測データ](https://github.com/miyagawayuu/phv/tree/v1.3.2/bench/results)

### 更新

画像変換は16ワーカーで6枠です。v1.3.0 以前から更新して共有セッションを使う場合は、v1.3.1 で追加された OAuth の使用済み state 記録を全プロセスが使うよう、一斉に切り替えてください。共有ファイルセッションを使う v1.2 の全プロセスは、新版の起動前に停止してください。[v1.3.1 の移行手順](https://github.com/miyagawayuu/phv/releases/tag/v1.3.1)

## English

- **JIT.** Eligible loops reuse checked reads of non-nullable `int`, `bool`, and `float` properties when the JIT can prove the property remains unchanged. Set `PHV_JIT_LOOP_PROP_CACHE=0` to disable this optimization or `PHV_JIT=0` to use the interpreter.
- **Generators.** The VM forwards keys and values through transparent `yield from` chains and reuses the exception-handler buffer across suspension and resumption. Set `PHV_VM_YIELD_FROM_FORWARD=0` or `PHV_VM_REUSE_GEN_HANDLERS=0` to disable the respective optimizations.
- **String appends.** The VM borrows the source value when the source and destination occupy separate registers.
- **Lists and strings.** The JIT reads scalar list elements without cloning their values and updates in-bounds elements of unshared lists directly. `sortBy` avoids an intermediate copy of comparison arguments. Converting `Html` to a plain string shares the underlying text while preserving the distinction between the two types.
- **Experimental settings.** `PHV_VM_COMPACT_NEW_GENERATORS=1` retains only arguments and captures until the first resume. The separate `PHV_VM_PRUNE_GEN_REGS=1` setting keeps only the registers needed for resumption, closing, and exception handling in eligible suspended frames.
- **PHP 8.5 comparison.** A CLI benchmark measures typed property reads and generators with the same inputs, records checksums, and compares runs with and without JIT. [Instructions and v1.3.1 results](https://github.com/miyagawayuu/phv/blob/v1.3.2/bench/README.md)

### Measurements

On Windows x64 release builds, at 600 requests per second, the median request p99 of an object-processing page was 0.842 ms with loop property reuse disabled and 0.594 ms with it enabled. Reusing the exception-handler buffer cut the warm median for 100,000 yields inside `try/finally` from 18.109 ms to 13.110 ms. The experimental compact setting reduced median peak RSS from 34.89 MiB to 13.38 MiB when holding 20,000 unstarted generators. [Benchmark data](https://github.com/miyagawayuu/phv/tree/v1.3.2/bench/results)

### Upgrading

Image conversion uses six slots with 16 workers. When upgrading from v1.3.0 or earlier with a shared session store, switch all processes together so that they share the consumed OAuth state records introduced in v1.3.1. Stop all v1.2 processes that use a shared file session directory before starting the new version. [v1.3.1 migration instructions](https://github.com/miyagawayuu/phv/releases/tag/v1.3.1)

## インストール / Installing

| 配布経路 / Distribution | インストール方法 / Installation |
|---|---|
| Windows・Linux・macOS | [GitHub Releases](https://github.com/miyagawayuu/phv/releases/tag/v1.3.2) から対象のアーカイブを取得し、`SHA256SUMS` と照合してください。 Download the matching archive and verify it against `SHA256SUMS`. |
| Homebrew | `brew install miyagawayuu/phv/phv` |
| Scoop | `scoop bucket add phv https://github.com/miyagawayuu/scoop-phv` · `scoop install phv` |
| Docker | `docker pull ghcr.io/miyagawayuu/phv:1.3.2` |
