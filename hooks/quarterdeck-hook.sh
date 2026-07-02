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
# Windows Store python3 alias) cannot make us drop valid events. When no working
# parser is present we fail open: the caller's `{ ... }` brace guard already ran,
# and the deck quarantines any malformed spool file it later reads (R-3.5).
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
    return 0
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
            claude | claude-* | node | node-* | bun | bun-*)
                printf '%s' "$ppid"
                return 0
                ;;
        esac
        walk="$ppid"
    done
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

    # extra: claudePid only on SessionStart
    if [ "$event" = "SessionStart" ]; then
        cpid="$(claude_ancestor_pid)"
        if [ -n "$cpid" ]; then
            extra="{\"claudePid\":$cpid}"
        else
            extra="{\"claudePid\":null}"
        fi
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
