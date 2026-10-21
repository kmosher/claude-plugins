//! `builtin:codex-lens`: a generator node backed by Codex. Runs `codex exec`
//! on the node's prompt and maps its answer into the finding schema.
//!
//! stdin is the command-provider envelope `{node, workspace, prompt, schema}`;
//! stdout is generator output `{findings, upstream_reading, meta}`. A finding
//! Codex cannot anchor to a file and line is kept at confidence "low". A
//! missing or unauthenticated `codex` yields no findings and succeeds, with the
//! reason on stderr and in `meta`.

use super::{head_bytes, json_stream, present, Fail, Opts, Runner, TempDir};
use serde_json::{json, Map, Value};

fn log(msg: &str) {
    eprintln!("codex-lens: {msg}");
}

/// No findings, and why.
fn empty(why: &str) -> Result<String, Fail> {
    log(why);
    Ok(json!({"findings": [], "upstream_reading": [], "meta": why}).to_string())
}

/// The prompt Codex is given: the node's own, the two extra fields every
/// finding must carry, and the finding schema. Codex's own `--output-schema`
/// wants a stricter dialect than the harness constrains Claude to, so the
/// schema travels in the prompt.
pub fn build_prompt(prompt: &str, schema: &Value) -> String {
    format!(
        "{prompt}\n\
\n\
Each finding must also carry two non-empty string fields:\n\
- invariant: one sentence, the property that should hold and is violated at the cited site (for a bug, the broken guarantee).\n\
- verify: how an implementer confirms the fix: a named existing test to run, a test to write (what it asserts; it fails before the fix and passes after), a command, or the literal \"by inspection\" when no automated check is sensible.\n\
\n\
Answer with only a single JSON object matching this JSON Schema, with no prose and no code fence:\n\
{schema}\n"
    )
}

/// The JSON in Codex's final message: code-fence lines dropped, then
/// everything from the first line that holds a `{`, so a sentence before the
/// object is harmless. None when it is not valid JSON.
pub fn extract_answer(last: &str) -> Result<Value, &'static str> {
    let kept: Vec<&str> = last.lines().filter(|l| !l.starts_with("```")).collect();
    let Some(first) = kept.iter().position(|l| l.contains('{')) else {
        return Err("codex answered without JSON");
    };
    match json_stream(&kept[first..].join("\n")) {
        Some(mut docs) if !docs.is_empty() => Ok(docs.swap_remove(0)),
        _ => Err("codex answer was not valid JSON"),
    }
}

fn text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

fn one_of(v: Option<&Value>, allowed: &[&str], otherwise: &str) -> String {
    match v {
        Some(Value::String(s)) if allowed.contains(&s.as_str()) => s.clone(),
        _ => otherwise.into(),
    }
}

/// jq's `.[]`: an array's items or an object's values; a scalar is an error.
fn each(v: &Value) -> Result<Vec<&Value>, Fail> {
    match v {
        Value::Array(a) => Ok(a.iter().collect()),
        Value::Object(o) => Ok(o.values().collect()),
        _ => Err(Fail {
            code: 5,
            message: "codex-lens: the answer's findings are not a list".into(),
        }),
    }
}

