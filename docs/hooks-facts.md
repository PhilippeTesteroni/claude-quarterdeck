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
