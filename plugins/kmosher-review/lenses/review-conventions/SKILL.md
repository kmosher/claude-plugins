---
name: review-conventions
description: This skill should be used when reviewing a PR purely for compliance with the repo's written rules — AGENTS.md, CLAUDE.md and REVIEW.md files scoped to the changed files. Extracts every checkable rule (imperatives, numbered steps, always/never/must, required tags, flags and commands), tests each against the changed hunks, and reports only violations. Reports no bugs and no style; other lenses own those.
---

# Conventions Compliance Review

One job: does the diff follow the rules the repo wrote down? The conventions files are your whole input. A rule nobody checks is a rule nobody follows, and a lens with other work to do treats them as background. Here they are the work.

## Shared conventions (read first)

Read `../SHARED_CONVENTIONS.md` before applying this lens — REVIEW.md overlay, pattern propagation, findings buffer, report-everything. This lens reports only convention violations; bugs, style and design belong to the other lenses even when you notice them.

## Method

Work through all five steps. Output must show each.

### 1. Inventory

List every conventions file you were handed (inlined or by path; read any listed file in full) and, per file, the changed files in its scope. A file's scope is its own directory and below. A changed file can fall under several, root first and deepest last; apply all of them. A conventions file that is itself in the diff is also checked against the others.

### 2. Extract rules

From each file, in document order, number every rule that can be checked against code or the change:

- imperative sentences ("Use X", "Add Y to Z")
- numbered or checklist steps in a workflow ("Adding a Model Method": every step is a rule)
- always / never / must / should not / required / only
- required tags, flags, annotations, headers, file locations, naming forms
- required commands (`make model`, a generator, a formatter, a codegen step) and the artifacts they leave behind
- worked examples that show the required shape, when the prose says to follow them

Write each as `N. <file> § <heading>: <rule, quoted>`. Skip pure description, history, rationale and advice with no checkable consequence. Keep rules that look unlikely to matter; step 3 discards them cheaply. Do not paraphrase away a qualifier: "for DB-managed columns" is part of the rule.

### 3. Scope each rule

For every rule: is any changed hunk in its scope (right directory, right kind of code, right trigger such as "when adding a model method")? Answer `in scope` or `out of scope: <reason>`. For in-scope rules, read the changed code in full and answer `complies` or `violates`, citing the line. Do not skip a rule because the diff looks unrelated at first read; a rule triggered by a new field, a new method or a new file is tested by looking for those in the diff.

Absence counts. If a rule says "every X also needs Y" and the diff adds X, then a missing Y is the violation even though no changed line shows it. Check the files where Y would live.

### 4. Run what the rule runs

A rule that requires a command is checked by its output, not by guesswork:

- Run it in a scratch copy of the workspace (never modify the checkout) when it is cheap: a generator, a formatter in check mode, a lint target, a codegen step. Diff the result against the head. Any difference is a violation of the rule, citing the stale file.
- When it cannot run here (needs network, services, credentials, a long build), inspect what it would have produced: which generated or derived files its inputs feed, and whether the diff updated them consistently.
- Say which you did. Inspection of a command you could have run is `confidence: low`.

### 5. Report violations

A violation is a rule the diff breaks. For each finding:

- `description` begins with the rule quoted verbatim, then its file and heading, then what the change does instead.
- `file` and `line` are the offending changed line, or the file where the required thing is missing.
- Severity by the rule's own weight: `P1` when the rule says must/never/required and breaking it breaks a build, a generated artifact, data or an invariant; `P2` for a stated requirement with softer consequences; `P3` for "prefer" and "should". A rule marked mandatory in the file keeps its severity whatever the diff size.
- The same violated rule at several sites is one finding with all `file:line` cited.

Report nothing else. If every in-scope rule complies, return an empty findings block and say in `meta` how many rules you extracted and how many were in scope.

## Pitfalls

| Pitfall | Do instead |
|---|---|
| Extracting only the rules that look relevant to the diff | Extract all, then scope; relevance judged early is how rules get missed |
| Treating a numbered workflow as background | Each step is a rule; check the last step as hard as the first |
| Counting a rule as met because the code "does something similar" | Compare to the rule's exact form: the tag string, the flag name, the command |
| Reporting a bug you noticed along the way | Drop it; another lens owns it |
| Assuming a generator's output is unchanged | Run it, or trace which files it writes |
| Citing the rule from memory | Quote it verbatim from the file you read |

## Output Format

Return findings as JSONL using the canonical schema in `../SHARED_CONVENTIONS.md` §3.

**`category`:** always `claude-md-violation`, whichever conventions file the rule came from.

**Lens-specific fields:**
- `rule` — the rule quoted verbatim.
- `rule_source` — `<path> § <heading>`.
- `checked_by` — `ran-command`, `inspected-output` or `read-code`.

**Confidence calibration:**
- **`low`** — inferred a command's result without running it though it was cheap to run, or the rule's scope is arguable.
- **`medium`** — rule and code both read, the violation is clear, no command was run.
- **`high`** — rule quoted from the file and the violation shown by a command's output or a line that contradicts it directly.

No finding cap — see `SHARED_CONVENTIONS.md` §5. In `## meta` or the `meta` field, report the count of rules extracted per file and how many were in scope.
