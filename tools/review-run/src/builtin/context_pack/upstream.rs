//! The upstream stage: what the registries know about the dependencies the
//! diff touches.
//!
//! For each dependency bump or addition in `go.mod`, `package.json` (and its
//! lockfiles) and `Cargo.toml`, and for each module newly imported, it fetches
//! the release notes between the old and new version with `go`, `gh` and
//! `curl` into `<out>/upstream/<pkg>/`, and points at them with an
//! `upstream <pkg> <old>→<new> (latest <v>): <path>` line. A pin that is not
//! the latest also raises a diagnostic. The whole stage gets 12 seconds and
//! each call 3; any failure becomes `upstream <pkg>: unavailable (<why>)`.

// `re!` compiles each pattern once per call site, which the lint cannot see.
#![allow(clippy::regex_creation_in_loops)]

use super::pyish::{self, basename, dirname, fmt_g, join, splitlines};
use super::{declared_npm, free_form, module_dir, read_text, spec_version, Ctx, Imported, Item};
use crate::builtin::Opts;
use crate::schema::Diagnostic;
use regex::Regex;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const BUDGET: Duration = Duration::from_secs(12);
const FETCH_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_UPSTREAM: usize = 6;
const MAX_NOTES_BYTES: usize = 40 * 1024;
const UA: &str = "kmosher-review-upstream (https://github.com/kmosher/claude-plugins)";

macro_rules! re {
    ($pattern:expr) => {{
        static RE: OnceLock<Regex> = OnceLock::new();
        RE.get_or_init(|| Regex::new($pattern).expect("static regex"))
    }};
}

/// A version or tag as a sort key: a release sorts above its own prereleases.
type VKey = (u64, u64, u64, u8, String);

fn number(s: &str) -> u64 {
    s.parse().unwrap_or(u64::MAX)
}

/// The sort key of a version or tag; None when it holds no version.
fn vkey(s: &str) -> Option<VKey> {
    let c = re!(r"v?(\d+)(?:\.(\d+))?(?:\.(\d+))?(?:[-+]([0-9A-Za-z.+-]+))?").captures(s)?;
    let part = |i: usize| c.get(i).map_or(0, |m| number(m.as_str()));
    let pre = c.get(4).map(|m| m.as_str().to_string());
    Some((
        number(&c[1]),
        part(2),
        part(3),
        if pre.is_some() { 0 } else { 1 },
        pre.unwrap_or_default(),
    ))
}

fn is_pseudo(v: &str) -> bool {
    re!(r"-\d{14}-[0-9a-f]{12}$").is_match(v)
}

/// `owner/repo` of a GitHub URL or module path.
fn github_slug(s: &str) -> Option<String> {
    let m = re!(r"github\.com[/:]([\w.-]+)/([\w.-]+?)(?:\.git)?(?:[/#?].*)?$").captures(s)?;
    Some(format!("{}/{}", &m[1], &m[2]))
}

fn safe_name(name: &str) -> String {
    re!(r"[^\w.@+-]").replace_all(name, "_").into_owned()
}

/// One `+` or `-` line of a diff with its new-side line number.
#[derive(Debug, Clone, PartialEq)]
struct Row {
    sign: char,
    line: Option<usize>,
    text: String,
}

/// The `+`/`-` lines of a unified diff by file, files in the order they first appear.
fn diff_by_file(text: &str) -> Vec<(String, Vec<Row>)> {
    let mut out: Vec<(String, Vec<Row>)> = Vec::new();
    let mut path: Option<String> = None;
    let mut n = 0usize;
    let mut push = |p: &str, row: Row| match out.iter_mut().find(|e| e.0 == p) {
        Some(e) => e.1.push(row),
        None => out.push((p.to_string(), vec![row])),
    };
    for line in splitlines(text) {
        if line.starts_with("+++ ") {
            path = super::header_path(line);
        } else if line.starts_with("--- ") {
            continue;
        } else if line.starts_with("@@") {
            n = super::hunk_start(line);
        } else if let (Some(p), Some(rest)) = (&path, line.strip_prefix('+')) {
            push(
                p,
                Row {
                    sign: '+',
                    line: Some(n),
                    text: rest.to_string(),
                },
            );
            n += 1;
        } else if let (Some(p), Some(rest)) = (&path, line.strip_prefix('-')) {
            push(
                p,
                Row {
                    sign: '-',
                    line: None,
                    text: rest.to_string(),
                },
            );
        } else if path.is_some() && line.starts_with(' ') {
            n += 1;
        }
    }
    out
}

/// A dependency the diff adds or moves.
#[derive(Debug, Clone)]
struct Dep {
    eco: &'static str,
    name: String,
    old: Option<String>,
    new: String,
    file: String,
    line: Option<usize>,
    /// 0 for a bump, 1 for an addition, 2 for an import, 3 for an indirect requirement.
    rank: u8,
    go_mod_dir: String,
}

