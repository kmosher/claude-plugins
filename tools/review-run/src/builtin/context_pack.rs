//! `builtin:context-pack`: point the generators at the files outside the diff
//! that the changed files depend on or are depended on by, so a lens can check
//! a claim against the file it is about instead of trusting it.
//!
//! The node writes `{"diagnostics": [], "context": [...], "triggers": []}`.
//! Each context line is one pointer, never file contents:
//!
//! ```text
//! <changed file> → <kind>: <path>[:<line>] — <why>
//! ```
//!
//! The kinds are `referrer` (a file elsewhere that mentions a changed, renamed
//! or deleted path), `include` (a Makefile include, a relative TS/JS import, a
//! Dockerfile build input), `package` (a Go sibling in the same package) and
//! `sibling` (a function in the same file with a similar name to a changed
//! one). Four further sources emit free-form lines:
//!
//! - `cites #N: "<title>" (<state>)` for issues and pull requests named in the
//!   added lines, the commit messages or the PR body;
//! - `dependency <import> → <dir>` for modules newly imported;
//! - `precedent for <call>: <path:line>, ...` for qualified calls newly written;
//! - `upstream <pkg> <old>→<new> (latest <v>): <path>` for the dependency bumps
//!   and additions in the diff (see [`upstream`]).
//!
//! The whole output is capped at `CONTEXT_PACK_MAX_ITEMS` lines (default 118).
//! Any error yields fewer items, or none; this node never fails the run.

// `re!` compiles each pattern once per call site, which the lint cannot see.
#![allow(clippy::regex_creation_in_loops)]

mod pyish;
mod upstream;

use super::{Opts, Runner};
use crate::provider::NO_GH_ENV;
use crate::schema::Diagnostic;
use pyish::{basename, dirname, ext, fnmatch, join, normpath, py_str, splitlines};
use regex::{Captures, Regex};
use serde::Serialize;
use serde_json::Value;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const DEFAULT_MAX_ITEMS: usize = 118;
const MAX_FILES: usize = 30;
const MAX_REFERRERS: usize = 8;
const MAX_IMPORTS: usize = 6;
/// Seconds every `git` call and the stages around it share.
const DEADLINE_SECS: u64 = 8;

const GENERIC_BASENAMES: &[&str] = &[
    "index.ts",
    "index.js",
    "index.tsx",
    "main.go",
    "main.py",
    "main.rs",
    "mod.rs",
    "lib.rs",
    "readme.md",
    "makefile",
    "dockerfile",
    "build.bazel",
    "package.json",
    "config.ts",
    "config.go",
    "config.py",
    "utils.ts",
    "utils.go",
    "types.ts",
    "types.go",
    "test.ts",
    "setup.py",
    "__init__.py",
    "tsconfig.json",
    "go.mod",
    "cargo.toml",
    "pulumi.yaml",
    ".gitignore",
    ".dockerignore",
];
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "vendor",
    "third_party",
    "dist",
    "build",
    "bazel-*",
    "generated",
    "__generated__",
];
const SKIP_FILES: &[&str] = &[
    "*.pb.go",
    "*.pb.ts",
    "*_generated.*",
    "*.gen.*",
    "*.lock",
    "package-lock.json",
    "go.sum",
    "*.min.js",
    "*.snap",
    "CHANGELOG*",
    "*.svg",
    "*.png",
];
const KEYWORDS: &[&str] = &[
    "if",
    "for",
    "while",
    "switch",
    "catch",
    "function",
    "return",
    "constructor",
    "else",
    "do",
    "try",
];
const TS_EXTS: &[&str] = &[".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs"];

/// Seconds of the deadline the citation lookups may spend.
const CITE_BUDGET: Duration = Duration::from_secs(4);
const MAX_CITES: usize = 8;
const MAX_PRECEDENT_IDENTS: usize = 10;
const MAX_SITES: usize = 3;
const MAX_GO_MODULES: usize = 5;
const MAX_TS_PACKAGES: usize = 5;
const GH_TIMEOUT: Duration = Duration::from_secs(3);
const GO_LIST_TIMEOUT: Duration = Duration::from_secs(2);

/// Receivers and methods that are the standard library or a test harness: a
/// precedent for them teaches nothing.
const NOISE_RECEIVERS: &[&str] = &[
    "fmt", "errors", "strings", "strconv", "os", "io", "ioutil", "bytes", "sort", "time", "sync",
    "context", "log", "filepath", "path", "json", "reflect", "regexp", "bufio", "atomic", "slog",
    "t", "b", "require", "assert", "console", "this", "self", "super", "process", "window",
    "document", "Math", "expect", "JSON", "Object", "Array", "Promise", "err", "ctx", "wg", "mu",
    "cmd",
];
const NOISE_METHODS: &[&str] = &[
    "Error", "String", "Close", "Done", "Lock", "Unlock", "RLock", "RUnlock", "Add", "Wait",
    "Helper", "Run", "Fatal", "Fatalf", "Errorf", "Log", "Logf", "Skip", "Skipf", "Parallel",
    "Cleanup", "Unwrap", "Is", "As", "Len", "Get", "Set", "Sprintf", "Printf", "Println", "Equal",
    "NoError", "Nil", "True", "False", "log", "map", "filter", "forEach", "push", "join", "split",
    "then", "catch", "includes", "slice", "toString", "keys", "values", "entries", "trim",
    "replace", "test", "has", "get", "set", "add", "delete",
];
const NODE_BUILTINS: &[&str] = &[
    "fs",
    "path",
    "os",
    "util",
    "http",
    "https",
    "url",
    "stream",
    "events",
    "crypto",
    "child_process",
    "assert",
    "buffer",
    "zlib",
    "net",
    "tls",
    "dns",
    "readline",
    "process",
    "module",
    "worker_threads",
    "timers",
    "vm",
];

/// A regex compiled once per call site.
macro_rules! re {
    ($pattern:expr) => {{
        static RE: OnceLock<Regex> = OnceLock::new();
        RE.get_or_init(|| Regex::new($pattern).expect("static regex"))
    }};
}

/// The environment the node reads, gathered once so tests can set it.
pub struct Config {
    pub max_items: usize,
    pub no_gh: bool,
    /// `REVIEW_PR_BODY`: a file path, else the body text itself.
    pub pr_body: String,
    /// `REVIEW_PR_NUMBER`.
    pub pr_number: String,
    /// `GOMODCACHE`, when set and non-empty.
    pub go_mod_cache: Option<String>,
    pub home: Option<String>,
}

