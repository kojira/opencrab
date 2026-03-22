# プロンプトキャッシュ最適構成 設計書 v7

> 作成日: 2026-03-23（v6を改訂）
> 対象バージョン: opencrab（Claude Sonnet 4.6使用）
> 参照: `/Volumes/2TB/openclaw/workspace/data/anthropic-cache-spec.md`
> **前提**: hermit-shellの `cache_control` パススルーは実装済み (commit bc34123)（ただし後述の修正が必要）

---

## 1. 現状の問題点

### 1-1. 全会話履歴を1つの `user` メッセージに詰め込んでいる

`build_conversation_string()` (`crates/server/src/process.rs`) がセッションログを
すべて `[speaker]: content` 形式の文字列に連結し、単一の `user` ロールメッセージとして LLM に渡している。

```
エンジンが送るリクエスト（現状）:
messages:
  {role: "system", content: "You are ...（固定）...\n## Runtime\nCurrent date: 2026-03-23..."}
  {role: "user",   content: "[kojira]: こんにちは\n[agent]: やあ！\n[kojira]: 今日は何の日？\n...（全履歴）"}
```

この構造では **Automatic Caching が効くのは tools と system のみ**。
会話履歴は毎回 `user` メッセージの中身が変わるため、**会話ターンをまたいだキャッシュは一切されない**。

### 1-2. systemプロンプトに変動値が含まれている

`build_agent_context()` が生成する systemプロンプトの末尾:

```
...（固定コンテンツ）...

## Runtime
Current date and time: 2026-03-23 05:28:00 +09:00  ← 毎リクエストで変化
Current discussion topic: direct_message           ← セッションによって変化
```

system が変わると `tools → system → messages` の **全キャッシュが無効化** される。
`cache_control` を付与してもここを直さない限り効果ゼロ。

### 1-3. `cache_control` フィールドが未実装

`ChatMessage` / `ChatRequestSimple` / `ToolDefinition` のいずれにも
`cache_control` フィールドがなく、現状ではキャッシュが**一切有効になっていない**。

コードの流れ:
```
SkillEngine (engine.rs)
  → ChatRequestSimple (cache_controlフィールドなし)
  → LlmRouterAdapter (llm_adapter.rs: to_llm_request)
  → OpenAiProvider (providers/openai.rs: build_request_body)
  → hermit-shell :8765/v1  ← cache_control パススルー済み（ただし要修正 → 5-7参照）
  → Anthropic API          ← でも渡す cache_control がない
```

### 1-4. CallerIdentity問題（既存設計からの継続）

`BridgedExecutor.list_tools()` が `CallerIdentity` に基づきツールをフィルタリングするため、
ツール定義がユーザー種別で異なる。ツール定義が変わると以降の全キャッシュが無効化される。

**解決策: ハッシュの自然分離（追加実装不要）**
Anthropicキャッシュはコンテンツハッシュで一意化されるため、
CallerIdentity別にツール定義が異なっても自動的に別キャッシュエントリになる。

---

## 2. 新アーキテクチャ: マルチターン形式

### 2-1. 全体像

```
┌─────────────────────────────────────────────────────────────────┐
│ tools（CallerIdentity別・安定）                                   │
│   ...                                                           │
│   最後のツール: cache_control(ephemeral, ttl: "1h")  ← BP1       │
├─────────────────────────────────────────────────────────────────┤
│ system（変動値削除後は完全固定）                                   │
│   "You are {name} ({persona}).\n..."                            │
│   cache_control(ephemeral, ttl: "1h")  ← BP2                   │
├─────────────────────────────────────────────────────────────────┤
│ messages:                                                       │
│   {role: "user",      content: "こんにちは"}                     │
│   {role: "assistant", content: "やあ！"}                         │
│   {role: "user",      content: "今日は何の日？"}                  │
│   {role: "assistant", content: "..."}                           │
│   ...（過去の会話 → Automatic Cachingで5分TTLキャッシュ）          │
│   {role: "user",      content:                                  │
│     "[Context]\nCurrent date and time: 05:51\n\n最新のメッセージ"} │
│   ← 変動値付き最後のuserメッセージ（キャッシュ対象外）               │
└─────────────────────────────────────────────────────────────────┘
```

### 2-2. ロール割り当てルール（重要）

| 発言者 | ロール | 理由 |
|--------|--------|------|
| エージェント自身（自分） | `assistant` | LLMの自己発言 |
| 人間ユーザー | `user` | 標準のユーザーターン |
| **他のBot（らぼみbot等）** | **`user`** | **`assistant`ロールにしない！** |

