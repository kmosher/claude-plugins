# Operational Readiness Review Prompt Template

Copy-paste and fill in the bracketed sections.

---

# Operational readiness review of <PR/branch>

You are reviewing through a release-engineering lens. Assume the code is correct. Find what makes it unsafe to deploy, hard to observe, or impossible to revert.

## What to review

Repo: <path>
Branch: <branch> (latest commit <SHA>)
Files: <list>

## Required reading first

[Pre-load deploy/runbook context: the service's deploy mechanism (e.g. `gh-push-status` workflow, Pulumi stack, ArgoCD app), its SLOs, the on-call rotation that owns it, the relevant Grafana/Datadog dashboards, and any prior incidents in this surface area.]

## Apply this checklist

For each section, list every failure with file:line or PR-level concern.

### Revertability checks
- [ ] Is `git revert <SHA>` sufficient to restore prior behavior? If not, what additional steps?
- [ ] If data shape changes, can old code read new data AND new code read old data? Both directions.
- [ ] If a public API surface is removed/renamed, is there a deprecation period?
- [ ] If a config-derived runtime value changes, does it default to old behavior so partial rollout doesn't split-brain?

### Blast radius checks
- [ ] What's the worst case if this returns wrong values for 5%/50%/100% of requests?
- [ ] Does the failure mode degrade gracefully or hard-fail?
- [ ] Is there a circuit breaker / kill switch without a redeploy?
- [ ] New external dependency? What's the failure mode if it's down?

### Observability checks
- [ ] Will the *first signal* of misbehavior be a user complaint or a metric/alert?
- [ ] If metrics: do they exist *before this PR ships* or is the PR adding both?
- [ ] Are error paths logged at a level actually shipped (not just `glog.V(9)` nobody tails)?
- [ ] Latency-sensitive path: is there a histogram, not just count + error rate?
- [ ] Cost-sensitive path: is there a counter to detect runaway loops?

### Rollout checks
- [ ] Has it soaked in staging for at least one full traffic cycle?
- [ ] Config / feature-flag rollout schedule documented (1% → 10% → 50% → 100% with gates)?
- [ ] Multi-service: deploy ordering documented?
- [ ] Explicit rollback procedure, confirmed working?

### Self-verification (before submitting)
For each finding, re-read the cited code/config and confirm the claim. State which findings you verified by inspecting the actual artifact (deploy config, observability code, etc.) and which are inferred from naming or structure.

## Output format

Numbered findings. Each:
- **Severity**: P0 (will cause incident on deploy), P1 (significant operational risk, must address before merge), P2 (defer or document, not a blocker), P3 (preference)
- **File:line ref or PR-level concern**
- **Which check / anti-pattern flagged it**
- **What's wrong** (concrete)
- **Required action** for P0/P1; **suggested action** for P2/P3

For each P0/P1 finding, also propose:
- The **deployment pattern** that would mitigate (flag, canary, two-phase)
- The **observability** that would detect a regression
- The **rollback procedure** if it ships and is wrong

If the PR genuinely passes the checklist, say so. Don't manufacture findings.
