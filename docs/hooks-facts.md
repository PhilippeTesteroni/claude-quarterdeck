# Claude Code hooks — verified facts (official docs, checked 2026-07-02)

Sources: code.claude.com/docs/en/hooks.md, hooks-guide.md, sessions.md, env-vars.md, cli-reference.md. Implementation MUST rely only on facts below; anything else → check docs first.

## Events we subscribe to

| Event | Fires | Payload fields we use |
|---|---|---|
| `SessionStart` | session begins/resumes (`source`: startup/resume/clear/compact) | `session_id`, `transcript_path`, `cwd`, `source`, optional `session_title` |
| `UserPromptSubmit` | user submits a prompt, before processing; no matcher support | `session_id`, `cwd`, `prompt` |
| `Notification` | Claude Code sends a notification; matcher filters `notification_type` | `message`, `notification_type` ∈ permission_prompt / idle_prompt / auth_success / elicitation_dialog / elicitation_complete / elicitation_response |
| `Stop` | Claude finishes responding; no matcher support | `stop_hook_active` |
| `SessionEnd` | session terminates | `reason` ∈ clear / resume / logout / prompt_input_exit / bypass_permissions_disabled / other |

All payloads also carry `session_id`, `prompt_id` (v2.1.196+), `transcript_path`, `cwd`, `permission_mode`, `effort`, `hook_event_name`.

## Config schema (settings.json)

