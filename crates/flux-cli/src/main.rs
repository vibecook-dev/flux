//! `flux`: the command line for a running flux server, for people and for
//! their agents (docs/AGENT-NATIVE.md). Every command is one of the
//! server's operations (`POST /api/v1/ops/{name}`); `flux call` runs any of
//! them with JSON, and `flux mcp` carries MCP over stdio to the server's
//! `/mcp`. Output is JSON (indented on a terminal); errors are JSON on
//! stderr, with an exit code per kind.

mod args;
mod client;
mod mcp;
mod skills;

use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use base64::Engine as _;
use clap::{Parser, Subcommand};
use serde_json::{json, Map, Value};

use client::{Client, Failure};

#[derive(Debug, Parser)]
#[command(
    name = "flux",
    version,
    about = "The flux media server from the command line: for you and your agents",
    long_about = "The flux media server from the command line: for you and your agents.\n\n\
        Every command runs one of the server's operations and prints JSON. It finds a server on this \
        machine by itself; pass --server for another (https://flux.tail1234.ts.net:8096). Agents: \
        see `flux skills`, and docs/AGENT-NATIVE.md.",
    after_help = "Examples:\n  flux status\n  flux gaps\n  flux find --gap identity --limit 20\n  flux get 42\n  \
        flux edit 42 title=\"Spirited Away\" year=2001 directors=\"Hayao Miyazaki\" \\\n      \
        --source https://en.wikipedia.org/wiki/Spirited_Away --summary \"Identified from the file name\"\n  \
        flux art add 42 poster https://example.org/p.jpg --credit \"Studio Ghibli\" --from https://example.org\n  \
        flux changes undo 17\n  flux lens add board.json\n  flux call items.find '{\"gap\": \"year\"}'\n\n\
        Exit codes: 0 ok, 1 error, 2 usage, 3 not found, 4 conflict, 5 forbidden, 6 server unreachable."
)]
struct Cli {
    /// The server [default: FLUX_SERVER; the one this machine's data dir
    /// names (server.json); http://127.0.0.1:8096].
    #[arg(long, global = true, env = "FLUX_SERVER", value_name = "URL")]
    server: Option<String>,
    /// flux's data dir, where a running server writes server.json.
    #[arg(long, global = true, env = "FLUX_DATA_DIR", value_name = "DIR")]
    data_dir: Option<PathBuf>,
    /// The agent you are, credited with your changes [default: FLUX_AGENT,
    /// or the agent this runs under: Claude Code].
    #[arg(long, global = true, env = "FLUX_AGENT", value_name = "NAME")]
    agent: Option<String>,
    /// You are a person, not an agent: changes are yours, and the server
    /// withholds nothing.
    #[arg(long, global = true)]
    human: bool,
    /// Indent the JSON (the default on a terminal).
    #[arg(long, global = true)]
    pretty: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// The server, its libraries, who you are, what you may do, the rules.
    Status,
    /// What's missing, per library.
    Gaps {
        /// One library's id.
        library: Option<i64>,
    },
    /// Find items: by library, kind, gap, match state, or text.
    Find {
        #[arg(long)]
        library: Option<i64>,
        /// movie, show, season, episode or extra [default: top-level items].
        #[arg(long)]
        kind: Option<String>,
        /// identity, year, overview, poster, backdrop, episode_titles or offline.
        #[arg(long)]
        gap: Option<String>,
        /// unmatched, parsed, matched or locked.
        #[arg(long = "match", value_name = "STATE")]
        match_state: Option<String>,
        /// Items below this one (a show's seasons and episodes, …).
        #[arg(long)]
        parent: Option<i64>,
        /// Text in the title or a file's path.
        #[arg(short, long = "query", value_name = "TEXT")]
        q: Option<String>,
        /// Items per page (1–200) [default: 50].
        #[arg(long)]
        limit: Option<u32>,
        /// `next` of the previous page.
        #[arg(long)]
        cursor: Option<String>,
        /// Every page, one list (at most 5,000 items).
        #[arg(long)]
        all: bool,
    },
    /// One item: its fields and their sources, its files' evidence, its art.
    Get { item: i64 },
    /// Set or clear fields of an item, with sources: one changeset.
    #[command(
        after_help = "Values: FIELD=TEXT (lists comma-separated: genres=\"Animation, Fantasy\"; \
        cast=\"Rumi Hiiragi as Chihiro; Miyu Irino as Haku\"), FIELD:=JSON (raw), ids.PROVIDER=ID.\n\
        Fields: title original_title year overview tagline genres tags release_date air_date languages \
        countries directors writers cast studios content_rating links ids.*"
    )]
    Edit {
        /// The item (leave out with --file).
        item: Option<i64>,
        /// FIELD=VALUE or FIELD:=JSON.
        values: Vec<String>,
        /// Hand a field back to flux (repeatable).
        #[arg(long, value_name = "FIELD")]
        clear: Vec<String>,
        /// A page that supports the values (repeatable).
        #[arg(long, value_name = "URL")]
        source: Vec<String>,
        /// What and why, for the person reviewing it.
        #[arg(long)]
        summary: Option<String>,
        /// Propose it instead of applying it.
        #[arg(long)]
        propose: bool,
        /// Also replace what a person set (only when they asked you to).
        #[arg(long)]
        force: bool,
        /// Show what would change; change nothing.
        #[arg(long)]
        dry_run: bool,
        /// A whole items.edit input (JSON; `-` for stdin), for many items.
        #[arg(long, value_name = "FILE")]
        file: Option<PathBuf>,
    },
    /// Say what a file is when flux read its name wrong: one changeset.
    #[command(
        after_help = "Fix: kind=movie|episode|extra|ignore of=ITEM title=TEXT year=N season=N \
        episodes=1,2 extra=trailer|featurette|behind_the_scenes|deleted_scene|interview|short|scene|other \
        edition=TEXT part=N. Give only what's wrong.\n\
        Examples:\n  flux fix 812 of=40 --summary \"Another copy of Heat\"\n  flux fix 813 season=2 episodes=5\n  \
        flux fix 814 kind=extra extra=trailer of=40\n  flux fix 815 --clear"
    )]
    Fix {
        /// The file (`files[].id` in `flux get`).
        file: i64,
        /// KEY=VALUE.
        values: Vec<String>,
        /// Clear the file's fix: its name decides again.
        #[arg(long)]
        clear: bool,
        /// A page that supports it (repeatable).
        #[arg(long, value_name = "URL")]
        source: Vec<String>,
        #[arg(long)]
        summary: Option<String>,
        #[arg(long)]
        propose: bool,
        /// Show where it would go; change nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Films (or shows) that are one work: the files of FROM join INTO.
    Merge {
        into: i64,
        #[arg(required = true)]
        from: Vec<i64>,
        #[arg(long)]
        summary: Option<String>,
        #[arg(long)]
        propose: bool,
        #[arg(long)]
        dry_run: bool,
    },
    /// Plan naming items' files the standard way, or moving files where you
    /// say: a proposal; nothing moves until it's applied (flux changes apply).
    #[command(
        after_help = "Examples:\n  flux plan 40 41 --summary \"Name the Ghibli films' files\"\n  \
        flux plan --move \"812=Kids/Spirited Away (2001)/Spirited Away (2001).mkv\"\n  flux changes apply 23"
    )]
    Plan {
        /// Films, shows, seasons, episodes or extras to name.
        items: Vec<i64>,
        /// FILE=PATH: move a file to PATH (relative to its library folder).
        #[arg(long = "move", value_name = "FILE=PATH")]
        moves: Vec<String>,
        /// Put ids in folder names ([imdbid-tt…]).
        #[arg(long)]
        ids: bool,
        #[arg(long)]
        summary: Option<String>,
    },
    /// Images: look at one, add one, choose one.
    Art {
        #[command(subcommand)]
        cmd: ArtCmd,
    },
    /// Changesets: list them, show one, undo, apply or reject.
    Changes {
        #[command(subcommand)]
        cmd: Option<ChangesCmd>,
        /// proposed, applied, reverted or rejected.
        #[arg(long)]
        state: Option<String>,
        /// Only those changing this item.
        #[arg(long)]
        item: Option<i64>,
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Lenses kept on the server.
    Lens {
        #[command(subcommand)]
        cmd: LensCmd,
    },
    /// Scan a library now.
    Scan { library: i64 },
    /// Follow the server's events, one JSON object a line (Ctrl-C to stop).
    Events,
    /// Every operation the server offers.
    Ops {
        /// The whole manifest, with every schema.
        #[arg(long)]
        full: bool,
    },
    /// One JSON Schema: lens, fields, item, changeset, or OP.input / OP.output.
    Schema { name: String },
    /// Run any operation with a JSON input (an argument, or `-` for stdin).
    Call { op: String, input: Option<String> },
    /// MCP over stdio, carried to the server's /mcp (for agents that start servers).
    Mcp,
    /// flux's skills for agents: list, show, install.
    Skills {
        #[command(subcommand)]
        cmd: Option<SkillsCmd>,
    },
}

