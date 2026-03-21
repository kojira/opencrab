# 設計ドキュメント: サブタスク委譲アーキテクチャ（もぐたろう防止）

> TODO #19 — 一次回答 + バックグラウンド処理分離  
> ステータス: 設計中（レビュー待ち: のすたろう・らぼみ・kojira）

---

## 1. 現状分析

### 1.1 `crates/server/src/process.rs` — `run_agent_response` の処理フロー

```
run_agent_response()
  ├─ ワークスペース・BridgedExecutor・LlmRouterAdapter を構築
  ├─ SkillEngine::new(llm, executor, max_iterations=20) を生成
  ├─ engine.run_with_model_override(...) を await  ← ここで全処理がブロック
  └─ EngineResult を返す
```

**重要:** `run_agent_response` は完全に同期的（awaitで完了待ち）。
呼び出し元（`message_loop.rs`）はこの完了を待ってから Discord に送信する。

### 1.2 `crates/core/src/engine.rs` — `run_with_model_override` のループ構造

```rust
loop {
    iterations += 1;
    if iterations > self.max_iterations {
        // stopped_by_limit = true で打ち切り
        return Ok(EngineResult { stopped_by_limit: true, ... });
    }
    
    let response = self.llm.chat(request).await?;  // LLM呼び出し
    
    if !response.tool_calls.is_empty() {
        // ツール実行 → メッセージに追加 → continue
        for tool_call in &response.tool_calls {
            let result = self.executor.execute(...).await;
            messages.push(tool result);
        }
        continue;
    }
    
    // ツールコールなし → 最終応答として return
    return Ok(EngineResult { response: final_text, stopped_by_limit: false, ... });
}
```

**ループの特性:**
- 1 ループ = 1 LLM呼び出し + N ツール実行
- ツールコールがある限りループし続ける
- `execute_shell` が長時間かかる場合もループ内でブロック
- 最大 20 ループ（= 最大 20 LLM呼び出し）

### 1.3 max_iterations の場所

`crates/server/src/process.rs` の `run_agent_response` 内:
```rust
let mut engine = opencrab_core::SkillEngine::new(
    Box::new(llm_client),
    Box::new(executor),
    20, // max iterations  ← ここ
);
```

`engine.rs` の `SkillEngine::new` の引数として渡される。
実際の上限チェックは `engine.rs` の `run_with_model_override` ループ先頭で行われる。

### 1.4 Discord への応答タイミング

```
message_loop.rs::run_discord_loop()
  ├─ Discord メッセージ受信
  ├─ gateway.start_typing(channel_id)  ← タイピングインジケーター開始
  ├─ run_agent_response(...).await     ← ここで全処理（数十秒〜数分）ブロック
  └─ gateway.send_to_channel(...)      ← 処理完了後にやっと送信
```

**問題点:**
- エンジン完了まで Discord に一切応答なし（「もぐたろう状態」）
- タイピングインジケーターは最大 10 秒で自動消える（Discord仕様）
- max_iterations=20 に達すると途中状態のまま「最大ステップ数に達しました」を返す
- 画像生成・複数ステップのシェル処理などは特に長くなる

---

## 2. 設計方針の選択肢と比較

### 案A: 即時一次応答 + 残り処理を非同期タスクとして spawn ⭐推奨

**概要:**
1. メッセージ受信後、まず「少し待って、処理中だよ」等の一次応答を即送信
2. 残りの処理（SkillEngine 実行）を `tokio::spawn` でバックグラウンドに委譲
3. 処理完了後、結果を Discord に送信

```
受信 → 一次応答を即送信 → tokio::spawn(engine実行) → 完了後に結果送信
```

**メリット:**
- ユーザー体験が大幅改善（即レスポンス）
- Discord の「タイピングインジケーター」と組み合わせれば進捗感も演出可能
- LLMの max_iterations 上限の問題とは独立して解決できる
- 既存の `EngineResult` 構造を変えずに済む

**デメリット:**
- バックグラウンドタスクのライフサイクル管理が必要
- 複数メッセージが連続した場合の競合状態（同一チャンネルへの並行処理）
- セッション履歴への記録タイミングが複雑化
- 一次応答の内容をどう決めるか（LLM判断 vs ハードコード）

