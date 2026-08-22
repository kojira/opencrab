# opencrab メッセージループ再設計書 v2

**作成日:** 2026-03-23  
**ステータス:** レビュー待ち  
**対象バグ:**
- P0: 二重送信バグ（on_first_response + final response で同一メッセージが2回Discord送信される）
- P1: 割り込み不可（LLM処理中にメインループが完全ブロックし、新着メッセージが処理されない）
- P2: subtask completion callback の DB 競合リスク

---

## 1. 概要・問題点

### 1.1 P0: 二重送信バグ

**症状:**  
Discordにメッセージを送ると、同じ内容が2回連続して送信される。特にツール呼び出し（tool_calls）が発生したケースで顕著。

**根本原因（コードトレース）:**

```
ユーザーメッセージ受信
    ↓
run_discord_loop（message_loop.rs）
    ↓
on_first_response コールバック設定（iteration=1のLLM応答時に発火）
    ↓
run_agent_response → SkillEngine.run_with_model_override
    ↓
[SkillEngine内 iteration=1]
  LLM応答: content="確認します" + tool_calls=[search()]
    ↓
  engine.rs L396付近:
    if iterations == 1 {
        if let Some(ref text) = response.content {
            if !text.is_empty() {
                // on_first_response 発火！ → Discord送信 #1
            }
        }
    }
    ↓
  tool_calls あり → ツール実行 → 次のiterationへ
    ↓
[iteration=2]
  LLM応答: content="検索結果: ..." + tool_calls=[]
  → 最終レスポンス: "検索結果: ..." を返す
    ↓
SkillEngine.run() が EngineResult{response: "検索結果: ...", tool_calls_made: 1} を返す
    ↓
message_loop.rs L384:
  let should_send = !first_sent || engine_result.tool_calls_made > 0;
  // first_sent=true かつ tool_calls_made=1 → should_send=true → Discord送信 #2
```

**P0の本質:** `on_first_response` は「中間テキスト（ツール前の思考）」を即送するための仕組みだが、`should_send` の条件が「ツールあり → 必ず再送」になっているため、最終レスポンスも重複して送られる。

### 1.2 P1: 割り込み不可

**症状:**  
LLMが処理中（特に複数ツール呼び出しで数十秒かかる場合）に別のメッセージを送っても、処理中のレスポンスが完了するまで新しいメッセージが処理されない。

**根本原因:**

```rust
// message_loop.rs（現状）
loop {
    let incoming = gateway.recv().await;  // ここでメッセージ受信
    // ...
    let result = state.run_agent_response(...).await;  // ← 完全ブロック
    // ↑ この間に来たメッセージはgatewayのキューに積まれるだけで処理されない
}
```

`run_agent_response` は複数のLLM呼び出しとツール実行を含む同期的な処理であり、完了するまで次のメッセージを受信する `gateway.recv()` が呼ばれない。

### 1.3 P2: subtask completion callback の DB 競合

**症状:**  
サブタスク完了時にtokio::spawnで新しいエージェントレスポンスを実行するが、親処理と同一セッションのDBアクセスが競合する可能性がある。

**問題箇所:**

```rust
// message_loop.rs（現状）
tokio::spawn(async move {
    // ...
    state.run_agent_response(  // ← 別タスクで同sessionにアクセス
        &agent_id,
        &session_id,  // 同じsession_id!
        // ...
    ).await
});
```

`std::sync::Mutex`でDBをロックしているが、非同期コンテキストでMutexを保持するのは問題がある（tokioのMutexを使うべき）。また、複数のサブタスクが同時に完了した場合の競合も考慮が必要。

---

## 2. 現在のアーキテクチャ

### 2.1 コンポーネント図（現状）

