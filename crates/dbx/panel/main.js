// A database explorer for the space's repo, on `dbx serve`: connections → schemas → tables on the
// left, consoles and table views on the right. Open it as a tab: tab.new {mod: "dbx", name: "Databases"}.
import { View, div, svg } from "gpui-kit";
import { v_flex, h_flex, InputState, Input, Editor, EditorState, Scrollbar, ScrollHandle, VirtualListScrollHandle, v_virtual_list, h_resizable, v_resizable, resizable_panel } from "gpui-base";
import { spaces, focused, theme, settings, props, nextCommand, run } from "herdr";
import { platform } from "process";
import { call, missing, INSTALL } from "./api.js";
import { grid, tint, displayOrder } from "./grid.js";
import { blocked, changes, startEdit, commitEdit, cancelEdit, toggleDelete, editBars } from "./edit.js";
import { complete, lineCol, quoteIdent, csv, json, literal } from "./sql.js";
import { openForm, formBody, removeConnection, editFile } from "./form.js";
import { openMenu, menuLayer, menuKey } from "./menu.js";
import { helpBody } from "./help.js";
import { go, back, resetTrail, cellTarget, selected, prefetch, cellSummary, trailBar, stepLabel } from "./nav.js";
import { inspector } from "./inspector.js";
import { openFinder, closeFinder, finderKey, finderPanel, openSave, submitSave, saveBar } from "./finder.js";
import { openAsk, closeAsk, sendAsk, askBar } from "./ask.js";
import * as lineOps from "./lines.js";

const LIMITS = [100, 500, 2000];
const TREE_ROW = 26;
const SUGGEST_ROWS = 8;
// Bindings take the key itself: Cmd on macOS, Ctrl elsewhere.
const MOD = platform === "darwin" ? "cmd" : "ctrl";
const KIND_ICON = { postgres: "mods/dbx/icons/postgres.svg", clickhouse: "mods/dbx/icons/clickhouse.svg" };

function stageColor(t, stage) {
  if (["local", "dev", "test"].includes(stage)) return t.ansi2;
  if (["prod", "production"].includes(stage)) return t.ansi1;
  if (["stg", "staging", "preprod"].includes(stage)) return t.ansi3;
  return t.ansi4;
}

const compact = (n) => (n >= 1e6 ? `${(n / 1e6).toFixed(1)}M` : n >= 1e3 ? `${(n / 1e3).toFixed(1)}k` : String(n));
const RELKINDS = { v: "view", m: "materialized view" };
const bytes = (n) => (n >= 1 << 30 ? `${(n / (1 << 30)).toFixed(1)} GB` : n >= 1 << 20 ? `${(n / (1 << 20)).toFixed(1)} MB` : `${Math.ceil(n / 1024)} kB`);

export default class Dbx extends View {
  init(_props, cx) {
    this.handle = cx.focus_handle();
    this.gridHandle = cx.focus_handle();
    this.treeHandle = cx.focus_handle();
    this.menuHandle = cx.focus_handle();
    this.treeScroll = VirtualListScrollHandle.new();
    this.treeSel = null; this.ctx = null; this.origin = null;
    // The space this tab was opened in: it keeps showing that repo while other spaces are shown.
    this.spaceId = props()?.space ?? spaces().find((s) => s.active)?.id;
    this.cwd = props()?.folder ?? null;
    this.conns = null; this.error = null;
    this.sources = {};        // connection name → { session, catalog, open, schemas: Set, loading, error }
    this.tabs = []; this.active = null; this.current = null; this.seq = 0;
    this.filter = "";
    this.filterInput = InputState.new({ placeholder: "Filter tables…" });
    this.filterInput.on("change", (_e, cx) => { this.filter = this.filterInput.value().toLowerCase(); cx.notify(); });
    this.filterInput.on("submit", (_e, cx) => this.treeFromFilter(true, cx));
    this.findInput = InputState.new({ placeholder: "Search saved queries and history…" });
    this.findInput.on("change", (_e, cx) => { const t = this.activeTab(); if (t?.find) t.find.at = 0; cx.notify(); });
    this.saveInput = InputState.new({ placeholder: "e.g. stuck-invoices" });
    this.saveInput.on("submit", (_e, cx) => { const t = this.activeTab(); if (t) submitSave(this, t, cx); });
    this.askInput = InputState.new({ placeholder: "Your question (the SQL, error and tables go along)" });
    this.askInput.on("submit", (_e, cx) => { const t = this.activeTab(); if (t?.ask) sendAsk(this, t, cx); });
    this.confirmInput = InputState.new({ placeholder: "Type the connection name" });
    this.confirmInput.on("submit", (_e, cx) => this.confirmWrite(cx));
    // In the editor's own context, so they win over its Tab (indent) and Enter (newline).
    const inConsole = "DbxConsole > Input", completing = "DbxCompleting > Input";
    const line = (keystroke, op) => ({ keystroke, action: `dbx_line_${op}`, context: inConsole });
    try {
      cx.bind_keys([
        { keystroke: "tab", action: "dbx_accept", context: completing },
        { keystroke: "enter", action: "dbx_accept", context: completing },
        { keystroke: "escape", action: "dbx_dismiss", context: completing },
        { keystroke: "down", action: "dbx_next", context: completing },
        { keystroke: "up", action: "dbx_prev", context: completing },
        { keystroke: "pagedown", action: "dbx_next_page", context: completing },
        { keystroke: "pageup", action: "dbx_prev_page", context: completing },
        ...["ctrl-space", "cmd-space"].map((k) => ({ keystroke: k, action: "dbx_complete", context: inConsole })),
        { keystroke: "ctrl-r", action: "dbx_find", context: inConsole },
        { keystroke: `${MOD}-s`, action: "dbx_save", context: inConsole },
        ...["up", "down", "tab", "escape", "enter"].map((k) => ({ keystroke: k, action: `dbx_find_${k}`, context: "DbxFind > Input" })),
        ...[`${MOD}-enter`, "ctrl-enter"].map((k) => ({ keystroke: k, action: "dbx_find_run", context: "DbxFind > Input" })),
        { keystroke: "escape", action: "dbx_save_cancel", context: "DbxSave > Input" },
        { keystroke: "escape", action: "dbx_ask_cancel", context: "DbxAsk > Input" },
        { keystroke: "tab", action: "dbx_ask_next", context: "DbxAsk > Input" },
        // Free in herdr on both platforms (its Alt+↑↓, Cmd+D, Cmd+/ and tab keys stay its own).
        line(`${MOD}-c`, "copyLine"), line(`${MOD}-x`, "cutLine"), line("ctrl-d", "duplicate"),
        line("alt-shift-up", "moveUp"), line("alt-shift-down", "moveDown"), line("ctrl-/", "toggleComment"),
        { keystroke: "down", action: "dbx_filter_down", context: "DbxFilter > Input" },
        ...["tab", "right"].flatMap((k) => [
          { keystroke: k, action: `dbx_fill_where_${k}`, context: "DbxWhere > Input" },
          { keystroke: k, action: `dbx_fill_order_${k}`, context: "DbxOrder > Input" },
        ]),
        { keystroke: "escape", action: "dbx_filter_clear", context: "DbxFilter > Input" },
        ...["ctrl-enter", "cmd-enter"].map((k) => ({ keystroke: k, action: "dbx_run", context: inConsole })),
        ...["ctrl-shift-enter", "cmd-shift-enter"].map((k) => ({ keystroke: k, action: "dbx_run_all", context: inConsole })),
      ]);
    } catch (e) { this.keysError = String(e); }
    this.load(cx);
    cx.timer.every(15000, (cx) => this.load(cx, true));
    cx.spawn(async (cx) => {
      for (;;) {
        const command = await nextCommand();
        if (command === "focus") this.handle.focus();
        if (command === "run") this.runActive(false, cx);
        if (command === "refresh") this.load(cx);
        cx.notify();
      }
    });
  }

  /** The repo folder this panel is for, settled once: it never drifts to another space's. */
  folder() {
    if (this.cwd) return this.cwd;
    const s = spaces().find((x) => x.id === this.spaceId);
    const found = s?.folder || focused()?.cwd || null;
    if (found) this.cwd = found;
    return found;
  }

  /** A session request, scoped to this panel's repo: the engine refuses other repos' sessions. */
  api(method, path, body) {
    const sep = path.includes("?") ? "&" : "?";
    return call(method, `${path}${sep}cwd=${encodeURIComponent(this.folder() ?? "")}`, body);
  }


  // ── data ──────────────────────────────────────────────────────────────

  load(cx, quiet = false) {
    const cwd = this.folder();
    if (!cwd || this.loadingConns) return;
    this.loadingConns = true;
    cx.spawn(async (cx) => {
      try {
        this.conns = await call("GET", `/connections?cwd=${encodeURIComponent(cwd)}`);
        this.error = null;
        if (!this.current && this.conns.connections.length) this.current = this.conns.connections[0].name;
      } catch (e) {
        if (!quiet || !this.conns) this.error = missing(e) ? `dbx is not installed. ${INSTALL}` : String(e.message ?? e);
      } finally { this.loadingConns = false; cx.notify(); }
    });
  }

  conn(name) {
    return this.conns?.connections.find((c) => c.name === name);
  }

  async openSession(name, write = false, confirm) {
    return call("POST", "/sessions", { cwd: this.folder(), name, write, confirm });
  }

  /** Runs `fn(sessionId)` on a holder's session, opening it (again, if it expired) as needed. */
  async withSession(holder, name, fn) {
    if (!holder.session) holder.session = await this.openSession(name);
    const truthful = (out) => {
      if (typeof out?.readonly === "boolean" && holder.session) holder.session.readonly = out.readonly;
      return out;
    };
    try {
      return truthful(await fn(holder.session.id));
    } catch (e) {
      if (e.status !== 404) throw e;
      holder.session = await this.openSession(name);
      return truthful(await fn(holder.session.id));
    }
  }

  source(name) {
    return (this.sources[name] ??= { session: null, catalog: null, open: false, schemas: new Set(), loading: false, error: null });
  }

  toggleConnection(name, cx) {
    const src = this.source(name);
    if (src.open) { src.open = false; this.current = name; cx.notify(); return; }
    this.connect(name, cx);
  }

  refreshCatalog(name, cx) {
    const src = this.source(name);
    if (src.loading) return;
    src.loading = true; src.error = null;
    cx.spawn(async (cx) => {
      try {
        src.catalog = await this.withSession(src, name, (id) => this.api("GET", `/sessions/${id}/catalog`));
        const cat = src.catalog, first = cat.schemas.find((x) => cat.tables.some((y) => y.schema === x)) ?? cat.schemas[0];
        if (!src.schemas.size && first) src.schemas.add(first);
      } catch (e) { src.error = String(e.message ?? e); }
      finally { src.loading = false; this.load(cx, true); cx.notify(); }
    });
  }

  // ── tabs ──────────────────────────────────────────────────────────────

