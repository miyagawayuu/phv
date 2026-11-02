# WARDOGS match status

Reads the match on a [WARDOGS](https://wardogs.com) dedicated server through its RCON API and shows it
in the terminal or as a web page that refreshes itself: map, lighting, game mode, player count, match
time, each faction's score, and every player's kills, deaths, cash and ping, grouped by faction.

The RCON listener of a WARDOGS server is a plain HTTP API that answers JSON
(`GET /v1/status`, `GET /v1/players`, with `Authorization: Bearer <password>`). It is set in
`[/Script/WDRCON.WDRCONSettings]` of `ServerSettings.ini` (`Port`, 7776 by default, and `Password`).
The routes are documented by the [warcon](https://github.com/warcon-app/warcon/blob/main/docs/wardogs-api.md)
project.

| File | What it does |
|---|---|
| `lib/wardogs.phv` | Reads `/v1/status` and `/v1/players` into typed classes (`MatchStatus`, `Player`, `Snapshot`) |
| `status.phv` | Prints the match in the terminal |
| `public/index.phv` | The web page (`views/status.phv.html`), refreshed every 10 seconds |
| `mock/` | A stand-in listener with a made-up match, for trying this without a server |
| `tests/` | Tests of the parsing (no server needed) |

## Set up

The address and the password are environment variables, read through `[http.wardogs]` in `phv.toml`,
so they never appear in the code:

```bash
export WARDOGS_RCON_URL="http://203.0.113.10:7776"
export WARDOGS_RCON_AUTH="Bearer <RCON password>"
```

On Windows (PowerShell): `$env:WARDOGS_RCON_URL = "http://203.0.113.10:7776"` and
`$env:WARDOGS_RCON_AUTH = "Bearer <RCON password>"`.

## Run

```bash
phv run examples/wardogs/status.phv            # once, in the terminal
phv serve examples/wardogs --port 8110         # the web page at http://localhost:8110
phv test examples/wardogs
```

To try it without a server, start the mock and point the variables at it:

```bash
phv serve examples/wardogs/mock --port 7776
WARDOGS_RCON_URL=http://127.0.0.1:7776 WARDOGS_RCON_AUTH="Bearer mock-password" phv run examples/wardogs/status.phv
```

## Notes

- Live builds of the game do not send `scoreCap` or `matchSeconds`. The score bars are then scaled to the
  leader, and the time is not shown.
- The listener allows 600 requests a minute per address, and each look sends two. `phv.toml` limits each
  visitor of the web page to 60 requests a minute.
- The listener speaks plain HTTP, so `phv.toml` has `tls = false`. Across the internet the password then
  travels unencrypted: put the listener behind a TLS proxy (and remove `tls = false`), or run this tool on
  the game server's machine and keep `BindAddress=127.0.0.1`.
- Only reading routes are used. Kicking, banning and changing the map are `POST` routes of the same API
  (`HttpClient::postJson`) and can be added to `lib/wardogs.phv` the same way.