```
Discord Gateway
    │
    ↓ recv()（ブロッキング）
run_discord_loop()  [single async task]
    │
    ├── on_first_response コールバック作成
    │       │
    │       └── iteration=1 のテキストを即Discord送信（tokio::spawn）
    │
    ├── run_agent_response().await  ←【完全ブロック】
    │       │
    │       └── SkillEngine.run_with_model_override()
    │               │
    │               ├── iteration 1: LLM呼び出し
    │               │       └── on_first_response発火（textあれば）
    │               ├── tool実行
    │               ├── iteration 2: LLM呼び出し
    │               └── 最終レスポンス返却
    │
    ├── should_send判定（二重送信バグここ）
    │       └── first_sent=true && tool_calls_made>0 → 再送信
    │
    └── subtask completion callback（tokio::spawn）
            └── run_agent_response().await（同セッション）
```

### 2.2 on_first_response の意図

`on_first_response` は**ストリーミング的な体験**を提供するための仕組みとして実装された。LLMが思考テキストを返してからツールを呼び出す場合（例：「確認します」→ search()）に、その中間テキストを即座にDiscordへ送ることでレスポンスタイムを短く見せる効果がある。

**問題は、ツール呼び出し後の最終レスポンスをどう扱うかが曖昧なこと。**

### 2.3 SkillEngine のイテレーション構造

```
iteration 1:
  入力: [system, user]
  出力パターンA: {content: "テキスト", tool_calls: []}  → 最終レスポンス（tool_calls なし）
  出力パターンB: {content: "考え中...", tool_calls: [tool_a()]}  → on_first_response発火 + ツール実行継続
  出力パターンC: {content: null, tool_calls: [tool_a()]}  → ツール実行継続（on_first_response発火しない）

iteration 2+:
  入力: [system, user, assistant(tool_calls), tool_result, ...]
  最終的に tool_calls=[] になったら EngineResult として返却
```

---

## 3. P0 修正方針：二重送信防止

### 3.1 問題の整理

| 状況 | on_first_response | final response | 期待動作 |
|------|------------------|----------------|----------|
| ツールなし・テキストのみ | 発火しない(or 発火してもfinal=同一テキスト) | EngineResultで返る | 1回だけ送信 |
| ツールあり・iteration1にテキストあり | 発火 → 送信#1 | 異なるテキスト | 2回送信（正しい！） |
| ツールあり・iteration1にテキストあり | 発火 → 送信#1 | 同一テキスト（バグケース？） | 1回だけ送信 |
| ツールあり・iteration1にテキストなし | 発火しない | EngineResultで返る | 1回だけ送信 |

**重要な洞察:** `on_first_response` の設計意図は「中間テキストを即送信」だが、SkillEngineは現在 `EngineResult.response` に**最終テキスト**を返す。iteration=1のテキストはすでに送信済みで、最終テキストは別のもの。

よって「ツールあり && iteration1テキストあり」の場合：
- iteration1テキスト → on_first_responseで送信（正しい）
- 最終テキスト（iteration2以降）→ finalとして送信（正しい）
- **これは正常動作のはず。なぜ二重送信？**

**実際のバグケース再確認:**

```
iteration 1: content="検索します", tool_calls=[search()]
  → on_first_response("検索します") → first_sent=true, Discord送信#1

[ツール実行]

iteration 2: content="検索結果は..." tool_calls=[]
  → EngineResult{response: "検索結果は...", tool_calls_made: 1}

message_loop.rs:
  let should_send = !first_sent || tool_calls_made > 0;
  // = !true || 1>0 = false || true = true → Discord送信#2
```

もし `engine_result.response == "検索結果は..."` でon_first_responseの "検索します" と異なるなら、2回送信は**正しい**（それが意図された動作）。

**しかし実際に報告されているバグは「同じメッセージが2回送信される」ということ。**

これは以下のケースで発生する：
```
iteration 1: content="こんにちは", tool_calls=[]  ← ツールなし！
  → on_first_response("こんにちは") → first_sent=true, Discord送信#1
  → tool_calls が空 → EngineResult{response: "こんにちは", tool_calls_made: 0} を即返却

message_loop.rs:
  let should_send = !first_sent || tool_calls_made > 0;
  // = !true || 0>0 = false || false = false → 送信しない ✓
```

ん、ツールなしのケースはshouldがfalseになるのでは？

もう一度バグを確認：`should_send = !first_sent || tool_calls_made > 0`

