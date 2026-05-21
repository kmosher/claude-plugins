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

## 3. Findings buffer (also the output format)

Don't emit findings as you discover them. **Buffer, dedupe, self-verify, then return.**

Append candidates to `${TMPDIR:-/tmp}/review-findings-<lens>.jsonl` — one JSON object per line. **This buffer IS the lens's output.** Rendering to GitHub-flavored markdown, severity labels, suggestion blocks, etc. is the invoker's job, not the lens's. When invoked via `/review`, the coordinator does the rendering. When invoked directly, return the JSONL and let the caller decide.

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
2. **Self-verify** — re-open each cited file, re-read the cited lines, confirm the claim is true *as written*. Drop findings you can't verify by reading actual code.
3. **Sort** by severity (`P0` first), then `file`, then `line`.
4. **Return** the JSONL block. When invoked from `/review`, paste the buffer contents inside a ` ```jsonl ` fenced block in the response, preceded by a one-paragraph meta note (what you read, what was settled, what was inferred). When invoked directly, the lens may also render a human-readable view *after* the JSONL block, but the JSONL must come first and unmodified.

Discard the buffer file when done.

Why a buffer: pattern propagation (convention 2) needs an inventory; severity miscalibration only pops out when you see the full list at once; verification before emission catches the fabricated-finding class that wastes everyone's time; structured output lets the coordinator dedupe and verify mechanically instead of regexing markdown.
