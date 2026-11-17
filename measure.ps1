# Performance Measurement Script for Kbrowser
$ErrorActionPreference = "Continue"

Write-Host "=========================================="
Write-Host "  Kbrowser Performance Measurement Script "
Write-Host "=========================================="

$toolchain = & rustc --version
$cargoVer = & cargo --version
$osInfo = (Get-CimInstance Win32_OperatingSystem).Caption
$cpuInfo = (Get-CimInstance Win32_Processor).Name

Write-Host "Machine: $osInfo / $cpuInfo"
Write-Host "Toolchain: $toolchain | $cargoVer"
Write-Host "Timestamp: $(Get-Date -Format 'o')"
Write-Host ""

# 1. Asset Sizes
Write-Host "--- Frontend Asset Sizes ---"
$frontendFiles = Get-ChildItem -Path "src" -File | Sort-Object Name
$totalFrontendBytes = 0
foreach ($f in $frontendFiles) {
    $totalFrontendBytes += $f.Length
    Write-Host ("{0,-20} : {1,8} bytes" -f $f.Name, $f.Length)
}
Write-Host ("{0,-20} : {1,8} bytes" -f "TOTAL FRONTEND", $totalFrontendBytes)
Write-Host ""

# 2. Benchmarks (Multiple Runs)
Write-Host "--- Running Benchmarks (5 iterations) ---"
$suggestTimes = @()
$historyTimes = @()
$fps = @{}

for ($i = 1; $i -le 5; $i++) {
    $output = & cargo test --manifest-path src-tauri/Cargo.toml -- bench_and_fingerprint --nocapture 2>&1 | Out-String
    
    if ($output -match "SUGGEST_TIME_MICROS:\s+(\d+)") {
        $suggestTimes += [int64]$matches[1]
    }
    if ($output -match "HISTORY_TIME_MICROS:\s+(\d+)") {
        $historyTimes += [int64]$matches[1]
    }
    if ($output -match "FP_SEARCH_URL:\s+([a-f0-9]+)") {
        $fps["SEARCH_URL"] = $matches[1]
    }
    if ($output -match "FP_SUGGEST_RUST:\s+([a-f0-9]+)") {
        $fps["SUGGEST_RUST"] = $matches[1]
    }
    if ($output -match "FP_HISTORY_CRATES:\s+([a-f0-9]+)") {
        $fps["HISTORY_CRATES"] = $matches[1]
    }
    if ($output -match "FP_SETTINGS:\s+([a-f0-9]+)") {
        $fps["SETTINGS"] = $matches[1]
    }
    Write-Host "Run $i : Suggest = $($suggestTimes[-1]) µs, History = $($historyTimes[-1]) µs"
}

function Get-Stats($arr) {
    $sorted = $arr | Sort-Object
    $count = $sorted.Count
    $min = $sorted[0]
    $max = $sorted[-1]
    $median = if ($count % 2 -eq 1) { $sorted[[math]::Floor($count / 2)] } else { ($sorted[$count/2 - 1] + $sorted[$count/2]) / 2 }
    return @{ Median = $median; Min = $min; Max = $max; Spread = ($max - $min) }
}

$sStats = Get-Stats $suggestTimes
$hStats = Get-Stats $historyTimes

Write-Host ""
Write-Host "--- Benchmark Summary ---"
Write-Host ("Suggest 100 queries (5k entries) : Median = {0:N0} µs (Min={1:N0}, Max={2:N0}, Spread={3:N0})" -f $sStats.Median, $sStats.Min, $sStats.Max, $sStats.Spread)
Write-Host ("History 50 filters (5k entries)  : Median = {0:N0} µs (Min={1:N0}, Max={2:N0}, Spread={3:N0})" -f $hStats.Median, $hStats.Min, $hStats.Max, $hStats.Spread)
Write-Host ""

Write-Host "--- API / Data Fingerprints (SHA-256) ---"
foreach ($k in $fps.Keys | Sort-Object) {
    Write-Host ("{0,-20} : {1}" -f $k, $fps[$k])
}
Write-Host "=========================================="