ツールあり（1回以上）：
- first_sent=true, tool_calls_made=1 → should_send=true → **送信#2が発生**

この「2回送信」が問題なのか？iteration1テキストとfinal textが同じになるケース：
```
iteration 1: content="はい、了解です", tool_calls=[log_event()]  ← 軽量ツール
iteration 2: content="はい、了解です", tool_calls=[]  ← 同じテキスト
```

これが「二重送信バグ」の本体。

### 3.2 修正案A: final != first の場合のみ再送

**変更箇所: `message_loop.rs`**

```rust
// 修正前
let should_send = !first_sent.load(std::sync::atomic::Ordering::SeqCst)
    || engine_result.tool_calls_made > 0;

// 修正後
let first_response_text = first_response_speech.lock().ok()
    .and_then(|g| g.clone());
let should_send = if first_sent.load(std::sync::atomic::Ordering::SeqCst) {
    // on_first_responseが既に送信済み
    if engine_result.tool_calls_made > 0 {
        // ツールが使われた場合: finalテキストが異なる場合のみ送信
        first_response_text.as_deref() != Some(&engine_result.response)
    } else {
        // ツールなし: 送信しない（on_first_responseが最終レスポンスを既に送った）
        false
    }
} else {
    // on_first_responseが発火しなかった: finalを送信
    true
};
```

**利点:** 最小限の変更でP0を解決  
**欠点:** StrightForwardなロジックではなく、将来の誤解を招く可能性

### 3.3 修正案B: on_first_response の発火タイミング変更

**根本解決**: iteration=1でtool_callsがある場合、`on_first_response` を**発火させない**。on_first_responseは「これが最終レスポンスである（ツール不要）」と判断できる時のみ発火させる。

**変更箇所: `engine.rs`**

```rust
// 修正前（iteration=1かつテキストあれば無条件発火）
if iterations == 1 {
    if let Some(ref text) = response.content {
        if !text.is_empty() {
            if let Some(ref cb_lock) = self.on_first_response {
                if let Ok(mut guard) = cb_lock.lock() {
                    if let Some(cb) = guard.take() {
                        cb(text.clone());
                    }
                }
            }
        }
    }
}

// 修正後（ツールがない場合のみ発火）
if iterations == 1 && response.tool_calls.is_empty() {
    if let Some(ref text) = response.content {
        if !text.is_empty() {
            if let Some(ref cb_lock) = self.on_first_response {
                if let Ok(mut guard) = cb_lock.lock() {
                    if let Some(cb) = guard.take() {
                        cb(text.clone());
                    }
                }
            }
        }
    }
}
```

**連動して `message_loop.rs` の `should_send` も修正:**

```rust
// 修正後
// on_first_responseが送信済み → finalを送らない（on_first_responseがfinal=同一テキスト）
// on_first_responseが未送信 → finalを送る
let should_send = !first_sent.load(std::sync::atomic::Ordering::SeqCst);
```

**利点:**
- ロジックが明確：on_first_responseは「ツールなしの即レスポンス専用」
- message_loopのshould_sendがシンプルになる
- ツールあり→on_first_response発火しない→finalのみ送信→1回で正確

**欠点:**
- iteration=1にテキスト+ツールがある場合（例：「確認します → search()」）は中間テキストを送らなくなる
- ユーザー体験として若干遅く感じる可能性

### 3.4 修正案C: 2フェーズ送信の明示化（P1設計と統合）

P1の割り込み設計を実装する際に、メッセージ送信を完全に再設計する。詳細は §4.4 参照。

### 3.5 P0 推奨方針

**短期:** 修正案B（engine.rs の on_first_response 発火条件にツールチェック追加）

理由:
1. バグの根本原因（発火タイミングの曖昧さ）を解消する
2. 変更箇所が最小（engine.rs 1箇所 + message_loop.rs 1箇所）
3. P1設計後も整合性を保てる

**長期（P1実装後）:** 修正案Cで全体を再設計（§4参照）

---

## 4. P1 設計：割り込みアーキテクチャ

### 4.1 要件定義

