//! Node kinds that ship inside the harness. A command node whose first word is
//! `builtin:<name>` runs this binary again as `review-run __builtin <name>`, so
//! it gets the same timeout, working directory, `--no-gh` environment and
//! captured stdout/stderr as any other command, and needs no interpreter.
//! Each reads the node's stdin and writes the node's output as JSON on stdout.

pub mod changed_lint;
pub mod codex_lens;
pub mod context_pack;
pub mod prior_comments;

use crate::graph::Kind;
use serde_json::Value;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

/// The prefix of a command that names a built-in node.
pub const PREFIX: &str = "builtin:";

/// The argument after the binary name that makes `review-run` act as the
/// built-in named by the next one.
pub const SUBCOMMAND: &str = "__builtin";

/// The kind of node each built-in implements, or None for an unknown name.
pub fn expects(name: &str) -> Option<Kind> {
    match name {
        "prior-comments" | "changed-lint" | "context-pack" => Some(Kind::Mechanical),
        "codex-lens" => Some(Kind::Generator),
        _ => None,
    }
}

/// `argv` with a leading `builtin:<name>` replaced by this binary's own
/// invocation of it; any other command comes back unchanged.
pub fn resolve(argv: &[String]) -> Result<Vec<String>, String> {
    let Some(name) = argv.first().and_then(|a| a.strip_prefix(PREFIX)) else {
        return Ok(argv.to_vec());
    };
    let exe = std::env::current_exe().map_err(|e| format!("cannot find review-run: {e}"))?;
    let mut out = vec![
        exe.to_string_lossy().into_owned(),
        SUBCOMMAND.to_string(),
        name.to_string(),
    ];
    out.extend(argv[1..].iter().cloned());
    Ok(out)
}

/// Entry point for `review-run __builtin <name>`.
pub fn main(name: &str) -> ExitCode {
    let mut stdin = String::new();
    let _ = std::io::stdin().read_to_string(&mut stdin);
    let runner = RealRunner;
    let result = match name {
        "prior-comments" => Ok(prior_comments::run(
            &stdin,
            &prior_comments::Config::from_env(),
            &runner,
        )),
        "changed-lint" => changed_lint::run(
            &stdin,
            &std::env::var("LINT_RECIPE").unwrap_or_else(|_| "lint-recipe".into()),
            &runner,
        ),
        "codex-lens" => codex_lens::run(&stdin, &runner),
        "context-pack" => Ok(context_pack::run(
            &stdin,
            &context_pack::Config::from_env(),
            &runner,
        )),
        other => Err(Fail {
            code: 2,
            message: format!("review-run: no built-in node `{other}`"),
        }),
    };
    match result {
        Ok(out) => {
            println!("{out}");
            ExitCode::SUCCESS
        }
        Err(f) => {
            eprintln!("{}", f.message);
            ExitCode::from(f.code)
        }
    }
}

/// A node that could not do its job: the message for stderr and the exit code.
#[derive(Debug)]
pub struct Fail {
    pub code: u8,
    pub message: String,
}

/// What an external command printed and how it ended.
#[derive(Debug, Clone, Default)]
pub struct Cmd {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Cmd {
    pub fn ok(&self) -> bool {
        self.code == 0
    }
}

#[derive(Default)]
pub struct Opts<'a> {
    pub cwd: Option<&'a Path>,
    pub stdin: Option<&'a str>,
    pub env: Vec<(&'a str, String)>,
    /// The child is killed and the call fails with `ErrorKind::TimedOut` when
    /// it has not finished by then.
    pub timeout: Option<Duration>,
}

/// How a built-in reaches the tools it drives, so tests can stand in for them.
/// `Sync` because a node may drive several tools from threads at once.
pub trait Runner: Sync {
    /// Whether `tool` names an executable: a path as given, else a `PATH` search.
    fn on_path(&self, tool: &str) -> bool;
    /// Runs `prog` to completion. Err when it could not be started.
    fn run(&self, prog: &str, args: &[String], opts: &Opts) -> std::io::Result<Cmd>;
}

pub struct RealRunner;

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// The child's output once it exits, or None after it is killed for running
/// past `limit`. The pipes are drained on threads so a chatty child cannot
/// stall against its own full pipe; after a kill they are left to close on
/// their own, since a grandchild may still hold them open.
fn wait_within(
    mut child: std::process::Child,
    limit: Duration,
) -> std::io::Result<Option<std::process::Output>> {
    fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            buf
        })
    }
    let out = drain(child.stdout.take());
    let err = drain(child.stderr.take());
    let deadline = std::time::Instant::now() + limit;
    let mut nap = Duration::from_millis(1);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(nap);
        nap = (nap * 2).min(Duration::from_millis(20));
    };
    Ok(Some(std::process::Output {
        status,
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
    }))
}

