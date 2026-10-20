//! `builtin:changed-lint`: lint the changed files with the tools `lint-recipe`
//! picks for them, and report what those tools say as diagnostics.
//!
//! `lint-recipe` is an external command, not part of this repo; without it on
//! `PATH` (or at `$LINT_RECIPE`) this node reports nothing. It answers with
//! nothing when the repo declares its own `lint-fast` target, so such a repo
//! gets no diagnostics from this node. Lint complaints are diagnostics and the
//! node succeeds; failing at all means it could not do its job.

use super::{head_bytes, json_stream, present, text_of, Fail, Opts, Runner, TempDir};
use crate::schema::Diagnostic;
use regex::Regex;
use serde_json::{json, Value};
use std::path::Path;

fn die(message: &str) -> Fail {
    Fail {
        code: 2,
        message: format!("changed-lint: {message}"),
    }
}

/// One diagnostic row. Tabs and newlines in a message become spaces, trailing
/// spaces are dropped, and `file` is made relative to the workspace.
fn emit(ws: &str, tool: &str, file: &str, line: &str, message: &str) -> Diagnostic {
    let file = file.strip_prefix(&format!("{ws}/")).unwrap_or(file);
    let file = file.strip_prefix("./").unwrap_or(file);
    Diagnostic {
        tool: tool.into(),
        file: (!file.is_empty()).then(|| file.to_string()),
        line: line.parse().ok(),
        message: message
            .replace(['\t', '\n'], " ")
            .trim_end_matches(' ')
            .into(),
    }
}

/// Lines shaped `file:line[:col]: message`, which most linters can produce.
pub fn parse_located(ws: &str, tool: &str, text: &str) -> Vec<Diagnostic> {
    let shape = Regex::new(r"^([^:]+):([0-9]+)(:[0-9]+)?: (.+)$").expect("static regex");
    text.lines()
        .filter_map(|l| {
            let c = shape.captures(l)?;
            Some(emit(ws, tool, &c[1], &c[2], &c[4]))
        })
        .collect()
}

/// `rustfmt --check` reports `Diff in <file>:<line>:` or
/// `Diff in <file> at line <line>:`.
fn parse_rustfmt(ws: &str, tool: &str, text: &str) -> Vec<Diagnostic> {
    let shape = Regex::new(r"^Diff in (.+)(:| at line )([0-9]+):?$").expect("static regex");
    text.lines()
        .filter_map(|l| {
            let c = shape.captures(l)?;
            Some(emit(ws, tool, &c[1], &c[3], "would be reformatted"))
        })
        .collect()
}

/// Escapes a value the way a tab-separated row would carry it.
fn tsv(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// ESLint's `--format json`: an array of files, each with its messages.
fn parse_eslint(ws: &str, tool: &str, docs: &[Value]) -> Vec<Diagnostic> {
    let mut rows = Vec::new();
    for doc in docs {
        let Some(files) = doc.as_array() else {
            continue;
        };
        for f in files {
            let path = present(f.get("filePath")).map(text_of).unwrap_or_default();
            for m in f
                .get("messages")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let line = present(m.get("line")).map(text_of).unwrap_or_default();
                let mut message = text_of(m.get("message").unwrap_or(&Value::Null));
                if let Some(rule) = present(m.get("ruleId")) {
                    message = format!("{message} ({})", text_of(rule));
                }
                rows.push(emit(ws, tool, &tsv(&path), &line, &tsv(&message)));
            }
        }
    }
    rows
}

/// The argv for a plan entry, asking each known tool for the output shape that
/// carries a line number.
pub fn adjust_argv(tool: &str, argv: &[String]) -> Vec<String> {
    let s = |v: &str| v.to_string();
    match tool {
        "shellcheck" => {
            let mut out = vec![argv[0].clone(), s("-f"), s("gcc")];
            out.extend_from_slice(&argv[1..]);
            out
        }
        "ruff" => {
            let cut = argv.len().min(2);
            let mut out = argv[..cut].to_vec();
            out.push(s("--output-format=concise"));
            out.extend_from_slice(&argv[cut..]);
            out
        }
        "eslint" => {
            let mut out = argv.to_vec();
            out.extend([s("--format"), s("json")]);
            out
        }
        _ => argv.to_vec(),
    }
}

