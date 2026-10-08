use std::path::PathBuf;

use anyhow::{Result, bail};
use prognost::api::{self, Request};
use prognost::origin::{self, Scope};
use prognost::report::{self, Kind, Row};
use prognost::rev::Rev;
use prognost::{align, flow_tui, graph, html, repo, ts_extract, tui, workspace};

const USAGE: &str = "\
prognost — a prognosis for a code change: what it touches, how far it reaches, what looks risky

The diff is always taken from the merge base of --base (default: origin's
default branch) and --head (default: the working tree), like
`git diff base...head`.

  prognost graph [--base <rev>] [--head <rev>]
                                         every function the diff changed, its callers and callees, as a graph
                                         in the terminal (pan, zoom, expand, open a diff, mark files seen)
  prognost graph … --html <file>         the same graph as a self-contained web page (click lights call chains)
  prognost graph … --lod <pkg>=<n>       start a package at detail level n (0 functions … 3 one node)
  prognost serve [host:port] [--base <rev>] [--head <rev>] [--pr <number>]
                                         the web page over HTTP (default 127.0.0.1:7357), with seen marks and
                                         GitHub Viewed sync (--pr: default the open PR containing the head)
  prognost plan [--base <rev>] [--head <rev>] [--json] [--hops N]
                                         what the diff changes and how far it reaches, before it ships: the
                                         changed functions, their callers up to the entry points, the files
                                         (schema: the plan key of `prognost --schema`)
  prognost assess [<plan.json> | -] [--sarif <file>] [--json] [--fail-on high|medium|low]
                                         assess a plan's risks with the rules in force; reads `prognost plan --json`
                                         (stdin by default): `prognost plan --json | prognost assess -`;
                                         --sarif adds an analyser's results on added lines;
                                         --fail-on exits 1 when a finding is at least that severe
  prognost rules [--json]                the rules in force: presets plus the repository's prognost.toml

  prognost <file[:symbol]> [--base <rev>] [--head <rev>] [--text]
                                         one file: align its functions across the two revisions (TUI, or text)
  prognost map                           the older file-level dependency map (see `prognost map --help`)
  prognost --api '<JSON>' | --schema     one request in an envelope / the JSON schemas

  --color <auto|always|never>, --no-color
                                         colour in any command's output (default auto: on for a terminal; off when
                                         piped, or when NO_COLOR is set; CLICOLOR_FORCE=1 forces it on)

  Earlier spellings still work: `prognost --base <rev>` (= graph), `--dump <file>` (the graph as text),
  `--serve`, and `--impact [--json] [--hops N]` (superseded by plan).

In the one-file TUI: j/k move, tab jumps to the next change, a shows/hides
what didn't change, enter opens the selected node in hide, q quits.
";

fn out(body: &str) {
    use std::io::Write;
    let mut w = std::io::stdout().lock();
    let _ = writeln!(w, "{body}");
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `--color` / `--no-color` apply to every command, wherever they sit.
    let (choice, args) = prognost::color::take_option(args).map_err(|e| anyhow::anyhow!(e))?;
    prognost::color::init(choice);
    let first = args.first().map(String::as_str).unwrap_or("");

    match first {
        "-h" | "--help" => {
            print!("\n{}\n{USAGE}", prognost::color::logo());
            return Ok(());
        }
        "--schema" => {
            out(&serde_json::to_string_pretty(&api::schema_document())?);
            return Ok(());
        }
        "--api" => {
            let req: Request = serde_json::from_str(args.get(1).map(String::as_str).unwrap_or(""))?;
            let id = api::request_id(&req);
            let res = api::dispatch(req);
            out(&serde_json::to_string(&api::Envelope::wrap(id, res))?);
            return Ok(());
        }
        "map" => return run_map(&args[1..]),
        "plan" => return run_compare(&args[1..], Some(Stage::Plan)),
        "graph" => return run_compare(&args[1..], Some(Stage::Graph)),
        "serve" => {
            // `serve [host:port] …`: the address, if any, comes first.
            let mut rest: Vec<String> = Vec::new();
            let mut addr = "127.0.0.1:7357".to_string();
            for (i, a) in args[1..].iter().enumerate() {
                if i == 0 && !a.starts_with("--") {
                    addr = a.clone();
                } else {
                    rest.push(a.clone());
                }
            }
            rest.push("--serve".into());
            rest.push(addr);
            return run_compare(&rest, Some(Stage::Graph));
        }
        "assess" => return run_assess(&args[1..]),
        "rules" => return run_rules(&args[1..]),
        _ => {}
    }

    run_compare(&args, None)
}

