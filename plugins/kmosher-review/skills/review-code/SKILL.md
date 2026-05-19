---
name: review-code
description: Use when reviewing a non-trivial PR before merge to find correctness bugs — logic errors, ignored errors, panics, predicate-evaluation-site bugs, schema/shape mismatches, recursion bounds, mutated caller state, tests that pass with a buggy implementation. Especially for PRs that have had multiple prior review rounds and you want to find what they missed.
---

# Adversarial Code Review

Find bugs that domain experts catch and automated reviewers miss. Forces reading the actual implementation (not just signatures), enumerating what a change does NOT cover, and grounding analysis in concrete real-world scenarios.

## When to Use

**Use for:** final review before merging non-trivial PRs; lifecycle/schema/invariant changes; PRs that have had several rounds of review where you want to find what they missed; code landing as a single squashed commit.

**Don't use for:** trivial refactors or renames; first-pass review on a fresh PR (use a normal review first); domains where you can't access upstream docs.

## Tooling available

When the diff touches Go code and the `gopls` MCP is available, use
`mcp__gopls__go_diagnostics` on changed files to surface analyzer findings
(modernize hints like `mapsloop`, `forvar`, `minmax`, plus type and analysis
errors). For whole-package sweeps, see the `modernize` CLI recipe in
`review-automated-checks.md`. TypeScript and Rust have parallel
recipes there — load only the relevant language section on demand.

If `/review` already ran the automated checks step, don't re-run —
cross-reference its findings. If you're invoked directly, run the relevant
tooling yourself before reporting bug-finding results so you can ground them
in concrete diagnostic output rather than speculation.

## Prioritization Hierarchy

When choosing where to spend attention:

1. **Real bugs that hit users** — incorrect logic, panics, data corruption, security exposure
2. **Logic gaps the change does NOT cover** — failure modes the mitigation trades into existence
3. **Test weaknesses that hide bugs** — tests that pass with a buggy implementation
4. **Comment/doc inaccuracies that mislead future readers**

Higher beats lower. The 6–15 finding cap forces this prioritization.

## Why Normal Reviews Miss Real Bugs

Five recurring failure modes:

1. **Grep without reading.** Locate a call but don't open the body. (`DefaultValue()` calls `DefaultFunc()` and ignores the error — only obvious if you read it.)
2. **Tests as documentation.** Take test names as evidence; don't ask "what buggy implementation passes this test?"
3. **Verify claims, not gaps.** Confirm what the change does; don't enumerate what it doesn't.
4. **Speculate from naming.** A function named `stripMapOfBlocks` *sounds* right; whether the shape exists in production needs reading shim docs the code references but the reviewer skips.
5. **Re-raise settled issues.** Without an "already-adjudicated" list, each reviewer wastes budget rediscovering decisions.

## The Eight Techniques + Self-Verification

Force the review through these explicitly. Output must show reasoning for each.

### 1. Required upstream reading

3–6 files or doc-blocks the reviewer **must read in full** before analyzing. Choose: the interface/shim layer; canonical doc-blocks; implementations of non-obvious calls (especially anything invoking a callback); referenced PRs/issues; the PR description; root and directory-level `CLAUDE.md` files for every touched directory.

Then require, per file: "I read this. The relevant facts are X, Y, Z. What I expected vs. what I found." Converts grep into reading.

### 2. CLAUDE.md compliance audit

After upstream reading, run an explicit pass: for each CLAUDE.md file you read, walk its instructions against the changes. Flag where the diff violates documented conventions — "CLAUDE.md says X but the diff does Y." Distinguish *hard* violations (CLAUDE.md explicitly forbids the pattern) from *soft* drift (CLAUDE.md prefers X, diff does adjacent-but-different Y).

CLAUDE.md is guidance for Claude writing code, so not every line is a review rule. Filter to: explicit prohibitions, mandated formats/patterns, named workflows, decisions captured ("we use X because Y"). Skip stylistic flavor.

### 3. Adjacent comment compliance

