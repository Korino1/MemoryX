[CmdletBinding()]
param(
    [string]$Module = 'all',
    [string]$MemoryXBinary
)

$ErrorActionPreference = 'Stop'
$systemRoot = Split-Path -Parent $PSScriptRoot
$repoRoot = Split-Path -Parent $systemRoot
$manifest = Get-Content -LiteralPath (Join-Path $systemRoot 'manifest.json') -Raw | ConvertFrom-Json
$baseRoot = [IO.Path]::GetFullPath((Join-Path $repoRoot '.memoryx/bases'))

if (-not $MemoryXBinary) {
    foreach ($candidate in @(
        (Join-Path $repoRoot 'target/release/memoryx.exe'),
        (Join-Path $repoRoot 'target/debug/memoryx.exe')
    )) {
        if (Test-Path -LiteralPath $candidate -PathType Leaf) {
            $MemoryXBinary = $candidate
            break
        }
    }
}
if (-not $MemoryXBinary) {
    $command = Get-Command memoryx -ErrorAction SilentlyContinue
    if ($null -ne $command) {
        $MemoryXBinary = $command.Source
    }
}
if (-not $MemoryXBinary -or -not (Test-Path -LiteralPath $MemoryXBinary -PathType Leaf)) {
    throw 'MemoryX executable was not found. Build it or pass -MemoryXBinary.'
}
$MemoryXBinary = (Resolve-Path -LiteralPath $MemoryXBinary).Path

$definitions = [Collections.Generic.List[object]]::new()
if ($Module -eq 'all' -or $Module -eq 'root' -or $Module -eq 'MX-ROOT') {
    $definitions.Add([pscustomobject]@{ id = 'MX-ROOT'; base_name = $manifest.root_contour.base_name })
}
if ($Module -eq 'all') {
    foreach ($entry in $manifest.modules) {
        $definitions.Add($entry)
    }
} elseif ($Module -ne 'root' -and $Module -ne 'MX-ROOT') {
    $match = @($manifest.modules | Where-Object { $_.id -eq $Module -or $_.slug -eq $Module })
    if ($match.Count -ne 1) {
        throw "Module selector must resolve exactly once: $Module"
    }
    $definitions.Add($match[0])
}

$results = [Collections.Generic.List[object]]::new()
foreach ($definition in $definitions) {
    $basePath = [IO.Path]::GetFullPath((Join-Path $baseRoot $definition.base_name))
    if (-not $basePath.StartsWith($baseRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Refusing base outside repository-local root: $basePath"
    }

    $existingEntries = if (Test-Path -LiteralPath $basePath -PathType Container) { @(Get-ChildItem -LiteralPath $basePath -Force) } else { @() }
    if ($existingEntries.Count -gt 0) {
        $results.Add([ordered]@{
            module_id = $definition.id
            base_path = $basePath
            status = 'existing_not_reinitialized'
            exit_code = $null
        })
        continue
    }

    $output = & $MemoryXBinary --format json init --base $basePath 2>&1
    $exitCode = $LASTEXITCODE
    if ($exitCode -ne 0) {
        throw "MemoryX init failed for $($definition.id) with exit code ${exitCode}: $($output -join [Environment]::NewLine)"
    }
    $results.Add([ordered]@{
        module_id = $definition.id
        base_path = $basePath
        status = 'initialized'
        exit_code = $exitCode
        observed_output = ($output -join [Environment]::NewLine)
    })
}

[ordered]@{
    schema_version = 'memoryx.base-initialization.v1'
    executable = $MemoryXBinary
    executable_version = (& $MemoryXBinary --version 2>&1 | Out-String).Trim()
    base_root = $baseRoot
    project_local_only = $true
    initialized_or_existing = $results.Count
    results = @($results)
    statement = 'No user-scoped or foreign base was opened.'
} | ConvertTo-Json -Depth 10
