#!/usr/bin/env bash
# Quarterdeck hook (macOS/Linux).
#
# Reads a Claude Code hook event JSON from stdin, wraps it in the spool
# envelope { v, event, receivedAt, payload, extra } and atomically writes it to
# <data>/spool/<id>.json.
#
# Contract (SPEC R-4.3):
#   * always exit 0 (a non-zero Stop hook would block the conversation),
#   * silent on stdout/stderr, swallow every error,
#   * garbage / empty stdin writes nothing,
#   * on SessionStart, extra.claudePid = nearest ancestor process whose command
#     matches claude|node|bun (walk the parent chain via `ps`),
#   * <=2 s typical.
#
# Data dir = $QUARTERDECK_DATA_DIR, else ~/Library/Application Support/quarterdeck.

# Never abort: we always exit 0.
set +e

# Validate JSON with whatever parser is available. A parser is only trusted if
# it round-trips a trivial document first, so a broken interpreter stub (e.g. a
# Windows Store python3 alias) cannot make us drop valid events. Order:
# python3, jq, then perl+JSON::PP (a core module since Perl 5.14, so this covers
# a stock macOS — a spec target — where python3 is a gated stub and jq is
# absent). If NONE is a working parser we fail CLOSED (return non-zero -> write
# nothing) rather than spool brace-wrapped garbage: R-4.3's per-script contract
# is "garbage stdin -> write nothing", which the cheap `{ ... }` brace guard
# alone cannot guarantee.
json_is_valid() {
    if command -v python3 >/dev/null 2>&1 &&
        printf '{}' | python3 -c 'import sys,json; json.load(sys.stdin)' >/dev/null 2>&1; then
        printf '%s' "$1" | python3 -c 'import sys,json; json.load(sys.stdin)' >/dev/null 2>&1
        return $?
    fi
    if command -v jq >/dev/null 2>&1 &&
        printf '{}' | jq -e . >/dev/null 2>&1; then
        printf '%s' "$1" | jq -e . >/dev/null 2>&1
        return $?
    fi
    if command -v perl >/dev/null 2>&1 &&
        printf '{}' | perl -MJSON::PP -e 'decode_json(do { local $/; <STDIN> })' >/dev/null 2>&1; then
        printf '%s' "$1" | perl -MJSON::PP -e 'decode_json(do { local $/; <STDIN> })' >/dev/null 2>&1
        return $?
    fi
    return 1
}

# UTC ISO-8601; millisecond precision on GNU date, second precision on BSD/macOS.
iso_now() {
    ns="$(date -u +%N 2>/dev/null)"
    case "$ns" in
        '' | *[!0-9]*) date -u +%Y-%m-%dT%H:%M:%SZ ;;
        *) date -u +%Y-%m-%dT%H:%M:%S.%3NZ ;;
    esac
}

# Print the PID of the nearest ancestor whose command is claude/node/bun.
# Prints nothing when none is found.
claude_ancestor_pid() {
    walk="$$"
    i=0
    while [ "$i" -lt 40 ]; do
        i=$((i + 1))
        ppid="$(ps -o ppid= -p "$walk" 2>/dev/null | tr -d ' ')"
        case "$ppid" in
            '' | 0 | 1) return 0 ;;
        esac
        comm="$(ps -o comm= -p "$ppid" 2>/dev/null)"
        base="${comm##*/}"
        case "$base" in
            claude | node | bun)
                printf '%s' "$ppid"
                return 0
                ;;
        esac
        walk="$ppid"
    done
    return 0
}

