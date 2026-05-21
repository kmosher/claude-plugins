---
name: review-compatibility
description: Use when a PR could break callers, consumers, or future code that hasn't redeployed yet — DDL changes, protobuf/Avro schemas, message formats, serialized state, API request/response shapes, function signatures of exported/public symbols, behavior changes that callers depend on, config-key/env-var renames, exit-code changes, default-value flips. Anything where "old + new code coexist briefly" or "external callers can't update atomically" applies.
---

# Compatibility Review

Review changes through the lens of "what happens to code/data/callers that haven't seen this change yet?" Two flavors of compatibility live here:

1. **Data-shape compatibility** (the bulk of this skill) — DB schemas, message formats, stored state, file layouts. Asks: can this be deployed safely when readers and writers don't update atomically?
2. **Interface compatibility** — exported function signatures, public-API request/response shapes, config-key and env-var names, exit codes, behavior semantics (return-value meaning, default-value flips). Asks: will existing callers break, silently misbehave, or need coordinated upgrades?

Distinct from review-releng — that's about operational posture (revert, rollout, observability). This is specifically the joint state of code, data, and callers across a deploy boundary.

## Shared conventions (read first)

Before applying this lens, read `../SHARED_CONVENTIONS.md` for the four conventions that apply to every `kmosher-review` skill:

- **REVIEW.md overlay** — if the repo has a `REVIEW.md`, its rules override this skill's defaults.
- **Pattern propagation** — when you find a shape-compatibility issue on one endpoint/schema/message, scan all other diff files for the same shape before moving on. Sibling endpoints and migrations almost always share the same gap.
- **Findings buffer** — buffer findings to a JSONL file, dedupe and self-verify before emitting.
- **Comment body schema** — the canonical render shape for findings posted to GitHub.

## When to Use

**Use for any of:**
- **Data shape** — DB schema changes (DDL); message format changes (protobuf, Avro, JSON); stored state shape (cache keys, file layouts, serialized objects); API request/response body changes; cross-system data migrations.
- **Interface** — exported / public function signatures (added/removed/reordered args, narrowed types, return-type changes); REST/gRPC endpoint shape changes; CLI flag rename/removal; env var rename/removal; config-key rename/removal; behavior semantics change (same signature, different meaning); default-value flips.

**Don't use for:** in-memory data structure changes that don't cross deploy boundaries; single-binary atomic-deploy systems with no external callers; test fixtures; internal helpers with no external consumers.

If unsure: "during the deploy window or after merge, will there be **code, data, or callers** that haven't seen this change AND that depend on the prior shape/contract?" If yes, this skill applies.

## Tooling available

Migration review is mostly judgment, but a few automated checks back it up:

- **Generated-code drift**: if the change touches `.proto`, `.avsc`, or
  similar IDLs, run the project's codegen target (often `make generate` or
  `buf generate`) and check that the committed generated code matches the
  schema. Drift is a deploy hazard.
- **Type-check across the boundary**: run `tsc --noEmit`, `cargo check`, or
  `go build ./...` after the change, especially for client/server contract
  changes — type errors here are usually the first symptom of an unsafe
  shape change.
- **DB migration linters** (project-specific): `sqitch verify`, `atlas migrate
  lint`, `pgcli` syntax check. Look for project lint targets first.

Per-language recipes in `review-automated-checks.md`. The Go MCP
(`mcp__gopls__go_diagnostics`) and equivalent LSPs for other languages catch
type-shape regressions on the *consumer* side after a producer change.

## Prioritization Hierarchy

1. **Forward + backward compatibility** — old code reading new data, new code reading old data, both directions, every time.
2. **Reversibility** — abandon the migration without data loss?
3. **Idempotency** — migrations get retried; running twice must be safe.
4. **Speed** — fast is usually safer, but never sacrifice the above three for it.

## Core Principles

- **Deploys are partial.** Old + new code coexist for tens of minutes (rolling deploys) to hours (multi-service). Plan for it.
- **The migration is its own deploy.** Migration + code-that-depends-on-it = two PRs, ideally separated by a release.
- **Data outlives code.** Code reverts in seconds; data lives indefinitely. Asymmetry of stakes → migrations need more scrutiny than ordinary code.
- **Every migration runs at least twice.** Staging + production, often more (multi-region, retries, manual re-runs). Idempotency isn't nice-to-have; it's the contract.
- **Never trust "no one is reading this column."** Verify with logs, query auditing, or a deprecation period — not grep.

## Migration Patterns

### Schema additions (low risk)