impl Config {
    pub fn from_env() -> Config {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        Config {
            max_items: var("CONTEXT_PACK_MAX_ITEMS")
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(DEFAULT_MAX_ITEMS),
            no_gh: var(NO_GH_ENV).is_some(),
            pr_body: var("REVIEW_PR_BODY").unwrap_or_default(),
            pr_number: var("REVIEW_PR_NUMBER").unwrap_or_default(),
            go_mod_cache: var("GOMODCACHE"),
            home: var("HOME"),
        }
    }
}

/// What every stage shares: the tools, the workspace and the clocks.
pub(crate) struct Ctx<'a> {
    run: &'a dyn Runner,
    cfg: &'a Config,
    ws: String,
    start: Instant,
    deadline: Instant,
}

impl<'a> Ctx<'a> {
    fn new(run: &'a dyn Runner, cfg: &'a Config, ws: &str) -> Ctx<'a> {
        let start = Instant::now();
        Ctx {
            run,
            cfg,
            ws: ws.to_string(),
            start,
            deadline: start + Duration::from_secs(DEADLINE_SECS),
        }
    }

    /// Time left on the shared deadline, never less than half a second.
    fn remaining(&self) -> Duration {
        self.deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(500))
    }

    fn spent(&self) -> bool {
        Instant::now() > self.deadline
    }

    /// Stdout of a git command, or "" on any failure or when the deadline is spent.
    fn git<S: AsRef<str>>(&self, args: &[S]) -> String {
        if self.spent() {
            return String::new();
        }
        let mut argv = vec!["-C".to_string(), self.ws.clone()];
        argv.extend(args.iter().map(|a| a.as_ref().to_string()));
        let opts = Opts {
            timeout: Some(self.remaining()),
            ..Opts::default()
        };
        match self.run.run("git", &argv, &opts) {
            Ok(c) if c.ok() => pyish::universal_newlines(&c.stdout),
            _ => String::new(),
        }
    }
}

/// A file's text as read in text mode, "" when it cannot be read.
fn read_text(path: &str) -> String {
    std::fs::read(path)
        .map(|b| pyish::universal_newlines(&String::from_utf8_lossy(&b)))
        .unwrap_or_default()
}

pub(crate) struct Repo {
    rev: String,
    tree: HashSet<String>,
    cache: RefCell<HashMap<String, String>>,
    edited: HashSet<String>,
}

impl Repo {
    fn new(ctx: &Ctx, head: &str) -> Repo {
        let listing = if head.is_empty() {
            ctx.git(&["ls-files"])
        } else {
            ctx.git(&["ls-tree", "-r", "--name-only", head])
        };
        Repo {
            rev: head.to_string(),
            tree: splitlines(&listing).into_iter().map(String::from).collect(),
            cache: RefCell::new(HashMap::new()),
            edited: HashSet::new(),
        }
    }

    /// A file at the head revision, else in the working tree; "" when absent.
    fn read(&self, ctx: &Ctx, path: &str) -> String {
        if let Some(t) = self.cache.borrow().get(path) {
            return t.clone();
        }
        let text = if self.rev.is_empty() {
            read_text(&join(&[&ctx.ws, path]))
        } else {
            ctx.git(&["show", &format!("{}:{path}", self.rev)])
        };
        self.cache.borrow_mut().insert(path.into(), text.clone());
        text
    }
}

/// One pointer before ranking. `changed` and `kind` are None for the
/// free-form lines, whose `why` is the whole message and `ptr` an identity
/// used only to drop duplicates.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Item {
    pri: i32,
    changed: Option<String>,
    kind: Option<&'static str>,
    ptr: String,
    why: String,
}

fn pointer(pri: i32, changed: &str, kind: &'static str, ptr: String, why: String) -> Item {
    Item {
        pri,
        changed: Some(changed.to_string()),
        kind: Some(kind),
        ptr,
        why,
    }
}

fn free_form(pri: i32, ptr: String, message: String) -> Item {
    Item {
        pri,
        changed: None,
        kind: None,
        ptr,
        why: message,
    }
}

fn skipped(path: &str) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    let (name, dirs) = parts.split_last().expect("split yields one part");
    dirs.iter().any(|p| SKIP_DIRS.iter().any(|d| fnmatch(p, d)))
        || SKIP_FILES.iter().any(|f| fnmatch(name, f))
}

/// `(status, old, new)` with status one of A M D R; renames need a base and head.
type Change = (char, String, String);

fn changed_set(ctx: &Ctx, base: &str, head: &str, contract: &Value) -> Result<Vec<Change>, String> {
    let mut out = Vec::new();
    if !base.is_empty() && !head.is_empty() {
        let raw = ctx.git(&["diff", "-M", "--name-status", "-z", base, head]);
        let toks: Vec<&str> = raw.split('\0').collect();
        let mut i = 0;
        while i < toks.len() && !toks[i].is_empty() {
            let st = toks[i].chars().next().expect("non-empty");
            let at = |k: usize| {
                toks.get(k)
                    .map(|s| s.to_string())
                    .ok_or_else(|| "list index out of range".to_string())
            };
            if st == 'R' || st == 'C' {
                out.push((if st == 'R' { 'R' } else { 'A' }, at(i + 1)?, at(i + 2)?));
                i += 3;
            } else {
                out.push((st, at(i + 1)?, at(i + 1)?));
                i += 2;
            }
        }
    }
    if out.is_empty() {
        if let Some(files) = contract.get("changed_files").and_then(Value::as_array) {
            out = files
                .iter()
                .filter_map(Value::as_str)
                .map(|f| ('M', f.to_string(), f.to_string()))
                .collect();
        }
    }
    Ok(out)
}

/// The path a `+++ ` header names, or None for `/dev/null`.
fn header_path(line: &str) -> Option<String> {
    let path = line[4..].trim();
    match path.strip_prefix("b/") {
        Some(rest) => Some(rest.to_string()),
        None if path == "/dev/null" => None,
        None => Some(path.to_string()),
    }
}

/// The new-side start line of a hunk header, 0 when it does not parse.
fn hunk_start(line: &str) -> usize {
    re!(r"^@@ -\d+(?:,\d+)? \+(\d+)")
        .captures(line)
        .and_then(|c| c[1].parse().ok())
        .unwrap_or(0)
}

/// The saved diff the harness gives the node, else git's.
fn diff_text(ctx: &Ctx, base: &str, head: &str, contract: &Value) -> String {
    if let Some(p) = contract.get("diff").and_then(Value::as_str) {
        let text = read_text(p);
        if !text.is_empty() {
            return text;
        }
    }
    if !base.is_empty() && !head.is_empty() {
        ctx.git(&["diff", "-U0", "-M", base, head])
    } else {
        String::new()
    }
}

