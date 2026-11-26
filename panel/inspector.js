// The selected row beside the grid: its fields (JSON laid out), where each foreign key points
// (in a few words, a click follows it), and the rows pointing at it ("referenced by").
import { div } from "gpui-kit";
import { v_flex, h_flex } from "gpui-base";
import { kindOf } from "./grid.js";
import { go, summarize } from "./nav.js";
import { openCopy, targets } from "./copy.js";

const COLORS = (t) => ({ number: t.ansi4, time: t.ansi6, json: t.ansi5, uuid: t.muted, text: t.foreground });

function pretty(kind, v) {
  if (kind !== "json") return v;
  try { return JSON.stringify(JSON.parse(v), null, 2); } catch { return v; }
}

export function inspector(view, tab, t, font) {
  const set = tab.results?.[tab.picked ?? 0], row = tab.sel?.row, r = set?.rows[row];
  if (!r) return div();
  const title = tab.step ? `${tab.step.table} · row ${row + 1}` : `${tab.kind === "table" ? tab.table : "row"} ${row + 1 + (tab.offset ?? 0)}`;
  const colors = COLORS(t);
  const section = (text) => div().px(12).pt(10).pb(4).text_size(10.5).font_semibold().text_color(t.dim).child(text);
  const link = (id, text, click) => div().id(id).px(6).py(1).rounded(4).text_size(11.5).text_color(t.accent).cursor_pointer()
    .hover((s) => s.bg(t.hover)).on_click((_e, cx) => { cx.stop_propagation(); click(cx); }).child(text);

  const fields = set.columns.map((c, i) => {
    const key = `${row}:${i}`, v = tab.pending?.has(key) ? tab.pending.get(key) : r[i], kind = kindOf(c);
    const refRow = c.refs && v !== null ? set._refRows?.get(i)?.rows?.get(v) : null;
    const about = c.refs && v !== null
      ? (refRow ? summarize(set._refRows.get(i).columns, refRow) : set._refRows?.get(i) === null ? "…" : "")
      : "";
    return v_flex().id(`furet-insp-f-${i}`).px(12).py(3).gap(1).rounded(4).cursor_pointer().hover((s) => s.bg(t.hover))
      .on_click((_e, cx) => view.copy(v ?? "", c.name, cx))
      .child(h_flex().gap(6).items_center()
        .child(div().flex_1().min_w(0).truncate().text_size(11).text_color(t.dim).child(c.name))
        .when(Boolean(c.type), (e) => e.child(div().text_size(10).text_color(t.dim).child(c.type))))
      .child(div().font_family(font.font_family).text_size(font.font_size - 1)
        .text_color(v === null ? t.dim : c.refs ? t.accent : colors[kind] ?? t.foreground).when(v === null, (e) => e.italic())
        .child(v === null ? "null" : pretty(kind, v).slice(0, 6000)))
      .when(Boolean(c.refs) && v !== null, (e) => e.child(link(`furet-insp-go-${i}`, `→ ${c.refs.table}${about ? ` · ${about}` : ""}${c.refs.guessed ? " · guessed from the name" : ""}`,
        (cx) => go(view, tab, { ...c.refs, value: v }, cx))));
  });

  const referrers = set._referrers?.get(row);
  const referred = referrers === undefined ? [] : referrers === null
    ? [div().px(12).py(2).text_size(11.5).text_color(t.dim).child("Looking…")]
    : referrers.error ? [div().px(12).py(2).text_size(11.5).text_color(t.ansi1).child(referrers.error)]
      : !referrers.length ? [div().px(12).py(2).text_size(11.5).text_color(t.dim).child("Nothing points at this row")]
        : referrers.map((x, i) => h_flex().id(`furet-insp-ref-${i}`).px(12).py(3).gap(8).items_center().rounded(4)
          .when(x.count > 0, (e) => e.cursor_pointer().hover((s) => s.bg(t.hover))
            .on_click((_e, cx) => go(view, tab, { schema: x.schema, table: x.table, column: x.column, value: x.value, reverse: true }, cx)))
          .child(div().flex_1().min_w(0).truncate().text_size(12).text_color(x.count ? t.foreground : t.dim).child(`${x.table}.${x.column}${x.guessed ? " (by name)" : ""}`))
          .child(div().text_size(11.5).text_color(x.count ? t.accent : t.dim)
            .child(x.count ? `${x.count}${x.more ? "+" : ""} row${x.count === 1 ? "" : "s"} →` : "none")));

  return v_flex().id(`furet-insp-${tab.id}`).size_full().overflow_y_scroll().border_l(1).border_color(t.border).pb(10)
    .child(h_flex().h(30).flex_none().px(12).gap(6).items_center().border_b(1).border_color(t.border)
      .child(div().flex_1().min_w(0).truncate().text_size(12).font_semibold().child(title))
      .when(targets(view, tab).length > 0 && set.columns.some((c) => c.origin), (e) => e.child(link(`furet-insp-copy-${tab.id}`, "Copy to…", (cx) => openCopy(view, tab, cx))))
      .child(link(`furet-insp-json-${tab.id}`, "JSON", (cx) =>
        view.copy(JSON.stringify(Object.fromEntries(set.columns.map((c, i) => [c.name, r[i]])), null, 2), "the row as JSON", cx))))
    .children(fields)
    .when(set.columns.some((c) => c.origin), (e) => e.child(section("REFERENCED BY")).children(referred));
}
