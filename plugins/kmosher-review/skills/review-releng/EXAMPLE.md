# review-releng: worked example

Real case: PR adds a new endpoint to a SaaS backend that calls a third-party billing API to validate a customer's plan tier before serving a feature.

**Per-call review of revertability:** `git revert` would remove the endpoint. No data shape change. ✓ but: the endpoint path is referenced in client code shipped to browsers — clients with the path cached will 404 after revert. Finding: P2, propose 410 with "feature unavailable" instead of removing the route entirely until clients drain.

**Blast radius:** call to billing API is in the request hot path with no timeout. If billing is down, every request to this endpoint hangs until the upstream client disconnects. Finding: P1, propose 5s timeout + cached fallback to "unknown plan tier, allow with warning."

**Observability:** the PR adds the endpoint with no metrics. Existing service-level error rate would mask a 100% error rate on this single endpoint. Finding: P1, propose per-endpoint error rate + p99 latency before merge, since adding metrics during an incident is too late.

**Rollout:** PR proposes deploying directly to all production. Finding: P1, propose feature flag defaulting to off, gradual ramp, with explicit kill-switch documented in the runbook.

**Self-verification step caught:** an earlier draft of the report claimed the timeout was 30 seconds. Re-reading the code showed there was no timeout at all (the HTTP client used the Go default, which is unlimited). Reviewer corrected the finding from "raise the timeout to 5s" to "add a timeout, no current bound."

**Net result:** 4 findings (1× P2, 3× P1). PR author accepted the feature flag, added per-endpoint metrics, and added the timeout. Soaked behind flag at 1%/10%/50% over 3 days before 100%.
