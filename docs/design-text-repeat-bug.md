# text重複バグ 設計書

## バージョン
- 初版: 2026-03-24
- 対象リポジトリ: opencrab / hermit-shell

---

## 1. 問題の概要

LLMがtextとtool_callを同時に返した時に、「調べてみる。\n調べてみる。」のように
同じテキストがDiscordに2回届く問題が発生している。

### 観察された症状

```
[Discord]
のすたろう: 調べてみる。
のすたろう: 調べてみる。

詳細な検索結果はこちら...
```

---

## 2. アーキテクチャ概要

```
Discord → message_loop.rs
            ↓
         run_agent_response()
            ↓
         SkillEngine::run()  ← in-memory messages管理
            ↓
         LlmRouterAdapter::chat()
            ↓
         LlmRouter → AnthropicProvider (直接) or OpenAiProvider → hermit-shell → Anthropic
```

### tool_call IDの流れ（正常系）

```
Anthropic API
  response.content[1].type = "tool_use"
  response.content[1].id   = "toolu_01XXXXXXXXXX"
        ↓ AnthropicProvider.parse_response() or hermit-shell convertResponseWithTools()
ChatResponseSimple.tool_calls[0].id = "toulu_01XXXXXXXXXX"
        ↓ skill_engine: messages.push(assistant message)
ChatMessage { role: "assistant", tool_calls: [{id: "toolu_01XXXXXXXXXX", ...}] }
        ↓ skill_engine: tool実行後 messages.push(tool result)
ChatMessage { role: "tool", tool_call_id: Some("toolu_01XXXXXXXXXX"), ... }
        ↓ 次のLLMコール: to_llm_message() → build_request_body()
Anthropic API
  messages[N].content[0].type          = "tool_use"
  messages[N].content[0].id            = "toulu_01XXXXXXXXXX"  ← ✅ 一致
  messages[N+1].content[0].type        = "tool_result"
  messages[N+1].content[0].tool_use_id = "toulu_01XXXXXXXXXX"  ← ✅ 一致
```

---

## 3. 根本原因の特定

### 3.1 調査：同期ツール内ループ（skill_engine内）

**結論: IDs・テキスト保持ともに正しい。バグなし。**

`skill_engine.rs` のtool_callループでは in-memory `messages` を使用する。

```rust
// skill_engine.rs: tool_callがある場合
messages.push(ChatMessage {
    role: "assistant".to_string(),
    content: response.content.clone().unwrap_or_default(),  // "調べてみる。" ✅保持
    tool_calls: response.tool_calls.clone(),                 // IDsを含む
    ...
});

for tool_call in &response.tool_calls {
    // ツール実行
    messages.push(ChatMessage {
        role: "tool".to_string(),
        content: result_json,
        tool_call_id: Some(tool_call.id.clone()),  // ✅ assistantのIDと一致
        ...
    });
}
```

### 3.2 調査：hermit-shell tool変換（tool_convert.ts）

**結論: OpenAI→Anthropic変換は正しい。バグなし。**

```typescript
// tool_convert.ts: openaiMessagesToAnthropic()
if (msg.role === "assistant") {
  if (msg.tool_calls && msg.tool_calls.length > 0) {
    const content: Array<unknown> = [];

    if (msg.content) {                          // "調べてみる。" → truthy ✅
      content.push({ type: "text", text: msg.content });  // テキスト保持 ✅
    }

    for (const tc of msg.tool_calls) {
      content.push({
        type: "tool_use",
        id: tc.id,          // "toulu_01XXXXXXXXXX" ✅
        name: tc.function.name,
        input: JSON.parse(tc.function.arguments),
      });
    }
    result.push({ role: "assistant", content });
  }
}

// tool result変換
} else if (msg.role === "tool") {
  result.push({
    role: "user",
    content: [{
      type: "tool_result",
      tool_use_id: msg.tool_call_id ?? "",  // "toulu_01XXXXXXXXXX" ✅ 一致
      content: msg.content ?? "",
    }]
  });
}
```

### 3.3 調査：AnthropicProvider直接呼び出しパス

**結論: build_request_body()は正しい。バグなし。**

```rust
// anthropic.rs: build_request_body() - Role::Assistant
if has_tool_calls {
    let mut content_blocks: Vec<Value> = Vec::new();

    if let Some(text) = msg.text_content() {
        if !text.is_empty() {
            content_blocks.push(json!({"type": "text", "text": text}));  // ✅
        }
    }
    for tc in msg.tool_calls.as_ref().unwrap() {
        content_blocks.push(json!({
            "type": "tool_use",
            "id": tc.id,          // ✅ IDは正しく保持
            "name": tc.function.name,
            "input": input,
        }));
    }
}

// Role::Tool
Role::Tool => {
    let tool_call_id = msg.tool_call_id.as_deref().unwrap_or("");
    messages.push(json!({
        "role": "user",
        "content": [{
            "type": "tool_result",
            "tool_use_id": tool_call_id,  // ✅ 一致
            "content": text,
        }]
    }));
}
```

