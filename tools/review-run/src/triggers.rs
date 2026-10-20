//! Triggers: named predicates over the change that the harness evaluates
//! itself, so whether a lens runs is never a model's call.

use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use regex::Regex;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawTrigger {
    #[serde(default)]
    touches: Vec<String>,
    diff_matches: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Trigger {
    pub name: String,
    paths: GlobSet,
    diff: Option<Regex>,
}

impl Trigger {
    pub fn compile(name: &str, raw: &RawTrigger) -> Result<Trigger, String> {
        if raw.touches.is_empty() && raw.diff_matches.is_none() {
            return Err(format!(
                "trigger `{name}` needs `touches` or `diff_matches`"
            ));
        }
        let mut paths = GlobSetBuilder::new();
        for g in &raw.touches {
            // `*` stays inside one path segment; `**` is how a pattern crosses them.
            let glob: Glob = GlobBuilder::new(g)
                .literal_separator(true)
                .build()
                .map_err(|e| format!("trigger `{name}`: invalid glob `{g}`: {e}"))?;
            paths.add(glob);
        }
        let paths = paths
            .build()
            .map_err(|e| format!("trigger `{name}`: {e}"))?;
        let diff = raw
            .diff_matches
            .as_deref()
            .map(|r| {
                Regex::new(r).map_err(|e| format!("trigger `{name}`: invalid regex `{r}`: {e}"))
            })
            .transpose()?;
        Ok(Trigger {
            name: name.to_string(),
            paths,
            diff,
        })
    }

    /// What made the trigger fire, or `None`. A matching path is reported in
    /// preference to a matching line, each the first in diff order.
    pub fn fired_by(&self, changed_files: &[String], diff: &str) -> Option<String> {
        if let Some(p) = changed_files
            .iter()
            .find(|p| self.paths.is_match(p.as_str()))
        {
            return Some(format!("touches {p}"));
        }
        let re = self.diff.as_ref()?;
        changed_lines(diff)
            .find(|l| re.is_match(&l[1..]))
            .map(|l| format!("diff line {}", clip(l)))
    }
}

fn clip(line: &str) -> String {
    const MAX: usize = 100;
    match line.char_indices().nth(MAX) {
        Some((i, _)) => format!("{}…", line[..i].trim_end()),
        None => line.to_string(),
    }
}

/// `(old, new)` line counts from a hunk header such as `@@ -3,2 +3 @@`.
fn hunk_counts(header: &str) -> Option<(usize, usize)> {
    let mut parts = header.strip_prefix("@@ ")?.split(' ');
    Some((range_len(parts.next(), '-')?, range_len(parts.next(), '+')?))
}

/// The length in `-3,2` or `+3`, where a bare start means one line.
fn range_len(range: Option<&str>, sign: char) -> Option<usize> {
    match range?.strip_prefix(sign)?.split_once(',') {
        Some((_, n)) => n.parse().ok(),
        None => Some(1),
    }
}

/// The added and removed lines of a unified diff, marker included. Lines are
/// told apart from the `---`/`+++` file headers by position, not by prefix: a
/// removed SQL comment is `--- text` inside a hunk and is still a line.
pub fn changed_lines(diff: &str) -> impl Iterator<Item = &str> {
    let (mut old, mut new) = (0usize, 0usize);
    diff.lines().filter(move |l| {
        if (old, new) == (0, 0) || l.starts_with("diff --git ") {
            (old, new) = hunk_counts(l).unwrap_or((0, 0));
            return false;
        }
        match l.as_bytes().first() {
            Some(b'+') => {
                new = new.saturating_sub(1);
                true
            }
            Some(b'-') => {
                old = old.saturating_sub(1);
                true
            }
            Some(b'\\') => false,
            _ => {
                old = old.saturating_sub(1);
                new = new.saturating_sub(1);
                false
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trigger(touches: &[&str], diff_matches: Option<&str>) -> Result<Trigger, String> {
        let raw = RawTrigger {
            touches: touches.iter().map(|s| s.to_string()).collect(),
            diff_matches: diff_matches.map(str::to_string),
        };
        Trigger::compile("t", &raw)
    }

    const DIFF: &str = "diff --git a/x.sql b/x.sql\n--- a/x.sql\n+++ b/x.sql\n@@ -1,3 +1,2 @@\n context\n--- drop it\n+select 2;\n";

    #[test]
    fn changed_lines_skip_file_headers_but_keep_lines_that_look_like_them() {
        assert_eq!(
            changed_lines(DIFF).collect::<Vec<_>>(),
            ["--- drop it", "+select 2;"]
        );
        let two_files = "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-a\n+b\ndiff --git a/c b/c\n--- a/c\n+++ b/c\n@@ -0,0 +1 @@\n+d\n";
        assert_eq!(
            changed_lines(two_files).collect::<Vec<_>>(),
            ["-a", "+b", "+d"]
        );
    }

    #[test]
    fn globs_match_whole_paths_and_stars_stay_in_a_segment() {
        let t = trigger(&["**/migrations/**", "*.proto"], None).unwrap();
        assert_eq!(
            t.fired_by(&["db/migrations/001.sql".into()], ""),
            Some("touches db/migrations/001.sql".into())
        );
        assert!(t.fired_by(&["migrations/001.sql".into()], "").is_some());
        assert!(t.fired_by(&["a.proto".into()], "").is_some());
        assert!(
            t.fired_by(&["pkg/a.proto".into()], "").is_none(),
            "`*.proto` is the root only"
        );
        assert!(t.fired_by(&["pkg/a.go".into()], "").is_none());
    }

    #[test]
    fn the_regex_sees_line_text_without_its_marker_and_never_a_header() {
        let by_header = trigger(&[], Some(r"x\.sql")).unwrap();
        assert_eq!(by_header.fired_by(&[], DIFF), None);
        let by_line = trigger(&[], Some(r"^select")).unwrap();
        assert_eq!(
            by_line.fired_by(&[], DIFF),
            Some("diff line +select 2;".into())
        );
    }

    #[test]
    fn either_half_fires_and_the_path_is_reported_first() {
        let t = trigger(&["*.sql"], Some("select")).unwrap();
        assert_eq!(
            t.fired_by(&["x.sql".into()], DIFF),
            Some("touches x.sql".into())
        );
        assert_eq!(
            t.fired_by(&["x.go".into()], DIFF),
            Some("diff line +select 2;".into())
        );
        assert_eq!(t.fired_by(&["x.go".into()], ""), None);
    }

    #[test]
    fn bad_definitions_name_the_trigger() {
        assert!(trigger(&["[a"], None)
            .unwrap_err()
            .starts_with("trigger `t`: invalid glob `[a`"));
        assert!(trigger(&[], Some("("))
            .unwrap_err()
            .starts_with("trigger `t`: invalid regex `(`"));
        assert!(trigger(&[], None)
            .unwrap_err()
            .contains("needs `touches` or `diff_matches`"));
    }
}
