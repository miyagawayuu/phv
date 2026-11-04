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
    let rel;
    try {
      rel = decodeURIComponent(new URL(req.url, "http://x").pathname);
    } catch {
      // A malformed URL or escape (`%E0%A4%A`) is the request's fault, not a reason to stop.
      res.writeHead(400, { "content-type": "text/plain; charset=utf-8" });
      res.end("bad request");
      return;
    }
    if (rel.endsWith("/")) rel += "index.html";
    const file = path.join(dist, path.normalize(rel));
    // A file that vanishes or cannot be read (a rebuild of dist/) ends this
    // response, not the server.
    const send = (status, name, type) => {
      const stream = fs.createReadStream(name);
      stream.on("open", () => {
        res.writeHead(status, { ...headers, "content-type": type });
        stream.pipe(res);
      });
      stream.on("error", () => {
        if (!res.headersSent) res.writeHead(500, { "content-type": "text/plain; charset=utf-8" });
        res.end();
      });
    };
    let stat;
    try {
      stat = file.startsWith(dist + path.sep) ? fs.statSync(file) : undefined;
    } catch {
      stat = undefined;
    }
    if (!stat || stat.isDirectory()) {
      // Like Cloudflare Pages: unknown paths get 404.html with status 404.
      send(404, path.join(dist, "404.html"), "text/html; charset=utf-8");
      return;
    }
    send(200, file, types[path.extname(file)] || "application/octet-stream");
  })
  .listen(port, "127.0.0.1", () => console.log(`http://127.0.0.1:${port}`));
