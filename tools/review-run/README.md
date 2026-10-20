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

## Cost and time

Each lens is a full agent session, so a review of a mid-sized change takes
several minutes and spends on the order of a dollar or more of model usage;
the graph decides. Use `--skip`, a smaller graph (`quick`, `spot`) or
`--model NODE=...` to trade depth for cost, and `--dry-run` first.

## Caveats

- Reviewers read the checkout with permissions bypassed under the graph's tool allow-lists. Review code you are content for a model to read, and keep write tools out of lens nodes.
- `--safe-mode` children load no plugins or MCP servers, so `LSP` in a node's `tools` has no effect.
- Only the `claude` and `command` providers exist; a Codex-backed node is a `command` node wrapping the CLI.
