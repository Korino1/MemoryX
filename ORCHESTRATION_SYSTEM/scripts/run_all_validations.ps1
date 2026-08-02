[CmdletBinding()]
param(
    [switch]$RequireBases,
    [switch]$IncludeCargoGates
)

$ErrorActionPreference = 'Stop'
$systemRoot = Split-Path -Parent $PSScriptRoot
$repoRoot = Split-Path -Parent $systemRoot
$failures = [Collections.Generic.List[string]]::new()
$observations = [Collections.Generic.List[object]]::new()
$moduleResults = [Collections.Generic.List[object]]::new()
$manifestPath = Join-Path $systemRoot 'manifest.json'
$sessionRegistryPath = Join-Path $systemRoot 'session_registry.json'

function Add-Observation {
    param([string]$Name, [bool]$Passed, [string]$Evidence)
    $observations.Add([ordered]@{ name = $Name; passed = $Passed; evidence = $Evidence })
    if (-not $Passed) {
        $failures.Add("${Name}: $Evidence")
    }
}

function Normalize-OwnedPath {
    param([string]$Path)
    return $Path.Replace('\', '/').TrimStart('./').ToLowerInvariant()
}

try {
    $manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
    Add-Observation 'manifest parses' $true $manifest.schema_version
} catch {
    Add-Observation 'manifest parses' $false $_.Exception.Message
    $manifest = $null
}

foreach ($schemaName in @('manifest.schema.json', 'module.schema.json', 'memoryx-contract.schema.json', 'evidence-return.schema.json', 'session-registry.schema.json')) {
    $schemaPath = Join-Path $systemRoot "schemas/$schemaName"
    try {
        $null = Get-Content -LiteralPath $schemaPath -Raw | ConvertFrom-Json
        Add-Observation "schema parses: $schemaName" $true $schemaPath
    } catch {
        Add-Observation "schema parses: $schemaName" $false $_.Exception.Message
    }
}

$rootControlFiles = @('ARCHITECTURE.md', 'README.md', 'INTER_AGENT_COMMUNICATION.md', 'CANONICAL_PACKET.md', 'TASK.md', 'PLAN.md', 'PROGRESS.md', 'DECISIONS.md', 'ACCEPTANCE.md', 'COMPACT_CONTEXT.md', 'session_registry.json')
$missingRootControlFiles = @($rootControlFiles | Where-Object { -not (Test-Path -LiteralPath (Join-Path $systemRoot $_) -PathType Leaf) })
Add-Observation 'root control-plane files' ($missingRootControlFiles.Count -eq 0) $(if ($missingRootControlFiles.Count -eq 0) { "present=$($rootControlFiles.Count)" } else { $missingRootControlFiles -join ',' })

if ($null -ne $manifest) {
    try {
        $manifestSchemaPassed = (Get-Content -LiteralPath $manifestPath -Raw) | Test-Json -SchemaFile (Join-Path $systemRoot 'schemas/manifest.schema.json') -ErrorAction Stop
        Add-Observation 'manifest schema validation' $manifestSchemaPassed 'draft-2020-12 schema evaluated'
    } catch {
        Add-Observation 'manifest schema validation' $false $_.Exception.Message
    }
    Add-Observation 'manifest schema version' ($manifest.schema_version -eq 'memoryx.orchestration-manifest.v1') $manifest.schema_version
    Add-Observation 'module domain count' ($manifest.modules.Count -eq 11) "observed=$($manifest.modules.Count), expected=11"
    Add-Observation 'immutable manifest model' ($manifest.execution_profile.model -eq 'gpt-5.6-sol') $manifest.execution_profile.model
    Add-Observation 'immutable manifest reasoning' ($manifest.execution_profile.reasoning_effort -eq 'xhigh') $manifest.execution_profile.reasoning_effort
    Add-Observation 'manifest max forbidden' (@($manifest.execution_profile.forbidden_reasoning_effort) -contains 'max') (@($manifest.execution_profile.forbidden_reasoning_effort) -join ',')
    Add-Observation 'manifest English-only inter-agent policy' ($manifest.communication_policy.inter_agent_language -eq 'English' -and $manifest.communication_policy.user_facing_language -eq 'user-selected' -and $manifest.communication_policy.lexical_gate -eq 'ascii-english-with-explicit-technical-literals') ($manifest.communication_policy | ConvertTo-Json -Compress)
    Add-Observation 'manifest project-local only' ($manifest.storage_policy.scope -eq 'project-local-only' -and $manifest.storage_policy.forbid_user_scope -eq $true -and $manifest.storage_policy.forbid_foreign_bases -eq $true) ($manifest.storage_policy | ConvertTo-Json -Compress)
    Add-Observation 'manifest one mutable owner' ([int]$manifest.storage_policy.mutable_owners_per_physical_base -eq 1) ([string]$manifest.storage_policy.mutable_owners_per_physical_base)

    foreach ($property in @(
        @{ name = 'module ids'; values = @($manifest.modules.id) },
        @{ name = 'module slugs'; values = @($manifest.modules.slug) },
        @{ name = 'module base names'; values = @($manifest.modules.base_name) }
    )) {
        $duplicates = @($property.values | Group-Object | Where-Object Count -gt 1 | Select-Object -ExpandProperty Name)
        Add-Observation "unique $($property.name)" ($duplicates.Count -eq 0) $(if ($duplicates.Count -eq 0) { 'all unique' } else { $duplicates -join ',' })
    }

    $owned = [Collections.Generic.List[object]]::new()
    foreach ($module in $manifest.modules) {
        foreach ($path in @($module.owned_paths)) {
            $owned.Add([pscustomobject]@{ module = $module.id; original = $path; normalized = (Normalize-OwnedPath $path) })
        }
    }
    $overlaps = [Collections.Generic.List[string]]::new()
    for ($i = 0; $i -lt $owned.Count; $i++) {
        for ($j = $i + 1; $j -lt $owned.Count; $j++) {
            if ($owned[$i].module -eq $owned[$j].module) {
                continue
            }
            $left = $owned[$i].normalized
            $right = $owned[$j].normalized
            $same = $left -eq $right
            $leftContains = $left.EndsWith('/') -and $right.StartsWith($left, [StringComparison]::OrdinalIgnoreCase)
            $rightContains = $right.EndsWith('/') -and $left.StartsWith($right, [StringComparison]::OrdinalIgnoreCase)
            if ($same -or $leftContains -or $rightContains) {
                $overlaps.Add("$($owned[$i].module):$($owned[$i].original) <-> $($owned[$j].module):$($owned[$j].original)")
            }
        }
    }
    Add-Observation 'non-overlapping primary ownership' ($overlaps.Count -eq 0) $(if ($overlaps.Count -eq 0) { 'no overlap' } else { $overlaps -join '; ' })
}

$sessionRegistry = $null
try {
    $sessionRegistry = Get-Content -LiteralPath $sessionRegistryPath -Raw | ConvertFrom-Json
    Add-Observation 'session registry parses' $true $sessionRegistry.schema_version
} catch {
    Add-Observation 'session registry parses' $false $_.Exception.Message
}

if ($null -ne $sessionRegistry) {
    try {
        $registrySchemaPassed = (Get-Content -LiteralPath $sessionRegistryPath -Raw) | Test-Json -SchemaFile (Join-Path $systemRoot 'schemas/session-registry.schema.json') -ErrorAction Stop
        Add-Observation 'session registry schema validation' $registrySchemaPassed 'draft-2020-12 schema evaluated'
    } catch {
        Add-Observation 'session registry schema validation' $false $_.Exception.Message
    }

    if ($null -ne $manifest) {
        $expectedSessions = @(
            [pscustomobject]@{ module_id = 'MX-ROOT'; slug = 'root-orchestrator'; binding_file = 'session_id.txt' }
        ) + @($manifest.modules | ForEach-Object {
            [pscustomobject]@{
                module_id = $_.id
                slug = $_.slug
                binding_file = "modules/$($_.id)-$($_.slug)/session_id.txt"
            }
        })
        Add-Observation 'session registry entry count' ($sessionRegistry.entries.Count -eq $expectedSessions.Count) "observed=$($sessionRegistry.entries.Count), expected=$($expectedSessions.Count)"
        $duplicateSessionIds = @($sessionRegistry.entries.module_id | Group-Object | Where-Object Count -gt 1 | Select-Object -ExpandProperty Name)
        Add-Observation 'session registry unique module ids' ($duplicateSessionIds.Count -eq 0) $(if ($duplicateSessionIds.Count -eq 0) { 'all unique' } else { $duplicateSessionIds -join ',' })
        $sessionIdDifference = @(Compare-Object -ReferenceObject @($expectedSessions.module_id | Sort-Object) -DifferenceObject @($sessionRegistry.entries.module_id | Sort-Object))
        Add-Observation 'session registry manifest coverage' ($sessionIdDifference.Count -eq 0) $(if ($sessionIdDifference.Count -eq 0) { 'root and all modules covered' } else { ($sessionIdDifference.InputObject -join ',') })

        foreach ($expectedSession in $expectedSessions) {
            $entry = @($sessionRegistry.entries | Where-Object module_id -eq $expectedSession.module_id)
            if ($entry.Count -ne 1) {
                Add-Observation "session binding: $($expectedSession.module_id)" $false "registry entries=$($entry.Count)"
                continue
            }
            $entry = $entry[0]
            $bindingMatches = $entry.binding_file -eq $expectedSession.binding_file -and $entry.slug -eq $expectedSession.slug
            $bindingPath = Join-Path $systemRoot $expectedSession.binding_file
            if (-not (Test-Path -LiteralPath $bindingPath -PathType Leaf)) {
                Add-Observation "session binding: $($expectedSession.module_id)" $false "missing binding file: $bindingPath"
                continue
            }
            $actualId = [IO.File]::ReadAllText($bindingPath).Trim()
            $actualState = if ($actualId.Length -eq 0) { 'UNBOUND' } else { 'BOUND' }
            $registryId = if ($null -eq $entry.session_id) { '' } else { [string]$entry.session_id }
            $stateMatches = $entry.state -eq $actualState -and $registryId -eq $actualId
            $profileMatches = $entry.model -eq 'gpt-5.6-sol' -and $entry.reasoning_effort -eq 'xhigh'
            Add-Observation "session binding: $($expectedSession.module_id)" ($bindingMatches -and $stateMatches -and $profileMatches) "state=$actualState, id=$(if ($actualId) { $actualId } else { 'null' }), binding=$($entry.binding_file)"
        }
    }
}

try {
    $buildCheck = & (Join-Path $systemRoot 'scripts/build_scheme.ps1') -Check | ConvertFrom-Json
    Add-Observation 'scheme completeness check' $true "verified=$($buildCheck.verified_count), created=$($buildCheck.created_count)"
} catch {
    Add-Observation 'scheme completeness check' $false $_.Exception.Message
}

$scriptFiles = @(Get-ChildItem -LiteralPath $systemRoot -Filter '*.ps1' -File -Recurse)
$scriptErrors = [Collections.Generic.List[string]]::new()
foreach ($scriptFile in $scriptFiles) {
    $parseErrors = $null
    [Management.Automation.Language.Parser]::ParseFile($scriptFile.FullName, [ref]$null, [ref]$parseErrors) | Out-Null
    foreach ($parseError in @($parseErrors)) {
        $scriptErrors.Add("$($scriptFile.FullName): $($parseError.Message)")
    }
}
Add-Observation 'PowerShell syntax' ($scriptErrors.Count -eq 0) $(if ($scriptErrors.Count -eq 0) { "parsed=$($scriptFiles.Count)" } else { $scriptErrors -join '; ' })

try {
    $languageValidation = & (Join-Path $systemRoot 'scripts/validate_interagent_english.ps1') | ConvertFrom-Json
    Add-Observation 'English-only inter-agent validation' ($languageValidation.passed -eq $true) "contours=$($languageValidation.contours_checked), files=$($languageValidation.files_checked)"
} catch {
    Add-Observation 'English-only inter-agent validation' $false $_.Exception.Message
}

try {
    $languageControls = & (Join-Path $systemRoot 'scripts/test_interagent_english_fail_closed.ps1') | ConvertFrom-Json
    Add-Observation 'English-only fail-closed controls' ($languageControls.passed -eq $true) "controls=$($languageControls.controls.Count)"
} catch {
    Add-Observation 'English-only fail-closed controls' $false $_.Exception.Message
}

$selectors = @('root')
if ($null -ne $manifest) {
    $selectors += @($manifest.modules.id)
}
foreach ($selector in $selectors) {
    try {
        $arguments = @{ Module = $selector }
        if ($RequireBases) {
            $arguments.RequireBase = $true
        }
        $moduleResult = & (Join-Path $systemRoot 'scripts/validate_module.ps1') @arguments | ConvertFrom-Json
        $moduleResults.Add($moduleResult)
        if (-not $moduleResult.passed) {
            $failures.Add("Module $selector failed: $($moduleResult.errors -join '; ')")
        }
    } catch {
        $failures.Add("Module $selector validation threw: $($_.Exception.Message)")
    }
}
Add-Observation 'all contour validations' ($moduleResults.Count -eq $selectors.Count -and @($moduleResults | Where-Object passed -ne $true).Count -eq 0) "validated=$($moduleResults.Count), expected=$($selectors.Count)"

$sessionSummary = @($moduleResults | ForEach-Object { [ordered]@{ module_id = $_.module_id; state = $_.session_state; id = $_.session_id } })
$cargoResults = [Collections.Generic.List[object]]::new()

if ($IncludeCargoGates) {
    $lease = $null
    try {
        $lease = & (Join-Path $systemRoot 'scripts/resource_coordination.ps1') -Action Acquire -OwnerPid $PID -Purpose 'orchestration full cargo gates' | ConvertFrom-Json
        foreach ($gate in @(
            @{ name = 'cargo fmt'; args = @('+nightly', 'fmt', '--all', '--', '--check') },
            @{ name = 'cargo check'; args = @('+nightly', 'check', '--all-targets', '--all-features', '--locked') },
            @{ name = 'cargo clippy'; args = @('+nightly', 'clippy', '--all-targets', '--all-features', '--locked', '--', '-D', 'warnings') },
            @{ name = 'cargo test'; args = @('+nightly', 'test', '--all-targets', '--all-features', '--locked') }
        )) {
            $output = & cargo @($gate.args) 2>&1
            $exitCode = $LASTEXITCODE
            $cargoResults.Add([ordered]@{ name = $gate.name; exit_code = $exitCode; observed_tail = (($output | Select-Object -Last 8) -join [Environment]::NewLine) })
            if ($exitCode -ne 0) {
                $failures.Add("$($gate.name) failed with exit code $exitCode")
                break
            }
        }
    } catch {
        $failures.Add("Cargo gate coordination failed: $($_.Exception.Message)")
    } finally {
        if ($null -ne $lease) {
            try {
                & (Join-Path $systemRoot 'scripts/resource_coordination.ps1') -Action Release -OwnerPid $PID -Token $lease.token | Out-Null
            } catch {
                $failures.Add("Resource coordination release failed: $($_.Exception.Message)")
            }
        }
    }
}

$result = [ordered]@{
    schema_version = 'memoryx.orchestration-validation.v1'
    passed = ($failures.Count -eq 0)
    structural_only = (-not $IncludeCargoGates)
    bases_required = [bool]$RequireBases
    manifest_modules = if ($null -eq $manifest) { 0 } else { $manifest.modules.Count }
    contour_validations = $moduleResults.Count
    observations = @($observations)
    session_states = $sessionSummary
    cargo_gates = @($cargoResults)
    failures = @($failures)
    live_gates_not_proven = @('real_hook_lifecycle', 'real_compact_resume', 'cache_reuse', 'model_quality', 'MemoryX_semantic_acceptance', 'N5_completion')
    shared_host_statement = 'No foreign process was stopped or modified.'
}

$result | ConvertTo-Json -Depth 12
if ($failures.Count -gt 0) {
    exit 1
}
