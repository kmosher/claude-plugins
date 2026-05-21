# Shared Conventions for `kmosher-review` Lens Skills

Four conventions apply to every lens skill (`review-code`, `review-legibility`, `review-compatibility`, `review-releng`). Read this file when invoked. Each lens applies its own technique on top.

These were adapted from the github-bot review prompt — patterns proven in production on an autonomous reviewer that ships findings without a human in the loop.

---

## 1. REVIEW.md overlay (read FIRST)

Before applying any lens, check whether the repo has a `REVIEW.md` file at the root. If it exists, **read it before doing anything else** and treat its contents as the highest-priority instructions for this review.

`REVIEW.md` may:

- Redefine what counts as **Important** vs **Nit** for the repo.
- Declare codebase precedents — assertions the lens should treat as true and **not flag** ("Cloudflare Workers run in V8 isolates, no memory-safety findings apply", "env vars from our secret manager are trusted", etc.).
- Cap nits (e.g. "report at most 5 Nits per review").
- Forbid specific finding categories (e.g. "don't report anything CI already enforces").
- Add repo-specific must-checks (e.g. "any change under `packages/sandbox-runtime/` must bump `CACHE_BUSTER`").
- Specify summary shape.

When `REVIEW.md` rules conflict with this skill's defaults, **`REVIEW.md` wins.** Severity calibration, what to flag, what to skip, output shape — all overridable.

If `REVIEW.md` does not exist, fall through to the skill's defaults.

The `/review` command reads `REVIEW.md` once at the top level and passes its contents into every lens subagent's prompt, so a subagent invoked from `/review` already has it. When a lens is invoked directly (not via `/review`), check for `REVIEW.md` yourself.

## 2. Pattern propagation

**When you find an issue that could be a pattern, immediately scan all other files in the diff for the same pattern before moving on.**

Most bug-shaped findings repeat. A missing `await` in one handler is usually a missing `await` in three handlers. A wrong copyright year in one file is wrong everywhere. A predicate-at-evaluation-site bug in one switch is likely the same in adjacent switches. A schema validation gap on one endpoint usually applies to the sibling endpoints touched in the same PR.

Drip-feeding these across review rounds wastes the author's time and erodes trust in the review. **Find them all in one pass.**

How to apply:

1. When a finding crystallizes, name the *class* of issue in one sentence ("unhandled error from `db.Query`", "JSX prop with untyped any", "missing rate-limit header on POST handler", "comment claims behavior the implementation doesn't deliver").
2. Pick a search strategy proportional to the class:
   - Mechanical (named function call, named identifier, named string literal) → `grep`/`rg` across the diff's file set.
   - Structural (pattern shape, e.g. "async function without try/catch", "switch with no default") → re-read each file in the diff with the pattern explicitly in mind.
3. Record every site where the pattern applies, then collapse them into a single finding with a list of file:line citations, OR (if the call sites genuinely differ) separate findings cross-referencing each other.
4. Do this *before* moving to the next candidate finding. Lose the pattern context once you've moved on, and the second pass usually catches less than half of what the first sweep would have.

Applies to all four lens skills. The pattern types differ:

- `review-code` — bug-shaped patterns (missing error handling, predicate-at-evaluation-site bugs, mutation of caller state).
- `review-legibility` — readability-shaped patterns (restated-in-English comments, branch fanout, iteration-history scars across files).
- `review-compatibility` — shape-shaped patterns (one endpoint changed contract, others probably did too; one DDL added NOT NULL without the three-phase pattern, others probably did too).
- `review-releng` — operational-shaped patterns (one new code path missing telemetry, others probably are too; one config change without a feature-flag wrap, siblings probably aren't either).

## 3. Findings buffer

Don't post or report findings as you discover them. **Buffer them, then dedupe and classify, then emit.**

The buffer lives at `${TMPDIR:-/tmp}/review-findings-<lens>.jsonl`, one JSON object per line:

```jsonl
{"file": "<path>", "line": <number>, "severity": "P0|P1|P2|P3", "confidence": "low|medium|high", "category": "<lens-specific>", "description": "<one or two sentences>", "why_it_matters": "<one or two sentences>", "recommendation": "<one or two sentences, or a suggestion block>"}
```

Why a buffer:

- **Deduplication.** Two techniques (per-call trace + adversarial test critique) often surface the same bug. Without a buffer you post twice.
- **Pattern propagation** (convention 2) needs an inventory. The buffer *is* that inventory.
- **Verification before emission.** The skill's self-verification step (re-open file, re-read, confirm claim) iterates over the buffer; findings that don't survive get dropped *before* the user sees them.
- **Severity calibration is easier in batch.** When you see the full list at once, miscalibrated severities pop out.

Discipline:

1. Append to the buffer the moment a candidate finding crystallizes. Don't editorialize during discovery.
2. After all techniques have run, sweep the buffer for dedups (same `file:line:category` → keep one, prefer the higher-severity framing).
3. Run self-verification per buffered finding before emitting (re-open file, confirm the claim is true *as written*).
4. Emit only the survivors, sorted by severity, in the skill's Output Format.

Discard the buffer file when done.

## 4. Comment body schema

When findings get rendered into a comment posted on GitHub (whether one comment per finding posted inline, or all findings concatenated into a single aggregated comment), each individual finding renders in this shape:

```markdown
**<Severity>** — <category>

<description>

<recommendation>

<details><summary>Why this matters</summary>

<why_it_matters>

</details>
```

Rules:

- Severity labels are plain text — `**P0**`, `**P1**`, `**P2**`, `**P3**`, or repo-specific (`**Important**`, `**Nit**`, `**Pre-existing**` if `REVIEW.md` defines them). **No emoji.**
- The `<details>` block carries the *why-it-matters* context. Readers scanning a review skip them; readers debugging a specific finding expand them. Keep them short — one or two sentences.
- **Omit the `<details>` block entirely** when `why_it_matters` is already obvious from the description. Empty details blocks are noise.
- **For self-contained fixes ≤ 5 lines**, append a GitHub suggestion block after the `recommendation` paragraph, at the **top level** of the comment body (NOT nested inside `<details>` — GitHub won't render suggestions inside details):

  ````markdown
  **P2** — bug

  The retry loop ignores the context cancellation; if the caller cancels, we keep retrying.

  Check `ctx.Err()` in the loop body and bail when non-nil.

  ```suggestion
      for {
          if err := ctx.Err(); err != nil {
              return err
          }
          // ... existing retry logic
      }
  ```
  ````

- **For larger fixes**, describe the change in prose without a suggestion block. GitHub suggestion blocks are for one-click applies; if the user has to think to apply it, prose is clearer.
- **One comment per unique finding.** Pattern-propagated findings either get one comment with a list of all sites, or separate comments cross-referencing each other — never one comment per site that obscures the pattern.
- **Severity-line trailer** (HTML comment, invisible to humans, machine-readable): append `<!-- kmosher-review: severity=<severity> confidence=<confidence> lens=<lens> -->` as the last line of every comment body. Lets later tools (or future bot integrations) parse what was posted.

The `/review` command currently posts findings in an aggregated comment, but the schema applies to each finding rendered inside it. If/when an inline-posting mode lands, the same schema covers it.
