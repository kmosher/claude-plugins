//! How a node becomes a child process. One trait, one implementation per
//! provider; the runner never knows which it is talking to.

use crate::graph::{Kind, Node};
use serde::Serialize;
use serde_json::{json, Value};
use std::ffi::OsString;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub struct Invocation<'a> {
    pub node: &'a Node,
    pub workspace: &'a Path,
    pub prompt: &'a str,
    pub schema: &'a Value,
    /// The node's directory in the bundle; event logs and stderr land here.
    pub dir: &'a Path,
    pub attempt: u32,
    /// A session to continue rather than start cold: the retry turn's own
    /// session, or with `fork` the prewarm session a generator starts from.
    pub resume: Option<&'a str>,
    /// Continue a copy of `resume` under a new session id instead of
    /// appending to it, so the sessions forked from one prewarm never touch it.
    pub fork: bool,
    /// `prompt` is the node's whole stdin, for a command node, rather than a
    /// field of the envelope that carries it.
    pub verbatim: bool,
    /// GitHub access to withhold from the child.
    pub no_gh: Option<&'a NoGh>,
}

/// The environment variables `gh` takes a credential from.
const GH_TOKEN_VARS: [&str; 4] = [
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
];

/// Set in every child of a `--no-gh` run, so a mechanical script can skip its
/// GitHub work up front instead of failing call by call.
pub const NO_GH_ENV: &str = "REVIEW_RUN_NO_GH";

/// Told to every reviewer of a `--no-gh` run, so it does not spend turns
/// retrying a command that cannot work.
pub const NO_GH_NOTICE: &str = "GitHub access is disabled in this run: `gh` fails and no GitHub credentials are available. Work from the checkout and the diff.";

/// What a run needs to keep its children off GitHub: a directory whose `gh`
/// always fails, to shadow the real one on `PATH`, and an empty `gh` config
/// directory. The stub only saves turns, since a child can still run `gh` by
/// absolute path; the control is that, without a token variable or a config
/// directory to read one from, that `gh` has no login. Removed on drop.
pub struct NoGh {
    root: PathBuf,
    bin: PathBuf,
    config: PathBuf,
}

impl NoGh {
    pub fn create() -> Result<NoGh, String> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!(
            "review-run-no-gh-{}-{nanos:08x}",
            std::process::id()
        ));
        let (bin, config) = (root.join("bin"), root.join("config"));
        let io = |e: std::io::Error| format!("{}: {e}", root.display());
        std::fs::create_dir_all(&bin).map_err(io)?;
        std::fs::create_dir_all(&config).map_err(io)?;
        let stub = bin.join("gh");
        std::fs::write(
            &stub,
            "#!/bin/sh\necho 'gh disabled for this run' >&2\nexit 1\n",
        )
        .map_err(io)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).map_err(io)?;
        }
        Ok(NoGh { root, bin, config })
    }

    fn path(&self) -> OsString {
        let rest = std::env::var_os("PATH").unwrap_or_default();
        let dirs = std::iter::once(self.bin.clone()).chain(std::env::split_paths(&rest));
        std::env::join_paths(dirs).unwrap_or_else(|_| self.bin.clone().into())
    }

    fn apply(&self, cmd: &mut Command) {
        for var in GH_TOKEN_VARS {
            cmd.env_remove(var);
        }
        cmd.env("GH_CONFIG_DIR", &self.config)
            .env("PATH", self.path())
            .env(NO_GH_ENV, "1");
    }
}

