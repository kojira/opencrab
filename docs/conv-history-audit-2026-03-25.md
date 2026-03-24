# 会話履歴監査レポート
**日時:** 2026-03-25  
**対象:** opencrab プロジェクト — 会話履歴フォーマット監査  
**判断基準:** エージェントが「いつ・何が起きたか」を正しく理解できるか  

---

## 調査フロー

```
メッセージ受信
  → memory_sessions に SessionLogRow 保存 (log_type, content, metadata_json, created_at)
  → build_conversation_string() がログをテキスト変換
  → format_single_log() が各 log_type を文字列化
  → LLM の user メッセージとして渡す
```

---

## 現状のフォーマット

### `format_single_log()` の出力 (`crates/server/src/process.rs:314`)

| log_type | 現在の出力形式 |
|----------|--------------|
| `speech` | `[speaker_id] [YYYY-MM-DD HH:MM:SS]:\n{content}` |
| `tool_call` | `[tool_call] [YYYY-MM-DD HH:MM:SS]:\n{content}\nTools: name1, name2` |
| `tool_result` | `[tool_result: tool_name] [YYYY-MM-DD HH:MM:SS]:\n{content}` |
| `system` | `[system: {type}] [YYYY-MM-DD HH:MM:SS]:\n{pretty json}` |

### subtask_cancelled の保存先
- `log_type = "system"`, `content = JSON({"type":"subtask_cancelled","subtask_id":"...","session_id":"...","task":"..."})`
- 表示: `[system: subtask_cancelled] [ts]:\n{json}`

---

## 期待値 vs 現状ギャップ

### ギャップ一覧

| # | 観点 | 期待値 | 現状 | 影響 |
|---|------|--------|------|------|
| G1 | **tool_call に id なし** | `tool_call [id=abc123]: search_files(query="config")` | `[tool_call][ts]:\nTools: search_files` | LLMがtool_callとtool_resultを紐付けられない |
| G2 | **tool_result に id なし** | `tool_result [id=abc123]: search_files → 結果` | `[tool_result: search_files][ts]:\n{raw_json}` | 同上 |
| G3 | **subtask_cancelled の命名** | `tool_cancelled` | `[system: subtask_cancelled]` | タスクで指定の統一名と不一致 |
| G4 | **subtask_cancelled の log_type** | `log_type="tool_cancelled"` で tool_call/tool_result と統一 | `log_type="system"` | log_type別フィルタが効かない |
| G5 | **subtask_cancelled に id なし** | `tool_cancelled [id=abc123]: search_files がキャンセルされた` | subtask_id は content JSON 内に埋まっているが tool_call_id と別系統 | 照合不可 |
| G6 | **tool_call の引数表示** | `name(query="config")` | `Tools: name` (引数なし) | デバッグしにくい |
| G7 | **タイムスタンプ位置** | `[ts] type [id=...]:` (ts先頭) | `[type][ts]:` (typeが先、tsが後) | 可読性の問題（軽微）|

### 最重要ギャップ: **id 照合不可 (G1, G2, G5)**

現在の DB 保存構造:
```
tool_call ログ:
  log_type = "tool_call"
  metadata_json = {"tool_calls_json": "[{\"id\":\"abc123\",\"name\":\"search_files\",...}]"}
  → format_single_log() は id を表示しない ❌

tool_result ログ:
  log_type = "tool_result"
  metadata_json = {"tool_call_id": "abc123", "tool_name": "search_files", "is_error": false}
  → format_single_log() は id を表示しない ❌

subtask_cancelled ログ:
  log_type = "system"
  content = {"type":"subtask_cancelled","subtask_id":"xyz","..."}
  → tool_call の id (call id) と subtask_id は別系統 ❌
```

id が表示されないため、LLM に渡る会話履歴では **どの tool_call にどの tool_result が対応しているか** が分からない。

---

## 修正提案

### 提案1: `format_single_log()` の改修 ⭐️最優先

**対象:** `crates/server/src/process.rs`

