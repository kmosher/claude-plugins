//! `builtin:prior-comments`: report concerns that earlier reviewers raised on
//! the changed files and that were adjudicated against, so the generators do
//! not raise them again.
//!
//! For the largest changed files it finds merged pull requests that touched
//! them, collects their review discussion, and has a small model list the
//! settled issues. Each becomes a diagnostic. Anything that stops the mining
//! (no `gh`, no `claude`, no GitHub remote, no pull requests) yields no
//! diagnostics: this node never fails the run.

use super::{head_bytes, present, text_of, Opts, Runner};
use crate::provider::NO_GH_ENV;
use crate::schema::Diagnostic;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::HashSet;

/// How much of a pull request's discussion the model is shown, in characters.
const DISCUSSION_LIMIT: usize = 30_000;

pub struct Config {
    pub no_gh: bool,
    pub max_files: usize,
    /// Passed to `gh search --limit` as given.
    pub max_prs: String,
}

impl Config {
    pub fn from_env() -> Config {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        Config {
            no_gh: var(NO_GH_ENV).is_some(),
            max_files: var("PRIOR_COMMENTS_MAX_FILES")
                .and_then(|v| v.parse().ok())
                .unwrap_or(10),
            max_prs: var("PRIOR_COMMENTS_MAX_PRS").unwrap_or_else(|| "5".into()),
        }
    }
}

fn log(msg: &str) {
    eprintln!("prior-comments: {msg}");
}

fn output(rows: Vec<Diagnostic>) -> String {
    json!({"diagnostics": rows, "triggers": []}).to_string()
}

/// `owner/repo` from a remote URL in any of the usual forms.
pub fn owner_repo(url: &str) -> Option<String> {
    let scheme = Regex::new(r"^(git@|ssh://git@|https?://)[^/:]+[:/]").expect("static regex");
    let rest = scheme.replace(url, "");
    let rest = rest.strip_suffix(".git").unwrap_or(&rest);
    rest.contains('/').then(|| rest.to_string())
}

/// Changed files from `git diff --numstat` output, largest diffs first, binary
/// files last. Ties fall in reverse path order.
pub fn rank_numstat(numstat: &str) -> Vec<String> {
    let mut rows: Vec<(u64, String)> = numstat
        .lines()
        .filter_map(|l| {
            let mut f = l.splitn(3, '\t');
            let (added, deleted, path) = (f.next()?, f.next()?, f.next()?);
            let n = if added == "-" {
                0
            } else {
                added.parse::<u64>().unwrap_or(0) + deleted.parse::<u64>().unwrap_or(0)
            };
            Some((n, path.to_string()))
        })
        .collect();
    rows.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    rows.into_iter().map(|(_, p)| p).collect()
}

/// The first `max` non-empty paths. At least one is taken, as the count is
/// checked after each.
fn take_files(paths: impl IntoIterator<Item = String>, max: usize) -> Vec<String> {
    let mut files = Vec::new();
    for p in paths {
        if !p.is_empty() {
            files.push(p);
        }
        if files.len() >= max {
            break;
        }
    }
    files
}

fn login(v: &Value) -> String {
    text_of(
        v.get("author")
            .and_then(|a| a.get("login"))
            .unwrap_or(&Value::Null),
    )
}