### 3.4 【根本原因】非同期ツール（spawn_subtask）とDBの欠落

**これが根本原因。**

#### 問題の構造

```
skill_engineのin-memoryメッセージ（DB未保存）:
  messages[0]: system
  messages[1]: user
  messages[2]: assistant { content: "調べてみる。", tool_calls: [{id: "toulu_XYZ", name: "spawn_subtask"}] }
  messages[3]: tool { tool_call_id: "toulu_XYZ", content: {status: "spawned", subtask_id: "sub-123"} }
  messages[4]: assistant { content: "サブタスクを起動しました。" }
                                  ↑
                          ⚠️ DBに保存されるのはこれだけ（最終レスポンス）
```

```
DB session_logの実態:
  [speech] user: "○○を調べて"
  [speech] agent: "サブタスクを起動しました。"  ← tool_call/tool_result の記録なし
```

#### process_subtask_completed での再構築

```rust
// message_loop.rs: process_subtask_completed
let conversation_raw = state.build_conversation_string(&session_id, &agent_id, ...);
// ↑ DBからのみ再構築。tool_use + tool_result は含まれない

state.run_agent_response(
    ...
    &system_prompt,
    &conversation,  // tool_callコンテキスト欠落
    ...
    None,           // on_first_response = None（first_sentチェックなし）
).await;
```

#### Anthropicが受け取るコンテキスト（2回目のrun_agent_response）

```json
{
  "messages": [
    {"role": "user", "content": "○○を調べて"},
    {"role": "assistant", "content": "サブタスクを起動しました。"},
    {"role": "user", "content": "[subtask_completed: subtask_id=sub-123, ...]\n\n○○を調べて"}
  ]
}
```

**Anthropicはtool_useとtool_resultの対応が存在しないため、前回のコンテキスト（"調べてみる。"を既に言ったこと）を把握できない。**

#### テキスト重複の発生メカニズム

```
① on_first_response発火（skill_engine iteration=1）
   → Discordへ "調べてみる。" を送信
   → first_sent = true

② spawn_subtaskツール実行（同期完了: {status: "spawned"}）

③ skill_engine iteration=2: LLMが返答
   → EngineResult { response: "サブタスクを起動しました。" }
   → handle_agent_response: first_sent=true → should_send=false（Discord送信せず）
   → ただしDBには保存される

④ バックグラウンドでサブタスク実行

⑤ サブタスク完了 → process_subtask_completed()
   → DBから会話再構築（tool_callコンテキスト欠落）
   → 新しいSkillEngine.run()でAnthropicを呼ぶ
   → Anthropicが前回の "調べてみる。" を知らないまま生成:
     "調べてみる。\n\n調べた結果は以下の通りです: ..."
   → on_first_response=None → Discord送信に直行（抑制なし）

⑥ Discordの表示:
   "調べてみる。"                          ← ①の送信
   "調べてみる。\n\n調べた結果は..."       ← ⑤の送信
```

---

## 4. Anthropic APIの制約

Anthropic Messages APIには以下の制約がある:

> assistantメッセージに`tool_use`ブロックが含まれる場合、**必ず**後続のuserメッセージに対応する`tool_result`ブロックが含まれなければならない。

process_subtask_completedで再構築した会話には`tool_use`/`tool_result`が含まれないため、
Anthropicは前のtool_callの文脈を一切持たない状態でレスポンスを生成する。

---

## 5. 修正方針

### 方針A: DBにtool_call交換を保存する（推奨）

skill_engineのtool_callループ内で、each turn の tool_call/tool_result を DB session_log に保存する。

**変更箇所: `message_loop.rs` または `skill_engine.rs`**

skill_engine にlog callbackを追加し、assistant + tool_result メッセージをDBに保存:

```rust
// skill_engine.rs または llm_adapter.rs の log_callback を活用
// 既存のlog_callback（LlmCallLog）を拡張する、または
// 新しい on_tool_call コールバックを追加する

pub on_tool_call: Option<Box<dyn Fn(&ToolCallRecord) + Send + Sync>>,
// ToolCallRecord = { assistant_msg_with_tool_call, tool_result }
```

DBに保存するログタイプ:
```
log_type: "tool_call"
content: {
  "tool_call_id": "toulu_XYZ",
  "tool_name": "spawn_subtask",
  "tool_arguments": {...},
  "tool_result": {...}
}
```

build_conversation_string() でtool_call logsを読んで会話を再構築する際、
Anthropicフォーマット（tool_use + tool_result）に変換して含める。

### 方針B: process_subtask_completedに前回のtool_callコンテキストを渡す（局所的修正）

spawn_subtask実行時に tool_call_id と関連コンテキストを保存し、
subtask完了時に `system_prompt` の `[subtask_completed]` ブロックに含める。

