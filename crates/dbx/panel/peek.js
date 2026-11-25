// Following foreign keys from a result: the row a value points at, then the rows that one points
// at, as a trail (orders row 3 › customers #8 › orgs #9). Postgres only; the engine tells which
// columns are foreign keys (`column.refs`).
import { div } from "gpui-kit";
import { v_flex, h_flex } from "gpui-base";
import { record } from "./grid.js";
import { literal, quoteIdent } from "./sql.js";

/** Opens the row `refs` + `value` points at, on top of the trail (or a new trail from the result). */
export function peek(view, tab, refs, value, cx, { fresh = false } = {}) {
  if (value === null || value === undefined) { view.flash("null points at nothing", cx); return; }
  if (fresh || !tab.peek) tab.peek = [];
  const step = { refs, value, set: null, error: null };
  tab.peek.push(step);
  cx.spawn(async (cx) => {
    try {
      step.set = await view.withSession(view.holder(tab), tab.conn, (id) =>
        view.api("POST", `/sessions/${id}/lookup`, { ...refs, value }));
      if (!step.set.rows.length) step.error = `no ${refs.table} row with ${refs.column} = ${value}`;
    } catch (e) { step.error = String(e.message ?? e); }
    cx.notify();
  });
  cx.notify();
}

/** The selected cell's reference, when its column is a foreign key. */
export function cellRef(tab) {
  const set = tab.results?.[tab.picked ?? 0], sel = tab.sel;
  const col = sel && sel.col !== null ? set?.columns[sel.col] : null;
  if (!col?.refs) return null;
  const key = `${sel.row}:${sel.col}`;
  return { refs: col.refs, value: tab.pending?.has(key) ? tab.pending.get(key) : set.rows[sel.row][sel.col] };
}

export function closePeek(tab, cx) {
  tab.peek = null;
  cx.notify();
}

const label = (s) => `${s.refs.table} ${s.refs.column === "id" ? "#" : `${s.refs.column}=`}${s.value}`;

/** The trail and the row at its end, in place of the selected row's fields. */
export function peekPanel(view, tab, t, font) {
  const trail = tab.peek, last = trail[trail.length - 1];
  const crumb = (id, text, on, click) => div().id(id).px(6).py(1).rounded(5).text_size(12).cursor_pointer().flex_none()
    .text_color(on ? t.foreground : t.accent).when(on, (e) => e.font_semibold()).hover((s) => s.bg(t.hover))
    .on_click((_e, cx) => click(cx)).child(text);
  const set = tab.results?.[tab.picked ?? 0];
  const root = set && tab.sel ? `${tab.kind === "table" ? tab.table : "result"} row ${tab.sel.row + 1 + (tab.offset ?? 0)}` : "result";
  const head = h_flex().id(`dbx-peek-trail-${tab.id}`).h(30).flex_none().px(8).gap(2).items_center().border_t(1).border_color(t.border)
    .bg(t.raised).overflow_x_scroll()
    .child(crumb(`dbx-peek-0-${tab.id}`, root, false, (cx) => closePeek(tab, cx)))
    .children(trail.map((s, i) => h_flex().gap(2).items_center().flex_none()
      .child(div().text_size(11).text_color(t.dim).child("›"))
      .child(crumb(`dbx-peek-${i + 1}-${tab.id}`, label(s), i === trail.length - 1, (cx) => { tab.peek = trail.slice(0, i + 1); cx.notify(); }))))
    .child(div().flex_1())
    .child(crumb(`dbx-peek-open-${tab.id}`, "Open the table ↗", false, (cx) => {
      const r = last.refs;
      view.openTable(tab.conn, r.schema, r.table, "table", cx, `${quoteIdent(r.column)} = ${literal(last.value)}`);
    }))
    .child(crumb(`dbx-peek-close-${tab.id}`, "✕", false, (cx) => closePeek(tab, cx)));
  const body = last.error
    ? div().px(12).py(8).text_size(12).text_color(t.ansi1).child(last.error)
    : !last.set
      ? div().px(12).py(8).text_size(12).text_color(t.dim).child("Loading…")
      : record(last.set, 0, { t, font, onCopy: (v, cx) => view.copy(v, "value", cx), onRef: (refs, v, cx) => peek(view, tab, refs, v, cx) });
  return v_flex().flex_none().child(head).child(body);
}