/// Normalizes a finding: every schema field present and well-typed, plus
/// `invariant` and `verify`, with unanchored findings at low confidence.
fn finding(f: &Map<String, Value>) -> Value {
    let get = |k: &str| f.get(k);
    let file = match get("file") {
        Some(Value::String(s)) if !s.is_empty() => Value::String(s.clone()),
        _ => Value::Null,
    };
    let line = match get("line").and_then(Value::as_f64) {
        Some(n) if n >= 0.0 => json!(n.floor() as u64),
        _ => Value::Null,
    };
    let why = text(get("why_it_matters"));
    let invariant = match text(get("invariant")) {
        s if s.is_empty() => why.clone(),
        s => s,
    };
    let verify = match text(get("verify")) {
        s if s.is_empty() => "by inspection".to_string(),
        s => s,
    };
    let mut confidence = one_of(get("confidence"), &["low", "medium", "high"], "low");
    if file.is_null() || line.is_null() {
        confidence = "low".into();
    }
    let mut out = f.clone();
    out.insert("file".into(), file);
    out.insert("line".into(), line);
    out.insert(
        "severity".into(),
        one_of(get("severity"), &["P0", "P1", "P2", "P3"], "P3").into(),
    );
    out.insert("confidence".into(), confidence.into());
    out.insert("category".into(), text(get("category")).into());
    out.insert("description".into(), text(get("description")).into());
    out.insert("why_it_matters".into(), why.into());
    out.insert("recommendation".into(), text(get("recommendation")).into());
    out.insert("invariant".into(), invariant.into());
    out.insert("verify".into(), verify.into());
    Value::Object(out)
}

/// Codex's parsed answer as generator output. The answer may be the findings
/// list itself or an object holding `findings`, `upstream_reading` and `meta`.
pub fn map_answer(root: &Value) -> Result<Value, Fail> {
    let findings = match root {
        Value::Array(_) => Some(root),
        other => present(other.get("findings")),
    };
    let mut kept = Vec::new();
    if let Some(list) = findings {
        for item in each(list)? {
            let Value::Object(f) = item else { continue };
            if text(f.get("description")).is_empty() {
                continue;
            }
            kept.push(finding(f));
        }
    }
    let reading: Vec<Value> = present(root.get("upstream_reading"))
        .map(|r| match r {
            Value::Array(a) => a.iter().collect(),
            Value::Object(o) => o.values().collect::<Vec<_>>(),
            _ => vec![],
        })
        .unwrap_or_default()
        .into_iter()
        .filter(|r| {
            r.get("path").is_some_and(Value::is_string)
                && r.get("told_me").is_some_and(Value::is_string)
        })
        .cloned()
        .collect();
    Ok(json!({
        "findings": kept,
        "upstream_reading": reading,
        "meta": text(present(root.get("meta"))),
    }))
}