fn lock_versions(path: &str, text: &str, names: &HashSet<String>) -> HashMap<String, String> {
    let mut out = HashMap::new();
    match basename(path) {
        "package-lock.json" => {
            let Ok(v) = serde_json::from_str::<Value>(text) else {
                return out;
            };
            let Some(pk) = v.get("packages").and_then(Value::as_object) else {
                return out;
            };
            for n in names {
                let ver = pk
                    .get(&format!("node_modules/{n}"))
                    .and_then(|p| p.get("version"))
                    .and_then(Value::as_str)
                    .filter(|v| !v.is_empty());
                if let Some(v) = ver {
                    out.insert(n.clone(), v.to_string());
                }
            }
        }
        "yarn.lock" => {
            let pat = re!(r#"(?m)^"?((?:@[^@\s"]+/)?[^@\s"]+)@[^\n]*:\n\s+version "?([^"\n]+)"?"#);
            for m in pat.captures_iter(text) {
                if names.contains(&m[1]) {
                    out.entry(m[1].to_string())
                        .or_insert_with(|| m[2].to_string());
                }
            }
        }
        "pnpm-lock.yaml" => {
            let pat = re!(r"(?m)^\s+/?'?((?:@[^@\s']+/)?[^@\s']+)@(\d[^\s:'()]*)[^\n]*:\s*$");
            for m in pat.captures_iter(text) {
                if names.contains(&m[1]) {
                    out.entry(m[1].to_string())
                        .or_insert_with(|| m[2].to_string());
                }
            }
        }
        _ => {}
    }
    out
}

/// Names declared in the dependency tables of a `Cargo.toml`.
fn declared_cargo(text: &str) -> HashSet<String> {
    let mut names = HashSet::new();
    let mut sect = String::new();
    for line in splitlines(text) {
        let s = line.trim();
        if s.starts_with('[') {
            sect = s.to_string();
            if let Some(m) =
                re!(r"^\[(?:[\w.-]+\.)?(?:dev-|build-)?dependencies\.([A-Za-z0-9_-]+)\]")
                    .captures(s)
            {
                names.insert(m[1].to_string());
            }
        } else if sect.ends_with("dependencies]") {
            if let Some(m) =
                re!(r#"^\s*([A-Za-z0-9_-]+)\s*=\s*(?:"([^"]+)"|\{[^}]*\bversion\s*=\s*"([^"]+)")"#)
                    .captures(line)
            {
                names.insert(m[1].to_string());
            }
        }
    }
    names
}

fn cargo_dep(text: &str) -> Option<(String, Option<String>)> {
    let m = re!(r#"^\s*([A-Za-z0-9_-]+)\s*=\s*(?:"([^"]+)"|\{[^}]*\bversion\s*=\s*"([^"]+)")"#)
        .captures(text)?;
    let spec = m.get(2).or_else(|| m.get(3)).map(|s| s.as_str());
    Some((m[1].to_string(), spec.and_then(spec_version)))
}

/// Dependency changes in a diff, one per package, the strongest signal first.
fn dependency_changes(
    by_file: &[(String, Vec<Row>)],
    show_head: &dyn Fn(&str) -> String,
    show_base: &dyn Fn(&str) -> String,
) -> Vec<Dep> {
    let mut found: Vec<Dep> = Vec::new();
    for (path, rows) in by_file {
        let base = basename(path);
        let d = dirname(path);
        match base {
            "go.mod" => {
                let pat = re!(
                    r"^\s*(?:require\s+)?([\w.~/-]+\.[\w.~/-]+)\s+(v[\w.+-]+)(\s*//\s*indirect)?"
                );
                let mut old: HashMap<String, String> = HashMap::new();
                for r in rows.iter().filter(|r| r.sign == '-') {
                    if let Some(m) = pat.captures(&r.text) {
                        old.insert(m[1].to_string(), m[2].to_string());
                    }
                }
                for r in rows.iter().filter(|r| r.sign == '+') {
                    let Some(m) = pat.captures(&r.text) else {
                        continue;
                    };
                    if old.get(&m[1]).map(String::as_str) == Some(&m[2]) {
                        continue;
                    }
                    let o = old.get(&m[1]).cloned();
                    let rank = if m.get(3).is_some() {
                        3
                    } else if o.is_some() {
                        0
                    } else {
                        1
                    };
                    found.push(Dep {
                        eco: "go",
                        name: m[1].to_string(),
                        old: o,
                        new: m[2].to_string(),
                        file: path.clone(),
                        line: r.line,
                        rank,
                        go_mod_dir: d.clone(),
                    });
                }
            }
            "package.json" => {
                let declared = declared_npm(&show_head(path));
                let pat = re!(r#"^\s*"([^"]+)"\s*:\s*"([^"]*)""#);
                let mut old: HashMap<String, Option<String>> = HashMap::new();
                for r in rows.iter().filter(|r| r.sign == '-') {
                    if let Some(m) = pat.captures(&r.text) {
                        old.insert(m[1].to_string(), spec_version(&m[2]));
                    }
                }
                for r in rows.iter().filter(|r| r.sign == '+') {
                    let Some(m) = pat.captures(&r.text) else {
                        continue;
                    };
                    if !declared.contains_key(&m[1]) {
                        continue;
                    }
                    let Some(new) = spec_version(&m[2]) else {
                        continue;
                    };
                    let o = old.get(&m[1]).cloned().flatten();
                    if o.as_deref() == Some(new.as_str()) {
                        continue;
                    }
                    found.push(Dep {
                        eco: "npm",
                        name: m[1].to_string(),
                        rank: if o.is_some() { 0 } else { 1 },
                        old: o,
                        new,
                        file: path.clone(),
                        line: r.line,
                        go_mod_dir: String::new(),
                    });
                }
            }
            "Cargo.toml" => {
                let declared = declared_cargo(&show_head(path));
                let mut old: HashMap<String, Option<String>> = HashMap::new();
                for r in rows.iter().filter(|r| r.sign == '-') {
                    if let Some((name, v)) = cargo_dep(&r.text) {
                        old.insert(name, v);
                    }
                }
                for r in rows.iter().filter(|r| r.sign == '+') {
                    let Some((name, new)) = cargo_dep(&r.text) else {
                        continue;
                    };
                    if !declared.contains(&name) {
                        continue;
                    }
                    let Some(new) = new else { continue };
                    let o = old.get(&name).cloned().flatten();
                    if o.as_deref() == Some(new.as_str()) {
                        continue;
                    }
                    found.push(Dep {
                        eco: "cargo",
                        rank: if o.is_some() { 0 } else { 1 },
                        name,
                        old: o,
                        new,
                        file: path.clone(),
                        line: r.line,
                        go_mod_dir: String::new(),
                    });
                }
            }
            "package-lock.json" | "yarn.lock" | "pnpm-lock.yaml" => {
                let pj = join(&[&d, "package.json"]);
                let names: HashSet<String> = declared_npm(&show_head(&pj)).into_keys().collect();
                if names.is_empty() {
                    continue;
                }
                let before = lock_versions(path, &show_base(path), &names);
                let after = lock_versions(path, &show_head(path), &names);
                let mut after: Vec<_> = after.into_iter().collect();
                after.sort();
                for (n, v) in after {
                    if let Some(b) = before.get(&n).filter(|b| !b.is_empty() && **b != v) {
                        found.push(Dep {
                            eco: "npm",
                            name: n,
                            old: Some(b.clone()),
                            new: v,
                            file: path.clone(),
                            line: None,
                            rank: 0,
                            go_mod_dir: String::new(),
                        });
                    }
                }
            }
            _ => {}
        }
    }
    // Stable: a package.json row beats its lockfile row.
    found.sort_by_key(|f| f.rank);
    let mut seen = HashSet::new();
    found
        .into_iter()
        .filter(|f| seen.insert((f.eco, f.name.clone())))
        .collect()
}

/// Sections of a markdown changelog whose heading names a version in
/// `(lo, hi]`; the whole text when none parse.
fn trim_changelog(text: &str, lo: &Option<VKey>, hi: &Option<VKey>) -> String {
    let mut starts: Vec<usize> = vec![0];
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        let hashes = line.chars().take_while(|&c| c == '#').count();
        if at > 0 && (1..=3).contains(&hashes) && line[hashes..].starts_with(' ') {
            starts.push(at);
        }
        at += line.len();
    }
    starts.push(text.len());
    let mut keep = String::new();
    let mut kept_any = false;
    for w in starts.windows(2) {
        let part = &text[w[0]..w[1]];
        let head = if part.trim().is_empty() {
            ""
        } else {
            splitlines(part).first().copied().unwrap_or("")
        };
        let key = if head.starts_with('#') {
            vkey(head)
        } else {
            None
        };
        if let Some(k) = key {
            if lo.as_ref().is_none_or(|l| &k > l) && hi.as_ref().is_none_or(|h| &k <= h) {
                keep.push_str(part);
                kept_any = true;
            }
        }
    }
    if kept_any {
        keep
    } else {
        text.to_string()
    }
}

fn head_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// An OS error as its message alone, without the `(os error N)` suffix.
fn strerror(e: &std::io::Error) -> String {
    let s = e.to_string();
    match s.rfind(" (os error ") {
        Some(i) => s[..i].to_string(),
        None => s,
    }
}

/// A failure of the registry, or of reading its answer.
enum Reason {
    /// A tool failed, timed out or ran out of budget.
    Fetch(String),
    /// An answer that is not what the registry documents.
    Value(String),
    /// A field the answer should hold is missing.
    Key(&'static str),
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Reason::Fetch(m) | Reason::Value(m) => f.write_str(m),
            Reason::Key(k) => write!(f, "'{k}'"),
        }
    }
}

struct Up<'a, 'c> {
    ctx: &'a Ctx<'c>,
    end: Instant,
}

impl Up<'_, '_> {
    /// Stdout of a `gh`, `curl` or `go` call, bounded by 3 s and the stage budget.
    fn fetch_cmd(
        &self,
        argv: &[String],
        cwd: Option<&str>,
        env: Vec<(&str, String)>,
    ) -> Result<String, String> {
        let left = self.end.saturating_duration_since(Instant::now());
        if left <= Duration::from_millis(100) {
            return Err("stage budget spent".into());
        }
        let t = FETCH_TIMEOUT.min(left);
        let opts = Opts {
            cwd: cwd.map(Path::new),
            env,
            timeout: Some(t),
            ..Opts::default()
        };
        match self.ctx.run.run(&argv[0], &argv[1..], &opts) {
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => Err(format!(
                "{} timed out after {}s",
                argv[0],
                fmt_g(t.as_secs_f64())
            )),
            Err(e) => Err(format!("{}: {}", argv[0], strerror(&e))),
            Ok(c) if !c.ok() => {
                let err = c.stderr.trim().to_string();
                let last = splitlines(&err)
                    .last()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| format!("exit {}", c.code));
                Err(format!("{}: {}", argv[0], head_chars(&last, 80)))
            }
            Ok(c) => Ok(pyish::universal_newlines(&c.stdout)),
        }
    }

    fn curl(&self, url: &str) -> Result<String, String> {
        let argv: Vec<String> = [
            "curl",
            "-sSfL",
            "--max-time",
            &fmt_g(FETCH_TIMEOUT.as_secs_f64()),
            "-A",
            UA,
            url,
        ]
        .map(String::from)
        .to_vec();
        self.fetch_cmd(&argv, None, Vec::new())
    }

    /// `(latest, repository slug, version list)` from the ecosystem's registry.
    #[allow(clippy::type_complexity)]
    fn registry_latest(
        &self,
        dep: &Dep,
        cwd: &str,
    ) -> Result<(String, Option<String>, Option<Vec<String>>), Reason> {
        let fetch = Reason::Fetch;
        let name = &dep.name;
        match dep.eco {
            "go" => {
                let env = vec![
                    ("GOFLAGS", "-mod=mod".to_string()),
                    ("GOTOOLCHAIN", "local".to_string()),
                    ("GOSUMDB", "off".to_string()),
                ];
                let argv: Vec<String> = ["go", "list", "-m", "-versions", name]
                    .map(String::from)
                    .to_vec();
                let out = self.fetch_cmd(&argv, Some(cwd), env).map_err(fetch)?;
                let vs: Vec<String> = out.split_whitespace().skip(1).map(String::from).collect();
                if vs.is_empty() {
                    return Err(Reason::Fetch("go list: no versions".into()));
                }
                let latest = vs
                    .iter()
                    .rev()
                    .find(|v| !v.contains('-'))
                    .unwrap_or_else(|| vs.last().expect("non-empty"))
                    .clone();
                Ok((latest, github_slug(name), Some(vs)))
            }
            "npm" => {
                let url = format!(
                    "https://registry.npmjs.org/{}/latest",
                    name.replace('/', "%2F")
                );
                let raw = self.curl(&url).map_err(Reason::Fetch)?;
                let j: Value =
                    serde_json::from_str(&raw).map_err(|e| Reason::Value(e.to_string()))?;
                let repo = j.get("repository");
                let slug = match repo {
                    Some(Value::Object(o)) => o.get("url").and_then(Value::as_str),
                    Some(Value::String(s)) => Some(s.as_str()),
                    _ => None,
                }
                .and_then(github_slug);
                let version = j
                    .get("version")
                    .map(pyish::py_str)
                    .ok_or(Reason::Key("version"))?;
                Ok((version, slug, None))
            }
            _ => {
                let raw = self
                    .curl(&format!("https://crates.io/api/v1/crates/{name}"))
                    .map_err(Reason::Fetch)?;
                let j: Value =
                    serde_json::from_str(&raw).map_err(|e| Reason::Value(e.to_string()))?;
                let empty = Value::Object(Default::default());
                let c = j
                    .get("crate")
                    .filter(|c| pyish::truthy(c))
                    .unwrap_or(&empty);
                let mut vs: Vec<String> = j
                    .get("versions")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter(|v| !v.get("yanked").is_some_and(pyish::truthy))
                            .filter_map(|v| v.get("num").map(pyish::py_str))
                            .collect()
                    })
                    .unwrap_or_default();
                vs.reverse();
                let latest = match c.get("max_stable_version").filter(|v| pyish::truthy(v)) {
                    Some(v) => pyish::py_str(v),
                    None => c
                        .get("newest_version")
                        .map(pyish::py_str)
                        .ok_or(Reason::Key("newest_version"))?,
                };
                let slug = c
                    .get("repository")
                    .and_then(Value::as_str)
                    .and_then(github_slug);
                Ok((latest, slug, Some(vs)))
            }
        }
    }

    /// Markdown of the releases of a GitHub repo whose tag is in `(lo, hi]`,
    /// oldest first so the 40 KB cap drops the releases past the new pin
    /// rather than the ones between old and new; "" if none.
    fn github_releases(
        &self,
        slug: &str,
        lo: &Option<VKey>,
        hi: &Option<VKey>,
    ) -> Result<String, String> {
        let url = format!("repos/{slug}/releases?per_page=60");
        let raw = match self.fetch_cmd(&["gh".into(), "api".into(), url.clone()], None, Vec::new())
        {
            Ok(r) => r,
            Err(_) => self.curl(&format!("https://api.github.com/{url}"))?,
        };
        let list: Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
        let mut rels: Vec<(VKey, &Value)> = Vec::new();
        for r in list.as_array().into_iter().flatten() {
            let tag = r.get("tag_name").and_then(Value::as_str).unwrap_or("");
            let Some(k) = vkey(tag) else { continue };
            let draft = r.get("draft").is_some_and(pyish::truthy);
            if !draft && lo.as_ref().is_none_or(|l| &k > l) && hi.as_ref().is_none_or(|h| &k <= h) {
                rels.push((k, r));
            }
        }
        rels.sort_by(|a, b| a.0.cmp(&b.0));
        let mut out = String::new();
        for (_, r) in rels {
            let published = r
                .get("published_at")
                .filter(|v| pyish::truthy(v))
                .map(pyish::py_str)
                .unwrap_or_default();
            let body = r
                .get("body")
                .filter(|v| pyish::truthy(v))
                .map(pyish::py_str)
                .unwrap_or_default();
            out.push_str(&format!(
                "# {} ({})\n\n{}\n\n",
                r.get("tag_name").map(pyish::py_str).unwrap_or_default(),
                head_chars(&published, 10),
                body.trim()
            ));
        }
        Ok(out)
    }

    /// The CHANGELOG* shipped in the module cache or `node_modules` copy, "" if none.
    fn local_changelog(&self, dep: &Dep) -> String {
        let ws = &self.ctx.ws;
        let root = match dep.eco {
            "go" => module_dir(self.ctx, &dep.go_mod_dir, &dep.name, &dep.new),
            "npm" => {
                let mut d = if dep.file.is_empty() {
                    String::new()
                } else {
                    dirname(&dep.file)
                };
                loop {
                    let cand = join(&[ws, &d, "node_modules", &dep.name]);
                    if Path::new(&cand).is_dir() {
                        break Some(cand);
                    }
                    if d.is_empty() {
                        break None;
                    }
                    d = dirname(&d);
                }
            }
            _ => None,
        };
        let Some(root) = root else {
            return String::new();
        };
        let Ok(rd) = std::fs::read_dir(&root) else {
            return String::new();
        };
        let mut names: Vec<String> = rd
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        for f in names {
            let up = f.to_uppercase();
            let wanted = ["CHANGELOG", "CHANGES", "HISTORY"]
                .iter()
                .any(|p| up.starts_with(p));
            let path = join(&[&root, &f]);
            if wanted && Path::new(&path).is_file() {
                return read_text(&path);
            }
        }
        String::new()
    }

    /// Fetches and writes the notes for one dependency. Ok is the context
    /// message and a diagnostic; Err is an I/O failure writing the notes.
    fn resolve(
        &self,
        dep: &Dep,
        out_dir: &str,
        scratch_note: &str,
    ) -> Result<(String, Option<Diagnostic>), String> {
        let (name, new) = (&dep.name, &dep.new);
        let old = dep.old.as_ref();
        let (latest, slug, versions) = match self.registry_latest(dep, out_dir) {
            Ok(r) => r,
            Err(e) => return Ok((format!("upstream {name}: unavailable ({e})"), None)),
        };
        let mut lo = old.filter(|o| !is_pseudo(o)).and_then(|o| vkey(o));
        let pin = vkey(new);
        let latest_key = vkey(&latest);
        let stale = matches!((&pin, &latest_key), (Some(p), Some(l)) if l > p);
        let hi = if stale { latest_key } else { pin.clone() };
        if let (None, Some(p)) = (old, &pin) {
            // An addition: the notes for the pinned release itself, then anything newer.
            lo = Some((p.0, p.1, p.2, 0, String::new()));
        }
        let pdir = join(&[out_dir, "upstream", &safe_name(name)]);
        std::fs::create_dir_all(&pdir).map_err(|e| strerror(&e))?;
        let (mut notes, mut why, mut fname) = (String::new(), String::new(), "releases.md");
        if let Some(slug) = &slug {
            match self.github_releases(slug, &lo, &hi) {
                Ok(n) => notes = n,
                Err(e) => why = e,
            }
        }
        if notes.is_empty() {
            let mut text = self.local_changelog(dep);
            if text.is_empty() {
                if let Some(slug) = &slug {
                    for f in ["CHANGELOG.md", "CHANGES.md"] {
                        if let Ok(t) = self.curl(&format!(
                            "https://raw.githubusercontent.com/{slug}/HEAD/{f}"
                        )) {
                            text = t;
                            break;
                        }
                    }
                }
            }
            if !text.is_empty() {
                notes = trim_changelog(&text, &lo, &hi);
                fname = "CHANGELOG.md";
            }
        }
        let mut wrote: Vec<&str> = Vec::new();
        if !notes.is_empty() {
            if notes.len() > MAX_NOTES_BYTES {
                notes = format!(
                    "{}\n\n[truncated at 40 KB]\n",
                    crate::builtin::head_bytes(&notes, MAX_NOTES_BYTES)
                );
            }
            let through = if stale { &latest } else { new };
            let adopted = if stale {
                format!("; the diff pins {new}, so everything above it is not yet adopted")
            } else {
                String::new()
            };
            let header = format!(
                "<!-- {name}: releases after {} through {through}{adopted} -->\n\n{notes}",
                old.map_or("the pinned release", |o| if o.is_empty() {
                    "the pinned release"
                } else {
                    o.as_str()
                })
            );
            std::fs::write(join(&[&pdir, fname]), header).map_err(|e| strerror(&e))?;
            wrote.push(fname);
        }
        if let Some(vs) = versions.filter(|v| !v.is_empty()) {
            std::fs::write(
                join(&[&pdir, "versions.txt"]),
                format!("{}\n", vs.join("\n")),
            )
            .map_err(|e| strerror(&e))?;
            wrote.push("versions.txt");
        }
        let span = match old.filter(|o| !o.is_empty()) {
            Some(o) => format!("{o}→{new}"),
            None => format!("added at {new}"),
        };
        if wrote.is_empty() {
            let extra = if why.is_empty() {
                String::new()
            } else {
                format!("; {why}")
            };
            return Ok((
                format!("upstream {name} {span} (latest {latest}): unavailable (no release notes found{extra})"),
                None,
            ));
        }
        let target = join(&[&pdir, wrote[0]]);
        let msg = format!("upstream {name} {span} (latest {latest}): {target}{scratch_note}");
        let diag = stale.then(|| Diagnostic {
            tool: "upstream".into(),
            file: Some(dep.file.clone()).filter(|f| !f.is_empty()),
            line: dep.line.map(|l| l as u64),
            message: format!("{name} is pinned at {new}; the latest release is {latest}"),
        });
        Ok((msg, diag))
    }
}

