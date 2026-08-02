[CmdletBinding()]
param(
    [ValidateSet('Inspect', 'Acquire', 'Release')]
    [string]$Action = 'Inspect',
    [int]$OwnerPid = $PID,
    [string]$Purpose = 'unspecified',
    [string]$Token,
    [int]$WaitSeconds = 300,
    [int]$StaleAfterSeconds = 3600,
    [int]$MaxCpuLoadPercent = 85,
    [int]$MinFreeMemoryMb = 2048
)

$ErrorActionPreference = 'Stop'
$systemRoot = Split-Path -Parent $PSScriptRoot
$stateRoot = [IO.Path]::GetFullPath((Join-Path $systemRoot 'state'))
$lockPath = [IO.Path]::GetFullPath((Join-Path $stateRoot 'resource-coordination.lock'))
$ownerPath = Join-Path $lockPath 'owner.json'

if (-not $lockPath.StartsWith($stateRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
    throw 'Resource lock path escaped ORCHESTRATION_SYSTEM/state.'
}

function Get-HostObservation {
    $os = Get-CimInstance Win32_OperatingSystem
    $cpu = @(Get-CimInstance Win32_Processor | Measure-Object -Property LoadPercentage -Average).Average
    $freeMb = [math]::Floor([double]$os.FreePhysicalMemory / 1024)
    $heavy = @(Get-Process -Name cargo, rustc -ErrorAction SilentlyContinue | ForEach-Object {
        [ordered]@{ name = $_.ProcessName; pid = $_.Id; cpu_seconds = $_.CPU }
    })
    [ordered]@{
        observed_at_utc = [DateTime]::UtcNow.ToString('o')
        cpu_load_percent = if ($null -eq $cpu) { $null } else { [math]::Round([double]$cpu, 1) }
        free_memory_mb = $freeMb
        cargo_or_rustc_processes = $heavy
    }
}

function Read-Owner {
    if (-not (Test-Path -LiteralPath $ownerPath -PathType Leaf)) {
        return $null
    }
    try {
        return Get-Content -LiteralPath $ownerPath -Raw | ConvertFrom-Json
    } catch {
        return $null
    }
}

function Remove-VerifiedLock {
    if (Test-Path -LiteralPath $lockPath -PathType Container) {
        $resolved = [IO.Path]::GetFullPath($lockPath)
        if ($resolved -ne $lockPath -or -not $resolved.StartsWith($stateRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
            throw 'Refusing to remove an unverified resource lock path.'
        }
        Remove-Item -LiteralPath $resolved -Recurse -Force
    }
}

if ($Action -eq 'Inspect') {
    [ordered]@{
        schema_version = 'memoryx.resource-observation.v1'
        host = Get-HostObservation
        lock = Read-Owner
        statement = 'No process was stopped or modified.'
    } | ConvertTo-Json -Depth 8
    exit 0
}

if ($Action -eq 'Release') {
    $owner = Read-Owner
    if ($null -eq $owner) {
        throw 'No valid resource coordination owner record exists.'
    }
    if ([int]$owner.owner_pid -ne $OwnerPid -or $owner.token -ne $Token) {
        throw 'Resource coordination release denied: owner PID or token mismatch.'
    }
    Remove-VerifiedLock
    [ordered]@{
        schema_version = 'memoryx.resource-release.v1'
        released = $true
        owner_pid = $OwnerPid
        purpose = $owner.purpose
    } | ConvertTo-Json -Depth 5
    exit 0
}

$deadline = [DateTime]::UtcNow.AddSeconds($WaitSeconds)
$acquireToken = [Guid]::NewGuid().ToString('D')
while ([DateTime]::UtcNow -lt $deadline) {
    if (Test-Path -LiteralPath $lockPath -PathType Container) {
        $owner = Read-Owner
        if ($null -ne $owner) {
            $ownerProcess = Get-Process -Id ([int]$owner.owner_pid) -ErrorAction SilentlyContinue
            $age = [DateTime]::UtcNow - [DateTime]::Parse($owner.acquired_at_utc).ToUniversalTime()
            if ($null -eq $ownerProcess -and $age.TotalSeconds -ge $StaleAfterSeconds) {
                Remove-VerifiedLock
                continue
            }
        }
        Start-Sleep -Seconds 2
        continue
    }

    $observation = Get-HostObservation
    $cpuBusy = $null -ne $observation.cpu_load_percent -and $observation.cpu_load_percent -gt $MaxCpuLoadPercent
    $memoryBusy = $observation.free_memory_mb -lt $MinFreeMemoryMb
    if ($cpuBusy -or $memoryBusy) {
        Start-Sleep -Seconds 5
        continue
    }

    try {
        New-Item -ItemType Directory -Path $lockPath -ErrorAction Stop | Out-Null
    } catch {
        Start-Sleep -Milliseconds 500
        continue
    }

    $record = [ordered]@{
        schema_version = 'memoryx.resource-lease.v1'
        token = $acquireToken
        owner_pid = $OwnerPid
        purpose = $Purpose
        acquired_at_utc = [DateTime]::UtcNow.ToString('o')
        observation = $observation
        policy = [ordered]@{
            max_cpu_load_percent = $MaxCpuLoadPercent
            min_free_memory_mb = $MinFreeMemoryMb
            foreign_process_termination_forbidden = $true
        }
    }
    [IO.File]::WriteAllText($ownerPath, ($record | ConvertTo-Json -Depth 8) + [Environment]::NewLine, [Text.UTF8Encoding]::new($false))
    $record | ConvertTo-Json -Depth 8
    exit 0
}

throw "Timed out after $WaitSeconds seconds waiting for shared-host resources."

