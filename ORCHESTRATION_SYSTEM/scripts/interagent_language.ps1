$script:MemoryXInterAgentLanguageMarker = 'Inter-agent language: English only.'
$script:MemoryXAllowedNonAsciiTechnicalLiterals = @('Concept/Расширение.txt')

function Read-MemoryXStrictUtf8Text {
    param([Parameter(Mandatory)][string]$Path)

    $encoding = [Text.UTF8Encoding]::new($false, $true)
    return [IO.File]::ReadAllText($Path, $encoding)
}

function Test-MemoryXEnglishInterAgentText {
    param(
        [Parameter(Mandatory)][AllowEmptyString()][string]$Text,
        [Parameter(Mandatory)][string]$Label
    )

    $candidate = $Text
    foreach ($literal in $script:MemoryXAllowedNonAsciiTechnicalLiterals) {
        $candidate = $candidate.Replace($literal, '')
    }

    $violations = [Collections.Generic.List[string]]::new()
    for ($offset = 0; $offset -lt $candidate.Length; $offset++) {
        $character = $candidate[$offset]
        if ([int]$character -le 0x7f) {
            continue
        }
        $category = [Globalization.CharUnicodeInfo]::GetUnicodeCategory($character)
        if ($category -in @(
            [Globalization.UnicodeCategory]::UppercaseLetter,
            [Globalization.UnicodeCategory]::LowercaseLetter,
            [Globalization.UnicodeCategory]::TitlecaseLetter,
            [Globalization.UnicodeCategory]::ModifierLetter,
            [Globalization.UnicodeCategory]::OtherLetter,
            [Globalization.UnicodeCategory]::NonSpacingMark,
            [Globalization.UnicodeCategory]::SpacingCombiningMark,
            [Globalization.UnicodeCategory]::EnclosingMark
        )) {
            $codePoint = ([int]$character).ToString('X4')
            $violations.Add("$Label contains disallowed U+$codePoint at offset $offset.")
            if ($violations.Count -ge 8) {
                break
            }
        }
    }

    [pscustomobject]@{
        passed = ($violations.Count -eq 0)
        label = $Label
        violations = @($violations)
    }
}

function Assert-MemoryXEnglishInterAgentText {
    param(
        [Parameter(Mandatory)][AllowEmptyString()][string]$Text,
        [Parameter(Mandatory)][string]$Label
    )

    $result = Test-MemoryXEnglishInterAgentText -Text $Text -Label $Label
    if (-not $result.passed) {
        throw "English-only inter-agent contract violation: $($result.violations -join ' ')"
    }
}

function Get-MemoryXInterAgentCommunicationFiles {
    param(
        [Parameter(Mandatory)][string]$ContourRoot,
        [Parameter(Mandatory)][string]$SystemRoot
    )

    $files = [Collections.Generic.List[string]]::new()
    $directNames = @(
        'CANONICAL_PACKET.md',
        'TASK.md',
        'PLAN.md',
        'PROGRESS.md',
        'DECISIONS.md',
        'ACCEPTANCE.md',
        'COMPACT_CONTEXT.md'
    )
    if ([IO.Path]::GetFullPath($ContourRoot) -eq [IO.Path]::GetFullPath($SystemRoot)) {
        $directNames += @('ARCHITECTURE.md', 'README.md', 'INTER_AGENT_COMMUNICATION.md')
    }

    foreach ($name in $directNames) {
        $path = Join-Path $ContourRoot $name
        if (Test-Path -LiteralPath $path -PathType Leaf) {
            $files.Add((Resolve-Path -LiteralPath $path).Path)
        }
    }

    foreach ($directoryName in @('DOSSIERS', 'evidence', 'hooks', 'state')) {
        $directory = Join-Path $ContourRoot $directoryName
        if (-not (Test-Path -LiteralPath $directory -PathType Container)) {
            continue
        }
        Get-ChildItem -LiteralPath $directory -Recurse -File | Where-Object {
            $_.Extension.ToLowerInvariant() -in @('.md', '.json', '.jsonl', '.txt', '.ps1')
        } | ForEach-Object {
            $files.Add($_.FullName)
        }
    }

    return @($files | Sort-Object -Unique)
}

function Test-MemoryXContourEnglishContract {
    param(
        [Parameter(Mandatory)][string]$ContourRoot,
        [Parameter(Mandatory)][string]$SystemRoot
    )

    $violations = [Collections.Generic.List[string]]::new()
    $canonicalPacketPath = Join-Path $ContourRoot 'CANONICAL_PACKET.md'
    if (-not (Test-Path -LiteralPath $canonicalPacketPath -PathType Leaf)) {
        $violations.Add("Missing canonical packet: $canonicalPacketPath")
    } else {
        $canonicalText = Read-MemoryXStrictUtf8Text -Path $canonicalPacketPath
        if (-not $canonicalText.Contains($script:MemoryXInterAgentLanguageMarker)) {
            $violations.Add("Canonical packet lacks the exact English-only marker: $canonicalPacketPath")
        }
    }

    $files = Get-MemoryXInterAgentCommunicationFiles -ContourRoot $ContourRoot -SystemRoot $SystemRoot
    foreach ($path in $files) {
        try {
            $text = Read-MemoryXStrictUtf8Text -Path $path
            $result = Test-MemoryXEnglishInterAgentText -Text $text -Label $path
            foreach ($violation in $result.violations) {
                $violations.Add($violation)
            }
        } catch {
            $violations.Add("Unable to validate UTF-8 communication surface ${path}: $($_.Exception.Message)")
        }
    }

    [pscustomobject]@{
        passed = ($violations.Count -eq 0)
        contour_root = $ContourRoot
        files_checked = $files.Count
        violations = @($violations)
    }
}
