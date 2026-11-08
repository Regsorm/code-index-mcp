# Repository admin web panel

Russian: [README.md](README.md).

An optional part of a server deployment: when `bsl-indexer` (or `code-index`) runs on a dedicated
machine for a team, the panel lets you manage repositories from a browser — no SSH and no manual
`daemon.toml` edits. Binaries and the index format are unchanged.

## Features

- list of repositories from `[[paths]]` in `daemon.toml`: status from the daemon's `/health`, file
  count, size of sources and of the index (`.code-index`), branch and last commit for git repos;
- free disk space and memory;
- add a repository: `git clone` (private repos — URL with a token), ZIP upload from the browser
  (e.g. a 1C configuration XML dump), or a directory already copied to the server;
- `git pull` for git repos, delete a repository together with its directory;
- tail of `daemon.log` and a ready-made `.mcp.json` block for clients.

## Design

- [`app.py`](app.py) — Python 3.11+, standard library only; [`index.html`](index.html) — a single
  page, no build step, no external dependencies.
- Only the `[[paths]]` blocks of `daemon.toml` are rewritten: sections before and after them and
  their fields are kept; comments inside a changed block may be lost. The alias may be omitted — it
  is derived from the directory name exactly as `serve` does. The result is parsed again before the
  file is replaced: if the repository list or any other section differs, the file is left untouched.
  The daemon picks up the file change by itself.
- Repository directories are created and deleted only inside `ADMIN_REPOS_ROOT`; ZIP entries are
  checked for path traversal, and extraction is capped by file count and free space.
- `admin-meta.json` stores git URLs with tokens and is created with mode `0600`; the unit sets
  `UMask=0077`.
- `serve` reads the repository list at startup, so after adding or removing a repository the panel
  restarts its unit (`ADMIN_SERVE_UNIT`, via `sudo -n`). MCP clients then need to reconnect
  (in Claude Code: `/mcp` → Reconnect).
- No built-in authentication. The panel listens on an internal address and is meant to sit behind
  a reverse proxy with TLS and a password. Mutating requests require the `X-Admin: 1` header, which
  a plain cross-site HTML form cannot set (CSRF protection).

## Installation (Linux, systemd)

The daemon is assumed to be running already (see [`../systemd`](../systemd/bsl-indexer-daemon.service))
as the same user the panel will use.

```bash
sudo mkdir -p /opt/code-index/admin-panel /srv/repos
sudo cp app.py index.html /opt/code-index/admin-panel/
sudo chown -R codeindex:codeindex /srv/repos

sudo cp bsl-indexer-serve.service code-index-admin.service /etc/systemd/system/   # adjust paths
sudo cp sudoers.example /etc/sudoers.d/code-index-admin && sudo visudo -cf /etc/sudoers.d/code-index-admin
sudo systemctl daemon-reload
sudo systemctl enable --now bsl-indexer-serve code-index-admin
```

The panel user must be able to write `daemon.toml` (the daemon and the panel run as the same user)
and `ADMIN_REPOS_ROOT`.

Reverse proxy — see [`nginx.conf.example`](nginx.conf.example): basic auth for the panel, a Bearer
token for `/mcp`, streaming upload of large ZIP files.

### Environment variables

| Variable | Default | Purpose |
|---|---|---|
| `CODE_INDEX_HOME` | — (required) | daemon directory: `daemon.toml`, `daemon.json`, `daemon.log` |
| `ADMIN_REPOS_ROOT` | `/srv/repos` | where the panel creates and deletes repository directories |
| `ADMIN_HOST`, `ADMIN_PORT` | `127.0.0.1`, `8080` | panel address |
| `ADMIN_MCP_URL` | empty | MCP URL shown in the connection block |
| `ADMIN_MCP_TOKEN_FILE` | empty | file with the token for the `Authorization` header in that block |
| `ADMIN_SERVE_UNIT` | empty | `serve` unit to restart after the repository list changes |

The daemon port is read from `daemon.json`.

## Large dumps on Linux

The daemon watches every repository directory via inotify. The default `fs.inotify.max_user_watches`
is around 60k, while a large 1C dump (ERP) has more than 100k files. When the limit is hit the
daemon stops the worker with `create_watcher: OS file watch limit reached` and the repository goes
to the error state. Raise the limit:

```bash
cat <<'EOF' | sudo tee /etc/sysctl.d/90-code-index.conf
fs.inotify.max_user_watches = 2097152
fs.inotify.max_user_instances = 1024
EOF
sudo sysctl --system
sudo systemctl restart bsl-indexer-daemon
```

## Tests

```bash
cd deploy/admin-panel
python3 -m unittest test_app.py
```

They cover `daemon.toml` section preservation, alias derivation, rejection of unsafe ZIP files and
insufficient space, and refusal to delete through a symlink.

## Tested on

Ubuntu 24.04, Python 3.12, `bsl-indexer` 1.15.6; repositories — XML dumps of standard 1C
configurations, the largest about 125k files. Plus the panel unit tests.
