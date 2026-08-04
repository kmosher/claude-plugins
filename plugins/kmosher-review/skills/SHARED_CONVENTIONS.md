# Shared Conventions for `kmosher-review` Lens Skills

Four conventions apply to every lens skill (`review-code`, `review-legibility`, `review-compatibility`, `review-releng`, `review-agent-skills`). Read this file when invoked. Each lens applies its own technique on top.

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

## 3. Findings buffer (also the output format)

Don't emit findings as you discover them. **Buffer, dedupe, then return.**

Append candidates to `${TMPDIR:-/tmp}/review-findings-<lens>.jsonl` — one JSON object per line. **The buffer is where findings accumulate; the response carries them under `## findings`** (see Process below). Rendering to GitHub-flavored markdown, severity labels, suggestion blocks, etc. is the invoker's job, not the lens's — when invoked via `/review`, the coordinator does it.

### Canonical finding schema

Every finding has these fields:

| Field | Type | Notes |
|---|---|---|
| `file` | string | Path relative to repo root. Use `null` for PR-level findings (no specific file). |
| `line` | int \| null | 1-indexed line number, or `null` for PR-level / multi-line findings. |
| `severity` | string | `P0` / `P1` / `P2` / `P3`. Lens-specific tiers if `REVIEW.md` overrides. |
| `confidence` | string | `low` / `medium` / `high` — based on what you actually did to verify (see each lens's confidence calibration). |
| `category` | string | Lens-specific enum (see each lens's Output Format). |
| `description` | string | 1–3 sentences. What's wrong, concretely. |
| `why_it_matters` | string | 1–2 sentences. The real-world bug/regression this would cause. |
| `recommendation` | string | 1–2 sentences, or a fenced ```suggestion``` block when ≤ 5 lines. |

Lens skills may add lens-specific fields (e.g. `heuristic_number` for `review-legibility`, `deploy_ordering` for `review-compatibility`). Document them in the lens's Output Format section.

### Process

When all techniques have run:

1. **Dedupe** — same `file:line:category` → keep the higher-severity framing; merge `description`s.
2. **Sort** by severity (`P0` first), then `file`, then `line`.
3. **Return** the JSONL block under a `## findings` header, as a ` ```jsonl ` fenced block, followed by a `## meta` section — one paragraph on what you read, what was settled, what was inferred. The header is mandatory even when the block is empty: a bare fenced block with no section header is not a valid lens response, because everything downstream locates findings by header, not by guessing at the shape of the rows. When invoked directly, the lens may render a human-readable view *after* the JSONL block, but the JSONL must come first and unmodified. Under `/review` a third section is required in addition, `## upstream_reading` between the other two: one JSONL row per file you read outside the diff, `{"path": "<path>", "told_me": "<one-line summary>"}`.

Leave the buffer file in place — it lives under the session tmpdir and is swept with it.

Why a buffer: pattern propagation (convention 2) needs an inventory; severity miscalibration only pops out when you see the full list at once; structured output lets the coordinator dedupe and audit mechanically instead of regexing markdown.

## 4. Report everything; filtering happens downstream

**Emit every finding you believe is real, at whatever severity it lands.** A lens has no finding cap, no severity floor, and no quota. A P3 you're sure of belongs in the buffer next to a P0.

Two things follow from this, and they are the whole reason the rule can be stated so bluntly:

- **Filtering is a separate, later pass.** `/review` runs an independent auditor over the merged buffer and renders under the user's own thresholds. A lens that pre-trims is filtering on strictly less information than the pass built to do it — it can't see the other lenses' findings, the settled-issues list applied globally, or the repo's `REVIEW.md` thresholds as the coordinator resolved them.
- **Trim on truth, not on volume.** Don't report a finding you don't believe: state confidence honestly, and if you couldn't establish the claim against actual code, say `low` rather than dropping it. But never drop a finding you *do* believe because the list is getting long. A long list of real findings is a correct result, and "nothing further surfaced" is also a correct result — report whichever one is true.

Padding is still forbidden. Reporting everything real is not the same as manufacturing filler to hit a number; there is no number.
