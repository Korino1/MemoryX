param(
    [string]$MemoryXPath = "target/release/memoryx.exe",
    [string]$BaseName = "rag-comparison"
)

$ErrorActionPreference = "Stop"

if (-not (Test-Path $MemoryXPath)) {
    throw "MemoryX binary not found at $MemoryXPath. Build first with: cargo +nightly build --release --features mcp"
}

$root = Split-Path -Parent $PSScriptRoot
$dataset = Join-Path $PSScriptRoot "rag_comparison_cases.json"
$outDir = Join-Path $PSScriptRoot "results"
New-Item -ItemType Directory -Force -Path $outDir | Out-Null

$timestamp = Get-Date -Format "yyyyMMdd-HHmmss"
$outFile = Join-Path $outDir "memoryx-$timestamp.jsonl"

& $MemoryXPath --base-scope project --base-name $BaseName init --force | Out-Null

$cases = (Get-Content $dataset -Raw | ConvertFrom-Json).cases
foreach ($case in $cases) {
    $contractJson = & $MemoryXPath --base-scope project --base-name $BaseName --format json query --emit-contract $case.query
    $record = [ordered]@{
        id = $case.id
        query = $case.query
        expected_capability = $case.expected_capability
        metrics = $case.metrics
        compiled_contract = ($contractJson | ConvertFrom-Json)
        note = "Populate the base with case facts before collecting comparable results. This scaffold records protocol shape, not final scores."
    }
    ($record | ConvertTo-Json -Depth 20 -Compress) | Add-Content -Path $outFile
}

Write-Host "Wrote scaffold records to $outFile"
