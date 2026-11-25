# dbx

Database connections tied to the repo you're in, stored outside it. Secrets are read when a
connection opens (env, `.env`, any command), port-forwards start and stop on their own, and
anything that isn't local opens read-only.

```
dbx                     pick a connection (type to filter); back to the list when the client exits
dbx <name> [-w]         open it in its client (psql, clickhouse-client, or yours)
dbx ls [-a] [--json]    connections visible from here (⇄ = tunnel, green when up)
dbx run <name> -- cmd   run cmd with PG*/CLICKHOUSE_* and DATABASE_URL set, tunnel up
dbx url <name>          the URL, password included        dbx env <name>   as export lines
dbx tunnel <name>       keep a tunnel up until Ctrl-C
dbx add <name> <url> [--stage stg] [--tunnel '<cmd>'] [--global]
dbx rm <name>           remove a connection (and its keychain password)
dbx include <path>      also use another clone's connections
dbx cp <name> --from <path> [--as <new>]
dbx edit [--global]     open the connections file in $EDITOR
dbx setup herdr         name and icon in the herdr app
dbx serve --ensure      the engine behind the herdr explorer (see below); --stop stops it
```

## Connections

`dbx edit` opens this repo's file (`dbx path` prints where it is):

```toml
include = ["~/src/api"]            # another clone's connections, by reference

[conn.local]
url = { dotenv = ".env", key = "DATABASE_URL" }   # each worktree's own .env

[conn.stg]
url = "postgres://app@localhost:15432/app"
password = { cmd = "kubectl -n stg get secret pg -o jsonpath='{.data.password}' | base64 -d" }
tunnel = "kubectl -n stg port-forward svc/postgres 15432:5432"
stage = "stg"
note = "needs the VPN"

[conn.events]
kind = "clickhouse"
host = "localhost"
port = 19000
user = "readonly"
password = { env = "CH_PROD_PASSWORD" }
database = "events"
http_port = 18123                   # the panel's engine talks HTTP: forward that port too
tunnel = "kubectl -n prod port-forward svc/clickhouse 19000:9000 18123:8123"
stage = "prod"
```

- Any of `url`, `host`, `user`, `password`, `database` can be a string, `{ keychain = "…" }` (the OS keychain),
  `{ env = "VAR" }`, `{ dotenv = "path", key = "VAR" }` or `{ cmd = "shell command" }`.
