[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$systemRoot = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot 'interagent_language.ps1')

$failures = [Collections.Generic.List[string]]::new()
$manifest = Get-Content -LiteralPath (Join-Path $systemRoot 'manifest.json') -Raw | ConvertFrom-Json

$policy = $manifest.communication_policy
if ($null -eq $policy -or
    $policy.inter_agent_language -ne 'English' -or
    $policy.user_facing_language -ne 'user-selected' -or
    $policy.lexical_gate -ne 'ascii-english-with-explicit-technical-literals') {
    $failures.Add('Manifest communication_policy is missing or not canonical.')
}

$contours = [Collections.Generic.List[string]]::new()
$contours.Add($systemRoot)
foreach ($module in $manifest.modules) {
    $contours.Add((Join-Path $systemRoot "modules/$($module.id)-$($module.slug)"))
}

$filesChecked = 0
foreach ($contour in $contours) {
    $result = Test-MemoryXContourEnglishContract -ContourRoot $contour -SystemRoot $systemRoot
    $filesChecked += $result.files_checked
    foreach ($violation in $result.violations) {
        $failures.Add($violation)
    }

    $recoveryPath = Join-Path $contour 'state/RECOVERY.json'
    try {
        $recovery = Get-Content -LiteralPath $recoveryPath -Raw | ConvertFrom-Json
        if ($recovery.inter_agent_language -ne 'English') {
            $failures.Add("Recovery record does not declare English: $recoveryPath")
        }
    } catch {
        $failures.Add("Recovery record cannot be checked: ${recoveryPath}: $($_.Exception.Message)")
    }

    foreach ($evidencePath in @(Get-ChildItem -LiteralPath (Join-Path $contour 'evidence') -File -Filter 'EVIDENCE_RETURN*.json')) {
        try {
            $evidence = Get-Content -LiteralPath $evidencePath.FullName -Raw | ConvertFrom-Json
            if ($evidence.communication.inter_agent_language -ne 'English') {
                $failures.Add("EvidenceReturn does not declare English: $($evidencePath.FullName)")
            }
        } catch {
            $failures.Add("EvidenceReturn cannot be checked: $($evidencePath.FullName): $($_.Exception.Message)")
        }
    }
}

$invokePath = Join-Path $systemRoot 'scripts/invoke_module.ps1'
$invokeText = Get-Content -LiteralPath $invokePath -Raw
foreach ($required in @(
    'Inter-agent language: English only.',
    'Assert-MemoryXEnglishInterAgentText -Text $Prompt',
    'Assert-MemoryXEnglishInterAgentText -Text $stablePrefix'
)) {
    if (-not $invokeText.Contains($required)) {
        $failures.Add("Invocation wrapper lacks required language enforcement: $required")
    }
}

$buildPath = Join-Path $systemRoot 'scripts/build_scheme.ps1'
$buildText = Get-Content -LiteralPath $buildPath -Raw
foreach ($required in @(
    'Inter-agent language: English only.',
    "inter_agent_language = 'English'",
    "'INTER_AGENT_COMMUNICATION.md'"
)) {
    if (-not $buildText.Contains($required)) {
        $failures.Add("Scheme generator lacks required language inheritance: $required")
    }
}

[ordered]@{
    schema_version = 'memoryx.inter-agent-language-validation.v1'
    passed = ($failures.Count -eq 0)
    inter_agent_language = 'English'
    contours_checked = $contours.Count
    files_checked = $filesChecked
    technical_literal_allowlist = @($script:MemoryXAllowedNonAsciiTechnicalLiterals)
    failures = @($failures)
    semantic_language_detection_claimed = $false
} | ConvertTo-Json -Depth 8

if ($failures.Count -gt 0) {
    exit 1
}
