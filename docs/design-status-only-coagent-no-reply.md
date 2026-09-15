# Status-only co-agent no-reply policy

## Problem and evidence

Issue #991 concerns a semantic reply loop where an agent answers a message that only acknowledges receipt, confirms a test, or announces that the sender will stop. Delta reduction reproduced the behavior twice with only an identity system message and one plain user-role status message. OpenCrab runtime, database state, history wrappers, speaker headers, tool schemas, and the production prompt were not required.

This establishes an input/output boundary, not an internal model mechanism: there is no evidence that OpenCrab duplicated history or roles, and this design does not label the behavior as imitation.

A candidate content policy was then checked twice per condition against both the minimal request and a production-derived request. Status-only inputs produced exactly `NO_REPLY` in all checks, while substantive questions received answers in all checks.

## Contract

The shared system prompt adds this paragraph immediately after the conversation-history guidance and before `## Turn completion`:

> A message that only acknowledges receipt, confirms a test result, expresses agreement, or announces that it will stop adds no new information and needs no response. In that case, respond with exactly NO_REPLY. Respond normally when the message contains a question, request, correction, new evidence, or unresolved work.

The decision remains with the model and is based on message content. Questions, requests, corrections, new evidence, and unresolved work remain reply-worthy. `NO_REPLY` retains its existing meaning as an explicit turn termination that is not delivered as speech.

## Non-goals

- No routing, queue, state, database, or engine heuristic changes.
- No output filtering or semantic classification outside the model.
- No new marker, configuration switch, or history format.
- No transport-specific or sender-type branch.
- No claim about the model's internal mechanism.

## Acceptance

1. The paragraph appears exactly once at the production prompt seam, after conversation-history guidance and before `## Turn completion`.
2. The paragraph is shared and transport-neutral.
3. Focused prompt tests pass and existing turn-completion wording remains unchanged.
4. The already-established live checks remain the behavioral evidence: status-only inputs terminate with `NO_REPLY`, while substantive questions still receive answers.
