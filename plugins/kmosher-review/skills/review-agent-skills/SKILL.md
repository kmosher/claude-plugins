---
name: review-agent-skills
description: This skill should be used when reviewing a PR diff that touches Claude Code agent skill files (`SKILL.md`, `EXAMPLE.md`, `PROMPT_TEMPLATE.md` under `*/skills/<name>/`), slash-command files under `*/commands/<name>.md`, or plugin manifests (`.claude-plugin/plugin.json` / `marketplace.json`). Reviews skill-authoring quality: frontmatter schema, description-as-trigger, body voice, supporting-file references, side-effect safety, eval coverage. Distinct from `review-code` (correctness of any code in the skill) and `review-legibility` (general readability) — this lens applies rules specific to how Claude Code loads and routes skills, drawn from Anthropic's skill-creator, the official plugin-dev skill-development guide, and the Claude Code skills docs.
---

# Agent Skill Authoring Review

Review changes to Claude Code skill files against authoring best practices. Find skills that won't trigger when they should, will trigger when they shouldn't, silently drift from the frontmatter schema, or break their own bundled scripts and references.

## Shared conventions (read first)

Read `../SHARED_CONVENTIONS.md` before applying this lens — covers REVIEW.md overlay, pattern propagation, and the findings buffer. Skill-authoring violations are unusually pattern-shaped: if one new skill has "Use this skill when…" in its description (second-person, wrong), siblings authored at the same time usually do too. Always propagate.

## When to Use

**Use for:** any PR adding or modifying files under `**/skills/<name>/SKILL.md`, `**/commands/<name>.md`, `**/.claude-plugin/plugin.json`, `**/.claude-plugin/marketplace.json`, or `**/agents/<name>.md`. Also for renames of skill directories (the `name` frontmatter field, `${CLAUDE_SKILL_DIR}` references, and cross-skill mentions all need to track).

**Don't use for:** PRs that only touch test fixtures of skills, generated artifacts (`evals/`), or pure documentation files outside the skill body. Use `review-legibility` instead if the change is purely prose polish.

## Tooling available

Validation tools for SKILL.md are still thin. The lens leans on:

- **Schema reference**: `gh api repos/anthropics/skills/contents/skills/skill-creator/SKILL.md --jq .content | base64 -d` — canonical Anthropic skill-creator. Pull the latest version when reviewing; the schema can drift.
- **Plugin schema reference**: `gh api repos/anthropics/claude-plugins-official/contents/plugins/plugin-dev/skills/skill-development/SKILL.md --jq .content | base64 -d` — official plugin-dev writing-style rules (third-person description, imperative body, 1,500–2,000-word target).
- **Claude Code docs**: `WebFetch https://docs.claude.com/en/docs/claude-code/skills` — canonical for the frontmatter field set, character limits, `${CLAUDE_SKILL_DIR}` substitution, `allowed-tools` syntax, `context: fork` semantics, `disable-model-invocation` rules.
- **Repo `REVIEW.md`**: per-repo conventions override the schema defaults (e.g. a repo may forbid `version:` even though the docs tolerate it).

## Detection: is this a skill change?

Trigger this lens when the diff matches any of:

- `**/skills/<name>/SKILL.md` (added, modified, or deleted)
- `**/skills/<name>/<EXAMPLE|PROMPT_TEMPLATE|references|scripts>/**` — supporting-file changes that affect a skill
- `**/commands/<name>.md` — slash-command definitions follow the same frontmatter conventions
- `**/agents/<name>.md` — agent definitions follow similar conventions
- `**/.claude-plugin/plugin.json` or `**/.claude-plugin/marketplace.json` — plugin/marketplace manifests
- A directory rename under `skills/` — even if no file contents change, the `name` field, `${CLAUDE_SKILL_DIR}` references, and cross-skill mentions must track

If the diff has none of these paths, the lens declines to review and returns an empty findings block with a meta note.

## The Rules