/// A pull request's reviews (those with a body) and then its comments, from
/// `gh pr view --json comments,reviews`.
pub fn format_thread(pr: &Value) -> String {
    let list = |k: &str| {
        pr.get(k)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let mut parts = Vec::new();
    for r in list("reviews") {
        let body = present(r.get("body")).map(text_of).unwrap_or_default();
        if !body.is_empty() {
            let state = text_of(r.get("state").unwrap_or(&Value::Null));
            parts.push(format!("{} ({state}): {body}", login(&r)));
        }
    }
    for c in list("comments") {
        let body = text_of(c.get("body").unwrap_or(&Value::Null));
        parts.push(format!("{}: {body}", login(&c)));
    }
    parts.join("\n\n")
}

/// Inline review comments from `gh api .../pulls/N/comments --paginate`, whose
/// output is one JSON array per page. Reading stops at the first thing that is
/// not an array of comment objects.
pub fn format_inline(raw: &str) -> String {
    let mut lines = Vec::new();
    'pages: for page in serde_json::Deserializer::from_str(raw).into_iter::<Value>() {
        let Ok(Value::Array(items)) = page else { break };
        for c in items {
            if !matches!(c, Value::Object(_) | Value::Null) {
                break 'pages;
            }
            let get = |k: &str| c.get(k);
            let user = text_of(
                get("user")
                    .and_then(|u| u.get("login"))
                    .unwrap_or(&Value::Null),
            );
            let path = text_of(get("path").unwrap_or(&Value::Null));
            let line = present(get("line"))
                .or_else(|| present(get("original_line")))
                .map(text_of)
                .unwrap_or_else(|| "?".into());
            let body = text_of(get("body").unwrap_or(&Value::Null));
            lines.push(format!("{user} on {path}:{line}: {body}"));
        }
    }
    lines.join("\n")
}

pub fn prompt(n: u64, file: &str, discussion: &str) -> String {
    let shown: String = discussion.chars().take(DISCUSSION_LIMIT).collect();
    format!(
        "Below is the review discussion from a merged pull request (#{n}) that touched {file}.\n\
List only SETTLED issues: a concern a reviewer raised that was then adjudicated against (the author or another reviewer explained why it is not a problem, or it was deliberately accepted). Skip concerns that were fixed, requested style guidance, and chatter.\n\
Answer with only a JSON list, no prose and no code fence: [{{\"file\": \"<path the concern is about, or {file}>\", \"concern\": \"...\", \"reason\": \"why it was settled\"}}]. Answer [] when nothing qualifies.\n\
\n\
{shown}"
    )
}

/// The model's answer from `claude -p --output-format json`: its `result`
/// string with any code-fence lines dropped, and a newline after it as
/// printing it would add. Empty when the reply has no result.
pub fn answer_of(reply: &str) -> String {
    let result = serde_json::from_str::<Value>(reply)
        .ok()
        .and_then(|v| present(v.get("result")).map(text_of));
    let Some(result) = result else {
        return String::new();
    };
    format!("{result}\n")
        .split_inclusive('\n')
        .filter(|l| !l.starts_with("```"))
        .collect()
}

/// The settled issues in a model's answer, as diagnostics for PR `n`, and
/// whether the whole answer was readable. Rows read before a problem are kept.
pub fn rows_from_answer(answer: &str, n: u64, file: &str) -> (Vec<Diagnostic>, bool) {
    let mut rows = Vec::new();
    for value in serde_json::Deserializer::from_str(answer).into_iter::<Value>() {
        let Ok(value) = value else {
            return (rows, false);
        };
        let Value::Array(items) = value else { continue };
        for item in items {
            let (concern, reason) = match &item {
                Value::Null => continue,
                Value::Object(o) => (present(o.get("concern")), present(o.get("reason"))),
                _ => return (rows, false),
            };
            let (Some(concern), Some(reason)) = (concern, reason) else {
                continue;
            };
            let (concern, reason) = (text_of(concern), text_of(reason));
            if concern.is_empty() || reason.is_empty() {
                continue;
            }
            let file = match present(item.get("file")) {
                Some(Value::String(s)) => s.clone(),
                _ => file.to_string(),
            };
            rows.push(Diagnostic {
                tool: format!("prior-pr#{n}"),
                file: Some(file),
                line: None,
                message: format!("settled: {concern} — {reason}").replace(['\n', '\t'], " "),
            });
        }
    }
    (rows, true)
}

