# 設計ドキュメント: サブタスク委譲アーキテクチャ（もぐたろう防止）

> TODO #19 — 一次回答 + バックグラウンド処理分離
> ステータス: **設計確定**（フロー確定: 2026-03-22）

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

> **[2026-03-22 更新]** 案Aの変更点:
> - 変更前: バックグラウンド完了後に直接Discord送信
> - 変更後: バックグラウンド完了後 → セッション履歴追加 → メインエンジン再呼び出し → Discord送信

**概要:**
1. メッセージ受信後、まず「少し待って、処理中だよ」等の一次応答を即送信
2. 残りの処理（SkillEngine 実行）を `tokio::spawn` でバックグラウンドに委譲
3. 処理完了後、結果をセッション履歴に追加
4. メインエンジンを再度呼び出し、LLMが結果を解釈してエージェントのキャラクターとして最終応答を生成
5. 最終応答を Discord に送信

```
受信 → 一次応答(ack)を即送信
     ↓
     tokio::spawn でバックグラウンド処理開始
     ↓
     バックグラウンド処理完了
     ↓
     セッション履歴に subtask_result を追加
     ↓
     メインエンジン再呼び出し（LLMがsubtask_resultを解釈）
     ↓
     最終応答（エージェントの口調）をDiscordに送信
```

> バックグラウンドタスクも `run_agent_response` と同じエンジン構成（LLM + スキル + パーソナリティ）で実行する。
> サブタスクはDiscordに直接送信せず、処理結果をセッション履歴（subtask_result）として返すだけ。
> メインエンジンが subtask_result を受け取り、エージェントとして解釈・整形して最終応答を生成する。

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

### 案D: `spawn_subtask` gateway action — エージェントの自発的サブタスク起動 ⭐案Aと統合

**[2026-03-22 更新] 案A（即時ack + バックグラウンド化）と統合する設計として確定。**

**概要:**
`spawn_subtask` をgateway actionとして実装し、LLMがどこからでも呼べるようにする。
メインエンジン（depth=0）が `spawn_subtask` を呼ぶと、サブエンジン（depth=1）が
tokio::spawn で起動され、処理結果がセッション履歴に追加される。

```
[Discordメッセージ受信]
   ↓
メインエンジン起動（depth=0）
   ↓
LLMが判断: 「これは時間がかかる → spawn_subtask を使おう」
   ↓
spawn_subtask ツール呼び出し → tokio::spawn で サブエンジン起動（depth=1）
   ↓ （spawn直後に即リターン）
LLMが ack テキストを返す: 「少し待ってて、処理するね ⚡」
   ↓
Discord に ack を送信（メインエンジン完了）

[バックグラウンド処理]
サブエンジン（depth=1）が処理完了
   ↓
セッション履歴に subtask_result を追加
   ↓
メインエンジンを再呼び出し（depth=0）
   ↓
LLMが subtask_result を解釈してエージェントのキャラクターとして応答
   ↓
Discord に最終応答を送信
```

**なぜ gateway action として実装するか:**
- gateway action = LLMがツールとして呼び出せる実行単位（execute_shell等と同様）
- どのコンテキスト（Discordメッセージ返信中・ハートビート処理中・自律タスク中）でも呼べる
- LLMが「長い処理かどうか」を文脈から判断して自発的に spawn できる

**トリガーのバリエーション:**
| トリガー | 説明 |
|---------|------|
| Discordメッセージ | 「画像生成して」→ spawn_subtask → ack返信 |
| ハートビート | 定期処理中に「メール確認が重い → サブに投げよう」と判断 |
| 自律タスク | スケジュールされたタスクから別タスクをサブに委譲 |

**メリット:**
- LLMの文脈判断でバックグラウンド委譲を制御できる（機械的ルールより柔軟）
- message_loop.rs の複雑な分岐が不要になる（常に同じ流れ）
- ハートビートや自律タスクからもサブを起動できる
- depth制限（3.7節）で再帰防止も保証される

**デメリット・リスク:**
- LLMが spawn_subtask を呼ぶかどうかは確率的（プロンプト設計が重要）
- 短い処理でも spawn_subtask を呼んでしまう可能性
- ack を返すかどうかもLLM任せ（固定文言よりバラつく可能性）

**難易度:** 中〜高（gateway action の実装 + depth制限 + セッション履歴連携）

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
// Step 1: エンジン第1ターンのテキストを即座に送信（一次応答）
// generate_ack_message不要: LLMが自然にackテキストを生成する
// engine.rs の immediate_text_callback が第1ターンのテキストをDiscordに直接送信

// Step 2: 長い処理をバックグラウンドで実行（フロー変更）
let bg_state = state.clone();
let bg_session_id = session_id.clone();
let bg_channel_id = channel_id;
let bg_gateway = gateway.clone();