/// A file at a revision, "" when absent; independent of the stage deadlines.
fn show_rev(ctx: &Ctx, rev: &str, path: &str) -> String {
    if rev.is_empty() {
        return String::new();
    }
    let args: Vec<String> = ["-C", &ctx.ws, "show", &format!("{rev}:{path}")]
        .map(String::from)
        .to_vec();
    let opts = Opts {
        timeout: Some(Duration::from_secs(2)),
        ..Opts::default()
    };
    match ctx.run.run("git", &args, &opts) {
        Ok(c) if c.ok() => pyish::universal_newlines(&c.stdout),
        _ => String::new(),
    }
}

/// Whether files can be created in `dir`.
fn writable(dir: &str) -> bool {
    if !Path::new(dir).is_dir() {
        return false;
    }
    let probe = join(&[dir, &format!(".review-run-probe-{}", std::process::id())]);
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(e) => e.kind() == std::io::ErrorKind::AlreadyExists,
    }
}

/// A fresh private directory under the system temp dir.
fn scratch_dir() -> Option<String> {
    use std::os::unix::fs::DirBuilderExt;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "review-upstream-{}-{nanos:08x}",
        std::process::id()
    ));
    std::fs::DirBuilder::new().mode(0o700).create(&dir).ok()?;
    Some(dir.to_string_lossy().into_owned())
}