**他Botを `assistant` にしてはいけない理由:**
Anthropicの仕様では `assistant` ロールは「自分自身の過去の発言」を表す。
他のBotの発言を `assistant` に入れると、LLMが「自分がそれを言った」と誤解する。
他Botは `user` ロールで、発言者名をコンテンツ内に含める（例: `[らぼみbot]: ...`）。

**実装上の判定方法:**
`Sender.is_bot` フィールド（`crates/gateway/src/message.rs`）が `true` の場合でも、
発言者が「自エージェント (`agent_id`)」でない限り `user` ロールを使用する。

```
is_own_agent = (speaker_id == agent_id)
role = if is_own_agent { "assistant" } else { "user" }
```

### 2-3. 変動値の配置

**変更前（問題あり）:**
```
system末尾:
  ## Runtime
  Current date and time: 2026-03-23 05:28:00 +09:00  ← キャッシュを毎回無効化
  Current discussion topic: direct_message
```

**変更後（正解）:**
- **systemプロンプトから完全に削除**（完全固定化）
- 各過去メッセージの先頭に **発言時刻（`created_at`）** を付与（会話の間隔を伝える）
- **最後の `user` メッセージの先頭**に `[Context]` セクションとして **現在時刻（処理タイミング）** を付与

**2種類の時刻の使い分け:**

| 時刻 | 場所 | 意味 |
|------|------|------|
| 各メッセージの `created_at` | 全過去メッセージの先頭 | 発言した時刻。会話の間隔・文脈を伝える（「3時間前の話」等） |
| 現在時刻（処理時刻） | 最後のuserメッセージの`[Context]` | LLMが処理している時刻。発言処理の遅延も検知できる |

```
過去メッセージの例:
  [2026-03-23 02:30:15] [kojira (390732846236434452)]: こんにちは
  [2026-03-23 02:31:07] [agent]: やあ！
  [2026-03-23 05:51:42] [kojira (390732846236434452)]: 久しぶりだけど...  ← 3時間の間隔が分かる

最後のuserメッセージ:
  [Context]
  Current date and time: 2026-03-23 05:52:10 +09:00  ← 処理時刻（発言から1分遅延等が分かる）
  Current discussion topic: Discord conversation

  （最新のユーザー発言テキスト）
```

### 2-4. キャッシュ構成（最終）

| BP | 位置 | TTL | 効果 |
|----|------|-----|------|
| BP1 | toolsの最後のツール | 1h | CallerIdentity別に自然分離。長時間会話もヒット |
| BP2 | systemの末尾ブロック | 1h | 人格/スキル/instructionsをキャッシュ。5分超の会話もヒット |
| Automatic | messagesの最後のuserより前まで | 5m | 会話履歴全体をキャッシュ（マルチターン形式で初めて有効） |

**TTL混在ルール準拠:** 長いTTL → 短いTTL の順序
`tools(1h) → system(1h) → messages(5m)` ✓

---

## 3. 影響を受けるファイルと変更内容

### 3-1. `crates/core/src/engine.rs`

**変更1: `CacheControl` 構造体を追加**
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheControl {
    pub r#type: String,       // "ephemeral"
    pub ttl: Option<String>,  // None = 5分, Some("1h") = 1時間
}
```

**変更2: `ChatMessage` に `cache_control` フィールドを追加**
```rust
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content_parts: Vec<ChatContentPart>,
    pub cache_control: Option<CacheControl>,  // ← 追加
}
```

**変更3: `ToolDefinition` に `cache_control` フィールドを追加**
```rust
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub cache_control: Option<CacheControl>,  // ← 追加
}
```

**変更4: `run_with_model_override()` でBP1/BP2を設定**

現在の実装（`messages` 構築部分）:
```rust
let mut messages = vec![
    ChatMessage { role: "system".to_string(), content: system_context.to_string(), ... },
    ChatMessage { role: "user".to_string(), content: user_message.to_string(), ... },
];
```

変更後（Phase 2: マルチターン形式対応）:
```rust
// tools の最後のツールに BP1(1h) を付与（list_tools()返却後に設定）
// system に BP2(1h) を付与
// messages はマルチターン形式（後述の Phase 2 実装）
```

---

### 3-2. `crates/server/src/process.rs`

**変更1: `build_agent_context()` からRuntimeセクションを削除**

```rust
// 変更前（末尾）:
let prompt = format!(
    "...{skills_text}{character_section}{instructions_section}\n\
     \n\
     ## Runtime\n\
     Current date and time: {now}\n\
     Current discussion topic: {session_theme}"
);