| What | Pattern |
|---|---|
| Nullable column | Single deploy: ship code that writes + reads. Old code ignores it. |
| Index | `CREATE INDEX CONCURRENTLY` (Postgres) or equivalent. Verify the index is *used*. |
| New table | Single deploy. |
| New protobuf field (tagged) | Single deploy. Protobuf is forward-compatible by design — verify receivers don't choke on unknown fields. |

### Schema modifications (higher risk, multi-phase)

| What | Pattern |
|---|---|
| **NOT NULL on populated table** | Three phases: (1) add nullable; (2) backfill + write to it; (3) add NOT NULL after verified. Never one step. |
| **Rename column** | Don't. Add new, dual-write, switch readers, deprecate old, drop after a release. |
| **Change column type** | Add new column with new type, dual-write, migrate readers, drop old. Shortcut (`ALTER COLUMN TYPE`) often locks the table. |
| **Add unique constraint / check** | Validate passes for existing data first. Postgres: `ADD CONSTRAINT ... NOT VALID` then `VALIDATE CONSTRAINT`. |
| **Drop column** | Three steps: stop reading; verify nothing reads for one full traffic cycle; drop. |
| **Rename table** | Same as column rename — view-based redirect or dual-write, never atomic. |

### Data backfills (highest risk)

| What | Pattern |
|---|---|
| Computed-column backfill | Idempotent batch job, not inline with deploy. Progress tracking, pause/resume. |
| Format conversion | Dual-write in code; backfill historical data with separate job; verify before switching reads. |
| Cross-system migration | Dual-write to both systems; audit comparison; switch reads; retire old writes. |

### Message format changes

| What | Pattern |
|---|---|
| New optional field | Forward-compatible by default. Verify receivers tolerate unknown fields. |
| Remove field | Deprecate (keep but stop using); verify no consumers; remove later. |
| Rename field | Dual-write pattern; never atomic. |
| Change semantics (same type, different meaning) | Add a *new* field; never silently change existing meaning. |

### Interface compatibility (exported APIs, signatures, config, CLI)

| What | Pattern |
|---|---|
| Add optional arg / field with safe default | Single deploy. Verify callers compile against old signature (if statically typed) or behave identically (if dynamic). |
| Add required arg | Don't. Either add as optional with a default, or introduce a new function and deprecate the old. |
| Remove arg / field | Two steps: stop reading on the callee side, soak for a release, then remove from the signature. |
| Rename arg / field / config key / env var | Don't atomic-rename. Accept both names (old as alias); log a deprecation when the old name is used; remove the old after a release. |
| Reorder args | Don't. New function with new arg order; deprecate old. Positional reorders are silent miscompilation hazards. |
| Narrow a type (e.g. `string` → `enum`) | Validate first that all callers pass values in the new narrower set. Add validation that *logs* before it *rejects*; flip to reject after soak. |
| Change return-value semantics (same type, different meaning) | Don't. Add a new function / endpoint with the new semantics. Silent semantic drift is the worst-case compat failure — callers don't know they need to update. |
| Change a default value | Treat as a behavior change. Document; pre-announce; gate behind a config / flag with the old default initially; flip the default after consumers have opted in. |
| Remove a CLI flag | Two steps: accept-and-ignore-with-deprecation-warning, then remove. Scripts in CI / runbooks / users' shells will break otherwise. |
| Change a CLI exit code | Treat as a contract change. Document; treat the same as removing a flag. Scripts branch on exit codes. |
| Remove a public REST/gRPC endpoint | Return 410 Gone (not 404) with a deprecation header for at least a release. Verify with logs that no clients are calling it. |

**The unifying check:** for any *interface* element your change touches, ask "who calls this from outside the change's blast radius?" (other services, CLIs, scripts, browser clients, third parties, automation). If the answer is non-empty, the change is a compatibility event and needs the corresponding pattern above — not just a code update.

## Common Misconceptions

| Misconception | Reality |
|---|---|
| "It's just a column rename" | Renames are dual-writes in disguise. No atomic rename across rolling deploys. |
| "Old code won't write to the column anymore" | Old code might run for 30 minutes during deploy. Or 6 months in a stale background worker. |
| "Nobody reads this column" | Verify, don't assume. `pg_stat_statements`, query logs, or deprecation period. Grep is not evidence. |
| "Postgres handles it" | Postgres handles a lot. Not "add NOT NULL to populated table" without scanning under a lock. Read the actual DDL semantics. |
| "Migration ran in staging fine" | Staging tables are 1000x smaller. 50ms in staging can be 1 hour locking prod. |
| "We can always rollback" | Rolling back a schema change is *another* schema change, deployed under pressure during an incident. Forward-only is often safer. |
| "Idempotency is just `IF NOT EXISTS`" | True for DDL, not backfills. "Increment counter" is not idempotent; "set to default if missing" is. |
| "We'll backfill on read" (lazy) | Lazy migrations leave data inconsistent for as long as some rows are unread. For long-tail data, that's forever. |
| "Dual-write means safe" | Dual-write is safe *if* you have parity audit and *if* you actually verify it. Without verification, it's twice the bug surface. |

