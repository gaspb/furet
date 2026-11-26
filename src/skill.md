# furet: the user's database connections

The user keeps database connections per repo with `furet` (stored outside the repo, shared by its
worktrees). Use it instead of asking for credentials or reading them from files.

- `furet ls` lists the connections visible from the current directory: name, stage (local, stg, prod…),
  kind (postgres or clickhouse) and endpoint. `furet ls --json` for the same, machine-readable.
- `furet query <name> "<sql>"` runs SQL and prints a table: the way to look at data. `--json` (rows as objects)
  or `--csv` for parsing, `--limit N` (default 200), `-` reads the SQL from stdin, `--saved <q>` runs the repo's
  saved query `.furet/queries/<q>.sql` (look there first: they're the team's known-good queries). It goes through
  the user's furet engine, so tunnels start by themselves, read-only rules apply, and the user sees what you ran
  (and that an agent ran it) in their panel's history.
- `furet drift <a> <b>` shows how two connections' schemas differ (missing migrations, say) and the SQL to align `b`.
  Show it to the user rather than running it.
- `furet run <name> -- <command>` runs a command with the connection's variables set
  (`PGHOST`/`PGUSER`/`PGPASSWORD`/… or `CLICKHOUSE_*`, and `DATABASE_URL`), starting its tunnel
  (e.g. a kubectl port-forward) if it has one and stopping it afterwards. Examples:
  - `furet run local -- psql -c 'select count(*) from users'`
  - `furet run events -- sh -c 'clickhouse-client --host $CLICKHOUSE_HOST --port $CLICKHOUSE_PORT --query "select 1"'`
  - `furet run local -- cargo sqlx migrate run`
- `furet url <name>` prints the URL (it contains the password: don't echo it back to the user or into files).

Rules:
- Connections whose stage isn't local/dev/test are read-only (Postgres sessions start with
  `default_transaction_read_only`, ClickHouse with `readonly=1`). It's a guard against mistakes, not a
  permission system, so it's on you not to get around it (no `set default_transaction_read_only`,
  `begin read write`…). `-w` allows writes; on prod it asks for the connection's name: that question is the
  user's, never type it yourself. Ask the user before writing anywhere but local.
- Don't run `furet` with no arguments or `furet <name>`: both open interactive clients.
- To add a connection: `furet add <name> <url> [--stage stg] [--tunnel '<cmd>']` (a password in the URL goes to the
  keychain, not the file). Prefer secrets by
  reference (`password = { cmd = "…" }`, `{ env = "VAR" }`, `{ dotenv = ".env", key = "VAR" }`) in the
  file shown by `furet path`, rather than a password in the URL.
