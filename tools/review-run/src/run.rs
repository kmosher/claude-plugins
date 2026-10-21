//! Runs a graph over one change and writes the capture bundle. Everything a
//! model does not decide lives here: which nodes run, what each is handed,
//! whether its output is accepted, and how verdicts become a report.

use crate::conventions::{Conventions, Mode};
use crate::graph::{Graph, Kind, Node, ProviderKind};
use crate::provider::{
    compose_lens, compose_system_prompt, Invocation, NoGh, Provider, Usage, CACHE_TTL, NO_GH_NOTICE,
};
use crate::reuse::Previous;
use crate::schema::{
    check_groups, check_summary, check_verdicts, critic_schema, generator_schema,
    mechanical_schema, merger_schema, ContextLine, CriticOutput, Diagnostic, Finding,
    GeneratorOutput, MechanicalOutput, MergeGroup, MergerOutput, PrewarmOutput, Severity, Verdict,
    VerdictKind,
};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// The change under review, resolved before any node runs.
#[derive(Debug, Clone)]
pub struct Target {
    pub workspace: PathBuf,
    pub base_sha: String,
    pub head_sha: String,
    pub branch: String,
    pub owner_repo: Option<String>,
    /// The diff includes working-tree changes that `head_sha` does not.
    pub uncommitted: bool,
    pub diff: String,
    pub changed_files: Vec<String>,
    /// The pull request for the head branch, when GitHub has one.
    pub pr_number: Option<u64>,
    pub pr_body: Option<String>,
}

pub struct Options {
    pub out: PathBuf,
    pub run_id: String,
    pub skip: Vec<String>,
    pub concurrency: usize,
    /// An earlier bundle of this change to take unchanged nodes from.
    pub reuse: Option<Previous>,
    /// Leave prewarm nodes out; their forks run as plain generators.
    pub no_prewarm: bool,
    /// Withhold GitHub from every node: reviewers and mechanical scripts run
    /// with no token, an empty `gh` config and a `gh` that fails. Set after
    /// this crate's own pull request lookup, which needs `gh`.
    pub no_gh: Option<NoGh>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NodeRecord {
    pub name: String,
    pub kind: &'static str,
    /// `ok`, `failed` or `skipped`.
    pub status: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub needs: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Why the first attempt was rejected, when a retry then succeeded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_reason: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub command: Vec<String>,
    pub session_id: Option<String>,
    /// The prewarm node whose session this generator was forked from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fork_from: Option<String>,
    /// The prompt-cache lifetime the node's requests asked for, when the
    /// harness set one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_ttl: Option<&'static str>,
    /// Things that made this node's run differ from the graph's intent,
    /// without failing it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    pub attempts: u32,
    pub wall_ms: u64,
    pub turns: u64,
    pub usage: Usage,
    pub cost_usd: f64,
    pub exit: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub findings: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<usize>,
    /// The bundle this node's work was copied from instead of being run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reused_from: Option<String>,
    /// Carried over unchanged from the bundle being resumed; not written.
    #[serde(skip)]
    pub kept: bool,
}

impl NodeRecord {
    fn new(node: &Node) -> NodeRecord {
        NodeRecord {
            name: node.name.clone(),
            kind: node.kind.as_str(),
            status: "ok",
            needs: node.needs.clone(),
            reason: None,
            retry_reason: None,
            model: node.model.clone(),
            effort: node.effort.clone(),
            command: Vec::new(),
            session_id: None,
            fork_from: node.fork_from.clone(),
            cache_ttl: (node.kind == Kind::Prewarm || node.fork_from.is_some())
                .then_some(CACHE_TTL),
            warnings: Vec::new(),
            attempts: 0,
            wall_ms: 0,
            turns: 0,
            usage: Usage::default(),
            cost_usd: 0.0,
            exit: None,
            findings: None,
            diagnostics: None,
            reused_from: None,
            kept: false,
        }
    }

    /// Whether this run did not run the node: it was copied from another
    /// bundle or carried over from the one being resumed.
    fn carried(&self) -> bool {
        self.kept || self.reused_from.is_some()
    }

    /// An earlier run's record of `node` carried into this one: what it
    /// found and how it was run, with none of what it cost. When resuming in
    /// place the record is the node's own, cost and all.
    fn reused(node: &Node, old: &Value, prev: &Previous) -> NodeRecord {
        let text = |key: &str| old[key].as_str().map(str::to_string);
        let count = |key: &str| old[key].as_u64();
        let own = if prev.in_place {
            NodeRecord {
                wall_ms: count("wall_ms").unwrap_or(0),
                usage: serde_json::from_value(old["usage"].clone()).unwrap_or_default(),
                cost_usd: old["cost_usd"].as_f64().unwrap_or(0.0),
                reused_from: text("reused_from"),
                kept: true,
                ..NodeRecord::new(node)
            }
        } else {
            NodeRecord {
                reused_from: Some(prev.dir.display().to_string()),
                ..NodeRecord::new(node)
            }
        };
        NodeRecord {
            retry_reason: text("retry_reason"),
            command: old["command"]
                .as_array()
                .map(|c| {
                    c.iter()
                        .filter_map(|a| a.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            session_id: text("session_id"),
            warnings: old["warnings"]
                .as_array()
                .map(|w| {
                    w.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            attempts: count("attempts").unwrap_or(0) as u32,
            turns: count("turns").unwrap_or(0),
            exit: old["exit"].as_i64().map(|e| e as i32),
            findings: count("findings").map(|n| n as usize),
            diagnostics: count("diagnostics").map(|n| n as usize),
            ..own
        }
    }

    fn skipped(node: &Node, why: &str) -> NodeRecord {
        NodeRecord {
            status: "skipped",
            reason: Some(why.to_string()),
            ..NodeRecord::new(node)
        }
    }
}

pub struct Summary {
    pub nodes: Vec<NodeRecord>,
    pub report: String,
    /// False when any node failed; the report says which.
    pub complete: bool,
}

#[derive(Clone, Copy)]
pub struct Providers<'a> {
    pub claude: &'a dyn Provider,
    pub command: &'a dyn Provider,
}

impl<'a> Providers<'a> {
    fn get(&self, kind: ProviderKind) -> &'a dyn Provider {
        match kind {
            ProviderKind::Claude => self.claude,
            ProviderKind::Command => self.command,
        }
    }
}

pub type Events<'a> = &'a (dyn Fn(Value) + Sync);

fn now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// A diff this size or smaller rides in the prompt; a larger one is read
/// from the bundle, so a cold node does not pay for hunks it never opens.
const INLINE_DIFF_BYTES: usize = 200_000;

fn change_header(t: &Target, diff_path: &Path) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "workspace: {}", t.workspace.display());
    let _ = writeln!(s, "base: {}", t.base_sha);
    let _ = writeln!(
        s,
        "head: {}{}",
        t.head_sha,
        if t.uncommitted {
            " plus the uncommitted changes in the working tree"
        } else {
            ""
        }
    );
    let _ = writeln!(s, "diff: {}", diff_path.display());
    s
}

/// A diagnostic as one line, in the report and in the generator prompt.
fn diagnostic_line(d: &Diagnosed) -> String {
    let at = match (&d.diagnostic.file, d.diagnostic.line) {
        (Some(f), Some(l)) => format!(" {f}:{l}"),
        (Some(f), None) => format!(" {f}"),
        _ => String::new(),
    };
    format!(
        "{}{at}: {}",
        d.diagnostic.tool,
        d.diagnostic
            .message
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    )
}

/// Past this many, a list of diagnostics points at the bundle instead.
const LISTED_DIAGNOSTICS: usize = 100;

fn list_diagnostics(s: &mut String, diagnostics: &[Diagnosed], bullet: &str) {
    for d in diagnostics.iter().take(LISTED_DIAGNOSTICS) {
        let _ = writeln!(s, "{bullet}{}", diagnostic_line(d));
    }
    if diagnostics.len() > LISTED_DIAGNOSTICS {
        let _ = writeln!(
            s,
            "{bullet}... and {} more in diagnostics.jsonl",
            diagnostics.len() - LISTED_DIAGNOSTICS
        );
    }
}

/// Past this many characters an earlier reader's description is cut.
const EARLIER_DESCRIPTION_CHARS: usize = 300;

/// A finding as one line in a later reader's prompt.
fn earlier_finding_line(f: &Finding) -> String {
    let description = f
        .description
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let description = match description.char_indices().nth(EARLIER_DESCRIPTION_CHARS) {
        Some((end, _)) => format!("{}...", &description[..end]),
        None => description,
    };
    format!(
        "{} [{:?}, {}] {description}",
        location(f),
        f.severity,
        f.category
    )
}

fn changed_files_block(s: &mut String, t: &Target) {
    let _ = writeln!(s, "\nchanged files:");
    for f in &t.changed_files {
        let _ = writeln!(s, "  {f}");
    }
}

fn context_block(s: &mut String, context: &[ContextLine]) {
    if !context.is_empty() {
        let _ = writeln!(
            s,
            "\nContext from mechanical tools — files outside the diff that bear on the change. Open them before judging; they are pointers, not findings:"
        );
        for c in context {
            let _ = writeln!(
                s,
                "  {}: {}",
                c.tool,
                c.message.split_whitespace().collect::<Vec<_>>().join(" ")
            );
        }
    }
}

fn diagnostics_block(s: &mut String, diagnostics: &[Diagnosed]) {
    if !diagnostics.is_empty() {
        let _ = writeln!(
            s,
            "\nmechanical diagnostics, already reported; do not raise them again as findings:"
        );
        list_diagnostics(s, diagnostics, "  ");
    }
}

fn earlier_block(s: &mut String, earlier: &[(String, Finding)]) {
    if !earlier.is_empty() {
        let _ = writeln!(
            s,
            "\nfindings already reported by earlier readers; do not raise them again, and do not spend time confirming them:"
        );
        for (_, f) in earlier {
            let _ = writeln!(s, "  {}", earlier_finding_line(f));
        }
    }
}

fn diff_block(s: &mut String, t: &Target) {
    if t.diff.len() <= INLINE_DIFF_BYTES {
        let _ = write!(s, "\nThe diff:\n\n{}", t.diff);
    } else {
        let _ = writeln!(
            s,
            "\nThe diff is {} bytes; read it from the path above.",
            t.diff.len()
        );
    }
}

fn generator_prompt(
    t: &Target,
    node: &Node,
    diff_path: &Path,
    context: &[ContextLine],
    diagnostics: &[Diagnosed],
    earlier: &[(String, Finding)],
    conventions: &str,
) -> String {
    let mut s = String::from("Review this change with your lens.\n\n");
    s.push_str(&change_header(t, diff_path));
    if !node.system_prompt.is_empty() {
        let files: Vec<String> = node
            .system_prompt
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        let _ = writeln!(s, "lens instructions assembled from: {}", files.join(", "));
    }
    changed_files_block(&mut s, t);
    context_block(&mut s, context);
    s.push_str(conventions);
    diagnostics_block(&mut s, diagnostics);
    earlier_block(&mut s, earlier);
    diff_block(&mut s, t);
    s
}

/// What a prewarm node is asked: the same sections a generator gets, then
/// what to read and the shape to end on. It reads for the reviewers that
/// will continue its session and judges nothing.
fn prewarm_prompt(
    t: &Target,
    node: &Node,
    diff_path: &Path,
    context: &[ContextLine],
    diagnostics: &[Diagnosed],
    conventions: &str,
) -> String {
    let mut s = String::from(
        "Read this change so that reviewers continuing this session start with all of it in context. \
         You are not reviewing it: do not look for defects, rank risks, or recommend changes.\n\n",
    );
    s.push_str(&change_header(t, diff_path));
    changed_files_block(&mut s, t);
    context_block(&mut s, context);
    s.push_str(conventions);
    diagnostics_block(&mut s, diagnostics);
    diff_block(&mut s, t);
    let lsp = node.tools.iter().any(|x| x == "LSP");
    let _ = writeln!(s, "\nRead, in this order:");
    let _ = writeln!(
        s,
        "1. The diff, from the path above if it is not shown in full."
    );
    let _ = writeln!(s, "2. Every changed file in full, not only its hunks.");
    let _ = writeln!(s, "3. Every file the context section above points at.");
    let _ = writeln!(
        s,
        "4. Every convention file listed above that was not inlined."
    );
    if lsp {
        let _ = writeln!(s, "5. With LSP, the definition and the references of each symbol the diff adds or changes.");
    } else {
        let _ = writeln!(s, "5. For each symbol the diff adds or changes, its definition and its callers, found with Grep.");
    }
    let turns = node
        .max_turns
        .map(|n| format!(" and {n} turns"))
        .unwrap_or_default();
    let _ = writeln!(
        s,
        "\nStay within about {} bytes of file content read in total{turns}. Cover the changed files completely before anything else; \
         when the budget is short, drop the callers of unchanged symbols first.",
        node.read_budget_bytes
    );
    let _ = writeln!(
        s,
        "\nWhen you have read what you can, stop calling tools and finish by returning the output object with `findings` empty, \
         `upstream_reading` empty, and in `meta` a summary in exactly this shape and no other:\n\n\
         {}\n- one line per file: its path, then what it is and what the diff does to it\n\n\
         {}\n- each symbol the diff adds or changes, then who calls it (path:line)\n\n\
         {}\n- the rules from the convention files that bear on these files, each with its file and heading\n\n\
         {}\n- things you noticed but did not judge, as questions",
        crate::schema::SUMMARY_HEADINGS[0],
        crate::schema::SUMMARY_HEADINGS[1],
        crate::schema::SUMMARY_HEADINGS[2],
        crate::schema::SUMMARY_HEADINGS[3]
    );
    s
}

/// What a generator forked from a prewarm session is asked. The change, the
/// files and the diff are already in the session; this is the lens and the
/// instruction to use it.
fn fork_prompt(node: &Node, earlier: &[(String, Finding)]) -> Result<String, String> {
    let mut s = String::new();
    let lens = compose_lens(node)?;
    if !lens.trim().is_empty() {
        let _ = write!(s, "Your lens:\n\n{}\n---\n\n", lens.trim_end());
        // The lens text names sibling files by name; they sit beside the files it came from.
        let files: Vec<String> = node.lens.iter().map(|p| p.display().to_string()).collect();
        let _ = write!(
            s,
            "lens instructions assembled from: {}\n\n",
            files.join(", ")
        );
    }
    s.push_str(
        "Review this change with your lens. Everything you read in the prewarm is still in context; cite it, \
         re-open a file only when you need lines you did not read.\n",
    );
    earlier_block(&mut s, earlier);
    Ok(s)
}

fn critic_prompt(
    t: &Target,
    diff_path: &Path,
    inputs: &[(String, Finding)],
    conventions: &str,
) -> String {
    let mut s = String::from(
        "Audit these findings against the code. Return exactly one verdict per index.\n\n",
    );
    s.push_str(&change_header(t, diff_path));
    s.push_str(conventions);
    s.push_str("\nfindings:\n\n");
    for (index, f) in inputs {
        let mut row = serde_json::to_value(f).expect("a finding serializes");
        row["index"] = json!(index);
        let _ = writeln!(s, "{row}");
    }
    s
}

fn merger_prompt(t: &Target, diff_path: &Path, rows: &[Reported]) -> String {
    let mut s = String::from(
        "Group the findings below that describe one defect. Return only the groups.\n\n",
    );
    s.push_str(&change_header(t, diff_path));
    s.push_str("\nfindings:\n\n");
    for r in rows {
        let f = &r.finding;
        let _ = writeln!(
            s,
            "{}",
            json!({
                "index": r.index, "file": f.file, "line": f.line, "severity": f.severity, "category": f.category,
                "description": f.description, "invariant": f.invariant,
                "recommendation": f.recommendation, "verify": f.verify,
            })
        );
    }
    s
}

/// Whether two different reported findings in one file come from different
/// lenses, which is the only case a merger has anything to look at.
fn lenses_share_a_file(rows: &[Reported]) -> bool {
    rows.iter().enumerate().any(|(i, a)| {
        a.finding.file.is_some()
            && rows[i + 1..].iter().any(|b| {
                a.finding.file == b.finding.file
                    && a.lenses.iter().any(|l| b.lenses.iter().any(|m| l != m))
            })
    })
}

/// What a critic is told about conventions: the listing, when any generator
/// it audits was handed them, so a cited rule can be checked.
fn critic_conventions(graph: &Graph, conventions: &Conventions, audits: &[String]) -> String {
    let handed = audits
        .iter()
        .filter_map(|g| graph.node(g))
        .any(|g| graph.conventions_of(g) != Mode::None);
    if handed {
        conventions.listing()
    } else {
        String::new()
    }
}

/// Accepts a generator's output if it has the canonical shape, and rewrites
/// each path the way the rest of the pipeline keys on it: relative to the
/// repo root.
fn accept_findings(t: &Target, v: Value) -> Result<GeneratorOutput, String> {
    let mut out: GeneratorOutput =
        serde_json::from_value(v).map_err(|e| format!("output does not match the schema: {e}"))?;
    let root = format!("{}/", t.workspace.display());
    for f in &mut out.findings {
        if let Some(p) = &f.file {
            let p = p.strip_prefix(&root).unwrap_or(p);
            f.file = Some(p.strip_prefix("./").unwrap_or(p).to_string());
        }
    }
    Ok(out)
}

/// Accepts a mechanical node's output if it has the right shape and raises
/// only triggers the node declared, with paths rewritten as for findings.
/// Accepts a prewarm node's output: the generator shape with no findings and
/// the fixed-shape summary in `meta`. Returns the summary.
fn accept_summary(t: &Target, v: Value) -> Result<String, String> {
    let out = accept_findings(t, v)?;
    if !out.findings.is_empty() {
        return Err("a reading turn reports no findings; return `findings` empty and put the summary in `meta`".into());
    }
    check_summary(&out.meta)
}

fn accept_mechanical(t: &Target, node: &Node, v: Value) -> Result<MechanicalOutput, String> {
    let mut out: MechanicalOutput =
        serde_json::from_value(v).map_err(|e| format!("output does not match the schema: {e}"))?;
    if let Some(name) = out.triggers.iter().find(|n| !node.emits.contains(n)) {
        return Err(format!(
            "emitted trigger `{name}`, which the node does not declare in `emits`"
        ));
    }
    for c in out.context.iter_mut().filter(|c| c.tool.is_empty()) {
        c.tool = node.name.clone();
    }
    let root = format!("{}/", t.workspace.display());
    for d in &mut out.diagnostics {
        if let Some(p) = &d.file {
            let p = p.strip_prefix(&root).unwrap_or(p);
            d.file = Some(p.strip_prefix("./").unwrap_or(p).to_string());
        }
    }
    Ok(out)
}

fn mechanical_contract(t: &Target, node: &Node, bundle: &Path) -> Value {
    json!({
        "node": node.name,
        "workspace": t.workspace,
        "base": t.base_sha,
        "head": t.head_sha,
        "changed_files": t.changed_files,
        "diff": bundle.join("diff.patch"),
        "out": bundle.join("nodes").join(&node.name).join("out"),
        "pr_number": t.pr_number,
        "pr_body": t.pr_body,
    })
}

fn accept_groups(v: Value, indices: &[String]) -> Result<Vec<MergeGroup>, String> {
    let out: MergerOutput =
        serde_json::from_value(v).map_err(|e| format!("output does not match the schema: {e}"))?;
    check_groups(&out, indices)?;
    Ok(out.groups)
}

fn accept_verdicts(v: Value, indices: &[String]) -> Result<Vec<Verdict>, String> {
    let out: CriticOutput =
        serde_json::from_value(v).map_err(|e| format!("output does not match the schema: {e}"))?;
    check_verdicts(&out, indices)?;
    Ok(out.verdicts)
}

/// What a node is asked and how often it may be asked again.
struct Ask<'a> {
    prompt: &'a str,
    schema: &'a Value,
    /// `prompt` is the node's whole stdin; see `Invocation::verbatim`.
    verbatim: bool,
    /// Two for a reviewer, one for a mechanical node, which does not retry.
    attempts: u32,
    /// The session to fork the first attempt from; later attempts continue
    /// the fork itself.
    fork_of: Option<&'a str>,
    /// What the node is told after a rejection, following the reason.
    retry: &'a str,
}

const RETRY_STRUCTURED: &str = "Return the complete corrected output.";
const RETRY_SUMMARY: &str = "Do not read anything more. Return the complete output again, with the summary in the required shape in `meta` and `findings` empty.";

impl<'a> Ask<'a> {
    fn new(prompt: &'a str, schema: &'a Value, verbatim: bool, attempts: u32) -> Ask<'a> {
        Ask {
            prompt,
            schema,
            verbatim,
            attempts,
            fork_of: None,
            retry: RETRY_STRUCTURED,
        }
    }
}

/// Runs one node to an accepted output, or to failure after one retry. The
/// retry resumes the child's session with the rejection, so it corrects its
/// answer rather than redoing the review; a child that left no session to
/// resume never got far enough for a second try to differ.
fn execute<T: Serialize>(
    provider: &dyn Provider,
    node: &Node,
    t: &Target,
    out: &Path,
    ask: &Ask,
    no_gh: Option<&NoGh>,
    accept: &dyn Fn(Value) -> Result<T, String>,
) -> (NodeRecord, Option<T>) {
    let (prompt, schema) = (ask.prompt, ask.schema);
    // A reviewer is told once; a fork continues a copy of the session that
    // already was, and a mechanical script's stdin is its contract.
    let told;
    let prompt = if no_gh.is_some() && node.kind != Kind::Mechanical && ask.fork_of.is_none() {
        told = format!("{NO_GH_NOTICE}\n\n{prompt}");
        told.as_str()
    } else {
        prompt
    };
    let mut rec = NodeRecord::new(node);
    let dir = out.join("nodes").join(&node.name);
    // The composed system prompt is what a later run compares to decide
    // whether this node's work can be reused.
    let system_prompt = if node.kind == Kind::Mechanical {
        Ok(String::new())
    } else {
        compose_system_prompt(node)
    };
    let prepared = system_prompt.and_then(|sp| {
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        if node.kind == Kind::Mechanical {
            std::fs::create_dir_all(dir.join("out"))
                .map_err(|e| format!("{}: {e}", dir.join("out").display()))?;
        }
        std::fs::write(dir.join("prompt.md"), prompt)
            .map_err(|e| format!("{}: {e}", dir.display()))?;
        match node.kind {
            Kind::Mechanical => Ok(()),
            _ => std::fs::write(dir.join("system-prompt.md"), sp)
                .map_err(|e| format!("{}: {e}", dir.display())),
        }
    });
    if let Err(e) = prepared {
        rec.status = "failed";
        rec.reason = Some(e);
        return (rec, None);
    }
    let started = Instant::now();
    let mut resume: Option<String> = ask.fork_of.map(str::to_string);
    let mut forking = ask.fork_of.is_some();
    let mut next = prompt.to_string();
    let mut accepted = None;
    for attempt in 1..=ask.attempts {
        let inv = Invocation {
            node,
            workspace: &t.workspace,
            prompt: &next,
            schema,
            dir: &dir,
            attempt,
            resume: resume.as_deref(),
            fork: forking,
            verbatim: ask.verbatim,
            no_gh,
        };
        if attempt == 1 {
            rec.command = provider.command(&inv).unwrap_or_default();
        }
        let o = provider.run(&inv);
        forking = false;
        rec.attempts = attempt;
        rec.turns += o.turns;
        rec.cost_usd += o.cost_usd;
        rec.usage.add(&o.usage);
        rec.exit = o.exit;
        if o.session_id.is_some() {
            rec.session_id = o.session_id.clone();
        }
        match o.output.and_then(accept) {
            Ok(v) => {
                rec.retry_reason = rec.reason.take();
                // Without it the node still ran; a later run just cannot reuse it.
                let _ = std::fs::write(
                    dir.join("output.json"),
                    serde_json::to_string_pretty(&v).expect("an output serializes"),
                );
                accepted = Some(v);
                break;
            }
            Err(e) => {
                next = format!("Your output was rejected: {e}\n\n{}", ask.retry);
                rec.reason = Some(e);
                match o.session_id {
                    Some(id) => resume = Some(id),
                    None => break,
                }
            }
        }
    }
    if accepted.is_none() {
        rec.status = "failed";
    }
    rec.wall_ms = started.elapsed().as_millis() as u64;
    (rec, accepted)
}

