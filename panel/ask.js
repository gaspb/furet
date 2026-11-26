// Asking an agent: you type the question, the panel adds what the agent needs (connection and
// stage, the SQL, the error, the tables involved, how to query on its own with `furet query`).
// By default a new agent starts in its own tab with that as its first message; Tab picks another
// agent, or one already running in this space (`hc send`). Row values only go along from
// local/dev/test connections.
import { div } from "gpui-kit";
import { h_flex, Input } from "gpui-base";
import { agents, hc, knownAgents, run } from "herdr";
import { statementAt } from "./sql.js";

const SAFE = ["local", "dev", "test"];

/** Where a question can go: a new agent in a tab (installed ones, most used first), then the
 *  agents running in this space, idle before busy. */
function targets(view) {
  const fresh = (view.installedAgents ?? []).map((p) => ({ key: `new:${p.run.join(" ")}`, fresh: p }));
  const rank = (a) => (a.status === "idle" || a.status === "done" ? 0 : 1) * 10 + (a.focused ? 0 : 1);
  const running = agents().filter((a) => a.space === view.spaceId)
    .sort((a, b) => rank(a) - rank(b) || b.seq - a.seq)
    .map((a) => ({ key: `run:${a.address}`, agent: a }));
  return [...fresh, ...running];
}

/** Which agents are installed, asked of herdr (it caches the answer). */
export function loadAgents(view, cx) {
  cx.spawn(async (cx) => {
    try {
      view.installedAgents = (await knownAgents()).filter((p) => p.installed && p.run?.length);
      const a = view.activeTab()?.ask;
      if (a) { a.targets = targets(view); a.at = Math.max(0, a.targets.findIndex((x) => x.key === view.askChoice)); }
    } catch { view.installedAgents ??= []; }
    cx.notify();
  });
}

export function nextTarget(view, a) {
  if (!a.targets.length) return;
  a.at = (a.at + 1) % a.targets.length;
  view.askChoice = a.targets[a.at].key;
}

/** `about`: "statement", "error", "result", "table". */
export function openAsk(view, tab, about, cx) {
  const list = targets(view);
  if (!list.length && view.installedAgents) { view.flash("No agent found: install one (Claude Code, Codex…) or start one in a pane", cx); return; }
  tab.ask = { about, targets: list, at: Math.max(0, list.findIndex((x) => x.key === view.askChoice)), sending: false };
  loadAgents(view, cx);
  const src = view.source(tab.conn);
  if (!src.catalog && !src.loading) view.refreshCatalog(tab.conn, cx);
  view.askInput.set_value("");
  view.askInput.focus();
  cx.notify();
}

export function closeAsk(view, tab, cx) {
  tab.ask = null;
  cx.notify();
}

const fence = (s) => "```sql\n" + s.trim() + "\n```";

function tablesIn(view, tab, sql) {
  const tables = view.source(tab.conn).catalog?.tables ?? [];
  const named = tables.filter((x) => new RegExp(`\\b${x.name}\\b`, "i").test(sql));
  if (tab.kind === "table" && !named.some((x) => x.name === tab.table)) {
    const own = tables.find((x) => x.schema === tab.schema && x.name === tab.table);
    if (own) named.unshift(own);
  }
  return named.slice(0, 8).map((x) => `- ${x.schema}.${x.name}: ${(x.columns ?? []).map((c) => `${c.name} ${c.type}`).join(", ")}`);
}