  addTab(tab, cx) {
    tab.id = `t${++this.seq}`;
    this.tabs.push(tab);
    this.active = tab.id;
    cx.notify();
    return tab;
  }

  closeTab(id, cx) {
    const i = this.tabs.findIndex((x) => x.id === id);
    if (i < 0) return;
    const [tab] = this.tabs.splice(i, 1);
    tab.editor?.release();
    for (const input of Object.values(tab.f ?? {})) input.release();
    for (const r of [tab.scroll, tab.editInput, tab.whereInput, tab.orderInput]) r?.release();
    if (tab.kind === "console" && tab.session) this.api("DELETE", `/sessions/${tab.session.id}`).catch(() => {});
    if (this.active === id) this.active = this.tabs[Math.min(i, this.tabs.length - 1)]?.id ?? null;
    cx.notify();
  }

  storageKey(name) {
    return `console:${this.folder()}:${name}`;
  }

  /** A console on `name`; `sql`, when given, is added at the end and run. */
  newConsole(name, cx, sql) {
    if (!name) return;
    const c = this.conn(name);
    const tab = this.addTab({ kind: "console", conn: name, title: `${name} console`, session: null, results: null, error: null,
      running: false, limit: 500, picked: 0, sel: null, sort: null, suggestions: [], at: 0 }, cx);
    tab.scroll = ScrollHandle.new();
    let saved = null;
    try { saved = localStorage.getItem(this.storageKey(name)); } catch {}
    const t = theme(), font = this.font();
    tab.editor = EditorState.new({ language: "sql", value: saved ?? `-- ${name}${c ? ` · ${c.stage}` : ""}\nselect 1;\n`,
      font_family: font.font_family, font_size: font.font_size,
      syntax: { keyword: t.ansi5, string: t.ansi2, comment: t.dim, function: t.ansi4, type: t.ansi3, number: t.ansi1, constant: t.ansi6 } });
    tab.editor.on("change", (_e, cx) => {
      const text = tab.editor.value();
      try { localStorage.setItem(this.storageKey(name), text); } catch {}
      this.suggest(tab, cx);
      cx.notify();
    });
    // Every Enter reports a submit; only Cmd/Ctrl+Enter (secondary) runs.
    tab.editor.on("submit", (e, cx) => { if (e?.secondary) this.run(tab, Boolean(e.shift), cx); });
    tab.editor.on("focus", () => { this.zone = "editor"; });
    if (sql) {
      const text = tab.editor.value().replace(/\s*$/, "\n\n");
      tab.editor.set_value(text + sql);
      tab.editor.set_selected_range({ start: text.length, end: text.length });
    }
    tab.editor.focus();
    cx.spawn(async (cx) => {
      try { tab.session = await this.openSession(name); } catch (e) { tab.error = { message: String(e.message ?? e) }; }
      this.load(cx, true);
      if (sql && tab.session) this.run(tab, false, cx);
      cx.notify();
    });
  }

  /** The first rows of a table, in a new console. */
  selectConsole(name, schema, table, cx) {
    this.newConsole(name, cx, `select * from ${quoteIdent(schema)}.${quoteIdent(table)} limit 100;`);
  }

  /** A table view; `where`, when given, filters it (an open one is filtered again). */
  openTable(name, schema, table, kind, cx, where) {
    const existing = this.tabs.find((x) => x.kind === "table" && x.conn === name && x.schema === schema && x.table === table);
    if (existing) {
      this.active = existing.id;
      if (where !== undefined && !changes(existing)) { existing.where = where; existing.whereInput.set_value(where); existing.offset = 0; this.loadTable(existing, cx); }
      cx.notify();
      return;
    }
    const tab = this.addTab({ kind: "table", conn: name, schema, table, relkind: kind, title: table, view: "data",
      where: "", order: "", offset: 0, limit: 200, results: null, info: null, error: null, running: false, sel: null, sort: null,
      pending: new Map(), deleted: new Set(), editing: null }, cx);
    tab.scroll = ScrollHandle.new();
    tab.editInput = InputState.new({ placeholder: "value" });
    tab.editInput.on("submit", (_e, cx) => commitEdit(tab, tab.editInput.value(), cx));
    const cols = this.source(name).catalog?.tables.find((y) => y.schema === schema && y.name === table)?.columns ?? [];
    const first = cols[0], quoted = first ? quoteIdent(first.name) : "id";
    const sample = first && !/int|numeric|float|double|real|serial|UInt|Int|Float|Decimal/.test(first.type ?? "") ? "''" : "1";
    tab.placeholders = { where: `${quoted} = ${sample}`, order: `${quoted} desc` };
    tab.whereInput = InputState.new({ placeholder: tab.placeholders.where });
    tab.orderInput = InputState.new({ placeholder: tab.placeholders.order });
    if (where) { tab.where = where; tab.whereInput.set_value(where); }
    const apply = (_e, cx) => { tab.where = tab.whereInput.value(); tab.order = tab.orderInput.value(); tab.sort = null; tab.offset = 0; this.loadTable(tab, cx); };
    tab.whereInput.on("submit", apply);
    tab.orderInput.on("submit", apply);
    this.loadTable(tab, cx);
    cx.spawn(async (cx) => {
      try { tab.info = await this.withSession(this.source(name), name, (id) => this.api("GET", `/sessions/${id}/table?schema=${encodeURIComponent(schema)}&name=${encodeURIComponent(table)}`)); }
      catch (e) { tab.infoError = String(e.message ?? e); }
      cx.notify();
    });
  }

  loadTable(tab, cx) {
    if (changes(tab)) { this.flash("Submit or revert the pending changes first", cx); return; }
    const rel = `${quoteIdent(tab.schema)}.${quoteIdent(tab.table)}`;
    const sql = `select * from ${rel}${tab.where.trim() ? ` where ${tab.where}` : ""}${tab.order.trim() ? ` order by ${tab.order}` : ""}`;
    tab.running = true; tab.error = null;
    resetTrail(tab);
    cx.spawn(async (cx) => {
      try {
        const out = await this.withSession(this.source(tab.conn), tab.conn,
          (id) => this.api("POST", `/sessions/${id}/query`, { sql, cursor: 0, limit: tab.limit, offset: tab.offset }));
        tab.results = out.results; tab.error = out.error; tab.elapsed = out.elapsed_ms; tab.sel = null;
        prefetch(this, tab, cx);
      } catch (e) { tab.error = { message: String(e.message ?? e) }; }
      finally { tab.running = false; cx.notify(); }
    });
    cx.notify();
  }

  /** A header click: table views sort on the server (all rows), consoles sort the loaded page. */
  sortBy(tab, col, cx) {
    const dir = tab.sort?.col !== col ? "asc" : tab.sort.dir === "asc" ? "desc" : null;
    tab.sort = dir ? { col, dir } : null;
    if (tab.kind === "table" && !tab.step) {
      if (changes(tab)) { this.flash("Submit or revert the pending changes first", cx); return; }
      const name = tab.results?.[0]?.columns[col]?.name;
      tab.order = dir && name ? `${quoteIdent(name)} ${dir}` : "";
      tab.orderInput.set_value(tab.order);
      tab.offset = 0;
      this.loadTable(tab, cx);
    }
    cx.notify();
  }

  /** Keys while a grid has the keyboard (it takes it when clicked). */
  gridKey(tab, e, cx) {
    const k = e.keystroke;
    if (tab.editing || menuKey(this, e, cx)) return;
    if (tab.kind === "table" && tab.sel && k === "enter") startEdit(this, tab, cx);
    else if (tab.kind === "table" && tab.sel && (k === "delete" || k === "backspace")) toggleDelete(this, tab, cx);
    else if (tab.sel && (k === "shift-enter" || k === "shift-f10")) {
      const o = this.origin ?? { x: 0, y: 0, width: 800, height: 600 };
      this.cellMenu(tab, tab.sel.row, tab.sel.col, { position: { x: o.x + o.width / 2, y: o.y + o.height / 2 } }, cx);
    }
    else if (tab.sel && (k === "cmd-c" || k === "ctrl-c")) {
      const set = tab.results?.[tab.picked ?? 0], key = `${tab.sel.row}:${tab.sel.col}`;
      const v = tab.pending?.has(key) ? tab.pending.get(key) : set?.rows[tab.sel.row]?.[tab.sel.col];
      if (tab.sel.col !== null && v !== undefined) this.copy(v ?? "", "the cell", cx);
    } else if (k === "cmd-enter" || k === "ctrl-enter") {
      const target = cellTarget(tab);
      if (target) go(this, tab, target, cx);
      else this.flash("⌘/Ctrl+Enter follows a foreign key: this column isn't one", cx);
    } else if (k === "r") this.referrersMenu(tab, cx);
    else if (k === "space") { tab.inspector = tab.inspector === false; cx.notify(); }
    else if (k === "escape" && tab.trail?.length) back(tab, cx);
    else if (k === "escape") this.focusZone(tab.kind === "console" ? "editor" : "where", cx);
    else if (["up", "down", "left", "right"].includes(k)) {
      const [dr, dc] = { up: [-1, 0], down: [1, 0], left: [0, -1], right: [0, 1] }[k];
      this.moveSel(tab, dr, dc, cx);
    } else return;
    cx.stop_propagation();
  }

  pick(tab, row, col, double, cx, follow = false) {
    tab.sel = { row, col };
    this.gridHandle.focus();
    this.zone = "grid";
    const target = follow ? cellTarget(tab) : null;
    if (target) { go(this, tab, target, cx); return; }
    selected(this, tab, cx);
    if (double && tab.kind === "table") startEdit(this, tab, cx);
    cx.notify();
  }

  /** Arrow keys over a grid: move the selected cell (in display order). */
  moveSel(tab, dr, dc, cx) {
    const set = tab.results?.[tab.picked ?? 0];
    if (!set || !set.rows.length) return;
    const order = tab.kind === "console" || tab.step ? displayOrder(set, tab.sort) : set.rows.map((_, i) => i);
    const cur = tab.sel ?? { row: order[0], col: 0 };
    const k = Math.max(0, Math.min(order.length - 1, order.indexOf(cur.row) + dr));
    tab.sel = { row: order[k], col: Math.max(0, Math.min(set.columns.length - 1, (cur.col ?? 0) + dc)) };
    if (dr) selected(this, tab, cx);
    cx.notify();
  }

  // ── console ───────────────────────────────────────────────────────────

  activeTab() {
    return this.tabs.find((x) => x.id === this.active);
  }

  runActive(all, cx) {
    const tab = this.activeTab();
    if (tab?.kind === "console") this.run(tab, all, cx);
  }