// 変更後:
let prompt = format!(
    "...{skills_text}{character_section}{instructions_section}"
    // ← ## Runtime セクションを完全に削除
);
```

**変更2: `prepend_runtime_context()` ヘルパーを追加**
```rust
/// 変動コンテキストを最後のuserメッセージに前置するヘルパー
pub fn prepend_runtime_context(
    user_message: &str,
    session_theme: &str,
) -> String {
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %Z");
    format!(
        "[Context]\nCurrent date and time: {now}\nCurrent discussion topic: {session_theme}\n\n{user_message}"
    )
}
```

**変更3: `build_conversation_messages()` 関数を追加（Phase 2）**

マルチターン形式でメッセージリストを返す新関数（Phase 2実装）:
```rust
/// セッションログからマルチターン形式のChatMessageリストを構築する。
///
/// - エージェント自身の発言 → role: "assistant"
/// - それ以外（人間/他Bot）→ role: "user"
/// - 最後のメッセージには変動コンテキスト（時刻等）を前置しない
///   （呼び出し元が prepend_runtime_context() で付与する）
pub fn build_conversation_messages(
    conn: &rusqlite::Connection,
    session_id: &str,
    agent_id: &str,
) -> Vec<ChatMessage> {
    let logs = opencrab_db::queries::list_session_logs_by_session(conn, session_id)
        .unwrap_or_default();

    if logs.is_empty() {
        return vec![];
    }

    // 各ログをロールと時刻付きコンテンツに変換
    let raw: Vec<(String, String)> = logs.iter().map(|log| {
        let is_own_agent = log.speaker_id.as_deref() == Some(agent_id)
            || log.agent_id == agent_id && log.speaker_id.is_none();
        let role = if is_own_agent { "assistant" } else { "user" };
        let speaker = log.speaker_id.as_deref().unwrap_or(&log.agent_id);
        // 各メッセージに created_at を付与して会話の間隔を伝える
        // DBのcreated_atはRFC3339形式 (例: "2026-03-23T02:30:15.123456+00:00") で秒以下を含む
        // 必要に応じて NaiveDateTime::parse_from_str() 等で整形すること
        // (表示例: "[2026-03-23 02:30:15] [speaker]: content")
        let content = if is_own_agent {
            format!("[{}] {}", log.created_at, log.content)
        } else {
            let speaker_id = log.speaker_id.as_deref().unwrap_or("");
            format!("[{}] [{} ({})]: {}", log.created_at, speaker, speaker_id, log.content)
        };
        (role.to_string(), content)
    }).collect();

    // 連続する同じroleをマージ（Alternating Role制約対応）
    let mut merged: Vec<ChatMessage> = vec![];
    for (role, content) in raw {
        if let Some(last) = merged.last_mut() {
            if last.role == role {
                last.content.push('\n');
                last.content.push_str(&content);
                continue;
            }
        }
        merged.push(ChatMessage {
            role,
            content,
            tool_call_id: None,
            tool_calls: vec![],
            content_parts: vec![],
            cache_control: None,
        });
    }
    merged
}
```

**既存 `build_conversation_string()`:** Phase 1では継続使用。Phase 2で `build_conversation_messages()` に置換。

---

### 3-3. `crates/server/src/api/agents_messages.rs`

**Phase 1変更:** `build_agent_context()` + `prepend_runtime_context()` の適用

```rust
// 変更前:
let (system_prompt, agent_name) = {
    let conn = state.db.lock().unwrap();
    process::build_agent_context(&conn, &id, "direct_message")
};
let conversation = {
    let conn = state.db.lock().unwrap();
    process::build_conversation_string(&conn, &session_id)
};

// 変更後 (Phase 1):
let (system_prompt, agent_name) = {
    let conn = state.db.lock().unwrap();
    process::build_agent_context(&conn, &id)  // session_theme引数を削除
};
let conversation_raw = {
    let conn = state.db.lock().unwrap();
    process::build_conversation_string(&conn, &session_id)
};
let conversation = process::prepend_runtime_context(&conversation_raw, "direct_message");
```

---

### 3-4. `crates/discord/src/message_loop.rs`

**Phase 1変更:** `build_agent_context()` + `prepend_runtime_context()` の適用、および `message_id` のsystemからの除外

```rust
// 変更前:
let (base_prompt, agent_name) = state.build_agent_context(agent_id, "Discord conversation");
let system_prompt = format!(
    "{}\n\n[Discord context: channel_id={}, message_id={}]",
    base_prompt, channel_id_str, discord_message_id
);
let conversation = state.build_conversation_string(&session_id);

