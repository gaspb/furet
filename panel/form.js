// Adding and editing a connection: a form tab that tests and saves through `furet serve`.
// Passwords go to the OS keychain by default; the file only names them.
import { div } from "gpui-kit";
import { v_flex, h_flex, InputState, Input } from "gpui-base";
import { run } from "herdr";
import { call } from "./api.js";

const STAGES = ["local", "dev", "stg", "prod"];
const SOURCES = [
  ["keychain", "Keychain"],
  ["env", "Env var"],
  ["dotenv", ".env"],
  ["cmd", "Command"],
  ["none", "None"],
];

function inputs(tab) {
  const make = (placeholder, value = "") => InputState.new({ placeholder, value });
  tab.f = {
    name: make("e.g. stg"),
    url: make("e.g. postgres://user@localhost:5432/app"),
    secret: make("Password (saved in the keychain)"),
    env: make("e.g. PGPASSWORD_STG"),
    dotenvFile: make(".env", ".env"),
    dotenvKey: make("e.g. DB_PASSWORD"),
    cmd: make("e.g. kubectl -n stg get secret pg -o jsonpath='{.data.password}' | base64 -d"),
    tunnel: make("e.g. kubectl -n stg port-forward svc/postgres 15432:5432"),
    note: make("Optional"),
    stage: make("e.g. qa"),
  };
  tab.f.secret.set_masked(true);
}

/** Opens the form: empty for a new connection, filled from the file for `name`. */
export function openForm(view, name, cx, { fromDetected = false } = {}) {
  const previous = fromDetected ? null : name ?? null;
  const existing = view.tabs.find((x) => x.kind === "form" && x.previous === previous && x.from === (fromDetected ? name : undefined));
  if (existing) { view.active = existing.id; cx.notify(); return; }
  const tab = view.addTab({ kind: "form", conn: name ?? view.current, title: fromDetected ? `Save ${name}` : name ? `Edit ${name}` : "New connection",
    previous, from: fromDetected ? name : undefined, pw: "keychain", stage: "local", global: false, keep: false, busy: null, result: null, error: null, loading: Boolean(name) }, cx);
  inputs(tab);
  if (!name) { tab.f.name.focus(); return; }
  cx.spawn(async (cx) => {
    try {
      const d = await call("GET", `/connections/draft?cwd=${encodeURIComponent(view.folder())}&name=${encodeURIComponent(name)}`);
      tab.f.name.set_value(d.name);
      tab.f.url.set_value(d.url);
      tab.f.tunnel.set_value(d.tunnel ?? "");
      tab.f.note.set_value(d.note ?? "");
      tab.global = d.global;
      if (STAGES.includes(d.stage ?? "local")) tab.stage = d.stage ?? "local";
      else { tab.stage = "other"; tab.f.stage.set_value(d.stage); }
      const p = d.password ?? { kind: "none" };
      tab.keep = p.kind === "keychain" || p.kind === "file";
      tab.docker = p.kind === "docker" ? p : null;
      tab.pw = p.kind === "file" || p.kind === "docker" ? "keychain" : p.kind;
      if (p.kind === "env") tab.f.env.set_value(p.name);
      if (p.kind === "dotenv") { tab.f.dotenvFile.set_value(p.file); tab.f.dotenvKey.set_value(p.key); }
      if (p.kind === "cmd") tab.f.cmd.set_value(p.cmd);
    } catch (e) { tab.error = String(e.message ?? e); }
    finally { tab.loading = false; cx.notify(); }
  });
}

function draft(tab) {
  const v = (k) => tab.f[k].value().trim();
  const password = {
    keychain: v("secret") ? { kind: "keychain", secret: tab.f.secret.value() } : tab.keep ? { kind: "keep" } : tab.docker ?? { kind: "none" },
    env: { kind: "env", name: v("env") },
    dotenv: { kind: "dotenv", file: v("dotenvFile") || ".env", key: v("dotenvKey") },
    cmd: { kind: "cmd", cmd: v("cmd") },
    none: { kind: "none" },
  }[tab.pw];
  return {
    name: v("name"), url: v("url"), password, global: tab.global,
    stage: tab.stage === "other" ? v("stage") || null : tab.stage,
    tunnel: v("tunnel") || null, note: v("note") || null,
  };
}

function submit(view, tab, what, cx) {
  if (tab.busy) return;
  tab.busy = what; tab.result = null; tab.error = null;
  cx.spawn(async (cx) => {
    try {
      const out = await call("POST", `/connections/${what}`, { cwd: view.folder(), draft: draft(tab), previous: tab.previous });
      if (what === "test") tab.result = out;
      else view.saved(tab, draft(tab).name, out.notes ?? [], cx);
    } catch (e) { tab.error = String(e.message ?? e); }
    finally { tab.busy = null; cx.notify(); }
  });
  cx.notify();
}

export function removeConnection(view, name, cx) {
  cx.spawn(async (cx) => {
    try {
      await call("DELETE", `/connections?cwd=${encodeURIComponent(view.folder())}&name=${encodeURIComponent(name)}`);
      view.forget(name, cx);
      view.flash(`Removed ${name}`, cx);
    } catch (e) { view.flash(String(e.message ?? e), cx); }
    view.menu = null;
    cx.notify();
  });
}

export function editFile(view, global = false) {
  run("terminal.open", { argv: ["furet", "edit", ...(global ? ["--global"] : [])], folder: view.folder(), place: "right", name: "furet connections" });
}

