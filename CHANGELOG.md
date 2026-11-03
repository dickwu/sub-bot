# Changelog

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
