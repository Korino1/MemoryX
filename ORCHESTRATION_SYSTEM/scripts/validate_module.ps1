[CmdletBinding()]
param(
    [string]$Module,
    [string]$ModulePath,
    [switch]$RequireBase
)

$ErrorActionPreference = 'Stop'
$systemRoot = Split-Path -Parent $PSScriptRoot
$repoRoot = Split-Path -Parent $systemRoot
$manifest = Get-Content -LiteralPath (Join-Path $systemRoot 'manifest.json') -Raw | ConvertFrom-Json
$errors = [Collections.Generic.List[string]]::new()
$checks = [Collections.Generic.List[string]]::new()

function Add-Check {
    param([string]$Condition, [bool]$Passed, [string]$Failure)
    if ($Passed) {
        $checks.Add($Condition)
    } else {
        $errors.Add($Failure)
    }
}

function Test-ExactProperties {
    param($Value, [string[]]$Expected, [string]$Label)
    $actual = @($Value.PSObject.Properties.Name | Sort-Object)
    $wanted = @($Expected | Sort-Object)
    $difference = @(Compare-Object -ReferenceObject $wanted -DifferenceObject $actual)
    Add-Check "$Label exact properties" ($difference.Count -eq 0) "$Label has missing or unexpected properties: $($difference.InputObject -join ', ')"
}

function Read-JsonObject {
    param([string]$Path, [string]$Label)
    try {
        $value = Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json
        Add-Check "$Label parses as JSON" $true ''
        return $value
    } catch {
        $errors.Add("$Label is invalid JSON: $($_.Exception.Message)")
        return $null
    }
}

if ($ModulePath) {
    $contourRoot = [IO.Path]::GetFullPath($ModulePath)
} elseif (-not $Module -or $Module -eq 'MX-ROOT' -or $Module -eq 'root') {
    $contourRoot = $systemRoot
} else {
    $definition = @($manifest.modules | Where-Object { $_.id -eq $Module -or $_.slug -eq $Module })
    if ($definition.Count -ne 1) {
        throw "Module selector must resolve exactly once: $Module"
    }
    $contourRoot = Join-Path $systemRoot "modules/$($definition[0].id)-$($definition[0].slug)"
}

if (-not (Test-Path -LiteralPath $contourRoot -PathType Container)) {
    throw "Module contour does not exist: $contourRoot"
}

$requiredFiles = @(
    'module.json',
    'session_id.txt',
    'CANONICAL_PACKET.md',
    'TASK.md',
    'PLAN.md',
    'PROGRESS.md',
    'DECISIONS.md',
    'ACCEPTANCE.md',
    'COMPACT_CONTEXT.md',
    'MEMORYX_CONTRACT.json',
    'evidence/EVIDENCE_RETURN.example.json',
    'DOSSIERS/INDEX.md',
    'state/RECOVERY.json',
    'hooks/SessionStart.ps1',
    'hooks/PreCompact.ps1',
    'hooks/PostCompact.ps1'
)
$requiredDirectories = @('DOSSIERS', 'state', 'evidence', 'logs', 'hooks')

foreach ($relative in $requiredFiles) {
    $path = Join-Path $contourRoot $relative
    Add-Check "required file $relative" (Test-Path -LiteralPath $path -PathType Leaf) "Missing required file: $path"
}
foreach ($relative in $requiredDirectories) {
    $path = Join-Path $contourRoot $relative
    Add-Check "required directory $relative" (Test-Path -LiteralPath $path -PathType Container) "Missing required directory: $path"
}

foreach ($relative in $requiredFiles | Where-Object { $_ -ne 'session_id.txt' }) {
    $path = Join-Path $contourRoot $relative
    if (Test-Path -LiteralPath $path -PathType Leaf) {
        Add-Check "nonempty file $relative" ((Get-Item -LiteralPath $path).Length -gt 0) "Required file is empty: $path"
    }
}

$moduleJsonPath = Join-Path $contourRoot 'module.json'
$contractPath = Join-Path $contourRoot 'MEMORYX_CONTRACT.json'
$recoveryPath = Join-Path $contourRoot 'state/RECOVERY.json'
$evidenceReturnPath = Join-Path $contourRoot 'evidence/EVIDENCE_RETURN.example.json'
$moduleJson = Read-JsonObject -Path $moduleJsonPath -Label 'module.json'
$contract = Read-JsonObject -Path $contractPath -Label 'MEMORYX_CONTRACT.json'
$recovery = Read-JsonObject -Path $recoveryPath -Label 'state/RECOVERY.json'
$evidenceReturn = Read-JsonObject -Path $evidenceReturnPath -Label 'EvidenceReturn example'

