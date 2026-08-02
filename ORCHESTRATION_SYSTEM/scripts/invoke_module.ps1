[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$Module,
    [string]$Prompt,
    [switch]$DryRun,
    [switch]$DangerFullAccess
)

$ErrorActionPreference = 'Stop'
$systemRoot = Split-Path -Parent $PSScriptRoot
$repoRoot = Split-Path -Parent $systemRoot
$manifest = Get-Content -LiteralPath (Join-Path $systemRoot 'manifest.json') -Raw | ConvertFrom-Json
. (Join-Path $PSScriptRoot 'interagent_language.ps1')

if ($Module -eq 'root' -or $Module -eq 'MX-ROOT') {
    $contourRoot = $systemRoot
} else {
    $definition = @($manifest.modules | Where-Object { $_.id -eq $Module -or $_.slug -eq $Module })
    if ($definition.Count -ne 1) {
        throw "Module selector must resolve exactly once: $Module"
    }
    $contourRoot = Join-Path $systemRoot "modules/$($definition[0].id)-$($definition[0].slug)"
}

$validationJson = & (Join-Path $systemRoot 'scripts/validate_module.ps1') -ModulePath $contourRoot
$validation = $validationJson | ConvertFrom-Json
if ($validation.passed -ne $true) {
    throw "Module validation failed: $validationJson"
}
$moduleJson = Get-Content -LiteralPath (Join-Path $contourRoot 'module.json') -Raw | ConvertFrom-Json

if ($moduleJson.execution.model -ne 'gpt-5.6-sol' -or $moduleJson.execution.reasoning_effort -ne 'xhigh') {
    throw 'Refusing to invoke a module with a changed immutable execution profile.'
}
if (@($moduleJson.execution.forbidden_reasoning_effort) -notcontains 'max') {
    throw 'Refusing to invoke a module that does not explicitly forbid max reasoning.'
}

$sessionPath = Join-Path $contourRoot 'session_id.txt'
$sessionText = [IO.File]::ReadAllText($sessionPath).Trim()
$isBound = $sessionText.Length -gt 0
$sessionGuid = [Guid]::Empty
if ($isBound -and -not [Guid]::TryParseExact($sessionText, 'D', [ref]$sessionGuid)) {
    throw 'Bound session_id.txt is not one canonical UUID.'
}

$hookOutput = & (Join-Path $contourRoot 'hooks/SessionStart.ps1')
$null = $hookOutput | ConvertFrom-Json

if (-not $Prompt) {
    $Prompt = Get-Content -LiteralPath (Join-Path $contourRoot 'TASK.md') -Raw
}
Assert-MemoryXEnglishInterAgentText -Text $Prompt -Label "task prompt for $($moduleJson.id)"

$stablePrefix = @"
You are the permanently assigned developer for MemoryX module $($moduleJson.id) ($($moduleJson.display_name)).
Inter-agent language: English only. Write every task packet, handoff, progress
or evidence narrative, and compact recovery instruction in English. Translate
user requests into English before persisting or forwarding them. User-facing
responses may follow the user's language, but untranslated user-facing text
must not enter inter-agent artifacts.
Before acting, read these durable module contracts from ${contourRoot}:
CANONICAL_PACKET.md, TASK.md, PLAN.md, PROGRESS.md, DECISIONS.md, ACCEPTANCE.md,
COMPACT_CONTEXT.md, MEMORYX_CONTRACT.json, state/RECOVERY.json, and DOSSIERS/INDEX.md.
Read the canonical concept and roadmap references named by module.json. Do not
silently change the MemoryX concept or roadmap. Work only inside declared
ownership; record cross-module handoffs. Keep model gpt-5.6-sol and reasoning
xhigh; max is forbidden. Before compact, update task, plan, progress,
decisions, and the recovery record. Return a schema-valid EvidenceReturn and
do not present structural checks as proof of hooks, compact, cache reuse,
model quality, MemoryX semantic acceptance, or N5 completion.

Current task packet:
$Prompt
"@
Assert-MemoryXEnglishInterAgentText -Text $stablePrefix -Label "stable prefix for $($moduleJson.id)"

