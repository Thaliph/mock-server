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
| `rust-dev` | Sonnet | each Rust step, test-first |
| `ops` | Haiku | each CI / Dockerfile / Helm / kind step |
| `reviewer` | Sonnet | each PR, one pass |

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
1. Brief `rust-dev` (Rust) or `ops` (delivery chain): issue, branch, step, criterion IDs.
2. Check its report; run `cargo test --workspace` yourself only if the report is unclear.

One PR per feature by default (`gh pr create` from the feature branch, body: `Closes #<n>`,
criteria covered, how verified, `🤖 Generated with [Claude Code](https://claude.com/claude-code)`).
Split into stacked PRs only when the diff exceeds ~1500 lines.

## 4. Review and merge

1. Brief `reviewer` with the PR number.
2. Blocking findings → brief the author agent with only those findings; repeat review once.
   Disagreement on a product point → ask the user.
3. Merge when `gh pr checks <n> --watch` is green **and** the last `mimic-review` marker is
   `APPROVE`: `gh pr merge <n> --merge --delete-branch`. A failing e2e unrelated to the diff may
   be re-run once (`gh run rerun <id> --failed`).
4. Close the loop: tick the feature in the issue, update memory with the feature status, tell the
   user in a few lines (what works now, how to try it, what is next).

## Cost rules

- Read only the spec sections and files a step needs; prefer `gh … --json` with `-q`.
- No Opus agent for implementation or review; no second review pass for non-blocking findings.
- Public repo + GitHub-hosted runners + GHCR only (free tier).
