# Delivery team (Claude Code)

mimic is built by a small team of Claude Code agents, run by a product manager. Each agent
starts with **only** a GitHub issue and a short brief, not the whole conversation, so it stays
focused on one simple task.

## How to use it

In Claude Code, at the root of the repository:

```
/product-manager <goal, or "the next P0 items of todo.md">
```

The product manager (the skill `.claude/skills/product-manager/`, run by the main session):

1. splits the goal into tickets the size of one pull request and asks you, the product owner,
   the product questions;
2. creates one GitHub issue per ticket (form `.github/ISSUE_TEMPLATE/ticket.yml`, labels
   `type:*`, `persona:*`, `status:*`);
3. runs each ticket through the personas below, on a branch `t/<issue>-<slug>`;
4. merges the pull request when CI is green and no reviewer asked for changes, then reports.

## Personas (`.claude/agents/`)

| Agent | Model | Role |
|-------|-------|------|
| `app-architect` | Opus | design options and recommendation with the product owner, ADRs in `docs/adr/` |
| `k8s-architect` | Opus | challenges CRDs, webhooks, RBAC, pod lifecycle, HA, upgrades, version support |
| `security-engineer` | Opus | threat model, security practices in `docs/security/`, security test cases, security review |
| `tester` | Sonnet | spec criteria (`docs/SPEC.md` §11.2) and failing tests named after them, before any code |
| `developer` | Opus | makes the tests pass, runs the checks, opens the pull request |
| `sre` | Sonnet | metrics, logs, alerts, runbook, sizing, FinOps, load-test plan, operability review |
| `devops` | Sonnet | workflows, images, release, pod configuration, delivery review |

## Order of a ticket

```
design gate (architects, security) → spec + red tests (tester) → implementation (developer)
→ reviews in parallel (security, devops, sre, k8s-architect, tester) → fixes → merge
```

Only the steps a ticket needs are run. Reviewers post one comment each on the pull request,
ending with a verdict block (`APPROVE` or `REQUEST_CHANGES`).

## Rules every agent follows

- The spec (`docs/SPEC.md`) comes first, then the failing tests, then the code (`CLAUDE.md`).
- Code, comments, spec and commit messages in French; consumer documentation in English.
- Never push to `main`; only the product manager merges, and only when CI is green.
- Product decisions belong to the product owner: agents report them, never guess.
