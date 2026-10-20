use clap::Parser;
use review_run::conventions::{Conventions, Mode};
use review_run::graph::{Graph, Kind, Node, ProviderKind};
use review_run::provider::{
    cache_env, compose_system_prompt, Claude, Exec, Invocation, NoGh, Provider,
};
use review_run::reuse::Previous;
use review_run::reviewed::{self, Review};
use review_run::run::{run, NodeRecord, Options, Providers, Target};
use review_run::schema::{critic_schema, generator_schema, mechanical_schema, merger_schema};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

/// Review a change by running a graph of reviewer processes over its checkout.
///
/// Exits 0 when every node ran, 1 when the report is incomplete because a
/// node failed, 2 when the run could not start or its bundle could not be
/// written, 3 when `--if-changed` found this change already reviewed.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// A graph file, or the name of one under $REVIEW_RUN_GRAPHS.
    #[arg(long, default_value = "default")]
    graph: String,
    /// The checkout to review. It must have the reviewed code checked out.
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    /// The commit the change is measured from, usually the merge base.
    #[arg(long)]
    base: String,
    /// The reviewed commit. Without it the working tree is reviewed,
    /// uncommitted changes included.
    #[arg(long)]
    head: Option<String>,
    /// The pull request's number, for a checkout that is not on its branch
    /// (a detached historic commit). Without it the checked-out branch's pull
    /// request is looked up, then $REVIEW_PR_NUMBER.
    #[arg(long, value_name = "NUMBER")]
    pr: Option<u64>,
    /// Where the bundle is written. Without it the bundle goes under
    /// $REVIEW_RUN_CAPTURE_DIR/<run id> to keep it, else a temporary
    /// directory that the system may clear.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Leave a node out; repeatable.
    #[arg(long, value_name = "NODE")]
    skip: Vec<String>,
    /// Run a node on another model; repeatable.
    #[arg(long, value_name = "NODE=MODEL")]
    model: Vec<String>,
    /// Run a node at another effort; repeatable.
    #[arg(long, value_name = "NODE=LEVEL")]
    effort: Vec<String>,
    /// Take the nodes whose inputs are unchanged from this earlier bundle of
    /// the same change instead of running them.
    #[arg(long, value_name = "BUNDLE_DIR", conflicts_with = "resume")]
    reuse_from: Option<PathBuf>,
    /// Finish an interrupted or partly failed run in its own bundle: nodes
    /// that finished ok stay, the rest run now. Needs the graph, `--model`
    /// and `--effort` of the original run.
    #[arg(long, value_name = "BUNDLE_DIR", conflicts_with = "out")]
    resume: Option<PathBuf>,
    /// Leave prewarm nodes out and run their forked generators as plain
    /// generators, with the lens files back in the system prompt.
    #[arg(long)]
    no_prewarm: bool,
    /// Cut every node off from GitHub, after this tool's own pull request
    /// lookup: no `GH_TOKEN`, `GITHUB_TOKEN`, `GH_ENTERPRISE_TOKEN` or
    /// `GITHUB_ENTERPRISE_TOKEN`, an empty `GH_CONFIG_DIR`, and a `gh` first on
    /// `PATH` that always fails. Reviewers are told. For replaying a change
    /// that has already been reviewed on GitHub, so they cannot read the review.
    #[arg(long)]
    no_gh: bool,
    /// How many nodes run at once.
    #[arg(long, default_value_t = 4)]
    concurrency: usize,
    /// Run nothing, and exit 3, when the last complete run of this graph on
    /// this checkout had the same base, head and diff, uncommitted changes
    /// included, the same untracked files that git does not ignore, and the
    /// same graph, prompt files, convention files, overrides and review-run
    /// version. A changed PR description alone does not count as new.
    #[arg(long)]
    if_changed: bool,
    /// Print which nodes would run and their commands, and run nothing.
    #[arg(long)]
    dry_run: bool,
    /// Stream node events as JSON lines instead of printing the report.
    #[arg(long)]
    json: bool,
    #[arg(long, default_value = "claude", hide = true)]
    claude_bin: String,
}

