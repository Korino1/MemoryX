[CmdletBinding()]
param(
    [string]$AcceptanceState = 'not_evaluated'
)

$ErrorActionPreference = 'Stop'
$moduleRoot = Split-Path -Parent $PSScriptRoot
$trackedDocuments = @('TASK.md', 'PLAN.md', 'PROGRESS.md', 'DECISIONS.md')
$recoveryPath = Join-Path $moduleRoot 'state/RECOVERY.json'
$previous = $null

if (Test-Path -LiteralPath $recoveryPath -PathType Leaf) {
    $previous = Get-Content -LiteralPath $recoveryPath -Raw | ConvertFrom-Json
}

$hashes = [ordered]@{}
foreach ($name in $trackedDocuments) {
    $path = Join-Path $moduleRoot $name
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "Required pre-compact document is missing: $name"
    }
    if ((Get-Item -LiteralPath $path).Length -eq 0) {
        throw "Required pre-compact document is empty: $name"
    }
    $hashes[$name] = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash
}

if ($null -ne $previous -and $null -ne $previous.document_hashes) {
    foreach ($name in $trackedDocuments) {
        $oldHash = $previous.document_hashes.PSObject.Properties[$name].Value
        if ($null -ne $oldHash -and $oldHash -eq $hashes[$name]) {
            throw "$name was not updated since the previous pre-compact record."
        }
    }
}

$compactContext = Join-Path $moduleRoot 'COMPACT_CONTEXT.md'
if (-not (Test-Path -LiteralPath $compactContext -PathType Leaf) -or (Get-Item -LiteralPath $compactContext).Length -eq 0) {
    throw 'COMPACT_CONTEXT.md is missing or empty.'
}

$sessionText = [IO.File]::ReadAllText((Join-Path $moduleRoot 'session_id.txt')).Trim()
$record = [ordered]@{
    schema_version = 'memoryx.compact-recovery.v1'
    inter_agent_language = 'English'
    status = 'ready_for_compact'
    recorded_at_utc = [DateTime]::UtcNow.ToString('o')
    session_state = if ($sessionText.Length -eq 0) { 'UNBOUND' } else { 'BOUND' }
    session_id = if ($sessionText.Length -eq 0) { $null } else { $sessionText }
    acceptance_state = $AcceptanceState
    document_hashes = $hashes
    compact_context_sha256 = (Get-FileHash -LiteralPath $compactContext -Algorithm SHA256).Hash
    recovery_sources = @(
        'CANONICAL_PACKET.md',
        'TASK.md',
        'PLAN.md',
        'PROGRESS.md',
        'DECISIONS.md',
        'ACCEPTANCE.md',
        'COMPACT_CONTEXT.md',
        'MEMORYX_CONTRACT.json'
    )
    statement = 'This record contains only hashes and references to English inter-agent state saved before compact.'
}

$json = $record | ConvertTo-Json -Depth 10
[IO.File]::WriteAllText($recoveryPath, $json + [Environment]::NewLine, [Text.UTF8Encoding]::new($false))
$json