## Anti-Pattern Checklist

Almost always wrong.

- [ ] `ALTER TABLE` adding NOT NULL on populated table without `NOT VALID` (table-lock for scan duration)
- [ ] `ALTER COLUMN TYPE` on populated table (rewrites whole table under a lock)
- [ ] `CREATE INDEX` without `CONCURRENTLY` (Postgres: locks writes for build duration)
- [ ] `DROP COLUMN` without verifying no readers
- [ ] One-shot `UPDATE` over whole table (long txn holds locks; killed = all work lost)
- [ ] Migration in same PR as the code that depends on it (couples changes that should ship separately)
- [ ] Backfill in the migration itself (DDL + DML interleaved; extends lock window)
- [ ] No rollback plan for irreversible migration
- [ ] Schema migration without corresponding ORM/code migration
- [ ] Format change without a version field
- [ ] Dual-write without audit / comparison job
- [ ] "Convenient" backfill that touches every row to set a value 99% already have

**Interface compatibility anti-patterns:**

- [ ] Add a required argument to an exported / public function (silent caller breakage)
- [ ] Reorder positional arguments (silent caller miscompilation; statically-typed languages may catch some, dynamic languages won't)
- [ ] Rename a config key, env var, or CLI flag without an alias / deprecation period
- [ ] Change return-value semantics on the same type (e.g. nil-means-success now means error; same struct, different meaning of a field)
- [ ] Flip a default value silently (existing callers depending on the old default now misbehave)
- [ ] Remove a CLI flag or REST endpoint without a deprecation phase
- [ ] Narrow an accepted-value set (e.g. enum addition) without first validating that all callers send values in the new narrower set
- [ ] Change a CLI exit code without documenting (scripts branch on exit codes)
- [ ] Public REST/gRPC endpoint removal with 404 instead of 410 + deprecation header

## Self-Verification (before submitting findings)

For each finding, re-read the migration SQL, the code that writes data, and the code that reads data. Confirm the cited risk is real. State which findings were verified by inspecting actual artifacts versus inferred from structure.

## Output Format

Numbered findings. Each: severity (P0–P3), file:line or PR-level, which check/anti-pattern, what's wrong (concrete — name the locked table, the unbatched UPDATE, the missing dual-write), required action (P0/P1) or suggestion (P2/P3).

For each P0/P1: propose the migration pattern that addresses it, the deploy ordering required (which PR ships first, soak time), and the rollback procedure.

**Output phrasing — concrete language wins:**
- ❌ "Consider safety of this migration"
- ✓ "`ALTER TABLE foo ADD CONSTRAINT bar NOT NULL` on a 50M-row table will hold an exclusive lock for the table-scan duration. Use `ADD CONSTRAINT bar NOT NULL NOT VALID` followed by `VALIDATE CONSTRAINT bar` (non-blocking) instead."

If the PR passes the checklist, say so. Don't manufacture.

## Worked Example

See `EXAMPLE.md` in this skill's directory — a PR adding `region` column to a `customers` table that surfaced 2× P0 (DDL lock-out, non-concurrent index) and 2× P1 (reader/writer ordering, migration-code coupling).

## Pairing With Other Skills

- **review-code** first — assume correctness before deployability
- **review-compatibility** alongside **review-releng** — overlapping concerns (rollout staging, observability) but different focus. Compatibility is shape-of-contract-specific (data + interface); releng is system-wide operational posture.
- **review-legibility** independent

## Customizing for Your Stack

Replace placeholders with project-specific artifacts:

- **DB** → actual technology (Postgres, MySQL, DynamoDB, Spanner) — each has different DDL semantics
- **Migration tool** → actual tool (Flyway, Liquibase, Atlas, in-house)
- **Backfill** → actual job framework (Sidekiq, Cloud Run jobs, K8s CronJob)
- **Audit / comparison** → actual reconciliation tooling

A skill that says "verify dual-write" is generic; one that says "run `audit_dual_write.py` for 24 hours and confirm divergence count is zero" is operational.

## Prompt Template

Full prompt template in `PROMPT_TEMPLATE.md`.

## Reference

DDL safety patterns reflect Postgres community guidance on safe schema migrations (locking semantics, `CONCURRENTLY`, `NOT VALID`). Dual-write / strangler-fig migration patterns are standard in distributed systems literature; Martin Fowler's "evolutionary database design" is one canonical writeup.