/** The question, then what the agent needs to answer it. */
export function context(view, tab, question) {
  const c = view.conn(tab.conn) ?? {}, about = tab.ask.about;
  const stage = c.stage ?? "?", readonly = tab.session?.readonly ?? view.source(tab.conn).session?.readonly ?? c.readonly;
  const set = tab.results?.[tab.picked ?? 0];
  let sql = "";
  if (tab.kind === "console" && tab.editor) {
    const sel = tab.editor.selected_range(), text = tab.editor.value();
    sql = sel.start !== sel.end ? text.slice(Math.min(sel.start, sel.end), Math.max(sel.start, sel.end)) : statementAt(text, sel.start).text;
  } else if (tab.kind === "table") {
    sql = `select * from ${tab.schema}.${tab.table}${tab.where ? ` where ${tab.where}` : ""}${tab.order ? ` order by ${tab.order}` : ""}`;
  }
  if (tab.step) sql = tab.step.sql;
  const parts = [question.trim() || "Help me with this.", "",
    `Context from the furet panel: connection \`${tab.conn}\` (${c.kind ?? "postgres"}, stage ${stage}${readonly ? ", read-only" : ""}).`];
  if (sql) parts.push("", "SQL:", fence(sql));
  if (tab.error?.message && (about === "error" || about === "statement")) {
    parts.push("", `Error: ${tab.error.message}${tab.error.detail ? `\nDetail: ${tab.error.detail}` : ""}${tab.error.hint ? `\nHint: ${tab.error.hint}` : ""}`);
  }
  if (set?.columns.length && about !== "error") {
    parts.push("", `Result: ${set.rows.length}${set.more ? "+" : ""} rows; columns ${set.columns.map((x) => `${x.name} ${x.type ?? ""}`.trim()).join(", ")}.`);
    if (SAFE.includes(stage)) {
      const rows = set.rows.slice(0, 10).map((r) => r.map((v) => (v === null ? "null" : v.length > 80 ? v.slice(0, 80) + "…" : v)).join(" | "));
      parts.push("First rows:", "```", ...rows, "```");
    } else parts.push(`(Row values left out: ${stage} data. Query it yourself if you need them.)`);
  }
  const tables = tablesIn(view, tab, sql);
  if (tables.length) parts.push("", "Tables:", ...tables);
  parts.push("", `You can run queries yourself: \`furet query ${tab.conn} "select …"\` (add --json; read-only rules apply; \`furet --skill\` for the rest).`);
  return parts.join("\n");
}

export function sendAsk(view, tab, cx) {
  const a = tab.ask, target = a?.targets[a.at];
  if (!target || a.sending) return;
  const text = context(view, tab, view.askInput.value());
  view.askChoice = target.key;
  if (target.fresh) {
    run("tab.new", { name: `Ask · ${tab.conn}`, folder: view.folder(), argv: target.fresh.run, task: text, focus: true });
    tab.ask = null;
    view.flash(`Asked a new ${target.fresh.name} in its own tab`, cx);
    cx.notify();
    return;
  }
  const agent = target.agent;
  a.sending = true;
  cx.spawn(async (cx) => {
    try {
      const r = await hc(["send", agent.address, text]);
      if (r.code !== 0) throw new Error(r.stderr.trim() || `hc send exited with ${r.code}`);
      tab.ask = null;
      run("agent.focus", { terminal: agent.address });
      view.flash(`Sent to ${agent.app ?? agent.title} (${agent.address})`, cx);
    } catch (e) { a.sending = false; view.flash(String(e.message ?? e), cx); }
    cx.notify();
  });
  cx.notify();
}

function targetName(target) {
  if (!target) return "looking for agents…";
  if (target.fresh) return `new ${target.fresh.name} tab`;
  const g = target.agent;
  return `${g.app ?? g.title ?? "agent"} · ${g.tab_name ?? g.address}${g.status ? ` · ${g.status}` : ""}`;
}

export function askBar(view, tab, t) {
  const a = tab.ask, name = targetName(a.targets[a.at]);
  const what = { statement: "about this statement", error: "about this error", result: "about these rows", table: `about ${tab.table ?? "this table"}` }[a.about];
  return h_flex().h(40).flex_none().px(10).gap(8).items_center().bg(t.raised).border_b(1).border_color(t.border).key_context("FuretAsk")
    .child(div().text_size(12).text_color(t.muted).child("Ask"))
    .child(div().id(`furet-ask-agent-${tab.id}`).px(8).py(2).rounded(5).text_size(12).text_color(t.accent).cursor_pointer().hover((s) => s.bg(t.hover))
      .on_click((_e, cx) => { nextTarget(view, a); cx.notify(); })
      .child(a.targets.length > 1 ? `${name} ▾` : name))
    .child(div().text_size(12).text_color(t.muted).child(what))
    .child(h_flex().h(28).flex_1().min_w(120).px(8).items_center().rounded(6).border_1().border_color(t.accent).bg(t.background)
      .child(Input.new(view.askInput).flex_1().h_full().text_size(12)))
    .child(div().text_size(11).text_color(t.dim).child(a.sending ? "sending…" : "Enter · Tab another agent · Esc"));
}
