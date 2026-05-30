# Changelog (English)

Russian version: [CHANGELOG.md](CHANGELOG.md).

Format — [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).
Versioning — [SemVer](https://semver.org/).

> This English changelog is being introduced gradually. Earlier releases are documented only in the Russian [CHANGELOG.md](CHANGELOG.md) for now.

## [Unreleased]

**Optional whitelist of MCP tools via `[tools].enabled` in `daemon.toml`.**

The fight for your tokens and speed continues: the server can now be configured to expose only a subset of tools in `tools/list` instead of all 25 (18 universal + 7 BSL). Fewer schema tokens on every `initialize`, less confusion for weaker models when picking a tool, same functionality for stronger ones. Default behavior is unchanged — if there is no `[tools]` section or `enabled` is empty, all registered tools remain available (backward compatible).

### Added

- **`[tools]` section in `daemon.toml`** with an `enabled: Vec<String>` field. Empty array or missing section — all tools available. Filled — only listed names appear in `tools/list`; others are blocked at `tools/call` with `-32602 Invalid params: tool 'X' is disabled by [tools].enabled whitelist in daemon.toml`. Double protection is needed because the model may invoke a tool from its memory / system prompt bypassing `tools/list` — a `list_tools`-only filter would not stop that.
- **`CodeIndexServer::with_allowed_tools(Option<BTreeSet<String>>)`** — builder for setting the whitelist programmatically (used by `cli.rs`).
- **`CodeIndexServer::validate_whitelist(&BTreeSet<String>) -> Vec<String>`** — returns names that do not match any registered tool (typos, removed tools). Used by `cli.rs` for a startup warning.
- **Startup logs**: empty `enabled` → `[tools].enabled is empty — whitelist disabled, all tools available`; non-empty → `[tools].enabled whitelist active: N known tools enabled (M in list)` + warning on unknown names.
- **3 parsing tests** for the `[tools]` section in `daemon_core::config::tests` (`tools_section_default_empty`, `parses_tools_whitelist`, `parses_empty_tools_section`).

### Compatibility

- Fully backward compatible. Old `daemon.toml` without a `[tools]` section continues to work as before (all tools available). Default behavior matches v0.10.x.
- Minimum functionally safe set: `read_file`, `grep_code`, `get_function`, `find_symbol`, `list_files`, `get_stats`, `health`. Trimming below this (e.g., keeping only `grep_body` without `grep_code`) leads to blindness on imports / directives / module-level code and fallbacks via the expensive full `read_file` — the token savings will be destroyed.
