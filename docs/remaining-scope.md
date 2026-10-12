# Rust port — remaining scope

The Rust crate mirrors the TypeScript harness functionally. This file tracks what is ported and what still lives only in the TypeScript implementation. Persisted/wire formats (session JSONL, cache prefix hashes, event JSON, permissions rules, settings keys) are byte-compatible with TypeScript; runtime-only internals may differ.

## Ported (phase 1)

- Core event/tool loop, compaction with model summary + deterministic fallback.
- OpenAI-compatible + Anthropic providers; retry/timeout; opencode-go/openrouter headers; per-request model override; model discovery.
- CachePrefix planning with byte-identical canonical serialization and hashes (tested against a recomputation of the TS canonical form).
- Pi JSONL v3 sessions: header, message/usage/cache/compaction/context_edit/branch_summary/custom entries, unknown-entry preservation, branching, active-context projection, `--session` resume.
- Bounded project-confined tools: read/write/edit/ls/find/grep/ripgrep/bash/json_get/json_edit/git_status/git_diff/git_log/git_apply_check/git_apply/git_conflicts, including structured conflict reports.
- Permission gate: modes, scoped rules (tool/risk/server), always-allow, audit; noninteractive default deny; `--yes`/`--permission-mode`.
- Four-layer settings hierarchy with flag/env/hl-harness/Pi precedence; `settings` command; `trust status|list|grant|revoke`; `cache` inspection; `models` discovery; `run --json` event stream.

## Ported (phase 4, in addition to phase 3)

- MCP client implemented directly on the MCP 2024-11-05 JSON-RPC protocol (no Rust SDK dependency): stdio transport (spawn + newline JSON-RPC, writer serialization, server→client requests answered through an interaction surface, notifications) and Streamable HTTP transport (POST request/response, JSON + SSE bodies, `mcp-session-id` handling, bearer tokens).
- `tools/list`/`tools/call` exposed as `mcp__<server>__<tool>` with sanitized names, `[server]`-prefixed descriptions, and isError→error conversion (tested against a real Python stdio MCP server).
- `resources/list|read`, `prompts/list|get`.
- OAuth: RFC 8414 metadata discovery, PKCE S256, authorization URL construction on 401, `finishAuth` token exchange, token + verifier files persisted at mode 0600.
- Manager: `connect` with operation approval (tool `mcp:<name>`, risk `mcp`), gating identical to the TS harness; `mcp list|add|remove` CLI with trust gating and OAuth flags; `run --mcp <name>` connects through the permission gate.
- Interaction surface: non-interactive runs decline elicitation/sampling; capability advertisement is conditional in the protocol payload.

## Ported (phase 3, in addition to phase 2)

- Browser automation over CDP without Playwright (`tokio-tungstenite`): `browser_launch` (persistent profile `~/.pi/agent/browser-profile`, candidate discovery incl. `HARNESS_BROWSER_COMMAND`), `browser_reconnect`, `browser_tabs`, `browser_select`, `browser_open` (http(s)-only navigation), `browser_eval`, `browser_click`, `browser_text`, `browser_screenshot` (project-local PNG), `browser_close`, `browser_shutdown`. Tested against a mock CDP endpoint (HTTP /json/list + simulated WebSocket CDP commands) plus an opt-in real-Chrome e2e (`HARNESS_BROWSER_E2E=1`, headless via `HARNESS_BROWSER_HEADLESS=1`).

## Ported (phase 2, in addition to phase 1)

- `web_search` (Brave key or DuckDuckGo HTML parsing) and `web_fetch` with literal private/local host blocking (unit-tested against the TS host list).
- Background jobs (`bash_background`/`bash_job`/`bash_cancel`), persistent shells (`bash_session`/`bash_session_close`) with the marker protocol and 120s timeout.
- `bash_sandbox` (macOS `sandbox-exec` profile / Linux bubblewrap arg set) with fail-closed behavior on unsupported hosts or missing backends.
- Filesystem watches (`watch_start`/`watch_changes`/`watch_stop`) on the `notify` crate, bounded to 100 events.
- SQLite (`database_query` read-only SELECT/EXPLAIN, `database_execute`) on `rusqlite` (bundled), run on the blocking pool.
- Session compaction persistence: `appendCompactionFromMessageIndex` ported and wired into the CLI compaction callback.

## Not ported yet (in functional order)

1. **TUI** — transcript, tool cards, workspace widgets, diff/cache/conflicts views, approval modal, MCP elicitation/sampling prompts. (The Rust TUI will use crossterm/ratatui with the same feature set.)
2. **MCP** — stdio + Streamable HTTP clients, OAuth persistence, elicitation/sampling, permission-gated `mcp__server__tool` calls.
4. **Extensions/packages** — child-process extension host (custom tools, commands, providers, hooks), resource discovery, package install/update/remove, package settings layers.
5. **Provider runtime extras** — abort/cancellation plumbing (TS `AbortSignal` ⇄ tokio cancellation), provider-specific reasoning blocks, usage/cost normalization, provider fallbacks.
6. **Console surfaces** — `packages` CLI, prompt templates, skills, `/skill:name`, image attachment previewing; `--image` load is ported but without preview tooling.

## Deliberate differences

- Regex handling for `grep` uses the `regex` crate (the TS version uses JavaScript RegExp); for full ripgrep-quality semantics prefer the `ripgrep` tool, as in TS.
- `HashSet`-based always-allow preserves semantics but not TS insertion order (order is not observable in decisions).
- Approval prompts are not interactive in the CLI phase (the TS `run` mode also has no TTY prompt; `read-only` default and `--yes` produce identical behavior).
- Tool-call argument validation errors abort the run in both implementations; the error text is equivalent but not byte-identical.
