# review-code: worked example

Real case: a PR adding `stripStaleDefaults` to a Go bridge between Pulumi and Terraform. Function strips entries from a `__defaults` metadata map when a field's TF schema no longer has a `Default`. ~10 prior review rounds, including LLM-driven adversarial passes, marked the PR ready.

**Required reading list given to the reviewer:**
- `pkg/tfshim/shim.go:130-200` — Schema interface, especially `Elem()` cases 1–5 and `Default()`/`DefaultFunc()`/`DefaultValue()` distinction
- `pkg/tfbridge/schema.go:740-995` — `applyDefaults` implementation
- The PR description's "settled issues" section

**Per-call trace finding:** the reviewer's predicate at `provider.go:2184` called `tfSchema.DefaultValue()`. Reading the shim implementation showed `DefaultValue()` invokes `DefaultFunc()` at runtime and returns `(value, error)`. The PR ignored the error and treated nil-return as "no default." For env-backed `DefaultFunc` with the env var unset, this would misclassify the field as stale.

**Verification:** reviewer re-opened `pkg/tfshim/sdk-v2/schema.go`, traced `DefaultValue()` → `DefaultFunc()` → confirmed the side-effect path. Filed as P1, high confidence.

**Adversarial test critique finding:** the unit test "field with current TF Default is preserved" asserted `result == m`. A buggy implementation that always returns `m` unchanged would also pass. Filed as P3 (test-strength gap), recommended adding a stripping-fixture variant in the same test to prove the function actually does work when it should.

**Negative-space finding:** function had no logging, used `log.Printf` directly instead of the package's `pulumilog.V(N).Infof` convention. P3, easy fix, surfaced through the negative-space step that explicitly looks at "what's absent that the surrounding code uses?"

**Self-verification step caught:** an earlier draft of the report claimed `stripMapOfBlocks` handled a nested-block shape "incorrectly." Re-reading the function showed the shape mismatch was real but the function early-returned safely on it (the `inner.IsObject()` guard). The reviewer downgraded the finding from P1 to P3 (dead code, not a bug).

**Net result:** 6 findings. 2 fixed (P1 predicate, P3 logging). 1 became a follow-up issue. 3 confirmed-but-deferred with documented reasoning.
