<a href="https://infostart.ru/1c/tools/2677918/" title="Published on Infostart">
  <img src="https://infostart.ru/bitrix/templates/sandbox_empty/assets/tpl/abo/img/logo.svg" alt="Infostart" height="32">
</a>

Published on Infostart: [Code Index — структурный поиск по выгрузке кода 1С через MCP](https://infostart.ru/1c/tools/2677918/)

---

# code-index-mcp

[Русская версия](README_RU.md)

**Rust-native code index for AI agents. Static binary. Production-grade BSL/1C support.**

One static binary for Windows/Linux/macOS — no runtime, no dependencies. Indexes large repositories in seconds, returns results to AI agents over MCP in milliseconds. 31 tools: 20 universal + 11 BSL-specific for 1C:Enterprise configurations.

## What's inside

- **Performance.** 62,000 files indexed in 43 seconds, sub-ms search per query. Production-grade for 100K+ file monorepos.
- **31 MCP tools.** 20 universal (functions, classes, callers/callees, file content, grep) + 11 BSL-tools (object structure & profile, form handlers, event subscriptions, call graph, data links, register writers, impact map, read-only SQL).
- **Native BSL/1C.** Parses both Configurator (XML) and 1C:EDT (`.mdo`) exports of 1C:Enterprise 8.3 configurations. Data-link graph (object→object edges via reference types in attributes) — ~60,000 edges in seconds for a typical accounting configuration.
- **Federation.** One MCP server can serve multiple repositories across machines — pass `repo: "alias"` in each tool call.
- **Compressed content storage.** File contents stored in SQLite via zstd, cheap random-access reads for AI agents.
- **Tree-sitter AST.** 10 languages with full parsing (Rust, Python, JavaScript, TypeScript, Java, Kotlin, C#, Go, Objective-C, Zig) + fallback for 50+ formats.

Connects to Claude Code, Cursor, any MCP client over HTTP.

## Problem

AI models waste enormous time on repeated grep/find calls just to locate a single symbol. A real example: finding `RuntimeErrorProcessing` in a Java project required 14 sequential grep/find calls, each scanning thousands of files. With Code Index, that is one query returning results in under a millisecond.

## Solution

A compiled Rust binary with **one-writer / many-readers** architecture:

1. Parses source code into AST via tree-sitter
2. Indexes everything into SQLite with FTS5 full-text search
3. A separate **background daemon** is the sole writer: one process per machine watches a list of folders from its config and keeps `.code-index/index.db` up to date.
4. The **MCP server** is a thin **read-only** client: any number of Claude Code / VS Code / subagent sessions can connect to the same project in parallel — no pidlock conflicts, no per-session re-indexing.

## Supported Languages

| Language | Parser | Extensions |
|----------|--------|------------|
| Python | tree-sitter-python | `.py` |
| JavaScript | tree-sitter-javascript | `.js`, `.jsx` |
| TypeScript | tree-sitter-typescript | `.ts`, `.tsx` |
| Java | tree-sitter-java | `.java` |
| Rust | tree-sitter-rust | `.rs` |
| Go | tree-sitter-go | `.go` |
| 1C (BSL) | tree-sitter-onescript | `.bsl`, `.os` |
| XML (1C) | quick-xml | `.xml` (configuration metadata) |
| HTML | tree-sitter-html | `.html`, `.htm` (v0.7.1, by user request — see HTML-specific mapping below) |

Text files (`.md`, `.json`, `.yaml`, `.toml`, `.xml`, `.sql`, `.env`, etc.) are also indexed for full-text search.

### HTML — entity mapping (v0.7.2)

HTML has no native concept of "function" or "class", so the mapping is conventional. **Dual-indexing**: html files go through both AST parser AND `text_files` (so `search_text` / `grep_text` / `read_file` keep working alongside the new structural queries).

| HTML | → | code-index table | Name |
|------|---|------------------|------|
| `<element id="X">…</element>` | → | `classes` | `X` (body=outerHTML, bases=tag_name) |
| `<form id|name="X">` | → | `classes` | `form_X` (bases=`form`) |
| `<form>` without id/name | → | `classes` | `form_<line>` |
| `<input/select/textarea name="Y">` | → | `variables` | `Y` |
| `<a href="URL">` | → | `imports` | `module=URL`, `kind="link"` |
| `<link href="URL" rel="X">` | → | `imports` | `module=URL`, `kind=X` (or `"stylesheet"`) |
| `<script src="URL">` | → | `imports` | `module=URL`, `kind="script"` |
| `<img/iframe/video/audio/source/embed src="URL">` | → | `imports` | `module=URL`, `kind=tag` |
| `<script>…inline JS…</script>` | → | `functions` | `inline_script_<line>` (body=content) |
| `<style>…inline CSS…</style>` | → | `functions` | `inline_style_<line>` (body=content) |
| Attribute `class="foo bar baz"` | → | `variables` | `class:foo`, `class:bar`, `class:baz` (one record per class) |

All MCP tools that work for HTML files after re-indexing:

```
# === Discovery & metadata ===
list_files(repo="X", pattern="**/*.html")                # all html (returns language="html")
list_files(repo="X", path_prefix="src/templates/")
stat_file(repo="X", path="src/templates/base.html")      # returns language="html", category="text"
get_stats(repo="X")                                       # totals

# === Structural (AST) — new in 0.7.x ===
# Elements with id, forms, css-classes, links, inline blocks → AST tables
get_class(repo="X", name="cart")                          # outerHTML of <... id="cart">
get_class(repo="X", name="form_login")                    # full <form id="login">
search_class(repo="X", query="container", language="html")
get_function(repo="X", name="inline_script_42")           # body of <script> at line 42
search_function(repo="X", query="inline_script", language="html")
find_symbol(repo="X", name="form_login")                  # exact-name lookup across all 4 tables
find_symbol(repo="X", name="class:htmx-indicator")        # CSS class usage
get_imports(repo="X", module="https://unpkg.com/htmx.org@1.9.12")  # who depends on this CDN
get_file_summary(repo="X", path="src/templates/base.html")         # full map (functions/classes/imports/variables)

# === Body-level grep (works on inline_script bodies) ===
grep_body(repo="X", regex="fetch\\(", language="html")    # in <script> blocks
grep_body(repo="X", pattern="color:", language="html")    # in <style> blocks
grep_body(repo="X", regex="hx-target", language="html", path_glob="src/templates/**", context_lines=2)

# === Text-level (still works via dual-indexing) ===
read_file(repo="X", path="src/templates/base.html", line_start=1, line_end=20)
search_text(repo="X", query="DOCTYPE", language="html")
grep_text(repo="X", regex="\\{%\\s*include", path_glob="**/*.html", context_lines=1)  # Jinja includes
```

`get_callers` / `get_callees` are not populated for HTML (the parser does not extract call edges between scripts).

Template engines (Jinja/Django/EJS): `{{ … }}` and `{% … %}` are tolerated as text content; surrounding HTML elements are still parsed normally.

## Quick Start

### Install via npm (easiest)

```bash
npm install -g @regsorm/code-index-mcp
```

The `postinstall` step downloads the prebuilt native binary for your platform (Windows x64, Linux x64, macOS arm64) from GitHub Releases — nothing is compiled. Then run it as an MCP server:

```bash
npx @regsorm/code-index-mcp serve --path /path/to/your/repo
```

Also published to the [official MCP Registry](https://registry.modelcontextprotocol.io/) as `io.github.Regsorm/code-index`. This wrapper ships only the public `code-index` binary (no 1C support); for `bsl-indexer` build from source.

### Build from source

```bash
git clone https://github.com/Regsorm/code-index-mcp.git
cd code-index-mcp
cargo build --release -p code-index               # public binary for Python/Rust/Go/Java/JS/TS
cargo build --release -p bsl-indexer --features enrichment   # extra build with 1C support + LLM enrichment
```

Binaries:
* `target/release/code-index[.exe]` — main binary (no 1C support).
* `target/release/bsl-indexer[.exe]` — full 1C support (XML metadata parsers, BSL call graph, data-links graph, MCP tools `get_object_structure` / `get_form_handlers` / `find_path_bsl` / `search_terms` / `get_data_links` / `find_data_path` / `get_register_writers`, optional LLM enrichment under cargo feature `enrichment`).

GitHub Releases publish 6 ready artifacts per tag: `code-index` × {Win, Linux, macOS} + `bsl-indexer` × {Win, Linux, macOS}.

### Set up the background daemon (v0.5+)

Portable layout: one folder for everything (binary + config + runtime files). Pointed to by `CODE_INDEX_HOME` env var.

1. Create the daemon folder and drop `code-index.exe` into it (e.g. `C:\tools\code-index\`).

2. Set the `CODE_INDEX_HOME` environment variable to point at that folder:

   **Windows (persistent, user scope):**
   ```powershell
   setx CODE_INDEX_HOME "C:\tools\code-index"
   # Reopen your shell so the variable is visible.
   ```

   **Linux** — add to `~/.bashrc` or `~/.zshrc`:
   ```bash
   export CODE_INDEX_HOME="$HOME/.local/code-index"
   ```

   **macOS** — same as Linux for shells; for launchd agents use `launchctl setenv`.

   **Any OS — per-project fallback via `.mcp.json`** (no system env var needed):
   ```json
   {
     "mcpServers": {
       "code-index": {
         "command": "C:\\tools\\code-index\\code-index.exe",
         "args": ["serve", "--path", "."],
         "env": { "CODE_INDEX_HOME": "C:\\tools\\code-index" }
       }
     }
   }
   ```

3. Create `daemon.toml` inside that folder and list the paths to watch:

   ```toml
   [daemon]
   http_port = 0                  # 0 = pick free port automatically
   max_concurrent_initial = 1     # folders processed sequentially during initial indexing

   [[paths]]
   path = "C:\\RepoUT"

   [[paths]]
   path = "C:\\RepoBP_1"
   debounce_ms = 500              # per-folder override: react faster than the default 1500 ms
   batch_ms    = 1000
   ```

   Per-folder `debounce_ms` / `batch_ms` are **optional**. If omitted, the daemon falls back to `.code-index/config.json` inside that project, and then to built-in defaults (1500 ms / 2000 ms).

4. Start the daemon (foreground):

   ```bash
   code-index daemon run
   ```

   Or install it as a Windows Scheduled Task (auto-start at user logon; the script also sets `CODE_INDEX_HOME` via `setx`):

   ```powershell
   powershell -ExecutionPolicy Bypass -File scripts\install-daemon-autostart.ps1 `
     -BinaryPath "C:\tools\code-index\code-index.exe" `
     -CodeIndexHome "C:\tools\code-index" `
     -StartNow
   ```

5. Check status:

   ```bash
   code-index daemon status        # human-readable
   code-index daemon status --json # JSON
   code-index daemon reload        # re-read daemon.toml after edits
   code-index daemon stop
   ```

`CODE_INDEX_HOME` is **required** — there is no fallback. If it is unset, both `daemon` and `serve` exit with an error explaining how to set it.

> **Troubleshooting — "daemon not running / runtime-info missing" even though the daemon IS running.**
>
> The `serve` process and the daemon find each other only through `$CODE_INDEX_HOME/daemon.json`. If `serve` sees a different (or empty) `CODE_INDEX_HOME` than the daemon, it looks for `daemon.json` in the wrong place and reports the daemon as offline — while it is actually alive.
>
> The most common cause on Linux/macOS: **GUI MCP clients (VS Code, Continue, Cline) do not read `~/.bashrc` / `~/.zshrc`**, so a `serve` they launch with an empty `env` never sees the `CODE_INDEX_HOME` you exported in your shell. Meanwhile the daemon, started from a terminal, does — so they end up pointing at different folders.
>
> **Fix:** set `CODE_INDEX_HOME` explicitly in the `env` section of the client's MCP config, using the **same absolute path** the daemon uses (`$HOME` is not expanded there — use a real path). Restart the client and verify with `code-index daemon status`.

### One-shot indexing (no daemon)

```bash
code-index index /path/to/project
code-index stats --path /path/to/project --json
```

### Run as MCP server (read-only)

```bash
code-index serve --path /path/to/project
```

This is a thin read-only client of the daemon. It does not index anything itself — the daemon does. If the folder is still being indexed or not in `daemon.toml`, tools return a structured `{status, message, progress}` response instead of failing.

### Transports (stdio vs HTTP)

`serve` supports two transports:

| Transport | Process model | When to use |
|-----------|---------------|-------------|
| `stdio` (default) | One `serve` process per MCP session | Simple setups, single client, ad-hoc runs |
| `http` (streamable) | One shared `serve` process, many clients over `http://host:port/mcp` | Multi-project setups, supervisor-managed services, avoiding per-session CLI duplication |

```bash
# stdio — per-session, alias set at CLI
code-index serve --path ut=/repos/ut --path bp=/repos/bp

# HTTP — shared process, aliases come from daemon.toml
code-index serve --transport http --port 8011 --config /etc/code-index/daemon.toml
```

`--path` can be repeated in `alias=dir` form (multi-repo mode). Each tool call takes a `repo` parameter to select which repository to query. Without `=`, the single path uses `alias=default` (backward-compatible).

In HTTP mode, if `--config` is provided, aliases are taken from `[[paths]]` entries of `daemon.toml`: explicit `alias = "..."`, or derived from the path's last segment (lowercased, spaces → `_`) when not set. CLI `--path` takes precedence over the config file.

## Connecting to Claude Code

Add to `.mcp.json` in your project root. For `stdio`:

```json
{
  "mcpServers": {
    "code-index": {
      "command": "npx",
      "args": ["-y", "@regsorm/code-index-mcp", "serve", "--path", "."]
    }
  }
}
```

For a shared HTTP process:

```json
{
  "mcpServers": {
    "code-index": {
      "type": "http",
      "url": "http://127.0.0.1:8011/mcp"
    }
  }
}
```

## MCP Tools

| Tool | Description |
|------|-------------|
| `search_function` | Full-text search across functions (name, docstring, body) |
| `search_class` | Full-text search across classes |
| `get_function` | Get function by exact name (case-insensitive fallback; **(v0.44.0)** on 0 matches returns `did_you_mean` with similar names) |
| `get_class` | Get class by exact name (case-insensitive fallback; **(v0.44.0)** on 0 matches returns `did_you_mean` with similar names) |
| `get_callers` | Who calls this function? **(v0.35.0)** each row carries the caller's source `path` (distinguishes same-named callers from different files) |
| `get_callees` | What does this function call? **(v0.35.0)** each row carries the source `path` |
| `find_path` | **(v0.23.0)** Shortest path in the call graph between two functions `from`→`to` (iterative cycle-safe BFS over unique `calls` nodes, `max_depth=5`, any language). Returns path edges `[{caller, callee, line}]` |
| `get_call_tree` | **(v0.23.0)** Call tree from a `root` function up to `max_depth` (default 3). `direction`: `callees`/`down` (downstream) or `callers`/`up`. Flat edge list `[{caller, callee, line, depth, path}]` (**(v0.35.0)** `path` = source file of each edge) + nested `{name, children}` tree; `max_nodes` cap |
| `find_symbol` | Search everywhere (functions, classes, variables, imports) |
| `get_imports` | Imports by module or file |
| `get_file_summary` | Complete file map without reading source |
| `get_stats` | Index statistics |
| `search_text` | Full-text search across text files |
| `grep_body` | Substring or regex search in function/class bodies. Returns `match_lines` (first 3 line numbers) and `match_count` (total, if > 3). v0.7.0: optional `path_glob`, `context_lines` |
| `stat_file` | **(v0.7.0)** Metadata of a single file: exists, size, mtime, language, lines_total, content_hash, indexed_at, category (`text`/`code`). **(v0.8.0)** adds `oversize: bool` for code files |
| `list_files` | **(v0.7.0)** Flat file listing with optional `pattern` (glob like `**/*.py`), `path_prefix`, `language`, `limit` |
| `read_file` | **(v0.7.0)** Read content of a file. Optional `line_start`/`line_end` (1-based, inclusive). Soft-cap 5000 lines or 500 KB, hard-cap 2 MB. **(v0.8.0)** works for **code files** too (`.py`, `.bsl`, `.rs`, `.ts`, etc.) — content stored in `file_contents` table (zstd). Oversize files (default > 5 MB) return `oversize: true` with an empty `content` and a hint |
| `grep_text` | **(v0.7.0)** Regex search over text-file content (REGEXP). Closes the FTS5 special-character gap. Optional `path_glob`, `language`, `context_lines`. Hard-cap 1 MB on response size |
| `grep_code` | **(v0.8.0)** Regex search over **code-file** content (`.py`, `.bsl`, `.rs`, `.ts`, etc.) via `file_contents` table (zstd-decode in Rust). Same parameters as `grep_text`: `regex`, `path_glob?`, `language?`, `limit?`, `context_lines?`. Complements `grep_body` (which searches only inside function/class bodies). Oversize files are skipped |
| `health` | MCP server health and connected repos |

All search tools (`search_function`, `search_class`, `get_function`, `get_class`, `find_symbol`, `search_text`, `grep_body`) accept an optional **`path_glob`** parameter (v0.7.0) to scope results to a subtree (e.g., `src/auth/**`, `Documents/**/*.bsl`). Implementation: post-filter via the `globset` crate after the SQL query. Since v0.32.0, `path_glob`/`pattern` support `{a,b}` brace alternates (`**/*.{bsl,xml}`) — including in `grep_code`/`grep_text`/`grep_body`/`list_files`, where the filter runs at the SQLite GLOB level (the pattern is expanded into an OR group of conditions).

### Code-file content storage (v0.8.0)

Starting with v0.8.0, code-file content is stored in the `file_contents` table (zstd-compressed) and returned by `read_file` and searched by `grep_code`. Large files can be excluded from storage via `max_code_file_size_bytes` (default **5 MB**):

```toml
[indexer]
max_code_file_size_bytes = 5242880   # 5 MB global override

[[paths]]
path = "C:/RepoUT"
max_code_file_size_bytes = 10485760  # 10 MB for this repo only
```

Priority: per-path → `[indexer]` section → 5 MB default. Files exceeding the limit are stored with `oversize=1` and `content_blob=NULL`; AST parsing, FTS, and call-graph edges still work for them in full. `read_file` and `grep_code` return a hint explaining how to query such files via `get_function`/`get_class`/`grep_body`.

### Connection pool (v0.24.0)

In `serve`, each repo is read through a pool of read-only SQLite connections instead of a single connection behind a mutex. Concurrent requests to the **same** repo now run in parallel — a heavy query (`bsl_sql`, a full `grep_code`, recursive `find_path`/`get_call_tree`) no longer blocks a fast `get_function` on that repo. Tune via an optional `[pool]` section in `serve.toml`:

```toml
[pool]
pool_size = 4               # connections per repo (default 4)
per_conn_cache_kib = 16384  # page-cache per connection, KiB (default 16384 = 16 MB)
busy_timeout_ms = 5000      # SQLite busy_timeout per connection, ms (default 5000)
```

All fields are optional; omitting the section uses the defaults. The default is memory-neutral: `4 × 16 MB = 64 MB` per active repo, the same as the previous single connection. Connections open lazily up to `pool_size` and return to the pool when a request finishes; `0` is clamped to a safe value. WAL mode (already used by the index) makes the multiple readers safe alongside the indexing daemon's writes.

### Additional tools for 1C repos (only in `bsl-indexer`, v0.6+)

When BSL repos are present in `daemon.toml` (`language = "bsl"`), 11 BSL-specific tools are auto-registered:

| Tool | Description |
|------|-------------|
| `get_object_structure` | Full structure of a 1C metadata object by `full_name` (`Document.SalesInvoice`): attributes with 1C-notation types (+`synonym` — UI label, +`required` — fill checking, v0.32.0), tabular sections with columns, register dimensions/resources; `enum_values` for enumerations (+`enum_synonyms` — UI labels of values); `predefined` for objects with predefined items (catalogs, charts of accounts); `posting` for documents (posting properties from root `<Properties>`); `owners` — owners of a subordinate catalog; `value_types` — value type of a CCT/constant (for a CCT — available analytics); `properties` — header properties (register periodicity, write mode, numbering, hierarchy); `commands` — object commands with synonyms (all — v0.32.0). Base sections (`attributes`/`dimensions`/`resources`/`tabular_sections`) are always present (empty as `[]`). `sections` parameter (v0.29.0) — narrow selection: return only the requested sections (e.g. `["posting"]` — ~0.2 KB instead of the full object). **Criterion selector (v0.41.0):** `name_like` (object-name substring) + optional `meta_type` return the structures of ALL matching objects in one call (`{matched, truncated, results}`, cap 50) — for a thematic set (e.g. `name_like="EDI"`) instead of calling one by one |
| `get_form_handlers` | Managed-form event handlers by `(owner_full_name, form_name)`. Owner accepted in both formats — `Document.X` and export-folder `Documents.X` (v0.31.0). For typical document form returns ~120 `(event, handler)` pairs; unknown form → error with `available_forms` of the owner |
| `get_event_subscriptions` | All event subscriptions from `EventSubscriptions/*.xml`. Filters: handler module, event (Russian or English platform enum — `OnWrite`→`ПриЗаписи`), `source` — by source object (`Document.X`/`DocumentObject.X`/short name, v0.31.0). Unknown parameters are rejected with the list of valid filters; default limit 50 (`truncated`+`total` when exceeded) |
| `find_path_bsl` | Call-chain between two procedures via `proc_call_graph` (recursive CTE, max_depth=3). BSL-specific variant of the universal `find_path` — `proc_call_graph` carries `call_type` and procedure keys. **(v0.35.0)** `from`/`to` and procedure keys are `<rel_path>::<name>` (a bare name is accepted for unresolved leaves); walks by resolved `callee_proc_key`. **(v0.36.0)** BSL callees are stored glued (`Module.Method`, like Python's `obj.method`); call targets resolve precisely to common-module (`…/CommonModules/X/Ext/Module.bsl::M`) and manager-module (`…/Catalogs/X/Ext/ManagerModule.bsl::M`) addresses, object-method noise is pruned — direct-edge resolution ~80-82% |
| `search_terms` | **Meaning-based procedure search (v0.30.0):** terms are filled mechanically at index time — words of the procedure name (CamelCase split), the owner object's name and synonym, the comment above the procedure; no LLM needed. Trigram FTS: word forms and 3+ character substrings work, case and ё/е are irrelevant; a multi-word query is searched as an OR of words (best matches first). The first choice for "where is feature X implemented". LLM enrichment (`bsl-indexer enrich`) remains an optional add-on |
| `get_data_links` | **Data-links graph (v0.10.0):** what an object references / what references it, via reference-typed attributes, register dimensions and tabular-section attributes (`data_links` table). `direction=out\|in\|both`, `depth=1..4`. Replaces a series of `get_object_structure` calls when tracing relations. Targets like `*CatalogRef`/`*AnyRef`/`*DefinedType.X` are generalized refs (terminal, not expanded) |
| `find_data_path` | **Data-links graph (v0.10.0):** chain of reference links from one object to another (BFS over `data_links`, like `find_path` but for data, not calls) |
| `get_register_writers` | **Register recorders / document movements (v0.16.0):** for a register (`AccumulationRegister.Stock`) returns `writers` — documents writing movements; for a document — `writes_to` (target registers). From the declarative `<RegisterRecords>` set (recorder edges of `data_links`). One call covers both directions |
| `get_object_profile` | **Object passport in one call (v0.21.0):** the full portrait of an object — structure + forms + modules + data links — instead of a series of `get_object_structure`/`get_form_handlers`/`get_data_links`. The `sections` parameter (`['structure'\|'forms'\|'modules'\|'data_links']`) narrows the response |
| `find_references` | **Impact map (v0.21.0):** everything that references an object, in one call — reverse `data_links` (structural refs from metadata) + `metadata_code_usages` (usages in `.bsl` code) + `role_rights` (roles holding rights on it), broken down by kind with samples (`limit`) |
| `bsl_sql` | **Arbitrary read-only SQL (v0.21.0):** a `SELECT`/`WITH` query over the repo's `index.db` for the long tail of metadata/graph questions that have no dedicated tool (roles/RLS, joins, aggregations). Guard: `SELECT`/`WITH` only + `Statement::readonly()` + row cap + timeout. Tables: `metadata_objects`, `metadata_modules`, `metadata_forms`, `event_subscriptions`, `data_links`, `role_rights`, `proc_call_graph`, core `functions`/`files`. An empty result over the procedure tables auto-falls back to `search_terms` |

These tools appear in `tools/list` **only when at least one BSL repo is configured** (conditional registration). When the repo set changes in `daemon.toml`, the server emits `notifications/tools/list_changed`. On Claude Code 2.1.120 this notification is currently [ignored](https://github.com/anthropics/claude-code/issues/13646); workaround — manual `/mcp Reconnect`.

**Since v0.8.1**, these BSL-tools work in **all** scenarios (was broken in v0.8.0, see CHANGELOG):

* via `bsl-indexer.exe daemon run` — daemon now applies `schema_extensions` and `index_extras` for each BSL repo on startup (creates `metadata_objects` / `metadata_forms` / `event_subscriptions` / `proc_call_graph` and fills them from `Configuration.xml`).
* via federation — extension-tools are forwarded over the universal `POST /federate/extension` route. Both peers must be on **≥ 0.8.1**; older peers will return 404 on the new route.
* on repos without `Configuration.xml` (e.g. partial dumps containing only forms/processors) — the tables are created empty and the tools return `[]` instead of throwing `no such table: metadata_objects`.

Full instructions: [docs/bsl-indexer.md](docs/bsl-indexer.md).

All tools support a language filter: `search_function(query="X", language="python")`

### grep_body

Unlike FTS search, `grep_body` supports literal substrings (including dots and special characters) and regular expressions. This is essential for finding references like `Catalog.Contractors` or `Справочники.Контрагенты` that break FTS5 syntax.

```
grep_body(pattern="Справочники.Контрагенты", language="bsl")
grep_body(regex="Catalog\\.(Contractors|Organizations)", language="bsl")
```

Returns `[{file_path, name, kind, line_start, line_end, match_lines, match_count}]` — concrete functions/classes containing the match.

Each result includes `match_lines` — up to 3 absolute line numbers in the file where the pattern was found. If there are more than 3 matches, `match_count` shows the total.

```json
[
  {
    "file_path": "src/Catalogs/Products/ObjectModule.bsl",
    "name": "OnWrite",
    "kind": "function",
    "line_start": 45,
    "line_end": 82,
    "match_lines": [51, 63, 78]
  }
]
```

## CLI Reference

```bash
# Background daemon (writer — one per machine)
code-index daemon run                          # foreground, for Scheduled Task / systemd
code-index daemon status [--json]              # query GET /health via loopback
code-index daemon reload                       # re-read daemon.toml
code-index daemon stop                         # POST /stop

# MCP server (read-only client; used by Claude Code, VS Code, subagents)
code-index serve --path /project

# One-shot indexing (no daemon)
code-index index /project [--force]

# Project management
code-index init --path /project          # Create config
code-index clean --path /project         # Remove stale entries
code-index stats --path /project [--json]

# Symbol search
code-index query "name" --path /project [--language rust] [--json]

# Full-text search (JSON output)
code-index search-function "query" --path /project [--language python] [--limit 20]
code-index search-class "query" --path /project [--language python] [--limit 20]
code-index search-text "query" --path /project [--limit 20]

# Exact lookup (JSON output)
code-index get-function "exact_name" --path /project
code-index get-class "exact_name" --path /project

# Call graph (JSON output)
code-index get-callers "function_name" --path /project [--language python]
code-index get-callees "function_name" --path /project [--language python]

# Navigation (JSON output)
code-index get-imports --path /project [--module "name"] [--file-id 42]
code-index get-file-summary "src/main.rs" --path /project

# Substring / regex search in function and class bodies (supports dots and special chars)
code-index grep-body --pattern "Catalog.Contractors" --path /project [--language bsl] [--limit 100]
code-index grep-body --regex "Catalog\.(Contractors|Organizations)" --path /project
```

## Using CLI from Subagents

Subagents launched via the Agent tool in Claude Code do not have access to MCP servers — they run in isolated subprocesses with no connection to the parent MCP session. All 12 MCP tools are mirrored as CLI subcommands that output JSON, making code-index fully usable from any subprocess, script, or subagent.

```bash
# Instead of an MCP tool call, a subagent runs:
code-index search-function "authenticate" --path /my/project --language python

# Call graph from CLI:
code-index get-callers "process_order" --path /my/project

# File map:
code-index get-file-summary "src/auth/login.py" --path /my/project
```

Every command outputs valid JSON that the subagent can parse and reason over, identical in structure to what the MCP tools return.

> **Note:** CLI read commands use `SQLITE_OPEN_READ_ONLY` mode, so they work in parallel with the MCP daemon without database locking conflicts.

## CLAUDE.md Setup

Add this block to your project's `CLAUDE.md` to instruct Claude Code subagents to use the CLI indexer instead of grep, find, or reading files manually:

```markdown
## Code Index — fast code search

For code search, use the CLI indexer instead of grep/find/Read:
- Search: code-index query "name" --path /path/to/project --json
- FTS search: code-index search-function "query" --path /path/to/project
- Call graph: code-index get-callers "function" --path /path/to/project
- File map: code-index get-file-summary "file" --path /path/to/project
- Stats: code-index stats --path /path/to/project --json
All commands output JSON. This is instant search over an indexed database.
```

Use an absolute path to the binary and adjust `/path/to/project` to your setup. On Windows, specify the full path to `code-index.exe`, for example `C:\MCP-Servers\code-index\target\release\code-index.exe`.

## Daemon Mode (v0.5+)

Starting with v0.5, `code-index` uses a **one-writer / many-readers** architecture:

### Background daemon (single writer)

`code-index daemon run` starts a long-running process that:

1. Loads the list of watched folders from `daemon.toml`.
2. For each folder: opens `.code-index/index.db`, runs full reindex with mtime fast-path (v0.4.0), then switches to a `notify` watcher that re-indexes on change (1.5s debounce, 2s batch).
3. Exposes a local health / management HTTP endpoint on loopback (port written to `daemon.json` in the state directory).
4. Holds a global PID-lock (`daemon.pid`) to prevent two daemons per machine.

Per-folder lifecycle: `not_started → initial_indexing → ready ⇄ reindexing_batch / error`. Each status transition is visible via `daemon status`.

### MCP servers (many read-only readers)

`code-index serve --path <project>` opens `.code-index/index.db` in `SQLITE_OPEN_READ_ONLY` and exposes MCP tools over stdio. Multiple MCP instances on the same project run in parallel without blocking each other.

Before every tool call the MCP asks the daemon for the per-folder status. If it is not `ready`, the tool returns a structured JSON:

```json
{ "status": "indexing", "progress": {"files_done": 4200, "files_total": 10000, "percent": 42.0}, "message": "Первичная индексация в процессе" }
```

If the daemon is offline:

```json
{ "status": "daemon_offline", "message": "Демон code-index не доступен. Запустите 'code-index daemon run' или Scheduled Task." }
```

## Configuration

`.code-index/config.json` is created automatically on first run. Full reference:

```json
{
  "exclude_dirs": ["node_modules", ".venv", "__pycache__", ".git", "target", "output"],
  "extra_text_extensions": [],
  "max_file_size": 1048576,
  "max_files": 0,
  "bulk_threshold": 10,
  "languages": ["python", "javascript", "typescript", "java", "rust", "go", "bsl"],
  "batch_size": 500,
  "storage_mode": "auto",
  "memory_max_percent": 25,
  "debounce_ms": 1500,
  "batch_ms": 2000
}
```

Key fields:

- **storage_mode** — `auto` selects in-memory or disk SQLite based on available RAM; `memory` forces in-memory; `disk` forces on-disk
- **memory_max_percent** — maximum percentage of system RAM the in-memory database may use before falling back to disk (used in `auto` mode)
- **debounce_ms** — milliseconds to wait after a file change before triggering re-indexing (collects burst edits into one pass)
- **batch_ms** — upper bound on how long the watcher keeps accumulating events after the first one in a batch
- **batch_size** — number of records per SQLite transaction during indexing (higher = faster bulk inserts, higher peak memory)
- **bulk_threshold** — minimum number of files that triggers bulk mode (drop indexes, insert, rebuild indexes); faster for large batches

### Tuning watcher latency (`debounce_ms`, `batch_ms`)

Defaults are 1500 ms / 2000 ms — good for typical IDE save + formatter + linter bursts and for git operations that touch many files at once. For a lively single-user IDE session you can lower the debounce and trade throughput for responsiveness.

The daemon resolves these values in this order (first match wins):

1. **Per-folder override in `daemon.toml`:**
   ```toml
   [[paths]]
   path = "C:/RepoBP_1"
   debounce_ms = 500      # react in ~0.6 s instead of ~1.6 s
   batch_ms    = 1000
   ```
2. **Per-project `.code-index/config.json`** — applies to that project only.
3. **Built-in defaults** (1500 / 2000).

Re-read after editing `daemon.toml`:

```bash
code-index daemon reload
```

Recommended values:

| Use case | `debounce_ms` |
|----------|---------------|
| Interactive IDE, single-file edits | 300–500 |
| 1C repos / git operations / large bulk edits | 1500 (default) |
| CI or scripted batch edits | 3000+ |

### Guarding output against disk offload (`[cap]`, v0.38.0)

The client (`claude` CLI / Claude Code) caps a single `tool_result` streamed inline into context (`MAX_MCP_OUTPUT_TOKENS` ≈ 25,000 tokens). A response over the cap is dumped to a file on disk by the harness, handing the model only a path + preview — structured inline access is lost. To keep large outputs (a big module's map, long arrays of values/sources/attributes) from being offloaded, `serve` trims them at the source. Configured via an optional `[cap]` section in `daemon.toml`:

```toml
[cap]
max_response_bytes      = 48000   # response budget in JSON bytes; 0 disables cap_response
cap_enabled             = true    # global on/off for cap_response (takes precedence over cap_tools)
cap_tools               = ["get_event_subscriptions", "bsl_sql", "find_references", "get_register_writers"]
max_function_body_chars = 15000   # get_function/get_class body threshold; 0 keeps the full body
```

Mechanisms (all optional, act on the serve output layer, require no reindex):

- **`cap_response`** — while the response JSON exceeds `max_response_bytes`, halves the heaviest array, leaving `<key>_total` (original count) and `<key>_truncated: true`. Applied to tools in `cap_tools` (when `cap_enabled = true`). Only arrays are trimmed — large strings (`read_file`/`grep`) are untouched. `get_file_summary` (core) is wired in here too: a giant module's map (hundreds of functions) no longer goes to offload.
- **`omit_oversize_sections`** (for `get_object_structure`) — where an array/map is the full authoritative answer (a 1C object's structure), the heaviest section is dropped WHOLESALE with `<section>_omitted: true` + `<section>_count: N` (a partial sample would lie, “here are all the enum values”).
- **Navigational body cap** (`get_function`/`get_class`) — a body longer than `max_function_body_chars` is returned as a head+tail+marker stub with a hint to `read_file(line_start,line_end)` / `grep_body`.

## Benchmarks

Tested on 1C:Enterprise configurations (HDD, Windows):

| Project | Files | Initial index | Re-check (no changes) | Speedup |
|---------|-------|---------------|----------------------|---------|
| Trade Management | 63K | 65 sec | **5 sec** | 13x |
| Accounting | 93K | 164 sec | **4 sec** | 40x |

Re-check uses `mtime + file_size` fast-path: only `stat()` per file, zero reads, zero SHA-256 hashes.

| Metric | Value |
|--------|-------|
| Functions indexed | 282,575 |
| Call graph edges | 1,533,337 |
| Search time | < 1 ms |
| Binary size | 13.5 MB |

Comparison with grep:

| Operation | grep | Code Index |
|-----------|------|------------|
| Find function by name | O(n) files, seconds | < 1 ms |
| Who calls function X? | grep all files | < 1 ms |
| File map | cat + manual analysis | < 1 ms |
| Full-text search | `grep -r`, seconds | < 1 ms |

## Architecture

```
Source Files -> Tree-sitter Parser -> SQLite (in-memory) -> MCP Server -> AI Model
                                           ^
                      File Watcher --------+ (auto re-index)
```

Key optimizations:

- **In-memory SQLite with event-driven flush** — all reads and writes go to RAM; disk is written only when data actually changes (see below)
- **Rayon parallel parsing** — files are parsed across all CPU cores simultaneously
- **Bulk mode** — for large batches: drop indexes, bulk insert, rebuild indexes; significantly faster than incremental inserts
- **mtime/size fast-path** — on restart, each file is checked via `stat()` (mtime + file_size); if both match the stored values, the file is not read at all — zero I/O, zero SHA-256. Only changed files are read and re-hashed
- **PID-lock** — prevents multiple daemon instances from competing for the same `index.db`

### Flush to disk policy

The daemon works in in-memory mode for maximum performance. The database is flushed to disk **only** when data actually changes — no periodic timers, no unnecessary I/O:

| Event | Flush? | Condition |
|-------|--------|-----------|
| Initial indexing completes | Yes | At least 1 file was indexed or deleted |
| File watcher processes a batch | Yes | At least 1 write/delete occurred in the batch |
| File watcher fires but nothing changed | **No** | Hash unchanged → no write → no flush |
| Idle (no file changes) | **No** | Zero disk activity |
| Daemon shutdown (graceful) | Yes | Always — final safety flush |

This means: if you're just chatting with AI and not editing code, the daemon produces **zero disk I/O**.
- **Batch transactions** — 500 records per transaction reduces SQLite overhead by orders of magnitude

## For 1C Developers

Code Index has first-class support for 1C:Enterprise source files.

From **BSL files**, it extracts:
- Procedures and functions with full body text
- Compilation directives (`&AtServer`, `&AtClient`, `&AtServerNoContext`)
- Extension annotations (`&Instead`, `&After`, `&Before`)
- Bilingual keywords (Russian and English forms are both indexed)

These are stored in two dedicated fields:
- `override_type`: "Перед" (Before), "После" (After), or "Вместо" (Instead)
- `override_target`: name of the original procedure being overridden

From **XML configuration exports**, it extracts:
- Metadata objects: catalogs, documents, registers, and more
- Attributes and tabular sections
- Forms and their composition

This makes Code Index suitable as an offline search layer over full 1C configuration exports without requiring a running platform instance.

## System Requirements

- **OS**: Windows, Linux, macOS
- **RAM**: 512 MB for small projects; up to 4 GB for large 1C configurations (60K+ files)
- **Disk**: index size is approximately 1-2 GB for projects with 60K+ files
- **Build**: Rust 1.77 or later — install from [rustup.rs](https://rustup.rs)

## MCP tool response format (v0.9.0+)

All data tools return a unified JSON envelope:

```json
{
  "result": <previous plain payload>,
  "_meta": {
    "dependent_files": ["src/X.bsl", "src/Y.bsl"],
    "file_mtimes": { "src/X.bsl": 1717689600, "src/Y.bsl": 1717689600 }
  }
}
```

`_meta.dependent_files` lists files the response depends on; `_meta.file_mtimes` (0.20.0+) maps each of them to its indexed mtime (unix seconds). **As of 0.42.0 serve strips `_meta` from the client response itself** — the model only ever receives `result`. `_meta` is now an internal signal: serve has a **built-in result cache** (cross-session, local repos only) with **per-file** freshness. On a file change the daemon sends `POST /mark-dirty {repo, files:[{path, mtime}]}` (files changed on disk), then after commit `POST /invalidate {repo, file_paths}`. A response is not cached/served from cache **only** if one of its source files is dirty (disk mtime newer than the indexed mtime in `_meta.file_mtimes`); per-file invalidation drops only the cache keys depending on the changed file (a file→keys reverse index). Requests about untouched files are unaffected — no whole-repo coarsening. The channel is wired by `[[cache_targets]]` in `daemon.toml`. The same `_meta` was previously consumed by the companion caching proxy **`mcp-cache-ci`**; with serve's own cache the standalone proxy is no longer required for the ci chain. Observability: `GET /cache-stats` on serve.

Diagnostic tools (`health`, `get_stats`, `stat_file`) are not wrapped — their format is unchanged.

## License

MIT. See [LICENSE](LICENSE).

## Acknowledgements

- [tree-sitter](https://tree-sitter.github.io/tree-sitter/) — incremental parsing library
- [tree-sitter-onescript](https://github.com/1c-syntax/tree-sitter-onescript) — BSL/OneScript grammar by the 1c-syntax community
- [rusqlite](https://github.com/rusqlite/rusqlite) — SQLite bindings for Rust
- [rayon](https://github.com/rayon-rs/rayon) — data parallelism for Rust
- [rmcp](https://github.com/modelcontextprotocol/rust-sdk) — Rust MCP SDK
