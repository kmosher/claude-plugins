# Migration & Data-Compat Review Prompt Template

Copy-paste and fill in the bracketed sections.

---

# Migration / data-compat review of <PR/branch>

You are reviewing for **deploy-time data shape safety**. Assume the code is correct in isolation. Find what breaks when readers and writers don't update atomically.

## What to review

Repo: <path>
Branch: <branch> (latest commit <SHA>)
Files: <list>
Change category: [DDL / message format / stored state / API contract / cross-system migration]

## Required reading first

[Pre-load: the migration tooling docs (e.g. Postgres MVCC and lock semantics, Flyway/Atlas docs); the deploy ordering convention for your services; any existing dual-write or backfill patterns in this codebase; the current schema/format spec.]

## Apply this checklist

### Compatibility checks
- During rollout window, will old code read new data? new code read old data? *Both* must work.
- If old code can't read new data: deploy ordering coordinated and documented?
- If new code can't read old data: backfill step before new code reads, idempotent and resumable?
- Migration itself reversible? If not, called out and pre-deploy verification longer?

### DDL / schema checks
- DDL safe on populated table? (NOT NULL on populated → unsafe; ALTER COLUMN TYPE → often locking)
- `CONCURRENTLY` / non-locking variants used?
- `IF NOT EXISTS` / `IF EXISTS` for idempotency?
- Lock duration estimate (quantified, not "shouldn't be long")?
- Transaction boundary: full migration in one txn, or each step independently safe?

### Backfill checks
- Batched (not single UPDATE over whole table)?
- Each batch commits?
- Resumable from interruption?
- Progress tracking (cursor or count)?
- Throttling so backfill doesn't saturate DB?
- Idempotent — running twice produces same result?

### Reader/writer ordering
- Deploy ordering specified explicitly?
- New readers depend on new data shape: writers + backfill ship first?
- Old readers must drain: documented drain period?

### Validation checks
- Way to verify migration succeeded that doesn't depend on user reports?
- Irreversible migration: dry-run mode?
- Metric/alert for unexpected post-migration data shape?

### Self-verification (before submitting)
For each finding, re-read the migration SQL, the code that writes data, and the code that reads data. Confirm the cited risk is real. State which findings you verified by inspecting actual SQL/code versus inferred from structure.

## Output format

Numbered findings. Each:
- **Severity**: P0 (will lock prod / lose data), P1 (compatibility hazard, must fix before merge), P2 (improvement, defer), P3 (preference)
- **File:line or PR-level**
- **Which check / anti-pattern flagged it**
- **What's wrong** (concrete — name the locked table, the unbatched UPDATE, the missing dual-write)
- **Required action** for P0/P1; **suggested action** for P2/P3

For each P0/P1 finding, also propose:
- The migration **pattern** that addresses it (multi-phase, dual-write)
- The **deploy ordering** required (which PR ships first, what soak time)
- The **rollback procedure** if the migration goes wrong

For migration-specific findings, prefer concrete language:
❌ "Consider safety of this migration"
✓ "`ALTER TABLE foo ADD CONSTRAINT bar NOT NULL` on a 50M-row table will hold an exclusive lock for the table-scan duration. Use `ADD CONSTRAINT bar NOT NULL NOT VALID` followed by `VALIDATE CONSTRAINT bar` (non-blocking) instead."

The reviewer should be able to write the fix, not just identify the problem.
