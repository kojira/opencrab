# AGENTS.md

## Repository workflow

All repository changes must follow this workflow unless the user explicitly instructs otherwise.

1. Create or identify a GitHub Issue before changing files.
   - Every fix, documentation change, CI repair, refactor, or behavior change needs an Issue.
   - If a problem is discovered during unrelated work, do not fix it opportunistically; create a separate Issue unless it blocks the requested work.

2. Work only on a branch for that Issue.
   - Do not commit directly to `main`.
   - Do not advance local `main` as a substitute for a GitHub PR merge.

3. Open a Pull Request for the branch.
   - The PR should reference the Issue.
   - Include the user-visible goal, scope, and validation evidence.

4. Enter review before merge.
   - Use third-party/fresh-context review where practical.
   - Treat review as conformance checking, not a source of new requirements.
   - Address only comments that are part of the approved scope, affect the main user-facing goal, or are non-edge-case correctness/safety issues.
   - For valuable but out-of-scope findings, create follow-up Issues instead of mixing them into the PR.

5. Repeat fix and review until no adopted review comments remain.
   - After changes, request or perform another review pass.
   - Do not merge while accepted review findings remain unresolved.

6. Merge through GitHub PR only after review and required validation are complete.
   - Prefer the repository's existing PR merge style.
   - Verify the PR is merged on GitHub and `origin/main` contains the merge commit.

## Reporting

When working through Discord or another user-visible channel, report important state changes in normal visible messages:

- Issue created
- Branch created
- PR opened
- Review outcome
- PR merged
- CI or deployment failures

Do not rely on internal/hidden completion summaries for user-facing status.