- Passwords never need to be in the file: a URL given to `dbx add` (or the panel's form) with a password in it is
  saved without it, and the password goes to the OS keychain (macOS Keychain, or the Secret Service on Linux:
  GNOME Keyring, KWallet, KeePassXC). The files and their folders are readable only by you. Fields override what the URL says.
  Relative paths and commands start from the current worktree (from the included repo, for its connections).
- `tunnel` runs before connecting, unless the port already answers (a tunnel from another pane is reused),
  and is stopped when the client exits. In the picker it stays up until you quit.
- ClickHouse: `port` is the native port (9000), for `clickhouse-client`; `http_port` (8123, 8443 with
  `?secure=1`, or `?http_port=` in the URL) is the HTTP one, used by `dbx serve` (the panel). A `tunnel` forwards the one you use: HTTP for the panel, native for the client.
- `stage`: `local`, `dev` and `test` are writable; anything else is read-only unless `readonly = false`.
  Postgres sessions get `default_transaction_read_only`, ClickHouse `readonly=1` (`readonly=2` in the
  engine, which lets queries keep their `SETTINGS` but not lower `readonly`). `-w` lifts it,
  and on `prod` asks you to type the connection's name. It is a guard against mistakes, not a permission system:
  use a read-only database user for that. In the engine, statements that would lift it
  (`set default_transaction_read_only`, `begin read write`, `reset all`, `set_config`, `do`…) are refused, and
  each run re-reads the server's setting, so the panel shows what's true. Clients you open hand the session to you.
- `client = ["pgcli"]` picks another program; `{url}`, `{host}`, `{port}`, `{user}`, `{password}`, `{database}`
  are filled in. A default per kind goes in the global file:

```toml
# ~/.config/dbx/dbx.toml — connections and settings available from every repo
[clients]
postgres = ["pgcli"]

[conn.scratch]
url = "postgres://postgres:postgres@localhost:5432/postgres"
```

Names resolve from this repo first, then included repos, then the global file.

## Databases running in Docker

Running Postgres and ClickHouse containers show up without configuring anything (any image: a container that exposes
5432 or sets `PGDATA` is Postgres, one that exposes 8123 is ClickHouse),
under `docker` in `dbx ls`, the picker and the panel. User, password and database come from the container's environment
(`POSTGRES_USER`/`POSTGRES_PASSWORD`/`POSTGRES_DB`, `CLICKHOUSE_*`) each time; nothing is written. The address is the
published port (`localhost:5433`; for ClickHouse 9000's, and 8123's as `http_port`), else the container's IP (Linux only). Containers from this repo's compose file
(compose's labels put its working directory and compose files inside the repo, worktrees included) come first and
go by their service name (`dbx run db -- psql`); others by their container name. Save… stores one in this repo,
its password read by dbx into the keychain. A configured connection with the same name wins, and the container
goes by its container name. `DBX_DOCKER=0` turns this off.
`docker` is looked for on the PATH, then where Docker Desktop, Homebrew, OrbStack, Rancher Desktop and Colima install it
(apps started from the Dock get a minimal PATH); `DBX_DOCKER_BIN` points at another one. When Docker can't be asked,
`dbx ls` and the panel say why. A container whose port isn't published is listed with a note: it can't be reached
from the host (except on Linux, through its container IP).

## Storage

`~/.config/dbx/projects/<encoded-repo-path>/dbx.toml`, and `~/.config/dbx/dbx.toml` for global
connections (override with `$DBX_HOME`). The project is the main checkout of the git repo.

## With agents

`dbx --skill` prints a guide for agents: list connections, run queries through `dbx run`, never lift
read-only on their own. For Claude Code, add to `~/.claude/CLAUDE.md`:

```
- The user's database connections are managed with `dbx`. Before querying a database or asking
  for credentials, run `dbx --skill` and follow it.
```

## herdr

`dbx setup herdr` gives dbx panes their name and icon. The `dbx` mod in
[herdr-mods](https://github.com/gaspb/herdr-mods) is a database explorer (tree, consoles, table data
and structure) for the space's repo, running on `dbx serve`.

## Engine: `dbx serve`

A local server that holds database sessions for the explorer, using the same connections as the CLI.
`dbx serve --ensure` starts it in the background if it isn't running (one per user) and prints
`{url, token, …}`; it stops after an hour without requests and open sessions, and closes sessions
idle for 30 minutes. Postgres and ClickHouse.

- Listens on `127.0.0.1` on a random port. Every request needs `Authorization: Bearer <token>`
  and a `Host` of `127.0.0.1:<port>`, so web pages (including DNS rebinding) and other users can't
  use your connections. The token is in `~/.local/state/dbx/serve.json`, readable only by you;
  your own processes can read it, as they can read your connection files.
- Secrets (`env`, `dotenv`, `cmd`) are read by the engine when a session opens: `{ env = … }` sees
  the engine's environment (the herdr app's), not the terminal you typed in. A repo's `dotenv` files must be
  inside it. Editing a connection keeps its saved password only while host, port and user stay the same.
- Read-only works as in the CLI; writes on prod need the connection's name in `confirm`.
- ClickHouse goes through its HTTP interface (`http_port`), with a server-side session per panel session
  (`USE` and `SET` last): statements run one request each, in order; cancel is `KILL QUERY`; rows aren't
  edited from the panel (ClickHouse changes them with asynchronous `ALTER TABLE … UPDATE/DELETE` mutations).
  System databases are left out of the tree, unless the connection's database is one.

| | |
| --- | --- |
| `GET /connections?cwd=` | what `dbx ls --json` shows for that folder |
| `POST /sessions` `{cwd, name, write?, confirm?}` | connect (starting the tunnel) |
| `POST /sessions/:id/query` `{sql, cursor?, selection?, all?, limit?, offset?}` | run the statement under the cursor, the selection, or all; a lone read-only statement is paged with column types |
| `POST /sessions/:id/cancel` · `/mode` `{write, confirm?}` · `DELETE /sessions/:id` | |
| `GET /sessions/:id/catalog` · `/table?schema=&name=` | schemas, tables and columns; one table's columns, indexes, constraints (ClickHouse: databases; engine, keys, skipping indexes and `create`) |
| `POST /sessions/:id/apply` `{statements}` | row edits in one transaction (Postgres) |
