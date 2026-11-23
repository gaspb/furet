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
tunnel = "kubectl -n prod port-forward svc/clickhouse 19000:9000"
stage = "prod"
```

- Any of `url`, `host`, `user`, `password`, `database` can be a string, `{ env = "VAR" }`,
  `{ dotenv = "path", key = "VAR" }` or `{ cmd = "shell command" }`. Fields override what the URL says.
  Relative paths and commands start from the current worktree (from the included repo, for its connections).
- `tunnel` runs before connecting, unless the port already answers (a tunnel from another pane is reused),
  and is stopped when the client exits. In the picker it stays up until you quit.
- `stage`: `local`, `dev` and `test` are writable; anything else is read-only unless `readonly = false`.
  Postgres sessions get `default_transaction_read_only`, ClickHouse `readonly=1`. `-w` lifts it,
  and on `prod` asks you to type the connection's name. It is a guard against mistakes, not a permission system:
  use a read-only database user for that.
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
idle for 30 minutes. Postgres only for now.

- Listens on `127.0.0.1` on a random port. Every request needs `Authorization: Bearer <token>`
  and a `Host` of `127.0.0.1:<port>`, so web pages (including DNS rebinding) and other users can't
  use your connections. The token is in `~/.local/state/dbx/serve.json`, readable only by you;
  your own processes can read it, as they can read your connection files.
- Secrets (`env`, `dotenv`, `cmd`) are read by the engine when a session opens: `{ env = … }` sees
  the engine's environment (the herdr app's), not the terminal you typed in.
- Read-only works as in the CLI; writes on prod need the connection's name in `confirm`.

| | |
| --- | --- |
| `GET /connections?cwd=` | what `dbx ls --json` shows for that folder |
| `POST /sessions` `{cwd, name, write?, confirm?}` | connect (starting the tunnel) |
| `POST /sessions/:id/query` `{sql, cursor?, selection?, all?, limit?, offset?}` | run the statement under the cursor, the selection, or all; a lone read-only statement is paged with column types |
| `POST /sessions/:id/cancel` · `/mode` `{write, confirm?}` · `DELETE /sessions/:id` | |
| `GET /sessions/:id/catalog` · `/table?schema=&name=` | schemas, tables and columns; one table's columns, indexes, constraints |
