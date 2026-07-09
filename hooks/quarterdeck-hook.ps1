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

function Write-JsonAtomic {
    # Atomic write of a compact JSON string to <dir>/<id>.json (temp + rename).
    param([string]$Dir, [string]$Id, [string]$Json)
    if (-not (Test-Path -LiteralPath $Dir)) {
        New-Item -ItemType Directory -Force -Path $Dir | Out-Null
    }
    $final = Join-Path $Dir ($Id + '.json')
    $tmp = $final + '.tmp'
    $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText($tmp, $Json, $utf8NoBom)
    [System.IO.File]::Move($tmp, $final)
}

function ConvertTo-JsonStringLiteral {
    # Minimal JSON string escaping for values we hand-build into decision output.
    param([string]$Value)
    if ($null -eq $Value) { return '' }
    $sb = New-Object System.Text.StringBuilder
    foreach ($ch in $Value.ToCharArray()) {
        switch ($ch) {
            '"'  { [void]$sb.Append('\"') }
            '\'  { [void]$sb.Append('\\') }
            "`b" { [void]$sb.Append('\b') }
            "`f" { [void]$sb.Append('\f') }
            "`n" { [void]$sb.Append('\n') }
            "`r" { [void]$sb.Append('\r') }
            "`t" { [void]$sb.Append('\t') }
            default {
                if ([int]$ch -lt 32) {
                    [void]$sb.Append(('\u{0:x4}' -f [int]$ch))
                } else {
                    [void]$sb.Append($ch)
                }
            }
        }
    }
    return $sb.ToString()
}

