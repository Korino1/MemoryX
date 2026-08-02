[CmdletBinding()]
param(
    [switch]$Check
)

$ErrorActionPreference = 'Stop'
$systemRoot = Split-Path -Parent $PSScriptRoot
$repoRoot = Split-Path -Parent $systemRoot
$manifestPath = Join-Path $systemRoot 'manifest.json'
$manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
$created = [Collections.Generic.List[string]]::new()
$verified = [Collections.Generic.List[string]]::new()

function Ensure-Directory {
    param([Parameter(Mandatory)][string]$Path)
    if (Test-Path -LiteralPath $Path -PathType Container) {
        $verified.Add((Resolve-Path -LiteralPath $Path).Path)
        return
    }
    if ($Check) {
        throw "Required directory is missing: $Path"
    }
    New-Item -ItemType Directory -Path $Path | Out-Null
    $created.Add($Path)
}

function Write-TextIfMissing {
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][AllowEmptyString()][string]$Content
    )
    if (Test-Path -LiteralPath $Path -PathType Leaf) {
        $verified.Add((Resolve-Path -LiteralPath $Path).Path)
        return
    }
    if ($Check) {
        throw "Required file is missing: $Path"
    }
    $parent = Split-Path -Parent $Path
    Ensure-Directory -Path $parent
    [IO.File]::WriteAllText($Path, $Content, [Text.UTF8Encoding]::new($false))
    $created.Add($Path)
}

function Write-JsonIfMissing {
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)]$Value
    )
    $json = $Value | ConvertTo-Json -Depth 20
    Write-TextIfMissing -Path $Path -Content ($json + [Environment]::NewLine)
}

function New-MemoryXContract {
    param(
        [Parameter(Mandatory)][string]$ModuleId,
        [Parameter(Mandatory)][string]$BaseName
    )
    [ordered]@{
        schema_version = 'memoryx.module-contract.v1'
        module_id = $ModuleId
        physical_base = [ordered]@{
            scope = 'project-local'
            path = ".memoryx/bases/$BaseName"
            name = $BaseName
        }
        ownership = [ordered]@{
            max_mutable_owners = 1
            live_owner_required_for_shared_access = $true
            implicit_second_writer_forbidden = $true
        }
        knowledge = [ordered]@{
            unit = 'knowledge_atom'
            rules_authority = 'versioned_files'
            memoryx_role = 'structured_evidence_provenance_conflicts_query_recovery'
            candidate_is_not_proof = $true
        }
        evidence = [ordered]@{
            source_registration_required = $true
            provenance_required_for_acceptance = $true
            unverified_model_output_is_not_evidence = $true
        }
        query = [ordered]@{
            strict_query_contract = $true
            fixed_point_required_for_strict_answer = $true
            answer_graph_required = $true
            budgets_required = $true
        }
        conflicts = [ordered]@{
            policy = 'fail-closed'
            branching_preserved = $true
            unknowns_explicit = $true
        }
        recovery = [ordered]@{
            file_contracts_are_primary = $true
            history_required = $true
            n5_status = 'open'
        }
        forbidden = @('user-scoped-base', 'foreign-project-base', 'unsourced-acceptance-evidence')
    }
}

function New-RecoveryRecord {
    [ordered]@{
        schema_version = 'memoryx.compact-recovery.v1'
        status = 'never_compacted'
        recorded_at_utc = $null
        session_state = 'UNBOUND'
        session_id = $null
        acceptance_state = 'not_evaluated'
        document_hashes = [ordered]@{}
        compact_context_sha256 = $null
        recovery_sources = @()
        statement = 'No real pre-compact lifecycle has been observed.'
    }
}

function Join-MarkdownList {
    param([object[]]$Values)
    if ($Values.Count -eq 0) {
        return '- None declared.'
    }
    return (($Values | ForEach-Object { '- `' + $_ + '`' }) -join [Environment]::NewLine)
}

