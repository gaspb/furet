// The panel's shortcuts, shown by F1 or the tree's "?" (and listed in the README).
import { div } from "gpui-kit";
import { v_flex, h_flex } from "gpui-base";
import { platform } from "process";

/** A shortcut as this platform spells it: ⌘ and ⌥ are Ctrl and Alt outside macOS. */
export const keyLabel = (s) => (platform === "darwin" ? s
  : s.replaceAll("⌘", "Ctrl+").replaceAll("⌥", "Alt+").replaceAll("⇧", "Shift+").replace(/\+\s+/g, "+"));

export const KEYS = [
  ["Console", [
    ["⌘ Enter", "run the statement under the cursor, or the selection"],
    ["⌘ ⇧ Enter", "run everything"],
    ["Ctrl+Space", "suggestions (they also show while you type)"],
    ["↑ ↓ · PgUp PgDn", "move through suggestions (PgUp PgDn: Fn ↑ ↓ on a Mac laptop)"],
    ["Tab · Enter", "take the suggestion · Esc closes the list"],
    ["Ctrl+R", "find a saved query or one that ran before (yours, agents'): Enter puts it in, ⌘ Enter runs it, Tab: every connection"],
    ["⌘ S", "save the statement under the cursor in .dbx/queries (commit it to share)"],
    ["⌘ C · ⌘ X", "copy or cut the line when nothing is selected"],
    ["Ctrl+D", "duplicate the line or the selection"],
    ["⌥⇧↑ · ⌥⇧↓", "move the line (or the selected lines)"],
    ["Ctrl+/", "comment the lines in or out"],
    ["⌘ F · ⌘ ⇧ F", "search · replace (Ctrl+H on Linux)"],
    ["⌘⌥↑ · ⌘⌥↓", "add a cursor above or below"],
    ["⌘ ] · ⌘ [", "indent · outdent"],
    ["⌘ Z · ⌘ ⇧ Z", "undo · redo (Ctrl+Y on Linux)"],
  ]],
  ["Tree", [
    ["↑ ↓", "move"],
    ["→ · ←", "open or go in · close or go to the parent"],
    ["Enter", "connect, open a schema or a table"],
    ["⌘ Enter", "new console (on a table: with its first rows)"],
    ["/", "filter tables · ↓ from the filter goes back to the tree"],
    ["⇧ Enter · right-click", "actions (or ⇧ F10)"],
    ["?", "this list"],
  ]],
  ["Results", [
    ["↑ ↓ ← →", "move the selected cell"],
    ["⌘ C", "copy the cell"],
    ["Enter · Delete", "edit the cell · mark the row for deletion (tables with a primary key)"],
    ["⌘ Enter · ⌘ click", "follow a foreign key: the results become the row it points at (on a trail)"],
    ["r", "the rows pointing at the selected row (referenced by)"],
    ["Esc", "back one step on the trail; with none, back to the editor"],
    ["Space", "show or hide the row inspector"],
    ["⇧ Enter · right-click", "copy as JSON or CSV, filter by the value, set NULL…"],
  ]],
  ["Table view", [
    ["Tab · →", "in an empty WHERE or ORDER BY: take the example (its value selected, to type over)"],
    ["Tab", "from WHERE to ORDER BY to the rows"],
    ["Enter", "apply WHERE and ORDER BY"],
  ]],
  ["Panel", [
    ["Ctrl+` · Ctrl+⇧`", "move between tree, editor and results (or F6 · ⇧ F6)"],
    ["Ctrl+⇧] · Ctrl+⇧[", "next or previous tab (or ⌥PgDn · ⌥PgUp)"],
    ["⌘ W", "close the tab"],
    ["F1 · ? in the tree", "this list"],
  ]],
];

export function helpBody(t, font) {
  return v_flex().id("dbx-help").size_full().overflow_y_scroll()
    .child(v_flex().max_w(760).w_full().px(20).py(16).gap(14)
      .child(div().text_size(15).font_semibold().child("Keys"))
      .children(KEYS.map(([section, rows]) => v_flex().gap(2)
        .child(div().pb(4).text_size(11).font_semibold().text_color(t.muted).child(section))
        .children(rows.map(([keys, what], i) => h_flex().id(`dbx-help-${section}-${i}`).py(3).gap(16).items_center()
          .child(div().w(210).flex_none().font_family(font.font_family).text_size(12).text_color(t.foreground).child(keyLabel(keys)))
          .child(div().flex_1().min_w(0).text_size(12).text_color(t.muted).child(what)))))));
}
