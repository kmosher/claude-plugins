use super::*;
use crate::builtin::stub::{fail, ok, Stub};
use crate::builtin::TempDir;
use serde_json::json;

fn cfg() -> Config {
    Config {
        max_items: DEFAULT_MAX_ITEMS,
        no_gh: false,
        pr_body: String::new(),
        pr_number: String::new(),
        go_mod_cache: None,
        home: None,
    }
}

fn tmp(tag: &str) -> TempDir {
    TempDir::new(&format!("cp-{tag}")).unwrap()
}

fn path_of(d: &TempDir) -> String {
    d.path().to_string_lossy().into_owned()
}

/// A stub answering `git -C <ws> <verb> ...` by verb; anything else fails.
fn git_stub(tree: &'static str, answer: fn(&[String]) -> Option<String>) -> Stub {
    Stub::new(
        &["git"],
        Box::new(move |prog, args, _| {
            if prog != "git" {
                return fail(127, "no such tool");
            }
            match args.get(2).map(String::as_str) {
                Some("ls-files") | Some("ls-tree") => ok(tree),
                _ => match answer(args) {
                    Some(out) => ok(&out),
                    None => fail(1, ""),
                },
            }
        }),
    )
}

fn messages(out: &str) -> Vec<String> {
    let v: Value = serde_json::from_str(out).unwrap();
    v["context"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["message"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn identifiers_split_at_underscores_and_case_changes() {
    assert_eq!(tokens("fetchUserByID"), ["fetch", "user", "by", "id"]);
    assert_eq!(
        tokens("parse_http_URLConfig"),
        ["parse", "http", "url", "config"]
    );
    assert_eq!(tokens("Run"), ["run"]);
}

#[test]
fn citations_are_found_in_every_spelling_and_not_in_lookalikes() {
    let mut got = find_citations("fix for #123 and (#45), see PR 67, issue #8");
    got.sort();
    let nums: Vec<u64> = got.iter().map(|r| r.1).collect();
    assert_eq!(nums, [8, 45, 67, 123]);
    assert!(find_citations("color: #123456; &#39; a/b#7 x#9").is_empty());
    assert_eq!(
        find_citations("https://github.com/example/repo/pull/4242"),
        [(Some("example/repo".to_string()), 4242)]
    );
    let nums: Vec<u64> = find_citations("see https://github.com/o/r/issues/9 and #9")
        .iter()
        .map(|r| r.1)
        .collect();
    assert_eq!(nums, [9, 9]);
    assert_eq!(find_citations("#1 #1"), [(None, 1)]);
}

#[test]
fn qualified_calls_skip_comments_noise_and_other_languages() {
    let c = qualified_calls(
        "+++ b/a.go\n+\tv := store.Fetch(x)\n+\tfmt.Println(1)\n+\t// store.Skip(x)\n\
         +++ b/a.ts\n+const r = api.getThing(1); api.getThing(2); console.log(r)\n\
         +++ b/README.md\n+call a.Doc(x)\n",
    );
    assert_eq!(
        c,
        [
            (("store.Fetch".to_string(), true), 1),
            (("api.getThing".to_string(), false), 2)
        ]
    );
}

#[test]
fn the_qualified_call_patterns_check_what_precedes_them() {
    let go_bad = re!(r"^[\w.]$");
    let ts_bad = re!(r"^[\w.$]$");
    let go = matches_where(
        re!(r"([A-Za-z_]\w*\.[A-Z]\w*)\("),
        "x := pkg.Func(a) + a.b.Method(c) + obj.method(d) + fmt.Sprintf(",
        |p| not_preceded_by(go_bad, p),
    );
    let got: Vec<&str> = go.iter().map(|c| c.get(1).unwrap().as_str()).collect();
    assert_eq!(got, ["pkg.Func", "fmt.Sprintf"]);
    let ts = matches_where(
        re!(r"([a-z]\w*\.[a-z]\w*)\("),
        "client.fetchAll(x); this.y(); a.b.c(); JSON.parse(z); foo.Bar(q)",
        |p| not_preceded_by(ts_bad, p),
    );
    let got: Vec<&str> = ts.iter().map(|c| c.get(1).unwrap().as_str()).collect();
    assert_eq!(got, ["client.fetchAll", "this.y"]);
}

#[test]
fn added_lines_and_content_follow_the_new_side_of_the_diff() {
    let diff = "+++ b/a.go\n@@ -1,2 +10,3 @@\n ctx\n+one\n-two\n+three\n+++ /dev/null\n+gone\n";
    assert_eq!(
        added_content(diff),
        [
            ("a.go".to_string(), "one".to_string()),
            ("a.go".to_string(), "three".to_string())
        ]
    );
    assert_eq!(added_lines(diff)["a.go"], [11, 12]);
    assert_eq!(added_lines("+++ b/a\n@@ -0,0 +0 @@\n+x\n").get("a"), None);
}

#[test]
fn generated_and_vendored_paths_are_skipped() {
    for p in [
        "node_modules/x/index.js",
        "a/vendor/b.go",
        "pkg/x.pb.go",
        "a/bazel-out/x",
        "go.sum",
        "CHANGELOG.md",
        "img/logo.png",
        "src/api_generated.ts",
    ] {
        assert!(skipped(p), "{p}");
    }
    for p in ["src/a.go", "vendorx/a.go", "docs/changelog.md", "a/dist"] {
        assert!(!skipped(p), "{p}");
    }
}

fn repo_in(ws: &TempDir, stub: &Stub, cfg: &Config) -> (Repo, String) {
    let ctx = Ctx::new(stub, cfg, &path_of(ws));
    (Repo::new(&ctx, ""), path_of(ws))
}

#[test]
fn a_changed_function_points_at_siblings_named_like_it() {
    let dir = tmp("sib");
    std::fs::write(
        dir.path().join("a.go"),
        "package a\n\nfunc parseHeader() {}\nfunc parseBody() {}\nfunc parseFooter() {}\nfunc other() {}\n",
    )
    .unwrap();
    let (cfg, stub) = (cfg(), git_stub("a.go\n", |_| None));
    let (repo, ws) = repo_in(&dir, &stub, &cfg);
    let ctx = Ctx::new(&stub, &cfg, &ws);
    let items = sibling_items(&repo, &ctx, "a.go", &[3]);
    let got: Vec<(String, String)> = items
        .iter()
        .map(|i| (i.ptr.clone(), i.why.clone()))
        .collect();
    assert_eq!(
        got,
        [
            (
                "a.go:4".to_string(),
                "`parseBody` is named like the changed `parseHeader`; compare guards and error handling".to_string()
            ),
            (
                "a.go:5".to_string(),
                "`parseFooter` is named like the changed `parseHeader`; compare guards and error handling".to_string()
            ),
        ]
    );
    assert!(items
        .iter()
        .all(|i| i.pri == 1 && i.kind == Some("sibling")));
    assert!(sibling_items(&repo, &ctx, "a.go", &[]).is_empty());
    assert!(sibling_items(&repo, &ctx, "missing.go", &[1]).is_empty());
}

#[test]
fn makefile_includes_point_at_the_files_they_pull_in() {
    let dir = tmp("mk");
    std::fs::write(
        dir.path().join("Makefile"),
        "include common.mk\n-include $(OUT)/gen.mk # trailing\nsinclude missing.mk\n",
    )
    .unwrap();
    let (cfg, stub) = (cfg(), git_stub("Makefile\ncommon.mk\n", |_| None));
    let (repo, ws) = repo_in(&dir, &stub, &cfg);
    let ctx = Ctx::new(&stub, &cfg, &ws);
    let got: Vec<(String, String)> = include_items(&repo, &ctx, "Makefile")
        .into_iter()
        .map(|i| (i.ptr, i.why))
        .collect();
    assert_eq!(
        got,
        [
            (
                "common.mk".to_string(),
                "`include common.mk` at Makefile:1; check variables and flags it sets (.SHELLFLAGS, .ONESHELL, SHELL)".to_string()
            ),
            (
                "Makefile:2".to_string(),
                "includes `$(OUT)/gen.mk`, a computed path; resolve it".to_string()
            ),
        ]
    );
}

#[test]
fn relative_imports_rank_by_whether_the_diff_edits_them() {
    let dir = tmp("ts");
    std::fs::write(
        dir.path().join("a.ts"),
        "import { x } from './b';\nimport y from \"./c\";\nconst z = require('./b');\nimport q from 'pkg';\n",
    )
    .unwrap();
    let (cfg, stub) = (cfg(), git_stub("a.ts\nb.ts\nc/index.ts\n", |_| None));
    let (mut repo, ws) = repo_in(&dir, &stub, &cfg);
    repo.edited = ["c/index.ts".to_string()].into();
    let ctx = Ctx::new(&stub, &cfg, &ws);
    let got: Vec<(i32, String, String)> = include_items(&repo, &ctx, "a.ts")
        .into_iter()
        .map(|i| (i.pri, i.ptr, i.why))
        .collect();
    assert_eq!(
        got,
        [
            (
                1,
                "c/index.ts".to_string(),
                "imported at a.ts:2; also edited in this diff".to_string()
            ),
            (5, "b.ts".to_string(), "imported at a.ts:1".to_string()),
        ]
    );
}

#[test]
fn dockerfiles_point_at_stages_sources_and_build_files() {
    let dir = tmp("df");
    std::fs::write(
        dir.path().join("Dockerfile"),
        "FROM node:20 AS Build\nCOPY src/ /app/\nCOPY --from=build /out /bin\nCOPY --from=ext /x /y\nADD missing /z\n",
    )
    .unwrap();
    let (cfg, stub) = (
        cfg(),
        git_stub("Dockerfile\nsrc\n.dockerignore\n", |_| None),
    );
    let (repo, ws) = repo_in(&dir, &stub, &cfg);
    let ctx = Ctx::new(&stub, &cfg, &ws);
    let got: Vec<(String, String)> = include_items(&repo, &ctx, "Dockerfile")
        .into_iter()
        .map(|i| (i.ptr, i.why))
        .collect();
    assert_eq!(
        got,
        [
            ("src".to_string(), "copied at Dockerfile:2".to_string()),
            ("Dockerfile:1".to_string(), "stage `build` copied from at line 3".to_string()),
            (
                "Dockerfile:4".to_string(),
                "`--from=ext` is a named build context or external image; find where it is supplied".to_string()
            ),
            (
                ".dockerignore".to_string(),
                "build definition beside the Dockerfile; check it still matches".to_string()
            ),
        ]
    );
}

#[test]
fn go_files_list_their_package_siblings_nearest_name_first() {
    let dir = tmp("go");
    let tree = "p/server.go\np/server_test.go\np/a.go\np/b.go\np/c.go\nq/x.go\n";
    let (cfg, stub) = (cfg(), git_stub(tree, |_| None));
    let (repo, ws) = repo_in(&dir, &stub, &cfg);
    let ctx = Ctx::new(&stub, &cfg, &ws);
    let got: Vec<(i32, String, String)> = include_items(&repo, &ctx, "p/server.go")
        .into_iter()
        .map(|i| (i.pri, i.ptr, i.why))
        .collect();
    assert_eq!(
        got,
        [
            (
                4,
                "p/server_test.go".to_string(),
                "same Go package".to_string()
            ),
            (4, "p/a.go".to_string(), "same Go package".to_string()),
            (4, "p/b.go".to_string(), "same Go package".to_string()),
            (
                4,
                "p/".to_string(),
                "1 more files in the package".to_string()
            ),
        ]
    );
}

fn deleted_answer(args: &[String]) -> Option<String> {
    (args.get(2).map(String::as_str) == Some("grep")).then(|| {
        "docs/a.md:7:see old/legacy.sh\ndocs/a.md:9:old/legacy.sh again\nsrc/b.go:2:run(\"old/legacy.sh\")\nold/legacy.sh:1:self\n"
            .to_string()
    })
}

#[test]
fn deleted_and_renamed_paths_still_named_elsewhere_rank_first() {
    let dir = tmp("ref");
    let (cfg, stub) = (cfg(), git_stub("docs/a.md\nsrc/b.go\n", deleted_answer));
    let (repo, ws) = repo_in(&dir, &stub, &cfg);
    let ctx = Ctx::new(&stub, &cfg, &ws);
    let changes = vec![(
        'D',
        "old/legacy.sh".to_string(),
        "old/legacy.sh".to_string(),
    )];
    let got: Vec<(i32, String, String)> = referrer_items(&repo, &ctx, &changes)
        .into_iter()
        .map(|i| (i.pri, i.ptr, i.why))
        .collect();
    // The full path and its distinctive basename are searched for separately.
    let full = "deleted path still referenced (`old/legacy.sh`)".to_string();
    let base = "deleted path still referenced (`legacy.sh`)".to_string();
    assert_eq!(
        got,
        [
            (0, "docs/a.md:7".to_string(), full.clone()),
            (0, "src/b.go:2".to_string(), full),
            (0, "docs/a.md:7".to_string(), base.clone()),
            (0, "src/b.go:2".to_string(), base),
        ]
    );
    let call = stub.calls.borrow().join("\n");
    assert!(
        call.contains(
            "grep -n -I -F -e old/legacy.sh -e legacy.sh -- :(exclude,glob)**/node_modules/**"
        ),
        "{call}"
    );
}

#[test]
fn a_rename_names_both_paths() {
    let dir = tmp("ren");
    let (cfg, stub) = (
        cfg(),
        git_stub("docs/a.md\n", |args| {
            (args.get(2).map(String::as_str) == Some("grep"))
                .then(|| "docs/a.md:1:old.txt and new.txt\n".to_string())
        }),
    );
    let (repo, ws) = repo_in(&dir, &stub, &cfg);
    let ctx = Ctx::new(&stub, &cfg, &ws);
    let changes = vec![('R', "old.txt".to_string(), "new.txt".to_string())];
    let got: Vec<(i32, String)> = referrer_items(&repo, &ctx, &changes)
        .into_iter()
        .map(|i| (i.pri, i.why))
        .collect();
    // The old name is both the old path and a distinctive basename, so it is
    // reported twice here and collapses in the node's final de-duplication.
    assert_eq!(
        got,
        [
            (
                0,
                "renamed from `old.txt`; still references `old.txt`".to_string()
            ),
            (
                0,
                "renamed from `old.txt`; still references `old.txt`".to_string()
            ),
            (3, "mentions `new.txt`".to_string()),
        ]
    );
}

#[test]
fn imports_of_vendored_packages_become_dependency_pointers() {
    let dir = tmp("dep");
    let ws = path_of(&dir);
    std::fs::create_dir_all(dir.path().join("web/node_modules/lodash")).unwrap();
    std::fs::create_dir_all(dir.path().join("mod/sub")).unwrap();
    let cfg = cfg();
    let stub = Stub::new(
        &["git", "go"],
        Box::new({
            let mod_dir = format!("{ws}/mod");
            move |prog, args, _| match (prog, args.get(2).map(String::as_str)) {
                ("git", Some("ls-tree")) => ok("web/package.json\ngo.mod\ncmd/x/main.go\n"),
                ("git", Some("show")) if args[3] == "h:web/package.json" => {
                    ok(r#"{"dependencies":{"lodash":"^4.17.0"}}"#)
                }
                ("git", Some("show")) if args[3] == "h:go.mod" => {
                    ok("module example.com/me\n\nrequire (\n\tgithub.com/foo/bar v1.2.3\n)\n")
                }
                ("go", _) => ok(&format!("{mod_dir}\n")),
                _ => fail(1, ""),
            }
        }),
    );
    let ctx = Ctx::new(&stub, &cfg, &ws);
    let repo = Repo::new(&ctx, "h");
    let text = "+++ b/web/a.ts\n@@ -0,0 +1 @@\n+import _ from 'lodash/fp';\n\
                +import fs from 'node:fs';\n\
                +++ b/cmd/x/main.go\n@@ -0,0 +1,3 @@\n\
                +\t\"github.com/foo/bar/sub\"\n+\t\"example.com/me/pkg\"\n+\t\"fmt\"\n";
    let mut imported = Vec::new();
    let got: Vec<(i32, String, String)> = dependency_items(&repo, &ctx, text, &mut imported)
        .into_iter()
        .map(|i| (i.pri, i.ptr, i.why))
        .collect();
    assert_eq!(
        got,
        [
            (
                -1,
                "dep:lodash".to_string(),
                "dependency lodash → web/node_modules/lodash — read it before judging a call's semantics".to_string()
            ),
            (
                -1,
                "dep:github.com/foo/bar/sub".to_string(),
                format!("dependency github.com/foo/bar/sub → {ws}/mod/sub — read it before judging a call's semantics")
            ),
        ]
    );
    let seen: Vec<(&str, &str, &str)> = imported
        .iter()
        .map(|i| (i.eco, i.name.as_str(), i.version.as_str()))
        .collect();
    assert_eq!(
        seen,
        [
            ("npm", "lodash", "4.17.0"),
            ("go", "github.com/foo/bar", "v1.2.3")
        ]
    );
}

fn citing_answer(args: &[String]) -> Option<String> {
    match args.get(2).map(String::as_str) {
        Some("diff") if args.contains(&"--name-status".to_string()) => Some("M\0a.go\0".into()),
        Some("diff") => {
            Some("+++ b/a.go\n@@ -1 +1 @@\n+// fixes #12 and #99 and org/repo#3\n".into())
        }
        Some("log") => Some("see PR 7\n".into()),
        Some("show") => Some("package a\n".into()),
        _ => None,
    }
}

fn gh_stub(tools: &[&'static str]) -> Stub {
    Stub::new(
        tools,
        Box::new(|prog, args, _| {
            if prog == "git" {
                return match citing_answer(args) {
                    Some(o) => ok(&o),
                    None if args.get(2).map(String::as_str) == Some("ls-tree") => ok("a.go\n"),
                    None => fail(1, ""),
                };
            }
            let a: Vec<&str> = args.iter().map(String::as_str).collect();
            match a.as_slice() {
                ["pr", "view", "--json", "number"] => ok(r#"{"number": 99}"#),
                ["issue", "view", "12", ..] => {
                    ok(r#"{"number":12,"title":"  A   bug\ttitle ","state":"OPEN","url":"u"}"#)
                }
                ["issue", "view", "7", ..] => fail(1, "not found"),
                ["pr", "view", "7", ..] => ok(r#"{"title":"Pull","state":"MERGED"}"#),
                _ => fail(1, "not found"),
            }
        }),
    )
}

fn citing_contract(ws: &str) -> String {
    json!({"workspace": ws, "base": "b", "head": "h", "changed_files": ["a.go"]}).to_string()
}

#[test]
fn citations_resolve_through_gh_and_skip_the_prs_own_number() {
    let dir = tmp("cite");
    let ws = path_of(&dir);
    let stub = gh_stub(&["git", "gh"]);
    let out = run(&citing_contract(&ws), &cfg(), &stub);
    assert_eq!(
        messages(&out),
        [
            "cites #12: \"A bug title\" (open) — check the citation says what the comment claims",
            "cites #7: \"Pull\" (merged) — check the citation says what the comment claims",
        ]
    );
    assert!(out.contains("\\u2014"), "{out}");
}

#[test]
fn no_gh_means_no_citation_lookups() {
    let dir = tmp("nogh");
    let ws = path_of(&dir);
    let stub = gh_stub(&["git", "gh"]);
    let mut c = cfg();
    c.no_gh = true;
    let out = run(&citing_contract(&ws), &c, &stub);
    assert!(messages(&out).is_empty(), "{out}");
    assert!(!stub.calls.borrow().iter().any(|c| c.starts_with("gh ")));
    let stub = gh_stub(&["git"]);
    assert!(messages(&run(&citing_contract(&ws), &cfg(), &stub)).is_empty());
}

#[test]
fn the_pr_body_comes_from_the_contract_the_environment_or_a_file() {
    let dir = tmp("body");
    let ws = path_of(&dir);
    let stub = git_stub("", |_| None);
    let mut c = cfg();
    let ctx = Ctx::new(&stub, &c, &ws);
    assert_eq!(pr_body(&ctx, &json!({"pr_body": "fixes #7"})), "fixes #7");
    assert_eq!(pr_body(&ctx, &json!({"pr_body": ""})), "");
    assert_eq!(pr_body(&ctx, &json!({"pr_body": null})), "");
    std::fs::write(dir.path().join("pr-body.md"), "from file\r\nline").unwrap();
    assert_eq!(pr_body(&ctx, &json!({})), "from file\nline");
    c.pr_body = "literal text".into();
    let ctx = Ctx::new(&stub, &c, &ws);
    assert_eq!(pr_body(&ctx, &json!({"pr_body": null})), "from file\nline");
    std::fs::remove_file(dir.path().join("pr-body.md")).unwrap();
    assert_eq!(pr_body(&ctx, &json!({})), "literal text");
}

#[test]
fn a_new_qualified_call_is_given_its_existing_uses_as_precedent() {
    let dir = tmp("prec");
    let ws = path_of(&dir);
    let cfg = cfg();
    let stub = Stub::new(
        &["git"],
        Box::new(|_, args, _| {
            match args.get(2).map(String::as_str) {
            Some("ls-files") => ok("a.go\n"),
            Some("grep") => ok(
                "x/a_test.go:4:store.Fetch(a)\nx/a.go:9:store.Fetch(b)\nx/a.go:20:store.Fetch(c)\ny/b.go:1:store.Fetch(d)\n",
            ),
            _ => fail(1, ""),
        }
        }),
    );
    let ctx = Ctx::new(&stub, &cfg, &ws);
    let repo = Repo::new(&ctx, "");
    let text = "+++ b/a.go\n@@ -0,0 +1 @@\n+\tv := store.Fetch(x)\n";
    let changes = vec![('M', "a.go".to_string(), "a.go".to_string())];
    let items = precedent_items(&repo, &ctx, &changes, text);
    assert_eq!(items.len(), 1);
    assert_eq!(
        items[0].why,
        "precedent for store.Fetch: x/a.go:9, y/b.go:1, x/a_test.go:4 — compare argument order and shape before calling the diff's use a preference"
    );
    let call = stub
        .calls
        .borrow()
        .iter()
        .find(|c| c.contains(" grep "))
        .cloned()
        .unwrap();
    assert!(
        call.contains("-e store.Fetch( -- *.go :(exclude,literal)a.go"),
        "{call}"
    );
}

#[test]
fn the_full_node_orders_dedupes_and_escapes_its_output() {
    let dir = tmp("full");
    let ws = path_of(&dir);
    std::fs::write(dir.path().join("Makefile"), "include common.mk\n").unwrap();
    let stub = git_stub("Makefile\ncommon.mk\n", |args| {
        (args.get(2).map(String::as_str) == Some("grep"))
            .then(|| "docs/build.md:3:run Makefile targets\n".to_string())
    });
    let contract = json!({"workspace": format!("{ws}/"), "changed_files": ["Makefile", "go.sum"]});
    let out = run(&contract.to_string(), &cfg(), &stub);
    assert_eq!(
        out,
        "{\"diagnostics\": [], \"context\": [\
         {\"tool\": \"context-pack\", \"message\": \"Makefile \\u2192 include: common.mk \\u2014 `include common.mk` at Makefile:1; check variables and flags it sets (.SHELLFLAGS, .ONESHELL, SHELL)\"}, \
         {\"tool\": \"context-pack\", \"message\": \"Makefile \\u2192 referrer: docs/build.md:3 \\u2014 mentions `Makefile`\"}\
         ], \"triggers\": []}"
    );
}

#[test]
fn output_beyond_the_cap_is_summarized() {
    let dir = tmp("cap");
    let ws = path_of(&dir);
    std::fs::write(dir.path().join("Makefile"), "include a.mk b.mk c.mk\n").unwrap();
    let stub = git_stub("Makefile\na.mk\nb.mk\nc.mk\n", |_| None);
    let mut c = cfg();
    c.max_items = 2;
    let contract = json!({"workspace": ws, "changed_files": ["Makefile"]});
    let got = messages(&run(&contract.to_string(), &c, &stub));
    assert_eq!(got.len(), 2);
    assert!(got[0].starts_with("Makefile → include: a.mk"));
    assert_eq!(got[1], "2 lower-priority pointers omitted");
}

#[test]
fn an_unreadable_contract_yields_the_empty_answer() {
    let stub = git_stub("", |_| None);
    let fallback = r#"{"diagnostics": [], "triggers": []}"#;
    assert_eq!(run("not json", &cfg(), &stub), fallback);
    assert_eq!(run("{}", &cfg(), &stub), fallback);
}
