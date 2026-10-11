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

## Not ported yet (in functional order)

1. **TUI** — transcript, tool cards, workspace widgets, diff/cache/conflicts views, approval modal, MCP elicitation/sampling prompts. (Cross-Language note: the TS TUI will not be reproduced line-for-line; the Rust TUI will use crossterm/ratatui with the same feature set.)
2. **Session compaction persistence** — `appendCompactionFromMessageIndex` linkage is not wired into the CLI's compaction callback yet (the agent compacts in memory; session persistence of compaction entries lands with phase 2).
3. **Tools**: background jobs, persistent shells, `bash_session*`, `bash_sandbox` (macOS sandbox-exec / Linux bubblewrap), `watch_start/changes/stop`, `database_query/execute` (SQLite), `web_search`/`web_fetch` (with private-host blocking).
4. **Browser automation** — CDP session (launch/reconnect/tabs/eval/click/text/screenshot/close/shutdown) without Playwright.
5. **MCP** — stdio + Streamable HTTP clients, OAuth persistence, elicitation/sampling, permission-gated `mcp__server__tool` calls.
6. **Extensions/packages** — child-process extension host (custom tools, commands, providers, hooks), resource discovery, package install/update/remove, package settings layers.
7. **Provider runtime extras** — abort/cancellation plumbing (TS `AbortSignal` ⇄ tokio cancellation), provider-specific reasoning blocks, usage/cost normalization, provider fallbacks.
8. **Console surfaces** — `packages` CLI, prompt templates, skills, `/skill:name`, image attachment previewing; `--image` load is ported but without preview tooling.

## Deliberate differences

- Regex handling for `grep` uses the `regex` crate (the TS version uses JavaScript RegExp); for full ripgrep-quality semantics prefer the `ripgrep` tool, as in TS.
- `HashSet`-based always-allow preserves semantics but not TS insertion order (order is not observable in decisions).
- Approval prompts are not interactive in the CLI phase (the TS `run` mode also has no TTY prompt; `read-only` default and `--yes` produce identical behavior).
- Tool-call argument validation errors abort the run in both implementations; the error text is equivalent but not byte-identical.