  run(tab, all, cx, offset = 0) {
    if (tab.running || !tab.editor) return;
    const sql = tab.editor.value(), sel = tab.editor.selected_range();
    tab.running = true; tab.error = null; tab.startedAt = Date.now(); tab.suggestions = [];
    resetTrail(tab);
    const body = { sql, all, limit: tab.limit, offset, cursor: sel.start };
    if (sel.end !== sel.start) body.selection = [sel.start, sel.end];
    tab.lastBody = body;
    cx.spawn(async (cx) => {
      try {
        const out = await this.withSession(tab, tab.conn, (id) => this.api("POST", `/sessions/${id}/query`, body));
        tab.results = out.results; tab.error = out.error; tab.elapsed = out.elapsed_ms; tab.paged = out.paged; tab.disconnected = false;
        tab.offset = offset; tab.picked = Math.max(0, out.results.length - 1); tab.sel = null; tab.sort = null;
        prefetch(this, tab, cx);
        if (out.error?.position !== undefined && out.error?.position !== null) {
          tab.editor.set_selected_range({ start: out.error.position, end: out.error.position });
        }
      } catch (e) { tab.error = { message: String(e.message ?? e) }; }
      finally { tab.running = false; cx.notify(); }
    });
    cx.notify();
  }

  page(tab, delta, cx) {
    if (tab.step) return;
    if (tab.kind === "table") { tab.offset = Math.max(0, tab.offset + delta * tab.limit); this.loadTable(tab, cx); return; }
    if (!tab.lastBody || tab.running) return;
    const offset = Math.max(0, (tab.offset ?? 0) + delta * tab.limit);
    const body = { ...tab.lastBody, offset };
    tab.running = true;
    cx.spawn(async (cx) => {
      try {
        const out = await this.withSession(tab, tab.conn, (id) => this.api("POST", `/sessions/${id}/query`, body));
        tab.results = out.results; tab.error = out.error; tab.offset = offset; tab.sel = null; tab.sort = null; tab.lastBody = body;
      } catch (e) { tab.error = { message: String(e.message ?? e) }; }
      finally { tab.running = false; cx.notify(); }
    });
  }

  cancel(tab, cx) {
    const id = tab.kind === "console" ? tab.session?.id : this.source(tab.conn).session?.id;
    if (id) this.api("POST", `/sessions/${id}/cancel`).catch(() => {});
    cx.notify();
  }

  /** `explicit` (Ctrl+Space) lists candidates with nothing typed; the list stays explicit while open. */
  suggest(tab, cx, explicit = false) {
    if (this.editingLines) return;
    const sel = tab.editor.selected_range();
    if (sel.start !== sel.end) { tab.suggestions = []; return; }
    explicit = explicit || (tab.explicit && tab.suggestions.length > 0);
    const r = complete(tab.editor.value(), sel.start, this.source(tab.conn).catalog, { explicit });
    tab.suggestions = r.items; tab.suggestFrom = r.start; tab.at = 0; tab.explicit = explicit && r.items.length > 0;
    const src = this.source(tab.conn);
    if (!src.catalog && !src.error) this.refreshCatalog(tab.conn, cx);
  }

  accept(tab, item, cx) {
    const sel = tab.editor.selected_range();
    tab.editor.set_selected_range({ start: tab.suggestFrom, end: sel.start });
    tab.editor.replace(item.label);
    tab.suggestions = []; tab.explicit = false;
    tab.editor.focus();
    cx.notify();
  }

  /** ±1 wraps around; a page stops at the ends. */
  moveSuggestion(delta, cx) {
    const c = this.activeTab(), n = c?.suggestions?.length ?? 0;
    if (!n) return;
    c.at = Math.abs(delta) === 1 ? (c.at + delta + n) % n : Math.max(0, Math.min(n - 1, c.at + delta));
    cx.notify();
  }

  /** A line command (see lines.js) on the active console's editor. */
  lineCommand(op, cx, ...args) {
    const tab = this.activeTab();
    if (tab?.kind !== "console" || !tab.editor) return;
    const r = lineOps[op](tab.editor.value(), tab.editor.selected_range(), ...args);
    if (r.clip !== undefined) cx.write_to_clipboard(r.clip);
    this.editingLines = true;
    try {
      if (r.replace) { tab.editor.set_selected_range(r.replace); tab.editor.replace(r.text); }
      if (r.select) tab.editor.set_selected_range(r.select);
    } finally { this.editingLines = false; }
    tab.suggestions = [];
    tab.editor.focus();
    cx.notify();
  }

  paste(tab, cx) {
    const text = cx.read_from_clipboard();
    if (text !== undefined) tab.editor.replace(text);
    tab.editor.focus();
    cx.notify();
  }

  /** Whose session a tab runs on: a console its own, a table view the connection's explorer session. */
  holder(tab) {
    return tab.kind === "console" ? tab : this.source(tab.conn);
  }

  toggleWrite(tab, cx) {
    const s = this.holder(tab).session;
    if (!s) return;
    if (s.readonly && s.prod) { tab.confirming = true; this.confirmInput.set_value(""); this.confirmInput.focus(); cx.notify(); return; }
    this.setMode(tab, s.readonly, undefined, cx);
  }

  confirmWrite(cx) {
    const tab = this.activeTab();
    if (!tab?.confirming) return;
    tab.confirming = false;
    this.setMode(tab, true, this.confirmInput.value().trim(), cx);
  }

  setMode(tab, write, confirm, cx) {
    cx.spawn(async (cx) => {
      const h = this.holder(tab);
      try { h.session = await this.api("POST", `/sessions/${h.session.id}/mode`, { write, confirm }); }
      catch (e) { tab.error = { message: String(e.message ?? e) }; }
      cx.notify();
    });
  }

  /** After the form saved `name` (maybe renamed from tab.previous): reconnect with the new settings. */
  saved(tab, name, notes, cx) {
    if (tab.previous) this.forget(tab.previous, cx);
    this.forget(name, cx);
    this.closeTab(tab.id, cx);
    this.current = name;
    this.flash([`Saved ${name}`, ...notes].join(" · "), cx);
  }

  /** Whether the engine holds a session on `name` for this repo (the panel's, or one it doesn't know yet). */
  isConnected(name) {
    return Boolean(this.sources[name]?.session) || this.tabs.some((x) => x.conn === name && x.session)
      || (this.conn(name)?.sessions ?? 0) > 0;
  }

  connect(name, cx) {
    const src = this.source(name);
    src.open = true;
    this.current = name;
    if (!src.session) this.refreshCatalog(name, cx);
    cx.notify();
  }

  /** Closes every session on `name` (the tree's, the table views', the consoles'): its tunnel stops. */
  disconnect(name, cx) {
    this.menu = null;
    const src = this.source(name), row = this.conn(name);
    src.session = null;
    src.open = false;
    if (row) row.sessions = 0;
    for (const tab of this.tabs.filter((x) => x.conn === name)) {
      if (tab.kind === "console") { tab.session = null; tab.disconnected = true; }
    }
    cx.spawn(async (cx) => {
      try {
        const out = await call("POST", "/connections/disconnect", { cwd: this.folder(), name });
        const tunnel = this.conn(name)?.tunnel ? " · tunnel stopped" : "";
        this.flash(`Disconnected ${name}${out.closed ? tunnel : ""}`, cx);
      } catch (e) { this.flash(String(e.message ?? e), cx); }
      this.load(cx, true);
    });
    cx.notify();
  }

  /** Drops what the panel holds for a connection (its explorer session, catalog) and reloads the list. */
  forget(name, cx) {
    const src = this.sources[name];
    if (src?.session) this.api("DELETE", `/sessions/${src.session.id}`).catch(() => {});
    delete this.sources[name];
    this.load(cx);
  }

  flash(message, cx) {
    this.toast = message;
    cx.timer.after(2500, (cx) => { this.toast = null; cx.notify(); });
    cx.notify();
  }

  copy(text, what, cx) {
    cx.write_to_clipboard(text);
    this.toast = `Copied ${what}`;
    cx.timer.after(1500, (cx) => { this.toast = null; cx.notify(); });
    cx.notify();
  }

  font() {
    const s = settings();
    return { font_family: s.font_family ?? "JetBrains Mono", font_size: Math.max(11, (s.font_size ?? 13) - 1) };
  }

  // ── explorer ──────────────────────────────────────────────────────────

  treeRows() {
    const out = [];
    let docker = false;
    for (const c of this.conns?.connections ?? []) {
      if (c.detected && !docker) { docker = true; out.push({ key: "d\u0001", depth: 0, type: "section", text: "Running in Docker" }); }
      const src = this.sources[c.name];
      out.push({ key: `c\u0001${c.name}`, depth: 0, type: "conn", c, src });
      if (!src?.open) continue;
      if (src.error) { out.push({ key: `e\u0001${c.name}`, depth: 1, type: "error", text: src.error }); continue; }
      if (!src.catalog) { out.push({ key: `l\u0001${c.name}`, depth: 1, type: "note", text: src.loading ? "Connecting…" : "" }); continue; }
      const f = this.filter;
      for (const s of src.catalog.schemas) {
        const tables = src.catalog.tables.filter((x) => x.schema === s && (!f || x.name.toLowerCase().includes(f)));
        if (f && !tables.length) continue;
        const open = src.schemas.has(s) || Boolean(f);
        out.push({ key: `s\u0001${c.name}\u0001${s}`, depth: 1, type: "schema", name: s, count: tables.length, open });
        if (open) for (const x of tables) out.push({ key: `t\u0001${c.name}\u0001${s}\u0001${x.name}`, depth: 2, type: "table", x });
      }
    }
    if (this.conns?.docker) {
      if (!docker) out.push({ key: "d\u0001", depth: 0, type: "section", text: "Running in Docker" });
      out.push({ key: "dx\u0001", depth: 1, type: "note", text: this.conns.docker });
    }
    return out;
  }

  clickTree(key, cx) {
    const [type, name, schema, table] = key.split("\u0001");
    if (["c", "s", "t"].includes(type)) { this.treeSel = key; this.zone = "tree"; this.treeHandle.focus(); }
    if (type === "c") this.toggleConnection(name, cx);
    else if (type === "s") {
      const src = this.source(name);
      src.schemas.has(schema) ? src.schemas.delete(schema) : src.schemas.add(schema);
      this.current = name;
      cx.notify();
    } else if (type === "t") {
      this.current = name;
      const x = this.source(name).catalog?.tables.find((y) => y.schema === schema && y.name === table);
      this.openTable(name, schema, table, x?.kind, cx);
    }
  }

  endpoint(c, s) {
    if (!s) return `${c.kind ?? "?"} · ${c.endpoint}`;
    return `${s.kind} · ${s.host}:${s.port}${s.database ? `/${s.database}` : ""}`;
  }

