// Editing a table view's rows: changes stay pending (drawn over the grid) until submitted as one
// transaction; the SQL can be previewed first. Needs a table with a primary key.
import { div } from "gpui-kit";
import { v_flex, h_flex, Input } from "gpui-base";
import { changeSql, quoteIdent } from "./sql.js";
import { tint } from "./grid.js";

const CELL_MAX = 4000;

const rel = (tab) => `${quoteIdent(tab.schema)}.${quoteIdent(tab.table)}`;
const set = (tab) => tab.results?.[0];

/** Why this table view can't be edited, or null. */
export function blocked(tab) {
  if (tab.step) return "these are followed rows: Esc goes back to the table";
  if (!tab.info) return "loading the table's structure…";
  if (tab.info.dialect === "clickhouse") return "ClickHouse changes rows with asynchronous mutations (ALTER TABLE … UPDATE), so they aren't edited here";
  if (tab.info.kind !== "r" && tab.info.kind !== "p") return "views can't be edited here";
  if (!tab.info.primary_key?.length) return "this table has no primary key, so rows can't be told apart";
  return null;
}

export function changes(tab) {
  return (tab.pending?.size ?? 0) + (tab.deleted?.size ?? 0);
}

export function startEdit(view, tab, cx) {
  const s = set(tab), sel = tab.sel;
  if (!s || !sel || sel.col === null) return;
  const why = blocked(tab);
  if (why) { view.flash(`Can't edit: ${why}`, cx); return; }
  const key = `${sel.row}:${sel.col}`, loaded = s.rows[sel.row][sel.col];
  if (loaded !== null && loaded.length > CELL_MAX) { view.flash("This value is too long to edit here", cx); return; }
  const v = tab.pending.has(key) ? tab.pending.get(key) : loaded;
  tab.editing = { ...sel };
  tab.editInput.set_value(v ?? "");
  tab.editNull = v === null;
  tab.editInput.focus();
  cx.notify();
}

/** Keeps the edited value pending; `value` null sets NULL. Going back to what was loaded drops it. */
export function commitEdit(tab, value, cx) {
  const e = tab.editing, s = set(tab);
  if (!e || !s) return;
  const key = `${e.row}:${e.col}`;
  if (value === s.rows[e.row][e.col]) tab.pending.delete(key);
  else tab.pending.set(key, value);
  tab.editing = null;
  tab.applyError = null;
  cx.notify();
}

export function cancelEdit(tab, cx) {
  tab.editing = null;
  cx.notify();
}

export function toggleDelete(view, tab, cx) {
  const why = blocked(tab);
  if (why) { view.flash(`Can't delete: ${why}`, cx); return; }
  const r = tab.sel?.row;
  if (r === undefined || r === null) return;
  tab.deleted.has(r) ? tab.deleted.delete(r) : tab.deleted.add(r);
  tab.applyError = null;
  cx.notify();
}

export function revert(tab, cx) {
  tab.pending.clear();
  tab.deleted.clear();
  tab.editing = null;
  tab.preview = false;
  tab.applyError = null;
  cx.notify();
}

function statements(tab) {
  return changeSql(rel(tab), set(tab), tab.info.primary_key, tab.pending, tab.deleted);
}

function submit(view, tab, cx) {
  const list = statements(tab);
  const src = view.source(tab.conn);
  tab.applying = true;
  tab.applyError = null;
  cx.spawn(async (cx) => {
    try {
      const out = await view.withSession(src, tab.conn, (id) => view.api("POST", `/sessions/${id}/apply`, { statements: list.map((x) => x.sql) }));
      if (out.error) {
        tab.applyError = { ...out.error, row: list[out.error.index]?.row, sql: list[out.error.index]?.sql };
      } else {
        view.flash(`Applied ${out.applied} change${out.applied === 1 ? "" : "s"}`, cx);
        revert(tab, cx);
        view.loadTable(tab, cx);
      }
    } catch (e) { tab.applyError = { message: String(e.message ?? e) }; }
    finally { tab.applying = false; cx.notify(); }
  });
  cx.notify();
}

/** The bar above a table view's grid: the cell being edited, or the pending changes. */
export function editBars(view, tab, t) {
  const s = set(tab), font = view.font();
  const out = v_flex().flex_none();
  if (tab.editing && s) {
    const col = s.columns[tab.editing.col];
    out.child(h_flex().h(40).px(10).gap(8).items_center().border_b(1).border_color(t.border).bg(t.raised)
      .child(div().text_size(12).font_semibold().child(col.name))
      .child(div().text_size(11).text_color(t.dim).child(col.type ?? ""))
      .child(h_flex().h(28).flex_1().min_w(0).px(8).items_center().rounded(6).border_1().border_color(t.accent).bg(t.background)
        .font_family(font.font_family).child(Input.new(tab.editInput).flex_1().h_full().text_size(12.5)))
      .child(view.button(`dbx-edit-null-${tab.id}`, "NULL", (cx) => commitEdit(tab, null, cx), { t }))
      .child(view.button(`dbx-edit-ok-${tab.id}`, "Set", (cx) => commitEdit(tab, tab.editInput.value(), cx), { t, primary: true }))
      .child(view.button(`dbx-edit-cancel-${tab.id}`, "Cancel", (cx) => cancelEdit(tab, cx), { t })));
  }
  const n = changes(tab);
  if (!n) return out;
  const holder = view.source(tab.conn), ro = holder.session?.readonly;
  let preview = [];
  try { preview = statements(tab); } catch (e) { preview = [{ sql: `-- ${e.message}` }]; }
  out.child(h_flex().h(40).px(10).gap(8).items_center().border_b(1).border_color(t.border).bg(tint(t.ansi3, "1a"))
    .child(div().text_size(12).font_semibold().child(`${n} pending change${n === 1 ? "" : "s"}`))
    .when(Boolean(ro), (e) => e.child(div().text_size(11.5).text_color(t.ansi1).child("read-only connection")))
    .child(div().flex_1())
    .child(view.button(`dbx-preview-${tab.id}`, tab.preview ? "Hide SQL" : "Preview SQL", (cx) => { tab.preview = !tab.preview; cx.notify(); }, { t }))
    .child(view.button(`dbx-revert-${tab.id}`, "Revert", (cx) => revert(tab, cx), { t }))
    .when(Boolean(ro), (e) => e.child(view.button(`dbx-allow-${tab.id}`, "Allow writes", (cx) => view.toggleWrite(tab, cx), { t, color: t.ansi1 })))
    .child(view.button(`dbx-submit-${tab.id}`, tab.applying ? "Applying…" : "Submit", (cx) => submit(view, tab, cx), { t, primary: true, disabled: Boolean(ro) || tab.applying })));
  if (tab.preview) {
    out.child(v_flex().id(`dbx-sql-${tab.id}`).max_h(160).overflow_y_scroll().px(12).py(8).gap(2).border_b(1).border_color(t.border)
      .font_family(font.font_family).text_size(12)
      .children(preview.map((x, i) => div().id(`dbx-sql-${tab.id}-${i}`).text_color(tab.applyError?.index === i ? t.ansi1 : t.foreground).child(`${x.sql};`))));
  }
  if (tab.applyError) {
    out.child(div().mx(10).my(6).px(10).py(6).rounded(7).border_1().border_color(t.ansi1).text_size(12).text_color(t.ansi1)
      .child(`${tab.applyError.row !== undefined ? `Row ${(tab.offset ?? 0) + tab.applyError.row + 1}: ` : ""}${tab.applyError.message}`));
  }
  return out;
}