impl Runner for RealRunner {
    fn on_path(&self, tool: &str) -> bool {
        if tool.contains('/') {
            return is_executable(Path::new(tool));
        }
        let path = std::env::var_os("PATH").unwrap_or_default();
        std::env::split_paths(&path).any(|d| is_executable(&d.join(tool)))
    }

    fn run(&self, prog: &str, args: &[String], opts: &Opts) -> std::io::Result<Cmd> {
        let mut cmd = Command::new(prog);
        cmd.args(args)
            .stdin(if opts.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(d) = opts.cwd {
            cmd.current_dir(d);
        }
        for (k, v) in &opts.env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn()?;
        // Feed stdin from a thread so a child that fills its stdout pipe
        // before reading everything cannot deadlock against this write.
        let writer = opts.stdin.map(|text| {
            let mut pipe = child.stdin.take().expect("piped");
            let bytes = text.as_bytes().to_vec();
            std::thread::spawn(move || {
                let _ = pipe.write_all(&bytes);
            })
        });
        let out = match opts.timeout {
            None => child.wait_with_output()?,
            Some(limit) => match wait_within(child, limit)? {
                Some(out) => out,
                None => return Err(std::io::ErrorKind::TimedOut.into()),
            },
        };
        if let Some(w) = writer {
            let _ = w.join();
        }
        use std::os::unix::process::ExitStatusExt;
        let code = out
            .status
            .code()
            .or_else(|| out.status.signal().map(|s| 128 + s))
            .unwrap_or(1);
        Ok(Cmd {
            code,
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

/// A scratch directory removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(prefix: &str) -> std::io::Result<TempDir> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("{prefix}-{}-{nanos:08x}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        Ok(TempDir(dir))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// jq's "falsy" values are null and false; everything else counts as present.
pub fn present(v: Option<&Value>) -> Option<&Value> {
    v.filter(|v| !matches!(v, Value::Null | Value::Bool(false)))
}

/// A value as jq's string interpolation and `-r` print it.
pub fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The first `n` bytes of `s`, cut back to a character boundary.
pub fn head_bytes(s: &str, n: usize) -> String {
    let mut end = n.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Every JSON value in `text`, or None if any of it fails to parse.
pub fn json_stream(text: &str) -> Option<Vec<Value>> {
    serde_json::Deserializer::from_str(text)
        .into_iter::<Value>()
        .collect::<Result<Vec<_>, _>>()
        .ok()
}

/// A canned `Runner` for tests: `script` answers each call by program and
/// arguments, and every call is recorded.
#[cfg(test)]
pub mod stub {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    pub type Script = Box<dyn Fn(&str, &[String], &Opts) -> std::io::Result<Cmd> + Send + Sync>;

    /// The calls a stub has answered, shared across the threads that made them.
    #[derive(Default)]
    pub struct Calls(Mutex<Vec<String>>);

    impl Calls {
        pub fn borrow(&self) -> MutexGuard<'_, Vec<String>> {
            self.0.lock().unwrap()
        }
    }

    pub struct Stub {
        pub tools: Vec<&'static str>,
        pub script: Script,
        pub calls: Calls,
    }

    impl Stub {
        pub fn new(tools: &[&'static str], script: Script) -> Stub {
            Stub {
                tools: tools.to_vec(),
                script,
                calls: Calls::default(),
            }
        }
    }

    impl Runner for Stub {
        fn on_path(&self, tool: &str) -> bool {
            self.tools.contains(&tool)
        }

        fn run(&self, prog: &str, args: &[String], opts: &Opts) -> std::io::Result<Cmd> {
            self.calls
                .borrow()
                .push(format!("{prog} {}", args.join(" ")));
            (self.script)(prog, args, opts)
        }
    }

    pub fn ok(stdout: &str) -> std::io::Result<Cmd> {
        Ok(Cmd {
            code: 0,
            stdout: stdout.into(),
            stderr: String::new(),
        })
    }

    pub fn fail(code: i32, stderr: &str) -> std::io::Result<Cmd> {
        Ok(Cmd {
            code,
            stdout: String::new(),
            stderr: stderr.into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_builtin_command_runs_this_binary_and_any_other_is_untouched() {
        let argv = vec!["builtin:prior-comments".to_string(), "x".to_string()];
        let out = resolve(&argv).unwrap();
        assert_eq!(&out[1..], ["__builtin", "prior-comments", "x"]);
        let plain = vec!["/g/mechanical/lint".to_string()];
        assert_eq!(resolve(&plain).unwrap(), plain);
    }

    #[test]
    fn each_builtin_names_the_kind_of_node_it_implements() {
        assert_eq!(expects("prior-comments"), Some(Kind::Mechanical));
        assert_eq!(expects("changed-lint"), Some(Kind::Mechanical));
        assert_eq!(expects("context-pack"), Some(Kind::Mechanical));
        assert_eq!(expects("codex-lens"), Some(Kind::Generator));
        assert_eq!(expects("nope"), None);
    }

    #[test]
    fn jq_truthiness_and_text() {
        use serde_json::json;
        assert!(present(Some(&json!(0))).is_some());
        assert!(present(Some(&json!(null))).is_none());
        assert!(present(Some(&json!(false))).is_none());
        assert!(present(None).is_none());
        assert_eq!(text_of(&json!("a")), "a");
        assert_eq!(text_of(&json!(null)), "null");
        assert_eq!(text_of(&json!(12)), "12");
    }

    #[test]
    fn head_bytes_never_splits_a_character() {
        assert_eq!(head_bytes("aé", 2), "a");
        assert_eq!(head_bytes("abc", 10), "abc");
    }

    #[test]
    fn a_json_stream_is_all_or_nothing() {
        assert_eq!(json_stream("[1] [2]").unwrap().len(), 2);
        assert!(json_stream("[1] oops").is_none());
        assert!(json_stream("").unwrap().is_empty());
    }

    #[test]
    fn the_real_runner_captures_output_and_the_exit_code() {
        let r = RealRunner;
        assert!(r.on_path("sh"));
        assert!(!r.on_path("no-such-tool-here"));
        let out = r
            .run(
                "sh",
                &["-c".into(), "cat; echo err >&2; exit 3".into()],
                &Opts {
                    stdin: Some("in"),
                    ..Opts::default()
                },
            )
            .unwrap();
        assert_eq!(
            (out.code, out.stdout.as_str(), out.stderr.as_str()),
            (3, "in", "err\n")
        );
        assert!(r.run("no-such-tool-here", &[], &Opts::default()).is_err());
    }

    #[test]
    fn a_call_past_its_timeout_is_killed_and_a_quick_one_is_unaffected() {
        let r = RealRunner;
        let slow = Opts {
            timeout: Some(Duration::from_millis(100)),
            ..Opts::default()
        };
        let started = std::time::Instant::now();
        let err = r.run("sleep", &["5".into()], &slow).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(4));
        let quick = Opts {
            timeout: Some(Duration::from_secs(10)),
            ..Opts::default()
        };
        let out = r
            .run(
                "sh",
                &["-c".into(), "echo hi; echo err >&2; exit 2".into()],
                &quick,
            )
            .unwrap();
        assert_eq!(
            (out.code, out.stdout.as_str(), out.stderr.as_str()),
            (2, "hi\n", "err\n")
        );
    }
}