/// `prognost rules [--json]`: the rules in force here — presets plus the
/// repository's config — or why the config is invalid.
fn run_rules(args: &[String]) -> Result<()> {
    let root = repo::root()?;
    let set = prognost::rules::RuleSet::load(&root)?;
    if args.iter().any(|a| a == "--json") {
        let rules: Vec<serde_json::Value> = set
            .rules
            .iter()
            .map(|r| {
                let mut v = serde_json::to_value(&r.config).unwrap_or_default();
                v["source"] = serde_json::Value::String(r.source.clone());
                v
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rules)?);
        return Ok(());
    }
    println!("{} {}", prognost::color::bold("config:"), prognost::seam::config_path(&root).display());
    for r in &set.rules {
        let c = &r.config;
        let kind = serde_json::to_value(c.kind).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
        let when = match c.kind {
            prognost::rules::Kind::Symbol => c.conditions.join(" && "),
            prognost::rules::Kind::Ast => c.query.clone().unwrap_or_default(),
            _ => c.pattern.clone().unwrap_or_default(),
        };
        use prognost::color as col;
        let sev = format!("{:<6}", c.severity.as_str());
        let sev = match c.severity {
            prognost::risk::Severity::High => col::bold_red(&sev),
            prognost::risk::Severity::Medium => col::bold_yellow(&sev),
            prognost::risk::Severity::Low => col::dim(&sev),
        };
        println!(
            "  {} {:<6} {} {} {}",
            col::bold(&format!("{:<36}", c.name)),
            kind,
            sev,
            col::cyan(&format!("{:<16}", r.source)),
            col::dim(&when.chars().take(70).collect::<String>())
        );
    }
    Ok(())
}

/// Which subcommand is running (`None`: the earlier flag-only spellings).
enum Stage {
    Plan,
    /// `graph` and `serve`: the whole diff as a graph.
    Graph,
}

struct AssessOptions {
    sarif: Vec<PathBuf>,
    fail_on: Option<prognost::risk::Severity>,
    json: bool,
    /// The plan to read: a file, or `-` for stdin.
    plan: Option<String>,
}

/// `--sarif` and `--fail-on` out of `args`; the rest returned.
fn split_assess_options(args: &[String]) -> Result<(AssessOptions, Vec<String>)> {
    let mut opts = AssessOptions { sarif: Vec::new(), fail_on: None, json: false, plan: None };
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--sarif" => opts
                .sarif
                .push(PathBuf::from(it.next().ok_or_else(|| anyhow::anyhow!("--sarif needs a file"))?)),
            "--fail-on" => {
                let v = it.next().ok_or_else(|| anyhow::anyhow!("--fail-on needs high, medium or low"))?;
                opts.fail_on = Some(match v.as_str() {
                    "high" => prognost::risk::Severity::High,
                    "medium" => prognost::risk::Severity::Medium,
                    "low" => prognost::risk::Severity::Low,
                    other => bail!("--fail-on {other}: expected high, medium or low"),
                });
            }
            _ => rest.push(a.clone()),
        }
    }
    Ok((opts, rest))
}