foreach ($schemaCheck in @(
    @{ label = 'module.json schema'; document = $moduleJsonPath; schema = (Join-Path $systemRoot 'schemas/module.schema.json') },
    @{ label = 'MEMORYX_CONTRACT schema'; document = $contractPath; schema = (Join-Path $systemRoot 'schemas/memoryx-contract.schema.json') },
    @{ label = 'EvidenceReturn schema'; document = $evidenceReturnPath; schema = (Join-Path $systemRoot 'schemas/evidence-return.schema.json') }
)) {
    try {
        $schemaPassed = (Get-Content -LiteralPath $schemaCheck.document -Raw) | Test-Json -SchemaFile $schemaCheck.schema -ErrorAction Stop
        Add-Check $schemaCheck.label $schemaPassed "$($schemaCheck.label) returned false."
    } catch {
        $errors.Add("$($schemaCheck.label) failed: $($_.Exception.Message)")
    }
}

if ($null -ne $moduleJson) {
    Test-ExactProperties $moduleJson @('schema_version', 'id', 'slug', 'display_name', 'responsibility', 'path', 'base', 'execution', 'session', 'ownership', 'canonical_refs', 'dossiers', 'hooks', 'evidence_return_schema') 'module.json'
    Add-Check 'module schema version' ($moduleJson.schema_version -eq 'memoryx.orchestration-module.v1') 'Unexpected module schema version.'
    Add-Check 'immutable model' ($moduleJson.execution.model -eq 'gpt-5.6-sol') 'Module model must be gpt-5.6-sol.'
    Add-Check 'immutable reasoning' ($moduleJson.execution.reasoning_effort -eq 'xhigh') 'Module reasoning effort must be xhigh.'
    Add-Check 'max forbidden' (@($moduleJson.execution.forbidden_reasoning_effort) -contains 'max') 'Module must forbid max reasoning effort.'
    Add-Check 'reassert profile' ($moduleJson.execution.reassert_on_start_resume -eq $true) 'Module must reassert model/reasoning on start and resume.'
    Add-Check 'session binding contract' ($moduleJson.session.binding_file -eq 'session_id.txt' -and $moduleJson.session.never_synthesize_uuid -eq $true -and $moduleJson.session.resume_only_when_bound -eq $true) 'Session binding contract is not strict.'
    Add-Check 'project-local base scope' ($moduleJson.base.scope -eq 'project-local') 'Module base scope must be project-local.'
    Add-Check 'single mutable owner' ([int]$moduleJson.base.max_mutable_owners -eq 1) 'Module base must allow exactly one mutable owner.'
    Add-Check 'base path shape' ($moduleJson.base.path -eq ".memoryx/bases/$($moduleJson.base.name)") 'Module base path/name mismatch.'

    if ($moduleJson.id -eq 'MX-ROOT') {
        Add-Check 'root registry identity' ($moduleJson.base.name -eq $manifest.root_contour.base_name -and $moduleJson.path -eq 'ORCHESTRATION_SYSTEM') 'Root contour does not match manifest root registry.'
    } else {
        $definition = @($manifest.modules | Where-Object { $_.id -eq $moduleJson.id })
        Add-Check 'module exists once in registry' ($definition.Count -eq 1) "Module $($moduleJson.id) is absent or duplicated in manifest."
        if ($definition.Count -eq 1) {
            $expectedPath = "ORCHESTRATION_SYSTEM/modules/$($definition[0].id)-$($definition[0].slug)"
            Add-Check 'module slug matches registry' ($moduleJson.slug -eq $definition[0].slug) 'Module slug differs from registry.'
            Add-Check 'module path matches registry' ($moduleJson.path -eq $expectedPath) 'Module path differs from registry.'
            Add-Check 'module base matches registry' ($moduleJson.base.name -eq $definition[0].base_name) 'Module base differs from registry.'
            Add-Check 'module responsibility matches registry' ($moduleJson.responsibility -eq $definition[0].responsibility) 'Module responsibility differs from registry.'
            Add-Check 'owned paths match registry' ((@($moduleJson.ownership.owned_paths) -join "`n") -eq (@($definition[0].owned_paths) -join "`n")) 'Owned paths differ from registry.'
            Add-Check 'shared surfaces match registry' ((@($moduleJson.ownership.shared_surfaces) -join "`n") -eq (@($definition[0].shared_surfaces) -join "`n")) 'Shared surfaces differ from registry.'
            Add-Check 'forbidden paths match registry' ((@($moduleJson.ownership.forbidden_paths) -join "`n") -eq (@($definition[0].forbidden_paths) -join "`n")) 'Forbidden paths differ from registry.'
        }
    }

    foreach ($dossier in @($moduleJson.dossiers)) {
        Add-Check "dossier $dossier" (Test-Path -LiteralPath (Join-Path $contourRoot $dossier) -PathType Leaf) "Declared dossier is missing: $dossier"
    }

    foreach ($canonicalRef in @($moduleJson.canonical_refs)) {
        $pathPart = ($canonicalRef -split '#', 2)[0]
        Add-Check "canonical reference $pathPart" (Test-Path -LiteralPath (Join-Path $repoRoot $pathPart)) "Canonical reference does not exist: $pathPart"
    }

    $baseRoot = [IO.Path]::GetFullPath((Join-Path $repoRoot '.memoryx/bases'))
    $baseFull = [IO.Path]::GetFullPath((Join-Path $repoRoot $moduleJson.base.path))
    $insideBaseRoot = $baseFull.StartsWith($baseRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)
    Add-Check 'base remains under repo-local root' $insideBaseRoot "Base escapes repository-local root: $baseFull"
    if ($RequireBase) {
        Add-Check 'physical base initialized' (Test-Path -LiteralPath $baseFull -PathType Container) "Physical module base is not initialized: $baseFull"
    }
}

