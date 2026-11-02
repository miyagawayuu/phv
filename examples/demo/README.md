# PHV live demo

The site behind <https://demo.phv-lang.org>. Every page is a PHV script and shows its own source.

| Page | What it shows |
|---|---|
| `/` | A greeting form (escaped output) |
| `/escape` | Escaping by position: text, attribute, `safe_url()`, `json_html()` in `<script>`, CSP nonces |
| `/board` | SQLite with `sql"..."`, automatic CSRF checks, sessions, flash messages, rate limiting |
| `/totp` | `totp_secret()`, `qr_svg()`, `totp_verify()` with replay protection |
| `/api` | JSON endpoints (`public/api/*.phv`) called with `fetch()` and `X-CSRF-Token` |

## Run locally

```bash
phv serve examples/demo --port 8090
phv test examples/demo
```

## Deploy

The demo runs on [Fly.io](https://fly.io) from the released Linux binary (see `PHV_VERSION` in the
`Dockerfile`). The SQLite file lives on a volume mounted at `/app/data`.

```bash
cd examples/demo
fly volumes create phv_demo_data --region nrt --size 1   # once
fly deploy
```

Behind Fly's proxy `request()->ip` is the proxy's address, so the pages rate-limit by the `Fly-Client-IP`
header (`lib/client.phv`) and `[security] requests_per_minute` stays off.