tokio::spawn(async move {
    // [サブタスクもフルSkillEngineで動かす]
    // エージェントと同じ人格・スキル構成のエンジンで処理
    // ただし出力はDiscordに送らず、セッション履歴にのみ追加
    let sub_result = bg_state.run_agent_response(
        agent_id,
        agent_name,          // エージェントと同じ人格
        bg_session_id.clone(),
        system_prompt,       // エージェントと同じシステムプロンプト
        sub_conversation,    // サブタスク用の会話（元メッセージ + サブ指示）
        "background",        // channel="background"（Discord送信なし）
        None,                // gateway_actions=None（Discordには送らない）
        caller,
        &image_urls,
    ).await;

    // [新フロー] 結果をセッション履歴に追加（Discordには直接送信しない）
    bg_state.append_to_session_history(
        &bg_session_id,
        SessionMessage {
            message_type: "subtask_result".to_string(),
            result: sub_result
                .map(|r| r.response)
                .unwrap_or_else(|e| format!("エラー: {e}")),
        }
    ).await;

    // [新フロー] メインエンジンを再度呼び出して最終応答を生成
    let final_result = bg_state.run_agent_response(
        agent_id,
        agent_name,
        bg_session_id.clone(),
        system_prompt,
        conversation_with_subtask_result,  // subtask_resultを含む履歴
        "discord",
        Some(gateway_actions),
        caller,
        &image_urls,
    ).await;

    // [新フロー] 最終応答をDiscordに送信
    match final_result {
        Ok(engine_result) if !engine_result.response.is_empty() => {
            bg_gateway.send_to_channel(bg_channel_id, &engine_result.response).await?;
        }
        Ok(_) => {}
        Err(e) => {
            bg_gateway.send_to_channel(bg_channel_id, "エラーが発生しました").await?;
        }
    }
});
```

#### `crates/server/src/process.rs`（変更なし、または軽微）

`run_agent_response` 自体は変更不要。
ただし `AppState` が `Clone` を実装していることを確認・対応する必要がある。

---

### 3.2 一次応答の設計（確定: LLMの第1ターン活用方式）

> **[2026-03-22 設計変更]** 旧設計（固定文言ハードコード・軽量LLM別途呼び出し）は廃止。
> LLMの最初のターンの動作に乗っかる方式に確定。

#### フロー

```
メッセージ受信
  ↓
メインエンジンで普通にLLM呼び出し（第1ターン）
  ↓
LLMのレスポンスを確認：
  ├─ テキスト応答のみ → 即Discordに送信（これが一次応答）、ツールなければ終了
  ├─ テキスト + tool_calls → テキストを即Discordに送信、tool_callsをバックグラウンドで処理
  └─ tool_callsのみ → バックグラウンドに切り替え（エンジン続行）
```

#### メリット

- **ハードコードなし**: 固定文言を埋め込む必要がない
- **追加コストなし**: 軽量LLM別途呼び出し不要
- **LLMが自然にackを生成**: 「ちょっと待ってね（ツール呼び出し）」という動作がLLM判断で実現
- **Anthropic API対応**: テキストとtool_callを同一ターンで返せる仕様を自然に活用

#### 実装変更箇所

**`crates/core/src/engine.rs` — `run_with_model_override` ループへの変更:**

```rust
loop {
    iterations += 1;
    let response = self.llm.chat(request).await?;

    // ★ 第1ターン: テキストが含まれる場合は即座にコールバック
    if iterations == 1 {
        if let Some(text) = &response.text {
            if !text.is_empty() {
                // コールバック経由で即Discord送信（一次応答）
                self.immediate_text_callback.call(text).await;
            }
        }
    }

    if !response.tool_calls.is_empty() {
        // ★ 第1ターン + テキストあり → バックグラウンドへの切り替えシグナル
        if iterations == 1 && response.text.is_some() {
            // テキストは送信済み、tool_calls処理をバックグラウンドに委譲
            // message_loop.rsがこのシグナルを受けてバックグラウンド化
        }
        // ツール実行
        for tool_call in &response.tool_calls {
            let result = self.executor.execute(...).await;
            messages.push(tool result);
        }
        continue;
    }

    // ツールコールなし → 最終応答
    return Ok(EngineResult { response: final_text, ... });
}
```

**`message_loop.rs` — バックグラウンド委譲のトリガー受け取り:**

エンジンから「テキスト送信済み + tool_calls継続中」のシグナルを受け取り、
処理をバックグラウンドに委譲するだけ。複雑な一次応答生成ロジックは不要。

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
変更後: 3段階でセッション履歴に記録

```rust
// [1] ack ログ（変更なし）
SessionLogRow {
    log_type: "speech",
    content: ack_msg,
    metadata_json: Some(json!({
        "source": "discord_ack",
        "channel_id": channel_id_str,
        "is_ack": true,
    }).to_string()),
}

// [2] subtask_result ログ（新追加）
// バックグラウンド処理完了後、結果をセッション履歴に追加
// LLMが参照できるよう conversation に組み込む
SessionLogRow {
    log_type: "subtask_result",
    content: bg_result.output,  // シェル実行結果等
    metadata_json: Some(json!({
        "source": "background_task",
        "task_type": "execute_shell",  // または画像生成等
        "channel_id": channel_id_str,
    }).to_string()),
}

