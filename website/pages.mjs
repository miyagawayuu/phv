// Content of the landing pages (English and Japanese). The code samples live in
// samples/; CI type-checks them with `phv check` (see .github/workflows/ci.yml).

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const sample = (name) => fs.readFileSync(path.join(here, "samples", name), "utf8").trimEnd();

const releases = "https://github.com/miyagawayuu/phv/releases";
const marketplace = "https://marketplace.visualstudio.com/items?itemName=phv-lang.phv-language";

// What `phv check samples/profile.phv` prints (CI checks that it still does).
const errorOutput = sample("profile.err");

const text = {
  en: {
    title: "PHV — PHP's ease, with safety built in",
    description: "PHV is a web language that keeps the feel of PHP while the compiler guarantees type safety, safe output and speed.",
    tagline: "PHP's ease, with safety built in.",
    lead: "PHV keeps what made PHP productive — <code>$variables</code>, HTML templates, routing by file, save and reload — and makes the compiler responsible for type safety, safe output and speed.",
    download: "Download",
    readSpec: "Read the specification",
    features: [
      ["Typed, without ceremony", "A variable's type comes from its first assignment. No implicit conversions, strict <code>==</code>, null safety with <code>?T</code>, <code>?-&gt;</code> and <code>??</code>, <code>list</code>, <code>map</code> and <code>set</code>, generics, interfaces and enums."],
      ["Escaping that knows where it is", "Output is escaped automatically, and the compiler tracks the HTML around every <code>&lt;?= ?&gt;</code>. A string at the start of a URL, inside <code>&lt;script&gt;</code> or in an event handler is a compile error."],
      ["SQL without injection", "Queries are <code>sql\"...\"</code> literals. Interpolated values always become bound parameters — there is no way to build SQL text from a string."],
      ["Locked down by default", "A Content-Security-Policy with automatic nonces, CSRF checks, server-side sessions, rate limiting, and file access only where <code>phv.toml</code> allows it."],
      ["Fast", "A typed register VM, a Cranelift JIT for numbers, strings, lists and maps, and a signed bytecode cache. <code>fib(30)</code> runs in about 7 ms, and in production the server keeps compiled pages and small static files in memory."],
      ["Everything in one command", "<code>phv serve</code> (HTTPS, HTTP/2 and WebSockets), <code>test</code>, <code>fmt</code>, <code>build</code> (one executable), <code>doc</code>, <code>lsp</code>, a debugger, <code>install</code> and <code>migrate</code> (from PHP), plus a VS Code extension."],
    ],
    mistakesTitle: "Mistakes are compile errors",
    mistakesLead: "Escaping alone cannot make a URL safe: <code>javascript:alert(1)</code> contains nothing to escape. PHV knows the value lands at the start of an <code>href</code> and refuses to compile until it is checked.",
    downloadTitle: "Download",
    downloadLead: (v) => `PHV ${v} for Windows, Linux and macOS. Unpack the archive and put <code>phv</code> on your <code>PATH</code>.`,
    platform: "Platform",
    archive: "Archive",
    verify: "Check the archive against <code>SHA256SUMS</code> from the same release, then run it:",
    source: "Or build from source with a recent stable Rust and a C compiler: <code>cargo build --release</code>.",
    docker: (image) => `Or run the Docker image (Linux, amd64 and arm64), which holds only <code>phv</code>: <code>${image}</code>.`,
    packages: `Or use a package manager: <code>brew install miyagawayuu/phv/phv</code> (macOS and Linux), or <code>scoop bucket add phv https://github.com/miyagawayuu/scoop-phv</code> and <code>scoop install phv</code> (Windows).`,
    editor: (version) => `For VS Code, install <a href="${marketplace}">PHV Language</a> from the Marketplace (<code>code --install-extension phv-lang.phv-language</code>) for highlighting, errors as you type, completion, renaming and the debugger. It is also attached to each release as <a href="${releases}/download/v${version}/phv-${version}.vsix"><code>phv-${version}.vsix</code></a>.`,
    securityTitle: "Security",
    securityLead: `PHV has been through thirteen security audits, each followed by fixes and regression tests. Read the <a href="docs/security/">audit report</a>. To report a vulnerability, email <a href="mailto:security@phv-lang.org">security@phv-lang.org</a>.`,
    getStarted: "Get started",
  },
  ja: {
    title: "PHV — PHP の手軽さに、安全を組み込んだ言語",
    description: "PHV は、PHP の書き味のまま、型安全・安全な出力・速度をコンパイラが保証する Web 向けの言語です。",
    tagline: "PHP の手軽さに、安全を組み込む。",
    lead: "<code>$変数</code>、HTML テンプレート、ファイル単位のルーティング、保存してすぐ反映 — PHP の生産性はそのままに、型安全・安全な出力・速度をコンパイラが引き受けます。",
    download: "ダウンロード",
    readSpec: "言語仕様を読む",
    features: [
      ["気負わない型安全", "変数の型は最初の代入で決まります。暗黙の型変換なし、<code>==</code> は常に厳密、<code>?T</code>・<code>?-&gt;</code>・<code>??</code> による null 安全、<code>list</code>・<code>map</code>・<code>set</code>、ジェネリクス、interface、enum。"],
      ["場所を知っているエスケープ", "出力は自動でエスケープされ、コンパイラはすべての <code>&lt;?= ?&gt;</code> の周りの HTML を追跡します。URL の先頭、<code>&lt;script&gt;</code> の中、イベント属性に文字列を置くとコンパイルエラーです。"],
      ["SQL インジェクションのない SQL", "クエリは <code>sql\"...\"</code> リテラルで書きます。埋め込んだ値は必ずプレースホルダになり、文字列から SQL を組み立てる方法はありません。"],
      ["既定で守りが固い", "nonce 付きの Content-Security-Policy、CSRF の検査、サーバー側のセッション、レート制限。ファイルに触れるのは <code>phv.toml</code> で許可した場所だけ。"],
      ["速い", "型に特化したレジスタ VM、数値・文字列・list・map を扱う Cranelift JIT、署名付きのバイトコードキャッシュ。<code>fib(30)</code> は約 7 ms。本番のサーバーは、コンパイルしたページと小さな静的ファイルをメモリに持ちます。"],
      ["ひとつのコマンドで全部", "<code>phv serve</code>(HTTPS・HTTP/2・WebSocket)・<code>test</code>・<code>fmt</code>・<code>build</code>(1 つの実行ファイルに)・<code>doc</code>・<code>lsp</code>・デバッガー・<code>install</code>・<code>migrate</code>(PHP からの移行)。VS Code 拡張もあります。"],
    ],
    mistakesTitle: "間違いはコンパイルエラーに",
    mistakesLead: "エスケープだけでは URL は安全になりません。<code>javascript:alert(1)</code> にはエスケープする文字がないからです。PHV は値が <code>href</code> の先頭に入ることを知っていて、確認されるまでコンパイルを通しません。",
    downloadTitle: "ダウンロード",
    downloadLead: (v) => `Windows・Linux・macOS 向けの PHV ${v}。アーカイブを展開して、<code>phv</code> を <code>PATH</code> の通った場所に置いてください。`,
    platform: "プラットフォーム",
    archive: "アーカイブ",
    verify: "同じリリースの <code>SHA256SUMS</code> でアーカイブを確かめてから実行します:",
    source: "ソースからビルドするなら、最近の安定版 Rust と C コンパイラで <code>cargo build --release</code>。",
    docker: (image) => `Docker のイメージ(Linux、amd64 と arm64。中身は <code>phv</code> だけ)でも動かせます: <code>${image}</code>。`,
    packages: `パッケージマネージャーでも入れられます: <code>brew install miyagawayuu/phv/phv</code>(macOS と Linux)、または <code>scoop bucket add phv https://github.com/miyagawayuu/scoop-phv</code> と <code>scoop install phv</code>(Windows)。`,
    editor: (version) => `VS Code では、Marketplace から <a href="${marketplace}">PHV Language</a> を入れると(<code>code --install-extension phv-lang.phv-language</code>)、色分け、入力中のエラー表示、補完、名前の変更、デバッガーが使えます。各リリースにも <a href="${releases}/download/v${version}/phv-${version}.vsix"><code>phv-${version}.vsix</code></a> として付いています。`,
    securityTitle: "セキュリティ",
    securityLead: `PHV は 13 回のセキュリティ監査を受け、そのたびに修正と回帰テストを加えています。<a href="docs/security/">監査報告</a>をご覧ください。脆弱性の報告は <a href="mailto:security@phv-lang.org">security@phv-lang.org</a> へ。`,
    getStarted: "はじめる",
  },
};

