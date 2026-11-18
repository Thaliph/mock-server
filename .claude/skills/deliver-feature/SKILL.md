---
name: deliver-feature
description: Deliver one mimic feature (F1…F10 of docs/SPEC.md §12) end to end with the agent team - issue, architect criteria, test-first steps by rust-dev or ops, review, CI, merge. Use when the user asks to start, continue or deliver a feature, or invokes /deliver-feature <F#>.
---

# Deliver a feature

The main session runs this; sub-agents cannot spawn sub-agents. Agents start **without** this
conversation: each brief is a few lines with the issue number, the branch, the step and the
criterion IDs. Never paste the conversation into a brief.

| Agent | Model | Used for |
|---|---|---|
| `architect` | Opus | once per feature: criteria, design, threat model, steps |
| `spec-writer` | Opus | each step with Rust code: the step spec (contract, reuse, tests) |
| `rust-dev` | Sonnet | each Rust step, test-first, from the step spec |
| `ops` | Haiku | each CI / Dockerfile / Helm / kind step |
| `code-critic` | Opus | each PR, before the reviewer: duplication, layering, coherence |
| `reviewer` | Sonnet | each PR, one pass, plus delta re-reviews |
| `ci-triage` | Haiku | a failing CI run: diff, flake or infra |
| `security-auditor` | Opus | once per milestone, on `main`, after its last merge |

Do small mechanical things yourself instead of spawning an agent (a typo, a version bump, a
one-line fix asked by a review): a spawn costs more than the fix.

## 1. Issue

If the feature has no issue yet: `gh issue create --title "F<n>: <name>" --label feature
--milestone v1 --body …` with the goal from §12, the user plan points, the criterion prefix
(e.g. `INJ`) and the out-of-scope list. Create branch `f<n>/<slug>` from `main` and push it.

## 2. Design gate

Brief `architect`: issue, branch. When it returns, read its issue comment.
- Product questions → ask the user (AskUserQuestion), record answers on the issue, re-brief
  only if the answers change the criteria.
- Otherwise check the criteria are testable and the steps are PR-sized, then go on.

## 3. Steps

For each step, in order (steps that share no file may run in parallel):
1. Rust step: brief `spec-writer` (issue, branch, step). Read its step spec; an open question
   goes to the architect (criteria) or to the user (product). Skip it for a step under ~50 lines.
2. Brief `rust-dev` (Rust) or `ops` (delivery chain): issue, branch, its own worktree, step,
   criterion IDs, the Valkey URL. Parallel agents never share a worktree.
3. Check its report; run `cargo test --workspace` yourself only if the report is unclear.

One PR per feature by default (`gh pr create` from the feature branch, body: `Closes #<n>`,
criteria covered, how verified, `🤖 Generated with [Claude Code](https://claude.com/claude-code)`).
Split into stacked PRs only when the diff exceeds ~1500 lines.

## 4. Review and merge

1. Brief `code-critic` with the PR number. Its Must items go to the author agent before the
   review; Could items only if cheap.
2. Brief `reviewer` with the PR number.
3. Blocking findings → brief the author agent with only those findings; repeat review once.
   Disagreement on a product point → ask the user. After any new commit or a merge of `main`,
   brief the reviewer for a delta re-review: the approval must be newer than the head.
4. A red CI run → brief `ci-triage` with the run id instead of reading the logs yourself.
5. Merge when `gh pr checks <n> --watch` is green **and** the last `mimic-review` marker is
   `APPROVE`: `gh pr merge <n> --merge --delete-branch`. A failing e2e unrelated to the diff may
   be re-run once (`gh run rerun <id> --failed`).
6. Close the loop: tick the feature in the issue, update memory with the feature status, tell the
   user in a few lines (what works now, how to try it, what is next).

## 5. Milestone audit

When the last PR of a milestone merges, brief `security-auditor` with the milestone issues and the
commit range. Its `[high]` and `[medium]` findings are fixed before the next milestone starts;
`[low]` ones join the follow-ups.

## Tooling (kept here so a fresh machine or session needs nothing else)

- `ship.sh <pr>` (this directory) is the merge gate of step 4: it waits for `ci-ok`, checks the
  last `mimic-review` marker is `APPROVE` and newer than the head, then merges. Run it in the
  background. `IGNORE=<check name>` only when the maintainer decided a check does not block.
- Pre-push check of the assert messages: `scripts/check-test-messages.sh origin/main` (the lint
  job runs it too). It fails on an added `assert!`/`assert_eq!`/`prop_assert*!` line whose message
  interpolates a secret-like or raw value (`{raw}`, `{json}`, `{body}`, `{text}`, `{line}`,
  `{canary}`, `{token}`, `{jwt}`, `{secret}`, `{password}`); pre-existing hits on main are not
  reported. Its self-test: `bash scripts/test_check_test_messages.sh`.
- `test-valkey.sh` starts the local Valkey that `cargo test` needs and prints
  `export MIMIC_TEST_VALKEY_URL=…`; pass that URL in every agent brief.
- Local kind e2e (`scripts/e2e.sh`, `scripts/e2e-load.sh`):
  - one run at a time (fixed ports, shared kubeconfig), and one `docker build` at a time on the
    machine: the Dockerfile's cargo cache mount is shared. Agents never run either; the main
    session runs them serially. A build failing with "unresolved import" of a committed module
    means a polluted cache: `docker builder prune -f --filter type=exec.cachemount`.
  - after a machine reboot the kept `mimic-e2e` cluster is stale: `kind delete cluster --name
    mimic-e2e` before the first run (the script recreates it).
- Background waits: never `pgrep -f <cmd>` (it matches itself); chain the steps in one command
  and give long commands a `timeout`.
- GitHub outages (a `504` or a checksum failure while downloading a release in CI) are not the
  diff: wait, then re-run the failed jobs once.

## Cost rules

- Read only the spec sections and files a step needs; prefer `gh … --json` with `-q`.
- Opus only for the judgment roles (architect, spec-writer, code-critic, security-auditor); never
  for implementation or the correctness review. No second review pass for non-blocking findings.
- Benchmarks follow the `bench` skill; e2e the `kind-e2e` skill.
- Public repo + GitHub-hosted runners + GHCR only (free tier).
