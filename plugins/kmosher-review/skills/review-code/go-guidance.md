# Go design-level review guidance

Design-level judgment for Go changes — what linters and the generic
Anti-Pattern Checklist in `SKILL.md` don't check. Curated from
[spf13/go-skills](https://github.com/spf13/go-skills), cross-pollinated from
the equivalent skill in `pulumi/background-agents`'s github-bot.

**Load this file only when the diff touches Go files (`.go`, `go.mod`).**
Skip it entirely otherwise — keep it out of the main `SKILL.md` body so
non-Go reviews don't pay for it.

Repo conventions win: `REVIEW.md`/`AGENTS.md`/`CLAUDE.md` rules take
precedence, and code following the surrounding codebase's established
pattern is not a finding. Skip anything the repo's linters or CI would
catch — that's `review-automated-checks.md`'s job, not this file's.
In move/rename-heavy diffs, report a pre-existing defect in moved code
only when the move promotes it to new API surface, labeled
`pre-existing`. Version-gated suggestions (`t.Context()` 1.24+,
`testing/synctest` 1.25+): check `go.mod` first.

Concurrency ownership (every new goroutine needs a stop condition) and
the other bug-shaped concurrency/error patterns are already covered by
`SKILL.md`'s Anti-Pattern Checklist at `Important` severity — don't
re-raise them here. This file is for the softer, Go-idiom-specific
judgment calls that checklist doesn't cover.

## Abstraction and interfaces

- A NEW interface with a single implementation and nothing motivating a
  second (no test fake, no second consumer) is premature abstraction —
  Nit; suggest starting concrete.
- A NEW wide exported interface (5+ methods) whose consumers each use
  only a small slice: suggest the smallest interface the consumer needs
  — Nit. Never flag additions to existing wide interfaces.
- Generics used to build type hierarchies (`Repository[T]` with
  Find/Save) instead of deduplicating an algorithm across types — Nit.

## Concurrency (idiom, not ownership)

- Unbounded fan-out (one goroutine per element of caller-sized input):
  suggest `errgroup` with `SetLimit` or the repo's established pattern —
  Nit; Important when the input size is externally controlled.
- `context.Background()` where a caller context is in scope — Nit. For
  work that must outlive a request, suggest `context.WithoutCancel(ctx)`
  so values and tracing survive detachment.

## Errors

- New error paths should wrap with operation context
  (`fmt.Errorf("parsing manifest %s: %w", path, err)`). Follow the
  repo's existing message style — capitalization or phrasing mismatches
  with this example are not findings.
- Logging an error and also returning it duplicates the signal — Nit;
  suggest handling it once (usually: return it, log at the boundary).
- Matching on `err.Error()` contents instead of `errors.Is`/`As` (or the
  repo's error helpers) breaks when messages change — Important.

## Tests

- Three or more near-identical test functions or assertion blocks added
  in this diff: suggest one table-driven test with `t.Run` subtests —
  Nit. Don't flag moved code or tests already structured as subtests.
- Assertion helpers missing `t.Helper()` report failures at the wrong
  line — Nit.
- `time.Sleep` to wait for concurrent work is a flake generator: suggest
  channels, `testing/synctest`, or the repo's wait helpers — Nit;
  Important in tests guarding correctness-critical paths.
- Large inline fixture or expected-output strings: suggest `testdata/`
  files — Nit.

## Naming — new identifiers only, all Nit

- Package-name stutter in new exported identifiers
  (`config.ConfigParser` → `config.Parser`).
- Receiver names inconsistent with the type's existing methods.
- Initialism casing inconsistent with the surrounding code (`Url`,
  `Id` where the codebase writes `URL`, `ID`).

## Go design-doc or spec PRs

When the PR is primarily a design document for Go work: check for
missing error/failure paths, a missing shutdown or rollback story for
long-running components, abstractions with no stated requirement
backing them (YAGNI), and interfaces designed before a second
implementation exists. Raise ambiguities as questions in the review
summary rather than Important findings — unless the gap would lead to a
flawed implementation (data loss, an unrecoverable rollout), which stays
Important.
