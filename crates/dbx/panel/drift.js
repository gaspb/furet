// Schema drift between two Postgres connections, as a tab: what's missing or different on each
// side, and the SQL that would bring the right side in line (shown and copied, never run).
import { div } from "gpui-kit";
import { v_flex, h_flex } from "gpui-base";
import { call } from "./api.js";

export function openDrift(view, left, right, cx) {
  const existing = view.tabs.find((x) => x.kind === "drift" && x.left === left && x.right === right);
  if (existing) { view.active = existing.id; cx.notify(); return; }
  const tab = view.addTab({ kind: "drift", conn: left, left, right, title: `${left} ⇄ ${right}`, data: null, error: null }, cx);
  loadDrift(view, tab, cx);
}

function loadDrift(view, tab, cx) {
  tab.data = null; tab.error = null;
  cx.spawn(async (cx) => {
    try { tab.data = await call("POST", "/drift", { cwd: view.folder(), left: tab.left, right: tab.right }); }
    catch (e) { tab.error = String(e.message ?? e); }
    cx.notify();
  });
  cx.notify();
}

/** The other Postgres connections a schema can be compared with. */
export function comparable(view, name) {
  return (view.conns?.connections ?? []).filter((c) => c.name !== name && (c.kind ?? "postgres") === "postgres" && !c.unreachable);
}

const KIND = (t) => ({ table: t.ansi4, column: t.foreground, index: t.ansi6, constraint: t.ansi5 });

export function driftBody(view, tab, t, font) {
  const d = tab.data;
  const head = h_flex().h(40).flex_none().px(12).gap(8).items_center().border_b(1).border_color(t.border)
    .child(div().font_semibold().child(`${tab.left} ⇄ ${tab.right}`))
    .child(div().text_size(11.5).text_color(t.dim).child(d ? `${d.differences.length} difference${d.differences.length === 1 ? "" : "s"} · ${d.same} the same` : tab.error ? "" : "comparing…"))
    .child(div().flex_1())
    .when(Boolean(d?.differences.length), (e) => e.child(view.button(`dbx-drift-all-${tab.id}`, "Copy all fixes",
      (cx) => view.copy(d.differences.map((x) => x.fix).join("\n"), "every fix", cx), { t })))
    .child(view.button(`dbx-drift-swap-${tab.id}`, "⇄ Swap", (cx) => { [tab.left, tab.right] = [tab.right, tab.left]; tab.title = `${tab.left} ⇄ ${tab.right}`; loadDrift(view, tab, cx); }, { t }))
    .child(view.button(`dbx-drift-reload-${tab.id}`, "↻", (cx) => loadDrift(view, tab, cx), { t }));
  if (tab.error) return v_flex().size_full().child(head).child(div().p(12).text_size(12).text_color(t.ansi1).child(tab.error));
  if (!d) return v_flex().size_full().child(head);
  if (!d.differences.length) return v_flex().size_full().child(head)
    .child(div().p(16).text_size(13).text_color(t.ansi2).child(`Same schema: ${d.same} tables, columns, indexes and constraints match.`));
  const colors = KIND(t);
  const rows = d.differences.map((x, i) => {
    const where = x.left === null ? `only in ${tab.right}` : x.right === null ? `only in ${tab.left}` : "differs";
    const side = (name, v) => v === null ? null : h_flex().gap(10).items_start()
      .child(div().w(110).flex_none().truncate().text_size(11).text_color(t.dim).child(name))
      .child(div().flex_1().min_w(0).font_family(font.font_family).text_size(11.5).child(v));
    return v_flex().id(`dbx-drift-${tab.id}-${i}`).px(14).py(8).gap(4).border_b(1).border_color(t.border)
      .child(h_flex().gap(8).items_center()
        .child(div().w(76).flex_none().text_size(10.5).text_color(colors[x.kind] ?? t.foreground).child(x.kind))
        .child(div().flex_1().min_w(0).truncate().text_size(12.5).font_semibold().child(x.object))
        .child(div().text_size(11).text_color(where === "differs" ? t.ansi3 : t.ansi1).child(where)))
      .child(side(tab.left, x.left) ?? div())
      .child(side(tab.right, x.right) ?? div())
      .child(h_flex().gap(10).items_start().pt(2)
        .child(div().w(110).flex_none().text_size(11).text_color(t.dim).child(`fix ${tab.right}`))
        .child(div().flex_1().min_w(0).font_family(font.font_family).text_size(11.5)
          .text_color(x.fix.startsWith("--") ? t.dim : t.ansi2).child(x.fix))
        .child(div().id(`dbx-drift-copy-${tab.id}-${i}`).px(6).rounded(4).text_size(11).text_color(t.accent).cursor_pointer()
          .hover((s) => s.bg(t.hover)).on_click((_e, cx) => view.copy(x.fix, "the fix", cx)).child("Copy")));
  });
  return v_flex().size_full().min_h(0).child(head)
    .child(v_flex().id(`dbx-drift-list-${tab.id}`).flex_1().min_h(0).overflow_y_scroll().children(rows));
}