For each changed line, read the surrounding comments (function-level docs, block comments, line comments within ~20 lines of the change). Comments often encode invariants the code itself doesn't enforce: "must hold lock X before calling Y," "callers must check Z first," "do not call this in the hot path." If the change violates an adjacent comment's claim, that's a finding — either the change is wrong, or the comment is now stale and should be removed in the same PR.

Distinct from review-legibility's stale-reference check. That asks "do referenced things exist?" This asks "did the change just violate guidance written next to it?"

### 4. Per-call trace (Meta semi-formal reasoning)

For every function call inside the change, document name, location, body behavior, and caller-vs-callee contract gap. Apply especially to: anything returning `(value, error)` whose caller ignores the error; pure-looking accessors that invoke callbacks; type assertions; recursion termination; calls to functions documented elsewhere.

### 5. Adversarial test critique

For each test: "Here's a buggy implementation. Would this test catch the bug?" Specifically: could it pass with the predicate flipped? Could it pass with the function doing nothing? Are fixtures realistic against the production code path?

### 6. Failure-mode enumeration

List ≥5 concrete real-world scenarios the change does NOT fix. Be specific: name the system, configuration, user-observed outcome. Mix production failures (system X down, env var Y unset, network partition) **and adversarial-input edges** (empty/null/zero, max-value, malformed input, unicode oddities, timezone/DST, encoding boundaries, very-large/very-small numbers).

### 7. Concrete walkthrough

Trace two specific real-world cases end-to-end through every function in the change. Real names, real versions, not placeholders. If you can't trace concretely, you don't understand the change.

### 8. Negative-space audit

What's absent that production code should have? Concrete absences only. Logging/telemetry, kill switch, edge-case handling documented but not enforced.

### 9. Self-verification (before submitting)

For each finding you're about to submit: re-open the file, re-read the code, confirm the claim is true. If you cannot verify by reading the actual code (not its name, not surrounding doc), mark confidence as low or drop the finding. State which findings you verified and which you didn't.

This is the step Meta's research treats as load-bearing: "verify your conclusions against the evidence you traced." Catches fabricated findings before they ship.

## Settled-Issues List

Explicitly list adjudicated concerns. Without this, each reviewer wastes attention rediscovering decisions. With it, they focus on novel issues.

Example phrasing:
> Settled — DO NOT raise these:
> - "X should also happen in Y" — investigated, decided no, because Z.
> - "Should we run W on inputs?" — not viable; tracked as #N.
> - "Architectural cleanup of M" — intentionally a follow-up, see #N.

If a reviewer disagrees, require new evidence — not a re-raise.

## Common Misconceptions

| Misconception | Reality |
|---|---|
| "The function name says what it does" | Function names are aspirational. The body is authoritative. |
| "The test name documents the test's claim" | Test names describe intent; assertions describe what's actually verified. Read the body. |
| "If the predicate is correct, the change is correct" | Where, when, and with what inputs the predicate runs all matter. Trace the evaluation site. |
| "The mitigation handles the failure mode" | It handles the one the author thought of. Enumerate the ones they didn't. |
| "Comments accurately describe the code" | Comments rot faster than code. Verify every load-bearing claim against the implementation. |
| "Previous reviewers would have caught it" | Previous LLM reviewers share the same blind spots. Re-derive, don't re-trust. |
| "CI green = logic correct" | CI proves the cases someone thought to test pass. Production has the cases nobody thought of. |

## Anti-Pattern Checklist

Bug-shaped patterns. Almost always real when found.

