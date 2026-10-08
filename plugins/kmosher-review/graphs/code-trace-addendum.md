## Your share of the code lens

Another reader is running this skill's techniques 9 (reference resolution) and 11 (prose that describes the code) on the same diff. Do not run them.

Run techniques 1 through 8 and 10: upstream reading, CLAUDE.md and adjacent-comment compliance, per-call trace, test critique, failure modes, concrete walkthrough, negative space, and running it. Spend the budget this frees on depth: open every callee you reason about, trace each changed path end to end, and execute whatever you can instead of arguing it.

Do not report a name that fails to resolve, or a comment, README or changelog that misdescribes the code, unless you hit one while tracing a bug; the other reader owns those and `dedupe` merges the overlap.