$sessionPath = Join-Path $contourRoot 'session_id.txt'
$sessionState = 'INVALID'
$sessionId = $null
if (Test-Path -LiteralPath $sessionPath -PathType Leaf) {
    $sessionBytes = [IO.File]::ReadAllBytes($sessionPath)
    if ($sessionBytes.Length -eq 0) {
        $sessionState = 'UNBOUND'
        Add-Check 'empty session means unbound' $true ''
    } else {
        $sessionText = [Text.Encoding]::UTF8.GetString($sessionBytes)
        $parsed = [Guid]::Empty
        if ($sessionText -eq $sessionText.Trim() -and [Guid]::TryParseExact($sessionText, 'D', [ref]$parsed)) {
            $sessionState = 'BOUND'
            $sessionId = $parsed.ToString('D')
            Add-Check 'bound session is observed UUID' $true ''
        } else {
            $errors.Add('session_id.txt must be zero bytes or one canonical UUID with no whitespace.')
        }
    }
}

if ($null -ne $contract -and $null -ne $moduleJson) {
    Test-ExactProperties $contract @('schema_version', 'module_id', 'physical_base', 'ownership', 'knowledge', 'evidence', 'query', 'conflicts', 'recovery', 'forbidden') 'MEMORYX_CONTRACT.json'
    Add-Check 'contract schema version' ($contract.schema_version -eq 'memoryx.module-contract.v1') 'Unexpected MemoryX contract schema version.'
    Add-Check 'contract module binding' ($contract.module_id -eq $moduleJson.id) 'MemoryX contract module id mismatch.'
    Add-Check 'contract physical base binding' ($contract.physical_base.scope -eq 'project-local' -and $contract.physical_base.path -eq $moduleJson.base.path -and $contract.physical_base.name -eq $moduleJson.base.name) 'MemoryX contract physical base mismatch.'
    Add-Check 'contract one owner' ([int]$contract.ownership.max_mutable_owners -eq 1 -and $contract.ownership.live_owner_required_for_shared_access -eq $true -and $contract.ownership.implicit_second_writer_forbidden -eq $true) 'MemoryX contract owner policy mismatch.'
    Add-Check 'contract atom unit' ($contract.knowledge.unit -eq 'knowledge_atom') 'MemoryX contract must use knowledge atoms.'
    Add-Check 'file rules remain authority' ($contract.knowledge.rules_authority -eq 'versioned_files' -and $contract.recovery.file_contracts_are_primary -eq $true) 'MemoryX cannot be the sole rules authority.'
    Add-Check 'candidate is not proof' ($contract.knowledge.candidate_is_not_proof -eq $true) 'MemoryX contract must distinguish candidates from proof.'
    Add-Check 'provenance required' ($contract.evidence.source_registration_required -eq $true -and $contract.evidence.provenance_required_for_acceptance -eq $true) 'MemoryX contract evidence policy is incomplete.'
    Add-Check 'fixed point and graph required' ($contract.query.fixed_point_required_for_strict_answer -eq $true -and $contract.query.answer_graph_required -eq $true) 'MemoryX query core cannot be bypassed.'
    Add-Check 'fail-closed conflicts' ($contract.conflicts.policy -eq 'fail-closed' -and $contract.conflicts.branching_preserved -eq $true) 'MemoryX conflict policy mismatch.'
    Add-Check 'N5 remains open' ($contract.recovery.n5_status -eq 'open') 'Module contract must not mark N5 complete.'
    $forbidden = @($contract.forbidden)
    Add-Check 'forbidden base/evidence modes' ($forbidden -contains 'user-scoped-base' -and $forbidden -contains 'foreign-project-base' -and $forbidden -contains 'unsourced-acceptance-evidence') 'MemoryX contract forbidden modes are incomplete.'
}

