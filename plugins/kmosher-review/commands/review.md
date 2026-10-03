---
allowed-tools: Bash(git:*), Bash(gh:*), Bash(date:*), Bash(command:*), Bash(test:*), Bash(review-run:*), Bash(/opt/metawork/bin/review-run:*), Bash(tail:*), Bash(jq:*), Bash(mv:*), Read, Write, Agent
description: Code review of the current change, run by the deterministic `review-run` harness — lenses, auditor, severity-sorted report, capture bundle. Offers to post the report to the PR.
disable-model-invocation: false
---

You are the wrapper around `review-run`, a harness that runs the `kmosher-review` lenses (correctness, compatibility, release-engineering, agent-skills, legibility) as separate `claude -p` processes over the checkout, audits every finding against the code, and renders one report. Lens selection, repo conventions (`REVIEW.md`/`AGENTS.md`/`CLAUDE.md`), the auditor, sorting and capture all happen inside the harness. Your job is the gate before it and the presentation after it. You do not read the code, run lenses, or judge findings.

## Step 0: Find the harness

`command -v review-run`, else `test -x /opt/metawork/bin/review-run`. Call whichever answers `<review-run>`.

If neither exists, or `$ARGUMENTS` contains `router`, say so in one line (`review-run not found — using the router` / `router requested`), then Read `${CLAUDE_PLUGIN_ROOT}/commands/review-router.md` and follow it from the top with the same `$ARGUMENTS`. Nothing below applies.

## Step 1: Eligibility gate

Dispatch `Agent(subagent_type="kmosher-review:eligibility-gate", description="PR eligibility check", prompt=...)`. The agent definition carries no checklist, so the prompt must:

- **PR identifier**: `$ARGUMENTS` if it names a PR number or URL; otherwise the current branch. **Repo**: `<owner>/<repo>`.
- **Steps**:
  1. `gh pr view <id> --json state,isDraft,mergeable,additions,deletions,changedFiles,comments,reviews,commits,author`. No PR for the branch: `proceed`, reason "no PR yet, reviewing branch directly".
  2. Skip if `state` is `CLOSED` or `MERGED`. Drafts proceed unless `$ARGUMENTS` includes `skip-draft`.
  3. Skip if `additions + deletions < 20` and every changed file is docs (`.md`), generated, a fixture (`.golden`, `.snap`), or tests with no source change.
  4. Skip if the PR has a top-level comment from the `gh auth status` user starting `## Review of` or `## Review summary` and no commit landed after it (compare timestamps in `comments` and `commits`).
  5. Otherwise `proceed`.
- **Output**, only this:
  ```
  verdict: proceed | skip
  reason: <one short sentence>
  pr_state: <OPEN|CLOSED|MERGED|n/a>
  is_draft: <true|false|n/a>
  size: <additions + deletions, or "n/a">
  prior_review: <none | "<sha of the last review's head commit>" | n/a>
  ```

On `skip`, report the reason and stop, unless `$ARGUMENTS` contains `force`.

## Step 2: Identify the change

Run in parallel, from the repo root (`git rev-parse --show-toplevel`):

- `git rev-parse HEAD` — head SHA
- `git merge-base origin/main HEAD` — base SHA (fall back to `origin/master`)
- `git rev-parse --abbrev-ref HEAD` — branch
- `git status --porcelain` — empty means a clean tree
- `git diff --stat <base>` — size and files touched
- `gh pr view --json title,body,number,state` — best effort; no PR is fine
- `gh repo view --json nameWithOwner`

If `$ARGUMENTS` contains `since <sha>`, use that SHA as the base instead of the merge-base.

If `$ARGUMENTS` names a PR or branch whose head is not what is checked out (`gh pr view <n> --json headRefOid`, or `git rev-parse <branch>`, differs from HEAD), say so and stop: the harness reviews the checkout it is pointed at. Tell the user to check that branch out first.

If the diff is trivial (under 20 changed lines, docs or tests only), say so and recommend skipping, then continue only if the user insists.

Security-touching changes — auth, tokens, credentials, new external endpoints, parsing untrusted input, query or shell construction, user-supplied paths, templating, cryptography, secrets in config: the lenses have no security pass. Recommend `/security-review` alongside this one.

## Step 3: Run the harness

Graph, from `$ARGUMENTS`:

- `defer-comments` if it contains `defer-comment-judgment` — the legibility lens leaves comment prose to a later comment pass (`kmo:finalize`) and still reports wrong or misplaced comments.
- `with-lint` if it contains `lint` — adds a mechanical `lint` node that runs the repo's lint tools over the changed files and feeds their diagnostics into the report.
- `default` otherwise.

Lens overrides: node names are `code`, `compatibility`, `releng`, `agent-skills`, `legibility`, `auditor`. `only legibility` means `--skip` every lens but that one; `skip releng` means `--skip releng`. Lenses whose triggers don't fire are skipped by the harness on its own.

Start one Bash call with `run_in_background: true` and `dangerouslyDisableSandbox: true` (the harness launches `claude -p` children, which the sandbox blocks):

```
<review-run> --graph ${CLAUDE_PLUGIN_ROOT}/graphs/<graph>.toml --workspace <repo root> --base <base sha> [--head <head sha>] [--skip <node>]... > <tmp>/review-run.out 2> <tmp>/review-run.err
```

`<tmp>` is the session temp directory from your system prompt. Pass `--head` only when the tree is clean; a dirty tree is reviewed as it stands, uncommitted changes included. Tell the user the graph, the base and head being reviewed, and that it takes about 5–8 minutes. Then wait for the completion notice; do not poll.

## Step 4: Present

When the command finishes, Read `<tmp>/review-run.out` and print it verbatim as your reply. You are not a second filter: do not trim, re-sort, re-word or re-judge a finding.

Read `<tmp>/review-run.err` (`tail` it if long). It holds one status line per node and ends with `bundle: <path>`. After the report, add one line per node that failed or was skipped, with the reason from its status line. Then by exit code (the background completion notice reports it):

- `0` — nothing more.
- `1` — say plainly that the report is incomplete, which node failed and why.
- `2` — the run never started or its bundle could not be written: show the last lines of stderr and offer `/kmosher-review:review-router` as a fallback. Stop here.

End with one line, `run captured to <path>`, taken from the `bundle:` line exactly as printed. It goes nowhere else — not into the posted comment. If there is no `bundle:` line, say nothing about capture.

## Step 5: Offer to post

Skip the offer if `$ARGUMENTS` contains `local`, `no post` or `don't post`; if Step 2 found no PR; or if the report has zero findings.

Otherwise ask once: `Post this review as a comment on PR #<num>? (yes / no)`, unless the user has already said they want it posted. Posting is visible to others; never post without a yes.

On yes, write the report to `<tmp>/review-comment.md` with two changes, then `gh pr comment <num> --body-file <tmp>/review-comment.md`:

1. Replace the first line `# Review of <head> against <base>` with `## Review of <head> against <base>`; Step 1's dedupe keys on that prefix.
2. Append `<sub>Generated by the [kmosher-review](https://github.com/kmosher/claude-plugins) skills suite. React 👍 if useful, 👎 if not.</sub>`

Record where it went, in the same turn as the post. `gh pr comment` prints the comment URL; its `#issuecomment-<id>` fragment is the id. If `<bundle>/manifest.json` still exists, get the time from `date -u +%Y-%m-%dT%H:%M:%SZ`, run `jq --argjson pr <num> --argjson cid <id> --arg at <time> '. + {posted_pr: $pr, posted_comment_id: $cid, posted_at: $at}' <bundle>/manifest.json > <tmp>/manifest.json`, then `mv <tmp>/manifest.json <bundle>/manifest.json`. The harness manifest has no `posted_*` keys; this adds them as JSON values (`posted_pr` an integer), touching nothing else. If the bundle is gone, skip silently — ingest has taken it. A failure here never blocks or retries.

## What this path doesn't do

No prior-PR comment mining, no codex cross-model pass, no GitHub permalinks in the report (findings cite `file:line`). `/kmosher-review:review-router` runs those steps; it is the previous model-run flow.

## Arguments

`$ARGUMENTS` may contain:
- A PR number or branch name — checked against the checkout per Step 2, not switched to
- A lens override (`only legibility`, `skip releng`, `skip code`)
- `since <sha>` — measure the change from that commit instead of the merge-base
- `local` (or `no post`, `don't post`) — no offer to post
- `force` — proceed past an eligibility `skip`
- `skip-draft` — let the gate skip drafts
- `defer-comment-judgment` — use the `defer-comments` graph. Set by `kmo:polish`, whose next stage is `kmo:finalize`.
- `lint` — use the `with-lint` graph
- `router` — run the previous model-run flow instead

`skip codex` is gone: codex runs only in the router.

If empty, review the current branch against its merge-base.