  treeRow(r, t) {
    const base = h_flex().id(r.key).h(TREE_ROW).flex_none().pl(8 + r.depth * 14).pr(8).gap(6).items_center().rounded(5);
    const chevron = (open) => div().w(10).flex_none().text_size(9).text_color(t.dim).child(open ? "▾" : "▸");
    if (r.type === "conn") {
      const { c, src } = r;
      const icon = KIND_ICON[c.kind];
      return this.selectable(base, r, t).when(this.current === c.name && this.treeSel !== r.key, (e) => e.bg(t.hover))
        .child(chevron(src?.open))
        .when(Boolean(icon), (e) => e.child(svg(icon).size(13).flex_none().text_color(c.kind === "clickhouse" ? t.ansi3 : t.ansi4)))
        .when(!icon, (e) => e.child(div().w(8).h(8).flex_none().rounded(4).bg(stageColor(t, c.stage))))
        .child(div().font_semibold().truncate().child(c.name))
        .child(div().flex_1().min_w(0).truncate().text_size(11).text_color(t.dim).child(this.endpoint(c, src?.session)))
        .when(c.tunnel, (e) => e.child(div().text_size(11).text_color(c.tunnel_up ? t.ansi2 : t.dim).child("⇄")))
        .when(this.isConnected(c.name), (e) => e.child(div().text_size(10).text_color(t.ansi2).child("connected")))
        .child(div().text_size(10).text_color(stageColor(t, c.stage)).child(c.stage));
    }
    if (r.type === "section") {
      return h_flex().id(r.key).h(TREE_ROW).flex_none().px(10).items_end().pb(3).text_size(10.5).text_color(t.dim).child(r.text);
    }
    if (r.type === "schema") {
      return this.selectable(base, r, t).child(chevron(r.open)).child(div().text_color(t.muted).child("◫")).child(div().flex_1().truncate().child(r.name))
        .child(div().text_size(11).text_color(t.dim).child(String(r.count)));
    }
    if (r.type === "table") {
      const glyph = r.x.kind === "view" ? "◇" : r.x.kind === "materialized view" ? "◆" : "▦";
      const open = this.tabs.find((x) => x.id === this.active && x.kind === "table" && x.table === r.x.name && x.schema === r.x.schema);
      return this.selectable(base, r, t).when(Boolean(open) && this.treeSel !== r.key, (e) => e.bg(t.selected)).child(div().w(10).flex_none())
        .child(div().text_color(r.x.kind === "table" ? t.ansi4 : t.ansi5).child(glyph))
        .child(div().flex_1().min_w(0).truncate().child(r.x.name))
        .child(div().text_size(11).text_color(t.dim).child(r.x.kind === "table" ? compact(r.x.rows) : ""));
    }
    return base.child(div().flex_1().min_w(0).truncate().text_size(11).text_color(r.type === "error" ? t.ansi1 : t.dim).child(r.text));
  }

  /** The keyboard's row in the tree: outlined, filled while the tree has the keyboard. */
  selectable(el, r, t) {
    if (this.treeSel !== r.key) return el;
    return this.treeHandle.is_focused?.() ? el.bg(t.selected).border_1().border_color(t.accent) : el.border_1().border_color(t.border);
  }

  // ── keyboard: tree, zones, tabs ───────────────────────────────────────

  treeKey(e, cx) {
    if (menuKey(this, e, cx)) return;
    const k = e.keystroke, rows = this.treeRows();
    const pickable = rows.filter((r) => ["conn", "schema", "table"].includes(r.type));
    if (!pickable.length) return;
    let i = pickable.findIndex((r) => r.key === this.treeSel);
    const cur = pickable[i], [type, name, schema, table] = cur?.key.split("\u0001") ?? [];
    const go = (j) => { const r = pickable[Math.max(0, Math.min(pickable.length - 1, j))]; this.treeSel = r.key; this.treeScroll.scroll_to_item(rows.indexOf(r)); };
    const parent = () => go(pickable.findLastIndex((r, j) => j < i && r.depth < cur.depth));
    if (k === "down") go(i < 0 ? 0 : i + 1);
    else if (k === "up") go(i < 0 ? 0 : i - 1);
    else if (k === "home") go(0);
    else if (k === "end") go(pickable.length - 1);
    else if (!cur) return;
    else if (k === "right") {
      if (type === "c" && !this.source(name).open) this.connect(name, cx);
      else if (type === "s" && !cur.open) this.clickTree(cur.key, cx);
      else if (type !== "t") go(i + 1);
    } else if (k === "left") {
      if (type === "c" && this.source(name).open) this.toggleConnection(name, cx);
      else if (type === "s" && cur.open && !this.filter) this.clickTree(cur.key, cx);
      else parent();
    } else if (k === "enter") { this.clickTree(cur.key, cx); this.treeHandle.focus(); }
    else if (k === "cmd-enter" || k === "ctrl-enter") {
      if (type === "t") this.selectConsole(name, schema, table, cx);
      else this.newConsole(name, cx);
    } else if (k === "/") this.filterInput.focus();
    else if (k === "?" || k === "shift-/") this.openHelp(cx);
    else if (k === "shift-f10" || k === "shift-enter" || k === "menu") {
      const o = this.origin ?? { x: 0, y: 0 };
      openMenu(this, { position: { x: o.x + 24, y: o.y + 90 } }, this.treeItems(cur.key, cx), cx);
    } else return;
    cx.stop_propagation();
    cx.notify();
  }

  /** From the filter: ↓ (or Enter, which also opens it) moves to the first table that matches. */
  treeFromFilter(open, cx) {
    const rows = this.treeRows();
    const first = rows.find((r) => r.type === "table") ?? rows.find((r) => ["conn", "schema"].includes(r.type));
    if (!first) return;
    this.treeSel = first.key;
    this.treeHandle.focus();
    this.zone = "tree";
    if (open) this.clickTree(first.key, cx);
    cx.notify();
  }

  /** Back to where the keyboard was, after a menu. */
  refocus(cx) {
    if (this.zone) this.focusZone(this.zone, cx);
    else this.handle.focus();
  }

  zones() {
    const tab = this.activeTab(), out = ["tree"];
    if (tab?.kind === "console") out.push("editor");
    if (tab?.kind === "table" && tab.view === "data") out.push("where");
    if ((tab?.kind === "console" || tab?.kind === "table") && tab.results?.[tab.picked ?? 0]?.columns.length) out.push("grid");
    return out;
  }

  focusZone(zone, cx) {
    const tab = this.activeTab();
    this.zone = zone;
    if (zone === "tree") {
      this.treeHandle.focus();
      if (!this.treeSel) this.treeSel = this.treeRows().find((r) => r.type === "conn")?.key ?? null;
    } else if (zone === "editor") tab?.editor?.focus();
    else if (zone === "where") tab?.whereInput?.focus();
    else if (zone === "grid") {
      if (!tab.sel) tab.sel = { row: 0, col: 0 };
      this.gridHandle.focus();
    }
    cx.notify();
  }

  cycleZone(delta, cx) {
    const z = this.zones(), i = z.indexOf(this.zone);
    this.focusZone(z[(Math.max(0, i) + delta + z.length) % z.length], cx);
  }

  cycleTab(delta, cx) {
    if (!this.tabs.length) return;
    const i = this.tabs.findIndex((x) => x.id === this.active);
    const tab = this.tabs[(i + delta + this.tabs.length) % this.tabs.length];
    this.active = tab.id;
    this.current = tab.conn ?? this.current;
    if (tab.kind === "console") tab.editor?.focus();
    cx.notify();
  }

  openHelp(cx) {
    const existing = this.tabs.find((x) => x.kind === "help");
    if (existing) { this.active = existing.id; cx.notify(); return; }
    this.addTab({ kind: "help", title: "Keys" }, cx);
  }

  // ── right-click menus ─────────────────────────────────────────────────

  treeItems(key, cx) {
    const [type, name, schema, table] = key.split("\u0001");
    const c = this.conn(name);
    if (type === "c") {
      const own = c && !c.detected && ["this repo", "global"].includes(c.source);
      return [
        this.isConnected(name)
          ? { label: "Disconnect", run: (cx) => this.disconnect(name, cx) }
          : { label: "Connect", run: (cx) => this.connect(name, cx) },
        { label: "New console", hint: "⌘↵", run: (cx) => this.newConsole(name, cx) },
        this.source(name).open && { label: "Refresh", run: (cx) => this.refreshCatalog(name, cx) },
        "-",
        { label: "Copy name", run: (cx) => this.copy(name, "the name", cx) },
        c?.detected && { label: "Save…", run: (cx) => openForm(this, name, cx, { fromDetected: true }) },
        own && { label: "Edit…", run: (cx) => openForm(this, name, cx) },
        !c?.detected && { label: "Open the file", run: () => editFile(this, c?.source === "global") },
        own && "-",
        own && { label: "Remove…", danger: true, run: (cx) => { this.menu = name; this.confirmRemove = true; cx.notify(); } },
      ];
    }
    if (type === "s") {
      const open = this.source(name).schemas.has(schema);
      return [
        { label: open ? "Collapse" : "Expand", run: (cx) => this.clickTree(key, cx) },
        { label: "Copy name", run: (cx) => this.copy(schema, "the name", cx) },
        { label: "Refresh", run: (cx) => this.refreshCatalog(name, cx) },
      ];
    }
    if (type === "t") {
      const kind = this.source(name).catalog?.tables.find((y) => y.schema === schema && y.name === table)?.kind;
      const qualified = `${quoteIdent(schema)}.${quoteIdent(table)}`;
      return [
        { label: "Open", hint: "↵", run: (cx) => this.openTable(name, schema, table, kind, cx) },
        { label: "Structure", run: (cx) => { this.openTable(name, schema, table, kind, cx); const t = this.activeTab(); if (t?.kind === "table") t.view = "structure"; } },
        { label: "First rows in a console", hint: "⌘↵", run: (cx) => this.selectConsole(name, schema, table, cx) },
        "-",
        { label: "Copy name", run: (cx) => this.copy(table, "the name", cx) },
        { label: "Copy qualified name", run: (cx) => this.copy(qualified, qualified, cx) },
        { label: "Copy SELECT", run: (cx) => this.copy(`select * from ${qualified};`, "the SELECT", cx) },
      ];
    }
    return [];
  }

  /**
   * Tab or → in a table view's WHERE/ORDER BY: an empty field takes its placeholder (the value
   * part selected, to type over); otherwise Tab moves on and → moves the cursor.
   */
  fillField(which, key, cx) {
    const tab = this.activeTab();
    if (tab?.kind !== "table") return;
    const input = which === "where" ? tab.whereInput : tab.orderInput;
    const text = input.value(), sel = input.selected_range();
    if (!text) {
      const fill = tab.placeholders[which];
      input.set_value(fill);
      const eq = fill.indexOf("= ");
      input.set_selected_range(eq >= 0 ? { start: eq + 2, end: fill.length } : { start: fill.length, end: fill.length });
    } else if (key === "right") {
      const at = sel.start === sel.end ? Math.min(text.length, sel.end + 1) : Math.max(sel.start, sel.end);
      input.set_selected_range({ start: at, end: at });
    } else if (which === "where") tab.orderInput.focus();
    else this.focusZone("grid", cx);
    cx.notify();
  }

