# review-run

The harness behind the `kmosher-review` plugin's `/kmosher-review:review`
command. It runs a TOML graph of reviewer nodes (mechanical checks, lens
generators, an auditor that checks every finding against the code, a duplicate
merger) as separate `claude -p` processes over a checkout, and renders one
severity-sorted report. Design and node contracts: `docs/review-harness.md`.

## Install

```
cargo install --path tools/review-run
```

This puts `review-run` in `~/.cargo/bin`; keep that on `PATH`. The plugin's
command finds the harness with `command -v review-run`, so nothing else is
needed. Requirements: the `claude` CLI, logged in, on `PATH`; `git`; and `gh`
for pull request lookup (optional, a failed lookup just means no PR context).

## Use

From the plugin, run `/kmosher-review:review`. By hand:

```
review-run --graph <plugin dir>/graphs/default.toml --base origin/main > report.md
```

- `--graph`: a file, or a name resolved as `$REVIEW_RUN_GRAPHS/<name>.toml`.
- `--workspace`: the checkout (default `.`); it must have the reviewed commit checked out.
- `--base` / `--head`: the change is `base..head`; without `--head` the working tree, uncommitted changes included, is reviewed.
- `--skip NODE`, `--model NODE=MODEL`, `--effort NODE=LEVEL`: per-node overrides.
- `--dry-run` shows what would run without calling a model; `--if-changed` exits 3 if this exact change was already reviewed; `--resume` and `--reuse-from` continue or partly reuse an earlier bundle.

The report goes to stdout, one status line per node and the bundle path to
stderr. Exit codes are listed in `review-run --help`.

## Configuration

| Variable | Effect |
|---|---|
| `REVIEW_RUN_GRAPHS` | Directory searched for `--graph <name>`. |
| `REVIEW_RUN_CAPTURE_DIR` | Keep each run's bundle under `<dir>/<run id>/`. Unset, the bundle is scratch space in the system temp directory. `--out` overrides both. |
| `REVIEW_RUN_REVIEWED_DIR` | Where `--if-changed` records are kept (default `$XDG_CACHE_HOME` or `~/.cache`, under `review-run/reviewed`). |
| `REVIEW_PR_NUMBER` | Pull request number to use when the branch has none. |

## Built-in nodes

A `command` that reads `builtin:<name>` runs `review-run` itself as the node, so
the shipped graphs need no shell, `jq`, interpreter or script on `PATH`:
`builtin:prior-comments`, `builtin:changed-lint` and `builtin:context-pack`
(mechanical) and `builtin:codex-lens` (generator). They call `git`, `gh`,
`claude`, `lint-recipe`, `codex`, `go` and `curl` when present and degrade to
empty output when not. Any other executable still works as a node; see
`docs/review-harness.md` for the contract and the built-ins' behavior.

## Cost and time

Each lens is a full agent session, so cost scales with the diff. One run of
the default graph over a 53-file, +4k/-1.3k line change cost about $7 and took
about 8 minutes: roughly $2.80 for correctness, $2.10 for legibility, $1.20 for
release-engineering, $1.05 for compatibility, and cents for the auditors. The
`## Nodes` table in the report shows each node's cost and time. Use `--skip`, a
smaller graph (`quick`, `spot`) or `--model NODE=...` to trade depth for cost,
and `--dry-run` first.

## Caveats

- Reviewers read the checkout with permissions bypassed under the graph's tool allow-lists. Review code you are content for a model to read, and keep write tools out of lens nodes.
- `--safe-mode` children load no plugins or MCP servers, so `LSP` in a node's `tools` has no effect.
- Only the `claude` and `command` providers exist; a Codex-backed node is a `command` node, and the harness ships it as the built-in `builtin:codex-lens`.
