# review-legibility: worked example

Real case: a `stripStaleDefaults` function in a Go bridge accumulated 6 ordered if-branches with ~50 lines of inline rationale during PR iteration. Adversarial review confirmed each branch was correct. Legibility review applied the heuristics:

**Test 3 (branch fanout):** counted 6 ordered if-branches with non-trivial reasoning each. Threshold violated. Extract classifier.

**Test 8 (comment-to-code distance):** found a 50-line inline comment block over a 6-line predicate. Comment was doing real work but in the wrong place — should be a function-level doc on the predicate.

**Test 5 (reader onramp):** parent function `stripStaleDefaults` had a 60-line doc that mostly described the *predicate* (which is just one of three things the function does — classify, recurse, build result). Doc didn't help a fresh reader understand the function's purpose, just the rule.

**Proposed cleanup (sketched in finding):**
1. Extract `classifyStaleDefault(name, tfs, ps) → staleDefaultDisposition` helper
2. Move the 6-branch logic into a switch with a one-line case per rule
3. Move the 50-line rationale to the helper's doc, structured as a numbered precedence list
4. Trim parent doc to focus on what `stripStaleDefaults` itself does

**Audit pass caught:** an earlier draft of the report claimed the parent doc was "30 lines about the predicate." Re-reading showed it was actually ~60 lines. Reviewer corrected the finding before submitting.

**Net result:** 70 inserts, 83 deletes — code shrank while gaining clarity. **Good cleanups are usually smaller than the original**; that's the legibility-review signal.
