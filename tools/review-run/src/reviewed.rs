//! What each checkout last had a complete review of, so `--if-changed` can
//! decline to review the same change twice.
//!
//! The fingerprint covers what the run reviews and what reviews it. The first
//! is the base, head and diff, uncommitted changes included, plus the contents
//! of every untracked file git does not ignore: nodes read the checkout, so a
//! new or edited untracked file is fresh work too, while one that was already
//! there unchanged is not. The second is this
//! binary's version, the graph file, each node as loaded after overrides (its
//! model, effort and the contents of its prompt files), and the convention
//! files the nodes are handed, tracked or not. Editing a lens or adding a
//! REVIEW.md therefore counts as a new review, as does any flag that changes
//! what nodes run or see. The pull request's description is left out: editing
//! it alone does not ask for a re-review.
//!
//! One record per checkout and graph file, overwritten by each complete run:
//! the question is only ever "is this the change reviewed last?". Records live
//! in a cache and are written best-effort; a missing or unreadable one means
//! review.

use crate::conventions::Conventions;
use crate::graph::Graph;
use crate::run::Target;
use serde_json::{json, Value};
use std::fmt::Write as _;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub struct Review<'a> {
    pub graph_file: &'a Path,
    /// The graph as it will run, `--model` and `--effort` applied.
    pub graph: &'a Graph,
    pub skip: &'a [String],
    pub no_prewarm: bool,
    pub no_gh: bool,
}