/// The context items and diagnostics for the dependencies the diff touches.
/// Notes land in `<out>/upstream/<pkg>/`: the contract's per-node out
/// directory, else the directory holding the saved diff, else a scratch
/// directory when neither is writable.
pub(super) fn stage(
    ctx: &Ctx,
    base: &str,
    head: &str,
    contract: &Value,
    text: &str,
    imported: &[Imported],
) -> (Vec<Item>, Vec<Diagnostic>) {
    let by_file = diff_by_file(text);
    let show_head = |p: &str| {
        if head.is_empty() {
            read_text(&join(&[&ctx.ws, p]))
        } else {
            show_rev(ctx, head, p)
        }
    };
    let show_base = |p: &str| show_rev(ctx, base, p);
    let mut deps = dependency_changes(&by_file, &show_head, &show_base);
    let mut have: HashSet<(&str, String)> = deps.iter().map(|d| (d.eco, d.name.clone())).collect();
    for i in imported {
        if !i.version.is_empty() && have.insert((i.eco, i.name.clone())) {
            deps.push(Dep {
                eco: i.eco,
                name: i.name.clone(),
                old: None,
                new: i.version.clone(),
                file: String::new(),
                line: None,
                rank: 2,
                go_mod_dir: i.go_mod_dir.clone(),
            });
        }
    }
    deps.sort_by_key(|d| d.rank);
    deps.truncate(MAX_UPSTREAM);
    if deps.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let mut cands: Vec<String> = Vec::new();
    if let Some(o) = contract
        .get("out")
        .and_then(Value::as_str)
        .filter(|o| !o.is_empty())
    {
        cands.push(o.to_string());
    }
    if let Some(d) = contract
        .get("diff")
        .and_then(Value::as_str)
        .filter(|d| !d.is_empty())
    {
        cands.push(dirname(d));
    }
    let (out_dir, note) = match cands.into_iter().find(|c| writable(c)) {
        Some(c) => (c, String::new()),
        None => match scratch_dir() {
            Some(d) => (
                d,
                " (scratch dir: the harness gave this node no output directory)".to_string(),
            ),
            None => return (Vec::new(), Vec::new()),
        },
    };
    let up = Up {
        ctx,
        end: Instant::now() + BUDGET,
    };
    let results: Vec<Result<(String, Option<Diagnostic>), String>> = std::thread::scope(|s| {
        let handles: Vec<_> = deps
            .iter()
            .map(|d| s.spawn(|| up.resolve(d, &out_dir, &note)))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|_| Err("internal error".into())))
            .collect()
    });
    let (mut items, mut diags) = (Vec::new(), Vec::new());
    for (d, r) in deps.iter().zip(results) {
        let (msg, diag) = r.unwrap_or_else(|e| {
            (
                format!("upstream {}: unavailable ({})", d.name, head_chars(&e, 80)),
                None,
            )
        });
        items.push(free_form(-2, format!("upstream:{}", d.name), msg));
        diags.extend(diag);
    }
    (items, diags)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::context_pack::Config;
    use crate::builtin::stub::{fail, ok, Stub};
    use serde_json::json;

    #[test]
    fn versions_sort_with_releases_above_their_prereleases() {
        assert!(vkey("v1.10.0") > vkey("v1.9.0"));
        assert!(vkey("v1.9.0") > vkey("v1.9.0-rc1"));
        assert!(vkey("v1.9.0-rc1") > vkey("1.2"));
        assert!(vkey("latest").is_none());
        assert!(vkey("").is_none());
    }

    #[test]
    fn spec_versions_and_slugs() {
        assert_eq!(spec_version("^1.2.3").as_deref(), Some("1.2.3"));
        assert_eq!(spec_version("workspace:*"), None);
        assert_eq!(spec_version("github:o/r"), None);
        assert_eq!(
            github_slug("git+https://github.com/o/r.git").as_deref(),
            Some("o/r")
        );
        assert_eq!(github_slug("github.com/o/r/v2").as_deref(), Some("o/r"));
        assert_eq!(github_slug("golang.org/x/net"), None);
        assert!(is_pseudo("v0.0.0-20240101000000-abcdef123456"));
        assert!(!is_pseudo("v1.2.3"));
        assert_eq!(safe_name("github.com/a/b"), "github.com_a_b");
        assert_eq!(safe_name("@scope/pkg"), "@scope_pkg");
    }

    const GOMOD: &str = "+++ b/go.mod\n@@ -5,2 +5,2 @@\n-\tgithub.com/stretchr/testify v1.8.0\n+\tgithub.com/stretchr/testify v1.9.0\n@@ -9 +9 @@\n+\tgolang.org/x/sys v0.1.0 // indirect\n";
    const PKGJSON: &str = "+++ b/web/package.json\n@@ -3 +3,2 @@\n+    \"left-pad\": \"^1.3.0\",\n+    \"name\": \"web\",\n";

    fn heads(p: &str) -> String {
        if p == "web/package.json" {
            r#"{"name": "web", "dependencies": {"left-pad": "^1.3.0"}}"#.into()
        } else {
            String::new()
        }
    }

    #[test]
    fn dependency_changes_read_go_npm_and_cargo_diffs() {
        let by_file = diff_by_file(&format!("{GOMOD}{PKGJSON}"));
        let ch = dependency_changes(&by_file, &heads, &|_| String::new());
        let got: Vec<_> = ch
            .iter()
            .map(|c| {
                (
                    c.eco,
                    c.name.as_str(),
                    c.old.as_deref(),
                    c.new.as_str(),
                    c.rank,
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                (
                    "go",
                    "github.com/stretchr/testify",
                    Some("v1.8.0"),
                    "v1.9.0",
                    0
                ),
                ("npm", "left-pad", None, "1.3.0", 1),
                ("go", "golang.org/x/sys", None, "v0.1.0", 3),
            ]
        );
        assert_eq!(ch[0].line, Some(5));
        assert_eq!(ch[0].go_mod_dir, "");

        let cargo = "+++ b/Cargo.toml\n@@ -1 +1 @@\n-serde = \"1.0.100\"\n+serde = { version = \"1.0.150\", features = [\"derive\"] }\n";
        let cg = dependency_changes(
            &diff_by_file(cargo),
            &|_| "[dependencies]\nserde = \"1\"\n".into(),
            &|_| String::new(),
        );
        let got: Vec<_> = cg
            .iter()
            .map(|c| (c.name.as_str(), c.old.as_deref(), c.new.as_str()))
            .collect();
        assert_eq!(got, [("serde", Some("1.0.100"), "1.0.150")]);
    }

    #[test]
    fn lockfile_bumps_count_only_for_declared_packages() {
        let names: HashSet<String> = ["left-pad".to_string()].into();
        let lock = r#"{"packages": {"node_modules/left-pad": {"version": "1.3.0"}, "node_modules/other": {"version": "9.0.0"}}}"#;
        let got = lock_versions("package-lock.json", lock, &names);
        assert_eq!(
            got,
            HashMap::from([("left-pad".to_string(), "1.3.0".to_string())])
        );
        let names: HashSet<String> = ["@a/b".to_string()].into();
        let yarn = "\"@a/b@^1.0.0\", \"@a/b@^1.1.0\":\n  version \"1.1.4\"\n";
        assert_eq!(lock_versions("yarn.lock", yarn, &names)["@a/b"], "1.1.4");
        let names: HashSet<String> = ["left-pad".to_string()].into();
        let pnpm = "packages:\n  /left-pad@1.3.0:\n    resolution: {}\n";
        assert_eq!(
            lock_versions("pnpm-lock.yaml", pnpm, &names)["left-pad"],
            "1.3.0"
        );
        assert!(lock_versions("package-lock.json", "", &names).is_empty());
    }

    #[test]
    fn cargo_tables_name_their_dependencies() {
        let toml = "[package]\nname=\"x\"\n[dependencies]\nserde = \"1.0\"\n[dev-dependencies.tokio]\nversion = \"1\"\n";
        let got = declared_cargo(toml);
        assert_eq!(
            got,
            HashSet::from(["serde".to_string(), "tokio".to_string()])
        );
    }

    #[test]
    fn a_changelog_is_cut_to_the_versions_between_old_and_new() {
        let text = "# Changelog\n\n## 1.2.0\nnew\n## 1.1.0\nmid\n## 1.0.0\nold\n";
        assert_eq!(
            trim_changelog(text, &vkey("1.0.0"), &vkey("1.1.0")),
            "## 1.1.0\nmid\n"
        );
        assert_eq!(
            trim_changelog("no headings here", &None, &None),
            "no headings here"
        );
        assert_eq!(
            trim_changelog("#### 9.9.9\nx\n", &None, &None),
            "#### 9.9.9\nx\n"
        );
    }

    #[test]
    fn diffs_split_by_file_keep_added_line_numbers() {
        let by = diff_by_file("+++ b/a\n@@ -1,2 +4,3 @@\n ctx\n+x\n-y\n+z\n+++ /dev/null\n+gone\n");
        assert_eq!(by.len(), 1);
        let rows: Vec<_> = by[0].1.iter().map(|r| (r.sign, r.line)).collect();
        assert_eq!(rows, [('+', Some(5)), ('-', None), ('+', Some(6))]);
    }

    fn registry(argv: &[String]) -> std::io::Result<crate::builtin::Cmd> {
        let rels: Vec<Value> = ["v1.7.0", "v1.8.0", "v1.8.4", "v1.9.0", "v1.10.0"]
            .iter()
            .enumerate()
            .map(|(i, t)| {
                json!({"tag_name": t, "published_at": format!("2024-01-0{}T00:00:00Z", i + 1),
                       "body": format!("notes for {t}"), "draft": false})
            })
            .collect();
        let last = argv.last().map(String::as_str).unwrap_or("");
        if last == "golang.org/x/sys" {
            return fail(1, "go: no network");
        }
        if argv[..3] == ["go", "list", "-m"] {
            return ok(&format!(
                "{last} v1.7.0 v1.8.0 v1.8.4 v1.9.0 v1.10.0 v1.11.0-rc1\n"
            ));
        }
        if argv[0] == "gh" && last.contains("stretchr/testify/releases") {
            return ok(&Value::Array(rels).to_string());
        }
        if argv[0] == "curl" && last == "https://registry.npmjs.org/left-pad/latest" {
            return ok(
                r#"{"version": "1.3.0", "repository": {"url": "git+https://github.com/stevemao/left-pad.git"}}"#,
            );
        }
        if argv[0] == "gh" && last.contains("stevemao/left-pad/releases") {
            return ok(
                r#"[{"tag_name": "v1.3.0", "body": "left-pad 1.3.0 body", "published_at": "2018-04-01"}]"#,
            );
        }
        fail(1, "stub: no network")
    }

    fn stub() -> Stub {
        Stub::new(
            &["go", "gh", "curl", "git"],
            Box::new(|prog, args, _| {
                let mut argv = vec![prog.to_string()];
                argv.extend(args.iter().cloned());
                if prog == "git" {
                    return fail(1, "");
                }
                registry(&argv)
            }),
        )
    }

    fn config() -> Config {
        Config {
            max_items: 118,
            no_gh: false,
            pr_body: String::new(),
            pr_number: String::new(),
            go_mod_cache: None,
            home: None,
        }
    }

    fn tmp(tag: &str) -> crate::builtin::TempDir {
        crate::builtin::TempDir::new(&format!("cp-up-{tag}")).unwrap()
    }

    #[test]
    fn notes_for_each_dependency_land_in_the_out_directory() {
        let dir = tmp("out");
        let ws = dir.path().to_string_lossy().into_owned();
        let out = join(&[&ws, "node-out"]);
        std::fs::create_dir_all(&out).unwrap();
        std::fs::create_dir_all(join(&[&ws, "web"])).unwrap();
        std::fs::write(
            join(&[&ws, "web", "package.json"]),
            heads("web/package.json"),
        )
        .unwrap();
        let (cfg, stub) = (config(), stub());
        let ctx = Ctx::new(&stub, &cfg, &ws);
        let contract = json!({"out": out});
        let text = format!("{GOMOD}{PKGJSON}");
        let (items, diags) = stage(&ctx, "", "", &contract, &text, &[]);
        let msgs: HashMap<String, String> = items
            .iter()
            .map(|i| {
                (
                    i.why
                        .split(' ')
                        .nth(1)
                        .unwrap()
                        .trim_end_matches(':')
                        .to_string(),
                    i.why.clone(),
                )
            })
            .collect();
        let tpath = join(&[&out, "upstream", "github.com_stretchr_testify"]);
        assert_eq!(
            msgs["github.com/stretchr/testify"],
            format!("upstream github.com/stretchr/testify v1.8.0→v1.9.0 (latest v1.10.0): {tpath}/releases.md")
        );
        let notes = std::fs::read_to_string(join(&[&tpath, "releases.md"])).unwrap();
        for tag in ["v1.10.0", "v1.9.0", "v1.8.4"] {
            assert!(notes.contains(&format!("notes for {tag}")), "{notes}");
        }
        for tag in ["v1.8.0", "v1.7.0"] {
            assert!(!notes.contains(&format!("notes for {tag}")), "{notes}");
        }
        assert!(notes.starts_with("<!-- github.com/stretchr/testify: releases after v1.8.0 through v1.10.0; the diff pins v1.9.0, so everything above it is not yet adopted -->\n\n# v1.8.4 (2024-01-03)\n\nnotes for v1.8.4\n\n"));
        let versions = std::fs::read_to_string(join(&[&tpath, "versions.txt"])).unwrap();
        assert!(versions.contains("v1.11.0-rc1"));
        let lp = join(&[&out, "upstream", "left-pad", "releases.md"]);
        assert_eq!(
            msgs["left-pad"],
            format!("upstream left-pad added at 1.3.0 (latest 1.3.0): {lp}")
        );
        assert!(std::fs::read_to_string(&lp)
            .unwrap()
            .contains("left-pad 1.3.0 body"));
        assert!(msgs["golang.org/x/sys"].starts_with("upstream golang.org/x/sys: unavailable ("));
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].file.as_deref(), Some("go.mod"));
        assert_eq!(diags[0].line, Some(5));
        assert_eq!(
            diags[0].message,
            "github.com/stretchr/testify is pinned at v1.9.0; the latest release is v1.10.0"
        );
    }

    #[test]
    fn without_an_out_directory_notes_go_beside_the_diff_or_to_a_scratch_directory() {
        let dir = tmp("diff");
        let ws = dir.path().to_string_lossy().into_owned();
        std::fs::create_dir_all(join(&[&ws, "web"])).unwrap();
        std::fs::write(
            join(&[&ws, "web", "package.json"]),
            heads("web/package.json"),
        )
        .unwrap();
        let (cfg, stub) = (config(), stub());
        let ctx = Ctx::new(&stub, &cfg, &ws);
        let diff = join(&[&ws, "diff.patch"]);
        let (items, _) = stage(&ctx, "", "", &json!({"diff": diff}), PKGJSON, &[]);
        assert_eq!(
            items[0].why,
            format!(
                "upstream left-pad added at 1.3.0 (latest 1.3.0): {}",
                join(&[&ws, "upstream", "left-pad", "releases.md"])
            )
        );
        let (items, _) = stage(&ctx, "", "", &json!({}), PKGJSON, &[]);
        assert!(
            items[0].why.contains("review-upstream-"),
            "{}",
            items[0].why
        );
        assert!(items[0]
            .why
            .ends_with("(scratch dir: the harness gave this node no output directory)"));
    }

    #[test]
    fn imports_with_no_diff_row_are_looked_up_as_additions() {
        let dir = tmp("imp");
        let ws = dir.path().to_string_lossy().into_owned();
        let (cfg, stub) = (config(), stub());
        let ctx = Ctx::new(&stub, &cfg, &ws);
        let imported = [Imported {
            eco: "npm",
            name: "left-pad".into(),
            version: "1.3.0".into(),
            go_mod_dir: String::new(),
        }];
        let (items, diags) = stage(
            &ctx,
            "",
            "",
            &json!({"out": ws}),
            "+++ b/a.go\n+x := 1\n",
            &imported,
        );
        assert_eq!(items.len(), 1);
        assert!(items[0]
            .why
            .starts_with("upstream left-pad added at 1.3.0 (latest 1.3.0): "));
        assert!(diags.is_empty());
    }

    #[test]
    fn a_diff_without_dependencies_does_nothing() {
        let dir = tmp("none");
        let ws = dir.path().to_string_lossy().into_owned();
        let (cfg, stub) = (config(), stub());
        let ctx = Ctx::new(&stub, &cfg, &ws);
        let (items, diags) = stage(&ctx, "", "", &json!({}), "+++ b/a.go\n+x := 1\n", &[]);
        assert!(items.is_empty() && diags.is_empty());
        assert!(stub.calls.borrow().is_empty());
    }

    #[test]
    fn failures_read_as_unavailable_with_the_tool_and_its_last_line() {
        let dir = tmp("fail");
        let ws = dir.path().to_string_lossy().into_owned();
        let (cfg, stub) = (config(), stub());
        let ctx = Ctx::new(&stub, &cfg, &ws);
        let up = Up {
            ctx: &ctx,
            end: Instant::now() + BUDGET,
        };
        let argv: Vec<String> = ["go", "list", "-m", "golang.org/x/sys"]
            .map(String::from)
            .to_vec();
        assert_eq!(
            up.fetch_cmd(&argv, None, Vec::new()).unwrap_err(),
            "go: go: no network"
        );
        let spent = Up {
            ctx: &ctx,
            end: Instant::now(),
        };
        assert_eq!(
            spent.fetch_cmd(&argv, None, Vec::new()).unwrap_err(),
            "stage budget spent"
        );
    }
}
