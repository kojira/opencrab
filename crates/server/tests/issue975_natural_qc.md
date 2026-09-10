# Issue #975 natural-language QC

This is an opt-in live QC case. It is not part of ordinary CI.

## Preconditions

- Use the isolated QC runtime and Discord channel only.
- Set `conversation.typed_history = false`.
- Use `chatgpt:gpt-5.6-sol`; do not use OpenRouter.
- `curl` must already be in the agent's allowed-command list.
- Do not add tool arguments, execution counts, expected output, or retry instructions to the user message.

## User message

Send exactly this natural user message:

> 途中経過は不要です。curlで https://example.com を取得して、ページタイトルを教えてください。

## Pass criteria

Inspect the persisted logs and the actual `ChatRequest.messages` recorded in `llm_logs.prompt`.

- The URL retrieval reaches a persisted `subtask_completed` event.
- No equivalent retrieval of the same URL is dispatched more than once.
- The first LLM request started after completion contains the saved result, including `Example Domain`.
- One final response reports the retrieved page title.
- `CONTINUE` and `NO_REPLY` are not persisted or delivered as ordinary speech.
- There is no provider error.

Do not infer a pass from Discord output alone. A run that does not reach background completion is inconclusive.
