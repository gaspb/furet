// Following rows, like pages: a foreign key leads to the row it points at, and a row leads to the
// rows pointing at it. Each step replaces the tab's results and goes on its trail (Esc goes back),
// in consoles and table views alike. Postgres only: result columns carry `refs` (the foreign key)
// and `origin` (the table column they copy).
import { div } from "gpui-kit";
import { h_flex } from "gpui-base";
import { literal, quoteIdent } from "./sql.js";
import { changes } from "./edit.js";

const SAVED = ["results", "sel", "sort", "offset", "picked", "paged", "elapsed", "step"];
const snapshot = (tab) => Object.fromEntries(SAVED.map((k) => [k, tab[k]]));

/** A new query or a reload starts the trail over. */
export function resetTrail(tab) {
  tab.trail = [];
  tab.step = null;
}

export const stepSql = (s) =>
  `select * from ${quoteIdent(s.schema)}.${quoteIdent(s.table)} where ${quoteIdent(s.column)} = ${literal(s.value)}`;

/** "customers #8" for a row found by its id, "orders · customer_id = 8" for rows found by a column. */
export const stepLabel = (s) => (s.column === "id" ? `${s.table} #${s.value}` : `${s.table} · ${s.column} = ${s.value}`);

/** Shows the rows of `target.table` whose `target.column` is `target.value`: one step on. */
export function go(view, tab, target, cx) {
  if (target.value === null || target.value === undefined) { view.flash("null points at nothing", cx); return; }
  if (tab.kind === "table" && changes(tab)) { view.flash("Submit or revert the pending changes first", cx); return; }
  tab.navigating = true;
  cx.spawn(async (cx) => {
    try {
      const set = await view.withSession(view.holder(tab), tab.conn, (id) =>
        view.api("POST", `/sessions/${id}/lookup`, { schema: target.schema, table: target.table, column: target.column, value: target.value, limit: 500 }));
      (tab.trail ??= []).push(snapshot(tab));
      Object.assign(tab, { results: [set], picked: 0, sort: null, offset: 0, paged: false, error: null,
        sel: set.rows.length ? { row: 0, col: Math.max(0, set.columns.findIndex((c) => c.name === target.column)) } : null,
        step: { ...target, label: stepLabel(target), sql: stepSql(target), count: set.rows.length, more: set.more } });
      prefetch(view, tab, cx);
      selected(view, tab, cx);
    } catch (e) { view.flash(String(e.message ?? e), cx); }
    finally { tab.navigating = false; cx.notify(); }
  });
  cx.notify();
}

/** Back one step, or to step `to` of the trail. */
export function back(tab, cx, to) {
  if (!tab.trail?.length) return false;
  const i = to ?? tab.trail.length - 1;
  Object.assign(tab, tab.trail[i]);
  tab.trail = tab.trail.slice(0, i);
  cx.notify();
  return true;
}

const currentSet = (tab) => tab.results?.[tab.picked ?? 0];

/** Where the selected cell points, when its column is a foreign key. */
export function cellTarget(tab, row = tab.sel?.row, col = tab.sel?.col) {
  const set = currentSet(tab), c = col === null || col === undefined ? null : set?.columns[col];
  if (!c?.refs || !set.rows[row]) return null;
  const key = `${row}:${col}`;
  return { ...c.refs, value: tab.pending?.has(key) ? tab.pending.get(key) : set.rows[row][col] };
}

/** The tables a row's columns come from, each with the row's values by column name. */
function rowTables(set, row) {
  const out = new Map();
  set.columns.forEach((c, i) => {
    if (!c.origin) return;
    const key = `${c.origin.schema}.${c.origin.table}`;
    if (!out.has(key)) out.set(key, { schema: c.origin.schema, table: c.origin.table, row: {} });
    out.get(key).row[c.origin.column] = set.rows[row][i];
  });
  return [...out.values()];
}

/** After the selected row changes: who points at it (for the inspector), once per row. */
export function selected(view, tab, cx) {
  const set = currentSet(tab), row = tab.sel?.row;
  if (!set || row === undefined || row === null || !set.rows[row]) return;
  set._referrers ??= new Map();
  if (set._referrers.has(row)) return;
  const tables = rowTables(set, row);
  if (!tables.length) { set._referrers.set(row, []); return; }
  set._referrers.set(row, null);
  cx.spawn(async (cx) => {
    const found = [];
    try {
      for (const t of tables) {
        const out = await view.withSession(view.holder(tab), tab.conn, (id) => view.api("POST", `/sessions/${id}/referrers`, t));
        found.push(...out.referrers);
      }
      set._referrers.set(row, found);
    } catch (e) { set._referrers.set(row, { error: String(e.message ?? e) }); }
    cx.notify();
  });
}

