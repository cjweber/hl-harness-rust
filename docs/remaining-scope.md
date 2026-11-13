# Rust port — remaining scope

The Rust crate mirrors the TypeScript harness functionally. This file tracks what is ported and what still lives only in the TypeScript implementation. Persisted/wire formats (session JSONL, cache prefix hashes, event JSON, permissions rules, settings keys) are byte-compatible with TypeScript; runtime-only internals may differ.

Status: functionally at parity except the two items in “Not ported yet” below. Earlier sections listed the old not-ported list (TUI, MCP, extensions/packages, console surfaces); all of those have since landed — the phase 8 section summarizes the final interactive-TUI gap closure.

## Ported (phase 1)

- Core event/tool loop, compaction with model summary + deterministic fallback.
- OpenAI-compatible + Anthropic providers; retry/timeout; opencode-go/openrouter headers; per-request model override; model discovery.
- CachePrefix planning with byte-identical canonical serialization and hashes (tested against a recomputation of the TS canonical form).
- Pi JSONL v3 sessions: header, message/usage/cache/compaction/context_edit/branch_summary/custom entries, unknown-entry preservation, branching, active-context projection, `--session` resume.
- Bounded project-confined tools: read/write/edit/ls/find/grep/ripgrep/bash/json_get/json_edit/git_status/git_diff/git_log/git_apply_check/git_apply/git_conflicts, including structured conflict reports.
- Permission gate: modes, scoped rules (tool/risk/server), always-allow, audit; noninteractive default deny; `--yes`/`--permission-mode`.
- Four-layer settings hierarchy with flag/env/hl-harness/Pi precedence; `settings` command; `trust status|list|grant|revoke`; `cache` inspection; `models` discovery; `run --json` event stream.

## Ported (phase 6, in addition to phase 5)

- Extension host: child-process isolation with a Node runner (`--experimental-strip-types` for `.ts`) shipped inside the crate (`assets/host-process.mjs`) speaking the identical JSONL protocol (`ready`, `tool`, `command`, `provider`, `tool_result`, `command_result`, `hook_result`, `extension_error`).
- Extension tools/commands/providers registered from `extensions/` directories (trusted projects), exposed as agent tools with risk `extension`, plus command dispatch and provider registry consulted via `--extension-provider`.
- Agent hooks `before_tool` (arg rewrite + block/reason veto), `after_tool` (result transformation) following the tool loop, and lifecycle event emit.
- Extension timeout/error isolation: stderr surfaced as `[extension]` lines, startup deadline with a 5s timeout, pending callers failed on extension errors, tested against a real Node extension including hooks and vetoes.

## Ported (phase 7, in addition to phase 6)

- Resource loader: conventional `skills/`/`prompts/`/`themes/`/`extensions/` directories, `package.json` `pi` manifests with `**`/`*`/`?` glob matching and `!` exclusions, frontmatter descriptions, SKILL.md directory discovery, theme JSON name from `name` key, index.<ext> naming rules.
- Context files (AGENTS.override.md/AGENTS.md/CLAUDE.md walk-up) and SYSTEM.md/APPEND_SYSTEM.md system-prompt assembly with the skill catalogue.
- Packages: npm (`npm:name`) and git (`git:`/https/ssh) resolution including Pi's mirrored `~/.pi/agent/git/...` layout, `npm install/update` + `git clone/pull` delegations, `packages list|install|enable|disable|update|remove` CLI with project-scope trust gating, managed-package files from the four-layer settings hierarchy, and tolerant `resolvePackageRoots` warnings instead of fatal errors (tested against npm/git layouts and settings round trips).
- `resources` CLI with per-kind counts; verified live: 15 skills + 3 extensions + 18 total package resources reading real Pi settings on the host machine.

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

## Ported (phase 8, interactive TUI completion)

- Full TUI feature set on crossterm: alternate-screen dashboard, transcript with markdown/tool cards, workspace widgets (exact tiling at any terminal width, per-section hues, semantic value tinting), diff/cache/conflicts views, approval modal, secret mode, MCP elicitation prompts, pacman working animation, quote line.
- Workspace widgets wired to live data: `git` (branch · changes · ahead/behind, refreshed after git tool calls and runs), `context` (window size vs provider context window with percent), `model`, `session`, `cache`, `project`.
- Skills: resources loaded once at TUI startup into the agent system prompt (same loader as `run`), first-class `skill` tool resolving SKILL.md instructions, `/skill` listing and `/skill:name [args...]` prompt expansion.
- Prompt templates: `/template` listing and `/template <name> [args...]` `$ARGUMENTS`/`${N}` expansion submitted as a prompt.
- Extensions in the TUI: trusted-project host starts at launch (same Node runner and protocol as `run`), extension tools join every run, `before_tool`/`after_tool`/`emit` hooks fire via the agent observer, `/<name>` slash commands dispatch to the host, completions include them, clean shutdown on quit.
- `/auth <provider> <api-key>` (alias `/login`): per-session key configuration with agent rebuild (shared `rebuild_agent` helper also upgrades `/model` with the resources system prompt and extension hooks); `/auth` alone reports each provider's key env status.
- Image attachments: `/attach` stages images and the next submission sends them through `run_checked`.
- Provider runtime extras: ordered provider fallbacks from the `fallbacks` settings key (`FallbackProvider`: early-failure fall-through, mid-stream error propagation, exhausted-chain last error) and usage/cost normalization (provider-supplied `usage.cost` flows through `Usage` events).

## Not ported yet (in functional order)

1. **Provider reasoning blocks** — Anthropic `thinking` config is sent and OpenAI-compatible `reasoning_effort` is set, but reasoning deltas (`thinking_delta` SSE events) are not surfaced as agent events yet.
2. **Image preview tooling** — images attach and send, but there is no preview rendering (the TS harness has none in the terminal either).

## Deliberate differences

- Regex handling for `grep` uses the `regex` crate (the TS version uses JavaScript RegExp); for full ripgrep-quality semantics prefer the `ripgrep` tool, as in TS.
- `HashSet`-based always-allow preserves semantics but not TS insertion order (order is not observable in decisions).
- Approval prompts are not interactive in the CLI phase (the TS `run` mode also has no TTY prompt; `read-only` default and `--yes` produce identical behavior).
- Tool-call argument validation errors abort the run in both implementations; the error text is equivalent but not byte-identical.
