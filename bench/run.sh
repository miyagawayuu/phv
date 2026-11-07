#!/usr/bin/env bash
# The micro-benchmarks of examples/bench/bench.phv: a release build, the script run
# --runs times with the JIT on and --runs times with it off (PHV_JIT=0), the minimum
# per row, written to bench/results/<short-sha>.json. See bench/README.md.
#
#   bench/run.sh [--runs 5] [--no-build] [--all] [--name <label>] [--compare <sha>]
#
#   --all            also run bench/serve.sh and bench/lsp.py (they add to the same file)
#   --name <label>   write bench/results/<label>.json instead of <short-sha>.json
#   --compare <sha>  afterwards, print the ratio new/old per row against bench/results/<sha>.json
#                    (read before this run writes its file, so comparing a commit with itself
#                    works; needs python3)
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=common.sh
. "$here/common.sh"

runs=5
build=1
all=0
name=""
compare=""
while [ $# -gt 0 ]; do
    case "$1" in
        --runs) runs="$2"; shift ;;
        --no-build) build=0 ;;
        --all) all=1 ;;
        --name) name="$2"; shift ;;
        --compare) compare="$2"; shift ;;
        *) echo "unknown option $1 (--runs N, --no-build, --all, --name LABEL, --compare SHA)" >&2; exit 2 ;;
    esac
    shift
done

[ "$build" = 1 ] && build_phv
phv="$(phv_path)"
sha="$(short_sha)"
mkdir -p "$root/bench/results"
[ -n "$name" ] || name="$sha"
out="$root/bench/results/$name.json"
bench="$root/examples/bench/bench.phv"
before=""
if [ -n "$compare" ]; then
    if [ ! -f "$root/bench/results/$compare.json" ]; then
        echo "$root/bench/results/$compare.json does not exist" >&2
        exit 1
    fi
    before="$(mktemp)"
    cp "$root/bench/results/$compare.json" "$before"
    trap 'rm -f "$before"' EXIT
fi

