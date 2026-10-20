# The review harness

`review-run` runs a code review as a graph of nodes, each a child process:
mechanical checks first, then lens generators, then a critic that audits
every finding against the code, a merger that folds duplicates, and one
severity-sorted report. Prompts and graphs live in the `kmosher-review`
plugin; this program supplies the scheduling, the contracts and the
bookkeeping. See `tools/review-run/README.md` for installation and flags.

## Why a program and not a skill

- **A subagent inherits the session.** Every lens starts at 67–81k tokens of
  MCP listings, CLAUDE.md, memory and skill catalogue before it reads a line
  of the diff, and re-reads it every turn; for a 36-turn lens that base is
  ~80% of its cache reads. A `claude -p` child sheds it: no MCP block, a
  lens-specific system prompt, a short tool list. 
- **Structured output is enforceable.** `claude -p --json-schema` returns
  validated JSON; free-text lens reports sometimes arrived without their `## findings`
  header and had to be recovered by row shape.
- **A lens is a command line.** Claude at any model and effort, or any
  executable speaking the `command` provider's contract (a Codex wrapper, a
  linter), all producing the same JSONL. Cheaper models and cross-model
  critics become configuration.
- **The mechanical rung wants an exit code.** Lint, typecheck, LSP
  diagnostics and the triggers that select later nodes are deterministic and
  unwaivable; a prompt asking an agent to run them is the weakest form.
- **Every run leaves a bundle.** The harness writes a directory with each
  node's input, output, wall-clock, tokens and exit status, so a run can be
  inspected, resumed, or partly reused.

What it gives up: a subagent's warmed cache and its streaming into the
session. The first is recovered with `--fork-session` (below); the second by
the harness printing one status line per node.

## The graph

A review is a directed acyclic graph of nodes. Node kinds:

| kind | reads | writes | gate |
|---|---|---|---|
| `mechanical` | workspace | diagnostics, triggers | exit code; a failure stops the graph |
| `prewarm` | workspace, diff | a session id and a context artifact | must end on a summary turn |
| `generator` | a prewarm session or the diff | findings JSONL | schema-validated, one retry |
| `critic` | findings, minimal context | verdicts JSONL | never gates on agreement |
| `merger` | reported findings, post-verdict | groups of duplicates | schema-validated, one retry; a failure leaves findings unmerged |
| `aggregate` | everything | the report and the bundle | |

A node starts as soon as everything it waits for has finished and a
concurrency slot is free (`--concurrency`, default 4); there are no waves, so a
slow node delays only what needs it. A node that is skipped is settled the
moment its turn comes and takes no slot. `node_start` and `node_end` events are
emitted when a node actually starts and ends, while `manifest.json` lists
nodes in graph order whatever order they finished in.

Edges carry artifacts, not prose. A generator edge from a prewarm node means
"fork from that session"; from the diff it means "start cold with this diff
and this system prompt". Every node that is not mechanical implicitly waits for
all mechanical nodes, so a graph author does not list them in `needs`.

A generator may `need` other generators (and only generators; the needs must
not form a cycle). It starts once they have finished, whether they succeeded,
were skipped or failed, and its prompt lists their reported findings as already
reported, so cheap first-pass readers can feed the expensive lenses.

### Triggers

A graph's `[triggers]` table names predicates the harness evaluates itself
from the change:

```toml
[triggers]
ddl = { touches = ["**/migrations/**", "**/*.sql"] }
wire-format = { touches = ["**/*.proto"], diff_matches = "^\\s*(rpc|message) " }
```

`touches` is a list of globs over changed paths (`*` stays within one path
segment; `**` crosses them). `diff_matches` is a regex tested against the text
of each added or removed line, marker excluded; file headers are never tested.
Either firing fires the trigger, and a trigger must have at least one.

A node declares `trigger = ["ddl", "wire-format"]` and runs if any of them
fired. Otherwise it is recorded `skipped` with the reason `no trigger fired
(ddl, wire-format)`, and a critic whose generators were skipped audits the
rest. A mechanical node declares the names it may raise in `emits`; those
names are valid in `trigger` without a `[triggers]` entry, and raising any
other name is a node failure. A mechanical node cannot itself have a trigger.
Skipping is a decision of the graph, never of a model.

### Mechanical nodes