# SPEC §16 (R-16.1): the PermissionRequest hook. Write a perm file to
# <data>/perms/, poll <data>/perm-answers/<id>.json until answered or the
# deadline, and emit the documented decision JSON on stdout ONLY for an
# allow/deny answer. Any other outcome exits 0 with NO output — fail-open, so
# Claude Code falls through to its own terminal dialog.
#
# tool_name/tool_input/session_id/cwd are extracted with whatever JSON tool is
# available (python3, else jq). Without either we cannot build a faithful perm
# file, so we fail open (return, no output) rather than emit garbage.
invoke_perm() {
    # $1 = data_dir, $2 = received_at, $3 = original payload json
    data_dir="$1"
    recv="$2"
    payload="$3"

    perms_dir="$data_dir/perms"
    answers_dir="$data_dir/perm-answers"
    mkdir -p "$perms_dir" 2>/dev/null || return 0

    id="$(date -u +%Y%m%dT%H%M%S 2>/dev/null)-$$-${RANDOM:-0}${RANDOM:-0}"
    final="$perms_dir/$id.json"
    tmp="$final.tmp"

    # NOTE: the payload arrives on stdin, so the python program is passed via
    # `-c` (a heredoc would itself redirect stdin and shadow the payload pipe).
    py_write='import sys, json
tmp, recv = sys.argv[1], sys.argv[2]
try:
    p = json.load(sys.stdin)
except Exception:
    sys.exit(1)
ti = p.get("tool_input")
# Indented (pretty-printed) JSON per R-16.2, capped to 2KB (R-16.1). Pretty-print
# BEFORE the cap so an over-length input stays indented up to the cut.
ti = json.dumps(ti, indent=2, ensure_ascii=False) if ti is not None else ""
if len(ti) > 2048:
    ti = ti[:2048]
rec = {"v": 1, "kind": "perm", "tool_name": p.get("tool_name") or "", "tool_input": ti, "session_id": p.get("session_id"), "cwd": p.get("cwd"), "receivedAt": recv}
open(tmp, "w", encoding="utf-8").write(json.dumps(rec, separators=(",", ":"), ensure_ascii=False))'
    if command -v python3 >/dev/null 2>&1 &&
        printf '{}' | python3 -c 'import sys,json; json.load(sys.stdin)' >/dev/null 2>&1; then
        if ! printf '%s' "$payload" | python3 -c "$py_write" "$tmp" "$recv" 2>/dev/null; then
            rm -f "$tmp" 2>/dev/null
            return 0
        fi
    elif command -v jq >/dev/null 2>&1 && printf '{}' | jq -e . >/dev/null 2>&1; then
        # jq has no pretty `tojson`, so this last-resort fallback (python3 absent)
        # writes compact tool_input; the deck re-indents it for the modal at
        # display time (pretty_tool_input, R-16.2) whenever it parses.
        if ! printf '%s' "$payload" | jq -c --arg recv "$recv" '{
            v: 1, kind: "perm",
            tool_name: (.tool_name // ""),
            tool_input: ((.tool_input | if . == null then "" else tojson end)[0:2048]),
            session_id: .session_id,
            cwd: .cwd,
            receivedAt: $recv
        }' >"$tmp" 2>/dev/null; then
            rm -f "$tmp" 2>/dev/null
            return 0
        fi
    else
        return 0
    fi

    mv -f "$tmp" "$final" 2>/dev/null || {
        rm -f "$tmp" 2>/dev/null
        return 0
    }

    answer_file="$answers_dir/$id.json"
    deadline_ms=85000
    case "${QUARTERDECK_PERM_POLL_DEADLINE_MS:-}" in
        '' | *[!0-9]*) : ;;
        *) deadline_ms="$QUARTERDECK_PERM_POLL_DEADLINE_MS" ;;
    esac
    # 250ms poll; iterations = deadline / 250.
    iters=$((deadline_ms / 250))
    [ "$iters" -lt 1 ] && iters=1
    i=0
    while [ "$i" -lt "$iters" ]; do
        i=$((i + 1))
        if [ -f "$answer_file" ]; then
            decision=""
            reason=""
            if command -v python3 >/dev/null 2>&1 &&
                printf '{}' | python3 -c 'import sys,json; json.load(sys.stdin)' >/dev/null 2>&1; then
                decision="$(python3 -c 'import sys,json;
try:
    d=json.load(open(sys.argv[1]))
    print(d.get("decision",""))
except Exception:
    pass' "$answer_file" 2>/dev/null)"
                reason="$(python3 -c 'import sys,json;
try:
    d=json.load(open(sys.argv[1]))
    print(d.get("reason","") or "")