/// `{new path: [added line numbers]}` of a unified diff.
fn added_lines(text: &str) -> HashMap<String, Vec<usize>> {
    let mut result: HashMap<String, Vec<usize>> = HashMap::new();
    let (mut path, mut n): (Option<String>, usize) = (None, 0);
    for line in splitlines(text) {
        if line.starts_with("+++ ") {
            path = header_path(line);
        } else if line.starts_with("@@") {
            n = hunk_start(line);
        } else if let (Some(p), true) = (&path, n != 0) {
            if line.starts_with('+') {
                result.entry(p.clone()).or_default().push(n);
                n += 1;
            } else if line.starts_with(' ') {
                n += 1;
            }
        }
    }
    result
}

/// `(new path, line text)` for every added line of a unified diff.
fn added_content(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut path: Option<String> = None;
    for line in splitlines(text) {
        if line.starts_with("+++ ") {
            path = header_path(line);
        } else if let (Some(rest), Some(p)) = (line.strip_prefix('+'), &path) {
            out.push((p.clone(), rest.to_string()));
        }
    }
    out
}

/// Lower-cased words of an identifier split at underscores and case changes.
fn tokens(name: &str) -> Vec<String> {
    let name = name.replace('_', " ");
    let name = re!(r"([a-z0-9])([A-Z])").replace_all(&name, "${1} ${2}");
    let name = re!(r"([A-Z]+)([A-Z][a-z])").replace_all(&name, "${1} ${2}");
    name.split_whitespace().map(|t| t.to_lowercase()).collect()
}

fn def_patterns(extension: &str) -> Option<Vec<&'static Regex>> {
    let ts = || {
        vec![
            re!(r"^\s*(?:export\s+)?(?:default\s+)?(?:async\s+)?function\*?\s+(\w+)"),
            re!(
                r"^\s*(?:export\s+)?(?:const|let|var)\s+(\w+)\s*(?::[^=]+)?=\s*(?:async\s*)?(?:\([^)]*\)|\w+)\s*(?::[^=]+)?=>"
            ),
            re!(
                r"^\s+(?:(?:public|private|protected|static|async|readonly)\s+)*(\w+)\s*\([^)]*\)\s*(?::\s*[^{;]+)?\{\s*$"
            ),
        ]
    };
    let sh = || vec![re!(r"^(?:function\s+)?(\w+)\s*\(\)\s*\{")];
    match extension {
        ".go" => Some(vec![re!(r"^func\s+(?:\([^)]*\)\s*)?(\w+)")]),
        ".ts" | ".tsx" | ".js" | ".jsx" | ".mjs" | ".cjs" => Some(ts()),
        ".py" => Some(vec![re!(r"^\s*(?:async\s+)?def\s+(\w+)")]),
        ".rs" => Some(vec![re!(
            r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?fn\s+(\w+)"
        )]),
        ".sh" | ".bash" | ".zsh" => Some(sh()),
        _ => None,
    }
}

/// `(line, name)` of each function-like definition in a file.
fn definitions(repo: &Repo, ctx: &Ctx, path: &str) -> Vec<(usize, String)> {
    let Some(pats) = def_patterns(&ext(path).to_lowercase()) else {
        return Vec::new();
    };
    let mut defs = Vec::new();
    for (i, line) in splitlines(&repo.read(ctx, path)).into_iter().enumerate() {
        if line.chars().count() > 300 {
            continue;
        }
        for p in &pats {
            if let Some(m) = p.captures(line) {
                if !KEYWORDS.contains(&&m[1]) {
                    defs.push((i + 1, m[1].to_string()));
                    break;
                }
            }
        }
    }
    defs
}

fn sibling_items(repo: &Repo, ctx: &Ctx, new: &str, lines: &[usize]) -> Vec<Item> {
    let defs = definitions(repo, ctx, new);
    if defs.is_empty() || lines.is_empty() {
        return Vec::new();
    }
    // The enclosing definition of each added line, in the order first seen.
    let mut changed: Vec<(String, usize)> = Vec::new();
    for &ln in lines {
        if let Some((dline, dname)) = defs.iter().rfind(|d| d.0 <= ln) {
            match changed.iter_mut().find(|c| &c.0 == dname) {
                Some(c) => c.1 = *dline,
                None => changed.push((dname.clone(), *dline)),
            }
        }
    }
    let mut items = Vec::new();
    for (name, _) in changed.iter().take(4) {
        let tk = tokens(name);
        if tk.len() < 2 {
            continue;
        }
        let mut sibs: Vec<(i64, usize, &str)> = Vec::new();
        for (line, other) in &defs {
            if other == name || changed.iter().any(|c| &c.0 == other) {
                continue;
            }
            let ot = tokens(other);
            let shared = tk.iter().zip(&ot).take_while(|(a, b)| a == b).count();
            if shared >= 2 || (shared == 1 && tk.len() <= 2) {
                sibs.push((-(shared as i64), *line, other));
            }
        }
        sibs.sort();
        for (_, line, other) in sibs.into_iter().take(3) {
            items.push(pointer(
                1,
                new,
                "sibling",
                format!("{new}:{line}"),
                format!(
                    "`{other}` is named like the changed `{name}`; compare guards and error handling"
                ),
            ));
        }
    }
    items
}

/// `target` resolved against `base_dir` when that names a tracked file.
fn resolve(repo: &Repo, base_dir: &str, target: &str) -> Option<String> {
    let cand = normpath(&join(&[base_dir, target]));
    repo.tree.contains(&cand).then_some(cand)
}