if ($null -ne $recovery) {
    Add-Check 'recovery schema version' ($recovery.schema_version -eq 'memoryx.compact-recovery.v1') 'Unexpected recovery schema version.'
    Add-Check 'recovery does not fabricate session' (-not ($recovery.session_state -eq 'BOUND' -and $null -eq $recovery.session_id)) 'Recovery record has a fabricated bound session state.'
}

if ($null -ne $evidenceReturn -and $null -ne $moduleJson) {
    Test-ExactProperties $evidenceReturn @('schema_version', 'module', 'session', 'execution', 'task', 'gates', 'changed_artifacts', 'commands', 'memoryx', 'unresolved_conflicts', 'unknowns', 'next_step') 'EvidenceReturn example'
    Add-Check 'EvidenceReturn schema version' ($evidenceReturn.schema_version -eq 'memoryx.evidence-return.v1') 'Unexpected EvidenceReturn schema version.'
    Add-Check 'EvidenceReturn module binding' ($evidenceReturn.module.id -eq $moduleJson.id -and $evidenceReturn.module.slug -eq $moduleJson.slug) 'EvidenceReturn example module mismatch.'
    Add-Check 'EvidenceReturn execution profile' ($evidenceReturn.execution.model -eq 'gpt-5.6-sol' -and $evidenceReturn.execution.reasoning_effort -eq 'xhigh') 'EvidenceReturn example execution profile mismatch.'
    Add-Check 'EvidenceReturn base binding' ($evidenceReturn.memoryx.base_path -eq $moduleJson.base.path) 'EvidenceReturn example base mismatch.'
    Add-Check 'EvidenceReturn initial epistemic state' ($evidenceReturn.session.state -eq 'UNBOUND' -and $null -eq $evidenceReturn.session.id -and $evidenceReturn.gates[0].status -eq 'not_run') 'EvidenceReturn example overclaims initial state.'
}

foreach ($hookName in @('SessionStart.ps1', 'PreCompact.ps1', 'PostCompact.ps1')) {
    $hookPath = Join-Path $contourRoot "hooks/$hookName"
    if (Test-Path -LiteralPath $hookPath -PathType Leaf) {
        $parseErrors = $null
        [Management.Automation.Language.Parser]::ParseFile($hookPath, [ref]$null, [ref]$parseErrors) | Out-Null
        Add-Check "hook syntax $hookName" ($parseErrors.Count -eq 0) "Hook syntax failed for ${hookName}: $($parseErrors.Message -join '; ')"
        $canonicalHook = Join-Path $systemRoot "hooks/$hookName"
        if ($contourRoot -ne $systemRoot) {
            $sameHook = (Get-FileHash -LiteralPath $hookPath -Algorithm SHA256).Hash -eq (Get-FileHash -LiteralPath $canonicalHook -Algorithm SHA256).Hash
            Add-Check "hook canonical copy $hookName" $sameHook "Module hook differs from canonical hook: $hookName"
        }
    }
}

$result = [ordered]@{
    schema_version = 'memoryx.module-validation.v1'
    module_id = if ($null -ne $moduleJson) { $moduleJson.id } else { $null }
    module_path = $contourRoot
    session_state = $sessionState
    session_id = $sessionId
    structural_only = $true
    physical_base_required = [bool]$RequireBase
    passed = ($errors.Count -eq 0)
    checks_passed = $checks.Count
    errors = @($errors)
    live_gates_not_proven = @('real_hook_lifecycle', 'real_compact_resume', 'cache_reuse', 'model_quality', 'MemoryX_semantic_acceptance', 'N5_completion')
}

$result | ConvertTo-Json -Depth 10
if ($errors.Count -gt 0) {
    exit 1
}
