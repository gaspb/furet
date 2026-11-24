mod config;
mod conn;
mod engine;
mod picker;
mod store;
mod tunnel;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use config::{Entry, File, Origin, Scope, tilde};
use conn::Resolved;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use toml_edit::DocumentMut;
use tunnel::Tunnel;

const SKILL: &str = include_str!("skill.md");
const ICON: &str = include_str!("../assets/herdr-icon.svg");

/// Database connections scoped to the repo you're in, kept outside it.
///
/// Run `dbx` to pick a connection, `dbx <name>` to open one in its client.
/// Secrets are read when needed (env, .env, a command); tunnels start and stop on their own;
/// anything that isn't local opens read-only unless you pass -w.
#[derive(Parser)]
#[command(
    name = "dbx",
    version,
    args_conflicts_with_subcommands = true,
    after_help = "Are you an AI agent? Run `dbx --skill` to learn how to query the user's databases."
)]
struct Cli {
    /// Print the guide for AI agents
    #[arg(long, exclusive = true)]
    skill: bool,

    /// Connection to open (default: pick one)
    name: Option<String>,

    /// Allow writes on a connection that is read-only by default
    #[arg(short, long)]
    write: bool,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// List the connections visible from here
    #[command(visible_alias = "ls")]
    List {
        /// Every project's own connections, and the global ones
        #[arg(short, long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Open a connection in its client (same as `dbx <name>`)
    Open {
        name: String,
        #[arg(short, long)]
        write: bool,
    },
    /// Run a command with the connection's env (PG*/CLICKHOUSE_*, DATABASE_URL) and tunnel
    Run {
        name: String,
        #[arg(short, long)]
        write: bool,
        #[arg(last = true, required = true)]
        argv: Vec<String>,
    },
    /// Print the connection URL, password included
    Url {
        name: String,
        #[arg(short, long)]
        write: bool,
    },
    /// Print the connection's variables as `export` lines
    Env {
        name: String,
        #[arg(short, long)]
        write: bool,
    },
    /// Keep a connection's tunnel up until Ctrl-C
    Tunnel { name: String },
    /// Add a connection to this repo (or the global file)
    Add {
        name: String,
        /// postgres://… or clickhouse://…
        url: String,
        /// local, dev, stg, prod…
        #[arg(long)]
        stage: Option<String>,
        /// Command that makes the url's host:port reachable
        #[arg(long)]
        tunnel: Option<String>,
        #[arg(long)]
        global: bool,
    },
    /// Remove a connection (and its keychain password) from this repo or the global file
    Rm { name: String },
    /// Also use another repo's connections from this one
    Include { path: PathBuf },
    /// Copy a connection from another repo into this one
    Cp {
        name: String,
        /// A folder of the repo to copy from
        #[arg(long)]
        from: PathBuf,
        /// Name in this repo
        #[arg(long = "as")]
        rename: Option<String>,
    },
    /// Open this repo's connections file (or the global one) in $EDITOR
    Edit {
        #[arg(long)]
        global: bool,
    },
    /// Print the path of this repo's connections file (or the global one)
    Path {
        #[arg(long)]
        global: bool,
    },
    /// Set up an integration: `herdr` gives dbx its own name and icon in the herdr app
    Setup { target: Integration },
    /// The local engine behind the herdr panel (127.0.0.1, token-protected)
    Serve {
        /// Start it in the background unless it's running; print its url and token as JSON
        #[arg(long)]
        ensure: bool,
        /// Stop the running server (its sessions and tunnels close)
        #[arg(long, conflicts_with = "ensure")]
        stop: bool,
        /// Stop after this many idle minutes with no session open
        #[arg(long, default_value_t = 60)]
        idle_minutes: u64,
    },
}

#[derive(Clone, ValueEnum)]
enum Integration {
    Herdr,
}

fn main() {
    tunnel::stop_tunnels_on_exit_signals();
    match run(Cli::parse()) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("dbx: {e:#}");
            std::process::exit(1);
        }
    }
}

fn cwd() -> Result<PathBuf> {
    std::env::current_dir().context("cannot read current directory")
}

fn run(cli: Cli) -> Result<i32> {
    if cli.skill {
        print!("{SKILL}");
        return Ok(0);
    }
    let cwd = cwd()?;
    match (cli.cmd, cli.name) {
        (None, Some(name)) => open(&Scope::load(&cwd)?, &name, cli.write, None),
        (None, None) => pick(&cwd),
        (Some(cmd), _) => command(cmd, &cwd),
    }
}