/// `prognost assess [<plan.json> | -] [--sarif <file>] [--json] [--fail-on <level>]`
fn run_assess(args: &[String]) -> Result<()> {
    let (mut opts, rest) = split_assess_options(args)?;
    for a in rest {
        match a.as_str() {
            "--json" => opts.json = true,
            s if !s.starts_with("--") || s == "-" => opts.plan = Some(a.clone()),
            other => bail!("assess: unknown option {other}"),
        }
    }
    let text = match opts.plan.as_deref() {
        None | Some("-") => {
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
            buf
        }
        Some(path) => std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("{path}: {e}"))?,
    };
    // The version first: an older plan has another shape altogether.
    let raw: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("not a prognost plan (prognost plan --json): {e}"))?;
    let version = raw.get("version").and_then(|v| v.as_u64()).unwrap_or(0);
    if version != u64::from(prognost::plan::PLAN_VERSION) {
        bail!(
            "plan version {version} — this prognost reads version {}; rerun prognost plan",
            prognost::plan::PLAN_VERSION
        );
    }
    let plan: prognost::plan::PlanReport = serde_json::from_value(raw)
        .map_err(|e| anyhow::anyhow!("not a prognost plan (prognost plan --json): {e}"))?;
    let root = repo::root()?;
    let result = prognost::assess::assess(&root, &plan, &opts.sarif)?;
    if opts.json {
        println!("{}", serde_json::to_string(&result)?);
    } else {
        print!("{}", result.to_text());
    }
    if opts.fail_on.is_some_and(|l| result.fails(l)) {
        std::process::exit(1);
    }
    Ok(())
}