#[derive(Debug, Subcommand)]
enum ArtCmd {
    /// Save an image to a file, to look at it.
    View {
        art: i64,
        /// The file [default: art-ID.jpg here].
        #[arg(short, long)]
        out: Option<PathBuf>,
        /// Scale down to this width (0: full size) [default: 480].
        #[arg(long)]
        width: Option<u32>,
    },
    /// Add an image (from a URL or a file) and show it.
    Add {
        item: i64,
        /// poster, backdrop, thumb or logo.
        kind: String,
        /// The image's address (flux fetches it).
        url: Option<String>,
        /// The image from a file instead.
        #[arg(long)]
        file: Option<PathBuf>,
        /// Who made it (needed for a found image).
        #[arg(long)]
        credit: Option<String>,
        /// The page you found it on.
        #[arg(long, value_name = "URL")]
        from: Option<String>,
        /// You made it, without AI.
        #[arg(long, conflicts_with = "generated")]
        made: bool,
        /// An AI model made it (labelled AI-generated).
        #[arg(long, value_name = "MODEL")]
        generated: Option<String>,
        /// Add it without showing it.
        #[arg(long)]
        no_pin: bool,
        #[arg(long)]
        summary: Option<String>,
    },
    /// Show image ART, or with --item, --kind and --auto let flux choose.
    Pin {
        art: Option<i64>,
        #[arg(long)]
        item: Option<i64>,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        auto: bool,
    },
}

