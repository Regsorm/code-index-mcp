# Code Index MCP

[Русская версия](README_RU.md)

Instant code search for AI models. Replaces grep with millisecond queries.

> 62K files indexed in 43s — 282K functions searchable in <1ms — 8 languages — 12 MCP tools

## Problem

AI models waste enormous time on repeated grep/find calls just to locate a single symbol. A real example: finding `RuntimeErrorProcessing` in a Java project required 14 sequential grep/find calls, each scanning thousands of files. With Code Index, that is one query returning results in under a millisecond.

## Solution

A compiled Rust binary that:

1. Parses source code into AST via tree-sitter
2. Indexes everything into SQLite with FTS5 full-text search
3. Exposes 12 tools over the MCP protocol for direct AI model use
4. Watches file changes in daemon mode and re-indexes automatically

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

Text files (`.md`, `.json`, `.yaml`, `.toml`, `.xml`, `.sql`, `.env`, etc.) are also indexed for full-text search.

## Quick Start

### Build from source

```bash
git clone https://github.com/Regsorm/code-index-mcp.git
cd code-index-mcp
cargo build --release
```

Binary: `target/release/code-index` (Linux/Mac) or `target/release/code-index.exe` (Windows)

### Index a project

```bash
code-index index /path/to/project
code-index stats --path /path/to/project --json
```

### Run as MCP server

```bash
code-index serve --path /path/to/project
```

## Connecting to Claude Code

Add to `.mcp.json` in your project root:

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
| `grep_body` | Substring or regex search in function/class bodies. Returns `match_lines` (first 3 line numbers) and `match_count` (total, if > 3) |

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
# MCP server (daemon mode)
code-index serve --path /project [--no-watch] [--flush-interval 30]

# One-shot indexing
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

## Daemon Mode

When running `code-index serve`, the process goes through four phases:

1. **Background scan** — indexes new and changed files in the background while the MCP server is already accepting requests
2. **File watcher** — tracks filesystem changes in real-time using the `notify` crate
3. **MCP server** — accepts tool calls via stdio (JSON-RPC)
4. **Periodic flush** — writes the in-memory database to disk every 30 seconds

When a file changes: 1.5s debounce window collects related edits, then the affected files are automatically re-indexed. The MCP server remains responsive throughout.

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
  "batch_ms": 2000,
  "flush_interval_sec": 30
}
```

Key fields:

- **storage_mode** — `auto` selects in-memory or disk SQLite based on available RAM; `memory` forces in-memory; `disk` forces on-disk
- **memory_max_percent** — maximum percentage of system RAM the in-memory database may use before falling back to disk (used in `auto` mode)
- **debounce_ms** — milliseconds to wait after a file change before triggering re-indexing (collects burst edits into one pass)
- **batch_size** — number of records per SQLite transaction during indexing (higher = faster bulk inserts, higher peak memory)
- **bulk_threshold** — minimum number of files that triggers bulk mode (drop indexes, insert, rebuild indexes); faster for large batches

## Benchmarks

Tested on a 1C:Enterprise Trade Management configuration:

| Metric | Value |
|--------|-------|
| Files | 61,706 |
| Functions | 282,575 |
| Call graph edges | 1,533,337 |
| Indexing time | 43 seconds |
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

- **In-memory SQLite with periodic flush** — all reads and writes go to RAM; disk is written every 30 seconds
- **Rayon parallel parsing** — files are parsed across all CPU cores simultaneously
- **Bulk mode** — for large batches: drop indexes, bulk insert, rebuild indexes; significantly faster than incremental inserts
- **SHA-256 hash check** — each file's hash is stored; unchanged files are skipped entirely on re-index
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