// [3] 最終応答ログ（変更なし）
// メインエンジン再呼び出し後の最終応答
SessionLogRow {
    log_type: "speech",
    content: engine_result.response,
    metadata_json: Some(json!({
        "source": "discord_response_final",  // "discord_response" から変更
        "channel_id": channel_id_str,
        "tool_calls_made": engine_result.tool_calls_made,
        "preceded_by_subtask": true,  // サブタスク経由であることを記録
    }).to_string()),
}
```

**なぜこの方式か:**
- バックグラウンド処理結果をそのままDiscordに出すのではなく、
  LLMがエージェントとして解釈・整形してから送信できる
- セッション履歴に subtask_result を挟むことで、LLMが
  「バックグラウンドで何が起きたか」を把握できる
- キャラクター一貫性（エージェントの口調）をLLMが担保できる

**注意:** バックグラウンドタスクが DB を参照する際、`state.db` の `Arc<Mutex<Connection>>` を
スレッドをまたいで使用する。既存の実装と同様のパターンで問題ない（既に `tokio::spawn` での
バックグラウンド DB 操作は `process.rs` で行われている）。

---

### 3.6 サブタスクのエンジン構成

サブタスク（バックグラウンド処理）は**エージェントと同じ人格・スキルを持つフルSkillEngineインスタンス**として動かす。

#### なぜフルエンジンか

- **人格一貫性:** サブタスクがただのシェル実行器では、複雑な判断（どのコマンドを使うか、エラーをどう解釈するか）をLLMに任せられない
- **スキル活用:** `execute_shell`・`read_file`・`web_fetch` 等のスキルをサブタスク側でも使える
- **エラーハンドリング:** サブタスクのLLMが自律的にリトライ・代替手段を試みられる
- **一貫したアーキテクチャ:** `run_agent_response` を再利用するだけ（新たな実行器を実装不要）

#### サブタスクとメインタスクの違い

| 項目 | メインタスク | サブタスク |
|------|-------------|-----------|
| SkillEngine構成 | LLM + スキル + パーソナリティ | **同じ**（LLM + スキル + パーソナリティ） |
| 人格（システムプロンプト） | エージェント | **エージェントと同じ** |
| Discord送信 | あり（最終応答） | **なし** |
| channel引数 | `"discord"` | `"background"`（Discord送信をスキップ） |
| gateway_actions | Some(...) | **None**（Discord操作不可） |
| 出力先 | Discordチャンネル | セッション履歴（subtask_result） |
| 目的 | ユーザーへの応答生成 | 処理の実行と結果の返却 |

#### channel="background" の実装

`message_loop.rs` で channel が `"background"` の場合はDiscord送信をスキップする:

```rust
// process.rs または message_loop.rs 内
if channel != "background" {
    gateway.send_to_channel(channel_id, &response).await?;
}
```

または、`gateway_actions = None` の場合は送信処理を行わない（既存の分岐を活用）。

#### サブタスク用の会話構成

サブタスクに渡す `sub_conversation` は元のユーザーメッセージに加えて、
「これはバックグラウンドタスクです。結果をそのまま返してください（Discordには送信しません）」
という指示を付加する:

```rust
let mut sub_conversation = conversation.clone();
sub_conversation.push(Message {
    role: "system",
    content: "これはバックグラウンド処理タスクです。\
              処理を実行して結果を返してください。\
              Discordには直接送信しません。エージェントとしての人格で判断してください。",
});
```

---

### 3.7 depth制限と再帰ループ防止

サブエンジンが再びサブエンジンを spawn すると無限ループに陥る危険がある。
これを防ぐため、エンジンの呼び出し深さ（depth）を制限する。

#### depth の定義

| depth | 意味 | spawn_subtask |
|-------|------|---------------|
| 0 | メインエンジン（Discord メッセージから直接起動） | 使用可 |
| 1 | サブエンジン（tokio::spawn 内で起動） | **使用不可** |
| 2以上 | 将来の多段サブタスク（現時点では到達しない） | **使用不可** |

#### `EngineContext` への depth フィールド追加

`run_agent_response` に渡すコンテキスト構造体（または引数）に `depth` を追加する:

```rust
// crates/server/src/process.rs または crates/core/src/engine.rs

pub struct EngineContext {
    pub agent_id: i64,
    pub agent_name: String,
    pub session_id: String,
    pub system_prompt: String,
    pub depth: u8,          // 0: メイン, 1: サブ, 2以上: 使用不可
    // ... 既存フィールド
}
```

または `run_agent_response` のシグネチャに直接 `depth: u8` を追加:

```rust
pub async fn run_agent_response(
    // ... 既存引数
    depth: u8,   // 0: メイン, 1: サブ
) -> Result<EngineResult, ...> {
    // ...
}
```

#### `spawn_subtask` ツールの depth チェック

`spawn_subtask` ツール（案D / 将来実装）の実行時に depth を検査する:

```rust
// crates/actions/src/spawn_subtask.rs（将来実装）

pub async fn execute_spawn_subtask(ctx: &EngineContext, ...) -> ToolResult {
    if ctx.depth >= 1 {
        return ToolResult::error(
            "サブエンジン内から spawn_subtask は使用できません（再帰防止）"
        );
    }
    // ... 通常の spawn 処理
}
```

#### メインエンジンからサブエンジンを呼ぶ際の depth 伝播

```rust
// message_loop.rs の tokio::spawn 内

// サブエンジンは depth=1 で呼び出す
let sub_result = bg_state.run_agent_response(
    agent_id,
    agent_name,
    bg_session_id.clone(),
    system_prompt,
    sub_conversation,
    "background",
    None,   // gateway_actions なし
    caller,
    &image_urls,
    1,      // depth = 1（サブエンジン）
).await;

// メインエンジンの最終呼び出しは depth=0 のまま
let final_result = bg_state.run_agent_response(
    // ...
    0,      // depth = 0（メインエンジン）
).await;
```

#### `AppState` への depth フィールド（代替案）

`EngineContext` ではなく `AppState` に持たせる場合（リクエストスコープで管理）:

```rust
pub struct AppState {
    // ... 既存フィールド
    // depth は AppState には持たせない（状態を共有するため不適切）
    // → run_agent_response の引数として渡す方式を推奨
}
```

> **方針:** `depth` は **`run_agent_response` の引数または `EngineContext` フィールド** として渡す。
> `AppState` はすべてのリクエストで共有されるため、depth のような呼び出しスコープの情報には不向き。

#### SkillEngine への depth 伝播

`SkillEngine` がツール実行時に depth を参照できるよう、executor にも伝播させる:

```rust
// engine.rs
impl SkillEngine {
    pub fn new(llm, executor, max_iterations, depth: u8) -> Self {
        Self { llm, executor, max_iterations, depth }
    }
}

