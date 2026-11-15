# PHV v1.3.3

## 日本語

- **インタープリター**。値の解放を軽くし(数値や `null` を置き換えるときは解放の呼び出しをしない)、使い終わった一時レジスターが文字列やコレクションを持ち越さないようにしました。ヘルパーにリストを渡して伸ばすループ、プロパティの要素を読んでから追加するループ、`$s = $s . $x` のような連結が、どれも線形の時間になります。
- **JIT**。ネイティブコードはプロパティの要素を読むときと文字列のプロパティを比べるときに、リストや文字列をコピーせず、プロパティを通して直接読みます(300 万回のループで 2〜3 倍の速さ)。メソッド呼び出し、プロパティの読み取り、オブジェクトの作成も軽くなりました。
- **ジェネレーター**。停止と再開で状態を書くときに解放の呼び出しをしません。単純な `yield` 100 万回が 10.2 ms から約 7.9 ms に、入れ子の `yield from` が 37.9 ms から約 30 ms になりました。

### 計測結果

Windows x64 のリリースビルドで `examples/bench/bench.phv` を 9 回ずつ走らせた最小値を v1.3.2 と比べると、JIT ではオブジェクトの作成 100 万回が 126.3 ms から 96.9 ms(0.77 倍)、例外 10 万回が 28.1 ms から 24.5 ms、インタープリターではエラトステネスの篩が 163.5 ms から 143.7 ms になり、15 行のうち 14 行が 0.77〜0.97 倍でした(インタープリターの Mandelbrot だけ 1.03 倍)。[計測データ](https://github.com/miyagawayuu/phv/blob/v1.3.3/bench/results/v1.3.3-windows-x64.json)

### 更新

- **テンプレート**。SVG・MathML の統合点(`<foreignObject>`、`<desc>`、`<mi>` など)の中の `<textarea>`・`<title>` などは生テキストとして検査します。SVG・MathML の中の HTML に置いた `<mglyph>`・`<malignmark>`、SVG・MathML の中の `<noscript>`・`</noscript>`、`<frameset>` はエラーです。
- **時間の上限**。再帰から戻る途中や、ループのない長いコードでも、時間の上限で止まります。上限で止まったスクリプトの `catch` と `finally` は、これまでどおり実行されません。
- **共有のレート制限**(`rate_limit_store`)。Redis に置くキーは 1 サイト 100,000 個(約 53 MB)までで、`phv:ratelimits:<namespace>:` の下に管理用のキーを置きます。満杯のときは、制限に達していないキーを古いものから捨て、制限に達したキーは窓が終わるまで捨てません。すべてが制限中なら、新しいキーはサーバーのプロセスの中で数えます。窓は Redis サーバーの時計で数えるので、段階的な更新の間は 1.3.2 のプロセスの時計を合わせておいてください。プロセス内の表も、満杯のときは制限に達していないものから捨て、制限中のものは窓の終わりが近いものから捨てます。
- **ファイル**。`file_append()` は呼び出しごとにまとめて追記します(リクエスト、ジョブ、プロセスをまたいでも行が混ざりません)。Windows ではファイル操作をすべてディレクトリーのハンドルからの相対で行います。読み取り中のファイルの名前がほかのリクエストに変えられた瞬間は、読み取りを拒否します。
- **ジョブ**。共有ストアにある本体が 1 MB、名前が 64 バイトを超えるジョブは、理由つきで失敗扱いにし、次のジョブを実行します(`phv jobs list --failed` に出ます)。
- **MySQL**。`transaction()` の中の文が、`EXECUTE`・`CALL`・暗黙のコミット(`CREATE TABLE` など)を含むどの経路でトランザクションを終えても、`transaction()` は「did not commit」の例外を投げます。テキストに `XA` を含むプロシージャ・関数・トリガー・イベントの作成と変更、`PREPARE … FROM` は拒否し、スクリプト自身の XA トランザクションは接続を使い終えたときにロールバックします。`LOAD XML LOCAL INFILE` は拒否します。
- **HttpClient**。応答の本文は、残りのメモリの 3 分の 1(バックグラウンドのリクエストは 40 分の 1)まで、最大 16 MB です。既定の 128 MB では、バックグラウンドの応答 1 件あたり約 3 MB になります。`Content.Length` や `Content_Length` のような綴りの予約ヘッダーは `ValueError` です。
- **メール**。254 バイトを超えるアドレスと 998 バイトを超える件名は `ValueError` です。
- **JSON**。`012`、`1.`、`-.5` は、JSON を読むすべての場所で不正な数値です。
- **CSRF**。同じ応答の中では `session()->regenerate()` を `csrf_token()`・`csrf_field()` より先に呼んでください(応答の中で新しく作ったクッキーのトークンは、そのあとの `regenerate()` でも有効のままです)。
- **OAuth**。遠隔の発行者が示したエンドポイントは、要求のときにその名前がこのマシンに向かうなら拒否します。`localhost.` やループバックだけに解決する名前で書いた発行者は、ローカルのエンドポイントを示せます。
- **型検査と整形**。名前は 255 バイトまで、値から推論した共用体の型は 256 メンバーまで、プログラム全体で省略できる引数は 1,000,000 個までです。`phv fmt` は、一度に 500 を超える括弧やブロックが開くファイル、整形すると 8 MB を超えるファイル、作業が多すぎるファイルを拒否して変更しません。
- **言語サーバー**。100,000 バイトを超えるか 256 フォルダーより深い URI の文書は開きません。プロジェクトを歩く範囲に上限を付け、すべてのファイルを見られなかった名前の変更は拒否します。
- **phv doc**。`--format md` の地の文は、どの Markdown レンダラーでもマークアップにならないように書きます。

### セキュリティと検証

1.3.3 の候補を含む実装全体を、10 人のレビュアーが攻撃を再現して診断しました(第 21 回、[security-audit.ja.md §32](https://github.com/miyagawayuu/phv/blob/v1.3.3/docs/security-audit.ja.md))。指摘は 46 件(High 3、Medium 12、Low 22、Info 9)で、すべて修正、または範囲を狭めて残存リスクとして文書化しました。修正で加わったコードへの再攻撃を 2 回行い、そこで見つかったものも修正しています。リリース済みの版に影響するもの:

- **テンプレート**(High 3 件、1.3.2 以前): SVG・MathML の統合点の中の生テキスト要素、`<p>` を暗黙に閉じたあとの `<mglyph>`、SVG の中の `<noscript>` で、追跡器とブラウザーの解釈がずれ、訪問者のエスケープした値がページのノンス付きのスクリプトとして実行されました。
- **時間の上限**(Medium、1.3.2 以前): 再帰から戻る途中の長い連結で、2 秒の上限の下 27 秒、メモリを増やした設定では 395 秒、1 つのワーカーを占有できました。
- **共有のレート制限**(Medium、1.3.2 以前): 訪問者がキーを際限なく増やして Redis を満杯にでき、時計のずれたサーバー間では制限が効きませんでした。
- **ファイル**(Medium、1.3.2 以前): 同時の `file_append()` で行が失われました。Windows では、許可されたディレクトリーに書き込めるローカルのプロセスが、ジャンクションで外のファイルを読み書きさせられました。
- **型検査、整形、言語サーバー**(Medium、1.3.2 以前): 小さなファイルで数 GB のメモリや数分の時間を使わせられました。
- ほかに Low として、MySQL の間接の `ROLLBACK`・`XA PREPARE`、`Mail` と `HttpClient` のメモリ、ジョブの行によるキューの停止、Windows の SQLite を開くときの競合、`phv doc` の Markdown、ディスカバリーのエンドポイントなどを修正しました。
- 1.3.3 の候補で入った退行(戻る呼び出しにオブジェクトを残したことによる二次時間)はリリース前に見つけ、取り消しました。
- JIT と VM は約 45,000、ジェネレーターは約 30,800 のランダムなプログラム、サーバーは約 220 万件の混在リクエストで確かめ、誤った結果もリクエスト間の漏れもありませんでした。

## English

- **Interpreter.** Dropping a value is cheaper (a number or `null` that is replaced costs no drop call), and a temporary register no longer keeps a string or collection past the expression it was made for. Loops that grow a list passed to a helper, read a property's element and then append to it, or write `$s = $s . $x`, all take linear time.
- **JIT.** Native code reads a property's elements, and compares a string property, through the property without copying the list or string (two to three times as fast in a 3-million-pass loop). Method calls, property reads and creating objects are cheaper too.
- **Generators.** Suspending and resuming write the generator's state without a drop call: 1 million plain `yield`s went from 10.2 ms to about 7.9 ms, and nested `yield from` from 37.9 ms to about 30 ms.

### Measurements

On Windows x64 release builds, the minimum of nine runs of `examples/bench/bench.phv` against v1.3.2: creating 1 million objects with the JIT went from 126.3 ms to 96.9 ms (0.77), 100,000 exceptions from 28.1 ms to 24.5 ms, and the interpreter's sieve from 163.5 ms to 143.7 ms; 14 of the 15 rows are 0.77–0.97 of v1.3.2's (the interpreter's Mandelbrot is 1.03). [Benchmark data](https://github.com/miyagawayuu/phv/blob/v1.3.3/bench/results/v1.3.3-windows-x64.json)

### Upgrading

- **Templates.** `<textarea>`, `<title>` and the other raw-text elements inside an SVG or MathML integration point (`<foreignObject>`, `<desc>`, `<mi>`…) are checked as raw text. `<mglyph>` and `<malignmark>` in HTML inside SVG or MathML, `<noscript>` and `</noscript>` inside SVG or MathML, and `<frameset>` are errors.
- **Time limit.** A script stops at its time limit also on the way back out of a recursion and in long code without loops. As before, a script stopped by the limit runs no `catch` or `finally`.
- **Shared rate limits** (`rate_limit_store`). A site keeps at most 100,000 keys in Redis (about 53 MB), with bookkeeping keys under `phv:ratelimits:<namespace>:`. When they are all in use, keys not at their limit are discarded oldest first; a key at its limit is never discarded before its window ends, and while every key is at its limit new keys are counted in the server process. Windows follow the Redis server's clock, so keep the clocks of 1.3.2 processes in step during a rolling upgrade. The in-process table, when full, also discards keys not at their limit first, and limited ones by soonest window end.
- **Files.** `file_append()` appends each call whole (lines no longer mix across requests, jobs and processes). On Windows every file operation is relative to its directory's handle; a file being read is refused if another request renames it at that moment.
- **Jobs.** A job in a shared store whose payload is over 1 MB or whose name is over 64 bytes is given up as failed with an error that says so, and the next job runs (it is listed by `phv jobs list --failed`).
- **MySQL.** `transaction()` throws "did not commit" whenever a statement in it ended the transaction, by any route: `EXECUTE`, `CALL`, an implicit commit (`CREATE TABLE`…). Creating or altering a procedure, function, trigger or event whose text holds `XA`, and `PREPARE … FROM` such text, are refused; a script's own XA transaction is rolled back when it is done with the connection. `LOAD XML LOCAL INFILE` is refused.
- **HttpClient.** A response body may be a third of the memory left (a background request a fortieth), at most 16 MB: about 3 MB per background response with the default 128 MB. Reserved header names spelled `Content.Length` or `Content_Length` raise `ValueError`.
- **Mail.** Addresses over 254 bytes and subjects over 998 bytes raise `ValueError`.
- **JSON.** `012`, `1.` and `-.5` are invalid numbers wherever JSON is read.
- **CSRF.** Call `session()->regenerate()` before `csrf_token()` or `csrf_field()` in the same response (a token for a cookie the response itself made stays valid after a later `regenerate()`).
- **OAuth.** An endpoint a remote issuer names is refused when its name leads to this machine at request time; an issuer written `localhost.` or by a name that resolves only to loopback may name local endpoints.
- **Type checker and formatter.** A name has at most 255 bytes, a union inferred from values at most 256 members, and the calls of a program may leave out at most 1,000,000 arguments in all. `phv fmt` refuses, leaving the file as it is, a file with more than 500 brackets and blocks open at once, one whose formatted text would pass 8 MB, and one that needs too much work.
- **Language server.** A document whose URI is over 100,000 bytes or deeper than 256 folders is not opened. Walks of a project are bounded, and a rename that could not look at every file is refused.
- **phv doc.** The prose of `--format md` is written so that no Markdown renderer makes markup of it.

### Security and verification

The whole implementation, including the 1.3.3 candidate, was assessed by ten reviewers who reproduced every attack (the twenty-first assessment, [security-audit.md §32](https://github.com/miyagawayuu/phv/blob/v1.3.3/docs/security-audit.md)): 46 findings (3 High, 12 Medium, 22 Low, 9 Info), all fixed, or narrowed and written down as residual risks. The code the fixes added was attacked twice more, and what that found was fixed too. Those that affect released versions:

- **Templates** (3 High, 1.3.2 and earlier): raw-text elements inside SVG or MathML integration points, `<mglyph>` after an implicitly closed `<p>`, and `<noscript>` inside SVG made the tracker and browsers read a page differently, so a visitor's escaped value ran as a script with the page's nonce.
- **Time limit** (Medium, 1.3.2 and earlier): long joins on the way back out of a recursion held a worker 27 seconds under a 2-second limit, and 395 seconds with more memory.
- **Shared rate limits** (Medium, 1.3.2 and earlier): a visitor could add keys until Redis was full, and limits did not hold across servers whose clocks differed.
- **Files** (Medium, 1.3.2 and earlier): concurrent `file_append()` calls lost lines; on Windows a local process that can write in a permitted directory could make file functions read and write outside it through a junction.
- **Type checker, formatter and language server** (Medium, 1.3.2 and earlier): small files could take gigabytes of memory or minutes.
- Low findings fixed besides: indirect `ROLLBACK` and `XA PREPARE` on MySQL, the memory of `Mail` and `HttpClient`, job rows that stopped the queue, a race opening SQLite on Windows, `phv doc`'s Markdown, discovered endpoints, and others.
- A regression of the 1.3.3 candidate (objects left in a returning call's registers made some loops quadratic) was found before the release and reverted.
- About 45,000 random programs for the JIT and the VM, 30,800 for generators and 2.2 million mixed requests to a running server showed no wrong result and no data leaking between requests.

## インストール / Installing

| 配布経路 / Distribution | インストール方法 / Installation |
|---|---|
| Windows・Linux・macOS | [GitHub Releases](https://github.com/miyagawayuu/phv/releases/tag/v1.3.3) から対象のアーカイブを取得し、`SHA256SUMS` と照合してください。 Download the matching archive and verify it against `SHA256SUMS`. |
| Homebrew | `brew install miyagawayuu/phv/phv` |
| Scoop | `scoop bucket add phv https://github.com/miyagawayuu/scoop-phv` · `scoop install phv` |
| Docker | `docker pull ghcr.io/miyagawayuu/phv:1.3.3` |
