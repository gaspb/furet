// Line commands for the console's editor, as pure functions of its text and selection (UTF-16
// offsets). Each returns what to do: `clip` for the clipboard, `replace` + `text` to edit, `select` after.

const lineStart = (s, i) => s.lastIndexOf("\n", i - 1) + 1;
const lineEnd = (s, i) => { const n = s.indexOf("\n", i); return n < 0 ? s.length : n; };

/** The lines a selection touches, without the last one's newline; a selection ending at a line's start leaves it out. */
export function lines(s, sel) {
  const a = Math.min(sel.start, sel.end), b = Math.max(sel.start, sel.end);
  return { start: lineStart(s, a), end: lineEnd(s, b > a && s[b - 1] === "\n" ? b - 1 : b) };
}

const ordered = (sel) => ({ a: Math.min(sel.start, sel.end), b: Math.max(sel.start, sel.end) });
const at = (i) => ({ start: i, end: i });

/** Copies the selection, or the whole line under the cursor. */
export function copyLine(s, sel) {
  const { a, b } = ordered(sel);
  if (a !== b) return { clip: s.slice(a, b) };
  const l = lines(s, sel);
  return { clip: s.slice(l.start, l.end) + "\n" };
}

/** Removes the lines a selection touches, with their newline. */
export function deleteLine(s, sel) {
  const l = lines(s, sel);
  const range = l.end < s.length ? { start: l.start, end: l.end + 1 } : { start: Math.max(0, l.start - 1), end: l.end };
  return { replace: range, text: "", select: at(range.start === l.start ? l.start : lineStart(s, l.start - 1)) };
}

/** Cuts the selection, or the whole line under the cursor. */
export function cutLine(s, sel) {
  const { a, b } = ordered(sel);
  if (a !== b) return { clip: s.slice(a, b), replace: { start: a, end: b }, text: "", select: at(a) };
  return { ...deleteLine(s, sel), clip: copyLine(s, sel).clip };
}

/** Duplicates the selection after itself, or the lines under the cursor below them. */
export function duplicate(s, sel) {
  const { a, b } = ordered(sel);
  if (a !== b) return { replace: at(b), text: s.slice(a, b), select: { start: b, end: b + (b - a) } };
  const l = lines(s, sel), block = s.slice(l.start, l.end), shift = block.length + 1;
  return { replace: at(l.end), text: "\n" + block, select: at(sel.start + shift) };
}

/** Moves the lines a selection touches up (`-1`) or down (`1`), keeping them selected. */
export function moveLines(s, sel, dir) {
  const l = lines(s, sel), block = s.slice(l.start, l.end);
  if (dir < 0) {
    if (l.start === 0) return {};
    const ps = lineStart(s, l.start - 1), prev = s.slice(ps, l.start - 1), d = -(prev.length + 1);
    return { replace: { start: ps, end: l.end }, text: block + "\n" + prev, select: { start: sel.start + d, end: sel.end + d } };
  }
  if (l.end >= s.length) return {};
  const ne = lineEnd(s, l.end + 1), next = s.slice(l.end + 1, ne), d = next.length + 1;
  return { replace: { start: l.start, end: ne }, text: next + "\n" + block, select: { start: sel.start + d, end: sel.end + d } };
}

/** Comments the lines out with `-- `, or back in when they all are. */
export function toggleComment(s, sel) {
  const l = lines(s, sel), list = s.slice(l.start, l.end).split("\n");
  const filled = list.filter((x) => x.trim());
  if (!filled.length) return {};
  const commented = filled.every((x) => x.trimStart().startsWith("--"));
  const indent = Math.min(...filled.map((x) => x.length - x.trimStart().length));
  const next = list.map((x) => {
    if (!x.trim()) return x;
    if (!commented) return x.slice(0, indent) + "-- " + x.slice(indent);
    const i = x.indexOf("--");
    return x.slice(0, i) + x.slice(i + (x[i + 2] === " " ? 3 : 2));
  }).join("\n");
  const { a, b } = ordered(sel);
  const firstDelta = next.split("\n")[0].length - list[0].length;
  const select = a === b
    ? at(Math.max(l.start, a + firstDelta))
    : { start: l.start, end: l.start + next.length };
  return { replace: { start: l.start, end: l.end }, text: next, select };
}