  /** A right-click on a result cell. */
  cellMenu(tab, row, col, e, cx) {
    const set = tab.results?.[tab.picked ?? 0];
    if (!set || col === null || !set.columns[col]) return;
    tab.sel = { row, col };
    const key = `${row}:${col}`, v = tab.pending?.has(key) ? tab.pending.get(key) : set.rows[row][col];
    const name = set.columns[col].name, record = Object.fromEntries(set.columns.map((c, i) => [c.name, set.rows[row][i]]));
    const editable = tab.kind === "table" && !blocked(tab);
    const filter = (not) => {
      const cond = v === null ? `${quoteIdent(name)} is ${not ? "not " : ""}null` : `${quoteIdent(name)} ${not ? "<>" : "="} ${literal(v)}`;
      tab.where = tab.where.trim() ? `${tab.where.trim()} and ${cond}` : cond;
      tab.whereInput.set_value(tab.where);
      tab.offset = 0;
      this.loadTable(tab, cx);
    };
    const fk = set.columns[col].refs;
    const referrers = set._referrers?.get(row);
    openMenu(this, e, [
      fk && v !== null && { label: `Go to ${stepLabel({ ...fk, value: v })}`, hint: "⌘↵", run: (cx) => go(this, tab, { ...fk, value: v }, cx) },
      fk && v !== null && { label: `Open ${fk.table} at this row`, run: (cx) => this.openTable(tab.conn, fk.schema, fk.table, "table", cx, `${quoteIdent(fk.column)} = ${literal(v)}`) },
      ...(Array.isArray(referrers) ? referrers.filter((x) => x.count).map((x) => ({
        label: `${x.table}.${x.column} · ${x.count}${x.more ? "+" : ""} row${x.count === 1 ? "" : "s"}`, hint: "r",
        run: (cx) => go(this, tab, { ...x, reverse: true }, cx) })) : []),
      (fk && v !== null) || (Array.isArray(referrers) && referrers.some((x) => x.count)) ? "-" : null,
      { label: "Copy value", hint: "⌘C", run: (cx) => this.copy(v ?? "", "the value", cx) },
      { label: "Copy column name", run: (cx) => this.copy(name, "the column name", cx) },
      { label: "Copy row as JSON", run: (cx) => this.copy(JSON.stringify(record, null, 2), "the row as JSON", cx) },
      { label: "Copy row as CSV", run: (cx) => this.copy(csv(set.columns, [set.rows[row]]), "the row as CSV", cx) },
      { label: "Copy column", run: (cx) => this.copy(set.rows.map((r) => r[col] ?? "").join("\n"), `${set.rows.length} values`, cx) },
      tab.kind === "table" && "-",
      tab.kind === "table" && { label: v === null ? "Only rows where it's null" : "Only rows with this value", run: () => filter(false) },
      tab.kind === "table" && { label: v === null ? "Only rows where it isn't null" : "Rows without this value", run: () => filter(true) },
      editable && "-",
      editable && { label: "Edit", hint: "↵", run: (cx) => startEdit(this, tab, cx) },
      editable && { label: "Set NULL", run: (cx) => { tab.editing = { row, col }; commitEdit(tab, null, cx); } },
      editable && { label: tab.deleted.has(row) ? "Keep the row" : "Delete the row", hint: "Del", danger: !tab.deleted.has(row), run: (cx) => toggleDelete(this, tab, cx) },
    ], cx);
  }

  headerMenu(tab, col, e, cx) {
    const set = tab.results?.[tab.picked ?? 0], name = set?.columns[col]?.name;
    if (!name) return;
    openMenu(this, e, [
      { label: "Sort ascending", run: (cx) => { tab.sort = null; this.sortBy(tab, col, cx); } },
      { label: "Sort descending", run: (cx) => { tab.sort = { col, dir: "asc" }; this.sortBy(tab, col, cx); } },
      tab.sort?.col === col && { label: "Clear the sort", run: (cx) => { tab.sort = { col, dir: "desc" }; this.sortBy(tab, col, cx); } },
      "-",
      { label: "Copy column name", run: (cx) => this.copy(name, "the column name", cx) },
      { label: "Copy column", run: (cx) => this.copy(set.rows.map((r) => r[col] ?? "").join("\n"), `${set.rows.length} values`, cx) },
    ], cx);
  }

  editorMenu(tab, e, cx) {
    const sel = tab.editor.selected_range(), picked = sel.start !== sel.end;
    openMenu(this, e, [
      { label: picked ? "Run the selection" : "Run the statement", hint: "⌘↵", run: (cx) => this.run(tab, false, cx) },
      { label: "Run everything", hint: "⌘⇧↵", run: (cx) => this.run(tab, true, cx) },
      "-",
      { label: picked ? "Cut" : "Cut the line", hint: "⌘X", run: (cx) => this.lineCommand("cutLine", cx) },
      { label: picked ? "Copy" : "Copy the line", hint: "⌘C", run: (cx) => this.lineCommand("copyLine", cx) },
      { label: "Paste", hint: "⌘V", run: (cx) => this.paste(tab, cx) },
      "-",
      { label: "Duplicate", hint: "Ctrl+D", run: (cx) => this.lineCommand("duplicate", cx) },
      { label: "Comment in or out", hint: "Ctrl+/", run: (cx) => this.lineCommand("toggleComment", cx) },
      { label: "Delete the line", run: (cx) => this.lineCommand("deleteLine", cx) },
      "-",
      { label: "Suggestions", hint: "Ctrl+Space", run: (cx) => { this.suggest(tab, cx, true); tab.editor.focus(); } },
      "-",
      { label: "Find a query…", hint: "Ctrl+R", run: (cx) => openFinder(this, tab, cx) },
      { label: "Save the statement…", hint: "⌘S", run: (cx) => openSave(this, tab, cx) },
      { label: "Ask an agent about it…", run: (cx) => openAsk(this, tab, tab.error ? "error" : "statement", cx) },
    ], cx);
  }

  tabMenu(tab, e, cx) {
    openMenu(this, e, [
      { label: "Close", hint: "⌘W", run: (cx) => this.closeTab(tab.id, cx) },
      { label: "Close the others", disabled: this.tabs.length < 2, run: (cx) => { for (const x of [...this.tabs]) if (x.id !== tab.id) this.closeTab(x.id, cx); } },
      { label: "Close all", run: (cx) => { for (const x of [...this.tabs]) this.closeTab(x.id, cx); } },
      tab.conn && "-",
      tab.conn && { label: `New ${tab.conn} console`, run: (cx) => this.newConsole(tab.conn, cx) },
    ], cx);
  }

  /** A newer dbx is out: one click updates it (the panel and the engine follow). */
  updateBar(t) {
    const v = this.conns.update;
    const link = (id, label, click) => div().id(id).px(6).py(2).rounded(5).text_size(12).cursor_pointer()
      .hover((s) => s.bg(t.hover)).on_click((_e, cx) => click(cx)).child(label);
    return h_flex().mx(8).mb(6).px(10).py(5).gap(6).items_center().rounded(7).bg(tint(t.accent, "1a")).text_size(12)
      .child(div().flex_1().min_w(0).truncate().child(`dbx ${v} is out`))
      .child(link("dbx-update", "Update", () => run("terminal.open", { argv: ["dbx", "update"], folder: this.folder(), place: "right", name: "dbx update" })))
      .child(link("dbx-update-later", "Later", (cx) => { this.dismissedUpdate = v; cx.notify(); }));
  }

  menuBar(t) {
    const name = this.menu, c = this.conn(name);
    const link = (id, label, color, click) => div().id(id).px(6).py(2).rounded(5).text_size(12).text_color(color).cursor_pointer()
      .hover((s) => s.bg(t.hover)).on_click((_e, cx) => click(cx)).child(label);
    const head = h_flex().gap(6).items_center()
      .child(div().flex_1().min_w(0).truncate().font_semibold().child(name))
      .when(Boolean(c && (c.detected || !["this repo", "global"].includes(c.source))), (e) => e.child(div().text_size(11).text_color(t.dim).child(c.detected ? "docker" : `from ${c.source}`)))
      .child(link("dbx-menu-close", "✕", t.dim, (cx) => { this.menu = null; cx.notify(); }));
    const actions = h_flex().gap(2).flex_wrap();
    const bar = v_flex().mx(8).mb(6).px(8).py(6).gap(4).rounded(7).bg(t.raised).text_size(12).child(head);
    if (this.confirmRemove) {
      return bar.child(actions.child(div().px(6).text_color(t.ansi1).child("Remove it?"))
        .child(link("dbx-menu-yes", "Remove", t.ansi1, (cx) => removeConnection(this, name, cx)))
        .child(link("dbx-menu-no", "Keep", t.muted, (cx) => { this.confirmRemove = false; cx.notify(); })));
    }
    const own = c && !c.detected && ["this repo", "global"].includes(c.source);
    return bar.child(actions
      .child(this.isConnected(name)
        ? link("dbx-menu-disconnect", "Disconnect", t.foreground, (cx) => this.disconnect(name, cx))
        : link("dbx-menu-connect", "Connect", t.foreground, (cx) => { this.menu = null; this.connect(name, cx); }))
      .child(link("dbx-menu-console", "Console", t.foreground, (cx) => { this.menu = null; this.newConsole(name, cx); }))
      .when(Boolean(c?.detected), (e) => e
        .child(link("dbx-menu-save", "Save…", t.foreground, (cx) => { this.menu = null; openForm(this, name, cx, { fromDetected: true }); })))
      .when(Boolean(own), (e) => e
        .child(link("dbx-menu-edit", "Edit", t.foreground, (cx) => { this.menu = null; openForm(this, name, cx); }))
        .child(link("dbx-menu-rm", "Remove", t.ansi1, (cx) => { this.confirmRemove = true; cx.notify(); })))
      .when(!c?.detected, (e) => e.child(link("dbx-menu-file", "File", t.muted, () => editFile(this, c?.source === "global")))));
  }

  explorer(t) {
    const rows = this.treeRows();
    const empty = this.error ?? (this.conns ? "No connections for this repo yet: `dbx add <name> <url>`, `dbx edit`, or `dbx include <other clone>`." : "Loading…");
    return v_flex().size_full().min_h(0).bg(t.background)
      .child(h_flex().h(36).flex_none().mx(8).my(6).px(10).items_center().rounded(8).border_1().border_color(t.border).key_context("DbxFilter")
        .child(Input.new(this.filterInput).flex_1().h_full().text_size(12).text_color(t.foreground)))
      .when(Boolean(this.conns?.update) && this.dismissedUpdate !== this.conns?.update, (e) => e.child(this.updateBar(t)))
      .when(Boolean(this.menu), (e) => e.child(this.menuBar(t)))
      .when(!rows.length, (e) => e.child(v_flex().p(12).gap(10).text_size(12).text_color(this.error ? t.ansi1 : t.dim).child(empty)
        .when(Boolean(this.conns) && !this.error, (e) => e.child(h_flex().child(this.button("dbx-first-add", "+ Add a connection", (cx) => openForm(this, null, cx), { t, primary: true }))))))
      .when(rows.length > 0, (e) => e.child(div().id("dbx-tree-wrap").flex_1().min_h(0).relative().px(4).text_size(12.5)
        .track_focus(this.treeHandle).on_key_down((e, cx) => this.treeKey(e, cx))
        .child(v_virtual_list("dbx-tree", rows.length, TREE_ROW, (i) => rows[i].key,
          (range) => rows.slice(range.start, range.end).map((r) => this.treeRow(r, t)))
          .size_full().track_scroll(this.treeScroll).on_item_click((key, cx) => this.clickTree(key, cx))
          .on_item_secondary_click((key, e, cx) => {
            const [type, name] = key.split("\u0001");
            if (!["c", "s", "t"].includes(type)) return;
            this.treeSel = key; this.current = name; this.zone = "tree";
            openMenu(this, e, this.treeItems(key, cx), cx);
          }))
        .child(Scrollbar.vertical("dbx-tree").absolute().inset_0())));
  }

