# Load test of `phv serve --prod` on a copy of examples/site: three pages, N keep-alive
# connections x M requests each, --runs times; the best requests/s and the lowest p50 / p99
# per page are written under "serve" in bench/results/<short-sha>.json. See bench/README.md.
#
#   .\bench\serve.ps1 [--connections 16] [--requests 500] [--runs 5] [--no-build] [--name <label>]

$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "common.ps1")

$connections = 16
$requests = 500
$warmup = 50
$runs = 5
$build = $true
$name = $null
for ($i = 0; $i -lt $args.Count; $i++) {
    switch ($args[$i]) {
        "--connections" { $i++; $connections = [int]$args[$i] }
        "--requests" { $i++; $requests = [int]$args[$i] }
        "--runs" { $i++; $runs = [int]$args[$i] }
        "--no-build" { $build = $false }
        "--name" { $i++; $name = [string]$args[$i] }
        default { throw "unknown option $($args[$i]) (--connections N, --requests M, --runs R, --no-build, --name LABEL)" }
    }
}

$root = Get-RepoRoot
$manifest = Join-Path $root "Cargo.toml"
if ($build) {
    Build-Phv $root
    Write-Host "cargo build --release -p phv_server --example load"
    & cargo build --release -p phv_server --example load --manifest-path $manifest
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
}
$phv = Get-PhvPath $root
$load = Join-Path (Join-Path (Join-Path (Get-TargetDir $root) "release") "examples") (Split-Path -Leaf $phv).Replace("phv", "load")
if (-not (Test-Path $load)) { throw "$load does not exist; run without --no-build" }
$sha = Get-ShortSha $root
if (-not $name) { $name = $sha }
$out = Join-Path (Join-Path $root "bench\results") "$name.json"

# A copy of the site, so the guestbook database starts the same every time: empty,
# then 20 seeded entries.
$site = Join-Path ([System.IO.Path]::GetTempPath()) "phv-bench-site-$PID"
if (Test-Path $site) { Remove-Item -Recurse -Force $site }
Copy-Item -Recurse (Join-Path $root "examples\site") $site
Get-ChildItem (Join-Path $site "data") -Filter "*.db*" -ErrorAction SilentlyContinue | Remove-Item -Force
Remove-Item -Recurse -Force (Join-Path $site ".phv-cache") -ErrorAction SilentlyContinue
$seed = @"
<?phv
import lib.guestbook.{guestbook_db, add_entry, recent_entries};

`$db = guestbook_db();
if (recent_entries(`$db, 1)->isEmpty()) {
    foreach (range(1, 20) as `$i) {
        add_entry(`$db, "Visitor " . `$i, "Entry " . `$i . " of the benchmark guestbook.");
    }
}
"@
[System.IO.File]::WriteAllText((Join-Path $site "seed.phv"), $seed.Replace("`r`n", "`n"), (New-Object System.Text.UTF8Encoding $false))
Push-Location $site
try {
    & $phv run seed.phv
    if ($LASTEXITCODE -ne 0) { throw "seeding the guestbook failed" }
} finally {
    Pop-Location
}

# A free port.
$listener = New-Object System.Net.Sockets.TcpListener([System.Net.IPAddress]::Loopback, 0)
$listener.Start()
$port = $listener.LocalEndpoint.Port
$listener.Stop()

$log = Join-Path $site "serve.log"
Write-Host "phv serve $site --prod --host 127.0.0.1 --port $port"
$server = Start-Process -FilePath $phv -ArgumentList @("serve", "`"$site`"", "--prod", "--host", "127.0.0.1", "--port", "$port") -RedirectStandardError $log -PassThru -NoNewWindow
try {
    $ready = $false
    for ($t = 0; $t -lt 300; $t++) {
        if ($server.HasExited) { break }
        try {
            $probe = New-Object System.Net.Sockets.TcpClient
            $probe.Connect("127.0.0.1", $port)
            $probe.Close()
            $ready = $true
            break
        } catch {
            Start-Sleep -Milliseconds 100
        }
    }
    if (-not $ready) { throw "the server did not start:`n$(Get-Content -Raw $log -ErrorAction SilentlyContinue)" }

    $pages = @("/", "/archive/2026/11", "/guestbook")
    $results = [ordered]@{}
    foreach ($page in $pages) {
        $best = $null
        for ($r = 1; $r -le $runs; $r++) {
            $line = & $load --url "http://127.0.0.1:$port" --path $page --connections $connections --requests $requests --warmup $warmup --json
            if ($LASTEXITCODE -ne 0) { throw "load failed on ${page}: $line`n$(Get-Content -Raw $log -ErrorAction SilentlyContinue)" }
            $m = $line | ConvertFrom-Json
            Write-Host ("  {0,-20} run {1}: {2,8:N0} req/s  p50 {3,6:N2} ms  p99 {4,6:N2} ms" -f $page, $r, $m.requests_per_s, $m.p50_ms, $m.p99_ms)
            if ($null -eq $best) {
                $best = [ordered]@{ requests_per_s = $m.requests_per_s; p50_ms = $m.p50_ms; p99_ms = $m.p99_ms }
            } else {
                $best.requests_per_s = [math]::Max($best.requests_per_s, $m.requests_per_s)
                $best.p50_ms = [math]::Min($best.p50_ms, $m.p50_ms)
                $best.p99_ms = [math]::Min($best.p99_ms, $m.p99_ms)
            }
        }
        $results[$page] = $best
    }
} finally {
    if (-not $server.HasExited) { Stop-Process -Id $server.Id -Force }
    $server.WaitForExit()
    Remove-Item -Recurse -Force $site -ErrorAction SilentlyContinue
}

$section = [ordered]@{
    mode = "prod"
    connections = $connections
    requests_per_connection = $requests
    warmup_per_connection = $warmup
    runs = $runs
    pages = $results
}
Set-JsonSection $out $sha "serve" $section
Write-Host ""
Write-Host ("{0,-24} {1,10} {2,9} {3,9}" -f "Page", "req/s", "p50 ms", "p99 ms")
foreach ($page in $results.Keys) {
    $m = $results[$page]
    Write-Host ("{0,-24} {1,10:N0} {2,9:N2} {3,9:N2}" -f $page, $m.requests_per_s, $m.p50_ms, $m.p99_ms)
}
Write-Host ""
Write-Host "Wrote the serve section of $out"