/// Runs the node over `stdin`, the command-provider envelope, and returns its
/// stdout JSON.
pub fn run(stdin: &str, r: &dyn Runner) -> Result<String, Fail> {
    let Ok(scratch) = TempDir::new("codex-lens") else {
        log("no temp directory");
        return Ok(json!({"findings": []}).to_string());
    };
    if !r.on_path("codex") {
        return empty("codex is not on PATH");
    }
    match r.run(
        "codex",
        &["login".into(), "status".into()],
        &Opts::default(),
    ) {
        Ok(c) if c.ok() => {}
        other => {
            let said = other
                .map(|c| format!("{}{}", c.stdout, c.stderr))
                .unwrap_or_default();
            let last = said.lines().last().unwrap_or("");
            return empty(&format!("codex is not authenticated: {last}"));
        }
    }

    let envelope: Value = serde_json::from_str(stdin).unwrap_or(Value::Null);
    let Some(Value::String(ws)) = present(envelope.get("workspace")) else {
        return empty("stdin is not an envelope");
    };
    let Some(Value::String(prompt)) = present(envelope.get("prompt")) else {
        return empty("envelope has no prompt");
    };
    let schema = envelope.get("schema").cloned().unwrap_or(Value::Null);

    let last_path = scratch.path().join("last");
    let args: Vec<String> = [
        "exec",
        "--ephemeral",
        "--skip-git-repo-check",
        "-s",
        "read-only",
        "-C",
        ws,
        "-o",
        &last_path.display().to_string(),
        "-",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let asked = build_prompt(prompt, &schema);
    match r.run(
        "codex",
        &args,
        &Opts {
            stdin: Some(&asked),
            ..Opts::default()
        },
    ) {
        Ok(c) if c.ok() => {}
        other => {
            let stderr = other.map(|c| c.stderr).unwrap_or_default();
            let lines: Vec<&str> = stderr.lines().collect();
            let tail = lines[lines.len().saturating_sub(3)..]
                .iter()
                .map(|l| format!("{l} "))
                .collect::<String>();
            return empty(&format!("codex exec failed: {}", head_bytes(&tail, 300)));
        }
    }
    let last = std::fs::read_to_string(&last_path).unwrap_or_default();
    if last.is_empty() {
        return empty("codex produced no final message");
    }
    match extract_answer(&last) {
        Ok(root) => map_answer(&root).map(|v| v.to_string()),
        Err(why) => empty(why),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::stub::{fail, ok, Script, Stub};
    use crate::builtin::Cmd;

    #[test]
    fn the_prompt_carries_the_extra_fields_and_the_schema() {
        let p = build_prompt("Review this.", &json!({"type": "object"}));
        assert!(p.starts_with(
            "Review this.\n\nEach finding must also carry two non-empty string fields:"
        ));
        assert!(p.contains("- invariant: one sentence"));
        assert!(p.contains("- verify: how an implementer confirms the fix"));
        assert!(p.ends_with("no code fence:\n{\"type\":\"object\"}\n"));
    }

    #[test]
    fn the_answer_is_found_after_prose_and_fences() {
        let last = "Here is my review.\n```json\n{\"findings\": []}\n```\n";
        assert_eq!(extract_answer(last).unwrap(), json!({"findings": []}));
        assert_eq!(
            extract_answer("no object here").unwrap_err(),
            "codex answered without JSON"
        );
        assert_eq!(
            extract_answer("{ not json").unwrap_err(),
            "codex answer was not valid JSON"
        );
    }

    #[test]
    fn findings_are_normalised_and_unanchored_ones_demoted() {
        let root = json!({"findings": [
            {"file": "a.rs", "line": 12.7, "severity": "P1", "confidence": "high",
             "category": "bug", "description": "d", "why_it_matters": "w",
             "recommendation": "r", "invariant": "i", "verify": "cargo test", "extra": 1},
            {"file": "b.rs", "line": 3, "severity": "urgent", "confidence": "certain",
             "description": "d2", "why_it_matters": "because"},
            {"file": "", "line": 4, "confidence": "high", "description": "unanchored"},
            {"file": "c.rs", "line": -1, "confidence": "medium", "description": "negative line"},
            {"description": ""},
            "not an object"
        ], "meta": "m"});
        let out = map_answer(&root).unwrap();
        let f = out["findings"].as_array().unwrap();
        assert_eq!(f.len(), 4);
        assert_eq!(
            (
                &f[0]["line"],
                &f[0]["confidence"],
                &f[0]["verify"],
                &f[0]["extra"]
            ),
            (&json!(12), &json!("high"), &json!("cargo test"), &json!(1))
        );
        assert_eq!(
            (
                &f[1]["severity"],
                &f[1]["confidence"],
                &f[1]["invariant"],
                &f[1]["verify"],
                &f[1]["recommendation"]
            ),
            (
                &json!("P3"),
                &json!("low"),
                &json!("because"),
                &json!("by inspection"),
                &json!("")
            )
        );
        assert_eq!(
            (&f[2]["file"], &f[2]["confidence"]),
            (&Value::Null, &json!("low"))
        );
        assert_eq!(
            (&f[3]["line"], &f[3]["confidence"]),
            (&Value::Null, &json!("low"))
        );
        assert_eq!(out["meta"], "m");
        assert_eq!(out["upstream_reading"], json!([]));
    }

    #[test]
    fn a_bare_list_is_the_findings_and_upstream_reading_keeps_only_well_formed_entries() {
        let out = map_answer(&json!([{"description": "d", "file": "a", "line": 1}])).unwrap();
        assert_eq!(out["findings"].as_array().unwrap().len(), 1);
        assert_eq!(out["meta"], "");

        let root = json!({"upstream_reading": [
            {"path": "p", "told_me": "t"}, {"path": "p"}, {"path": 1, "told_me": "t"}
        ]});
        let out = map_answer(&root).unwrap();
        assert_eq!(
            out["upstream_reading"],
            json!([{"path": "p", "told_me": "t"}])
        );
        assert_eq!(out["findings"], json!([]));
    }

    #[test]
    fn findings_that_are_not_a_list_fail_the_node() {
        let e = map_answer(&json!({"findings": "none"})).unwrap_err();
        assert_eq!(e.code, 5);
        assert!(map_answer(&json!({"findings": null})).is_ok());
    }

    fn envelope() -> String {
        json!({"node": "n", "workspace": "/w", "prompt": "Review.", "schema": {"type": "object"}})
            .to_string()
    }

    fn meta(out: &str) -> String {
        let v: Value = serde_json::from_str(out).unwrap();
        assert_eq!(v["findings"], json!([]));
        v["meta"].as_str().unwrap().to_string()
    }

    #[test]
    fn a_missing_or_logged_out_codex_means_no_findings_not_a_failure() {
        let s = Stub::new(&[], Box::new(|_, _, _| ok("")));
        assert_eq!(meta(&run(&envelope(), &s).unwrap()), "codex is not on PATH");

        let script: Script = Box::new(|_, _, _| {
            Ok(Cmd {
                code: 1,
                stdout: String::new(),
                stderr: "Not logged in\nrun codex login\n".into(),
            })
        });
        let s = Stub::new(&["codex"], script);
        assert_eq!(
            meta(&run(&envelope(), &s).unwrap()),
            "codex is not authenticated: run codex login"
        );
        assert_eq!(s.calls.borrow().len(), 1);
    }

    #[test]
    fn a_bad_envelope_is_reported_in_meta() {
        let s = Stub::new(&["codex"], Box::new(|_, _, _| ok("")));
        assert_eq!(
            meta(&run("nonsense", &s).unwrap()),
            "stdin is not an envelope"
        );
        assert_eq!(
            meta(&run(r#"{"workspace":"/w"}"#, &s).unwrap()),
            "envelope has no prompt"
        );
    }

    #[test]
    fn a_failed_exec_reports_the_tail_of_its_stderr() {
        let script: Script = Box::new(|_, args, _| {
            if args.first().map(String::as_str) == Some("login") {
                return ok("Logged in");
            }
            fail(1, "one\ntwo\nthree\nfour\n")
        });
        let s = Stub::new(&["codex"], script);
        assert_eq!(
            meta(&run(&envelope(), &s).unwrap()),
            "codex exec failed: two three four "
        );
    }

    #[test]
    fn a_successful_exec_is_mapped_and_gets_the_prompt_on_stdin() {
        let script: Script = Box::new(|_, args, opts| {
            if args.first().map(String::as_str) == Some("login") {
                return ok("Logged in");
            }
            assert!(args.windows(2).any(|w| w == ["-C", "/w"]));
            assert!(args.windows(2).any(|w| w == ["-s", "read-only"]));
            assert!(opts.stdin.unwrap().starts_with("Review."));
            let o = args.iter().position(|a| a == "-o").unwrap();
            std::fs::write(
                &args[o + 1],
                "```json\n{\"findings\":[{\"file\":\"a.rs\",\"line\":2,\"description\":\"d\"}]}\n```\n",
            )
            .unwrap();
            ok("")
        });
        let s = Stub::new(&["codex"], script);
        let out: Value = serde_json::from_str(&run(&envelope(), &s).unwrap()).unwrap();
        assert_eq!(out["findings"][0]["file"], "a.rs");
        assert_eq!(out["findings"][0]["confidence"], "low");
        assert_eq!(out["findings"][0]["verify"], "by inspection");
    }

    #[test]
    fn exec_that_writes_no_message_is_no_findings() {
        let script: Script = Box::new(|_, _, _| ok(""));
        let s = Stub::new(&["codex"], script);
        assert_eq!(
            meta(&run(&envelope(), &s).unwrap()),
            "codex produced no final message"
        );
    }
}
