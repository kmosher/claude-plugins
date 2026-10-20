# review-code: worked example

A hypothetical case, composed to show the techniques: a PR adding `pruneStaleDefaults` to a Go configuration package. The function drops entries from a `__defaults` metadata map when a field's schema no longer declares a default. About ten prior review rounds, including LLM-driven adversarial passes, had marked the PR ready.

**Required reading list given to the reviewer:**
- `pkg/schema/schema.go:130-200` — the `Field` interface, especially the `Elem()` cases and the difference between `Default()`, `DefaultFunc()` and `DefaultValue()`
- `pkg/config/apply.go:740-995` — the `applyDefaults` implementation
- The PR description's "settled issues" section

**Per-call trace finding:** the reviewer's predicate at `prune.go:218` called `field.DefaultValue()`. Reading the schema implementation showed that `DefaultValue()` invokes `DefaultFunc()` at runtime and returns `(value, error)`. The PR ignored the error and treated a nil return as "no default." For an environment-backed `DefaultFunc` with the variable unset, that would misclassify the field as stale.

**Verification:** the reviewer re-opened `pkg/schema/v2/field.go`, traced `DefaultValue()` → `DefaultFunc()` and confirmed the side-effect path. Filed as P1, high confidence.

**Adversarial test critique finding:** the unit test "field with a current default is preserved" asserted `result == m`. A buggy implementation that always returns `m` unchanged would also pass. Filed as P3 (test-strength gap), with a recommendation to add a pruning fixture to the same test to prove the function does work when it should.

**Negative-space finding:** the function had no logging and used `log.Printf` directly instead of the package's `logx.V(n).Infof` convention. P3, easy fix, surfaced by the negative-space step that explicitly looks at "what's absent that the surrounding code uses?"

**Audit pass caught:** an earlier draft of the report claimed `pruneNestedBlocks` handled a nested-block shape "incorrectly." Re-reading the function showed the shape mismatch was real but the function returned early and safely on it (the `inner.IsObject()` guard). The reviewer downgraded the finding from P1 to P3 (dead code, not a bug).

**Net result:** 6 findings. 2 fixed (the P1 predicate, the P3 logging). 1 became a follow-up issue. 3 confirmed but deferred with documented reasoning.