- [ ] **Type assertion without `, ok`** — panics on mismatch
- [ ] **Error returned but ignored** — silent failure
- [ ] **Pure-accessor that invokes a callback** — runtime side effect masquerading as a getter
- [ ] **Recursion with no base-case bound** — adversarial input → stack overflow
- [ ] **Map iteration assumed ordered** — Go iteration is non-deterministic
- [ ] **Mutation of caller's data** — function modifies a passed-in map/slice; caller's other references see it
- [ ] **Partial copy with shared inner refs** — `copy(newArr, arr)` shares inner pointers
- [ ] **`if err != nil { return nil }`** — error swallowed as success-with-no-data
- [ ] **Goroutine launched with no exit signal** — leaks (Go-specific case of the next pattern)
- [ ] **Async task launched with no cancellation / exit path** — language-agnostic: detached goroutine, unawaited Promise, orphaned `tokio::spawn`, background `Thread` without join
- [ ] **Shared mutable state accessed without synchronization** — two callers can mutate the same map/struct/array concurrently; missing mutex/atomic/channel; works in tests where only one caller runs
- [ ] **Lock ordering inconsistency** — function A takes lock X then Y; function B takes Y then X. Deadlock under contention.
- [ ] **Critical section split by an `await` / `yield` / blocking call** — invariant held inside the critical section is no longer guaranteed across the suspension point
- [ ] **Channel/queue send with no buffering and no matching receiver** — sender blocks forever; classic in Go (`ch <- x` on unbuffered channel without a goroutine waiting on `<-ch`)
- [ ] **Ordering assumption across concurrent operations** — code assumes "this `Promise.all` completes in input order" or "this goroutine ran before that one"; not guaranteed
- [ ] **Defer in a loop without closure barrier** — defers stack to function exit, not iteration
- [ ] **Predicate using runtime resolver** when structural accessor exists — `Resolve()`/`DefaultValue()` returns nil/error spuriously; use `Has()`/`Default()`/`DefaultFunc()` instead
- [ ] **Test that mocks the failure mode it's testing** — retry test that mocks the retry; timeout test that mocks the clock without verifying the timeout fires

## Output Format

Numbered findings. Each: severity (P0–P3), file:line ref, what's wrong (1–3 sentences, concrete), bug it would cause in a real scenario, proposed fix, confidence (low/medium/high distinguished by what the reviewer *actually did*).

**Confidence calibration:**
- **Low** — speculated from naming or surface reading
- **Medium** — read related code but didn't fully verify the failure path
- **High** — read the implementation and traced an end-to-end scenario demonstrating the bug

Aim for 6–15 findings. **If genuinely no novel issues**, say so with reasoning. Do not manufacture.

## Worked Example

See `EXAMPLE.md` in this skill's directory for a real case (PR with ~10 prior review rounds where this skill found a P1 predicate bug, a P3 test-strength gap, and a P3 negative-space finding).

## Common Mistakes When Writing the Prompt

| Mistake | Fix |
|---|---|
| "Be adversarial" without specifying techniques | Force the eight techniques explicitly |
| Inlining all context in the prompt | Save context files to disk; reference paths |
| No settled-issues list | Reviewers re-raise adjudicated concerns; budget wasted |
| Asking for N findings | Reviewer manufactures; ask for 6–15 *or* explicit "no novel" |
| Not requiring upstream reading | Reviewer greps, doesn't read, misses semantic facts |
| Vague "trace through" | Specify cases by name (real provider, product, version) |
| Letting tests speak for themselves | Force the "buggy implementation that passes" exercise per test |
| No self-verification step | Reviewer ships fabricated findings; severity inflated |

## Pairing With Other Skills

- **First**, when correctness might be at risk
- **review-legibility** runs after, when correctness is settled and cleanup remains
- **review-releng** / **review-compatibility** run alongside or after, for deploy-time concerns

## Prompt Template

The full prompt template is in `PROMPT_TEMPLATE.md` in this skill's directory. Copy it, fill in the bracketed sections, send to your reviewer agent.

## Reference

- Meta's semi-formal reasoning research (2026): structured prompting requiring premises, execution traces, formal conclusions before verdict. ~93% accuracy on benchmark code-review tasks.
- The eight techniques distill what domain experts do unconsciously: pre-load context, audit against documented conventions (CLAUDE.md), respect adjacent invariants in comments, trace calls, adversarially critique tests, enumerate failure modes (production *and* adversarial-input), ground in concrete cases, audit absences, and verify claims against evidence.
