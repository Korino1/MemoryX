[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$systemRoot = Split-Path -Parent $PSScriptRoot
$repoRoot = Split-Path -Parent $systemRoot
. (Join-Path $PSScriptRoot 'interagent_language.ps1')

$controls = [Collections.Generic.List[object]]::new()
$failures = [Collections.Generic.List[string]]::new()

function Add-Control {
    param([string]$Name, [bool]$Passed, [string]$Evidence)
    $controls.Add([ordered]@{ name = $Name; passed = $Passed; evidence = $Evidence })
    if (-not $Passed) {
        $failures.Add("${Name}: $Evidence")
    }
}

$english = Test-MemoryXEnglishInterAgentText -Text 'English task and evidence only.' -Label 'english-control'
Add-Control 'English text accepted' $english.passed ($english.violations -join ' ')

$technical = Test-MemoryXEnglishInterAgentText -Text 'Read `Concept/Расширение.txt` as an immutable path.' -Label 'technical-literal-control'
Add-Control 'allowlisted technical literal accepted' $technical.passed ($technical.violations -join ' ')

$cyrillic = Test-MemoryXEnglishInterAgentText -Text 'This packet contains русский text.' -Label 'cyrillic-control'
Add-Control 'Cyrillic text rejected' (-not $cyrillic.passed) ($cyrillic.violations -join ' ')

$combining = Test-MemoryXEnglishInterAgentText -Text "English plus e$([char]0x0301)." -Label 'combining-mark-control'
Add-Control 'non-ASCII combining mark rejected' (-not $combining.passed) ($combining.violations -join ' ')

$targetRoot = [IO.Path]::GetFullPath((Join-Path $repoRoot 'target/interagent-language-controls'))
$temporaryContour = Join-Path $targetRoot ([Guid]::NewGuid().ToString('N'))
try {
    foreach ($directory in @('DOSSIERS', 'evidence', 'hooks', 'state')) {
        New-Item -ItemType Directory -Force -Path (Join-Path $temporaryContour $directory) | Out-Null
    }
    [IO.File]::WriteAllText(
        (Join-Path $temporaryContour 'CANONICAL_PACKET.md'),
        "# Temporary Canonical Packet`n`nNo language declaration here.`n",
        [Text.UTF8Encoding]::new($false)
    )
    $missingMarkerResult = Test-MemoryXContourEnglishContract -ContourRoot $temporaryContour -SystemRoot $systemRoot
    Add-Control 'contour validator rejects missing canonical marker' (-not $missingMarkerResult.passed) ($missingMarkerResult.violations -join ' ')

    [IO.File]::WriteAllText(
        (Join-Path $temporaryContour 'CANONICAL_PACKET.md'),
        "# Temporary Canonical Packet`n`n$script:MemoryXInterAgentLanguageMarker`n",
        [Text.UTF8Encoding]::new($false)
    )
    [IO.File]::WriteAllText(
        (Join-Path $temporaryContour 'TASK.md'),
        "This persisted packet contains русский text.`n",
        [Text.UTF8Encoding]::new($false)
    )
    $persistedResult = Test-MemoryXContourEnglishContract -ContourRoot $temporaryContour -SystemRoot $systemRoot
    Add-Control 'contour validator rejects persisted non-English narrative' (-not $persistedResult.passed) ($persistedResult.violations -join ' ')
} finally {
    $resolvedTemporary = [IO.Path]::GetFullPath($temporaryContour)
    $insideTarget = $resolvedTemporary.StartsWith($targetRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)
    if (-not $insideTarget) {
        throw "Refusing to remove a language-control fixture outside target: $resolvedTemporary"
    }
    if (Test-Path -LiteralPath $resolvedTemporary) {
        Remove-Item -LiteralPath $resolvedTemporary -Recurse -Force
    }
}

$wrapperRejected = $false
$wrapperEvidence = ''
try {
    & (Join-Path $systemRoot 'scripts/invoke_module.ps1') -Module MX-95 -Prompt 'Передать задачу другому агенту.' -DryRun | Out-Null
    $wrapperEvidence = 'Wrapper unexpectedly accepted a non-English prompt.'
} catch {
    $wrapperRejected = $_.Exception.Message -match 'English-only inter-agent contract violation'
    $wrapperEvidence = $_.Exception.Message
}
Add-Control 'wrapper rejects non-English prompt before Codex' $wrapperRejected $wrapperEvidence

[ordered]@{
    schema_version = 'memoryx.inter-agent-language-controls.v1'
    passed = ($failures.Count -eq 0)
    controls = @($controls)
    failures = @($failures)
    foreign_processes_touched = $false
} | ConvertTo-Json -Depth 8

if ($failures.Count -gt 0) {
    exit 1
}
