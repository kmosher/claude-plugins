# The review ladder

Where the suite is heading: a sequence of increasingly expensive rungs, so the
cheap checks run after every commit and the deep ones run once, on code that is
about to ship.

## What the design responds to

- **Serial lenses cost the whole wall-clock.** The lenses do not depend on each
  other, and the gate that justifies ordering them almost never fires, so they
  can run concurrently.
- **A good deal of reading is repeated.** Each secondary lens re-reads much of
  what the first one read, and no lens uses an LSP tool.
- **Findings are recall-first.** The auditor downgrades a fair share and drops
  few, so the suite emits several times the findings per PR of a general-purpose
  review.
- **What other reviewers find that this suite doesn't**: names that stopped
  resolving (a renamed metric label, a build context not added to the lint
  target), prose that no longer describes the code, tests that cannot fail, and
  anything that needed running to see.
- **Blind comment readers find review-grade things** at a fraction of a
  review's cost: two readers, a few thousand output tokens, a few minutes.

## The rungs

1. **Mechanical.** One script, exit code, unwaivable: lint, typecheck, LSP
   diagnostics, and the triggers that select later rungs — test files
   touched, exported signature changed, migration or runtime config touched,
   deploy manifest touched. Seconds. Runs after every commit in a dev loop.
2. **Default suite.** The orchestrator pre-reads the diff, the changed files
   and their direct imports once, then forks `review-code` and
   `review-legibility` so both inherit that context; they run concurrently.
   `review-compatibility` and `review-releng` run only when rung 1 tripped
   their triggers. Set `subagentPromptCacheTtl` to `1h` first: the 5-minute
   default guarantees a cache miss on the second lens of any serial run.
3. **Fix.** Apply the cheap findings before anyone reads deeply, so the deep
   rungs read the code that will ship. A separate fixer role applies patches;
   reviewers emit them as suggestions and never grade their own fix.
4. **Deep.** Blind readers (finalize's), the altitude lens — was this the
   right fix, which invariants are load-bearing — and the cross-model critic
   on minimal context. Seeds: surviving mutants from rung 5, the fix diff.
5. **Verify.** Run the tests, mutate the changed files, execute a repro per
   candidate finding. Fails closed: a finding that cannot be made to fail is
   posted as a question, not a finding.

Full work-up = all five. Everyday `/review` = rungs 1–3. Dev loop = rung 1
after every commit, rung 2's readers after each unit of work.

## Re-review

Record the reviewed head per branch. A re-review diffs `<last head>..HEAD`,
carries the prior findings in as context, raises the severity floor each
round, and caps rounds before escalating to a person. A rebase invalidates
the recorded head; fall back to a full review rather than guess. Contested
decisions the author has already defended are recorded as settled and
suppress re-raising; security, logic and null-deref findings are never
suppressed by any learned rule.

## Model ladder

Haiku: eligibility, lint sweep, prior-PR mining. Sonnet: rung 1's gates,
mechanical fixes, and the legibility lens. Opus: review-code, the fixer for behaviour
changes, the altitude lens. Codex (whatever the CLI is configured with) as
the critic, never a fifth generator. One top-tier pass at the end of a full
work-up, on the diff plus the verify report, where the context is small and
the question narrow.

## Measuring it

Score each arm against the comments other reviewers left and against a
first-revision oracle (the regions the merged revision changed since the first,
by cause: review-driven, human-prompted, contested decision, drift). Each rung
change should move one of: findings on later-changed regions per dollar,
findings on never-changed code, wall-clock to first finding.