except Exception:
    pass' "$answer_file" 2>/dev/null)"
            elif command -v jq >/dev/null 2>&1; then
                decision="$(jq -r '.decision // ""' "$answer_file" 2>/dev/null)"
                reason="$(jq -r '.reason // ""' "$answer_file" 2>/dev/null)"
            fi
            rm -f "$answer_file" 2>/dev/null

            if [ "$decision" = "allow" ]; then
                printf '%s\n' '{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}'
            elif [ "$decision" = "deny" ]; then
                if [ -z "$reason" ]; then
                    printf '%s\n' '{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny"}}}'
                else
                    esc_reason=""
                    if command -v python3 >/dev/null 2>&1; then
                        esc_reason="$(printf '%s' "$reason" | python3 -c 'import sys,json; sys.stdout.write(json.dumps(sys.stdin.read()))' 2>/dev/null)"
                    elif command -v jq >/dev/null 2>&1; then
                        esc_reason="$(printf '%s' "$reason" | jq -Rs . 2>/dev/null)"
                    fi
                    if [ -n "$esc_reason" ]; then
                        printf '%s\n' "{\"hookSpecificOutput\":{\"hookEventName\":\"PermissionRequest\",\"decision\":{\"behavior\":\"deny\",\"reason\":$esc_reason}}}"
                    else
                        printf '%s\n' '{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny"}}}'
                    fi
                fi
            fi
            # defer / unknown / parse failure -> no output (fail-open).
            return 0
        fi
        sleep 0.25
    done
    # Deadline reached with no answer -> no output (fail-open).
    return 0
}

main() {
    input="$(cat)"

    # empty / whitespace-only stdin -> nothing to do
    case "$input" in
        *[![:space:]]*) : ;;
        *) return 0 ;;
    esac

    # must look like a JSON object; cheap guard before the (optional) real parse
    trimmed="$(printf '%s' "$input" | tr -d '[:space:]')"
    case "$trimmed" in
        \{*\}) : ;;
        *) return 0 ;;
    esac
    if ! json_is_valid "$input"; then
        return 0
    fi

    data_dir="${QUARTERDECK_DATA_DIR:-}"
    if [ -z "$data_dir" ]; then
        data_dir="$HOME/Library/Application Support/quarterdeck"
    fi
    spool_dir="$data_dir/spool"
    mkdir -p "$spool_dir" 2>/dev/null || return 0

    # event name: a plain identifier value of "hook_event_name"
    event="$(printf '%s' "$input" |
        sed -n 's/.*"hook_event_name"[[:space:]]*:[[:space:]]*"\([A-Za-z0-9_]*\)".*/\1/p' |
        head -n 1)"

    received_at="$(iso_now)"

    # PermissionRequest (SPEC §16): the deck-side take-over path. Poll for a
    # decision instead of writing the normal spool envelope. Fail-open always.
    if [ "$event" = "PermissionRequest" ]; then
        invoke_perm "$data_dir" "$received_at" "$input"
        return 0
    fi

    # extra: claudePid, only on SessionStart
    if [ "$event" = "SessionStart" ]; then
        cpid="$(claude_ancestor_pid)"
        if [ -n "$cpid" ]; then
            cpid_json="$cpid"
        else
            cpid_json="null"
        fi
        extra="{\"claudePid\":$cpid_json}"
    else
        extra="{}"
    fi

    # envelope: the raw (already-valid) payload is embedded verbatim
    envelope="{\"v\":1,\"event\":\"$event\",\"receivedAt\":\"$received_at\",\"payload\":$input,\"extra\":$extra}"

    # atomic write: temp file in the same dir, then rename
    id="$(date -u +%Y%m%dT%H%M%S 2>/dev/null)-$$-${RANDOM:-0}${RANDOM:-0}"
    final="$spool_dir/$id.json"
    tmp="$final.tmp"
    printf '%s' "$envelope" >"$tmp" 2>/dev/null || return 0
    mv -f "$tmp" "$final" 2>/dev/null || {
        rm -f "$tmp" 2>/dev/null
        return 0
    }
    return 0
}

main
exit 0
