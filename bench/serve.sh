#!/usr/bin/env bash
# Load test of `phv serve --prod` on a copy of examples/site: three pages, N keep-alive
# connections x M requests each, --runs times; the best requests/s and the lowest p50 / p99
# per page are written under "serve" in bench/results/<short-sha>.json. See bench/README.md.
# Needs python3 (for a free port and to update the JSON file).
#
#   bench/serve.sh [--connections 16] [--requests 500] [--runs 5] [--no-build] [--name <label>]
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=common.sh
. "$here/common.sh"

connections=16
requests=500
warmup=50
runs=5
build=1
name=""
while [ $# -gt 0 ]; do
    case "$1" in
        --connections) connections="$2"; shift ;;
        --requests) requests="$2"; shift ;;
        --runs) runs="$2"; shift ;;
        --no-build) build=0 ;;
        --name) name="$2"; shift ;;
        *) echo "unknown option $1 (--connections N, --requests M, --runs R, --no-build, --name LABEL)" >&2; exit 2 ;;
    esac
    shift
done

python="$(python_bin)"
if [ "$build" = 1 ]; then
    build_phv
    echo "cargo build --release -p phv_server --example load"
    cargo build --release -p phv_server --example load --manifest-path "$root/Cargo.toml"
fi
phv="$(phv_path)"
load="$(target_dir)/release/examples/load"
on_windows && load="$load.exe"
if [ ! -x "$load" ]; then
    echo "$load does not exist; run without --no-build" >&2
    exit 1
fi
sha="$(short_sha)"
[ -n "$name" ] || name="$sha"
out="$root/bench/results/$name.json"
mkdir -p "$root/bench/results"

# A copy of the site, so the guestbook database starts the same every time: empty,
# then 20 seeded entries.
tmp="${TMPDIR:-/tmp}"
# Git Bash: a path both bash and phv understand (C:/Users/...), since the paths are passed on.
on_windows && tmp="$(cygpath -m "$tmp")"
site="$tmp/phv-bench-site-$$"
rm -rf "$site"
cp -R "$root/examples/site" "$site"
rm -f "$site"/data/*.db "$site"/data/*.db-*
rm -rf "$site/.phv-cache"
cat >"$site/seed.phv" <<'EOF'
<?phv
import lib.guestbook.{guestbook_db, add_entry, recent_entries};

$db = guestbook_db();
if (recent_entries($db, 1)->isEmpty()) {
    foreach (range(1, 20) as $i) {
        add_entry($db, "Visitor " . $i, "Entry " . $i . " of the benchmark guestbook.");
    }
}
EOF
(cd "$site" && "$phv" run seed.phv)

port="$("$python" -I -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')"
log="$site/serve.log"
server_pid=""
cleanup() {
    if [ -n "$server_pid" ]; then
        kill "$server_pid" 2>/dev/null || true
        wait "$server_pid" 2>/dev/null || true
    fi
    rm -rf "$site"
}
trap cleanup EXIT

echo "phv serve $site --prod --host 127.0.0.1 --port $port"
"$phv" serve "$site" --prod --host 127.0.0.1 --port "$port" 2>"$log" &
server_pid=$!
ready=0
for ((t = 0; t < 300; t++)); do
    if ! kill -0 "$server_pid" 2>/dev/null; then break; fi
    if "$python" -I -c "import socket, sys; s = socket.socket(); s.settimeout(1); sys.exit(0 if s.connect_ex(('127.0.0.1', $port)) == 0 else 1)"; then
        ready=1
        break
    fi
    sleep 0.1
done
if [ "$ready" != 1 ]; then
    echo "the server did not start:" >&2
    cat "$log" >&2 || true
    exit 1
fi

pages=("/" "/archive/2026/11" "/guestbook")
lines=()
for page in "${pages[@]}"; do
    for ((r = 1; r <= runs; r++)); do
        # MSYS_NO_PATHCONV: Git Bash would otherwise turn "/" into the Git install directory.
        if ! line="$(MSYS_NO_PATHCONV=1 "$load" --url "http://127.0.0.1:$port" --path "$page" --connections "$connections" --requests "$requests" --warmup "$warmup" --json)"; then
            echo "load failed on $page: $line" >&2
            cat "$log" >&2 || true
            exit 1
        fi
        lines+=("$line")
        "$python" -I -c "import json, sys; m = json.loads(sys.argv[1]); print('  %-20s run %s: %8.0f req/s  p50 %6.2f ms  p99 %6.2f ms' % (m['path'], sys.argv[2], m['requests_per_s'], m['p50_ms'], m['p99_ms']))" "$line" "$r"
    done
done

"$python" -I - "$out" "$sha" "$connections" "$requests" "$warmup" "$runs" "${lines[@]}" <<'EOF'
import json, sys
out, sha, connections, requests, warmup, runs = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5]), int(sys.argv[6])
pages = {}
for line in sys.argv[7:]:
    m = json.loads(line)
    best = pages.setdefault(m["path"], {"requests_per_s": m["requests_per_s"], "p50_ms": m["p50_ms"], "p99_ms": m["p99_ms"]})
    best["requests_per_s"] = max(best["requests_per_s"], m["requests_per_s"])
    best["p50_ms"] = min(best["p50_ms"], m["p50_ms"])
    best["p99_ms"] = min(best["p99_ms"], m["p99_ms"])
try:
    doc = json.load(open(out, encoding="utf-8"))
except FileNotFoundError:
    doc = {"commit": sha}
doc["serve"] = {
    "mode": "prod",
    "connections": connections,
    "requests_per_connection": requests,
    "warmup_per_connection": warmup,
    "runs": runs,
    "pages": pages,
}
with open(out, "w", encoding="utf-8", newline="") as f:
    json.dump(doc, f, indent=2)
    f.write("\n")
print()
print(f"{'Page':<24} {'req/s':>10} {'p50 ms':>9} {'p99 ms':>9}")
for page, m in pages.items():
    print(f"{page:<24} {m['requests_per_s']:>10.0f} {m['p50_ms']:>9.2f} {m['p99_ms']:>9.2f}")
print()
print(f"Wrote the serve section of {out}")
EOF
