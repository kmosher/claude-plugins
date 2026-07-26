---
name: review-releng
description: This skill should be used when reviewing a PR that touches production services or anything that could page someone — runtime services, deployment manifests, CI/release configs, auth/secrets handling, Pulumi/IaC, feature flags, runtime config, observability code, or anything on a per-request hot path. Reviews for revertability, blast radius, observability gaps, rollout safety, and performance regressions. Assumes correctness; asks whether the change can be operated.
---

# Operational Readiness Review

Review through a release-engineering lens: can this be deployed safely, observed in production, and reverted in seconds when it's wrong? Assumes correctness; asks whether it can be operated.

## Shared conventions (read first)

Read `../SHARED_CONVENTIONS.md` before applying this lens — covers REVIEW.md overlay, pattern propagation, and the findings buffer. Operational gaps (missing telemetry, no kill switch, no rollback path) usually repeat across sibling code paths in the diff; pattern propagation is the rule that catches them.

## When to Use

**Use for:** any PR touching production services; schema migrations or config changes to load-bearing systems; changes to systems with SLOs/SLAs/compliance; anything that could page someone at 3am; final pre-merge gate after correctness is settled.

**Don't use for:** pure CLI/library changes with no runtime deploy surface; documentation; test-only PRs; local-dev tools.

If unsure: ask "if this is wrong in subtle ways, how does the org find out, and how fast can we undo it?" Weak answers → this skill applies.

## Tooling available

Operational readiness is judgment-heavy, but mechanical checks back it up:

- **CI lint/build green** — verify `make lint`, the project's typecheck, or
  the language LSP MCP (`mcp__gopls__go_diagnostics`, etc.) are clean on the
  changed code. A failing lint pre-deploy is a paging risk.
- **IaC preview** (`pulumi preview --json`, `terraform plan -json`, `cdk diff`)
  — for stack-config or infra changes, run the preview and `jq`/grep for the
  changed resources. Always preview before approving releng-sensitive changes.
- **Generated runtime artifacts** — for changes to Dockerfiles, deployment
  manifests, or systemd units, check the build target produces the expected
  artifact (image tag, helm chart version, etc.).

Full per-language and per-tool recipes in `review-automated-checks.md`.

## Prioritization Hierarchy

When constraints conflict, this is the tiebreaker. **Higher beats lower, every time.**

1. **Revertability** — Can we get back to prior state in minutes if wrong?
2. **Incremental rollout** — Can we limit blast radius if wrong but unnoticed?
3. **Observability** — Can we tell when it's wrong, before users report it?
4. **Performance stability** — Will this regress p50/p99 or resource use under realistic load?
5. **Speed of delivery** — Can we ship faster?

Speed never wins over the other three.

## Core Principles

- **Revert before debug.** Investigation while users are affected is a luxury you can't afford. Every change must be revertable cheaply enough that "revert first" is realistic.
- **Reversible decisions are free; irreversible ones aren't.** Data migrations, schema changes, side-effecting configs need more scrutiny than code-path swaps.
- **Production ≠ staging.** Staging traffic is an order of magnitude smaller, more tolerant of breakage, missing the long-tail edge cases. Staging success is a hypothesis, not a conclusion.
- **Blast radius is what's visible to a partial failure.** Not "what does this code touch?" but "what does this code's failure mode let users see?"

## Deployment Patterns

| Pattern | When | Required infra |
|---|---|---|
| Push to staging → soak → production | Default for service code | Working push-status tooling; representative staging |
| Feature flag + dark launch | Toggleable behavior changes, new code paths | Flag system; flag defaults to off / old behavior |
| Canary deploy | Infra changes, perf-sensitive, non-obvious failure modes | Traffic-split capability; ability to drain rapidly |
| Two-phase deploy (write-then-read) | Schema migrations, format changes, reader/writer agreement | Coordinated rollout: write new format → backfill → read new format |
| Shadow / dual-write | Migration between systems (DB, cache, queue) | Capacity for parallel run; comparison/audit tooling |
| Big-bang | Almost never; justify explicitly | Pair with kill switch |

Prefer the smallest blast radius that meets requirements. **A feature flag is almost always better than its absence** for non-trivial behavior changes.

## What to Flag in PR Review

### Revertability
- `git revert <SHA>` sufficient? If not, what else is needed?
- Data shape changes: old code reads new data? new code reads old data? *Both*.
- Public API removal/rename: deprecation period?
- Config-derived runtime value: defaults to old behavior on partial rollout?

### Blast radius
- Worst case if this returns wrong values for 5%/50%/100% of requests?
- Failure mode: graceful degradation or hard-fail?
- Circuit breaker / kill switch without redeploy?
- New external dependency: failure mode if it's down?

### Observability
- First signal of misbehavior: user complaint or metric?
- Metrics exist *before* this PR ships, or PR adds both?
- Errors logged at a level that's actually tailed?
- Latency-sensitive: histogram, not just count?
- Cost-sensitive: counter for runaway loops?

### Rollout
- Soaked in staging for one full traffic cycle?
- Flag rollout schedule documented (1% → 10% → 50% → 100%)?
- Multi-service: deploy ordering documented?
- Explicit rollback procedure, confirmed working?

### Performance regression
Performance regressions are operational events — they page, they cause SLO burns, they're often the first visible symptom of a bad deploy. Worth a dedicated pass.