# The rows of the README table, in the order bench.phv prints them.
ids=(fib loop append mandelbrot concat map_filter map_updates sieve list_floats prop_append string_eq escape new_objects throw_catch sort_by)
labels=(
    "fib(30) (recursive calls)"
    "10 million loop iterations"
    "Concatenating 1 million characters"
    "Mandelbrot 300x300 (floats)"
    "Building 1 million strings from numbers"
    "map / filter / sum over 1 million elements"
    "200,000 map updates"
    "Sieve of Eratosthenes up to 1 million (list<bool>)"
    "Building and summing a list of 1 million floats"
    "Appending to a property with .= 200,000 times"
    "1 million string comparisons (==)"
    "Escaping 200,000 strings for HTML"
    "Creating 1 million objects"
    "Throwing and catching 100,000 exceptions"
    "Sorting 200,000 elements with sortBy"
)
nrows=${#ids[@]}

# Runs bench.phv once ($1 = "1" for the JIT, "0" without) and prints the milliseconds of each row.
run_bench() {
    local output
    if [ "$1" = 1 ]; then
        output="$(env -u PHV_JIT "$phv" run "$bench")"
    else
        output="$(PHV_JIT=0 "$phv" run "$bench")"
    fi
    local times
    times="$(printf '%s\n' "$output" | sed -n -E 's/.*[: ]([0-9,.]+) ms[[:space:]]*$/\1/p' | tr -d ,)"
    if [ "$(printf '%s\n' "$times" | wc -l | tr -d ' ')" != "$nrows" ]; then
        echo "expected $nrows timed lines from bench.phv, got:" >&2
        printf '%s\n' "$output" >&2
        exit 1
    fi
    printf '%s\n' "$times"
}

echo "Running $bench $runs times with the JIT on and $runs times with it off..."
jit_runs=()    # one line of space-separated times per run
interp_runs=()
for ((r = 1; r <= runs; r++)); do
    jit_runs+=("$(run_bench 1 | tr '\n' ' ')")
    interp_runs+=("$(run_bench 0 | tr '\n' ' ')")
    echo "  run $r of $runs done"
done

# Column $1 (0-based) of every run in $2... as a JSON array, and its minimum.
column() {
    local k="$1"; shift
    local run vals=()
    for run in "$@"; do
        read -r -a fields <<<"$run"
        vals+=("${fields[$k]}")
    done
    printf '%s\n' "${vals[@]}"
}
# One value per line, as Python's json.dump(indent=2) writes it (the other scripts rewrite the file).
json_array() {
    local first=1 v
    printf '[\n'
    for v in "$@"; do
        [ $first = 1 ] || printf ',\n'
        printf '        %s' "$(json_number "$v")"
        first=0
    done
    printf '\n      ]'
}
min_of() { printf '%s\n' "$@" | sort -g | head -n 1; }

machine="$(machine_json)"
size="$(binary_bytes "$phv")"
{
    printf '{\n'
    printf '  "commit": "%s",\n' "$sha"
    printf '  "dirty": %s,\n' "$(dirty)"
    printf '  "date": "%s",\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    printf '  "machine": %s,\n' "$machine"
    printf '  "binary": "%s",\n' "$(basename "$phv")"
    printf '  "binary_bytes": %s,\n' "$size"
    printf '  "runs": %s,\n' "$runs"
    printf '  "rows": [\n'
    for ((k = 0; k < nrows; k++)); do
        jit=( $(column "$k" "${jit_runs[@]}") )
        interp=( $(column "$k" "${interp_runs[@]}") )
        printf '    {\n'
        printf '      "id": "%s",\n' "${ids[$k]}"
        printf '      "label": "%s",\n' "${labels[$k]}"
        printf '      "jit_ms": %s,\n' "$(json_number "$(min_of "${jit[@]}")")"
        printf '      "interp_ms": %s,\n' "$(json_number "$(min_of "${interp[@]}")")"
        printf '      "jit_runs_ms": %s,\n' "$(json_array "${jit[@]}")"
        printf '      "interp_runs_ms": %s\n' "$(json_array "${interp[@]}")"
        if [ "$k" -lt $((nrows - 1)) ]; then printf '    },\n'; else printf '    }\n'; fi
    done
    printf '  ]\n'
    printf '}\n'
} >"$out.new"
# Keep the serve / lsp sections of an earlier run of the same commit.
if [ -f "$out" ] && have_python; then
    "$(python_bin)" -I - "$out" "$out.new" <<'EOF'
import json, sys
old = json.load(open(sys.argv[1], encoding="utf-8"))
new = json.load(open(sys.argv[2], encoding="utf-8"))
for section in ("serve", "lsp"):
    if section in old:
        new[section] = old[section]
with open(sys.argv[2], "w", encoding="utf-8", newline="") as f:
    json.dump(new, f, indent=2)
    f.write("\n")
EOF
fi
mv -f "$out.new" "$out"

echo
printf '%-52s %10s %12s\n' "Benchmark" "JIT" "interpreter"
for ((k = 0; k < nrows; k++)); do
    jit=( $(column "$k" "${jit_runs[@]}") )
    interp=( $(column "$k" "${interp_runs[@]}") )
    printf '%-52s %7.1f ms %9.1f ms\n' "${labels[$k]}" "$(min_of "${jit[@]}")" "$(min_of "${interp[@]}")"
done
echo
echo "Wrote $out"

if [ "$all" = 1 ]; then
    # serve.sh builds the load tool itself (phv is up to date, so that build is quick).
    if [ "$build" = 1 ]; then
        "$here/serve.sh" --name "$name"
    else
        "$here/serve.sh" --no-build --name "$name"
    fi
    "$(python_bin)" -I "$here/lsp.py" --out "$out"
fi

if [ -n "$compare" ]; then
    compare_results "$before" "$out"
fi
