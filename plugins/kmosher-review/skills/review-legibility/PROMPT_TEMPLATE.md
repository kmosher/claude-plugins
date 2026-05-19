# Legibility Review Prompt Template

Copy-paste and fill in the bracketed sections.

---

# Legibility review of <PR/branch>

You are reviewing for **readability**, not correctness. Assume the code is correct. Find code that will make a reader stop and re-read, or that will mislead them about intent.

This prompt forces specific tests rather than vague "is this clear?". Apply each test explicitly and cite the result.

## What to review

Repo: <path>
Branch: <branch> (latest commit <SHA>)
Files: <list>

## Required reading first

[Pre-load surrounding-package conventions: 1–2 example files that exemplify "good" style for this codebase, the package-level doc-block, and any style guide referenced by the repo.]

## Apply these tests, in order

For each test, list every failure with file:line, what the failure looks like, and a concrete proposed cleanup (sketch the refactor structure, not just "make it clearer").

### Test 1: One-sentence purpose
For each function in the change, write its purpose in one sentence with no "and"/"also". A function that needs two clauses is doing two things.

### Test 2: Comment deletion
For each comment longer than one line, ask "if I delete this, what's harder to understand?" Keep only those whose answer is "the *why* behind a non-obvious choice."

### Test 3: Branch fanout
Count ordered branches in any if/else chain or switch. >3 with non-trivial reasoning per branch → propose a classifier/table.

### Test 4: Name genericization
For each identifier introduced by the change, mentally rename to `x`. If surrounding code still makes sense, the name is carrying its weight. If not, propose rename + structural change together.

### Test 5: Reader onramp
For each new function: if a developer jumped here from a search result with no other context, would doc + signature let them use it correctly in 30 seconds?

### Test 6: Reference staleness
For every PR/issue/test/file reference in a comment, verify it still resolves.

### Test 7: Iteration-history scars
Search for "recently", "we just fixed", "the X bug we caused", or other branch-scoped language that won't make sense after squash.

### Test 8: Comment-to-code distance
For each block comment of 4+ lines, count the lines of code it describes. A long comment over short code suggests either: (a) extract the code into its own function (comment becomes its doc), or (b) the comment is restating.

### Test 9: Duplication
Find any pattern repeated 3+ times. Extract unless extracting would hide a critical data flow.

**Cross-file structural duplication — required procedure, not optional check.** Textual repetition inside the diff is the easy case and grep finds it. The harder case is when the new code encodes a *decision* whose logic is also encoded elsewhere in the codebase — keyed on the same inputs, or producing the same outputs — even though names, syntactic form, and surrounding control flow differ. This kind of duplication never appears in the diff and is a frequent source of "keep in sync" / "must match" / "mirrors" comments. Comments rot; the duplication remains; the next bug is a drift bug.

A "decision" is any code that:
- **branches on input** (predicates, eligibility gates, if/else chains, switches)
- **maps input to output** (lookup tables, value/format transformations, parse/serialize pairs, type coercions)
- **enforces a constraint** (validators, schema checks, invariants over inputs)

Do not skip this. Execute the steps:

1. **Inventory the decisions the new code makes.** For each function, switch, table, validation, or transformation the change adds or non-trivially modifies, write down explicitly in your scratch reasoning: (a) the function/symbol name and file:line, (b) the INPUTS it keys on (accessors called, type/struct fields read, parameter names, key values for tables), (c) the OUTPUT it produces (return type and an example or two of return values; for side-effecting code, the state change). One row per decision. Skipping this step means failing the test — the inventory is the artifact the rest of the procedure operates on.
2. **For each row, search adjacent code for matches on its inputs or outputs.** Adjacent = the same package, plus any package the new code calls into. Search in both directions:
   - **Input grep**: code that calls the same accessors or reads the same fields (`grep -rn "\.Removed()" pkg/`, `grep -rn "case StatusForbidden" pkg/`, `grep -rn "FooConfig{" pkg/`).
   - **Output grep**: code that returns the same value tags or constructs the same outputs (`grep -rn "return ErrPermissionDenied" pkg/`, `grep -rn "\"Access denied\"" pkg/`, `grep -rn "return FooConfig{" pkg/`).
   Textual matches on the predicate body itself will rarely succeed — that's the whole reason this kind of duplication slips by. The accessors and the output tags are what survive renaming and reformatting.