function Materialize-Contour {
    param(
        [Parameter(Mandatory)]$Definition,
        [Parameter(Mandatory)][string]$ContourRoot,
        [Parameter(Mandatory)][string]$RelativePath,
        [Parameter(Mandatory)][bool]$IsRoot
    )

    foreach ($directory in @('DOSSIERS', 'state', 'evidence', 'logs', 'hooks')) {
        Ensure-Directory -Path (Join-Path $ContourRoot $directory)
    }

    $schemaPath = if ($IsRoot) { 'schemas/evidence-return.schema.json' } else { '../../schemas/evidence-return.schema.json' }
    $dossierPaths = @($Definition.dossiers | ForEach-Object { "DOSSIERS/$($_.id).md" })
    $moduleJson = [ordered]@{
        schema_version = 'memoryx.orchestration-module.v1'
        id = $Definition.id
        slug = $Definition.slug
        display_name = $Definition.display_name
        responsibility = $Definition.responsibility
        path = $RelativePath
        base = [ordered]@{
            scope = 'project-local'
            name = $Definition.base_name
            path = ".memoryx/bases/$($Definition.base_name)"
            max_mutable_owners = 1
        }
        execution = [ordered]@{
            model = $manifest.execution_profile.model
            reasoning_effort = $manifest.execution_profile.reasoning_effort
            forbidden_reasoning_effort = @('max')
            reassert_on_start_resume = $true
        }
        session = [ordered]@{
            binding_file = 'session_id.txt'
            initial_state = 'UNBOUND'
            resume_only_when_bound = $true
            never_synthesize_uuid = $true
        }
        ownership = [ordered]@{
            owned_paths = @($Definition.owned_paths)
            shared_surfaces = @($Definition.shared_surfaces)
            forbidden_paths = @($Definition.forbidden_paths)
        }
        canonical_refs = @($Definition.canonical_refs)
        dossiers = $dossierPaths
        hooks = [ordered]@{
            session_start = 'hooks/SessionStart.ps1'
            pre_compact = 'hooks/PreCompact.ps1'
            post_compact = 'hooks/PostCompact.ps1'
        }
        evidence_return_schema = $schemaPath
    }
    Write-JsonIfMissing -Path (Join-Path $ContourRoot 'module.json') -Value $moduleJson

    Write-TextIfMissing -Path (Join-Path $ContourRoot 'session_id.txt') -Content ''
    Write-JsonIfMissing -Path (Join-Path $ContourRoot 'MEMORYX_CONTRACT.json') -Value (New-MemoryXContract -ModuleId $Definition.id -BaseName $Definition.base_name)
    Write-JsonIfMissing -Path (Join-Path $ContourRoot 'state/RECOVERY.json') -Value (New-RecoveryRecord)
    Write-TextIfMissing -Path (Join-Path $ContourRoot 'state/.gitkeep') -Content ''
    Write-TextIfMissing -Path (Join-Path $ContourRoot 'evidence/.gitkeep') -Content ''
    Write-TextIfMissing -Path (Join-Path $ContourRoot 'logs/.gitkeep') -Content ''
    $evidenceReturnExample = [ordered]@{
        schema_version = 'memoryx.evidence-return.v1'
        module = [ordered]@{ id = $Definition.id; slug = $Definition.slug }
        session = [ordered]@{ state = 'UNBOUND'; id = $null }
        execution = [ordered]@{ model = 'gpt-5.6-sol'; reasoning_effort = 'xhigh' }
        task = 'No active task.'
        gates = @([ordered]@{ name = 'module_acceptance'; status = 'not_run'; evidence = 'No task has been activated.' })
        changed_artifacts = @()
        commands = @()
        memoryx = [ordered]@{ base_path = ".memoryx/bases/$($Definition.base_name)"; provenance_refs = @() }
        unresolved_conflicts = @()
        unknowns = @('No live module execution has been observed.')
        next_step = 'Activate a bounded task through the root manifest registry.'
    }
    Write-JsonIfMissing -Path (Join-Path $ContourRoot 'evidence/EVIDENCE_RETURN.example.json') -Value $evidenceReturnExample

    foreach ($hookName in @('SessionStart.ps1', 'PreCompact.ps1', 'PostCompact.ps1')) {
        $sourceHook = Join-Path $systemRoot "hooks/$hookName"
        $targetHook = Join-Path $ContourRoot "hooks/$hookName"
        if (-not (Test-Path -LiteralPath $targetHook -PathType Leaf)) {
            if ($Check) {
                throw "Required hook is missing: $targetHook"
            }
            Copy-Item -LiteralPath $sourceHook -Destination $targetHook
            $created.Add($targetHook)
        } else {
            $verified.Add((Resolve-Path -LiteralPath $targetHook).Path)
        }
    }

    $canonicalRefs = Join-MarkdownList -Values @($Definition.canonical_refs)
    $ownedPaths = Join-MarkdownList -Values @($Definition.owned_paths)
    $sharedSurfaces = Join-MarkdownList -Values @($Definition.shared_surfaces)
    $forbiddenPaths = Join-MarkdownList -Values @($Definition.forbidden_paths)

    $canonicalPacket = @"
# $($Definition.id) Canonical Packet

Module: **$($Definition.display_name)**

## Responsibility

$($Definition.responsibility)

## Canonical Authorities

$canonicalRefs

The module must read the relevant canonical passages and the current roadmap
before changing implementation. It must stop on a concept conflict rather than
silently reinterpret the concept or mark an open roadmap gate complete.

## Primary Ownership

$ownedPaths

## Shared Surfaces

$sharedSurfaces

Shared surfaces require an explicit handoff recorded in ``DECISIONS.md`` and in
the EvidenceReturn of every affected module.

## Forbidden Ownership

$forbiddenPaths

## Immutable Execution

- Model: ``gpt-5.6-sol``
- Reasoning effort: ``xhigh``
- ``max``: forbidden
- Bound sessions resume only through ``codex exec resume <UUID>`` with model and
  reasoning reasserted.
- An empty ``session_id.txt`` means ``UNBOUND``; no script may invent a UUID.

## Non-Regression

Atoms, contexts and conflicts, Heptapod backward+forward reasoning,
FixedPointSolver, minimal AnswerGraph, provenance federation, CAS/Merkle,
CRDT/WAL/repair, full MCP, and explicit local storage scopes cannot be removed
or bypassed. N5 remains open until its own acceptance evidence passes.
"@
    Write-TextIfMissing -Path (Join-Path $ContourRoot 'CANONICAL_PACKET.md') -Content ($canonicalPacket + [Environment]::NewLine)

    $task = @"
# Task

Status: ``NO_ACTIVE_TASK``

No implementation task has been activated. The root orchestrator must write a
bounded task here before invoking this module. The task must name affected
canonical requirements, owned surfaces, acceptance gates, and stop conditions.
"@
    Write-TextIfMissing -Path (Join-Path $ContourRoot 'TASK.md') -Content ($task + [Environment]::NewLine)

    $plan = @"
# Plan

Status: ``IDLE``

1. Re-read ``CANONICAL_PACKET.md`` and the referenced concept/roadmap passages.
2. Confirm the task is inside this module's ownership boundary.
3. Record assumptions, conflicts, and handoffs before implementation.
4. Make the smallest coherent change and add focused evidence.
5. Run module acceptance gates, then return a schema-valid EvidenceReturn.

No task-specific steps are approved until ``TASK.md`` is activated.
"@
    Write-TextIfMissing -Path (Join-Path $ContourRoot 'PLAN.md') -Content ($plan + [Environment]::NewLine)

    $progress = @"
# Progress

State: ``UNBOUND``

- The contour was materialized from ``ORCHESTRATION_SYSTEM/manifest.json``.
- No real Codex session has been observed or bound.
- No module task, live hook lifecycle, compact/resume, cache reuse, model
  quality, or MemoryX semantic acceptance is claimed.
"@
    Write-TextIfMissing -Path (Join-Path $ContourRoot 'PROGRESS.md') -Content ($progress + [Environment]::NewLine)

    $decisions = @"
# Decisions

## D-000: Inherit Root Contracts

Accepted:

- Preserve the MemoryX concept and current roadmap without hidden changes.
- Use only the module-local project base ``.memoryx/bases/$($Definition.base_name)``.
- Permit one mutable owner for that physical base.
- Keep mandatory rules in files; use MemoryX for structured evidence,
  provenance, conflicts, query, and recovery.
- Treat stable session/prefix as cache optimization only.

No task-specific architectural decision has been made.
"@
    Write-TextIfMissing -Path (Join-Path $ContourRoot 'DECISIONS.md') -Content ($decisions + [Environment]::NewLine)

    $acceptance = @"
# Acceptance

Current state: ``NOT_EVALUATED``

Mandatory module gates:

- [ ] Task is inside declared ownership or has explicit handoffs.
- [ ] Canonical requirements and roadmap status are cited.
- [ ] Changed artifacts and commands have observed results.
- [ ] MemoryX evidence has registered sources and provenance.
- [ ] Conflicts, unknowns, and limitations are explicit.
- [ ] Module-specific tests and relevant repository gates pass.
- [ ] EvidenceReturn validates against the canonical schema.

Structural validation cannot check these boxes automatically.
"@
    Write-TextIfMissing -Path (Join-Path $ContourRoot 'ACCEPTANCE.md') -Content ($acceptance + [Environment]::NewLine)

    $compact = @"
# Compact Context

Module: ``$($Definition.id)`` / ``$($Definition.slug)``
Session: ``UNBOUND``
Model: ``gpt-5.6-sol``
Reasoning: ``xhigh``
Active task: none

Recovery order:

1. ``CANONICAL_PACKET.md``
2. ``TASK.md``
3. ``PLAN.md``
4. ``PROGRESS.md``
5. ``DECISIONS.md``
6. ``ACCEPTANCE.md``
7. ``state/RECOVERY.json``
8. ``MEMORYX_CONTRACT.json``

This file is an initial durable pointer set. It is not evidence of a real
compact event, cache reuse, retained hidden context, or model quality.
"@
    Write-TextIfMissing -Path (Join-Path $ContourRoot 'COMPACT_CONTEXT.md') -Content ($compact + [Environment]::NewLine)

    $indexLines = [Collections.Generic.List[string]]::new()
    $indexLines.Add("# $($Definition.id) Dossier Index")
    $indexLines.Add('')
    $indexLines.Add('Each dossier is a separate contract boundary. A task must cite the dossiers it changes.')
    $indexLines.Add('')
    foreach ($dossier in $Definition.dossiers) {
        $indexLines.Add("- [$($dossier.title)]($($dossier.id).md): $($dossier.scope)")
    }
    Write-TextIfMissing -Path (Join-Path $ContourRoot 'DOSSIERS/INDEX.md') -Content (($indexLines -join [Environment]::NewLine) + [Environment]::NewLine)

    foreach ($dossier in $Definition.dossiers) {
        $dossierContent = @"
# $($dossier.title)

ID: ``$($dossier.id)``
Kind: ``$($dossier.kind)``
Owner: ``$($Definition.id)``

## Scope

$($dossier.scope)

## Contract

- Canonical requirements must be cited before this dossier is changed.
- Definitions, invariants, preconditions, postconditions, and failure behavior
  must be explicit for an activated task.
- Retrieval candidates are not proof; accepted claims require evidence and
  provenance.
- Unknown or disputed semantics remain unresolved rather than being guessed.

## Current State

No task-specific contract has been activated. This dossier is an ownership and
recovery boundary, not a claim of implementation completeness.

## Required Evidence

- source references and MemoryX provenance;
- affected symbols or formats;
- focused tests with observed output;
- compatibility and non-regression analysis;
- unresolved risks and the next falsifying test.
"@
        Write-TextIfMissing -Path (Join-Path $ContourRoot "DOSSIERS/$($dossier.id).md") -Content ($dossierContent + [Environment]::NewLine)
    }
}