// executor 側（BridgedExecutor）で spawn_subtask を呼ぶ前に depth チェック
impl ActionExecutor for BridgedExecutor {
    async fn execute(&self, tool_name: &str, args: Value) -> ToolResult {
        if tool_name == "spawn_subtask" && self.depth >= 1 {
            return ToolResult::error("再帰的なサブタスク呼び出しは禁止されています");
        }
        // ... 通常の実行
    }
}
```

#### 実装上の注意

- depth の上限チェックは `spawn_subtask` ツール実行時だけで十分（他ツールは影響なし）
- 将来的に depth=2（サブのサブ）を許容する場合は上限値を定数化しておく
  ```rust
  const MAX_SUBTASK_DEPTH: u8 = 1;
  ```
- depth はログにも記録しておくと、デバッグ時にどのエンジンが発火したか追跡しやすい

---

### 3.8 `spawn_subtask` gateway action の実装

`spawn_subtask` はLLMがツールとして呼び出せる gateway action として実装する。

#### ツール定義（LLMに見せるスキーマ）

```json
{
  "name": "spawn_subtask",
  "description": "長い処理や並列実行が必要なタスクをバックグラウンドのサブエンジンに委譲する。サブエンジンはエージェントと同じ人格・スキル構成で動く。結果はセッション履歴に追加され、その後メインエンジンが呼ばれて最終応答を生成する。depth=0（メインエンジン）でのみ使用可能。",
  "parameters": {
    "type": "object",
    "properties": {
      "task": {
        "type": "string",
        "description": "サブエンジンに渡すタスク指示。何をすべきかを明確に記述する。"
      },
      "reason": {
        "type": "string",
        "description": "なぜサブタスクに委譲するか（ログ・デバッグ用）"
      }
    },
    "required": ["task"]
  }
}
```

#### Rust 実装（`crates/actions/src/spawn_subtask.rs`）

```rust
pub struct SpawnSubtaskAction {
    pub state: Arc<AppState>,
    pub depth: u8,
    pub session_id: String,
    pub agent_id: i64,
    pub agent_name: String,
    pub system_prompt: String,
    pub gateway: Arc<dyn GatewayActions>,
    pub channel_id: u64,
    pub caller: Option<String>,
}

impl GatewayAction for SpawnSubtaskAction {
    async fn execute(&self, args: Value) -> ToolResult {
        // depth チェック（3.7節の再帰防止）
        if self.depth >= 1 {
            return ToolResult::error(
                "サブエンジン内から spawn_subtask は使用できません（再帰防止: depth >= 1）"
            );
        }

        let task = args["task"].as_str().unwrap_or("").to_string();
        let reason = args["reason"].as_str().unwrap_or("").to_string();

        let state = self.state.clone();
        let session_id = self.session_id.clone();
        let agent_id = self.agent_id;
        let agent_name = self.agent_name.clone();
        let system_prompt = self.system_prompt.clone();
        let gateway = self.gateway.clone();
        let channel_id = self.channel_id;
        let caller = self.caller.clone();

        // バックグラウンドでサブエンジン起動
        tokio::spawn(async move {
            // サブタスク用会話を構成
            let sub_conversation = vec![
                Message::system("これはバックグラウンドサブタスクです。エージェントとして処理し、結果を返してください。Discordには直接送信しません。"),
                Message::user(&task),
            ];

            // サブエンジン実行（depth=1, gateway_actions=None）
            let sub_result = state.run_agent_response(
                agent_id, &agent_name, &session_id,
                &system_prompt, sub_conversation,
                "background", None, caller.as_deref(), &[],
                1, // depth = 1
            ).await;

            // 結果をセッション履歴に追加
            state.append_to_session_history(&session_id, SessionMessage {
                message_type: "subtask_result".to_string(),
                result: sub_result
                    .map(|r| r.response)
                    .unwrap_or_else(|e| format!("エラー: {e}")),
                metadata: json!({ "task": task, "reason": reason }),
            }).await;

            // メインエンジンを再呼び出し（最終応答生成）
            let final_result = state.run_agent_response(
                agent_id, &agent_name, &session_id,
                &system_prompt,
                vec![], // セッション履歴から自動ロード
                "discord", Some(gateway.clone()), caller.as_deref(), &[],
                0, // depth = 0（メインエンジン）
            ).await;

            if let Ok(engine_result) = final_result {
                if !engine_result.response.is_empty() {
                    let _ = gateway.send_to_channel(channel_id, &engine_result.response).await;
                }
            }
        });

        // spawn直後にすぐ返す（LLMはこの後ackテキストを返せる）
        ToolResult::ok(json!({
            "status": "spawned",
            "message": "サブタスクをバックグラウンドで起動しました"
        }))
    }
}
```

#### LLMの典型的な呼び出しパターン

ユーザー: 「データを分析してレポートを作って」

```
LLM のターン:
1. spawn_subtask を呼ぶ
   → args: { "task": "指定されたデータを分析してレポートを作成する", "reason": "分析処理は時間がかかるためサブに委譲" }
   → ToolResult: { "status": "spawned" }
2. テキストで ack を返す
   → "少し待ってて、バックグラウンドで処理してるね ⚡"
3. メインエンジン終了 → Discord に ack 送信

[バックグラウンド]
サブエンジン: 指定されたツールで処理を実行
   → セッション履歴に subtask_result 追加
   → メインエンジン再呼び出し
   → "処理が完了したよ！ [結果]" を Discord 送信
