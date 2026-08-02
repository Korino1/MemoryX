[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$moduleRoot = Split-Path -Parent $PSScriptRoot
$recoveryPath = Join-Path $moduleRoot 'state/RECOVERY.json'

if (-not (Test-Path -LiteralPath $recoveryPath -PathType Leaf)) {
    throw 'No saved recovery record exists.'
}

$recovery = Get-Content -LiteralPath $recoveryPath -Raw | ConvertFrom-Json
if ($recovery.status -ne 'ready_for_compact') {
    throw "Recovery record is not compact-ready: $($recovery.status)"
}

$compactContextPath = Join-Path $moduleRoot 'COMPACT_CONTEXT.md'
$actualHash = (Get-FileHash -LiteralPath $compactContextPath -Algorithm SHA256).Hash
if ($actualHash -ne $recovery.compact_context_sha256) {
    throw 'COMPACT_CONTEXT.md changed after the pre-compact recovery record.'
}

foreach ($property in $recovery.document_hashes.PSObject.Properties) {
    $documentPath = Join-Path $moduleRoot $property.Name
    if (-not (Test-Path -LiteralPath $documentPath -PathType Leaf)) {
        throw "Saved recovery document is missing: $($property.Name)"
    }
    $documentHash = (Get-FileHash -LiteralPath $documentPath -Algorithm SHA256).Hash
    if ($documentHash -ne $property.Value) {
        throw "$($property.Name) changed after the pre-compact recovery record."
    }
}

[ordered]@{
    schema_version = 'memoryx.hook-post-compact.v1'
    recovery_recorded_at_utc = $recovery.recorded_at_utc
    session_state = $recovery.session_state
    session_id = $recovery.session_id
    acceptance_state = $recovery.acceptance_state
    recovery_sources = $recovery.recovery_sources
    compact_context = (Get-Content -LiteralPath $compactContextPath -Raw)
    statement = 'Restored only from the saved recovery record and compact context.'
    live_compact_proven = $false
    cache_reuse_proven = $false
    model_quality_proven = $false
} | ConvertTo-Json -Depth 10