export function formBody(view, tab, t) {
  const field = (input, opts = {}) => h_flex().h(30).flex_1().min_w(0).px(8).items_center().rounded(6).border_1().border_color(t.border).bg(t.background)
    .when(Boolean(opts.mono), (e) => e.font_family(view.font().font_family))
    .child(Input.new(input).flex_1().h_full().text_size(12.5));
  const row = (label, content, hint) => h_flex().gap(12).items_start().py(5)
    .child(div().w(110).flex_none().pt(6).text_size(12).text_color(t.muted).child(label))
    .child(v_flex().flex_1().min_w(0).gap(4).child(content).when(Boolean(hint), (e) => e.child(div().text_size(11).text_color(t.dim).child(hint))));
  const seg = (id, options, value, pick) => h_flex().gap(2).p(2).rounded(7).bg(t.raised).flex_wrap()
    .children(options.map(([k, label]) => div().id(`${id}-${k}`).px(10).h(24).flex().items_center().rounded(5).text_size(12).cursor_pointer()
      .bg(k === value ? t.selected : t.raised).text_color(k === value ? t.foreground : t.muted)
      .hover((s) => s.text_color(t.foreground)).on_click((_e, cx) => { pick(k); cx.notify(); }).child(label)));
  const pwField = {
    keychain: v_flex().gap(4).child(field(tab.f.secret)),
    env: field(tab.f.env, { mono: true }),
    dotenv: h_flex().gap(6).child(field(tab.f.dotenvFile, { mono: true })).child(field(tab.f.dotenvKey, { mono: true })),
    cmd: field(tab.f.cmd, { mono: true }),
    none: div().text_size(12).text_color(t.dim).pt(6).child("No password, or the one in the URL (moved to the keychain on save)."),
  }[tab.pw];
  const pwHint = {
    keychain: tab.keep ? "Leave empty to keep the saved password; type one to replace it."
      : tab.docker ? "Leave empty to save the container's own password (read by furet, never shown)." : "Stored in the OS keychain; the file only names it.",
    env: "Read from the herdr app's environment when the connection opens.",
    dotenv: "File (relative to the worktree) and key; each worktree reads its own.",
    cmd: "Its output is the password, e.g. a kubectl secret or `op read`.",
    none: null,
  }[tab.pw];
  const readonly = !["local", "dev", "test"].includes(tab.stage === "other" ? tab.f.stage.value().trim() || "remote" : tab.stage);
  const status = tab.error ? [t.ansi1, tab.error] : tab.result ? [tab.result.ok ? t.ansi2 : t.ansi1, tab.result.message] : null;
  return v_flex().id(`furet-form-${tab.id}`).size_full().overflow_y_scroll()
    .child(v_flex().max_w(720).w_full().px(20).py(16).gap(2)
      .child(div().text_size(15).font_semibold().pb(8).child(tab.title))
      .when(Boolean(tab.from), (e) => e.child(div().text_size(12).text_color(t.dim).pb(6)
        .child("Found running in Docker. Saving keeps it when the container is stopped or renamed; the password goes to the keychain.")))
      .when(tab.loading, (e) => e.child(div().text_size(12).text_color(t.dim).child("Loading…")))
      .child(row("Name", field(tab.f.name), "Letters, digits, - _ ."))
      .child(row("URL", field(tab.f.url, { mono: true }), "postgres://… or clickhouse://… (native port; add ?http_port= if HTTP isn't on 8123), with or without the password"))
      .child(row("Password", v_flex().gap(6).child(seg(`furet-pw-${tab.id}`, SOURCES, tab.pw, (k) => { tab.pw = k; })).child(pwField), pwHint))
      .child(row("Stage", v_flex().gap(6)
        .child(seg(`furet-stage-${tab.id}`, [...STAGES.map((s) => [s, s]), ["other", "other…"]], tab.stage, (k) => { tab.stage = k; }))
        .when(tab.stage === "other", (e) => e.child(field(tab.f.stage))),
        readonly ? "Opens read-only; writes need a confirmation (typing the name, on prod)." : "Writable."))
      .child(row("Tunnel", field(tab.f.tunnel, { mono: true }), "Optional: started before connecting (unless the port already answers), stopped after."))
      .child(row("Note", field(tab.f.note)))
      .child(row("Saved in", seg(`furet-scope-${tab.id}`, [["repo", "This repo"], ["global", "Every repo"]], tab.global ? "global" : "repo", (k) => { tab.global = k === "global"; })))
      .child(h_flex().gap(8).pt(14).items_center()
        .child(view.button(`furet-form-save-${tab.id}`, tab.busy === "save" ? "Saving…" : "Save", (cx) => submit(view, tab, "save", cx), { t, primary: true, disabled: Boolean(tab.busy) }))
        .child(view.button(`furet-form-test-${tab.id}`, tab.busy === "test" ? "Connecting…" : "Test", (cx) => submit(view, tab, "test", cx), { t, disabled: Boolean(tab.busy) }))
        .child(view.button(`furet-form-cancel-${tab.id}`, "Cancel", (cx) => view.closeTab(tab.id, cx), { t }))
        .child(div().flex_1())
        .child(div().id(`furet-form-file-${tab.id}`).text_size(11.5).text_color(t.dim).cursor_pointer().hover((s) => s.text_color(t.foreground))
          .on_click(() => editFile(view, tab.global)).child("Edit the file instead")))
      .when(Boolean(status), (e) => e.child(div().mt(10).px(10).py(8).rounded(7).text_size(12).text_color(status[0]).border_1().border_color(status[0]).child(status[1]))));
}
