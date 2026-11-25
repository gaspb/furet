// A result set as a grid: header with types (click to sort), virtual rows, horizontal scroll,
// cell selection, and pending edits drawn over the loaded values.
import { div } from "gpui-kit";
import { v_flex, h_flex, Scrollbar, v_virtual_list } from "gpui-base";

// Postgres' type names, and ClickHouse's (UInt64, Nullable(Float64), Decimal(12, 2)…).
const NUMERIC = /^(int[248]|float[48]|numeric|oid|money|smallint|integer|bigint|real|double precision)|^(Nullable\(|LowCardinality\()*(U?Int\d+|Float\d+|BFloat16|Decimal\d*)\b/;
const KINDS = [
  ["bool", /^bool$|^(Nullable\()?Bool\b/],
  ["number", NUMERIC],
  ["time", /^(timestamp|timestamptz|date|time|timetz|interval)\b|^(Nullable\()?(Date|DateTime)/],
  ["json", /^jsonb?$|^(Nullable\()?(JSON|Object)\b/],
  ["uuid", /^uuid$|^(Nullable\()?UUID\b/],
];
/** What a column holds, for colouring its values: number, time, json, uuid, bool, or text. */
export const kindOf = (c) => (KINDS.find(([, re]) => re.test(c.type ?? "")) ?? ["text"])[0];

/** How a value looks in its column: colour, and `t`/`f` spelled out. */
function shown(t, kind, v, isRef) {
  if (v === null) return { text: "null", color: t.dim, italic: true };
  if (kind === "bool") {
    const yes = v === "t" || v === "true" || v === "1";
    return { text: yes ? "true" : "false", color: yes ? t.ansi2 : t.ansi1 };
  }
  if (isRef) return { text: oneLine(v), color: t.accent, underline: true };
  const color = { number: t.ansi4, time: t.ansi6, json: t.ansi5, uuid: t.muted }[kind] ?? t.foreground;
  return { text: oneLine(v), color };
}

export const rowHeight = (size) => Math.ceil(size * 1.75);
const charWidth = (size) => size * 0.62;
const GUTTER = 52;
// The last press in each grid; its click arrives through the list, after a possible re-render.
const presses = new Map();

/** `color` (#rgb, #rrggbb or #rrggbbaa) at `alpha` (two hex digits), replacing any alpha it had. */
export function tint(color, alpha) {
  if (typeof color !== "string" || !color.startsWith("#")) return color;
  const hex = color.length === 4 ? "#" + [...color.slice(1)].map((c) => c + c).join("") : color.slice(0, 7);
  return hex + alpha;
}

/** Column widths from the header and the first rows, kept on the set. */
function widths(set, size) {
  if (set._widths?.size === size) return set._widths.list;
  const list = set.columns.map((c, i) => {
    const typeLine = (c.type ?? "").length + (c.refs ? c.refs.table.length + 5 : 0) + 5;
    let longest = Math.max(c.name.length + 2, typeLine);
    for (const r of set.rows.slice(0, 200)) longest = Math.max(longest, Math.min(60, (r[i] ?? "null").length));
    return Math.round(Math.min(420, Math.max(64, longest * charWidth(size) + 22)));
  });
  set._widths = { size, list };
  return list;
}

const oneLine = (v) => (v.length > 300 ? v.slice(0, 300) + "…" : v).replace(/\s*\n\s*/g, " ⏎ ");

/** Row indices in display order: as loaded, or sorted by a column on the client. */
export function displayOrder(set, sort) {
  const order = set.rows.map((_, i) => i);
  if (!sort || sort.col === null || sort.col === undefined) return order;
  const numeric = NUMERIC.test(set.columns[sort.col]?.type ?? "");
  const key = (i) => set.rows[i][sort.col];
  const cmp = (a, b) => {
    const x = key(a), y = key(b);
    if (x === y) return 0;
    if (x === null) return 1;
    if (y === null) return -1;
    return numeric ? Number(x) - Number(y) : x.localeCompare(y);
  };
  order.sort((a, b) => (sort.dir === "desc" ? -cmp(a, b) : cmp(a, b)));
  return order;
}

/** Which column a click `x` px into the grid's viewport lands on, given its scroll. */
function columnAt(set, size, x, scrollX) {
  let at = x + scrollX - GUTTER;
  const w = widths(set, size);
  for (let i = 0; i < w.length; i++) {
    if (at < w[i]) return i;
    at -= w[i];
  }
  return null;
}

/**
 * `set`: { columns: [{name, type}], rows: [[string|null]] }.
 * `sel`: { row, col } in loaded-row indices. `order`: display order (see displayOrder).
 * `onPick(row, col, double, cx)`: a cell was clicked (rows can't hold handlers, so the grid
 * works the column out from where the press was). `onSort(col, cx)`: a header was clicked.
 * `pending`: Map "row:col" → new value; `deleted`: Set of rows marked for deletion.
 * `onMenu(row, col, event, cx)` / `onHeaderMenu(col, event, cx)`: a right-click on a cell or a header.
 * `onPick(row, col, double, cx, follow)`: `follow` when Ctrl/Cmd was held. `tip(row, col)`: a hover card's text.
 */
export function grid(id, set, opts) {
  const { t, font, sel, offset = 0, sort, onSort, onPick, onMenu, onHeaderMenu, pending, deleted, scroll, zoom = 1, focus, onKey, tip } = opts;
  const order = opts.order ?? set.rows.map((_, i) => i);
  const size = font.font_size;
  const w = widths(set, size), total = GUTTER + w.reduce((a, b) => a + b, 0) + 16;
  const numeric = set.columns.map((c) => NUMERIC.test(c.type ?? ""));
  const kinds = set.columns.map(kindOf);
  const keyCols = new Set(opts.pk ?? []);
  const h = rowHeight(size);
  const header = h_flex().h(h + 8).flex_none().border_b(1).border_color(t.border).bg(t.raised)
    .child(div().w(GUTTER).flex_none())
    .children(set.columns.map((c, i) => {
      const arrow = sort?.col === i ? (sort.dir === "desc" ? " ↓" : " ↑") : "";
      return v_flex().id(`${id}-h${i}`).w(w[i]).flex_none().px(8).justify_center().border_l(1).border_color(t.border)
        .when(Boolean(onSort), (e) => e.cursor_pointer().hover((s) => s.bg(t.hover)).on_click((_e, cx) => onSort(i, cx)))
        .when(Boolean(onHeaderMenu), (e) => e.on_mouse_down("right", (ev, cx) => onHeaderMenu(i, ev, cx)))
        .child(div().truncate().font_semibold().text_color(sort?.col === i ? t.accent : t.foreground)
          .child(c.name + arrow))
        .when(Boolean(c.type), (e) => e.child(div().truncate().text_size(Math.max(9, size - 3)).text_color(t.dim)
          .child([c.type, keyCols.has(c.name) && "pk", c.refs && `→ ${c.refs.table}`].filter(Boolean).join(" · "))));
    }));
  const row = (k) => {
    const i = order[k], r = set.rows[i], onRow = sel?.row === i, gone = deleted?.has(i);
    return h_flex().id(`${id}-r${i}`).h(h).flex_none().items_center()
      .when(gone, (e) => e.bg(tint(t.ansi1, "22")))
      .when(!gone && onRow, (e) => e.bg(tint(t.accent, "18")))
      .when(!gone && !onRow && k % 2 === 1, (e) => e.bg(tint(t.raised, "66")))
      .when(!gone && !onRow, (e) => e.hover((s) => s.bg(t.hover)))
      .child(div().w(GUTTER).flex_none().px(6).text_right().text_color(gone ? t.ansi1 : t.dim).text_size(Math.max(9, size - 2))
        .child(gone ? "✕" : String(offset + i + 1)))
      .children(r.map((loaded, j) => {
        const key = `${i}:${j}`, edited = pending?.has(key), v = edited ? pending.get(key) : loaded, on = onRow && sel?.col === j;
        return div().w(w[j]).flex_none().h_full().px(8).flex().items_center().border_l(1).border_color(tint(t.border, "55"))
          .when(numeric[j], (e) => e.justify_end())
          .when(edited, (e) => e.bg(tint(t.ansi3, "33")))
          .when(on, (e) => e.border_1().border_color(t.accent))
          .when(gone, (e) => e.text_color(t.dim))
          .when(Boolean(set.columns[j].refs && tip), (e) => { const text = tip(i, j); return text ? e.tooltip(text) : e; })
          .child(((look) => div().truncate().text_color(gone ? t.dim : look.color)
            .when(Boolean(look.italic), (e) => e.italic()).when(Boolean(look.underline) && !gone, (e) => e.underline())
            .when(keyCols.has(set.columns[j].name), (e) => e.font_semibold()).child(look.text))(shown(t, kinds[j], v, Boolean(set.columns[j].refs))));
      }));
  };
  return div().id(`${id}-wrap`).flex_1().min_h(0).relative().font_family(font.font_family).text_size(size)
    .when(Boolean(focus), (e) => e.track_focus(focus).on_key_down((ev, cx) => onKey?.(ev, cx)))
    .on_mouse_down("left", (e) => {
      const x = (e.local_position?.x ?? 0) / zoom, y = (e.local_position?.y ?? 0) / zoom;
      presses.set(id, { col: y > h + 8 ? columnAt(set, size, x, scroll?.offset?.().x ?? 0) : null, double: e.click_count >= 2,
        follow: Boolean(e.modifiers?.platform || e.modifiers?.control) });
    })
    .child(div().id(`${id}-x`).size_full().overflow_x_scrollbar().when(Boolean(scroll), (e) => e.track_scroll(scroll))
      .child(v_flex().w_full().min_w(total).h_full()
        .child(header)
        .child(div().flex_1().min_h(0)
          .child(v_virtual_list(`${id}-rows`, order.length, h, (k) => String(order[k]),
            (range) => Array.from({ length: range.end - range.start }, (_, n) => row(range.start + n)))
            .size_full().on_item_click((key, cx) => {
              const p = presses.get(id);
              onPick(Number(key), p?.col ?? null, Boolean(p?.double), cx, Boolean(p?.follow));
            })
            .when(Boolean(onMenu), (e) => e.on_item_secondary_click((key, ev, cx) =>
              onMenu(Number(key), columnAt(set, size, (ev.local_position?.x ?? 0) / zoom, 0), ev, cx)))))))
    .child(Scrollbar.vertical(`${id}-rows`).viewport_from_layout().absolute().inset_0());
}

/** The selected row as name → value, for reading long values and copying one. */
/** `onRef(refs, value, cx)`: a foreign key field's ↗ was clicked. */
export function record(set, index, { t, font, onCopy, onRef, pending }) {
  const r = set.rows[index];
  if (!r) return null;
  return v_flex().id("dbx-record").max_h(180).flex_none().overflow_y_scroll().border_t(1).border_color(t.border)
    .font_family(font.font_family).text_size(font.font_size - 1).py(4)
    .children(set.columns.map((c, i) => {
      const key = `${index}:${i}`, v = pending?.has(key) ? pending.get(key) : r[i];
      return h_flex().id(`dbx-rec-${i}`).px(10).py(1).gap(10).items_start().rounded(4).cursor_pointer()
        .hover((s) => s.bg(t.hover)).on_click((_e, cx) => onCopy(v ?? "", cx))
        .child(div().w(160).flex_none().truncate().text_color(t.dim).child(c.name))
        .child(div().flex_1().min_w(0).text_color(v === null ? t.dim : t.foreground).child(v === null ? "null" : v.slice(0, 4000)))
        .when(Boolean(c.refs && onRef && v !== null), (e) => e.child(div().id(`dbx-rec-ref-${i}`).flex_none().px(6).rounded(4)
          .text_size(11).text_color(t.accent).hover((s) => s.bg(t.selected))
          .on_click((_e, cx) => { cx.stop_propagation(); onRef(c.refs, v, cx); }).child(`↗ ${c.refs.table}`)));
    }));
}
