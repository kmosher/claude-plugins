//! Written project conventions, collected by the harness rather than left for
//! a model to go looking for: every `AGENTS.md`, `CLAUDE.md` and `REVIEW.md`
//! at the repo root or in an ancestor directory of a changed file, each scoped
//! to the directory it sits in.

use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// How a node is handed the collected files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Contents in the prompt, up to the graph's byte budget.
    Inline,
    /// Paths and scopes, to be read by the node.
    List,
    None,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Inline => "inline",
            Mode::List => "list",
            Mode::None => "none",
        }
    }
}

/// Used when neither a node nor `[defaults]` says.
pub const DEFAULT_MODE: Mode = Mode::List;
pub const DEFAULT_BUDGET: usize = 150_000;

const NAMES: [&str; 3] = ["AGENTS.md", "CLAUDE.md", "REVIEW.md"];

#[derive(Debug, Clone)]
pub struct File {
    /// Relative to the workspace root.
    pub path: String,
    /// The directory the file governs, relative to the root; empty for the root.
    pub scope: String,
    pub bytes: usize,
    pub text: String,
}

impl File {
    fn scope_label(&self) -> String {
        if self.scope.is_empty() {
            "the whole repo".into()
        } else {
            format!("{}/", self.scope)
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Conventions {
    pub files: Vec<File>,
}

/// The `@path` targets of a file's import lines, and whether its entire
/// non-blank content is import lines. Only a line that is nothing but `@path`
/// counts, and nothing inside a code fence does.
fn imports(text: &str) -> (Vec<&str>, bool) {
    let (mut found, mut only, mut fenced) = (Vec::new(), true, false);
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if line.starts_with("```") {
            fenced = !fenced;
            only = false;
        } else if !fenced
            && line.len() > 1
            && line.starts_with('@')
            && !line.contains(char::is_whitespace)
        {
            found.push(&line[1..]);
        } else {
            only = false;
        }
    }
    (found, only)
}

struct Cand {
    path: String,
    scope: String,
    canon: PathBuf,
    text: String,
}

/// The file at `path` if it is a regular file whose real location, after
/// following symlinks, is inside the canonical workspace `root`.
fn read_inside(root: &Path, path: &Path) -> Option<(PathBuf, String)> {
    let canon = path.canonicalize().ok()?;
    if !canon.starts_with(root) || !canon.is_file() {
        return None;
    }
    let bytes = std::fs::read(&canon).ok()?;
    Some((canon, String::from_utf8_lossy(&bytes).into_owned()))
}

fn depth(dir: &str) -> usize {
    if dir.is_empty() {
        0
    } else {
        dir.matches('/').count() + 1
    }
}

impl Conventions {
    /// Root first, then by directory depth and name; `AGENTS.md`, `CLAUDE.md`,
    /// `REVIEW.md` within a directory. A file reachable twice, by a symlink or
    /// an import, is kept once, where it first appears. A `CLAUDE.md` that is
    /// only imports of collected files is dropped; its other imports are
    /// resolved one level and follow it, scoped to the importer's directory.
    pub fn collect(workspace: &Path, changed_files: &[String]) -> Conventions {
        let Ok(root) = workspace.canonicalize() else {
            return Conventions::default();
        };
        let mut dirs: BTreeSet<String> = BTreeSet::from([String::new()]);
        for f in changed_files {
            let mut p = Path::new(f).parent();
            while let Some(d) = p.filter(|d| !d.as_os_str().is_empty()) {
                dirs.insert(d.to_string_lossy().into_owned());
                p = d.parent();
            }
        }
        let mut dirs: Vec<String> = dirs.into_iter().collect();
        dirs.sort_by_key(|d| (depth(d), d.clone()));

        let mut seen: HashSet<PathBuf> = HashSet::new();
        let mut cands: Vec<Cand> = Vec::new();
        for dir in &dirs {
            for name in NAMES {
                let path = if dir.is_empty() {
                    name.to_string()
                } else {
                    format!("{dir}/{name}")
                };
                if let Some((canon, text)) = read_inside(&root, &root.join(&path)) {
                    if seen.insert(canon.clone()) {
                        cands.push(Cand {
                            path,
                            scope: dir.clone(),
                            canon,
                            text,
                        });
                    }
                }
            }
        }

        let resolve = |scope: &str, target: &str| -> Option<(PathBuf, String)> {
            if target.starts_with('~') {
                return None;
            }
            read_inside(&root, &root.join(scope).join(target))
        };
        let collected: HashSet<PathBuf> = cands.iter().map(|c| c.canon.clone()).collect();
        let mut files = Vec::new();
        for c in &cands {
            let (targets, only_imports) = imports(&c.text);
            let resolved: Vec<(PathBuf, String)> = targets
                .iter()
                .filter_map(|t| resolve(&c.scope, t))
                .collect();
            let is_claude_md = c.path.rsplit('/').next() == Some("CLAUDE.md");
            if is_claude_md
                && only_imports
                && !resolved.is_empty()
                && resolved.len() == targets.len()
                && resolved.iter().all(|(p, _)| collected.contains(p))
            {
                continue;
            }
            files.push(File {
                path: c.path.clone(),
                scope: c.scope.clone(),
                bytes: c.text.len(),
                text: c.text.clone(),
            });
            for (canon, text) in resolved {
                if seen.insert(canon.clone()) {
                    let path = canon
                        .strip_prefix(&root)
                        .unwrap_or(&canon)
                        .to_string_lossy()
                        .into_owned();
                    files.push(File {
                        path,
                        scope: c.scope.clone(),
                        bytes: text.len(),
                        text,
                    });
                }
            }
        }
        Conventions { files }
    }

    /// Which files, by position, fit the byte budget. A file that does not fit
    /// is passed over and a later, smaller one may still be inlined.
    pub fn inlined(&self, budget: usize) -> Vec<bool> {
        let mut left = budget;
        self.files
            .iter()
            .map(|f| {
                let fits = f.bytes <= left;
                if fits {
                    left -= f.bytes;
                }
                fits
            })
            .collect()
    }

    /// The prompt section a generator in `mode` is given; empty for `None` or
    /// when nothing was collected.
    pub fn section(&self, mode: Mode, budget: usize) -> String {
        if self.files.is_empty() {
            return String::new();
        }
        match mode {
            Mode::None => String::new(),
            Mode::List => self.listing(),
            Mode::Inline => self.inline(budget),
        }
    }

    /// The form a critic gets: enough to read a cited rule's file.
    pub fn listing(&self) -> String {
        if self.files.is_empty() {
            return String::new();
        }
        let mut s = String::from("\nproject conventions: written rules, each applying to code under its own directory. Read each before reviewing code under that directory.\n");
        for f in &self.files {
            let _ = writeln!(
                s,
                "  {} ({} bytes): applies to {}",
                f.path,
                f.bytes,
                f.scope_label()
            );
        }
        s
    }

    fn inline(&self, budget: usize) -> String {
        let fits = self.inlined(budget);
        let mut s = String::from(
            "\nproject conventions\n\nThese are the project's written rules, loaded for you; nothing else will load them. \
             Each applies to code under its own directory. A violation of a written rule in changed code is a finding: \
             cite the rule's file and heading.\n",
        );
        for (f, _) in self.files.iter().zip(&fits).filter(|(_, fit)| **fit) {
            let _ = write!(
                s,
                "\n<file path=\"{}\" applies-to=\"{}\">\n{}\n</file>\n",
                f.path,
                f.scope_label(),
                f.text.trim_end()
            );
        }
        if fits.iter().any(|fit| !fit) {
            s.push_str("\nThese did not fit; read these yourself before reviewing code under their directory:\n");
            for (f, _) in self.files.iter().zip(&fits).filter(|(_, fit)| !**fit) {
                let _ = writeln!(
                    s,
                    "  {} ({} bytes): before reviewing code under {}",
                    f.path,
                    f.bytes,
                    f.scope_label()
                );
            }
        }
        s
    }

    /// The manifest record: what was collected and what an inline node sees.
    pub fn record(&self, budget: usize) -> Value {
        let fits = self.inlined(budget);
        let files: Vec<Value> = self
            .files
            .iter()
            .zip(fits)
            .map(|(f, inlined)| json!({"path": f.path, "scope": f.scope, "bytes": f.bytes, "inlined": inlined}))
            .collect();
        json!({"budget": budget, "files": files})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn ws(files: &[(&str, &str)]) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "review-run-conv-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        for (path, text) in files {
            let p = dir.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        dir
    }

    fn paths(c: &Conventions) -> Vec<&str> {
        c.files.iter().map(|f| f.path.as_str()).collect()
    }

    fn changed(files: &[&str]) -> Vec<String> {
        files.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn collects_ancestors_root_first_then_by_depth() {
        let w = ws(&[
            ("a/b/AGENTS.md", "deep"),
            ("AGENTS.md", "root"),
            ("a/REVIEW.md", "a review"),
            ("a/CLAUDE.md", "a claude"),
            ("a/AGENTS.md", "a agents"),
            ("z/AGENTS.md", "not an ancestor"),
            ("REVIEW.md", "root review"),
        ]);
        let c = Conventions::collect(&w, &changed(&["a/b/x.go", "a/y.go"]));
        assert_eq!(
            paths(&c),
            [
                "AGENTS.md",
                "REVIEW.md",
                "a/AGENTS.md",
                "a/CLAUDE.md",
                "a/REVIEW.md",
                "a/b/AGENTS.md"
            ]
        );
        assert_eq!(c.files[5].scope, "a/b");
        assert_eq!(c.files[0].scope, "");
        assert_eq!(c.files[0].bytes, 4);
    }

    #[test]
    fn siblings_at_one_depth_sort_by_path_and_each_file_is_kept_once() {
        let w = ws(&[("b/AGENTS.md", "b"), ("a/AGENTS.md", "a")]);
        let c = Conventions::collect(&w, &changed(&["b/x.go", "a/x.go", "b/y.go"]));
        assert_eq!(paths(&c), ["a/AGENTS.md", "b/AGENTS.md"]);
    }

    #[test]
    fn a_symlinked_duplicate_is_collected_once_and_an_escaping_symlink_not_at_all() {
        let w = ws(&[("AGENTS.md", "rules")]);
        std::os::unix::fs::symlink("AGENTS.md", w.join("CLAUDE.md")).unwrap();
        let outside = ws(&[("secret.md", "outside")]);
        std::fs::create_dir_all(w.join("d")).unwrap();
        std::os::unix::fs::symlink(outside.join("secret.md"), w.join("d/AGENTS.md")).unwrap();
        let c = Conventions::collect(&w, &changed(&["d/x.go"]));
        assert_eq!(paths(&c), ["AGENTS.md"]);
    }

    #[test]
    fn a_claude_md_of_only_imports_of_collected_files_is_dropped() {
        let w = ws(&[("AGENTS.md", "rules"), ("CLAUDE.md", "\n@AGENTS.md\n\n")]);
        assert_eq!(
            paths(&Conventions::collect(&w, &changed(&["x.go"]))),
            ["AGENTS.md"]
        );

        let w = ws(&[
            ("AGENTS.md", "rules"),
            ("CLAUDE.md", "@AGENTS.md\nand more\n"),
        ]);
        assert_eq!(
            paths(&Conventions::collect(&w, &changed(&["x.go"]))),
            ["AGENTS.md", "CLAUDE.md"]
        );

        let w = ws(&[
            ("AGENTS.md", "rules"),
            ("docs/extra.md", "e"),
            ("CLAUDE.md", "@AGENTS.md\n@docs/extra.md\n"),
        ]);
        let c = Conventions::collect(&w, &changed(&["x.go"]));
        assert_eq!(
            paths(&c),
            ["AGENTS.md", "CLAUDE.md", "docs/extra.md"],
            "an import of an uncollected file keeps the importer"
        );
    }

    #[test]
    fn imports_resolve_one_level_relative_to_the_importer_inside_the_workspace() {
        let w = ws(&[
            ("pkg/CLAUDE.md", "# pkg\n@rules/style.md\n@../outside.md\n@missing.md\n@~/home.md\n```\n@fenced.md\n```\n"),
            ("pkg/rules/style.md", "@deeper.md\nstyle"),
            ("pkg/rules/deeper.md", "not followed"),
            ("pkg/fenced.md", "inside a fence"),
            ("outside.md", "inside the workspace but named by a parent path"),
        ]);
        let c = Conventions::collect(&w, &changed(&["pkg/x.go"]));
        assert_eq!(
            paths(&c),
            ["pkg/CLAUDE.md", "pkg/rules/style.md", "outside.md"]
        );
        assert_eq!(
            c.files[1].scope, "pkg",
            "an import is scoped to the file that named it"
        );
    }

    #[test]
    fn an_import_that_leaves_the_workspace_is_not_followed() {
        let outside = ws(&[("secret.md", "outside")]);
        let w = ws(&[(
            "CLAUDE.md",
            &format!("# c\n@{}\n", outside.join("secret.md").display()),
        )]);
        assert_eq!(
            paths(&Conventions::collect(&w, &changed(&["x.go"]))),
            ["CLAUDE.md"]
        );
    }

    fn sized() -> Conventions {
        let w = ws(&[
            ("AGENTS.md", &"r".repeat(100)),
            ("a/AGENTS.md", &"a".repeat(300)),
            ("a/b/AGENTS.md", &"b".repeat(50)),
        ]);
        Conventions::collect(&w, &changed(&["a/b/x.go"]))
    }

    #[test]
    fn a_file_over_the_budget_is_listed_and_a_later_smaller_one_still_fits() {
        let c = sized();
        assert_eq!(c.inlined(1000), [true, true, true]);
        assert_eq!(c.inlined(200), [true, false, true]);
        let s = c.section(Mode::Inline, 200);
        assert!(s.contains("<file path=\"AGENTS.md\" applies-to=\"the whole repo\">"));
        assert!(!s.contains(&"a".repeat(300)));
        assert!(s.contains("<file path=\"a/b/AGENTS.md\" applies-to=\"a/b/\">"));
        assert!(s.contains("These did not fit; read these yourself"));
        assert!(s.contains("  a/AGENTS.md (300 bytes): before reviewing code under a/"));
        assert!(!c.section(Mode::Inline, 1000).contains("did not fit"));
    }

    #[test]
    fn each_mode_has_its_own_text() {
        let c = sized();
        let inline = c.section(Mode::Inline, 1000);
        assert!(
            inline.contains("project's written rules")
                && inline.contains("cite the rule's file and heading")
        );
        assert!(inline.contains(&"r".repeat(100)));
        let list = c.section(Mode::List, 1000);
        assert!(list.contains("Read each before reviewing code under that directory"));
        assert!(list.contains("  a/AGENTS.md (300 bytes): applies to a/"));
        assert!(list.contains("applies to the whole repo") && !list.contains(&"r".repeat(100)));
        assert_eq!(c.section(Mode::None, 1000), "");
        assert_eq!(Conventions::default().section(Mode::Inline, 1000), "");
    }

    #[test]
    fn the_record_says_what_was_collected_and_inlined() {
        let r = sized().record(200);
        assert_eq!(r["budget"], 200);
        assert_eq!(
            r["files"][1],
            json!({"path": "a/AGENTS.md", "scope": "a", "bytes": 300, "inlined": false})
        );
        assert_eq!(r["files"][2]["inlined"], true);
    }
}
