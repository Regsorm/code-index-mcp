# Changelog (English)

Russian version: [CHANGELOG.md](CHANGELOG.md).

Format — [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).
Versioning — [SemVer](https://semver.org/).

## [0.13.0] — 2026-05-30

**Compact JSON in MCP tool output instead of pretty.**

MCP tool output is consumed by the model, not a human — pretty-JSON indentation and newlines waste tokens for nothing. We switch response serialization to compact (`to_string` instead of `to_string_pretty`). ~30% saved on every tool response, especially noticeable for federation (remote repos) and text tools. The data itself is unchanged — same JSON, just unformatted.

### Changed

- **MCP tool response serialization is now compact** (`to_string`): `wrap_with_meta` (18 universal tools — read_file, grep_*, get_function, list_files, etc.), `to_json` (`get_stats`/`stat_file`/`health`), `format_unavailable`, federation forwarding (`federation_error` + per-repo `get_stats` aggregation).
- BSL-tools already emitted compact via `CallToolResult::structured` — unaffected.

### Compatibility

- The data format is unchanged — only pretty-formatting (indentation/newlines) was removed. Any JSON parser reads the result as before.
- **CLI output** (`--json`) and the `daemon.json`/`config.json` files stay pretty — they are human-readable and not on the model's hot path.

## [0.12.0] — 2026-05-30

**`grep_code`: default `limit` lowered 500→100 and an explicit `truncated` flag added.**

Based on real usage stats (a 2-month sample, ~240 `grep_code` calls): when the model sets `limit` itself, it picks ~20–40 matches (median 30), and specified 500 only twice out of a hundred calls. The old default of 500 (with a `path_glob`/`language` filter) inflated the response twofold versus native Grep (`head_limit` 250) — especially with `context_lines`. We lower the default to 100 and make truncation visible so the model can re-request a larger `limit` instead of treating a truncated list as complete.

### Changed

- **`grep_code` default `limit` 500 → 100** (new `GREP_CODE_DEFAULT_LIMIT` constant). Previously the default depended on the filter: 100 on full-scan / 500 with a `path_glob`/`language` filter; now a single default of 100. An explicitly passed `limit` works as before.
- **`grep_code` result format**: instead of a bare array `[{path, line, content, context}]`, it now returns an object `{matches: [...], shown, limit, truncated}`. `truncated=true` means the result was cut off by `limit` or the byte cap (1 MB) — there are more matches, re-request with a larger `limit`. Previously truncation was silent and read as "these are all matches".
- **`Storage::grep_code_filtered`** now returns `(Vec<GrepTextMatch>, bool)` — the second tuple element is the truncation flag.

### Compatibility

- **`grep_code` response format change** (array → object `{matches, …}`). Consumers that parsed the response as an array must read `result.matches`. `mcp-cache-ci` (uses only `_meta.dependent_files`) and federation forwarding are unaffected. `grep_text`/`grep_body` formats are **unchanged** — still arrays.

## [0.11.0] — 2026-05-30

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

## [0.10.4] — 2026-05-22

**Fix for publishing to the MCP registry: namespace case.**

The registry rejected `server.json` with a 403 — the namespace was given in lowercase (`io.github.regsorm`), while OIDC grants rights to a namespace that exactly matches the GitHub login (`io.github.Regsorm`). The case in `name`/`mcpName` is fixed. npm publishing already succeeded in 0.10.2/0.10.3; this patch completes the registration of the listing in the official registry.

### Fixed

- **`server.json` `name` and `package.json` `mcpName`** — namespace case `io.github.Regsorm/code-index` (exactly as the GitHub login).

### Changed

- **Workspace version** 0.10.3 → 0.10.4.

## [0.10.3] — 2026-05-22

**Fix for publishing to the MCP registry: description length.**

The registry rejected `server.json` with a 422 — the `description` field exceeded the 100-character limit. Shortened to 98. npm publishing already succeeded in 0.10.2; this patch completes the registration of the listing in the official registry.

### Fixed

- **`server.json` `description`** shortened to ≤100 characters (registry requirement).

### Changed

- **Workspace version** 0.10.2 → 0.10.3.

## [0.10.2] — 2026-05-22

**Auto-publish fix: a working workflow trigger.**

In 0.10.1 publishing did not fire — `publish-registry.yml` was on a `workflow_run` trigger, which GitHub only runs when the file is present on the default branch (`main`); releases, however, are tagged from a working branch. In addition, the `mcp-publisher` download pattern was picking up an extra asset.

### Fixed

- **`publish-registry.yml` trigger** switched from `workflow_run` to `push: tags: ['v*']` — works from any branch. Added a step that waits for the GitHub Release (the code-index archives) to be ready before `npm publish`, to eliminate a race.
- **`mcp-publisher` download** — exact asset pattern `mcp-publisher_linux_amd64.tar.gz` (previously `*linux_amd64.tar.gz` also matched `registry_linux_amd64.tar.gz`).
- The `mcp-publisher login github-oidc` and `publish` commands were verified against the actual CLI (v1.7.9).

### Changed

- **Workspace version** 0.10.1 → 0.10.2.

## [0.10.1] — 2026-05-22

**Publishing to npm and the official MCP registry.**

The public `code-index` can now be installed via `npx`/`npm` and is registered in the [official MCP registry](https://registry.modelcontextprotocol.io/) (`io.github.regsorm/code-index`). The Rust binary is still distributed via GitHub Releases — the npm package is only a thin wrapper that downloads the archive for the current platform on install. `bsl-indexer` stays private and is not published to the registry.

### Added

- **npm wrapper `@regsorm/code-index-mcp`** (the `npm/` directory): `package.json` with `mcpName`, `bin/cli.js` (transparently proxies arguments and stdio to the native binary), `scripts/postinstall.js` (downloads the `code-index-<platform>` archive from GitHub Releases and unpacks it with the system `tar`/bsdtar). Supports Windows x64, Linux x64, macOS arm64.
- **`server.json`** — the listing for the official MCP registry (npm package, stdio transport, the `serve` subcommand).
- **`.github/workflows/publish-registry.yml`** — after a successful `Release` on a `v*` tag: `npm publish` + `mcp-publisher publish`. The version is substituted from the tag. Requires the `NPM_TOKEN` secret.

### Changed

- **Workspace version** 0.10.0 → 0.10.1.

### Compatibility

- Fully backward compatible. There are no changes in the indexer code — only the distribution infrastructure.

## [0.10.0] — 2026-05-21

**1C data-link graph (data-graph): new BSL tools `get_data_links` and `find_data_path`.**

Complements the CALL graph (`proc_call_graph`) with a DATA-LINK graph — "object → object" edges built from the reference types of attributes, register dimensions, and tabular-section attributes. It closes a common "wandering through the structure" pattern: instead of a series of `get_object_structure`/`get_metadata_structure` calls to trace links by hand — a single graph traversal. (On the real "collapse stock by customs declaration" case this used to be 37 structure queries → now a single `get_data_links`.)

### Added

- **`data_links` table** in the `bsl-extension` schema (`SCHEMA_EXTENSIONS`, additive via `CREATE TABLE IF NOT EXISTS` — no migration required): `from_object`, `from_path` (attribute / `Table.attribute` / dimension), `to_object`, `link_kind` (`attr`/`tabular_attr`/`register_dim`), `is_composite`, `is_universal`. Indexes `idx_dl_from` (forward traversal) and `idx_dl_to` (reverse — "who references X").
- **`crates/bsl-extension/src/xml/object_attributes.rs`** — a parser for reference types from individual objects' XML (`Catalogs/<X>.xml`, `Documents/<Y>.xml`, registers). Type classification: a concrete `cfg:CatalogRef.Контрагенты` → an edge to `Catalog.Контрагенты`; a composite one (several `<v8:Type>`) → several edges (`is_composite`); a generic one (`cfg:CatalogRef` without a name, `cfg:AnyRef`, `cfg:DefinedType.X`) → a terminal `*`-node (`is_universal`, not expanded during traversal — protection against fan-out and noise); primitives (`xs:`/`v8:`) are discarded. A safety cap for pathological type lists (>30 concrete types → `*Multiple`).
- **`index_data_links`** in `index_extras::run_index_extras` — traverses the object XML and populates `data_links` via a full rebuild (like the rest of `index_extras`). On a large configuration (~1900 object XMLs / ~68 MB) — ~1.3–1.9 s; incrementality is not needed.
- **MCP tool `get_data_links(repo, object, direction=out|in|both, depth=1..4)`** — the neighborhood of an object in the data-link graph via a recursive CTE. `out` — what it references; `in` — who references it; terminal `*`-nodes are not expanded during traversal.
- **MCP tool `find_data_path(repo, from, to, max_depth=4)`** — a path (a chain of reference links) between two objects (BFS over `data_links`, analogous to `find_path` for the call graph).
- Both tools are registered in `BslLanguageProcessor::additional_tools` (now **7 BSL tools**, **25** in total in the `bsl-indexer` build), available through federation as well (`POST /federate/extension`). Parser unit tests (3 type cases, tabular section, dimensions, cap) and population tests.

### Changed

- **Workspace version** 0.9.1 → 0.10.0.

### Compatibility

- Fully backward compatible. The new table is created idempotently at startup; existing indexes and tools are untouched. The public `code-index` binary does not change — the feature lives only in `bsl-indexer` (`bsl-extension`).

## [0.9.1] — 2026-05-12

**Stage 3 of the migration to event-based cache invalidation: notifying `mcp-cache-ci` after reindexing.**

It closes the loop: file saved → daemon (watcher) detected it → reindexed into SQLite → **sent `POST /invalidate {file_paths: [...]}` to cache-ci**. Using `reverse_index` (populated in stage 2 via `_meta.dependent_files`), cache-ci surgically drops only the dependent entries; the rest of the cache hits are preserved.

### Added

- **`crates/code-index-core/src/daemon_core/cache_client.rs`** — `CacheClient` with a pool of `reqwest::Client` (timeout 2s, keep-alive 60s) and a list of target URLs. The `invalidate_files(&[String])` method POSTs to all targets in parallel; on failure (network, 5xx, timeout) — an `eprintln!` warning and we move on; it must not panic, and the TTL on the cache-ci side serves as a safety net.
- **`[[cache_targets]]` section in `daemon.toml`** + the `CacheTargetEntry { url: String }` struct in `daemon_core/config.rs`. Example:

  ```toml
  [[cache_targets]]
  url = "http://127.0.0.1:8011"
  ```

  Multiple entries are allowed (multi-cache-ci topologies: local Windows + remote rag-VM). Absence of the section (or an empty list) → the event channel is off, behavior as before v0.9.1.
- **Helper `worker::collect_invalidate_paths(root, batch)`** — collects a deduplicated list of relative file paths from a batch of FS events. It accounts for all types (Modified/Created/Deleted) — deleting a file must also drop the associated cache entries.
- **`cache_client: Option<Arc<CacheClient>>` parameter** in `worker::run_worker` and `runner::spawn_worker`. It is threaded through from `runner::run` and `runner::handle_reload` (reload recreates `CacheClient` from the new config for added folders; existing workers keep their client until a daemon restart).
- **Unit tests** for `cache_client.rs`: empty targets → `is_empty()`; trailing slashes are stripped; an invalid target does not panic (connection refused → 0 successes). Tests for config.rs `cache_targets_default_empty` and `parses_cache_targets_list`.

### Changed

- **`worker::run_worker` signature** — a new trailing parameter `cache_client`.
- **`runner::spawn_worker` signature** — the same.
- **`commit_batch()` now returns a check result** — if the commit failed, no invalidate is sent (there is no new data in the index anyway; let cache-ci keep serving the old data — it will be corrected either on the next successful batch or via TTL).
- **Workspace version** 0.9.0 → 0.9.1.

### Compatibility

- `daemon.toml` without `[[cache_targets]]` — fully functional (behavior as before v0.9.1, no network traffic to cache-ci).
- `daemon.toml` with `[[cache_targets]]` — the event channel is activated automatically at startup.
- The `run_worker` / `spawn_worker` API — the signature changed (additive last param). External clients of the `code-index-core` crate (if any) must pass `None` for compatibility.

### Architecture (final state of the chain)

After v0.9.1 + cache-ci 0.2.0:

1. **The daemon's read-tools** return `{result, _meta: {dependent_files: [...]}}` (v0.9.0).
2. **`mcp-cache-ci`** on cache-fill writes `cache_key → file_paths` into `reverse_index` (cache-ci 0.2.0).
3. **The daemon watcher** on an FS event → reindex → `commit_batch` → `cache_client.invalidate_files(...)` → cache-ci drops surgically via `reverse_index` (v0.9.1).
4. **TTL fallback** — the third echelon of the safety net: if an event is lost (network, daemon crash, ReadDirectoryChangesW buffer overflow), the entry expires on its own after 600s/3600s.

## [0.9.0] — 2026-05-12

**Phase 2 (a stage of the migration to event-based cache invalidation): `_meta.dependent_files` in read responses.**

All MCP data tools now return a unified JSON format:

```json
{
  "result": <prev plain payload>,
  "_meta": { "dependent_files": ["src/X.bsl", "src/Y.bsl"] }
}
```

`dependent_files` is the list of file paths the response was built from. The intended consumer is `mcp-cache-ci`: on cache-fill it registers `cache_key → file_path` links in `reverse_index` and then surgically drops the affected entries on a signal from the daemon after a file is reindexed (stage 3, in preparation).

### Compatibility (BREAKING CHANGE to the response format)

All read-tool clients must be ready for the new `{result, _meta}` structure:

- Before: `search_function` returned a flat array `[FunctionRecord, ...]`.
- Now: `{"result": [FunctionRecord, ...], "_meta": {"dependent_files": [...]}}`.

For the existing consumer (`mcp-cache-ci` 0.2.0+) the behavior is backward compatible: cache-ci parses `_meta.dependent_files` if present, otherwise works as before (insert without dependencies, TTL fallback).

Tools **without** the wrapper (response format unchanged):

- `health` — non-cacheable.
- `get_stats` — diagnostic; its format is extended across federation, and a wrapper would break the aggregation.
- `stat_file` — trivial single-file.

### Added

- **Wrapper helpers in `crates/code-index-core/src/mcp/tools.rs`:**
  - `wrap_with_meta<T: Serialize>(result, dependent_files)` — final serialization into `{result, _meta}` with deduplication of file paths.
  - `collect_paths_via<R>(storage, records, extract: fn(&R) -> file_id)` — collect paths from a vec of records via an extractor.
- **Wrapper helpers in `crates/bsl-extension/src/tools/mod.rs`:**
  - `wrap_with_meta(result: Value, dependent_files: Vec<String>) -> Value` for BSL extension tools.
  - `wrap_error(error_value: Value) -> Value` — even on error the format is unified.
- **Support for `_meta.dependent_files` in core data tools:**
  - `search_function`, `search_class` — DISTINCT file paths from the vec of records.
  - `get_function`, `get_class` — the same.
  - `find_symbol` — the union of paths from functions+classes+variables+imports.
  - `get_imports` (by file and by module).
  - `get_file_summary` — path from args.
  - `get_callers`, `get_callees` — file ids from CallRecord.
  - `grep_body` — file_path directly from GrepBodyMatch.
  - `grep_code`, `grep_text`, `search_text` — path directly from the match structs.
  - `read_file` — path from args.
  - `list_files` — paths from ListedFile.
- **Support for `_meta.dependent_files` in BSL extension tools** (an empty array for now — the XML metadata parser is not tied to file_path; real dependencies are a task for the next iteration):
  - `get_object_structure`, `get_form_handlers`, `get_event_subscriptions`, `find_path`, `search_terms`.

### Changed

- **Workspace version** bumped 0.8.1 → 0.9.0 (minor — a backward-compatible format extension for the cache-ci client, breaking for clients that parsed the flat payload).

### Next steps

- Stage 3: `POST /invalidate {file_paths}` from the daemon to cache-ci after the SQLite `transaction.commit()` for a batch of FS events. The cache-ci 0.2.0 side is already ready to receive it.

## [0.8.1] — 2026-05-06

**Patch release: BSL extension tools in daemon mode and through federation.** It fixes two public regressions of v0.8.0 that made five BSL tools (`get_object_structure`, `get_form_handlers`, `get_event_subscriptions`, `find_path`, `search_terms`) non-functional in the standard production scenario (repos served by the daemon, federation repos on a remote node).

### How we found it and why we fixed it ourselves

The regression was discovered by us **while operating v0.8.0** (2026-05-06): an attempt to call `get_object_structure` on any BSL repo led to `database error: no such table: metadata_objects`, and on a federation repo — to `extension tool '...' currently supports only local repos`. No one had reported the errors before us — external users of v0.8.0 may not have reached the 1C branch. Localized to two points in `code-index-core`: the calls to `apply_schema_extensions` / `index_extras` existed only in the CLI `index` command (`cli.rs`) and were absent in `daemon_core/worker.rs`; and `mcp::call_tool` had a hard rejection for `is_local == false`. After a full verification cycle (235 unit tests + a smoke on 4 BSL repos locally and through federation on the VM) — the fix was rolled out as the v0.8.1 patch without any involvement of the external community.

### Fixed

- **The daemon now applies the processors' `schema_extensions` and `index_extras`.** In v0.8.0 these calls were only in the CLI `index <path>` command, while the daemon worker did not make them. The result: on any BSL repo indexed via `bsl-indexer.exe daemon run`, the BSL tools failed with `database error: no such table: metadata_objects`. Now the `daemon_core/worker.rs` worker resolves the processor itself using the rule "explicit `language` from `daemon.toml` → fallback `detect()`", applies `apply_schema_extensions` BEFORE `full_reindex` (creates empty tables — the DDL is idempotent), and calls `index_extras` BEFORE `flush_to_disk` (populates the tables from `Configuration.xml`). For repos without a `Configuration.xml` (e.g., old data-processor dumps) the tables are created empty — the tools respond with `[]` and no exception.
- **Federation now forwards extension tools to remote nodes.** Previously any BSL-tool call on a remote repo (UT/BP_SS/BP_TDK/ZUP on the rag VM) returned `extension tool '...' currently supports only local repos`. A universal route `POST /federate/extension` was introduced with the payload `{tool_name, args}` — a single route for all extension tools, extensible when new LanguageProcessors are added. On the source side `mcp::call_tool` forwards the call through `dispatcher::dispatch_remote_value`. Both federation nodes must be upgraded to 0.8.1 synchronously — an old node will return 404 on the new route.

### Added

- **`ProcessorRegistry::resolve(explicit_language, repo_root)`** — a two-step processor resolution: first by the explicit `language` from the config, then a fallback to `detect()` by root markers. Used in the daemon worker and in the CLI `index` command. It unifies "indexing" behavior regardless of how it was launched.
- **The `mcp::ExtensionToolParams { tool_name, args }` struct** — the payload for the federation forward of extension tools.
- **Universal handler `handle_extension_tool` in `federation::server`** — finds the tool in the `extension_tools` snapshot, builds a `ToolContext` for a local repo, and calls `IndexTool::execute`. If there is no such tool on the target node (e.g., it was launched without bsl-extension) — it returns a `federation_error` with a clear message.

### Changed

- **`run_worker` takes `processor_registry: Option<Arc<ProcessorRegistry>>`** (the last parameter). `None` = universal-only build (`code-index.exe`); `Some(reg)` = `bsl-indexer.exe`. Used to resolve the processor of the current repo.
- **`runner::run` takes `processor_registry`** and threads it into `spawn_worker` (initial loop + `handle_reload`).
- **`cli::handle_daemon` takes `processor_registry`** — passed to `runner::run` when the daemon starts.
- **`Commands::Index` uses `resolve(None, root)`** instead of a direct `detect(root)` — identical behavior, but a single code path.

### Compatibility

The public API signature changes in `daemon_core::worker`/`runner`/`cli` are additive (new parameters at the end). The `bsl-indexer` 0.8.1 build is compatible with a v0.8.0 `daemon.toml` — no DB migration is needed (`apply_schema_extensions`'s DDL is idempotent).

**Federation:** both nodes must be upgraded at the same time. A pre-0.8.1 node will return `404 Not Found` on `POST /federate/extension`, and the new node will surface this as `federation_error`.

## [0.8.0] — 2026-05-05

**Phase 2 "content for code files"** — closing the main limitation of Phase 1. Before v0.8.0, `read_file` for `.py`/`.bsl`/`.rs`/`.ts` and other code files returned `category="code"` with an empty `content`. Now the content is stored in a new `file_contents` table (zstd compression, migration v4) and served on every call. Additionally: a new `grep_code` tool for regex search directly over code-file content, and an oversize mechanism for files larger than a configurable limit.

### Added

- **`file_contents` table (migration v4).** DDL: `file_contents(file_id INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE, content_blob BLOB, oversize INTEGER NOT NULL DEFAULT 0)`. Backfill is automatic — performed as part of `full_reindex` on the first run of v0.8.0 on an existing DB. Idempotent: a repeated call is safe (`INSERT OR REPLACE`). Estimate for UT (~15,665 `.bsl`, ~620 MB of sources): ~120 MB blob after zstd (~5×), a one-time backfill of ~1–2 minutes (pure I/O + zstd encode).

- **`read_file` works fully for code files.** For `.py`, `.bsl`, `.rs`, `.ts` and other AST languages the decompressed content from `file_contents` is returned. `category="code"`. The old logic for reading text files via `text_files` is unchanged.

- **Oversize mechanism.** Files larger than `max_code_file_size_bytes` (default **5 MB**) are stored with `oversize=1, content_blob=NULL`. AST parsing, FTS, and the call graph work fully for them. `read_file` for an oversize file returns a special response:
  ```json
  {
    "category": "code",
    "content": "",
    "oversize": true,
    "file_size": 8650240,
    "size_limit": null,
    "hint": "File is oversize: content is not stored in the index. Use get_function/get_class/grep_body."
  }
  ```

- **`stat_file` reports `oversize`** for code files: an `Option<bool>` field was added to the response. For text files it is always `null`.

- **The `max_code_file_size_bytes` limit configuration.** The hardcoded default is 5 MB (`DEFAULT_MAX_CODE_FILE_SIZE_BYTES` in `crate::daemon_core::config`). Overridden in `daemon.toml`:
  ```toml
  [indexer]
  max_code_file_size_bytes = 5242880   # global override (5 MB)

  [[paths]]
  path = "C:/RepoUT"
  max_code_file_size_bytes = 10485760  # for this repo — 10 MB
  ```
  Priority: per-path → the `[indexer]` section → the 5 MB default. The selection logic is the helper `PathEntry::effective_max_code_file_size(&IndexerSection)`.

- **New MCP tool `grep_code` (Phase 2 bonus).** Regex search over code-file content — it closes the blind spot of `grep_body` (which searches only in function/class bodies). The data source is the `file_contents` table (zstd-decode on the fly in Rust; SQL does a pre-filter by path/language). The parameters are identical to `grep_text`: `regex`, `path_glob?`, `language?`, `limit?`, `context_lines?`. Files with `oversize=1` are skipped. Storage method: `Storage::grep_code_filtered(regex, path_glob, language, limit, context_lines, max_total_bytes) -> Vec<GrepTextMatch>`. The pub function signature: `pub async fn grep_code(entry, regex, path_glob, language, limit, context_lines)`.

- **Federation route `/federate/grep_code`** — additive, does not break existing clients. A request to an old node (< 0.8.0) returns `404` — expected behavior; both nodes must be upgraded synchronously to use `grep_code` in federation.

### Changed

- **`Indexer::write_code_to_db`** — added a trailing parameter `raw_content: Option<&str>`. If set — the content is stored in `file_contents` (zstd encode). Internal API.
- **`Storage::read_file_text`** — added a trailing parameter `size_limit_bytes: Option<i64>`. Used to fill the `size_limit` field in the oversize response. The MCP layer passes `None`.
- **The `ParsedFile::Code` enum variant** — added a `raw_content: String` field.
- **`worker::run_worker`** — added an `IndexerSection` parameter (last). Inside, the effective limit is computed and written into `IndexConfig.max_code_file_size_bytes`.
- **`runner::spawn_worker`** — added an `IndexerSection` parameter, threaded into `run_worker`.

### Security

- **Protection against a zstd bomb.** All decompression calls in `read_file_content` and `grep_code_filtered` go through the private helper `Storage::decode_zstd_safe(blob) -> Result<Vec<u8>>`. It uses a streaming decoder with `io::Read::take(limit + 1)` — if the decompressed size exceeds `FILE_CONTENTS_MAX_DECOMPRESSED_BYTES` (256 MB), it returns an error and allocates no more RAM. 256 MB is well above any valid code file (5 MB default × ~5× zstd = ~25 MB; with headroom in case an operator raises `max_code_file_size_bytes`).

### Fixed

- **Backfill now works for all code files on a stable DB (a bug fix for the first preview build).** Previously the backfill was embedded in the processing of `metadata_updates` in `full_reindex` — a container of files with a changed mtime/file_size but the same content_hash. On a "stable" DB (nobody touched files since the last indexing) `metadata_updates` is empty, so the backfill **did not run for UT/BP_SS/ZUP** — only repos with actually changed files were populated (BP_TDK got ~15 files out of 90K). Fix: moved into a **separate phase** `Stage 6` after removing stale entries, via the new Storage method `list_code_files_without_content() -> Vec<(file_id, path)>`. Now the backfill hits all code files that have no record in `file_contents` AND no record in `text_files`, regardless of whether the mtime changed. Real figures on the rag VM after the fix: UT 32599/32599 in 31.7 s, BP_SS 37535/37535 in 37.9 s, ZUP 19066/19066 in 17.5 s, BP_TDK likewise.
- **Backfill in batches instead of one mega-transaction.** For a 90K-file repo, the whole phase inside a `BEGIN TRANSACTION` without a commit would bloat the WAL to many GB. An intermediate `commit_batch + begin_batch` every `batch_size.max(500)` files keeps the WAL within reasonable bounds.

### Compatibility

- **MCP API with no breaking changes.** All new response fields are `Option<...>` or `default false`; old clients will not break. The change to `read_file` for code files (returning real content instead of empty) is an improvement, not a breaking change.
- **DB schema** — migration v4 is idempotent and safe on an existing v0.7.x DB. Rolling back to v0.7.x simply ignores the new table — both versions are compatible for reading old data.
- **Storage API changed incompatibly** for direct users of the `code-index-core` crate: `Indexer::write_code_to_db`, `Storage::read_file_text`, `worker::run_worker`, `runner::spawn_worker` — new parameters. New public methods were also added: `Storage::upsert_file_content`, `read_file_content`, `has_file_content`, `delete_file_content`, `get_file_id_by_path`, `has_text_file`, `list_code_files_without_content`, `grep_code_filtered`. There are no external callers in the public API, but if there is private code with direct calls — update it.
- **Federation** — the new route `/federate/grep_code` is additive. **Both federation nodes must be upgraded synchronously** to use `grep_code` in federation (otherwise the old node returns 404 on this route). The general `v0.7.0+` principle remains.
- **`grep_code` skips oversize files** — this is a documented limitation, not a bug. For such files `get_function`/`get_class`/`grep_body` over AST data still work.

## [0.7.3] — 2026-05-04

**Bug fix**: extension tools (`get_object_structure`, `get_form_handlers`, and others provided via `LanguageProcessor::additional_tools()`) **were not registered in `tools/list`** when the server runs in federated mode (`serve.toml` present). For users in mono mode everything was correct.

### Fixed

- **`CodeIndexServer::from_federated`** now takes two extra parameters: `registry: Option<ProcessorRegistry>` and `local_languages: BTreeMap<String, String>`. The processor registry is stored in `Self.registry`, and right after building the repo map `extension_tools = collect_extension_tools(&active_languages, &reg)` is computed. Previously the federated constructor always initialized `extension_tools = Vec::new()` and `registry = None`, which zeroed out the conditional registration at serve start and on subsequent `reload_extensions` (`registry_opt = None` → `new_tools = Vec::new()`).
- **`local_languages` for federation**: the `alias → language` map is collected from the local `daemon.toml` (`PathEntry::effective_alias()` + `PathEntry.language`) and set into `RepoEntry.language` for **local repos**. Without this, `collect_active_languages` did not find bsl/python/rust in the federation scenario (`federation::repos::merge` returns a `FederatedRepo` without the language field). Remote repos via federation still arrive without a language — for them extension tools are registered only if the same language is active on a local repo on this node.
- **Behavioral consequence**: on the `bsl-indexer` build in federated mode, `tools/list` now returns 22 tools instead of 17 — `find_path`, `get_event_subscriptions`, `get_form_handlers`, `get_object_structure`, `search_terms` (the 5 BSL tools from `bsl-extension`) are added.

### Compatibility

- **MCP API unchanged** — the tool list changes only in the federated mode of the `bsl-indexer` build when there is at least one local repo with `language = "bsl"` in `daemon.toml`. The client sees this as a regular `notifications/tools/list_changed`.
- **DB schema with no migrations.**
- **Federation requires a synchronous upgrade of both nodes** — the general v0.7.0+ principle remains (the cross-node API did not change, but the useful effect is achieved only when both nodes are built at 0.7.3).
- The `from_federated` signature changed incompatibly. There are no external calls in the public code-index API (it was used only from `cli::run`), but if you have private code with a direct call — update it.

## [0.7.2] — 2026-04-29

**Bug fix to v0.7.1**: the HTML parser was not picked up in repos with an explicit `language="..."` (python/rust/bsl, etc.) in `daemon.toml`. An attempt to index `.html` files produced the error `No parser for extension: html`.

### Fixed

- **`ParserRegistry::from_languages`** now registers the HTML parser **always**, in addition to the specified `language`. HTML is a universal asset (templates, generated docs, sphinx output, vue/svelte SFCs, etc.) that occurs in repos of any "primary language" and is not listed separately in `daemon.toml`. The `"html" => …` branch in the `match` is kept as an explicit no-op for documentation; the actual registration happens after the `match`, unconditionally.
- This fixes the bug on `code-index index <repo> --force` for python/rust/bsl repos with html files.

### Compatibility

- MCP API unchanged.
- DB schema unchanged.
- A 0.7.1 binary without this fix may remain in production — html files simply will not get AST records until 0.7.2 + reindexing.

## [0.7.1] — 2026-04-28

**HTML parser** via tree-sitter — added **at a user's request**. Before 0.7.1, `.html` was indexed only as a text file (FTS+regex+read_file). Now it is a full AST with extraction of structural entities: elements with id, forms, input fields, links, inline scripts/styles, CSS classes. Backward compatibility is preserved: search_text/grep_text/read_file for html keep working via **dual indexing** (text_files + AST).

### Added

- **New parser** `crates/code-index-core/src/parser/html.rs` (~430 lines) based on `tree-sitter-html` 0.23. Supports `.html` and `.htm`. Registered in `ParserRegistry::new_all()` and `from_languages()`.
- **HTML semantics → code-index tables mapping:**

  | HTML construct | → | Table | Name |
  |---|---|---|---|
  | `<element id="X">…</element>` | `classes` | `X` (body=outerHTML, bases=tag_name) |
  | `<form id|name="X">` | `classes` | `form_X` (bases="form") |
  | `<form>` without id/name | `classes` | `form_<line>` |
  | `<input/select/textarea name="Y">` | `variables` | `Y` (value=type/value attribute) |
  | `<a href="URL">` | `imports` | `module=URL`, `kind="link"` |
  | `<link href="URL" rel="X">` | `imports` | `module=URL`, `kind=X` (or "stylesheet") |
  | `<script src="URL">` | `imports` | `module=URL`, `kind="script"` |
  | `<img/iframe/video/audio/source/embed src="URL">` | `imports` | `module=URL`, `kind=tag_name` |
  | `<script>…inline JS…</script>` | `functions` | `inline_script_<line>` (body=content) |
  | `<style>…inline CSS…</style>` | `functions` | `inline_style_<line>` (body=content) |
  | The `class="foo bar baz"` attribute | `variables` | `class:foo`, `class:bar`, `class:baz` (one record each) |

- **Dual indexing**: for languages from `is_dual_indexed_language()` (in 0.7.1 — only `html`), a record in `text_files` is created in parallel during indexing. This keeps `search_text`/`grep_text`/`read_file` working for HTML files alongside the new structured queries (`get_class("cart")`, `find_symbol("submitOrder")`, `get_imports(module="bootstrap.css")`, etc.). Implemented via a new field `text_for_fts: Option<String>` in `ParsedFile::Code` + an extra parameter `text_for_fts: Option<&str>` in `Indexer::write_code_to_db`.
- **File extensions**: `("html", "html")` and `("htm", "html")` moved from TEXT_EXTENSIONS to CODE_EXTENSIONS (`indexer/file_types.rs`). Added the public function `is_dual_indexed_language(language: &str) -> bool`.
- **13 unit tests** for the html parser (`parser/html.rs::tests`): id element, a form with id/name/without both, input/select/textarea, link/script/img imports, inline script, inline style, the classes attribute, tolerance to Jinja templates, empty HTML, nested elements. Plus `file_types::html_is_code_with_dual_indexing` to check the categorization.
- **Tolerance to templating engines**: `{{ … }}` and `{% … %}` are parsed as text content without crashing. Structural elements around them are extracted normally.

### Changed

- **`Indexer::write_code_to_db` signature**: added a trailing parameter `text_for_fts: Option<&str>`. An internal API, not MCP-visible. All known callers (worker.rs:380 for html, worker.rs:416 for xml_1c) are updated.

### Compatibility

- **MCP API unchanged** — no new tools, no new parameters. After reindexing, html files automatically become available to the existing tools: `get_class`, `find_symbol`, `search_function`, `get_imports`, `grep_body` + `search_text`, `grep_text`, `read_file`, `list_files`, `stat_file` keep working.
- **DB schema with no migrations.** The existing files / functions / classes / imports / variables / text_files tables are used. The dual insert for html goes through the former `insert_text_file`.
- **Federation with no new routes.** An internal mechanism; both nodes must be the same version (the 0.7.0 requirement still applies).
- **Reindexing:** on the first run of v0.7.1, the daemon finds the mtime of html files unchanged relative to the last indexing and **will not** reindex them (the mtime pre-filter from v0.4.0). To get new structured records for already-indexed html, you need either an explicit re-index (`code-index index <repo>`) or a change to the file mtime. Recommended on the first upgrade to 0.7.1 — a one-time full re-index of repos with html files.

## [0.7.0] — 2026-04-28

**Phase 1 "read-only tools"** — closing gaps in code-index so that a remote repo over federation works "like a local one" for most reconnaissance and reading tasks. A read-only release: the DB schema is untouched, no reindexing is needed, backward compatibility is preserved.

### Added

- **`stat_file(repo, path)`** — metadata of a single file from the `files` table. Returns `{exists, path, language, size, mtime, lines_total, content_hash, indexed_at, category}`. `category` — `"text"` (content available via `read_file`) or `"code"` (Phase 1 does not store content for AST languages).
- **`list_files(repo, pattern?, path_prefix?, language?, limit?)`** — a flat list of files with filtering. `pattern` — glob (`**/*.py`), `path_prefix` — a prefix (`src/auth/`). Returns `[{path, language, lines_total, size, mtime}]`. No separate `tree` endpoint — the structure is reconstructed from path strings.
- **`read_file(repo, path, line_start?, line_end?)`** — the content of a **text file** (yaml/md/json/toml/xml/sh/INI/CSV/SQL, etc.) via the `text_files` table. `line_start`/`line_end` are 1-based, inclusive. Soft-cap **5000 lines OR 500 KB** (whichever comes first) with a `truncated=true` flag. Hard-cap **2 MB** even with a range (rejection). For code files — `category="code"` and an empty `content` (to be closed in Phase 2). Returns `{content, lines_returned, lines_total, truncated, indexed_at, category}`.
- **`grep_text(repo, regex, path_glob?, language?, limit?, context_lines?)`** — regex search over text-file content via REGEXP. It closes the FTS5 gap with special characters (dots, parentheses, escapes). `path_glob` or `language` is desirable — otherwise it's a full scan, and the default limit is lowered to 100. `context_lines` — N lines before/after a match. A hard-cap on the total output size (1 MB).
- **`path_glob` parameter** in `search_function`, `search_class`, `get_function`, `get_class`, `find_symbol`, `search_text`, `grep_body`. It narrows the output by file path. Implementation — a post-filter via the `globset` crate after the SQL fetch. The SQL LIMIT is increased up to 5× (but no more than 500) so the filter does not leave an empty result.
- **`context_lines` parameter** in `grep_body` — N lines of context around the first up to 3 matches. Via the new `Storage::grep_body_with_options`. The existing `grep_body` without the context parameter works as before (backward compatibility for cli.rs/tests).
- **A hard-cap on the total response size** in `grep_body` (with context_lines) and `grep_text` — 1 MB. Protection against overflowing the model context on a wide regex with a large context_lines.
- **`Storage::get_path_by_file_id`** — a public method for the post-filter in the MCP layer.
- **`storage::normalize_glob`** (pub(crate)) — `**` → `*` for compatibility with the usual glob syntax (SQLite GLOB and `globset` already understand `*` as multi-char + `/`).
- **Federation routes:** `/federate/stat_file`, `/federate/list_files`, `/federate/read_file`, `/federate/grep_text`. Existing routes are extended with new parameters in the Params structs.
- **20 new unit tests** for Phase 1: `normalize_glob`, `slice_with_caps` (4 cases), `stat_file_meta` (3 cases), `list_files_filtered` (3 cases), `read_file_text` (4 cases), `grep_text_filtered` (3 cases), `grep_body_with_options`, `get_path_by_file_id`.

### Compatibility

- **MCP API with no breaking changes.** All new parameters are `Option<...>`, optional. Old clients unaware of `path_glob`/`context_lines` work as before.
- **Storage API with no breaking changes.** All existing methods (`search_functions`, `search_classes`, `search_text`, `grep_body`, `find_symbol`) kept their signature. New functionality is in new methods (`grep_body_with_options`) and in the post-filter in the MCP layer.
- **DB schema unchanged.** No migrations, no reindexing required.
- **Federation with no breaking changes.** New routes are additive. **Important:** both federation nodes (Windows and the VM) must be upgraded to 0.7.0 at the same time — otherwise calling new tools on an old node yields a 404.

### Known limitations of Phase 1

- **`read_file` for code files** (.py/.rs/.bsl/.ts/...) returns `category="code"` and an empty `content`. To be closed in Phase 2 with migration v4 + a zstd-compressed blob in the new `file_contents` table.
- **Files without an extension** (Dockerfile, Makefile, Jenkinsfile, .gitignore, LICENSE) are not indexed by the walker — a blind spot for DevOps repos. A deliberate limitation.
- **Binary 1C formats** (.epf, .erf, .cfe, .cf) are not indexed. Unpacking happens in an external pipeline.

## [0.6.1] — 2026-04-26

The rc7 technical debt is closed: a per-host port for the remote `code-index serve` used by federate forwarding. Up to and including 0.6.0 the remote node's port was hardcoded in `client.rs::DEFAULT_REMOTE_PORT = 8011`, and two serve nodes on the same machine inevitably overlapped in the connection pool — a pair was keyed only by IP. The change is fully backward compatible: a `serve.toml` without a `port` field works exactly as before (the default 8011 is used).

### Added

- **The `port: Option<u16>` field** in the `[[paths]]` section of `serve.toml` (`federation::config::ServePathEntry`). Optional, default — `DEFAULT_REMOTE_PORT` (8011). The `effective_port()` method returns the explicit value or the default. Validation forbids `port = 0` (reserved).
- **The `port: u16` field** in `federation::repos::FederatedRepo` and `mcp::RepoEntry` — mandatory, filled from `ServePathEntry::effective_port()` at `merge`. For local records the value is informational (forwarding is not used for them).
- **Tests:** `port_field_is_optional_and_defaults_to_remote_port`, `port_field_parses_when_explicit`, `zero_port_fails_validation` (config.rs), `port_defaults_when_not_set_and_propagates_when_set` (repos.rs), `pool_creates_separate_clients_for_different_ports_on_same_ip` (client.rs).

### Changed

- **`RemoteClientPool` keys clients by `(String, u16)`** instead of `String`. The signature is `get_or_create(&self, ip: &str, port: u16)`. The `default_port` field was removed: the pool itself does not fix a port; it is supplied per call from `RepoEntry::port`. `RemoteClientPool::new(timeout)` now takes only the timeout.
- **`dispatcher::dispatch_remote` and `dispatch_remote_value` take `port: u16`** between `ip` and `tool`. All 13 tool handlers (`mcp/mod.rs`) and `tools::remote_stats` are updated — they thread `entry.port`.

### Compatibility

- **A `serve.toml` without a `port` field** parses as before; `DEFAULT_REMOTE_PORT` is used for all records. No migrations are required.
- **The external MCP API is unchanged** — the `port` field does not appear in any tool call or tool result. It is a serve configuration detail and does not leave the process.
- **The caching proxy (planned)** will read `serve.toml` to determine which `port` to use for each repo — now this is a single source of truth.

## [0.6.0] — 2026-04-26

A large release: a workspace refactor, the new `bsl-indexer` binary with full 1C specificity, multi-config processing of a single repo with base/ + extensions/, parsing of `ConfigDumpInfo.xml` for debug UUID identifiers, optional LLM enrichment of procedures via the `enrichment` cargo feature, and protection against model drift via `embedding_signature`. All of it was done on the `workspace-refactor` branch (24+ commits, 249 tests).

### Added

- **Cargo Workspace**. The single mono-crate is split into 4 crates with clear areas of responsibility:
  - `code-index-core` (lib, publish=true) — the universal core: file scanner, tree-sitter parsers (Python/Rust/Go/Java/JS/TS/BSL), the SQLite schema, the MCP server, federation.
  - `code-index` (bin, publish=true) — the public binary without 1C specifics.
  - `bsl-extension` (lib, publish=false) — 1C specifics: XML parsers for the dump, the BSL call graph, the MCP tools `get_object_structure`/`get_form_handlers`/`get_event_subscriptions`/`find_path`/`search_terms`, optional LLM enrichment.
  - `bsl-indexer` (bin, publish=false) — the private binary = core + bsl-extension. Used on the rag VM for indexing 1C configurations.

- **Conditional MCP-tool registration**. At startup the MCP server reads `daemon.toml`, for each `[[paths]]` determines the `language` (explicitly or auto-detected by the repo root), collects the set of active languages, and registers ONLY the tools from matching `LanguageProcessor`s. If there is no BSL repository at all, the 1C tools do not appear in `tools/list` at all. A `notifications/tools/list_changed` notification is sent when `daemon.toml` is edited (file-watch with a 500ms debounce via `notify-debouncer-full`).

- **`bsl-indexer` — a new separate binary** for 1C configurations. The release CI builds it for Windows/Linux/macOS (with the `enrichment` feature for production). Detailed instructions — in [docs/bsl-indexer.md](docs/bsl-indexer.md); deployment to the rag VM — [docs/deploy-vm-rag.md](docs/deploy-vm-rag.md).

- **Multi-config layout** (`<repo>/base/Configuration.xml` + `<repo>/extensions/<EF_*>/Configuration.xml`). `BslLanguageProcessor::detects()` now recursively (depth ≤ 2) finds any `Configuration.xml`. `index_metadata_objects` traverses ALL configurations found in the tree and merges their objects into a single table (objects borrowed in extensions are skipped via `INSERT OR IGNORE`). `extension_name` is stored for each module — a filter between base and CFE is available via a query.

- **The `metadata_modules` table** with the UUID triple for the 1C platform debugger (`dbgs-debug` setBreakpoint):
  - `object_id` — the object/form UUID from the `uuid` attribute of the root element in its XML.
  - `property_id` — the UUID of the module type (Module/ManagerModule/FormModule/...) — a platform constant; the dictionary is in `module_constants.rs`.
  - `config_version` — a hash of the version from `ConfigDumpInfo.xml` (a separate parser). It changes on every configuration change.

  This triple lets agents set breakpoints by a human-readable module name without touching a live infobase. On the UT scale ~8K modules, on BP configurations ~10K.

- **MCP tool `search_terms`** — the third semantic search channel (after `search_function` and the future `semantic_search`). It uses FTS5 on the `procedure_enrichment.terms` column populated by LLM enrichment. Supports FTS syntax (AND, OR, NOT, "exact phrase", prefix*). NULL records (non-enriched procedures) are simply not found — this is progressive enhancement, not a bug.

- **The `bsl-indexer enrich [--path P] [--limit N] [--reenrich]` subcommand** under the `enrichment` cargo feature. An HTTP client to an OpenAI-compatible chat-completions endpoint (OpenRouter / Ollama / any compatible). Parallel processing via `tokio::task::JoinSet` with a configurable `batch_size`. Protection against model drift via `embedding_meta.enrichment_signature` — when the model in the config changes, a warning is printed suggesting `--reenrich`.

- **The `[enrichment]` section in `daemon.toml`** — provider, endpoint URL, model name, the name of the API-key env variable, batch size, the prompt template. Off by default (the feature is optional).

- **Language auto-detect with a write-back into `daemon.toml`** via `toml_edit` (preserves comments). Algorithm: `Configuration.xml` → bsl, `pyproject.toml`/`setup.py` → python, `Cargo.toml` → rust, `package.json` → javascript/typescript, otherwise by the prevailing extension. If the heuristic does not fire — a warning to the log and a skip (no silent fallback).

- **`Storage::apply_schema_extensions(extensions: &[&str])`** — the point of applying additional DDL from LanguageProcessors. Called once on the first open of a repo's DB for a language that needs specific tables.

- **`LanguageProcessor::index_extras(repo_root, &mut storage)`** — a hook for specific post-processing after the main indexing (e.g., parsing XML and populating the `metadata_*` tables). The default implementation is a no-op.

### Changed

- **A parallel run of 4 repos on the rag VM (8-core Intel Xeon)** — the total time of a full indexing of UT + BP_1 + BP_2 + ZUP dropped from ~8m30s (sequential) to **3m11s** (×2.7 speedup). The bottleneck is the single-thread SQLite FTS rebuild in each process; the disk (NVMe) does not block, and the cold↔warm cache difference is only ~5 s.

- **Protection against cascade transaction errors**. In each `index_*` function and in `build_call_graph` an idempotent `ROLLBACK` before `BEGIN` was added — if the previous function left an open transaction, the next one correctly closes it instead of crashing with "cannot start a transaction within a transaction".

- **`config_watch::run_watch` — an initial seeding of active_languages at startup**. Before the fix, a client connecting BEFORE the first file change saw only core tools (because in mono mode `RepoEntry.language=None` when loaded via `cli::run`). After the fix — the first `tools/list` immediately contains the correct set for the current `daemon.toml`.

- **CI setup**. `.github/workflows/release.yml` now builds 6 artifacts per tag: `code-index` × {Windows, Linux, macOS} + `bsl-indexer` × {Windows, Linux, macOS} (with `--features enrichment`). The cargo registry/git/target cache is keyed by `${{ runner.os }}-${{ matrix.target }}-${{ matrix.crate }}`.

### Security

- **`.mcp.json` excluded from tracking** via `.gitignore` + `git rm --cached`. The file is a local configuration; it contains SSH paths and URLs of a specific host and has no place in the repo.

- **Internal IPs replaced with RFC 5737 doc-IP** (`192.0.2.0/24`) in all federation tests, comments, and config examples. The specific rag VM addresses in the deployment instructions — with the placeholder `<vm-rag-ip>`.

### Empirical production verification (stages 7–8)

- **Conditional registration on Claude Code 2.1.120** — `tools/list` correctly contains 18 tools (5 BSL + 13 core) when there is a BSL repo in `daemon.toml`, and 13 tools (core only) without one.
- **`notifications/tools/list_changed` is IGNORED by Claude Code on 2.1.120** — the bug [anthropics/claude-code#13646](https://github.com/anthropics/claude-code/issues/13646) is confirmed empirically. The workaround is a manual `/mcp Reconnect`. Reconnect (issue #33779) on 2.1.120 already re-reads `tools/list` correctly.
- **The rag VM (Linux, 8 cores, NVMe)** — RepoUT 53.6 s cold cache, 57.7 s warm, a 5 s difference = the disk is not the bottleneck. A parallel indexing of all 4 repos in 3m11s on 8 cores × ~2 rayon cores per process.

### Documentation

- **[docs/bsl-indexer.md](docs/bsl-indexer.md)** — the user guide for `bsl-indexer`: what it can do, how to build with/without the `enrichment` feature, how to set up enrichment with OpenRouter / Ollama, and the MCP-client limitations with a workaround.
- **[docs/bsl-indexer-architecture.md](docs/bsl-indexer-architecture.md)** — the full architectural spec of the workspace refactor with the rationale for decisions.
- **[docs/deploy-vm-rag.md](docs/deploy-vm-rag.md)** — a step-by-step deployment guide for the VM (installing the Rust toolchain, copying the sources, configuring daemon.toml, the systemd unit, the A/B protocol for comparison with pg_indexer).
- **[deploy/systemd/bsl-indexer-daemon.service](deploy/systemd/bsl-indexer-daemon.service)** — a ready systemd unit with resource limits and protection against writing outside the allowed directories.

## [0.5.0-rc6] — 2026-04-25

### Added

- **Federated `code-index serve` architecture** (modeled on `1c-router`/`mcp__1c__`). A single serve process serves a registry of repositories from several machines: for each tool call with `repo=X` the local serve looks at the ip — if it matches `[me].ip`, it reads the local SQLite, otherwise it makes an HTTP call to the remote serve. The source of truth for each repo is on a single machine (this is a proxy, not replication).

  **New config** [`serve.toml`](src/federation/config.rs) — global, identical on all nodes (rolled out via a shared git repo `code-index-config`):

  ```toml
  [me]
  ip = "192.0.2.10"
  # token = "..."   # optional, not validated in rc6 (a stub for rc7)

  [[paths]]
  alias = "ut"
  ip = "192.0.2.50"

  [[paths]]
  alias = "dev"
  ip = "192.0.2.10"
  ```

  `daemon.toml` stays local (only this machine's paths, no schema changes).

- **An internal endpoint `POST /federate/<tool_name>`** ([`src/federation/server.rs`](src/federation/server.rs)) — the receiving side of forwarding. The request body is JSON matching our `*Params` structs exactly. The response is whatever the local tool handler would have returned. `/federate` lives on the same axum router as `/mcp` and is protected by a shared whitelist middleware.

- **IP whitelist middleware** ([`src/federation/whitelist.rs`](src/federation/whitelist.rs)). serve binds to `[me].ip` (not to `127.0.0.1`, not to `0.0.0.0`) — the port is active only on one interface. The allowed peer IPs are from `{all [[paths]].ip} ∪ {127.0.0.1, ::1}`. A foreign peer → `403 {"error":"forbidden","peer":"..."}`.

- **A parallel fan-out in `get_stats(repo=None)`** ([`src/mcp/tools.rs`](src/mcp/tools.rs)) via `tokio::task::JoinSet`. Each remote repo is polled with a 5 s timeout; unreachable ones are returned as `{"repo":"...","status":"unreachable","error":"..."}` without blocking the rest.

- **The `--serve-config <FILE>` flag on `code-index serve`**. If the flag is not set — `$CODE_INDEX_HOME/serve.toml` is searched. If there is no file — serve works as rc5 (mono mode, bind 127.0.0.1, no whitelist). With `transport=stdio` or an explicit `--path`, federation is not activated.

  ```bash
  # Federated mode (rc6+):
  code-index serve --transport http --port 8011

  # Compatible rc5 mode (mono):
  code-index serve --transport http --port 8011 --path ut=C:/RepoUT
  ```

- **A pool of reusable HTTP clients** ([`src/federation/client.rs`](src/federation/client.rs)) — one `reqwest::Client` per remote IP, lazy init via `RemoteClientPool::get_or_create`. Timeout 5 s; idle pool 60 s.

### Changed

- **`RepoEntry` now stores `ip` and `is_local`**, and the `root_path` and `storage` fields are wrapped in `Option` (`None` for remote). The old constructors `open_readonly_multi` / `open_readonly` / `with_storage` set `is_local=true`, `ip="127.0.0.1"` — backward compatibility for tests and mono mode.

- **`serve_http` takes optional `federate_router` and `whitelist`**. If passed — `Router::merge` for `/federate/*` and `axum::middleware::from_fn_with_state` for the whitelist. The listener now uses `into_make_service_with_connect_info::<SocketAddr>()` — without it the peer IP is not extracted in the middleware.

- **`--host` became `Option<String>`**. If set — CLI takes priority; otherwise, if serve.toml is present — `[me].ip`, otherwise `127.0.0.1` (the rc5 default).

### Loop protection

- **No headers** like `X-Forwarded-Already`. Protection is static, via the config: each node knows its own `[me].ip` and forwards only if `repo.ip != own_ip`. On a config mismatch (`A: X→B`, `B: X→A`) the request fails by the 5s timeout with a clear error.
- The `/federate/get_stats` receiver without `repo` limits the fan-out to its own local repos (it does not recursively traverse to others) to exclude a loop between nodes.

### Roadmap (outside rc6)

- Creating the `code-index-config` git repo with a `serve.toml` template — an operational task.
- A Linux binary + a systemd unit for deployment to VM 200.
- `[me].token` authorization — a Bearer header in `/federate/*`, checked in the whitelist middleware. The field is already parsed in the serve.toml schema.
- A HEAD ping to the remote nodes in `health` — a low-priority feature.
- Hot-reload of `serve.toml` without a restart (`POST /reload` for serve).

## [0.5.0-rc5] — 2026-04-22

### Added

- **HTTP transport on `code-index serve`** via rmcp's `StreamableHttpService`. A single process serves all repositories under `mcp-supervisor`; clients connect to a shared URL without copying `--path` into each `.mcp.json`.

  ```bash
  # stdio (per-session, as before)
  code-index serve --path ut=C:/RepoUT --path bp=C:/RepoBP

  # http (shared process)
  code-index serve --transport http --port 8011 --config C:/tools/code-index/daemon.toml
  ```

  The client `.mcp.json`:
  ```json
  "code-index": { "type": "http", "url": "http://127.0.0.1:8011/mcp" }
  ```

  Implementation — [`src/main.rs`](src/main.rs) `serve_http`: `StreamableHttpService::new(factory, LocalSessionManager, StreamableHttpServerConfig::default())`, mounted into `axum::Router::nest_service("/mcp", svc)`. The factory clones the already-built `CodeIndexServer` (it is `Clone`), so all sessions share a common set of open SQLite databases.

- **Multi-repo in a single serve process**. `--path` now takes `alias=dir` and may be specified multiple times — each tool call passes a `repo=<alias>` parameter to select the repository. Without `=` — the old `alias=default` contract. The tool parameters got a `repo: String` field; the internal `RepoEntry` struct holds an open read-only `Storage` and `root_path` per repo.

- **The `alias` field in `[[paths]]` of daemon.toml** — [`src/daemon_core/config.rs`](src/daemon_core/config.rs) `PathEntry::alias: Option<String>`. If not set — the alias is computed via `PathEntry::effective_alias()` from the last path segment (lowercase, spaces → `_`). The daemon ignores the field; only `code-index serve --config ...` uses it when building the repo list. Old configs without `alias` keep working (`#[serde(default)]`).

- **The `--host`, `--port`, `--config` flags on `serve`**. `--config` points at `daemon.toml` — the list of repos and aliases is taken from there. CLI `--path` takes priority over the config. The default port is 8011 (the next free one in the mcp-supervisor range: 8001/8002/8007/8010).

### Dependencies

- Enabled the `rmcp/transport-streamable-http-server` feature (it pulled in `transport-streamable-http-server-session`, `server-side-http`, and transitively — `uuid`, `sse-stream`). `axum` and `tower` were already in deps for the daemon's health endpoint.

## [0.5.0-rc4] — 2026-04-17

### Fixed

- **The daemon crashed when the console was closed on Windows**. `code-index` is built as a console-subsystem application: when launched in a user session (a Scheduled Task with `LogonType=Interactive`, a manual call from `cmd`/PowerShell), the process gets a console window and becomes its child. Closing the window sends `CTRL_CLOSE_EVENT`, and the daemon dies with it. For the standard installation via `scripts/install-daemon-autostart.ps1` this meant the console window popped up at logon, and closing it stopped the indexing.

  **Fix**: in [`src/main.rs`](src/main.rs), `handle_daemon` for `daemon run` on Windows performs a self-detach — it restarts itself with the `DETACHED_PROCESS | CREATE_NO_WINDOW` flags, sets the environment variable `CODE_INDEX_DAEMON_DETACHED=1`, and terminates the parent process. The detached clone runs without a console and survives the closing of any parent session. On Unix the self-detach is not performed — daemonization is managed by `systemd`/`launchd`.

  The implementation uses only `std::os::windows::process::CommandExt::creation_flags` and adds no new dependencies.

## [0.5.0-rc3] — 2026-04-17

### Fixed

- **A race condition on editors' atomic save**. Editors (VS Code, IDEs, `git`) save files atomically: first they write to a temporary `<name>.tmp.<pid>.<ts>`, then rename it to the target file. The watcher via `ReadDirectoryChangesW` managed to see a `Create` event on the `.tmp` file, but by the time `hasher::file_hash()` was called the file had already been renamed. A wall of errors poured into the logs of the form `file_hash \\?\...\.mcp.json.tmp.10296.1776427368309: The system cannot find the file specified. (os error 2)`.

  **Fix**: in [`daemon_core/worker.rs`](src/daemon_core/worker.rs), `apply_event` on `io::ErrorKind::NotFound` from `file_hash` now silently exits the handler. Only real errors are logged (permission denied, read error, etc.).

### Added

- **The `exclude_file_patterns` field in `.code-index/config.json`** — glob patterns of file names to exclude from indexing. It complements the existing `exclude_dirs`:

  ```json
  {
    "exclude_dirs": [".vscode", "experimental"],
    "exclude_file_patterns": ["*.tmp.*", "*.bak", "*.orig", "*.swp"]
  }
  ```

  Patterns are matched by **basename** (the file name without the path). They are applied:
  - in [`watcher.rs`](src/watcher.rs) — events from files matching a pattern are discarded before the `file_hash` call;
  - in [`indexer/mod.rs`](src/indexer/mod.rs) `collect_candidates` / `collect_candidates_standalone` — the file is excluded from the WalkDir traversal before categorization.

  The glob syntax is via the `globset` crate. Invalid patterns are logged and skipped (they do not break the startup).

### Dependencies

- Added `globset = "0.4"`.

## [0.5.0-rc2] — 2026-04-17

### Fixed

- **WAL files grew to tens of GB in production**. After a day of work on our stand with 13 indexed folders (5 large 1C repos + 8 MCP modules) the WAL files took ~43 GB while the total size of the main DBs was ~16 GB:

  | Repo | `index.db` | `index.db-wal` (before the fix) |
  |------|-----------|---------------------------|
  | RepoBP_2 | 4.7 GB | **19 GB** |
  | RepoUT | 2.1 GB | **17 GB** |
  | RepoZUP | 5.1 GB | 5.1 GB |
  | dbgs-debug | 1.4 GB | 1.4 GB |

  Free space on the system drive shrank by ~45 GB in a day.

  **The cause**, found by code analysis:
  1. `PRAGMA wal_autocheckpoint=500` (added in v0.5.0-rc1) moves pages from the WAL into the main DB, but **does not reduce the physical WAL file** — only an explicit `PRAGMA wal_checkpoint(TRUNCATE)` does that.
  2. Under a bulk load (the initial reindex of 90K files, frequent watcher batches) the checkpoint does not keep up with the write rate.
  3. The worker called `Storage::flush_to_disk()` via `Connection::backup()` after every batch — in disk mode (and the worker is always in it after a reopen) this is a useless copy of the DB onto itself, and the WAL does not shrink.

  **Fix**:
  - Added a `Storage::checkpoint_truncate()` method — a wrapper over `PRAGMA wal_checkpoint(TRUNCATE)` that actually collapses the WAL.
  - In `worker.rs` after the initial reindex (when the worker is guaranteed to be in disk mode) — a mandatory `checkpoint_truncate`. This is the "fattest" source of WAL.
  - In the watcher loop after `commit_batch` — `flush_to_disk` replaced with `checkpoint_truncate`. On graceful shutdown — the same.

  **The result of the check on the same 13 folders**: the WAL stays at **0 bytes** after the initial reindex and after file edits through the watcher. ~48 GB freed.

## [0.5.0-rc1] — 2026-04-17

A major rework of the architecture: splitting into a **background writer daemon** and **MCP readers**.

### Breaking changes

- **`code-index serve` is now read-only**. It no longer indexes and does not hold a watcher — it only connects to the DB maintained by a separate daemon. If the daemon is not running or the folder is not in its config, a tool call returns a structured response `{"status": "daemon_offline" | "not_started" | "indexing", ...}` rather than crashing.
- **The per-project PID lock was removed** (the `.code-index/serve.pid` file is no longer created). Any number of MCP processes can connect to a single `.code-index/index.db` in parallel.
- **The `--no-watch`, `--flush-interval` flags** on `serve` were removed — they were specific to the former writer role and are inapplicable to read-only.

### Added

- **The `daemon` subcommand**: `code-index daemon run/start/stop/status/reload`. `run` — foreground (for a Scheduled Task / systemd), `start`/`stop`/`status`/`reload` — an HTTP client to a running daemon.
- **The `CODE_INDEX_HOME` environment variable** — a single point of configuration. It contains `daemon.toml`, and the runtime files `daemon.pid`, `daemon.json`, `daemon.log` are placed there too. Works both via a system variable (`setx`) and via an `"env"` block in `.mcp.json`.
- **The `daemon.toml` config** with the list of watched folders and parameters:
  - `max_concurrent_initial` — how many folders are in the initial-reindex phase at once (default `1`, protection against a RAM spike).
  - Per-folder `debounce_ms` / `batch_ms` — overriding the watcher delay per project.
- **HTTP health IPC on loopback**: `GET /health`, `GET /path-status?path=...`, `POST /reload`, `POST /stop`. The port is chosen automatically and written into `daemon.json`.
- **A per-folder lifecycle**: `not_started → initial_indexing → ready ⇄ reindexing_batch | error`. Visible in `daemon status`.
- **A PowerShell script** `scripts/install-daemon-autostart.ps1` to install a Scheduled Task (the trigger is the user logon; it automatically runs `setx CODE_INDEX_HOME`).

### Changed

- **Memory**: only one in-memory SQLite storage at a time. After the initial reindex the worker flushes → reopens the same file in disk mode (WAL) → releases the semaphore permit. Peak RAM does not sum across folders.
- **Repeated startup**: if `.code-index/index.db` already exists, the worker opens it directly in disk mode (skipping the backup disk→memory→disk). On 2 1C repos of ~90K files each, a repeated start takes **~12 s** (previously ~600 s with the same code before the fix).
- **SQLite**: added `PRAGMA wal_autocheckpoint=500` and `PRAGMA journal_size_limit=67108864` — the WAL file does not bloat over long transactions and is truncated to 64 MB after a checkpoint.
- **The MCP server** checks the folder status at the daemon before each tool call. If the folder is not `ready` — it returns a structured JSON with progress/a hint rather than an empirical result from a stale index.

### Removed

- The legacy modules `src/daemon.rs` and `src/pidlock.rs`.

### Measurements (1C repos, 2 folders of 88–92K files, 80% XML)

| Scenario | Time |
|----------|-------|
| Initial reindex from scratch, both folders sequentially (`max_concurrent_initial=1`) | ~10 min, RAM peak ~6 GB |
| Repeated start on an existing DB | ~12 s |
| Watcher: from a file edit to its appearance in the index | ~1.6 s (of which 1.5 s is the debounce — configurable) |
| Graceful shutdown (`daemon stop`) | DB on disk without `-wal`/`-shm` files |

### Technical debt

- A `0/0` progress in `daemon status` during the initial reindex (cosmetic — it is not updated from the blocking phase).
- Linux / macOS are not verified live — there is only theoretical cross-platform support via the `dirs` and `notify` crates. Feedback on the first non-Windows runs is appreciated.
- There are no integration tests for daemon_core — only unit tests for `config`, `ipc`, `state`.

## [0.4.0] — 2026-03-30

- An `mtime`+`file_size` pre-filter for the initial reindex: 93K files are re-checked in ~4 s instead of ~163 s (SHA-256 only for changed files).
- The `migrate_v3` migration — adds the `mtime`, `file_size` columns to the `files` table.
- A per-project PID lock (`.code-index/serve.pid`) — protection against running two `serve` instances on one project simultaneously.

## [0.3.0] — 2026-03-...

- Parallel read+hash via rayon.
- `hash_ast` without `to_sexp` (faster).
- Removal of `max_file_size` for code — large BSL/XML files are now indexed in full.
- Tuning of `mmap_size`, `batch_size` for the initial indexation.