```rust
// subtask_engine.rs の execute_spawn_subtask() で保存
let tool_call_context = json!({
    "tool_call_id": args["__tool_call_id"].as_str().unwrap_or(""),
    "assistant_text": args["__assistant_text"].as_str().unwrap_or(""),
    "tool_name": "spawn_subtask",
    "tool_args": task_description,
});
// DBに保存（subtask_spawned ログに含める）

// process_subtask_completed() で参照
// system_prompt に挿入:
// "[Context] 以前あなたはこう言いました: '調べてみる。' そしてspawn_subtaskを実行しました。
//  その結果が届きました。重複して述べないでください。"
```

### 方針C: process_subtask_completedのon_first_response制御（現状バグの軽減）

process_subtask_completedでも `first_sent` パターンを使い、
既に送信済みのテキストを検出して抑制する。

ただしこれは根本解決でなく、テキスト重複を防ぐだけで
LLMが不完全なコンテキストで動作する問題は残る。

---

## 6. 推奨修正（優先度順）

| 優先度 | 方針 | 実装コスト | 根本解決度 |
|--------|------|-----------|------------|
| P0 | 方針B（コンテキスト注入） | 低 | 中（spawn_subtask限定） |
| P1 | 方針A（DBにtool_call保存） | 高 | 高（全ツール対応） |
| P2 | 方針C（重複抑制） | 低 | 低（症状緩和のみ） |

### P0修正の具体的な変更箇所

**opencrab側 (`subtask_engine.rs`)**

`execute_spawn_subtask()` で `__tool_call_id` と `__assistant_text` を受け取り、
`subtask_spawned` ログに保存する。

**opencrab側 (`message_loop.rs`)**

`process_subtask_completed()` で subtask_spawnedログを参照し、
`system_prompt` に以下を追加:

```
[Subtask Context]
あなたは以前のターンで「{assistant_text}」と述べ、{tool_name}ツールを実行しました。
そのサブタスク（subtask_id={subtask_id}）が完了しました。
前のターンで述べたことを繰り返さないでください。
```

### P1修正の具体的な変更箇所

**opencrab側 (`skill_engine.rs` または `llm_adapter.rs`)**

```rust
// 新しいコールバック
pub on_tool_exchange: Option<Box<dyn Fn(&AssistantMsg, &[ToolResultMsg]) + Send + Sync>>,
```

**opencrab側 (`message_loop.rs`)**

skill_engine作成時に `on_tool_exchange` コールバックを設定し、
DBの session_log に `log_type: "tool_exchange"` として保存。

**opencrab側 (`build_conversation_string()` の実装箇所)**

`tool_exchange` ログを読み出し、Anthropicの `tool_use`/`tool_result` ブロックとして
会話に組み込む。

---

## 7. 修正後の期待動作

修正後、以下のフローになる:

```
① on_first_response: "調べてみる。" → Discord送信

② spawn_subtask実行 → DBにtool_call_id + assistant_text保存

③ バックグラウンドでサブタスク実行

④ サブタスク完了 → process_subtask_completed()
   → DBからsubtask_spawned取得
   → system_promptに「前のターンで"調べてみる。"と述べた。繰り返し不要」を注入
   → Anthropicが生成:
     "検索結果が届きました。○○についての詳細は以下の通りです: ..."
     ← "調べてみる。" の繰り返しなし

⑤ Discordの表示:
   "調べてみる。"                           ← ①の送信
   "検索結果が届きました。○○の詳細: ..."   ← ④の送信（重複なし）
```

---

## 8. 補足: 同期ツール（spawn_subtask以外）の挙動

fetch_web、list_tools等の同期ツールでは、skill_engineの内部ループで完結するため
process_subtask_completedは呼ばれない。

この場合:
- tool_call ID・テキスト保持: **正しい**（hermit-shell/AnthropicProviderどちらも）
- on_first_response発火後、最終レスポンスは `first_sent=true` で抑制される
- **副作用**: ツール結果を含む最終レスポンスがDiscordに届かない（別バグ）

この副作用の修正は本バグの対象外だが、P1修正（tool_exchange DBログ）と
連動してフォローアップが必要。

---

## 9. 調査コード証拠まとめ

| ファイル | 箇所 | 問題 |
|--------|------|------|
| `message_loop.rs` L330付近 | `handle_agent_response`: DBにengine_result.responseのみ保存 | tool_call交換がDB未保存 |
| `message_loop.rs` L400付近 | `process_subtask_completed`: `build_conversation_string`でDB再構築 | tool_callコンテキスト欠落 |
| `message_loop.rs` L406付近 | `run_agent_response(..., None)` | on_first_response=Noneで重複抑制なし |
| `subtask_engine.rs` L40付近 | `execute_spawn_subtask`: tool_call_idを保存しない | コンテキスト復元不可 |
| `tool_convert.ts` L130付近 | `openaiMessagesToAnthropic`: ID変換 | バグなし（正しく変換） |
| `anthropic.rs` L90付近 | `build_request_body`: Role::Assistant | バグなし（正しく変換） |
