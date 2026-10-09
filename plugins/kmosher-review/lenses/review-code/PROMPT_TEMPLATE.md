# Adversarial Review Prompt Template

Copy-paste and fill in the bracketed sections. Send the result to your reviewer agent.

---

# Adversarial review of <PR/branch>

You are reviewing a PR that has already had multiple rounds of review. Your job is not to redo their work — it's to find what they missed.

This prompt forces reviewing patterns automated reviewers skip. Follow it literally and **state your reasoning explicitly** for each technique.

## What to review

Repo: <path>
Branch: <branch> (latest commit <SHA>)
Files: <files in scope>

## REQUIRED reading before you analyze the change

[List 3–6 files or doc-blocks the reviewer must read in full. Choose: the
interface/shim layer the change uses, the canonical doc-block for the subsystem,
the implementation of any non-obvious function the change calls, prior PRs/issues
referenced by the code, and the PR description.]

After reading, the reviewer must report per file: "I read this. The relevant
facts are X, Y, Z. What I expected vs. what I found."

## Settled — DO NOT raise these (already adjudicated)

[List concerns that have been investigated and decided. Each gets a one-line
reason. If a reviewer disagrees with one, they should present new evidence —
don't surface it as a fresh finding.]

## Adversarial techniques you must use

For each, write your reasoning out explicitly. Don't claim you did it; show your work.

### 1. Per-call trace
For every function call inside the change, document: name, location, what it
actually does (read the body), caller's assumed precondition vs. callee's actual
contract.

### 2. Adversarial test critique
For each test in the change, write: "Here's a buggy implementation of the
function under test. Would this test catch the bug?"

### 3. Failure-mode enumeration (5+ scenarios)
List at least 5 concrete real-world cases the change does NOT fix. Be specific:
name the system, the configuration, the user-observed outcome.

### 4. Concrete walkthrough
Trace the full lifecycle for two specific real-world cases through every code
path the values touch. Real names, not placeholders.

### 5. Schema/data-shape semantics check
For test fixtures using complex types, verify the fixture's shape matches what
production code actually produces.

### 6. Negative-space audit
List what's absent that production code should have. Concrete only.

## Output format

Numbered findings. Each:
- **Severity**: P0 (data loss/panic), P1 (real bug, will hit users),
  P2 (correctness gap, narrow scope), P3 (test quality / readability)
- **File:line ref**
- **What's wrong** (1–3 sentences, concrete)
- **Bug it would cause** in a real user scenario
- **Proposed fix** (1–3 sentences)
- **Confidence**: low (speculated from naming) / medium (read related code,
  didn't fully verify the failure path) / high (read the implementation and
  traced an end-to-end scenario demonstrating the bug)

Report every finding you believe is real — no cap, no severity floor. Set
confidence honestly rather than dropping what you couldn't fully establish;
an independent pass downstream does the filtering. **If there are genuinely
no novel issues, say so explicitly** with reasoning. Do not manufacture
findings.