ownerの要件：「イテレーション処理中に別メッセージが来たら割り込めるようにしたい」

**ユースケース:**
1. 重い処理（30秒かかるツール呼び出し）中に「キャンセル」と送る → 即停止
2. 誤ったコマンドを送った直後に訂正メッセージを送る → 前の処理を破棄して新しい処理
3. 緊急メッセージ（「止めて！」）が届いた → 現在の処理を中断して緊急対応

**非要件（今回のスコープ外）:**
- 複数メッセージの並列処理（後述の理由で不採用）
- ストリーミング途中での部分送信

### 4.2 アーキテクチャ選択肢

#### 選択肢A: Channel + tokio::select（推奨）

```
Discord Gateway
    │
    ↓ recv()
[メッセージ受信タスク]  ← 常時動作
    │
    ↓ sender.send(msg)
[mpsc channel]
    │
    ↓
[メッセージ処理タスク]  ← 1つのメッセージを処理中
    │
    ├── tokio::select!
    │       ├── run_agent_response(...) [CancellationToken付き]
    │       └── interrupt_rx.recv()  ← 割り込みメッセージ受信
    │               └── CancellationToken.cancel() → 処理中断
    │                       └── 割り込みメッセージを新たに処理開始
```

**実装方針:**
1. `run_discord_loop` を2つのタスクに分割
   - **受信タスク**: `gateway.recv()` → mpsc channelに送る
   - **処理タスク**: channelからメッセージを受け取り処理
2. 処理中に新メッセージが来たら `CancellationToken` でキャンセル
3. キャンセル後、新メッセージで処理を再開

#### 選択肢B: キュー方式（非推奨）

メッセージをキューに積んで順番に処理。割り込みは「次のメッセージを優先的に処理」。

**問題:** ownerの要件「処理中に割り込む」ではなく「後でまとめて処理」になる。

#### 選択肢C: spawn-per-message（非推奨）

各メッセージを独立したタスクで並列処理。

**問題:** 
- セッション状態（会話履歴）の並行アクセス競合
- LLMリクエストが複数同時に走り、コスト・レート制限問題
- 応答順序が不定になる

### 4.3 CancellationToken の伝播

`tokio_util::sync::CancellationToken` を使用してキャンセルを伝播する。

**問題:** 現在の `SkillEngine.run_with_model_override()` はキャンセル機構を持たない。

**必要な変更:**

```rust
// engine.rs に追加
pub struct SkillEngine {
    // 既存フィールド...
    cancellation_token: Option<tokio_util::sync::CancellationToken>,
}

impl SkillEngine {
    pub fn set_cancellation_token(&mut self, token: tokio_util::sync::CancellationToken) {
        self.cancellation_token = Some(token);
    }
}
```

**LLM呼び出しのキャンセル:**

```rust
// engine.rs の run_with_model_override() 内
let llm_result = if let Some(ref token) = self.cancellation_token {
    tokio::select! {
        result = self.llm.chat(request) => result,
        _ = token.cancelled() => {
            return Err(anyhow::anyhow!("cancelled"));
        }
    }
} else {
    self.llm.chat(request).await
};
```

**ツール実行のキャンセル（チェックポイント方式）:**

```rust
// engine.rs のツール実行前
if let Some(ref token) = self.cancellation_token {
    if token.is_cancelled() {
        return Err(anyhow::anyhow!("cancelled"));
    }
}
let result = self.executor.execute(&tool_call.name, &tool_call.arguments).await;
```

### 4.4 新しいメッセージループ設計

