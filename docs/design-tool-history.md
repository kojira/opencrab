# 設計書: tool_call履歴管理

## 概要

LLMがtextとtool_callを同時に返した際に「調べてみる。」がDiscordに重複して届く問題について、tool_call IDとtool_resultのmessages配列への記録方法・DBへの永続化・非同期実行時のID対応付けを設計する。

---

## 1. 現状確認: skill_engine.rsのtool_call処理

### 1-1. messages配列へのtool_call/tool_result追加

`crates/core/src/engine/skill_engine.rs` の `run_with_model_override()` 内:

```rust
// tool_callsがある場合、assistantメッセージをhistoryに追加
messages.push(ChatMessage {
    role: "assistant".to_string(),
    content: response.content.clone().unwrap_or_default(), // "調べてみる。" （問題箇所）
    tool_calls: response.tool_calls.clone(),               // [spawn_subtask(id="toolu_01XYZ")]
    ..
});

// 各tool_callを実行してtool resultをhistoryに追加
for tool_call in &response.tool_calls {
    let result = self.executor.execute(&tool_call.name, &tool_call.arguments).await;
    messages.push(ChatMessage {
        role: "tool".to_string(),
        content: result_json,
        tool_call_id: Some(tool_call.id.clone()), // "toolu_01XYZ" ← 対応付け
        ..
    });
}
```

**messages配列内でのtool_call IDマッチング（同期ツールの場合）**:

```
messages[n]   = {role:"assistant", content:"調べてみる。", tool_calls:[{id:"toolu_01XYZ", name:"search", ...}]}
messages[n+1] = {role:"tool", tool_call_id:"toolu_01XYZ", content:"{\"results\":[...]}"}
```

→ `tool_call.id` と `tool_call_id` が一致している。AnthropicのAPIが要求する形式を満たしている。

### 1-2. messages配列のスコープ（重要）

**この`messages`配列は1回のエンジン実行内でのみ存在する。**

- `run_with_model_override()` のローカル変数
- エンジン実行が完了するとメモリから消える
- DBには **最終テキスト応答のみ** が `session_logs` に記録される（tool_call/tool_result構造体は保存されない）

```
DBに記録されるもの:
  session_logs: [{role:"speech", content:"調べてみる。〇〇でした。"}]

DBに記録されないもの:
  - tool_call_id
  - tool_result (JSON)
  - tool_calls配列構造
```

### 1-3. on_first_responseのDB未記録

`message_loop.rs` の `on_first_response` コールバック:

```rust
first_sent_for_cb.store(true, Ordering::SeqCst);
tokio::spawn(async move {
    gateway_for_cb.send_to_channel(channel_id, &text).await // Discordに送信
    // ← DBへのsession_log追加は行わない
});
```

**on_first_responseが発火してDiscordに「調べてみる。」を送っても、DBのsession_logsには記録されない。**

---

## 2. 問題特定: tool_call IDとhistoryの不整合

### 2-1. 同期ツール（search等）の場合

**messages配列内の対応**: ✅ 正しくマッチしている

```
iteration 1: LLM → "調べてみる。" + tool_calls:[{id:"toolu_01XYZ", name:"search"}]
              on_first_response("調べてみる。") → Discord #1, DB ✗
              messages[] に assistant + tool_result 追加
iteration 2: LLM → "〇〇でした。" （"調べてみる。"を再度含む場合も）
              handle_agent_response: first_sent=true → should_send=false
              DB ← "〇〇でした。" 記録
```

同期ツールでは、iteration 2でAnthropicが「調べてみる。」を再度含む応答を生成するのが直接的な重複原因（「中断された思考の補完」動作 - 詳細は`design-text-repeat-bug.md`参照）。

ただしP0修正（`should_send = !first_sent`）により、iteration 2の最終応答はDiscordに送信されない。

**実際の重複パターン**:
- `on_first_response` → 「調べてみる。」
- `handle_agent_response` の最終応答 → 「調べてみる。〇〇でした。」 ← `should_send=false`で抑制

P0修正後は単純な同期ツールでは重複は発生しない。

### 2-2. 非同期ツール（spawn_subtask）の場合 ← **問題の核心**

`spawn_subtask` はタスクを `tokio::spawn` で起動し、**即座に** `{"status":"spawned","subtask_id":"<uuid>"}` を返す。

**フロー**:

```
iteration 1: LLM → "調べてみる。" + tool_calls:[{id:"toolu_MAIN", name:"spawn_subtask"}]
             on_first_response("調べてみる。") → Discord #1, DB ✗
             messages[] に assistant追加

iteration 2: tool_result = {"status":"spawned","subtask_id":"sub-abc123"}
             messages[] に tool_result追加
             LLM → 空応答 or "OK、サブタスクが実行中です" or NO_REPLY
```

