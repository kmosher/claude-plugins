# Shared Conventions for `kmosher-review` Lens Skills

Three conventions apply to every lens skill (`review-code`, `review-legibility`, `review-compatibility`, `review-releng`). Read this file when invoked. Each lens applies its own technique on top.

Adapted from the github-bot's review prompt — patterns proven on an autonomous reviewer that ships findings without a human in the loop.

## 1. REVIEW.md overlay (read FIRST)

If the repo has a `REVIEW.md` at its root, read it before doing anything else. Its rules **override this skill's defaults** — severity calibration, what to flag, what to skip, output shape, codebase precedents to treat as true.

A `REVIEW.md` may, for example: declare env vars from the secret manager trusted (so don't flag findings predicated on env-var control); cap nits at 5 per review; forbid findings CI already enforces; add repo-specific must-checks (e.g. "any change under `packages/sandbox-runtime/` must bump `CACHE_BUSTER`").

When `REVIEW.md` conflicts with the skill's defaults, `REVIEW.md` wins.

When invoked via `/review`, the coordinator reads `REVIEW.md` once at the top and passes it into every lens subagent's prompt — you'll have it in context already. When invoked directly, check for it yourself.

## 2. Pattern propagation

**When a finding crystallizes, scan all other diff files for the same shape before moving on.**

Most findings repeat. A missing `await` in one handler is usually missing in three. A wrong copyright year is wrong everywhere. A predicate-at-evaluation-site bug in one switch is likely the same in adjacent switches. Drip-feeding these across review rounds wastes the author's time.

How to apply:

1. Name the *class* of issue in one sentence ("unhandled error from `db.Query`", "missing rate-limit header on POST handler").
2. Search proportional to the class: `grep`/`rg` for mechanical patterns (named call, named identifier, named literal); re-read each diff file with the pattern in mind for structural ones.
3. Either collapse all sites into a single finding with multiple `file:line` citations, OR emit separate findings cross-referencing each other.
4. Do this *before* moving to the next candidate. Pattern context evaporates once you've moved on; the second sweep catches less than half.

## 3. Findings buffer

Don't emit findings as you discover them. **Buffer, dedupe, self-verify, then emit.**

Append candidates to `${TMPDIR:-/tmp}/review-findings-<lens>.jsonl` (one JSON object per line: `file`, `line`, `severity`, `confidence`, `category`, `description`, `why_it_matters`, `recommendation`). When all techniques have run:

1. **Dedupe** — same `file:line:category` → keep the higher-severity framing.
2. **Self-verify** — re-open each cited file, re-read the cited lines, confirm the claim is true *as written*. Drop findings you can't verify by reading actual code.
3. **Emit** the survivors, sorted by severity, in the skill's Output Format.

Why a buffer: pattern propagation (convention 2) needs an inventory; severity miscalibration only pops out when you see the full list at once; verification before emission catches the fabricated-finding class that wastes everyone's time.

Discard the buffer file when done.