**難易度:** 中（既存アーキテクチャへの影響が局所的）

---

### 案B: LLM に「短い応答を先に返せ」というシステムプロンプト指示

**概要:**
システムプロンプトに「長い処理を始める前に必ず短い確認応答を返してください」という指示を追加。

```
システムプロンプト追加:
"長い処理（ファイル操作・検索・生成等）が必要な場合は、
まず1文の確認応答を返してから tools を使い始めてください。"
```

**メリット:**
- 実装コスト最小（プロンプト変更のみ）
- LLM が文脈を理解して一次応答内容を決められる
- Rust コードの変更不要

**デメリット:**
- LLM の指示遵守は保証できない（特にツールコールと同時には返せない）
- OpenAI/Anthropic API の構造上、テキスト応答とツールコールは同一ターンで返せるが、
  Discord への送信は engine 完了後のため意味がない
- 根本的な解決にならない（処理中はまだブロック）
- **結論: 案A と組み合わせる補完策であり、単体では解決にならない**

**難易度:** 低（ただし効果も限定的）

---

### 案C: ツールコール結果を段階的に Discord に送信

**概要:**
各ツールコール完了のたびに中間結果を Discord に送信する。

```
LLM → tool_call(execute_shell) → 実行完了 → Discord に中間送信 → 次の LLM 呼び出し
```

**メリット:**
- ユーザーが処理の進捗をリアルタイムで確認できる
- 長いシェルコマンドのリアルタイムストリーミングにも発展できる

**デメリット:**
- Discord がメッセージで溢れる（各ステップごとにメッセージ送信）
- `engine.rs` の `ActionExecutor` が Discord 送信能力を持つ必要がある（依存関係の逆転）
- 既存の `BridgedExecutor` と `GatewayActions` の接続が複雑
- DiscordメッセージのID管理（編集 vs 新規送信）
- engine の責務が膨らむ

**難易度:** 高（アーキテクチャへの影響大）

---

### 案D: エージェント自身が「サブタスクツール」を呼ぶ ─ オプション案

**概要:**
`spawn_background_task` というツールを追加し、LLM 自身が長い処理を「委譲すべき」と判断したら呼ぶ。

```
LLM → spawn_background_task(command, description) → 「了解、バックグラウンドで実行します」を返す
                                                    ↓
                                                tokio::spawn(実行) → 完了後に Discord 送信
```

**メリット:**
- LLM の文脈判断でバックグラウンド委譲を制御できる
- ツールとして追加するだけなので既存アーキテクチャへの影響が小さい
- エージェントが「これは時間かかる」と判断して自律的に適用できる

**デメリット:**
- LLM が適切に判断・呼び出すかは不確実
- 単発の短い処理まで委譲される可能性
- 最初の応答（SkillEngine実行前）がまだブロックされる問題は残る

**難易度:** 中（案A の変形版として実装可能）

---

## 3. 推奨案の詳細設計

### 推奨: 案A（即時一次応答 + 非同期 spawn）

案A を基本とし、案B（プロンプト強化）で LLM の応答品質を補完する。

---

### 3.1 変更するファイル・関数

#### `crates/discord/src/message_loop.rs`

**`run_discord_loop` 関数の変更:**

現在:
```rust
// 各エージェントで同期処理
let result = state.run_agent_response(...).await;
// ↑ エンジン完了まで待機

match result {
    Ok(engine_result) => gateway.send_to_channel(channel_id, &engine_result.response).await,
    ...
}
```