#[derive(Debug, Subcommand)]
enum ChangesCmd {
    /// Newest first.
    List,
    /// One changeset, change by change.
    Show { id: i64 },
    /// Revert an applied changeset.
    Undo { id: i64 },
    /// Apply a proposal.
    Apply { id: i64 },
    /// Turn a proposal down.
    Reject { id: i64 },
}

#[derive(Debug, Subcommand)]
enum LensCmd {
    /// Yours and everyone's.
    List,
    /// Check a lens file (`-` for stdin): errors, and what the browser removes.
    Check { file: PathBuf },
    /// Keep a lens on the server; it shows on every device.
    Add {
        file: PathBuf,
        /// For everyone (Owner or Admin).
        #[arg(long)]
        everyone: bool,
    },
    /// Forget a lens.
    Remove {
        id: String,
        #[arg(long)]
        everyone: bool,
    },
}

#[derive(Debug, Subcommand)]
enum SkillsCmd {
    /// The skills and what each is for.
    List,
    /// Print a skill's SKILL.md.
    Show { name: String },
    /// Copy them where your agent finds skills.
    Install {
        /// [default: ~/.claude/skills]
        #[arg(long)]
        dir: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let pretty = cli.pretty || std::io::stdout().is_terminal();
    match run(&cli) {
        Ok(Some(v)) => {
            print_json(&v, pretty);
            ExitCode::SUCCESS
        }
        Ok(None) => ExitCode::SUCCESS,
        Err(f) => {
            let e = json!({ "error": { "code": f.code, "message": f.message } }).to_string();
            eprintln!("{}", for_pipe(e, std::io::stderr().is_terminal()));
            ExitCode::from(f.exit)
        }
    }
}

fn print_json(v: &Value, pretty: bool) {
    let s = if pretty {
        serde_json::to_string_pretty(v)
    } else {
        serde_json::to_string(v)
    };
    println!(
        "{}",
        for_pipe(s.unwrap_or_default(), std::io::stdout().is_terminal())
    );
}

/// JSON as it should leave through a pipe. On Windows, PowerShell (5.1)
/// decodes a program's piped output with the console's legacy code page,
/// which garbles UTF-8 (a CJK title) and breaks the JSON; written as `\u`
/// escapes, every character survives any decoding. A console gets UTF-16
/// from Rust, and other systems pipe UTF-8 untouched.
fn for_pipe(json: String, terminal: bool) -> String {
    if cfg!(windows) && !terminal {
        ascii_json(&json)
    } else {
        json
    }
}

/// `json` with every non-ASCII character as a JSON `\u` escape (still the
/// same JSON: outside strings JSON is ASCII).
fn ascii_json(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    for c in json.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut units = [0u16; 2];
            for u in c.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{u:04x}"));
            }
        }
    }
    out
}