/// What one tool run said, as diagnostics. `list_only` marks a tool that
/// prints the paths it would change and exits 0 (`gofmt -l`). Output no parser
/// could place, from a tool that said it failed, still reaches the reader,
/// unlocated.
pub fn diagnostics_for(
    ws: &str,
    tool: &str,
    list_only: bool,
    rc: i32,
    out: &str,
    err: &str,
) -> Vec<Diagnostic> {
    let mut rows = Vec::new();
    let eslint_docs = (tool == "eslint")
        .then(|| json_stream(out))
        .flatten()
        .filter(|d| d.last().is_some_and(|v| present(Some(v)).is_some()));
    if list_only {
        for f in out.lines().filter(|f| !f.is_empty()) {
            rows.push(emit(ws, tool, f, "", "would be reformatted"));
        }
        // A file it could not parse is reported on stderr instead.
        rows.extend(parse_located(ws, tool, err));
    } else if let Some(docs) = eslint_docs {
        rows = parse_eslint(ws, tool, &docs);
    } else if tool == "rustfmt" {
        rows = parse_rustfmt(ws, tool, out);
    } else {
        rows = parse_located(ws, tool, &format!("{out}\n{err}"));
    }
    if rows.is_empty() && rc != 0 {
        let said = if err.is_empty() { out } else { err };
        let said = head_bytes(said, 300);
        let said = said.trim_end_matches('\n');
        rows.push(emit(ws, tool, "", "", &format!("exited {rc}: {said}")));
    }
    rows
}