// 変更後 (Phase 1):
let (base_prompt, agent_name) = state.build_agent_context(agent_id);  // theme引数削除
// message_idをsystemから除外。channel_idのみ残す（固定値なので問題なし）
let system_prompt = format!(
    "{}\n\n[Discord context: channel_id={}]",
    base_prompt, channel_id_str
    // ← message_id は削除（systemを毎回変えてしまうため）
);
let conversation_raw = state.build_conversation_string(&session_id);
// message_idは最後のuserメッセージのContextセクションに含める
let conversation = process::prepend_runtime_context_discord(
    &conversation_raw,
    "Discord conversation",
    discord_message_id,  // ← Contextセクションへ移動
);
```

`prepend_runtime_context_discord()` は `prepend_runtime_context()` の拡張版として追加:
```rust
pub fn prepend_runtime_context_discord(
    user_message: &str,
    session_theme: &str,
    message_id: u64,
) -> String {
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %Z");
    format!(
        "[Context]\nCurrent date and time: {now}\nCurrent discussion topic: {session_theme}\nDiscord message_id: {message_id}\n\n{user_message}"
    )
}
```

**Phase 2変更:** マルチターン形式への移行。`build_conversation_messages()` を使用するよう変更。
`is_bot` フラグは `Sender.is_bot` から取得可能だが、role判定はすでに `speaker_id == agent_id` の比較で十分。

---

### 3-5. `crates/server/src/agent_runner_impl.rs` および他の呼び出し元

`build_agent_context()` の引数変更（`session_theme` 削除）に伴い、
すべての呼び出し箇所を更新する。
`grep -rn "build_agent_context" crates/` で洗い出すこと。

---

## 4. 実装手順

### Phase 1（必須・最大効果・比較的低リスク）

1. **`build_agent_context()` からRuntimeセクションを削除**
   - `now` / `session_theme` をsystemから除去
   - 関数シグネチャから `session_theme: &str` 引数を削除
   - `prepend_runtime_context()` ヘルパーを追加

2. **すべての呼び出し元で `prepend_runtime_context()` を適用**
   - `api/agents_messages.rs`
   - `discord/src/message_loop.rs`（2箇所: 通常処理 + subtask completion callback）
   - その他 `build_agent_context()` を呼ぶ箇所すべて

3. **`message_loop.rs` の `[Discord context]` から `message_id` を除外（Phase 1必須）**
   - `message_id` は毎リクエストで変化するため、systemに含めるとsystemが固定にならない
   - `channel_id` のみsystemに残す（セッション内で固定の値なので問題なし）
   - `message_id` は `prepend_runtime_context_discord()` 経由で最後のuserメッセージの
     `[Context]` セクションに含める
   - **これはPhase 1と同時に行わないと、Discord経由リクエストではsystemが毎回変わり
     BP2のキャッシュが一切効かない**（5-10参照）

4. **`CacheControl` 構造体を追加** (`engine.rs`)

5. **`ChatMessage` / `ToolDefinition` に `cache_control` フィールドを追加** (`engine.rs`)

6. **ツール定義の順序安定化（必須・5-4参照）**
   - `crates/actions/src/dispatcher.rs` の `actions: HashMap<String, Arc<dyn Action>>` を
     **`IndexMap<String, Arc<dyn Action>>`** に変更する（`indexmap` クレートを依存に追加）
   - または `BTreeMap` に変更（キー順でソートされる）
   - または `Vec<(String, Arc<dyn Action>)>` で登録順を保持する
   - **これを行わないとキャッシュが無効化され続ける**（詳細は5-4参照）

7. **`run_with_model_override()` でBP1(tools) / BP2(system) を設定** (`engine.rs`)
   ```rust
   // BP1: toolsの最後のツールに cache_control(1h) を付与
   let mut tools = self.executor.list_tools();
   if let Some(last_tool) = tools.last_mut() {
       last_tool.cache_control = Some(CacheControl {
           r#type: "ephemeral".to_string(),
           ttl: Some("1h".to_string()),
       });
   }

   // BP2: systemメッセージに cache_control(1h) を付与
   let system_msg = ChatMessage {
       role: "system".to_string(),
       content: system_context.to_string(),
       cache_control: Some(CacheControl {
           r#type: "ephemeral".to_string(),
           ttl: Some("1h".to_string()),
       }),
       ..Default::default()
   };
   ```

8. **`to_llm_message()` / `to_function_def()` で `cache_control` を伝播** (`llm_adapter.rs`)

9. **`convert_message()` / `build_request_body()` でJSONに `cache_control` を出力** (`providers/openai.rs`)
   - Anthropicのtoolsでは `cache_control` はツールオブジェクトのトップレベルに置く:
     ```json
     {"name": "...", "description": "...", "input_schema": {...}, "cache_control": {...}}
     ```

10. **hermit-shell の `openaiToolsToAnthropic()` を `cache_control` パススルー対応に修正（必須・5-7参照）**
    - `OpenAITool` インターフェースに `cache_control?: object` フィールドを追加
    - `openaiToolsToAnthropic()` でツールトップレベルの `cache_control` を出力に含める
    - **これを行わないと、opencrabが付与した `cache_control` がhermit-shellで落とされる**

11. **サーバー再起動 → `cache_read_input_tokens` > 0 を確認**
    - `/api/llm-logs` でキャッシュヒット率・節約トークン数を確認
    - `cache_read_input_tokens` が増加していれば正常動作

### Phase 2（最大効果・高インパクト: 会話履歴キャッシュ）

> Phase 1完了後に着手。会話履歴全体のキャッシュを有効にする最重要フェーズ。

12. **`build_conversation_messages()` を実装** (`process.rs`)
    - セッションログをマルチターン形式の `Vec<ChatMessage>` として返す
    - ロール判定: `speaker_id == agent_id` → `assistant`、それ以外 → `user`
    - 他Botの発言も `user` ロール（`[bot名]: content` 形式でコンテンツに発言者名を含める）

13. **`run_with_model_override()` をマルチターン対応に変更** (`engine.rs`)
    - `system_context` + `user_message` の2引数から、`system_context` + `Vec<ChatMessage>` の形式へ変更
    - または: systemとuserメッセージを外側で組み立てて `messages` ベクタを渡す新シグネチャを追加

14. **呼び出し元で `build_conversation_messages()` を使用**
    - 最後のメッセージに `prepend_runtime_context()` を適用
    - `process_rs` 内の `build_conversation_string()` 呼び出しを置換

15. **Automatic Cachingの動作確認**
    - マルチターン会話で `cache_read_input_tokens` が会話ターンごとに増加することを確認
    - 2ターン目以降で会話履歴分のトークンがキャッシュヒットすることを確認

### Phase 3（オプション: tool-useループ最適化）

16. **ループ内でBP3(messages)を設定**
    - `run_with_model_override()` 内で、2回目以降のイテレーション時に
      メッセージリストの最後から2番目に `cache_control(5m)` を付与
    - ツール呼び出しが多い複雑なエージェントタスクで効果大

---

## 5. 注意事項・エッジケース

### 5-1. 初回リクエスト（会話履歴なし）
`build_conversation_messages()` が空のVecを返す場合、
最後の `user` メッセージは変動コンテキスト + 現在のメッセージのみ。
Automatic Cachingは会話が2ターン以上になってから効く（初回は書き込みのみ）。

### 5-2. Heartbeatなど初回userメッセージなしのケース
会話ログが空でも `prepend_runtime_context("No messages yet.", theme)` を前置して問題なし。

### 5-3. `[Context]` セクションの二重付与防止
過去の `user` メッセージに `[Context]` セクションを含めないよう注意。
`build_conversation_messages()` は過去ログをそのまま返し、
呼び出し元が **最後のメッセージにのみ** `prepend_runtime_context()` を適用する。

### 5-4. ツール定義のJSON安定性 ⚠️ 要修正

**実際にコードを確認した結果:**

`crates/actions/src/dispatcher.rs` の `get_definitions()` は以下の実装になっている:

```rust
// dispatcher.rs
pub struct ActionDispatcher {
    actions: HashMap<String, Arc<dyn Action>>,  // ← HashMap
    ...
}