**iteration 2で空応答/NO_REPLYが返ったケース**（最も問題が起きやすい）:

```
handle_agent_response:
  engine_result.response = "" or NO_REPLY
  → Ok(_) => debug!("empty") 分岐
  → DBにsession_log追加されない
  → "調べてみる。" のDBレコードが存在しないまま
```

**後続処理（サブタスク完了後）**:

```
tokio::spawn内のサブタスク完了
  → completion_registry.get(parent_session_id)
  → cb(subtask_id, result, "completed")
  → LoopEvent::SubtaskCompleted → process_subtask_completed()

process_subtask_completed():
  conversation_raw = build_conversation_string(session_id, ...)
  → DBから取得: [user: "教えて"]のみ ← エージェント応答がDBに存在しない!
  
  system_prompt = "... [subtask_completed: subtask_id=sub-abc123, task=..., exit_reason=completed]"
  
  run_agent_response(system_prompt, conversation_raw, ...)
  → LLM: 会話履歴にエージェント発言がなく、subtask_completedが通知された
  → LLM: "調べてみる。〇〇でした。" と生成 ← "調べてみる。" が再出現
  → Discord #2 送信 ← 重複！
```

### 2-3. 問題の構造図

```
[正常フロー（DBあり）]
user: "教えて" → agent: "調べてみる。"(DB有) → subtask_completed
                                              ↓
                               LLM: "調べてみる。"知ってる → 重複しない

[現状のバグ（DBなし）]
user: "教えて" → on_first_response → Discord #1
                → DB未記録
                → iteration2: 空応答 → DB未記録
                   → subtask_completed
                      ↓
         LLM: 会話に"調べてみる。"がない → Discord #2 ← 重複！
```

### 2-4. Anthropic Messages APIの制約（根本原因）

Anthropic Messages APIには以下の重要な制約がある：

- **assistantメッセージに`tool_use`ブロックが含まれる場合、必ず後続のuserメッセージに対応する`tool_result`ブロックが含まれなければならない**
- この制約に違反するとAPIエラーになるか、Anthropicが文脈を無視した応答を返す

`process_subtask_completed`で再構築した会話には`tool_use`/`tool_result`が含まれないため：

```
[process_subtask_completedが再構築する会話]
user: "教えて"
agent: "調べてみる。"  ← tool_useブロックなし（テキストのみ）
[subtask_completed通知]

↓ この状態でAnthropicにリクエスト

AnthropicはAPIの制約として前のtool_callの文脈を一切持たない状態でレスポンスを生成してしまう
→ "調べてみる。" という発話の背景（何のtool_callをしたのか）が失われる
→ LLMが「前のターンで何かアクションを起こした」という認識を持てない
→ 結果として「調べてみる。〇〇でした。」と再度まとめて生成してしまう
```

**根本的な問題**: on_first_responseテキストのDB未記録（方針1で解決）だけでなく、
`process_subtask_completed`時に`tool_use`/`tool_result`の完全な履歴が復元されないため、
Anthropicが正しい文脈でレスポンスを生成できていない。

**真の解決策（P1）**: tool_use/tool_resultをDBに保存し、`process_subtask_completed`時に
完全なtool_use→tool_resultの会話履歴を復元してAnthropicに渡す。

---

## 3. 非同期実行時のtool_call IDとサブセッションIDの対応

### 3-1. 現在の対応付け方式

`spawn_subtask` が返すデータ:

```json
{
  "status": "spawned",
  "subtask_id": "f4a2b91c-...",        // 内部管理用UUID
  "session_id": "subtask-f4a2b91c-...", // DBのセッションID
  "spawned_at": "2024-..."
}
```

Anthropicが発行したtool_call ID（例: `toolu_01XYZ`）と、内部の`subtask_id`の対応は:

```
messages配列（skill_engine.rs内のローカル変数）:
  tool_call_id: "toolu_01XYZ" ↔ content: {"status":"spawned","subtask_id":"f4a2b91c-..."}

DBのparent_session_id session_logs:
  type:"subtask_spawned", subtask_id:"f4a2b91c-..."
  type:"subtask_completed", subtask_id:"f4a2b91c-..."
```

**Anthropicのtool_call ID（`toolu_01XYZ`）は現在どこにも永続化されていない。**

### 3-2. 現状の問題点

```
Anthropicのtool_call ID: "toolu_01XYZ"  ← DBに保存されない
内部のsubtask_id: "f4a2b91c-..."         ← DBに保存される
```

両者の対応が取れていないため:

1. `process_subtask_completed` がtool_call IDを知る手段がない
2. 新しいエンジン実行でAnthropicのcontextにtool_call/tool_result履歴を再現できない
3. `process_subtask_completed` は完全に新しいエンジン実行として処理される（意図的設計）

→ 現在の非同期設計では、tool_call IDの永続化は必要とされていない（再接続しない設計）が、
　 エージェントの発話（"調べてみる。"）のDB未記録が問題を引き起こしている。

---

## 4. 修正方針

### 方針1（推奨・優先度高）: on_first_responseテキストをDBに記録する

**変更箇所**: `crates/discord/src/message_loop.rs`

```rust
let on_first_response: Option<Box<dyn FnOnce(String) + Send>> = {
    let state_db = state.db().clone();
    let session_id_for_db = session_id.clone();  // ← 追加
    let agent_id_for_db = agent_id.clone();       // ← 追加
    let channel_id_str_for_db = channel_id_str.clone(); // ← 追加

    Some(Box::new(move |text: String| {
        if text.is_empty() || text.trim() == "NO_REPLY" {
            return;
        }
        
        // ← 追加: DBにエージェント発話を記録
        if let Ok(conn) = state_db.lock() {
            let log = opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: agent_id_for_db.clone(),
                session_id: session_id_for_db.clone(),
                log_type: "speech".to_string(),
                content: text.clone(),
                speaker_id: Some(agent_id_for_db.clone()),
                turn_number: None,
                metadata_json: Some(serde_json::json!({
                    "source": "discord_response",
                    "channel_id": channel_id_str_for_db,
                    "via_on_first_response": true,
                }).to_string()),
            };
            opencrab_db::queries::insert_session_log(&conn, &log).ok();
        }
        
        // ... 既存のDiscord送信処理 ...
    }))
};
```

**効果**:
- on_first_response発火時に "調べてみる。" がDBに記録される
- process_subtask_completedのbuild_conversation_stringが [agent: "調べてみる。"] を含む
- LLMが「既に言った」と認識して重複しない

**注意点**:
- handle_agent_response の最終DB記録と二重記録になる可能性がある
- 対策: handle_agent_response側で `via_on_first_response=true` のレコードが既にある場合はスキップ
- OR: 最終EngineResultが空の場合のみon_first_responseテキストをDB記録する

### 方針2（推奨・優先度中）: assistantメッセージのcontentを空にする（tool_callsある場合）

**変更箇所**: `crates/core/src/engine/skill_engine.rs`

```rust
// 現状:
messages.push(ChatMessage {
    role: "assistant".to_string(),
    content: response.content.clone().unwrap_or_default(), // "調べてみる。"
    tool_calls: response.tool_calls.clone(),
    ..
});

// 修正後:
let history_content = if response.tool_calls.is_empty() {
    response.content.clone().unwrap_or_default()
} else {
    String::new() // tool_callsがある場合はcontentを空にする
};
messages.push(ChatMessage {
    role: "assistant".to_string(),
    content: history_content,
    tool_calls: response.tool_calls.clone(),
    ..
});
```

**効果**:
- AnthropicのContextに "調べてみる。" が含まれなくなる
- iteration 2でAnthropicが「中断された思考の補完」をしなくなる
- 最終応答が「調べてみる。」なしになる

**トレードオフ**:
- Anthropicのcontextから"調べてみる。"という発話記録が消える
- エージェントの「考え中」発言が後のターンで参照できなくなる

### 方針3（P1・根本解決策）: tool_use/tool_resultをDBに保存してprocess_subtask_completed時に復元する

> **⚠️ P1優先方針**: 2-3で判明したAPI制約により、これが根本的な解決策として優先される。
> 方針1・2はworkaroundとして有効だが、tool_use/tool_result履歴の完全な復元なしには
> 非同期ツール使用時の文脈喪失問題は本質的に解決されない。

**優先実装事項**

```
DBに追加:
  subtask_tool_calls テーブル:
    session_id, tool_call_id (Anthropic ID), subtask_id, input_json

process_subtask_completedで:
  1. subtask_tool_callsからtool_call_idを取得
  2. messages配列を再構築:
     [{role:"assistant", tool_calls:[{id:tool_call_id,...}]},
      {role:"tool", tool_call_id:tool_call_id, content:subtask_result}]
  3. これを会話historyに組み込んでLLM呼び出し
```

**効果**: Anthropicに完全なtool_use/tool_result文脈を渡せる  
**コスト**: DBスキーマ変更、process_subtask_completedの大幅改修が必要

---

## 5. 推奨修正の組み合わせ