```

#### ハートビートからのサブタスク起動例

```
ハートビートエンジン（depth=0）:
→ 「メールを確認してください」という自律タスクを処理中
→ LLMが「メール確認は時間がかかる → spawn_subtask を使おう」と判断
→ spawn_subtask 呼び出し → サブエンジン（depth=1）でメール確認
→ 結果をセッション履歴に追加 → メインエンジン再呼び出し → まとめてDiscordに通知
```

---

## 4. 実装ステップ（優先順位付き）

### Phase概要（更新版）

| Phase | 内容 | 優先度 |
|-------|------|-------|
| Phase 1（最小実装） | 別セッション生成、steer（sessions_send）、depth制限（MAX_DEPTH=2）、Discord系アクションブロック、タイムアウト、ログトレース、**サブのRAGアクセス（記憶検索）** | ⭐最優先 |
| Phase 2 | `ask_parent`実装、出力トランケーション、一次応答LLM生成 | 次フェーズ |
| Phase 3 | co-agent呼び出し、相互評価システム、スマートなspawn_subtask判断 | 将来 |

### Phase 1: 最小実装（2〜4時間）⭐最優先

**目標:** もぐたろう状態の解消（一次応答を即送信する）

> **方式:** LLMの第1ターンのテキスト応答を即Discord送信（ハードコードなし）。engine.rsのiterations==1時にコールバック経由で送信。

1. **`engine.rs` の `run_with_model_override` への変更**
   - `iterations == 1` かつLLMレスポンスにテキストが含まれる場合、コールバックで即Discord送信
   - テキスト + tool_calls の場合: テキスト送信後、tool_calls処理をバックグラウンドに委譲
   - tool_calls のみの場合: そのままバックグラウンドに切り替え

2. **`message_loop.rs` の改修**
   - エンジンのバックグラウンド委譲シグナルを受け取る仕組みを実装
   - `generate_ack_message` 関数は不要（削除）
   - `tokio::spawn` でバックグラウンド処理を継続

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

8. **depth制限の実装**
   - `run_agent_response` に `depth: u8` 引数を追加、サブ呼び出し時は `depth=1` を渡す
   - `spawn_subtask` ツール内で `depth >= 1` チェックを追加

9. **spawn_subtask gateway action の実装**: `crates/actions/src/spawn_subtask.rs` を新規作成。depth チェック、tokio::spawn、セッション履歴追加、メインエンジン再呼び出しの一連のフローを実装

### Phase 3: 高度機能（将来）

9. **案D（サブタスクツール）の実装**
   - `spawn_background_task` ツールを `crates/actions` に追加
   - LLM が自律的に長い処理を委譲できるようにする

10. **タイピングインジケーターの継続**
   - バックグラウンドタスク実行中、定期的に `start_typing` を再送信
   - Discord の 10 秒タイムアウトに対応

11. **進捗通知の仕組み**
    - ツールコール完了ごとに絵文字リアクション等で進捗を示す
    - 案C の軽量版として実装

---

## 5. レビューポイント

- [x] ✅ **確定** **一次応答の内容**: LLMの第1ターンのテキストをそのまま使用。固定文言ハードコードなし・軽量LLM別途呼び出しなし。Anthropic APIの「テキスト+tool_calls同時返却」を自然に活用。
- [x] ✅ **確定** **バックグラウンド→Discord方式**: 直接送信 **なし**。subtask_result → セッション履歴 → メインエンジン再呼び出し → 最終送信の流れで統一
- [ ] **未定** **並行タスクの上限**: 同一チャンネルで同時実行を許すか、直列化するか（Phase 2で決定）
- [ ] **未定** **バックグラウンドタスクの永続化**: プロセス再起動時に実行中タスクが消える（許容するか）
- [ ] **未定** **ack を送るかどうかの判断**: 常に送る vs 長そうな時だけ送る（誤検知の問題）
- [x] ✅ **確定** **一次応答のキャラクター**: ackはシンプルな固定文言。最終応答のエージェントらしさはLLMが担保
- [x] ✅ **確定** **サブタスクのエンジン構成**: フルSkillEngine（エージェントと同じ人格・スキル）を使用。単純なシェル実行器ではない
- [x] ✅ **確定** **再帰防止**: depth制限を採用（depth: 0=メイン、1=サブ）。depth >= 1 では spawn_subtask を使用不可
- [ ] **未定** **spawn_subtask のプロンプト設計**: LLMが適切に spawn_subtask を呼ぶようシステムプロンプトをどう書くか（過剰呼び出し防止）
- [x] ✅ **確定** **spawn_subtask の実装位置**: gateway action として `crates/actions/src/spawn_subtask.rs` に実装

---

## 6. 関連ファイル

| ファイル | 関連箇所 |
|---------|---------|
| `crates/discord/src/message_loop.rs` | メイン変更箇所: `run_discord_loop` |
| `crates/server/src/process.rs` | `run_agent_response`（変更なし or 軽微）、max_iterations=20 |
| `crates/core/src/engine.rs` | `run_with_model_override`（変更なし）、EngineResult.stopped_by_limit |
| `crates/server/src/lib.rs` | `AppState` の Clone 対応確認 |
| `crates/actions/src/` | 案D の場合: `spawn_background_task` ツール追加 |
| `crates/core/src/engine.rs` | `SkillEngine::new` に `depth` 引数追加、BridgedExecutor に depth 伝播 |
| `crates/actions/src/spawn_subtask.rs` | spawn_subtask gateway action（新規実装: depth チェック + tokio::spawn + セッション履歴連携）|

---

## 7. サブエンジンのアーキテクチャ: 人格・スキルを持つSkillEngine

### 7.1 サブタスクは単純なシェル実行器ではない

**重要な設計判断:** バックグラウンドタスク（サブエンジン）は、
メインエンジンと **同じ `run_agent_response` の仕組み** で動く。

```
メインエンジン (depth=0)
  ├─ LLM: claude-sonnet (フルコンテキスト)
  ├─ スキル: 全スキル有効
  ├─ パーソナリティ: エージェントのシステムプロンプト（長め）
  └─ ループ: max_iterations=20

サブエンジン (depth=1)
  ├─ LLM: 同じモデル or 軽量モデル（設定可）
  ├─ スキル: 同じスキルセット（spawn_subtaskのみ無効化）
  ├─ パーソナリティ: タスク指向の短いシステムプロンプト
  └─ ループ: max_iterations=20（または専用上限）