pub fn get_definitions(&self, filter: &[String]) -> Vec<ActionDefinition> {
    self.actions
        .values()  // ← HashMap::values() = 非安定な順序
        .filter(...)
        .map(...)
        .collect()
}
```

`HashMap::values()` は**イテレーション順序が非安定**（ビルドごと・実行ごとに変わりうる）。
`bridge.rs` の `list_tools()` は `get_definitions()` の結果を Vec に push するが、
その基盤が非安定なため、**最終的なツールリストの順序も非安定**となる。

**ツール定義の順序が変わると tools の `cache_control` ハッシュが変わり、BP1のキャッシュが毎回無効化される。**
これはPhase 1を機能させるために必須の修正。

**対処方針:**
- `HashMap` → `IndexMap<String, Arc<dyn Action>>` に変更（登録順を保持、`indexmap` クレートを依存に追加）
- または `BTreeMap<String, Arc<dyn Action>>` に変更（キー名でソートされ決定的）
- **推奨: `IndexMap`**（登録順を保持するため、意図した順序でツールが並ぶ）

`bridge.rs` の gateway actionsのpush順序は固定のため問題なし（こちらは明示的なforループ）。

**実装前にユニットテストで順序安定性を確認すること:**
```rust
#[test]
fn test_get_definitions_order_is_stable() {
    let dispatcher = create_dispatcher();
    let defs1 = dispatcher.get_definitions(&[]);
    let defs2 = dispatcher.get_definitions(&[]);
    let names1: Vec<_> = defs1.iter().map(|d| &d.name).collect();
    let names2: Vec<_> = defs2.iter().map(|d| &d.name).collect();
    assert_eq!(names1, names2);
}
```

### 5-5. TTL混在ルール（Anthropic仕様）
**長いTTL → 短いTTL の順序が必須。**
設計: `tools(1h) → system(1h) → messages(5m)` → **ルール準拠。** ✓

### 5-6. 最小キャッシュサイズ
Claude Sonnet 4.6は2048トークン以上でないとキャッシュされない。
現在のsystemプロンプトは推定5,000〜8,000トークン → **条件クリア。**
tools単独では2048未満の可能性があるが、Anthropicはtools+systemを累積してチェックするため実質問題なし。

### 5-7. tools の `cache_control` フィールド配置と hermit-shell の対応 ⚠️ 要修正

Anthropic APIでは `cache_control` はツールオブジェクトのトップレベルに置く（`function` の外）:
```json
{
  "name": "some_tool",
  "description": "...",
  "input_schema": {...},
  "cache_control": {"type": "ephemeral", "ttl": "1h"}  ← トップレベル
}
```

**実際にコードを確認した結果:**

`hermit-shell/src/utils/tool_convert.ts` の `openaiToolsToAnthropic()` は以下の実装:

```typescript
export function openaiToolsToAnthropic(tools: OpenAITool[]): AnthropicTool[] {
  return tools.map((t) => ({
    name: t.function.name,
    description: t.function.description,
    input_schema: t.function.parameters ?? { type: "object", properties: {} },
    // ← cache_control が一切出力されていない！
  }));
}
```

さらに `OpenAITool` インターフェースにも `cache_control` フィールドが定義されていない:
```typescript
export interface OpenAITool {
  type: "function";
  function: OpenAIFunction;
  // ← cache_control フィールドなし
}
```

つまり **opencrabが `cache_control` 付きのツール定義をhermit-shellに送っても、
`openaiToolsToAnthropic()` でフィールドが落とされてAnthropicには届かない**。

**対処方針（hermit-shell側の修正が必要）:**

```typescript
// 修正: OpenAITool インターフェースに cache_control を追加
export interface OpenAITool {
  type: "function";
  function: OpenAIFunction;
  cache_control?: object;  // ← 追加
}

