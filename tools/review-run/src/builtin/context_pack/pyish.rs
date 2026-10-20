//! Small string and path helpers whose behavior is fixed by the output the
//! node has always produced: the line and path semantics of the interpreter
//! the first implementation ran on, and its JSON layout.

use serde::Serialize;
use serde_json::{ser::Formatter, Value};
use std::io::{self, Write};

/// Lines as `str.splitlines()` cuts them: at `\n`, `\r`, `\r\n`, and the
/// vertical tab, form feed, file/group/record separators, NEL and the Unicode
/// line and paragraph separators. The separator is dropped; a trailing one
/// does not add an empty line.
pub fn splitlines(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let width = match c {
            '\r' => {
                if matches!(chars.peek(), Some((_, '\n'))) {
                    chars.next();
                    2
                } else {
                    1
                }
            }
            '\n' | '\x0b' | '\x0c' | '\x1c' | '\x1d' | '\x1e' | '\u{85}' | '\u{2028}'
            | '\u{2029}' => c.len_utf8(),
            _ => continue,
        };
        out.push(&s[start..i]);
        start = i + width;
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// Text as a child's captured output reads in text mode: `\r\n` and lone `\r`
/// become `\n`.
pub fn universal_newlines(s: &str) -> String {
    if !s.contains('\r') {
        return s.to_string();
    }
    s.replace("\r\n", "\n").replace('\r', "\n")
}

/// `str(value)` for the JSON values a registry or `gh` can hand back.
pub fn py_str(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Whether a JSON value counts as present the way `x or default` sees it.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null | Value::Bool(false) => false,
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::Bool(true) => true,
    }
}

/// `os.path.join` for any number of parts.
pub fn join(parts: &[&str]) -> String {
    let mut path = parts.first().copied().unwrap_or("").to_string();
    for p in &parts[1.min(parts.len())..] {
        if p.starts_with('/') {
            path = (*p).to_string();
        } else if path.is_empty() || path.ends_with('/') {
            path.push_str(p);
        } else {
            path.push('/');
            path.push_str(p);
        }
    }
    path
}

/// `os.path.normpath`.
pub fn normpath(path: &str) -> String {
    if path.is_empty() {
        return ".".into();
    }
    let initial = if path.starts_with("//") && !path.starts_with("///") {
        2
    } else if path.starts_with('/') {
        1
    } else {
        0
    };
    let mut comps: Vec<&str> = Vec::new();
    for comp in path.split('/') {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp != ".." || (initial == 0 && comps.is_empty()) || comps.last() == Some(&"..") {
            comps.push(comp);
        } else if !comps.is_empty() {
            comps.pop();
        }
    }
    let joined = format!("{}{}", "/".repeat(initial), comps.join("/"));
    if joined.is_empty() {
        ".".into()
    } else {
        joined
    }
}

/// `os.path.basename`.
pub fn basename(p: &str) -> &str {
    &p[p.rfind('/').map(|i| i + 1).unwrap_or(0)..]
}

/// `os.path.dirname`.
pub fn dirname(p: &str) -> String {
    let head = &p[..p.rfind('/').map(|i| i + 1).unwrap_or(0)];
    if !head.is_empty() && head.chars().any(|c| c != '/') {
        head.trim_end_matches('/').to_string()
    } else {
        head.to_string()
    }
}

/// `os.path.splitext(path)[1]`: the extension, dot included, or "". Leading
/// dots of the file name never start one.
pub fn ext(path: &str) -> &str {
    let sep = path.rfind('/').map(|i| i as isize).unwrap_or(-1);
    let Some(dot) = path.rfind('.') else {
        return "";
    };
    if (dot as isize) > sep {
        let name_start = (sep + 1) as usize;
        if path[name_start..dot].chars().any(|c| c != '.') {
            return &path[dot..];
        }
    }
    ""
}

/// `fnmatch.fnmatchcase` for the patterns the skip lists use: `*` and `?`.
pub fn fnmatch(name: &str, pat: &str) -> bool {
    let n: Vec<char> = name.chars().collect();
    let p: Vec<char> = pat.chars().collect();
    let (mut i, mut j) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while i < n.len() {
        if j < p.len() && (p[j] == '?' || p[j] == n[i]) && p[j] != '*' {
            i += 1;
            j += 1;
        } else if j < p.len() && p[j] == '*' {
            star = Some(j);
            mark = i;
            j += 1;
        } else if let Some(s) = star {
            j = s + 1;
            mark += 1;
            i = mark;
        } else {
            return false;
        }
    }
    while j < p.len() && p[j] == '*' {
        j += 1;
    }
    j == p.len()
}

/// `"%g" % t` for the small positive durations the timeout messages show.
pub fn fmt_g(t: f64) -> String {
    if t == 0.0 {
        return "0".into();
    }
    let exp = t.abs().log10().floor() as i32;
    let decimals = (5 - exp).max(0) as usize;
    let s = format!("{t:.decimals$}");
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    }
}

/// Formats JSON the way the first implementation wrote it: `, ` and `: `
/// separators and every non-ASCII character as a `\uXXXX` escape.
struct PyFormatter;