impl Drop for NoGh {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[derive(Debug, Default, Clone, Serialize, serde::Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub output_tokens: u64,
}

impl Usage {
    pub fn add(&mut self, o: &Usage) {
        self.input_tokens += o.input_tokens;
        self.cache_creation_input_tokens += o.cache_creation_input_tokens;
        self.cache_read_input_tokens += o.cache_read_input_tokens;
        self.output_tokens += o.output_tokens;
    }
}

#[derive(Debug, Clone)]
pub struct Outcome {
    /// The node's structured output, not yet checked against its type.
    pub output: Result<Value, String>,
    pub session_id: Option<String>,
    pub usage: Usage,
    pub cost_usd: f64,
    pub turns: u64,
    pub exit: Option<i32>,
}

impl Outcome {
    pub fn failed(why: impl Into<String>) -> Outcome {
        Outcome {
            output: Err(why.into()),
            session_id: None,
            usage: Usage::default(),
            cost_usd: 0.0,
            turns: 0,
            exit: None,
        }
    }
}

pub trait Provider: Sync {
    /// The command line a run of this invocation executes, for `--dry-run`
    /// and the bundle. The prompt goes on stdin and is not part of it.
    fn command(&self, inv: &Invocation) -> Result<Vec<String>, String>;
    fn run(&self, inv: &Invocation) -> Outcome;
}

/// Runs `argv` in the workspace with `stdin` fed to it and stdout and stderr
/// sent to files, killing it at the timeout. Returns the exit code.
fn spawn(
    argv: &[String],
    env: &[(&str, &str)],
    inv: &Invocation,
    stdin: &[u8],
    stdout: &Path,
    stderr: &Path,
) -> Result<Option<i32>, String> {
    let io = |e: std::io::Error| e.to_string();
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).envs(env.iter().copied());
    if let Some(no_gh) = inv.no_gh {
        no_gh.apply(&mut cmd);
    }
    let mut child = cmd
        .current_dir(inv.workspace)
        .stdin(Stdio::piped())
        .stdout(File::create(stdout).map_err(io)?)
        .stderr(File::create(stderr).map_err(io)?)
        .spawn()
        .map_err(|e| format!("could not start {}: {e}", argv[0]))?;
    // A child that exits without reading its input is reported by its exit
    // status below, not by the broken pipe here.
    let _ = child.stdin.take().expect("piped").write_all(stdin);
    let deadline = Instant::now() + Duration::from_secs(inv.node.timeout);
    loop {
        if let Some(status) = child.try_wait().map_err(io)? {
            return Ok(status.code());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("timed out after {}s", inv.node.timeout));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn attempt_file(inv: &Invocation, stem: &str, ext: &str) -> std::path::PathBuf {
    match inv.attempt {
        1 => inv.dir.join(format!("{stem}.{ext}")),
        n => inv.dir.join(format!("{stem}-{n}.{ext}")),
    }
}

/// `claude -p`, cold: no MCP servers, no CLAUDE.md, no plugins, skills or
/// hooks, and a system prompt that is the lens and nothing else.
pub struct Claude {
    pub bin: String,
}

/// A skill or agent file is a prompt with YAML frontmatter the harness that
/// loads it would have consumed; here nothing does, so it is dropped.
fn strip_frontmatter(text: &str) -> &str {
    if let Some(rest) = text.strip_prefix("---\n") {
        if let Some(end) = rest.find("\n---\n") {
            return rest[end + 5..].trim_start_matches('\n');
        }
    }
    text
}

/// The node's whole system prompt: its prompt files in order, frontmatter
/// dropped. Empty for a node with none.
pub fn compose_system_prompt(node: &Node) -> Result<String, String> {
    compose_files(&node.system_prompt)
}

/// A fork's lens: the files it declared itself, composed the same way, for
/// its user turn.
pub fn compose_lens(node: &Node) -> Result<String, String> {
    compose_files(&node.lens)
}

fn compose_files(paths: &[std::path::PathBuf]) -> Result<String, String> {
    let mut out = String::new();
    for p in paths {
        let text = std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
        out.push_str(strip_frontmatter(&text).trim_end());
        out.push_str("\n\n");
    }
    Ok(out)
}

impl Claude {
    fn system_prompt(&self, inv: &Invocation) -> Result<String, String> {
        compose_system_prompt(inv.node)
    }
}

/// A prewarm node and the forks that resume it keep their transcript in the
/// prompt cache for an hour; the CLI's own default is five minutes unless the
/// account is a subscription within its limits.
pub const CACHE_TTL: &str = "1h";

pub fn cache_env(node: &Node) -> Vec<(&'static str, &'static str)> {
    if node.kind == Kind::Prewarm || node.fork_from.is_some() {
        vec![("CLAUDE_CODE_PROMPT_CACHE_TTL", CACHE_TTL)]
    } else {
        Vec::new()
    }
}

impl Provider for Claude {
    fn command(&self, inv: &Invocation) -> Result<Vec<String>, String> {
        let mut argv: Vec<String> = vec![self.bin.clone(), "-p".into(), "--safe-mode".into()];
        if let Some(id) = inv.resume {
            argv.extend(["--resume".into(), id.into()]);
            if inv.fork {
                argv.push("--fork-session".into());
            }
        }
        if let Some(m) = &inv.node.model {
            argv.extend(["--model".into(), m.clone()]);
        }
        if let Some(e) = &inv.node.effort {
            argv.extend(["--effort".into(), e.clone()]);
        }
        argv.extend([
            "--system-prompt-file".into(),
            inv.dir
                .join("system-prompt.md")
                .to_string_lossy()
                .into_owned(),
        ]);
        // A prewarm node and its forks are all handed the same schema: the output tool it adds
        // joins the tool list the cached prefix begins with, so a fork given another schema, or
        // none, would miss everything the prewarm cached.
        argv.extend(["--json-schema".into(), inv.schema.to_string()]);
        if let Some(n) = inv.node.max_turns {
            argv.extend(["--max-turns".into(), n.to_string()]);
        }
        argv.extend([
            "--output-format".into(),
            "stream-json".into(),
            "--verbose".into(),
            // The child cannot answer a prompt; what it may do is bounded by
            // its tool list and the scratch workspace instead.
            "--dangerously-skip-permissions".into(),
            "--tools".into(),
            inv.node.tools.join(","),
        ]);
        Ok(argv)
    }

    fn run(&self, inv: &Invocation) -> Outcome {
        let prepared = self
            .system_prompt(inv)
            .and_then(|sp| {
                std::fs::write(inv.dir.join("system-prompt.md"), sp).map_err(|e| e.to_string())
            })
            .and_then(|_| self.command(inv));
        let argv = match prepared {
            Ok(a) => a,
            Err(e) => return Outcome::failed(e),
        };
        let events = attempt_file(inv, "events", "jsonl");
        let exit = match spawn(
            &argv,
            &cache_env(inv.node),
            inv,
            inv.prompt.as_bytes(),
            &events,
            &attempt_file(inv, "stderr", "txt"),
        ) {
            Ok(code) => code,
            Err(e) => {
                // A timed-out child may still have spent tokens worth counting.
                let mut o = parse_events(&std::fs::read_to_string(&events).unwrap_or_default());
                o.output = Err(e);
                return o;
            }
        };
        let mut o = parse_events(&std::fs::read_to_string(&events).unwrap_or_default());
        o.exit = exit;
        if exit != Some(0) && o.output.is_ok() {
            o.output = Err(format!("claude exited {exit:?}"));
        }
        o
    }
}

/// Reads a `--output-format stream-json` log down to its result event.
pub fn parse_events(text: &str) -> Outcome {
    let result = text
        .lines()
        .rev()
        .filter(|l| l.contains("\"type\":\"result\""))
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["type"] == "result");
    let Some(r) = result else {
        return Outcome::failed("the event stream ended without a result");
    };
    let n = |v: &Value| v.as_u64().unwrap_or(0);
    let u = &r["usage"];
    let output = if r["is_error"].as_bool().unwrap_or(false) {
        Err(format!(
            "claude reported an error ({}): {}",
            r["subtype"].as_str().unwrap_or("unknown"),
            head(r["result"].as_str().unwrap_or(""))
        ))
    } else if r["structured_output"].is_null() {
        Err(format!(
            "no structured output; the reply began: {}",
            head(r["result"].as_str().unwrap_or(""))
        ))
    } else {
        Ok(r["structured_output"].clone())
    };
    Outcome {
        output,
        session_id: r["session_id"].as_str().map(str::to_string),
        usage: Usage {
            input_tokens: n(&u["input_tokens"]),
            cache_creation_input_tokens: n(&u["cache_creation_input_tokens"]),
            cache_read_input_tokens: n(&u["cache_read_input_tokens"]),
            output_tokens: n(&u["output_tokens"]),
        },
        cost_usd: r["total_cost_usd"].as_f64().unwrap_or(0.0),
        turns: n(&r["num_turns"]),
        exit: None,
    }
}

fn head(s: &str) -> String {
    s.chars().take(300).collect()
}

/// Any executable that reads the node contract as JSON on stdin and writes
/// its output as JSON on stdout.
pub struct Exec;

impl Provider for Exec {
    fn command(&self, inv: &Invocation) -> Result<Vec<String>, String> {
        Ok(inv.node.command.clone())
    }

    fn run(&self, inv: &Invocation) -> Outcome {
        let envelope = json!({
            "node": inv.node.name,
            "workspace": inv.workspace,
            "prompt": inv.prompt,
            "schema": inv.schema,
        });
        let stdin = if inv.verbatim {
            inv.prompt.to_string()
        } else {
            envelope.to_string()
        };
        let stdout = attempt_file(inv, "stdout", "json");
        let exit = match spawn(
            &inv.node.command,
            &[],
            inv,
            stdin.as_bytes(),
            &stdout,
            &attempt_file(inv, "stderr", "txt"),
        ) {
            Ok(code) => code,
            Err(e) => return Outcome::failed(e),
        };
        let mut o = Outcome::failed("");
        o.exit = exit;
        o.output = if exit != Some(0) {
            Err(format!("{} exited {exit:?}", inv.node.command[0]))
        } else {
            std::fs::read_to_string(&stdout)
                .map_err(|e| e.to_string())
                .and_then(|t| {
                    serde_json::from_str(&t).map_err(|e| format!("stdout is not JSON: {e}"))
                })
        };
        o
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_is_dropped_and_a_plain_file_is_untouched() {
        assert_eq!(
            strip_frontmatter("---\nname: x\n---\n\n# Body\n"),
            "# Body\n"
        );
        assert_eq!(
            strip_frontmatter("# Body\n---\nnot frontmatter\n"),
            "# Body\n---\nnot frontmatter\n"
        );
    }

    #[test]
    fn a_command_node_gets_the_envelope_or_exactly_what_the_runner_wrote() {
        let dir = std::env::temp_dir().join(format!("review-run-exec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let g = crate::graph::Graph::parse(
            "[[node]]\nname = \"echo\"\nkind = \"generator\"\nprovider = \"command\"\ncommand = [\"cat\"]\n",
            Path::new("."),
            "t",
        )
        .unwrap();
        let schema = json!({});
        let run = |verbatim| {
            let inv = Invocation {
                node: &g.nodes[0],
                workspace: &dir,
                prompt: "{\"base\":\"abc\"}",
                schema: &schema,
                dir: &dir,
                attempt: 1,
                resume: None,
                fork: false,
                verbatim,
                no_gh: None,
            };
            Exec.run(&inv).output.unwrap()
        };
        assert_eq!(run(true), json!({"base": "abc"}));
        let wrapped = run(false);
        assert_eq!(
            (wrapped["node"].as_str(), wrapped["prompt"].as_str()),
            (Some("echo"), Some("{\"base\":\"abc\"}"))
        );
    }

    /// Runs `script` as a command node's child, with fake credentials in this
    /// process's environment, and returns the JSON it printed.
    fn child_of_a_no_gh_run(script: &str, no_gh: Option<&NoGh>) -> Value {
        let dir = std::env::temp_dir().join(format!(
            "review-run-no-gh-test-{}-{}",
            std::process::id(),
            script.len()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("child.sh"), script).unwrap();
        let toml = format!("[[node]]\nname = \"probe\"\nkind = \"generator\"\nprovider = \"command\"\ncommand = [\"sh\", \"{}\"]\n", dir.join("child.sh").display());
        let g = crate::graph::Graph::parse(&toml, Path::new("."), "t").unwrap();
        let schema = json!({});
        let inv = Invocation {
            node: &g.nodes[0],
            workspace: &dir,
            prompt: "",
            schema: &schema,
            dir: &dir,
            attempt: 1,
            resume: None,
            fork: false,
            verbatim: true,
            no_gh,
        };
        Exec.run(&inv).output.unwrap()
    }

    const PROBE: &str = concat!(
        "printf '{\"tokens\":\"%s\",\"config\":\"%s\",\"gh\":\"%s\",\"flag\":\"%s\"}' ",
        "\"${GH_TOKEN-}${GITHUB_TOKEN-}${GH_ENTERPRISE_TOKEN-}${GITHUB_ENTERPRISE_TOKEN-}\" ",
        "\"${GH_CONFIG_DIR-}\" \"$(command -v gh)\" \"${REVIEW_RUN_NO_GH-}\"\n",
    );

    #[test]
    fn a_no_gh_child_has_no_token_an_empty_gh_config_and_a_failing_gh_first_on_path() {
        for var in GH_TOKEN_VARS {
            std::env::set_var(var, "fake-credential");
        }
        let no_gh = NoGh::create().unwrap();
        let seen = child_of_a_no_gh_run(PROBE, Some(&no_gh));
        assert_eq!(seen["tokens"], "", "{seen}");
        assert_eq!(seen["flag"], "1");
        let config = PathBuf::from(seen["config"].as_str().unwrap());
        assert_eq!(config, no_gh.config);
        assert_eq!(std::fs::read_dir(&config).unwrap().count(), 0);
        assert_eq!(seen["gh"], no_gh.bin.join("gh").to_string_lossy().as_ref());

        // Without the option the same child inherits everything.
        let plain = child_of_a_no_gh_run(PROBE, None);
        assert_eq!(plain["tokens"], "fake-credential".repeat(4), "{plain}");
        assert_eq!(plain["flag"], "");
        for var in GH_TOKEN_VARS {
            std::env::remove_var(var);
        }
    }

    #[test]
    fn the_stub_gh_exits_one_and_says_why_and_is_removed_with_the_run() {
        let no_gh = NoGh::create().unwrap();
        let root = no_gh.root.clone();
        let seen = child_of_a_no_gh_run("gh pr view 1 2>err.txt\nstatus=$?\nprintf '{\"status\":%s,\"err\":\"%s\"}' \"$status\" \"$(cat err.txt)\"\n", Some(&no_gh));
        assert_eq!(
            seen,
            json!({"status": 1, "err": "gh disabled for this run"})
        );
        drop(no_gh);
        assert!(!root.exists());
    }

    #[test]
    fn result_event_yields_output_and_accounting() {
        let log = concat!(
            "{\"type\":\"system\",\"subtype\":\"init\"}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"\\\"type\\\":\\\"result\\\"\"}]}}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"session_id\":\"abc\",\"num_turns\":5,",
            "\"total_cost_usd\":0.14,\"structured_output\":{\"findings\":[]},",
            "\"usage\":{\"input_tokens\":8,\"cache_creation_input_tokens\":100,\"cache_read_input_tokens\":200,\"output_tokens\":50}}\n",
        );
        let o = parse_events(log);
        assert_eq!(o.output.unwrap(), json!({"findings": []}));
        assert_eq!(o.session_id.as_deref(), Some("abc"));
        assert_eq!(
            (
                o.turns,
                o.usage.cache_read_input_tokens,
                o.usage.output_tokens
            ),
            (5, 200, 50)
        );
        assert!((o.cost_usd - 0.14).abs() < 1e-9);
    }

    #[test]
    fn a_stream_without_usable_output_is_an_error_that_keeps_the_session() {
        assert!(parse_events("{\"type\":\"system\"}\n")
            .output
            .unwrap_err()
            .contains("without a result"));
        let prose = parse_events(
            "{\"type\":\"result\",\"is_error\":false,\"session_id\":\"s1\",\"result\":\"Here are my findings\",\"structured_output\":null}\n",
        );
        assert!(prose.output.unwrap_err().contains("Here are my findings"));
        assert_eq!(prose.session_id.as_deref(), Some("s1"));
        let err = parse_events("{\"type\":\"result\",\"is_error\":true,\"subtype\":\"error_max_turns\",\"result\":\"\"}\n");
        assert!(err.output.unwrap_err().contains("error_max_turns"));
    }
}