```rust
// tool_call の表示例（提案）:
// [tool_call] [2026-03-25 06:30:16]:
// [id=abc123]: search_files({"query":"config"})
// [id=def456]: read_file({"path":"/tmp/x"})

"tool_call" => {
    if let Some(meta_json) = log.metadata_json.as_deref() {
        if let Ok(meta) = serde_json::from_str::<serde_json::Value>(meta_json) {
            if let Some(calls_json) = meta.get("tool_calls_json").and_then(|v| v.as_str()) {
                if let Ok(calls) = serde_json::from_str::<serde_json::Value>(calls_json) {
                    if let Some(items) = calls.as_array() {
                        let call_lines: Vec<String> = items.iter()
                            .filter_map(|item| {
                                let id = item.get("id")?.as_str()?;
                                let name = item.get("name")?.as_str()?;
                                let args = item.get("arguments")
                                    .map(|a| a.to_string())
                                    .unwrap_or_default();
                                Some(format!("[id={}]: {}({})", id, name, args))
                            })
                            .collect();
                        if !call_lines.is_empty() {
                            return format!("[tool_call]{}:\n{}", ts, call_lines.join("\n"));
                        }
                    }
                }
            }
        }
    }
    format!("[tool_call]{}:\n{}", ts, log.content)
}

// tool_result の表示例（提案）:
// [tool_result] [2026-03-25 06:30:17]:
// [id=abc123]: search_files → {content}

"tool_result" => {
    let meta = log.metadata_json.as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());
    let tool_call_id = meta.as_ref()
        .and_then(|m| m.get("tool_call_id").and_then(|v| v.as_str()))
        .unwrap_or("?");
    let tool_name = meta.as_ref()
        .and_then(|m| m.get("tool_name").and_then(|v| v.as_str()))
        .unwrap_or("unknown");
    format!("[tool_result]{}:\n[id={}]: {} → {}", ts, tool_call_id, tool_name, log.content)
}
```

### 提案2: `subtask_cancelled` → `tool_cancelled` リネーム ⭐️

**対象 A: `crates/discord/src/gateway_actions/subtask_engine.rs`**

```rust
// 変更前:
log_type: "system".to_string(),
content: serde_json::json!({
    "type": "subtask_cancelled",
    "subtask_id": subtask_id,
    ...
}).to_string(),

// 変更後:
// subtask_id を tool_call_id として保存（spawn_subtask の return data["subtask_id"] と一致させる）
log_type: "tool_cancelled".to_string(),
content: format!("subtask '{}' was cancelled", task_description),
metadata_json: Some(serde_json::json!({
    "tool_call_id": subtask_id,  // spawn_subtask が返す subtask_id
    "tool_name": "spawn_subtask",
    "task": task_description,
}).to_string()),
```

**対象 B: `format_single_log()` に tool_cancelled ケース追加**

```rust
"tool_cancelled" => {
    let meta = log.metadata_json.as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());
    let tool_call_id = meta.as_ref()
        .and_then(|m| m.get("tool_call_id").and_then(|v| v.as_str()))
        .unwrap_or("?");
    let tool_name = meta.as_ref()
        .and_then(|m| m.get("tool_name").and_then(|v| v.as_str()))
        .unwrap_or("unknown");
    format!("[tool_cancelled]{}:\n[id={}]: {} がキャンセルされた\n{}", ts, tool_call_id, tool_name, log.content)
}
```

### 提案3: spawn_subtask の id 一貫性確保

`spawn_subtask` は subtask_id を返すが、これを「spawn_subtask ツールの tool_call_id」として扱うのは無理がある（LLM が発行した tool_call の id ≠ subtask_id）。

**実用的な落とし所:** subtask_id を `[tool_cancelled]` のラベルとして表示し、エージェントが「自分が呼んだ `spawn_subtask` の結果 subtask_id=xxx がキャンセルされた」と分かれば十分。

```
[tool_cancelled] [2026-03-25 06:30:18]:
[subtask_id=abc-uuid]: spawn_subtask がキャンセルされた
Task: ファイルを検索して
```

---

## まとめ: 修正優先度

| 優先度 | 対象 | 修正内容 | ファイル |
|--------|------|---------|---------|
| 🔴 必須 | G1/G2: id照合 | tool_call/tool_result に `[id=xxx]` を表示 | `process.rs:format_single_log()` |
| 🔴 必須 | G3/G4: 命名統一 | `subtask_cancelled` → `log_type="tool_cancelled"` に変更 | `subtask_engine.rs`, `process.rs` |
| 🟡 推奨 | G5: id連携 | subtask_id を tool_cancelled の metadata に `tool_call_id` として格納 | `subtask_engine.rs` |
| 🟡 推奨 | G6: 引数表示 | tool_call で `name(args)` 形式表示 | `process.rs:format_single_log()` |
| 🟢 任意 | G7: ts位置 | `[ts] type [id=...]:` 形式に統一 | `process.rs:format_single_log()` |

---

## 期待される修正後の出力

```
[speech] [2026-03-25 06:30:15]:
[User123]: ファイルを検索して

[tool_call] [2026-03-25 06:30:16]:
[id=abc123]: search_files({"query":"config"})

[tool_result] [2026-03-25 06:30:17]:
[id=abc123]: search_files → {"success":true,"data":["config.toml","config.yaml"]}

[speech] [2026-03-25 06:30:17]:
[AgentBot]: 3件見つかりました

--- キャンセルパターン ---

[tool_call] [2026-03-25 06:30:16]:
[id=abc123]: search_files({"query":"config"})

[speech] [2026-03-25 06:30:17]:
[User123]: やっぱりやめて

[tool_cancelled] [2026-03-25 06:30:18]:
[id=abc123]: search_files がキャンセルされた
```

---

*生成: サブエージェント opencrab-conv-history-audit (2026-03-25)*
