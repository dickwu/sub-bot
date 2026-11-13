# Changelog

## [0.10.4] - 2026-11-13

### Added
- **Turn budget controls (caller-only)**: the calling ("main") agent controls the per-run turn budget (`maxTurns` = model calls per run). There is no cap on the value or on extensions, and the model has no tool to extend its own turns.
  - Profile fields `onMaxTurns` (`stop` default | `ask` | `summarize`) and `maxTurnsAskTimeoutSecs` (default 600, >= 1); `maxTurns` is now validated `>= 1`.
  - Per-session / per-request `maxTurns`, `onMaxTurns`, `maxTurnsAskTimeoutSecs` in `POST /v1/sessions` and the `/v1/chat` `session` object (invalid ⇒ 400); persisted in the session file like `toolApproval`, so rehydrated sessions keep them.
  - Runtime adjustment: `POST /v1/sessions/{id}/turns` with exactly one of `{"maxTurns":N}`, `{"addTurns":N}` (may be negative, result clamped to >= 1) or `{"stop":true}` (409 when nothing is paused); response `{sessionId, maxTurns, previous, turnsUsed, running, paused}`; 400 invalid body, 404 unknown session / other owner. `PATCH /v1/sessions/{id}` also accepts `maxTurns`. The limit is read before every model call, so changes apply to the running run and to later runs; lowering it below the turns already used stops the run at the next check.
  - On reaching the limit: `stop` ends the run (`max_turns`); `summarize` injects a `[sub-bot]` user message and makes one tool-less call (still `max_turns`); `ask` pauses before the model call until the caller raises the limit (continue), sends `stop`, aborts (abort / delete / run timeout), or `maxTurnsAskTimeoutSecs` expires (stop). The session view and `get_state` show `maxTurns` and `turnBudget` (`paused`, `pause`, `lastRun`, …); paused tasks stay `running`.
  - Events `turn_budget_low` (once per run at ceil(0.8·maxTurns), re-armed after a change), `turn_budget_exhausted {turnsUsed, maxTurns, action, timeoutSecs?, expiresAt?}`, `turn_budget_changed {maxTurns, previous, turnsUsed, by}`, `turn_budget_resolved {decision, maxTurns, waitMs}`, with INFO logs (`session_id`, structured fields).
  - Webhooks: new `webhookEvents` value `turn_budget` (included in `all`) delivering `task.turn_budget_low` and `task.turn_budget_exhausted` (`data` adds `taskId`, `sessionId`, `turnsUrl`).
  - Final results (`/v1/chat` sync and async, `run --json`, `ask --json` `session_state`, `ask --async`) always carry `sessionId`, `turnsUsed`, `maxTurns`; when stopped by the limit also `stopReason: "max_turns"` and `canContinue: true`, and the error reads `max_turns reached (N/M turns used); continue by sending another prompt with sessionId <id>`.
  - RPC command `set_max_turns` (`maxTurns` | `addTurns` | `stop`).
  - CLI: `--max-turns N` / `--on-max-turns stop|ask|summarize` on `run`, `ask` and `rpc` (`ask --session` / `run --resume` apply `--max-turns` to the existing session); `ask --auto-extend-turns N`; new `sub-bot turns <sessionId> (--set N | --add N | --stop)` with ask's connection options, printing the JSON response. Text mode prints `max_turns reached (N/M). Continue: …` on stderr and keeps the non-zero exit code.

### Changed
- Default `maxTurns` raised from 50 (profiles) / 30 (built-in fallback config) to **150**.
- `max_turns` issue reporting is unchanged.

### Dependencies
- `reqwest` 0.12 → **0.13** for the whole workspace (one reqwest version; the `reqwest13` alias used by the MCP client is gone). TLS stays rustls + ring + webpki-roots (no aws-lc, no OpenSSL): reqwest is built with `rustls-no-provider` and every client is created through the new shared `agent_ai::http::client_builder()` (preconfigured ring TLS, HTTP/1.1 ALPN).
- `rand` 0.8 → **0.10**, `sha2` 0.10 → **0.11**, `hmac` 0.12 → **0.13**, `directories` 5 → **6**; `rmcp` 3.8 → 3.10 and other semver-compatible updates via `cargo update`.
- Token SHA-256 values and webhook HMAC signatures are unchanged (covered by tests). Duplicate crates removed: reqwest 0.12, rand 0.8 / rand_core 0.6, sha2/digest 0.10 and related. No new license types.