Findings get severity per the calibration below. The lens reports the named rule (`rule` field on each finding) and the source doc (`source` field) so the author can verify the claim against the canonical reference.

### Hard rules (block-merge)

- **R-FRONTMATTER-PRESENT.** SKILL.md begins with YAML frontmatter delimited by `---` markers. *Source: Claude Code docs.*
- **R-NAME-VALID.** If `name:` is present: lowercase letters, numbers, hyphens only; max 64 chars; matches the containing directory name exactly (case-sensitive). *Source: Claude Code docs.*
- **R-DESCRIPTION-TRIGGERS.** `description:` states *when to invoke* (contexts, trigger phrases), not only *what the skill does*. Empty trigger surface ⇒ the skill won't load. *Source: skill-creator, skill-development, Claude Code docs.*
- **R-DESCRIPTION-LENGTH.** Combined `description` (+ `when_to_use` if present) fits under 1,536 characters. Past that, the listing silently truncates. Key trigger in the first sentence. *Source: Claude Code docs.*
- **R-DESCRIPTION-VOICE.** `description:` is third-person ("This skill should be used when…"), not "Use this skill when you…" or imperative-to-the-user. *Source: skill-development.*
- **R-BODY-VOICE.** SKILL.md body is imperative ("Parse the frontmatter"), not second-person ("You should parse the frontmatter"). *Source: skill-development.*
- **R-REFERENCES-EXIST.** Every file path referenced from SKILL.md (`references/foo.md`, `scripts/bar.py`, `${CLAUDE_SKILL_DIR}/baz.sh`) actually exists in the tree after the PR. *Source: skill-creator, skill-development.*
- **R-SCRIPT-PATH-PORTABLE.** Executable scripts referenced for Claude to run are addressed via `${CLAUDE_SKILL_DIR}/...`, never bare relative paths or hard-coded `~/.claude/...`. *Source: Claude Code docs.*
- **R-ALLOWED-TOOLS-SYNTAX.** `allowed-tools:` values use valid syntax — `Read`, `Edit`, `Bash(git add *)`, `Bash(git status)`, etc. Free-form `git, bash` is broken. *Source: Claude Code docs.*
- **R-SIDE-EFFECTS-GUARDED.** Skills with side effects (deploy, commit, push, send, post, delete, write-to-prod) set `disable-model-invocation: true`. Side-effect verbs in the description or the body trigger this rule. *Source: Claude Code docs.*
- **R-FORK-NEEDS-TASK.** Skills declaring `context: fork` contain an imperative task body, not just guidelines. A fork given only conventions returns nothing. *Source: Claude Code docs.*
- **R-SUPPORTING-FILES-REFERENCED.** `references/`, `scripts/`, `examples/` directories that exist in the skill MUST be mentioned somewhere in SKILL.md. Unreferenced files are invisible to Claude at runtime. *Source: skill-development.*
- **R-NO-TRIGGER-DUPE.** A new skill does not duplicate the trigger surface of another in the same plugin/repo without one of them setting `disable-model-invocation: true`. Description-budget contention silently routes to the wrong skill. *Source: Claude Code docs.*
- **R-RENAME-COMPLETE.** When a skill directory is renamed in the diff, *all* of: the `name:` frontmatter field, `${CLAUDE_SKILL_DIR}/<name>/...` references in this skill and others, cross-skill mentions (`Read \`../<name>/...\``), and any marketplace.json entries update consistently. *Source: skill-creator (preserve-name rule).*

### Soft rules (should-fix)