$configOverride = 'model_reasoning_effort="xhigh"'
$execPrefix = @('exec')
if ($DangerFullAccess) {
    $execPrefix += '--dangerously-bypass-approvals-and-sandbox'
}
if ($isBound) {
    $arguments = $execPrefix + @('resume', $sessionGuid.ToString('D'), '-m', 'gpt-5.6-sol', '-c', $configOverride, '--json', $stablePrefix)
    $mode = 'resume'
} else {
    $arguments = $execPrefix + @('-m', 'gpt-5.6-sol', '-c', $configOverride, '-C', $repoRoot, '--json', $stablePrefix)
    $mode = 'start'
}

if ($DryRun) {
    [ordered]@{
        schema_version = 'memoryx.module-invocation-preview.v1'
        module_id = $moduleJson.id
        mode = $mode
        session_state = $validation.session_state
        session_id = if ($isBound) { $sessionGuid.ToString('D') } else { $null }
        executable = 'codex'
        arguments_without_prompt = @($arguments | Select-Object -First ($arguments.Count - 1))
        model_reasserted = $true
        reasoning_reasserted = $true
        danger_full_access = [bool]$DangerFullAccess
        hook_trust_bypass_used = $false
        uuid_will_only_be_bound_if_observed = $true
    } | ConvertTo-Json -Depth 8
    exit 0
}

$codexCommand = Get-Command codex -ErrorAction Stop
$timestamp = [DateTime]::UtcNow.ToString('yyyyMMddTHHmmssZ')
$logPath = Join-Path $contourRoot "logs/invoke-$timestamp.log"
$observedThreadIds = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
$previousLocation = Get-Location

try {
    Set-Location -LiteralPath $repoRoot
    & $codexCommand.Source @arguments 2>&1 | ForEach-Object {
        $line = $_.ToString()
        [IO.File]::AppendAllText($logPath, $line + [Environment]::NewLine, [Text.UTF8Encoding]::new($false))
        Write-Output $line
        try {
            $event = $line | ConvertFrom-Json
            if ($event.type -eq 'thread.started' -and $event.thread_id) {
                $candidate = [Guid]::Empty
                if ([Guid]::TryParseExact([string]$event.thread_id, 'D', [ref]$candidate)) {
                    $null = $observedThreadIds.Add($candidate.ToString('D'))
                }
            }
        } catch {
            # Non-JSON stderr is retained in the invocation log but cannot bind a session.
        }
    }
    $exitCode = $LASTEXITCODE
} finally {
    Set-Location -LiteralPath $previousLocation
}

if ($observedThreadIds.Count -gt 1) {
    throw 'Codex emitted more than one distinct session UUID; refusing ambiguous binding.'
}

$observedId = if ($observedThreadIds.Count -eq 1) { @($observedThreadIds)[0] } else { $null }
if ($isBound) {
    if ($null -ne $observedId -and $observedId -ne $sessionGuid.ToString('D')) {
        throw 'Resumed invocation emitted a different session UUID; existing binding was preserved.'
    }
} elseif ($null -ne $observedId) {
    [IO.File]::WriteAllText($sessionPath, $observedId, [Text.UTF8Encoding]::new($false))
}

if ($exitCode -ne 0) {
    throw "Codex invocation exited with code $exitCode. A real observed UUID was preserved for resume; inspect $logPath."
}
if (-not $isBound -and $null -eq $observedId) {
    throw 'Codex start completed without an observed thread.started UUID; module remains UNBOUND.'
}

[ordered]@{
    schema_version = 'memoryx.module-invocation-result.v1'
    module_id = $moduleJson.id
    mode = $mode
    exit_code = $exitCode
    session_id = if ($isBound) { $sessionGuid.ToString('D') } else { $observedId }
    log_path = $logPath
    model = 'gpt-5.6-sol'
    reasoning_effort = 'xhigh'
    danger_full_access = [bool]$DangerFullAccess
    hook_trust_bypass_used = $false
} | ConvertTo-Json -Depth 6
