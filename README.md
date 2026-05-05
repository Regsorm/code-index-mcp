<a href="https://infostart.ru/1c/tools/2677918/" title="Published on Infostart">
  <img src="https://infostart.ru/bitrix/templates/sandbox_empty/assets/tpl/abo/img/logo.svg" alt="Infostart" height="32">
</a>

Published on Infostart: [Code Index — структурный поиск по выгрузке кода 1С через MCP](https://infostart.ru/1c/tools/2677918/)

---

# Code Index MCP

[Русская версия](README_RU.md)

Instant code search for AI models. Replaces grep with millisecond queries.

> 93K files re-checked in 4s (mtime fast-path) — 282K functions searchable in <1ms — 9 languages — 18 MCP tools

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

### Build from source

```bash
git clone https://github.com/Regsorm/code-index-mcp.git
cd code-index-mcp
cargo build --release -p code-index               # public binary for Python/Rust/Go/Java/JS/TS
cargo build --release -p bsl-indexer --features enrichment   # extra build with 1C support + LLM enrichment
```

Binaries:
* `target/release/code-index[.exe]` — main binary (no 1C support).
* `target/release/bsl-indexer[.exe]` — full 1C support (XML metadata parsers, BSL call graph, MCP tools `get_object_structure` / `get_form_handlers` / `find_path` / `search_terms`, optional LLM enrichment under cargo feature `enrichment`).

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

If `CODE_INDEX_HOME` is not set, the daemon falls back to `%APPDATA%\code-index\daemon.toml` for config and `%LOCALAPPDATA%\code-index\` for runtime files (on Linux/macOS the XDG-standard equivalents).

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
      "type": "stdio",
      "command": "/path/to/code-index",
      "args": ["serve", "--path", "."]
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
| `get_function` | Get function by exact name |
| `get_class` | Get class by exact name |
| `get_callers` | Who calls this function? |
| `get_callees` | What does this function call? |
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

All search tools (`search_function`, `search_class`, `get_function`, `get_class`, `find_symbol`, `search_text`, `grep_body`) accept an optional **`path_glob`** parameter (v0.7.0) to scope results to a subtree (e.g., `src/auth/**`, `Documents/**/*.bsl`). Implementation: post-filter via the `globset` crate after the SQL query.

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

### Additional tools for 1C repos (only in `bsl-indexer`, v0.6+)

When BSL repos are present in `daemon.toml` (`language = "bsl"`), 5 BSL-specific tools are auto-registered:

| Tool | Description |
|------|-------------|
| `get_object_structure` | Structure of a 1C metadata object (Catalog, Document, InformationRegister, ...) by `full_name` like `Document.SalesInvoice` |
| `get_form_handlers` | Managed-form event handlers by `(owner_full_name, form_name)`. For typical document form returns ~120 `(event, handler)` pairs |
| `get_event_subscriptions` | All event subscriptions from `EventSubscriptions/*.xml`, optional filter by handler module |
| `find_path` | Call-chain between two procedures via `proc_call_graph` (recursive CTE, max_depth=3) |
| `search_terms` | FTS search by business terms enriched per procedure by an LLM (after `bsl-indexer enrich`) |

These tools appear in `tools/list` **only when at least one BSL repo is configured** (conditional registration). When the repo set changes in `daemon.toml`, the server emits `notifications/tools/list_changed`. On Claude Code 2.1.120 this notification is currently [ignored](https://github.com/anthropics/claude-code/issues/13646); workaround — manual `/mcp Reconnect`.

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

## License

MIT. See [LICENSE](LICENSE).

## Acknowledgements

- [tree-sitter](https://tree-sitter.github.io/tree-sitter/) — incremental parsing library
- [tree-sitter-onescript](https://github.com/1c-syntax/tree-sitter-onescript) — BSL/OneScript grammar by the 1c-syntax community
- [rusqlite](https://github.com/rusqlite/rusqlite) — SQLite bindings for Rust
- [rayon](https://github.com/rayon-rs/rayon) — data parallelism for Rust
- [rmcp](https://github.com/modelcontextprotocol/rust-sdk) — Rust MCP SDK