  // ── tabs and bodies ───────────────────────────────────────────────────

  button(id, label, onClick, { t, primary = false, disabled = false, color } = {}) {
    return div().id(id).h(26).px(10).flex().items_center().rounded(6).text_size(12).flex_none()
      .bg(primary && !disabled ? t.solid : t.raised).text_color(disabled ? t.dim : primary ? t.solid_foreground : color ?? t.foreground)
      .when(!disabled, (e) => e.cursor_pointer().hover((s) => s.opacity(0.85)).on_click((_e, cx) => onClick(cx)))
      .child(label);
  }

  tabStrip(t) {
    return h_flex().id("dbx-tabs").h(36).flex_none().px(6).gap(4).items_center().border_b(1).border_color(t.border).overflow_x_scroll()
      .children(this.tabs.map((tab) => {
        const on = tab.id === this.active, c = this.conn(tab.conn);
        return h_flex().id(`dbx-tab-${tab.id}`).h(26).px(8).gap(6).items_center().rounded(6).flex_none().cursor_pointer()
          .bg(on ? t.selected : t.background).hover((s) => s.bg(on ? t.selected : t.hover))
          .on_click((_e, cx) => { this.active = tab.id; this.current = tab.conn ?? this.current; cx.notify(); })
          .on_mouse_down("right", (e, cx) => this.tabMenu(tab, e, cx))
          .child(div().w(6).h(6).rounded(3).bg(tab.kind === "form" || tab.kind === "help" ? t.dim : stageColor(t, c?.stage)))
          .child(div().text_size(12).text_color(on ? t.foreground : t.muted).child(tab.kind === "console" ? `${tab.conn} ›_` : tab.kind === "form" || tab.kind === "help" ? tab.title : `${tab.conn}.${tab.title}`))
          .child(div().id(`dbx-close-${tab.id}`).px(3).rounded(3).text_size(11).text_color(t.dim).hover((s) => s.bg(t.hover).text_color(t.foreground))
            .on_click((_e, cx) => { cx.stop_propagation(); this.closeTab(tab.id, cx); }).child("✕"));
      }))
      .child(div().id("dbx-new-console").h(26).px(8).flex().items_center().rounded(6).text_size(12).text_color(t.muted).cursor_pointer()
        .hover((s) => s.bg(t.hover).text_color(t.foreground)).flex_none()
        .on_click((_e, cx) => this.newConsole(this.current, cx)).child(this.current ? `+ ${this.current} console` : "+ console"));
  }

  errorBox(err, text, t, tab) {
    const where = err.position !== undefined && err.position !== null && text ? lineCol(text, err.position) : null;
    return v_flex().m(8).p(10).gap(4).rounded(8).border_1().border_color(tint(t.ansi1, "88")).bg(tint(t.ansi1, "14")).text_size(12)
      .child(div().text_color(t.ansi1).font_semibold().child(err.message + (where ? `  (line ${where.line}, column ${where.col})` : "")))
      .when(Boolean(err.detail), (e) => e.child(div().text_color(t.foreground).child(err.detail)))
      .when(Boolean(err.hint), (e) => e.child(div().text_color(t.muted).child(`Hint: ${err.hint}`)))
      .when(Boolean(err.closed), (e) => e.child(div().text_color(t.muted).child("The connection was lost; the next run reconnects.")))
      .when(Boolean(tab), (e) => e.child(h_flex().pt(4).child(this.button(`dbx-ask-error-${tab.id}`, "Ask an agent to fix it",
        (cx) => openAsk(this, tab, "error", cx), { t }))));
  }

  results(tab, t, text) {
    const font = this.font();
    const sets = tab.results ?? [];
    const set = sets[tab.picked ?? 0];
    const body = v_flex().flex_1().min_h(0);
    if (tab.error) return body.child(this.errorBox(tab.error, text, t, tab));
    if (!set) return body.child(div().p(12).text_size(12).text_color(t.dim).child(tab.running ? "Running…" : tab.kind === "console" ? "⌘/Ctrl+Enter runs the statement under the cursor, ⇧ runs everything." : ""));
    const rowsShown = set.rows.length, offset = tab.offset ?? 0;
    const status = set.columns.length
      ? `${offset ? `${offset + 1}–${offset + rowsShown}` : rowsShown} row${rowsShown === 1 ? "" : "s"}${set.more ? "+" : ""}`
      : `${set.affected ?? 0} row${set.affected === 1 ? "" : "s"} affected`;
    return body
      .when(sets.length > 1, (e) => e.child(h_flex().h(30).flex_none().px(8).gap(4).items_center().border_b(1).border_color(t.border)
        .children(sets.map((s, i) => div().id(`dbx-set-${i}`).px(8).py(2).rounded(5).text_size(11).cursor_pointer()
          .bg(i === tab.picked ? t.selected : t.background).hover((x) => x.bg(t.hover))
          .on_click((_e, cx) => { tab.picked = i; tab.sel = null; tab.sort = null; cx.notify(); })
          .child(s.columns.length ? `Result ${i + 1}` : `Statement ${i + 1}`)))))
      .when(set.columns.length > 0, (e) => e.child(this.gridWithInspector(tab, set, t, font, offset)))
      .child(this.statusBar(tab, set, t, offset, rowsShown, status));
  }

  /** The grid, and the selected row's inspector beside it (Space hides it). */
  gridWithInspector(tab, set, t, font, offset) {
    const g = grid(`dbx-grid-${tab.id}`, set, {
        t, font, offset, sel: tab.sel, sort: tab.sort, scroll: tab.scroll, zoom: settings().zoom ?? 1,
        order: tab.kind === "console" || tab.step ? displayOrder(set, tab.sort) : undefined,
        tip: (row, col) => cellSummary(tab, row, col),
        pending: tab.pending, deleted: tab.deleted, pk: tab.info?.primary_key,
        onSort: (col, cx) => this.sortBy(tab, col, cx),
        onPick: (row, col, double, cx) => this.pick(tab, row, col, double, cx),
        onMenu: (row, col, e, cx) => this.cellMenu(tab, row, col, e, cx),
        onHeaderMenu: (col, e, cx) => this.headerMenu(tab, col, e, cx),
        focus: this.gridHandle, onKey: (e, cx) => this.gridKey(tab, e, cx),
      });
    const showing = tab.inspector !== false && Boolean(set.rows[tab.sel?.row]);
    const area = div().flex_1().min_h(0).flex().flex_col()
      .when(Boolean(tab.trail?.length), (e) => e.child(trailBar(this, tab, t)))
      .when(Boolean(tab.navigating), (e) => e.child(div().px(12).py(2).text_size(11).text_color(t.dim).child("Following…")));
    if (!showing) return area.child(g);
    return area.child(h_resizable(`dbx-insp-split-${tab.id}`).flex_1().min_h(0)
      .child(resizable_panel().child(v_flex().size_full().min_w(0).child(g)))
      .child(resizable_panel().size(330).size_range(220, 900).child(inspector(this, tab, t, font))));
  }

  statusBar(tab, set, t, offset, rowsShown, status) {
    return h_flex().h(32).flex_none().px(10).gap(8).items_center().border_t(1).border_color(t.border).text_size(11.5)
        .child(div().text_color(t.muted).child(status))
        .child(div().text_color(t.dim).child(`${tab.elapsed ?? 0} ms`))
        .when(tab.kind === "table" && Boolean(tab.info) && !tab.step, (e) => e.child(div().truncate().text_color(t.dim)
          .child(blocked(tab) ?? "double-click a cell to edit · Del marks a row")))
        .when(Boolean(tab.step), (e) => e.child(div().truncate().text_color(t.dim).child("followed rows · Esc goes back")))
        .child(div().flex_1())
        .when(set.columns.length > 0 && !tab.step && (tab.paged !== false || tab.kind === "table"), (e) => e
          .child(this.button(`dbx-prev-${tab.id}`, "‹ Prev", (cx) => this.page(tab, -1, cx), { t, disabled: !offset || tab.running }))
          .child(this.button(`dbx-next-${tab.id}`, "Next ›", (cx) => this.page(tab, 1, cx), { t, disabled: !set.more || tab.running })))
        .when(set.columns.length > 0, (e) => e
          .child(this.button(`dbx-csv-${tab.id}`, "CSV", (cx) => this.copy(csv(set.columns, set.rows), `${rowsShown} rows as CSV`, cx), { t }))
          .child(this.button(`dbx-json-${tab.id}`, "JSON", (cx) => this.copy(json(set.columns, set.rows), `${rowsShown} rows as JSON`, cx), { t }))
          .child(this.button(`dbx-ask-rows-${tab.id}`, "Ask agent", (cx) => openAsk(this, tab, tab.kind === "table" && !tab.step ? "table" : "result", cx), { t })));
  }

  /** The rows pointing at the selected row, as a menu (`r`). */
  referrersMenu(tab, cx) {
    const set = tab.results?.[tab.picked ?? 0], list = set?._referrers?.get(tab.sel?.row);
    const o = this.origin ?? { x: 0, y: 0, width: 800, height: 600 };
    const at = { position: { x: o.x + o.width / 2, y: o.y + o.height / 3 } };
    if (!Array.isArray(list)) { this.flash(list === null ? "Still looking…" : "Select a row first", cx); return; }
    const items = list.filter((x) => x.count).map((x) => ({ label: `${x.table}.${x.column} · ${x.count}${x.more ? "+" : ""} row${x.count === 1 ? "" : "s"}`,
      run: (cx) => go(this, tab, { ...x, reverse: true }, cx) }));
    if (!items.length) { this.flash("Nothing points at this row", cx); return; }
    openMenu(this, at, items, cx);
  }

  /** A followed step as SQL: at the end of a console's editor, or in a new console. */
  stepToSql(tab, step, cx) {
    const sql = `${step.sql};`;
    if (tab.kind !== "console") { this.newConsole(tab.conn, cx, sql); return; }
    const text = tab.editor.value().replace(/\s*$/, "\n\n");
    tab.editor.set_value(text + sql);
    tab.editor.set_selected_range({ start: text.length, end: text.length + sql.length });
    tab.editor.focus();
    cx.notify();
  }