3. **For each match, compare the encodings side by side.** Are they making the same decision? Is one a strict subset, or are they fully equivalent? Is the discrepancy intentional and load-bearing, or accidental? If the encodings agree and there's no shared source of truth — even more so if the author's comment hints at synchronization — this is a finding.
4. **Severity P2 minimum if the decisions encode a correctness invariant.** Drift causes silent bugs. Recommend the appropriate fix shape:
   - Predicates / gates → extract a shared helper called from both sites.
   - Lookup tables / mappings → pick one source of truth; have the others import, codegen from, or test against it.
   - Parse/serialize pairs → add a property test asserting round-trip identity (`parse(format(x)) == x`).
   - Validators → consolidate to one validator function applied at every entry point, or share a common rule set.
   Author-written "keep in sync" comments are not a fix.

**Worked examples — different decision shapes, same procedure:**

*Example A — predicate.* A PR adds `classifyStaleDefault` whose switch contains `case sch.Removed() != "":` and `case sch.Deprecated() != "" && !sch.Required():`. Step-1 inventory captures the switch with inputs `{sch.Removed(), sch.Deprecated(), sch.Required()}` and output `{strip, preserve}`. Step-2 input grep for `Removed()` in the package surfaces `applyDefaults` in another file containing the same accessor checks gating two of its branches. Step-3 comparison confirms the boolean shapes are equivalent. Finding: extract `schemaSkipsDefault(sch) bool`, called from all three sites.

*Example B — lookup table.* A PR adds a Go map `awsErrorToUserMessage` mapping AWS SDK error codes to user-facing strings. Step-1 inventory: inputs are AWS error code strings (`"AccessDenied"`, `"InvalidParameter"`, …), outputs are user-facing message strings. Step-2 input grep for `"AccessDenied"` surfaces a TypeScript constant `ERROR_MESSAGES = { ... }` in a frontend file with a parallel mapping. Step-2 output grep for `"You don't have permission"` surfaces a localization YAML file with a third copy. Step-3 comparison shows three overlapping enumerations of the same logical mapping. Finding: pick one source of truth (the localization file is the natural choice), have the Go map and TS constant codegen or load from it; add a test that fails when an error code is added to one site but not the others.

*Example C — parse/serialize pair.* A PR adds `parseFooConfig(s string) FooConfig`. Step-1 inventory: input string, output `FooConfig{Name, Limit, Tags}`. Step-2 input grep for `FooConfig{` finds `formatFooConfig(c FooConfig) string` in the same package. Step-3 comparison shows the two functions consume each other's outputs but have no test asserting the round-trip identity holds. Finding: add a property/fuzz test `parse(format(c)) == c` and `format(parse(s)) == s` for valid inputs. Extraction isn't the right fix here — the two functions must remain distinct — but the implicit invariant needs to be made explicit and enforced.

Skipping step 1 means missing the finding even when you have read both files. The procedure works because the inventory forces you to articulate inputs and outputs in a form a grep can match. If you didn't write down the decision inventory explicitly, you didn't run this test.

### Test 10: Tests as documentation
For each new test, read only the name + top-level asserts. Does the contract emerge?

### Test 11: Comment density and lead

For each comment of two or more sentences, evaluate the prose. The other tests check whether the *code* is legible; this one checks whether the *comments* are. The failure mode is bidirectional: a comment can be too verbose (buries the take-home, restates mechanism) or too terse (cuts load-bearing context, leaves the reader without enough detail to modify the code safely). This test exists to catch both, not just verbosity.

Apply each step, then weigh the results before proposing any rewrite:

1. **Lead test.** Read only the first sentence. Does it stand alone as the take-home? If sentence 1 is a setup clause ("In order to handle X,…"), an aside ("Note that…"), or context that defers the actual point to sentence 2 or later, that's a finding. The first sentence must answer "what does this comment tell me?" by itself.

2. **Mechanism-restatement test.** Per sentence, ask "if I read the next 1–3 lines of code, does this sentence add anything?" Sentences that restate visible mechanism are findings; sentences that document a non-obvious *why* (a constraint, an invariant, a workaround, an interaction with code elsewhere) earn their place.

