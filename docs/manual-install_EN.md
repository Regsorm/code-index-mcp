# Manual Installation, Step by Step (Windows)

[Русский](manual-install.md)

Installation without scripts or npm: download one file, put it into a folder,
create a settings file, start it and connect your AI client (Claude Code, Cursor,
VS Code and other MCP clients). No administrator rights are required.

One-command automatic installation — `install.ps1`, see [README](../README_EN.md#quick-start).

## How it works

The program is a single executable. It runs in two roles:

| Process    | Command                       | What it does                                              |
|------------|-------------------------------|-----------------------------------------------------------|
| Indexer    | `bsl-indexer.exe daemon run`  | Reads the sources, builds the index, watches file changes |
| MCP server | `bsl-indexer.exe serve ...`   | Answers requests from the AI client                       |

The two processes find each other through the folder set in the `CODE_INDEX_HOME`
environment variable.

The HTTP server is built into `bsl-indexer.exe` itself — no IIS, nginx or other
software is needed. It listens on `127.0.0.1` only, i.e. it is reachable from this
computer alone; you do not need to open ports or publish anything.

There are two ways to connect the AI client (step 6):

- **Option A — HTTP.** The MCP server runs permanently; the client talks to it at
  `http://127.0.0.1:8011/mcp`. One server for all client windows and all projects.
- **Option B — stdio.** You do not start the MCP server yourself: the client
  launches `bsl-indexer.exe serve` when a session opens and talks to it directly,
  without a port. The process ends with the session.

**In both options the indexer (`daemon run`) must be running.** The MCP server only
reads the index; without the indexer every request gets `daemon_offline` — even if
the index was built earlier.

---

## Step 1. Download the archive

Open the latest release page:
https://github.com/Regsorm/code-index-mcp/releases/latest

Download one of these files:

- `bsl-indexer-windows-x64.zip` — with 1C:Enterprise support (Designer and EDT exports);
- `code-index-windows-x64.zip` — without 1C (Python, Rust, Go, Java, JS/TS and other languages).

The archive holds a single file: `bsl-indexer.exe` (or `code-index.exe`). This guide
uses `bsl-indexer.exe`; for the other build substitute `code-index.exe`.

## Step 2. Create the program folder and put the file there

1. Create the folder `C:\tools\code-index` (any other works — then change the path in every step).
2. Extract `bsl-indexer.exe` into it.

Check in a command prompt:

```
C:\tools\code-index\bsl-indexer.exe --version
```

It should print the version, for example `code-index 1.2.2`.

## Step 3. Set the CODE_INDEX_HOME environment variable

**Required.** The indexer will not start without it.

Way 1 — by command:

```
setx CODE_INDEX_HOME "C:\tools\code-index"
```

Way 2 — via the dialog: Start → "Edit environment variables for your account" →
"New…" → name `CODE_INDEX_HOME`, value `C:\tools\code-index`.

Then **close and reopen** the command prompt — old windows do not see the variable.
Restart the AI client (VS Code, Cursor) as well.

## Step 4. Create the daemon.toml settings file

In `C:\tools\code-index` create a text file `daemon.toml` (UTF-8 encoding) and list
the source folders — one `[[paths]]` section per folder:

```toml
[daemon]
http_host = "127.0.0.1"
http_port = 8015
log_level = "info"

[[paths]]
path = "C:/Repo1C"
alias = "main"

[[paths]]
path = "C:/Repo1C-2"
alias = "second"
```

- `path` — the folder with the configuration export or the project. Use forward slashes `/`.
- `alias` — a short folder name; the AI client passes it in every request.
- `8015` — the indexer port. If it is taken, use any free one.

## Step 5. Start and check

Window 1 — the indexer:

```
C:\tools\code-index\bsl-indexer.exe daemon run
```

Window 2 — the MCP server (option A only; skip it for option B):

```
C:\tools\code-index\bsl-indexer.exe serve --transport http --port 8011 --config "C:\tools\code-index\daemon.toml"
```

Without `--config` the server does not pick up the folders from `daemon.toml`.

Check in one more window:

```
C:\tools\code-index\bsl-indexer.exe daemon status
```

Expected output: status `running` and the list of folders. A `[ready]` mark next to
a folder means its index is built. Until the mark appears the first indexing is in
progress; on a large configuration it takes noticeable time, later starts are fast.
The status output is in Russian (`статус: running`).

## Step 6. Connect the AI client

Add a block to the project's `.mcp.json` (for Claude Code) or to your client's MCP
settings.

**Option A — HTTP** (the MCP server from window 2 is running):

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

Aliases come from `daemon.toml`.

**Option B — stdio** (the client starts the server; only the indexer is running):

```json
{
  "mcpServers": {
    "code-index": {
      "command": "C:\\tools\\code-index\\bsl-indexer.exe",
      "args": ["serve", "--path", "main=C:\\Repo1C", "--path", "second=C:\\Repo1C-2"],
      "env": { "CODE_INDEX_HOME": "C:\\tools\\code-index" }
    }
  }
}
```

- Every folder in `--path` must be listed in `daemon.toml` (letter case in the path
  does not matter). Otherwise requests return `not_started` — "path is not watched
  by the daemon".
- In this option the alias comes from `--path`, not from `daemon.toml`: the AI client
  uses exactly that alias. Using the same aliases as in `daemon.toml` is convenient
  but not required.
- Backslashes are doubled in JSON: `C:\\tools\\...`.
- Always set `CODE_INDEX_HOME` in `env`: the client may not see the variable from step 3.

Restart the client. Tools such as `get_function`, `grep_code`, `get_object_structure`
should appear in its tool list.

## Step 7. Autostart at Windows logon (optional)

To avoid keeping windows open:

1. In `C:\tools\code-index` create `start-hidden.vbs`:

   ```vbscript
   Set sh = CreateObject("WScript.Shell")
   sh.Run """C:\tools\code-index\bsl-indexer.exe"" daemon run", 0, False
   WScript.Sleep 3000
   sh.Run """C:\tools\code-index\bsl-indexer.exe"" serve --transport http --port 8011 --config ""C:\tools\code-index\daemon.toml""", 0, False
   ```

   For option B the first two lines are enough — delete the last two (the pause and
   the `serve` start).

   If the path contains non-Latin characters, save the file in the system ANSI code
   page, not UTF-8.

2. Press Win+R, type `shell:startup`, Enter — the Startup folder opens.
3. Put a shortcut to `start-hidden.vbs` there.

To start right away without logging off, double-click `start-hidden.vbs`. No windows
appear; check with `daemon status` from step 5.

---

## Files that appear

| Where                            | File                        | Purpose                                     |
|----------------------------------|-----------------------------|---------------------------------------------|
| `C:\tools\code-index`            | `daemon.json`, `daemon.pid` | Service files: indexer address and PID      |
| `C:\tools\code-index`            | `daemon.log`, `serve.log`   | Logs                                        |
| Each folder from `daemon.toml`   | `.code-index\index.db`      | The index itself                            |

If a source folder is under git, add `.code-index/` to `.gitignore`.

## Upgrading

1. Stop the indexer: `C:\tools\code-index\bsl-indexer.exe daemon stop`.
2. Stop the MCP server: Task Manager → `bsl-indexer.exe` → "End task"
   (or `taskkill /IM bsl-indexer.exe /F` — ends every process with that name).
3. Replace `bsl-indexer.exe` with the file from the new archive.
4. Start again (step 5 or `start-hidden.vbs`) and restart the AI client.

## Uninstalling

1. Stop both processes (see "Upgrading", items 1–2).
2. Delete the shortcut from `shell:startup`.
3. Delete `C:\tools\code-index`.
4. Delete the `CODE_INDEX_HOME` variable (dialog from step 3).
5. Delete the `.code-index` folders inside the source folders.

## Troubleshooting

Program messages are in Russian; the key phrase is given in the first column.

| Symptom | Cause and fix |
|---|---|
| `daemon run` prints «Переменная окружения CODE_INDEX_HOME не задана» (variable not set) | The variable is missing or the window was opened before it was set — step 3, then a new window |
| `daemon_offline` with «CODE_INDEX_HOME не задана для этого процесса» (not set for this process) | The AI client does not see the variable — add `env` to `.mcp.json` (step 6, option B) and restart the client |
| `daemon_offline` with «отсутствует runtime-info файл …daemon.json» (runtime-info file missing) | The indexer is not running, or the server and the indexer use different `CODE_INDEX_HOME` — start `daemon run`, compare the paths |
| `not_started` «не отслеживается демоном» (not watched by the daemon) | The folder is not in `daemon.toml` — add a `[[paths]]` section and run `bsl-indexer.exe daemon reload` |
| `unknown_repo` | The client used an alias the server does not know: in option A — not in `daemon.toml`, in option B — not in `--path`. The answer lists the available ones |
| On start: `os error 10048` (the address is already in use) | Port 8011 or 8015 is taken (often by an already running copy of the program). Change it in the `serve` command / in `daemon.toml` and in `.mcp.json` |
| Client does not see the tools | The server is not running (option A) or the client was not restarted after editing `.mcp.json` |
| Folder in `daemon status` has no `[ready]` | First indexing is in progress — wait; progress is visible in `daemon.log` |

Full guide: [README_EN.md](../README_EN.md).