fn include_items(repo: &Repo, ctx: &Ctx, new: &str) -> Vec<Item> {
    let base = basename(new);
    let d = dirname(new);
    let extension = ext(new).to_lowercase();
    let mut items = Vec::new();
    if matches!(base, "Makefile" | "makefile" | "GNUmakefile")
        || extension == ".mk"
        || extension == ".mak"
    {
        for (i, line) in splitlines(&repo.read(ctx, new)).into_iter().enumerate() {
            let i = i + 1;
            let Some(m) = re!(r"^-?(?:s?include)\s+(.+)$").captures(line) else {
                continue;
            };
            let args = m[1].split('#').next().unwrap_or("");
            for t in args.split_whitespace() {
                if t.contains('$') {
                    items.push(pointer(
                        2,
                        new,
                        "include",
                        format!("{new}:{i}"),
                        format!("includes `{t}`, a computed path; resolve it"),
                    ));
                    continue;
                }
                let hit =
                    resolve(repo, &d, t).or_else(|| repo.tree.contains(t).then(|| t.to_string()));
                if let Some(hit) = hit {
                    items.push(pointer(
                        2,
                        new,
                        "include",
                        hit,
                        format!(
                            "`include {t}` at {new}:{i}; check variables and flags it sets (.SHELLFLAGS, .ONESHELL, SHELL)"
                        ),
                    ));
                }
            }
        }
    } else if TS_EXTS.contains(&extension.as_str()) {
        let text = repo.read(ctx, new);
        let mut seen: HashSet<String> = HashSet::new();
        let mut imports: Vec<(String, String)> = Vec::new();
        let pat = re!(r#"(?:from\s+|require\(\s*|import\(\s*|import\s+)['"](\.{1,2}/[^'"]+)['"]"#);
        for m in pat.captures_iter(&text) {
            let t = &m[1];
            for suffix in [
                "",
                ".ts",
                ".tsx",
                ".js",
                ".jsx",
                ".mjs",
                "/index.ts",
                "/index.tsx",
                "/index.js",
            ] {
                if let Some(hit) = resolve(repo, &d, &format!("{t}{suffix}")) {
                    if !seen.contains(&hit) && hit != new {
                        seen.insert(hit.clone());
                        let start = m.get(0).expect("whole match").start();
                        let ln = text.as_bytes()[..start]
                            .iter()
                            .filter(|&&b| b == b'\n')
                            .count()
                            + 1;
                        imports.push((hit, format!("imported at {new}:{ln}")));
                        break;
                    }
                }
            }
        }
        // An import that is itself edited in this diff is the likeliest cross-file contradiction.
        imports.sort_by_key(|(hit, _)| !repo.edited.contains(hit));
        for (hit, why) in imports.iter().take(MAX_IMPORTS) {
            let edited = repo.edited.contains(hit);
            items.push(pointer(
                if edited { 1 } else { 5 },
                new,
                "include",
                hit.clone(),
                format!(
                    "{why}{}",
                    if edited {
                        "; also edited in this diff"
                    } else {
                        ""
                    }
                ),
            ));
        }
        if imports.len() > MAX_IMPORTS {
            items.push(pointer(
                5,
                new,
                "include",
                "...".into(),
                format!("{} more relative imports", imports.len() - MAX_IMPORTS),
            ));
        }
    } else if base == "Dockerfile" || base.starts_with("Dockerfile.") || extension == ".dockerfile"
    {
        let text = repo.read(ctx, new);
        let lines = splitlines(&text);
        let mut stages: HashMap<String, usize> = HashMap::new();
        for (i, line) in lines.iter().enumerate() {
            if let Some(m) = re!(r"(?i)^\s*FROM\s+\S+(?:\s+--\S+)*\s+AS\s+(\S+)").captures(line) {
                stages.insert(m[1].to_lowercase(), i + 1);
            }
        }
        for (i, line) in lines.iter().enumerate() {
            let i = i + 1;
            if let Some(m) = re!(r"(?i)^\s*COPY\s+--from=(\S+)").captures(line) {
                let name = &m[1];
                match stages.get(&name.to_lowercase()) {
                    Some(at) => items.push(pointer(
                        2,
                        new,
                        "include",
                        format!("{new}:{at}"),
                        format!("stage `{name}` copied from at line {i}"),
                    )),
                    None => items.push(pointer(
                        2,
                        new,
                        "include",
                        format!("{new}:{i}"),
                        format!(
                            "`--from={name}` is a named build context or external image; find where it is supplied"
                        ),
                    )),
                }
            } else if re!(r"(?i)^\s*(?:COPY|ADD)\s").is_match(line) {
                let args: Vec<&str> = line
                    .split_whitespace()
                    .skip(1)
                    .filter(|a| !a.starts_with("--"))
                    .collect();
                for src in &args[..args.len().saturating_sub(1)] {
                    for cand in [normpath(&join(&[&d, src])), normpath(src)] {
                        if repo.tree.contains(&cand) {
                            items.push(pointer(
                                2,
                                new,
                                "include",
                                cand,
                                format!("copied at {new}:{i}"),
                            ));
                            break;
                        }
                    }
                }
            }
        }
        for extra in ["BUILD.bazel", "BUILD", ".dockerignore"] {
            let p = normpath(&join(&[&d, extra]));
            if repo.tree.contains(&p) {
                items.push(pointer(
                    2,
                    new,
                    "include",
                    p,
                    "build definition beside the Dockerfile; check it still matches".into(),
                ));
            }
        }
    } else if extension == ".go" {
        let stem_full: String = {
            let n = base.chars().count();
            base.chars().take(n.saturating_sub(3)).collect()
        };
        let stem = stem_full.strip_suffix("_test").unwrap_or(&stem_full);
        let mut sibs: Vec<&String> = repo
            .tree
            .iter()
            .filter(|p| dirname(p) == d && p.ends_with(".go") && p.as_str() != new)
            .collect();
        sibs.sort_by_key(|p| (!basename(p).starts_with(stem), p.to_string()));
        for p in sibs.iter().take(3) {
            items.push(pointer(
                4,
                new,
                "package",
                (*p).clone(),
                "same Go package".into(),
            ));
        }
        if sibs.len() > 3 {
            items.push(pointer(
                4,
                new,
                "package",
                format!("{d}/"),
                format!("{} more files in the package", sibs.len() - 3),
            ));
        }
    }
    items
}

/// A changed file's claim on a `git grep` pattern.
struct Owner {
    st: char,
    old: String,
    new: String,
}

/// Rows of `git grep -n` output as `(path, line number text, text)`, with the
/// revision prefix removed and anything that is not a hit row dropped.
fn grep_rows<'a>(out: &'a str, rev: &str) -> Vec<(&'a str, &'a str, &'a str)> {
    let prefix = format!("{rev}:");
    let mut rows = Vec::new();
    for row in splitlines(out) {
        let row = if !rev.is_empty() {
            row.strip_prefix(prefix.as_str()).unwrap_or(row)
        } else {
            row
        };
        let mut parts = row.splitn(3, ':');
        let (Some(path), Some(ln), Some(text)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        if ln.is_empty() || !ln.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        rows.push((path, ln, text));
    }
    rows
}

fn skip_pathspecs() -> Vec<String> {
    let mut v: Vec<String> = SKIP_DIRS
        .iter()
        .map(|d| format!(":(exclude,glob)**/{d}/**"))
        .collect();
    v.extend(SKIP_FILES.iter().map(|f| format!(":(exclude,glob)**/{f}")));
    v
}

/// One `git grep` for every changed path and distinctive basename.
fn referrer_items(repo: &Repo, ctx: &Ctx, changes: &[Change]) -> Vec<Item> {
    let mut order: Vec<String> = Vec::new();
    let mut pats: HashMap<String, Vec<Owner>> = HashMap::new();
    let mut add = |key: &str, st: char, old: &str, new: &str| {
        if !pats.contains_key(key) {
            order.push(key.to_string());
        }
        pats.entry(key.to_string()).or_default().push(Owner {
            st,
            old: old.to_string(),
            new: new.to_string(),
        });
    };
    for (st, old, new) in changes {
        let mut keys = vec![if matches!(st, 'D' | 'R') { old } else { new }];
        if *st == 'R' {
            keys.push(new);
        }
        for k in keys {
            add(k, *st, old, new);
        }
        if matches!(st, 'D' | 'R') {
            let b = basename(old);
            if !GENERIC_BASENAMES.contains(&b.to_lowercase().as_str()) && b.chars().count() >= 6 {
                add(b, *st, old, new);
            }
        }
    }
    if order.is_empty() {
        return Vec::new();
    }
    let mut args: Vec<String> = ["grep", "-n", "-I", "-F"].map(String::from).to_vec();
    for p in &order {
        args.push("-e".into());
        args.push(p.clone());
    }
    if !repo.rev.is_empty() {
        args.push(repo.rev.clone());
    }
    args.push("--".into());
    args.extend(skip_pathspecs());
    let out = ctx.git(&args);
    // First line number of each (pattern, referrer path) pair.
    let mut hits: HashMap<(&str, &str), usize> = HashMap::new();
    for (path, ln, text) in grep_rows(&out, &repo.rev) {
        let ln: usize = ln.parse().unwrap_or(0);
        for p in &order {
            if text.contains(p.as_str()) {
                hits.entry((p.as_str(), path)).or_insert(ln);
            }
        }
    }
    let edited_news: HashSet<&str> = changes.iter().map(|c| c.2.as_str()).collect();
    let mut items = Vec::new();
    for p in &order {
        for o in &pats[p] {
            let mut found: Vec<(&str, usize)> = hits
                .iter()
                .filter(|((q, path), _)| q == &p.as_str() && *path != o.old && *path != o.new)
                .map(|((_, path), ln)| (*path, *ln))
                .collect();
            found.sort();
            if found.is_empty() {
                continue;
            }
            for (path, ln) in found.iter().take(MAX_REFERRERS) {
                let (why, pri) = if o.st == 'D' {
                    (format!("deleted path still referenced (`{p}`)"), 0)
                } else if o.st == 'R' && *p != o.new {
                    (
                        format!("renamed from `{}`; still references `{p}`", o.old),
                        0,
                    )
                } else {
                    (format!("mentions `{p}`"), 3)
                };
                let why = if edited_news.contains(path) {
                    format!("{why}; also edited in this diff")
                } else {
                    why
                };
                items.push(pointer(
                    pri,
                    &o.new,
                    "referrer",
                    format!("{path}:{ln}"),
                    why,
                ));
            }
            if found.len() > MAX_REFERRERS {
                items.push(pointer(
                    3,
                    &o.new,
                    "referrer",
                    "...".into(),
                    format!("{} more files mention `{p}`", found.len() - MAX_REFERRERS),
                ));
            }
        }
    }
    items
}

/// Every match of `re` in `text` whose start passes `ok`, scanning left to
/// right without overlap: a candidate that fails `ok` is retried one
/// character later. Stands in for a lookbehind the regex engine lacks.
fn matches_where<'t>(
    re: &Regex,
    text: &'t str,
    ok: impl Fn(Option<char>) -> bool,
) -> Vec<Captures<'t>> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos <= text.len() {
        let Some(c) = re.captures_at(text, pos) else {
            break;
        };
        let m = c.get(0).expect("whole match");
        let prev = text[..m.start()].chars().next_back();
        if ok(prev) {
            pos = m.end().max(m.start() + 1);
            while pos < text.len() && !text.is_char_boundary(pos) {
                pos += 1;
            }
            out.push(c);
        } else {
            pos = m.start() + text[m.start()..].chars().next().map_or(1, char::len_utf8);
        }
    }
    out
}

