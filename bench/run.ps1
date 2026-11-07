# The micro-benchmarks of examples/bench/bench.phv: a release build, the script run
# --runs times with the JIT on and --runs times with it off (PHV_JIT=0), the minimum
# per row, written to bench/results/<short-sha>.json. See bench/README.md.
#
#   .\bench\run.ps1 [--runs 5] [--no-build] [--all] [--name <label>] [--compare <sha>]
#
#   --all            also run bench/serve.ps1 and bench/lsp.py (they add to the same file)
#   --name <label>   write bench/results/<label>.json instead of <short-sha>.json
#   --compare <sha>  afterwards, print the ratio new/old per row against bench/results/<sha>.json
#                    (read before this run writes its file, so comparing a commit with itself works)

$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "common.ps1")

$runs = 5
$build = $true
$all = $false
$name = $null
$compare = $null
for ($i = 0; $i -lt $args.Count; $i++) {
    switch ($args[$i]) {
        "--runs" { $i++; $runs = [int]$args[$i] }
        "--no-build" { $build = $false }
        "--all" { $all = $true }
        "--name" { $i++; $name = [string]$args[$i] }
        "--compare" { $i++; $compare = [string]$args[$i] }
        default { throw "unknown option $($args[$i]) (--runs N, --no-build, --all, --name LABEL, --compare SHA)" }
    }
}
if ($runs -lt 1) { throw "--runs must be at least 1" }

$root = Get-RepoRoot
if ($build) { Build-Phv $root }
$phv = Get-PhvPath $root
$sha = Get-ShortSha $root
$resultsDir = Join-Path $root "bench\results"
New-Item -ItemType Directory -Force $resultsDir | Out-Null
if (-not $name) { $name = $sha }
$out = Join-Path $resultsDir "$name.json"
$before = $null
if ($compare) {
    $comparePath = Join-Path $resultsDir "$compare.json"
    if (-not (Test-Path $comparePath)) { throw "$comparePath does not exist" }
    $before = Read-Json $comparePath
}

# The rows of the README table, in the order bench.phv prints them.
$rows = @(
    @("fib", "fib(30) (recursive calls)"),
    @("loop", "10 million loop iterations"),
    @("append", "Concatenating 1 million characters"),
    @("mandelbrot", "Mandelbrot 300x300 (floats)"),
    @("concat", "Building 1 million strings from numbers"),
    @("map_filter", "map / filter / sum over 1 million elements"),
    @("map_updates", "200,000 map updates"),
    @("sieve", "Sieve of Eratosthenes up to 1 million (list<bool>)"),
    @("list_floats", "Building and summing a list of 1 million floats"),
    @("prop_append", "Appending to a property with .= 200,000 times"),
    @("string_eq", "1 million string comparisons (==)"),
    @("escape", "Escaping 200,000 strings for HTML"),
    @("new_objects", "Creating 1 million objects"),
    @("throw_catch", "Throwing and catching 100,000 exceptions"),
    @("sort_by", "Sorting 200,000 elements with sortBy")
)
$bench = Join-Path $root "examples\bench\bench.phv"

# Runs bench.phv once and returns the milliseconds of each row, in order.
function Invoke-Bench([bool] $jit) {
    if ($jit) { Remove-Item Env:PHV_JIT -ErrorAction SilentlyContinue } else { $env:PHV_JIT = "0" }
    try {
        $lines = & $phv run $bench 2>&1 | ForEach-Object { "$_" }
        if ($LASTEXITCODE -ne 0) { throw "phv run $bench failed:`n$($lines -join "`n")" }
    } finally {
        Remove-Item Env:PHV_JIT -ErrorAction SilentlyContinue
    }
    $times = @()
    foreach ($line in $lines) {
        if ($line -match "([0-9,.]+) ms\s*$") {
            $times += [double]::Parse($Matches[1].Replace(",", ""), [System.Globalization.CultureInfo]::InvariantCulture)
        }
    }
    if ($times.Count -ne $rows.Count) {
        throw "expected $($rows.Count) timed lines from bench.phv, got $($times.Count):`n$($lines -join "`n")"
    }
    return $times
}

Write-Host "Running $bench $runs times with the JIT on and $runs times with it off..."
$jitRuns = @()
$interpRuns = @()
for ($r = 1; $r -le $runs; $r++) {
    $jitRuns += , (Invoke-Bench $true)
    $interpRuns += , (Invoke-Bench $false)
    Write-Host "  run $r of $runs done"
}

$result = [ordered]@{
    commit = $sha
    dirty = (Test-Dirty $root)
    date = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
    machine = (Get-MachineInfo)
    binary = (Split-Path -Leaf $phv)
    binary_bytes = (Get-Item $phv).Length
    runs = $runs
    rows = @()
}
for ($k = 0; $k -lt $rows.Count; $k++) {
    $jit = @($jitRuns | ForEach-Object { $_[$k] })
    $interp = @($interpRuns | ForEach-Object { $_[$k] })
    $result.rows += [ordered]@{
        id = $rows[$k][0]
        label = $rows[$k][1]
        jit_ms = ($jit | Measure-Object -Minimum).Minimum
        interp_ms = ($interp | Measure-Object -Minimum).Minimum
        jit_runs_ms = $jit
        interp_runs_ms = $interp
    }
}

# Keep the serve / lsp / lsp_typing sections of an earlier run of the same commit.
if (Test-Path $out) {
    $old = Read-Json $out
    foreach ($section in @("serve", "lsp", "lsp_typing")) {
        if ($old.PSObject.Properties[$section]) { $result[$section] = $old.$section }
    }
}
Write-Json $out $result
Write-Host ""
Write-Host ("{0,-52} {1,10} {2,12}" -f "Benchmark", "JIT", "interpreter")
foreach ($row in $result.rows) {
    Write-Host ("{0,-52} {1,7:N1} ms {2,9:N1} ms" -f $row.label, $row.jit_ms, $row.interp_ms)
}
Write-Host ""
Write-Host "Wrote $out"

if ($all) {
    # serve.ps1 builds the load tool itself (phv is up to date, so that build is quick).
    $serveArgs = @("--name", $name)
    if (-not $build) { $serveArgs = @("--no-build") + $serveArgs }
    & (Join-Path $PSScriptRoot "serve.ps1") @serveArgs
    if ($LASTEXITCODE -ne 0) { throw "serve.ps1 failed" }
    $python = Get-Python
    & $python -I (Join-Path $PSScriptRoot "lsp.py") --out $out
    if ($LASTEXITCODE -ne 0) { throw "lsp.py failed" }
}

if ($compare) {
    Compare-Results $before (Read-Json $out)
}