/// The agent this runs as: the flag or FLUX_AGENT, else one detected from
/// the environment it runs in; none with --human.
fn agent(cli: &Cli) -> Option<String> {
    if cli.human {
        return None;
    }
    cli.agent
        .clone()
        .filter(|a| !a.trim().is_empty())
        .or_else(|| std::env::var_os("CLAUDECODE").map(|_| "claude-code".to_string()))
}

fn client(cli: &Cli) -> Client {
    Client::new(
        client::server_url(cli.server.as_deref(), cli.data_dir.as_deref()),
        agent(cli),
    )
}

fn read_input(arg: &Path) -> Result<String, Failure> {
    if arg == Path::new("-") {
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .map_err(|e| Failure::usage(format!("reading stdin: {e}")))?;
        Ok(s)
    } else {
        std::fs::read_to_string(arg).map_err(|e| Failure::usage(format!("reading {}: {e}", arg.display())))
    }
}

fn parse_json(text: &str, what: &str) -> Result<Value, Failure> {
    serde_json::from_str(text).map_err(|e| Failure::usage(format!("{what} isn't JSON: {e}")))
}

fn put(m: &mut Map<String, Value>, k: &str, v: Option<Value>) {
    if let Some(v) = v {
        m.insert(k.into(), v);
    }
}