// 修正: openaiToolsToAnthropic() で cache_control をパススルー
export function openaiToolsToAnthropic(tools: OpenAITool[]): AnthropicTool[] {
  return tools.map((t) => {
    const result: AnthropicTool & { cache_control?: object } = {
      name: t.function.name,
      description: t.function.description,
      input_schema: t.function.parameters ?? { type: "object", properties: {} },
    };
    if (t.cache_control) {
      result.cache_control = t.cache_control;
    }
    return result;
  });
}
```

**順序について:** `Array.map()` を使用しているため順序は入力配列と同一になる（問題なし）。
ただし opencrab側の `list_tools()` が非安定順序を返す問題（5-4参照）が先に解決されている必要がある。

### 5-8. Discord `message_loop.rs` の2箇所更新
通常のメッセージ処理（line ~195）と、
subtask completion callback（line ~277）の2箇所で
`build_agent_context()` / `build_conversation_string()` を呼んでいる。
**両方を更新すること。** 片方だけ更新すると挙動が非一貫になる。

### 5-9. 各メッセージへの発言時刻付与（Phase 2）

`build_conversation_messages()` では各メッセージの `created_at` を先頭に付与する（2-3参照）。
これにより「3時間前の会話の続き」「昨日の話」等の会話間隔がLLMに伝わる。

**発言フォーマット（非エージェント発言）:**
```
[2026-03-23 02:30:15] [kojira (390732846236434452)]: こんにちは
[2026-03-23 02:31:00] [らぼみbot (1468658779917910191)]: よろしく
```
`speaker_id`を付与することで、同名ユーザーが複数存在する場合でも正確に識別できる。

### 5-10. Alternating Role問題（Phase 2必須対応）

Anthropic APIのmessages配列は **user→assistant→user→assistant の交互形式が必須**。
しかし現実の会話では複数のユーザーや他Botが連続して発言するケースがある。

**問題の例:**
```
[kojira]: こんにちは        → user
[らぼみbot]: よろしく       → user（連続！APIがエラーになる）
[kojira]: 質問があって...   → user（さらに連続！）
[agent]: はい、どうぞ       → assistant
```

**対処方針: 連続するuserメッセージをマージする**

連続する `user` ロールのメッセージを1つの `user` メッセージに結合する。
発言者名はコンテンツ内に含まれているため情報は失われない。

```
マージ結果:
[kojira]: こんにちは
[らぼみbot]: よろしく
[kojira]: 質問があって...
→ role: "user", content: "[kojira]: こんにちは\n[らぼみbot]: よろしく\n[kojira]: 質問があって..."

