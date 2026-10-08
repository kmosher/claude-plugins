## Your share of the code lens

Another reader is running this skill's techniques 1 through 8 and 10 (upstream reading, per-call trace, test critique, failure modes, walkthrough, negative space, run it) on the same diff. Do not run them.

Run technique 9 (reference resolution) and technique 11 (prose that describes the code), in every changed file read in full at head. Then do the sibling sweep.

**Sibling sweep.** For every defect you find, or that the diff itself reveals (a rename, a moved file, a changed signature, a new required input), enumerate every other site with the same shape before you write the finding. Grep for it: the old name, the old path, the same call, the same stanza in the neighbouring file. Do not stop at the first hit. Shapes to look for:

- A renamed `dump.sql`, still named by a Bazel `filegroup`, a second `BUILD.bazel`, and two Python scripts. The referrers span build files and code, so grep the whole tree, not one language.
- Two `dockerfile_lint` stages that both lack their build context, where the first one found is not the only one.
- The same payload gap in `Dockerfile.full` that the diff fixed in `Dockerfile`.
- A config key renamed at its writer while a second consumer, an alarm or a test fixture still reads the old one.

Report each site as its own finding, with that site's `file` and `line`. The canonical schema carries one anchor per finding, and `dedupe` by `file:line:category` would otherwise fold distinct sites into one. Repeat the shared cause in each `description` and name the sibling sites there, so each finding stands alone. If a site cannot be given a line, use `line: null` and quote the text.

Do not trace logic or critique tests unless a reference you resolved leads there; the other reader owns that.