fn git(ws: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(ws)
        .args(args)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn owner_repo(url: &str) -> Option<String> {
    let rest = url.trim().trim_end_matches(".git");
    let rest = rest
        .strip_prefix("git@github.com:")
        .or_else(|| rest.split("github.com/").nth(1))?;
    (rest.split('/').count() == 2).then(|| rest.to_string())
}

/// The pull request `number`, or the checked-out branch's when none is given,
/// as `gh` knows it. Any failure, or no answer within five seconds, is no pull
/// request.
fn pull_request(ws: &Path, number: Option<u64>) -> Option<(u64, String)> {
    use std::io::Read;
    use std::process::Stdio;
    let mut gh = Command::new("gh");
    gh.args(["pr", "view"]);
    if let Some(n) = number {
        gh.arg(n.to_string());
    }
    let mut child = gh
        .args(["--json", "number,body"])
        .current_dir(ws)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let text = reader.join().ok()?;
    let v: Value = status
        .success()
        .then(|| serde_json::from_str(&text).ok())
        .flatten()?;
    Some((
        v["number"].as_u64()?,
        v["body"].as_str().unwrap_or_default().to_string(),
    ))
}

/// `--pr` wins; otherwise the branch's pull request, then `$REVIEW_PR_NUMBER`
/// when the branch has none.
fn resolve_pr(
    ws: &Path,
    flag: Option<u64>,
    env: Option<&str>,
    lookup: &dyn Fn(&Path, Option<u64>) -> Option<(u64, String)>,
) -> Option<(u64, String)> {
    if flag.is_some() {
        return lookup(ws, flag);
    }
    lookup(ws, None).or_else(|| lookup(ws, Some(env?.trim().parse().ok()?)))
}

fn target(cli: &Cli) -> Result<Target, String> {
    let ws = PathBuf::from(git(&cli.workspace, &["rev-parse", "--show-toplevel"])?.trim());
    let rev = |r: &str| {
        git(&ws, &["rev-parse", "--verify", &format!("{r}^{{commit}}")])
            .map(|s| s.trim().to_string())
    };
    let base_sha = rev(&cli.base)?;
    let head_sha = rev("HEAD")?;
    let uncommitted = match &cli.head {
        Some(h) => {
            // Nodes read the workspace's files, so those must be the reviewed ones.
            if rev(h)? != head_sha {
                return Err(format!("{h} is not what {} has checked out", ws.display()));
            }
            false
        }
        None => !git(&ws, &["status", "--porcelain", "--untracked-files=no"])?
            .trim()
            .is_empty(),
    };
    let range: Vec<&str> = if cli.head.is_some() {
        vec![&base_sha, &head_sha]
    } else {
        vec![&base_sha]
    };
    let diff = git(&ws, &[&["diff"], &range[..]].concat())?;
    if diff.trim().is_empty() {
        return Err(format!(
            "no difference between {} and the reviewed tree",
            cli.base
        ));
    }
    let owner_repo = git(&ws, &["remote", "get-url", "origin"])
        .ok()
        .and_then(|u| owner_repo(&u));
    let pr = owner_repo.as_ref().and_then(|_| {
        resolve_pr(
            &ws,
            cli.pr,
            std::env::var("REVIEW_PR_NUMBER").ok().as_deref(),
            &pull_request,
        )
    });
    Ok(Target {
        pr_number: pr.as_ref().map(|p| p.0),
        pr_body: pr.map(|p| p.1),
        changed_files: git(&ws, &[&["diff", "--name-only"], &range[..]].concat())?
            .lines()
            .map(str::to_string)
            .collect(),
        branch: git(&ws, &["rev-parse", "--abbrev-ref", "HEAD"])?
            .trim()
            .to_string(),
        owner_repo,
        workspace: ws,
        base_sha,
        head_sha,
        uncommitted,
        diff,
    })
}

fn graph_path(arg: &str) -> Result<PathBuf, String> {
    let p = PathBuf::from(arg);
    if p.is_file() {
        return Ok(p);
    }
    let dir = std::env::var("REVIEW_RUN_GRAPHS").map_err(|_| {
        format!("`{arg}` is not a file, and REVIEW_RUN_GRAPHS is not set to look it up by name")
    })?;
    Ok(Path::new(&dir).join(format!("{arg}.toml")))
}

/// `NODE=VALUE` overrides, applied to the loaded graph.
fn apply(
    graph: &mut Graph,
    specs: &[String],
    what: &str,
    set: fn(&mut Node, String),
) -> Result<(), String> {
    for spec in specs {
        let (name, value) = spec
            .split_once('=')
            .ok_or_else(|| format!("--{what} wants NODE=VALUE, got `{spec}`"))?;
        let node = graph
            .nodes
            .iter_mut()
            .find(|n| n.name == name)
            .ok_or_else(|| format!("--{what}: no node `{name}`"))?;
        set(node, value.to_string());
    }
    Ok(())
}

/// A kept bundle lives under `$REVIEW_RUN_CAPTURE_DIR`; without it the bundle
/// is scratch space in the temp directory, still needed by the run itself
/// (`--resume`, `--reuse-from`) but not retained on purpose.
fn default_out(run_id: &str) -> PathBuf {
    match std::env::var("REVIEW_RUN_CAPTURE_DIR") {
        Ok(root) if !root.is_empty() => Path::new(&root).join(run_id),
        _ => std::env::temp_dir().join("review-run").join(run_id),
    }
}

fn run_id(t: &Target) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!(
        "{}-{}-{}-{:08x}",
        chrono::Utc::now().format("%Y%m%d-%H%M"),
        t.workspace
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        &t.head_sha[..8.min(t.head_sha.len())],
        nanos ^ std::process::id().rotate_left(16)
    )
}

