---
name: review-legibility
description: Use when reviewing a PR for readability after correctness is settled — comments that restate code, misleading names, branch fanout, iteration-history scars, stale references, comment-to-code-distance bloat, function docs describing the predicate instead of the function's purpose, tests that don't document behavior. Final pre-merge cleanup pass.
---

# Legibility Code Review

Review for **readability**, not correctness. Find code that makes a fresh reader stop and re-read, or that misleads them about intent. Catches cleanup opportunities adversarial reviews miss — branch fanout, restated-in-English comments, comment-to-code-distance bloat, iteration-history scars.

## When to Use

**Use for:** final cleanup before merge (especially after heavy iteration); functions/files you keep stumbling over; code landing as a single squashed commit; any "can we make this easier to reason about?" moment.

**Don't use for:** initial bug-finding (use review-code); pure formatting (use a linter); code where you suspect correctness issues (fix those first).

## Tooling available

This skill is about reader experience, not mechanical issues — but mechanical
issues that *also* hurt legibility (unused identifiers, dead code branches,
modernize hints) should be caught by tooling, not human judgment. Defer those
to:

- **Go**: `mcp__gopls__go_diagnostics` (gopls MCP) and `staticcheck` for
  unused-code / simplification findings.
- **TypeScript**: ESLint with the project config; `tsc --noEmit` for unused
  imports/locals if `noUnusedLocals` is on.
- **Rust**: `cargo clippy` for `complexity` and `style` lints.

Full per-language recipes in `review-automated-checks.md`. If
`/review` already ran them, don't re-run — focus your budget on
findings tooling can't catch (misleading names, branch fanout, comment-to-code
distance, restated-in-English comments).

## Prioritization Hierarchy

Legibility findings can balloon. The 10-finding budget is enforced by this order:

1. **Misleading code** — comment claims X, code does Y; function name promises one behavior, body does another; stale references resolving wrong
2. **High-friction structure** — branch fanout, oversized functions, comment-to-code-distance bloat
3. **Unfocused naming or docs** — names not carrying weight, docs describing the predicate not the function's purpose
4. **Defensible preference** — alternate phrasing, optional extraction

Higher beats lower. Level-4 issues should never crowd out level-1.

## Why It's a Different Problem

Bug-finding asks "what could go wrong?" Legibility asks "what makes a fresh reader stop?" Patterns that catch bugs (per-call trace, failure-mode enumeration) miss patterns that catch unclear code (function-too-big, comment-restating-code, branch-fanout).

Without a dedicated lens, legibility issues accumulate during PR iteration: comments referencing "the regression we just fixed", branch chains that grew one if at a time, function docs describing the *predicate* rather than the function's *purpose*. None are bugs. All make the next reader slower.

## The Heuristics

Each is a *test* the reviewer applies and flags failures. Concrete enough that the reviewer can't hide behind taste.

| # | Heuristic | Test | Failure means |
|---|---|---|---|
| 1 | One-sentence purpose | State the function's purpose in one sentence with no "and"/"also" | Doing two things; split |
| 2 | Comment deletion | Mentally delete; what's harder to understand? | If "nothing" → delete. If "what the code does" → delete. Keep only "the *why*". |
| 3 | Branch fanout | Count ordered branches in any if/else chain or switch | >3 with non-trivial reasoning → extract classifier/table |
| 4 | Name genericization | Mentally rename to `x`; can you still figure out the role? | If yes, name is load-bearing. If no, name carries meaning the code should — improve both. |
| 5 | Reader onramp | Jump from a search result; can you use it correctly in 30s from doc+signature? | If no, doc inadequate or signature too clever |
| 6 | Reference staleness | Verify every PR/issue/test/file reference resolves | Stale references erode trust in all comments |
| 7 | Iteration-history scars | Search for "recently", "we just fixed", "the X bug we caused" | Won't make sense after squash; rewrite self-contained |
| 8 | Comment-to-code distance | For each 4+ line block comment, count code lines it describes | Long comment over short code → extract function (comment becomes doc) or trim |
| 9 | Duplication | Pattern repeated 3+ times **OR** a new decision (predicate, switch, lookup table, transformation, validator) whose logic is also encoded elsewhere | Inventory each decision (function + inputs + outputs), grep adjacent code for matches on inputs OR outputs, compare side-by-side. Extract a helper, consolidate to one source of truth, or add a drift-catching test as appropriate to the shape. |
| 10 | Tests as documentation | Read only test name + top-level asserts; does the contract emerge? | If you must read the body, the test isn't documenting behavior |
| 11 | Comment density and lead | For each multi-sentence comment: lead test, mechanism-restatement check, hedge-and-passive sweep, load-bearing test. **Hand non-trivial rewrites to the `comment-writer` agent** — don't draft them yourself. Bake-off when uncertain. | Bidirectional failure mode: too verbose buries the take-home; too terse cuts load-bearing context. Default to leaving alone when the load-bearing test passes; delegate rewrites to the virtuoso. |
| 12 | Self-verification | Before submitting, re-open each cited line and confirm the finding | Drop findings you can't verify; state which you did |