A mechanical node is a `command` node; a relative `command` path containing a
slash is resolved against the graph file, since children run in the workspace.
It receives JSON on stdin with `node`,
`workspace`, `base`, `head`, `changed_files`, `diff` (the path of
`diff.patch` in the bundle), `out`, `pr_number` and `pr_body`, and writes JSON
on stdout. `out` is `nodes/<name>/out` in the bundle, created before the node
runs and kept with it: write files there for later nodes and point at them by
absolute path in `context` lines. A later run with `--reuse-from` copies it
into the new bundle before the node runs again. `pr_number` and `pr_body` come
from `gh pr view` for the checked-out branch, each null when GitHub has no
pull request or does not answer within five seconds; a known body is also
written to `pr-body.md` in the bundle.

```json
{"diagnostics": [{"tool": "golangci-lint", "file": "a/b.go", "line": 12, "message": "..."}],
 "triggers": ["exports-changed"]}
```

`file` and `line` may be null; `triggers` defaults to empty. A non-zero exit,
a timeout or invalid output fails the node without retry, and a mechanical
failure stops the graph: every node not yet run is recorded skipped, the
report says it is incomplete, and the bundle is still written. Diagnostics go
to `diagnostics.jsonl`, the report's `## Mechanical` section, and every
generator's prompt as already-reported findings it must not repeat; the
manifest records their count and `triggers_fired`, each with what fired it.

Optional `"context": [{"tool": "...", "message": "..."} | "..."]` carries
pointers to files outside the diff, not findings; `tool` defaults to the node
name. Every generator's prompt lists them as `  <tool>: <message>` under a
"Context from mechanical tools" heading, after the changed-file list and before
conventions, diagnostics, earlier findings and the diff. They are kept in the
node's `output.json` (tool filled in) and not counted in the manifest.

A critic with `per_need = true` is not run once after all its generators but
once per needed generator, each instance starting when that generator finishes
with at least one finding and auditing only that generator's findings (indices
unchanged, `<generator>:<n>`), concurrently with generators still running. An
instance is its own child with its own `nodes/<critic>-<generator>/` and its own
manifest record of that name, kind `critic`, so cost and time stay attributable.
A generator with no findings, or one that failed or was skipped, yields no
instance and no record; if no instance runs the critic is recorded once as
skipped. `--skip`, `--model` and `--effort` on the critic apply to every
instance, and `verdicts.jsonl` is their union. The rule against a second critic
counts graph nodes, not instances.

A graph may have one `merger`, which waits for every generator and every critic
instance. The auditor runs once per lens and never sees another lens's
findings, so two lenses describing one defect arrive as two findings; on the
saved reviews about a third of reported findings sat within five lines of one
from a different lens, mostly code and legibility. The merger is handed the
findings that would be reported (index, file, line, post-verdict severity,
category, description, recommendation) and returns `{"groups": [{"indices":
[...], "reason": "..."}]}`, rejected and retried once when an index is unknown,
in two groups, or a group has fewer than two. It is skipped, like a critic with
nothing to audit, when no file has reported findings from two different lenses.
It takes `model`, `effort`, `timeout` and `system_prompt` like any model node,
and needs no tools.

`aggregate` applies the groups after verdicts. The primary is the member with
the highest severity, the earliest given on a tie; the rest leave the report and
stay in `findings.jsonl` with disposition `duplicate` and `duplicate_of`. The
primary gains their lenses and a line `Also raised by <lens> (<index>): <reason>`;
descriptions are not concatenated. The merger's raw groups are `merges.jsonl`.
A failed merger leaves the findings as they were and the node failed, so the
report is never lost to it. The exact `(file, line, category)` merge is
unchanged and runs first.

Consensus is not a gate anywhere. Critics are given isolated context so they
cannot anchor on the generator, and a critic from another model family is
preferred where one exists; agreement between same-family agents measures
shared priors, and the only thing that falsifies a finding is execution.

### A typical graph

```
mechanical ──► gen:code ──────────────┐
               gen:legibility ────────┤
               gen:compatibility ─────┤  trigger: ddl|wire-format|...
               gen:agent-skills ──────┤  trigger: agent-skills
               gen:releng ────────────┤  trigger: deploy|auth|...
                                      ▼
                         critic:auditor ◄── findings
                                      ▼
                         merger:dedupe
                                      ▼
                                 aggregate
```

The `kmosher-review` plugin ships several graphs (`default`, `quick`, `spot`,
`with-lint`, and variants); a graph is a TOML file, so a new topology is a file
and not a release. The harness ends at aggregate. A finding is a brief for an
implementing agent and carries `invariant` and `verify`; applying and verifying
it is the consumer's job.