| 修正 | 効果 | 難易度 |
|------|------|--------|
| 方針1: on_first_responseテキストをDB記録 | process_subtask_completedでの重複防止 | 低 |
| 方針2: tool_calls時にassistant contentを空に | Anthropic側での重複テキスト生成を防止 | 低 |
| 方針3（P1）: tool_use/tool_resultをDB記録・復元 | 非同期ツールでの完全な文脈復元（根本解決） | 高 |

**両方を適用するのが最善**:
- 方針2: 直接的な重複テキスト生成を防ぐ（同期ツールの場合も安全）
- 方針1: 方針2でcontent=""になっても、DBの会話履歴は正しく保たれる

---

## 6. 期待動作（修正後）

### 6-1. 同期ツール（search等）の場合

```
iteration 1: LLM → "調べてみる。" + tool_calls:[search]
             on_first_response("調べてみる。") → Discord + DB記録 ✓
             messages[] ← {role:assistant, content:"", tool_calls:[...]}  ← 方針2
             
iteration 2: tool_result追加
             LLM → "〇〇でした。" （"調べてみる。"含まず ← 方針2の効果）
             handle_agent_response: first_sent=true → should_send=false → 送信なし
             DB ← "〇〇でした。"

最終Discordメッセージ: 「調べてみる。」のみ（1回）
```

### 6-2. 非同期ツール（spawn_subtask）の場合

```
iteration 1: LLM → "調べてみる。" + tool_calls:[spawn_subtask]
             on_first_response("調べてみる。") → Discord + DB記録 ✓  ← 方針1
             messages[] ← {role:assistant, content:"", tool_calls:[...]}  ← 方針2

iteration 2: tool_result = {"status":"spawned","subtask_id":"f4a2b..."}
             LLM → "" or NO_REPLY
             handle_agent_response: 空応答 → DBへ追記なし（方針1で既に記録済みなのでOK）

サブタスク完了時:
  build_conversation_string → DB:
    [user: "教えて"]
    [agent: "調べてみる。"] ← 方針1で記録済み！

  LLM: "既に「調べてみる。」と言っている → 重複しない"
  LLM: "〇〇の結果です。" → Discord

最終Discordメッセージ: 「調べてみる。」+ 後で「〇〇の結果です。」（重複なし）
```

### 6-3. なぜLLMが「調べてみる。」を繰り返さなくなるか

`process_subtask_completed` のシステムプロンプトには以下が含まれる:

```
## Async Behavior
Do NOT repeat what you already said in the previous turn.
Do NOT re-explain what you're about to do if you already said it.
Just act on the result.

Before responding after [subtask_completed: ...]:
1. Check your last message in the conversation history
2. If your last message already covers the same result → NO_REPLY
3. Only respond if you have genuinely NEW information to report
```

修正前: DBの会話履歴に「調べてみる。」がない → LLMは「前のターンで言っていない」と判断し繰り返す  
修正後: DBの会話履歴に「[agent]: 調べてみる。」がある → LLMは「既に言った」と認識し繰り返さない

---

## 7. 関連ファイル・設計書

| ファイル | 役割 |
|---------|------|
| `crates/core/src/engine/skill_engine.rs` | **修正対象**: tool_call時のassistant content処理（方針2） |
| `crates/discord/src/message_loop.rs` | **修正対象**: on_first_responseのDB記録（方針1） |
| `crates/discord/src/gateway_actions/subtask_engine.rs` | spawn_subtask実装（参照） |
| `crates/server/src/process.rs` | run_agent_response / build_conversation_string |
| `docs/design-text-repeat-bug.md` | 同問題の別アングル分析（Anthropicの中断思考補完動作） |
| `docs/design-message-loop-v3.md` | P0修正（should_send=!first_sent）の設計 |
| `docs/design-async-instructions.md` | 非同期エンジン設計全般 |

---

## 8. まとめ

| 確認項目 | 状態 |
|---------|------|
| skill_engine.rs内のtool_call ID ↔ tool_result マッチング | ✅ 正常（同一実行内で正しくマッチ） |
| DBへのtool_call構造体の永続化 | ❌ 未実装（最終テキストのみ） |
| on_first_responseテキストのDB記録 | ❌ 未実装（これが主要な問題） |
| Anthropicの「中断された思考の補完」による重複 | ❌ 未修正（方針2で解決可能） |
| spawn_subtask完了時のtool_call ID再現 | ❌ 未実装（方針3で将来対応） |

**最優先修正（P0）**: 方針1（on_first_responseのDB記録）+ 方針2（tool_calls時のcontent空化）を組み合わせて適用。
**P1（根本解決）**: 方針3（tool_use/tool_resultをDBに保存・process_subtask_completed時に復元）を優先実装。