```rust
// message_loop.rs（新設計）

/// メッセージ処理コンテキスト（1つのエージェント処理の状態）
struct ProcessingContext {
    cancellation_token: CancellationToken,
    agent_id: String,
    session_id: String,
    started_at: Instant,
}

/// 2タスク構成のDiscordループ
pub async fn run_discord_loop_v2<T: AgentRunner>(
    gateway: Arc<DiscordGateway>,
    state: T,
    // ...
) {
    let (msg_tx, mut msg_rx) = mpsc::channel::<ProcessableMessage>(32);
    
    // タスク1: 受信専用（常時動作）
    let recv_task = tokio::spawn({
        let gateway = gateway.clone();
        async move {
            loop {
                match gateway.recv().await {
                    Ok(msg) => {
                        if msg_tx.send(msg.into()).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        error!("Discord recv error: {e}");
                        break;
                    }
                }
            }
        }
    });
    
    // タスク2: 処理タスク（シングル）
    let process_task = tokio::spawn(async move {
        let mut current_ctx: Option<ProcessingContext> = None;
        
        loop {
            // 現在処理中かどうかで動作を変える
            let next_msg = if current_ctx.is_some() {
                // 処理中: 割り込みメッセージを非同期的に待つ（タイムアウトなし）
                match msg_rx.try_recv() {
                    Ok(msg) => Some(msg),
                    Err(_) => None,
                }
            } else {
                // アイドル: 次のメッセージを待つ（ブロッキング）
                msg_rx.recv().await
            };
            
            if let Some(msg) = next_msg {
                // 割り込み: 現在の処理をキャンセル
                if let Some(ctx) = current_ctx.take() {
                    info!(
                        agent_id = %ctx.agent_id,
                        elapsed_ms = ctx.started_at.elapsed().as_millis(),
                        "Cancelling current processing due to interrupt"
                    );
                    ctx.cancellation_token.cancel();
                    // キャンセル完了を待つ（オプション）
                }
                
                // 新しい処理を開始
                let token = CancellationToken::new();
                let ctx = ProcessingContext {
                    cancellation_token: token.clone(),
                    // ...
                    started_at: Instant::now(),
                };
                
                // バックグラウンドで処理を開始
                tokio::spawn(process_message(msg, state.clone(), token, /* ... */));
                current_ctx = Some(ctx);
            }
            
            // TODO: 処理完了の通知を受け取る機構
        }
    });
}
```

**改善版（より実用的）:**

```rust
pub async fn run_discord_loop_v2<T: AgentRunner>(
    gateway: Arc<DiscordGateway>,
    state: T,
    agent_ids: Vec<String>,
    gateway_actions: Arc<dyn GatewayActions>,
    owner_discord_id: String,
    completion_registry: CompletionRegistry,
) {
    // チャンネルごとに独立した処理状態を管理
    // キー: channel_id, 値: CancellationToken
    let mut channel_tokens: HashMap<u64, CancellationToken> = HashMap::new();
    
    loop {
        let incoming = match gateway.recv().await {
            Ok(msg) => msg,
            Err(e) => { error!("Discord recv error: {e}"); break; }
        };
        
        let channel_id: u64 = /* extract channel_id */;
        
        // 同チャンネルの処理中タスクをキャンセル
        if let Some(old_token) = channel_tokens.remove(&channel_id) {
            if !old_token.is_cancelled() {
                info!(channel = channel_id, "Cancelling previous processing for interrupt");
                old_token.cancel();
                // Note: tokio::spawnで動いているので、cancelledを待たずに次へ進める
            }
        }
        
        // 新しいCancellationTokenで処理を開始
        let token = CancellationToken::new();
        channel_tokens.insert(channel_id, token.clone());
        
        let state = state.clone();
        let gateway = gateway.clone();
        let gateway_actions = gateway_actions.clone();
        let agent_ids = agent_ids.clone();
        
        tokio::spawn(async move {
            process_incoming_message(
                incoming,
                state,
                gateway,
                agent_ids,
                gateway_actions,
                owner_discord_id,
                completion_registry,
                token,
            ).await;
        });
    }
}
```

### 4.5 キャンセル済み処理の状態管理

**中断時の動作:**

| 状態 | 動作 |
|------|------|
| on_first_response 未発火 | 何も送らない（ユーザーは割り込みを送った） |
| on_first_response 発火済み（送信#1完了） | 処理を中断、送信済みテキストはそのまま |
| LLM呼び出し中 | LLM呼び出しをキャンセル（セレクト） |
| ツール実行中 | ツール完了後にチェックポイントでキャンセル |

**DBログの扱い:**
- ユーザーメッセージはキャンセル前に記録済み → そのまま
- エージェント応答は送信された分のみ記録