- **R-BODY-LENGTH.** Body under ~500 lines / ~2,000 words. Past that, split into SKILL.md (orchestration) + `references/*.md` (depth). *Source: skill-creator, skill-development.*
- **R-REFS-HAVE-TOC.** Reference files >300 lines include a table of contents; >10k words also document grep patterns in SKILL.md for selective loading. *Source: skill-creator.*
- **R-DESCRIPTION-EXAMPLES.** Description includes 3+ concrete trigger phrases in quotes (e.g. `"create a hook"`, `"add a PreToolUse hook"`). *Source: skill-development.*
- **R-DESCRIPTION-PUSHY.** Description leans slightly pushy ("Make sure to use this skill whenever…") to combat undertriggering. *Source: skill-creator.*
- **R-NO-DUPLICATION.** Information lives in SKILL.md OR a reference file, not both. *Source: skill-development.*
- **R-VARIANTS-IN-REFS.** Multi-domain skills organize per-variant guidance under `references/<variant>.md` rather than inlining all variants in SKILL.md. *Source: skill-creator.*
- **R-EXPLAIN-WHY.** Heavy use of ALL-CAPS MUST/NEVER without explaining the reason is a yellow flag. Each "MUST" should justify itself. *Source: skill-creator.*
- **R-EVALS-PRESENT.** Skills with verifiable outputs ship `evals/evals.json` with realistic prompts. (Not all skills can — flag only when the skill is plausibly evaluatable.) *Source: skill-creator.*
- **R-DIRECTORY-LAYOUT.** Plugin skills live at `plugins/<plugin>/skills/<skill-name>/SKILL.md`. *Source: Claude Code docs, skill-development.*

### Anti-patterns (auto-flag)