```

サブエンジンも `SkillEngine::new(llm, executor, max_iterations)` で生成される完全なエージェント。
「シェルコマンドを実行して終わり」ではなく、必要なら複数のツールを組み合わせて自律的に問題を解く。

### 7.2 サブ用システムプロンプト

メインとサブでシステムプロンプトの目的が異なる:

| 項目 | メインエンジン | サブエンジン |
|------|--------------|-------------|
| 目的 | キャラクターとして会話・応答 | タスクを遂行して結果を返す |
| 長さ | 長め（人格・記憶・文脈） | 短め（タスク指向） |
| キャラクター | エージェントのフルパーソナリティ | 最小限（タスク完遂が優先） |
| 出力形式 | Discordメッセージ（自然な文章） | 構造化された結果（後でLLMが解釈） |

**サブ用システムプロンプトのテンプレート（案）:**
```
あなたはバックグラウンドタスクエージェントです。
以下のタスクを遂行し、結果を簡潔に返してください。
結果はメインエージェントがユーザーに伝えます。

タスク: {task_description}
```

### 7.3 Agentic RAG パターン

サブエンジンがスキルを持つことで、より高度な自律タスクが実現できる:

```
ユーザー: 「先月の会話から画像生成に関する話を全部まとめて」

[メインエンジン (depth=0)]
  ↓ spawn_subtask("セッション履歴から画像生成に関する会話を検索・抽出")

[サブエンジン (depth=1)]
  ↓ memory_search_skill("画像生成", date_range="last_month")  ← スキル使用
  ↓ memory_search_skill("image generation", ...)             ← 複数クエリ
  ↓ 結果をまとめて subtask_result として返す

[メインエンジン (depth=0)]
  ↓ subtask_result を受け取り、エージェントとして解釈・整形
  ↓ Discordに最終応答
```

このパターンは **Agentic RAG** と呼ばれ、サブエージェントが能動的に情報を掘り出し、
メインエージェントが豊富な文脈で応答する設計。

> **確定:** サブも最初から（Phase 1から）記憶検索スキルを自由に使える。
> 全セッション履歴はコンテキスト爆発を防ぐため渡さないが、
> RAGスキルで必要な情報を能動的に取得できる。この設計はPhase 1で有効とする。

---

## 8. depth制限: 再帰無限ループの防止

### 8.1 問題: スポーンの連鎖

サブエンジンも `spawn_subtask` ツールを使えると、以下の無限ループが発生しうる:

```
メイン → サブ1 → サブ2 → サブ3 → ... → 無限
```

### 8.2 解決策: depth パラメータ

`SkillEngine` または `run_agent_response` に `depth: usize` を追加し、
depth >= 1 の場合は `spawn_subtask` ツールをツールリストから除外する。

```rust
// crates/server/src/process.rs
pub async fn run_agent_response(
    // ... 既存パラメータ ...
    depth: usize,  // 追加: 0=メイン, 1=サブ, 将来的に2以上も
) -> Result<EngineResult, Error> {

    // depth に応じてツールリストをフィルタリング
    let executor = BridgedExecutor::new(
        actions,
        depth,  // depth を渡す
    );

    let engine = SkillEngine::new(llm, executor, 20);
    engine.run_with_model_override(...).await
}
```

```rust
// crates/server/src/bridge.rs（またはactions側）
impl BridgedExecutor {
    pub fn new(actions: Vec<Box<dyn Action>>, depth: usize) -> Self {
        let filtered_actions = if depth >= 1 {
            // depth >= 1 では spawn_subtask を除外
            actions.into_iter()
                .filter(|a| a.name() != "spawn_subtask")
                .collect()
        } else {
            actions
        };

        BridgedExecutor { actions: filtered_actions }
    }
}
```

### 8.3 depth の意味

| depth | 説明 | spawn_subtask |
|-------|------|--------------|
| 0 | メインエンジン（Discordメッセージに直接応答） | ✅ 使用可 |
| 1 | サブエンジン（バックグラウンドタスク） | ❌ 除外 |
| 2以上 | 将来的な多段サブ（現時点では未使用） | ❌ 除外 |

**設計判断:** depth=1 以上では一律 `spawn_subtask` を除外する。
これにより再帰の深さを 1 レベルに限定でき、リソース枯渇を防げる。

### 8.4 関連する変更ファイル

| ファイル | 変更内容 |
|---------|---------|
| `crates/server/src/process.rs` | `run_agent_response` に `depth: usize` 引数追加 |
| `crates/server/src/bridge.rs` | `BridgedExecutor::new` で depth>=1 時にフィルタリング |
| `crates/discord/src/message_loop.rs` | メイン呼び出し時は `depth=0`、バックグラウンド時は `depth=1` |

---

## 9. LLMが自発的にspawn_subtaskを呼べる設計（案A+D統合）

### 9.1 背景: 案Aと案Dの融合

[セクション2の案A・案D]を組み合わせた **最終設計**:

- **案A** のバックグラウンド化フローを基盤とする
- **案D** の「LLMが自分でspawnを判断できる」設計を `spawn_subtask` ゲートウェイアクションとして実装

```
[従来の案A]
message_loop.rs が強制的にバックグラウンド化
  → LLMに委譲の判断権がない

[案A+D統合]
spawn_subtask ツールをメインエンジン(depth=0)に提供
  → LLMが自分でいつでも spawn_subtask を呼べる
  → メッセージ受信以外のトリガー（ハートビート・定期タスク）でも動く
```

### 9.2 spawn_subtask の実装方針

`spawn_subtask` を **gateway action** として実装する:

```rust
// crates/actions/src/spawn_subtask.rs（新規）
pub struct SpawnSubtaskAction {
    app_state: Arc<AppState>,
    channel_id: u64,
    depth: usize,  // 呼び出し元のdepth（depth=0のみ使用可）
}

