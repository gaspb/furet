// Copying a row and what it needs to a local, dev or test connection (stg → local, to reproduce a
// bug): the plan shows first (which tables, how many rows), personal data is masked unless you
// say otherwise, and the rows pointing at it come along when you ask.
import { div } from "gpui-kit";
import { h_flex } from "gpui-base";
import { call } from "./api.js";

const WRITABLE = ["local", "dev", "test"];

/** Where copies can go: this repo's Postgres connections on a writable stage, other than this one. */
export function targets(view, tab) {
  return (view.conns?.connections ?? []).filter((c) => c.name !== tab.conn && (c.kind ?? "postgres") === "postgres"
    && WRITABLE.includes(c.stage) && !c.unreachable);
}

/** The row to copy, by its key: the table's primary key, else an `id`, else its first column. */
function startOf(tab) {
  const set = tab.results?.[tab.picked ?? 0], row = tab.sel?.row;
  if (!set?.rows[row]) return null;
  const pk = tab.kind === "table" && !tab.step ? tab.info?.primary_key ?? [] : [];
  const at = (pred) => set.columns.findIndex(pred);
  let i = at((c) => c.origin && pk.includes(c.origin.column));
  if (i < 0) i = at((c) => c.origin?.column === "id");
  if (i < 0) i = at((c) => c.origin);
  if (i < 0 || set.rows[row][i] === null) return null;
  const o = set.columns[i].origin;
  return { schema: o.schema, table: o.table, column: o.column, value: set.rows[row][i] };
}

export function openCopy(view, tab, cx) {
  const start = startOf(tab), to = targets(view, tab);
  if (!start) { view.flash("Copying needs a row from a table (with a key)", cx); return; }
  if (!to.length) { view.flash("No local, dev or test Postgres connection to copy to: add one with +", cx); return; }
  tab.copy = { start, to: to.map((c) => c.name), at: 0, children: false, mask: true, plan: null, busy: false };
  plan(view, tab, cx);
  cx.notify();
}

const request = (view, tab, dry) => ({ cwd: view.folder(), from: tab.conn, to: tab.copy.to[tab.copy.at], ...tab.copy.start,
  children: tab.copy.children, mask: tab.copy.mask, dry });

function plan(view, tab, cx) {
  const c = tab.copy;
  c.plan = null; c.error = null;
  cx.spawn(async (cx) => {
    try { if (tab.copy === c) c.plan = (await call("POST", "/copy", request(view, tab, true))).plan; }
    catch (e) { c.error = String(e.message ?? e); }
    cx.notify();
  });
}

function go(view, tab, cx) {
  const c = tab.copy;
  if (c.busy || !c.plan) return;
  c.busy = true;
  cx.spawn(async (cx) => {
    try {
      const out = await call("POST", "/copy", request(view, tab, false));
      tab.copy = null;
      view.flash(`Copied ${out.inserted} row${out.inserted === 1 ? "" : "s"} to ${c.to[c.at]}`
        + (out.existing ? ` (${out.existing} already there)` : "") + (out.masked ? ` · ${out.masked} values masked` : ""), cx);
    } catch (e) { c.busy = false; c.error = String(e.message ?? e); }
    cx.notify();
  });
  cx.notify();
}

export function copyBar(view, tab, t) {
  const c = tab.copy, s = c.start;
  const chip = (id, label, on, click) => div().id(id).px(8).py(2).rounded(5).text_size(12).cursor_pointer().flex_none()
    .text_color(on ? t.foreground : t.dim).bg(on ? t.selected : t.raised).hover((x) => x.bg(t.hover))
    .on_click((_e, cx) => { click(); plan(view, tab, cx); cx.notify(); }).child(`${on ? "✓" : "○"} ${label}`);
  const summary = c.error ? c.error : c.plan ? c.plan.map(([name, n]) => `${name.split(".").pop()} ${n}`).join(" · ") : "planning…";
  return h_flex().id(`furet-copy-${tab.id}`).min_h(40).flex_none().px(10).py(4).gap(8).items_center().flex_wrap()
    .bg(t.raised).border_b(1).border_color(t.border)
    .child(div().text_size(12).text_color(t.muted).child(`Copy ${s.table} ${s.column === "id" ? "#" : `${s.column}=`}${s.value} and what it needs to`))
    .child(div().id(`furet-copy-to-${tab.id}`).px(8).py(2).rounded(5).text_size(12).text_color(t.accent).cursor_pointer().hover((x) => x.bg(t.hover))
      .on_click((_e, cx) => { c.at = (c.at + 1) % c.to.length; plan(view, tab, cx); cx.notify(); })
      .child(c.to.length > 1 ? `${c.to[c.at]} ▾` : c.to[c.at]))
    .child(chip(`furet-copy-children-${tab.id}`, "with the rows pointing at it", c.children, () => { c.children = !c.children; }))
    .child(chip(`furet-copy-mask-${tab.id}`, "mask personal data", c.mask, () => { c.mask = !c.mask; }))
    .child(div().flex_1().min_w(80).truncate().text_size(11.5).text_color(c.error ? t.ansi1 : t.dim).child(summary))
    .child(view.button(`furet-copy-go-${tab.id}`, c.busy ? "Copying…" : "Copy", (cx) => go(view, tab, cx), { t, primary: true, disabled: !c.plan || c.busy }))
    .child(view.button(`furet-copy-cancel-${tab.id}`, "Cancel", (cx) => { tab.copy = null; cx.notify(); }, { t }));
}