## [0.10.3] - 2026-11-03

### Fixed
- **macOS Keychain items asked for authorization after every `brew upgrade`**: `sub-bot keys set` created items through Security.framework (`SecItemAdd`) from the sub-bot binary itself. The Homebrew binary is ad-hoc/linker-signed, so the item ACL trusted only that binary's `cdhash` (and the partition list was `cdhash:<hash>`); every upgrade changed the cdhash, so the new binary triggered an authorization dialog on first read — or, under launchd with nobody to click it, timed out (`no answer from macOS Keychain within 30s …`) and the service sat in a restart loop. Trusting a stable path (`/opt/homebrew/bin/sub-bot`) does not help because both checks are cdhash-based. Keychain items are now read, written and deleted through Apple's `/usr/bin/security` (same approach as `gh` / go-keyring): items created by `security add-generic-password` carry the ACL `identifier "com.apple.security" and anchor apple` with partition `apple-tool:`, and `security find-generic-password -w` reads them without any prompt regardless of which sub-bot build spawns it. `keys set` deletes an existing item first (an in-place update would keep the legacy ACL), then re-adds and verifies by reading back. The secret never appears in a command line: the add goes through `security -i` on stdin as `-X <hex>` (visible in `ps` only as `security -i`); service/account names used for writing are restricted to `[A-Za-z0-9._/@:-]`. Exit codes of the tool are mapped to the same errors as before (`44` → not found, `36` → interaction not allowed, `51` / `128` → denied); the tool's stdout never enters errors or logs. `keys list` still uses a Security.framework attribute search (no data read, no prompt). **One-time migration needed** for items saved by sub-bot <= 0.10.2 — see `keys reauth`.

### Added
- **`sub-bot keys reauth`** (macOS): migrates Keychain items created by older sub-bot versions (ACL tied to an old binary) to the `/usr/bin/security`-owned form, so later upgrades need nothing. Targets: managed items under service `sub-bot` (or `--service`) plus `apiKeyKeychain` entries of `--config`, filtered by `--provider` / `--account`, deduplicated. Each item is read once (a legacy item shows one dialog — click "Always Allow" / enter the login password; the usual `keychainTimeoutSecs` timeout and waiting hint apply), deleted, re-added and read back. Reports `migrated` / `not found` / `FAILED: <reason>` per item; non-zero exit if any failed. `--from-env` takes the key from `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` (named accounts: their `apiKeyEnv`) instead of reading the old item — zero dialogs. `--dry-run` lists the targets without reading or changing anything; `--json` outputs an array like `keys list`. On Linux it prints that Keychain is macOS-only and exits 0. Output and logs never contain the key.
- New `KeychainError::Tool` (`keychain tool failed (service=…, account=…): <reason>`) for `/usr/bin/security` not being runnable, a name that cannot be written safely, or an add that reported success but does not read back.

### Changed
- Keychain error and hint texts now point to `sub-bot keys reauth` (one-time migration) instead of re-running `keys set` after every upgrade; the `keychain error <code>` text may now carry the exit status of `/usr/bin/security` (= OSStatus & 0xff) for read/write/delete, and the OSStatus for attribute searches.

### Tests
- `strip_thinking_keeps_text_and_tool_calls_and_drops_emptied_messages` no longer fails intermittently (expected messages are cloned from the inputs instead of being rebuilt with a fresh timestamp).

### Docs
- README: Keychain section rewritten — since 0.10.3 upgrades need no action; one-time `keys reauth` (or `keys reauth --from-env` / `keys set`) after upgrading from <= 0.10.2, then `launchctl kickstart -k gui/$(id -u)/com.dickwu.sub-bot`; the trade-off (any process running as the same user can read the item with `security find-generic-password -w` without a prompt, comparable to a 0600 `apiKeyFile`) and how to opt out of it.

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
