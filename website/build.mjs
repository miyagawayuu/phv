// Builds the static phv-lang.org site into dist/:
//   /                 landing page (English)     /ja/              landing page (Japanese)
//   /docs/spec/       language specification     /ja/docs/spec/
//   /docs/security/   security audit report      /ja/docs/security/
//   /404.html         page for unknown paths
// The specification and the audit report are rendered from ../docs/*.md, and
// PHV code is highlighted with the VS Code extension's TextMate grammars.

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { Marked } from "marked";
import { createHighlighter } from "shiki";
import { landing } from "./pages.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const dist = path.join(here, "dist");
const version = /^version = "(.*)"/m.exec(fs.readFileSync(path.join(repo, "Cargo.toml"), "utf8"))[1];

const grammar = (file) => JSON.parse(fs.readFileSync(path.join(repo, "editors/vscode/syntaxes", file), "utf8"));
const highlighter = await createHighlighter({
  themes: ["github-light", "github-dark"],
  langs: [
    "html",
    "sql",
    "bash",
    "toml",
    { ...grammar("phv-html.tmLanguage.json"), name: "phv-html", embeddedLangs: ["html", "sql"] },
    { ...grammar("phv.tmLanguage.json"), name: "phv", embeddedLangs: ["phv-html", "sql"] },
  ],
});

/** Highlighted HTML for a code block; light and dark colors via CSS variables. */
export function code(text, lang) {
  const known = ["phv", "phv-html", "html", "sql", "bash", "toml"];
  return highlighter.codeToHtml(text, {
    lang: known.includes(lang) ? lang : "text",
    themes: { light: "github-light", dark: "github-dark" },
    defaultColor: "light",
  });
}

const escape = (s) => s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);

/** Heading ids like GitHub's: lowercase, punctuation removed, spaces to `-`. */
function slug(text, seen) {
  let s = text
    .toLowerCase()
    .replace(/<[^>]+>/g, "")
    .replace(/[^\p{L}\p{N}\s-]/gu, "")
    .trim()
    .replace(/\s+/g, "-");
  const n = seen.get(s) || 0;
  seen.set(s, n + 1);
  return n ? `${s}-${n}` : s;
}

// Where the Markdown files' relative links go on the site.
const docLinks = {
  en: { "spec.md": "../spec/", "spec.ja.md": "../../ja/docs/spec/", "security-audit.md": "../security/", "security-audit.ja.md": "../../ja/docs/security/" },
  ja: { "spec.md": "../../../docs/spec/", "spec.ja.md": "../spec/", "security-audit.md": "../../../docs/security/", "security-audit.ja.md": "../security/" },
};