## Node contract

Nodes run in the checkout given by `--workspace`, which must have the
reviewed commit checked out. A node receives the base, head, a real unified
patch, and, when forked, the prewarm session, and returns findings or verdicts
in the canonical schema (every finding names its `invariant` and `verify`),
validated against the schema before anything downstream sees it.
A node that returns something else gets one retry with the validation error;
a second failure is a node failure, reported, never a silent empty result.

Per-node configuration: `provider` (`claude`, `command`), `model`,
`effort`, `system_prompt` (files), `tools` (allow-list), `timeout`,
`max_turns` (`--max-turns`; unlimited if unset), `conventions` (below),
`fork_from` and `read_budget_bytes` (below), and for `claude` nodes whether to
strip MCP servers (default yes). `max_turns` and `read_budget_bytes` can also
be set in `[defaults]`. Nothing loads CLAUDE.md for a child; the harness hands
it the project's written rules itself. The harness owns
permissions: children run with permissions bypassed and with a tool allow-list
that each node's `tools` sets. Run it on a checkout you are content for the
reviewers to read freely; write access is a matter of the allow-list in the
graph, so keep `Write`, `Edit` and `Bash` out of lens nodes.

### Conventions

A cold child has no `AGENTS.md`, `CLAUDE.md` or `REVIEW.md`, and a model asked
to find the ones that apply reads some and misses others, so the harness decides.
For the changed files it collects every such file that exists in the workspace
at the repo root or in any ancestor directory of a changed file, root first and
then by depth, each kept once however it is reached (a symlink to another
collected file, an import). A file's own directory is its scope. A `CLAUDE.md`
whose only content is `@`-imports of collected files is dropped; any other
whole-line `@path` import, in any collected file, is resolved one level
relative to the importing file and added, scoped to the importer. Symlinks and
imports that leave the workspace are not followed.

`conventions` is set in `[defaults]` and overridable per node:

- `inline`: a generator's prompt carries each file's path, scope and contents,
  states that a violation of a written rule in changed code is a finding citing
  the rule's file and heading, and lists by path and size any file that did not
  fit `conventions_budget` (bytes, default 150000) to be read before reviewing
  code under its directory. A file too big is passed over; a later smaller one
  can still be inlined.
- `list` (the default): paths and scopes only, with the instruction to read each.
- `none`: nothing.

A critic is given the `list` form whenever the generators it audits got either
of the first two, so a cited rule can be checked. The manifest records under
`conventions` each collected file's path, scope, bytes and whether an inline
node saw it inlined, with the budget and each generator's mode. `--dry-run`
prints the same.

### Pre-warm and fork

A `prewarm` node is a `claude` node that waits only for the mechanical nodes,
is given the sections a generator gets (change header, changed files, context
pointers, conventions, diagnostics, diff) and is told to read, not judge: the
diff, every changed file in full, every file the context section names, every
convention file not inlined, and the definition and callers of each symbol the
diff adds or changes (with `LSP` in its tools, by definition and references;
otherwise with Grep). It finishes with the generator output object, `findings`
empty and in `meta` a summary of four sections, `## Files`, `## Symbols
changed`, `## Conventions in scope` and `## Open questions`; findings, or a
summary missing a section, are sent back once, and a second miss fails the
node. Its session is kept on disk, because forks resume it from there.

- `read_budget_bytes` (node or `[defaults]`, default 400000): the bytes of file
  content the prewarm is told it may read in total. It is an instruction; the
  harness does not count. `max_turns` is enforced by the CLI, and a prewarm that
  hits it is asked once to stop reading and write the summary.
- `fork_from = "<prewarm node>"` on a generator starts it with
  `claude -p --resume <session> --fork-session`: a copy of the prewarm's
  session, so forks run side by side without touching it or each other. The
  fork's `tools` must equal the prewarm's (a graph that differs is refused),
  and it runs under the prewarm's `system_prompt`. A fork's own
  `system_prompt` files are therefore not a system prompt: they are its lens,
  and open its user turn under `Your lens:`, followed by a
  `lens instructions assembled from:` line so sibling files they name can be
  found, then `Review this change with your lens. Everything you read in the
  prewarm is still in context; cite it, re-open a file only when you need lines
  you did not read.` The diff and the file list are not repeated. A fork
  needing other generators still lists them in `needs`.