enum Product {
    Mechanical(MechanicalOutput),
    Findings(GeneratorOutput),
    Prewarm(PrewarmOutput),
    Verdicts(Vec<Verdict>),
    Groups(Vec<MergeGroup>),
    Nothing,
}

/// A diagnostic and the mechanical node that reported it.
#[derive(Debug, Clone, Serialize)]
pub struct Diagnosed {
    pub node: String,
    #[serde(flatten)]
    pub diagnostic: Diagnostic,
}

/// A finding as it leaves the run: where it came from, what the critic said,
/// and the severity that verdict leaves it at.
#[derive(Debug, Clone, Serialize)]
pub struct Reported {
    pub index: String,
    pub lenses: Vec<String>,
    /// `reported`, `already-decided` for a settled or pre-existing one, or
    /// `duplicate` for one the merger folded into another.
    pub disposition: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict: Option<Verdict>,
    /// The index of the finding this one was merged into.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duplicate_of: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permalink: Option<String>,
    /// What was merged into this finding and why; rendered, not serialized,
    /// since each merged finding names its primary.
    #[serde(skip)]
    pub also: Option<Also>,
    #[serde(flatten)]
    pub finding: Finding,
}

#[derive(Debug, Clone)]
pub struct Also {
    /// Each merged finding's lenses and index.
    pub by: Vec<(String, String)>,
    pub reason: String,
}

pub struct Aggregate {
    pub reported: Vec<Reported>,
    pub decided: Vec<Reported>,
    pub dropped: Vec<Reported>,
    pub merged: Vec<Reported>,
}

/// Applies verdicts, then merges findings that landed on the same cell, then
/// folds each of the merger's groups into its most severe member. Only
/// `false-positive` removes a finding; a finding with no verdict is reported
/// as its generator wrote it.
pub fn aggregate(
    inputs: &[(String, Finding)],
    verdicts: &[Verdict],
    groups: &[MergeGroup],
) -> Aggregate {
    let mut agg = Aggregate {
        reported: Vec::new(),
        decided: Vec::new(),
        dropped: Vec::new(),
        merged: Vec::new(),
    };
    for (index, f) in inputs {
        let verdict = verdicts.iter().find(|v| &v.index == index).cloned();
        let mut finding = f.clone();
        let kind = verdict.as_ref().map(|v| v.verdict);
        if let (Some(VerdictKind::Downgraded | VerdictKind::Upgraded), Some(to)) =
            (kind, verdict.as_ref().and_then(|v| v.to))
        {
            finding.severity = to;
        }
        let lens = index.split(':').next().unwrap_or_default().to_string();
        let mut r = Reported {
            index: index.clone(),
            lenses: vec![lens],
            disposition: "reported",
            verdict,
            duplicate_of: None,
            permalink: None,
            also: None,
            finding,
        };
        match kind {
            Some(VerdictKind::FalsePositive) => agg.dropped.push(r),
            Some(VerdictKind::Settled | VerdictKind::PreExisting) => {
                r.disposition = "already-decided";
                agg.decided.push(r)
            }
            _ => {
                let same = agg.reported.iter_mut().find(|o| {
                    o.finding.file.is_some()
                        && o.finding.line.is_some()
                        && (&o.finding.file, o.finding.line, &o.finding.category)
                            == (&r.finding.file, r.finding.line, &r.finding.category)
                });
                match same {
                    Some(o) => {
                        // Keep the more severe framing; carry the other's text.
                        if r.finding.severity < o.finding.severity {
                            std::mem::swap(o, &mut r);
                        }
                        o.finding.description =
                            format!("{}\n\n{}", o.finding.description, r.finding.description);
                        o.lenses.extend(r.lenses);
                    }
                    None => agg.reported.push(r),
                }
            }
        }
    }
    for g in groups {
        merge_group(&mut agg, g);
    }
    // Stable, so findings of equal severity stay in graph order.
    agg.reported.sort_by_key(|r| r.finding.severity);
    agg
}

/// Folds a group into its most severe member, the earliest given on a tie.
/// A group naming fewer than two reported findings changes nothing.
fn merge_group(agg: &mut Aggregate, g: &MergeGroup) {
    // Positions, so members are taken in the order they were given and not the order the group names them.
    let mut at: Vec<usize> = g
        .indices
        .iter()
        .filter_map(|i| agg.reported.iter().position(|r| &r.index == i))
        .collect();
    at.sort_unstable();
    let Some(&primary) = at
        .iter()
        .min_by_key(|&&p| (agg.reported[p].finding.severity, p))
        .filter(|_| at.len() > 1)
    else {
        return;
    };
    let mut by = Vec::new();
    let mut absorbed = Vec::new();
    for &p in at.iter().filter(|&&p| p != primary) {
        let mut r = agg.reported[p].clone();
        by.push((r.lenses.join("/"), r.index.clone()));
        for l in &r.lenses {
            if !agg.reported[primary].lenses.contains(l) {
                agg.reported[primary].lenses.push(l.clone());
            }
        }
        r.disposition = "duplicate";
        r.duplicate_of = Some(agg.reported[primary].index.clone());
        absorbed.push((p, r));
    }
    agg.reported[primary].also = Some(Also {
        by,
        reason: g.reason.clone(),
    });
    let gone: Vec<usize> = absorbed.iter().map(|(p, _)| *p).collect();
    agg.merged.extend(absorbed.into_iter().map(|(_, r)| r));
    let mut kept = 0;
    agg.reported.retain(|_| {
        kept += 1;
        !gone.contains(&(kept - 1))
    });
}

fn location(f: &Finding) -> String {
    match (&f.file, f.line) {
        (Some(p), Some(l)) => format!("{p}:{l}"),
        (Some(p), None) => p.clone(),
        _ => "the change as a whole".to_string(),
    }
}

/// `https://github.com/<repo>/blob/<head>/<path>`, anchored to the cited line
/// and one either side. None when the run has no GitHub origin, reviews
/// uncommitted work that no commit holds, or the finding names no file.
fn permalink(t: &Target, f: &Finding) -> Option<String> {
    let repo = t.owner_repo.as_deref().filter(|_| !t.uncommitted)?;
    let mut url = format!("https://github.com/{repo}/blob/{}/", t.head_sha);
    for c in f.file.as_deref()?.chars() {
        match c {
            ' ' | '(' | ')' | '#' | '?' | '%' => {
                let _ = write!(url, "%{:02X}", c as u8);
            }
            c => url.push(c),
        }
    }
    if let Some(l) = f.line.filter(|&l| l > 0) {
        let _ = write!(url, "#L{}-L{}", l.saturating_sub(1).max(1), l + 1);
    }
    Some(url)
}

fn add_permalinks(t: &Target, agg: &mut Aggregate) {
    for r in agg
        .reported
        .iter_mut()
        .chain(&mut agg.decided)
        .chain(&mut agg.dropped)
        .chain(&mut agg.merged)
    {
        r.permalink = permalink(t, &r.finding);
    }
}

/// The location in backticks, linked to the commit's copy of the file when
/// there is one.
fn location_link(r: &Reported) -> String {
    let at = format!("`{}`", location(&r.finding));
    match &r.permalink {
        Some(url) => format!("[{at}]({url})"),
        None => at,
    }
}

fn render_finding(s: &mut String, r: &Reported) {
    let f = &r.finding;
    let _ = writeln!(
        s,
        "### {} — {} ({})\n",
        location_link(r),
        f.category,
        r.lenses.join(", ")
    );
    let _ = writeln!(s, "{}\n", f.description);
    let _ = writeln!(s, "**Why it matters:** {}\n", f.why_it_matters);
    if !f.invariant.is_empty() {
        let _ = writeln!(s, "**Invariant:** {}\n", f.invariant);
    }
    let _ = writeln!(s, "**Recommendation:** {}\n", f.recommendation);
    if !f.verify.is_empty() {
        let _ = writeln!(s, "**Verify:** {}\n", f.verify);
    }
    if let Some(also) = &r.also {
        let by: Vec<String> = also
            .by
            .iter()
            .map(|(lens, index)| format!("{lens} (`{index}`)"))
            .collect();
        let _ = writeln!(s, "_Also raised by {}: {}_\n", by.join(", "), also.reason);
    }
    let audit = r.verdict.as_ref().map(|v| {
        let moved = match (v.verdict, v.from) {
            (VerdictKind::Downgraded, Some(from)) => format!("downgraded from {from:?}"),
            (VerdictKind::Upgraded, Some(from)) => format!("upgraded from {from:?}"),
            (k, _) => serde_json::to_value(k)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default(),
        };
        format!(" Auditor: {moved}. {}", v.reason)
    });
    let _ = writeln!(
        s,
        "_Lens confidence: {}.{}_\n",
        confidence(f),
        audit.unwrap_or_default()
    );
}

fn confidence(f: &Finding) -> String {
    serde_json::to_value(f.confidence)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn render(
    t: &Target,
    agg: &Aggregate,
    nodes: &[NodeRecord],
    audited: bool,
    diagnostics: &[Diagnosed],
) -> String {
    let mut s = String::new();
    let short = |sha: &str| sha.chars().take(8).collect::<String>();
    let _ = writeln!(
        s,
        "# Review of {} against {}\n",
        short(&t.head_sha),
        short(&t.base_sha)
    );
    let count = |sev: Severity| {
        agg.reported
            .iter()
            .filter(|r| r.finding.severity == sev)
            .count()
    };
    let merged = match agg.merged.len() {
        0 => String::new(),
        m => format!(", {m} merged as duplicates"),
    };
    let _ = writeln!(
        s,
        "{} findings: {} P0, {} P1, {} P2, {} P3. {} already decided, {} dropped by the auditor{merged}.\n",
        agg.reported.len(),
        count(Severity::P0),
        count(Severity::P1),
        count(Severity::P2),
        count(Severity::P3),
        agg.decided.len(),
        agg.dropped.len()
    );
    if !agg.reported.is_empty() && count(Severity::P3) == agg.reported.len() {
        let _ = writeln!(s, "No blocking issues.\n");
    }
    for n in nodes.iter().filter(|n| n.status == "failed") {
        let _ = writeln!(
            s,
            "**Incomplete:** `{}` failed: {}\n",
            n.name,
            n.reason.as_deref().unwrap_or("no reason recorded")
        );
    }
    for n in nodes.iter().filter(|n| n.status != "failed") {
        for w in &n.warnings {
            let _ = writeln!(s, "**Warning:** `{}`: {w}\n", n.name);
        }
    }
    if !audited && !(agg.reported.is_empty() && agg.decided.is_empty()) {
        let _ = writeln!(s, "**Not audited:** no critic checked these findings; they are as the lenses wrote them.\n");
    }
    for sev in [Severity::P0, Severity::P1, Severity::P2, Severity::P3] {
        let rows: Vec<&Reported> = agg
            .reported
            .iter()
            .filter(|r| r.finding.severity == sev)
            .collect();
        if rows.is_empty() {
            continue;
        }
        let _ = writeln!(s, "## {sev:?}\n");
        for r in rows {
            render_finding(&mut s, r);
        }
    }
    if !agg.decided.is_empty() {
        let _ = writeln!(s, "## Already decided\n");
        for r in &agg.decided {
            render_finding(&mut s, r);
        }
    }
    if !agg.dropped.is_empty() {
        let _ = writeln!(s, "## Dropped by the auditor\n");
        for r in &agg.dropped {
            let why = r
                .verdict
                .as_ref()
                .map(|v| v.reason.as_str())
                .unwrap_or_default();
            let _ = writeln!(s, "- `{}` {}: {}", r.index, location_link(r), why);
        }
        s.push('\n');
    }
    if !diagnostics.is_empty() {
        let _ = writeln!(s, "## Mechanical\n");
        list_diagnostics(&mut s, diagnostics, "- ");
        s.push('\n');
    }
    let _ = writeln!(s, "## Nodes\n\n| node | status | model | effort | minutes | turns | cost |\n|---|---|---|---|---|---|---|");
    for n in nodes {
        let _ = writeln!(
            s,
            "| {} | {} | {} | {} | {:.1} | {} | ${:.2} |",
            n.name,
            if n.carried() { "ok, reused" } else { n.status },
            n.model.as_deref().unwrap_or("-"),
            n.effort.as_deref().unwrap_or("-"),
            n.wall_ms as f64 / 60_000.0,
            n.turns,
            n.cost_usd
        );
    }
    s
}

fn jsonl<T: Serialize>(rows: impl IntoIterator<Item = T>) -> String {
    rows.into_iter()
        .map(|r| serde_json::to_string(&r).expect("serializes") + "\n")
        .collect()
}

/// Where a record sorts in the manifest: its node's place in the graph, then
/// zero, or for a per-need critic instance one past its generator's place.
type Key = (usize, usize);

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Pending,
    Started,
    Finished,
}

/// What one child is asked, built before it starts so the thread holds
/// nothing the scheduler is still changing.
enum Job {
    Mechanical(String),
    Generator(String),
    Prewarm(String),
    /// A generator that continues a copy of `session`.
    Fork {
        prompt: String,
        session: String,
    },
    Critic {
        prompt: String,
        indices: Vec<String>,
    },
    Merger {
        prompt: String,
        indices: Vec<String>,
    },
}

struct Task {
    key: Key,
    /// Renamed for a per-need instance, so its directory and record are its own.
    node: Node,
    job: Job,
    warnings: Vec<String>,
}

struct Done {
    key: Key,
    rec: NodeRecord,
    product: Product,
}

fn work(task: &Task, t: &Target, opts: &Options, provider: &dyn Provider) -> (NodeRecord, Product) {
    let node = &task.node;
    let (mut rec, product) = match &task.job {
        Job::Mechanical(contract) => {
            let schema = mechanical_schema();
            let ask = Ask::new(contract, &schema, true, 1);
            let (mut rec, v) = execute(
                provider,
                node,
                t,
                &opts.out,
                &ask,
                opts.no_gh.as_ref(),
                &|v| accept_mechanical(t, node, v),
            );
            rec.diagnostics = v.as_ref().map(|o| o.diagnostics.len());
            (rec, v.map_or(Product::Nothing, Product::Mechanical))
        }
        Job::Critic { prompt, indices } => {
            let schema = critic_schema();
            let ask = Ask::new(prompt, &schema, false, 2);
            let (rec, v) = execute(
                provider,
                node,
                t,
                &opts.out,
                &ask,
                opts.no_gh.as_ref(),
                &|v| accept_verdicts(v, indices),
            );
            (rec, v.map_or(Product::Nothing, Product::Verdicts))
        }
        Job::Merger { prompt, indices } => {
            let schema = merger_schema();
            let ask = Ask::new(prompt, &schema, false, 2);
            let (rec, v) = execute(
                provider,
                node,
                t,
                &opts.out,
                &ask,
                opts.no_gh.as_ref(),
                &|v| accept_groups(v, indices),
            );
            (rec, v.map_or(Product::Nothing, Product::Groups))
        }
        Job::Generator(prompt) => {
            let schema = generator_schema();
            let ask = Ask::new(prompt, &schema, false, 2);
            let (mut rec, v) = execute(
                provider,
                node,
                t,
                &opts.out,
                &ask,
                opts.no_gh.as_ref(),
                &|v| accept_findings(t, v),
            );
            rec.findings = v.as_ref().map(|o| o.findings.len());
            (rec, v.map_or(Product::Nothing, Product::Findings))
        }
        Job::Fork { prompt, session } => {
            let schema = generator_schema();
            let ask = Ask {
                fork_of: Some(session),
                ..Ask::new(prompt, &schema, false, 2)
            };
            let (mut rec, v) = execute(
                provider,
                node,
                t,
                &opts.out,
                &ask,
                opts.no_gh.as_ref(),
                &|v| accept_findings(t, v),
            );
            rec.findings = v.as_ref().map(|o| o.findings.len());
            (rec, v.map_or(Product::Nothing, Product::Findings))
        }
        Job::Prewarm(prompt) => {
            let schema = generator_schema();
            let ask = Ask {
                retry: RETRY_SUMMARY,
                ..Ask::new(prompt, &schema, false, 2)
            };
            let (rec, v) = execute(
                provider,
                node,
                t,
                &opts.out,
                &ask,
                opts.no_gh.as_ref(),
                &|v| accept_summary(t, v),
            );
            let out = v.map(|summary| PrewarmOutput {
                session_id: rec.session_id.clone(),
                summary,
            });
            if let Some(o) = &out {
                // `execute` kept the bare summary; the session is what a fork or a reuse needs.
                let _ = std::fs::write(
                    opts.out.join("nodes").join(&node.name).join("output.json"),
                    serde_json::to_string_pretty(o).expect("an output serializes"),
                );
            }
            (rec, out.map_or(Product::Nothing, Product::Prewarm))
        }
    };
    rec.warnings = task.warnings.clone();
    (rec, product)
}

/// Trigger name to what fired it: the graph's own, then the mechanical
/// nodes' in graph order, so which node gets the credit never depends on
/// which finished first.
fn fired_now(
    base: &BTreeMap<String, String>,
    graph: &Graph,
    mech: &BTreeMap<usize, (Vec<Diagnosed>, Vec<String>)>,
) -> BTreeMap<String, String> {
    let mut fired = base.clone();
    for (&i, (_, triggers)) in mech {
        for name in triggers {
            fired
                .entry(name.clone())
                .or_insert_with(|| format!("mechanical node {}", graph.nodes[i].name));
        }
    }
    fired
}

/// Why a node is not run, by request, by a stopped graph or by its triggers.
fn left_out(
    node: &Node,
    opts: &Options,
    stopped: &Option<String>,
    fired: &BTreeMap<String, String>,
) -> Option<String> {
    if opts.skip.contains(&node.name) {
        return Some("skipped by request".into());
    }
    if opts.no_prewarm && node.kind == Kind::Prewarm {
        return Some("--no-prewarm: its forks run as plain generators".into());
    }
    if let Some(failed) = stopped {
        return Some(format!("mechanical node {failed} failed"));
    }
    if !node.trigger.is_empty() && !node.trigger.iter().any(|n| fired.contains_key(n)) {
        return Some(format!("no trigger fired ({})", node.trigger.join(", ")));
    }
    None
}

/// Whether node `j` of the graph comes out the same as in the earlier run:
/// it was reused, or it was left out both times. A per-need critic is the
/// same when no instance of it ran afresh.
fn unchanged(
    graph: &Graph,
    prev: &Previous,
    j: usize,
    records: &BTreeMap<Key, NodeRecord>,
    fresh: &[bool],
) -> bool {
    let node = &graph.nodes[j];
    match records.get(&(j, 0)) {
        _ if node.per_need && fresh[j] => false,
        None => node.per_need,
        Some(r) => r.carried() || (r.status == "skipped" && prev.skipped(&node.name)),
    }
}

/// Whether everything `task` reads from other nodes is as the earlier run
/// left it. A critic or merger reads findings and verdicts; a generator reads
/// what its needs found.
fn upstream_unchanged(
    graph: &Graph,
    prev: &Previous,
    task: &Task,
    records: &BTreeMap<Key, NodeRecord>,
    fresh: &[bool],
) -> bool {
    let same = |j: usize| unchanged(graph, prev, j, records, fresh);
    match (task.node.kind, task.key) {
        (Kind::Merger, _) => (0..graph.nodes.len())
            .filter(|&j| matches!(graph.nodes[j].kind, Kind::Generator | Kind::Critic))
            .all(same),
        // A per-need instance reads only the generator it audits.
        (_, (_, g)) if g > 0 => same(g - 1),
        _ => task
            .node
            .needs
            .iter()
            .chain(task.node.fork_from.iter())
            .all(|need| {
                graph
                    .nodes
                    .iter()
                    .position(|n| n.name == *need)
                    .is_some_and(same)
            }),
    }
}

/// What `task` would have produced, taken from the earlier bundle, or None
/// when its work is not there to take or has changed.
fn reuse_task(prev: &Previous, task: &Task, out: &Path) -> Option<(NodeRecord, Product)> {
    let node = &task.node;
    let old = prev.matching(node, &compose_system_prompt(node).ok()?)?;
    if let Job::Fork { prompt, .. } = &task.job {
        // A fork's lens rides in its user turn, which the system prompt the match compared does not hold.
        let before =
            std::fs::read_to_string(prev.dir.join("nodes").join(&node.name).join("prompt.md"))
                .ok()?;
        if before != *prompt {
            return None;
        }
    }
    let product = match &task.job {
        Job::Mechanical(_) => return None,
        Job::Generator(_) => Product::Findings(prev.output(&node.name).ok()?),
        Job::Fork { .. } => Product::Findings(prev.output(&node.name).ok()?),
        Job::Prewarm(_) => {
            let out: PrewarmOutput = prev.output(&node.name).ok()?;
            // Forks cannot resume a session that was not kept.
            out.session_id.as_ref()?;
            Product::Prewarm(out)
        }
        Job::Critic { .. } => Product::Verdicts(prev.output(&node.name).ok()?),
        Job::Merger { .. } => Product::Groups(prev.output(&node.name).ok()?),
    };
    if prev.copy_node(&node.name, out).is_err() {
        // A half-copied directory must not pass for the node's own.
        let _ = std::fs::remove_dir_all(out.join("nodes").join(&node.name));
        return None;
    }
    Some((NodeRecord::reused(node, old, prev), product))
}

