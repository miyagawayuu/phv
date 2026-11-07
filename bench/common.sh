# Shared by bench/run.sh and bench/serve.sh (sourced). Needs bash, git, cargo;
# python3 for serve.sh, lsp.py and --compare.

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export LC_NUMERIC=C

short_sha() { git -C "$root" rev-parse --short HEAD; }

# "true" when tracked files under crates/ (the code being measured) differ from HEAD.
dirty() {
    if [ -n "$(git -C "$root" status --porcelain --untracked-files=no -- crates)" ]; then echo true; else echo false; fi
}

target_dir() { echo "${CARGO_TARGET_DIR:-$root/target}"; }

on_windows() {
    case "$(uname -s)" in
        MINGW* | MSYS* | CYGWIN*) return 0 ;;
        *) return 1 ;;
    esac
}

build_phv() {
    echo "cargo build --release -p phv_cli"
    cargo build --release -p phv_cli --manifest-path "$root/Cargo.toml"
}

phv_path() {
    local phv="$(target_dir)/release/phv"
    on_windows && phv="$phv.exe"
    if [ ! -x "$phv" ]; then
        echo "$phv does not exist; run without --no-build" >&2
        exit 1
    fi
    echo "$phv"
}

python_bin() {
    local p
    for p in python3 python py; do
        if command -v "$p" >/dev/null 2>&1 && "$p" -c 'import sys; sys.exit(0 if sys.version_info >= (3, 9) else 1)' >/dev/null 2>&1; then
            command -v "$p"
            return 0
        fi
    done
    echo "Python 3 was not found" >&2
    return 1
}
have_python() { python_bin >/dev/null 2>&1; }

binary_bytes() {
    if stat -c %s "$1" >/dev/null 2>&1; then stat -c %s "$1"; else stat -f %z "$1"; fi
}

cpu_name() {
    case "$(uname -s)" in
        Darwin) sysctl -n machdep.cpu.brand_string ;;
        Linux) sed -n -E 's/^model name[[:space:]]*: *//p' /proc/cpuinfo | head -n 1 ;;
        MINGW* | MSYS* | CYGWIN*)
            powershell.exe -NoProfile -Command '(Get-CimInstance Win32_Processor | Select-Object -First 1).Name.Trim()' 2>/dev/null | tr -d '\r'
            ;;
        *) uname -m ;;
    esac
}

os_name() {
    case "$(uname -s)" in
        Darwin) echo "macOS $(sw_vers -productVersion 2>/dev/null)" ;;
        Linux)
            if [ -r /etc/os-release ]; then
                (. /etc/os-release && echo "${PRETTY_NAME:-Linux}")
            else
                uname -sr
            fi
            ;;
        MINGW* | MSYS* | CYGWIN*)
            powershell.exe -NoProfile -Command '(Get-CimInstance Win32_OperatingSystem).Caption.Trim()' 2>/dev/null | tr -d '\r'
            ;;
        *) uname -sr ;;
    esac
}

core_count() {
    nproc 2>/dev/null || sysctl -n hw.logicalcpu 2>/dev/null || getconf _NPROCESSORS_ONLN
}

json_string() {
    local s="$1"
    s="${s//\\/\\\\}"
    s="${s//\"/\\\"}"
    printf '"%s"' "$s"
}

# A number as JSON (bench.phv always prints one decimal).
json_number() { printf '%s' "$1"; }

machine_json() {
    printf '{\n    "os": %s,\n    "cpu": %s,\n    "cores": %s\n  }' "$(json_string "$(os_name)")" "$(json_string "$(cpu_name)")" "$(core_count)"
}

# Prints the ratio new/old of every number both files have (python3).
compare_results() {
    "$(python_bin)" -I - "$1" "$2" <<'EOF'
import json, sys

def load(path):
    try:
        return json.load(open(path, encoding="utf-8"))
    except FileNotFoundError:
        sys.exit(f"{path} does not exist")

old, new = load(sys.argv[1]), load(sys.argv[2])

def ratio(a, b):
    return "n/a" if not a else f"{b / a:.2f}"

print()
print(f"Ratio new/old: {new.get('commit')} / {old.get('commit')} (times: below 1.00 is faster; requests/s: above 1.00 is faster)")
print()
if old.get("rows") and new.get("rows"):
    print(f"{'Benchmark':<52} {'old JIT':>9} {'new JIT':>9} {'ratio':>6}   {'old int.':>9} {'new int.':>9} {'ratio':>6}")
    by_id = {r["id"]: r for r in old["rows"]}
    for r in new["rows"]:
        o = by_id.get(r["id"])
        if not o:
            continue
        print(f"{r['label']:<52} {o['jit_ms']:>9.1f} {r['jit_ms']:>9.1f} {ratio(o['jit_ms'], r['jit_ms']):>6}   "
              f"{o['interp_ms']:>9.1f} {r['interp_ms']:>9.1f} {ratio(o['interp_ms'], r['interp_ms']):>6}")
    print()
if old.get("binary_bytes") and new.get("binary_bytes"):
    print(f"{'Binary size (bytes)':<52} {old['binary_bytes']:>9} {new['binary_bytes']:>9} {ratio(old['binary_bytes'], new['binary_bytes']):>6}")
    print()
op, np_ = (old.get("serve") or {}).get("pages"), (new.get("serve") or {}).get("pages")
if op and np_:
    print(f"{'Page':<24} {'old req/s':>9} {'new req/s':>9} {'ratio':>6}   {'old p50':>8} {'new p50':>8} {'ratio':>6}   {'old p99':>8} {'new p99':>8} {'ratio':>6}")
    for page, b in np_.items():
        a = op.get(page)
        if not a:
            continue
        print(f"{page:<24} {a['requests_per_s']:>9.0f} {b['requests_per_s']:>9.0f} {ratio(a['requests_per_s'], b['requests_per_s']):>6}   "
              f"{a['p50_ms']:>8.2f} {b['p50_ms']:>8.2f} {ratio(a['p50_ms'], b['p50_ms']):>6}   "
              f"{a['p99_ms']:>8.2f} {b['p99_ms']:>8.2f} {ratio(a['p99_ms'], b['p99_ms']):>6}")
    print()
ol, nl = old.get("lsp"), new.get("lsp")
if ol and nl:
    print(f"{'LSP (ms)':<24} {'old':>9} {'new':>9} {'ratio':>6}")
    pairs = [
        ("open", ol.get("open_ms"), nl.get("open_ms")),
        ("diagnostics median", ol["diagnostics_ms"]["median"], nl["diagnostics_ms"]["median"]),
        ("diagnostics p99", ol["diagnostics_ms"]["p99"], nl["diagnostics_ms"]["p99"]),
        ("completion median", ol["completion_ms"]["median"], nl["completion_ms"]["median"]),
        ("completion p99", ol["completion_ms"]["p99"], nl["completion_ms"]["p99"]),
    ]
    for name, a, b in pairs:
        if a is None or b is None:
            continue
        print(f"{name:<24} {a:>9.2f} {b:>9.2f} {ratio(a, b):>6}")
    print()
EOF
}