function Invoke-QuarterdeckPerm {
    # SPEC §16 (R-16.1): the PermissionRequest hook. Write a perm file to
    # <data>/perms/, poll <data>/perm-answers/<id>.json until answered or the
    # deadline, and emit the documented decision JSON on stdout ONLY for an
    # allow/deny answer. Any other outcome (defer / no answer / deck down / any
    # error) exits 0 with NO output — the hook MUST be fail-open so Claude Code
    # falls through to its own terminal dialog.
    param($Payload, [string]$DataDir, [string]$ReceivedAt)

    $now = [DateTimeOffset]::UtcNow
    $id = ('{0}-{1}-{2}' -f `
            $now.UtcDateTime.ToString('yyyyMMddTHHmmssfff', [System.Globalization.CultureInfo]::InvariantCulture), `
            $PID, `
        ([Guid]::NewGuid().ToString('N').Substring(0, 8)))

    $toolName = ''
    if ($Payload.PSObject.Properties.Name -contains 'tool_name') {
        $toolName = [string]$Payload.tool_name
    }
    # tool_input serialized to compact JSON, then truncated to 2KB (R-16.1 cap).
    # Compact (-Compress) packs far more real content under the 2KB cap than the
    # indented form did, so the truncation now rarely lands mid-JSON; the deck
    # re-indents the parsed blob for the modal (R-16.2 §28). A blob that still
    # overflows is truncated and kept verbatim deck-side (never re-structured).
    $toolInput = ''
    if ($Payload.PSObject.Properties.Name -contains 'tool_input') {
        try { $toolInput = $Payload.tool_input | ConvertTo-Json -Depth 20 -Compress } catch { $toolInput = '' }
    }
    if ($null -ne $toolInput -and $toolInput.Length -gt 2048) {
        $toolInput = $toolInput.Substring(0, 2048)
    }
    $sessionId = $null
    if ($Payload.PSObject.Properties.Name -contains 'session_id') { $sessionId = [string]$Payload.session_id }
    $cwd = $null
    if ($Payload.PSObject.Properties.Name -contains 'cwd') { $cwd = [string]$Payload.cwd }

    $perm = [ordered]@{
        v          = 1
        kind       = 'perm'
        tool_name  = $toolName
        tool_input = $toolInput
        session_id = $sessionId
        cwd        = $cwd
        receivedAt = $ReceivedAt
    }
    Write-JsonAtomic -Dir (Join-Path $DataDir 'perms') -Id $id -Json ($perm | ConvertTo-Json -Depth 10 -Compress)

    # Poll for a decision. 85s default so the hook always finishes before its
    # 90s Claude Code timeout (R-16.1); a short deadline via env for the timeout
    # test.
    $answerFile = Join-Path (Join-Path $DataDir 'perm-answers') ($id + '.json')
    $deadlineMs = 85000
    if (-not [string]::IsNullOrWhiteSpace($env:QUARTERDECK_PERM_POLL_DEADLINE_MS)) {
        try { $deadlineMs = [int]$env:QUARTERDECK_PERM_POLL_DEADLINE_MS } catch { $deadlineMs = 85000 }
    }
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    while ($sw.ElapsedMilliseconds -lt $deadlineMs) {
        if (Test-Path -LiteralPath $answerFile) {
            $decision = $null
            $reason = $null
            try {
                $ans = (Get-Content -Raw -LiteralPath $answerFile -ErrorAction Stop) | ConvertFrom-Json -ErrorAction Stop
                if ($ans.PSObject.Properties.Name -contains 'decision') { $decision = [string]$ans.decision }
                if ($ans.PSObject.Properties.Name -contains 'reason') { $reason = [string]$ans.reason }
            } catch {
                $decision = $null
            }
            Remove-Item -LiteralPath $answerFile -Force -ErrorAction SilentlyContinue

            if ($decision -eq 'allow') {
                Write-Output '{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}'
            }
            elseif ($decision -eq 'deny') {
                if ([string]::IsNullOrEmpty($reason)) {
                    Write-Output '{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny"}}}'
                }
                else {
                    $esc = ConvertTo-JsonStringLiteral $reason
                    Write-Output ('{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny","reason":"' + $esc + '"}}}')
                }
            }
            # else: defer / unknown / parse failure -> no output (fail-open).
            return
        }
        Start-Sleep -Milliseconds 250
    }
    # Deadline reached with no answer -> no output (fail-open).
}

function Get-SessionStartExtra {
    # Walk THIS process's parent chain ONCE (a single Win32_Process snapshot) and
    # resolve, from the same walk:
    #   * claudePid  = nearest ancestor whose exe is claude/node/bun (R-4.3),
    #   * ancestor   = nearest ancestor owning a real top-level window
    #                  (MainWindowHandle != 0), as {pid, hwnd, exe} (R-15.4a) —
    #                  the terminal window a row click should focus.
    # Returns an ordered hashtable { claudePid; ancestor } (ancestor may be $null).
    $result = [ordered]@{ claudePid = $null; ancestor = $null }
    try {
        $procs = @{}
        Get-CimInstance -ClassName Win32_Process -ErrorAction Stop |
            ForEach-Object { $procs[[int]$_.ProcessId] = $_ }

        # Window handles by pid (Get-Process, best-effort — a process may be gone
        # or inaccessible; those simply have no window entry).
        $winByPid = @{}
        Get-Process -ErrorAction SilentlyContinue | ForEach-Object {
            try { $winByPid[[int]$_.Id] = $_ } catch { }
        }

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
            if ($null -eq $result.claudePid -and
                ($stem -eq 'claude' -or $stem -eq 'node' -or $stem -eq 'bun')) {
                $result.claudePid = $parentId
            }

            if ($null -eq $result.ancestor) {
                $win = $winByPid[$parentId]
                if ($null -ne $win) {
                    $hwnd = 0
                    try { $hwnd = [int64]$win.MainWindowHandle } catch { $hwnd = 0 }
                    if ($hwnd -ne 0) {
                        $result.ancestor = [ordered]@{
                            pid  = $parentId
                            hwnd = $hwnd
                            exe  = [string]$parent.Name
                        }
                    }
                }
            }

            if ($null -ne $result.claudePid -and $null -ne $result.ancestor) { break }
            $walk = $parentId
        }
    } catch {
    }
    return $result
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

    # --- PermissionRequest (SPEC §16): the deck-side take-over path. It writes a
    #     perm file, polls for a decision, and emits the allow/deny JSON on
    #     stdout (or nothing) — NOT the normal spool envelope. Fail-open always. ---
    if ($eventName -eq 'PermissionRequest') {
        Invoke-QuarterdeckPerm -Payload $payload -DataDir $dataDir -ReceivedAt $receivedAt
        exit 0
    }

    # --- extra (claudePid + terminal ancestor, only on SessionStart) ---
    $extra = [ordered]@{}
    if ($eventName -eq 'SessionStart') {
        $ss = Get-SessionStartExtra
        $extra['claudePid'] = $ss.claudePid
        if ($null -ne $ss.ancestor) {
            $extra['ancestor'] = $ss.ancestor
        }
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
