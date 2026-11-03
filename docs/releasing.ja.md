# PHV のリリース手順

[English](releasing.md) | 日本語

リリースを公開するのは、追加したものすべてのセキュリティ監査を行って見つかった問題を直し、ドキュメントがそのリリースの
内容どおりになってからです。小さなリリースでも、1 と 2 は省きません。

## 1. セキュリティ監査と脆弱性診断

対象: 前回監査したリリースからの変更すべて(`git diff vX.Y.Z..main`)。CI のワークフロー、VS Code 拡張機能、
サンプルサイトも含む。

- 分野ごとに分けて調べ(ネットワーククライアント、データベースのドライバー、Web サーバー、テンプレート、エディタ、
  CI と公開など)、指摘はすべてリリースビルドに対する実際の攻撃(PoC)で確かめる。
- 致命的・高・中の指摘はすべて直す。低の指摘は直すか、理由を付けて残存リスクに挙げる。
- 修正ごとに回帰テストを加え、修正したビルドですべての PoC を再実行する。
- 依存関係を確かめる: `cargo audit`(CI でも実行)、`editors/vscode` と `website` の `npm audit --omit=dev`。
- 結果を [security-audit.md](security-audit.md) と [security-audit.ja.md](security-audit.ja.md) に新しい節として書く:
  対象、方法、指摘と修正、オーナーが変える設定、残存リスク、修正の確かめ方。

リリースまでに直せない指摘があれば、リリースを待つ。

## 2. ドキュメント

変更は、そのドキュメントを同じコミットで更新する。リリースの前に、次のものが英語と日本語の両方でリリースの内容どおりか
確かめる:

- [spec.md](spec.md) / [spec.ja.md](spec.ja.md): 新しい機能や変わった機能、制限、エラーのすべてと、今後の予定(§18)。
- [README.md](../README.md) / [README.ja.md](../README.ja.md): 機能、クレートの表、テスト、監査の回数。
- [SECURITY.md](../SECURITY.md): サポートするバージョンの表(最新のマイナーバージョン)。
- [editors/vscode/README.md](../editors/vscode/README.md): エディタの機能と `.vsix` のファイル名。
- Web サイト(`website/pages.mjs`): `docs/` から生成しない文章(監査の回数など)。
- 変更したサンプルの README。

## 3. バージョン

新しいバージョンを次に書く:

- `Cargo.toml`(`[workspace.package] version`)
- `editors/vscode/package.json` と、`editors/vscode/package-lock.json` の先頭にある 2 つの `version`
- `editors/vscode/README.md` の `.vsix` のファイル名

一度ビルドして `Cargo.lock` を更新し、コミットして `main` をプッシュする。Linux・Windows・macOS で CI が通るまで待つ。

## 4. タグとドラフト

```bash
git tag -a vX.Y.Z -m "PHV vX.Y.Z" <commit>
git push origin vX.Y.Z
```

`v*` タグを作る・動かす・消すことができるのはリポジトリの管理者だけ(ルールセット「release tags」)。タグで
`.github/workflows/release.yml` が動き、4 つのプラットフォームと拡張機能をビルドして、`SHA256SUMS` 付きの**ドラフト**の
リリースを作る。macOS のジョブがランナー不足で取り消されたら `gh run rerun <id> --failed` で再実行する。

公開の前にドラフトを確かめる: 自分のプラットフォームのアーカイブをダウンロードして `SHA256SUMS` と照合し、
`phv --version` を実行し、このリリースの修正を公開するバイナリで試す。

## 5. 公開

リリースノート(直したもの。セキュリティの修正があればその節。新しいもの。インストールの表)を書いて、ドラフトを公開する:

```bash
gh release edit vX.Y.Z --notes-file notes.md --draft=false --latest
```

公開すると `.github/workflows/publish-extension.yml` が動き、`marketplace` 環境の承認待ちになる。実行の画面の
**Review deployments** で承認すると、`.vsix` を `SHA256SUMS` と照合して Visual Studio Marketplace に公開する。

## 6. リリースの後

- `examples/demo/Dockerfile` の `ARG PHV_VERSION` を新しいバージョンにしてプッシュし、デモをデプロイする
  (`examples/demo` で `fly deploy -a phv-demo`)。https://demo.phv-lang.org/ のすべてのページが応答するか確かめる。
- Web サイトは、`main` の `docs/` か `website/` が変わると `.github/workflows/website.yml` がデプロイする。