/// Whether `prev` is absent or does not match the one-character class `bad`.
fn not_preceded_by(bad: &Regex, prev: Option<char>) -> bool {
    match prev {
        None => true,
        Some(c) => !bad.is_match(c.encode_utf8(&mut [0; 4])),
    }
}

/// Ordered distinct `(repo or None, number)` cited in `text`.
fn find_citations(text: &str) -> Vec<(Option<String>, u64)> {
    let mut found: Vec<(Option<String>, u64)> = Vec::new();
    for m in re!(r"https?://github\.com/([\w.-]+/[\w.-]+)/(?:issues|pull)/(\d{1,6})\b")
        .captures_iter(text)
    {
        if let Ok(n) = m[2].parse() {
            found.push((Some(m[1].to_string()), n));
        }
    }
    for m in re!(r"(?i)\b(?:PR|pull request|issue)s?\s+#?(\d{1,6})\b").captures_iter(text) {
        if let Ok(n) = m[1].parse() {
            found.push((None, n));
        }
    }
    let bad = re!(r"^[\w&/#.-]$");
    for m in matches_where(re!(r"#(\d{1,5})\b"), text, |p| not_preceded_by(bad, p)) {
        if let Ok(n) = m[1].parse() {
            found.push((None, n));
        }
    }
    let mut seen = HashSet::new();
    found
        .into_iter()
        .filter(|r| r.1 > 0 && seen.insert(r.clone()))
        .collect()
}

/// The PR body: the contract's `pr_body` when it is a string (null means
/// unknown), else `REVIEW_PR_BODY` (a file path, else the text itself), or a
/// `pr-body.md` in the workspace or beside the saved diff.
fn pr_body(ctx: &Ctx, contract: &Value) -> String {
    if let Some(s) = contract.get("pr_body").and_then(Value::as_str) {
        return s.to_string();
    }
    let env = ctx.cfg.pr_body.as_str();
    let mut cands: Vec<String> = Vec::new();
    if !env.is_empty() {
        cands.push(env.to_string());
    }
    cands.push(join(&[&ctx.ws, "pr-body.md"]));
    if let Some(diff) = contract
        .get("diff")
        .and_then(Value::as_str)
        .filter(|d| !d.is_empty())
    {
        cands.push(join(&[&dirname(diff), "pr-body.md"]));
    }
    for c in &cands {
        if std::path::Path::new(c).is_file() {
            if let Ok(b) = std::fs::read(c) {
                return pyish::universal_newlines(&String::from_utf8_lossy(&b));
            }
        }
    }
    if !env.is_empty() && !std::path::Path::new(env).exists() {
        env.to_string()
    } else {
        String::new()
    }
}

/// `gh` with a per-call timeout; goes quiet when `gh` is missing,
/// unauthenticated or too slow, and is dead from the start in a `--no-gh` run.
struct Gh<'a> {
    ctx: &'a Ctx<'a>,
    dead: Cell<bool>,
}

