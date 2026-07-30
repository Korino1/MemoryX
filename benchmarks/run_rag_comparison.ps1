#requires -Version 7.0

[CmdletBinding()]
param(
    [ValidateSet('prepare', 'record', 'score', 'report', 'validate', 'selftest')]
    [string]$Mode = 'prepare',
    [string]$RunDirectory,
    [ValidateSet('memoryx', 'rag')]
    [string]$System,
    [string]$AdapterCommand,
    [string]$MemoryXResults,
    [string]$RagResults,
    [string]$MemoryXRepeatResults,
    [string]$RagRepeatResults,
    [string]$ScoreFile,
    [string]$OutputDirectory
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$BenchmarkRoot = Join-Path $PSScriptRoot 'rag_comparison'
$CorpusPath = Join-Path $BenchmarkRoot 'corpus.jsonl'
$CasesPath = Join-Path $BenchmarkRoot 'cases.jsonl'
$SchemaPath = Join-Path $BenchmarkRoot 'schema.json'
$ScenarioKeys = @(
    'conflict_visible',
    'temporal_current',
    'multi_hop_complete',
    'constraints_respected',
    'missing_evidence_safe',
    'provenance_complete',
    'context_isolated',
    'reproducible'
)

function Get-Jsonl {
    param([Parameter(Mandatory)][string]$Path)
    if (-not (Test-Path -LiteralPath $Path)) { throw "JSONL file not found: $Path" }
    $items = [System.Collections.Generic.List[object]]::new()
    $lineNumber = 0
    foreach ($line in Get-Content -LiteralPath $Path) {
        $lineNumber++
        if ([string]::IsNullOrWhiteSpace($line)) { continue }
        try { $items.Add(($line | ConvertFrom-Json -Depth 100)) }
        catch { throw "Invalid JSON at ${Path}:$lineNumber. $($_.Exception.Message)" }
    }
    return $items.ToArray()
}

function Write-Jsonl {
    param([Parameter(Mandatory)][object[]]$Items, [Parameter(Mandatory)][string]$Path)
    $parent = Split-Path -Parent $Path
    New-Item -ItemType Directory -Force -Path $parent | Out-Null
    $lines = foreach ($item in $Items) { $item | ConvertTo-Json -Depth 100 -Compress }
    [System.IO.File]::WriteAllLines($Path, [string[]]$lines, [System.Text.UTF8Encoding]::new($false))
}

function Write-Json {
    param([Parameter(Mandatory)]$Value, [Parameter(Mandatory)][string]$Path)
    $parent = Split-Path -Parent $Path
    New-Item -ItemType Directory -Force -Path $parent | Out-Null
    $json = $Value | ConvertTo-Json -Depth 100
    [System.IO.File]::WriteAllText($Path, $json + [Environment]::NewLine, [System.Text.UTF8Encoding]::new($false))
}

function Get-FileHashString {
    param([Parameter(Mandatory)][string]$Path)
    return (Get-FileHash -Algorithm SHA256 -LiteralPath $Path).Hash.ToLowerInvariant()
}

function Get-PropertyValue {
    param($Object, [Parameter(Mandatory)][string]$Name, $Default = $null)
    if ($null -eq $Object) { return $Default }
    $property = $Object.PSObject.Properties[$Name]
    if ($null -eq $property) { return $Default }
    return $property.Value
}

function Require-Property {
    param($Object, [Parameter(Mandatory)][string]$Name, [Parameter(Mandatory)][string]$Label)
    $property = $Object.PSObject.Properties[$Name]
    if ($null -eq $property) { throw "$Label is missing required property '$Name'." }
    return $property.Value
}

function Assert-String {
    param($Value, [Parameter(Mandatory)][string]$Label, [switch]$AllowEmpty)
    if ($Value -isnot [string] -or ((-not $AllowEmpty) -and [string]::IsNullOrWhiteSpace($Value))) {
        throw "$Label must be a non-empty string."
    }
}

function ConvertTo-StringArray {
    param($Value, [Parameter(Mandatory)][string]$Label)
    if ($null -eq $Value) { return @() }
    # PowerShell unwraps a one-element JSON array when it crosses a function boundary.
    if ($Value -is [string]) { return [string[]]@($Value) }
    if ($Value -isnot [System.Collections.IEnumerable]) { throw "$Label must be an array." }
    $result = @($Value)
    foreach ($entry in $result) { Assert-String $entry "$Label entry" }
    return [string[]]$result
}

function Assert-Corpus {
    param([object[]]$Corpus)
    if ($Corpus.Count -eq 0) { throw 'Corpus must contain at least one source record.' }
    $seen = @{}
    foreach ($record in $Corpus) {
        $id = Require-Property $record 'id' 'Corpus record'
        Assert-String $id 'Corpus record id'
        if ($seen.ContainsKey($id)) { throw "Corpus has duplicate id '$id'." }
        $seen[$id] = $true
        Assert-String (Require-Property $record 'text' "Corpus record '$id'") "Corpus record '$id'.text"
        Assert-String (Require-Property $record 'context' "Corpus record '$id'") "Corpus record '$id'.context"
        Assert-String (Require-Property $record 'provenance' "Corpus record '$id'") "Corpus record '$id'.provenance"
    }
    return $seen
}

function Assert-Cases {
    param([object[]]$Cases, [hashtable]$CorpusIds)
    if ($Cases.Count -eq 0) { throw 'Cases must contain at least one benchmark case.' }
    $seen = @{}
    foreach ($case in $Cases) {
        $id = Require-Property $case 'id' 'Case'
        Assert-String $id 'Case id'
        if ($seen.ContainsKey($id)) { throw "Cases have duplicate id '$id'." }
        $seen[$id] = $true
        Assert-String (Require-Property $case 'suite' "Case '$id'") "Case '$id'.suite"
        Assert-String (Require-Property $case 'query' "Case '$id'") "Case '$id'.query"
        $required = ConvertTo-StringArray (Require-Property $case 'required_evidence_ids' "Case '$id'") "Case '$id'.required_evidence_ids"
        foreach ($evidenceId in $required) {
            if (-not $CorpusIds.ContainsKey($evidenceId)) { throw "Case '$id' references missing corpus id '$evidenceId'." }
        }
        $limits = Require-Property $case 'limits' "Case '$id'"
        foreach ($limit in @('retrieval_limit', 'answer_max_chars', 'timeout_ms')) {
            $value = Require-Property $limits $limit "Case '$id'.limits"
            if ($value -isnot [System.ValueType] -or [double]$value -le 0) { throw "Case '$id'.limits.$limit must be positive." }
        }
        $scenarios = ConvertTo-StringArray (Require-Property $case 'functional_scenarios' "Case '$id'") "Case '$id'.functional_scenarios"
        foreach ($scenario in $scenarios) {
            if ($ScenarioKeys -notcontains $scenario) { throw "Case '$id' has unknown functional scenario '$scenario'." }
        }
    }
    return $seen
}

function Test-FileAgainstSchema {
    param([Parameter(Mandatory)][string]$Path)
    $schema = Get-Content -Raw -LiteralPath $SchemaPath
    foreach ($line in Get-Content -LiteralPath $Path) {
        if ([string]::IsNullOrWhiteSpace($line)) { continue }
        if (-not (Test-Json -Json $line -Schema $schema -ErrorAction SilentlyContinue)) {
            throw "A JSONL record in '$Path' does not match $SchemaPath."
        }
    }
}

function Get-FrozenInputs {
    Test-FileAgainstSchema $CorpusPath
    Test-FileAgainstSchema $CasesPath
    $corpus = Get-Jsonl $CorpusPath
    $cases = Get-Jsonl $CasesPath
    $corpusIds = Assert-Corpus $corpus
    Assert-Cases $cases $corpusIds | Out-Null
    return [pscustomobject]@{
        Corpus = $corpus
        Cases = $cases
        CorpusHash = Get-FileHashString $CorpusPath
        CasesHash = Get-FileHashString $CasesPath
    }
}

function New-RunManifest {
    param([Parameter(Mandatory)]$Inputs, [Parameter(Mandatory)][string]$Directory)
    $manifest = [ordered]@{
        schema_version = 'memoryx-rag-benchmark-run-v1'
        run_id = Split-Path -Leaf $Directory
        created_at_utc = [DateTime]::UtcNow.ToString('o')
        corpus_path = (Resolve-Path -LiteralPath $CorpusPath).Path
        cases_path = (Resolve-Path -LiteralPath $CasesPath).Path
        corpus_sha256 = $Inputs.CorpusHash
        cases_sha256 = $Inputs.CasesHash
        case_count = $Inputs.Cases.Count
        systems = @('memoryx', 'rag')
        fairness_rules = @(
            'Both systems must receive the frozen corpus, exact query text, and per-case limits.',
            'Do not add system-specific hints, evidence, post-processing, or hidden retrieval expansion.',
            'End-to-end latency is measured by the runner around each adapter invocation.'
        )
    }
    Write-Json $manifest (Join-Path $Directory 'run-manifest.json')
    return [pscustomobject]$manifest
}

function Read-RunManifest {
    param([Parameter(Mandatory)][string]$Directory)
    $path = Join-Path $Directory 'run-manifest.json'
    if (-not (Test-Path -LiteralPath $path)) { throw "Run manifest not found: $path. Run prepare first." }
    return Get-Content -Raw -LiteralPath $path | ConvertFrom-Json -Depth 100
}

function Assert-AdapterOutput {
    param($Output, [Parameter(Mandatory)]$Case)
    $status = Require-Property $Output 'status' "Adapter output for '$($Case.id)'"
    if (@('answered', 'closed', 'insufficient_evidence', 'conflicted', 'policy_blocked', 'error', 'timeout', 'unscored') -notcontains $status) {
        throw "Adapter output for '$($Case.id)' has unsupported status '$status'."
    }
    foreach ($name in @('retrieved_evidence_ids', 'provenance_evidence_ids', 'supporting_claim_ids')) {
        ConvertTo-StringArray (Get-PropertyValue $Output $name @()) "Adapter output '$($Case.id)'.$name" | Out-Null
    }
    $observations = Get-PropertyValue $Output 'observations' ([pscustomobject]@{})
    foreach ($scenario in $ScenarioKeys) {
        $value = Get-PropertyValue $observations $scenario $null
        if ($null -ne $value -and $value -isnot [bool]) { throw "Adapter output '$($Case.id)'.observations.$scenario must be boolean or null." }
    }
}

function Invoke-Adapter {
    param([Parameter(Mandatory)]$Case, [Parameter(Mandatory)]$Manifest, [Parameter(Mandatory)][string]$InputPath, [Parameter(Mandatory)][string]$OutputPath)
    $input = [ordered]@{
        schema_version = 'memoryx-rag-benchmark-adapter-input-v1'
        run_id = $Manifest.run_id
        system = $System
        case = $Case
        corpus_path = $Manifest.corpus_path
        corpus_sha256 = $Manifest.corpus_sha256
        cases_sha256 = $Manifest.cases_sha256
        contract = [ordered]@{
            query = $Case.query
            limits = $Case.limits
            required_evidence_ids = $Case.required_evidence_ids
            required_provenance = $true
        }
    }
    Write-Json $input $InputPath
    Remove-Item -LiteralPath $OutputPath -Force -ErrorAction SilentlyContinue
    $env:MX_BENCHMARK_INPUT = $InputPath
    $env:MX_BENCHMARK_OUTPUT = $OutputPath
    $env:MX_BENCHMARK_SYSTEM = $System
    $started = [System.Diagnostics.Stopwatch]::StartNew()
    try {
        $global:LASTEXITCODE = 0
        Invoke-Expression $AdapterCommand
        if ($LASTEXITCODE -ne 0) { throw "Adapter command exited with code $LASTEXITCODE." }
        if (-not (Test-Path -LiteralPath $OutputPath)) { throw "Adapter did not write `$env:MX_BENCHMARK_OUTPUT for case '$($Case.id)'." }
        $output = Get-Content -Raw -LiteralPath $OutputPath | ConvertFrom-Json -Depth 100
        Assert-AdapterOutput $output $Case
        return [pscustomobject]@{ Output = $output; ElapsedMs = [Math]::Round($started.Elapsed.TotalMilliseconds, 3) }
    }
    catch {
        $errorOutput = [pscustomobject]@{
            status = 'error'; answer = $null; retrieved_evidence_ids = @(); provenance_evidence_ids = @(); supporting_claim_ids = @(); logical_output_hash = $null
            observations = [pscustomobject]@{ conflict_visible = $null; temporal_current = $null; multi_hop_complete = $null; constraints_respected = $null; missing_evidence_safe = $null; provenance_complete = $null; context_isolated = $null; reproducible = $null }
            judgement = [pscustomobject]@{ accuracy = $null; factual_statements = $null; grounded_factual_statements = $null; closed_case = $null; notes = $null }
            adapter_metadata = $null; error = $_.Exception.Message
        }
        return [pscustomobject]@{ Output = $errorOutput; ElapsedMs = [Math]::Round($started.Elapsed.TotalMilliseconds, 3) }
    }
    finally {
        $started.Stop()
        Remove-Item Env:MX_BENCHMARK_INPUT -ErrorAction SilentlyContinue
        Remove-Item Env:MX_BENCHMARK_OUTPUT -ErrorAction SilentlyContinue
        Remove-Item Env:MX_BENCHMARK_SYSTEM -ErrorAction SilentlyContinue
    }
}

function New-ResultRecord {
    param([Parameter(Mandatory)]$Case, [Parameter(Mandatory)]$Manifest, [Parameter(Mandatory)]$Adapter, [Parameter(Mandatory)][double]$ElapsedMs)
    $observations = [ordered]@{}
    $adapterObservations = Get-PropertyValue $Adapter 'observations' ([pscustomobject]@{})
    foreach ($scenario in $ScenarioKeys) { $observations[$scenario] = Get-PropertyValue $adapterObservations $scenario $null }
    $judgement = Get-PropertyValue $Adapter 'judgement' ([pscustomobject]@{})
    return [ordered]@{
        schema_version = 'memoryx-rag-benchmark-result-v1'
        run_id = $Manifest.run_id
        system = $System
        case_id = $Case.id
        suite = $Case.suite
        query = $Case.query
        corpus_sha256 = $Manifest.corpus_sha256
        cases_sha256 = $Manifest.cases_sha256
        limits = $Case.limits
        status = $Adapter.status
        answer = Get-PropertyValue $Adapter 'answer' $null
        retrieved_evidence_ids = @(ConvertTo-StringArray (Get-PropertyValue $Adapter 'retrieved_evidence_ids' @()) 'adapter.retrieved_evidence_ids')
        provenance_evidence_ids = @(ConvertTo-StringArray (Get-PropertyValue $Adapter 'provenance_evidence_ids' @()) 'adapter.provenance_evidence_ids')
        supporting_claim_ids = @(ConvertTo-StringArray (Get-PropertyValue $Adapter 'supporting_claim_ids' @()) 'adapter.supporting_claim_ids')
        logical_output_hash = Get-PropertyValue $Adapter 'logical_output_hash' $null
        observations = $observations
        judgement = [ordered]@{
            accuracy = Get-PropertyValue $judgement 'accuracy' $null
            factual_statements = Get-PropertyValue $judgement 'factual_statements' $null
            grounded_factual_statements = Get-PropertyValue $judgement 'grounded_factual_statements' $null
            closed_case = Get-PropertyValue $judgement 'closed_case' $null
            notes = Get-PropertyValue $judgement 'notes' $null
        }
        latency_ms = [ordered]@{ end_to_end = $ElapsedMs }
        runner_limit_exceeded = ($ElapsedMs -gt [double]$Case.limits.timeout_ms)
        adapter_metadata = Get-PropertyValue $Adapter 'adapter_metadata' $null
        error = Get-PropertyValue $Adapter 'error' $null
    }
}

function Assert-ResultSet {
    param([object[]]$Results, [Parameter(Mandatory)]$Manifest, [object[]]$Cases, [object[]]$Corpus, [Parameter(Mandatory)][string]$ExpectedSystem)
    if ($Results.Count -eq 0) { throw "No $ExpectedSystem result records were supplied." }
    $caseIds = @{}; foreach ($case in $Cases) { $caseIds[$case.id] = $case }
    $corpusIds = @{}; foreach ($source in $Corpus) { $corpusIds[$source.id] = $true }
    $seen = @{}
    foreach ($result in $Results) {
        foreach ($property in @('schema_version', 'run_id', 'system', 'case_id', 'query', 'corpus_sha256', 'cases_sha256', 'limits', 'status', 'observations', 'judgement', 'latency_ms')) {
            Require-Property $result $property "Result record" | Out-Null
        }
        if ($result.schema_version -ne 'memoryx-rag-benchmark-result-v1') { throw 'Result schema_version is not supported.' }
        if ($result.run_id -ne $Manifest.run_id) { throw "Result '$($result.case_id)' is from a different run_id." }
        if ($result.system -ne $ExpectedSystem) { throw "Expected a $ExpectedSystem record, got '$($result.system)'." }
        if ($result.corpus_sha256 -ne $Manifest.corpus_sha256 -or $result.cases_sha256 -ne $Manifest.cases_sha256) { throw "Result '$($result.case_id)' does not use the frozen corpus/case files." }
        if (-not $caseIds.ContainsKey($result.case_id)) { throw "Result references unknown case '$($result.case_id)'." }
        if ($seen.ContainsKey($result.case_id)) { throw "Duplicate $ExpectedSystem result for case '$($result.case_id)'. Use a separate run for reproducibility repeats." }
        $seen[$result.case_id] = $true
        if (($result.query -ne $caseIds[$result.case_id].query) -or (($result.limits | ConvertTo-Json -Compress) -ne ($caseIds[$result.case_id].limits | ConvertTo-Json -Compress))) { throw "Result '$($result.case_id)' changed the shared query or limits." }
        $latency = Get-PropertyValue $result.latency_ms 'end_to_end' $null
        if ($null -eq $latency -or $latency -isnot [System.ValueType] -or [double]$latency -lt 0) { throw "Result '$($result.case_id)' has invalid end_to_end latency." }
        foreach ($field in @('retrieved_evidence_ids', 'provenance_evidence_ids')) {
            foreach ($evidenceId in ConvertTo-StringArray (Get-PropertyValue $result $field @()) "Result '$($result.case_id)'.$field") {
                if (-not $corpusIds.ContainsKey($evidenceId)) { throw "Result '$($result.case_id)' reports unknown evidence id '$evidenceId'." }
            }
        }
        $accuracy = Get-PropertyValue $result.judgement 'accuracy' $null
        if ($null -ne $accuracy -and ($accuracy -isnot [System.ValueType] -or [double]$accuracy -lt 0 -or [double]$accuracy -gt 1)) { throw "Result '$($result.case_id)' has invalid accuracy." }
        foreach ($field in @('factual_statements', 'grounded_factual_statements')) {
            $value = Get-PropertyValue $result.judgement $field $null
            if ($null -ne $value -and ($value -isnot [System.ValueType] -or [double]$value -lt 0 -or [Math]::Floor([double]$value) -ne [double]$value)) { throw "Result '$($result.case_id)' has invalid $field." }
        }
        $facts = Get-PropertyValue $result.judgement 'factual_statements' $null
        $grounded = Get-PropertyValue $result.judgement 'grounded_factual_statements' $null
        if ($null -ne $facts -and $null -ne $grounded -and [double]$grounded -gt [double]$facts) { throw "Result '$($result.case_id)' has more grounded statements than factual statements." }
        $closed = Get-PropertyValue $result.judgement 'closed_case' $null
        if ($null -ne $closed -and $closed -isnot [bool]) { throw "Result '$($result.case_id)' has invalid closed_case judgement." }
        foreach ($scenario in $ScenarioKeys) {
            $value = Get-PropertyValue $result.observations $scenario $null
            if ($null -ne $value -and $value -isnot [bool]) { throw "Result '$($result.case_id)' has invalid '$scenario' observation." }
        }
    }
    foreach ($case in $Cases) { if (-not $seen.ContainsKey($case.id)) { throw "Missing $ExpectedSystem result for case '$($case.id)'." } }
}

function Assert-RepeatResultSet {
    param([object[]]$Results, [Parameter(Mandatory)]$Manifest, [object[]]$Cases, [Parameter(Mandatory)][string]$ExpectedSystem)
    if ($Results.Count -eq 0) { return }
    $caseIds = @{}; foreach ($case in $Cases) { $caseIds[$case.id] = $case }
    $seen = @{}
    foreach ($result in $Results) {
        if ($result.system -ne $ExpectedSystem) { throw "Repeat record '$($result.case_id)' has wrong system '$($result.system)'." }
        if (-not $caseIds.ContainsKey($result.case_id) -or $seen.ContainsKey($result.case_id)) { throw "Repeat results have an unknown or duplicate case '$($result.case_id)'." }
        $seen[$result.case_id] = $true
        $case = $caseIds[$result.case_id]
        if ($result.corpus_sha256 -ne $Manifest.corpus_sha256 -or $result.cases_sha256 -ne $Manifest.cases_sha256 -or $result.query -ne $case.query) {
            throw "Repeat result '$($result.case_id)' does not match the frozen input." }
        if (($result.limits | ConvertTo-Json -Compress) -ne ($case.limits | ConvertTo-Json -Compress)) { throw "Repeat result '$($result.case_id)' changed shared limits." }
    }
}

function Get-Rate {
    param([object[]]$Values)
    $scored = @($Values | Where-Object { $null -ne $_ })
    if ($scored.Count -eq 0) { return [ordered]@{ value = $null; scored = 0; total = $Values.Count } }
    return [ordered]@{ value = [Math]::Round((($scored | Measure-Object -Average).Average), 6); scored = $scored.Count; total = $Values.Count }
}

function Get-Percentile {
    param([double[]]$Values, [double]$Percentile)
    if ($Values.Count -eq 0) { return $null }
    $ordered = @($Values | Sort-Object)
    $position = [Math]::Ceiling($Percentile * $ordered.Count) - 1
    return [Math]::Round($ordered[[Math]::Max(0, [Math]::Min($position, $ordered.Count - 1))], 3)
}

function Get-SystemScore {
    param([object[]]$Results, [object[]]$Cases, [string]$SystemName, [object[]]$RepeatResults = @())
    $caseById = @{}; foreach ($case in $Cases) { $caseById[$case.id] = $case }
    $accuracy = @(); $recall = @(); $groundedness = @(); $closed = @(); $latencies = @()
    $functional = @{}; foreach ($scenario in $ScenarioKeys) { $functional[$scenario] = @() }
    foreach ($result in $Results) {
        $case = $caseById[$result.case_id]
        $accuracy += Get-PropertyValue $result.judgement 'accuracy' $null
        $required = @(ConvertTo-StringArray $case.required_evidence_ids "Case '$($case.id)'.required_evidence_ids" | Select-Object -Unique)
        if ($required.Count -eq 0) { $recall += $null }
        else {
            $found = @(ConvertTo-StringArray (Get-PropertyValue $result 'retrieved_evidence_ids' @()) "Result '$($case.id)'.retrieved_evidence_ids" | Select-Object -Unique)
            $hits = @($required | Where-Object { $found -contains $_ }).Count
            $recall += ($hits / $required.Count)
        }
        $facts = Get-PropertyValue $result.judgement 'factual_statements' $null
        $grounded = Get-PropertyValue $result.judgement 'grounded_factual_statements' $null
        if ($null -eq $facts -or $null -eq $grounded -or [double]$facts -le 0) { $groundedness += $null }
        else { $groundedness += ([double]$grounded / [double]$facts) }
        if ([bool](Get-PropertyValue $case 'human_closure_expected' $false)) { $closed += Get-PropertyValue $result.judgement 'closed_case' $null }
        $latencies += [double]$result.latency_ms.end_to_end
        foreach ($scenario in $case.functional_scenarios) {
            if ($scenario -ne 'reproducible') { $functional[$scenario] += Get-PropertyValue $result.observations $scenario $null }
        }
    }
    $repeatById = @{}
    foreach ($repeat in $RepeatResults) {
        $repeatCaseId = Get-PropertyValue $repeat 'case_id' $null
        if ($null -ne $repeatCaseId) { $repeatById[$repeatCaseId] = $repeat }
    }
    foreach ($case in $Cases | Where-Object { $_.functional_scenarios -contains 'reproducible' }) {
        $base = $Results | Where-Object { $_.case_id -eq $case.id } | Select-Object -First 1
        $repeat = $repeatById[$case.id]
        $baseHash = Get-PropertyValue $base 'logical_output_hash' $null
        $repeatHash = Get-PropertyValue $repeat 'logical_output_hash' $null
        if ($null -eq $repeat -or [string]::IsNullOrWhiteSpace($baseHash) -or [string]::IsNullOrWhiteSpace($repeatHash)) { $functional['reproducible'] += $null }
        else { $functional['reproducible'] += ($baseHash -eq $repeatHash) }
    }
    $functionalScores = [ordered]@{}
    foreach ($scenario in $ScenarioKeys) { $functionalScores[$scenario] = Get-Rate $functional[$scenario] }
    $limitExceededCaseIds = @($Results | Where-Object { $_.runner_limit_exceeded } | ForEach-Object { $_.case_id })
    return [ordered]@{
        system = $SystemName
        cases = $Results.Count
        accuracy = Get-Rate $accuracy
        recall = Get-Rate $recall
        groundedness = Get-Rate $groundedness
        closed_case_rate = Get-Rate $closed
        end_to_end_latency_ms = [ordered]@{ samples = $latencies.Count; p50 = Get-Percentile $latencies 0.50; p95 = Get-Percentile $latencies 0.95; p99 = Get-Percentile $latencies 0.99 }
        functional_scenarios = $functionalScores
        runner_limit_exceeded_cases = $limitExceededCaseIds
        unscored_fields = [ordered]@{
            accuracy = @($accuracy | Where-Object { $null -eq $_ }).Count
            groundedness = @($groundedness | Where-Object { $null -eq $_ }).Count
            closed_case = @($closed | Where-Object { $null -eq $_ }).Count
        }
    }
}

function New-ScoreReport {
    param([Parameter(Mandatory)]$Manifest, [object[]]$Cases, [object[]]$MemoryX, [object[]]$Rag, [object[]]$MemoryXRepeat = @(), [object[]]$RagRepeat = @())
    $memoryXScore = Get-SystemScore -Results $MemoryX -Cases $Cases -SystemName 'memoryx' -RepeatResults $MemoryXRepeat
    $ragScore = Get-SystemScore -Results $Rag -Cases $Cases -SystemName 'rag' -RepeatResults $RagRepeat
    return [ordered]@{
        schema_version = 'memoryx-rag-benchmark-score-v1'
        generated_at_utc = [DateTime]::UtcNow.ToString('o')
        run_id = $Manifest.run_id
        corpus_sha256 = $Manifest.corpus_sha256
        cases_sha256 = $Manifest.cases_sha256
        case_count = $Cases.Count
        comparability = [ordered]@{ same_corpus = $true; same_queries = $true; same_limits = $true; note = 'Validated against the same frozen run manifest; this does not establish statistical significance.' }
        systems = @($memoryXScore, $ragScore)
        publication_guard = 'Scores with zero judged samples are null, not zero. This report contains no superiority claim.'
    }
}

function Format-Rate {
    param($Rate)
    if ($null -eq $Rate.value) { return "unscored (0/$($Rate.total))" }
    return "{0:P2} ({1}/{2})" -f [double]$Rate.value, $Rate.scored, $Rate.total
}

function Write-MarkdownReport {
    param([Parameter(Mandatory)]$Score, [Parameter(Mandatory)][string]$Path)
    $lines = [System.Collections.Generic.List[string]]::new()
    $lines.Add('# MemoryX and RAG Benchmark Report')
    $lines.Add('')
    $lines.Add('This is a computed report from the listed raw JSONL files. It makes no claim of superiority. Null metrics are unscored, not failures or zeroes.')
    $lines.Add('')
    $lines.Add("- Run: ``$($Score.run_id)``")
    $lines.Add("- Corpus SHA-256: ``$($Score.corpus_sha256)``")
    $lines.Add("- Cases SHA-256: ``$($Score.cases_sha256)``")
    $lines.Add("- Cases: $($Score.case_count)")
    $lines.Add('')
    $lines.Add('## Aggregate Metrics')
    $lines.Add('')
    $lines.Add('| System | Accuracy | Recall | Groundedness | Closed-case rate | E2E p50/p95/p99 (ms) |')
    $lines.Add('| --- | --- | --- | --- | --- | --- |')
    foreach ($systemScore in $Score.systems) {
        $latency = $systemScore.end_to_end_latency_ms
        $latencyText = if ($latency.samples -eq 0) { 'unscored' } else { "$($latency.p50) / $($latency.p95) / $($latency.p99)" }
        $lines.Add("| $($systemScore.system) | $(Format-Rate $systemScore.accuracy) | $(Format-Rate $systemScore.recall) | $(Format-Rate $systemScore.groundedness) | $(Format-Rate $systemScore.closed_case_rate) | $latencyText |")
    }
    $lines.Add('')
    $lines.Add('## Functional Scenarios')
    $lines.Add('')
    $lines.Add('| System | Conflicts | Temporal supersession | Multi-hop | Constraints | Missing evidence | Provenance | Contexts | Reproducibility |')
    $lines.Add('| --- | --- | --- | --- | --- | --- | --- | --- |')
    foreach ($systemScore in $Score.systems) {
        $values = foreach ($scenario in $ScenarioKeys) { Format-Rate $systemScore.functional_scenarios.$scenario }
        $lines.Add("| $($systemScore.system) | $($values -join ' | ') |")
    }
    $lines.Add('')
    $lines.Add('## Publication Checks')
    $lines.Add('')
    $lines.Add('- Review all raw answers, evidence identifiers, and human judgements before publishing.')
    $lines.Add('- A closed case requires a human judgement; it is not inferred from answer text or status.')
    $lines.Add('- Reproducibility requires repeated runs with an adapter-provided logical output hash; this runner does not infer it from one run.')
    $lines.Add('- Runner time includes adapter process startup and callback work; record hardware and adapter versions alongside the raw results.')
    [System.IO.File]::WriteAllLines($Path, [string[]]$lines, [System.Text.UTF8Encoding]::new($false))
}

function Invoke-SelfTest {
    $inputs = Get-FrozenInputs
    $temp = Join-Path ([System.IO.Path]::GetTempPath()) ("memoryx-rag-benchmark-selftest-" + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Force -Path $temp | Out-Null
    try {
        $manifest = New-RunManifest $inputs $temp
        $records = foreach ($systemName in @('memoryx', 'rag')) {
            foreach ($case in $inputs.Cases) {
                [pscustomobject]@{
                    schema_version = 'memoryx-rag-benchmark-result-v1'; run_id = $manifest.run_id; system = $systemName; case_id = $case.id; suite = $case.suite; query = $case.query
                    corpus_sha256 = $manifest.corpus_sha256; cases_sha256 = $manifest.cases_sha256; limits = $case.limits; status = 'unscored'; answer = $null
                    retrieved_evidence_ids = @(); provenance_evidence_ids = @(); supporting_claim_ids = @(); logical_output_hash = $null
                    observations = [pscustomobject]@{ conflict_visible = $null; temporal_current = $null; multi_hop_complete = $null; constraints_respected = $null; missing_evidence_safe = $null; provenance_complete = $null; context_isolated = $null; reproducible = $null }
                    judgement = [pscustomobject]@{ accuracy = $null; factual_statements = $null; grounded_factual_statements = $null; closed_case = $null; notes = $null }
                    latency_ms = [pscustomobject]@{ end_to_end = 1.0 }; runner_limit_exceeded = $false; adapter_metadata = $null; error = $null
                }
            }
        }
        $mx = @($records | Where-Object { $_.system -eq 'memoryx' })
        $rag = @($records | Where-Object { $_.system -eq 'rag' })
        Assert-ResultSet $mx $manifest $inputs.Cases $inputs.Corpus 'memoryx'
        Assert-ResultSet $rag $manifest $inputs.Cases $inputs.Corpus 'rag'
        $score = New-ScoreReport $manifest $inputs.Cases $mx $rag
        if ($score.systems[0].accuracy.value -ne $null -or $score.systems[0].groundedness.value -ne $null -or $score.systems[0].closed_case_rate.value -ne $null) { throw 'Selftest incorrectly scored null fixture values.' }
        if ($score.systems[0].accuracy.total -ne $inputs.Cases.Count -or $score.systems[0].accuracy.scored -ne 0) { throw 'Selftest lost unscored accuracy cases.' }
        if ($score.systems[0].recall.total -ne $inputs.Cases.Count -or $score.systems[0].recall.scored -ne ($inputs.Cases.Count - 1)) { throw 'Selftest calculated an incorrect Recall denominator.' }
        if ($score.systems[0].groundedness.total -ne $inputs.Cases.Count -or $score.systems[0].groundedness.scored -ne 0) { throw 'Selftest lost unscored Groundedness cases.' }
        $closureCaseCount = @($inputs.Cases | Where-Object { $_.human_closure_expected }).Count
        if ($score.systems[0].closed_case_rate.total -ne $closureCaseCount -or $score.systems[0].closed_case_rate.scored -ne 0) { throw 'Selftest calculated an incorrect closed-case denominator.' }
        $reportPath = Join-Path $temp 'report.md'; Write-MarkdownReport $score $reportPath
        if (-not (Test-Path -LiteralPath $reportPath)) { throw 'Selftest did not create Markdown report.' }
        Write-Host 'Selftest passed: schema, input validation, paired result validation, null handling, aggregate generation, and Markdown generation.'
    }
    finally { Remove-Item -LiteralPath $temp -Recurse -Force -ErrorAction SilentlyContinue }
}

switch ($Mode) {
    'prepare' {
        $inputs = Get-FrozenInputs
        if ([string]::IsNullOrWhiteSpace($RunDirectory)) { $RunDirectory = Join-Path $BenchmarkRoot (Join-Path 'runs' (Get-Date -Format 'yyyyMMdd-HHmmss')) }
        New-Item -ItemType Directory -Force -Path $RunDirectory | Out-Null
        $manifest = New-RunManifest $inputs $RunDirectory
        Write-Host "Prepared frozen run '$($manifest.run_id)' at $RunDirectory"
    }
    'validate' { Get-FrozenInputs | Out-Null; Write-Host 'Corpus, cases, and JSON Schema validation passed.' }
    'record' {
        if ([string]::IsNullOrWhiteSpace($System)) { throw '-System memoryx or -System rag is required for record.' }
        if ([string]::IsNullOrWhiteSpace($AdapterCommand)) { throw '-AdapterCommand is required for record. See docs/BENCHMARK_RAG_COMPARISON.md.' }
        if ([string]::IsNullOrWhiteSpace($RunDirectory)) { throw '-RunDirectory is required for record.' }
        $inputs = Get-FrozenInputs; $manifest = Read-RunManifest $RunDirectory
        if ($manifest.corpus_sha256 -ne $inputs.CorpusHash -or $manifest.cases_sha256 -ne $inputs.CasesHash) { throw 'Frozen input files changed after prepare. Create a new run directory.' }
        $inputDir = Join-Path $RunDirectory "adapter-inputs/$System"; $outputDir = Join-Path $RunDirectory "adapter-outputs/$System"; $resultDir = Join-Path $RunDirectory 'results'
        New-Item -ItemType Directory -Force -Path $inputDir, $outputDir, $resultDir | Out-Null
        $records = foreach ($case in $inputs.Cases) {
            $invocation = Invoke-Adapter $case $manifest (Join-Path $inputDir "$($case.id).json") (Join-Path $outputDir "$($case.id).json")
            New-ResultRecord $case $manifest $invocation.Output $invocation.ElapsedMs
        }
        $path = Join-Path $resultDir "$System.jsonl"; Write-Jsonl $records $path
        Write-Host "Recorded $($records.Count) $System results at $path"
    }
    'score' {
        if ([string]::IsNullOrWhiteSpace($RunDirectory)) { throw '-RunDirectory is required for score.' }
        $inputs = Get-FrozenInputs; $manifest = Read-RunManifest $RunDirectory
        if ([string]::IsNullOrWhiteSpace($MemoryXResults)) { $MemoryXResults = Join-Path $RunDirectory 'results/memoryx.jsonl' }
        if ([string]::IsNullOrWhiteSpace($RagResults)) { $RagResults = Join-Path $RunDirectory 'results/rag.jsonl' }
        $memoryX = Get-Jsonl $MemoryXResults; $rag = Get-Jsonl $RagResults
        Assert-ResultSet $memoryX $manifest $inputs.Cases $inputs.Corpus 'memoryx'; Assert-ResultSet $rag $manifest $inputs.Cases $inputs.Corpus 'rag'
        $memoryXRepeat = @(); $ragRepeat = @()
        if (-not [string]::IsNullOrWhiteSpace($MemoryXRepeatResults)) { $memoryXRepeat = Get-Jsonl $MemoryXRepeatResults; Assert-RepeatResultSet $memoryXRepeat $manifest $inputs.Cases 'memoryx' }
        if (-not [string]::IsNullOrWhiteSpace($RagRepeatResults)) { $ragRepeat = Get-Jsonl $RagRepeatResults; Assert-RepeatResultSet $ragRepeat $manifest $inputs.Cases 'rag' }
        $score = New-ScoreReport $manifest $inputs.Cases $memoryX $rag $memoryXRepeat $ragRepeat
        if ([string]::IsNullOrWhiteSpace($OutputDirectory)) { $OutputDirectory = Join-Path $RunDirectory 'reports' }
        $jsonPath = Join-Path $OutputDirectory 'score.json'; Write-Json $score $jsonPath
        Write-Host "Wrote aggregate score report at $jsonPath"
    }
    'report' {
        if ([string]::IsNullOrWhiteSpace($ScoreFile)) {
            if ([string]::IsNullOrWhiteSpace($RunDirectory)) { throw '-ScoreFile or -RunDirectory is required for report.' }
            $ScoreFile = Join-Path $RunDirectory 'reports/score.json'
        }
        if ([string]::IsNullOrWhiteSpace($OutputDirectory)) { $OutputDirectory = Split-Path -Parent $ScoreFile }
        $score = Get-Content -Raw -LiteralPath $ScoreFile | ConvertFrom-Json -Depth 100
        $markdownPath = Join-Path $OutputDirectory 'presentation-report.md'; Write-MarkdownReport $score $markdownPath
        Write-Host "Wrote Markdown presentation report at $markdownPath"
    }
    'selftest' { Invoke-SelfTest }
}