impl Formatter for PyFormatter {
    fn begin_array_value<W: ?Sized + Write>(&mut self, w: &mut W, first: bool) -> io::Result<()> {
        if first {
            Ok(())
        } else {
            w.write_all(b", ")
        }
    }

    fn begin_object_key<W: ?Sized + Write>(&mut self, w: &mut W, first: bool) -> io::Result<()> {
        if first {
            Ok(())
        } else {
            w.write_all(b", ")
        }
    }

    fn begin_object_value<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        w.write_all(b": ")
    }

    fn write_string_fragment<W: ?Sized + Write>(
        &mut self,
        w: &mut W,
        fragment: &str,
    ) -> io::Result<()> {
        for c in fragment.chars() {
            if c.is_ascii() {
                w.write_all(&[c as u8])?;
            } else {
                let mut units = [0u16; 2];
                for u in c.encode_utf16(&mut units) {
                    write!(w, "\\u{u:04x}")?;
                }
            }
        }
        Ok(())
    }
}

/// `json.dumps(value)` with its defaults.
pub fn dumps<T: Serialize>(value: &T) -> String {
    let mut buf = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, PyFormatter);
    value.serialize(&mut ser).expect("serializable");
    String::from_utf8(buf).expect("ascii")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn splitlines_matches_the_interpreter() {
        assert_eq!(splitlines("a\nb\r\nc\rd\n"), ["a", "b", "c", "d"]);
        assert_eq!(splitlines("a\n\nb"), ["a", "", "b"]);
        assert_eq!(splitlines("a\x0bb\u{2028}c"), ["a", "b", "c"]);
        assert!(splitlines("").is_empty());
        assert_eq!(splitlines("\n"), [""]);
    }

    #[test]
    fn universal_newlines_folds_carriage_returns() {
        assert_eq!(universal_newlines("a\r\nb\rc\n"), "a\nb\nc\n");
        assert_eq!(universal_newlines("plain"), "plain");
    }

    #[test]
    fn path_helpers_follow_posixpath() {
        assert_eq!(join(&["a", "b"]), "a/b");
        assert_eq!(join(&["", "b"]), "b");
        assert_eq!(join(&["a", ""]), "a/");
        assert_eq!(join(&["a", "/b"]), "/b");
        assert_eq!(join(&["ws", "", "node_modules", "x"]), "ws/node_modules/x");
        assert_eq!(normpath("a/./b/../c"), "a/c");
        assert_eq!(normpath("../x"), "../x");
        assert_eq!(normpath("a/../.."), "..");
        assert_eq!(normpath(""), ".");
        assert_eq!(normpath("a/.."), ".");
        assert_eq!(normpath("//a"), "//a");
        assert_eq!(normpath("///a"), "/a");
        assert_eq!(basename("a/b.go"), "b.go");
        assert_eq!(basename("b.go"), "b.go");
        assert_eq!(dirname("a/b/c.go"), "a/b");
        assert_eq!(dirname("c.go"), "");
        assert_eq!(dirname("/c.go"), "/");
        assert_eq!(ext("a/b.TS"), ".TS");
        assert_eq!(ext("a.b/c"), "");
        assert_eq!(ext(".gitignore"), "");
        assert_eq!(ext("..x"), "");
        assert_eq!(ext("a/Makefile"), "");
    }

    #[test]
    fn fnmatch_handles_star_and_question() {
        assert!(fnmatch("a.pb.go", "*.pb.go"));
        assert!(fnmatch("bazel-out", "bazel-*"));
        assert!(fnmatch("x_generated.ts", "*_generated.*"));
        assert!(fnmatch("CHANGELOG", "CHANGELOG*"));
        assert!(!fnmatch("changelog.md", "CHANGELOG*"));
        assert!(fnmatch("abc", "a?c"));
        assert!(!fnmatch("abcd", "a?c"));
        assert!(fnmatch("a.gen.go", "*.gen.*"));
        assert!(!fnmatch("a.go", "*.gen.*"));
    }

    #[test]
    fn g_format_drops_trailing_zeros() {
        assert_eq!(fmt_g(3.0), "3");
        assert_eq!(fmt_g(2.5), "2.5");
        assert_eq!(fmt_g(0.5), "0.5");
        assert_eq!(fmt_g(1.234_567_89), "1.23457");
    }

    #[test]
    fn dumps_escapes_non_ascii_and_spaces_separators() {
        let v = json!({"a": ["é", "→", "😀"], "b": null, "c": "q\"\n"});
        let out = dumps(&v);
        assert_eq!(
            out,
            "{\"a\": [\"\\u00e9\", \"\\u2192\", \"\\ud83d\\ude00\"], \"b\": null, \"c\": \"q\\\"\\n\"}"
        );
        assert_eq!(dumps(&json!([])), "[]");
    }

    #[test]
    fn truthiness_and_str() {
        assert!(!truthy(&json!("")));
        assert!(truthy(&json!("x")));
        assert!(!truthy(&json!(null)));
        assert_eq!(py_str(&json!(null)), "None");
        assert_eq!(py_str(&json!(true)), "True");
        assert_eq!(py_str(&json!(7)), "7");
    }
}
