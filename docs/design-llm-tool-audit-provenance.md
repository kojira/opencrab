# Issue #967: LLMログのtool履歴表示

- 状態: 承認済み・実装中
- 設計版: v0.6（第三者レビュー反映・スコープ縮小版）
- 対象: 既存local tool/subtask履歴の表示、ChatGPT native web search履歴の保存・表示
- 非対象: secret検査、汎用監査基盤、cancel/timeout/restart state machine、Gemini、Anthropic native search、Issue #964

## 1. 問題

QCで次を確認した。

1. `execute_shell`の実引数は既にDBへ保存されているが、後続LLMリクエストでは
   `execute_shell(→log:113104)`と表示され、同じ画面から実行内容・subtask・結果を追えない。
2. ChatGPT native `web_search`のeventとcitationをparserが破棄するため、検索履歴がLLMログに残らない。

## 2. 方針

新しい汎用trace基盤は作らない。既存データを使って表示を直し、現在失われているChatGPT native
web search情報だけを追加保存する。

モデルへ渡す会話コンテキストの`→log:N`圧縮は変更しない。変更するのは監査用LLMログ画面だけ。

## 3. local tool / background subtask

local toolについて新しい実行state machineやtableを作らない。既存情報を利用する。

- `llm_logs.response/tool_calls`: tool名、call ID、実引数
- `memory_sessions`の`tool_call`: 完全なtool call
- `memory_sessions`の`tool_result`: call ID、tool名、spawn ack、subtask ID
- `memory_sessions`の`system/subtask_completed`: subtask ID、完了結果

追加endpointを作る。

```text
GET /api/agents/{agent_id}/llm-logs/{llm_log_id}/tool-history
```

endpointは対象LLM logのagent/session内だけを検索し、次を返す。

1. そのLLM response自身のtool calls
2. request本文中の`→log:N`が参照する元`memory_sessions` tool call
3. call IDに対応するtool result
4. spawn ackがある場合はsubtask IDに対応するcompletion

`tool_logs`にはcall ID / subtask IDが無いため結合しない。同じcommandの複数実行を時刻や文字列で
推測対応させない。call ID / subtask IDで結べる`llm_logs`と`memory_sessions`だけを表示する。
既存の古いrowも、保存済み情報の範囲で表示する。

## 4. ChatGPT native web search

ChatGPT Responses API parserで以下を収集する。

- `web_search_call`のID、status、action objectの許可field
- URL citationのURL、title
- web searchが要求されたが使用されなかった状態
- web search eventを観測したが解析できなかった状態

既存`ChatResponse`や通常`ToolCall`へ混ぜない。provider自身が実行済みのsearchをOpenCrabが
再実行しないためである。

`llm_logs`へJSON列を一つ追加する。

```text
provider_tool_history TEXT NOT NULL DEFAULT '{}'
```

保存形:

```json
{
  "state": "legacy_unknown | not_requested | not_used | captured | incomplete",
  "provider": "chatgpt",
  "calls": [],
  "citations": []
}
```

action typeごとに許可fieldを保持し、`search`系actionだけqueryを必須にする。queryを持たない
`open_page`等を解析失敗へ誤分類しないfixtureを置く。

raw SSEは保存しない。新規のChatGPT以外は`not_requested`とする。migration前の既存row `{}`は
`legacy_unknown`（旧ログのため判定不能）として表示し、`not_requested`へ推測変換しない。

## 5. 型と互換性

既存`ChatResponse`、`ToolCall`、`LlmCallLog`、`chat()`、既存callbackを変更しない。

付加的な`LlmExchange { response, provider_tool_history }`はcanonical leaf crate
`opencrab-llm-types`に一度だけ定義する。default実装付き`chat_with_history()`を
provider/router/coreへ追加し、既存provider/mockは従来の`chat()`だけで動く。

公開済み`ChatGptProvider::parse_response(...) -> Result<ChatResponse>`のsignatureとtest FQNは維持する。
内部へ`parse_exchange()`を追加し、既存`parse_response()`はその`.response`だけを返す。

serverは新callbackだけでLLM rowをINSERTする。既存callbackは外部利用者向けに従来どおり各call一回
発火させるが、serverでは同時登録せず二重INSERTしない。通常のtool dispatch順序、background化、
LLM直列性は変えない。

## 6. dashboard

LLMログ詳細へ「Tool history」を追加する。

- tool名
- 実引数
- call ID
- backgroundの場合はsubtask ID
- tool result / subtask completion
- ChatGPT native web search query/status/citation

`→log:N`はモデルへ実際に送られた文字列として残し、その直下に解決結果を表示する。
別のTool Logsページを探さなくても、ユーザーが提示した一連の履歴を同じ画面で確認できるようにする。

native search表示は以下を区別する。

- 旧ログのため判定不能
- 未要求
- 要求したが未使用
- 履歴取得済み
- 履歴解析失敗

## 7. migration

schema v48で`llm_logs.provider_tool_history`だけを追加する。

- 既存DB migration
- fresh DB schema
- `LlmLogRow` query/serialization
- 月次archive round-trip
- synthetic v47→v48 fixture
- 二度適用

既存rowは`{}`のまま保持し、UI/APIでは`legacy_unknown`と解釈する。存在しないnative履歴を
推測でbackfillしない。

## 8. テスト

1. 会話コンテキストの`→log:N`圧縮は不変。
2. `execute_shell`のtool名・完全な引数・call IDをtool-history APIで取得できる。
3. spawn ackのcall IDからsubtask ID、completionまで辿れる。
4. 複数tool/subtaskを別のcall IDへ誤結合しない。
5. 旧rowは保存済み情報だけを部分表示し、存在しない履歴を生成しない。
6. ChatGPT SSE fixtureから`web_search_call`とcitationを保存できる。
7. `legacy_unknown/not_requested/not_used/captured/incomplete`を区別する。
8. native search履歴を通常ToolCallとして再dispatchしない。
9. Claude通常`tool_use`は既存local tool履歴として表示できる。
10. v47→v48 migration、fresh schema、archive round-trip。
11. dashboardのloading/error/空/複数履歴表示。

## 9. QC

1. `execute_shell sleep`をbackground実行する。
2. 同じLLMログ画面でcommand/args、call ID、subtask ID、完了結果を確認する。
3. 実行中に天気要求を連続送信し、tool/subtask履歴が消えないことを確認する。
4. ChatGPT native web searchのaction/query/status/citationを確認する。
5. Discord応答、QC API/dashboard、core/Discord/Nostrの稼働を確認する。

## 10. 実装単位

1. ChatGPT native history parserとfixture
2. v48の1列migration
3. tool-history API
4. dashboard表示
5. affected suite、独立review、QC