- **A-DESCRIPTION-USE-THIS.** Description begins with "Use this skill when…", "Load when…", "Provides…", or any first/second-person phrasing.
- **A-DESCRIPTION-NO-TRIGGERS.** Description has zero quoted trigger phrases and zero concrete scenarios.
- **A-BODY-SECOND-PERSON.** SKILL.md body contains **directive** second-person constructions: "you should X", "you need to X", "you must X", or instructions addressed to a "you" subject ("Then you parse the frontmatter"). Imperative ("Parse the frontmatter") is the target. *Not* a violation: rhetorical-conditional ("if you can't trace concretely…"), idiomatic phrases ("a luxury you can't afford"), or "you" in a misconception/quote/table column where the construction is paraphrasing user thought.
- **A-BLOATED-NO-REFS.** SKILL.md >500 lines AND `references/` is empty or missing.
- **A-ORPHAN-SUPPORT.** `references/`, `scripts/`, or `examples/` directories exist but are not mentioned anywhere in SKILL.md.
- **A-NAME-CASE.** `name:` present but contains uppercase, underscores, spaces, or doesn't match directory name exactly.
- **A-RENAME-STALE.** Skill renamed in the diff but the old name still appears in this skill's frontmatter, body, or cross-references.
- **A-BROAD-TOOLS-NO-GUARD.** `allowed-tools:` grants broad surface (`Bash`, `Bash(*)`, `Write`) without `disable-model-invocation: true`.
- **A-SIDE-EFFECT-OPEN.** Skill body or description implies side effects (deploy/commit/push/send/post/delete/write-to-prod) without `disable-model-invocation: true`.
- **A-FORK-NO-TASK.** `context: fork` declared without an imperative task body.
- **A-BAD-SHELL-INJECT.** `!`-prefixed shell injection appears mid-line rather than at line start or after whitespace — won't execute, will appear as literal text in the prompt.
- **A-FRONTMATTER-TYPO.** Frontmatter contains unknown fields (`tools:`, `allowedTools:`, `disable_model_invocation:`) — typos of the real schema.
- **A-VERSION-FIELD.** `version:` field present — not in the Claude Code frontmatter schema. Some Anthropic examples use it (skill-development's own SKILL.md), so flag as a Nit ("tolerated but unrecognized") rather than a P0 unless the repo's `REVIEW.md` says otherwise.

### Negative-space rules (things a good skill includes that bad ones omit)

- **N-WHEN-NOT.** Description has a "when NOT to use" paragraph or negative-trigger phrase. Prevents stealing turns from adjacent skills.
- **N-OUTPUT-FORMAT.** Skills producing structured output document the output shape.
- **N-WORKED-EXAMPLES.** 1–3 worked examples (input → output) inline or in `examples/`.
- **N-SUPPORT-FILE-HINTS.** Each supporting file gets a one-line "load when…" hint in SKILL.md.
- **N-FORK-DECISION.** Skills that don't need conversation history declare `context: fork` (and pick an agent type).
- **N-ARGUMENT-CONTRACT.** Skills using `$ARGUMENTS`/`$0`/`$1` declare `argument-hint` and document each positional.
- **N-EVAL-ASSERTION-SHAPE.** Bundled evals use the assertion fields `text`/`passed`/`evidence` (not `name`/`met`/`details` — the viewer breaks).

## Severity Calibration

- **P0** — `R-FRONTMATTER-PRESENT`, `R-NAME-VALID`, `R-SIDE-EFFECTS-GUARDED` for genuinely destructive side effects, `R-RENAME-COMPLETE` when the rename leaves dangling references. The skill won't load or will load and then misbehave.
- **P1** — Most other hard rules; all `A-*` anti-patterns that materially affect routing or trust (`A-BROAD-TOOLS-NO-GUARD`, `A-SIDE-EFFECT-OPEN`, `A-DESCRIPTION-NO-TRIGGERS`).
- **P2** — Soft rules; voice violations that don't break loading; missing TOC on large reference files.
- **P3** — Negative-space items; `A-VERSION-FIELD`; minor frontmatter style.

## Confidence Calibration

- **`low`** — flagged from naming or shape without reading the file body.
- **`medium`** — read the SKILL.md but didn't cross-check against the canonical schema doc on this run.
- **`high`** — read the SKILL.md *and* verified the cited rule against the canonical source (Claude Code docs, skill-creator, or skill-development) on this run.

## Output Format

Return findings as JSONL using the canonical schema in `../SHARED_CONVENTIONS.md` §3. Rendering to markdown is the invoker's job.

**Lens-specific fields:**
- `rule` — the named rule (e.g. `R-DESCRIPTION-TRIGGERS`, `A-DESCRIPTION-USE-THIS`, `N-WHEN-NOT`). The reviewer cites the rule code so the author can find it in this skill body.
- `source` — `claude-code-docs` | `skill-creator` | `skill-development` | `repo-review-md`. Lets the author check the canonical reference.

**`category` values for this lens:** `frontmatter` (schema, fields, lengths), `description` (triggers, voice, length), `body` (voice, length, structure), `supporting-files` (references, scripts, examples), `routing` (trigger contention, side-effect guard, fork semantics), `rename` (consistency across rename), `eval` (evaluator coverage/schema).

If the diff doesn't actually touch any skill files (false positive on routing), return an empty JSONL block with a meta note explaining the lens declined.

## Pairing With Other Skills

- **`review-code`** — runs alongside if the skill bundles non-trivial scripts (`scripts/*.py`, `scripts/*.sh`). This lens covers the SKILL.md contract; `review-code` covers the scripts' correctness.
- **`review-legibility`** — runs alongside for the body's general readability after structural rules are satisfied.
- **`review-compatibility`** — runs alongside on a skill rename, since renames are a compatibility event (the trigger surface changes for any caller referencing the old name).
- **`review-releng`** — generally not applicable; skills are configuration, not a deployable surface, with the exception of `disable-model-invocation` for destructive ops (which this lens already covers).

## Reference

- Anthropic skill-creator: <https://github.com/anthropics/skills/blob/main/skills/skill-creator/SKILL.md>
- Official plugin-dev skill-development: <https://github.com/anthropics/claude-plugins-official/blob/main/plugins/plugin-dev/skills/skill-development/SKILL.md>
- Claude Code skills docs: <https://docs.claude.com/en/docs/claude-code/skills>
- Pattern of citing the rule code in findings adapted from EON's structured-finding category field.
