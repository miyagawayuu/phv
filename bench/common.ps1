# Shared by bench/run.ps1 and bench/serve.ps1 (dot-sourced).

function Get-RepoRoot {
    return (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
}

function Get-ShortSha([string] $root) {
    $sha = (& git -C $root rev-parse --short HEAD).Trim()
    if ($LASTEXITCODE -ne 0 -or -not $sha) { throw "git rev-parse failed in $root" }
    return $sha
}

# Whether tracked files under crates/ (the code being measured) differ from HEAD.
function Test-Dirty([string] $root) {
    $status = & git -C $root status --porcelain --untracked-files=no -- crates
    return [bool]($status | Where-Object { $_ })
}

function Get-TargetDir([string] $root) {
    if ($env:CARGO_TARGET_DIR) { return $env:CARGO_TARGET_DIR }
    return (Join-Path $root "target")
}

function Build-Phv([string] $root) {
    Write-Host "cargo build --release -p phv_cli"
    & cargo build --release -p phv_cli --manifest-path (Join-Path $root "Cargo.toml")
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
}

function Get-PhvPath([string] $root) {
    $name = if ($IsWindows -or $env:OS -eq "Windows_NT") { "phv.exe" } else { "phv" }
    $phv = Join-Path (Join-Path (Get-TargetDir $root) "release") $name
    if (-not (Test-Path $phv)) { throw "$phv does not exist; run without --no-build" }
    return $phv
}

function Get-Python {
    # The Microsoft Store placeholder is called python3 too, and only complains on
    # stderr; try each candidate (errors stay in this scope).
    $ErrorActionPreference = "SilentlyContinue"
    foreach ($name in @("python3", "python", "py")) {
        $cmd = Get-Command $name -ErrorAction SilentlyContinue
        if (-not $cmd) { continue }
        $v = & $cmd.Source -c "import sys; print(sys.version_info >= (3, 9))" 2>&1
        if ($LASTEXITCODE -eq 0 -and ($v | ForEach-Object { "$_" }) -contains "True") { return $cmd.Source }
    }
    throw "Python 3 was not found (bench/lsp.py needs it)"
}

function Get-MachineInfo {
    $cpu = $null
    $os = $null
    try {
        $cpu = (Get-CimInstance Win32_Processor | Select-Object -First 1).Name.Trim()
        $os = (Get-CimInstance Win32_OperatingSystem).Caption.Trim()
    } catch {
        $cpu = $env:PROCESSOR_IDENTIFIER
        $os = [System.Environment]::OSVersion.VersionString
    }
    return [ordered]@{ os = $os; cpu = $cpu; cores = [System.Environment]::ProcessorCount }
}

function Read-Json([string] $path) {
    return (Get-Content -Raw -Encoding UTF8 $path | ConvertFrom-Json)
}

# JSON in the shape of Python's json.dump(indent=2), so every script writes the same file.
function ConvertTo-BenchJson($value, [int] $depth = 0) {
    $pad = "  " * $depth
    $inner = "  " * ($depth + 1)
    if ($null -eq $value) { return "null" }
    if ($value -is [bool]) { return $(if ($value) { "true" } else { "false" }) }
    if ($value -is [string]) {
        $s = $value.Replace("\", "\\").Replace('"', '\"').Replace("`n", "\n").Replace("`r", "\r").Replace("`t", "\t")
        $sb = New-Object System.Text.StringBuilder
        foreach ($ch in $s.ToCharArray()) {
            if ([int]$ch -lt 32 -or [int]$ch -gt 126) { [void]$sb.AppendFormat("\u{0:x4}", [int]$ch) } else { [void]$sb.Append($ch) }
        }
        return '"' + $sb.ToString() + '"'
    }
    if ($value -is [int] -or $value -is [long] -or $value -is [int16] -or $value -is [byte]) { return $value.ToString() }
    if ($value -is [double] -or $value -is [single] -or $value -is [decimal]) {
        $d = [double]$value
        if ([math]::Floor($d) -eq $d -and [math]::Abs($d) -lt 1e15) { return ([long]$d).ToString() + ".0" }
        return $d.ToString("R", [System.Globalization.CultureInfo]::InvariantCulture)
    }
    if ($value -is [System.Collections.IDictionary]) {
        if ($value.Count -eq 0) { return "{}" }
        $parts = @()
        foreach ($key in $value.Keys) {
            $parts += $inner + (ConvertTo-BenchJson ([string]$key)) + ": " + (ConvertTo-BenchJson $value[$key] ($depth + 1))
        }
        return "{`n" + ($parts -join ",`n") + "`n$pad}"
    }
    if ($value -is [System.Collections.IEnumerable]) {
        $items = @($value)
        if ($items.Count -eq 0) { return "[]" }
        $parts = @()
        foreach ($item in $items) { $parts += $inner + (ConvertTo-BenchJson $item ($depth + 1)) }
        return "[`n" + ($parts -join ",`n") + "`n$pad]"
    }
    if ($value -is [System.Management.Automation.PSCustomObject]) {
        $props = @($value.PSObject.Properties)
        if ($props.Count -eq 0) { return "{}" }
        $parts = @()
        foreach ($p in $props) {
            $parts += $inner + (ConvertTo-BenchJson ([string]$p.Name)) + ": " + (ConvertTo-BenchJson $p.Value ($depth + 1))
        }
        return "{`n" + ($parts -join ",`n") + "`n$pad}"
    }
    return ConvertTo-BenchJson ([string]$value)
}

# Writes the file with LF line endings and no byte-order mark.
function Write-Json([string] $path, $value) {
    $text = (ConvertTo-BenchJson $value) + "`n"
    [System.IO.File]::WriteAllText($path, $text, (New-Object System.Text.UTF8Encoding $false))
}

# Adds or replaces one top-level section of a results file (creating the file if needed).
function Set-JsonSection([string] $path, [string] $commit, [string] $name, $section) {
    if (Test-Path $path) {
        $doc = Read-Json $path
    } else {
        $doc = [PSCustomObject]@{ commit = $commit }
    }
    if ($doc.PSObject.Properties[$name]) {
        $doc.$name = $section
    } else {
        $doc | Add-Member -NotePropertyName $name -NotePropertyValue $section
    }
    Write-Json $path $doc
}

function Format-Ratio([double] $new, [double] $old) {
    if ($old -eq 0) { return "n/a" }
    return ($new / $old).ToString("0.00", [System.Globalization.CultureInfo]::InvariantCulture)
}

# Prints the ratio new/old of every number both files have. Below 1.0 is faster for
# times, above 1.0 is faster for requests per second.
function Compare-Results($old, $new) {
    Write-Host ""
    Write-Host "Ratio new/old: $($new.commit) / $($old.commit) (times: below 1.00 is faster; requests/s: above 1.00 is faster)"
    Write-Host ""
    if ($old.rows -and $new.rows) {
        Write-Host ("{0,-52} {1,9} {2,9} {3,6}   {4,9} {5,9} {6,6}" -f "Benchmark", "old JIT", "new JIT", "ratio", "old int.", "new int.", "ratio")
        foreach ($row in $new.rows) {
            $o = $old.rows | Where-Object { $_.id -eq $row.id } | Select-Object -First 1
            if (-not $o) { continue }
            Write-Host ("{0,-52} {1,9:N1} {2,9:N1} {3,6}   {4,9:N1} {5,9:N1} {6,6}" -f $row.label, $o.jit_ms, $row.jit_ms, (Format-Ratio $row.jit_ms $o.jit_ms), $o.interp_ms, $row.interp_ms, (Format-Ratio $row.interp_ms $o.interp_ms))
        }
        Write-Host ""
    }
    if ($old.binary_bytes -and $new.binary_bytes) {
        Write-Host ("{0,-52} {1,9} {2,9} {3,6}" -f "Binary size (bytes)", $old.binary_bytes, $new.binary_bytes, (Format-Ratio $new.binary_bytes $old.binary_bytes))
        Write-Host ""
    }
    if ($old.serve -and $new.serve -and $old.serve.pages -and $new.serve.pages) {
        Write-Host ("{0,-24} {1,9} {2,9} {3,6}   {4,8} {5,8} {6,6}   {7,8} {8,8} {9,6}" -f "Page", "old req/s", "new req/s", "ratio", "old p50", "new p50", "ratio", "old p99", "new p99", "ratio")
        foreach ($p in $new.serve.pages.PSObject.Properties) {
            $o = $old.serve.pages.PSObject.Properties[$p.Name]
            if (-not $o) { continue }
            $a = $o.Value
            $b = $p.Value
            Write-Host ("{0,-24} {1,9:N0} {2,9:N0} {3,6}   {4,8:N2} {5,8:N2} {6,6}   {7,8:N2} {8,8:N2} {9,6}" -f $p.Name, $a.requests_per_s, $b.requests_per_s, (Format-Ratio $b.requests_per_s $a.requests_per_s), $a.p50_ms, $b.p50_ms, (Format-Ratio $b.p50_ms $a.p50_ms), $a.p99_ms, $b.p99_ms, (Format-Ratio $b.p99_ms $a.p99_ms))
        }
        Write-Host ""
    }
    if ($old.lsp -and $new.lsp) {
        Write-Host ("{0,-24} {1,9} {2,9} {3,6}" -f "LSP (ms)", "old", "new", "ratio")
        foreach ($pair in @(@("open", $old.lsp.open_ms, $new.lsp.open_ms), @("diagnostics median", $old.lsp.diagnostics_ms.median, $new.lsp.diagnostics_ms.median), @("diagnostics p99", $old.lsp.diagnostics_ms.p99, $new.lsp.diagnostics_ms.p99), @("completion median", $old.lsp.completion_ms.median, $new.lsp.completion_ms.median), @("completion p99", $old.lsp.completion_ms.p99, $new.lsp.completion_ms.p99))) {
            if ($null -eq $pair[1] -or $null -eq $pair[2]) { continue }
            Write-Host ("{0,-24} {1,9:N2} {2,9:N2} {3,6}" -f $pair[0], $pair[1], $pair[2], (Format-Ratio $pair[2] $pair[1]))
        }
        Write-Host ""
    }
}
