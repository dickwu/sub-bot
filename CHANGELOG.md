# Changelog

## [0.10.2] - 2026-11-03

### Fixed
- **One HTTP request reported as two issues**: an unauthenticated request (401) wrote the same issue id twice to `issues.log` — first `count: 1`, a few ms later `count: 2` — and a dedup summary was delivered at the end of the window. The `auth` middleware's WARN `unauthorized request …` and the per-request access log WARN `request finished status=401` both became `log_warn` issues from `sub_bot::http` with the same dedup key, so the access log was counted as a repeat of the auth failure (any other 4xx in the same window was folded in too). The access log no longer acts as an issue source on its own: 4xx `request finished` lines are marked `issue = "none"` (client errors; the caller already gets the reason, and the cases that matter — 401, 409 session rehydration failure, 429 `session_limit` — have their own issue), and 5xx lines are marked `issue = "fallback"`: they are reported only when nothing more specific was reported for that request (e.g. `session rehydration failed`, `session purge failed`, `list models failed`, a typed `run_timeout` on 504). One 401 now yields exactly one issue with `count: 1` and no summary.
- **MCP tool call timeout reported twice**: the `MCP tool call timed out` WARN became a `log_warn` issue in addition to the session's `tool_timeout` issue for the same failure; it is now a typed (diagnostic-only) log line.
- **Issue redaction masked plain words**: the `Bearer <value>` pattern matched ordinary text, turning `unauthorized request (missing or unknown bearer token)` into `… missing or unknown [redacted]`. `Bearer` values are now masked only when they look like a credential (>= 8 chars containing a digit or symbol such as `-._~+/=`, or >= 20 chars), so phrases like `bearer token`, `api key`, `token expired`, `Authorization header missing` stay readable. Still masked: `Authorization: Bearer …` (also `Basic` / `Token` schemes), `Bearer sk-…`, JWTs, `sk-…`, GitHub tokens (`ghp_` / `gho_` / `ghu_` / `ghs_` / `ghr_` / `github_pat_`; arbitrary `gh*_` identifiers no longer), `api_key|api key|token|secret|password|authorization = <value>` assignments including JSON (`"api_key":"…"`), and configured secret values.

### Docs
- README (Keychain / LaunchAgent): what to do after `brew upgrade` (re-authorize with `keys check` in a GUI terminal, then `launchctl kickstart -k`), why skipping it leaves the service in a restart loop, and a sample `com.dickwu.sub-bot` LaunchAgent plist.

## [0.10.1] - 2026-11-03

### Fixed
- **Context compaction broke Anthropic extended-thinking sessions**: after a compaction every following Anthropic call failed with HTTP 400 `Invalid signature in thinking block … bound to a different conversation`, because the kept assistant messages still carried signed `thinking` blocks whose signature is bound to the (now rewritten) preceding conversation. The session stayed broken, including after a restart. Kept messages created before a compaction now have their `thinking` / `redacted_thinking` blocks removed from the context sent to the model — for live auto compaction, manual compaction (HTTP / RPC / CLI) and when the context is rebuilt from the JSONL on resume. Text, tool calls and tool_use / tool_result pairing are unchanged; an assistant message left with no content is dropped (it has no tool calls). Messages produced after a compaction keep their thinking until a later compaction keeps them. The JSONL still stores the original messages, so sessions already broken by 0.10.0 heal when rehydrated with 0.10.1.

## [0.10.0] - 2026-11-03

### Added
- **Built-in `grep` / `find` tools** backed by ripgrep's library crates (`grep-searcher`, `grep-regex`, `grep-matcher`, `ignore`, `globset`) embedded in the binary — no `rg` install needed. ripgrep-like semantics: smart-case, fixed strings, multiline, context lines, binary-file skipping, max-count, glob / type filters, `.gitignore` aware. Included in the `default`, `readonly` and `no-shell` presets.
- **Session persistence & resume**: sessions are saved under `<dataDir>/sessions` by default (`sessionDir`, `persistSessions`, `--no-persist-sessions`). Resume after restart via `run --resume`, `rpc --resume`, `ask --resume`, or simply by using the session id over HTTP. `GET /v1/sessions?limit=`, `DELETE /v1/sessions/{id}?purge=true`.
- **Context compaction**: automatic (`compaction.thresholdPct`, `keepRecentTokens`, `model`, `maxSummaryTokens`, per-profile `contextWindow`) and manual (`POST /v1/sessions/{id}/compact`, RPC `compact`); `compaction` events and `compaction_failed` issues.
- **Tool approval**: per-profile `toolApproval` policy (`allow` / `ask` / `deny`, globs, timeout + `onTimeout`). `approval_required` / `approval_resolved` events, `GET/POST /v1/sessions/{id}/approvals[/{callId}]`, RPC `approve`, `approval_required` webhook event, interactive TTY prompt plus `--auto-approve` / `--auto-deny` for `ask` / `run`.
- **MCP client** (official `rmcp` SDK): stdio and streamable HTTP servers via global / per-profile `mcpServers`; tools exposed as `mcp__<server>__<tool>`; crash detection with restart backoff; `tools/list_changed` refresh; `GET /v1/mcp`, `sub-bot mcp list`; MCP tools go through `toolApproval`.
- Licensed under **MIT OR Apache-2.0** (`LICENSE-MIT`, `LICENSE-APACHE`); `THIRD-PARTY-NOTICES.md` carries ripgrep's MIT/Unlicense attribution. Release archives now include these files and this changelog.

### Fixed
- `session_run` tracing span no longer records `reasoning_effort` twice (the requested value is `requested_reasoning_effort`).
- Session listing sort key now equals the returned `updatedAt` for disk-only sessions.