変更後:
```rust
// Step 1: 一次応答を即座に送信
let ack_msg = generate_ack_message(&text);  // "少し待ってね" 等
gateway.send_to_channel(channel_id, &ack_msg).await?;

// Log ack to session
log_agent_ack(&state, &session_id, agent_id, &ack_msg);

// Step 2: 残り処理をバックグラウンドで実行
let bg_state = state.clone();
let bg_session_id = session_id.clone();
let bg_channel_id = channel_id;
let bg_gateway = gateway.clone();
// ... その他必要な引数のクローン

tokio::spawn(async move {
    let result = bg_state.run_agent_response(
        agent_id, agent_name, session_id, system_prompt, conversation,
        "discord", Some(gateway_actions), caller, &image_urls,
    ).await;
    
    // 完了後に結果を Discord に送信
    match result {
        Ok(engine_result) if !engine_result.response.is_empty() => {
            // writable check ...
            bg_gateway.send_to_channel(bg_channel_id, &engine_result.response).await?;
            // Log to DB ...
        }
        Ok(_) => { /* empty response */ }
        Err(e) => {
            // エラーも Discord に通知
            bg_gateway.send_to_channel(bg_channel_id, "エラーが発生しました").await?;
        }
    }
});
```

#### `crates/server/src/process.rs`（変更なし、または軽微）

`run_agent_response` 自体は変更不要。
ただし `AppState` が `Clone` を実装していることを確認・対応する必要がある。

---

### 3.2 一次応答の生成方法

**シンプル実装（推奨）:** 固定パターンの一次応答
```rust
fn generate_ack_message(user_text: &str) -> String {
    // キーワードベースの簡単なルーティング
    if user_text.len() > 100 || contains_task_keywords(user_text) {
        "ちょっと待ってて、処理するね ⚡".to_string()
    } else {
        // 短いメッセージは通常通り処理（バックグラウンドに回さない）
        // → この判断自体を別ロジックで行う
        "...".to_string()
    }
}
```

**高度実装（将来案）:** 軽量 LLM で一次応答を生成
- 別の軽量モデル（gemini-flash 等）で 1 ターンだけ応答を生成
- 一次応答として即送信後、フル SkillEngine をバックグラウンドで実行

**判断基準（バックグラウンド化するかどうか）:**
- メッセージの長さ（100文字超）
- タスクキーワードの検出（「作って」「調べて」「生成して」等）
- 常にバックグラウンド化（最もシンプル）← **最初はこれ推奨**

---

### 3.3 非同期タスクのライフサイクル管理

**競合状態の防止:**

同一チャンネルへの同時並行処理を防ぐため、チャンネルごとのロック機構が必要:

```rust
// AppState に追加
pub active_tasks: Arc<DashMap<String, tokio::task::JoinHandle<()>>>,
//                           ^^^^^^^^^^^^^^
//                           key: "discord-{guild_id}-{channel_id}"
```

処理フロー:
```
メッセージ受信
  ↓
active_tasks に同一チャンネルのタスクが存在?
  → YES: 前のタスクをキャンセル or キューに追加 or 無視（最初は無視でOK）
  → NO: 新規タスクを spawn して登録
  ↓
タスク完了時: active_tasks から削除
```

**タスクのキャンセル:**
- `JoinHandle::abort()` で tokio タスクをキャンセル可能
- ただし `execute_shell` 等のネイティブプロセスは別途 kill が必要

---

### 3.4 エラーハンドリング

| シナリオ | 対応 |
|---------|------|
| エンジンがエラーで終了 | Discord にエラーメッセージ送信（例: 「処理中にエラーが発生しました」） |
| max_iterations 到達 | `stopped_by_limit=true` を検知してユーザーに通知（「最大ステップ数に達したため一部未完了かもしれません」） |
| タスクがパニック | `tokio::spawn` の JoinHandle で `Err` をキャッチ、Discord に通知 |
| ネットワークエラー | 既存の retry ロジック（またはそのまま失敗として通知） |
| 書き込み不可チャンネル | 既存の writable チェックをバックグラウンドタスク内にも引き継ぐ |

---

### 3.5 セッション履歴への記録方法

**変更点:**

現在: engine 完了後に一度だけ `agent_response` をログ
変更後:
1. 一次応答（ack）を `speech` ログとして記録（`metadata_json` に `is_ack: true`）
2. engine 完了後の最終応答も通常通り `speech` ログとして記録

