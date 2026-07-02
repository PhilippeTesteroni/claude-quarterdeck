<#
    Quarterdeck hook (Windows).

    Reads a Claude Code hook event JSON from stdin, wraps it in the spool
    envelope { v, event, receivedAt, payload, extra } and atomically writes it
    to <data>/spool/<id>.json.

    Contract (SPEC R-4.3):
      * always exit 0 (a non-zero Stop hook would block the conversation),
      * silent on stdout/stderr, swallow every error,
      * garbage / empty stdin writes nothing,
      * on SessionStart, extra.claudePid = nearest ancestor process whose name
        matches claude|node|bun (walk the parent chain via CIM),
      * <=2 s typical (a single Win32_Process snapshot, no per-level queries).

    Data dir = $env:QUARTERDECK_DATA_DIR, else %APPDATA%\quarterdeck (SPEC R-3.3).
#>

$ErrorActionPreference = 'Stop'

function Get-ClaudeAncestorPid {
    # Nearest ancestor of THIS process whose executable is claude/node/bun.
    # Returns an int PID, or $null if none is found.
    try {
        $procs = @{}
        Get-CimInstance -ClassName Win32_Process -ErrorAction Stop |
            ForEach-Object { $procs[[int]$_.ProcessId] = $_ }

        $walk = $PID
        for ($i = 0; $i -lt 40; $i++) {
            $proc = $procs[[int]$walk]
            if ($null -eq $proc) { break }
            $parentId = [int]$proc.ParentProcessId
            if ($parentId -le 0 -or $parentId -eq $walk) { break }
            $parent = $procs[$parentId]
            if ($null -eq $parent) { break }

            $stem = ([string]$parent.Name) -replace '\.exe$', ''
            $stem = $stem.ToLowerInvariant()
            if ($stem -eq 'claude' -or $stem -eq 'node' -or $stem -eq 'bun') {
                return $parentId
            }
            $walk = $parentId
        }
    } catch {
    }
    return $null
}

try {
    # --- read stdin fully as UTF-8 (Claude Code writes UTF-8; the console's
    #     default input encoding would corrupt Cyrillic/emoji payloads) ---
    $raw = $null
    try {
        $stdin = [Console]::OpenStandardInput()
        $reader = New-Object System.IO.StreamReader(
            $stdin, (New-Object System.Text.UTF8Encoding($false)), $true)
        $raw = $reader.ReadToEnd()
        $reader.Dispose()
    } catch {
        $raw = [Console]::In.ReadToEnd()
    }
    if ([string]::IsNullOrWhiteSpace($raw)) { exit 0 }

    # --- parse; garbage / non-object -> write nothing ---
    $payload = $null
    try {
        $payload = $raw | ConvertFrom-Json -ErrorAction Stop
    } catch {
        exit 0
    }
    if ($null -eq $payload -or $payload -isnot [System.Management.Automation.PSCustomObject]) {
        exit 0
    }

    # --- resolve data dir + spool dir ---
    $dataDir = $env:QUARTERDECK_DATA_DIR
    if ([string]::IsNullOrWhiteSpace($dataDir)) {
        $dataDir = Join-Path $env:APPDATA 'quarterdeck'
    }
    $spoolDir = Join-Path $dataDir 'spool'
    if (-not (Test-Path -LiteralPath $spoolDir)) {
        New-Item -ItemType Directory -Force -Path $spoolDir | Out-Null
    }

    # --- event name + timestamp ---
    $eventName = $null
    if ($payload.PSObject.Properties.Name -contains 'hook_event_name') {
        $eventName = [string]$payload.hook_event_name
    }
    $now = [DateTimeOffset]::UtcNow
    $receivedAt = $now.ToString(
        'yyyy-MM-dd\THH:mm:ss.fff\Z',
        [System.Globalization.CultureInfo]::InvariantCulture)

    # --- extra (claudePid only on SessionStart) ---
    $extra = [ordered]@{}
    if ($eventName -eq 'SessionStart') {
        $extra['claudePid'] = Get-ClaudeAncestorPid
    }

    # --- envelope ---
    $envelope = [ordered]@{
        v          = 1
        event      = $eventName
        receivedAt = $receivedAt
        payload    = $payload
        extra      = $extra
    }
    $json = $envelope | ConvertTo-Json -Depth 30 -Compress

    # --- atomic write: temp file in the same dir, then rename ---
    $id = ('{0}-{1}-{2}' -f `
        $now.UtcDateTime.ToString('yyyyMMddTHHmmssfff', [System.Globalization.CultureInfo]::InvariantCulture), `
        $PID, `
        ([Guid]::NewGuid().ToString('N').Substring(0, 8)))
    $final = Join-Path $spoolDir ($id + '.json')
    $tmp = $final + '.tmp'

    $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText($tmp, $json, $utf8NoBom)
    [System.IO.File]::Move($tmp, $final)
} catch {
    # swallow everything: a hook must never disrupt Claude Code
}

exit 0
