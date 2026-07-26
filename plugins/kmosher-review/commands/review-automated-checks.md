# Automated lint/diagnostic checks for /review

This file is loaded **on demand** by the `/review` orchestrator when the
change touches a language whose recipes live here. Don't load it preemptively;
each language section is independent. The orchestrator should:

1. Detect which languages the diff touches (extension globs).
2. Read **only** the relevant subsection(s) of this file.
3. Run the project's own lint target first (`make lint`, `npm run lint`, etc.) if
   it exists — that's the authoritative configuration. Use this file's recipes
   as a *floor*, not a replacement.
4. Pass any findings into the review aggregation step alongside the skill
   findings, tagged as `automated/<tool>` so the user can see which came from
   tooling vs. judgment.

## Non-negotiable: honesty about tool failures

**If a tool invocation fails (non-zero exit due to an error, not merely due to findings), you MUST:**

1. Report the exact exit code and the exact stderr/stdout the tool produced — verbatim, unedited.
2. State clearly: "The tool failed; I cannot report findings from this run."
3. Do NOT synthesize, infer, or invent findings from memory or training knowledge.
4. Do NOT pretend the tool ran successfully and produce a finding list.

The failure itself IS the finding. A fabricated finding is worse than no finding because it takes hours of a human's time to disprove.

**Before filtering or summarizing any tool output, paste the raw output lines verbatim into your response.** Only then annotate or filter. This makes it impossible to silently confabulate results — the model cannot claim a finding exists if it can't point to the raw line.

### Sandbox requirements for Go CLI tools

`go run <module>@<version>` requires network access to fetch the module. In sandbox mode this goes through a MITM proxy and fails with TLS certificate errors. Two options:

1. **Preferred**: Use the pre-installed binary at `~/go/bin/modernize` (no network needed):
   ```bash
   ~/go/bin/modernize ./...
   ```

2. **Fallback if not installed**: Run with `dangerouslyDisableSandbox: true` to bypass the MITM proxy:
   ```bash
   # Only use dangerouslyDisableSandbox if ~/go/bin/modernize doesn't exist
   go run golang.org/x/tools/gopls/internal/analysis/modernize/cmd/modernize@v0.21.1 ./...
   ```

If even the fallback fails, report the error verbatim and do not produce findings.

---

## Project-target detection (run before language-specific commands)

Always check for these first; the project's own target is the source of truth:

```bash
# Make-based projects
[ -f Makefile ] && grep -E '^(lint|check|verify):' Makefile

# Node-based projects
[ -f package.json ] && jq -r '.scripts | keys[]' package.json | grep -E '^(lint|typecheck|check)'

# Cargo projects
[ -f Cargo.toml ] && grep -A2 '\[workspace\]\|\[package\]' Cargo.toml >/dev/null

# Bazel + nogo (build-time Go lint, common in Pulumi's Bazel-based repos)
[ -f MODULE.bazel -o -f WORKSPACE ] && [ -f tools/lint/nogo_config.json ]
```

If a project target exists, run it first and report its output. The
language-specific commands below catch things the project target may not
configure (e.g. modernize hints, unused-code warnings).

---

## Go

### Tool stack (run all that apply)