**会話履歴の整合性:**
- キャンセル時に部分的なアシスタントメッセージがDBに書き込まれていないか確認が必要
- SkillEngine は処理途中で EngineResult を返さないため、キャンセル時の会話ログは汚染されない

### 4.6 P0修正との整合性

P1設計後のP0修正：

P1実装では各メッセージを `tokio::spawn` で処理するため、on_first_responseの仕組みは維持される。ただし：

- `on_first_response` は「最初のLLMレスポンスを即送信」の意図を維持
- P0修正案B（tool_calls がある場合はon_first_response発火しない）は P1設計と整合する
- `should_send` は `!first_sent` のシンプルな条件で良くなる

**P1実装後の送信ロジック（明確化）:**

```
Case 1: ツールなし（直接回答）
  iteration 1: text="はい", tool_calls=[]
  → on_first_response発火 → Discord送信（first_sent=true）
  → EngineResult{response: "はい", tool_calls_made: 0}
  → should_send = !first_sent = false → 送信しない ✓

Case 2: ツールあり + iteration1テキストなし
  iteration 1: text=null, tool_calls=[search()]
  → on_first_response発火しない（first_sent=false）
  → [ツール実行]
  iteration 2: text="検索結果は...", tool_calls=[]
  → EngineResult{response: "検索結果は...", tool_calls_made: 1}
  → should_send = !first_sent = true → 送信 ✓

Case 3: ツールあり + iteration1テキストあり（P0バグケース、修正後）
  iteration 1: text="確認します", tool_calls=[search()]
  → on_first_response発火しない（修正案B: tool_callsあれば発火しない）
  → [ツール実行]
  iteration 2: text="検索結果は...", tool_calls=[]
  → EngineResult{response: "検索結果は...", tool_calls_made: 1}
  → should_send = !first_sent = true → 送信 ✓
  ※「確認します」テキストは送信されなくなる（トレードオフ）

Case 3': より高度な対応（オプション）
  iteration1のテキストを「タイピング中テキスト」として別途追跡し、
  最終レスポンスが異なればCase3テキストも送信する仕組みを実装
```

---

## 5. P2: subtask completion callback の競合対策

### 5.1 問題の詳細

```rust
// 現状（message_loop.rs）
let completion_cb: SubtaskCompletionFn = Arc::new(move |subtask_id, _result, exit_reason| {
    tokio::spawn(async move {
        // ↑ 複数のサブタスクが同時に完了した場合、
        //   複数のtokio::spawnが同一セッションに対してrun_agent_responseを呼ぶ可能性
        
        let conn = state.db().lock().unwrap();  // ← sync Mutex in async context!
        // ... DBアクセス
        
        state.run_agent_response(&session_id, ...).await  // 同sessionへの並行アクセス
    });
});
```

**問題点:**
1. **Sync Mutex in async context**: `state.db().lock().unwrap()` を `await` をまたいで保持する可能性
2. **同セッション並行実行**: 複数のサブタスクが同時完了→複数のエージェント応答が走る
3. **応答順序不定**: 先に完了した方が先に送信される（意図しない順序）

### 5.2 修正案: Session Lock + Queue

**セッションごとの排他ロック:**

```rust
// 新規: SessionLockRegistry
struct SessionLockRegistry {
    locks: Arc<DashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl SessionLockRegistry {
    fn get_or_create(&self, session_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.locks
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }
}
```

**使用例:**

```rust
// subtask completion callback 内
let session_lock = session_lock_registry.get_or_create(&session_id);
tokio::spawn(async move {
    let _guard = session_lock.lock().await;  // セッションロック取得（待機）
    
    // ここからは同セッションで1タスクのみ実行
    state.run_agent_response(&session_id, ...).await
});
```

### 5.3 修正案: Completion Queue（より堅牢）

複数のサブタスク完了を同一セッションでキューイングして順番に処理：

