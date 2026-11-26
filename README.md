# furet

Database connections tied to the repo you're in, stored outside it. Secrets are read when a
connection opens (env, `.env`, any command), port-forwards start and stop on their own, and
anything that isn't local opens read-only.

## Quick start

```sh
curl -LsSf https://github.com/gaspb/furet/releases/latest/download/furet-installer.sh | sh
furet setup herdr         # if you use herdr: the Databases panel (optional)
cd ~/src/my-api
furet add stg 'postgres://app:secret@localhost:15432/app' --stage stg \
  --tunnel 'kubectl -n stg port-forward svc/postgres 15432:5432'
furet stg                 # tunnel up, psql read-only; the tunnel stops when you quit
```

The password goes to the OS keychain, not the file. Databases running in Docker show up on their own.
Clients are your own: `psql`, `clickhouse-client`, `kubectl` for tunnels. On Linux the keychain needs a
Secret Service (GNOME Keyring, KWallet, KeePassXC). For the herdr panel, see [herdr](#herdr).

macOS and Linux (Intel and ARM). `furet update` updates it later. From source:
`cargo install --locked --git https://github.com/gaspb/furet furet`.

There's also [furet-app](crates/app), the same explorer as a native window, on the same engine.

furet was called dbx: on first run it takes over dbx's connections, history, keychain passwords
and herdr panel.

## Commands

```
furet                     pick a connection (type to filter); back to the list when the client exits
furet <name> [-w]         open it in its client (psql, clickhouse-client, or yours)
furet ls [-a] [--json]    connections visible from here (⇄ = tunnel, green when up)
furet query <name> "sql"  run SQL through the engine, print a table (--json, --csv, --saved <q>, - for stdin)
furet drift <a> <b>       how two connections' schemas differ, and the SQL to align b (Postgres)
furet run <name> -- cmd   run cmd with PG*/CLICKHOUSE_* and DATABASE_URL set, tunnel up
furet url <name>          the URL, password included        furet env <name>   as export lines
furet tunnel <name>       keep a tunnel up until Ctrl-C
furet add <name> <url> [--stage stg] [--tunnel '<cmd>'] [--global]
furet rm <name>           remove a connection (and its keychain password)
furet include <path>      also use another clone's connections
furet cp <name> --from <path> [--as <new>]
furet edit [--global]     open the connections file in $EDITOR
furet setup herdr         the Databases panel, and furet's name and icon in the herdr app
furet update              update furet (the panel follows)
furet serve --ensure      the engine behind the herdr explorer (see below); --stop stops it
```

## Connections

`furet edit` opens this repo's file (`furet path` prints where it is):

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
- Passwords never need to be in the file: a URL given to `furet add` (or the panel's form) with a password in it is
  saved without it, and the password goes to the OS keychain (macOS Keychain, or the Secret Service on Linux:
  GNOME Keyring, KWallet, KeePassXC). The files and their folders are readable only by you. Fields override what the URL says.
  Relative paths and commands start from the current worktree (from the included repo, for its connections).
- `tunnel` runs before connecting and stops when the client exits (in the picker, when you quit). A tunnel
  belongs to the connection that started it: opening the same connection from another pane or repo reuses it.
  If the port is held by another connection's tunnel, or by a program furet didn't start, furet refuses rather than
  send your credentials there: give the tunnel another local port.
- ClickHouse: `port` is the native port (9000), for `clickhouse-client`; `http_port` (8123, 8443 with
  `?secure=1`, or `?http_port=` in the URL) is the HTTP one, used by `furet serve` (the panel). A `tunnel` forwards the one you use: HTTP for the panel, native for the client.
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
# ~/.config/furet/furet.toml — connections and settings available from every repo
[clients]
postgres = ["pgcli"]

[conn.scratch]
url = "postgres://postgres:postgres@localhost:5432/postgres"
```

Names resolve from this repo first, then included repos, then the global file.

## Databases running in Docker

Running Postgres and ClickHouse containers show up under `docker` in `furet ls`, the picker and the panel, with
nothing to configure.

- Any image: a container that exposes 5432 or sets `PGDATA` is Postgres, one that exposes 8123 is ClickHouse.
- User, password and database come from the container's environment (`POSTGRES_USER`/`POSTGRES_PASSWORD`/`POSTGRES_DB`,
  `CLICKHOUSE_*`) each time; nothing is written.
- The address is the published port (`localhost:5433`; for ClickHouse 9000's, and 8123's as `http_port`), else the
  container's IP (Linux only). A container whose port isn't published is listed with a note.
- This repo's compose services (worktrees included) come first, by service name (`furet run db -- psql`); other
  containers go by their container name, as does one whose name a configured connection already uses.
- **Save…** in the panel keeps one as a configured connection, its password read by furet into the keychain.
- `docker` is looked for on the PATH, then where Docker Desktop, Homebrew, OrbStack, Rancher Desktop and Colima
  install it (apps started from the Dock get a minimal PATH); `FURET_DOCKER_BIN` points at another one. When Docker
  can't be asked, `furet ls` and the panel say why. `FURET_DOCKER=0` turns this off.

## Storage

`~/.config/furet/projects/<repo>-<hash>/furet.toml`, and `~/.config/furet/furet.toml` for global
connections (override with `$FURET_HOME`). The project is the main checkout of the git repo, so its worktrees
share its connections. Files and folders are readable only by you; nothing is written inside the repo.

## Isolation between repos

A repo only sees its own connections, the repos it `include`s, and the global ones, and that holds in the engine too:

- Engine sessions belong to the repo that opened them; another repo's panel can't list, query or close them.
- Keychain entries are named after their repo (`<repo>#<name>`); a connection that points at another repo's entry
  is refused, and `furet cp` copies the secret instead of sharing it.
- A repo's `.env` references must stay inside it.
- Editing a connection keeps its saved password only while host, port and user stay the same.
- Tunnels are never shared between different connections (see `tunnel` above).

## History and saved queries

Every statement the engine runs is kept, per repo and connection: from the panel's consoles, from `furet query`, and
from agents through it (marked with the agent's name, so you see what they ran against stg). Saved queries are files
in the repo, `.furet/queries/<name>.sql` (a first line `-- connection: stg` says where they run), so the team shares
them through git and agents can run them: `furet query stg --saved stuck-invoices`. In the panel, Ctrl+R searches both
and Ctrl/Cmd+S saves the statement under the cursor.

## Copying rows to local

From the panel's row inspector (or a cell's menu), **Copy to…** takes a row and what it needs to a local, dev or test
connection, to reproduce a bug from stg with real data: the rows its foreign keys point at, all the way up, and with
*with the rows pointing at it*, its children (an invoice's lines, a customer's orders). A plan shows first (tables and
row counts). The copy is one transaction, parents first; rows already there stay as they are, generated columns are
skipped, and sequences move past the copied ids. *Mask personal data* (on by default) replaces emails, phones, people's
names, addresses and tax ids with stand-ins, the same for the same value. Postgres to Postgres, up to 2000 rows.

## Schema drift

`furet drift stg prod` (or **Compare schema with…** on a connection in the panel) lists the tables, columns, indexes and
constraints that are missing or different between two Postgres connections, each with the SQL that would bring the
second in line with the first: `alter table … add column …`, a full `create table` for a missing table, the index or
constraint to add or drop. furet only shows it; drops are commented out. It exits 1 when they differ, so it fits a CI
check.

## With agents

`furet --skill` prints a guide for agents: list connections, look at data with `furet query` (or run their own tools
through `furet run`), never lift read-only on their own. For Claude Code, add to `~/.claude/CLAUDE.md`:

```
- The user's database connections are managed with `furet`. Before querying a database or asking
  for credentials, run `furet --skill` and follow it.
```

From the herdr panel, **Ask agent** (on a statement, an error, a result or a table) sends your question to an agent
in the same space with what it needs: the connection and its stage, the SQL, the error, the tables involved, and how
to query on its own with `furet query`. Row values go along only from local, dev and test connections.

## herdr

`furet setup herdr` installs the Databases panel (in `panel/`, carried by the binary so it always matches the
engine) and gives furet panes their name and icon. Open it from the command palette: **Databases**. It's a database
explorer for the space's repo, on `furet serve`; the panel's own guide is in
[herdr-mods](https://github.com/gaspb/herdr-mods#furet).

Updates: the engine looks for a newer release twice a day (`FURET_NO_UPDATE_CHECK=1` turns that off) and the panel
offers **Update**, which runs `furet update`. After an update, the engine refreshes the panel it installed; herdr
reloads it live. A panel folder you linked or made yourself is left alone.

## Engine: `furet serve`

A local server that holds database sessions for the explorer, using the same connections as the CLI.
`furet serve --ensure` starts it in the background if it isn't running (one per user) and prints
`{url, token, …}`; it stops after an hour without requests and open sessions, and closes sessions
idle for 30 minutes. Postgres and ClickHouse.

- Listens on `127.0.0.1` on a random port. Every request needs `Authorization: Bearer <token>`
  and a `Host` of `127.0.0.1:<port>`, so web pages (including DNS rebinding) and other users can't
  use your connections. The token is in `~/.local/state/furet/serve.json`, readable only by you;
  your own processes can read it, as they can read your connection files.
- Secrets (`env`, `dotenv`, `cmd`) are read by the engine when a session opens: `{ env = … }` sees
  the engine's environment (the herdr app's), not the terminal you typed in.
- Read-only works as in the CLI; writes on prod need the connection's name in `confirm`.
- ClickHouse goes through its HTTP interface (`http_port`), with a server-side session per panel session
  (`USE` and `SET` last): statements run one request each, in order; cancel is `KILL QUERY`; rows aren't
  edited from the panel (ClickHouse changes them with asynchronous `ALTER TABLE … UPDATE/DELETE` mutations).
  System databases are left out of the tree, unless the connection's database is one.

| | |
| --- | --- |
| `GET /connections?cwd=` | what `furet ls --json` shows for that folder |
| `POST /sessions` `{cwd, name, write?, confirm?}` | connect (starting the tunnel) |
| `POST /sessions/:id/query` `{sql, cursor?, selection?, all?, limit?, offset?}` | run the statement under the cursor, the selection, or all; a lone read-only statement is paged with column types |
| `POST /sessions/:id/cancel` · `/mode` `{write, confirm?}` · `DELETE /sessions/:id` | |
| `GET /sessions/:id/catalog` · `/table?schema=&name=` | schemas, tables and columns; one table's columns, indexes, constraints (ClickHouse: databases; engine, keys, skipping indexes and `create`) |
| `POST /sessions/:id/apply` `{statements}` | row edits in one transaction (Postgres) |
| `POST /sessions/:id/lookup` `{schema, table, column, value \| values, limit?}` | the rows where `column = value` (or `in values`), Postgres. Result columns carry `origin` (the table column they copy) and, for a foreign key, `refs` (where it points) |
| `POST /copy` `{cwd, from, to, schema, table, column, value, children?, mask?, dry?}` | copy a row and what it needs to a local/dev/test connection (`dry`: the plan only) |
| `POST /drift` `{cwd, left, right, schema?}` | schema differences and the SQL to align `right` |
| `GET /history?cwd=&name=&limit=` | the statements that ran here (one connection, or all), newest first |
| `GET /queries?cwd=` · `POST /queries` `{cwd, name, sql, connection?}` · `DELETE /queries?cwd=&name=` | the repo's saved queries |
| `POST /sessions/:id/referrers` `{schema, table, row}` | the foreign keys pointing at that row, each with how many rows point there (counted up to 1000) |