impl<'a> Gh<'a> {
    fn new(ctx: &'a Ctx<'a>) -> Gh<'a> {
        Gh {
            ctx,
            dead: Cell::new(ctx.cfg.no_gh || !ctx.run.on_path("gh")),
        }
    }

    fn run(&self, args: &[&str]) -> Option<String> {
        if self.dead.get() || self.ctx.spent() {
            return None;
        }
        let argv: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        let opts = Opts {
            cwd: Some(std::path::Path::new(&self.ctx.ws)),
            timeout: Some(GH_TIMEOUT.min(self.ctx.remaining())),
            ..Opts::default()
        };
        match self.ctx.run.run("gh", &argv, &opts) {
            Err(_) => {
                self.dead.set(true);
                None
            }
            Ok(c) if !c.ok() => {
                let err = c.stderr.to_lowercase();
                if err.contains("auth") || err.contains("login") || err.contains("gh_token") {
                    self.dead.set(true);
                }
                None
            }
            Ok(c) => Some(c.stdout),
        }
    }

    fn lookup(&self, repo: Option<&str>, n: u64) -> Option<Value> {
        let n = n.to_string();
        for kind in ["issue", "pr"] {
            let mut args = vec![kind, "view", n.as_str(), "--json", "number,title,state,url"];
            if let Some(r) = repo {
                args.extend(["--repo", r]);
            }
            if let Some(out) = self.run(&args).filter(|o| !o.is_empty()) {
                if let Ok(v @ Value::Object(_)) = serde_json::from_str(&out) {
                    return Some(v);
                }
            }
        }
        None
    }
}

fn citation_items(ctx: &Ctx, base: &str, head: &str, contract: &Value, text: &str) -> Vec<Item> {
    let mut refs: Vec<(Option<String>, u64)> = Vec::new();
    for (path, line) in added_content(text) {
        if !skipped(&path) {
            refs.extend(find_citations(&line));
        }
    }
    if !base.is_empty() && !head.is_empty() {
        refs.extend(find_citations(&ctx.git(&[
            "log",
            "--format=%B",
            &format!("{base}..{head}"),
        ])));
    }
    refs.extend(find_citations(&pr_body(ctx, contract)));
    let mut seen = HashSet::new();
    let distinct: Vec<_> = refs
        .into_iter()
        .filter(|r| seen.insert(r.clone()))
        .collect();
    if distinct.is_empty() {
        return Vec::new();
    }
    let gh = Gh::new(ctx);
    let mut own = match contract.get("pr_number") {
        Some(Value::Number(n)) => n.as_i64().filter(|&n| n > 0).map(|n| n.to_string()),
        _ => None,
    }
    .unwrap_or_default();
    if own.is_empty() {
        own = ctx.cfg.pr_number.trim().trim_start_matches('#').to_string();
    }
    if own.is_empty() || !own.bytes().all(|b| b.is_ascii_digit()) {
        own = gh
            .run(&["pr", "view", "--json", "number"])
            .filter(|o| !o.is_empty())
            .and_then(|o| serde_json::from_str::<Value>(&o).ok())
            .and_then(|v| v.get("number").map(py_str))
            .unwrap_or_default();
    }
    let mut items = Vec::new();
    for (repo, n) in &distinct {
        if items.len() >= MAX_CITES || ctx.start.elapsed() > CITE_BUDGET {
            break;
        }
        if repo.is_none() && n.to_string() == own {
            continue;
        }
        let info = gh.lookup(repo.as_deref(), *n);
        if gh.dead.get() && info.is_none() {
            break;
        }
        let Some(info) = info else { continue };
        let name = match repo {
            Some(r) => format!("{r}#{n}"),
            None => format!("#{n}"),
        };
        let title: String = info
            .get("title")
            .map(py_str)
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(90)
            .collect();
        let state = info
            .get("state")
            .map(py_str)
            .unwrap_or_else(|| "?".into())
            .to_lowercase();
        items.push(free_form(
            -2,
            format!("cite:{name}"),
            format!(
                "cites {name}: \"{title}\" ({state}) — check the citation says what the comment claims"
            ),
        ));
    }
    items
}

/// Qualified call identifiers in added lines with their counts, in the order
/// first seen. The flag is true for Go.
fn qualified_calls(text: &str) -> Vec<((String, bool), usize)> {
    let mut counts: Vec<((String, bool), usize)> = Vec::new();
    let go_bad = re!(r"^[\w.]$");
    let ts_bad = re!(r"^[\w.$]$");
    for (path, line) in added_content(text) {
        let extension = ext(&path).to_lowercase();
        let trimmed = line.trim_start();
        if skipped(&path)
            || ["//", "#", "*", "/*"]
                .iter()
                .any(|p| trimmed.starts_with(p))
        {
            continue;
        }
        let is_go = extension == ".go";
        let found: Vec<String> = if is_go {
            matches_where(re!(r"([A-Za-z_]\w*\.[A-Z]\w*)\("), &line, |p| {
                not_preceded_by(go_bad, p)
            })
            .into_iter()
            .map(|c| c[1].to_string())
            .collect()
        } else if TS_EXTS.contains(&extension.as_str()) {
            matches_where(re!(r"([a-z]\w*\.[a-z]\w*)\("), &line, |p| {
                not_preceded_by(ts_bad, p)
            })
            .into_iter()
            .map(|c| c[1].to_string())
            .collect()
        } else {
            continue;
        };
        for ident in found {
            let (recv, meth) = ident.split_once('.').expect("one dot");
            if NOISE_RECEIVERS.contains(&recv) || NOISE_METHODS.contains(&meth) {
                continue;
            }
            let key = (ident, is_go);
            match counts.iter_mut().find(|c| c.0 == key) {
                Some(c) => c.1 += 1,
                None => counts.push((key, 1)),
            }
        }
    }
    counts
}