fn run_compare(args: &[String], plan: Option<Stage>) -> Result<()> {
    let (file_symbol, base, head, as_text, dump, lods, impact, json, hops, html, serve, pr) =
        parse_compare_args(args)?;

    let root = repo::root()?;
    // Always the fork point, never the trunk's tip: see origin::merge_base.
    let base_sha = origin::merge_base(&root, base.as_deref(), head.as_deref())?;
    let base_rev = Rev::commit(base_sha.clone());
    let head = head.map(|h| origin::commit_sha(&root, &h)).transpose()?;
    let head_rev = match &head {
        Some(h) => Rev::commit(h.clone()),
        None => Rev::working(),
    };

    // No file: the whole diff is the entrypoint — every changed
    // function, across every changed file, as one graph.
    if let (Some(Stage::Graph | Stage::Plan), Some(f)) = (&plan, &file_symbol) {
        bail!("{f}: graph, serve and plan take the whole diff, not a file; for one file use `prognost {f}`");
    }
    let Some(file_symbol) = file_symbol else {
        if as_text {
            bail!("--text needs a file; the whole-diff view is TUI only");
        }
        if let Some(stage @ Stage::Plan) = &plan {
            let base_ws = workspace::discover(&root, &base_rev)?;
            let head_ws = workspace::discover(&root, &head_rev)?;
            let mut app = flow_tui::App::from_changes(root.clone(), base_rev, head_rev, base_ws, head_ws)?;
            let report = app.plan_report(hops)?;
            let _ = stage;
            if json {
                println!("{}", serde_json::to_string(&report)?);
            } else {
                print!("{}", report.to_text());
            }
            return Ok(());
        }
        let base_ws = workspace::discover(&root, &base_rev)?;
        let head_ws = workspace::discover(&root, &head_rev)?;
        let mut app = match flow_tui::App::from_changes(root.clone(), base_rev, head_rev, base_ws, head_ws) {
            Ok(app) => app,
            // A diff with no function-level change (constants, config,
            // markup): --impact still names the packages it touched.
            Err(e) if impact && e.to_string().starts_with("no changed functions") => {
                let base_rev = Rev::commit(base_sha.clone());
                let head_rev = match &head {
                    Some(h) => Rev::commit(h.clone()),
                    None => Rev::working(),
                };
                let ws = workspace::discover(&root, &head_rev)?;
                let files = origin::changed_files_between(&root, &base_rev, &head_rev)?;
                let mut by_pkg: std::collections::BTreeMap<String, usize> = Default::default();
                for f in &files {
                    let pkg = ws
                        .owning_package(f)
                        .and_then(|p| p.name.clone())
                        .unwrap_or_else(|| "(outside any package)".to_string());
                    *by_pkg.entry(pkg).or_default() += 1;
                }
                if json {
                    let report = prognost::impact::ImpactReport {
                        version: prognost::impact::IMPACT_VERSION,
                        base: base_sha.clone(),
                        head: head.clone(),
                        changed: Vec::new(),
                        chains: Vec::new(),
                        packages: prognost::impact::Packages {
                            changed: by_pkg
                                .iter()
                                .map(|(p, n)| prognost::impact::PackageChange {
                                    package: p.clone(),
                                    files: *n,
                                    functions: 0,
                                })
                                .collect(),
                            affected: Vec::new(),
                            crossings: Vec::new(),
                        },
                        truncated: false,
                        limits: prognost::impact::Limits { hops, nodes: 0 },
                    };
                    println!("{}", serde_json::to_string(&report)?);
                } else {
                    println!("changed (no function-level changes — data, config or markup only):");
                    for (p, n) in &by_pkg {
                        println!("  {p:<32} {n} file{}", if *n == 1 { "" } else { "s" });
                    }
                    println!("affected upstream: not derivable without a changed function");
                }
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        for (pkg, level) in &lods {
            app.set_lod(pkg, *level);
        }
        app.pr_number = pr;
        if impact {
            if json {
                let report = app.impact_report(hops);
                println!("{}", serde_json::to_string(&report)?);
            } else {
                print!("{}", app.impact(hops).to_text());
            }
            return Ok(());
        }
        if let Some(path) = dump {
            // The whole canvas as text, no terminal: for eyeballing a
            // big graph in a file, and for diffing two runs.
            std::fs::write(&path, app.dump_world())?;
            println!("wrote {path}");
            return Ok(());
        }
        if let Some(path) = html {
            // The page is static: follow callers a few hops up first,
            // so "who calls this" has an answer beyond the first hop.
            app.walk_upstream(hops);
            std::fs::write(&path, app.dump_html())?;
            println!("wrote {path}");
            return Ok(());
        }
        if let Some(addr) = serve {
            app.walk_upstream(hops);
            return serve_page(app, &addr);
        }
        let mut terminal = ratatui::init();
        let result = app.run(&mut terminal);
        ratatui::restore();
        return result;
    };
    let (file, symbol) = match file_symbol.rsplit_once(':') {
        Some((f, s)) if !s.is_empty() && !s.contains('/') => {
            (PathBuf::from(f), Some(s.to_string()))
        }
        _ => (PathBuf::from(&file_symbol), None),
    };

    // A file new in HEAD has no BASE text: everything in it is added.
    let base_src = base_rev.read(&root, &file).unwrap_or_default();
    let Some(head_src) = head_rev.read(&root, &file) else {
        bail!(
            "could not read {} at {} (head)",
            file.display(),
            head_rev.label()
        );
    };

    let base_fns = ts_extract::extract_for_path(&file, &base_src)?;
    let head_fns = ts_extract::extract_for_path(&file, &head_src)?;
    let alignment = align::align(&base_fns, &head_fns);
    let report = report::build(&base_fns, &head_fns, &alignment);

    if as_text {
        print_report(&file, symbol.as_deref(), &base_rev, &head_rev, &report);
        return Ok(());
    }

    let focus = symbol.unwrap_or_else(|| {
        head_fns
            .iter()
            .find(|f| f.exported)
            .or_else(|| head_fns.first())
            .map(align::label)
            .unwrap_or_default()
    });
    let base_ws = workspace::discover(&root, &base_rev)?;
    let head_ws = workspace::discover(&root, &head_rev)?;
    let Some(app) = flow_tui::App::new(flow_tui::Init {
        root,
        file: file.clone(),
        base_rev,
        head_rev,
        base_ws,
        head_ws,
        base_fns,
        head_fns,
        alignment,
        focus: focus.clone(),
    }) else {
        bail!("no function named {focus} in {}", file.display());
    };
    let mut terminal = ratatui::init();
    let result = app.run(&mut terminal);
    ratatui::restore();
    result
}

fn print_report(
    file: &std::path::Path,
    symbol: Option<&str>,
    base_rev: &Rev,
    head_rev: &Rev,
    report: &report::Report,
) {
    let entry = symbol
        .map(|s| format!("{}:{s}", file.display()))
        .unwrap_or_else(|| file.display().to_string());
    use prognost::color as col;
    out(&format!("{} {entry}", col::bold("flow:")));
    out(&format!("{} {}", col::bold("base:"), col::dim(&base_rev.label())));
    out(&format!("{} {}", col::bold("head:"), col::dim(&head_rev.label())));
    out("");
    out(&format!(
        "{} changed, {} unchanged (collapsed)",
        report.changed, report.unchanged
    ));
    out("");
    for row in report.rows.iter().filter(|r| r.kind != Kind::Unchanged) {
        print_row(row);
    }
}

fn print_row(row: &Row) {
    let indent = "  ".repeat(row.depth);
    use prognost::color as col;
    let marker = match row.kind {
        Kind::Added => col::green("+ added   "),
        Kind::Removed => col::red("- removed "),
        Kind::Changed => col::yellow("  matched "),
        Kind::Unchanged => "  matched ".to_string(),
    };
    let exported = if row.exported { "  (exported)" } else { "" };
    let range = match (row.base_range, row.head_range) {
        (Some(b), Some(h)) => format!("  base:{}-{} head:{}-{}", b.0, b.1, h.0, h.1),
        (Some(b), None) => format!("  base:{}-{}", b.0, b.1),
        (None, Some(h)) => format!("  head:{}-{}", h.0, h.1),
        (None, None) => String::new(),
    };
    out(&format!("{indent}{marker}{}{exported}{}", col::bold(&row.label), col::dim(&range)));
    for change in &row.details {
        match change {
            align::CallChange::Added(c) => out(&format!("{indent}    {}{c}", col::green("+ call added:   "))),
            align::CallChange::Removed(c) => out(&format!("{indent}    {}{c}", col::red("- call removed: "))),
        }
    }
}

/// `--serve`: the page over HTTP with a small API behind it, so seen
/// marks land in the shared store and Viewed reaches GitHub — things a
/// file:// page cannot do.
fn serve_page(mut app: flow_tui::App, addr: &str) -> Result<()> {
    let server = tiny_http::Server::http(addr)
        .map_err(|e| anyhow::anyhow!("cannot listen on {addr}: {e}"))?;
    println!("serving on http://{addr}/  (ctrl-c to stop)");
    let json_header = tiny_http::Header::from_bytes("Content-Type", "application/json; charset=utf-8").unwrap();
    let html_header = tiny_http::Header::from_bytes("Content-Type", "text/html; charset=utf-8").unwrap();
    for mut req in server.incoming_requests() {
        let url = req.url().to_string();
        let (path, query) = url.split_once('?').unwrap_or((&url, ""));
        let param = |key: &str| -> Option<String> {
            query.split('&').find_map(|kv| {
                let (k, v) = kv.split_once('=')?;
                (k == key).then(|| percent_decode(v))
            })
        };
        let json = |body: String| tiny_http::Response::from_string(body).with_header(json_header.clone());
        let _ = std::io::Read::read_to_end(&mut req.as_reader(), &mut Vec::new());
        let response = match (req.method().as_str(), path) {
            ("GET", "/") | ("GET", "/index.html") => {
                tiny_http::Response::from_string(app.render_html(true)).with_header(html_header.clone())
            }
            ("GET", "/api/seen") => json(app.seen_json()),
            ("POST", "/api/seen/toggle") => {
                if let Some(p) = param("path") {
                    app.toggle_seen_path(&p);
                }
                json(app.seen_json())
            }
            ("POST", "/api/viewed/push") => json(match app.push_viewed() {
                Ok((m, t, n)) => format!("{{\"marked\":{m},\"total\":{t},\"pr\":{n}}}"),
                Err(e) => format!("{{\"error\":{}}}", html::json_str(&e)),
            }),
            ("POST", "/api/viewed/pull") => json(match app.pull_viewed() {
                Ok((i, v, n)) => format!("{{\"imported\":{i},\"viewed\":{v},\"pr\":{n}}}"),
                Err(e) => format!("{{\"error\":{}}}", html::json_str(&e)),
            }),
            _ => tiny_http::Response::from_string("not found").with_status_code(404),
        };
        let _ = req.respond(response);
    }
    Ok(())
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

type CompareArgs = (
    Option<String>,
    Option<String>,
    Option<String>,
    bool,
    Option<String>,
    Vec<(String, u8)>,
    bool,
    bool,
    usize,
    Option<String>,
    Option<String>,
    Option<u64>,
);

fn parse_compare_args(args: &[String]) -> Result<CompareArgs> {
    let mut file_symbol = None;
    let mut base = None;
    let mut head = None;
    let mut as_text = false;
    let mut dump = None;
    let mut lods: Vec<(String, u8)> = Vec::new();
    let mut impact = false;
    let mut json = false;
    let mut hops = 12usize;
    let mut html = None;
    let mut serve = None;
    let mut pr = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--base" => {
                base = Some(
                    args.get(i + 1)
                        .ok_or_else(|| anyhow::anyhow!("--base needs a value"))?
                        .clone(),
                );
                i += 2;
            }
            "--head" => {
                head = Some(
                    args.get(i + 1)
                        .ok_or_else(|| anyhow::anyhow!("--head needs a value"))?
                        .clone(),
                );
                i += 2;
            }
            "--text" => {
                as_text = true;
                i += 1;
            }
            "--impact" => {
                impact = true;
                i += 1;
            }
            "--json" => {
                json = true;
                i += 1;
            }
            "--hops" => {
                hops = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow::anyhow!("--hops needs a number"))?
                    .parse()?;
                i += 2;
            }
            "--lod" => {
                let spec = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow::anyhow!("--lod needs <package>=<0-3>"))?;
                let (pkg, level) = spec
                    .rsplit_once('=')
                    .ok_or_else(|| anyhow::anyhow!("--lod needs <package>=<0-3>, got {spec}"))?;
                lods.push((pkg.to_string(), level.parse()?));
                i += 2;
            }
            "--serve" => {
                // An address is optional: `--serve` alone picks one.
                let next = args.get(i + 1).filter(|a| !a.starts_with("--") && a.contains(':'));
                serve = Some(next.cloned().unwrap_or_else(|| "127.0.0.1:7357".to_string()));
                i += if next.is_some() { 2 } else { 1 };
            }
            "--pr" => {
                pr = Some(
                    args.get(i + 1)
                        .ok_or_else(|| anyhow::anyhow!("--pr needs a number"))?
                        .parse()?,
                );
                i += 2;
            }
            "--html" => {
                html = Some(
                    args.get(i + 1)
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("--html needs a file path"))?,
                );
                i += 2;
            }
            "--dump" => {
                dump = Some(
                    args.get(i + 1)
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("--dump needs a file path"))?,
                );
                i += 2;
            }
            other => {
                file_symbol = Some(other.to_string());
                i += 1;
            }
        }
    }
    Ok((file_symbol, base, head, as_text, dump, lods, impact, json, hops, html, serve, pr))
}