fn command(cmd: Cmd, cwd: &Path) -> Result<i32> {
    match cmd {
        Cmd::List { all, json } => list(cwd, all, json),
        Cmd::Open { name, write } => open(&Scope::load(cwd)?, &name, write, None),
        Cmd::Run { name, write, argv } => {
            let scope = Scope::load(cwd)?;
            let (r, _tunnel) = prepare(&scope, &name, write)?;
            exec(&r, &argv)
        }
        Cmd::Url { name, write } => {
            let scope = Scope::load(cwd)?;
            println!("{}", resolve(&scope, &name, write)?.url());
            Ok(0)
        }
        Cmd::Env { name, write } => {
            let scope = Scope::load(cwd)?;
            for (k, v) in resolve(&scope, &name, write)?.env() {
                println!("export {k}='{}'", v.replace('\'', r"'\''"));
            }
            Ok(0)
        }
        Cmd::Tunnel { name } => keep_tunnel(&Scope::load(cwd)?, &name),
        Cmd::Add {
            name,
            url,
            stage,
            tunnel,
            global,
        } => add(cwd, &name, &url, stage, tunnel, global),
        Cmd::Rm { name } => {
            println!("removed {name} from {}", tilde(&store::remove(cwd, &name)?));
            Ok(0)
        }
        Cmd::Include { path } => include(cwd, &path),
        Cmd::Cp { name, from, rename } => copy(cwd, &name, &from, rename.as_deref()),
        Cmd::Edit { global } => edit(&store::target_file(cwd, global)?, global),
        Cmd::Path { global } => {
            println!("{}", store::target_file(cwd, global)?.display());
            Ok(0)
        }
        Cmd::Serve { stop: true, .. } => {
            println!(
                "{}",
                if engine::server::stop() {
                    "stopped"
                } else {
                    "not running"
                }
            );
            Ok(0)
        }
        Cmd::Serve { ensure: true, .. } => {
            println!("{}", serde_json::to_string(&engine::server::ensure()?)?);
            Ok(0)
        }
        Cmd::Serve { idle_minutes, .. } => {
            engine::server::serve(std::time::Duration::from_secs(idle_minutes * 60))?;
            Ok(0)
        }
        Cmd::Setup {
            target: Integration::Herdr,
        } => {
            common::herdr::setup(&common::herdr::Program {
                id: "dbx",
                name: "Databases",
                icon_svg: ICON,
                tile: "#336791",
                matches: &["dbx"],
            })?;
            Ok(0)
        }
    }
}

fn resolve(scope: &Scope, name: &str, write: bool) -> Result<Resolved> {
    let e = scope.get(name)?;
    if write && e.conn.readonly() && e.conn.is_prod() {
        confirm_prod(name)?;
    }
    let mut r = conn::resolve(e, scope.layer(e))?;
    if write {
        r.readonly = false;
    }
    Ok(r)
}

fn confirm_prod(name: &str) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        bail!("writing to {name} (prod) needs a person at a terminal to confirm");
    }
    eprint!(
        "{} Type its name to allow writes: ",
        paint("1;31", &format!("{name} is prod."))
    );
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if answer.trim() != name {
        bail!("not confirmed");
    }
    Ok(())
}

fn prepare(scope: &Scope, name: &str, write: bool) -> Result<(Resolved, Option<Tunnel>)> {
    let r = resolve(scope, name, write)?;
    let e = scope.get(name)?;
    let tunnel = match &e.conn.tunnel {
        Some(cmd) => Tunnel::ensure(cmd, &r.host, r.port, &scope.layer(e).base)?,
        None => None,
    };
    Ok((r, tunnel))
}

fn exec(r: &Resolved, argv: &[String]) -> Result<i32> {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).envs(r.env());
    let status = tunnel::run_foreground(&mut cmd).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => anyhow::anyhow!("{} not found on PATH", argv[0]),
        _ => e.into(),
    })?;
    Ok(status.code().unwrap_or(1))
}

fn set_title(title: &str) {
    if std::io::stdout().is_terminal() {
        print!("\x1b]0;{title}\x07");
        let _ = std::io::stdout().flush();
    }
}

/// Opens `name` in its client; `tunnels` keeps tunnels alive across opens from the picker.
fn open(scope: &Scope, name: &str, write: bool, tunnels: Option<&mut Vec<Tunnel>>) -> Result<i32> {
    let (r, tunnel) = prepare(scope, name, write)?;
    let e = scope.get(name)?;
    let custom = e
        .conn
        .client
        .as_deref()
        .or_else(|| scope.clients.get(r.kind.name()).map(Vec::as_slice));
    let argv = r.client(custom);
    eprintln!("{}", banner(e, &r));
    set_title(&format!("dbx · {name}"));
    let code = exec(&r, &argv);
    set_title("dbx");
    if let (Some(keep), Some(t)) = (tunnels, tunnel) {
        keep.push(t);
    }
    code
}

fn stage_color(stage: &str) -> &'static str {
    match stage {
        "local" | "dev" | "test" => "32",
        "prod" | "production" => "1;31",
        "stg" | "staging" | "preprod" => "33",
        _ => "34",
    }
}

