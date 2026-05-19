# review-compatibility: worked example (data-shape variant)

Real case: PR adds a `region` column to `customers` table to support geo-routing. Migration is a single SQL file:

```sql
ALTER TABLE customers ADD COLUMN region VARCHAR(32) NOT NULL DEFAULT 'us-east-1';
CREATE INDEX customers_region_idx ON customers(region);
```

Backend code reads `region` and routes requests accordingly. PR is a single commit that includes both the migration and the code.

**Compatibility check:** old code (running during the deploy window) doesn't know about `region`. New code reads it and routes on it. Old code writing rows during deploy → new column gets `'us-east-1'` default. Forward-compat OK. But: new code reading rows written by old code (before this migration runs) → column doesn't exist yet → query errors. **Migration must run before new code ships.** Finding: P1, propose splitting into two PRs: (1) migration, (2) code that uses the column, gated by feature flag.

**DDL check:** `ALTER TABLE ... ADD COLUMN ... NOT NULL DEFAULT 'us-east-1'` on Postgres pre-11 rewrites the entire table under a lock. Postgres 11+ optimizes this for non-volatile defaults, but the table size matters. Finding: P0 if the customers table is large (>1M rows) and Postgres is pre-11. **Required action:** add column nullable, backfill in batches, then add NOT NULL with `NOT VALID`/`VALIDATE`.

**Index check:** `CREATE INDEX` (without `CONCURRENTLY`) blocks writes to the table for the duration of the index build. On a 50M-row table, that's an outage window. Finding: P0, **required action:** use `CREATE INDEX CONCURRENTLY` and verify the index builds successfully (it can fail and leave a partial index).

**Reader/writer ordering check:** PR couples migration and code. If the code deploys before the migration finishes, new code reads `region` from a column that doesn't exist yet → 500s. Finding: P1, **required action:** ship migration alone, soak, verify column populated, then ship code that reads it.

**Self-verification step caught:** an earlier draft claimed Postgres rewrites the table for ANY `ADD COLUMN ... NOT NULL DEFAULT`. Re-reading Postgres docs showed pg11+ optimizes non-volatile defaults to skip the rewrite. Reviewer corrected the finding to be conditional on PG version.

**Net result:** 4 findings (2× P0, 2× P1). PR was split into three: (1) add column nullable + backfill job; (2) verify backfill complete + add NOT NULL with `NOT VALID`/`VALIDATE`; (3) deploy code that reads `region`, gated behind feature flag. `CREATE INDEX CONCURRENTLY` used throughout. Total deploy time: 2 days. Zero downtime.
