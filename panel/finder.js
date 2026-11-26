// Ctrl+R in a console: one search over the repo's saved queries (.furet/queries, shared through git)
// and what ran before on this connection (you, the panel, `furet query`, agents). Enter puts it in
// the editor, Ctrl/Cmd+Enter runs it too. Ctrl/Cmd+S saves the statement under the cursor.
import { div } from "gpui-kit";
import { v_flex, h_flex, Input } from "gpui-base";
import { call } from "./api.js";
import { statementAt } from "./sql.js";

const ROWS = 9;

const ago = (at) => {
  const s = Math.max(0, Date.now() / 1000 - at);
  return s < 60 ? "just now" : s < 3600 ? `${Math.floor(s / 60)} min ago` : s < 86400 ? `${Math.floor(s / 3600)} h ago` : `${Math.floor(s / 86400)} d ago`;
};
const firstLine = (sql) => sql.split("\n").map((l) => l.trim()).find((l) => l && !l.startsWith("--")) ?? sql.trim();

export function openFinder(view, tab, cx) {
  tab.find = { items: null, at: 0, all: false };
  view.findInput.set_value("");
  view.findInput.focus();
  loadFinder(view, tab, cx);
  cx.notify();
}

function loadFinder(view, tab, cx) {
  const cwd = encodeURIComponent(view.folder());
  const name = tab.find.all ? "" : `&name=${encodeURIComponent(tab.conn)}`;
  cx.spawn(async (cx) => {
    try {
      const [q, h] = await Promise.all([
        call("GET", `/queries?cwd=${cwd}`),
        call("GET", `/history?cwd=${cwd}${name}&limit=500`),
      ]);
      const seen = new Set();
      const history = h.history.filter((e) => { const k = e.sql.trim(); if (seen.has(k)) return false; seen.add(k); return true; });
      if (tab.find) tab.find.items = [...q.queries.map((s) => ({ saved: true, ...s })), ...history];
    } catch (e) { if (tab.find) tab.find.error = String(e.message ?? e); }
    cx.notify();
  });
}

export function closeFinder(view, tab, cx) {
  tab.find = null;
  tab.editor?.focus();
  cx.notify();
}

/** What matches the search: every word somewhere in the name or the SQL. */
export function matches(view, tab) {
  const words = view.findInput.value().toLowerCase().split(/\s+/).filter(Boolean);
  return (tab.find?.items ?? []).filter((it) => {
    const hay = `${it.name ?? ""} ${it.sql}`.toLowerCase();
    return words.every((w) => hay.includes(w));
  });
}

export function finderKey(view, tab, key, cx) {
  const list = matches(view, tab), f = tab.find;
  if (!f) return;
  if (key === "down") f.at = Math.min(list.length - 1, f.at + 1);
  else if (key === "up") f.at = Math.max(0, f.at - 1);
  else if (key === "tab") { f.all = !f.all; f.items = null; f.at = 0; loadFinder(view, tab, cx); }
  else if (key === "escape") { closeFinder(view, tab, cx); return; }
  else if (key === "enter" || key === "run") { const it = list[f.at]; if (it) use(view, tab, it.sql, key === "run", cx); return; }
  cx.notify();
}

/** Puts a query in the editor (in place of a blank one, else after it, selected) and maybe runs it. */
function use(view, tab, sql, runIt, cx) {
  const ed = tab.editor, text = ed.value();
  const blank = !text.split("\n").some((l) => l.trim() && !l.trim().startsWith("--"));
  const body = sql.trim().replace(/;?$/, ";");
  const before = blank ? "" : text.replace(/\s*$/, "\n\n");
  ed.set_value(before + body + "\n");
  ed.set_selected_range({ start: before.length, end: before.length + body.length });
  closeFinder(view, tab, cx);
  if (runIt) view.run(tab, false, cx);
}