fn banner(e: &Entry, r: &Resolved) -> String {
    let stage = e.conn.stage();
    let mode = if r.readonly {
        "read-only"
    } else {
        "read-write"
    };
    let who = r
        .user
        .as_deref()
        .map(|u| format!("{u}@"))
        .unwrap_or_default();
    let db = r
        .database
        .as_deref()
        .map(|d| format!("/{d}"))
        .unwrap_or_default();
    let c = |code: &str, s: &str| paint(code, s);
    format!(
        "{} {} {} {}",
        c("2", "dbx:"),
        c("1", &e.name),
        c(stage_color(stage), stage),
        c(
            "2",
            &format!(
                "{} · {mode} · {who}{}:{}{db}",
                r.kind.name(),
                r.host,
                r.port
            )
        ),
    )
}

/// Colours text for stderr, when it is a terminal.
pub fn paint(code: &str, s: &str) -> String {
    if std::io::stderr().is_terminal() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

fn keep_tunnel(scope: &Scope, name: &str) -> Result<i32> {
    let e = scope.get(name)?;
    let Some(cmd) = &e.conn.tunnel else {
        bail!("{name} has no tunnel")
    };
    let r = conn::resolve(e, scope.layer(e))?;
    let Some(mut t) = Tunnel::ensure(cmd, &r.host, r.port, &scope.layer(e).base)? else {
        eprintln!(
            "{}:{} is already reachable (another tunnel?)",
            r.host, r.port
        );
        return Ok(0);
    };
    eprintln!(
        "tunnel for {name} is up on {}:{}; Ctrl-C to stop",
        r.host, r.port
    );
    set_title(&format!("dbx · {name} tunnel"));
    let status = t.wait();
    eprintln!("tunnel stopped");
    Ok(status.map(|s| s.code().unwrap_or(0)).unwrap_or(0))
}

#[derive(serde::Serialize)]
pub struct Row {
    pub name: String,
    pub kind: Option<&'static str>,
    pub stage: String,
    pub readonly: bool,
    pub endpoint: String,
    pub source: String,
    pub tunnel: bool,
    pub tunnel_up: Option<bool>,
    pub note: Option<String>,
}

pub fn rows(scope: &Scope) -> Vec<Row> {
    scope
        .entries
        .iter()
        .map(|e| {
            let c = &e.conn;
            let host = c
                .host
                .as_ref()
                .and_then(|h| h.plain())
                .unwrap_or("localhost");
            Row {
                name: e.name.clone(),
                kind: c.kind().map(|k| k.name()),
                stage: c.stage().into(),
                readonly: c.readonly(),
                endpoint: conn::endpoint(c),
                source: scope.layer(e).label(),
                tunnel: c.tunnel.is_some(),
                tunnel_up: c
                    .tunnel
                    .as_ref()
                    .and_then(|_| conn::local_port(c))
                    .map(|p| tunnel::is_up(host, p)),
                note: c.note.clone(),
            }
        })
        .collect()
}

fn list(cwd: &Path, all: bool, json: bool) -> Result<i32> {
    let scopes: Vec<(String, Scope)> = if all {
        let mut v = Vec::new();
        for root in config::all_projects() {
            let mut s = Scope::load(&root)?;
            s.entries
                .retain(|e| s.layers[e.layer].origin == Origin::Project);
            v.push((tilde(&root), s));
        }
        let mut g = Scope::load(cwd)?;
        g.entries
            .retain(|e| g.layers[e.layer].origin == Origin::Global);
        v.push(("global".into(), g));
        v
    } else {
        let s = Scope::load(cwd)?;
        vec![(tilde(&s.root), s)]
    };
    if json {
        let out: Vec<_> = scopes
            .iter()
            .map(|(_, s)| {
                serde_json::json!({
                    "root": s.root,
                    "file": config::project_file(&s.root),
                    "connections": rows(s),
                })
            })
            .collect();
        let out = if all {
            serde_json::Value::Array(out)
        } else {
            out.into_iter().next().unwrap()
        };
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(0);
    }
    let tty = std::io::stdout().is_terminal();
    let c = |code: &str, s: &str| {
        if tty {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    };
    for (i, (label, s)) in scopes.iter().enumerate() {
        let rows = rows(s);
        if all && rows.is_empty() {
            continue;
        }
        if i > 0 {
            println!();
        }
        println!("{}", c("1", label));
        if rows.is_empty() {
            println!(
                "  {}",
                c(
                    "2",
                    "no connections yet: `dbx add <name> <url>`, `dbx edit` or `dbx include <other clone>`"
                )
            );
        }
        let w = rows.iter().map(|r| r.name.len()).max().unwrap_or(0);
        let sw = rows.iter().map(|r| r.stage.len()).max().unwrap_or(0);
        for r in rows {
            let tunnel = match r.tunnel_up {
                Some(true) => c("32", "⇄"),
                Some(false) => c("2", "⇄"),
                None => " ".into(),
            };
            let from = if r.source == "this repo" || all {
                String::new()
            } else {
                c("2", &format!("  ({})", r.source))
            };
            let note = r
                .note
                .map(|n| c("2", &format!("  {n}")))
                .unwrap_or_default();
            println!(
                "  {tunnel} {}  {}  {}  {}{from}{note}",
                c("36", &format!("{:w$}", r.name)),
                c(stage_color(&r.stage), &format!("{:sw$}", r.stage)),
                c("2", &format!("{:10}", r.kind.unwrap_or("?"))),
                r.endpoint,
            );
        }
    }
    Ok(0)
}

fn pick(cwd: &Path) -> Result<i32> {
    if !std::io::stdout().is_terminal() {
        return list(cwd, false, false);
    }
    let mut tunnels = Vec::new();
    let mut message: Option<String> = None;
    loop {
        let scope = Scope::load(cwd);
        let rows = scope.as_ref().map(rows).unwrap_or_default();
        if let Err(e) = &scope {
            message = Some(format!("{e:#}"));
        }
        set_title("dbx");
        let label = scope.as_ref().map(|s| tilde(&s.root)).unwrap_or_default();
        match picker::run(&label, &rows, message.take())? {
            picker::Action::Quit => return Ok(0),
            picker::Action::Refresh => {}
            picker::Action::Edit => {
                edit(&store::target_file(cwd, false)?, false)?;
            }
            picker::Action::Open { name, write } => {
                let Ok(scope) = scope else { continue };
                if let Err(e) = open(&scope, &name, write, Some(&mut tunnels)) {
                    message = Some(format!("{e:#}"));
                }
            }
        }
    }
}

fn add(
    cwd: &Path,
    name: &str,
    url: &str,
    stage: Option<String>,
    tunnel: Option<String>,
    global: bool,
) -> Result<i32> {
    let draft = store::Draft {
        name: name.into(),
        url: url.into(),
        stage,
        tunnel,
        global,
        ..Default::default()
    };
    let saved = store::save(cwd, &draft, None)?;
    println!("added {name} to {}", tilde(&saved.file));
    for n in saved.notes {
        println!("{n}");
    }
    Ok(0)
}

fn include(cwd: &Path, path: &Path) -> Result<i32> {
    let other = common::scope::project_root(&cwd.join(path));
    let own = common::scope::project_root(cwd);
    if other == own {
        bail!("that is this repo");
    }
    let file = store::target_file(cwd, false)?;
    let mut doc = store::load_doc(&file)?;
    let list = doc
        .entry("include")
        .or_insert(toml_edit::value(toml_edit::Array::new()));
    let arr = list.as_array_mut().context("`include` is not a list")?;
    let entry = tilde(&other);
    if arr.iter().any(|v| v.as_str() == Some(&entry)) {
        println!("already including {entry}");
        return Ok(0);
    }
    arr.push(entry.as_str());
    store::save_doc(&file, &doc, false)?;
    let n = File::read(&config::project_file(&other))?.conn.len();
    println!("{} now includes {entry} ({n} connections)", tilde(&own));
    Ok(0)
}

fn copy(cwd: &Path, name: &str, from: &Path, rename: Option<&str>) -> Result<i32> {
    let source = Scope::load(&cwd.join(from))?;
    let e = source.get(name)?;
    let src_file = &source.layer(e).file;
    let src: DocumentMut = std::fs::read_to_string(src_file)?.parse()?;
    let item = src["conn"][name].clone();
    let new = rename.unwrap_or(name);
    let file = store::target_file(cwd, false)?;
    let mut doc = store::load_doc(&file)?;
    let conns = doc
        .entry("conn")
        .or_insert(toml_edit::table())
        .as_table_mut()
        .context("`conn` is not a table")?;
    conns.set_implicit(true);
    if conns.contains_key(new) {
        bail!("{new} already exists in {}", file.display());
    }
    conns.insert(new, item);
    store::save_doc(&file, &doc, false)?;
    println!("copied {name} from {} as {new}", tilde(src_file));
    Ok(0)
}

fn edit(file: &Path, global: bool) -> Result<i32> {
    if !file.exists() {
        store::write_private(file, &store::template(file, global))?;
    }
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".into());
    let status = tunnel::run_foreground(
        Command::new("sh")
            .arg("-c")
            .arg(format!("{editor} \"$1\""))
            .arg("sh")
            .arg(file),
    )?;
    if let Err(e) = File::read(file) {
        eprintln!("dbx: {e:#}");
        return Ok(1);
    }
    Ok(status.code().unwrap_or(0))
}