```rust
// ack ログ
SessionLogRow {
    log_type: "speech",
    content: ack_msg,
    metadata_json: Some(json!({
        "source": "discord_ack",
        "channel_id": channel_id_str,
        "is_ack": true,
    }).to_string()),
    ...
}

// 最終応答ログ（既存と同じ）
SessionLogRow {
    log_type: "speech",
    content: engine_result.response,
    metadata_json: Some(json!({
        "source": "discord_response",
        "channel_id": channel_id_str,
        "tool_calls_made": engine_result.tool_calls_made,
    }).to_string()),
    ...
}
```

**注意:** バックグラウンドタスクが DB を参照する際、`state.db` の `Arc<Mutex<Connection>>` を
スレッドをまたいで使用する。既存の実装と同様のパターンで問題ない（既に `tokio::spawn` での
バックグラウンド DB 操作は `process.rs` で行われている）。

---

## 4. 実装ステップ（優先順位付き）

### Phase 1: 最小実装（2〜4時間）⭐最優先

**目標:** もぐたろう状態の解消（一次応答を即送信する）

1. **`message_loop.rs` の改修**
   - `run_discord_loop` でエンジン実行前に ack メッセージを Discord 送信
   - エンジン実行を `tokio::spawn` でバックグラウンド化
   - ack ログを DB に記録

2. **`AppState` の `Clone` 対応確認**
   - `tokio::spawn` クロージャに渡すため `Clone` が必要
   - 現状確認: `Arc<T>` フィールドのみなら追加 derive で対応可

3. **基本エラーハンドリング**
   - バックグラウンドタスクの `Err` を Discord に通知

4. **動作確認**
   - 画像生成コマンドで即時応答が返ることを確認
   - DB ログが正しく記録されることを確認

### Phase 2: 品質向上（4〜8時間）

5. **チャンネルごとのタスク競合管理**
   - `DashMap<channel_id, JoinHandle>` による並行制御
   - 前のタスクがある場合の処理方針（中断 or キュー or 無視）を決定・実装

6. **一次応答の内容改善**
   - キーワードベースで応答内容を変化させる
   - または案B（プロンプト強化）と組み合わせて LLM が一次応答をテキストで返す設計

7. **`stopped_by_limit` の通知改善**
   - max_iterations 到達時に「処理が途中で止まりました」をわかりやすく通知

### Phase 3: 高度機能（将来）

8. **案D（サブタスクツール）の実装**
   - `spawn_background_task` ツールを `crates/actions` に追加
   - LLM が自律的に長い処理を委譲できるようにする

9. **タイピングインジケーターの継続**
   - バックグラウンドタスク実行中、定期的に `start_typing` を再送信
   - Discord の 10 秒タイムアウトに対応

10. **進捗通知の仕組み**
    - ツールコール完了ごとに絵文字リアクション等で進捗を示す
    - 案C の軽量版として実装

---

## 5. レビューポイント

- [ ] **一次応答の内容**: 固定文言 vs 動的生成、どちらが体験として良いか
- [ ] **並行タスクの上限**: 同一チャンネルで同時実行を許すか、直列化するか
- [ ] **バックグラウンドタスクの永続化**: プロセス再起動時に実行中タスクが消える（許容するか）
- [ ] **ack を送るかどうかの判断**: 常に送る vs 長そうな時だけ送る（誤検知の問題）
- [ ] **一次応答のキャラクター**: かいろらしい口調にするか（「ちょっと待ってて」等）

---

## 6. 関連ファイル

| ファイル | 関連箇所 |
|---------|---------|
| `crates/discord/src/message_loop.rs` | メイン変更箇所: `run_discord_loop` |
| `crates/server/src/process.rs` | `run_agent_response`（変更なし or 軽微）、max_iterations=20 |
| `crates/core/src/engine.rs` | `run_with_model_override`（変更なし）、EngineResult.stopped_by_limit |
| `crates/server/src/lib.rs` | `AppState` の Clone 対応確認 |
| `crates/actions/src/` | 案D の場合: `spawn_background_task` ツール追加 |

---

*作成: のすたろう, 2026-03-22*  
*レビュー待ち: のすたろう・らぼみ・kojira（TODO #19 より）*