```rust
struct CompletionQueue {
    // session_id → 完了通知キュー
    queues: Arc<DashMap<String, mpsc::Sender<CompletionEvent>>>,
}

struct CompletionEvent {
    subtask_id: String,
    result: String,
    exit_reason: String,
}

// セッションごとに独立したキュー処理タスクを持つ
// 各タスクは順番にサブタスク完了を処理する
```

### 5.4 短期対応（P2）

SessionLockRegistry の導入は効果的だが、実装コストがある。

**最小限の修正:** tokio::Mutex を使ったセッションロック。既存の `DashMap` があれば流用可能。

```rust
// message_loop.rs に追加
use dashmap::DashMap;
use std::sync::Arc;

lazy_static! {
    static ref SESSION_LOCKS: Arc<DashMap<String, Arc<tokio::sync::Mutex<()>>>> = 
        Arc::new(DashMap::new());
}
```

---

## 6. 実装計画

### 6.1 フェーズ1: P0修正（即時対応可能）

**変更ファイル:**
- `crates/core/src/engine.rs`: on_first_response 発火条件に `response.tool_calls.is_empty()` を追加
- `crates/discord/src/message_loop.rs`: `should_send` を `!first_sent` に簡略化

**テスト:**
1. ツールなし → 1回だけ送信されることを確認
2. ツールあり → 1回だけ送信されることを確認（on_first_responseが発火しないため）
3. NO_REPLY → 送信されないことを確認

**工数:** 2-3時間（実装 + テスト）

### 6.2 フェーズ2: P1実装（中期）

**変更ファイル:**
- `crates/core/src/engine.rs`: `CancellationToken` フィールド追加、LLM呼び出しに tokio::select 追加
- `crates/discord/src/message_loop.rs`: チャンネルごとのトークン管理、メッセージ処理を tokio::spawn に変更

**依存関係:** `tokio-util` の `CancellationToken` feature が必要
```toml
# Cargo.toml
tokio-util = { version = "0.7", features = ["rt"] }
```

**テスト:**
1. 処理中に割り込みメッセージ → 前の処理がキャンセルされることを確認
2. キャンセル後に新処理が開始されることを確認
3. キャンセルされた処理の部分送信がないことを確認

**工数:** 1-2日（設計の複雑さによる）

### 6.3 フェーズ3: P2修正（低優先度）

**変更ファイル:**
- `crates/discord/src/message_loop.rs`: SessionLockRegistry の導入

**工数:** 4-8時間

### 6.4 フェーズ別優先度

```
優先度 HIGH  → P0修正（二重送信バグ）: ユーザー体験への直接影響
優先度 MED   → P1割り込み設計: 機能追加だが重要なUX改善
優先度 LOW   → P2競合対策: 低頻度のエッジケース
```

---

## 7. テスト方針

### 7.1 P0のテストシナリオ

| シナリオ | 送信回数 | 期待内容 |
|----------|----------|----------|
| 単純な質問（ツールなし） | 1回 | LLMの回答のみ |
| ツール呼び出しあり（中間テキストなし） | 1回 | 最終回答 |
| ツール呼び出しあり（中間テキストあり） | 1回 | 最終回答（中間テキストは送信しない） |
| NO_REPLY | 0回 | 送信なし |
| 複数ツール呼び出し | 1回 | 最終回答 |

### 7.2 P1のテストシナリオ

| シナリオ | 期待動作 |
|----------|----------|
| 処理中に同チャンネルからメッセージ | 旧処理キャンセル、新処理開始 |
| 処理中に異なるチャンネルからメッセージ | 旧処理継続、新処理並行開始 |
| LLM呼び出し中に割り込み | LLM呼び出しキャンセル |
| ツール実行中に割り込み | ツール完了後キャンセル（チェックポイント） |
| 割り込み後の応答送信 | 旧処理の中間テキストが送られないこと |

### 7.3 統合テスト（手動）

1. **基本動作確認**: 単純なメッセージ → 応答1回
2. **ツール使用確認**: ツールを使う質問 → 応答1回、内容が正確
3. **割り込み確認**: 重いタスク実行中に別メッセージ → 前の処理が止まること
4. **連続メッセージ確認**: 連続送信 → 最後のメッセージが処理されること

