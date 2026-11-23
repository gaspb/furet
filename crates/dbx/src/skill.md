# dbx: the user's database connections

The user keeps database connections per repo with `dbx` (stored outside the repo, shared by its
worktrees). Use it instead of asking for credentials or reading them from files.

- `dbx ls` lists the connections visible from the current directory: name, stage (local, stg, prod…),
  kind (postgres or clickhouse) and endpoint. `dbx ls --json` for the same, machine-readable.
- `dbx run <name> -- <command>` runs a command with the connection's variables set
  (`PGHOST`/`PGUSER`/`PGPASSWORD`/… or `CLICKHOUSE_*`, and `DATABASE_URL`), starting its tunnel
  (e.g. a kubectl port-forward) if it has one and stopping it afterwards. Examples:
  - `dbx run local -- psql -c 'select count(*) from users'`
  - `dbx run events -- sh -c 'clickhouse-client --host $CLICKHOUSE_HOST --port $CLICKHOUSE_PORT --query "select 1"'`
  - `dbx run local -- cargo sqlx migrate run`
- `dbx url <name>` prints the URL (it contains the password: don't echo it back to the user or into files).

Rules:
- Connections whose stage isn't local/dev/test are read-only (Postgres sessions start with
  `default_transaction_read_only`, ClickHouse with `readonly=1`). Don't try to get around it.
  `-w` allows writes; on prod it asks the user to type the name, so you can't use it there. Ask the
  user before writing anywhere but local.
- Don't run `dbx` with no arguments or `dbx <name>`: both open interactive clients.
- To add a connection: `dbx add <name> <url> [--stage stg] [--tunnel '<cmd>']`. Prefer secrets by
  reference (`password = { cmd = "…" }`, `{ env = "VAR" }`, `{ dotenv = ".env", key = "VAR" }`) in the
  file shown by `dbx path`, rather than a password in the URL.
