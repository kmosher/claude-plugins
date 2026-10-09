## Cross-read: the files outside the diff

The "Context from mechanical tools" section of the prompt holds entries from the `context-pack` tool. They are not findings and are not to be repeated or reported. Each is a pointer of the form `<changed file> → <kind>: <path>[:<line>] — <why>` to a file outside the diff that bears on a changed file: a `referrer` mentions a changed, renamed or deleted path; an `include` is something a changed file includes, imports or builds from; a `package` entry is a Go sibling in the same package; a `sibling` is a function in the same file named like one that changed. The pointers are a map, not a complete one. Search for more of the same kind yourself.

Before judging a changed file, open what points at it and what it points at. Read the other file; do not infer what it says from its name.

- **Verify every claim in prose against the file it is about.** A changed comment, commit message, PR description or doc that says "only", "always", "never", "already", "unused", "the sole caller" or "defaults to" is a claim about another place. Open that place. If a pointer names a file that contradicts the claim, that is a finding, with both locations quoted.
- **Check inherited settings.** For a Makefile, read each included file before reasoning about shell flags, `.ONESHELL`, `SHELL` or variable defaults; a header may already set what the diff adds, or override it.
- **Compare each edited function with its named siblings.** Read the `sibling` and `package` pointers for the guards, null checks, transaction handling, error wrapping and cleanup they have and the edited function lacks, or the reverse. A difference with no reason in the code is a finding.
- **Walk every referrer of a deleted or renamed path.** For each `referrer` entry on a deleted or renamed path, open the line and decide whether it still resolves: build files, scripts, docs, CI and Dockerfiles do not fail at compile time. Report each stale one with its `file:line`. Also grep for the path in forms the pointers could not match (relative, split across a variable, with a different prefix).
- **Ask what an error-handling edit now swallows.** For `catch`, `recover`, `|| true`, `.catch`, `set +e`, `2>/dev/null` or a broadened `except`, list what else can reach the handler besides the case the change had in mind, and say what the caller sees when that happens.

Spend the reads on the pointers that touch the changed lines first. If the pack is empty or none apply, say nothing about it.