[agent]: はい、どうぞ
→ role: "assistant", content: "はい、どうぞ"
```

**`build_conversation_messages()` の実装にマージロジックを含める:**

```rust
pub fn build_conversation_messages(
    conn: &rusqlite::Connection,
    session_id: &str,
    agent_id: &str,
) -> Vec<ChatMessage> {
    let logs = ...; // DBから取得
    let raw_messages: Vec<(String, String)> = logs.iter().map(|log| {
        let role = if is_own_agent { "assistant" } else { "user" };
        let content = ...;
        (role.to_string(), content)
    }).collect();

    // 連続する同じroleをマージ（Alternating Rule対応）
    let mut merged: Vec<ChatMessage> = vec![];
    for (role, content) in raw_messages {
        if let Some(last) = merged.last_mut() {
            if last.role == role {
                // 同じroleが連続 → 既存メッセージに追記
                last.content.push('\n');
                last.content.push_str(&content);
                continue;
            }
        }
        merged.push(ChatMessage { role, content, ..Default::default() });
    }
    merged
}
```

### 5-11. `is_bot` フラグとロール判定の関係
`Sender.is_bot` は現状Discordから受信したメッセージのフィールドとして存在するが、
**セッションログDB (`session_logs`)には `is_bot` が保存されていない**。
Phase 2の `build_conversation_messages()` 実装では、`speaker_id == agent_id` の比較のみで
ロールを判定する（DBに `is_bot` を保存するスキーマ変更は不要）。

必要であれば `session_logs.metadata_json` に `is_bot` フラグを保存する拡張も可能だが、
現時点では不要（他Botは `user` ロールにすれば十分）。

### 5-12. `[Discord context]` の `message_id` 問題 → Phase 1で対応（実装手順3参照）

~~`message_loop.rs` では `system_prompt` に `[Discord context: channel_id=..., message_id=...]` を
後付けしており、`message_id` が毎リクエストで変化するためsystemプロンプトが固定にならない問題。~~

**Phase 1での対応方針（セクション4の手順3）:**
- `message_id` をsystemから除外し、`prepend_runtime_context_discord()` 経由で
  最後のuserメッセージの `[Context]` セクションに移動する
- `channel_id` のみsystemに残す（セッション内で固定値なので問題なし）
- **この修正はPhase 1と同時に行わないと、Discord経由のリクエストではsystemが毎回変わってしまい
  BP2のキャッシュが一切効かない**

---

## 6. キャッシュ効果の確認

実装後、LLMログの `cache_read_input_tokens` が増加していれば正常動作:

```json
{
  "prompt_tokens": 1500,
  "completion_tokens": 34,
  "cache_read_input_tokens": 7000,    ← キャッシュヒット（tools+system）
  "cache_creation_input_tokens": 0    ← キャッシュ済みなので書き込みなし
}
```

Phase 2（マルチターン）後は `cache_read_input_tokens` がさらに増加（会話履歴分を含む）:
```json
{
  "prompt_tokens": 200,               ← 最新メッセージのみ
  "cache_read_input_tokens": 15000,   ← tools+system+会話履歴全体
  "cache_creation_input_tokens": 500  ← 新しい会話ターン（次回からヒット）
}
```

opencrabの `UsageInfo` 構造体と `from_llm_response()` は既に
`cache_read_input_tokens` / `cache_creation_input_tokens` を読み取り済み。
ダッシュボードのLLMログ統計画面でヒット率・節約トークン数を確認できる。

---

## 7. コスト削減試算

### Phase 1完了時（tools + system のみキャッシュ）

| キャッシュ対象 | トークン数（推定） | キャッシュなし | キャッシュヒット時 | 節約率 |
|-------------|---------|-------------|-------------|-------|
| tools | ~1,000 | $0.003/req | $0.0003/req | 90% |
| system | ~6,000 | $0.018/req | $0.0018/req | 90% |
| **合計** | **~7,000** | **$0.021/req** | **$0.0021/req** | **90%** |

### Phase 2完了時（会話履歴も含む）

10ターンの会話（各500トークン）でキャッシュが効く場合:

| キャッシュ対象 | トークン数（推定） | キャッシュなし | キャッシュヒット時 | 節約率 |
|-------------|---------|-------------|-------------|-------|
| tools + system | ~7,000 | $0.021/req | $0.0021/req | 90% |
| 会話履歴 | ~5,000 | $0.015/req | $0.0015/req | 90% |
| **合計** | **~12,000** | **$0.036/req** | **$0.0036/req** | **90%** |

（Claude Sonnet 4.6 ベース $3/MTok、ヒット $0.30/MTok）

1時間TTLの書き込みコストはベースの2倍 ($6/MTok) だが、
1時間以内に10回以上リクエストがあれば十分ペイする。

---

*作成: 2026-03-23 v1 → v2全面改訂: 2026-03-23 → v3改訂: 2026-03-23*
*v3変更点:*
- *修正1: 5-10の `message_id` 問題をPhase 1に格上げ（手順3に追加、3-4節を更新）*
- *修正2: 5-4のツール順序「問題なし」を削除。dispatcher.rsのHashMap非安定順序を確認・記載。対処方針（IndexMap/BTreeMap）を追加*
- *修正3: 5-7にhermit-shell `openaiToolsToAnthropic()` の `cache_control` 非対応を確認・記載。修正方針（TypeScript）を追加*

*v4変更点:*
- *5-9（旧）: Alternating Role問題（連続userメッセージ）を追加。対処方針（マージ）と`build_conversation_messages()`への実装例を記載*

*v5変更点:*
- *2-3: 各過去メッセージへの`created_at`付与を追加。現在時刻（処理タイミング）と各発言時刻（会話文脈）の2種類の時刻の使い分けを明記。各発言に`[created_at]`を付与する設計を追加*
- *3-2: `build_conversation_messages()`の実装例に`created_at`付与とAlternating Roleマージを統合*
- *5-9: 各メッセージへの発言時刻付与セクションを追加*
- *5-10（旧5-9）: Alternating Role問題（連続userメッセージのマージ）*
- *5-11（旧5-10）: is_botフラグとロール判定のセクション番号修正*
- *5-12（旧5-11）: Discord context message_idのセクション番号修正*

*v6変更点:*
- *各発言の時刻フォーマットを秒まで含む形式に変更: `[YYYY-MM-DD HH:MM]` → `[YYYY-MM-DD HH:MM:SS]`*
- *2-3節の例示コードのタイムスタンプを秒まで含む形式に更新*
- *3-2節の`build_conversation_messages()`にDBのcreated_atがRFC3339形式（秒以下含む）で保存されていることを注記*

*v7変更点:*
- *発言フォーマットにspeaker_idを追加: `[speaker (speaker_id)]: content` 形式。名前が被った場合でも正確な識別が可能*
- *2-3節の例示コードを更新*
- *5-9節に発言フォーマット例を追加*
- *3-2節の`build_conversation_messages()`のformat文字列を更新（`speaker_id = log.speaker_id.as_deref().unwrap_or("")`で取得）*