foreach ($controlFile in @(
    'ARCHITECTURE.md',
    'README.md',
    'manifest.json',
    'session_registry.json',
    'schemas/manifest.schema.json',
    'schemas/module.schema.json',
    'schemas/memoryx-contract.schema.json',
    'schemas/evidence-return.schema.json',
    'schemas/session-registry.schema.json',
    'hooks/SessionStart.ps1',
    'hooks/PreCompact.ps1',
    'hooks/PostCompact.ps1'
)) {
    $controlPath = Join-Path $systemRoot $controlFile
    if (-not (Test-Path -LiteralPath $controlPath -PathType Leaf)) {
        throw "Canonical orchestration control file is missing: $controlPath"
    }
    $verified.Add((Resolve-Path -LiteralPath $controlPath).Path)
}

Ensure-Directory -Path (Join-Path $systemRoot 'modules')
Ensure-Directory -Path (Join-Path $systemRoot 'state')
Ensure-Directory -Path (Join-Path $systemRoot 'evidence')
Ensure-Directory -Path (Join-Path $systemRoot 'logs')
Ensure-Directory -Path (Join-Path $systemRoot 'DOSSIERS')

$rootDefinition = [pscustomobject]@{
    id = 'MX-ROOT'
    slug = 'root-orchestrator'
    display_name = 'MemoryX Root Orchestrator'
    base_name = $manifest.root_contour.base_name
    responsibility = 'Route bounded work through the manifest registry, coordinate ownership and resources, and preserve concept and roadmap authority.'
    owned_paths = @('ORCHESTRATION_SYSTEM/')
    shared_surfaces = @('cross-module handoffs', 'root validation', 'release acceptance routing')
    forbidden_paths = @('unreviewed production implementation', 'module-local physical bases')
    canonical_refs = @('Concept/SKF.txt', 'Concept/SKF-1.1 Implementer-Ready Spec.txt', 'Concept/Расширение.txt', 'CONCEPT_EXTENSION_IMPLEMENTATION_PLAN.md', 'ORCHESTRATION_PLAN.md')
    dossiers = @(
        [pscustomobject]@{ id = 'ROUTING'; title = 'Registry Routing and Ownership'; kind = 'conceptual'; scope = 'module selection, handoffs, ownership conflicts and bounded task packets' },
        [pscustomobject]@{ id = 'RECOVERY'; title = 'Orchestrator Recovery Contract'; kind = 'technical'; scope = 'session binding, compact records, evidence return and shared-host continuity' }
    )
}
Materialize-Contour -Definition $rootDefinition -ContourRoot $systemRoot -RelativePath 'ORCHESTRATION_SYSTEM' -IsRoot $true

foreach ($module in $manifest.modules) {
    $folderName = "$($module.id)-$($module.slug)"
    $moduleRoot = Join-Path $systemRoot "modules/$folderName"
    Ensure-Directory -Path $moduleRoot
    Materialize-Contour -Definition $module -ContourRoot $moduleRoot -RelativePath "ORCHESTRATION_SYSTEM/modules/$folderName" -IsRoot $false
}

[ordered]@{
    schema_version = 'memoryx.build-scheme-result.v1'
    mode = if ($Check) { 'check' } else { 'materialize_missing_only' }
    module_count = $manifest.modules.Count
    created_count = $created.Count
    verified_count = $verified.Count
    created = @($created)
    statement = 'Existing progress and decision files were not overwritten.'
} | ConvertTo-Json -Depth 6
