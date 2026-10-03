## Data access: queries and migrations

When the diff adds or changes a query, an index or a migration, review the access path as well as the operational path. Read the repo's schema and migration files; do not assume.

- **Each new or changed query.** Name the index that serves it, citing the migration or schema file that creates it. If none does, or the index cannot serve the predicate and ordering as written (wrong column order, a function applied to the column, a leading wildcard), say that it scans and estimate against the table's expected size.
- **Queries and indexes against each other.** Check whether two queries in the change, or a query and an index choice, work against one another: one orders or filters in a way the other's index cannot serve, an index added for one path that slows writes for another, a lookup repeated per row of another query.
- **DDL.** Find the online-safety rules the repo itself states for migrations, in its migration docs, a lint rule or an `AGENTS.md`, and cite them. Typically these require explicit `ALGORITHM` and `LOCK` clauses on `ALTER TABLE` and index creation, or a stated reason a statement is exempt. Check every changed statement against them, and report a missing clause as a finding that cites the rule.
- **Table size.** If the repo states how large the affected tables are or will be, use it to judge scan cost and lock time. If it does not, say the finding depends on size rather than assuming.