fn file_prs(r: &dyn Runner, repo: &str, file: &str, cfg: &Config) -> Option<Vec<u64>> {
    let args: Vec<String> = [
        "search",
        "prs",
        "--repo",
        repo,
        "--merged",
        "--limit",
        &cfg.max_prs,
        "--json",
        "number,title,url",
        "--",
        file,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    match r.run("gh", &args, &Opts::default()) {
        Ok(c) if c.ok() => Some(numbers_of(&c.stdout)),
        Ok(c) => {
            log(&format!(
                "gh search failed for {file}: {}",
                head_bytes(&c.stderr, 200)
            ));
            None
        }
        Err(e) => {
            log(&format!("gh search failed for {file}: {e}"));
            None
        }
    }
}

/// The `number` of each entry in a `gh search prs --json` array.
pub fn numbers_of(json: &str) -> Vec<u64> {
    serde_json::from_str::<Value>(json)
        .ok()
        .and_then(|v| v.as_array().cloned())
        .map(|a| a.iter().filter_map(|p| p.get("number")?.as_u64()).collect())
        .unwrap_or_default()
}

fn changed_files(contract: &Value, r: &dyn Runner, ws: &str) -> Vec<String> {
    let text_field = |k: &str| match present(contract.get(k)) {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    };
    let (base, head) = (text_field("base"), text_field("head"));
    let mut ranked = Vec::new();
    if !base.is_empty() && !head.is_empty() {
        let args: Vec<String> = ["-C", ws, "diff", "--numstat", &base, &head]
            .iter()
            .map(|s| s.to_string())
            .collect();
        if let Ok(c) = r.run("git", &args, &Opts::default()) {
            ranked = rank_numstat(&c.stdout);
        }
    }
    if ranked.is_empty() {
        ranked = contract
            .get("changed_files")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|f| f.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
    }
    ranked
}

/// Runs the node over `stdin`, the mechanical contract, and returns its
/// stdout JSON.
pub fn run(stdin: &str, cfg: &Config, r: &dyn Runner) -> String {
    output(mine(stdin, cfg, r))
}

fn mine(stdin: &str, cfg: &Config, r: &dyn Runner) -> Vec<Diagnostic> {
    if cfg.no_gh {
        log("GitHub is disabled for this run (--no-gh)");
        return vec![];
    }
    for tool in ["gh", "claude", "git"] {
        if !r.on_path(tool) {
            log(&format!("{tool} is not on PATH"));
            return vec![];
        }
    }
    let contract: Value = serde_json::from_str(stdin).unwrap_or(Value::Null);
    let Some(Value::String(ws)) = present(contract.get("workspace")) else {
        log("stdin is not a mechanical contract");
        return vec![];
    };
    let ws = ws.strip_suffix('/').unwrap_or(ws);

    let origin = r.run(
        "git",
        &[
            "-C".into(),
            ws.into(),
            "remote".into(),
            "get-url".into(),
            "origin".into(),
        ],
        &Opts::default(),
    );
    let Some(url) = origin.ok().filter(|c| c.ok()).map(|c| c.stdout) else {
        log("no origin remote");
        return vec![];
    };
    let url = url.trim_end_matches('\n');
    let Some(repo) = owner_repo(url) else {
        log(&format!("cannot derive owner/repo from {url}"));
        return vec![];
    };

    let files = take_files(changed_files(&contract, r, ws), cfg.max_files);
    if files.is_empty() {
        log("no changed files");
        return vec![];
    }
    log(&format!("{repo}: {} files", files.len()));

    let mut rows = Vec::new();
    let mut seen = HashSet::new();
    for f in &files {
        let Some(prs) = file_prs(r, &repo, f, cfg) else {
            continue;
        };
        for n in prs {
            if !seen.insert(n) {
                continue;
            }
            rows.extend(settled_issues(r, &repo, n, f));
        }
    }
    rows
}

/// The settled issues from PR `n`'s discussion, as diagnostics pinned to `f`
/// unless the model names another file.
fn settled_issues(r: &dyn Runner, repo: &str, n: u64, f: &str) -> Vec<Diagnostic> {
    let gh = |args: &[&str]| {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        r.run("gh", &args, &Opts::default())
            .map(|c| c.stdout)
            .unwrap_or_default()
    };
    let view = gh(&[
        "pr",
        "view",
        &n.to_string(),
        "--repo",
        repo,
        "--json",
        "comments,reviews",
    ]);
    let thread = serde_json::from_str::<Value>(&view)
        .map(|v| format_thread(&v))
        .unwrap_or_default();
    // The adjudicated concerns mostly live in inline review threads, which the
    // JSON above omits.
    let inline = gh(&[
        "api",
        &format!("repos/{repo}/pulls/{n}/comments"),
        "--paginate",
    ]);
    let inline = format_inline(&inline);
    let thread = format!(
        "{}\n\n{}",
        inline.trim_end_matches('\n'),
        thread.trim_end_matches('\n')
    );
    let thread = thread.trim_start();
    if thread.is_empty() {
        return vec![];
    }
    log(&format!(
        "PR #{n} touching {f}: {} bytes of discussion",
        thread.len()
    ));

    let ask = [
        "-p".to_string(),
        "--model".into(),
        "haiku".into(),
        "--output-format".into(),
        "json".into(),
    ];
    let prompt = prompt(n, f, thread);
    let reply = r.run(
        "claude",
        &ask,
        &Opts {
            stdin: Some(&prompt),
            ..Opts::default()
        },
    );
    let reply = match reply {
        Ok(c) if c.ok() => c.stdout,
        Ok(c) => {
            log(&format!(
                "claude failed for PR #{n}: {}",
                head_bytes(&c.stderr, 200)
            ));
            return vec![];
        }
        Err(e) => {
            log(&format!("claude failed for PR #{n}: {e}"));
            return vec![];
        }
    };
    let answer = answer_of(&reply);
    log(&format!("PR #{n}: haiku answered {} bytes", answer.len()));
    let (rows, readable) = rows_from_answer(&answer, n, f);
    if !readable {
        log(&format!("PR #{n}: answer was not a JSON list"));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::stub::{fail, ok, Script, Stub};

    fn cfg() -> Config {
        Config {
            no_gh: false,
            max_files: 10,
            max_prs: "5".into(),
        }
    }

    #[test]
    fn owner_and_repo_come_from_every_remote_form() {
        for url in [
            "git@github.com:o/r.git",
            "ssh://git@github.com/o/r.git",
            "https://github.com/o/r",
            "https://github.com/o/r.git",
        ] {
            assert_eq!(owner_repo(url).as_deref(), Some("o/r"), "{url}");
        }
        assert_eq!(owner_repo("solo"), None);
        assert_eq!(owner_repo("https://host/solo").as_deref(), None);
    }

    #[test]
    fn the_largest_diffs_come_first_and_binary_files_last() {
        let numstat = "1\t1\tsmall.rs\n40\t2\tbig.rs\n-\t-\timg.png\n5\t5\tmid.rs\n";
        assert_eq!(
            rank_numstat(numstat),
            ["big.rs", "mid.rs", "small.rs", "img.png"]
        );
        assert_eq!(rank_numstat("2\t0\ta\n2\t0\tb\n"), ["b", "a"]);
        assert!(rank_numstat("").is_empty());
    }

    #[test]
    fn files_are_capped_but_never_below_one() {
        let p = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(take_files(p(&["a", "", "b", "c"]), 2), ["a", "b"]);
        assert_eq!(take_files(p(&["a", "b"]), 0), ["a"]);
        assert!(take_files(p(&["", ""]), 3).is_empty());
    }

    #[test]
    fn a_thread_lists_reviews_with_a_body_then_comments() {
        let pr = json!({
            "reviews": [
                {"author": {"login": "ann"}, "state": "COMMENTED", "body": "why a map?"},
                {"author": {"login": "bob"}, "state": "APPROVED", "body": ""},
            ],
            "comments": [{"author": {"login": "cy"}, "body": "ok by me"}],
        });
        assert_eq!(
            format_thread(&pr),
            "ann (COMMENTED): why a map?\n\ncy: ok by me"
        );
        assert_eq!(format_thread(&json!({})), "");
    }

    #[test]
    fn inline_comments_read_across_pages_and_stop_at_a_bad_one() {
        let pages = r#"[{"user":{"login":"a"},"path":"x.rs","line":3,"body":"one"}]
[{"user":{"login":"b"},"path":"y.rs","line":null,"original_line":9,"body":"two"},
 {"user":{"login":"c"},"path":"z.rs","body":"three"}]"#;
        assert_eq!(
            format_inline(pages),
            "a on x.rs:3: one\nb on y.rs:9: two\nc on z.rs:?: three"
        );
        assert_eq!(format_inline(r#"{"message":"Not Found"}"#), "");
        assert_eq!(
            format_inline(
                r#"[{"user":{"login":"a"},"path":"x","line":1,"body":"k"}, "junk", {"body":"lost"}]"#
            ),
            "a on x:1: k"
        );
        assert_eq!(format_inline(""), "");
    }

    #[test]
    fn the_model_is_shown_at_most_thirty_thousand_characters() {
        let long = "é".repeat(31_000);
        let p = prompt(7, "src/a.rs", &long);
        assert!(p.contains("(#7) that touched src/a.rs."));
        assert!(p.ends_with(&"é".repeat(30_000)));
        assert!(!p.contains(&"é".repeat(30_001)));
    }

    #[test]
    fn the_answer_is_the_result_without_code_fences() {
        let reply = json!({"result": "```json\n[{\"concern\":\"c\"}]\n```"}).to_string();
        assert_eq!(answer_of(&reply), "[{\"concern\":\"c\"}]\n");
        assert_eq!(answer_of("not json"), "");
        assert_eq!(answer_of(r#"{"result":null}"#), "");
        assert_eq!(answer_of(r#"{"result":""}"#), "\n");
    }

    #[test]
    fn settled_issues_become_diagnostics_on_the_file_or_the_one_the_model_names() {
        let answer = r#"[
            {"file": "other.rs", "concern": "uses unwrap", "reason": "it cannot fail here"},
            {"concern": "two\nlines", "reason": "tabs\there"},
            {"concern": "no reason"},
            {"concern": "", "reason": "empty concern"},
            null
        ]"#;
        let (rows, readable) = rows_from_answer(answer, 12, "a.rs");
        assert!(readable);
        let got: Vec<_> = rows
            .iter()
            .map(|d| (d.tool.as_str(), d.file.as_deref(), d.message.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                (
                    "prior-pr#12",
                    Some("other.rs"),
                    "settled: uses unwrap — it cannot fail here"
                ),
                (
                    "prior-pr#12",
                    Some("a.rs"),
                    "settled: two lines — tabs here"
                ),
            ]
        );
        assert!(rows.iter().all(|d| d.line.is_none()));
    }

    #[test]
    fn an_answer_that_is_not_a_list_gives_nothing_and_a_bad_tail_keeps_the_good_head() {
        let (rows, readable) = rows_from_answer("", 1, "f");
        assert!(rows.is_empty() && readable);
        let (rows, ok) = rows_from_answer(r#"{"concern":"c","reason":"r"}"#, 1, "f");
        assert!(rows.is_empty() && ok);
        let (rows, ok) = rows_from_answer("this is prose", 1, "f");
        assert!(rows.is_empty() && !ok);
        let (rows, ok) = rows_from_answer(
            r#"[{"concern":"c","reason":"r"}, "x", {"concern":"d","reason":"s"}]"#,
            1,
            "f",
        );
        assert_eq!((rows.len(), ok), (1, false));
    }

    fn quiet(stub: &Stub) -> Vec<String> {
        stub.calls.borrow().clone()
    }

    #[test]
    fn missing_tools_the_flag_and_a_missing_remote_all_yield_no_diagnostics() {
        let contract = r#"{"workspace":"/w","changed_files":["a.rs"]}"#;
        let empty = r#"{"diagnostics":[],"triggers":[]}"#;

        let s = Stub::new(&["gh", "git"], Box::new(|_, _, _| fail(1, "")));
        assert_eq!(run(contract, &cfg(), &s), empty);
        assert!(quiet(&s).is_empty(), "no claude means no work");

        let s = Stub::new(&["gh", "claude", "git"], Box::new(|_, _, _| fail(1, "")));
        let off = Config {
            no_gh: true,
            ..cfg()
        };
        assert_eq!(run(contract, &off, &s), empty);
        assert!(quiet(&s).is_empty());

        assert_eq!(run("nonsense", &cfg(), &s), empty);
        assert!(quiet(&s).is_empty());

        assert_eq!(run(contract, &cfg(), &s), empty);
        assert_eq!(quiet(&s).len(), 1, "only the remote lookup ran");
    }

    #[test]
    fn a_failed_search_skips_that_file_and_a_failed_model_skips_that_pr() {
        let script: Script = Box::new(|prog, args, _| {
            let a = args.join(" ");
            match (prog, a.as_str()) {
                ("git", s) if s.contains("remote") => ok("git@github.com:o/r.git\n"),
                ("gh", s) if s.starts_with("search") && s.ends_with("bad.rs") => {
                    fail(1, "rate limited")
                }
                ("gh", s) if s.starts_with("search") => ok(r#"[{"number":4}]"#),
                ("gh", s) if s.starts_with("pr view") => {
                    ok(r#"{"comments":[{"author":{"login":"a"},"body":"fine"}],"reviews":[]}"#)
                }
                ("gh", _) => ok("[]"),
                ("claude", _) => fail(1, "boom"),
                _ => fail(1, "unexpected"),
            }
        });
        let s = Stub::new(&["gh", "claude", "git"], script);
        let contract = r#"{"workspace":"/w/","changed_files":["bad.rs","good.rs","also.rs"]}"#;
        assert_eq!(
            run(contract, &cfg(), &s),
            r#"{"diagnostics":[],"triggers":[]}"#
        );
        let calls = quiet(&s);
        let searches = calls.iter().filter(|c| c.starts_with("gh search")).count();
        let asked = calls.iter().filter(|c| c.starts_with("claude")).count();
        assert_eq!(searches, 3);
        assert_eq!(asked, 1, "PR 4 is mined once however many files it touched");
        assert!(calls.iter().any(|c| c.contains("--limit 5")));
    }

    #[test]
    fn a_pull_request_found_through_two_files_is_mined_once_and_pinned_to_the_first() {
        let script: Script = Box::new(|prog, args, _| {
            let a = args.join(" ");
            match (prog, a.as_str()) {
                ("git", s) if s.contains("remote") => ok("https://github.com/o/r\n"),
                ("git", s) if s.contains("numstat") => ok("1\t0\tsmall.rs\n9\t9\tbig.rs\n"),
                ("gh", s) if s.starts_with("search") => {
                    ok(r#"[{"number":4,"title":"t","url":"u"}]"#)
                }
                ("gh", s) if s.starts_with("pr view") => ok(r#"{"comments":[],"reviews":[]}"#),
                ("gh", _) => {
                    ok(r#"[{"user":{"login":"a"},"path":"big.rs","line":2,"body":"leave it"}]"#)
                }
                ("claude", _) => ok(
                    &json!({"result": r#"[{"concern":"leave it","reason":"documented"}]"#})
                        .to_string(),
                ),
                _ => fail(1, "unexpected"),
            }
        });
        let s = Stub::new(&["gh", "claude", "git"], script);
        let contract =
            r#"{"workspace":"/w","base":"b","head":"h","changed_files":["small.rs","big.rs"]}"#;
        let out: Value = serde_json::from_str(&run(contract, &cfg(), &s)).unwrap();
        assert_eq!(
            out,
            json!({"diagnostics": [{
                "tool": "prior-pr#4", "file": "big.rs", "line": null,
                "message": "settled: leave it — documented"
            }], "triggers": []})
        );
        let calls = quiet(&s);
        assert_eq!(calls.iter().filter(|c| c.starts_with("claude")).count(), 1);
    }
}