fn precedent_items(repo: &Repo, ctx: &Ctx, changes: &[Change], text: &str) -> Vec<Item> {
    let mut counts = qualified_calls(text);
    if counts.is_empty() {
        return Vec::new();
    }
    let mut excludes: Vec<String> = changes
        .iter()
        .map(|(_, _, new)| format!(":(exclude,literal){new}"))
        .collect();
    excludes.extend(skip_pathspecs());
    // Most frequent first; equal counts keep the order they were first seen.
    counts.sort_by_key(|c| std::cmp::Reverse(c.1));
    let mut items = Vec::new();
    for ((ident, is_go), _) in counts.into_iter().take(MAX_PRECEDENT_IDENTS) {
        let globs: &[&str] = if is_go {
            &["*.go"]
        } else {
            &["*.ts", "*.tsx", "*.js", "*.jsx", "*.mjs"]
        };
        let mut args: Vec<String> = vec![
            "grep".into(),
            "-n".into(),
            "-I".into(),
            "-F".into(),
            "-e".into(),
            format!("{ident}("),
        ];
        if !repo.rev.is_empty() {
            args.push(repo.rev.clone());
        }
        args.push("--".into());
        args.extend(globs.iter().map(|g| g.to_string()));
        args.extend(excludes.iter().cloned());
        let out = ctx.git(&args);
        let mut files: HashSet<&str> = HashSet::new();
        let mut sites: Vec<String> = Vec::new();
        for (path, ln, _) in grep_rows(&out, &repo.rev) {
            if files.insert(path) {
                sites.push(format!("{path}:{ln}"));
            }
        }
        let testy = re!(r"_test\.go|\.test\.|\.spec\.|/tests?/");
        sites.sort_by_key(|s| testy.is_match(s.split(':').next().unwrap_or("")));
        if !sites.is_empty() {
            let shown = sites
                .iter()
                .take(MAX_SITES)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            items.push(free_form(
                0,
                format!("precedent:{ident}"),
                format!(
                    "precedent for {ident}: {shown} — compare argument order and shape before calling the diff's use a preference"
                ),
            ));
        }
    }
    items
}

/// A dependency a changed file imports, noted for the upstream stage.
pub(crate) struct Imported {
    eco: &'static str,
    name: String,
    version: String,
    go_mod_dir: String,
}

/// `(module path, go.mod dir, version)` for the longest go.mod requirement
/// that prefixes `imp`; None for the module's own packages and unknown imports.
fn go_module_for(
    repo: &Repo,
    ctx: &Ctx,
    start_dir: &str,
    imp: &str,
) -> Option<(String, String, String)> {
    let mut d = start_dir.to_string();
    let gomod = loop {
        let gm = if d.is_empty() {
            "go.mod".to_string()
        } else {
            join(&[&d, "go.mod"])
        };
        if repo.tree.contains(&gm) {
            break gm;
        }
        if d.is_empty() {
            return None;
        }
        d = dirname(&d);
    };
    let text = repo.read(ctx, &gomod);
    if let Some(own) = re!(r"(?m)^module\s+(\S+)").captures(&text) {
        let own = own[1].trim_matches('"');
        if imp == own || imp.starts_with(&format!("{own}/")) {
            return None;
        }
    }
    let mut best: Option<(String, String)> = None;
    for m in
        re!(r"(?m)^\s*(?:require\s+)?([\w.~/-]+\.[\w.~/-]+)\s+(v[\w.+-]+)").captures_iter(&text)
    {
        let (module, ver) = (&m[1], &m[2]);
        let prefixes = imp == module || imp.starts_with(&format!("{module}/"));
        if prefixes && best.as_ref().is_none_or(|b| module.len() > b.0.len()) {
            best = Some((module.to_string(), ver.to_string()));
        }
    }
    best.map(|(module, ver)| (module, d, ver))
}

/// The on-disk directory of a Go module: what `go list` reports offline, else
/// the module cache's copy at that version.
pub(crate) fn module_dir(ctx: &Ctx, go_mod_dir: &str, module: &str, ver: &str) -> Option<String> {
    let cwd = if go_mod_dir.is_empty() {
        ctx.ws.clone()
    } else {
        join(&[&ctx.ws, go_mod_dir])
    };
    let opts = Opts {
        cwd: Some(std::path::Path::new(&cwd)),
        env: vec![("GOFLAGS", "-mod=mod".into()), ("GOPROXY", "off".into())],
        timeout: Some(GO_LIST_TIMEOUT.min(ctx.remaining())),
        ..Opts::default()
    };
    let args: Vec<String> = ["list", "-m", "-f", "{{.Dir}}", module]
        .map(String::from)
        .to_vec();
    if let Ok(c) = ctx.run.run("go", &args, &opts) {
        let out = c.stdout.trim();
        if c.ok() && !out.is_empty() && std::path::Path::new(out).is_dir() {
            return Some(out.to_string());
        }
    }
    let root = ctx.cfg.go_mod_cache.clone().or_else(|| {
        ctx.cfg
            .home
            .as_ref()
            .map(|h| join(&[h, "go", "pkg", "mod"]))
    })?;
    let esc = re!(r"[A-Z]").replace_all(module, |c: &Captures| format!("!{}", c[0].to_lowercase()));
    let cand = join(&[&root, &format!("{esc}@{ver}")]);
    std::path::Path::new(&cand).is_dir().then_some(cand)
}

/// The first concrete version in an npm or Cargo range, or None for tags,
/// URLs and workspace references.
pub(crate) fn spec_version(spec: &str) -> Option<String> {
    let m = re!(r"\d+(?:\.\d+){0,2}(?:-[0-9A-Za-z.-]+)?").find(spec)?;
    let reference = ["git", "http", "file", "link", "workspace", "npm:"];
    (!reference.iter().any(|p| spec.starts_with(p))).then(|| m.as_str().to_string())
}

/// Dependencies named in `package.json` text: `{name: spec}`.
pub(crate) fn declared_npm(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Ok(Value::Object(j)) = serde_json::from_str::<Value>(text) else {
        return out;
    };
    for k in ["dependencies", "devDependencies", "optionalDependencies"] {
        if let Some(Value::Object(deps)) = j.get(k) {
            for (name, spec) in deps {
                out.insert(name.clone(), spec.as_str().unwrap_or("").to_string());
            }
        }
    }
    out
}

/// The version a package.json at or above `start_dir` declares for `pkg`.
fn ts_declared(repo: &Repo, ctx: &Ctx, start_dir: &str, pkg: &str) -> Option<String> {
    let mut d = start_dir.to_string();
    loop {
        let pj = join(&[&d, "package.json"]);
        if repo.tree.contains(&pj) {
            let declared = declared_npm(&repo.read(ctx, &pj));
            if let Some(v) = declared.get(pkg).and_then(|s| spec_version(s)) {
                return Some(v);
            }
        }
        if d.is_empty() {
            return None;
        }
        d = dirname(&d);
    }
}