fn run(cli: &Cli) -> Result<Option<Value>, Failure> {
    let c = client(cli);
    let out = match &cli.cmd {
        Cmd::Status => c.op("status", json!({}))?,
        Cmd::Gaps { library } => c.op("library.gaps", json!({ "library": library }))?,
        Cmd::Find {
            library,
            kind,
            gap,
            match_state,
            parent,
            q,
            limit,
            cursor,
            all,
        } => {
            let mut input = Map::new();
            put(&mut input, "library", library.map(Value::from));
            put(&mut input, "kind", kind.clone().map(Value::from));
            put(&mut input, "gap", gap.clone().map(Value::from));
            put(&mut input, "match_state", match_state.clone().map(Value::from));
            put(&mut input, "parent", parent.map(Value::from));
            put(&mut input, "q", q.clone().map(Value::from));
            put(&mut input, "limit", limit.map(Value::from));
            put(&mut input, "cursor", cursor.clone().map(Value::from));
            if *all {
                input.insert("limit".into(), json!(200));
                let mut items = Vec::new();
                let total = loop {
                    let page = c.op("items.find", Value::Object(input.clone()))?;
                    items.extend(page["items"].as_array().cloned().unwrap_or_default());
                    match page["next"].as_str() {
                        Some(n) if items.len() < 5000 => {
                            input.insert("cursor".into(), json!(n));
                        }
                        _ => break page["total"].clone(),
                    }
                };
                json!({ "items": items, "total": total })
            } else {
                c.op("items.find", Value::Object(input))?
            }
        }
        Cmd::Get { item } => c.op("item.get", json!({ "item": item }))?,
        Cmd::Fix {
            file,
            values,
            clear,
            source,
            summary,
            propose,
            dry_run,
        } => {
            let fix = if *clear {
                if !values.is_empty() {
                    return Err(Failure::usage("--clear takes no values"));
                }
                Value::Null
            } else {
                let f = args::fix(values).map_err(Failure::usage)?;
                if f.is_empty() {
                    return Err(Failure::usage(
                        "say what's wrong: KEY=VALUE (of=ITEM, season=2 episodes=5, …), or --clear",
                    ));
                }
                Value::Object(f)
            };
            let summary = summary.clone().unwrap_or_else(|| {
                if *clear {
                    format!("Let file {file}'s name decide again")
                } else {
                    format!("Fixed what file {file} is")
                }
            });
            c.op(
                "files.fix",
                json!({
                    "summary": summary,
                    "fixes": [{ "file": file, "fix": fix, "sources": source }],
                    "mode": if *propose { "propose" } else { "apply" },
                    "dry_run": dry_run,
                }),
            )?
        }
        Cmd::Merge {
            into,
            from,
            summary,
            propose,
            dry_run,
        } => {
            let summary = summary.clone().unwrap_or_else(|| {
                let list: Vec<String> = from.iter().map(|f| f.to_string()).collect();
                format!("Merged {} into {into}: one work", list.join(", "))
            });
            c.op(
                "items.merge",
                json!({
                    "summary": summary, "into": into, "from": from,
                    "mode": if *propose { "propose" } else { "apply" },
                    "dry_run": dry_run,
                }),
            )?
        }
        Cmd::Plan {
            items,
            moves,
            ids,
            summary,
        } => {
            let mut list = Vec::new();
            for m in moves {
                let (file, to) = m
                    .split_once('=')
                    .ok_or_else(|| Failure::usage(format!("--move {m:?}: give FILE=PATH")))?;
                let file: i64 = file
                    .trim()
                    .parse()
                    .map_err(|_| Failure::usage(format!("--move {m:?}: FILE is a file id")))?;
                list.push(json!({ "file": file, "to": to }));
            }
            if items.is_empty() && list.is_empty() {
                return Err(Failure::usage(
                    "which items? flux plan ITEM… (or --move FILE=PATH)",
                ));
            }
            let summary = summary
                .clone()
                .unwrap_or_else(|| match (items.len(), list.len()) {
                    (0, n) => format!("Move {n} file{}", if n == 1 { "" } else { "s" }),
                    (n, _) => format!(
                        "Name {n} item{}'s files the standard way",
                        if n == 1 { "" } else { "s" }
                    ),
                });
            c.op(
                "files.plan",
                json!({ "summary": summary, "items": items, "moves": list, "ids_in_names": ids }),
            )?
        }
        Cmd::Edit {
            item,
            values,
            clear,
            source,
            summary,
            propose,
            force,
            dry_run,
            file,
        } => {
            let mut input = match file {
                Some(f) => {
                    if item.is_some() || !values.is_empty() || !clear.is_empty() {
                        return Err(Failure::usage(
                            "--file holds the whole edit: leave out the item and its values",
                        ));
                    }
                    parse_json(&read_input(f)?, "the edit")?
                }
                None => {
                    let item =
                        item.ok_or_else(|| Failure::usage("which item? flux edit ITEM FIELD=VALUE…"))?;
                    let set = args::assignments(values).map_err(Failure::usage)?;
                    if set.is_empty() && clear.is_empty() {
                        return Err(Failure::usage(
                            "nothing to set or clear: give FIELD=VALUE or --clear FIELD",
                        ));
                    }
                    let summary = summary
                        .clone()
                        .unwrap_or_else(|| args::default_summary(&set, clear));
                    json!({
                        "summary": summary,
                        "edits": [{ "item": item, "set": set, "clear": clear, "sources": source }],
                    })
                }
            };
            let o = input
                .as_object_mut()
                .ok_or_else(|| Failure::usage("the edit is a JSON object"))?;
            if *propose {
                o.insert("mode".into(), json!("propose"));
            }
            if *force {
                o.insert("force".into(), json!(true));
            }
            if *dry_run {
                o.insert("dry_run".into(), json!(true));
            }
            if let (Some(s), Some(_)) = (summary, file) {
                o.insert("summary".into(), json!(s));
            }
            c.op("items.edit", input)?
        }
        Cmd::Art { cmd } => art(&c, cmd)?,
        Cmd::Changes {
            cmd,
            state,
            item,
            limit,
        } => match cmd {
            None | Some(ChangesCmd::List) => c.op(
                "changes.list",
                json!({ "state": state, "item": item, "limit": limit }),
            )?,
            Some(ChangesCmd::Show { id }) => c.op("changes.get", json!({ "changeset": id }))?,
            Some(ChangesCmd::Undo { id }) => c.op("changes.undo", json!({ "changeset": id }))?,
            Some(ChangesCmd::Apply { id }) => c.op("changes.apply", json!({ "changeset": id }))?,
            Some(ChangesCmd::Reject { id }) => c.op("changes.reject", json!({ "changeset": id }))?,
        },
        Cmd::Lens { cmd } => match cmd {
            LensCmd::List => c.op("lenses.list", json!({}))?,
            LensCmd::Check { file } => {
                let spec = parse_json(&read_input(file)?, "the lens")?;
                c.op("lens.check", json!({ "spec": spec }))?
            }
            LensCmd::Add { file, everyone } => {
                let spec = parse_json(&read_input(file)?, "the lens")?;
                c.op("lens.add", json!({ "spec": spec, "everyone": everyone }))?
            }
            LensCmd::Remove { id, everyone } => {
                c.op("lens.remove", json!({ "id": id, "everyone": everyone }))?
            }
        },
        Cmd::Scan { library } => c.op("library.scan", json!({ "library": library }))?,
        Cmd::Events => {
            c.events()?;
            return Ok(None);
        }
        Cmd::Ops { full } => {
            let m = c.get_json("/api/v1/agent")?;
            if *full {
                m
            } else {
                let ops: Vec<Value> = m["ops"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|o| {
                        json!({
                            "name": o["name"], "title": o["title"], "effect": o["effect"], "who": o["who"],
                            "description": o["description"],
                        })
                    })
                    .collect();
                json!({
                    "contract": m["contract"],
                    "ops": ops,
                    "schemas": "flux schema OP.input | OP.output | lens | fields | item | changeset",
                })
            }
        }
        Cmd::Schema { name } => c.get_json(&format!("/api/v1/schemas/{name}"))?,
        Cmd::Call { op, input } => {
            let input = match input.as_deref() {
                None => json!({}),
                Some("-") => parse_json(&read_input(Path::new("-"))?, "the input")?,
                Some(s) => parse_json(s, "the input")?,
            };
            c.op(op, input)?
        }
        Cmd::Mcp => {
            mcp::bridge(&c)?;
            return Ok(None);
        }
        Cmd::Skills { cmd } => match cmd {
            None | Some(SkillsCmd::List) => skills::list(),
            Some(SkillsCmd::Show { name }) => {
                print!(
                    "{}",
                    skills::show(name).ok_or_else(|| Failure::not_found(format!("no skill {name:?}")))?
                );
                return Ok(None);
            }
            Some(SkillsCmd::Install { dir }) => skills::install(dir.as_deref())?,
        },
    };
    Ok(Some(out))
}

