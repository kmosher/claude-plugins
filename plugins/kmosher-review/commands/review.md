---
allowed-tools: Bash(git:*), Bash(gh:*), Bash(grep:*), Bash(rg:*), Bash(ls:*), Bash(find:*), Bash(wc:*), Bash(cat:*), Bash(head:*), Bash(tail:*), Read, Write, Glob, Grep, Skill, Agent
description: Lightweight router that picks the right code-review skill(s) for the current change, runs them in the right order, and aggregates findings
disable-model-invocation: false
---

You are routing a code review across the available `kmo` review skills:

- **`review-code`** — bug-finding via per-call trace, adversarial test critique, failure-mode enumeration, concrete walkthroughs, schema/shape semantics, negative-space audit, self-verification. Always runs.
- **`review-legibility`** — readability via 11 concrete heuristic tests (one-sentence purpose, comment deletion, branch fanout, name genericization, reader onramp, reference staleness, iteration-history scars, comment-to-code distance, duplication, tests-as-documentation, self-verification). Always runs after review-code.
- **`review-compatibility`** — compat across deploy / caller boundaries: data shape (DDL, message formats, stored state) AND interface (exported signatures, public APIs, config keys, env vars, CLI flags, behavior semantics). Optional: use when the change crosses any of those boundaries.
- **`review-releng`** — operational readiness via revertability/blast-radius/observability/rollout checklist + deployment patterns + anti-patterns. Optional: use for changes touching production services, deploy infra, or anything that could page someone.
- **`review-agent-skills`** — skill-authoring quality for Claude Code skills, slash commands, and plugin manifests: frontmatter schema, description-as-trigger, body voice, supporting-file references, side-effect safety, rename consistency. Optional: use when the diff touches `**/skills/<name>/`, `**/commands/<name>.md`, `**/agents/<name>.md`, or `.claude-plugin/*.json`.

## What this command does

Given a PR or branch (default: the current branch), this command:

0. **Eligibility check** — cheap Haiku gate that skips closed/draft/trivial/already-reviewed PRs before spending any Sonnet budget.
1. **Identifies the change** — gather the diff, the files touched, and the PR description (if any).
1.5. **Prior-PR-comment mining** — surface adjudicated concerns and prior reviewer guidance from past PRs that touched these files (parallel subagent).
2. **Classifies the change** to decide which review lenses apply.
3. **Runs automated lint/diagnostic tooling** appropriate to the languages in the diff (Go, TypeScript, Rust, …) — see Step 2.5 and the sibling file `review-automated-checks.md`.
4. **Runs the relevant skills in the right order**, each in an isolated subagent that invokes the lens skill in its own context.
4.5. **Verifier pass** — a Haiku subagent re-checks each finding against the actual code, downgrades false positives, and flags pre-existing patterns.
5. **Aggregates findings** across automated tools, skill lenses, and the verifier; deduplicates; produces a single prioritized report with GitHub permalink citations.
6. **Offers to post the report as a PR comment** (skipped if `$ARGUMENTS` includes `local`, no PR exists, or no findings survived).

## Step-by-step

### Step 0: Eligibility gate

Before spending Sonnet budget, dispatch a cheap Haiku subagent to check whether
this PR is even worth reviewing. Skipping closed/draft/trivial PRs early is the
cheapest defense against wasted dispatches.

Dispatch via `Agent(subagent_type="general-purpose", description="PR eligibility check", model="haiku", prompt=...)` with a prompt covering:

- **Goal**: determine whether `/review` should proceed on this PR. Return a structured verdict; the orchestrator decides what to do next.
- **PR identifier**: `$ARGUMENTS` if it contains a PR number/URL; otherwise the current branch with `gh pr view --json title,body,number,state,isDraft,reviewDecision,comments,reviews`.
- **Repo**: `<owner>/<repo>` (derive from `gh repo view --json nameWithOwner` if needed).
- **Steps** (the subagent does these — orchestrator does not run them inline):
  1. Run `gh pr view <id> --json state,isDraft,mergeable,additions,deletions,changedFiles,comments,reviews,author`. If no PR exists for the branch, return `proceed` with a note "no PR yet, reviewing branch directly."
  2. Skip if `state == "CLOSED"` or `state == "MERGED"`. Note: drafts proceed by default — many users intentionally run `/review` on drafts before marking ready. Only skip a draft if `$ARGUMENTS` includes `skip-draft`.
  3. Skip if total `additions + deletions < 20` AND all changed files are docs (`.md`), generated, fixtures (`.golden`, `.snap`), or tests-only without source changes.
  4. Skip if the PR already has a top-level review comment from the current `gh auth status` user that starts with `# Review summary` (the marker this command's Step 5 emits) AND no new commits have landed since that review. Cross-check `pr view --json reviews,commits` — compare timestamps.
  5. Otherwise: proceed.
- **Output format** (only this, no transcript):
  ```
  verdict: proceed | skip
  reason: <one short sentence>
  pr_state: <OPEN|CLOSED|MERGED|n/a>
  is_draft: <true|false|n/a>
  size: <additions + deletions, or "n/a">
  prior_review: <none | "<sha of last review's head commit>" | n/a>
  ```

If verdict is `skip`, report the reason to the user and stop. Do not proceed
to Step 1. If `$ARGUMENTS` contains the override phrase `force`, ignore the
skip verdict and proceed anyway.

### Step 1: Identify the change

Run these in parallel:

- `git rev-parse HEAD` — current commit SHA
- `git rev-parse --abbrev-ref HEAD` — current branch
- `git log --oneline origin/main..HEAD` — commits in the change (fall back to `origin/master` if `origin/main` doesn't exist)
- `git diff --stat origin/main..HEAD` — files touched + line counts
- `git diff --name-only origin/main..HEAD` — files-changed list
- `gh pr view --json title,body,number,state` (best effort; OK if no PR exists yet)

If the user provided a specific PR number or branch name as `$ARGUMENTS`, use that instead of the current branch.

Also check for a repo-local review overlay:

- `test -f REVIEW.md && cat REVIEW.md` — if present, this is the authoritative overlay for what to flag, severity calibration, codebase precedents to treat as true, and output shape. Its rules **override the defaults baked into the lens skills.**
- If `REVIEW.md` is absent, skip silently. Falling back to skill defaults is the expected path for most repos.

Cache the contents (or absence) in a variable; you'll pass it into every lens subagent in Step 4.

### Step 1.5: Prior-PR-comment mining (parallel subagent)

Past PRs on the same files often contain adjudicated concerns ("we decided X
because Y"), pattern guidance, or reviewer comments that still apply. Mining
this is free signal that no skill body can encode — team knowledge lives in
PR threads.

Dispatch in parallel with Step 2 (classification). The orchestrator does not
read raw PR comments; the subagent distills.

Dispatch via `Agent(subagent_type="general-purpose", description="Prior PR comment mining", prompt=...)` with a prompt covering:

- **Goal**: surface adjudicated concerns and reviewer guidance from past PRs touching the files in this change. Output is passed to each lens subagent so they don't re-raise settled issues.
- **Repo**: `<owner>/<repo>`
- **Changed files**: `<file list from Step 1>`
- **Steps**:
  1. For each changed file (cap at 10 files; pick the largest by line count if more), run `gh search prs --repo <owner>/<repo> --state merged --json number,title,url -- <file>` to find recent merged PRs touching it. Cap at 5 PRs per file.
  2. For each candidate PR, fetch comments via `gh pr view <num> --json comments,reviews,reviewRequests` (limit to top-level review summaries and inline comments on the relevant file).
  3. Distill into **applicable prior guidance**: a concern, decision, or pattern the current PR might re-raise. Skip PR-specific noise (release notes, nit fixes, bot comments).
  4. Distill into **settled-issues**: concerns explicitly adjudicated ("we decided not to do X because Y"). These should NOT be raised again by the review lenses.
- **Output format** (only this):
  ```
  ## Applicable prior guidance
  <numbered list. Each: source PR # + URL, file affected, one-sentence guidance, why it might apply here>

  ## Settled issues — DO NOT re-raise
  <numbered list. Each: source PR # + URL, the concern, the adjudication reason>

  ## Coverage
  Files mined: <list>
  PRs scanned: <count>
  Files skipped: <list with reason — e.g. "new file, no prior PRs">
  ```
- **Do not** quote large comment blocks. Distill to one sentence per item. If a file has no prior PRs, say so and move on.

The orchestrator passes both lists into each Step 4 lens subagent's prompt
under "Prior PR context."

### Step 2: Classify the change

`review-code` and `review-legibility` always run. Use this decision tree to decide whether to add the optional lenses.

**Compatibility category** — add `review-compatibility` if any of:
- *Data-shape signals*: DDL files (`*.sql`, files matching `migrations/*`, `schema/*.go`); protobuf or Avro definitions (`*.proto`, `*.avsc`); files defining message formats (queue payloads, API request/response shapes); changes to serialized-state types (cache keys, stored objects); cross-system data flow (DB-to-DB migrations, dual-writes).
- *Interface signals*: changes to exported / `pub` function signatures; REST/gRPC handler bodies or routes; CLI flag definitions; config-key / env-var reads (Viper, `os.Getenv`, `process.env`, similar); default-value changes in public types; CHANGELOG / API-version-bump files; SDK / client-library code.

**Service/deploy category** — add `review-releng` if any of:
- Files in `pkg/server/`, `cmd/server/`, `services/`, `api/`, anything that's a runtime service
- IaC stack configs / deployment manifests (`*.yaml` or `*.ts` under `deploy/`, `k8s/`, `helm/`, `pulumi/`, `terraform/`, `cdk/`)
- CI / release configs (`.github/workflows/`, `Dockerfile`, `Procfile`)
- Anything that changes runtime behavior of a deployed system
- Files that touch authentication, authorization, secrets, credential handling

**Agent-skill category** — add `review-agent-skills` if any of:
- `**/skills/<name>/SKILL.md` added, modified, or deleted
- `**/skills/<name>/{references,scripts,examples}/**` changes (supporting files for a skill)
- `**/commands/<name>.md` (slash-command definitions follow the same frontmatter conventions)
- `**/agents/<name>.md` (agent definitions follow similar conventions)
- `**/.claude-plugin/plugin.json` or `**/.claude-plugin/marketplace.json`
- A directory rename under `skills/` — even with no file content changes, the lens checks rename consistency

**If everything is small and trivial** (< 20 lines changed, no new functions, doc/test only) — say so and recommend skipping the review suite.

**Security-touching changes — recommend `/security-review`.** This command's lenses don't have a dedicated security pass; security review is a separate Claude Code skill (`/security-review`, built-in). If the diff touches any of the following, mention `/security-review` in the routing announcement and suggest running it alongside `/review`:

- Auth / authz / session / token / cookie / credential handling
- New endpoints exposed to external callers
- Input parsing / deserialization of untrusted data
- Anything that calls out to external services with user-supplied data
- SQL/NoSQL query construction (especially string-built queries)
- Path / file-system operations with user-supplied paths
- Command execution (`exec`, `subprocess`, shell out)
- Template rendering with user-supplied data
- Cryptography (key handling, signing, encryption)
- Logging that may include user-supplied input (log injection risk)
- Secrets / API keys / config containing credentials

Do **not** silently fold security into the review-code lens — it's its own discipline with its own threat-model framing. The router's job is to call attention to it; the user decides whether to invoke `/security-review`.

### Step 2.5: Run automated lint/diagnostic tooling (before the human-judgment skills)

Before invoking the review skills, run the project's own lint targets and
language-specific diagnostic tools on the change. These catch mechanical issues
(unused code, modernize hints, type errors, fmt drift) that the review skills
shouldn't waste judgment on, and surface findings the IDE may not show on files
outside the open editor pane.

**Dispatch this to a subagent — do not run lint inline.** Raw lint output can
be thousands of lines; running it in the orchestrator context burns the
budget you need for aggregation. The recipes live in
`review-automated-checks.md` (sibling of this file); the subagent reads it
on demand per language.

If the diff is purely documentation, generated, or otherwise lint-irrelevant
(only `.md`, `.golden`, `.json` fixture changes), skip this step entirely
and say so explicitly in the routing announcement. Otherwise:

Dispatch via `Agent(subagent_type="general-purpose", description="Automated lint/diagnostic sweep", prompt=...)` with a prompt covering:

- **Goal**: run mechanical lint/diagnostic checks on a PR, return structured findings only. The orchestrator will fold them into the final report alongside human-judgment findings.
- **Repo path**: `<absolute path>`
- **Change**: branch `<branch>`, SHA `<sha>`, base `<base ref>`
- **Changed files**: `<list>` (so the subagent knows the scope)
- **Recipe file**: read **only the language-relevant subsection** of `<absolute path to review-automated-checks.md>`. Do not load the whole file unless multiple languages are touched.
- **Steps**:
  1. Detect languages in the diff by extension (`.go` → Go, `.ts/.tsx/.js/.jsx` → TypeScript, `.rs` → Rust). For anything else, check `Makefile`/`package.json` `scripts` for a project-level lint target and run it if present.
  2. Run the project's own lint target first (`make lint`, `npm run lint`, `cargo clippy`, etc.) — the project config is authoritative; the recipes are a floor.
  3. Run the language-specific diagnostic tools per the recipe (Go: `mcp__gopls__go_diagnostics` for changed files, `modernize` for package sweeps; TypeScript: ESLint + `tsc --noEmit`; Rust: `clippy`, `check`, `fmt`).
- **Output format** (the subagent must return only this, no transcript):
  ```
  ## Automated findings
  <numbered list. Each: severity (P0–P3 by author judgment), source `automated/<tool>` tag, file:line, what the tool said, suggested fix or "see tool output">

  ## Tools run
  <one line each: tool name, exit status, finding count>

  ## Tools skipped
  <one line each: tool name, reason — e.g. "no Go files", "make lint target not present">
  ```
- **Do not** dump raw lint output. Summarize. If a single tool produced >50 findings, group them and report counts per category rather than listing each.

### Step 3: Sequencing

Run lenses in this order. Earlier lenses can invalidate later findings, so don't parallelize:

1. **`review-code`** — always. Bugs make other findings moot.
2. **`review-compatibility`** — if selected. Compat issues are deploy-blockers.
3. **`review-releng`** — if selected. Operational concerns assume data layer is settled.
4. **`review-agent-skills`** — if selected. Skill-authoring rules are routing-correctness, not bugs; run after correctness/compat are settled so findings don't get re-classified.
5. **`review-legibility`** — always, last. Polish after correctness and ops are settled.

Print which lenses you'll run before invoking them, in the form:

```
Routing this change through:
  1. review-code (always)
  2. review-compatibility (DDL + exported-signature changes detected)
  3. review-releng (touches production service)
  4. review-agent-skills (touches plugins/foo/skills/bar/SKILL.md)
  5. review-legibility (always)
```

### Step 4: Run each selected skill **in a subagent**

For each selected skill, in order, dispatch a `general-purpose` subagent that
loads the skill in **its own** context and returns only the structured
findings. **Never invoke the review-* skills via the `Skill` tool directly
from this command** — that loads the full SKILL.md plus all upstream-reading
files into the orchestrator's context, which is exactly the cost this command
exists to avoid.

For each lens, in order:

Call `Agent(subagent_type="general-purpose", description="<lens> review", prompt=...)` with a prompt covering:

- **Goal**: run the `kmosher-review:<lens>` skill on a PR and return structured findings to the orchestrator. The skill itself encodes the technique; your job is to invoke it and follow it literally.
- **First action**: invoke `Skill(skill="kmosher-review:<lens>")` (e.g. `kmosher-review:review-code`, `kmosher-review:review-legibility`, `kmosher-review:review-compatibility`, `kmosher-review:review-releng`, `kmosher-review:review-agent-skills`). Follow the skill's instructions exactly; do not re-derive its method.
- **Repo path**: `<absolute path>`
- **Owner/repo for citations**: `<owner>/<repo>` (so the subagent can build GitHub permalinks)
- **Change**: branch `<branch>`, SHA `<sha>` (the full SHA — required for permalinks), base `<base ref>`
- **Diff**: paste the full unified diff (or, if huge, the file list + per-file hunk ranges and instructions to read full files from disk). The subagent will read further files itself.
- **PR description** (if any): paste verbatim.
- **Settled-issues list** (if the user provided one as `$ARGUMENTS`): paste verbatim, mark as DO NOT raise.
- **Repo-local `REVIEW.md`** (from Step 1, if present): paste the file's contents verbatim under a clearly labeled heading. Instruct the subagent that `REVIEW.md` rules **override** the skill's defaults — severity calibration, what to flag, what to skip, output shape. If `REVIEW.md` declares codebase precedents (e.g. "trusted env vars", "no XSS in React unless `dangerouslySetInnerHTML`"), the subagent must NOT flag findings predicated on violating those precedents.
- **Prior PR context** (from Step 1.5): paste both the "Applicable prior guidance" and "Settled issues — DO NOT re-raise" lists. The subagent treats the latter as additional settled issues.
- **Automated findings from Step 2.5** (if run): paste them so the subagent doesn't re-surface mechanical issues. The subagent may cross-reference but should not duplicate.
- **Build the "required upstream reading" list yourself.** The skill calls for 3–6 upstream files; do NOT expect the orchestrator to enumerate them. As the subagent, you derive the list from the diff by: (a) `grep` for imports in each changed file and pull the most-referenced module's interface/shim file; (b) `find` root + directory-level `CLAUDE.md` files for every directory in the changed-file list; (c) extract any PR/issue references from diff comments or commit messages; (d) the PR description's own "related issues" or "see also" links. Cap at 6 files total. Skip auto-generated files (`*.pb.go`, `*_gen.go`, lockfiles, etc.) as candidates.
- **Permalink field — mandatory on every finding**: every finding must include a `permalink` field (in addition to `file` and `line` from the canonical schema). Build as `https://github.com/<owner>/<repo>/blob/<full-sha>/<path>#L<start>-L<end>`. The range must include **at least 1 line of context above and below** the cited code (e.g. flagging line 42 → `L41-L43`; range 100–105 → `L99-L106`). Markdown won't render the preview correctly without the full SHA, so use the SHA from this step's input — do not invoke `git rev-parse` inside the citation string.
- **Output format** — the subagent must return only this, structured for mechanical consumption:

  ````
  ## findings

  ```jsonl
  {"file": "...", "line": ..., "permalink": "...", "severity": "...", "confidence": "...", "category": "...", "description": "...", "why_it_matters": "...", "recommendation": "...", <lens-specific fields>}
  {"file": "...", "line": ..., ...}
  ```

  ## upstream_reading

  ```jsonl
  {"path": "<path>", "told_me": "<one-line summary>"}
  {"path": "<path>", "told_me": "<one-line summary>"}
  ```

  ## meta

  <one paragraph, free-form. Note anything the orchestrator should know: "diff is mostly generated code, applied skill only to handwritten files"; "REVIEW.md declared 3 codebase precedents; flagged none of them"; "no novel findings, what I checked was X/Y/Z".>
  ````

  The `findings` JSONL block follows the canonical finding schema from `SHARED_CONVENTIONS.md` §3 plus any lens-specific fields documented in the lens's Output Format section. If no findings survived self-verification, emit an empty `jsonl` code block and explain in `meta`.

- **Do not** return a transcript, file dumps, narration, or any markdown rendering of findings. The orchestrator renders. The subagent emits raw structured data.

After each subagent returns:

- If any P0 findings emerged, **stop and report to the user before running the next lens**. P0s should be fixed before further review.
- If only P1/P2/P3 emerged, continue to the next lens. The user can address them in batch.
- The orchestrator's context now contains only the structured findings text (a few KB per lens), not the file reads or the skill body — that's the whole point of this step.

### Step 4.5: Verifier pass (cheap Haiku audit)

The lens subagents are smart and motivated to find issues — they have a known
false-positive bias. Before aggregating to the user, run a separate Haiku
subagent that re-checks each finding against the actual code. This catches:

- Findings that don't survive a re-read of the cited code (hallucinations).
- Findings flagging behavior that's pre-existing or intentional elsewhere in the codebase (the pattern is by-design, not a bug).
- Findings whose severity was inflated; should be downgraded.

This pass is cheap (Haiku, structured output) and catches what a single
self-review-step in the lens can't.

Skip Step 4.5 if zero findings emerged across all lenses.

Dispatch via `Agent(subagent_type="general-purpose", description="Findings verifier", model="haiku", prompt=...)` with a prompt covering:

- **Goal**: audit each finding against the actual code; produce a verdict per finding. Output is folded into the final report.
- **Repo path**: `<absolute path>`
- **Owner/repo, full SHA**: pass through from Step 1.
- **Findings to verify**: the concatenated JSONL `findings` blocks from every lens in Step 4. Assign each finding a stable global index (`<lens>:<n>`, e.g. `code:0`, `legibility:3`).
- **Settled issues + prior guidance**: pass through from Step 1.5. The verifier uses these to mark findings that re-raise adjudicated concerns.
- **For each finding, the verifier does**:
  1. `Read` the file + cited `line` (with a few lines of context). Confirm the cited code matches the `description`.
  2. If the finding cites behavior elsewhere in the codebase, `Grep` for the same pattern. If the pattern is widespread and consistent, flag the finding as "pattern is by-design, not a regression."
  3. Check whether the cited lines are actually changed in this PR or pre-existing (`git blame` the file at the SHA; if the commit isn't in the PR's commit range, it's pre-existing).
  4. Re-score severity per the strict rubric below.
  5. Cross-check against the "Settled issues — DO NOT re-raise" list; if a finding matches a settled issue, mark it as `settled` regardless of severity.
- **Strict severity rubric** (use this verbatim, do not invent intermediate levels):
  - **P0** — compilation failure, data corruption, exploitable security hole, guaranteed crash, schema migration that will fail at scale.
  - **P1** — logic error that WILL trigger under realistic production conditions; resource leak; real concurrency bug; revertability gap that becomes unfixable post-merge.
  - **P2** — edge case that COULD trigger; missing error handling; compatibility risk in a non-hot path; observability gap.
  - **P3** — code quality, minor concern, test-strength issue, legibility friction.
  - **nitpick / false positive** — pre-existing, by-design, or doesn't survive re-reading. Mark and drop from the user-facing report.
- **Output format** (only this, no transcript):

  ````
  ## verdicts

  ```jsonl
  {"index": "code:0", "verdict": "kept", "reason": "..."}
  {"index": "code:1", "verdict": "downgraded", "from": "P1", "to": "P2", "reason": "..."}
  {"index": "legibility:3", "verdict": "settled", "reason": "matches Settled-Issues #2"}
  {"index": "compatibility:0", "verdict": "false-positive", "reason": "re-read code, claim does not hold"}
  ```

  `verdict` is one of `kept`, `downgraded`, `settled`, `nitpick`, `false-positive`. Include `from` and `to` only for `downgraded`.

  ## adjusted_counts

  ```json
  {"P0": <n>, "P1": <n>, "P2": <n>, "P3": <n>, "dropped": <n>}
  ```
  ````

The orchestrator applies the verdicts in Step 5: drop findings tagged `settled`, `nitpick`, or `false-positive`; relabel severities for downgrades; preserve `kept` findings as-is.

### Step 5: Aggregate and render

The orchestrator now holds structured JSONL `findings` blocks from each lens (Step 4) and `verdicts` from the verifier (Step 4.5). Aggregation is mechanical:

- **Apply verifier verdicts first.** Drop findings whose verdict is `settled`, `nitpick`, or `false-positive`. For `downgraded` findings, set `severity` to the verdict's `to` field. Keep `kept` findings as-is.
- **Deduplicate by `(file, line, category)`.** When two lenses flag the same cell (e.g. `review-code` flags the bug, `review-legibility` flags the unclear branching), merge into one entry: keep the higher-severity framing, concatenate the `description`s, and record both lenses in a `lenses` array on the merged finding.
- **Sort** by severity (`P0` first), then by lens (run order), then by `file:line`.
- **Render** each surviving finding using the Finding render schema below. This is the first time markdown enters the pipeline.
- **Summarize** at the top: counts by severity, lenses run/skipped, verifier drop/downgrade counts.
- **Recommend next step**: fix `P0`/`P1` findings, re-run the relevant lens(es), then merge.

Every finding has a `permalink` field built by the lens subagent (full SHA, ≥1 line context above/below). Use it as the clickable header for each rendered finding.

#### Finding render schema

Plain-text severity labels (no emoji). Collapse `why_it_matters` into a
`<details>` block when it adds context beyond the description; otherwise
omit. For self-contained fixes ≤ 5 lines, append a GitHub `suggestion` block
at the **top level** of the finding (NOT nested inside `<details>` — GitHub
won't render suggestions inside details). Append a machine-readable trailer
as the last line of every rendered finding:
`<!-- kmosher-review: severity=<sev> confidence=<conf> lens=<lens> -->`

If `REVIEW.md` (from Step 1) defines its own severity labels
(e.g. `Important` / `Nit` / `Pre-existing` instead of P0–P3), use those —
`REVIEW.md` overrides the default tier names.

Final report format:

```
# Review summary for <PR/branch> (SHA <short-sha>)

Lenses run: <list>
Lenses skipped: <list, with one-line reason each>
Automated tools run: <list>
Verifier: <n findings audited; n dropped as false-positive/settled/nitpick; n downgraded>

## Findings by severity

### P0 (X findings)
- **<source lens>** — <permalink>

  **P0** — <category>

  <what's wrong (1–3 sentences, concrete)>

  *Fix:* <proposed fix>

  <details><summary>Why this matters</summary>

  <real-scenario impact>

  </details>

  *Confidence:* <low/medium/high>

  <!-- kmosher-review: severity=P0 confidence=<confidence> lens=<lens> -->

### P1 (Y findings)
[same structure, **P1** label]

### P2 (Z findings)
[same structure, **P2** label]

### P3 (W findings)
[same structure, **P3** label]

## Prior PR guidance noted (informational)
<list from Step 1.5 "Applicable prior guidance" that wasn't directly raised as a finding>

## Recommended next steps

[What to fix first; what to defer; whether to re-run any lens after fixes]
```

If no findings survived verification: say so explicitly. The PR is ready to merge from these lenses' perspectives. Mention how many findings the verifier dropped so the user knows the verifier ran and did its job.

### Step 6: Offer to post the report to the PR

After printing the report, decide whether to offer posting it as a PR comment:

- **Skip the offer entirely** if any of: `$ARGUMENTS` contains `local` / `no post` / `don't post`; no PR exists for the branch (Step 1's `gh
  pr view` returned no PR); zero findings survived verification (nothing useful
  to post).
- **Otherwise, ask the user** with a single concise prompt: `Post this review as
  a comment on PR #<num>? (yes / no )`. Unless the user has already indicated
  intent they want the review posted

If the user says yes, post via `gh pr comment <num> --body-file <path>`. Write the comment body to a file under the session tmpdir. The comment body should be the same final report from Step 5, with two adjustments:

1. Replace the top-line `# Review summary for <PR/branch> (SHA <short-sha>)` with `## Review summary (SHA <short-sha>)` — GitHub renders the `##` better in a PR comment, and the PR number is implicit.
2. Append a trailing line: `<sub>Generated by the [kmosher-review](https://github.com/kmosher/claude-plugins) skills suite. React 👍 if useful, 👎 if not.</sub>`

Do not auto-post without clear intent or confirmation. Posting to a PR is visible to others.

## Notes for the orchestrator

- **Stay in routing mode, not reviewing mode.** Your job is to dispatch subagents and aggregate their output. You should never read the files being reviewed yourself; never run lint inline; never load the review-* skills via `Skill` in this conversation. The subagents do all of that in their own context.
- **Never manufacture findings.** If a subagent reports no novel issues, faithfully relay that. Padding the report wastes the user's attention.
- **Don't re-derive the skill's logic.** Each skill encodes its own technique; the subagent invokes it. Pass the change, accept the output.
- **Confidence calibration matters.** Pass through each finding's confidence label (low/medium/high). Low-confidence findings are still surfaced but called out as such.
- **Auto-route, but explain.** Always tell the user which lenses you picked and why before dispatching subagents. They may want to override (e.g. "only legibility, the correctness is settled").
- **Respect the user's override.** If `$ARGUMENTS` includes a hint like "only legibility" or "skip migration", honor it rather than the auto-routing. The two always-run lenses (review-code, review-legibility) can also be skipped on explicit request.

## Arguments

`$ARGUMENTS` may contain:
- A PR number (e.g. `3405` or `#3405`)
- A branch name
- An override phrase (e.g. `only legibility`, `skip migration`, `all lenses`, `quick`)
- `local` (or `local review`, `no post`, `don't post`) — suppresses the Step 6 offer to post the report as a PR comment
- Combinations (e.g. `3405 only code`, `3405 skip legibility`, `3405 local`)

If empty, default to the current branch with auto-routing.