/// Runs the graph and writes the bundle. An `Err` means the bundle itself
/// could not be written; a node failing is a completed run that says so.
///
/// A node starts as soon as everything it waits for has finished and one of
/// `opts.concurrency` slots is free. A node that is left out is recorded the
/// moment its turn comes and takes no slot. A per-need critic is not started
/// as a node: an instance is queued for each generator it needs as that
/// generator finishes with findings.
pub fn run(
    graph: &Graph,
    t: &Target,
    opts: &Options,
    providers: Providers,
    events: Events,
) -> Result<Summary, String> {
    let started_at = now();
    let started = Instant::now();
    if let Some(p) = opts.reuse.as_ref().filter(|p| p.in_place) {
        if p.dir != opts.out {
            return Err(format!(
                "resuming {} but writing {}",
                p.dir.display(),
                opts.out.display()
            ));
        }
        p.settings_match(graph)?;
    }
    let write = |name: &str, body: &str| {
        std::fs::write(opts.out.join(name), body)
            .map_err(|e| format!("{}: {e}", opts.out.join(name).display()))
    };
    std::fs::create_dir_all(&opts.out).map_err(|e| format!("{}: {e}", opts.out.display()))?;
    write("diff.patch", &t.diff)?;
    if let Some(body) = &t.pr_body {
        write("pr-body.md", body)?;
    }
    let diff_path = opts.out.join("diff.patch");
    graph.waves()?;

    let n = graph.nodes.len();
    let deps: Vec<Vec<usize>> = graph
        .nodes
        .iter()
        .map(|node| {
            graph
                .waits_for(node)
                .iter()
                .filter_map(|d| graph.nodes.iter().position(|o| o.name == *d))
                .collect()
        })
        .collect();
    let mut state = vec![State::Pending; n];
    // Instances of each per-need critic, queued or running, and ever made.
    let mut outstanding = vec![0usize; n];
    let mut made = vec![0usize; n];
    // Whether an instance of each per-need critic ran afresh rather than being reused.
    let mut fresh = vec![false; n];
    let mut records: BTreeMap<Key, NodeRecord> = BTreeMap::new();
    let mut outputs: HashMap<String, GeneratorOutput> = HashMap::new();
    let mut verdicts: BTreeMap<Key, Vec<Verdict>> = BTreeMap::new();
    let mut audited = false;
    let mut groups: Vec<MergeGroup> = Vec::new();
    let mut mech: BTreeMap<usize, (Vec<Diagnosed>, Vec<String>)> = BTreeMap::new();
    let mut mech_context: BTreeMap<usize, Vec<ContextLine>> = BTreeMap::new();
    // Each prewarm node's session and model, once it has one to fork.
    let mut sessions: HashMap<String, (String, Option<String>)> = HashMap::new();
    let base_fired: BTreeMap<String, String> = graph
        .triggers
        .iter()
        .filter_map(|tr| {
            tr.fired_by(&t.changed_files, &t.diff)
                .map(|by| (tr.name.clone(), by))
        })
        .collect();
    // The mechanical node whose failure stopped the graph.
    let mut stopped: Option<String> = None;
    let diagnostics_of = |mech: &BTreeMap<usize, (Vec<Diagnosed>, Vec<String>)>| -> Vec<Diagnosed> {
        mech.values().flat_map(|(d, _)| d.iter().cloned()).collect()
    };

    let conventions = Conventions::collect(&t.workspace, &t.changed_files);
    let mut conventions_record = conventions.record(graph.conventions_budget);
    conventions_record["modes"] = graph
        .nodes
        .iter()
        .filter(|n| n.kind == Kind::Generator)
        .map(|n| (n.name.clone(), json!(graph.conventions_of(n).as_str())))
        .collect::<serde_json::Map<_, _>>()
        .into();
    // The manifest as the run stands: written after every node finishes, so a
    // run that dies leaves a bundle `--resume` can finish, and once more at
    // the end. A resume keeps the first start and the records of nodes it has
    // not got to yet.
    let resuming = opts.reuse.as_ref().filter(|p| p.in_place);
    let manifest_of = |records: &[&NodeRecord],
                       outputs: &HashMap<String, GeneratorOutput>,
                       audited: bool,
                       diagnostics: usize,
                       fired: &BTreeMap<String, String>,
                       in_progress: bool|
     -> Value {
        let mut usage = Usage::default();
        records.iter().for_each(|r| usage.add(&r.usage));
        let ran = |kind: Kind| -> Vec<&NodeRecord> {
            records
                .iter()
                .copied()
                .filter(|r| r.kind == kind.as_str() && r.status != "skipped")
                .collect()
        };
        let models: serde_json::Map<String, Value> = records
            .iter()
            .map(|r| (r.name.clone(), json!(r.model)))
            .collect();
        let mut nodes: Vec<Value> = records.iter().map(|r| json!(r)).collect();
        if let Some(p) = resuming.filter(|_| in_progress) {
            let have: HashSet<&str> = records.iter().map(|r| r.name.as_str()).collect();
            nodes.extend(p.records_except(&have));
        }
        let mut m = json!({
            "run_id": resuming.and_then(|p| p.run_id.clone()).unwrap_or_else(|| opts.run_id.clone()),
            "captured_at": now(),
            "source": "review-run",
            "harness_version": env!("CARGO_PKG_VERSION"),
            "graph": graph.name,
            "repo_path": t.workspace,
            "repo": t.workspace.file_name().map(|n| n.to_string_lossy().into_owned()),
            "owner_repo": t.owner_repo,
            "pr_number": t.pr_number,
            "branch": t.branch,
            "base_sha": t.base_sha,
            "head_sha": t.head_sha,
            "uncommitted": t.uncommitted,
            "started_at": resuming.and_then(|p| p.started_at.clone()).unwrap_or_else(|| started_at.clone()),
            "ended_at": now(),
            "wall_ms": resuming.map_or(0, |p| p.wall_ms) + started.elapsed().as_millis() as u64,
            "lenses": ran(Kind::Generator).iter().map(|r| &r.name).collect::<Vec<_>>(),
            "models": models,
            "session_id": records.iter().find_map(|r| r.session_id.clone()),
            "prewarm_session_id": records.iter().find(|r| r.kind == Kind::Prewarm.as_str()).and_then(|r| r.session_id.clone()),
            "no_prewarm": opts.no_prewarm,
            "no_gh": opts.no_gh.is_some(),
            "warnings": records.iter().flat_map(|r| r.warnings.iter().map(move |w| format!("{}: {w}", r.name))).collect::<Vec<_>>(),
            "audited": audited,
            "diagnostics": diagnostics,
            "conventions": conventions_record,
            "triggers_fired": fired,
            "cost_usd": nodes.iter().filter_map(|n| n["cost_usd"].as_f64()).sum::<f64>(),
            "usage": usage,
            "nodes": nodes,
            "upstream_reading": graph.nodes.iter().filter_map(|n| outputs.get(&n.name).map(|o| (n.name.clone(), json!(o.upstream_reading)))).collect::<serde_json::Map<_, _>>(),
            "meta": graph.nodes.iter().filter_map(|n| outputs.get(&n.name).map(|o| (n.name.clone(), json!(o.meta)))).collect::<serde_json::Map<_, _>>(),
        });
        if in_progress {
            m["in_progress"] = json!(true);
        }
        if let Some(p) = resuming {
            m["resumed_at"] = json!(p
                .resumed_at
                .iter()
                .cloned()
                .chain([started_at.clone()])
                .collect::<Vec<_>>());
        }
        m
    };
    // Through a temporary file, so a kill mid-write never truncates the last good manifest.
    let write_manifest = |m: Value| -> Result<(), String> {
        let tmp = format!("manifest.json.{}", std::process::id());
        write(
            &tmp,
            &(serde_json::to_string_pretty(&m).expect("serializes") + "\n"),
        )?;
        std::fs::rename(opts.out.join(&tmp), opts.out.join("manifest.json"))
            .map_err(|e| format!("{}: {e}", opts.out.join("manifest.json").display()))
    };
    if resuming.is_none() {
        write_manifest(manifest_of(&[], &outputs, false, 0, &base_fired, true))?;
    }
    let slots = opts.concurrency.max(1);
    std::thread::scope(|scope| -> Result<(), String> {
        let (tx, rx) = std::sync::mpsc::channel::<Done>();
        let mut queue: VecDeque<Task> = VecDeque::new();
        let mut running = 0usize;
        let leave = |records: &mut BTreeMap<Key, NodeRecord>, node: &Node, key: Key, why: &str| {
            let rec = NodeRecord::skipped(node, why);
            events(json!({"type": "node_end", "node": rec}));
            records.insert(key, rec);
        };
        loop {
            // Settle every node whose turn has come; one left out can free
            // the next, so go round until nothing changes.
            loop {
                let mut changed = false;
                for i in 0..n {
                    let node = &graph.nodes[i];
                    let waiting = !deps[i].iter().all(|&d| state[d] == State::Finished);
                    if state[i] != State::Pending
                        || outstanding[i] > 0
                        || (waiting && stopped.is_none())
                    {
                        continue;
                    }
                    changed = true;
                    let fired = fired_now(&base_fired, graph, &mech);
                    if let Some(why) = left_out(node, opts, &stopped, &fired) {
                        leave(&mut records, node, (i, 0), &why);
                        state[i] = State::Finished;
                        continue;
                    }
                    let mut tnode = node.clone();
                    let mut warnings: Vec<String> = Vec::new();
                    let job = match node.kind {
                        Kind::Mechanical => {
                            if opts.no_gh.is_some() {
                                warnings.push("ran with GitHub disabled (--no-gh): any `gh` lookup in this stage fails".into());
                            }
                            Job::Mechanical(mechanical_contract(t, node, &opts.out).to_string())
                        }
                        Kind::Critic if node.per_need => {
                            if made[i] == 0 {
                                leave(&mut records, node, (i, 0), "no findings to audit");
                            }
                            state[i] = State::Finished;
                            continue;
                        }
                        Kind::Merger => {
                            let all: Vec<Verdict> = verdicts.values().flatten().cloned().collect();
                            let reported =
                                aggregate(&indexed(graph, &outputs, None), &all, &[]).reported;
                            if !lenses_share_a_file(&reported) {
                                leave(
                                    &mut records,
                                    node,
                                    (i, 0),
                                    "no findings from different lenses share a file",
                                );
                                state[i] = State::Finished;
                                continue;
                            }
                            let indices = reported.iter().map(|r| r.index.clone()).collect();
                            Job::Merger {
                                prompt: merger_prompt(t, &diff_path, &reported),
                                indices,
                            }
                        }
                        Kind::Critic => {
                            let inputs = indexed(graph, &outputs, Some(&node.needs));
                            if inputs.is_empty() {
                                leave(&mut records, node, (i, 0), "no findings to audit");
                                state[i] = State::Finished;
                                continue;
                            }
                            let indices = inputs.iter().map(|(i, _)| i.clone()).collect();
                            Job::Critic {
                                prompt: critic_prompt(
                                    t,
                                    &diff_path,
                                    &inputs,
                                    &critic_conventions(graph, &conventions, &node.needs),
                                ),
                                indices,
                            }
                        }
                        Kind::Prewarm => {
                            let context: Vec<ContextLine> =
                                mech_context.values().flatten().cloned().collect();
                            Job::Prewarm(prewarm_prompt(
                                t,
                                node,
                                &diff_path,
                                &context,
                                &diagnostics_of(&mech),
                                &conventions.section(node.conventions, graph.conventions_budget),
                            ))
                        }
                        _ => {
                            let earlier = indexed(graph, &outputs, Some(&node.needs));
                            let context: Vec<ContextLine> =
                                mech_context.values().flatten().cloned().collect();
                            let forked = node
                                .fork_from
                                .as_deref()
                                .and_then(|from| sessions.get(from).map(|s| (from, s)));
                            if let Some((from, (session, model))) = forked {
                                if *model != node.model {
                                    warnings.push(format!(
                                        "model {} differs from prewarm `{from}`'s {}: the forked transcript carries over but its prompt cache does not, so this fork pays to write the whole prefix again",
                                        node.model.as_deref().unwrap_or("(default)"),
                                        model.as_deref().unwrap_or("(default)")
                                    ));
                                }
                                match fork_prompt(node, &earlier) {
                                    Ok(prompt) => Job::Fork {
                                        prompt,
                                        session: session.clone(),
                                    },
                                    Err(e) => {
                                        let mut rec = NodeRecord::new(node);
                                        rec.status = "failed";
                                        rec.reason = Some(e);
                                        events(json!({"type": "node_end", "node": rec}));
                                        records.insert((i, 0), rec);
                                        state[i] = State::Finished;
                                        continue;
                                    }
                                }
                            } else {
                                if let Some(from) = &node.fork_from {
                                    // Not forked: run it as the plain generator its graph would be without a prewarm node.
                                    tnode = graph.unforked(node);
                                    if !opts.no_prewarm {
                                        let why = graph
                                            .nodes
                                            .iter()
                                            .position(|n| n.name == *from)
                                            .and_then(|p| records.get(&(p, 0)))
                                            .map_or("it did not run".to_string(), |r| {
                                                r.reason
                                                    .clone()
                                                    .unwrap_or_else(|| r.status.to_string())
                                            });
                                        warnings.push(format!("prewarm `{from}` left no session to fork ({why}); ran as a plain generator with the lens in the system prompt"));
                                    }
                                }
                                Job::Generator(generator_prompt(
                                    t,
                                    &tnode,
                                    &diff_path,
                                    &context,
                                    &diagnostics_of(&mech),
                                    &earlier,
                                    &conventions
                                        .section(node.conventions, graph.conventions_budget),
                                ))
                            }
                        }
                    };
                    state[i] = State::Started;
                    queue.push_back(Task {
                        key: (i, 0),
                        node: tnode,
                        job,
                        warnings,
                    });
                }
                if !changed {
                    break;
                }
            }
            while running < slots {
                let Some(task) = queue.pop_front() else { break };
                if let Some(prev) = opts
                    .reuse
                    .as_ref()
                    .filter(|p| upstream_unchanged(graph, p, &task, &records, &fresh))
                {
                    if let Some((rec, product)) = reuse_task(prev, &task, &opts.out) {
                        events(
                            json!({"type": "node_reused", "node": task.node.name, "kind": task.node.kind.as_str(), "from": prev.dir}),
                        );
                        events(json!({"type": "node_end", "node": rec}));
                        // Handled like a finished child, without taking a slot from one.
                        let _ = tx.send(Done {
                            key: task.key,
                            rec,
                            product,
                        });
                        running += 1;
                        continue;
                    }
                }
                if opts.reuse.as_ref().is_some_and(|p| p.in_place) {
                    // What an earlier try left here must not pass for this one's.
                    let _ = std::fs::remove_dir_all(opts.out.join("nodes").join(&task.node.name));
                } else if let (Some(prev), Job::Mechanical(_)) = (&opts.reuse, &task.job) {
                    // A mechanical node always runs again; its old `out` is there for it to build on.
                    prev.seed_out(&task.node.name, &opts.out);
                }
                events(
                    json!({"type": "node_start", "node": task.node.name, "kind": task.node.kind.as_str()}),
                );
                let tx = tx.clone();
                let provider = providers.get(task.node.provider);
                scope.spawn(move || {
                    // Whatever happens, the scheduler is told, or it would wait for ever.
                    let ended = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let (rec, product) = work(&task, t, opts, provider);
                        events(json!({"type": "node_end", "node": rec}));
                        (rec, product)
                    }));
                    let (rec, product) = ended.unwrap_or_else(|_| {
                        let mut rec = NodeRecord::new(&task.node);
                        rec.status = "failed";
                        rec.reason = Some("node thread panicked".into());
                        (rec, Product::Nothing)
                    });
                    let _ = tx.send(Done {
                        key: task.key,
                        rec,
                        product,
                    });
                });
                running += 1;
            }
            if running == 0 {
                break;
            }
            let Done {
                key: (i, g),
                mut rec,
                product,
            } = rx.recv().map_err(|e| e.to_string())?;
            running -= 1;
            if let Some(old) = resuming
                .filter(|_| !rec.carried())
                .and_then(|p| p.record(&rec.name))
            {
                // An earlier try at this node spent what it spent, failed or not.
                rec.cost_usd += old["cost_usd"].as_f64().unwrap_or(0.0);
                rec.wall_ms += old["wall_ms"].as_u64().unwrap_or(0);
                rec.usage
                    .add(&serde_json::from_value(old["usage"].clone()).unwrap_or_default());
            }
            if g > 0 && !rec.carried() {
                fresh[i] = true;
            }
            if rec.kind == Kind::Mechanical.as_str() && rec.status == "failed" && stopped.is_none()
            {
                stopped = Some(rec.name.clone());
            }
            let mut found = false;
            match product {
                Product::Mechanical(o) => {
                    let diagnosed = o.diagnostics.into_iter().map(|diagnostic| Diagnosed {
                        node: rec.name.clone(),
                        diagnostic,
                    });
                    mech.insert(i, (diagnosed.collect(), o.triggers));
                    mech_context.insert(i, o.context);
                }
                Product::Prewarm(o) => {
                    if let Some(id) = &o.session_id {
                        sessions.insert(rec.name.clone(), (id.clone(), rec.model.clone()));
                    }
                }
                Product::Findings(o) => {
                    write(&format!("findings-{}.jsonl", rec.name), &jsonl(&o.findings))?;
                    found = !o.findings.is_empty();
                    outputs.insert(rec.name.clone(), o);
                }
                Product::Verdicts(v) => {
                    audited = true;
                    verdicts.insert((i, g), v);
                }
                Product::Groups(g) => groups = g,
                Product::Nothing => {}
            }
            records.insert((i, g), rec);
            let so_far: Vec<&NodeRecord> = records.values().collect();
            write_manifest(manifest_of(
                &so_far,
                &outputs,
                audited,
                diagnostics_of(&mech).len(),
                &fired_now(&base_fired, graph, &mech),
                true,
            ))?;
            if g > 0 {
                outstanding[i] -= 1;
                continue;
            }
            state[i] = State::Finished;
            if !found {
                continue;
            }
            // The generator's findings can be audited now.
            let fired = fired_now(&base_fired, graph, &mech);
            for (c, critic) in graph.nodes.iter().enumerate() {
                if !critic.per_need
                    || !critic.needs.contains(&graph.nodes[i].name)
                    || left_out(critic, opts, &stopped, &fired).is_some()
                {
                    continue;
                }
                let inputs = indexed(graph, &outputs, Some(&[graph.nodes[i].name.clone()]));
                let indices = inputs.iter().map(|(i, _)| i.clone()).collect();
                let mut node = critic.clone();
                node.name = critic.instance_name(&graph.nodes[i].name);
                queue.push_back(Task {
                    key: (c, i + 1),
                    node,
                    warnings: Vec::new(),
                    job: Job::Critic {
                        prompt: critic_prompt(
                            t,
                            &diff_path,
                            &inputs,
                            &critic_conventions(
                                graph,
                                &conventions,
                                &[graph.nodes[i].name.clone()],
                            ),
                        ),
                        indices,
                    },
                });
                outstanding[c] += 1;
                made[c] += 1;
            }
        }
        Ok(())
    })?;

    let records: Vec<NodeRecord> = records.into_values().collect();
    let verdicts: Vec<Verdict> = verdicts.into_values().flatten().collect();
    let diagnostics = diagnostics_of(&mech);
    let fired = fired_now(&base_fired, graph, &mech);

    let mut agg = aggregate(&indexed(graph, &outputs, None), &verdicts, &groups);
    add_permalinks(t, &mut agg);
    let report = render(t, &agg, &records, audited, &diagnostics);
    write("verdicts.jsonl", &jsonl(&verdicts))?;
    write("merges.jsonl", &jsonl(&groups))?;
    write(
        "findings.jsonl",
        &jsonl(agg.reported.iter().chain(&agg.decided).chain(&agg.merged)),
    )?;
    write("diagnostics.jsonl", &jsonl(&diagnostics))?;
    write("report.md", &report)?;

    let all: Vec<&NodeRecord> = records.iter().collect();
    write_manifest(manifest_of(
        &all,
        &outputs,
        audited,
        diagnostics.len(),
        &fired,
        false,
    ))?;

    let complete = records.iter().all(|r| r.status != "failed");
    Ok(Summary {
        nodes: records,
        report,
        complete,
    })
}

