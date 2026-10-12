# hl-harness (Rust port)

A from-scratch Rust implementation of the [hl-harness](https://github.com/hyperlocalinnovations/hl-harness) coding-agent harness, ported 1:1 functionally from the TypeScript original. Session JSONL, cache-prefix hashing, permission semantics, provider request shapes, and event schemas match the TypeScript harness byte-for-byte where crossing a persisted or wire boundary.

## Status

Phase 1 of the port — the non-interactive core — is implemented and tested:

- **Core loop** (`src/core/agent.rs`): multi-step tool loop, assistant deltas, tool calls/results, usage propagation, cache-plan events, model-driven compaction with deterministic fallback, max-step limits.
- **Providers** (`src/model/`): OpenAI-compatible SSE streaming (OpenRouter, OpenCode Go, custom endpoints) and native Anthropic Messages streaming, with retry/backoff, timeouts, session-affinity headers (`x-opencode-session`), attribution headers, image data-URL/Anthropic base64 encoding, and model discovery.
- **Cache planning** (`src/cache/prefix.rs`): deterministic canonical serialization and SHA-256 prefix hashes byte-identical with TypeScript (verified by test).
- **Sessions** (`src/sessions/store.rs`): Pi JSONL v3 — headers, parent/leaf branching, context edits, compaction (including `appendCompactionFromMessageIndex`, wired into the CLI), custom entries, unknown-entry preservation, active-context projection, resume via `--session`.
- **Tools** (`src/tools/`): `read`, `write`, `edit`, `ls`, `find`, `grep`, `ripgrep`, `bash`, background jobs (`bash_background`/`bash_job`/`bash_cancel`), persistent shells (`bash_session*`), `bash_sandbox` (macOS `sandbox-exec` / Linux bubblewrap), `watch_start/changes/stop`, `json_get`, `json_edit`, `web_search`, `web_fetch` (private-host blocking), `database_query`/`database_execute` (SQLite), `git_status`, `git_diff`, `git_log`, `git_apply_check`, `git_apply`, `git_conflicts` — project-confined paths, bounded outputs, conflict-marker reporting.
- **MCP** (`src/mcp/`): direct MCP 2024-11-05 JSON-RPC client over stdio and Streamable HTTP, `mcp__<server>__<tool>` tools, resources + prompts, OAuth with PKCE and 0600 token persistence, connection approval through the permission gate, `mcp list|add|remove` and `run --mcp <name>`.
- **Permissions** (`src/security/permissions.rs`): modes (`read-only`/`ask`/`full`), scoped rules in `~/.pi/agent/permissions.json`, always-allow, audit records; noninteractive default denies unsafe operations.
- **Settings hierarchy** (`src/config.rs`): project `.harness/settings.json` > `~/.pi/agent/harness-settings.json` > project `.pi/settings.json` > `~/.pi/agent/settings.json`, with CLI flags and environment above all file layers (same precedence as the TS CLI).
- **Trust** (`src/config.rs`): `trust status|list|grant|revoke` with project fingerprints.

Run `cargo test` for 13 unit + 18 integration tests (the real-browser e2e is opt-in with `HARNESS_BROWSER_E2E=1`); MCP stdio is tested against a real Python MCP server, SSE transport against a real local TCP mock server; no network or keys required.

## Usage

```bash
cargo build --release
target/release/hl-harness run "Read package.json and list the scripts"
target/release/hl-harness run "..." --provider opencode-go --yes
target/release/hl-harness run "..." --session <session.jsonl>   # resume
target/release/hl-harness cache <session.jsonl>                 # cache request history
target/release/hl-harness models --provider anthropic
target/release/hl-harness settings                              # show the effective merge
target/release/hl-harness trust status|list|grant|revoke
```

Environment: `OPENROUTER_API_KEY`, `OPENCODE_API_KEY`, `ANTHROPIC_API_KEY`, `HARNESS_API_KEY`, `HARNESS_ENDPOINT`, `HARNESS_CONTEXT_WINDOW`, `HARNESS_TRUST_PROJECT` — identical to the TypeScript harness.

## Port boundaries

See `docs/remaining-scope.md` for the definitive list of what is and is not ported yet. Notably: the TUI, MCP, extensions/packages, browser CDP, sandbox shells, background/persistent shells, SQLite, file watches, and web tools are still on the TypeScript side for now.
