# Benchmark harness

Reproducible numbers for the runtime, the server and the language server, written to
`bench/results/<short-sha>.json` so that two commits can be compared. A pull request that touches
performance attaches the before/after JSON (the baseline's file is usually already here; commit
the new one next to it) and the table that `--compare` prints.

[日本語](README.ja.md)

## Running

Windows (PowerShell 5.1 or later):

```powershell
.\bench\run.ps1 --all                 # everything: micro-benchmarks, server, language server
.\bench\run.ps1 --compare ae06940     # micro-benchmarks, then the ratio against results/ae06940.json
.\bench\serve.ps1                     # the server alone
python bench\lsp.py                   # the language server alone
```

Linux and macOS (bash; `python3` is used for the server script, `lsp.py` and `--compare`):

```bash
bench/run.sh --all
bench/run.sh --compare ae06940
bench/serve.sh
python3 bench/lsp.py
```

Each script builds what it needs (`cargo build --release -p phv_cli`, and the load tool
`crates/phv_server/examples/load.rs`) unless `--no-build` is given. The three write to the same
file, `bench/results/<short-sha>.json`, where the short SHA is that of `HEAD`; `run` writes the
file (keeping the `serve` and `lsp` sections of an earlier run of the same commit), `serve` and
`lsp.py` add their section to it. `"dirty": true` means tracked files under `crates/` differed from
`HEAD` when the numbers were taken.

Close other heavy programs first; the numbers are only as reproducible as the machine is quiet.

## What the numbers mean

**`rows`** (`run.ps1` / `run.sh`): the rows of the table in the repository's README, from
`examples/bench/bench.phv`, which times each benchmark itself with `microtime()` (compile time
is not included). The script is run `runs` times (5 by default) with the JIT on and `runs` times
with it off (`PHV_JIT=0`); `jit_ms` and `interp_ms` are the **minimum** over those runs, the
measurement least disturbed by whatever else the machine was doing. Every run is kept in
`jit_runs_ms` / `interp_runs_ms` so the spread can be seen. `binary_bytes` is the size of the
release `phv` binary; `machine` names the CPU, its logical core count and the OS.

**`serve`** (`serve.ps1` / `serve.sh`): `phv serve --prod` on a copy of `examples/site` (so the
guestbook database starts empty and is seeded with 20 entries every time), on a free port on
127.0.0.1. The load tool opens `connections` keep-alive connections (16) and sends
`requests_per_connection` requests (500) on each, after `warmup_per_connection` requests (50)
that are not counted, for three pages:

| Page | What it exercises |
|---|---|
| `/` | The home page: a template, the layout, a list of posts |
| `/archive/2026/11` | A page three directories deep (`public/archive/2026/11/index.phv`): routing |
| `/guestbook` | A SQLite query (`SELECT ... LIMIT 20`), a session, a CSRF field |

Each page is measured `runs` times (5); `requests_per_s` is the **best** run, `p50_ms` and
`p99_ms` the **lowest** median and 99th-percentile latency (nearest rank) of a single request,
seen by the client. A response other than 200 fails the run.

**`lsp`** (`lsp.py`): `phv lsp` over stdio. The session opens the largest page under
`examples/site/public` (`document`), then `edits` times (20) sends a `didChange` that appends one
line to the page and, at once, a `completion` request at the end of that line (after `$req->`),
as an editor does while someone types. Two latencies per edit, as the editor sees them:
`completion_ms` from sending the request to its answer, and `diagnostics_ms` from sending the edit
to the `publishDiagnostics` for the page (the server answers the completion first, so this includes
it). `median` and `p99` are over the 20 edits; with 20 samples the p99 is the slowest one.
`open_ms` is the time to the first diagnostics after `didOpen`.

## Comparing two commits

```powershell
.\bench\run.ps1 --compare <old-sha>
```

prints, for every number both files have, the ratio new/old: below 1.00 is faster for times and
sizes, above 1.00 is faster for requests per second. Run `serve` and `lsp.py` first (or use
`--all`) if those sections should be compared too. The old file is read before the run writes
its own, so a second run of the same commit compares with the first and shows ratios around 1.00;
differences of a few percent in the micro-benchmarks, and more in the server's p99, are noise.
`--name <label>` writes `results/<label>.json` instead of `<short-sha>.json` (the uncommitted state
of a branch, say) and is accepted by `serve` too (`lsp.py` takes `--out <file>`).

## Files

| File | Purpose |
|---|---|
| `run.ps1`, `run.sh` | Micro-benchmarks and `--compare` |
| `serve.ps1`, `serve.sh` | The server under load |
| `lsp.py` | The language server (Python 3.9 or later) |
| `common.ps1`, `common.sh` | Shared helpers: the binary's path, machine info, JSON |
| `results/` | One JSON file per commit measured |
| `../crates/phv_server/examples/load.rs` | The load tool (`cargo run --release -p phv_server --example load -- --url http://127.0.0.1:8080 --path /`) |
