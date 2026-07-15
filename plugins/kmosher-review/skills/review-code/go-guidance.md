# Go review guidance

Two things for Go changes that don't belong in `SKILL.md`'s
language-agnostic body: bug-shaped anti-patterns whose underlying
mechanism only exists in Go, and design-level judgment — what linters
don't check. Curated from
[spf13/go-skills](https://github.com/spf13/go-skills), cross-pollinated
from the equivalent skill in `pulumi/background-agents`'s github-bot.

**Load this file only when the diff touches Go files (`.go`, `go.mod`).**
Skip it entirely otherwise — keep it out of the main `SKILL.md` body so
non-Go reviews don't pay for it.

Repo conventions win: `REVIEW.md`/`AGENTS.md`/`CLAUDE.md` rules take
precedence, and code following the surrounding codebase's established
pattern is not a finding. Skip anything the repo's linters or CI would
catch — that's `review-automated-checks.md`'s job, not this file's (on
a Bazel/`nogo` repo, most of the anti-patterns below are already
enforced at build time; check for `tools/lint/nogo_config.json` before
raising one as a novel finding — it may already be a required check).
In move/rename-heavy diffs, report a pre-existing defect in moved code
only when the move promotes it to new API surface, labeled
`pre-existing`. Version-gated suggestions (`t.Context()` 1.24+,
`testing/synctest` 1.25+): check `go.mod` first.

Concurrency ownership (every new goroutine needs a stop condition) and
the other language-agnostic bug-shaped concurrency/error patterns are
already covered by `SKILL.md`'s Anti-Pattern Checklist at `Important`
severity — don't re-raise them here.

## Anti-patterns (bug-shaped, Important by default)

Go-specific counterparts to `SKILL.md`'s Anti-Pattern Checklist — moved
here because the underlying mechanism (two-value type assertion,
randomized map iteration, `defer` semantics) doesn't exist in other
languages, so keeping them in the language-agnostic checklist would
waste attention on non-Go diffs.

- **Type assertion without `, ok`** — `x.(T)` single-value form panics
  on mismatch; use `v, ok := x.(T)` and handle `!ok`.
- **Map iteration assumed ordered** — Go randomizes map iteration order
  per the spec; code that reads `for k, v := range m` and depends on a
  stable order is non-deterministic. Watch for this feeding into
  generated output, hashing, or test assertions.
- **`defer` in a loop without a closure barrier** — defers stack to
  function exit, not loop iteration; a `defer` inside a `for` loop over
  N items holds N resources open until the function returns, not until
  each iteration ends. Wrap the loop body in a named function or an
  immediately-invoked closure to scope the defer per iteration.
- **Predicate using a runtime resolver when a structural accessor
  exists** — `Resolve()`/`DefaultValue()`-style calls (common in
  Viper-derived config APIs) return `nil`/error spuriously in cases a
  structural check (`Has()`/`Default()`/`DefaultFunc()`) would handle
  correctly; prefer the structural accessor when the codebase has one.

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