- **Hot-path allocation** — new code in a per-request path that allocates per call (slice growth, map creation, JSON marshal/unmarshal, string concatenation in a loop). What was zero-alloc, is it still?
- **Algorithmic complexity change** — was an O(n) loop replaced with something O(n²) or O(n log n) under load? Look for nested loops over the same collection, repeated lookups instead of pre-indexed maps, recursive walks over growing structures.
- **N+1 queries / RPCs** — single-row reads inside a loop where a batched read would do; per-item external calls instead of bulk. Especially: ORM lazy-loads, GraphQL nested field resolvers, repeated calls into other services.
- **Missing pagination / cursor** — query that returns "all rows" without `LIMIT` or a cursor; full-table-scan with `ORDER BY` on an unindexed column. Works on dev, takes the prod DB out.
- **Unindexed query patterns** — new query against a column that has no index; new `WHERE` clause that the existing index doesn't cover. EXPLAIN it.
- **Synchronous work on an async hot path** — blocking I/O inside an event loop / async handler (`fetch` without `await` properly handled, `time.Sleep` in a request handler, file I/O on the request thread).
- **Cache-key shape change** — every existing cached entry becomes a cache miss on deploy. Hot path takes a thundering-herd hit until the cache warms.
- **New external dependency on the hot path** — adds the dependency's p99 latency to your own; if it has none of its own SLOs, you've imported its outage modes.
- **Resource exhaustion potential** — unbounded queues, unbounded goroutine/thread spawn, unbounded retry loops, no rate limit on a new fanout, no timeout on an expensive op.

Output for perf findings: cite the specific line + the cost model (per-call alloc count; loop iteration count under expected load; query plan; p50/p99 estimate). Vague "this might be slow" is not actionable; concrete "this allocates 3 strings per request × 50k req/s = 150k alloc/s on the hot path" is.

## Common Misconceptions

| Misconception | Reality |
|---|---|
| "Tests pass, safe to deploy" | Tests cover what someone thought of. Production has the rest. Necessary, not sufficient. |
| "It worked in staging" | Staging is qualitatively different. Confidence bounded by representativeness. |
| "It's a small change" | Blast radius set by what code touches, not lines changed. One-line config changes take down services. |
| "We can always revert" | Only if revert was designed in. Schema migrations, irreversible state changes, "convenient" backfills make revert impossible. |
| "Error rate is fine" | Aggregate hides per-customer hot spots. 0.1% global = 100% for one customer. |
| "We'll add metrics if we need them" | Adding metrics during an incident is too late. The metric you need is the one you didn't think to add. |
| "Feature flags are overhead" | A flag adds ~2 hours of work; turns a 4-hour incident into a 30-second toggle. |
| "Automated deploy is safe" | Automation is rapid execution, not safe execution. Bad changes deploy faster too. |

## Anti-Pattern Checklist

- [ ] Schema change without a migration plan
- [ ] "Just deploy and watch" — no observability, rollback, or staging plan
- [ ] New behavior to 100% on first deploy with no flag/canary
- [ ] Cross-service change with no documented deploy ordering
- [ ] Config change with side effects on first read (revert doesn't undo)
- [ ] New external dep with no timeout / retry budget
- [ ] Hardcoded values that should be config (you'll need to change them at 3am)
- [ ] Logs include user-controlled input without sanitization (log injection → page)
- [ ] Background job with no idempotency
- [ ] CHANGELOG missing for user-visible change
- [ ] Tests that mock the failure mode they're testing

## Output Format

Return findings as JSONL using the canonical schema in `../SHARED_CONVENTIONS.md` §3. Rendering to markdown is the invoker's job.

**Lens-specific fields:**
- `check` — the named operational check that fired (e.g. `no-revert-path`, `missing-kill-switch`, `no-canary`, `unbounded-blast-radius`, `missing-observability`, `perf-regression-risk`). Lets the coordinator group findings by class.
- `deployment_pattern` (P0/P1 only) — the rollout pattern that mitigates the risk (feature flag, canary, two-phase, shadow, etc.).
- `observability_signal` (P0/P1 only) — the metric/log/alert that would catch the regression in production.
- `rollback_procedure` (P0/P1 only) — exact steps to get back to prior state.

**`category` values for this lens:** `revertability` (post-deploy revert path), `blast-radius` (graceful degradation, kill switch), `observability` (telemetry, logging, alerting), `rollout-safety` (canary, feature flag, two-phase), `performance` (p50/p99 risk, resource-use regression).

If the PR passes the checklist, return an empty JSONL block with a meta note. Don't manufacture.

## Worked Example

See `EXAMPLE.md` in this skill's directory — a PR adding a new endpoint that hit a third-party billing API. Surfaced 1× P2 (revertability with cached clients) and 3× P1 (missing timeout, missing metrics, no rollout staging).

## Pairing With Other Skills

- **First**, run **review-code** — assume correctness before reviewing for ops
- **review-legibility** runs independently
- **review-compatibility** runs alongside if data shapes or interface contracts change

## Customizing for Your Stack

Replace generic placeholders with project-specific artifacts:

- "Push to staging → production" → your actual deploy mechanism (`pulumi up` against a stack, GH Actions deploy job, ArgoCD sync)
- "Feature flag system" → name it (GrowthBook, LaunchDarkly, in-house)
- "On-call" → reference the actual rotation (`pd-oncall <schedule>`, PagerDuty service)
- "Metrics" → reference the actual telemetry stack
- "Staging traffic cycle" → reference your actual SLOs

A skill that says "deploy to staging" is generic; one that says "run `gh-push-status`, watch the relevant Grafana board for 30 minutes" is operational.

## Prompt Template

Full prompt template in `PROMPT_TEMPLATE.md`.

## Reference

Structure adapted from Sarah Maeve's releng-skill template — particularly the prioritization hierarchy, misconceptions, and anti-pattern checklist patterns. Operational discipline reflects SRE literature (Beyer et al., *Site Reliability Engineering*) and current practice.