```json
{"hooks": {"Notification": [{"matcher": "permission_prompt|idle_prompt|elicitation_dialog",
  "hooks": [{"type": "command", "command": "...", "timeout": 10}]}]}}
```
- Matcher optional; `UserPromptSubmit` and `Stop` CANNOT use matchers (always fire).
- User-level `~/.claude/settings.json` hooks apply to ALL projects. Precedence: user < project < project-local.
- Default command timeout 600 s (we set 10 s). `Stop` hook exit 2 blocks conversation — our scripts must ALWAYS exit 0. `Notification` output is ignored.
- Windows hook shell: **Git Bash if available, else PowerShell** (per-hook `shell` field exists but don't rely on it for old versions) → command lines must be shell-agnostic: absolute paths, forward slashes, `powershell.exe -NoProfile -ExecutionPolicy Bypass -File "C:/…/quarterdeck-hook.ps1"`.
- Env available to hooks: `CLAUDE_PROJECT_DIR` etc. (not needed by us).

## Transcripts & sessions

- `~/.claude/projects/<slug>/<session-id>.jsonl`; slug = cwd with non-alphanumerics → `-`. Entry format is **explicitly internal/unstable** → we only `stat()` transcripts for activity (R-2.2) and do one guarded best-effort parse for cold-start titles.
- No CLI to list sessions programmatically. `CLAUDE_CONFIG_DIR` relocates `~/.claude` (documented minimally) — used for live-smoke isolation.
- Official "is Claude waiting?" mechanism = `Notification` hook with `notification_type` — exactly what we use.

---

# v1.1 addendum — verified facts (official docs, checked 2026-07-03)

Sources: code.claude.com/docs/en/hooks.md (PreToolUse / PermissionRequest contracts), code.claude.com/docs/en/mcp.md and env-vars.md (MCP timeout model). Referenced by SPEC §16 intro and R-19.3. Implementation MUST rely only on facts below; anything else → check docs first.

## PermissionRequest hook (SPEC §16, R-16.1/R-16.2)

Fires when a tool-permission dialog would appear. Supports a tool-name matcher (e.g. `Bash`, `Edit|Write`, `mcp__.*`). Runs BEFORE the terminal permission prompt; we take it over so the deck can answer.

**Input JSON (stdin)** — common fields (`session_id`, `transcript_path`, `cwd`, `hook_event_name`) plus:

```json
{
  "session_id": "abc123",
  "transcript_path": "/path/to/transcript.jsonl",
  "cwd": "/current/working/dir",
  "permission_mode": "default",
  "hook_event_name": "PermissionRequest",
  "tool_name": "Bash",
  "tool_input": { "command": "rm -rf /tmp/build" }
}
```

- `tool_name` = the tool being called; `tool_input` = its arguments (arbitrary nested object). We serialize `tool_input` to a JSON string, pretty-print it, and cap it at 2KB (R-16.1/R-16.2).
- `permission_mode` ∈ `default` / `plan` / `acceptEdits` / `auto` / `dontAsk` / `bypassPermissions` (we don't branch on it in v1.1).

**Output JSON (stdout, exit 0)** — `hookSpecificOutput` carries a `decision` OBJECT (note: PermissionRequest uses `decision`, NOT PreToolUse's flat `permissionDecision` string — see below):

```json
{ "hookSpecificOutput": { "hookEventName": "PermissionRequest",
    "decision": { "behavior": "allow", "updatedInput": { "command": "npm run lint" } } } }
```

- `behavior` ∈ `"allow"` | `"deny"`. `updatedInput` is optional (allow-only; replaces the tool input). We do NOT use `updatedInput` in v1.1 (no "always allow" / input-rewrite — explicit non-goal, R-16.5). Our allow = `{"behavior":"allow"}`; deny = `{"behavior":"deny"}`.
- **Deny reason key — undocumented, so a best-effort forward-compat field, NOT part of the verified contract.** The docs' `decision` object for PermissionRequest defines ONLY `behavior` (+ the allow-only `updatedInput`); there is **no documented reason/message field** for a deny (unlike PreToolUse, whose flat `permissionDecisionReason` string carries a reason — see below). SPEC R-16.1 nonetheless says deny carries "the user's optional reason", and the spec is silent on the key, so our hook attaches it as `decision.reason` (`quarterdeck-hook.ps1` ~L137 / `quarterdeck-hook.sh` ~L199), JSON-escaped. Because current Claude Code does not read a reason field here, it is IGNORED by the CLI today — harmlessly: the `behavior:"deny"` always takes effect regardless (fail-safe; the deny is never lost, only its human-readable reason may not surface). If a future Claude Code adds a reason field under a different name (e.g. `message`), revisit this key; nothing downstream breaks in the meantime.
- **Fail-open (the contract R-16.1 depends on):** exit 0 with **no stdout** = "no decision" → Claude Code continues its normal permission flow (the terminal dialog appears). Only an explicit `behavior:"deny"` (or exit 2 with stderr) blocks. Staying silent never approves. ⇒ our hook MUST exit 0 and print nothing on defer / no-answer / deck-down / any error.

The byte-for-byte live verification covers what the docs pin: the allow shape (`{"behavior":"allow"}`) and the behavior-only deny (`{"behavior":"deny"}`) match exactly. The extra `decision.reason` on a deny-with-reason is our undocumented addition (per the bullet above), NOT a verified doc fact — it is emitted with correct JSON escaping and is inert on the current CLI.

## PreToolUse — why we do NOT use it

PreToolUse fires before every tool call and can block it, but its decision shape is different: `hookSpecificOutput.permissionDecision` is a flat STRING ∈ `"allow"` / `"deny"` / `"ask"` / `"defer"` (+ `permissionDecisionReason`), not a `decision` object. It also runs on the hot path for EVERY tool (latency), whereas PermissionRequest fires only when a prompt would actually appear. SPEC §4/§13 deliberately keeps per-tool events out of scope; §16 uses PermissionRequest specifically because it is the prompt-time event. Same fail-open rule (exit 0 + no output = normal flow).

## MCP tool timeout model (SPEC R-19.2/R-19.3)

For remote HTTP/SSE/WebSocket MCP servers (ours is streamable HTTP, R-8.1):

- **Idle abort — 5 min.** As of Claude Code v2.1.187, a tool call that sends no response AND no progress notification for 5 minutes aborts with an error (instead of waiting for the wall-clock limit). Window is `CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT` (ms); `0` disables it. ⇒ R-19.3: while any `ask_user`/perm call is blocked we emit `notifications/progress` (with the caller's `progressToken` when sent) every 30s to reset this idle window, so persistent asks survive.
- **Wall-clock limit — `MCP_TOOL_TIMEOUT`,** default ≈ 28 h when unset. Progress notifications do NOT extend this hard limit. We leave it unset by default, so a persistent (no-timeout) ask (R-19.2) is bounded only by the ~28 h default, not the 5-min idle abort.
- **Per-server override.** A `timeout` field (ms) in the server's `.mcp.json` entry overrides `MCP_TOOL_TIMEOUT` for that server only; it is a HARD wall-clock cap that progress notifications do not extend, and values < 1000 are ignored. ⇒ R-19.3 skill note: for very long autonomy, set an explicit per-server `timeout` — but understand it is a hard ceiling. The `claude mcp add` command R-8.6 emits carries no `timeout` arg (relies on the ~28 h default).
- Startup: `MCP_TIMEOUT` (server connect timeout) and initial-connect retry (≤3× on transient errors, v2.1.121) — not load-bearing for us.
