# dbx-app

dbx's database explorer as its own native window, for when you're not in herdr. Built on
[GPUI](https://gpui.rs) (Zed's GPU UI framework) with [gpui-kit](https://gpui-kit.com): no webview, no
JavaScript runtime.

```sh
cargo install --locked --git https://github.com/gaspb/tui-tools dbx-app
dbx-app ~/src/my-api      # the repo whose connections to show (default: the current folder)
```

It needs `dbx` on the PATH (or `DBX_BIN`): it talks to the same `dbx serve` engine as the CLI and the herdr
panel, so connections, tunnels, read-only rules and the history are the same everywhere.

- The tree: connections (with their stage), then schemas and tables. Opening a connection connects.
- A table in the tree shows its first 200 rows.
- The SQL editor (tree-sitter highlighting): Cmd/Ctrl+Enter runs the statement under the cursor, Cmd/Ctrl+Shift+Enter
  everything.
- Results in a virtualized table: values coloured by type, null in italics, foreign keys underlined, each column's
  type (and what a foreign key points at) under its name.

An early version: the herdr panel has more (row browsing by foreign keys, the inspector, history, Ask agent, copying
rows, schema drift). They come over here next, on the same engine routes.

Not in the release yet (it's a heavy build): `cargo build -p dbx-app`, or the install line above. Linux needs the usual
GPUI libraries (X11 or Wayland, Vulkan, fontconfig).