## Severity Calibration

- **P1** — actively misleads readers (stale reference, comment claims X but code does Y, name promises wrong behavior). Should block merge.
- **P2** — high friction (6-branch chain unextracted, 30-line function with 5 responsibilities, magic constant unnamed). Should be addressed.
- **P3** — defensible either way. Author's choice.

## The 10-Finding Cap

Legibility critique balloons easily. Cap at 10. Forces prioritization: which 10 things are worst? Pair with explicit "DO NOT propose pure stylistic preferences." Filters preference from real friction.

If fewer than 10 issues clear the bar, the reviewer should say so. Performative thoroughness is worse than honest "this is fine."

## Common Misconceptions

| Misconception | Reality |
|---|---|
| "It's clear if I can read it" | Reviewer has loaded context; next reader hasn't. Test against onramp, not familiarity. |
| "Comments are always good" | Comments restating code waste attention. Comments explaining load-bearing *why* are invaluable. Distinguish. |
| "More tests = better tests" | Test count without contract-emergence is just more code to maintain. |
| "Function size doesn't matter" | The threshold isn't lines — it's distinct ideas. 30 lines doing one thing is fine; 12 lines doing four isn't. |
| "Preserve iteration history in comments" | History belongs in the commit log. Comments are for the *current* reader; PR scars confuse them. |
| "Renaming is bikeshedding" | Renames paired with structural improvement (genericization test failed → both change) are real refactors. Pure renames probably are bikeshedding. |
| "Linter happy = code clear" | Linters catch syntax. Manual review catches "this name is misleading" or "this branch chain wants a table." |
| "Tests don't need to be readable" | Tests document behavior. Unreadable tests = undocumented behavior. |

## Anti-Pattern Checklist

Almost always wrong.

- [ ] Comment that restates code (`// increment counter` over `counter++`)
- [ ] Iteration-history scar ("the regression we recently caused")
- [ ] Function doc describing the predicate, not the function's purpose
- [ ] Stale PR/issue/test/file reference
- [ ] 6+ ordered if/else branches with non-trivial reasoning each
- [ ] Name doesn't match observable behavior (`validateInput` that mutates, `isReady` with side effects)
- [ ] TODO with no owner, no date, no link
- [ ] Magic constant with no name (`if x > 50`)
- [ ] Test name that doesn't reveal the claim
- [ ] Comment promising behavior the code doesn't have ("we always validate" — code doesn't)
- [ ] Long inline rationale comment that should be a function doc

## Worked Example

See `EXAMPLE.md` in this skill's directory — a 6-branch predicate with 50 lines of inline rationale was correct (no bug) but unreadable. Adversarial review missed it; legibility review flagged it via branch fanout (test 3) + comment-to-code distance (test 8) + reader onramp (test 5).

## Common Mistakes When Writing the Prompt

| Mistake | Fix |
|---|---|
| Asking "is this clear?" | Force specific tests; "clear" is subjective |
| Allowing stylistic preferences | Explicitly disallow tab/brace/import-order findings |
| No finding cap | Cap at 10; preference noise drowns real friction |
| Mixing in correctness review | Two separate prompts. Legibility assumes correctness. |
| No "fewer than 10 is fine" escape hatch | Reviewer manufactures findings to fill the count |
| Not pre-loading package style | Reviewer flags "non-idiomatic" stuff that's idiomatic for the surrounding code |
| No self-verification step | Reviewer ships findings citing wrong line numbers or misquoted code |

## Pairing With Other Skills

- **review-code** first — don't polish code that might still be wrong
- **review-legibility** last — cleanup is safer when underlying logic is locked down
- **review-releng** / **review-compatibility** are independent; can run in parallel

If you only run one, pick the one matching actual risk. Schema-changing PR → adversarial. Renaming/extraction PR → legibility.

## Prompt Template

The full prompt template is in `PROMPT_TEMPLATE.md` in this skill's directory. Copy it, fill in the bracketed sections.

## Reference

The cleanup that motivated this skill: a 6-branch predicate with 50 lines of inline rationale was correct (no bug) but unreadable. Adversarial review missed it (correctness was fine); legibility review flagged it via branch fanout (test 3) + comment-to-code distance (test 8) + reader onramp (test 5). Same code, different lens.