export function finderPanel(view, tab, t, font) {
  const f = tab.find, list = matches(view, tab);
  f.at = Math.min(f.at, Math.max(0, list.length - 1));
  const first = Math.max(0, Math.min(list.length - ROWS, f.at - Math.floor(ROWS / 2)));
  const items = list.slice(first, first + ROWS).map((it, k) => {
    const i = first + k;
    const meta = it.saved
      ? [it.connection ? `for ${it.connection}` : "saved", ".furet/queries"].join(" · ")
      : [ago(it.at), it.error ? "failed" : it.rows !== undefined && it.rows !== null ? `${it.rows} row${it.rows === 1 ? "" : "s"}` : null,
        `${it.ms} ms`, it.by === "panel" ? null : it.by, f.all ? it.conn : null].filter(Boolean).join(" · ");
    return h_flex().id(`furet-find-${tab.id}-${i}`).h(26).mx(4).px(8).gap(10).items_center().rounded(5).cursor_pointer()
      .bg(i === f.at ? t.selected : t.raised).hover((s) => s.bg(t.hover))
      .on_click((_e, cx) => use(view, tab, it.sql, false, cx))
      .child(div().w(14).flex_none().text_size(11).text_color(it.saved ? t.ansi3 : t.dim).child(it.saved ? "★" : "↺"))
      .when(Boolean(it.saved), (e) => e.child(div().flex_none().text_size(12).font_semibold().child(it.name)))
      .child(div().flex_1().min_w(0).truncate().font_family(font.font_family).text_size(11.5)
        .text_color(it.error ? t.ansi1 : t.foreground).child(firstLine(it.sql)))
      .child(div().flex_none().text_size(10.5).text_color(it.by?.startsWith("agent") ? t.ansi5 : t.dim).child(meta));
  });
  return v_flex().absolute().top(0).left(0).right(0).bg(t.raised).border_b(1).border_color(t.border).shadow_md().py(4)
    .child(h_flex().h(30).mx(8).px(8).gap(8).items_center().rounded(6).border_1().border_color(t.accent).bg(t.background)
      .key_context("FuretFind")
      .child(div().text_size(11).text_color(t.dim).child("↺ ★"))
      .child(Input.new(view.findInput).flex_1().h_full().text_size(12)))
    .when(f.items === null && !f.error, (e) => e.child(div().px(14).py(6).text_size(12).text_color(t.dim).child("Loading…")))
    .when(Boolean(f.error), (e) => e.child(div().px(14).py(6).text_size(12).text_color(t.ansi1).child(f.error)))
    .when(f.items !== null && !list.length, (e) => e.child(div().px(14).py(6).text_size(12).text_color(t.dim)
      .child(f.items?.length ? "Nothing matches" : "Nothing yet: what you run shows here, and Ctrl/Cmd+S saves a query")))
    .children(items)
    .child(h_flex().h(20).px(14).gap(10).text_size(10.5).text_color(t.dim)
      .child(`${list.length ? f.at + 1 : 0} / ${list.length}`)
      .child(`Enter puts it in the editor · ⌘/Ctrl+Enter runs it · Tab: ${f.all ? "this connection only" : "every connection"} · Esc`));
}

// ── saving ───────────────────────────────────────────────────────────

export function openSave(view, tab, cx) {
  const sel = tab.editor.selected_range(), text = tab.editor.value();
  const sql = sel.start !== sel.end ? text.slice(Math.min(sel.start, sel.end), Math.max(sel.start, sel.end)).trim() : statementAt(text, sel.start).text;
  if (!sql) { view.flash("Nothing to save: put the cursor in a statement", cx); return; }
  tab.saving = { sql };
  view.saveInput.set_value("");
  view.saveInput.focus();
  cx.notify();
}

export function submitSave(view, tab, cx) {
  const name = view.saveInput.value().trim().replace(/\s+/g, "-").toLowerCase();
  if (!tab.saving || !name) return;
  const sql = tab.saving.sql;
  cx.spawn(async (cx) => {
    try {
      const out = await call("POST", "/queries", { cwd: view.folder(), name, sql, connection: tab.conn });
      tab.saving = null;
      tab.editor.focus();
      view.flash(`Saved ${out.saved} in .furet/queries (commit it to share)`, cx);
    } catch (e) { view.flash(String(e.message ?? e), cx); }
    cx.notify();
  });
}

export function saveBar(view, tab, t) {
  return h_flex().h(38).flex_none().px(10).gap(8).items_center().bg(t.raised).border_b(1).border_color(t.border).key_context("FuretSave")
    .child(div().text_size(12).text_color(t.muted).child("Save the statement as"))
    .child(h_flex().h(26).w(220).px(8).items_center().rounded(6).border_1().border_color(t.accent).bg(t.background)
      .child(Input.new(view.saveInput).flex_1().h_full().text_size(12)))
    .child(div().text_size(11).text_color(t.dim).child("in .furet/queries · Enter · Esc"))
    .child(div().flex_1())
    .child(div().max_w(300).truncate().text_size(11).text_color(t.dim).child(firstLine(tab.saving.sql)));
}
