# PHV v1.3.1 リリースノート

## 日本語

### セキュリティ

- OAuth コールバックは、トークン交換の前に使用済みの `state` をセッションストアへ原子的に記録します。ストアを共有するプロセスは同じ記録を参照します。すでに使用済みの場合やストア障害時は、トークンを交換せず `invalid_state` を返します。
- `std.image` の Base64 入力では、一時バッファと復号結果が同時に占める容量を確保前に確認します。`toDataUrl()` は出力を単一のバッファに組み立てます。

### 共有セッションを使う環境の更新手順

同じセッションストアを使う全プロセスを一斉に切り替えてください。

1. 新版の全プロセスでセッションストアとサイトの名前空間をそろえます。Redis では期限内の記録を保持するため `noeviction` を使います。PostgreSQL と MySQL では、初回の state 記録時に `phv_oauth_states` を作成できる権限があることを確認します。
2. 新しい OAuth ログインの受付を止め、旧版が最後に発行した state の有効期限である 10 分が経過するまで待ちます。
3. 旧版への新規リクエストを止め、処理中のリクエストが完了してから、共有ストアを使う旧プロセスをすべて停止します。1.2 から共有ファイルセッションを使って更新する場合も、新版の起動前に 1.2 の全プロセスを停止します。1.2 と新版ではロックファイルが異なります。
4. 新版の全プロセスを起動します。準備状態と新しく開始した OAuth ログインの成功を確認してから、トラフィックとログインの受付を再開します。

### 画像変換

`workers = 16` のとき、HTTP サーバーごとの画像変換枠を 4 から 6 に増やしました。他のワーカー数では、従来どおり整数除算で `max(1, workers / 4)` 枠です。全枠が使用中の場合、画像の書き出しは画素の復号前に `RuntimeException` を投げます。

16 ワーカーで通常ページ 90%・画像 10% の固定到着率を使った比較では、画像毎秒 200 件で 4 枠が 6,956/7,200 件、6 枠が 7,188/7,200 件を処理しました。6 枠での画像拒否率は 0.167%、通常ページの p99 の中央値は 0.540 ms でした。

### JIT

型が確定した非 nullable な `int`・`bool`・`float` のプロパティ読み取りで、JIT の専用経路を既定で使います。`PHV_JIT=0` でインタープリターを使用できます。

### 検証結果

- リリースビルドが成功し、Windows x86_64 では JIT 有効時と `PHV_JIT=0` でそれぞれ回帰テスト 39 件が通過しました。画像変換枠は 1〜1,024 ワーカーの全設定で確認しました。Linux AArch64 の Docker エミュレーションでは、専用経路を試作フラグで有効にした状態で JIT 回帰テスト 27 件が通過しました。
- Redis 7.4.12、PostgreSQL 17.11、MySQL 8.4.13 の各実機テストでは、2 つの PHV プロセスが同じ state を処理したときのトークン交換は 1 回でした。両プロセスがセッションを読んだあとでストアを停止した場合は 0 回でした。
- Windows x86_64 で 16 ワーカー・画像変換 6 枠にそろえ、JIT 既定化の前後を 7 組交互に比較しました。画像 10% を含む総到着率 2,000 件/秒で、プロパティ読み取りを多用するページの p99 の中央値は 1.413 ms から 0.859 ms に短縮しました（39%）。通常ページの p99 の中央値の差は、総到着率 1,200〜2,000 件/秒で最大 0.007 ms でした。

### インストール

| 配布経路 | インストール方法 |
|---|---|
| Windows・Linux・macOS | GitHub Releases から対象のアーカイブを取得し、`SHA256SUMS` と照合します。 |
| Homebrew | `brew install miyagawayuu/phv/phv` |
| Scoop | `scoop bucket add phv https://github.com/miyagawayuu/scoop-phv` のあとに `scoop install phv` を実行します。 |
| Docker | `docker pull ghcr.io/miyagawayuu/phv` |

## English

### Security

- OAuth callbacks atomically record a consumed `state` in the session store before exchanging the authorization code. Processes that share a store consult the same records. A used state or an unavailable store returns `invalid_state` before token exchange.
- `std.image` checks the combined memory needed for the temporary and decoded buffers before allocating them for Base64 input. `toDataUrl()` builds its output in a single buffer.

### Upgrading processes that share sessions

Switch every process that uses the same session store in a single cutover.

1. Configure all new processes with the same session store and site namespace. For Redis, use `noeviction` to retain claims for their lifetime. For PostgreSQL and MySQL, verify that the database account can create `phv_oauth_states` when it first records a claim.
2. Pause new OAuth sign-ins and wait 10 minutes from the last state issued by the old version so it can expire.
3. Stop routing new requests to the old version and let in-flight requests finish. Stop every old process that uses the shared store. When upgrading from 1.2 with a shared file session directory, stop every 1.2 process before starting the new version because the lock files differ.
4. Start all processes on the new version. Verify readiness and complete a fresh OAuth sign-in before restoring traffic and reopening sign-ins.

### Image conversion

With `workers = 16`, each HTTP server now allows six concurrent image conversions, up from four. Other worker counts use `max(1, workers / 4)` with integer division. When all slots are busy, writing an image raises `RuntimeException` before pixel decoding starts.

In a fixed-arrival test with 16 workers, 90% ordinary pages, and 10% image requests, four slots completed 6,956 of 7,200 image requests at 200 images per second. Six slots completed 7,188 of 7,200. With six slots, the image refusal rate was 0.167% and the median ordinary-page p99 was 0.540 ms.

### JIT

The JIT now uses a specialized path by default when reading a statically known non-nullable `int`, `bool`, or `float` property. Set `PHV_JIT=0` to use the interpreter.

### Verification

- The release build succeeded. On Windows x86_64, 39 regression tests passed in each mode, with JIT enabled and with `PHV_JIT=0`. The image slot regression covered all worker counts from 1 to 1,024. On Linux AArch64 under Docker emulation, 27 JIT regression tests passed with the specialized path enabled through the prototype flag.
- Tests against Redis 7.4.12, PostgreSQL 17.11, and MySQL 8.4.13 recorded one token exchange when two PHV processes handled the same state. They recorded zero exchanges when the store was stopped after both processes had loaded the session.
- On Windows x86_64, seven alternating pairs used 16 workers and six image slots on both builds. At 2,000 total requests per second with 10% images, the median p99 of a page that frequently reads properties fell from 1.413 ms to 0.859 ms, a 39% reduction. The ordinary-page median p99 differed by at most 0.007 ms across arrival rates from 1,200 to 2,000 requests per second.

### Installing

| Distribution | Installation |
|---|---|
| Windows, Linux, and macOS | Download the matching archive from GitHub Releases and verify it against `SHA256SUMS`. |
| Homebrew | `brew install miyagawayuu/phv/phv` |
| Scoop | Run `scoop bucket add phv https://github.com/miyagawayuu/scoop-phv`, then `scoop install phv`. |
| Docker | `docker pull ghcr.io/miyagawayuu/phv` |