3. **Hedge-and-passive sweep.** Flag passive constructions where active is shorter ("the file is written" → "writeFile writes the file"); hedge phrases ("might", "could", "seems", "essentially", "basically"); intensifiers and filler ("very", "in order to", "such a", "really"). These are usually safe to cut without loss.

4. **Load-bearing test — gate before proposing any compression.** Before proposing a rewrite that's materially shorter, list every load-bearing fact in the original. Examples of facts that are typically load-bearing:
   - Concrete enumeration of cases a function handles ("scalar fields, unknown fields, fields whose Elem is not a Resource")
   - Disambiguation between similarly-named functions ("use this when …, use the other when …")
   - Reasoning chains the next reader needs ("TypeSet was rejected above; TypeList elements have positional identity")
   - Mechanism explanations behind non-obvious test rationale (e.g. RawConfig presence semantics; the specific invariant from a referenced PR)
   - Marker / accessor lists that anyone modifying a parity gate needs to enumerate
   
   A rewrite that drops any load-bearing fact is worse than the original even if it scores better on tests 1–3. **Word count is not the metric.** The goal is every sentence carries weight, not that the comment be as short as possible.

5. **Hand non-trivial rewrites to the comment-writer agent.** If a comment fails one of tests 1–4 in a way that suggests a meaningful rewrite is warranted, do not write the rewrite yourself. Invoke the `comment-writer` agent (in kmo) with the code and the existing comment. The virtuoso has the eight comment-writing principles baked in, the audience model calibrated to "competent-but-file-naive", and a self-critique loop that handles compression-vs-load-bearing-content as a single decision. Use its output as the proposed rewrite, not your own draft. This avoids the failure mode where a reviewer over-trims under bias toward terseness.

6. **Bake-off when results are still uncertain.** For rewrites the virtuoso produces that you're not sure improve on the original, run a bake-off — present old and new to an independent reviewer (or to a separate virtuoso instance that hasn't seen the original) with the surrounding code. Use the bake-off result, not your own judgment, to decide. Restoring detail you cut is harder than tolerating mild verbosity.

Severity: P1 if a comment actively misleads (lead implies one thing, body says another); P2 if it's high-friction enough that a reader will skim past load-bearing detail; P3 for one-off prose nits. **Default is to leave comments alone when the load-bearing test reveals they earned their length** — restoring detail you cut is harder than tolerating mild verbosity.

Worked example — when to compress: the comment

> Two assertion styles are used, calibrated to scenario. No-op scenarios use assertNoChanges, which enforces that the preview reports only "same" — any spurious diff signals a real bug. Schema-migration scenarios verify the strip's effect by inspecting the post-Up exported stack via resourceInputs.

leads with meta-narration ("Two assertion styles are used") and a setup clause. Sentence 1 is filler. Tests 1 and 4 agree the rewrite is better:

> Tests use one of two assertions: assertNoChanges for no-op scenarios (preview reports only "same"), or post-Up resourceInputs checks for schema-migration scenarios (stored inputs no longer carry the stale field).

Worked example — when to leave alone: a 17-line test rationale comment lists two reasons a stored value is preserved (RawConfig falsy semantics from PR #3420; changed-default phantom-diff). Tests 1–3 might suggest compressing to 4 lines naming the two invariants without the mechanism. The load-bearing test (step 4) reveals: someone considering changing this rule needs the *mechanism* to evaluate the change, not just the existence-claim. Bake-off (step 5) confirms the longer version wins. The original earned its length; do not propose a rewrite.

### Test 12: Self-verification (before submitting)
For each finding you're about to submit: re-open the file, re-read the cited code, and confirm the finding accurately characterizes what's there. If you cannot verify the claim by reading the actual code, drop the finding. State which findings you verified.

## Output format

Numbered findings, **max 10**. Each:
- **Severity**: P1 (will actively mislead readers), P2 (high friction), P3 (preference, defensible either way)
- **File:line ref**
- **Which test flagged it**
- **What's wrong** (concrete — "this comment restates `if x { ... }` in English" not "this comment is unclear")
- **Proposed cleanup** (sketch the structure, not just "rewrite for clarity")

Cap findings at 10 to force prioritization. If fewer than 10 issues clear the bar, say so explicitly.

DO NOT propose pure stylistic preferences (tab vs spaces, brace style, import order). Only structural or semantic legibility issues.