impl Action for SpawnSubtaskAction {
    fn name(&self) -> &str { "spawn_subtask" }

    fn description(&self) -> &str {
        "時間のかかるタスクをバックグラウンドで実行します。
        画像生成・大量検索・シェルコマンド等、10秒以上かかりそうな処理に使用してください。"
    }

    async fn execute(&self, params: Value) -> Result<String, Error> {
        let task_description = params["task"].as_str()?;

        tokio::spawn(async move {
            // サブエンジン（depth=1）でタスク実行
            let result = self.app_state.run_agent_response(
                agent_id, session_id,
                subtask_system_prompt,
                vec![Message::user(task_description)],
                depth: 1,  // サブエンジン
            ).await;

            // 結果をセッション履歴に追加
            self.app_state.append_subtask_result(session_id, result).await;

            // メインエンジンを再度トリガー（コールバック）
            self.app_state.trigger_main_engine_callback(
                channel_id, session_id
            ).await;
        });

        // 即座に「バックグラウンドで開始しました」を返す
        Ok("バックグラウンドタスクを開始しました。完了後に結果をお知らせします。".to_string())
    }
}
```

### 9.3 フロー例: 画像生成リクエスト

```
ユーザー: 「データを処理してほしい」

[メインエンジン (depth=0)]
LLM判断: 「時間かかりそう → spawn_subtask を使おう」
  ↓ ack メッセージを Discord に返す: 「了解！バックグラウンドで処理するね ⚡」
  ↓ spawn_subtask({"task": "指定されたデータを処理して結果を返す"}) を呼ぶ
  ↓ 「バックグラウンドで開始しました」を受け取り、ループ終了

[サブエンジン (depth=1), バックグラウンド]
  ↓ 画像生成ツール実行（数十秒）
  ↓ 結果（画像URL等）を subtask_result としてセッション履歴に追加
  ↓ メインエンジン再トリガー

[メインエンジン (depth=0), 再呼び出し]
  ↓ subtask_result（画像URL）を受け取り
  ↓ Discordに最終応答: 「処理できたよ！[結果] ⚡」
```

### 9.4 メッセージ以外のトリガー

`spawn_subtask` が gateway action として独立することで、
**ユーザーメッセージ以外のトリガーでもサブを起動できる**:

```
[ハートビートトリガー]
定期実行 → メインエンジン起動（ユーザーメッセージなし）
  ↓ LLMが spawn_subtask("メールチェック")、spawn_subtask("カレンダー確認") を呼ぶ
  ↓ 各サブエンジンが並行実行
  ↓ 全完了後、メインエンジンが集約してまとめ報告

[自律タスクトリガー]
Cronジョブ → gateway.trigger_agent(task="朝の定例チェック")
  ↓ メインエンジン起動 → spawn_subtask で各チェックを委譲
  ↓ 結果集約 → Discordに送信