---

## 8. 懸念事項と未解決問題

### 8.1 on_first_response の廃止検討

P0修正案Bでは「ツールありの場合はon_first_responseを発火しない」としたが、これにより `確認します → [ツール実行] → 結果` のような自然なステップを見せる体験が失われる。

**代替案:** 将来的には「ストリーミングレスポンス」の実装（LLM生成をリアルタイムでDiscordに送る）により代替可能。現時点ではon_first_responseを削除ではなく、仕様を明確化して維持する方針。

### 8.2 P1: チャンネルを越えた割り込み

現在の設計ではチャンネルごとに独立した処理。同一Discordサーバーで複数チャンネルを処理している場合、全体のLLMリクエスト数が増える可能性がある。

**現時点での判断:** チャンネルごとの独立性を維持（シンプルで理解しやすい）。

### 8.3 キャンセルされたタスクのログ

キャンセルされた処理のLLMコール（コスト）をどうログするか。
- 案A: キャンセルされたコールもDBに記録（cancelled=trueフラグ）
- 案B: キャンセルされたコールは記録しない
- **推奨:** 案Aでログの完整性を保つ

### 8.4 tokio-util の依存追加

P1実装で `tokio-util` の `CancellationToken` が必要になる。現在の `Cargo.toml` に既に含まれているか確認が必要。

```bash
grep -r "tokio-util" /Volumes/2TB/openclaw/workspace/projects/opencrab/Cargo.toml
```

---

## 9. 設計決定サマリー

| 項目 | 決定 | 根拠 |
|------|------|------|
| P0修正方針 | 修正案B（engine.rsで発火条件変更） | 根本解決、変更最小 |
| P1アーキテクチャ | チャンネルごとのCancellationToken管理 | シンプル、P0と整合 |
| P1の割り込みスコープ | 同チャンネルのみ | 実装シンプル |
| P1のキャンセル方式 | tokio::select + CancellationToken | Rustの標準的パターン |
| P2対策 | SessionLockRegistry（tokio::Mutex） | 競合排除の最小実装 |
| on_first_response | 維持（発火条件の明確化のみ） | 将来のストリーミング実装への橋渡し |

---

## 10. コード変更箇所一覧

### P0修正（フェーズ1）

**`crates/core/src/engine.rs`**
```diff
-        // Fire on_first_response callback on the first iteration if there's text.
-        if iterations == 1 {
+        // Fire on_first_response callback on the first iteration if there's text
+        // AND no tool calls (tool calls mean this is not the final response).
+        if iterations == 1 && response.tool_calls.is_empty() {
             if let Some(ref text) = response.content {
```

**`crates/discord/src/message_loop.rs`**
```diff
-        let should_send = !first_sent.load(std::sync::atomic::Ordering::SeqCst)
-            || engine_result.tool_calls_made > 0;
+        // on_first_response is only fired for direct (no-tool) responses,
+        // so if it fired, the final response was already sent.
+        let should_send = !first_sent.load(std::sync::atomic::Ordering::SeqCst);
```

### P1実装（フェーズ2）

**`crates/core/src/engine.rs`**
- `SkillEngine` に `cancellation_token: Option<CancellationToken>` フィールド追加
- `set_cancellation_token()` メソッド追加
- `run_with_model_override()` 内の各LLM呼び出しに `tokio::select!` 追加
- ツール実行ループにキャンセルチェックポイント追加

**`crates/discord/src/message_loop.rs`**
- `channel_tokens: HashMap<u64, CancellationToken>` の状態管理追加
- 各メッセージ処理を `tokio::spawn` で独立タスク化
- 同チャンネルの古いトークンをキャンセルするロジック追加
- `process_agent_response()` 関数の切り出し

### P2修正（フェーズ3）

**`crates/discord/src/message_loop.rs`**
- `SessionLockRegistry` の導入
- `subtask completion callback` 内で session lock を取得してから `run_agent_response` を呼ぶ

---

*この設計書はレビュー後に実装フェーズへ移行する。*
*疑問点・変更提案は設計書へのコメントまたはDiscordで連絡。*