export function landing(lang, version, code) {
  const t = text[lang];
  // ARM Linux, the static builds and the Docker image exist from 0.9.0 on.
  const [major, minor] = version.split(".").map(Number);
  const more = major > 0 || minor >= 9;
  const platforms = [
    ["Windows x64", `phv-${version}-x86_64-pc-windows-msvc.zip`],
    ["Linux x64", `phv-${version}-x86_64-unknown-linux-gnu.tar.gz`],
    ...(more
      ? [
          ["Linux ARM64", `phv-${version}-aarch64-unknown-linux-gnu.tar.gz`],
          ["Linux x64 (static)", `phv-${version}-x86_64-unknown-linux-musl.tar.gz`],
          ["Linux ARM64 (static)", `phv-${version}-aarch64-unknown-linux-musl.tar.gz`],
        ]
      : []),
    ["macOS (Apple Silicon)", `phv-${version}-aarch64-apple-darwin.tar.gz`],
    ["macOS (Intel)", `phv-${version}-x86_64-apple-darwin.tar.gz`],
  ];
  const rows = platforms
    .map(([p, file]) => `<tr><td>${p}</td><td><a href="${releases}/download/v${version}/${file}"><code>${file}</code></a></td></tr>`)
    .join("\n");
  const cards = t.features.map(([h, p]) => `<div class="card"><h3>${h}</h3><p>${p}</p></div>`).join("\n");
  const docs = lang === "en" ? "docs/" : "docs/";
  const body = `<section class="hero">
  <div class="hero-text">
    <h1>${t.tagline}</h1>
    <p class="lead">${t.lead}</p>
    <p class="actions"><a class="button primary" href="#download">${t.download} ${version}</a> <a class="button" href="${docs}spec/">${t.readSpec}</a></p>
  </div>
  <div class="hero-code">${code(sample("hero.phv"), "phv")}</div>
</section>

<section class="features">
${cards}
</section>

<section class="split">
  <div>
    <h2>${t.mistakesTitle}</h2>
    <p>${t.mistakesLead}</p>
  </div>
  <div>
    ${code(sample("profile.phv"), "phv")}
    <pre class="terminal"><code>${errorOutput.replace(/&/g, "&amp;").replace(/</g, "&lt;")}</code></pre>
  </div>
</section>

<section id="download" class="download">
  <h2>${t.downloadTitle}</h2>
  <p>${t.downloadLead(version)}</p>
  <div class="table-wrap"><table>
    <thead><tr><th>${t.platform}</th><th>${t.archive}</th></tr></thead>
    <tbody>
${rows}
    </tbody>
  </table></div>
  <p>${t.verify}</p>
  ${code(`sha256sum -c SHA256SUMS --ignore-missing\ntar -xzf phv-${version}-x86_64-unknown-linux-gnu.tar.gz\n./phv-${version}-x86_64-unknown-linux-gnu/phv --version\n./phv-${version}-x86_64-unknown-linux-gnu/phv serve mysite   # http://127.0.0.1:8080`, "bash")}
  ${more ? `<p>${t.packages}</p>\n  <p>${t.docker(`docker run --rm ghcr.io/miyagawayuu/phv:${version} --version`)}</p>` : ""}
  <p>${t.source}</p>
  <p>${t.editor(version)}</p>
</section>

<section class="security">
  <h2>${t.securityTitle}</h2>
  <p>${t.securityLead}</p>
</section>`;
  return { title: t.title, description: t.description, body };
}