  consoleBody(tab, t) {
    const s = tab.session, c = this.conn(tab.conn), text = tab.editor?.value() ?? "";
    const ro = s ? s.readonly : c?.readonly, safe = ["local", "dev", "test"].includes(c?.stage);
    const toolbar = h_flex().h(40).flex_none().px(8).gap(6).items_center().border_b(1).border_color(t.border)
      .child(this.button(`dbx-run-${tab.id}`, tab.running ? "Running…" : "▶ Run", (cx) => this.run(tab, false, cx), { t, primary: true, disabled: tab.running || (!s && !tab.disconnected) }))
      .child(this.button(`dbx-runall-${tab.id}`, "Run all", (cx) => this.run(tab, true, cx), { t, disabled: tab.running || (!s && !tab.disconnected) }))
      .child(this.button(`dbx-queries-${tab.id}`, "↺ Queries", (cx) => openFinder(this, tab, cx), { t }))
      .child(this.button(`dbx-ask-${tab.id}`, "Ask agent", (cx) => openAsk(this, tab, tab.error ? "error" : "statement", cx), { t }))
      .when(tab.running, (e) => e.child(this.button(`dbx-cancel-${tab.id}`, "■ Cancel", (cx) => this.cancel(tab, cx), { t, color: t.ansi1 })))
      .child(div().w(1).h(18).bg(t.border).mx(4))
      .child(div().id(`dbx-mode-${tab.id}`).h(26).px(8).flex().items_center().gap(6).rounded(6).text_size(12).cursor_pointer()
        .border_1().border_color(ro || safe ? t.border : t.ansi1).text_color(ro ? t.muted : safe ? t.ansi2 : t.ansi1)
        .hover((x) => x.bg(t.hover)).on_click((_e, cx) => this.toggleWrite(tab, cx))
        .child(ro ? "read-only" : "read-write"))
      .child(div().id(`dbx-limit-${tab.id}`).h(26).px(8).flex().items_center().rounded(6).text_size(12).text_color(t.muted).cursor_pointer()
        .hover((x) => x.bg(t.hover)).on_click((_e, cx) => { tab.limit = LIMITS[(LIMITS.indexOf(tab.limit) + 1) % LIMITS.length]; cx.notify(); })
        .child(`limit ${tab.limit}`))
      .child(div().flex_1())
      .child(div().text_size(11).text_color(t.dim).truncate()
        .child(s ? `${s.user ?? ""}@${s.host}:${s.port}/${s.database ?? ""} · ${s.kind === "clickhouse" ? "ClickHouse" : "pg"} ${s.server_version}` : tab.disconnected ? "disconnected · Run reconnects" : tab.error ? "not connected" : "connecting…"));
    const confirm = tab.confirming && h_flex().h(40).flex_none().px(10).gap(8).items_center().bg(tint(t.ansi1, "14")).border_b(1).border_color(t.border)
      .child(div().text_size(12).text_color(t.ansi1).child(`${tab.conn} is prod. Type its name and Enter to allow writes:`))
      .child(h_flex().h(28).w(180).px(8).items_center().rounded(6).border_1().border_color(t.ansi1).child(Input.new(this.confirmInput).flex_1().h_full().text_size(12)))
      .child(this.button(`dbx-confirm-cancel-${tab.id}`, "Cancel", (cx) => { tab.confirming = false; cx.notify(); }, { t }));
    const n = tab.suggestions.length, first = Math.max(0, Math.min(n - SUGGEST_ROWS, tab.at - Math.floor(SUGGEST_ROWS / 2)));
    const kindColor = (k) => (k === "keyword" ? t.ansi5 : k === "column" ? t.foreground : k === "schema" ? t.ansi6 : t.ansi4);
    const suggestions = n > 0 && v_flex().absolute().top(0).left(0).right(0).border_b(1).border_color(t.border).bg(t.raised).shadow_md().py(3)
      .children(tab.suggestions.slice(first, first + SUGGEST_ROWS).map((it, k) => {
        const i = first + k;
        return h_flex().id(`dbx-sug-${tab.id}-${i}`).h(22).mx(4).px(8).gap(10).items_center().rounded(5).cursor_pointer().text_size(12)
          .bg(i === tab.at ? t.selected : t.raised).hover((x) => x.bg(t.hover))
          .on_click((_e, cx) => this.accept(tab, it, cx))
          .child(div().w(68).flex_none().text_size(10).text_color(t.dim).child(it.kind))
          .child(div().flex_none().font_family(this.font().font_family).text_color(kindColor(it.kind)).child(it.label))
          .child(div().flex_1().min_w(0).truncate().text_size(11).text_color(t.dim).child(it.detail ?? ""));
      }))
      .child(h_flex().h(18).px(12).gap(10).text_size(10.5).text_color(t.dim)
        .child(`${tab.at + 1} / ${n}`).child("↑↓ PgUp PgDn · Tab or Enter takes it · Esc"));
    return v_flex().size_full().min_h(0)
      .child(toolbar)
      .when(Boolean(confirm), (e) => e.child(confirm))
      .when(Boolean(tab.saving), (e) => e.child(saveBar(this, tab, t)))
      .when(Boolean(tab.ask), (e) => e.child(askBar(this, tab, t)))
      .child(v_resizable(`dbx-split-${tab.id}`).flex_1().min_h(0)
        .child(resizable_panel().size(200).size_range(80, 2000).child(v_flex().size_full().min_h(0)
          .child(v_flex().id(`dbx-editor-${tab.id}`).flex_1().min_h(0).key_context(tab.suggestions.length ? "DbxConsole DbxCompleting" : "DbxConsole")
            .on_mouse_down("right", (e, cx) => this.editorMenu(tab, e, cx))
            .child(Editor.new(tab.editor).flex_1().min_h(0)))))
        .child(resizable_panel().child(div().relative().size_full().flex().flex_col()
          .child(this.results(tab, t, text))
          .when(Boolean(suggestions), (e) => e.child(suggestions))
          .when(Boolean(tab.find), (e) => e.child(finderPanel(this, tab, t, this.font()))))));
  }

  structure(tab, t) {
    const info = tab.info;
    if (!info) return div().p(12).text_size(12).text_color(tab.infoError ? t.ansi1 : t.dim).child(tab.infoError ?? "Loading…");
    const section = (title, rows) => v_flex().gap(2).child(div().px(12).pt(12).pb(4).text_size(11).font_semibold().text_color(t.muted).child(title)).children(rows);
    const line = (id, cells) => h_flex().id(id).px(12).py(3).gap(12).text_size(12).hover((s) => s.bg(t.hover)).children(cells);
    const font = this.font();
    return v_flex().id(`dbx-structure-${tab.id}`).size_full().overflow_y_scroll().pb(12)
      .when(Boolean(info.comment), (e) => e.child(div().px(12).pt(10).text_size(12).text_color(t.muted).child(info.comment)))
      .child(section(`Columns · ${info.columns.length}`, info.columns.map((c, i) => line(`dbx-col-${i}`, [
        div().w(200).flex_none().truncate().font_semibold().child(c.name),
        div().w(200).flex_none().truncate().font_family(font.font_family).text_color(t.ansi4).child(c.type + (c.nullable === false ? " not null" : "")),
        div().flex_1().min_w(0).truncate().font_family(font.font_family).text_color(t.dim).child(c.default ? `${c.default_kind ?? "default"} ${c.default}` : ""),
        div().w(220).flex_none().truncate().text_color(t.muted).child(c.comment ?? ""),
      ]))))
      .when(Boolean(info.engine), (e) => e.child(section("Engine and keys", [["engine", info.engine], ...(info.keys ?? []).map((k) => [k.kind, k.definition])].map(([k, v], i) => line(`dbx-key-${i}`, [
        div().w(200).flex_none().text_color(t.muted).child(k),
        div().flex_1().min_w(0).truncate().font_family(font.font_family).text_color(t.foreground).child(v),
      ])))))
      .when(info.indexes.length > 0, (e) => e.child(section(`${info.dialect === "clickhouse" ? "Data skipping indexes" : "Indexes"} · ${info.indexes.length}`, info.indexes.map((x, i) => line(`dbx-idx-${i}`, [
        div().w(200).flex_none().truncate().font_semibold().child(x.name),
        div().flex_1().min_w(0).truncate().font_family(font.font_family).text_color(t.dim).child(x.definition),
      ])))))
      .when(info.constraints.length > 0, (e) => e.child(section(`Constraints · ${info.constraints.length}`, info.constraints.map((x, i) => line(`dbx-con-${i}`, [
        div().w(200).flex_none().truncate().font_semibold().child(x.name),
        div().w(100).flex_none().text_color(t.muted).child(x.kind),
        div().flex_1().min_w(0).truncate().font_family(font.font_family).text_color(t.dim).child(x.definition),
      ])))))
      .when(Boolean(info.view_definition), (e) => e.child(section("Definition", [
        div().px(12).font_family(font.font_family).text_size(12).text_color(t.foreground).child(info.view_definition),
      ])))
      .when(Boolean(info.create), (e) => e.child(section("CREATE", [
        div().px(12).font_family(font.font_family).text_size(12).text_color(t.foreground).child(info.create),
      ])));
  }

