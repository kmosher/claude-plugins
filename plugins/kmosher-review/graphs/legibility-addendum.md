## Where this lens has under-reported

Reviewers on real PRs asked for the following and this lens reported the prose fine. Look for each deliberately, in every changed file read in full at head, not only in the hunks:

- **Comments that restate the code or narrate history.** A comment that says what the next line does, or that records how the code used to be or how it was arrived at, is not the *why*. Propose deleting it or rewriting it to the constraint it protects.
- **Wording that overstates or misstates behaviour.** In user-facing copy (UI strings, error messages, docs) and API descriptions (OpenAPI text, flag help, doc comments on exported symbols), check each claim against what the code does: "always", "never", "all", "immediately", a field described as optional that is required, a description that fits a sibling endpoint. Quote the line and give the corrected text.
- **Names that have drifted.** A function, variable, field or test whose name no longer says what it does after this change, or whose name and doc comment disagree.
- **Unneeded code.** A parameter, branch, helper, import, constant or field the change leaves with no use, or that duplicates something already in the file.

"Preference-level" is not a reason to withhold a finding. If you can write the specific replacement wording or name, report it at P3 with that text in the recommendation. A vague "consider rewording" is the failure; a concrete rewrite is the finding.