fn run_map(args: &[String]) -> Result<()> {
    let (scope, path) = parse_launch_args(args)?;
    let root = repo::root()?;
    let ws = workspace::discover(&root, &Rev::working())?;
    let files = match path {
        Some(p) => vec![origin::parse_file_arg(&p).0],
        None => origin::changed_files(&root, scope.unwrap_or_default())?,
    };
    if files.is_empty() {
        bail!("nothing changed under this scope — try --scope branch, or pass a file");
    }
    let starting_graph = graph::origin(&root, &files, &ws);

    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture);
    let app = tui::App::new(root, ws, starting_graph);
    let result = app.run(&mut terminal);
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
    ratatui::restore();
    result
}

fn parse_launch_args(args: &[String]) -> Result<(Option<Scope>, Option<String>)> {
    let mut scope = None;
    let mut path = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--scope" => {
                let s = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow::anyhow!("--scope needs a value"))?;
                scope = Some(match s.as_str() {
                    "uncommitted" => Scope::Uncommitted,
                    "staged" => Scope::Staged,
                    "branch" => Scope::Branch,
                    other => bail!("unknown scope: {other} (uncommitted, staged, branch)"),
                });
                i += 2;
            }
            other => {
                path = Some(other.to_string());
                i += 1;
            }
        }
    }
    Ok((scope, path))
}