  tableBody(tab, t) {
    const info = tab.info;
    const kind = info && (RELKINDS[info.kind] ?? (info.rows === null ? info.kind : `~${compact(info.rows)} rows`));
    const engine = info?.kind === "table" ? info.engine?.split(/[ (]/)[0] : null;
    const facts = info ? [engine, kind, info.bytes === null ? null : bytes(info.bytes)].filter(Boolean).join(" · ") : "";
    const seg = (id, label, on, click) => div().id(id).px(10).h(24).flex().items_center().rounded(5).text_size(12).cursor_pointer()
      .bg(on ? t.selected : t.raised).text_color(on ? t.foreground : t.muted).on_click((_e, cx) => click(cx)).child(label);
    const field = (input, w) => h_flex().h(28).when(w === "fill", (e) => e.flex_1().min_w(120)).when(w !== "fill", (e) => e.w(w).flex_none()).px(8).items_center().rounded(6).border_1().border_color(t.border)
      .font_family(this.font().font_family).child(Input.new(input).flex_1().h_full().text_size(12));
    return v_flex().size_full().min_h(0)
      .child(h_flex().h(40).flex_none().px(10).gap(8).items_center().border_b(1).border_color(t.border)
        .child(div().font_semibold().child(`${tab.schema}.${tab.table}`))
        .child(div().text_size(11).text_color(t.dim).child(facts))
        .child(div().flex_1())
        .child(h_flex().gap(2).p(2).rounded(7).bg(t.raised)
          .child(seg(`dbx-data-${tab.id}`, "Data", tab.view === "data", (cx) => { tab.view = "data"; cx.notify(); }))
          .child(seg(`dbx-struct-${tab.id}`, "Structure", tab.view === "structure", (cx) => { tab.view = "structure"; cx.notify(); })))
        .child(this.button(`dbx-reload-${tab.id}`, "↻", (cx) => this.loadTable(tab, cx), { t })))
      .when(tab.view === "data", (e) => e
        .when(Boolean(tab.ask), (e) => e.child(askBar(this, tab, t)))
        .when(!tab.step, (e) => e.child(h_flex().h(40).flex_none().px(10).gap(8).items_center().border_b(1).border_color(t.border)
          .child(div().text_size(11).text_color(t.dim).child("WHERE")).child(field(tab.whereInput, "fill").key_context("DbxWhere"))
          .child(div().text_size(11).text_color(t.dim).child("ORDER BY")).child(field(tab.orderInput, 170).key_context("DbxOrder"))))
        .when(Boolean(tab.confirming), (e) => e.child(h_flex().h(40).flex_none().px(10).gap(8).items_center().bg(tint(t.ansi1, "14")).border_b(1).border_color(t.border)
          .child(div().text_size(12).text_color(t.ansi1).child(`${tab.conn} is prod. Type its name and Enter to allow writes:`))
          .child(h_flex().h(28).w(180).px(8).items_center().rounded(6).border_1().border_color(t.ansi1).child(Input.new(this.confirmInput).flex_1().h_full().text_size(12)))
          .child(this.button(`dbx-tconfirm-cancel-${tab.id}`, "Cancel", (cx) => { tab.confirming = false; cx.notify(); }, { t }))))
        .child(editBars(this, tab, t))
        .child(this.results(tab, t, null)))
      .when(tab.view === "structure", (e) => e.child(this.structure(tab, t)));
  }

  empty(t) {
    const names = this.conns?.connections.map((c) => c.name) ?? [];
    return v_flex().size_full().items_center().justify_center().gap(10).text_color(t.dim).text_size(13)
      .child(svg("mods/dbx/database.svg").size(36).text_color(t.border))
      .child(div().child(names.length ? "Open a table from the tree, or start a console" : "No connections yet"))
      .when(!names.length && Boolean(this.conns), (e) => e.child(this.button("dbx-empty-add", "+ Add a connection", (cx) => openForm(this, null, cx), { t, primary: true })))
      .child(h_flex().px(16).gap(6).flex_wrap().justify_center().children(names.slice(0, 6).map((n) =>
        this.button(`dbx-start-${n}`, `${n} console`, (cx) => this.newConsole(n, cx), { t }))));
  }

  render(cx) {
    const t = theme(), tab = this.activeTab();
    return v_flex().id("dbx-root").relative().size_full().min_h(0).bg(t.background).text_color(t.foreground).text_size(13)
      .track_focus(this.handle)
      .on_action("dbx_accept", (_e, cx) => { const c = this.activeTab(); if (c?.suggestions?.length) this.accept(c, c.suggestions[c.at], cx); })
      .on_action("dbx_dismiss", (_e, cx) => { const c = this.activeTab(); if (c) { c.suggestions = []; cx.notify(); } })
      .on_action("dbx_next", (_e, cx) => this.moveSuggestion(1, cx))
      .on_action("dbx_prev", (_e, cx) => this.moveSuggestion(-1, cx))
      .on_action("dbx_next_page", (_e, cx) => this.moveSuggestion(SUGGEST_ROWS, cx))
      .on_action("dbx_prev_page", (_e, cx) => this.moveSuggestion(-SUGGEST_ROWS, cx))
      .on_action("dbx_complete", (_e, cx) => { const c = this.activeTab(); if (c?.kind === "console") { this.suggest(c, cx, true); cx.notify(); } })
      .on_action("dbx_line_copyLine", (_e, cx) => this.lineCommand("copyLine", cx))
      .on_action("dbx_line_cutLine", (_e, cx) => this.lineCommand("cutLine", cx))
      .on_action("dbx_line_duplicate", (_e, cx) => this.lineCommand("duplicate", cx))
      .on_action("dbx_line_deleteLine", (_e, cx) => this.lineCommand("deleteLine", cx))
      .on_action("dbx_line_moveUp", (_e, cx) => this.lineCommand("moveLines", cx, -1))
      .on_action("dbx_line_moveDown", (_e, cx) => this.lineCommand("moveLines", cx, 1))
      .on_action("dbx_line_toggleComment", (_e, cx) => this.lineCommand("toggleComment", cx))
      .on_action("dbx_filter_down", (_e, cx) => this.treeFromFilter(false, cx))
      .on_action("dbx_find", (_e, cx) => { const t = this.activeTab(); if (t?.kind === "console") openFinder(this, t, cx); })
      .on_action("dbx_save", (_e, cx) => { const t = this.activeTab(); if (t?.kind === "console") openSave(this, t, cx); })
      .on_action("dbx_find_up", (_e, cx) => finderKey(this, this.activeTab(), "up", cx))
      .on_action("dbx_find_down", (_e, cx) => finderKey(this, this.activeTab(), "down", cx))
      .on_action("dbx_find_tab", (_e, cx) => finderKey(this, this.activeTab(), "tab", cx))
      .on_action("dbx_find_escape", (_e, cx) => finderKey(this, this.activeTab(), "escape", cx))
      .on_action("dbx_find_enter", (_e, cx) => finderKey(this, this.activeTab(), "enter", cx))
      .on_action("dbx_find_run", (_e, cx) => finderKey(this, this.activeTab(), "run", cx))
      .on_action("dbx_save_cancel", (_e, cx) => { const t = this.activeTab(); if (t) { t.saving = null; t.editor?.focus(); cx.notify(); } })
      .on_action("dbx_ask_cancel", (_e, cx) => { const t = this.activeTab(); if (t) closeAsk(this, t, cx); })
      .on_action("dbx_ask_next", (_e, cx) => { const a = this.activeTab()?.ask; if (a) { a.at = (a.at + 1) % a.agents.length; cx.notify(); } })
      .on_action("dbx_fill_where_tab", (_e, cx) => this.fillField("where", "tab", cx))
      .on_action("dbx_fill_where_right", (_e, cx) => this.fillField("where", "right", cx))
      .on_action("dbx_fill_order_tab", (_e, cx) => this.fillField("order", "tab", cx))
      .on_action("dbx_fill_order_right", (_e, cx) => this.fillField("order", "right", cx))
      .on_action("dbx_filter_clear", (_e, cx) => { this.filterInput.set_value(""); this.filter = ""; cx.notify(); })
      .on_mouse_down("right", (e) => {
        if (e.local_position && e.bounds) this.origin = { x: e.position.x - e.local_position.x, y: e.position.y - e.local_position.y, width: e.bounds.width, height: e.bounds.height };
      })
      .on_action("dbx_run", (_e, cx) => this.runActive(false, cx))
      .on_action("dbx_run_all", (_e, cx) => this.runActive(true, cx))
      .on_key_down((e, cx) => {
        const k = e.keystroke, cur = this.activeTab();
        if (menuKey(this, e, cx)) return;
        if (cur?.kind === "console" && cur.suggestions.length && (k === "tab" || k === "ctrl-space")) { this.accept(cur, cur.suggestions[cur.at], cx); }
        else if (cur?.kind === "console" && cur.suggestions.length && k === "escape") { cur.suggestions = []; cx.notify(); }
        else if (k === "cmd-enter" || k === "ctrl-enter") this.runActive(false, cx);
        else if (k === "cmd-shift-enter" || k === "ctrl-shift-enter") this.runActive(true, cx);
        else if ((k === "cmd-w" || k === "ctrl-w") && cur) this.closeTab(cur.id, cx);
        else if (/^(shift-f6|f6|(ctrl|cmd)-(shift-)?[`~])$/.test(k)) this.cycleZone(k.includes("shift") || k.endsWith("~") ? -1 : 1, cx);
        else if (/^(alt-page(up|down)|(ctrl|cmd)-shift-[[\]{}]|(ctrl|cmd)-[{}])$/.test(k)) this.cycleTab(/down|]|}/.test(k) ? 1 : -1, cx);
        else if (k === "f1") this.openHelp(cx);
        else if (cur?.kind === "table" && cur.editing && k === "escape") cancelEdit(cur, cx);

        else return;
        cx.stop_propagation();
      })
      .child(h_resizable("dbx-main").flex_1().min_h(0)
        .child(resizable_panel().size(280).size_range(180, 600).child(v_flex().size_full().border_r(1).border_color(t.border)
          .child(h_flex().h(36).flex_none().px(12).gap(8).items_center().border_b(1).border_color(t.border)
            .child(svg("mods/dbx/database.svg").size(14).text_color(t.muted))
            .child(div().font_semibold().child("Databases"))
            .child(div().flex_1().min_w(0).truncate().text_size(11).text_color(t.dim).child(this.conns?.root ?? ""))
            .child(div().id("dbx-add").px(6).rounded(5).text_color(t.dim).cursor_pointer().hover((s) => s.bg(t.hover).text_color(t.foreground))
              .on_click((_e, cx) => openForm(this, null, cx)).child("+"))
            .when(Boolean(this.current), (e) => e.child(div().id("dbx-edit-current").px(6).rounded(5).text_color(t.dim).cursor_pointer()
              .hover((s) => s.bg(t.hover).text_color(t.foreground)).on_click((_e, cx) => { this.menu = this.current; this.confirmRemove = false; cx.notify(); }).child("⋯")))
            .child(div().id("dbx-help-open").px(6).rounded(5).text_color(t.dim).cursor_pointer().hover((s) => s.bg(t.hover).text_color(t.foreground))
              .on_click((_e, cx) => this.openHelp(cx)).child("?"))
            .child(div().id("dbx-refresh").px(6).rounded(5).text_color(t.dim).cursor_pointer().hover((s) => s.bg(t.hover).text_color(t.foreground))
              .on_click((_e, cx) => { this.load(cx); for (const n of Object.keys(this.sources)) if (this.sources[n].open) this.refreshCatalog(n, cx); }).child("↻")))
          .child(this.explorer(t))))
        .child(resizable_panel().child(v_flex().size_full().min_h(0)
          .child(this.tabStrip(t))
          .child(div().flex_1().min_h(0)
            .when(!tab, (e) => e.child(this.empty(t)))
            .when(tab?.kind === "console", (e) => e.child(this.consoleBody(tab, t)))
            .when(tab?.kind === "table", (e) => e.child(this.tableBody(tab, t)))
            .when(tab?.kind === "form", (e) => e.child(formBody(this, tab, t)))
            .when(tab?.kind === "help", (e) => e.child(helpBody(t, this.font())))))))
      .when(Boolean(this.ctx), (e) => e.child(menuLayer(this, t, settings().zoom ?? 1)))
      .when(Boolean(this.toast), (e) => e.child(div().absolute().bottom(14).right(14).px(12).py(6).rounded(8).bg(t.popover).border_1().border_color(t.border).text_size(12).child(this.toast)));
  }
}