```

### 9.5 案Aとの違いと統合方針

| 観点 | 案A（既存設計） | 案A+D統合（本設計） |
|------|--------------|-------------------|
| spawn判断 | `message_loop.rs` が強制的に判断 | LLMが文脈に応じて自律判断 |
| トリガー | Discordメッセージのみ | メッセージ・ハートビート・Cron等 |
| 実装箇所 | `message_loop.rs` | `spawn_subtask` gateway action |
| 柔軟性 | 低（固定ルール） | 高（LLMが判断） |
| 実装難易度 | 低 | 中 |

**実装優先度:**
- Phase 1（最小実装）: 案Aの強制バックグラウンド化を先に実装
- Phase 3（高度機能）: `spawn_subtask` action を追加して案A+D統合へ移行

---

## 10. 確定済み設計事項（追加）

> 2026-03-22 確定済み設計事項を追記

### 10.1 サブは別セッション

サブタスク起動時に新しいセッションIDを生成する。

**metadata_json の記録:**
- サブセッション側: `parent_session_id`, `depth`
- メインセッション側: `sub_session_id`, `depth`

**クロスセッション参照:**
```json
{
  "cross_session_ref": {
    "session_id": "<session_id>",
    "message_id": "<message_id>"
  }
}
```
`cross_session_ref: {session_id, message_id}` でメッセージ単位のトレースも可能。
ダッシュボードから親子関係を辿れる設計とする。

### 10.2 steer = サブセッションへのsessions_send

メインからサブセッションにメッセージを送ることでsteer（軌道修正）を実現する。

**管理構造:**
```rust
DashMap<subtask_id, (JoinHandle, session_id)>
```

サブタスクIDをキーに、JoinHandle（tokioタスク）とセッションIDのペアを管理する。

**将来実装:** サブがmpscチャンネルを持ちsteerをリアルタイム受信できるようにする（Phase 2以降）。

### 10.3 depth上限: MAX_DEPTH=2（3段階: 0→1→2）

```rust
const MAX_DEPTH: u8 = 2;
```

| depth | 意味 | spawn_subtask |
|-------|------|---------------|
| 0 | メインエンジン（Discordメッセージに直接応答） | ✅ 使用可 |
| 1 | サブエンジン（メインから呼ばれた） | ✅ 使用可（MAX_DEPTH未満） |
| 2 | サブのサブ（depth=1から呼ばれた） | ❌ 使用不可（MAX_DEPTH到達） |

depth >= MAX_DEPTH(=2) のエンジンは `spawn_subtask` ツールを使用不可。

> **注:** 既存の設計（3.7節・8節）では depth=1 以上でブロックとしていたが、
> MAX_DEPTH=2（つまり3段階）に更新する。depth=1 のサブエンジンはサブのサブを起動できる。

### 10.4 depth≥1でDiscord系アクション全ブロック

`discord_send`、`discord_send_file` など、Discordに直接送るアクションは **depth≥1で全て禁止**。

サブの出力はDiscordに流さず、セッション履歴のみに記録してメインに渡す。

```rust
impl BridgedExecutor {
    pub fn new(actions: Vec<Box<dyn Action>>, depth: u8) -> Self {
        let filtered_actions = if depth >= 1 {
            actions.into_iter()
                .filter(|a| {
                    // Discord系アクションをすべてブロック
                    !matches!(a.name(), "discord_send" | "discord_send_file" | "spawn_subtask" /* depth>=MAX_DEPTH */)
                })
                .collect()
        } else {
            actions
        };
        BridgedExecutor { actions: filtered_actions }
    }
}
```

> depth=0 のみがDiscordに直接応答できる。サブエンジンはセッション履歴に書き込むのみ。

### 10.5 サブ→メインへの質問: ask_parent ツールコール方式

**Phase 2実装予定。Phase 1では実装しない（一方向のみ: メイン→サブへのsteerのみ）。**

`ask_parent(question: String)` をgateway actionとして実装する。

**サブのLLMコンテキストでの記録（自然なツールコール形式）:**
```
user: 「aaaを始めよ」
assistant: [tool_call: ask_parent("xxxはどういう意味ですか？")]
tool_result: 「それはyyyやで。」
assistant: 「aaaを始めます」
```

コンテキストが崩れず、ログも自然に辿れる。

**実装方針:**
- Phase 1: `ask_parent` は実装しない。一方向のみ（メイン→サブへのsteer）。
- Phase 2: `ask_parent` を gateway action として実装。サブがメインに質問し、メインがDiscord経由でユーザーに確認→回答をサブに返す。

### 10.6 ログ追跡の設計

全セッションのログに以下を記録する:

```json
{
  "depth": 0,
  "parent_session_id": null
}
```

```json
{
  "depth": 1,
  "parent_session_id": "<親セッションID>"
}
```

- `cross_session_ref` でメッセージ単位の参照も可能
- ダッシュボードでセッションツリーを可視化できる形

### 10.7 サブのコンテキスト（ログ）設計

サブには以下を渡す:
- システムプロンプト（人格・スキル）
- タスク指示
- 直近コンテキスト（全セッション履歴は渡さない）

全セッション履歴を渡すとコンテキスト爆発するため不可。

**記憶（RAG）アクセス: Phase 1から利用可能**
サブも最初からRAGアクセス（記憶検索スキル）を自由に使える。
全セッション履歴は渡さないが、記憶検索スキルを通じて必要な情報を能動的に掘り出せる。
これはAgentic RAGパターン（7.3節参照）であり、Phase 1から有効とする。

**出力トランケーション（Phase 2）:**
サブの出力（execute_shell等）が大きい場合はトランケーション（上限は設定値で）。

### 10.8 タイムアウト

サブタスクにはタイムアウト設定を設ける（configで変更可能、デフォルト300秒）。

```toml
[subtask]
timeout_seconds = 300  # デフォルト5分
```

**タイムアウト時の処理:**
1. メインに通知
2. メインがDiscordにエラーメッセージを送信
3. サブタスクのJoinHandleをabortし、子プロセスもkill（プロセスグループkill）

```rust
// タイムアウト付きspawn
tokio::spawn(async move {
    tokio::select! {
        result = run_subtask(...) => { handle_result(result).await; }
        _ = tokio::time::sleep(Duration::from_secs(timeout_seconds)) => {
            // タイムアウト処理
            notify_main_timeout(&session_id).await;
            // 子プロセスグループkill
            kill_process_group(subtask_pgid).await;
        }
    }
});
```

### 10.9 将来実装メモ（フューチャービジョン）

#### co-agent呼び出し
同一マシン上のco-agent（らぼみ、しのえもん等）をサブとして呼び出せる。

```
メインエンジン(エージェントA) → spawn_subtask → co-agent(エージェントB) として呼び出し
```

#### 相互評価システム
呼び出し側・被呼び出し側が相互に評価を記録する。
- データが蓄積すると各エージェントの得意分野が明らかになる
- 最適なエージェントへの自動委譲が可能になる

#### Agentic RAG（Phase 1から利用可能）
サブも最初から記憶検索スキルを自由に使える。記憶へのアクセスはPhase 1から有効。
サブが記憶検索スキルを使って情報を掘り出し→メインに返す設計（7.3節参照）。
将来的にco-agent呼び出しとも組み合わせることができる。

---

*2026-03-22 追記: 一次応答設計を変更（固定文言廃止 → LLM第1ターン活用方式に確定。engine.rsのiterations==1コールバック + message_loop.rsのバックグラウンド委譲）*

*作成: のすたろう, 2026-03-22*
*設計確定: 2026-03-22（サブ結果→メインフィードバック方式に変更）*
*2026-03-22 追記: サブタスクもフルSkillEngine（エージェント同等構成）で動かす設計に確定*
*2026-03-22 追記: depth制限（再帰防止）設計を追加*
*レビュー: のすたろう・らぼみ・kojira（TODO #19）*
*2026-03-22 追記: spawn_subtask gateway action + 案Dとの統合設計を追加（エージェントの自発的サブタスク起動）*
*2026-03-22 追記: 確定済み設計事項（サブ別セッション・steer・depth上限MAX_DEPTH=2・Discord系アクションブロック・ask_parent・ログ追跡・サブコンテキスト・タイムアウト・フューチャービジョン）を追加*
*2026-03-22 追記: Phase分け更新（Phase1=別セッション+steer+depth+ブロック+TO+ログ / Phase2=ask_parent+トランケーション / Phase3=co-agent）*
*2026-03-22 追記: サブのRAGアクセス（記憶検索）はPhase 1から使える設計に確定。「Phase 2でRAG追加」の記述を修正。*
