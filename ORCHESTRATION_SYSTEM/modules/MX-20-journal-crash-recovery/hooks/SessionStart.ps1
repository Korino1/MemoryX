[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$moduleRoot = Split-Path -Parent $PSScriptRoot
$cursor = Get-Item -LiteralPath $moduleRoot
$systemRoot = $null

while ($null -ne $cursor) {
    $candidate = Join-Path $cursor.FullName 'manifest.json'
    if (Test-Path -LiteralPath $candidate -PathType Leaf) {
        $systemRoot = $cursor.FullName
        break
    }
    $cursor = $cursor.Parent
}

if ($null -eq $systemRoot) {
    throw 'Unable to locate ORCHESTRATION_SYSTEM/manifest.json.'
}

& (Join-Path $systemRoot 'scripts/validate_module.ps1') -ModulePath $moduleRoot | Out-Null

$module = Get-Content -LiteralPath (Join-Path $moduleRoot 'module.json') -Raw | ConvertFrom-Json
$sessionText = [IO.File]::ReadAllText((Join-Path $moduleRoot 'session_id.txt')).Trim()
$recoveryPath = Join-Path $moduleRoot 'state/RECOVERY.json'
$recovery = Get-Content -LiteralPath $recoveryPath -Raw | ConvertFrom-Json

[ordered]@{
    schema_version = 'memoryx.hook-session-start.v1'
    inter_agent_language = 'English'
    module_id = $module.id
    session_state = if ($sessionText.Length -eq 0) { 'UNBOUND' } else { 'BOUND' }
    session_id = if ($sessionText.Length -eq 0) { $null } else { $sessionText }
    model = $module.execution.model
    reasoning_effort = $module.execution.reasoning_effort
    recovery_status = $recovery.status
    recovery_record = 'state/RECOVERY.json'
    compact_context = 'COMPACT_CONTEXT.md'
    statement = 'Only durably saved English inter-agent instructions and MemoryX state have been loaded.'
} | ConvertTo-Json -Depth 8