/// Runs the node over `stdin`, the mechanical contract, and returns its
/// stdout JSON.
pub fn run(stdin: &str, lint_recipe: &str, r: &dyn Runner) -> Result<String, Fail> {
    let output = |rows: Vec<Diagnostic>| json!({"diagnostics": rows, "triggers": []}).to_string();
    if !r.on_path(lint_recipe) {
        eprintln!("changed-lint: {lint_recipe} is not on PATH, so nothing is linted");
        return Ok(output(vec![]));
    }
    let contract: Value = serde_json::from_str(stdin).unwrap_or(Value::Null);
    let Some(Value::String(ws)) = present(contract.get("workspace")) else {
        return Err(die("stdin is not a mechanical contract"));
    };
    if !Path::new(ws).is_dir() {
        return Err(die(&format!("workspace {ws} is not a directory")));
    }
    let scratch = TempDir::new("changed-lint").map_err(|_| die("no temp directory"))?;

    // Deleted files have nothing to lint.
    let files: Vec<String> = contract
        .get("changed_files")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|f| Path::new(&format!("{ws}/{f}")).is_file())
        .map(String::from)
        .collect();
    if files.is_empty() {
        return Ok(output(vec![]));
    }
    let ws = ws.strip_suffix('/').unwrap_or(ws);

    let mut args = vec!["--repo".to_string(), ws.to_string(), "--".to_string()];
    args.extend(files);
    let plan = match r.run(lint_recipe, &args, &Opts::default()) {
        Ok(c) if c.ok() => c.stdout,
        _ => return Err(die("lint-recipe failed")),
    };

    let mut path = std::env::var("PATH").unwrap_or_default();
    if let Some(dir) = Path::new(lint_recipe)
        .parent()
        .filter(|d| lint_recipe.contains('/') && !d.as_os_str().is_empty())
    {
        path = format!("{}:{path}", dir.display());
    }
    let env = [
        (
            "RUFF_CACHE_DIR",
            scratch.path().join("ruff").display().to_string(),
        ),
        ("PATH", path),
    ];

    let mut rows = Vec::new();
    for line in plan.lines().filter(|l| !l.is_empty()) {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let tool = text_of(entry.get("tool").unwrap_or(&Value::Null));
        let list_only = match entry.get("fail_on_output") {
            Some(Value::Bool(b)) => *b,
            Some(Value::String(s)) => s == "true",
            _ => false,
        };
        let argv: Vec<String> = entry
            .get("argv")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(text_of).collect())
            .unwrap_or_default();
        if argv.is_empty() {
            continue;
        }
        let argv = adjust_argv(&tool, &argv);

        let opts = Opts {
            cwd: Some(Path::new(ws)),
            env: env.iter().map(|(k, v)| (*k, v.clone())).collect(),
            ..Opts::default()
        };
        let (rc, out, err) = match r.run(&argv[0], &argv[1..], &opts) {
            Ok(c) => (c.code, c.stdout, c.stderr),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (
                127,
                String::new(),
                format!("{}: command not found", argv[0]),
            ),
            Err(e) => (126, String::new(), format!("{}: {e}", argv[0])),
        };
        let (out, err) = (out.trim_end_matches('\n'), err.trim_end_matches('\n'));
        rows.extend(diagnostics_for(ws, &tool, list_only, rc, out, err));
    }
    Ok(output(rows))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::stub::{ok, Script, Stub};
    use crate::builtin::Cmd;

    fn summary(rows: &[Diagnostic]) -> Vec<(String, Option<String>, Option<u64>, String)> {
        rows.iter()
            .map(|d| (d.tool.clone(), d.file.clone(), d.line, d.message.clone()))
            .collect()
    }

    fn row(
        t: &str,
        f: Option<&str>,
        l: Option<u64>,
        m: &str,
    ) -> (String, Option<String>, Option<u64>, String) {
        (t.into(), f.map(String::from), l, m.into())
    }

    #[test]
    fn located_lines_become_rows_and_the_rest_is_ignored() {
        let text = "src/a.go:12:5: unused variable x\nnoise\nb.go:3: bare line\n/w/c.go:9: abs";
        let rows = parse_located("/w", "golangci-lint", text);
        assert_eq!(
            summary(&rows),
            [
                row(
                    "golangci-lint",
                    Some("src/a.go"),
                    Some(12),
                    "unused variable x"
                ),
                row("golangci-lint", Some("b.go"), Some(3), "bare line"),
                row("golangci-lint", Some("c.go"), Some(9), "abs"),
            ]
        );
    }

    #[test]
    fn tools_are_asked_for_the_output_that_carries_a_line_number() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            adjust_argv("shellcheck", &v(&["shellcheck", "a.sh"])),
            v(&["shellcheck", "-f", "gcc", "a.sh"])
        );
        assert_eq!(
            adjust_argv("ruff", &v(&["ruff", "check", "a.py"])),
            v(&["ruff", "check", "--output-format=concise", "a.py"])
        );
        assert_eq!(
            adjust_argv("ruff", &v(&["ruff"])),
            v(&["ruff", "--output-format=concise"])
        );
        assert_eq!(
            adjust_argv("eslint", &v(&["npx", "eslint", "a.js"])),
            v(&["npx", "eslint", "a.js", "--format", "json"])
        );
        assert_eq!(
            adjust_argv("gofmt", &v(&["gofmt", "-l"])),
            v(&["gofmt", "-l"])
        );
    }

    #[test]
    fn a_list_only_tool_flags_each_path_it_prints() {
        let rows = diagnostics_for(
            "/w",
            "gofmt",
            true,
            0,
            "a.go\nb/c.go",
            "bad.go:2:1: expected }",
        );
        assert_eq!(
            summary(&rows),
            [
                row("gofmt", Some("a.go"), None, "would be reformatted"),
                row("gofmt", Some("b/c.go"), None, "would be reformatted"),
                row("gofmt", Some("bad.go"), Some(2), "expected }"),
            ]
        );
    }

    #[test]
    fn eslint_json_gives_one_row_per_message_with_its_rule() {
        let out = json!([
            {"filePath": "/w/a.js", "messages": [
                {"line": 4, "message": "no-unused", "ruleId": "no-unused-vars"},
                {"line": 5, "message": "parse error", "ruleId": null},
            ]},
            {"filePath": "/w/b.js", "messages": []},
        ])
        .to_string();
        let rows = diagnostics_for("/w", "eslint", false, 1, &out, "");
        assert_eq!(
            summary(&rows),
            [
                row(
                    "eslint",
                    Some("a.js"),
                    Some(4),
                    "no-unused (no-unused-vars)"
                ),
                row("eslint", Some("a.js"), Some(5), "parse error"),
            ]
        );
        let clean = diagnostics_for("/w", "eslint", false, 0, "[]", "");
        assert!(clean.is_empty());
    }

    #[test]
    fn rustfmt_diffs_are_located_in_either_spelling() {
        let out = "Diff in /w/src/a.rs:12:\n-x\n+y\nDiff in /w/src/b.rs at line 30:\n";
        let rows = diagnostics_for("/w", "rustfmt", false, 1, out, "");
        assert_eq!(
            summary(&rows),
            [
                row(
                    "rustfmt",
                    Some("src/a.rs"),
                    Some(12),
                    "would be reformatted"
                ),
                row(
                    "rustfmt",
                    Some("src/b.rs"),
                    Some(30),
                    "would be reformatted"
                ),
            ]
        );
    }

    #[test]
    fn a_failure_nothing_could_place_is_reported_unlocated() {
        let rows = diagnostics_for("/w", "mypy", false, 2, "", "config error\nsecond line\n");
        assert_eq!(
            summary(&rows),
            [row(
                "mypy",
                None,
                None,
                "exited 2: config error second line"
            )]
        );
        let from_out = diagnostics_for("/w", "mypy", false, 1, "only stdout", "");
        assert_eq!(from_out[0].message, "exited 1: only stdout");
        assert!(diagnostics_for("/w", "mypy", false, 0, "chatter", "").is_empty());
        let long = "x".repeat(500);
        let cut = diagnostics_for("/w", "mypy", false, 1, "", &long);
        assert_eq!(cut[0].message.len(), "exited 1: ".len() + 300);
    }

    #[test]
    fn messages_are_flattened_and_stripped() {
        let d = emit("/w", "t", "./x", "7", "a\tb\nc  ");
        assert_eq!(
            (d.file.as_deref(), d.line, d.message.as_str()),
            (Some("x"), Some(7), "a b c")
        );
        let none = emit("/w", "t", "", "", "m");
        assert_eq!((none.file, none.line), (None, None));
    }

    fn workspace() -> std::path::PathBuf {
        let dir = TempDir::new("changed-lint-test").unwrap();
        let path = dir.path().to_path_buf();
        std::mem::forget(dir);
        std::fs::write(path.join("a.go"), "package a\n").unwrap();
        path
    }

    #[test]
    fn without_lint_recipe_the_node_reports_nothing() {
        let s = Stub::new(&[], Box::new(|_, _, _| ok("")));
        assert_eq!(
            run("{}", "lint-recipe", &s).unwrap(),
            r#"{"diagnostics":[],"triggers":[]}"#
        );
        assert!(s.calls.borrow().is_empty());
    }

    #[test]
    fn a_contract_without_a_real_workspace_fails_the_node() {
        let s = Stub::new(&["lint-recipe"], Box::new(|_, _, _| ok("")));
        let e = run("nonsense", "lint-recipe", &s).unwrap_err();
        assert_eq!(
            (e.code, e.message.as_str()),
            (2, "changed-lint: stdin is not a mechanical contract")
        );
        let e = run(r#"{"workspace":"/no/such/dir"}"#, "lint-recipe", &s).unwrap_err();
        assert!(e.message.contains("is not a directory"), "{}", e.message);
    }

    #[test]
    fn the_plan_is_run_in_the_workspace_and_its_findings_collected() {
        let ws = workspace();
        let script: Script = Box::new(|prog, args, opts| match prog {
            "lint-recipe" => {
                assert_eq!(&args[0..2], ["--repo", args[1].as_str()]);
                assert_eq!(&args[2..], ["--", "a.go"], "deleted files are left out");
                ok(&format!(
                    "{}\n\n{}\nnot json\n",
                    json!({"tool": "gofmt", "fail_on_output": true, "argv": ["gofmt", "-l", "a.go"]}),
                    json!({"tool": "vet", "fail_on_output": false, "argv": ["go", "vet", "./..."]}),
                ))
            }
            "gofmt" => {
                assert!(opts.cwd.is_some());
                ok("a.go\n")
            }
            "go" => Ok(Cmd {
                code: 1,
                stdout: String::new(),
                stderr: "a.go:3:1: composite literal uses unkeyed fields\n".into(),
            }),
            _ => Err(std::io::ErrorKind::NotFound.into()),
        });
        let s = Stub::new(&["lint-recipe"], script);
        let contract = json!({"workspace": ws, "changed_files": ["a.go", "gone.go"]}).to_string();
        let out: Value = serde_json::from_str(&run(&contract, "lint-recipe", &s).unwrap()).unwrap();
        assert_eq!(
            out,
            json!({"diagnostics": [
                {"tool": "gofmt", "file": "a.go", "line": null, "message": "would be reformatted"},
                {"tool": "vet", "file": "a.go", "line": 3, "message": "composite literal uses unkeyed fields"},
            ], "triggers": []})
        );
        let _ = std::fs::remove_dir_all(ws);
    }

    #[test]
    fn a_tool_that_is_not_installed_is_reported_and_a_failing_recipe_fails_the_node() {
        let ws = workspace();
        let plan = json!({"tool": "ghost", "fail_on_output": false, "argv": ["ghost", "a.go"]})
            .to_string();
        let script: Script = Box::new(move |prog, _, _| match prog {
            "lint-recipe" => ok(&plan),
            _ => Err(std::io::ErrorKind::NotFound.into()),
        });
        let s = Stub::new(&["lint-recipe"], script);
        let contract = json!({"workspace": ws, "changed_files": ["a.go"]}).to_string();
        let out: Value = serde_json::from_str(&run(&contract, "lint-recipe", &s).unwrap()).unwrap();
        assert_eq!(
            out["diagnostics"][0]["message"],
            "exited 127: ghost: command not found"
        );

        let broken = Stub::new(
            &["lint-recipe"],
            Box::new(|_, _, _| crate::builtin::stub::fail(3, "x")),
        );
        let e = run(&contract, "lint-recipe", &broken).unwrap_err();
        assert_eq!(e.message, "changed-lint: lint-recipe failed");
        let _ = std::fs::remove_dir_all(ws);
    }

    #[test]
    fn nothing_to_lint_never_calls_the_recipe() {
        let ws = workspace();
        let s = Stub::new(&["lint-recipe"], Box::new(|_, _, _| ok("boom")));
        let contract = json!({"workspace": ws, "changed_files": ["gone.go"]}).to_string();
        assert_eq!(
            run(&contract, "lint-recipe", &s).unwrap(),
            r#"{"diagnostics":[],"triggers":[]}"#
        );
        assert!(s.calls.borrow().is_empty());
        let _ = std::fs::remove_dir_all(ws);
    }
}