fn hash(text: &str) -> Result<String, String> {
    let mut child = Command::new("git")
        .args(["hash-object", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("git hash-object: {e}"))?;
    child
        .stdin
        .take()
        .expect("piped")
        .write_all(text.as_bytes())
        .map_err(|e| format!("git hash-object: {e}"))?;
    let out = child
        .wait_with_output()
        .map_err(|e| format!("git hash-object: {e}"))?;
    if !out.status.success() {
        return Err("git hash-object failed".into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn graph_id(graph_file: &Path) -> String {
    graph_file
        .canonicalize()
        .unwrap_or_else(|_| graph_file.to_path_buf())
        .display()
        .to_string()
}

/// One `<content hash> <path>` line per untracked file git does not ignore.
/// git lists a nested repository as a bare directory; its files are those its
/// own git tracks or leaves unignored, so its `.gitignore` applies inside it.
/// Any file that cannot be read is an error, never a stand-in line: a stand-in
/// would repeat run after run and match itself.
///
/// The hash is std's SipHash, which is not promised stable across Rust
/// releases. A toolchain change can only make a record miss, which costs one
/// extra review.
fn untracked(workspace: &Path) -> Result<String, String> {
    use std::hash::{Hash, Hasher};
    use std::os::unix::ffi::OsStrExt;
    fn ls(repo: &Path, args: &[&str]) -> Result<Vec<PathBuf>, String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["ls-files", "-z"])
            .args(args)
            .output()
            .map_err(|e| format!("git ls-files in {}: {e}", repo.display()))?;
        if !out.status.success() {
            return Err(format!(
                "git ls-files in {}: {}",
                repo.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(out
            .stdout
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| repo.join(std::ffi::OsStr::from_bytes(p)))
            .collect())
    }
    fn expand(path: PathBuf, files: &mut Vec<PathBuf>) -> Result<(), String> {
        let is_dir = std::fs::symlink_metadata(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?
            .is_dir();
        if !is_dir {
            files.push(path);
            return Ok(());
        }
        // Only a nested repository is listed as a directory.
        for p in ls(&path, &["--cached", "--others", "--exclude-standard"])? {
            expand(p, files)?;
        }
        Ok(())
    }
    let mut files = Vec::new();
    for p in ls(workspace, &["--others", "--exclude-standard"])? {
        expand(p, &mut files)?;
    }
    let mut s = String::new();
    for f in files {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        // A symlink is hashed as its target path: the link is what is untracked.
        let bytes = match std::fs::read_link(&f) {
            Ok(target) => target.as_os_str().as_bytes().to_vec(),
            Err(_) => std::fs::read(&f).map_err(|e| format!("{}: {e}", f.display()))?,
        };
        bytes.hash(&mut h);
        let rel = f.strip_prefix(workspace).unwrap_or(&f);
        let _ = writeln!(s, "{:016x} {:?}", h.finish(), rel);
    }
    Ok(s)
}

fn file_or_error(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| format!("<unreadable: {e}>"))
}

pub fn fingerprint(t: &Target, r: &Review) -> Result<String, String> {
    let mut skip = r.skip.to_vec();
    skip.sort();
    let mut s = format!(
        "review-run {}\nbase {}\nhead {}\nuncommitted {}\nskip {}\nno_prewarm {}\nno_gh {}\n\ngraph\n{}\n",
        env!("CARGO_PKG_VERSION"),
        t.base_sha,
        t.head_sha,
        t.uncommitted,
        skip.join(","),
        r.no_prewarm,
        r.no_gh,
        file_or_error(r.graph_file)
    );
    for n in &r.graph.nodes {
        let _ = writeln!(
            s,
            "node {} model {:?} effort {:?}",
            n.name, n.model, n.effort
        );
        for p in n.system_prompt.iter().chain(&n.lens) {
            let _ = writeln!(s, "file {}\n{}", p.display(), file_or_error(p));
        }
    }
    for f in Conventions::collect(&t.workspace, &t.changed_files).files {
        let _ = writeln!(s, "convention {} scope {:?}\n{}", f.path, f.scope, f.text);
    }
    let _ = write!(
        s,
        "\nuntracked\n{}\ndiff\n{}",
        untracked(&t.workspace)?,
        t.diff
    );
    hash(&s)
}

/// `None` when there is no home to keep records under: then nothing is
/// recorded and nothing counts as reviewed.
fn default_root() -> Option<PathBuf> {
    if let Ok(d) = std::env::var("REVIEW_RUN_REVIEWED_DIR") {
        if !d.is_empty() {
            return Some(PathBuf::from(d));
        }
    }
    if let Ok(x) = std::env::var("XDG_CACHE_HOME") {
        if !x.is_empty() {
            return Some(PathBuf::from(x).join("review-run/reviewed"));
        }
    }
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() => Some(PathBuf::from(h).join(".cache/review-run/reviewed")),
        _ => None,
    }
}

fn record_path(root: &Path, t: &Target, r: &Review) -> Result<PathBuf, String> {
    let key = hash(&format!(
        "{}\n{}",
        t.workspace.display(),
        graph_id(r.graph_file)
    ))?;
    Ok(root.join(format!("{key}.json")))
}

/// The record of the last complete review, when it reviewed exactly this change.
pub fn already(t: &Target, r: &Review) -> Option<Value> {
    already_in(&default_root()?, t, r)
}

fn already_in(root: &Path, t: &Target, r: &Review) -> Option<Value> {
    let text = std::fs::read_to_string(record_path(root, t, r).ok()?).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    (v["fingerprint"].as_str()? == fingerprint(t, r).ok()?).then_some(v)
}

/// Remembers a complete review of this change. `fingerprint` must be taken
/// before the run, so that a lens edited while it ran is not recorded as
/// reviewed. Failure is reported, never fatal.
pub fn record(t: &Target, r: &Review, fingerprint: &str, bundle: &Path) {
    let Some(root) = default_root() else {
        eprintln!("review-run: HOME is unset, so this review is not recorded for --if-changed");
        return;
    };
    if let Err(e) = record_in(&root, t, r, fingerprint, bundle) {
        eprintln!("review-run: could not record this review for --if-changed: {e}");
    }
}

fn record_in(
    root: &Path,
    t: &Target,
    r: &Review,
    fingerprint: &str,
    bundle: &Path,
) -> Result<(), String> {
    let path = record_path(root, t, r)?;
    std::fs::create_dir_all(root).map_err(|e| e.to_string())?;
    let v = json!({
        "fingerprint": fingerprint,
        "workspace": t.workspace,
        "graph": graph_id(r.graph_file),
        "base_sha": t.base_sha,
        "head_sha": t.head_sha,
        "uncommitted": t.uncommitted,
        "reviewed_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "bundle": bundle,
    });
    // Per process, so two runs finishing together cannot interleave one file.
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(
        &tmp,
        serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(ws: &Path, head: &str, diff: &str, uncommitted: bool) -> Target {
        Target {
            workspace: ws.to_path_buf(),
            base_sha: "b".repeat(40),
            head_sha: head.to_string(),
            branch: "feature/x".into(),
            owner_repo: None,
            uncommitted,
            diff: diff.to_string(),
            changed_files: vec![],
            pr_number: None,
            pr_body: None,
        }
    }

    const GRAPH: &str =
        "[[node]]\nname = \"code\"\nkind = \"generator\"\nsystem_prompt = [\"lens.md\"]\n";

    #[test]
    fn only_the_exact_change_and_review_last_run_counts_as_reviewed() {
        let dir = std::env::temp_dir().join(format!("review-run-reviewed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.join("records");
        let graph_file = dir.join("spot.toml");
        std::fs::write(&graph_file, GRAPH).unwrap();
        std::fs::write(dir.join("lens.md"), "find bugs\n").unwrap();
        let graph = Graph::load(&graph_file).unwrap();
        let none: Vec<String> = vec![];
        let r = Review {
            graph_file: &graph_file,
            graph: &graph,
            skip: &none,
            no_prewarm: false,
            no_gh: false,
        };
        let ws = dir.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        assert!(Command::new("git")
            .args(["-C", ws.to_str().unwrap(), "init", "-q"])
            .status()
            .unwrap()
            .success());
        std::fs::write(ws.join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(ws.join("notes.txt"), "already here\n").unwrap();
        let a = "a".repeat(40);

        let t = target(&ws, &a, "diff one\n", false);
        assert!(already_in(&root, &t, &r).is_none(), "nothing recorded yet");
        record_in(
            &root,
            &t,
            &r,
            &fingerprint(&t, &r).unwrap(),
            Path::new("/bundle"),
        )
        .unwrap();
        assert_eq!(already_in(&root, &t, &r).unwrap()["bundle"], "/bundle");

        // Fresh work, committed or not, is a different change.
        assert!(already_in(
            &root,
            &target(&ws, &"c".repeat(40), "diff one\n", false),
            &r
        )
        .is_none());
        assert!(already_in(&root, &target(&ws, &a, "diff one\nmore\n", true), &r).is_none());
        // So is another checkout of the same commit.
        assert!(already_in(
            &root,
            &target(&dir.join("other"), &a, "diff one\n", false),
            &r
        )
        .is_none());

        // A different review of the same change is not the one recorded.
        let skip = vec!["releng".to_string()];
        assert!(already_in(&root, &t, &Review { skip: &skip, ..r }).is_none());
        assert!(already_in(
            &root,
            &t,
            &Review {
                no_prewarm: true,
                ..r
            }
        )
        .is_none());
        assert!(already_in(&root, &t, &Review { no_gh: true, ..r }).is_none());
        for (model, effort) in [(Some("opus"), None), (None, Some("high"))] {
            let mut g = graph.clone();
            g.nodes[0].model = model.map(str::to_string);
            g.nodes[0].effort = effort.map(str::to_string);
            assert!(
                already_in(&root, &t, &Review { graph: &g, ..r }).is_none(),
                "{model:?} {effort:?}"
            );
        }
        std::fs::write(dir.join("lens.md"), "find more bugs\n").unwrap();
        assert!(
            already_in(&root, &t, &r).is_none(),
            "an edited lens is a new review"
        );
        std::fs::write(dir.join("lens.md"), "find bugs\n").unwrap();
        std::fs::write(&graph_file, format!("{GRAPH}timeout = 60\n")).unwrap();
        assert!(
            already_in(&root, &t, &r).is_none(),
            "an edited graph is a new review"
        );
        std::fs::write(&graph_file, GRAPH).unwrap();
        assert!(
            already_in(&root, &t, &r).is_some(),
            "restoring both restores the match"
        );

        // New or edited conventions change how the review runs, tracked or not.
        std::fs::write(ws.join("REVIEW.md"), "flag every unwrap\n").unwrap();
        assert!(
            already_in(&root, &t, &r).is_none(),
            "a new REVIEW.md is a new review"
        );
        std::fs::remove_file(ws.join("REVIEW.md")).unwrap();
        assert!(already_in(&root, &t, &r).is_some());

        // Untracked files count by content; ignored ones do not count at all.
        std::fs::write(ws.join("ignored.txt"), "build output\n").unwrap();
        assert!(
            already_in(&root, &t, &r).is_some(),
            "an ignored file is not fresh work"
        );
        std::fs::write(ws.join("new.go"), "package x\n").unwrap();
        assert!(
            already_in(&root, &t, &r).is_none(),
            "a new untracked file is fresh work"
        );
        std::fs::remove_file(ws.join("new.go")).unwrap();
        std::fs::write(ws.join("notes.txt"), "edited\n").unwrap();
        assert!(
            already_in(&root, &t, &r).is_none(),
            "an edited untracked file is fresh work"
        );
        std::fs::write(ws.join("notes.txt"), "already here\n").unwrap();
        assert!(
            already_in(&root, &t, &r).is_some(),
            "the same untracked contents as last time are not"
        );

        // A nested repository is listed as a directory; its files still count.
        let sub = ws.join("vendored");
        std::fs::create_dir_all(&sub).unwrap();
        assert!(Command::new("git")
            .args(["-C", sub.to_str().unwrap(), "init", "-q"])
            .status()
            .unwrap()
            .success());
        std::fs::write(sub.join("lib.go"), "package v\n").unwrap();
        record_in(
            &root,
            &t,
            &r,
            &fingerprint(&t, &r).unwrap(),
            Path::new("/bundle"),
        )
        .unwrap();
        assert!(already_in(&root, &t, &r).is_some());
        std::fs::write(sub.join(".gitignore"), "out/\n").unwrap();
        record_in(
            &root,
            &t,
            &r,
            &fingerprint(&t, &r).unwrap(),
            Path::new("/bundle"),
        )
        .unwrap();
        std::fs::create_dir_all(sub.join("out")).unwrap();
        std::fs::write(sub.join("out/x"), "build output\n").unwrap();
        assert!(
            already_in(&root, &t, &r).is_some(),
            "the nested repository's ignores apply inside it"
        );
        std::fs::write(sub.join("lib.go"), "package v // edited\n").unwrap();
        assert!(
            already_in(&root, &t, &r).is_none(),
            "an edit inside a nested repository is fresh work"
        );
        std::fs::remove_dir_all(&sub).unwrap();

        // A name with a newline is hashed as one file, by its exact bytes.
        let odd = ws.join("a\nb");
        std::fs::write(&odd, "one\n").unwrap();
        record_in(
            &root,
            &t,
            &r,
            &fingerprint(&t, &r).unwrap(),
            Path::new("/bundle"),
        )
        .unwrap();
        assert!(already_in(&root, &t, &r).is_some());
        std::fs::write(&odd, "two\n").unwrap();
        assert!(
            already_in(&root, &t, &r).is_none(),
            "an edit to an oddly named file is fresh work"
        );
        std::fs::remove_file(&odd).unwrap();
        record_in(
            &root,
            &t,
            &r,
            &fingerprint(&t, &r).unwrap(),
            Path::new("/bundle"),
        )
        .unwrap();

        // Untracked state that cannot be read never matches a record.
        let unreadable = ws.join("secret.txt");
        std::fs::write(&unreadable, "x\n").unwrap();
        std::fs::set_permissions(
            &unreadable,
            std::os::unix::fs::PermissionsExt::from_mode(0o000),
        )
        .unwrap();
        assert!(fingerprint(&t, &r).is_err());
        assert!(already_in(&root, &t, &r).is_none());
        std::fs::remove_file(&unreadable).unwrap();
        assert!(already_in(&root, &t, &r).is_some());

        // Many untracked files fingerprint without stalling.
        let many = ws.join("many");
        std::fs::create_dir_all(&many).unwrap();
        for i in 0..5000 {
            std::fs::write(many.join(format!("{i:0>60}.txt")), "x").unwrap();
        }
        assert!(already_in(&root, &t, &r).is_none());
        std::fs::remove_dir_all(&many).unwrap();
        assert!(already_in(&root, &t, &r).is_some());

        // A lens edited mid-run leaves the record describing what the run read.
        let before = fingerprint(&t, &r).unwrap();
        std::fs::write(dir.join("lens.md"), "edited mid-run\n").unwrap();
        record_in(&root, &t, &r, &before, Path::new("/bundle")).unwrap();
        assert!(
            already_in(&root, &t, &r).is_none(),
            "the edited lens was never reviewed"
        );
        std::fs::write(dir.join("lens.md"), "find bugs\n").unwrap();
        assert!(already_in(&root, &t, &r).is_some());

        // A newer review replaces the record.
        let t2 = target(&ws, &a, "diff two\n", true);
        record_in(
            &root,
            &t2,
            &r,
            &fingerprint(&t2, &r).unwrap(),
            Path::new("/bundle2"),
        )
        .unwrap();
        assert!(already_in(&root, &t, &r).is_none());
        assert_eq!(already_in(&root, &t2, &r).unwrap()["bundle"], "/bundle2");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
