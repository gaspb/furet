# tui-tools

Small terminal tools for working in [herdr](https://herdr.dev) spaces next to coding agents.
Each one is scoped to the repo you're in (the main checkout, so worktrees share), keeps its state
outside the repo, and has `--json` output for the panels in [herdr-mods](https://github.com/gaspb/herdr-mods).

| Tool | |
| --- | --- |
| [dbx](crates/dbx) | Postgres and ClickHouse connections per repo: secrets by reference, kubectl tunnels, read-only remotes |

| [dbx-app](crates/dbx-app) | dbx's database explorer as a native window (GPUI), on the same engine |

Related: [sidenote](https://github.com/gaspb/sidenote), per-repo scratch notes.

## Install

```sh
curl -LsSf https://github.com/gaspb/tui-tools/releases/latest/download/dbx-installer.sh | sh
```

macOS and Linux (Intel and ARM), into `~/.local/bin`. `dbx update` updates it later. From source:
`cargo install --locked --git https://github.com/gaspb/tui-tools dbx`.

## License

MIT
