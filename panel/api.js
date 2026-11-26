// Talks to `furet serve`: starts it if needed, sends the token, starts it again if it went idle.
import { exec } from "herdr";

let server = null;

async function ensure() {
  const r = await exec("furet", ["serve", "--ensure"], { timeout: 20000 });
  if (r.code !== 0) throw new Error(r.stderr.trim() || `furet serve exited with ${r.code}`);
  server = JSON.parse(r.stdout);
  return server;
}

async function send(method, path, body) {
  const s = server ?? (await ensure());
  const res = await fetch(s.url + path, {
    method,
    headers: { Authorization: `Bearer ${s.token}`, "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const data = await res.json().catch(() => ({ error: `furet serve answered ${res.status}` }));
  if (!res.ok) {
    const e = new Error(data.error ?? `HTTP ${res.status}`);
    e.status = res.status;
    throw e;
  }
  return data;
}

/** One request; a server that stopped (idle, upgraded) is started again once. */
export async function call(method, path, body) {
  try {
    return await send(method, path, body);
  } catch (e) {
    if (e.status) throw e;
    server = null;
    return send(method, path, body);
  }
}

export function missing(e) {
  return /not found|cannot start|No such file/i.test(String(e?.message ?? e));
}

export const INSTALL = "curl -LsSf https://github.com/gaspb/furet/releases/latest/download/furet-installer.sh | sh";
