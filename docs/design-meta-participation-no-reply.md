# Meta-level conversation participation

## Problem and evidence

Issue #991 concerns an agent speaking merely because a shared conversation produced a new inbound message. Receiving a message starts a turn, but it does not by itself create an obligation to reply.

The earlier status-category policy was rejected because it classified surface forms instead of asking the agent whether it should participate. Two softer English formulations of the meta-level rule were also rejected: both produced a visible reply in 2/2 checks against the production-derived failure request.

The exact participation block in this design was then added to the same production-derived request without other changes. Condition Y produced exactly `NO_REPLY` in 2/2 checks. Condition Z changed only the latest input to a direct request and produced a normal answer in 2/2 checks. This establishes the prompt-level behavior without claiming an internal model mechanism.

## Contract

Before composing a response in a multi-agent conversation, the agent decides whether this is a situation where it should speak. Being addressed directly is one consideration in that decision, not the whole mechanism. A shared-channel message being delivered to the agent is not itself a request for the agent to reply.

The shared system prompt contains this validated block immediately after the conversation-history guidance and before `## Turn completion`:

> ### 返事すべき場面の判断
> 複数のエージェントがいる場合は「返事すべき場面かどうか」を先に判断する。
> - 自分に直接話しかけられている → 返事する
> - 他のエージェント同士の会話 → 基本的に黙っておく（NO_REPLY）
> - 話が完結している → 黙っておく

This is a meta-level participation decision, not a taxonomy of acknowledgements, status messages, questions, or other surface forms. When the decision is not to speak, the existing `NO_REPLY` contract ends the turn without delivered speech.

## Preserved behavior and non-goals

- Existing `NO_REPLY`, asynchronous tool, background completion, and turn-continuation behavior is unchanged.
- No routing, queue, state, database, engine heuristic, or output filter is added.
- No configuration switch or history-format change is added.
- No transport-specific or sender-type branch is added.
- No claim is made about the model's internal mechanism.

## Acceptance

1. The validated block appears exactly once after conversation-history guidance and before `## Turn completion`.
2. The rejected status-category paragraph is absent.
3. The decision remains model-owned, shared, and transport-neutral.
4. Focused prompt, no-forced-reply, and transport-neutral tests pass without changing existing completion behavior.