/// Findings in graph order, each under the index `<node>:<n>` that verdicts
/// and the report refer to it by.
fn indexed(
    graph: &Graph,
    outputs: &HashMap<String, GeneratorOutput>,
    only: Option<&[String]>,
) -> Vec<(String, Finding)> {
    graph
        .nodes
        .iter()
        .filter(|n| only.is_none_or(|names| names.contains(&n.name)))
        .filter_map(|n| outputs.get(&n.name).map(|o| (n, o)))
        .flat_map(|(n, o)| {
            o.findings
                .iter()
                .enumerate()
                .map(move |(i, f)| (format!("{}:{i}", n.name), f.clone()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Outcome;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
    use std::sync::{mpsc, Mutex};
    use std::time::Duration;

    #[derive(Debug, Clone)]
    struct Call {
        node: String,
        attempt: u32,
        resume: Option<String>,
        fork: bool,
        prompt: String,
    }

    /// Hands each node its queued outcomes in order and records what it was asked.
    #[derive(Default)]
    struct Stub {
        queues: Mutex<HashMap<String, VecDeque<Outcome>>>,
        calls: Mutex<Vec<Call>>,
        /// A node listed here does not return until its sender fires or drops.
        gates: Mutex<HashMap<String, mpsc::Receiver<()>>>,
        active: AtomicUsize,
        /// The most nodes the stub was running at once.
        peak: AtomicUsize,
    }

    impl Stub {
        fn with(plan: Vec<(&str, Vec<Outcome>)>) -> Stub {
            let stub = Stub::default();
            for (node, outcomes) in plan {
                stub.queues
                    .lock()
                    .unwrap()
                    .insert(node.to_string(), outcomes.into());
            }
            stub
        }

        fn calls_to(&self, node: &str) -> Vec<Call> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c.node == node)
                .cloned()
                .collect()
        }
    }

    impl Provider for Stub {
        fn command(&self, inv: &Invocation) -> Result<Vec<String>, String> {
            Ok(match inv.node.provider {
                // What reuse reads back of a claude node's command line.
                ProviderKind::Claude => {
                    vec!["claude".into(), "--tools".into(), inv.node.tools.join(",")]
                }
                ProviderKind::Command => inv.node.command.clone(),
            })
        }

        fn run(&self, inv: &Invocation) -> Outcome {
            self.calls.lock().unwrap().push(Call {
                node: inv.node.name.clone(),
                attempt: inv.attempt,
                resume: inv.resume.map(str::to_string),
                fork: inv.fork,
                prompt: inv.prompt.to_string(),
            });
            if inv.node.name == "fetch" {
                // A script that keeps what it fetched and does not fetch it again.
                let notes = inv.dir.join("out").join("notes.md");
                if !notes.exists() {
                    std::fs::write(&notes, "release notes").unwrap();
                }
            }
            let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            let gate = self.gates.lock().unwrap().remove(&inv.node.name);
            if let Some(gate) = gate {
                let _ = gate.recv();
            }
            self.active.fetch_sub(1, Ordering::SeqCst);
            let next = self
                .queues
                .lock()
                .unwrap()
                .get_mut(&inv.node.name)
                .and_then(VecDeque::pop_front);
            next.unwrap_or_else(|| Outcome::failed(format!("nothing queued for {}", inv.node.name)))
        }
    }

    fn ok(output: Value, session: Option<&str>) -> Outcome {
        Outcome {
            output: Ok(output),
            session_id: session.map(str::to_string),
            turns: 3,
            cost_usd: 0.5,
            ..Outcome::failed("")
        }
    }

    fn rejected(session: Option<&str>) -> Outcome {
        Outcome {
            session_id: session.map(str::to_string),
            ..Outcome::failed("no structured output")
        }
    }

    fn finding(file: &str, line: u64, severity: &str, category: &str) -> Value {
        json!({
            "file": file, "line": line, "severity": severity, "confidence": "high", "category": category,
            "description": format!("description of {file}:{line} {category}"),
            "why_it_matters": "it matters", "invariant": "it holds", "recommendation": "fix it", "verify": "by inspection",
        })
    }

    fn found(findings: Vec<Value>) -> Value {
        json!({"findings": findings, "upstream_reading": [], "meta": "read the diff"})
    }

    fn verdict(index: &str, kind: &str) -> Value {
        json!({"index": index, "verdict": kind, "reason": format!("because {kind}")})
    }

    fn graph(generators: &[&str], critic: bool) -> Graph {
        let node = "provider = \"command\"\ncommand = [\"true\"]\n";
        let mut text = String::new();
        for g in generators {
            text.push_str(&format!(
                "[[node]]\nname = \"{g}\"\nkind = \"generator\"\n{node}"
            ));
        }
        if critic {
            let needs: Vec<String> = generators.iter().map(|g| format!("\"{g}\"")).collect();
            text.push_str(&format!(
                "[[node]]\nname = \"audit\"\nkind = \"critic\"\nneeds = [{}]\n{node}",
                needs.join(", ")
            ));
        }
        Graph::parse(&text, Path::new("."), "test").unwrap()
    }

    fn scratch() -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "review-run-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn target(workspace: &Path) -> Target {
        Target {
            workspace: workspace.to_path_buf(),
            base_sha: "b".repeat(40),
            head_sha: "a".repeat(40),
            branch: "feature".into(),
            owner_repo: None,
            uncommitted: false,
            diff: "diff --git a/a.rs b/a.rs\n".into(),
            changed_files: vec!["a.rs".into()],
            pr_number: None,
            pr_body: None,
        }
    }

    struct Ran {
        summary: Summary,
        out: PathBuf,
        stub: Stub,
    }

    impl Ran {
        fn read(&self, name: &str) -> String {
            std::fs::read_to_string(self.out.join(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
        }

        fn rows(&self, name: &str) -> Vec<Value> {
            self.read(name)
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect()
        }

        fn node(&self, name: &str) -> &NodeRecord {
            self.summary.nodes.iter().find(|n| n.name == name).unwrap()
        }
    }

    fn go(graph: &Graph, plan: Vec<(&str, Vec<Outcome>)>, skip: &[&str]) -> Ran {
        go_on(graph, |_| {}, plan, skip)
    }

    /// `go` over a change that `edit` has altered from the default one.
    fn go_on(
        graph: &Graph,
        edit: impl FnOnce(&mut Target),
        plan: Vec<(&str, Vec<Outcome>)>,
        skip: &[&str],
    ) -> Ran {
        go_from(graph, None, edit, plan, skip)
    }

    /// `go_on` reusing what it can of the bundle `earlier` left.
    fn go_from(
        graph: &Graph,
        earlier: Option<&Ran>,
        edit: impl FnOnce(&mut Target),
        plan: Vec<(&str, Vec<Outcome>)>,
        skip: &[&str],
    ) -> Ran {
        let ws = scratch();
        let out = ws.join("bundle");
        let stub = Stub::with(plan);
        let mut t = target(&ws);
        edit(&mut t);
        let reuse = earlier.map(|e| Previous::open(&e.out, &t).unwrap());
        let opts = Options {
            out: out.clone(),
            run_id: "test-run".into(),
            skip: skip.iter().map(|s| s.to_string()).collect(),
            concurrency: 4,
            reuse,
            no_prewarm: false,
            no_gh: None,
        };
        let summary = run(
            graph,
            &t,
            &opts,
            Providers {
                claude: &stub,
                command: &stub,
            },
            &|_| {},
        )
        .unwrap();
        Ran { summary, out, stub }
    }

    /// A graph from `[triggers]` text and node lines like `generator compat trigger = ["ddl"]`.
    fn graph_of(triggers: &str, nodes: &[(&str, &str, &str)]) -> Graph {
        let mut text = format!("[triggers]\n{triggers}\n");
        for (kind, name, extra) in nodes {
            text.push_str(&format!("[[node]]\nname = \"{name}\"\nkind = \"{kind}\"\nprovider = \"command\"\ncommand = [\"true\"]\n{extra}\n"));
        }
        Graph::parse(&text, Path::new("."), "test").unwrap()
    }

    const DDL: &str = "ddl = { touches = [\"**/migrations/**\", \"**/*.sql\"] }";

    fn manifest(ran: &Ran) -> Value {
        serde_json::from_str(&ran.read("manifest.json")).unwrap()
    }

    fn clean() -> Vec<Outcome> {
        vec![ok(found(vec![]), Some("g"))]
    }

    /// Generators `a` (inline unless told otherwise) and `b` (per `b_extra`), one critic over both.
    fn conv_graph(b_extra: &str, a_extra: &str) -> Graph {
        let node = "provider = \"command\"\ncommand = [\"true\"]\n";
        Graph::parse(
            &format!(
                "[defaults]\nconventions = \"inline\"\nconventions_budget = 60\n\
                 [[node]]\nname = \"a\"\nkind = \"generator\"\n{a_extra}{node}\
                 [[node]]\nname = \"b\"\nkind = \"generator\"\n{b_extra}{node}\
                 [[node]]\nname = \"audit\"\nkind = \"critic\"\nneeds = [\"a\", \"b\"]\nper_need = true\n{node}"
            ),
            Path::new("."),
            "test",
        )
        .unwrap()
    }

    fn with_rules(t: &mut Target) {
        let ws = t.workspace.clone();
        std::fs::write(ws.join("AGENTS.md"), "root rule: use goqu").unwrap();
        std::fs::create_dir_all(ws.join("sub")).unwrap();
        std::fs::write(ws.join("sub/AGENTS.md"), "x".repeat(80)).unwrap();
        t.changed_files = vec!["sub/a.rs".into()];
    }

    fn convention_plan() -> Vec<(&'static str, Vec<Outcome>)> {
        vec![
            (
                "a",
                vec![ok(
                    found(vec![finding("a.rs", 1, "P2", "style")]),
                    Some("ga"),
                )],
            ),
            (
                "b",
                vec![ok(
                    found(vec![finding("a.rs", 2, "P2", "style")]),
                    Some("gb"),
                )],
            ),
            (
                "audit-a",
                vec![ok(json!({"verdicts": [verdict("a:0", "kept")]}), Some("c"))],
            ),
            (
                "audit-b",
                vec![ok(json!({"verdicts": [verdict("b:0", "kept")]}), Some("c"))],
            ),
        ]
    }

    #[test]
    fn an_inline_generator_is_handed_the_rules_that_fit_and_a_critic_the_listing() {
        let g = conv_graph("", "");
        let ran = go_on(&g, with_rules, convention_plan(), &[]);
        let prompt = &ran.stub.calls_to("a")[0].prompt;
        assert!(prompt.contains("<file path=\"AGENTS.md\" applies-to=\"the whole repo\">\nroot rule: use goqu\n</file>"), "{prompt}");
        assert!(
            prompt.contains("sub/AGENTS.md (80 bytes): before reviewing code under sub/"),
            "over the 60-byte budget, so listed"
        );
        assert!(!prompt.contains(&"x".repeat(80)));
        assert!(prompt.find("project conventions").unwrap() < prompt.find("The diff:").unwrap());

        let critic = &ran.stub.calls_to("audit-a")[0].prompt;
        assert!(
            critic.contains("AGENTS.md (19 bytes): applies to the whole repo"),
            "{critic}"
        );
        assert!(
            !critic.contains("root rule: use goqu"),
            "a critic gets paths, not contents"
        );

        let m = manifest(&ran);
        assert_eq!(m["conventions"]["budget"], 60);
        assert_eq!(
            m["conventions"]["files"][0],
            json!({"path": "AGENTS.md", "scope": "", "bytes": 19, "inlined": true})
        );
        assert_eq!(m["conventions"]["files"][1]["inlined"], false);
        assert_eq!(
            m["conventions"]["modes"],
            json!({"a": "inline", "b": "inline"})
        );
    }

    #[test]
    fn a_node_can_list_or_drop_conventions_and_a_critic_follows_what_its_generator_got() {
        let g = conv_graph("conventions = \"none\"\n", "conventions = \"list\"\n");
        let ran = go_on(&g, with_rules, convention_plan(), &[]);
        let a = &ran.stub.calls_to("a")[0].prompt;
        assert!(
            a.contains("Read each before reviewing code under that directory")
                && !a.contains("root rule: use goqu")
        );
        let b = &ran.stub.calls_to("b")[0].prompt;
        assert!(
            !b.contains("conventions") && !b.contains("AGENTS.md"),
            "{b}"
        );
        assert!(ran.stub.calls_to("audit-a")[0].prompt.contains("AGENTS.md"));
        assert!(!ran.stub.calls_to("audit-b")[0].prompt.contains("AGENTS.md"));
        assert_eq!(
            manifest(&ran)["conventions"]["modes"],
            json!({"a": "list", "b": "none"})
        );
    }

    #[test]
    fn verdicts_decide_what_the_report_shows() {
        let g = graph(&["code", "releng"], true);
        let ran = go(
            &g,
            vec![
                (
                    "code",
                    vec![ok(
                        found(vec![
                            finding("a.rs", 1, "P1", "logic"),
                            finding("a.rs", 5, "P1", "errors"),
                        ]),
                        Some("g1"),
                    )],
                ),
                (
                    "releng",
                    vec![ok(
                        found(vec![
                            finding("b.rs", 2, "P2", "rollout"),
                            finding("c.rs", 3, "P2", "alerts"),
                        ]),
                        Some("g2"),
                    )],
                ),
                (
                    "audit",
                    vec![ok(
                        json!({"verdicts": [
                            verdict("code:0", "false-positive"),
                            {"index": "code:1", "verdict": "downgraded", "from": "P1", "to": "P3", "reason": "narrow"},
                            verdict("releng:0", "pre-existing"),
                            verdict("releng:1", "kept"),
                        ]}),
                        Some("c1"),
                    )],
                ),
            ],
            &[],
        );
        assert!(ran.summary.complete);

        let kept = ran.rows("findings.jsonl");
        let indices: Vec<&str> = kept.iter().map(|r| r["index"].as_str().unwrap()).collect();
        assert_eq!(
            indices,
            ["releng:1", "code:1", "releng:0"],
            "reported by severity, then what was already decided"
        );
        assert_eq!(kept[1]["severity"], "P3");
        assert_eq!(kept[2]["disposition"], "already-decided");

        let report = ran.read("report.md");
        assert!(
            !report.contains("## P1"),
            "the false positive's severity section is gone:\n{report}"
        );
        assert!(
            !report.contains("description of a.rs:1 logic"),
            "the dropped finding is not described"
        );
        let (sections, rest) = report.split_once("## Already decided").unwrap();
        assert!(
            sections.contains("## P2")
                && sections.contains("## P3")
                && sections.contains("downgraded from P1")
        );
        let (decided, dropped) = rest.split_once("## Dropped by the auditor").unwrap();
        assert!(decided.contains("b.rs:2"));
        assert!(dropped.contains("`code:0`") && dropped.contains("because false-positive"));
        assert!(!report.contains("Not audited") && !report.contains("Incomplete"));

        for name in [
            "manifest.json",
            "diff.patch",
            "report.md",
            "verdicts.jsonl",
            "findings.jsonl",
            "findings-code.jsonl",
            "findings-releng.jsonl",
        ] {
            assert!(ran.out.join(name).is_file(), "bundle lacks {name}");
        }
        assert_eq!(ran.rows("verdicts.jsonl").len(), 4);
        let manifest: Value = serde_json::from_str(&ran.read("manifest.json")).unwrap();
        assert_eq!(manifest["audited"], true);
        assert_eq!(manifest["lenses"], json!(["code", "releng"]));
    }

    #[test]
    fn rejected_output_is_retried_once_in_the_same_session() {
        let g = graph(&["code"], false);
        let ran = go(
            &g,
            vec![(
                "code",
                vec![rejected(Some("s1")), ok(found(vec![]), Some("s1"))],
            )],
            &[],
        );
        let calls = ran.stub.calls_to("code");
        assert_eq!(calls.iter().map(|c| c.attempt).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(calls[0].resume, None);
        assert_eq!(calls[1].resume.as_deref(), Some("s1"));
        assert!(
            calls[1].prompt.contains("rejected")
                && calls[1].prompt.contains("no structured output")
        );
        let n = ran.node("code");
        assert_eq!((n.attempts, n.status), (2, "ok"));
        assert!(n
            .retry_reason
            .as_deref()
            .unwrap()
            .contains("no structured output"));
        assert_eq!(n.reason, None);
        assert!(ran.summary.complete);
    }

    #[test]
    fn output_failing_the_schema_is_rejected_with_the_reason() {
        let g = graph(&["code"], false);
        let bad = ok(json!({"findings": [{"file": "a.rs"}]}), Some("s1"));
        let ran = go(
            &g,
            vec![("code", vec![bad, ok(found(vec![]), Some("s1"))])],
            &[],
        );
        assert!(ran.stub.calls_to("code")[1]
            .prompt
            .contains("does not match the schema"));
    }

    #[test]
    fn two_rejections_fail_the_node_and_the_critic_audits_the_rest() {
        let g = graph(&["code", "releng"], true);
        let ran = go(
            &g,
            vec![
                ("code", vec![rejected(Some("s1")), rejected(Some("s1"))]),
                (
                    "releng",
                    vec![ok(
                        found(vec![finding("b.rs", 2, "P2", "rollout")]),
                        Some("g2"),
                    )],
                ),
                (
                    "audit",
                    vec![ok(
                        json!({"verdicts": [verdict("releng:0", "kept")]}),
                        Some("c1"),
                    )],
                ),
            ],
            &[],
        );
        assert_eq!(ran.stub.calls_to("code").len(), 2);
        assert_eq!(ran.node("code").status, "failed");
        assert_eq!(ran.node("code").attempts, 2);
        assert!(!ran.summary.complete);
        assert!(ran.summary.report.contains("**Incomplete:** `code` failed"));
        let audit = ran.stub.calls_to("audit");
        assert_eq!(audit.len(), 1);
        assert!(audit[0].prompt.contains("releng:0") && !audit[0].prompt.contains("code:"));
        assert_eq!(ran.node("audit").status, "ok");
    }

    #[test]
    fn a_failure_with_no_session_is_not_retried() {
        let g = graph(&["code"], false);
        let ran = go(
            &g,
            vec![("code", vec![Outcome::failed("could not start claude")])],
            &[],
        );
        assert_eq!(ran.stub.calls_to("code").len(), 1);
        let n = ran.node("code");
        assert_eq!((n.status, n.attempts), ("failed", 1));
        assert_eq!(n.reason.as_deref(), Some("could not start claude"));
        assert!(!ran.summary.complete);
    }

    #[test]
    fn a_critic_that_skips_an_index_is_retried() {
        let g = graph(&["code"], true);
        let ran = go(
            &g,
            vec![
                (
                    "code",
                    vec![ok(
                        found(vec![
                            finding("a.rs", 1, "P1", "logic"),
                            finding("a.rs", 9, "P2", "errors"),
                        ]),
                        Some("g1"),
                    )],
                ),
                (
                    "audit",
                    vec![
                        ok(json!({"verdicts": [verdict("code:0", "kept")]}), Some("c1")),
                        ok(
                            json!({"verdicts": [verdict("code:0", "kept"), verdict("code:1", "kept")]}),
                            Some("c1"),
                        ),
                    ],
                ),
            ],
            &[],
        );
        let calls = ran.stub.calls_to("audit");
        assert_eq!(calls.len(), 2);
        assert!(calls[1].prompt.contains("no verdict for code:1"));
        assert_eq!(calls[1].resume.as_deref(), Some("c1"));
        assert_eq!(
            (ran.node("audit").status, ran.node("audit").attempts),
            ("ok", 2)
        );
        assert_eq!(ran.rows("verdicts.jsonl").len(), 2);
    }

    #[test]
    fn a_skipped_generator_is_recorded_and_the_critic_proceeds() {
        let g = graph(&["code", "releng"], true);
        let ran = go(
            &g,
            vec![
                (
                    "code",
                    vec![ok(
                        found(vec![finding("a.rs", 1, "P1", "logic")]),
                        Some("g1"),
                    )],
                ),
                (
                    "audit",
                    vec![ok(
                        json!({"verdicts": [verdict("code:0", "kept")]}),
                        Some("c1"),
                    )],
                ),
            ],
            &["releng"],
        );
        assert!(ran.stub.calls_to("releng").is_empty());
        let n = ran.node("releng");
        assert_eq!(
            (n.status, n.reason.as_deref()),
            ("skipped", Some("skipped by request"))
        );
        assert_eq!(ran.node("audit").status, "ok");
        assert!(ran.summary.complete);
        let manifest: Value = serde_json::from_str(&ran.read("manifest.json")).unwrap();
        assert_eq!(manifest["lenses"], json!(["code"]));
    }

    #[test]
    fn a_critic_with_nothing_to_audit_is_skipped() {
        let g = graph(&["code"], true);
        let ran = go(&g, vec![("code", vec![ok(found(vec![]), Some("g1"))])], &[]);
        assert!(ran.stub.calls_to("audit").is_empty());
        let n = ran.node("audit");
        assert_eq!(
            (n.status, n.reason.as_deref()),
            ("skipped", Some("no findings to audit"))
        );
        assert!(ran.summary.complete);
        assert!(
            !ran.summary.report.contains("Not audited"),
            "nothing was left unaudited"
        );
    }

    #[test]
    fn findings_without_verdicts_say_they_were_not_audited() {
        let g = graph(&["code"], true);
        let ran = go(
            &g,
            vec![(
                "code",
                vec![ok(
                    found(vec![finding("a.rs", 1, "P1", "logic")]),
                    Some("g1"),
                )],
            )],
            &["audit"],
        );
        assert!(ran.summary.report.contains("**Not audited:**"));
        assert!(ran.summary.report.contains("## P1"));
        assert_eq!(ran.rows("findings.jsonl").len(), 1);
        let manifest: Value = serde_json::from_str(&ran.read("manifest.json")).unwrap();
        assert_eq!(manifest["audited"], false);
    }

    #[test]
    fn findings_on_one_line_and_category_merge_at_the_higher_severity() {
        let g = graph(&["code", "releng"], false);
        let ran = go(
            &g,
            vec![
                (
                    "code",
                    vec![ok(
                        found(vec![finding("a.rs", 10, "P2", "logic")]),
                        Some("g1"),
                    )],
                ),
                (
                    "releng",
                    vec![ok(
                        found(vec![finding("a.rs", 10, "P1", "logic")]),
                        Some("g2"),
                    )],
                ),
            ],
            &[],
        );
        let rows = ran.rows("findings.jsonl");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["severity"], "P1");
        let mut lenses: Vec<&str> = rows[0]["lenses"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l.as_str().unwrap())
            .collect();
        lenses.sort();
        assert_eq!(lenses, ["code", "releng"]);
        let description = rows[0]["description"].as_str().unwrap();
        assert!(
            description.contains("a.rs:10 logic")
                && description.matches("description of").count() == 2
        );
        // Per-node files keep each lens's own view.
        assert_eq!(ran.rows("findings-code.jsonl")[0]["severity"], "P2");
    }

    fn on_github(t: &mut Target) {
        t.owner_repo = Some("example/demo".into());
    }

    fn permalink_plan() -> Vec<(&'static str, Vec<Outcome>)> {
        let no_line = json!({
            "file": "docs/a b(1)#2?.md", "line": null, "severity": "P2", "confidence": "high", "category": "docs",
            "description": "d", "why_it_matters": "w", "invariant": "i", "recommendation": "r", "verify": "by inspection",
        });
        vec![
            (
                "code",
                vec![ok(
                    found(vec![
                        finding("src/a.rs", 42, "P1", "logic"),
                        finding("b.rs", 1, "P2", "errors"),
                        no_line,
                        finding("c.rs", 7, "P2", "bad"),
                    ]),
                    Some("g"),
                )],
            ),
            (
                "audit",
                vec![ok(
                    json!({"verdicts": [verdict("code:0", "kept"), verdict("code:1", "kept"), verdict("code:2", "kept"), verdict("code:3", "false-positive")]}),
                    Some("c"),
                )],
            ),
        ]
    }

    #[test]
    fn a_finding_on_a_github_commit_links_to_the_cited_line_and_its_neighbors() {
        let g = graph(&["code"], true);
        let ran = go_on(&g, on_github, permalink_plan(), &[]);
        let sha = "a".repeat(40);
        let report = ran.read("report.md");
        assert!(report.contains(&format!("### [`src/a.rs:42`](https://github.com/example/demo/blob/{sha}/src/a.rs#L41-L43) — logic (code)")), "{report}");
        assert!(
            report.contains(&format!(
                "[`b.rs:1`](https://github.com/example/demo/blob/{sha}/b.rs#L1-L2)"
            )),
            "a first line has no line above it"
        );
        assert!(report.contains(&format!("[`docs/a b(1)#2?.md`](https://github.com/example/demo/blob/{sha}/docs/a%20b%281%29%232%3F.md)")), "no anchor without a line, and the path is escaped");
        assert!(report.contains(&format!("- `code:3` [`c.rs:7`](https://github.com/example/demo/blob/{sha}/c.rs#L6-L8): because false-positive")), "{report}");
        let rows = ran.rows("findings.jsonl");
        assert_eq!(
            rows[0]["permalink"],
            format!("https://github.com/example/demo/blob/{sha}/src/a.rs#L41-L43")
        );
    }

    #[test]
    fn without_a_github_origin_or_with_uncommitted_changes_headings_are_plain() {
        let g = graph(&["code"], true);
        for edit in [
            |_: &mut Target| {},
            |t: &mut Target| {
                t.owner_repo = Some("example/demo".into());
                t.uncommitted = true;
            },
        ] {
            let ran = go_on(&g, edit, permalink_plan(), &[]);
            let report = ran.read("report.md");
            assert!(
                report.contains("### `src/a.rs:42` — logic (code)") && !report.contains("https://"),
                "{report}"
            );
            assert!(ran
                .rows("findings.jsonl")
                .iter()
                .all(|r| r.get("permalink").is_none()));
        }
    }

    #[test]
    fn a_finding_renders_its_invariant_before_the_recommendation_and_its_verification_last() {
        let g = graph(&["code"], false);
        let report = go(
            &g,
            vec![(
                "code",
                vec![ok(found(vec![finding("a.rs", 1, "P2", "logic")]), None)],
            )],
            &[],
        )
        .read("report.md");
        let at = |needle: &str| {
            report
                .find(needle)
                .unwrap_or_else(|| panic!("no {needle} in:\n{report}"))
        };
        assert!(at("**Why it matters:**") < at("**Invariant:** it holds"));
        assert!(at("**Invariant:**") < at("**Recommendation:**"));
        assert!(at("**Recommendation:**") < at("**Verify:** by inspection"));

        let mut old = finding("a.rs", 1, "P2", "logic");
        old.as_object_mut().unwrap().remove("invariant");
        old.as_object_mut().unwrap().remove("verify");
        let report =
            go(&g, vec![("code", vec![ok(found(vec![old]), None)])], &[]).read("report.md");
        assert!(
            !report.contains("**Invariant:**") && !report.contains("**Verify:**"),
            "{report}"
        );
    }

    #[test]
    fn a_report_of_only_p3_findings_says_there_are_no_blocking_issues() {
        let g = graph(&["code"], false);
        let only_nits = go(
            &g,
            vec![(
                "code",
                vec![ok(found(vec![finding("a.rs", 1, "P3", "names")]), None)],
            )],
            &[],
        );
        let report = only_nits.read("report.md");
        assert!(report.contains("1 findings: 0 P0, 0 P1, 0 P2, 1 P3. 0 already decided, 0 dropped by the auditor.\n\nNo blocking issues.\n\n"), "{report}");

        let mixed = go(
            &g,
            vec![(
                "code",
                vec![ok(
                    found(vec![
                        finding("a.rs", 1, "P3", "names"),
                        finding("a.rs", 2, "P2", "logic"),
                    ]),
                    None,
                )],
            )],
            &[],
        );
        assert!(!mixed.read("report.md").contains("No blocking issues."));
        let none = go(&g, vec![("code", clean())], &[]);
        assert!(
            !none.read("report.md").contains("No blocking issues."),
            "nothing found is not the same claim"
        );
    }

    fn dedupe_graph() -> Graph {
        graph_of(
            "",
            &[
                ("generator", "code", ""),
                ("generator", "legibility", ""),
                (
                    "critic",
                    "audit",
                    "needs = [\"code\", \"legibility\"]\nper_need = true",
                ),
                ("merger", "dedupe", ""),
            ],
        )
    }

    fn groups(groups: Value) -> Outcome {
        ok(json!({ "groups": groups }), Some("m"))
    }

    /// code:0 and legibility:1 sit on one file from two lenses, with code:1 on another.
    fn duplicate_plan(merger: Vec<Outcome>) -> Vec<(&'static str, Vec<Outcome>)> {
        vec![
            (
                "code",
                vec![ok(
                    found(vec![
                        finding("a.rs", 10, "P2", "logic"),
                        finding("z.rs", 1, "P1", "gone"),
                    ]),
                    Some("g1"),
                )],
            ),
            (
                "legibility",
                vec![ok(
                    found(vec![
                        finding("b.rs", 1, "P3", "names"),
                        finding("a.rs", 12, "P3", "naming"),
                    ]),
                    Some("g2"),
                )],
            ),
            (
                "audit-code",
                vec![ok(
                    json!({"verdicts": [verdict("code:0", "kept"), verdict("code:1", "false-positive")]}),
                    Some("c1"),
                )],
            ),
            (
                "audit-legibility",
                vec![ok(
                    json!({"verdicts": [verdict("legibility:0", "kept"), verdict("legibility:1", "kept")]}),
                    Some("c2"),
                )],
            ),
            ("dedupe", merger),
        ]
    }

    #[test]
    fn a_merger_folds_a_lenss_duplicate_into_the_finding_it_repeats() {
        let ran = go(
            &dedupe_graph(),
            duplicate_plan(vec![groups(
                json!([{"indices": ["code:0", "legibility:1"], "reason": "one unclear name"}]),
            )]),
            &[],
        );
        assert!(ran.summary.complete);

        let prompt = &ran.stub.calls_to("dedupe")[0].prompt;
        assert!(
            prompt.contains("\"index\":\"code:0\"")
                && prompt.contains("\"index\":\"legibility:1\"")
                && prompt.contains("\"index\":\"legibility:0\"")
        );
        assert!(
            !prompt.contains("code:1"),
            "a false positive is not offered for merging"
        );
        assert!(
            prompt.contains("\"recommendation\":\"fix it\"") && !prompt.contains("why_it_matters")
        );
        assert!(
            prompt.contains("\"invariant\":\"it holds\"")
                && prompt.contains("\"verify\":\"by inspection\"")
        );

        let report = ran.read("report.md");
        assert!(
            report.contains("2 findings: 0 P0, 0 P1, 1 P2, 1 P3."),
            "the merged finding is not counted:\n{report}"
        );
        assert!(report.contains(", 1 merged as duplicates."), "{report}");
        assert!(report.contains("**Verify:** by inspection\n\n_Also raised by legibility (`legibility:1`): one unclear name_\n"), "{report}");
        assert!(!report.contains("a.rs:12"), "the duplicate is not rendered");
        assert_eq!(
            report.matches("description of").count(),
            2,
            "descriptions are not concatenated"
        );

        let rows = ran.rows("findings.jsonl");
        let by = |i: &str| rows.iter().find(|r| r["index"] == i).unwrap();
        assert_eq!(
            (
                by("legibility:1")["disposition"].as_str(),
                by("legibility:1")["duplicate_of"].as_str()
            ),
            (Some("duplicate"), Some("code:0"))
        );
        assert_eq!(by("code:0")["lenses"], json!(["code", "legibility"]));
        assert!(by("code:0").get("duplicate_of").is_none());
        assert_eq!(
            rows.len(),
            3,
            "nothing is lost from findings.jsonl; only the false positive is dropped"
        );
        assert_eq!(
            ran.rows("merges.jsonl"),
            [json!({"indices": ["code:0", "legibility:1"], "reason": "one unclear name"})]
        );
        assert!(ran.out.join("nodes/dedupe/prompt.md").is_file());
    }

    #[test]
    fn the_most_severe_member_of_a_group_is_the_primary_and_ties_go_to_graph_order() {
        let plan = |a: &str, b: &str| {
            vec![
                (
                    "code",
                    vec![ok(
                        found(vec![
                            finding("a.rs", 10, "P2", "logic"),
                            finding("a.rs", 20, a, "more"),
                        ]),
                        None,
                    )],
                ),
                (
                    "legibility",
                    vec![ok(found(vec![finding("a.rs", 11, b, "names")]), None)],
                ),
                (
                    "audit-code",
                    vec![ok(
                        json!({"verdicts": [verdict("code:0", "kept"), verdict("code:1", "kept")]}),
                        None,
                    )],
                ),
                (
                    "audit-legibility",
                    vec![ok(
                        json!({"verdicts": [verdict("legibility:0", "kept")]}),
                        None,
                    )],
                ),
                (
                    "dedupe",
                    vec![groups(
                        json!([{"indices": ["legibility:0", "code:1", "code:0"], "reason": "same"}]),
                    )],
                ),
            ]
        };
        let primary_of = |ran: &Ran| -> Vec<String> {
            ran.rows("findings.jsonl")
                .iter()
                .filter(|r| r["disposition"] != "duplicate")
                .map(|r| r["index"].as_str().unwrap().to_string())
                .collect()
        };
        let upgraded = go(&dedupe_graph(), plan("P3", "P1"), &[]);
        assert_eq!(
            primary_of(&upgraded),
            ["legibility:0"],
            "severity beats order"
        );
        let tied = go(&dedupe_graph(), plan("P2", "P2"), &[]);
        assert_eq!(
            primary_of(&tied),
            ["code:0"],
            "the order the findings were given in beats the order the group names them"
        );
        let dupes: Vec<Value> = tied
            .rows("findings.jsonl")
            .into_iter()
            .filter(|r| r["disposition"] == "duplicate")
            .collect();
        assert_eq!(
            dupes
                .iter()
                .map(|r| r["index"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["code:1", "legibility:0"]
        );
        assert!(tied
            .read("report.md")
            .contains("_Also raised by code (`code:1`), legibility (`legibility:0`): same_"));
    }

    #[test]
    fn a_merger_has_nothing_to_look_at_unless_two_lenses_share_a_file() {
        let alone = go(
            &dedupe_graph(),
            vec![
                (
                    "code",
                    vec![ok(
                        found(vec![
                            finding("a.rs", 1, "P2", "logic"),
                            finding("a.rs", 2, "P2", "errors"),
                        ]),
                        None,
                    )],
                ),
                (
                    "legibility",
                    vec![ok(found(vec![finding("b.rs", 1, "P3", "names")]), None)],
                ),
                (
                    "audit-code",
                    vec![ok(
                        json!({"verdicts": [verdict("code:0", "kept"), verdict("code:1", "kept")]}),
                        None,
                    )],
                ),
                (
                    "audit-legibility",
                    vec![ok(
                        json!({"verdicts": [verdict("legibility:0", "kept")]}),
                        None,
                    )],
                ),
            ],
            &[],
        );
        let n = alone.node("dedupe");
        assert_eq!(
            (n.status, n.reason.as_deref()),
            (
                "skipped",
                Some("no findings from different lenses share a file")
            )
        );
        assert!(alone.stub.calls_to("dedupe").is_empty() && alone.summary.complete);
        assert_eq!(alone.read("merges.jsonl"), "");

        // The shared file's other finding was a false positive, so only one lens reports there.
        let judged = go(
            &dedupe_graph(),
            vec![
                (
                    "code",
                    vec![ok(found(vec![finding("a.rs", 1, "P2", "logic")]), None)],
                ),
                (
                    "legibility",
                    vec![ok(found(vec![finding("a.rs", 2, "P3", "names")]), None)],
                ),
                (
                    "audit-code",
                    vec![ok(json!({"verdicts": [verdict("code:0", "kept")]}), None)],
                ),
                (
                    "audit-legibility",
                    vec![ok(
                        json!({"verdicts": [verdict("legibility:0", "pre-existing")]}),
                        None,
                    )],
                ),
            ],
            &[],
        );
        assert_eq!(judged.node("dedupe").status, "skipped");
    }

    #[test]
    fn a_merger_that_names_an_unknown_index_or_a_repeat_is_retried_and_may_group_nothing() {
        let bad = [
            (
                json!([{"indices": ["code:0", "nope:3"], "reason": "r"}]),
                "unknown index nope:3",
            ),
            (
                json!([{"indices": ["code:0", "legibility:1"], "reason": "r"}, {"indices": ["legibility:1", "legibility:0"], "reason": "r"}]),
                "legibility:1 appears in more than one group",
            ),
            (
                json!([{"indices": ["code:0"], "reason": "r"}]),
                "fewer than two indices",
            ),
        ];
        for (rejected, why) in bad {
            let ran = go(
                &dedupe_graph(),
                duplicate_plan(vec![groups(rejected), groups(json!([]))]),
                &[],
            );
            let calls = ran.stub.calls_to("dedupe");
            assert_eq!(calls.len(), 2, "{why}");
            assert!(
                calls[1].prompt.contains(why) && calls[1].resume.as_deref() == Some("m"),
                "{why}: {}",
                calls[1].prompt
            );
            assert_eq!(
                (ran.node("dedupe").status, ran.node("dedupe").attempts),
                ("ok", 2)
            );
            assert!(
                !ran.read("report.md").contains("merged as duplicates"),
                "an empty groups array merges nothing"
            );
            assert_eq!(ran.rows("findings.jsonl").len(), 3);
        }
    }

    #[test]
    fn a_failed_merger_leaves_the_findings_unmerged_and_the_report_incomplete() {
        let ran = go(
            &dedupe_graph(),
            duplicate_plan(vec![Outcome::failed("could not start claude")]),
            &[],
        );
        assert_eq!(ran.node("dedupe").status, "failed");
        assert!(!ran.summary.complete);
        let report = ran.read("report.md");
        assert!(
            report.contains("**Incomplete:** `dedupe` failed")
                && report.contains("3 findings:")
                && !report.contains("Also raised by"),
            "{report}"
        );
        assert!(ran
            .rows("findings.jsonl")
            .iter()
            .all(|r| r["disposition"] != "duplicate"));
    }

    #[test]
    fn a_merger_can_be_skipped_by_request() {
        let ran = go(&dedupe_graph(), duplicate_plan(vec![]), &["dedupe"]);
        assert_eq!(
            ran.node("dedupe").reason.as_deref(),
            Some("skipped by request")
        );
        assert!(ran.stub.calls_to("dedupe").is_empty() && ran.summary.complete);
    }

    #[test]
    fn a_merger_starts_only_after_every_critic_instance_and_sorts_last_in_the_manifest() {
        let plan = duplicate_plan(vec![groups(json!([]))]);
        let (ran, log) = drive(&dedupe_graph(), plan, &["audit-code"], &[], 4, |d| {
            d.wait_for("end:audit-legibility");
            d.wait_for("start:audit-code");
            assert!(!d.has("start:dedupe"), "a critic instance is still running");
            d.release("audit-code");
        });
        assert!(position(&log, "end:audit-code") < position(&log, "start:dedupe"));
        assert!(position(&log, "end:legibility") < position(&log, "start:dedupe"));
        let m = manifest(&ran);
        let named: Vec<(&str, &str)> = m["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| (n["name"].as_str().unwrap(), n["kind"].as_str().unwrap()))
            .collect();
        assert_eq!(
            named,
            [
                ("code", "generator"),
                ("legibility", "generator"),
                ("audit-code", "critic"),
                ("audit-legibility", "critic"),
                ("dedupe", "merger")
            ]
        );
    }

    #[test]
    fn a_merger_sees_the_severity_a_critic_gave() {
        let ran = go(
            &dedupe_graph(),
            vec![
                (
                    "code",
                    vec![ok(found(vec![finding("a.rs", 10, "P1", "logic")]), None)],
                ),
                (
                    "legibility",
                    vec![ok(found(vec![finding("a.rs", 12, "P3", "names")]), None)],
                ),
                (
                    "audit-code",
                    vec![ok(
                        json!({"verdicts": [{"index": "code:0", "verdict": "downgraded", "from": "P1", "to": "P2", "reason": "narrow"}]}),
                        None,
                    )],
                ),
                (
                    "audit-legibility",
                    vec![ok(
                        json!({"verdicts": [verdict("legibility:0", "kept")]}),
                        None,
                    )],
                ),
                ("dedupe", vec![groups(json!([]))]),
            ],
            &[],
        );
        assert!(ran.stub.calls_to("dedupe")[0]
            .prompt
            .contains("\"severity\":\"P2\""));
    }

    /// Findings for a hypothetical web UI change, with one hand-written group.
    fn panel_review() -> Vec<(&'static str, Vec<Outcome>)> {
        const TS: &str = "src/panel/panel.ts";
        const HTML: &str = "src/panel/panel.html";
        const SPEC: &str = "src/panel/panel.spec.ts";
        let row = |file: &str,
                   line: u64,
                   sev: &str,
                   conf: &str,
                   cat: &str,
                   description: &str,
                   rec: &str| {
            json!({"file": file, "line": line, "severity": sev, "confidence": conf, "category": cat,
                   "description": description, "why_it_matters": "it matters", "invariant": "it holds", "recommendation": rec, "verify": "by inspection"})
        };
        let code = vec![
            row(TS, 1783, "P2", "high", "correctness", "`bannerHidden()` returns true whenever the merged `sso` entry is disabled, whatever the reason.", "Only show the banner for the not-a-member reason."),
            row(TS, 1787, "P3", "medium", "correctness", "The predicate reads the merged `sso` entry from `this.methods`.", "Evaluate the raw entry instead."),
            row(SPEC, 252, "P3", "high", "tests", "Both new tests check component fields and call the predicate directly; neither checks what renders.", "Assert on the DOM, e.g. `findByText(/Signed in as @alice/)`."),
        ];
        let legibility = vec![
            row(HTML, 145, "P2", "high", "misleading", "The banner shows whenever `bannerHidden()` is true, and that check only asks whether the entry is disabled.", "Make the predicate match the banner."),
            row(SPEC, 252, "P2", "high", "tests-as-docs", "The test is named \"names the signed-in account\" but never checks the banner.", "Assert on the rendered banner."),
            row(HTML, 150, "P3", "high", "naming-or-docs", "The banner message contains a literal ASCII `--`.", "Split it into two sentences."),
            row(TS, 1779, "P3", "medium", "naming-or-docs", "The doc comment partly describes what the template does with the result.", "Limit the doc to the return value."),
            row("src/panel/index.ts", 3, "P3", "low", "naming-or-docs", "A barrel export was added.", "Drop it."),
        ];
        vec![
            ("code", vec![ok(found(code), Some("g1"))]),
            ("legibility", vec![ok(found(legibility), Some("g2"))]),
            (
                "audit-code",
                vec![ok(
                    json!({"verdicts": [verdict("code:0", "kept"), verdict("code:1", "kept"), verdict("code:2", "kept")]}),
                    Some("c1"),
                )],
            ),
            (
                "audit-legibility",
                vec![ok(
                    json!({"verdicts": [
                        verdict("legibility:0", "kept"),
                        {"index": "legibility:1", "verdict": "downgraded", "from": "P2", "to": "P3", "reason": "a test that does not check what it names is a test-strength issue"},
                        verdict("legibility:2", "kept"),
                        verdict("legibility:3", "kept"),
                        verdict("legibility:4", "false-positive"),
                    ]}),
                    Some("c2"),
                )],
            ),
            (
                "dedupe",
                vec![groups(
                    json!([{"indices": ["code:2", "legibility:1"], "reason": "both say the test asserts on component fields and not on the rendered banner"}]),
                )],
            ),
        ]
    }

    #[test]
    fn a_report_links_and_merges_together() {
        let g = dedupe_graph();
        let ran = go_on(
            &g,
            |t| {
                t.owner_repo = Some("example/app".into());
                t.head_sha = "0123456789abcdef0123456789abcdef01234567".into();
            },
            panel_review(),
            &[],
        );
        let report = ran.read("report.md");
        let head = report.split("## Nodes").next().unwrap();
        assert!(head.starts_with("# Review of 01234567 against bbbbbbbb\n\n6 findings: 0 P0, 0 P1, 2 P2, 4 P3. 0 already decided, 1 dropped by the auditor, 1 merged as duplicates.\n\n## P2"), "{head}");
        assert!(!head.contains("No blocking issues."));
        let p3 = head.split("## P3").nth(1).unwrap();
        let at: Vec<usize> = [
            "panel.ts:1787",
            "panel.spec.ts:252",
            "panel.html:150",
            "panel.ts:1779",
        ]
        .iter()
        .map(|l| p3.find(l).unwrap())
        .collect();
        assert!(
            at.windows(2).all(|w| w[0] < w[1]),
            "every P3 is shown, in graph order"
        );
        assert!(head.contains("_Also raised by legibility (`legibility:1`): both say"));
        assert!(head.contains("- `legibility:4` [`src/panel/index.ts:3`](https://github.com/example/app/blob/0123456789abcdef0123456789abcdef01234567/src/panel/index.ts#L2-L4)"));
        let rows = ran.rows("findings.jsonl");
        assert_eq!(rows.len(), 7, "every finding but the false positive");
        assert_eq!(
            rows.iter()
                .filter(|r| r["disposition"] == "duplicate")
                .count(),
            1
        );
    }

    #[test]
    fn a_path_under_the_workspace_is_made_repo_relative() {
        let g = graph(&["code"], false);
        let ws = scratch();
        let absolute = format!("{}/src/a.rs", ws.display());
        let stub = Stub::with(vec![(
            "code",
            vec![ok(
                found(vec![
                    finding(&absolute, 1, "P2", "logic"),
                    finding("./b.rs", 2, "P2", "logic"),
                ]),
                None,
            )],
        )]);
        let opts = Options {
            out: ws.join("bundle"),
            run_id: "r".into(),
            skip: vec![],
            concurrency: 1,
            reuse: None,
            no_prewarm: false,
            no_gh: None,
        };
        run(
            &g,
            &target(&ws),
            &opts,
            Providers {
                claude: &stub,
                command: &stub,
            },
            &|_| {},
        )
        .unwrap();
        let rows: Vec<Value> = std::fs::read_to_string(opts.out.join("findings-code.jsonl"))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(
            (rows[0]["file"].as_str(), rows[1]["file"].as_str()),
            (Some("src/a.rs"), Some("b.rs"))
        );
    }

    #[test]
    fn a_node_runs_when_its_trigger_matches_a_changed_path() {
        let g = graph_of(
            DDL,
            &[
                ("generator", "code", ""),
                ("generator", "compat", "trigger = [\"ddl\"]"),
            ],
        );
        let plan = || vec![("code", clean()), ("compat", clean())];

        let fired = go_on(
            &g,
            |t| t.changed_files = vec!["db/migrations/001.sql".into()],
            plan(),
            &[],
        );
        assert_eq!(fired.stub.calls_to("compat").len(), 1);
        assert_eq!(
            manifest(&fired)["triggers_fired"],
            json!({"ddl": "touches db/migrations/001.sql"})
        );

        let quiet = go(&g, plan(), &[]);
        assert!(quiet.stub.calls_to("compat").is_empty());
        let n = quiet.node("compat");
        assert_eq!(
            (n.status, n.reason.as_deref()),
            ("skipped", Some("no trigger fired (ddl)"))
        );
        assert_eq!(
            quiet.stub.calls_to("code").len(),
            1,
            "a node with no trigger always runs"
        );
        assert_eq!(manifest(&quiet)["triggers_fired"], json!({}));
        assert!(quiet.summary.complete);
    }

    #[test]
    fn a_trigger_reads_added_and_removed_lines_and_not_file_headers() {
        let g = graph_of(
            "wire = { diff_matches = \"x\\\\.proto|rpc \" }",
            &[("generator", "compat", "trigger = [\"wire\"]")],
        );
        let with_diff = |diff: &str| {
            let diff = diff.to_string();
            go_on(&g, move |t| t.diff = diff, vec![("compat", clean())], &[])
        };
        let header_only =
            "diff --git a/x.proto b/x.proto\n--- a/x.proto\n+++ b/x.proto\n@@ -1 +1 @@\n-a\n+b\n";
        assert_eq!(with_diff(header_only).node("compat").status, "skipped");

        let removed = "diff --git a/s.go b/s.go\n--- a/s.go\n+++ b/s.go\n@@ -1,2 +1 @@\n-rpc Old(Req)\n keep\n";
        let ran = with_diff(removed);
        assert_eq!(ran.node("compat").status, "ok");
        assert_eq!(
            manifest(&ran)["triggers_fired"],
            json!({"wire": "diff line -rpc Old(Req)"})
        );
    }

    #[test]
    fn any_one_of_a_nodes_triggers_is_enough() {
        let g = graph_of(
            "a = { touches = [\"*.a\"] }\nb = { touches = [\"*.b\"] }",
            &[("generator", "lens", "trigger = [\"a\", \"b\"]")],
        );
        let ran = go_on(
            &g,
            |t| t.changed_files = vec!["x.b".into()],
            vec![("lens", clean())],
            &[],
        );
        assert_eq!(ran.node("lens").status, "ok");
        let none = go(&g, vec![], &[]);
        assert_eq!(
            none.node("lens").reason.as_deref(),
            Some("no trigger fired (a, b)")
        );
    }

    #[test]
    fn a_critic_audits_what_is_left_when_triggers_skip_its_generators() {
        let g = graph_of(
            DDL,
            &[
                ("generator", "code", ""),
                ("generator", "compat", "trigger = [\"ddl\"]"),
                ("critic", "audit", "needs = [\"code\", \"compat\"]"),
            ],
        );
        let ran = go(
            &g,
            vec![
                (
                    "code",
                    vec![ok(
                        found(vec![finding("a.rs", 1, "P1", "logic")]),
                        Some("g1"),
                    )],
                ),
                (
                    "audit",
                    vec![ok(
                        json!({"verdicts": [verdict("code:0", "kept")]}),
                        Some("c1"),
                    )],
                ),
            ],
            &[],
        );
        assert_eq!(ran.node("compat").status, "skipped");
        assert_eq!(ran.node("audit").status, "ok");
        assert!(!ran.stub.calls_to("audit")[0].prompt.contains("compat:"));
        assert!(ran.summary.complete);
        assert_eq!(manifest(&ran)["lenses"], json!(["code"]));
    }

    const MECH: (&str, &str, &str) = ("mechanical", "lint", "emits = [\"exports-changed\"]");

    fn mechanical_says(diagnostics: Value, triggers: &[&str]) -> Outcome {
        ok(
            json!({"diagnostics": diagnostics, "triggers": triggers}),
            None,
        )
    }

    #[test]
    fn a_trigger_a_mechanical_node_emits_enables_a_later_node() {
        let g = graph_of(
            "",
            &[
                MECH,
                ("generator", "compat", "trigger = [\"exports-changed\"]"),
            ],
        );
        let ran = go(
            &g,
            vec![
                (
                    "lint",
                    vec![mechanical_says(json!([]), &["exports-changed"])],
                ),
                ("compat", clean()),
            ],
            &[],
        );
        assert_eq!(ran.node("compat").status, "ok");
        assert_eq!(
            manifest(&ran)["triggers_fired"],
            json!({"exports-changed": "mechanical node lint"})
        );

        let quiet = go(
            &g,
            vec![("lint", vec![mechanical_says(json!([]), &[])])],
            &[],
        );
        assert_eq!(
            quiet.node("compat").reason.as_deref(),
            Some("no trigger fired (exports-changed)")
        );
        assert!(quiet.summary.complete);
    }

    #[test]
    fn emitting_a_trigger_the_node_did_not_declare_fails_it() {
        let g = graph_of(DDL, &[MECH, ("generator", "compat", "trigger = [\"ddl\"]")]);
        let ran = go(
            &g,
            vec![("lint", vec![mechanical_says(json!([]), &["ddl"])])],
            &[],
        );
        let n = ran.node("lint");
        assert_eq!(n.status, "failed");
        assert!(n
            .reason
            .as_deref()
            .unwrap()
            .contains("`ddl`, which the node does not declare in `emits`"));
        assert_eq!(ran.stub.calls_to("lint").len(), 1, "no retry");
        assert!(!ran.summary.complete);
    }

    #[test]
    fn a_mechanical_failure_skips_everything_after_it_and_still_writes_the_bundle() {
        let g = graph_of(
            DDL,
            &[
                MECH,
                ("generator", "code", ""),
                ("generator", "compat", "trigger = [\"ddl\"]"),
                ("critic", "audit", "needs = [\"code\"]"),
            ],
        );
        for failure in [
            Outcome::failed("lint exited Some(3)"),
            ok(json!({"diagnostics": "not a list"}), None),
        ] {
            let ran = go(&g, vec![("lint", vec![failure])], &[]);
            assert_eq!(ran.node("lint").status, "failed");
            assert_eq!(
                ran.stub.calls_to("lint").len(),
                1,
                "a mechanical node is not retried"
            );
            for name in ["code", "compat", "audit"] {
                let n = ran.node(name);
                assert_eq!(
                    (n.status, n.reason.as_deref()),
                    ("skipped", Some("mechanical node lint failed")),
                    "{name}"
                );
                assert!(ran.stub.calls_to(name).is_empty());
            }
            assert!(!ran.summary.complete);
            assert!(ran.summary.report.contains("**Incomplete:** `lint` failed"));
            for file in [
                "manifest.json",
                "report.md",
                "diagnostics.jsonl",
                "findings.jsonl",
                "verdicts.jsonl",
                "diff.patch",
            ] {
                assert!(ran.out.join(file).is_file(), "bundle lacks {file}");
            }
        }
    }

    #[test]
    fn a_later_mechanical_node_is_skipped_when_an_earlier_one_fails() {
        let g = graph_of(
            "",
            &[
                ("mechanical", "first", ""),
                ("mechanical", "second", "needs = [\"first\"]"),
            ],
        );
        let ran = go(&g, vec![("first", vec![Outcome::failed("boom")])], &[]);
        assert_eq!(
            ran.node("second").reason.as_deref(),
            Some("mechanical node first failed")
        );
    }

    #[test]
    fn diagnostics_reach_the_bundle_the_report_and_every_generator_prompt() {
        let g = graph_of(
            "",
            &[
                ("mechanical", "lint", ""),
                ("generator", "code", ""),
                ("generator", "legibility", ""),
            ],
        );
        let diagnostics = json!([
            {"tool": "golangci-lint", "file": "a/b.go", "line": 12, "message": "unused\nvariable"},
            {"tool": "gofmt", "file": null, "line": null, "message": "needs formatting"},
        ]);
        let ran = go(
            &g,
            vec![
                ("lint", vec![mechanical_says(diagnostics, &[])]),
                ("code", clean()),
                ("legibility", clean()),
            ],
            &[],
        );
        let rows = ran.rows("diagnostics.jsonl");
        assert_eq!(rows.len(), 2);
        assert_eq!(
            (
                rows[0]["node"].as_str(),
                rows[0]["tool"].as_str(),
                rows[0]["line"].as_u64()
            ),
            (Some("lint"), Some("golangci-lint"), Some(12))
        );
        assert_eq!(manifest(&ran)["diagnostics"], 2);
        assert_eq!(ran.node("lint").diagnostics, Some(2));

        let report = ran.read("report.md");
        let (before, mechanical) = report.split_once("## Mechanical").unwrap();
        assert!(
            mechanical.contains("- golangci-lint a/b.go:12: unused variable")
                && mechanical.contains("- gofmt: needs formatting")
        );
        assert!(
            before.find("## Nodes").is_none() && mechanical.contains("## Nodes"),
            "listed before the node table"
        );

        for lens in ["code", "legibility"] {
            let prompt = &ran.stub.calls_to(lens)[0].prompt;
            assert!(
                prompt.contains("do not raise them again as findings"),
                "{lens}"
            );
            assert!(
                prompt.contains("golangci-lint a/b.go:12: unused variable"),
                "{lens}"
            );
        }
    }

    #[test]
    fn context_reaches_every_generator_before_the_diff_and_apart_from_diagnostics() {
        let g = graph_of(
            "",
            &[
                ("mechanical", "pack", ""),
                ("generator", "code", ""),
                ("generator", "legibility", ""),
            ],
        );
        let ran = go(
            &g,
            vec![
                (
                    "pack",
                    vec![ok(
                        json!({
                            "diagnostics": [{"tool": "gofmt", "file": null, "line": null, "message": "needs formatting"}],
                            "context": ["build/header.mk defines\nthe flags", {"tool": "callers", "message": "lib/x.go calls it"}],
                        }),
                        None,
                    )],
                ),
                ("code", clean()),
                ("legibility", clean()),
            ],
            &[],
        );
        let out = std::fs::read_to_string(ran.out.join("nodes/pack/output.json")).unwrap();
        let persisted: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            persisted["context"],
            json!([{"tool": "pack", "message": "build/header.mk defines\nthe flags"}, {"tool": "callers", "message": "lib/x.go calls it"}])
        );
        assert_eq!(manifest(&ran)["diagnostics"], 1);

        const HEADING: &str = "Context from mechanical tools — files outside the diff that bear on the change. Open them before judging; they are pointers, not findings:";
        for lens in ["code", "legibility"] {
            let prompt = &ran.stub.calls_to(lens)[0].prompt;
            let at = prompt
                .find(HEADING)
                .unwrap_or_else(|| panic!("{lens} got no context section"));
            assert!(
                prompt.contains(
                    "  pack: build/header.mk defines the flags\n  callers: lib/x.go calls it\n"
                ),
                "{lens}"
            );
            assert!(
                at < prompt
                    .find("The diff")
                    .or(prompt.find("diff is"))
                    .unwrap_or(usize::MAX),
                "{lens}: context precedes the diff"
            );
            let (before, after) = prompt
                .split_once("mechanical diagnostics, already reported")
                .unwrap();
            assert!(
                before.contains(HEADING) && !after.contains("build/header.mk"),
                "{lens}: context is not in the already-reported list"
            );
        }
    }

    #[test]
    fn with_no_context_there_is_no_context_section() {
        let g = graph_of("", &[("mechanical", "lint", ""), ("generator", "code", "")]);
        let ran = go(
            &g,
            vec![
                ("lint", vec![mechanical_says(json!([]), &[])]),
                ("code", clean()),
            ],
            &[],
        );
        assert!(!ran.stub.calls_to("code")[0]
            .prompt
            .contains("Context from mechanical tools"));
    }

    #[test]
    fn with_no_diagnostics_there_is_no_section_and_no_prompt_block() {
        let g = graph_of("", &[("mechanical", "lint", ""), ("generator", "code", "")]);
        let ran = go(
            &g,
            vec![
                ("lint", vec![mechanical_says(json!([]), &[])]),
                ("code", clean()),
            ],
            &[],
        );
        assert!(!ran.read("report.md").contains("## Mechanical"));
        assert!(!ran.stub.calls_to("code")[0]
            .prompt
            .contains("mechanical diagnostics"));
        assert_eq!(ran.read("diagnostics.jsonl"), "");
    }

    #[test]
    fn mechanical_nodes_run_first_get_the_contract_and_everyone_else_waits() {
        // Listed last, and not saying it needs anything.
        let g = graph_of(
            "",
            &[
                ("generator", "code", ""),
                ("critic", "audit", "needs = [\"code\"]"),
                ("mechanical", "lint", ""),
            ],
        );
        let ran = go(
            &g,
            vec![
                ("lint", vec![mechanical_says(json!([]), &[])]),
                (
                    "code",
                    vec![ok(found(vec![finding("a.rs", 1, "P2", "logic")]), None)],
                ),
                (
                    "audit",
                    vec![ok(json!({"verdicts": [verdict("code:0", "kept")]}), None)],
                ),
            ],
            &[],
        );
        let order: Vec<String> = ran
            .stub
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|c| c.node.clone())
            .collect();
        assert_eq!(order, ["lint", "code", "audit"]);

        let contract: Value = serde_json::from_str(&ran.stub.calls_to("lint")[0].prompt).unwrap();
        assert_eq!(contract["node"], "lint");
        assert_eq!(contract["base"], "b".repeat(40));
        assert_eq!(contract["head"], "a".repeat(40));
        assert_eq!(contract["changed_files"], json!(["a.rs"]));
        assert_eq!(
            contract["diff"],
            ran.out.join("diff.patch").to_str().unwrap()
        );
        assert!(contract["workspace"].is_string());
        let out = ran.out.join("nodes").join("lint").join("out");
        assert_eq!(contract["out"], out.to_str().unwrap());
        assert!(out.is_dir());
        assert!(contract.get("pr_number").is_some_and(Value::is_null));
        assert!(contract.get("pr_body").is_some_and(Value::is_null));
        assert!(!ran.out.join("pr-body.md").exists());
        let manifest: Value = serde_json::from_str(&ran.read("manifest.json")).unwrap();
        assert!(manifest.get("pr_number").is_some_and(Value::is_null));
    }

    #[test]
    fn a_mechanical_node_that_runs_again_finds_its_earlier_out_in_the_new_bundle() {
        let g = graph_of("", &[("mechanical", "fetch", "")]);
        let first = go(
            &g,
            vec![("fetch", vec![mechanical_says(json!([]), &[])])],
            &[],
        );
        assert_eq!(
            std::fs::read_to_string(first.out.join("nodes/fetch/out/notes.md")).unwrap(),
            "release notes"
        );

        let second = go_from(
            &g,
            Some(&first),
            |_| {},
            vec![("fetch", vec![mechanical_says(json!([]), &[])])],
            &[],
        );
        assert_ne!(second.out, first.out);
        assert_eq!(second.stub.calls_to("fetch").len(), 1);
        assert_eq!(
            std::fs::read_to_string(second.out.join("nodes/fetch/out/notes.md")).unwrap(),
            "release notes"
        );
    }

    #[test]
    fn a_pull_request_body_is_written_to_the_bundle_and_handed_to_mechanical_nodes() {
        let g = graph_of("", &[("mechanical", "lint", "")]);
        let ran = go_on(
            &g,
            |t| (t.pr_number, t.pr_body) = (Some(7), Some("Fixes the thing.".into())),
            vec![("lint", vec![mechanical_says(json!([]), &[])])],
            &[],
        );
        let contract: Value = serde_json::from_str(&ran.stub.calls_to("lint")[0].prompt).unwrap();
        assert_eq!(contract["pr_number"], 7);
        assert_eq!(contract["pr_body"], "Fixes the thing.");
        assert_eq!(ran.read("pr-body.md"), "Fixes the thing.");
        let manifest: Value = serde_json::from_str(&ran.read("manifest.json")).unwrap();
        assert_eq!(manifest["pr_number"], 7);
    }

    #[test]
    fn a_no_gh_run_says_so_in_the_manifest_tells_reviewers_and_flags_mechanical_nodes() {
        let g = graph_of(
            "",
            &[
                ("mechanical", "lint", ""),
                ("generator", "code", ""),
                ("generator", "legibility", ""),
            ],
        );
        let ws = scratch();
        let stub = Stub::with(vec![
            ("lint", vec![mechanical_says(json!([]), &[])]),
            ("code", clean()),
            ("legibility", clean()),
        ]);
        let opts = Options {
            out: ws.join("bundle"),
            run_id: "r".into(),
            skip: vec![],
            concurrency: 1,
            reuse: None,
            no_prewarm: false,
            no_gh: Some(NoGh::create().unwrap()),
        };
        let summary = run(
            &g,
            &target(&ws),
            &opts,
            Providers {
                claude: &stub,
                command: &stub,
            },
            &|_| {},
        )
        .unwrap();
        let ran = Ran {
            summary,
            out: ws.join("bundle"),
            stub,
        };
        assert_eq!(manifest(&ran)["no_gh"], true);
        assert!(ran
            .node("lint")
            .warnings
            .iter()
            .any(|w| w.contains("--no-gh")));
        assert!(ran.node("code").warnings.is_empty());
        for lens in ["code", "legibility"] {
            let prompt = &ran.stub.calls_to(lens)[0].prompt;
            assert!(prompt.starts_with(NO_GH_NOTICE), "{lens}");
            assert!(ran
                .read(&format!("nodes/{lens}/prompt.md"))
                .starts_with(NO_GH_NOTICE));
        }
        // A script's stdin is its contract, untouched.
        assert!(serde_json::from_str::<Value>(&ran.stub.calls_to("lint")[0].prompt).is_ok());
    }

    #[test]
    fn without_no_gh_the_manifest_says_false_and_prompts_carry_no_notice() {
        let g = graph_of("", &[("generator", "code", "")]);
        let ran = go(&g, vec![("code", clean())], &[]);
        assert_eq!(manifest(&ran)["no_gh"], false);
        assert!(!ran.stub.calls_to("code")[0].prompt.contains(NO_GH_NOTICE));
        assert!(ran.node("code").warnings.is_empty());
    }

    #[test]
    fn a_bundle_is_reused_only_by_a_run_with_the_same_github_access() {
        let g = graph_of("", &[("generator", "code", "")]);
        let ran = go(&g, vec![("code", clean())], &[]);
        let previous = Previous::open(&ran.out, &target(&scratch())).unwrap();
        assert!(previous.gh_access_matches(false).is_ok());
        let why = previous.gh_access_matches(true).unwrap_err();
        assert!(
            why.contains("ran without --no-gh") && why.contains("this one ran with --no-gh"),
            "{why}"
        );
    }

    #[test]
    fn a_diagnostic_path_under_the_workspace_is_made_repo_relative() {
        let g = graph_of("", &[("mechanical", "lint", "")]);
        let ws = scratch();
        let absolute = format!("{}/src/a.go", ws.display());
        let stub = Stub::with(vec![(
            "lint",
            vec![mechanical_says(
                json!([{"tool": "t", "file": absolute, "line": 1, "message": "m"}]),
                &[],
            )],
        )]);
        let opts = Options {
            out: ws.join("bundle"),
            run_id: "r".into(),
            skip: vec![],
            concurrency: 1,
            reuse: None,
            no_prewarm: false,
            no_gh: None,
        };
        run(
            &g,
            &target(&ws),
            &opts,
            Providers {
                claude: &stub,
                command: &stub,
            },
            &|_| {},
        )
        .unwrap();
        let row: Value = serde_json::from_str(
            std::fs::read_to_string(opts.out.join("diagnostics.jsonl"))
                .unwrap()
                .trim(),
        )
        .unwrap();
        assert_eq!(row["file"], "src/a.go");
    }

    /// The test thread's hold on a run in flight. Nodes named in `gated` block
    /// inside the stub until `release`d, and the run's events arrive as
    /// `start:<node>` and `end:<node>`, so a test waits on what the scheduler
    /// did rather than on time.
    struct Driver {
        release: HashMap<String, mpsc::Sender<()>>,
        events: mpsc::Receiver<String>,
        seen: Vec<String>,
        /// The bundle the run is writing.
        out: PathBuf,
    }

    impl Driver {
        fn wait_for(&mut self, event: &str) {
            while !self.seen.iter().any(|e| e == event) {
                let next = self.events.recv_timeout(Duration::from_secs(30));
                self.seen.push(
                    next.unwrap_or_else(|_| panic!("no `{event}` event; saw {:?}", self.seen)),
                );
            }
        }

        fn has(&self, event: &str) -> bool {
            self.seen.iter().any(|e| e == event)
        }

        fn release(&mut self, node: &str) {
            self.release
                .remove(node)
                .unwrap_or_else(|| panic!("{node} is not gated"))
                .send(())
                .unwrap();
        }
    }

    /// Runs `graph` on its own thread while `script` drives it from this one.
    /// Returns the run and every event, in the order the run emitted them.
    fn drive(
        graph: &Graph,
        plan: Vec<(&str, Vec<Outcome>)>,
        gated: &[&str],
        skip: &[&str],
        concurrency: usize,
        script: impl FnOnce(&mut Driver),
    ) -> (Ran, Vec<String>) {
        let ws = scratch();
        let out = ws.join("bundle");
        let stub = Stub::with(plan);
        let mut release = HashMap::new();
        for name in gated {
            let (tx, rx) = mpsc::channel();
            stub.gates.lock().unwrap().insert(name.to_string(), rx);
            release.insert(name.to_string(), tx);
        }
        let (etx, erx) = mpsc::channel::<String>();
        let etx = Mutex::new(etx);
        let events = |ev: Value| {
            let node = ev["node"]
                .as_str()
                .or_else(|| ev["node"]["name"].as_str())
                .unwrap_or_default();
            let kind = if ev["type"] == "node_start" {
                "start"
            } else {
                "end"
            };
            let _ = etx.lock().unwrap().send(format!("{kind}:{node}"));
        };
        let opts = Options {
            out: out.clone(),
            run_id: "test-run".into(),
            skip: skip.iter().map(|s| s.to_string()).collect(),
            concurrency,
            reuse: None,
            no_prewarm: false,
            no_gh: None,
        };
        let t = target(&ws);
        let (summary, log) = std::thread::scope(|scope| {
            let run = scope.spawn(|| {
                run(
                    graph,
                    &t,
                    &opts,
                    Providers {
                        claude: &stub,
                        command: &stub,
                    },
                    &events,
                )
                .unwrap()
            });
            let mut d = Driver {
                release,
                events: erx,
                seen: Vec::new(),
                out: out.clone(),
            };
            script(&mut d);
            // Anything still gated is let go; a sender dropped counts as a release.
            d.release.clear();
            let summary = run.join().unwrap();
            while let Ok(e) = d.events.try_recv() {
                d.seen.push(e);
            }
            (summary, d.seen)
        });
        (Ran { summary, out, stub }, log)
    }

    fn lenses(names: &[&str]) -> Graph {
        let nodes: Vec<(&str, &str, &str)> = names.iter().map(|n| ("generator", *n, "")).collect();
        graph_of("", &nodes)
    }

    fn position(log: &[String], event: &str) -> usize {
        log.iter()
            .position(|e| e == event)
            .unwrap_or_else(|| panic!("no `{event}` in {log:?}"))
    }

    #[test]
    fn a_fifth_first_wave_node_starts_while_a_slow_one_is_running() {
        let g = lenses(&["a", "b", "c", "d", "e"]);
        let plan = ["a", "b", "c", "d", "e"].map(|n| (n, clean())).into();
        let (ran, log) = drive(&g, plan, &["a"], &[], 4, |d| {
            d.wait_for("end:e");
            assert!(!d.has("end:a"), "a is still held");
            d.release("a");
        });
        assert!(position(&log, "end:e") < position(&log, "end:a"));
        assert!(ran.stub.peak.load(Ordering::SeqCst) <= 4);
        assert!(ran.summary.complete);
    }

    #[test]
    fn concurrency_one_never_overlaps_two_nodes() {
        let g = lenses(&["a", "b", "c"]);
        let plan = ["a", "b", "c"].map(|n| (n, clean())).into();
        let (ran, log) = drive(&g, plan, &["a"], &[], 1, |d| {
            d.wait_for("start:a");
            d.release("a");
        });
        assert_eq!(
            log,
            ["start:a", "end:a", "start:b", "end:b", "start:c", "end:c"]
        );
        assert_eq!(ran.stub.peak.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn nodes_left_out_take_no_slot() {
        let g = graph_of(
            DDL,
            &[
                ("generator", "code", ""),
                ("generator", "compat", "trigger = [\"ddl\"]"),
                ("generator", "skipme", ""),
                ("critic", "audit", "needs = [\"compat\", \"skipme\"]"),
            ],
        );
        let (ran, _) = drive(
            &g,
            vec![("code", clean())],
            &["code"],
            &["skipme"],
            1,
            |d| {
                // The one slot is held by code, yet both are settled and so is the critic behind them.
                d.wait_for("end:compat");
                d.wait_for("end:skipme");
                d.wait_for("end:audit");
                d.wait_for("start:code");
                assert!(d.has("start:code") && !d.has("end:code"));
                d.release("code");
            },
        );
        assert_eq!(
            ran.node("compat").reason.as_deref(),
            Some("no trigger fired (ddl)")
        );
        assert_eq!(
            ran.node("skipme").reason.as_deref(),
            Some("skipped by request")
        );
        assert_eq!(
            ran.node("audit").reason.as_deref(),
            Some("no findings to audit")
        );
        assert_eq!(ran.stub.peak.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_mechanical_failure_skips_what_has_not_started_while_a_running_node_finishes() {
        let g = graph_of(
            "",
            &[
                ("mechanical", "a", ""),
                ("mechanical", "b", ""),
                ("mechanical", "c", "needs = [\"b\"]"),
                ("generator", "code", ""),
                ("critic", "audit", "needs = [\"code\"]"),
            ],
        );
        let plan = vec![
            ("a", vec![Outcome::failed("a exited Some(3)")]),
            ("b", vec![mechanical_says(json!([]), &[])]),
        ];
        let (ran, _) = drive(&g, plan, &["b"], &[], 4, |d| {
            for name in ["c", "code", "audit"] {
                d.wait_for(&format!("end:{name}"));
            }
            assert!(!d.has("end:b"), "b is still running");
            d.release("b");
        });
        assert_eq!(ran.node("b").status, "ok");
        assert_eq!(ran.node("a").status, "failed");
        for name in ["c", "code", "audit"] {
            let n = ran.node(name);
            assert_eq!(
                (n.status, n.reason.as_deref()),
                ("skipped", Some("mechanical node a failed")),
                "{name}"
            );
            assert!(ran.stub.calls_to(name).is_empty());
        }
        assert!(!ran.summary.complete);
        assert!(ran.out.join("manifest.json").is_file());
    }

    #[test]
    fn the_manifest_is_in_graph_order_whatever_order_nodes_finished_in() {
        let g = graph_of(
            "",
            &[
                ("generator", "code", ""),
                ("generator", "legibility", ""),
                ("critic", "audit", "needs = [\"code\", \"legibility\"]"),
            ],
        );
        let plan = vec![
            (
                "code",
                vec![ok(found(vec![finding("a.rs", 1, "P1", "logic")]), None)],
            ),
            (
                "legibility",
                vec![ok(found(vec![finding("b.rs", 2, "P3", "names")]), None)],
            ),
            (
                "audit",
                vec![ok(
                    json!({"verdicts": [verdict("code:0", "kept"), verdict("legibility:0", "kept")]}),
                    None,
                )],
            ),
        ];
        let (ran, log) = drive(&g, plan, &["code"], &[], 4, |d| {
            d.wait_for("end:legibility");
            d.release("code");
        });
        assert!(position(&log, "end:legibility") < position(&log, "end:code"));
        let m = manifest(&ran);
        let names: Vec<&str> = m["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["code", "legibility", "audit"]);
        let findings: Vec<String> = ran
            .rows("findings.jsonl")
            .iter()
            .map(|r| r["index"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(findings, ["code:0", "legibility:0"]);
    }

    fn per_need_graph(names: &[&str]) -> Graph {
        let mut nodes: Vec<(&str, &str, &str)> =
            names.iter().map(|n| ("generator", *n, "")).collect();
        let needs = format!(
            "needs = [{}]\nper_need = true",
            names
                .iter()
                .map(|n| format!("\"{n}\""))
                .collect::<Vec<_>>()
                .join(", ")
        );
        nodes.push(("critic", "audit", &needs));
        graph_of("", &nodes)
    }

    #[test]
    fn a_per_need_critic_audits_a_finished_lens_while_a_slow_one_runs() {
        let g = per_need_graph(&["code", "legibility"]);
        let plan = vec![
            (
                "code",
                vec![ok(
                    found(vec![finding("a.rs", 1, "P1", "logic")]),
                    Some("g1"),
                )],
            ),
            (
                "legibility",
                vec![ok(
                    found(vec![finding("b.rs", 2, "P3", "names")]),
                    Some("g2"),
                )],
            ),
            (
                "audit-code",
                vec![ok(
                    json!({"verdicts": [verdict("code:0", "kept")]}),
                    Some("c1"),
                )],
            ),
            (
                "audit-legibility",
                vec![ok(
                    json!({"verdicts": [verdict("legibility:0", "kept")]}),
                    Some("c2"),
                )],
            ),
        ];
        let (ran, log) = drive(&g, plan, &["code", "audit-legibility"], &[], 4, |d| {
            d.wait_for("start:audit-legibility");
            assert!(!d.has("end:code") && !d.has("start:audit-code"));
            d.release("audit-legibility");
            d.wait_for("end:audit-legibility");
            assert!(!d.has("end:code"), "the slow lens is still running");
            d.release("code");
        });
        assert!(position(&log, "end:audit-legibility") < position(&log, "end:code"));
        assert!(position(&log, "end:code") < position(&log, "start:audit-code"));

        let m = manifest(&ran);
        let nodes = m["nodes"].as_array().unwrap();
        let named: Vec<(&str, &str)> = nodes
            .iter()
            .map(|n| (n["name"].as_str().unwrap(), n["kind"].as_str().unwrap()))
            .collect();
        assert_eq!(
            named,
            [
                ("code", "generator"),
                ("legibility", "generator"),
                ("audit-code", "critic"),
                ("audit-legibility", "critic")
            ]
        );
        assert!(nodes.iter().all(|n| n["status"] == "ok") && m["audited"] == true);
        assert!(
            ran.stub.calls_to("audit").is_empty(),
            "the critic itself never runs"
        );
        for (instance, mine, other) in [
            ("audit-code", "code:0", "legibility:"),
            ("audit-legibility", "legibility:0", "code:"),
        ] {
            let prompt = &ran.stub.calls_to(instance)[0].prompt;
            assert!(
                prompt.contains(mine) && !prompt.contains(other),
                "{instance}: {prompt}"
            );
            assert!(ran
                .out
                .join("nodes")
                .join(instance)
                .join("prompt.md")
                .is_file());
        }
        let union: Vec<String> = ran
            .rows("verdicts.jsonl")
            .iter()
            .map(|r| r["index"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(union, ["code:0", "legibility:0"]);
    }

    #[test]
    fn a_per_need_critic_makes_no_instance_for_a_lens_with_nothing_to_audit() {
        let g = graph_of(
            DDL,
            &[
                ("generator", "code", ""),
                ("generator", "legibility", ""),
                ("generator", "compat", "trigger = [\"ddl\"]"),
                ("generator", "releng", ""),
                (
                    "critic",
                    "audit",
                    "needs = [\"code\", \"legibility\", \"compat\", \"releng\"]\nper_need = true",
                ),
            ],
        );
        let plan = vec![
            ("code", clean()),
            (
                "legibility",
                vec![Outcome::failed("could not start claude")],
            ),
            (
                "releng",
                vec![ok(found(vec![finding("a.rs", 1, "P2", "rollout")]), None)],
            ),
            (
                "audit-releng",
                vec![ok(json!({"verdicts": [verdict("releng:0", "kept")]}), None)],
            ),
        ];
        let ran = go(&g, plan, &[]);
        let names: Vec<&str> = ran.summary.nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(
            names,
            ["code", "legibility", "compat", "releng", "audit-releng"]
        );
        assert_eq!(ran.node("legibility").status, "failed");
        assert_eq!(ran.node("compat").status, "skipped");
        assert_eq!(ran.rows("verdicts.jsonl").len(), 1);
        assert!(!ran.summary.complete);
    }

    #[test]
    fn a_per_need_critic_is_one_skipped_record_when_skipped_or_when_nothing_was_found() {
        let g = per_need_graph(&["code", "releng"]);
        let found_one = || {
            vec![
                (
                    "code",
                    vec![ok(found(vec![finding("a.rs", 1, "P1", "logic")]), None)],
                ),
                ("releng", clean()),
            ]
        };

        let by_request = go(&g, found_one(), &["audit"]);
        let nodes: Vec<(&str, &str, Option<&str>)> = by_request
            .summary
            .nodes
            .iter()
            .map(|n| (n.name.as_str(), n.status, n.reason.as_deref()))
            .collect();
        assert_eq!(nodes[2], ("audit", "skipped", Some("skipped by request")));
        assert_eq!(nodes.len(), 3);
        assert!(by_request.summary.report.contains("**Not audited:**"));

        let nothing = go(&g, vec![("code", clean()), ("releng", clean())], &[]);
        assert_eq!(nothing.summary.nodes.len(), 3);
        assert_eq!(
            nothing.node("audit").reason.as_deref(),
            Some("no findings to audit")
        );
        assert!(nothing.summary.complete);
    }

    const ALREADY: &str = "findings already reported by earlier readers; do not raise them again, and do not spend time confirming them:";

    fn funnel(needs: &str) -> Graph {
        graph_of(
            "",
            &[
                ("generator", "first", ""),
                ("generator", "second", ""),
                ("generator", "deep", needs),
                (
                    "critic",
                    "audit",
                    "needs = [\"first\", \"second\", \"deep\"]\nper_need = true",
                ),
            ],
        )
    }

    #[test]
    fn a_generator_starts_after_its_needs_and_is_told_their_findings_are_reported() {
        let g = funnel("needs = [\"first\", \"second\"]");
        let long = "x".repeat(400);
        let mut wordy = finding("b.rs", 7, "P3", "names");
        wordy["description"] = json!(format!("two\nlines {long}"));
        let plan = vec![
            (
                "first",
                vec![ok(found(vec![finding("a.rs", 1, "P1", "logic")]), None)],
            ),
            ("second", vec![ok(found(vec![wordy]), None)]),
            (
                "deep",
                vec![ok(found(vec![finding("c.rs", 9, "P2", "rollout")]), None)],
            ),
            (
                "audit-first",
                vec![ok(json!({"verdicts": [verdict("first:0", "kept")]}), None)],
            ),
            (
                "audit-second",
                vec![ok(json!({"verdicts": [verdict("second:0", "kept")]}), None)],
            ),
            (
                "audit-deep",
                vec![ok(json!({"verdicts": [verdict("deep:0", "kept")]}), None)],
            ),
        ];
        let (ran, log) = drive(&g, plan, &["first"], &[], 4, |d| {
            d.wait_for("end:second");
            assert!(!d.has("start:deep"), "deep waits for first");
            d.release("first");
        });
        assert!(position(&log, "end:first") < position(&log, "start:deep"));
        assert!(position(&log, "end:second") < position(&log, "start:deep"));

        let prompt = &ran.stub.calls_to("deep")[0].prompt;
        let (_, section) = prompt.split_once(ALREADY).expect("the section");
        let lines: Vec<&str> = section.lines().skip(1).take(2).collect();
        assert_eq!(lines[0], "  a.rs:1 [P1, logic] description of a.rs:1 logic");
        assert!(
            lines[1].starts_with("  b.rs:7 [P3, names] two lines xxx"),
            "{}",
            lines[1]
        );
        assert!(
            lines[1].ends_with(&format!("{}...", "x".repeat(300 - "two lines ".len()))),
            "cut at 300 characters: {}",
            lines[1]
        );
        assert!(!ran.stub.calls_to("first")[0].prompt.contains(ALREADY));

        let audit = &ran.stub.calls_to("audit-deep")[0].prompt;
        assert!(
            audit.contains("deep:0") && !audit.contains("first:") && !audit.contains("second:"),
            "{audit}"
        );
        assert_eq!(ran.node("deep").needs, ["first", "second"]);
        let m = manifest(&ran);
        assert_eq!(m["nodes"][2]["needs"], json!(["first", "second"]));
        assert!(m["nodes"][0].get("needs").is_none());
    }

    #[test]
    fn a_skipped_need_does_not_block_a_generator_and_adds_nothing_to_its_prompt() {
        let g = funnel("needs = [\"first\"]");
        let plan = vec![("second", clean()), ("deep", vec![ok(found(vec![]), None)])];
        let ran = go(&g, plan, &["first"]);
        assert_eq!(ran.node("first").status, "skipped");
        assert_eq!(ran.node("deep").status, "ok");
        assert!(!ran.stub.calls_to("deep")[0]
            .prompt
            .contains("already reported by earlier readers"));
    }

    #[test]
    fn a_failed_need_does_not_block_a_generator_and_the_others_still_count() {
        let g = funnel("needs = [\"first\", \"second\"]");
        let plan = vec![
            ("first", vec![Outcome::failed("could not start claude")]),
            (
                "second",
                vec![ok(found(vec![finding("b.rs", 2, "P2", "logic")]), None)],
            ),
            ("deep", clean()),
            (
                "audit-second",
                vec![ok(json!({"verdicts": [verdict("second:0", "kept")]}), None)],
            ),
        ];
        let ran = go(&g, plan, &[]);
        assert_eq!(ran.node("first").status, "failed");
        assert_eq!(ran.node("deep").status, "ok");
        let prompt = &ran.stub.calls_to("deep")[0].prompt;
        assert!(
            prompt.contains(ALREADY) && prompt.contains("b.rs:2 [P2, logic]"),
            "{prompt}"
        );

        let alone = go(
            &funnel("needs = [\"first\"]"),
            vec![
                ("first", vec![Outcome::failed("boom")]),
                ("second", clean()),
                ("deep", clean()),
            ],
            &[],
        );
        assert_eq!(alone.node("deep").status, "ok");
        assert!(!alone.stub.calls_to("deep")[0]
            .prompt
            .contains("already reported by earlier readers"));
    }

    #[test]
    fn a_need_that_found_nothing_leaves_the_section_out() {
        let ran = go(
            &funnel("needs = [\"first\"]"),
            vec![("first", clean()), ("second", clean()), ("deep", clean())],
            &[],
        );
        assert!(!ran.stub.calls_to("deep")[0]
            .prompt
            .contains("already reported by earlier readers"));
    }

    fn audited_plan() -> Vec<(&'static str, Vec<Outcome>)> {
        vec![
            (
                "code",
                vec![ok(
                    found(vec![finding("a.rs", 1, "P1", "logic")]),
                    Some("g1"),
                )],
            ),
            (
                "releng",
                vec![ok(
                    found(vec![finding("b.rs", 2, "P2", "rollout")]),
                    Some("g2"),
                )],
            ),
            (
                "audit",
                vec![ok(
                    json!({"verdicts": [verdict("code:0", "kept"), verdict("releng:0", "kept")]}),
                    Some("c1"),
                )],
            ),
        ]
    }

    fn code_audit_only() -> Vec<(&'static str, Vec<Outcome>)> {
        vec![(
            "audit",
            vec![ok(
                json!({"verdicts": [verdict("code:0", "kept")]}),
                Some("c1"),
            )],
        )]
    }

    fn no_calls(ran: &Ran) -> bool {
        ran.stub.calls.lock().unwrap().is_empty()
    }

    #[test]
    fn a_reused_generator_is_not_launched_and_its_findings_reach_the_report() {
        let g = graph(&["code", "releng"], true);
        let first = go(&g, audited_plan(), &[]);
        let second = go_from(&g, Some(&first), |_| {}, vec![], &[]);

        assert!(no_calls(&second), "nothing was launched");
        for name in ["code", "releng", "audit"] {
            let n = second.node(name);
            assert_eq!((n.status, n.cost_usd, n.wall_ms), ("ok", 0.0, 0), "{name}");
            assert_eq!(n.reused_from.as_deref(), Some(first.out.to_str().unwrap()));
            assert!(second
                .out
                .join("nodes")
                .join(name)
                .join("prompt.md")
                .is_file());
        }
        assert_eq!(second.node("code").findings, Some(1));
        assert_eq!(second.read("findings.jsonl"), first.read("findings.jsonl"));
        assert_eq!(second.read("verdicts.jsonl"), first.read("verdicts.jsonl"));
        assert_eq!(
            second.read("findings-code.jsonl"),
            first.read("findings-code.jsonl")
        );
        let m = manifest(&second);
        assert_eq!(
            (m["cost_usd"].as_f64(), m["audited"].clone()),
            (Some(0.0), json!(true))
        );
        assert_eq!(m["nodes"][0]["reused_from"], first.out.to_str().unwrap());
        assert_eq!(m["meta"]["code"], "read the diff");
        assert!(
            second.summary.report.contains("| code | ok, reused |"),
            "{}",
            second.summary.report
        );
    }

    #[test]
    fn a_changed_effort_on_the_critic_reruns_it_over_the_generators_it_reused() {
        let g = graph(&["code", "releng"], true);
        let first = go(&g, audited_plan(), &[]);
        let mut again = graph(&["code", "releng"], true);
        again.nodes[2].effort = Some("high".into());
        let plan = vec![(
            "audit",
            vec![ok(
                json!({"verdicts": [verdict("code:0", "false-positive"), verdict("releng:0", "kept")]}),
                Some("c2"),
            )],
        )];
        let second = go_from(&again, Some(&first), |_| {}, plan, &[]);

        assert!(
            second.stub.calls_to("code").is_empty() && second.stub.calls_to("releng").is_empty()
        );
        let calls = second.stub.calls_to("audit");
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0].prompt.contains("code:0") && calls[0].prompt.contains("releng:0"),
            "{}",
            calls[0].prompt
        );
        assert_eq!(second.node("audit").reused_from, None);
        assert_eq!(second.node("audit").effort.as_deref(), Some("high"));
        assert!(
            second.node("code").reused_from.is_some()
                && second.node("releng").reused_from.is_some()
        );
        let kept: Vec<Value> = second.rows("findings.jsonl");
        assert_eq!(kept.len(), 1, "the new verdict dropped code:0");
        assert_eq!(second.node("audit").cost_usd, 0.5);
    }

    fn lens_graph(dir: &Path) -> Graph {
        let node = "provider = \"command\"\ncommand = [\"true\"]\n";
        let text = format!(
            "[[node]]\nname = \"a\"\nkind = \"generator\"\nsystem_prompt = [\"a.md\"]\n{node}\
             [[node]]\nname = \"b\"\nkind = \"generator\"\n{node}\
             [[node]]\nname = \"audit\"\nkind = \"critic\"\nneeds = [\"a\", \"b\"]\nper_need = true\n{node}\
             [[node]]\nname = \"merge\"\nkind = \"merger\"\n{node}"
        );
        std::fs::write(dir.join("a.md"), "Read for logic.\n").unwrap();
        Graph::parse(&text, dir, "test").unwrap()
    }

    fn lens_plan() -> Vec<(&'static str, Vec<Outcome>)> {
        vec![
            (
                "a",
                vec![ok(
                    found(vec![finding("a.rs", 1, "P2", "style")]),
                    Some("ga"),
                )],
            ),
            (
                "b",
                vec![ok(
                    found(vec![finding("a.rs", 2, "P2", "style")]),
                    Some("gb"),
                )],
            ),
            (
                "audit-a",
                vec![ok(
                    json!({"verdicts": [verdict("a:0", "kept")]}),
                    Some("ca"),
                )],
            ),
            (
                "audit-b",
                vec![ok(
                    json!({"verdicts": [verdict("b:0", "kept")]}),
                    Some("cb"),
                )],
            ),
            ("merge", vec![ok(json!({"groups": []}), Some("m"))]),
        ]
    }

    #[test]
    fn a_changed_generator_prompt_reruns_it_its_critic_instance_and_the_merger_only() {
        let dir = scratch();
        let g = lens_graph(&dir);
        let first = go(&g, lens_plan(), &[]);
        assert_eq!(first.stub.calls_to("merge").len(), 1);

        std::fs::write(dir.join("a.md"), "Read for logic, and for rollout.\n").unwrap();
        let plan = vec![
            (
                "a",
                vec![ok(
                    found(vec![finding("a.rs", 1, "P2", "style")]),
                    Some("ga2"),
                )],
            ),
            (
                "audit-a",
                vec![ok(
                    json!({"verdicts": [verdict("a:0", "kept")]}),
                    Some("ca2"),
                )],
            ),
            ("merge", vec![ok(json!({"groups": []}), Some("m2"))]),
        ];
        let second = go_from(&g, Some(&first), |_| {}, plan, &[]);

        for name in ["a", "audit-a", "merge"] {
            assert_eq!(second.stub.calls_to(name).len(), 1, "{name} reran");
            assert_eq!(second.node(name).reused_from, None, "{name}");
        }
        for name in ["b", "audit-b"] {
            assert!(second.stub.calls_to(name).is_empty(), "{name} did not run");
            assert!(second.node(name).reused_from.is_some(), "{name}");
        }
        assert!(
            second.stub.calls_to("merge")[0].prompt.contains("a.rs"),
            "the merger read the reused finding too"
        );
        assert!(second.stub.calls_to("merge")[0]
            .prompt
            .contains("\"index\":\"b:0\""));
        assert_eq!(
            second.rows("verdicts.jsonl").len(),
            2,
            "the reused instance's verdict is still in the union"
        );
    }

    #[test]
    fn a_change_that_is_not_the_earlier_one_reuses_nothing() {
        let g = graph(&["code", "releng"], true);
        let first = go(&g, audited_plan(), &[]);
        let other_head: fn(&mut Target) = |t| t.head_sha = "c".repeat(40);
        let other_diff: fn(&mut Target) = |t| t.diff.push_str("+one more line\n");
        for edit in [other_head, other_diff] {
            let second = go_from(&g, Some(&first), edit, audited_plan(), &[]);
            for name in ["code", "releng", "audit"] {
                assert_eq!(second.stub.calls_to(name).len(), 1, "{name} ran again");
                assert_eq!(second.node(name).reused_from, None);
            }
        }
        let ws = scratch();
        let mut t = target(&ws);
        t.head_sha = "c".repeat(40);
        assert!(Previous::open(&first.out, &t)
            .unwrap()
            .unusable
            .unwrap()
            .contains("..cccccccc"));
    }

    #[test]
    fn a_node_that_failed_or_left_no_output_is_run_again() {
        let g = graph(&["code", "releng"], true);
        let releng_audit = vec![
            ("code", vec![Outcome::failed("boom")]),
            audited_plan().remove(1),
            (
                "audit",
                vec![ok(
                    json!({"verdicts": [verdict("releng:0", "kept")]}),
                    Some("c1"),
                )],
            ),
        ];
        let first = go(&g, releng_audit, &[]);
        assert_eq!(first.node("code").status, "failed");
        let second = go_from(&g, Some(&first), |_| {}, audited_plan(), &[]);
        assert_eq!(
            second.stub.calls_to("code").len(),
            1,
            "a failed node is not reusable"
        );
        assert!(second.stub.calls_to("releng").is_empty());
        assert_eq!(
            second.stub.calls_to("audit").len(),
            1,
            "and its critic reruns with it"
        );

        std::fs::remove_file(second.out.join("nodes/releng/output.json")).unwrap();
        let mut plan = code_audit_only();
        plan.push(("releng", vec![ok(found(vec![]), None)]));
        let third = go_from(&g, Some(&second), |_| {}, plan, &[]);
        assert_eq!(
            third.stub.calls_to("releng").len(),
            1,
            "a bundle without the node's output cannot supply it"
        );
        assert!(third.stub.calls_to("code").is_empty());
    }

    /// Finishes the bundle at `dir` in place.
    fn resume_dir(
        graph: &Graph,
        dir: &Path,
        plan: Vec<(&str, Vec<Outcome>)>,
    ) -> Result<Ran, String> {
        let ws = scratch();
        let stub = Stub::with(plan);
        let t = target(&ws);
        let reuse = Some(Previous::resuming(dir, &t)?);
        let opts = Options {
            out: dir.to_path_buf(),
            run_id: "resumed".into(),
            skip: vec![],
            concurrency: 4,
            reuse,
            no_prewarm: false,
            no_gh: None,
        };
        let summary = run(
            graph,
            &t,
            &opts,
            Providers {
                claude: &stub,
                command: &stub,
            },
            &|_| {},
        )?;
        Ok(Ran {
            summary,
            out: dir.to_path_buf(),
            stub,
        })
    }

    fn code_failed_first() -> Ran {
        let plan = vec![
            ("code", vec![Outcome::failed("usage limit reached")]),
            audited_plan().remove(1),
            (
                "audit",
                vec![ok(
                    json!({"verdicts": [verdict("releng:0", "kept")]}),
                    Some("c1"),
                )],
            ),
        ];
        go(&graph(&["code", "releng"], true), plan, &[])
    }

    #[test]
    fn a_resume_reruns_the_failed_node_and_what_reads_it_in_the_same_bundle() {
        let g = graph(&["code", "releng"], true);
        let first = code_failed_first();
        assert!(!first.summary.complete);
        let before = manifest(&first);

        let second = resume_dir(&g, &first.out, audited_plan()).unwrap();
        assert!(second.summary.complete);
        assert!(
            second.stub.calls_to("releng").is_empty(),
            "the ok node is kept"
        );
        assert_eq!(second.stub.calls_to("code").len(), 1);
        assert_eq!(
            second.stub.calls_to("audit").len(),
            1,
            "its critic read different findings before"
        );
        assert!(second.stub.calls_to("audit")[0].prompt.contains("code:0"));
        assert_eq!(second.node("code").status, "ok");
        assert!(second.node("releng").kept && second.node("releng").reused_from.is_none());
        assert_eq!(
            second.node("releng").cost_usd,
            0.5,
            "a kept node keeps its cost"
        );

        let m = manifest(&second);
        assert_eq!(m["started_at"], before["started_at"]);
        assert_eq!(m["run_id"], before["run_id"]);
        assert_eq!(m["resumed_at"].as_array().unwrap().len(), 1);
        assert!(m.get("in_progress").is_none());
        // releng 0.5, audit 0.5 spent before and again now, code 0.5 now.
        assert_eq!(m["cost_usd"].as_f64(), Some(2.0));
        assert_eq!(second.rows("findings.jsonl").len(), 2);
        assert_eq!(second.node("releng").findings, Some(1));

        let third = resume_dir(&g, &first.out, vec![]).unwrap();
        assert!(
            no_calls(&third),
            "a finished bundle has nothing left to run"
        );
        assert_eq!(manifest(&third)["resumed_at"].as_array().unwrap().len(), 2);
        assert_eq!(manifest(&third)["cost_usd"].as_f64(), Some(2.0));
    }

    #[test]
    fn a_resume_with_other_settings_or_another_change_is_refused() {
        let first = code_failed_first();
        let mut hotter = graph(&["code", "releng"], true);
        hotter.nodes[2].effort = Some("high".into());
        let err = resume_dir(&hotter, &first.out, vec![]).err().unwrap();
        assert!(
            err.contains("effort of node `audit`")
                && err.contains("unset")
                && err.contains("`high`"),
            "{err}"
        );

        let mut other_model = graph(&["code", "releng"], true);
        other_model.nodes[0].model = Some("sonnet".into());
        assert!(resume_dir(&other_model, &first.out, vec![])
            .err()
            .unwrap()
            .contains("model of node `code`"));

        let mut renamed = graph(&["code", "releng"], true);
        renamed.name = "other".into();
        assert!(resume_dir(&renamed, &first.out, vec![])
            .err()
            .unwrap()
            .contains("graph was `test`"));

        let ws = scratch();
        let mut t = target(&ws);
        t.head_sha = "c".repeat(40);
        assert!(Previous::resuming(&first.out, &t)
            .err()
            .unwrap()
            .contains("--resume"));
    }

    #[test]
    fn a_run_that_dies_after_its_first_node_leaves_a_bundle_a_resume_completes() {
        let g = graph(&["code", "releng"], true);
        let leftover = scratch().join("bundle");
        let (whole, _) = drive(&g, audited_plan(), &["releng"], &[], 1, |d| {
            d.wait_for("start:releng");
            // What is on disk while the second node is still running is what a kill would leave.
            let mut tries = 0;
            let seen = loop {
                let m = std::fs::read_to_string(d.out.join("manifest.json"))
                    .ok()
                    .and_then(|t| serde_json::from_str::<Value>(&t).ok());
                if let Some(m) = m.filter(|m| {
                    m["nodes"]
                        .as_array()
                        .is_some_and(|n| n.iter().any(|r| r["name"] == "code"))
                }) {
                    break m;
                }
                tries += 1;
                assert!(tries < 500, "no manifest with the first node");
                std::thread::sleep(Duration::from_millis(20));
            };
            assert_eq!(seen["in_progress"], true);
            assert!(
                d.out.join("nodes/code/output.json").is_file()
                    && d.out.join("findings-code.jsonl").is_file()
            );
            crate::reuse::copy_dir(&d.out, &leftover).unwrap();
            d.release("releng");
        });
        assert!(whole.summary.complete);

        let dead: Value =
            serde_json::from_str(&std::fs::read_to_string(leftover.join("manifest.json")).unwrap())
                .unwrap();
        let names: Vec<&str> = dead["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["code"]);

        let done = resume_dir(&g, &leftover, audited_plan()).unwrap();
        assert!(done.summary.complete);
        assert!(
            done.stub.calls_to("code").is_empty(),
            "the node that finished is not run again"
        );
        assert_eq!(
            done.stub.calls_to("releng").len() + done.stub.calls_to("audit").len(),
            2
        );
        let m = manifest(&done);
        assert!(m.get("in_progress").is_none());
        assert_eq!(m["started_at"], dead["started_at"]);
        assert_eq!(m["nodes"].as_array().unwrap().len(), 3);
        assert_eq!(done.rows("findings.jsonl").len(), 2);
        assert!(done.summary.report.contains("| code | ok, reused |"));
    }

    #[test]
    fn a_node_left_out_both_times_does_not_stop_its_critic_being_reused() {
        let g = graph(&["code", "releng"], true);
        let mut plan = code_audit_only();
        plan.push(audited_plan().remove(0));
        let first = go(&g, plan, &["releng"]);
        let second = go_from(&g, Some(&first), |_| {}, vec![], &["releng"]);
        assert!(no_calls(&second));
        assert!(second.node("audit").reused_from.is_some());

        let mut again = code_audit_only();
        again.push(("releng", vec![ok(found(vec![]), None)]));
        let third = go_from(&g, Some(&first), |_| {}, again, &[]);
        assert_eq!(
            third.stub.calls_to("releng").len(),
            1,
            "it was left out before, so there is nothing to take"
        );
        assert_eq!(
            third.stub.calls_to("audit").len(),
            1,
            "and its critic has a new input"
        );
    }

    const SUMMARY: &str = "## Files\n- a.rs: the changed file\n\n## Symbols changed\n- f: called from b.rs:3\n\n## Conventions in scope\n- none\n\n## Open questions\n- why f?";

    fn summary(session: &str) -> Outcome {
        ok(
            json!({"findings": [], "upstream_reading": [], "meta": SUMMARY}),
            Some(session),
        )
    }

    /// A prewarm node `warm` and generators `a` and `b` forked from it, each with a lens file of its own.
    fn fork_graph(dir: &Path, b_model: &str) -> Graph {
        std::fs::write(dir.join("reader.md"), "---\nname: reader\n---\nYou read.\n").unwrap();
        std::fs::write(dir.join("lens-a.md"), "---\nname: a\n---\nLENS A: logic.\n").unwrap();
        std::fs::write(dir.join("lens-b.md"), "LENS B: rollout.\n").unwrap();
        let text = format!(
            "[defaults]\nprovider = \"claude\"\nmodel = \"opus\"\ntools = [\"Read\", \"Grep\"]\nconventions = \"inline\"\n\
             [[node]]\nname = \"warm\"\nkind = \"prewarm\"\nsystem_prompt = [\"reader.md\"]\nmax_turns = 30\nread_budget_bytes = 1234\n\
             [[node]]\nname = \"a\"\nkind = \"generator\"\nfork_from = \"warm\"\nsystem_prompt = [\"lens-a.md\"]\n\
             [[node]]\nname = \"b\"\nkind = \"generator\"\nfork_from = \"warm\"\nmodel = \"{b_model}\"\nsystem_prompt = [\"lens-b.md\"]\n"
        );
        Graph::parse(&text, dir, "forks").unwrap()
    }

    fn fork_plan() -> Vec<(&'static str, Vec<Outcome>)> {
        vec![
            ("warm", vec![summary("pw")]),
            (
                "a",
                vec![ok(
                    found(vec![finding("a.rs", 1, "P2", "logic")]),
                    Some("fa"),
                )],
            ),
            ("b", vec![ok(found(vec![]), Some("fb"))]),
        ]
    }

    #[test]
    fn a_prewarm_node_reads_and_leaves_its_session_and_summary() {
        let dir = scratch();
        let g = fork_graph(&dir, "opus");
        let ran = go_on(&g, with_rules, fork_plan(), &[]);
        let prompt = &ran.stub.calls_to("warm")[0].prompt;
        assert!(
            prompt.starts_with("Read this change so that reviewers continuing this session"),
            "{prompt}"
        );
        assert!(
            prompt.contains("changed files:\n  sub/a.rs")
                && prompt.contains("The diff:")
                && prompt.contains("root rule: use goqu"),
            "{prompt}"
        );
        assert!(
            prompt.contains("about 1234 bytes of file content read in total and 30 turns"),
            "{prompt}"
        );
        assert!(
            prompt.contains("callers, found with Grep") && !prompt.contains("With LSP"),
            "no LSP tool, no LSP step"
        );
        for h in crate::schema::SUMMARY_HEADINGS {
            assert!(prompt.contains(h), "{h}");
        }
        assert!(!prompt.contains("lens"), "a prewarm node reads for no lens");

        let out: Value = serde_json::from_str(&ran.read("nodes/warm/output.json")).unwrap();
        assert_eq!(
            (out["session_id"].as_str(), out["summary"].as_str()),
            (Some("pw"), Some(SUMMARY))
        );
        assert_eq!(ran.node("warm").session_id.as_deref(), Some("pw"));
        assert_eq!(ran.node("warm").cache_ttl, Some("1h"));
        let m = manifest(&ran);
        assert_eq!(m["prewarm_session_id"], "pw");
        assert_eq!(
            m["lenses"],
            json!(["a", "b"]),
            "the prewarm node is not a lens"
        );
        assert_eq!(m["nodes"][0]["usage"]["input_tokens"], 0);
        assert!(m["nodes"][0]["usage"]
            .get("cache_read_input_tokens")
            .is_some());
    }

    #[test]
    fn a_summary_missing_a_heading_is_sent_back_once_and_then_fails_the_node() {
        let dir = scratch();
        let g = fork_graph(&dir, "opus");
        let short = |s: &str| {
            ok(
                json!({"findings": [], "upstream_reading": [], "meta": s}),
                Some("pw"),
            )
        };
        let plan = vec![
            ("warm", vec![short("## Files\n- a.rs"), summary("pw")]),
            ("a", vec![ok(found(vec![]), Some("fa"))]),
            ("b", vec![ok(found(vec![]), Some("fb"))]),
        ];
        let ran = go(&g, plan, &[]);
        let calls = ran.stub.calls_to("warm");
        assert_eq!(calls.len(), 2);
        assert_eq!(
            (calls[1].resume.as_deref(), calls[1].fork),
            (Some("pw"), false),
            "the retry continues the prewarm session itself"
        );
        assert!(
            calls[1].prompt.contains("lacks ## Symbols changed")
                && calls[1].prompt.contains("Do not read anything more"),
            "{}",
            calls[1].prompt
        );
        assert_eq!(ran.node("warm").status, "ok");

        let judged = json!({"findings": [finding("a.rs", 1, "P2", "x")], "upstream_reading": [], "meta": SUMMARY});
        let plan = vec![
            ("warm", vec![ok(judged, Some("pw")), summary("pw")]),
            ("a", clean()),
            ("b", clean()),
        ];
        let ran = go(&g, plan, &[]);
        assert!(
            ran.stub.calls_to("warm")[1]
                .prompt
                .contains("reports no findings"),
            "a reader that judges is sent back"
        );

        let plan = vec![
            ("warm", vec![short("nothing"), short("still nothing")]),
            ("a", vec![ok(found(vec![]), Some("fa"))]),
            ("b", vec![ok(found(vec![]), Some("fb"))]),
        ];
        let ran = go(&g, plan, &[]);
        assert_eq!(ran.node("warm").status, "failed");
        assert!(ran
            .node("warm")
            .reason
            .as_deref()
            .unwrap()
            .contains("lacks"));
    }

    #[test]
    fn a_fork_resumes_a_copy_of_the_prewarm_session_under_the_prewarms_system_prompt() {
        let dir = scratch();
        let g = fork_graph(&dir, "opus");
        let ran = go_on(&g, with_rules, fork_plan(), &[]);
        let a = &ran.stub.calls_to("a")[0];
        assert_eq!((a.resume.as_deref(), a.fork), (Some("pw"), true));
        assert_eq!(ran.stub.calls_to("warm")[0].resume, None);
        assert_eq!(
            ran.read("nodes/a/system-prompt.md"),
            "You read.\n\n",
            "the reader's prompt, frontmatter dropped, and no lens"
        );
        assert_eq!(
            ran.read("nodes/a/system-prompt.md"),
            ran.read("nodes/warm/system-prompt.md")
        );
        assert_eq!(
            ran.read("nodes/b/system-prompt.md"),
            ran.read("nodes/warm/system-prompt.md")
        );

        let lens_file = dir.join("lens-a.md");
        let want = format!("Your lens:\n\nLENS A: logic.\n---\n\nlens instructions assembled from: {}\n\nReview this change with your lens. Everything you read in the prewarm is still in context; cite it, re-open a file only when you need lines you did not read.", lens_file.display());
        assert!(a.prompt.starts_with(&want), "{}", a.prompt);
        assert!(
            !a.prompt.contains("name: a"),
            "frontmatter is dropped from the lens"
        );
        assert!(
            !a.prompt.contains("The diff:") && !a.prompt.contains("changed files:"),
            "the session already holds them"
        );
        assert!(ran.stub.calls_to("b")[0]
            .prompt
            .contains("LENS B: rollout."));

        let m = manifest(&ran);
        assert_eq!(m["nodes"][1]["fork_from"], "warm");
        assert_eq!(m["nodes"][1]["cache_ttl"], "1h");
        assert_eq!(m["warnings"], json!([]));
        assert_eq!(
            m["conventions"]["modes"],
            json!({"a": "inline", "b": "inline"}),
            "the rules came with the prewarm's prompt"
        );
        assert_eq!(ran.rows("findings.jsonl").len(), 1);
    }

    #[test]
    fn a_rejected_fork_answer_is_retried_in_the_forks_own_session() {
        let dir = scratch();
        let g = fork_graph(&dir, "opus");
        let plan = vec![
            ("warm", vec![summary("pw")]),
            (
                "a",
                vec![
                    rejected(Some("fa")),
                    ok(found(vec![finding("a.rs", 4, "P1", "logic")]), Some("fa")),
                ],
            ),
            ("b", vec![ok(found(vec![]), Some("fb"))]),
        ];
        let ran = go(&g, plan, &[]);
        let calls = ran.stub.calls_to("a");
        assert_eq!(calls.len(), 2);
        assert_eq!(
            (calls[1].resume.as_deref(), calls[1].fork),
            (Some("fa"), false)
        );
        assert!(
            calls[1].prompt.contains("rejected") && calls[1].prompt.contains("corrected output"),
            "{}",
            calls[1].prompt
        );
        assert_eq!(ran.node("a").findings, Some(1));
    }

    #[test]
    fn a_fork_on_another_model_than_its_prewarm_is_warned_about() {
        let dir = scratch();
        let g = fork_graph(&dir, "sonnet");
        let ran = go(&g, fork_plan(), &[]);
        let m = manifest(&ran);
        let warnings = m["warnings"].as_array().unwrap();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        let w = warnings[0].as_str().unwrap();
        assert!(
            w.starts_with("b: model sonnet differs from prewarm `warm`'s opus")
                && w.contains("cache does not"),
            "{w}"
        );
        assert_eq!(ran.node("b").warnings.len(), 1);
        assert!(ran.node("a").warnings.is_empty());
        assert!(ran.read("report.md").contains("**Warning:** `b`"));
        assert_eq!(
            ran.stub.calls_to("b")[0].resume.as_deref(),
            Some("pw"),
            "it still forks"
        );
    }

    #[test]
    fn without_prewarm_the_forks_run_as_plain_generators_with_the_lens_in_the_system_prompt() {
        let dir = scratch();
        let g = fork_graph(&dir, "opus");
        let ws = scratch();
        let out = ws.join("bundle");
        let stub = Stub::with(vec![
            ("a", vec![ok(found(vec![]), Some("fa"))]),
            ("b", vec![ok(found(vec![]), Some("fb"))]),
        ]);
        let t = target(&ws);
        let opts = Options {
            out: out.clone(),
            run_id: "r".into(),
            skip: vec![],
            concurrency: 4,
            reuse: None,
            no_prewarm: true,
            no_gh: None,
        };
        let summary = run(
            &g,
            &t,
            &opts,
            Providers {
                claude: &stub,
                command: &stub,
            },
            &|_| {},
        )
        .unwrap();
        let ran = Ran { summary, out, stub };

        assert!(ran.stub.calls_to("warm").is_empty());
        assert_eq!(ran.node("warm").status, "skipped");
        assert_eq!(
            ran.node("warm").reason.as_deref(),
            Some("--no-prewarm: its forks run as plain generators")
        );
        let a = &ran.stub.calls_to("a")[0];
        assert_eq!((a.resume.as_deref(), a.fork), (None, false));
        assert!(
            a.prompt.starts_with("Review this change with your lens.")
                && a.prompt.contains("The diff:")
                && !a.prompt.contains("Your lens:"),
            "{}",
            a.prompt
        );
        assert_eq!(
            ran.read("nodes/a/system-prompt.md"),
            "You read.\n\nLENS A: logic.\n\n",
            "the old shape: reader, then lens"
        );
        assert_eq!(ran.node("a").fork_from, None);
        let m = manifest(&ran);
        assert_eq!(
            (m["no_prewarm"].as_bool(), m["warnings"].clone()),
            (Some(true), json!([])),
            "asked for, so nothing to warn about"
        );
    }

    #[test]
    fn a_fork_whose_prewarm_failed_runs_as_a_plain_generator_and_says_so() {
        let dir = scratch();
        let g = fork_graph(&dir, "opus");
        let plan = vec![
            ("warm", vec![rejected(None)]),
            ("a", vec![ok(found(vec![]), Some("fa"))]),
            ("b", vec![ok(found(vec![]), Some("fb"))]),
        ];
        let ran = go(&g, plan, &[]);
        assert_eq!(ran.node("warm").status, "failed");
        let a = &ran.stub.calls_to("a")[0];
        assert_eq!(a.resume, None);
        assert_eq!(ran.node("a").status, "ok");
        assert_eq!(
            ran.read("nodes/a/system-prompt.md"),
            "You read.\n\nLENS A: logic.\n\n"
        );
        let w = &ran.node("a").warnings;
        assert!(
            w.len() == 1
                && w[0].starts_with("prewarm `warm` left no session to fork (no structured output")
                && w[0].contains("plain generator"),
            "{w:?}"
        );
        assert!(!ran.summary.complete);
    }

    #[test]
    fn forks_start_after_the_prewarm_and_run_side_by_side() {
        let dir = scratch();
        let g = fork_graph(&dir, "opus");
        let waves: Vec<Vec<&str>> = g
            .waves()
            .unwrap()
            .iter()
            .map(|w| w.iter().map(|n| n.name.as_str()).collect())
            .collect();
        assert_eq!(waves, vec![vec!["warm"], vec!["a", "b"]]);
    }

    #[test]
    fn a_changed_lens_reruns_its_fork_and_leaves_the_prewarm_and_the_other_fork_reused() {
        let dir = scratch();
        let g = fork_graph(&dir, "opus");
        let first = go(&g, fork_plan(), &[]);
        std::fs::write(dir.join("lens-a.md"), "LENS A: logic and tests.\n").unwrap();
        let second = go_from(
            &g,
            Some(&first),
            |_| {},
            vec![("a", vec![ok(found(vec![]), Some("fa2"))])],
            &[],
        );
        assert!(
            second.stub.calls_to("warm").is_empty() && second.node("warm").reused_from.is_some()
        );
        assert!(second.stub.calls_to("b").is_empty() && second.node("b").reused_from.is_some());
        assert_eq!(second.stub.calls_to("a").len(), 1);
        assert_eq!(
            second.stub.calls_to("a")[0].resume.as_deref(),
            Some("pw"),
            "it forks the reused prewarm's session"
        );
        assert!(second.stub.calls_to("a")[0].prompt.contains("and tests"));

        std::fs::write(dir.join("reader.md"), "You read twice.\n").unwrap();
        let third = go_from(
            &g,
            Some(&second),
            |_| {},
            vec![
                ("warm", vec![summary("pw3")]),
                ("a", clean()),
                ("b", clean()),
            ],
            &[],
        );
        for name in ["warm", "a", "b"] {
            assert_eq!(
                third.stub.calls_to(name).len(),
                1,
                "{name} reran, since the session its forks continue is new"
            );
        }
        assert_eq!(third.stub.calls_to("b")[0].resume.as_deref(), Some("pw3"));
    }

    #[test]
    fn the_claude_command_lines_for_a_prewarm_and_its_fork() {
        use crate::provider::Claude;
        let dir = scratch();
        let g = fork_graph(&dir, "opus");
        let claude = Claude {
            bin: "claude".into(),
        };
        let schema = generator_schema();
        let bundle = dir.join("bundle");
        let inv = |node: &'static str, resume: Option<&'static str>, fork: bool, schema: &Value| {
            let n = g.node(node).unwrap();
            claude
                .command(&Invocation {
                    node: n,
                    workspace: &dir,
                    prompt: "",
                    schema,
                    dir: &bundle.join(node),
                    attempt: 1,
                    resume,
                    fork,
                    verbatim: false,
                    no_gh: None,
                })
                .unwrap()
        };
        let warm = inv("warm", None, false, &schema);
        assert_eq!(
            warm.join(" "),
            format!(
                "claude -p --safe-mode --model opus --system-prompt-file {}/warm/system-prompt.md --json-schema {schema} --max-turns 30 --output-format stream-json --verbose --dangerously-skip-permissions --tools Read,Grep",
                bundle.display()
            )
        );
        let fork = inv("a", Some("pw"), true, &schema);
        assert_eq!(
            fork.join(" "),
            format!(
                "claude -p --safe-mode --resume pw --fork-session --model opus --system-prompt-file {}/a/system-prompt.md --json-schema {schema} --output-format stream-json --verbose --dangerously-skip-permissions --tools Read,Grep",
                bundle.display()
            )
        );
        let schema_arg = |argv: &Vec<String>| {
            argv.iter()
                .position(|a| a == "--json-schema")
                .map(|i| argv[i + 1].clone())
        };
        assert!(
            schema_arg(&warm).is_some() && schema_arg(&warm) == schema_arg(&fork),
            "one schema, so one tool list"
        );
        assert!(
            !warm.contains(&"--no-session-persistence".to_string())
                && !fork.contains(&"--no-session-persistence".to_string()),
            "forks resume the session from disk"
        );
        let retry = inv("a", Some("fa"), false, &schema);
        assert!(
            retry.contains(&"--resume".to_string())
                && !retry.contains(&"--fork-session".to_string())
        );
        let cold = |n: &Node| crate::provider::cache_env(n);
        assert_eq!(
            cold(g.node("warm").unwrap()),
            vec![("CLAUDE_CODE_PROMPT_CACHE_TTL", "1h")]
        );
        assert_eq!(
            cold(g.node("a").unwrap()),
            vec![("CLAUDE_CODE_PROMPT_CACHE_TTL", "1h")]
        );
        assert!(cold(&g.unforked(g.node("a").unwrap())).is_empty());
    }

    #[test]
    fn lsp_in_the_tool_list_adds_a_symbol_step_to_the_prewarm_prompt() {
        let dir = scratch();
        let mut g = fork_graph(&dir, "opus");
        for n in &mut g.nodes {
            n.tools.push("LSP".into());
        }
        let ran = go(&g, fork_plan(), &[]);
        let prompt = &ran.stub.calls_to("warm")[0].prompt;
        assert!(
            prompt.contains("5. With LSP, the definition and the references"),
            "{prompt}"
        );
    }
}