fn dependency_items(repo: &Repo, ctx: &Ctx, text: &str, imported: &mut Vec<Imported>) -> Vec<Item> {
    let mut items = Vec::new();
    let mut go_seen: HashSet<String> = HashSet::new();
    let mut ts_seen: HashSet<String> = HashSet::new();
    for (path, line) in added_content(text) {
        if skipped(&path) {
            continue;
        }
        let extension = ext(&path).to_lowercase();
        if extension == ".go" && go_seen.len() < MAX_GO_MODULES {
            let pat = re!(r#"^\s*(?:import\s+)?(?:[\w._]+\s+)?"([^"\s]+)"\s*(?://.*)?$"#);
            let Some(m) = pat.captures(&line) else {
                continue;
            };
            let imp = m[1].to_string();
            if !imp.split('/').next().unwrap_or("").contains('.') || go_seen.contains(&imp) {
                continue;
            }
            let Some((module, go_mod_dir, ver)) = go_module_for(repo, ctx, &dirname(&path), &imp)
            else {
                continue;
            };
            go_seen.insert(imp.clone());
            imported.push(Imported {
                eco: "go",
                name: module.clone(),
                version: ver.clone(),
                go_mod_dir: go_mod_dir.clone(),
            });
            let Some(mdir) = module_dir(ctx, &go_mod_dir, &module, &ver) else {
                continue;
            };
            let sub = imp[module.len()..].trim_start_matches('/');
            let pdir = if !sub.is_empty() && std::path::Path::new(&join(&[&mdir, sub])).is_dir() {
                join(&[&mdir, sub])
            } else {
                mdir
            };
            items.push(free_form(
                -1,
                format!("dep:{imp}"),
                format!("dependency {imp} → {pdir} — read it before judging a call's semantics"),
            ));
        } else if TS_EXTS.contains(&extension.as_str()) && ts_seen.len() < MAX_TS_PACKAGES {
            let pat = re!(
                r#"(?:\bfrom\s+|\brequire\(\s*|\bimport\(\s*|^\s*import\s+)['"]([^'".][^'"]*)['"]"#
            );
            let Some(m) = pat.captures(&line) else {
                continue;
            };
            if m[1].starts_with("node:") {
                continue;
            }
            let parts: Vec<&str> = m[1].split('/').collect();
            let pkg = if parts[0].starts_with('@') {
                parts[..parts.len().min(2)].join("/")
            } else {
                parts[0].to_string()
            };
            if NODE_BUILTINS.contains(&pkg.as_str()) || ts_seen.contains(&pkg) {
                continue;
            }
            let mut d = dirname(&path);
            if let Some(declared) = ts_declared(repo, ctx, &d, &pkg) {
                imported.push(Imported {
                    eco: "npm",
                    name: pkg.clone(),
                    version: declared,
                    go_mod_dir: String::new(),
                });
            }
            loop {
                let cand = join(&[&ctx.ws, &d, "node_modules", &pkg]);
                if std::path::Path::new(&cand).is_dir() {
                    ts_seen.insert(pkg.clone());
                    let rel = normpath(&join(&[&d, "node_modules", &pkg]));
                    items.push(free_form(
                        -1,
                        format!("dep:{pkg}"),
                        format!(
                            "dependency {pkg} → {rel} — read it before judging a call's semantics"
                        ),
                    ));
                    break;
                }
                if d.is_empty() {
                    break;
                }
                d = dirname(&d);
            }
        }
    }
    items
}

#[derive(Serialize)]
struct Line {
    tool: &'static str,
    message: String,
}

#[derive(Serialize)]
struct Output {
    diagnostics: Vec<Diagnostic>,
    context: Vec<Line>,
    triggers: Vec<String>,
}

/// The node's JSON for the contract on `stdin`. Never fails: a contract it
/// cannot read yields no context and a line on stderr.
pub fn run(stdin: &str, cfg: &Config, runner: &dyn Runner) -> String {
    match pack(stdin, cfg, runner) {
        Ok(out) => out,
        Err(e) => {
            eprintln!("context-pack: {e}");
            r#"{"diagnostics": [], "triggers": []}"#.to_string()
        }
    }
}

fn pack(stdin: &str, cfg: &Config, runner: &dyn Runner) -> Result<String, String> {
    let contract: Value = serde_json::from_str(stdin).map_err(|e| e.to_string())?;
    let ws = contract
        .get("workspace")
        .and_then(Value::as_str)
        .ok_or_else(|| "'workspace'".to_string())?
        .trim_end_matches('/');
    let field = |k: &str| {
        contract
            .get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let (base, head) = (field("base"), field("head"));
    let ctx = Ctx::new(runner, cfg, ws);
    let mut repo = Repo::new(&ctx, &head);
    let changes: Vec<Change> = changed_set(&ctx, &base, &head, &contract)?
        .into_iter()
        .filter(|c| !skipped(&c.2) || c.0 == 'D')
        .take(MAX_FILES)
        .collect();
    repo.edited = changes.iter().map(|c| c.2.clone()).collect();
    let mut items = referrer_items(&repo, &ctx, &changes);
    // The saved diff is read once for every stage that needs added lines.
    let text = diff_text(&ctx, &base, &head, &contract);
    let added = added_lines(&text);
    for (st, _, new) in &changes {
        if *st == 'D' {
            continue;
        }
        items.extend(include_items(&repo, &ctx, new));
        let lines = added.get(new).map(Vec::as_slice).unwrap_or(&[]);
        items.extend(sibling_items(&repo, &ctx, new, lines));
    }
    // Citations, then dependency source, then precedent, each ahead of the
    // structural pointers above them; every stage is time-boxed and silent
    // on failure.
    items.extend(citation_items(&ctx, &base, &head, &contract, &text));
    let mut imported = Vec::new();
    items.extend(dependency_items(&repo, &ctx, &text, &mut imported));
    items.extend(precedent_items(&repo, &ctx, &changes, &text));
    let (up_items, diagnostics) = upstream::stage(&ctx, &base, &head, &contract, &text, &imported);
    items.extend(up_items);
    items.sort_by_key(|i| i.pri);
    let mut seen: HashSet<(Option<String>, Option<&str>, String)> = HashSet::new();
    let mut lines: Vec<Line> = Vec::new();
    for it in items {
        let msg = match it.kind {
            None => it.why.clone(),
            Some(kind) => format!(
                "{} → {kind}: {} — {}",
                it.changed.as_deref().unwrap_or("None"),
                it.ptr,
                it.why
            ),
        };
        let msg: String = msg.chars().take(280).collect();
        let key = (it.changed.clone(), it.kind, it.ptr.clone());
        if seen.contains(&key) && it.ptr != "..." {
            continue;
        }
        seen.insert(key);
        lines.push(Line {
            tool: "context-pack",
            message: msg,
        });
    }
    if lines.len() > cfg.max_items {
        let keep = cfg.max_items.saturating_sub(1);
        let extra = lines.len() - keep;
        lines.truncate(keep);
        lines.push(Line {
            tool: "context-pack",
            message: format!("{extra} lower-priority pointers omitted"),
        });
    }
    // The context channel renders ahead of the diff as "open these before
    // judging"; the diagnostics channel would file the same lines under
    // "already reported" and lose them.
    Ok(pyish::dumps(&Output {
        diagnostics,
        context: lines,
        triggers: Vec::new(),
    }))
}

#[cfg(test)]
mod tests;