- The prewarm and its forks are all run with the same `--json-schema`, the
  generator schema. The flag adds a `StructuredOutput` tool to the tool list the
  cached prefix begins with, so a fork given another schema, or none, would miss
  everything the prewarm cached; with the same one the fork reads the prewarm's
  whole cache write (the prewarm's cache write is read in full by each fork). The session therefore ends on the `StructuredOutput` call and
  its result, and a fork resumes from there. Forks keep constrained output and
  the usual single retry in their own session.
- A fork on another `model` than the prewarm still forks, but its cache cannot
  be shared across models: the manifest records a warning on that node and the
  report repeats it. A different `effort` shares the cache.
- If the prewarm failed, was skipped or left no session, each fork runs as a
  plain generator (below) and says so in a warning.
- Reuse treats a fork's lens, which is in its user turn, as part of the node: a
  changed lens reruns that fork only, and a rerun prewarm reruns all its forks.

The harness sets `CLAUDE_CODE_PROMPT_CACHE_TTL=1h` for the prewarm and its forks
(the manifest records `cache_ttl`). The CLI's own default is an hour on a
subscription and five minutes on an API key. Forks launch as soon as the
prewarm ends, so the five-minute default would also hold. Because the CLI
records a session's system prompt on its first request and sends that record on
every resume, a fork cannot drift from the prewarm's prompt even if its flags
did; the harness builds both from the same files regardless.

`--no-prewarm` leaves prewarm nodes out (recorded as skipped) and runs every
fork as a plain generator: the prewarm's `system_prompt` files followed by the
fork's own, a cold start with the diff in the prompt. A
graph can be run both ways. The manifest has `prewarm_session_id` and
`no_prewarm`, and each fork record has `fork_from`.

`--no-gh` cuts every node off from GitHub, for replaying a change whose review
already exists there. It applies after the harness's own `gh pr view` and
`git remote` lookups, so the PR number and `owner/repo` still resolve. Each
child, reviewer or mechanical script, then runs with `GH_TOKEN`, `GITHUB_TOKEN`,
`GH_ENTERPRISE_TOKEN` and `GITHUB_ENTERPRISE_TOKEN` removed, `GH_CONFIG_DIR`
pointing at an empty directory, a `gh` that exits 1 first on `PATH`, and
`REVIEW_RUN_NO_GH=1` set. The scrub is the control: the `PATH` stub only spares
a reviewer from burning turns, and is bypassed by an absolute path to `gh`.
Reviewers' prompts open with a notice that GitHub is disabled. Mechanical
scripts that use `gh` should return nothing when `REVIEW_RUN_NO_GH` is set; the
manifest flags each mechanical node's record with a warning. The manifest has
`no_gh`, and `--reuse-from` and `--resume` refuse a bundle made with the other
setting. Not covered: anything else a child can reach on its own, such as `curl`
to a public repository's API or a credential read from the keychain.

`LSP` is accepted in `tools`, but children start with `--safe-mode`, which
loads no plugins and so no language server; the tool is not usable there.

## Providers

- **`claude`**: `claude -p --safe-mode` with `--output-format stream-json`,
  `--json-schema`, `--model`, `--effort`, `--system-prompt-file`, `--tools`,
  `--max-turns`, and `--resume <id> --fork-session` for a fork; the prompt goes
  on stdin. `--safe-mode` starts without CLAUDE.md, skills, plugins, hooks and
  MCP servers. It uses whatever authentication the `claude` CLI has;
  `--bare` skips keychain reads and cannot log in with a subscription.
- **`command`**: any executable that reads the node envelope (`node`,
  `workspace`, `prompt`, `schema`) on stdin and writes the generator output on
  stdout. A Codex wrapper, a linter adapter, another model.

## The bundle

Every run writes a directory: `manifest.json`, `findings.jsonl`,
`findings-<node>.jsonl`, `verdicts.jsonl`, `merges.jsonl`, `diagnostics.jsonl`,
`diff.patch`, `report.md`, and per node under `nodes/<name>/` the exact
command, model and effort, the streamed event log, stderr, wall-clock, tokens,
and the prewarm session id. The bundle is how `--resume` and `--reuse-from`
work, so it is always written, and a run whose bundle cannot be written fails.

Where it goes is the only thing capture configures: `--out <dir>` writes
exactly there, `REVIEW_RUN_CAPTURE_DIR=<root>` writes `<root>/<run id>/` so
bundles accumulate for later inspection, and with neither the bundle is
scratch space under the system temp directory. The format is the same
either way. The final line on stderr is `bundle: <path>`.

### The report

`report.md` is rendered from `findings.jsonl` and nothing else. When the run's
origin is on GitHub and the reviewed head is a commit rather than a working
tree, a finding's location links to that commit's copy of the file, anchored to
the cited line and one either side; the URL is also the row's `permalink`.

When every reported finding is P3 the report says `No blocking issues.` under
the counts. The counts line gains `, <m> merged as duplicates` when the merger folded any away.

### Reuse

`--reuse-from <bundle>` takes finished nodes from an earlier bundle of the same
change instead of running them, so an experiment that alters one node costs that
node. A node is taken when the bundle's `base_sha`, `head_sha` and `diff.patch`
match this run's and its record there is `ok`, of the same kind, on the same
model and effort (after `--model` and `--effort`), with the same tools or
command and a byte-identical `nodes/<name>/system-prompt.md`; and everything it
reads from other nodes was taken too (or left out both times): a generator its
`needs`, a critic its generators, a per-need instance its own generator, a
merger every generator and critic. A rerun anywhere upstream reruns what is
downstream. Mechanical nodes always run. A taken node's directory and record
are copied across with `reused_from` set and `cost_usd` and `wall_ms` zero, and
its output (`nodes/<name>/output.json`, written by every run) feeds triggers,
findings, verdicts and merges as if it had just run. Progress prints
`<name>: reused`, `--json` emits `node_reused`, and `--dry-run` lists what would
be taken. A bundle written before `output.json` existed supplies nothing.

`--resume <bundle>` finishes an interrupted or partly failed run in its own
bundle under the same rule: every node that finished ok and has nothing changed
upstream stays, with its record and cost as they were; a failed, missing or
never-reached node runs now, with what its earlier try spent added to its cost,
and so does everything downstream of it. The graph, and every node's model and
effort, must be those the bundle records, or the resume is refused naming the
field; so must the change (`base_sha`, `head_sha`, `diff.patch`). The manifest
keeps the original `run_id` and `started_at` and gains `resumed_at`, one entry
per resume. To make that possible after a kill, each node's `output.json` is
written the moment it is accepted and `manifest.json` is rewritten (through a
rename) after every node finishes, carrying `"in_progress": true` until the run
ends; a reader that treats a manifest as a finished run must check that field.

## Interface

```
review-run --base <commit> [--head <commit>] [--graph <file|name>] [--workspace <dir>]
           [--skip <node>] [--model <node>=<model>] [--effort <node>=<level>]
           [--reuse-from <bundle> | --resume <bundle>] [--pr <number>]
           [--no-prewarm] [--no-gh] [--if-changed] [--concurrency <n>]
           [--out <dir>] [--dry-run] [--json]
```

`--graph` is a file path, or a name looked up as `$REVIEW_RUN_GRAPHS/<name>.toml`.
Without `--head` the working tree is reviewed, uncommitted changes included.
`--pr <number>` names the pull request for a checkout that is not on its
branch, such as a detached historic commit; without it the branch's pull
request is looked up, then `$REVIEW_PR_NUMBER`, and the resolved number is the
manifest's `pr_number` (null when unknown).

`--if-changed` runs nothing and exits 3 when the last complete run of the graph
on this checkout had the same base, head, diff, untracked files, graph, prompt
and convention files, overrides and tool version. The record is kept under
`$REVIEW_RUN_REVIEWED_DIR`, else `$XDG_CACHE_HOME/review-run/reviewed`, else
`~/.cache/review-run/reviewed`.

`--dry-run` prints each trigger and whether it fired and why, the resolved
graph, which nodes would run, and the commands that would run. A trigger a
mechanical node decides is shown as `decided by <node> at run time`, and a node
that depends on one prints `runs if <names> fires`. `--json` streams node events
as JSON lines for a caller to render. Exit codes: 0 every node ran, 1 the report
is incomplete because a node failed, 2 the run could not start or its bundle
could not be written, 3 `--if-changed` found nothing new.

## Implementation

A Rust crate. Std threads run the process tree; `serde` and `toml` load the
graph; `clap` is the surface. Node execution is one trait with two
implementations, one per provider; the graph runner is a dependency-driven
scheduler with a slot count, so concurrency is a number. Tests stub the
providers with canned event streams and assert the graph's decisions: what
fired, what was skipped and why, what was retried, what the bundle contains.