/// The convention files the generators would see, and what each mode costs.
fn print_conventions(graph: &Graph, t: &Target, s: &mut String) {
    let c = Conventions::collect(&t.workspace, &t.changed_files);
    let budget = graph.conventions_budget;
    let modes: Vec<String> = graph
        .nodes
        .iter()
        .filter(|n| n.kind == Kind::Generator)
        .map(|n| format!("{}={}", n.name, n.conventions.as_str()))
        .collect();
    let _ = writeln!(
        s,
        "conventions ({}; budget {budget} bytes)",
        modes.join(", ")
    );
    let fits = c.inlined(budget);
    for (f, inlined) in c.files.iter().zip(fits) {
        let scope = if f.scope.is_empty() { "." } else { &f.scope };
        let _ = writeln!(
            s,
            "  {:<44} {:>7} bytes  scope {scope:<28} {}",
            f.path,
            f.bytes,
            if inlined { "inlined" } else { "listed" }
        );
    }
    if c.files.is_empty() {
        let _ = writeln!(s, "  none collected");
    }
    let _ = writeln!(
        s,
        "  prompt bytes added: inline {}, list {}",
        c.section(Mode::Inline, budget).len(),
        c.section(Mode::List, budget).len()
    );
}

/// What a dry run prints: the resolved graph and, for each node, what would
/// run or why it would not.
fn dry_run(
    graph: &Graph,
    t: &Target,
    opts: &Options,
    providers: Providers,
) -> Result<String, String> {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "graph {} over {} ({} changed files)",
        graph.name,
        t.workspace.display(),
        t.changed_files.len()
    );
    print_conventions(graph, t, &mut s);
    let mut fired: BTreeMap<&str, String> = BTreeMap::new();
    if !graph.triggers.is_empty() || graph.nodes.iter().any(|n| !n.emits.is_empty()) {
        let _ = writeln!(s, "triggers");
    }
    for tr in &graph.triggers {
        match tr.fired_by(&t.changed_files, &t.diff) {
            Some(by) => {
                let _ = writeln!(s, "  {:<18} fired: {by}", tr.name);
                fired.insert(&tr.name, by);
            }
            None => {
                let _ = writeln!(s, "  {:<18} not fired", tr.name);
            }
        }
    }
    let mut at_run_time: BTreeMap<&str, &str> = BTreeMap::new();
    for n in &graph.nodes {
        for name in &n.emits {
            at_run_time.insert(name, &n.name);
        }
    }
    for (name, by) in &at_run_time {
        let _ = writeln!(s, "  {name:<18} decided by {by} at run time");
    }
    if opts.no_gh.is_some() {
        let _ = writeln!(
            s,
            "no-gh: tokens removed, empty GH_CONFIG_DIR, failing gh first on PATH"
        );
    }
    if let Some(why) = opts.reuse.as_ref().and_then(|p| p.unusable.as_ref()) {
        let _ = writeln!(s, "reuse: nothing, {why}");
    }
    // Nodes that would come out as the earlier run had them, by name.
    let mut same: HashSet<String> = HashSet::new();
    for (i, wave) in graph.waves()?.iter().enumerate() {
        for node in wave {
            // Without a prewarm node a fork is the plain generator it would run as.
            let plain;
            let node = if opts.no_prewarm && node.fork_from.is_some() {
                plain = graph.unforked(node);
                &plain
            } else {
                node
            };
            let before = opts.reuse.as_ref().filter(|p| p.skipped(&node.name));
            if opts.no_prewarm && node.kind == Kind::Prewarm {
                let _ = writeln!(s, "{i}  {:<16} skipped: --no-prewarm", node.name);
                if before.is_some() {
                    same.insert(node.name.clone());
                }
                continue;
            }
            if opts.skip.contains(&node.name) {
                let _ = writeln!(s, "{i}  {:<16} skipped by request", node.name);
                if before.is_some() {
                    same.insert(node.name.clone());
                }
                continue;
            }
            let mut gate = String::new();
            if !node.trigger.is_empty()
                && !node.trigger.iter().any(|n| fired.contains_key(n.as_str()))
            {
                let waiting: Vec<&str> = node
                    .trigger
                    .iter()
                    .map(String::as_str)
                    .filter(|n| at_run_time.contains_key(n))
                    .collect();
                if waiting.is_empty() {
                    let _ = writeln!(
                        s,
                        "{i}  {:<16} skipped: no trigger fired ({})",
                        node.name,
                        node.trigger.join(", ")
                    );
                    if before.is_some() {
                        same.insert(node.name.clone());
                    }
                    continue;
                }
                gate = format!("runs if {} fires\n     ", waiting.join(", "));
            }
            if let Some(prev) = &opts.reuse {
                let upstream = match node.kind {
                    Kind::Merger => graph
                        .nodes
                        .iter()
                        .filter(|n| matches!(n.kind, Kind::Generator | Kind::Critic))
                        .all(|n| same.contains(n.name.as_str())),
                    _ => node
                        .needs
                        .iter()
                        .chain(node.fork_from.iter())
                        .all(|n| same.contains(n.as_str())),
                };
                // A per-need critic is reused instance by instance: one for each
                // needed generator the earlier run audited.
                let instances: Vec<Node> = if node.per_need {
                    node.needs
                        .iter()
                        .filter(|need| same.contains(need.as_str()))
                        .map(|need| Node {
                            name: node.instance_name(need),
                            ..Node::clone(node)
                        })
                        .filter(|inst| {
                            compose_system_prompt(inst)
                                .is_ok_and(|sp| prev.matching(inst, &sp).is_some())
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                if !instances.is_empty() {
                    let names: Vec<&str> = instances.iter().map(|n| n.name.as_str()).collect();
                    let _ = writeln!(
                        s,
                        "{i}  {:<16} reused instances: {}",
                        node.name,
                        names.join(", ")
                    );
                    if upstream {
                        same.insert(node.name.clone());
                    }
                    if instances.len() == node.needs.len() {
                        continue;
                    }
                }
                if upstream
                    && !node.per_need
                    && compose_system_prompt(node)
                        .is_ok_and(|sp| prev.matching(node, &sp).is_some())
                {
                    let _ = writeln!(
                        s,
                        "{i}  {:<16} reused from {}",
                        node.name,
                        prev.dir.display()
                    );
                    same.insert(node.name.clone());
                    continue;
                }
            }
            let schema = match node.kind {
                Kind::Critic => critic_schema(),
                Kind::Mechanical => mechanical_schema(),
                Kind::Merger => merger_schema(),
                _ => generator_schema(),
            };
            let dir = opts.out.join("nodes").join(&node.name);
            let inv = Invocation {
                node,
                workspace: &t.workspace,
                prompt: "",
                schema: &schema,
                dir: &dir,
                attempt: 1,
                // A fork's session does not exist until its prewarm node has run.
                resume: node.fork_from.as_ref().map(|_| "<prewarm session>"),
                fork: node.fork_from.is_some(),
                verbatim: node.kind == Kind::Mechanical,
                no_gh: opts.no_gh.as_ref(),
            };
            let provider: &dyn Provider = match node.provider {
                ProviderKind::Claude => providers.claude,
                ProviderKind::Command => providers.command,
            };
            let mut argv = provider.command(&inv)?;
            if let Some(i) = argv.iter().position(|a| a == "--json-schema") {
                argv[i + 1] = "<schema>".into();
            }
            let waits = graph.waits_for(node);
            let after = if waits.is_empty() {
                String::new()
            } else {
                format!(" after {}", waits.join(", "))
            };
            let kind = if node.per_need {
                "critic per generator"
            } else {
                node.kind.as_str()
            };
            let env: String = cache_env(node)
                .iter()
                .map(|(k, v)| format!("{k}={v} "))
                .collect();
            let _ = writeln!(
                s,
                "{i}  {:<16} {kind}{after}\n     {gate}{env}{}",
                node.name,
                argv.join(" ")
            );
        }
    }
    Ok(s)
}

fn progress(json_mode: bool, ev: Value) {
    if json_mode {
        println!("{ev}");
        return;
    }
    match ev["type"].as_str() {
        Some("node_start") => eprintln!("{}: started", ev["node"].as_str().unwrap_or("")),
        Some("node_reused") => eprintln!("{}: reused", ev["node"].as_str().unwrap_or("")),
        Some("node_end") => {
            let Ok(n) =
                serde_json::from_value::<serde_json::Map<String, Value>>(ev["node"].clone())
            else {
                return;
            };
            let s = |k: &str| n.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            let detail = match s("status").as_str() {
                "ok" => {
                    let count = |key: &str| n.get(key).and_then(Value::as_u64);
                    let found = match (count("findings"), count("diagnostics")) {
                        (Some(c), _) => format!("{c} findings, "),
                        (_, Some(c)) => format!("{c} diagnostics, "),
                        _ => String::new(),
                    };
                    format!(
                        "{found}{:.1} min, ${:.2}",
                        n["wall_ms"].as_f64().unwrap_or(0.0) / 60_000.0,
                        n["cost_usd"].as_f64().unwrap_or(0.0)
                    )
                }
                _ => s("reason"),
            };
            eprintln!("{}: {} ({detail})", s("name"), s("status"));
        }
        _ => {}
    }
}

/// `None` when `--if-changed` declined to run.
fn real_main(cli: &Cli) -> Result<Option<bool>, String> {
    let graph_file = graph_path(&cli.graph)?;
    let mut graph = Graph::load(&graph_file)?;
    apply(&mut graph, &cli.model, "model", |n, v| n.model = Some(v))?;
    apply(&mut graph, &cli.effort, "effort", |n, v| n.effort = Some(v))?;
    for name in &cli.skip {
        graph
            .node(name)
            .ok_or_else(|| format!("--skip: no node `{name}`"))?;
    }
    let t = target(cli)?;
    let review = Review {
        graph_file: &graph_file,
        graph: &graph,
        skip: &cli.skip,
        no_prewarm: cli.no_prewarm,
        no_gh: cli.no_gh,
    };
    if cli.if_changed {
        if let Some(last) = reviewed::already(&t, &review) {
            eprintln!(
                "review-run: already reviewed this change ({} at {}{}), bundle {}",
                &t.head_sha[..8.min(t.head_sha.len())],
                last["reviewed_at"].as_str().unwrap_or("?"),
                if t.uncommitted {
                    ", uncommitted changes included"
                } else {
                    ""
                },
                last["bundle"].as_str().unwrap_or("?")
            );
            return Ok(None);
        }
    }
    // Taken before the run: the record describes the inputs the nodes read.
    let fingerprint = reviewed::fingerprint(&t, &review);
    let run_id = run_id(&t);
    let out = cli
        .resume
        .clone()
        .or_else(|| cli.out.clone())
        .unwrap_or_else(|| default_out(&run_id));
    let reuse = match (&cli.reuse_from, &cli.resume) {
        (_, Some(dir)) => {
            let previous = Previous::resuming(dir, &t)?;
            previous
                .gh_access_matches(cli.no_gh)
                .map_err(|why| format!("--resume: {why}"))?;
            Some(previous)
        }
        (Some(dir), None) => {
            if matches!((dir.canonicalize(), out.canonicalize()), (Ok(a), Ok(b)) if a == b) {
                return Err("--reuse-from is the bundle this run writes".into());
            }
            let mut previous = Previous::open(dir, &t)?;
            if let Err(why) = previous.gh_access_matches(cli.no_gh) {
                previous.unusable.get_or_insert(why);
            }
            if let Some(why) = &previous.unusable {
                eprintln!("review-run: reusing nothing from {}: {why}", dir.display());
            }
            Some(previous)
        }
        (None, None) => None,
    };
    // Made only now: the pull request lookup in `target` is the one use of `gh` the run keeps.
    let no_gh = cli.no_gh.then(NoGh::create).transpose()?;
    let opts = Options {
        out,
        run_id,
        skip: cli.skip.clone(),
        concurrency: cli.concurrency,
        reuse,
        no_prewarm: cli.no_prewarm,
        no_gh,
    };
    let claude = Claude {
        bin: cli.claude_bin.clone(),
    };
    let providers = Providers {
        claude: &claude,
        command: &Exec,
    };
    if cli.dry_run {
        print!("{}", dry_run(&graph, &t, &opts, providers)?);
        return Ok(Some(true));
    }
    let json_mode = cli.json;
    let summary = run(&graph, &t, &opts, providers, &move |ev| {
        progress(json_mode, ev)
    })?;
    if cli.json {
        let failed: Vec<&NodeRecord> = summary
            .nodes
            .iter()
            .filter(|n| n.status == "failed")
            .collect();
        println!(
            "{}",
            json!({"type": "done", "out": opts.out, "complete": summary.complete, "failed": failed.iter().map(|n| &n.name).collect::<Vec<_>>()})
        );
    } else {
        print!("{}", summary.report);
        eprintln!("bundle: {}", opts.out.display());
    }
    match (&fingerprint, summary.complete) {
        (Ok(fp), true) => reviewed::record(&t, &review, fp, &opts.out),
        (Err(e), true) => {
            eprintln!("review-run: could not record this review for --if-changed: {e}")
        }
        _ => {}
    }
    Ok(Some(summary.complete))
}

fn main() -> ExitCode {
    match real_main(&Cli::parse()) {
        Ok(Some(true)) => ExitCode::SUCCESS,
        Ok(Some(false)) => ExitCode::from(1),
        Ok(None) => ExitCode::from(3),
        Err(e) => {
            eprintln!("review-run: {e}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pr_flag_is_a_number_and_reaches_the_lookup_before_the_branch_or_environment() {
        assert!(Cli::try_parse_from(["review-run", "--base", "b", "--pr", "abc"]).is_err());
        let cli = Cli::try_parse_from(["review-run", "--base", "b", "--pr", "42"]).unwrap();
        assert_eq!(cli.pr, Some(42));

        let seen = std::cell::RefCell::new(vec![]);
        let lookup = |_: &Path, n: Option<u64>| {
            seen.borrow_mut().push(n);
            n.map(|n| (n, format!("body {n}")))
        };
        let ws = Path::new(".");
        assert_eq!(
            resolve_pr(ws, cli.pr, Some("7"), &lookup),
            Some((42, "body 42".into()))
        );
        assert_eq!(*seen.borrow(), [Some(42)]);

        seen.borrow_mut().clear();
        assert_eq!(
            resolve_pr(ws, None, Some(" 7\n"), &lookup),
            Some((7, "body 7".into()))
        );
        assert_eq!(*seen.borrow(), [None, Some(7)]);

        seen.borrow_mut().clear();
        assert_eq!(resolve_pr(ws, None, Some("nope"), &lookup), None);
        assert_eq!(resolve_pr(ws, None, None, &lookup), None);
    }

    const FINDING: &str = r#"{"file":"a.rs","line":1,"severity":"P2","confidence":"high","category":"x","description":"d","why_it_matters":"w","invariant":"i","recommendation":"r","verify":"by inspection"}"#;

    /// Command nodes that print a canned output, so the bundle a dry run
    /// looks back at was written by a real run.
    fn graph(dir: &Path) -> Graph {
        let say = |name: &str, kind: &str, extra: &str, json: String| {
            let file = dir.join(format!("{name}.json"));
            std::fs::write(&file, json).unwrap();
            format!("[[node]]\nname = \"{name}\"\nkind = \"{kind}\"\nprovider = \"command\"\n{extra}command = [\"cat\", \"{}\"]\n", file.display())
        };
        let found =
            |_: ()| format!(r#"{{"findings":[{FINDING}],"upstream_reading":[],"meta":""}}"#);
        let verdicts = r#"{"verdicts":[{"index":"code:0","verdict":"kept","reason":"r"},{"index":"releng:0","verdict":"kept","reason":"r"}]}"#;
        let text = [
            say("code", "generator", "", found(())),
            say("releng", "generator", "", found(())),
            say(
                "audit",
                "critic",
                "needs = [\"code\", \"releng\"]\n",
                verdicts.to_string(),
            ),
        ]
        .concat();
        Graph::parse(&text, dir, "test").unwrap()
    }

    #[test]
    fn a_dry_run_lists_the_nodes_it_would_reuse_and_the_ones_it_would_run() {
        let dir = std::env::temp_dir().join(format!("review-run-dry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let t = Target {
            workspace: dir.clone(),
            base_sha: "b".repeat(40),
            head_sha: "a".repeat(40),
            branch: "feature".into(),
            owner_repo: None,
            uncommitted: false,
            diff: "diff --git a/a.rs b/a.rs\n".into(),
            changed_files: vec!["a.rs".into()],
            pr_number: None,
            pr_body: None,
        };
        let claude = Claude {
            bin: "claude".into(),
        };
        let providers = Providers {
            claude: &claude,
            command: &Exec,
        };
        let g = graph(&dir);
        let first = Options {
            out: dir.join("first"),
            run_id: "first".into(),
            skip: vec![],
            concurrency: 2,
            reuse: None,
            no_prewarm: false,
            no_gh: None,
        };
        assert!(run(&g, &t, &first, providers, &|_| {}).unwrap().complete);

        let options = |reuse| Options {
            out: dir.join("second"),
            run_id: "second".into(),
            skip: vec![],
            concurrency: 2,
            reuse,
            no_prewarm: false,
            no_gh: None,
        };
        let from = dir.join("first").display().to_string();
        let reused =
            |out: &str, name: &str| out.contains(&format!("{:<16} reused from {from}", name));

        let same = dry_run(
            &g,
            &t,
            &options(Some(Previous::open(&dir.join("first"), &t).unwrap())),
            providers,
        )
        .unwrap();
        assert!(
            ["code", "releng", "audit"].iter().all(|n| reused(&same, n)),
            "{same}"
        );

        let mut other = g.clone();
        other.nodes[2].effort = Some("high".into());
        let out = dry_run(
            &other,
            &t,
            &options(Some(Previous::open(&dir.join("first"), &t).unwrap())),
            providers,
        )
        .unwrap();
        assert!(
            reused(&out, "code") && reused(&out, "releng") && !reused(&out, "audit"),
            "{out}"
        );
        assert!(
            out.contains("audit            critic after code, releng"),
            "{out}"
        );

        let plain = dry_run(&g, &t, &options(None), providers).unwrap();
        assert!(!plain.contains("reused"));

        let mut elsewhere = t.clone();
        elsewhere.head_sha = "c".repeat(40);
        let out = dry_run(
            &g,
            &elsewhere,
            &options(Some(
                Previous::open(&dir.join("first"), &elsewhere).unwrap(),
            )),
            providers,
        )
        .unwrap();
        assert!(
            out.contains("reuse: nothing, it reviewed") && !out.contains("reused from"),
            "{out}"
        );
    }
}
