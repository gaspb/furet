// Right-click menus, drawn by the panel at the pointer. Up/Down/Enter/Escape work while one is
// open; a press anywhere else closes it. Items: { label, hint?, run(cx), danger?, disabled? } or "-".
import { div } from "gpui-kit";
import { v_flex, h_flex } from "gpui-base";
import { keyLabel } from "./help.js";

const ITEM = 26, WIDTH = 230;
const usable = (it) => it !== "-" && !it.disabled;

/** Opens `items` where `e` (a mouse press) happened. */
export function openMenu(view, e, items, cx) {
  const list = items.filter(Boolean);
  if (!list.length) return;
  view.ctx = { x: e.position.x, y: e.position.y, items: list, at: list.findIndex(usable) };
  // The menu isn't drawn yet: it takes the keyboard on the next frame.
  cx.timer.after(16, (cx) => { if (view.ctx) { view.menuHandle.focus(); cx.notify(); } });
  cx.notify();
}

export function closeMenu(view, cx) {
  if (!view.ctx) return;
  view.ctx = null;
  view.refocus(cx);
  cx.notify();
}

function choose(view, it, cx) {
  view.ctx = null;
  view.refocus(cx);
  it.run(cx);
  cx.notify();
}

/** Up/Down/Enter/Escape for the open menu; true when it took the key. */
export function menuKey(view, e, cx) {
  const m = view.ctx;
  if (!m) return false;
  const k = e.keystroke;
  const step = (d) => {
    for (let n = 1; n <= m.items.length; n++) {
      const i = (m.at + d * n + m.items.length * n) % m.items.length;
      if (usable(m.items[i])) { m.at = i; return; }
    }
  };
  if (k === "down") step(1);
  else if (k === "up") step(-1);
  else if (k === "enter" && usable(m.items[m.at])) choose(view, m.items[m.at], cx);
  else if (k === "escape") closeMenu(view, cx);
  else return false;
  cx.stop_propagation();
  cx.notify();
  return true;
}

/** The open menu, placed inside the panel (`origin`/`size` from its last press), or null. */
export function menuLayer(view, t, zoom = 1) {
  const m = view.ctx;
  if (!m) return null;
  const o = view.origin ?? { x: 0, y: 0, width: 4000, height: 4000 };
  const height = m.items.reduce((h, it) => h + (it === "-" ? 9 : ITEM), 10);
  const x = Math.max(4, Math.min((m.x - o.x) / zoom, o.width / zoom - WIDTH - 8));
  const y = Math.max(4, Math.min((m.y - o.y) / zoom, o.height / zoom - height - 8));
  return v_flex().id("dbx-ctx").absolute().left(x).top(y).w(WIDTH).py(5).rounded(8).bg(t.popover)
    .border_1().border_color(t.border).shadow_lg().text_size(12.5)
    .track_focus(view.menuHandle).on_key_down((e, cx) => menuKey(view, e, cx))
    .on_mouse_down_out((_e, cx) => closeMenu(view, cx))
    .children(m.items.map((it, i) => it === "-"
      ? div().h(1).mx(8).my(4).bg(t.border)
      : h_flex().id(`dbx-ctx-${i}`).h(ITEM).mx(4).px(8).gap(10).items_center().rounded(5)
        .text_color(it.disabled ? t.dim : it.danger ? t.ansi1 : t.foreground)
        .when(i === m.at, (e) => e.bg(t.hover))
        .when(!it.disabled, (e) => e.cursor_pointer().hover((s) => s.bg(t.hover)).on_click((_e, cx) => choose(view, it, cx)))
        .child(div().flex_1().min_w(0).truncate().child(it.label))
        .when(Boolean(it.hint), (e) => e.child(div().text_size(11).text_color(t.dim).child(keyLabel(it.hint))))));
}