/** Renders a Markdown document; returns the HTML and its table of contents. */
function renderDoc(markdown, lang) {
  const toc = [];
  const seen = new Map();
  const marked = new Marked({
    renderer: {
      code({ text, lang: l }) {
        return code(text, l || "text");
      },
      heading({ tokens, depth }) {
        const html = this.parser.parseInline(tokens);
        const id = slug(html, seen);
        if (depth === 2 || depth === 3) toc.push({ depth, id, html });
        return `<h${depth} id="${id}"><a class="anchor" href="#${id}" aria-hidden="true">#</a>${html}</h${depth}>\n`;
      },
      link({ href, title, tokens }) {
        const text = this.parser.parseInline(tokens);
        const target = docLinks[lang][href] ?? href;
        const external = /^https?:/.test(target);
        return `<a href="${escape(target)}"${title ? ` title="${escape(title)}"` : ""}${external ? ' rel="noopener"' : ""}>${text}</a>`;
      },
    },
  });
  // The first lines are the title and the language switch; the page layout shows both.
  const body = markdown.replace(/^# .*\n+(.*\| .*\n)?/, "");
  const title = /^# (.*)/m.exec(markdown)[1];
  // Wide tables scroll inside their own box instead of the page.
  const html = marked.parse(body).replace(/<table>/g, '<div class="table-wrap"><table>').replace(/<\/table>/g, "</table></div>");
  return { title, html, toc };
}

const strings = {
  en: { docs: "Docs", spec: "Specification", security: "Security", download: "Download", contents: "Contents", other: "日本語", otherLang: "ja" },
  ja: { docs: "ドキュメント", spec: "言語仕様", security: "セキュリティ", download: "ダウンロード", contents: "目次", other: "English", otherLang: "en" },
};

/** The page frame shared by every page. `depth` is how far the page is below the site root. */
export function layout({ lang, title, description, body, depth, active, otherHref, wide, absolute }) {
  // Links are relative so the site works from any directory; the 404 page,
  // which is served at any path, uses root-relative links instead.
  const up = absolute ? "/" : "../".repeat(depth);
  const t = strings[lang];
  const home = lang === "en" ? up || "./" : `${up}ja/`;
  const docs = lang === "en" ? `${up}docs/` : `${up}ja/docs/`;
  const nav = (key, href, label) => `<a href="${href}"${active === key ? ' aria-current="page"' : ""}>${label}</a>`;
  return `<!doctype html>
<html lang="${lang}">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>${escape(title)}</title>
<meta name="description" content="${escape(description)}">
<meta name="color-scheme" content="light dark">
<link rel="icon" href="${up}favicon.svg" type="image/svg+xml">
<link rel="apple-touch-icon" href="${up}apple-touch-icon.png">
<link rel="stylesheet" href="${up}style.css">
<script src="${up}site.js" defer></script>
</head>
<body>
<header class="site-header">
  <div class="inner">
    <a class="logo" href="${home}"><img src="${up}logo.svg" alt="" width="30" height="30">PHV</a>
    <nav>
      ${nav("spec", `${docs}spec/`, t.spec)}
      ${nav("security", `${docs}security/`, t.security)}
      ${nav("download", `${home}#download`, t.download)}
      <a class="lang" href="${otherHref}" hreflang="${t.otherLang}" lang="${t.otherLang}">${t.other}</a>
    </nav>
  </div>
</header>
<main class="${wide ? "wide" : ""}">
${body}
</main>
<footer class="site-footer">
  <div class="inner">
    <p>PHV ${version} · MIT License · <a href="mailto:security@phv-lang.org">security@phv-lang.org</a></p>
  </div>
</footer>
</body>
</html>
`;
}

function write(rel, html) {
  const file = path.join(dist, rel);
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, html);
}

function docPage(src, lang, out, active, otherHref) {
  const { title, html, toc } = renderDoc(fs.readFileSync(path.join(repo, "docs", src), "utf8"), lang);
  const items = toc
    .map((h) => `<li class="d${h.depth}"><a href="#${h.id}">${h.html.replace(/<a[^>]*>|<\/a>/g, "")}</a></li>`)
    .join("\n");
  const body = `<div class="doc">
<details class="toc" id="toc"><summary>${strings[lang].contents}</summary><ol>${items}</ol></details>
<article class="prose"><h1>${escape(title)}</h1>
${html}</article>
</div>`;
  const depth = out.split("/").length - 1;
  write(out, layout({ lang, title: `${title} · PHV`, description: title, body, depth, active, otherHref, wide: true }));
}

fs.rmSync(dist, { recursive: true, force: true });
fs.mkdirSync(dist, { recursive: true });
// style.css, site.js, the logo and icons, and _headers (HTTP headers for Cloudflare Pages).
for (const f of fs.readdirSync(path.join(here, "static"))) {
  fs.copyFileSync(path.join(here, "static", f), path.join(dist, f));
}

for (const lang of ["en", "ja"]) {
  const page = landing(lang, version, code);
  const out = lang === "en" ? "index.html" : "ja/index.html";
  const depth = lang === "en" ? 0 : 1;
  write(out, layout({ lang, ...page, depth, active: "home", otherHref: lang === "en" ? "ja/" : "../" }));
}
docPage("spec.md", "en", "docs/spec/index.html", "spec", "../../ja/docs/spec/");
docPage("spec.ja.md", "ja", "ja/docs/spec/index.html", "spec", "../../../docs/spec/");
docPage("security-audit.md", "en", "docs/security/index.html", "security", "../../ja/docs/security/");
docPage("security-audit.ja.md", "ja", "ja/docs/security/index.html", "security", "../../../docs/security/");
for (const dir of ["docs", "ja/docs"]) {
  write(`${dir}/index.html`, `<!doctype html><meta charset="utf-8"><meta http-equiv="refresh" content="0; url=spec/"><a href="spec/">spec/</a>
`);
}
write(
  "404.html",
  layout({
    lang: "en",
    title: "Page not found · PHV",
    description: "Page not found",
    body: `<section class="notfound"><h1>Page not found</h1><p>There is no page at this address. <a href="/">Go to the PHV home page</a> or read the <a href="/docs/spec/">specification</a>.</p><p lang="ja">ページが見つかりません。<a href="/ja/">トップページ</a>へ。</p></section>`,
    depth: 0,
    absolute: true,
    otherHref: "/ja/",
  }),
);

const count = (dir) => fs.readdirSync(dir, { withFileTypes: true }).reduce((n, e) => n + (e.isDirectory() ? count(path.join(dir, e.name)) : 1), 0);
console.log(`built ${count(dist)} files into ${path.relative(process.cwd(), dist) || "dist"}`);