/** For the hover cards and the inspector: every row a page's foreign keys point at, one query per column. */
export function prefetch(view, tab, cx) {
  const set = currentSet(tab);
  if (!set?.rows.length) return;
  set._refRows ??= new Map();
  set.columns.forEach((c, i) => {
    if (!c.refs || set._refRows.has(i)) return;
    const values = [...new Set(set.rows.map((r) => r[i]).filter((v) => v !== null))];
    if (!values.length) return;
    set._refRows.set(i, null);
    cx.spawn(async (cx) => {
      try {
        const got = await view.withSession(view.holder(tab), tab.conn, (id) =>
          view.api("POST", `/sessions/${id}/lookup`, { ...c.refs, values }));
        const at = got.columns.findIndex((x) => x.name === c.refs.column);
        set._refRows.set(i, { columns: got.columns, rows: new Map(got.rows.map((r) => [r[at], r])) });
      } catch { set._refRows.delete(i); }
      cx.notify();
    });
  });
}

const LABELS = ["name", "title", "label", "display_name", "full_name", "username", "email", "slug", "code", "number", "reference"];
const DETAILS = ["status", "state", "plan", "type", "kind", "currency", "email"];

/** A referenced row in a few words: its name-like field, then a status-like one. */
export function summarize(columns, row) {
  const pick = (names, skip) => names.map((n) => columns.findIndex((c) => c.name === n)).find((i) => i >= 0 && i !== skip && row[i] !== null);
  const text = columns.findIndex((c, i) => /text|char|String/.test(c.type ?? "") && row[i] !== null && row[i].length < 60);
  const main = pick(LABELS) ?? (text >= 0 ? text : undefined);
  const detail = pick(DETAILS, main);
  return [main, detail].filter((i) => i !== undefined).map((i) => row[i].slice(0, 60)).join(" · ");
}

/** What a foreign key cell points at, in a few words, once the page's lookups are in. */
export function cellSummary(tab, row, col) {
  const set = currentSet(tab), got = set?._refRows?.get(col);
  const v = set?.rows[row]?.[col];
  if (!got || v === null || v === undefined) return null;
  const r = got.rows.get(v);
  return r ? summarize(got.columns, r) || `${set.columns[col].refs.table} #${v}` : `no ${set.columns[col].refs.table} row ${v}`;
}

/** The trail above the results: back, where you came from, where you are, and the way out. */
export function trailBar(view, tab, t) {
  const crumb = (id, text, on, click) => div().id(id).px(6).py(1).rounded(5).text_size(12).cursor_pointer().flex_none()
    .text_color(on ? t.foreground : t.accent).when(on, (e) => e.font_semibold()).hover((s) => s.bg(t.hover))
    .when(Boolean(click), (e) => e.on_click((_e, cx) => click(cx))).child(text);
  const base = tab.kind === "table" ? tab.table : "query";
  const steps = [...tab.trail.slice(1).map((s) => s.step), tab.step];
  const step = tab.step;
  return h_flex().id(`dbx-trail-${tab.id}`).h(32).flex_none().px(6).gap(2).items_center().border_b(1).border_color(t.border)
    .bg(t.raised).overflow_x_scroll()
    .child(crumb(`dbx-trail-back-${tab.id}`, "← Back", false, (cx) => back(tab, cx)))
    .child(div().w(1).h(16).bg(t.border).mx(4).flex_none())
    .child(crumb(`dbx-trail-0-${tab.id}`, base, false, (cx) => back(tab, cx, 0)))
    .children(steps.map((s, i) => h_flex().gap(2).items_center().flex_none()
      .child(div().text_size(11).text_color(t.dim).child(s.reverse ? "‹" : "›"))
      .child(crumb(`dbx-trail-${i + 1}-${tab.id}`, s.label, i === steps.length - 1, i === steps.length - 1 ? null : (cx) => back(tab, cx, i + 1)))))
    .child(div().flex_1().min_w(8))
    .child(crumb(`dbx-trail-table-${tab.id}`, "Open as table", false, (cx) =>
      view.openTable(tab.conn, step.schema, step.table, "table", cx, `${quoteIdent(step.column)} = ${literal(step.value)}`)))
    .child(crumb(`dbx-trail-sql-${tab.id}`, "As SQL", false, (cx) => view.stepToSql(tab, step, cx)));
}
