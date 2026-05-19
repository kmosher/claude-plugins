# kmosher's Claude Code plugins

A small public marketplace of [Claude Code](https://docs.claude.com/en/docs/claude-code) plugins I use day-to-day. Currently ships:

- **`kmosher-review`** — a multi-lens code-review suite. The `/review` command routes a PR through four review lenses (correctness, legibility, compatibility, release-engineering) in their own subagents, runs project lint tools, and produces one prioritized report with GitHub permalinks.

## Install

```
/plugin marketplace add kmosher/claude-plugins
/plugin install kmosher-review@kmosher
```

Then in any repo with a current branch or PR:

```
/review              # auto-routes lenses based on what the diff touches
/review 1234         # review PR #1234
/review only legibility
/review local        # don't offer to post the report as a PR comment
```

## What's in `kmosher-review`

| Lens | When it runs | What it looks for |
|---|---|---|
| `review-code` | Always | Correctness bugs — logic errors, ignored errors, schema/shape mismatches, tests that pass with a buggy implementation |
| `review-legibility` | Always (last) | Readability — restated-code comments, misleading names, branch fanout, stale references, iteration-history scars |
| `review-compatibility` | If the diff crosses a deploy or caller boundary | DDL/protobuf/schema changes, exported signature changes, API/config/env-var changes |
| `review-releng` | If the diff touches a runtime service or deploy infra | Revertability, blast radius, observability gaps, rollout safety, performance |

Plus the `comment-virtuoso` agent, which `review-legibility` delegates to for non-trivial comment rewrites.

## How it differs from the built-in `/security-review`

`/security-review` ships with Claude Code and runs a security pass. `kmosher-review` is complementary: it doesn't do security review (run `/security-review` for that), but it surfaces correctness, readability, deploy-safety, and operational concerns that a security-only pass won't.

## Versioning

The marketplace pins to commits by default; bump `version` in `plugins/kmosher-review/.claude-plugin/plugin.json` for release notifications. Users on `/plugin update` get the new version.

## License

MIT — see [LICENSE](./LICENSE).