**If the project-target detection found `tools/lint/nogo_config.json` next
to a `MODULE.bazel`/`WORKSPACE`** (Pulumi's `nogo`-based repos — see
[nogo docs](https://github.com/bazelbuild/rules_go/blob/main/docs/go/nogo.md)),
treat that as the authoritative lint for Go files and skip straight to it —
`go vet`/`staticcheck` run standalone would either duplicate or miss what
`nogo` enforces (it wraps both plus repo-specific analyzers like a copyright
header check, an ORM-usage restriction, or a test-kind linter — read
`tools/lint/AGENTS.md` if present for the current analyzer list):

1. **`bazelisk build //...`** (or a narrower path covering the changed
   packages) — every registered `nogo` analyzer runs at build time and
   blocks on violations, so a failing build IS the finding set. Report the
   exact analyzer name and message from the error output.
2. **`bazelisk run //:fix -- //<pkg>/...`** is available in some repos for
   auto-fixable violations (gofmt/goimports-class findings) — don't run it
   yourself (it mutates files); just note it's available in the report.
3. Skip steps 2–4 below entirely — `nogo` supersedes them for this repo.
   Modernize hints are the one thing `nogo` typically doesn't cover; run
   step 4 anyway (see below) only for that gap, and label those findings as
   the modernize-only source so the report doesn't imply you re-ran vet or
   staticcheck redundantly.

**Otherwise** (no `nogo_config.json` — the common case for smaller/non-Bazel
Go repos):

1. **Project target**: `make lint` if Makefile has it, else skip.
2. **`go vet`**: `go vet ./...` — built-in correctness analyzers.
3. **`staticcheck`**: `staticcheck ./...` — additional analyzers (unused code,
   simplifications, common bugs). Install: `go install honnef.co/go/tools/cmd/staticcheck@latest`.
4. **gopls modernize**: catches `mapsloop`, `forvar`, `minmax`, `efaceany`,
   `slicescontains`, etc. Two ways to access:
   - **MCP route (preferred for changed files)**: call `mcp__gopls__go_diagnostics`
     with the changed file paths. Same engine the IDE uses; surfaces hints
     inline alongside errors. Requires the gopls MCP to be configured; if it
     isn't, skip this route and use the CLI route below.
   - **CLI route (for whole-package sweeps)**: prefer the pre-installed binary
     to avoid sandbox network issues:
     ```bash
     ~/go/bin/modernize ./...
     ```
     If not installed, see the sandbox requirements section above for the
     `go run @version` fallback. Pin the version to the locally installed
     `gopls version` for parity.

### Tooling-specific gotchas

- `mcp__gopls__go_diagnostics` only inspects files you pass in; it does not
  walk dependents. Use the modernize CLI to catch hints in untouched files
  the diff might affect (e.g. a renamed helper still referenced elsewhere).
- `staticcheck` and `go vet` share some checks but not all — run both.
- `nogo`'s `nogo_config.json` holds per-analyzer file exclusions
  (generated mocks, vendored code) — do not re-flag something the config
  deliberately excludes; that's a repo decision, not a gap.
- For codebases with verbose `go test ./pkg/...` (lots of integration-style
  tests, large packages), run with `-json` and `jq`/`grep` for failures. The Bash tool
  auto-truncates long output to a preview plus a saved file path — grep that
  file rather than dumping full output into the review context.

### What to surface in the review report

- **High-confidence** (real bugs): `nogo`-blocked build failures (`nogo` repos),
  vet errors, staticcheck SA-class findings, build failures.
- **Medium-confidence** (style/idiom): modernize hints, staticcheck S-class
  (style) findings — surface as P3 unless the diff touches the affected line.
- **Low-confidence**: modernize hints in untouched files — note as a footnote;
  don't promote to a finding unless asked.

---

## TypeScript

### Tool stack (run all that apply)

1. **Project target**: `npm run lint`, `npm run typecheck`, `pnpm lint`, or
   `yarn lint`, depending on the project's package manager. Check `package.json`
   `scripts` first.
2. **`tsc --noEmit`**: type-check without writing JS. The project may already
   have a `typecheck` script that does this with its own config — prefer that.
3. **ESLint**: `npx eslint --ext .ts,.tsx <changed-files>`. Honors the
   project's `.eslintrc` or flat config. Without project config, lint output
   is meaningless — skip if no config is present.
4. **Biome (alternative to ESLint)**: `npx biome check <changed-files>` if the
   project has a `biome.json` instead of an ESLint config.
5. **`tsc-strict-on-changed`** (optional): some teams run a stricter pass
   only on changed files. Look for a `tsc:strict` or `lint:strict` script
   before assuming.

### Tooling-specific gotchas

- ESLint with `--cache` is much faster on subsequent runs. For noisy first
  runs, use `--format json` and `jq` for the offending files.
- A TypeScript-LSP MCP (e.g. the `claude-plugins-official/typescript-lsp`
  plugin) provides equivalent diagnostics via LSP — comparable to gopls's MCP
  but for TS. If you have one installed and it exposes diagnostics through a
  tool, prefer that over CLI invocation for changed files.
- `tsc --noEmit` against a large project is slow; if the project has
  `tsconfig.json` references (composite mode), use `tsc --build` instead so
  it can use the build cache.
- Type errors caused by upstream library changes will appear in `node_modules`
  paths — filter those out of the report; they're not actionable in this PR.

### What to surface

- **High-confidence**: tsc errors, ESLint `error`-level rules, syntax errors.
- **Medium-confidence**: ESLint `warn` rules, no-explicit-any in new code.
- **Low-confidence**: project-wide stylistic warnings unrelated to the diff.

---

## Rust

### Tool stack (run all that apply)

1. **Project target**: `make check` or `make lint` if present, else `cargo`
   targets directly.
2. **`cargo check`**: `cargo check --all-targets` — fastest correctness pass.
3. **`cargo clippy`**: `cargo clippy --all-targets -- -D warnings` — the lint
   suite. The `-D warnings` flag turns warnings into errors; drop it for a
   review pass that just *surfaces* findings without failing.
4. **`cargo fmt --check`**: `cargo fmt --all -- --check` — formatter parity.
5. **`cargo test --no-run`**: compiles tests without running them. Surfaces
   test-only build errors that `cargo check` misses (test code is conditionally
   compiled).
6. **`cargo deny`** (optional): `cargo deny check` for license/security audits
   if the project has a `deny.toml`.

### Tooling-specific gotchas

- `cargo clippy` lints depend heavily on the `clippy.toml` and `lints` table
  in `Cargo.toml`. A blank-config run vs. a project-configured run will
  produce different findings — always honor the project config.
- Workspaces: use `cargo clippy --workspace --all-targets` to lint every crate.
- `cargo check` does not run proc-macros' compile-time checks the same way
  `cargo build` does in some edge cases — for diff-affecting macro work, run
  `cargo build` instead.
- `rust-analyzer` (the LSP) surfaces additional hints via diagnostics. If a
  rust-analyzer LSP/MCP integration is available in the harness, prefer that
  for changed-file diagnostics.

### What to surface

- **High-confidence**: rustc errors, clippy `correctness` and `suspicious`
  category lints, fmt drift on changed files.
- **Medium-confidence**: clippy `style` and `complexity` lints on changed code.
- **Low-confidence**: clippy `pedantic` (only run if explicitly requested),
  fmt drift in untouched files.

---

## Markdown (skills, docs)

1. **Formatter parity**: `prettier --prose-wrap always --print-width 100 --check <changed .md files>`
   — reflow drift on skill files and docs. If the project pins its own prettier
   config (`.prettierrc`, `package.json#prettier`), drop the flags and honor it.
2. Fix mode is the same command with `--write` — the `write-agent-skills` REFLOW
   step; safe to apply, prettier is semantic-preserving on markdown.

### What to surface

- **High-confidence**: drift on changed files only. Untouched-file drift is
  pre-existing; don't surface it.

---

## Adding a new language

When you find yourself running ad-hoc lint commands for a language not listed
above, add a section here. Pattern: project-target detection → tool stack →
gotchas → what to surface. Keep the language sections independent so the
orchestrator can load just one without dragging in the others.
