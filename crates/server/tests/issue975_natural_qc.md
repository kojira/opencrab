# Issue #975 natural-language QC

This is an opt-in live QC case. It is not part of ordinary CI.

## Preconditions

- Use the isolated QC runtime and Discord channel only.
- Set `conversation.typed_history = false`.
- Use `chatgpt:gpt-5.6-sol`; do not use OpenRouter.
- Do not add tool names, tool arguments, execution counts, expected output, or retry instructions beyond the natural request below.

## User message

Send exactly this natural user message:

> サブタスクに、17×23を計算して簡単な検算もするよう依頼し、完了したら結果を教えてください。

## Pass criteria

Inspect the persisted logs and the actual `ChatRequest.messages` recorded in `llm_logs.prompt`.

- Exactly one background subtask is dispatched for the calculation.
- The calculation reaches a persisted `subtask_completed` event.
- The first LLM request started after completion contains the saved calculation result.
- One final response reports `391` and reflects the subtask's verification.
- `CONTINUE` and `NO_REPLY` are not persisted or delivered as ordinary speech.
- There is no provider error.

Do not infer a pass from Discord output alone. A run that does not reach background completion is inconclusive.
