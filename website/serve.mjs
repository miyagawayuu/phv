// Serves dist/ on http://127.0.0.1:4173 to preview the site (development only).

import http from "node:http";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const dist = path.join(path.dirname(fileURLToPath(import.meta.url)), "dist");
const types = { ".html": "text/html; charset=utf-8", ".css": "text/css", ".js": "text/javascript", ".svg": "image/svg+xml" };
const port = Number(process.env.PORT || 4173);

// The headers Cloudflare Pages sends for every path (the `/*` block of _headers),
// so the preview runs under the same Content-Security-Policy.
const headers = {};
const lines = fs.readFileSync(path.join(dist, "_headers"), "utf8").split("\n");
for (const line of lines.slice(lines.indexOf("/*") + 1)) {
  const m = /^\s+([\w-]+):\s*(.*)$/.exec(line);
  if (!m) break;
  headers[m[1].toLowerCase()] = m[2];
}

http
  .createServer((req, res) => {
    let rel = decodeURIComponent(new URL(req.url, "http://x").pathname);
    if (rel.endsWith("/")) rel += "index.html";
    const file = path.join(dist, path.normalize(rel));
    if (!file.startsWith(dist + path.sep) || !fs.existsSync(file) || fs.statSync(file).isDirectory()) {
      // Like Cloudflare Pages: unknown paths get 404.html with status 404.
      res.writeHead(404, { ...headers, "content-type": "text/html; charset=utf-8" });
      fs.createReadStream(path.join(dist, "404.html")).pipe(res);
      return;
    }
    res.writeHead(200, { ...headers, "content-type": types[path.extname(file)] || "application/octet-stream" });
    fs.createReadStream(file).pipe(res);
  })
  .listen(port, "127.0.0.1", () => console.log(`http://127.0.0.1:${port}`));