fn art(c: &Client, cmd: &ArtCmd) -> Result<Value, Failure> {
    Ok(match cmd {
        ArtCmd::View { art, out, width } => {
            let v = c.op("art.view", json!({ "art": art, "width": width.unwrap_or(480) }))?;
            let mime = v["mime"].as_str().unwrap_or("image/jpeg").to_string();
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(v["data_base64"].as_str().unwrap_or_default())
                .map_err(|e| Failure::error(format!("the server sent an image that isn't base64: {e}")))?;
            let ext = match mime.as_str() {
                "image/png" => "png",
                "image/webp" => "webp",
                _ => "jpg",
            };
            let path = out
                .clone()
                .unwrap_or_else(|| PathBuf::from(format!("art-{art}.{ext}")));
            std::fs::write(&path, &bytes)
                .map_err(|e| Failure::error(format!("writing {}: {e}", path.display())))?;
            json!({ "art": art, "mime": mime, "file": path.display().to_string(), "bytes": bytes.len() })
        }
        ArtCmd::Add {
            item,
            kind,
            url,
            file,
            credit,
            from,
            made,
            generated,
            no_pin,
            summary,
        } => {
            let mut input = json!({
                "item": item,
                "kind": kind,
                "origin": if generated.is_some() { "generated" } else if *made { "made" } else { "found" },
                "credit": credit,
                "found_at": from,
                "generator": generated,
                "pin": !no_pin,
                "summary": summary,
            });
            match (url, file) {
                (Some(u), None) => input["url"] = json!(u),
                (None, Some(f)) => {
                    let bytes = std::fs::read(f)
                        .map_err(|e| Failure::usage(format!("reading {}: {e}", f.display())))?;
                    input["data_base64"] = json!(base64::engine::general_purpose::STANDARD.encode(bytes));
                }
                _ => return Err(Failure::usage("give the image's URL, or --file PATH")),
            }
            c.op("art.add", input)?
        }
        ArtCmd::Pin {
            art,
            item,
            kind,
            auto,
        } => match (art, auto) {
            (Some(a), false) => c.op("art.pin", json!({ "art": a }))?,
            (None, true) => c.op("art.pin", json!({ "item": item, "kind": kind }))?,
            _ => {
                return Err(Failure::usage(
                    "flux art pin ART, or flux art pin --item ITEM --kind KIND --auto",
                ))
            }
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_json_is_the_same_json() {
        let v = json!({ "title": "拾荒者 · Café 🎬", "n": 1 });
        let a = ascii_json(&v.to_string());
        assert!(a.is_ascii(), "{a}");
        assert!(a.contains("\\u62fe"));
        assert_eq!(serde_json::from_str::<Value>(&a).unwrap(), v);
    }
}
