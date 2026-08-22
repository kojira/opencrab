# opencrab メッセージループ再設計書 v3

> **更新（2026-07, #39）:** 本書に登場する `completion_registry` /
> `CompletionRegistry` / `SubtaskCompletionFn` は削除済み。サブタスクの完了・進捗
> 通知は、`DiscordGatewayActions.event_tx` から `LoopEvent::SubtaskCompleted` を
> イベントループへ直接送信する方式に置き換えられた（routing 情報は
> `parse_discord_session()` が session_id から復元する）。イベント駆動の
> アーキテクチャ自体（LoopEvent / 直列イベント処理）は本書のまま有効。

**作成日:** 2026-03-23  
**ステータス:** 設計確定（実装待ち）  
**前バージョン:** `docs/design-message-loop-v2.md`（CancellationToken方式、905行）  
**対象バグ・改善:**
- P0: 二重送信バグ（on_first_response + final response で同一メッセージが2回Discord送信される）
- P1: 割り込み不可（LLM処理中にメインループが完全ブロックし、新着メッセージが処理されない）
- P2: subtask completion callback の DB 競合リスク

---

## v2からの変更サマリー

v2はCancellationTokenによる割り込み実装を提案していたが、**より根本的なアーキテクチャ変更**が設計として確定した。

| 観点 | v2（CancellationToken方式） | v3（Event-Driven方式）|
|------|-----------------------------|-----------------------|
| メインの動作 | サブ起動→ブロック待機→キャンセル可 | サブ起動→**即イテレーション終了** |
| 割り込み実現方法 | CancellationTokenを伝播 | メインがブロックしないので**自然に実現** |
| コードの複雑さ | 中（Token伝播の実装が必要） | 低（メインは常に次のメッセージを待てる） |
| TODO #19との関係 | 別途統合が必要 | **そのまま同じモデルに統合** |
| P0修正との統合 | 独立した修正が必要 | アーキテクチャ設計から自然に解消 |

**コアコンセプト（v3）:**
```
メイン: LLM → ツール起動 → 即イテレーション終了（メッセージ受信に戻る）
サブ:   ツール実行 → 完了
           ↓ completion callback
メイン: 完了イベント受信 → 再LLM → 最終応答
```

---

## 1. 現在のアーキテクチャと問題点

### 1.1 現在のフロー（問題あり）

```
ユーザーメッセージ受信
    │
    ↓ gateway.recv()
run_discord_loop()
    │
    ├── on_first_response コールバック作成
    │       └── iteration=1のLLM応答テキストを即Discord送信
    │
    ├── run_agent_response().await ←【完全ブロック：ここが全問題の根源】
    │       │
    │       └── SkillEngine.run_with_model_override()
    │               ├── iteration 1: LLM呼び出し
    │               │       └── on_first_response発火（textあれば）
    │               ├── ツール実行（ここで30秒かかることも）
    │               ├── iteration 2: LLM呼び出し
    │               └── EngineResult返却
    │
    ├── should_send判定（二重送信バグここ）
    │       first_sent=true && tool_calls_made>0 → 再送信 ← バグ
    │
    └── 次のメッセージへ（ここに来るまで全ブロック）
```

**この設計の問題:**

1. **ブロッキング**: `run_agent_response().await` が完了するまで次のメッセージを受信できない
2. **二重送信**: `on_first_response`が送信後、`should_send`ロジックが再送信する
3. **DB競合**: subtask completion callbackが`tokio::spawn`で非同期起動するが、同一セッションへの並行アクセスが発生する

### 1.2 P0: 二重送信バグの詳細

**コードトレース（message_loop.rs, engine.rs）:**

```
iteration 1:
  LLM応答: content="確認します" + tool_calls=[search()]
    ↓
  engine.rs L410-422:
    if iterations == 1 {  ← ツールの有無を問わず発火！
        if let Some(ref text) = response.content {
            cb(text.clone());  ← Discord送信 #1（first_sent=true）
        }
    }
    ↓
  ツール実行 → iteration 2へ

iteration 2:
  LLM応答: content="検索結果: ..." tool_calls=[]
    ↓
  EngineResult{response: "検索結果: ...", tool_calls_made: 1}
    ↓

message_loop.rs L418-420:
  let should_send = !first_sent || engine_result.tool_calls_made > 0;
  // = !true || 1>0 = false || true = true → Discord送信 #2 ← バグ
```

**バグの本質**: `on_first_response`の発火条件とfinalレスポンス送信条件が矛盾している。  
「ツールを使ったら最終レスポンスを送る」というshould_sendのロジックが、  
「ツールを使う前のテキストも既に送信済み」という事実を無視している。

**二重送信が起きる具体ケース:**
```
iteration 1: content="はい、確認します", tool_calls=[log_event()]
  → on_first_response("はい、確認します") → 送信#1

[ツール実行: log_event() → "OK"]

iteration 2: content="はい、確認します", tool_calls=[]  ← 同じテキスト！
  → EngineResult{response: "はい、確認します", tool_calls_made: 1}
  → should_send = true → 送信#2 ← 重複！
```

### 1.3 P1: 割り込み不可の詳細

```rust
// message_loop.rs（現状の問題箇所）
loop {
    let incoming = gateway.recv().await;  // 次のメッセージ受信
    // ...
    let result = state.run_agent_response(...).await;  // ← 完全ブロック
    // ↑ ここで30秒かかっても、次の gateway.recv() は呼ばれない
    // ↑ 「やめて！」が来ても処理できない
}
```

**割り込みが必要なシナリオ（実際の体験）:**
- 重いタスクを頼んだ → 間違えた → 「キャンセル！」が届かない
- 検索コマンドを誤送信 → 正しいコマンドを送る → 前の処理が終わるまで待つ羽目
- 緊急停止したいのに、処理が全部終わってから止まる

### 1.4 P2: DB競合の詳細

```rust
// message_loop.rs 現状（サブタスク完了コールバック）
let completion_cb: SubtaskCompletionFn = Arc::new(move |subtask_id, _result, exit_reason| {
    tokio::spawn(async move {
        // ↑ 複数のサブタスクが同時完了 → 複数tokio::spawn → 競合！
        
        state.run_agent_response(
            &session_id,  // 同じsession_id！
            // ...
        ).await
    });
});
```

**問題点:**
1. **Sync Mutex in async context**: `state.db().lock().unwrap()` を `await` をまたいで保持する可能性
2. **並行セッションアクセス**: 複数サブタスクが同時に完了→同一セッションで複数エージェント応答
3. **応答順序不定**: DBロックを先に取れた方が先に送信される

---

## 2. 新アーキテクチャ：Event-Driven モデル

### 2.1 コアコンセプトの再定義

**旧設計（v2以前）の思想:**
> 「ツールはブロッキング処理。メインが待ちながら監視する」

**新設計（v3）の思想:**
> 「ツールは非同期イベント。メインは起動したら忘れて次へ進む。完了通知で戻ってくる」

これはHTTPサーバーのリクエスト/レスポンスモデルではなく、**Webhookモデル**に相当する。  
（TODO #19「長時間処理詰まり防止アーキテクチャ」が目指していたものがまさにこれ）

### 2.2 新しいフロー（全体像）

```
[ユーザーメッセージ受信]
         │
         ▼
run_discord_loop() ─── メッセージを受信し続けるループ
         │
         │  ① LLM呼び出し（iteration 1）
         ▼
    SkillEngine
         │
         ├── Case A: ツールなし（直接回答）
         │       └── テキスト返却 → Discord送信 → イテレーション終了
         │
         └── Case B: ツールあり（非同期作業）
                 │
                 ├── 「処理を開始します」（option: 中間テキスト送信）
                 ├── ツールを非同期起動（spawn）
                 └── イテレーション終了 ← ★ここがポイント！メインに制御が戻る
                         │
                         ▼
              [次のメッセージを待つ状態に戻る]
                         │
              [割り込みメッセージが来たらすぐ処理できる！]

〜〜〜 非同期でツール実行中 〜〜〜

[ツール完了イベント]
         │
         ▼
completion_registry に登録されたコールバック発火
         │
         ▼
メインループにイベントを通知
         │
         ▼
再LLM（iteration 2相当）
         │
         ▼
最終応答 → Discord送信
```

### 2.3 TODO #19（長時間処理詰まり防止）との統合

TODO #19で設計された「一次回答+バックグラウンド処理分離」は、まさにこのv3のEvent-Drivenモデルと同じ考え方だ。

**TODO #19のコミット（e04a3d3）が実装した内容:**
- 受信→即時一次応答（「確認します」）
- 長い処理はサブタスクとして分離
- 完了時にcompletionコールバックで再LLM

**v3での統合:**
- サブタスクだけでなく、**全ツール実行**を同じEvent-Drivenモデルで扱う
- `on_first_response`（即時テキスト送信）はこのモデルの一部として位置づける
- 「サブタスク委譲ツール」と「通常ツール」の実行フローを統一

```
現在（バラバラ）:
  通常ツール → SkillEngine内でブロック実行
  サブタスク → completion_registryを使った非同期実行

v3後（統一）:
  全ツール → Event-Drivenモデル
  └── 軽量ツール: 内部で実行してEvent発火（実質的に同期に近い）
  └── 重量ツール: 真の非同期実行、完了時にEvent発火
```

### 2.4 イベントループの設計

```rust
/// メッセージループへの内部イベント
enum LoopEvent {
    /// Discordからの新規メッセージ
    IncomingMessage(IncomingMessage),
    /// ツール/サブタスク完了通知
    ToolCompletion {
        session_id: String,
        agent_id: String,
        tool_name: String,
        tool_result: String,
        channel_id: u64,
    },
    /// サブタスク委譲完了（TODO #19）
    SubtaskCompleted {
        session_id: String,
        agent_id: String,
        subtask_id: String,
        result: String,
        exit_reason: String,
        channel_id: u64,
    },
}

/// 新しいDiscordループ（Event-Driven）
pub async fn run_discord_loop_v3<T: AgentRunner>(
    gateway: Arc<DiscordGateway>,
    state: T,
    agent_ids: Vec<String>,
    gateway_actions: Arc<dyn GatewayActions>,
    owner_discord_id: String,
    completion_registry: CompletionRegistry,
) {
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<LoopEvent>();
    
    // Discord受信をイベントに変換するタスク
    let recv_event_tx = event_tx.clone();
    tokio::spawn(async move {
        loop {
            match gateway.recv().await {
                Ok(msg) => {
                    let _ = recv_event_tx.send(LoopEvent::IncomingMessage(msg));
                }
                Err(e) => {
                    error!("Discord recv error: {e}");
                    break;
                }
            }
        }
    });
    
    // イベント処理ループ（直列）
    loop {
        match event_rx.recv().await {
            Some(LoopEvent::IncomingMessage(msg)) => {
                handle_incoming_message(&state, &msg, &event_tx, ...).await;
            }
            Some(LoopEvent::ToolCompletion { session_id, .. }) => {
                handle_tool_completion(&state, session_id, &event_tx, ...).await;
            }
            Some(LoopEvent::SubtaskCompleted { session_id, .. }) => {
                handle_subtask_completed(&state, session_id, &event_tx, ...).await;
            }
            None => break,
        }
    }
}
```

**設計のポイント:**
- イベントループが直列で動く → DB競合なし（P2自然解消）
- `handle_incoming_message`はツール起動後すぐ返る → ブロックなし（P1自然解消）
- 各イベントハンドラは独立して動く → P0は設計から明確に分離される

---

## 3. P0修正方針（新アーキテクチャでの位置づけ）

### 3.1 新アーキテクチャでのP0の位置づけ

v3のEvent-Drivenモデルでは、メッセージ送信のタイミングが明確に定義される。

```
イテレーション1（ユーザーメッセージ受信時）:
  Case A（ツールなし）: LLMテキスト → 1回送信 → 完了
  Case B（ツールあり）: 中間テキスト送信（任意）→ ツール起動 → イテレーション終了

イテレーション2（ツール完了イベント受信時）:
  LLMテキスト → 1回送信 → 完了（またはさらにツールあれば繰り返し）
```

この設計では「二重送信する理由がない」。各イテレーションは独立した処理として実行される。

### 3.2 P0の短期修正（engine.rs / message_loop.rs）

全体のEvent-Driven化を待たずに先行できるP0修正：

**変更1: `crates/core/src/engine.rs`**

```rust
// 修正前（ツールの有無を問わず発火）
if iterations == 1 {
    if let Some(ref text) = response.content {
        if !text.is_empty() {
            if let Some(ref cb_lock) = self.on_first_response {
                // ... cb発火
            }
        }
    }
}

// 修正後（ツールがない場合のみ発火 = これが最終レスポンス）
if iterations == 1 && response.tool_calls.is_empty() {
    if let Some(ref text) = response.content {
        if !text.is_empty() {
            if let Some(ref cb_lock) = self.on_first_response {
                // ... cb発火
            }
        }
    }
}
```

**変更2: `crates/discord/src/message_loop.rs`**

```rust
// 修正前
let should_send = !first_sent.load(std::sync::atomic::Ordering::SeqCst)
    || engine_result.tool_calls_made > 0;

// 修正後（on_first_responseが送信済みなら再送しない）
let should_send = !first_sent.load(std::sync::atomic::Ordering::SeqCst);
```

**修正の意味:**
- `on_first_response`は「ツールなしの即レスポンス専用」として役割を明確化
- ツールありの場合はon_first_responseが発火しない → finalが唯一の送信 → 重複なし
- `should_send`のロジックが `!first_sent` というシンプルな条件になる

### 3.3 修正後の送信ロジック一覧

| ケース | on_first_response | final response | 送信回数 |
|--------|------------------|----------------|----------|
| ツールなし・テキストあり | 発火（送信#1） | should_send=false | **1回** ✓ |
| ツールあり・iter1テキストなし | 発火しない | should_send=true | **1回** ✓ |
| ツールあり・iter1テキストあり | **発火しない**（修正後） | should_send=true | **1回** ✓ |
| NO_REPLY | 発火しない | 送信しない | **0回** ✓ |

**トレードオフ:** ツールありのケースで「確認します → [ツール実行] → 結果」という2段階表示ができなくなる。  
→ **v3 Event-Driven化後に「中間テキスト送信」として別途実装する**（§2.2参照）。

---

## 4. 割り込み処理（P1）

### 4.1 v2との比較

**v2の割り込み方式:**
```
メイン: ブロック中（run_agent_response.await）
割り込み: CancellationTokenを伝播 → LLM/ツール呼び出しをキャンセル
```

課題:
- SkillEngineにCancellationToken伝播ロジックの実装が必要
- ツール実行中のキャンセルはチェックポイント方式（ツール完了まで待つ）
- 実装が複雑

**v3の割り込み方式:**
```
メイン: 常にイベントを待つ（ブロックしない）
割り込み: 新メッセージがイベントとして届く → 自然に処理できる
```

**メインがブロックしないから、割り込みが「自然に実現」される。**

### 4.2 チャンネルごとの処理状態管理

```rust
/// チャンネルごとの処理状態
struct ChannelState {
    /// 現在実行中のツール/サブタスクのキャンセルハンドル
    active_canceller: Option<tokio::sync::oneshot::Sender<()>>,
    /// 現在処理中のセッションID
    session_id: String,
    /// 最後のメッセージ受信時刻
    last_message_at: Instant,
}

// メインループに追加
let mut channel_states: HashMap<u64, ChannelState> = HashMap::new();
```

**割り込みフロー:**
```
[チャンネル123でメッセージA処理中]
  → ツール起動中（非同期、キャンセル可）

[チャンネル123で「やめて！」受信]
  → イベントループがすぐ受信（ブロックしてないので！）
  → channel_states[123].active_canceller.send(()) → キャンセル信号送信
  → 「やめて！」を新規メッセージとして処理開始

[キャンセルされたツール側]
  → tokio::select! で canceller受信 → 終了
  → completion callbackは呼ばれない（または cancelled として呼ばれる）
```

### 4.3 割り込み後の状態整合性

| 割り込み発生タイミング | 処理 |
|------------------------|------|
| LLM呼び出し前 | そのまま破棄（何も送信されていない） |
| on_first_response発火済み | 送信済みテキストはそのまま。以降の処理を中止 |
| ツール実行中 | ツールのキャンセル（可能な場合）or 完了後に結果を破棄 |
| サブタスク実行中 | サブタスクにキャンセル信号を送る（既存のkill API活用） |

**会話履歴の整合性:**
- ユーザーメッセージはキャンセル前に記録済み → そのまま
- キャンセルされたエージェント応答はDBに書き込まれない（EngineResultが返らないため）
- 「やめて！」メッセージは新規ユーザーメッセージとしてDBに記録

### 4.4 既存の kill/abort API の活用

サブタスク委譲の場合は既存のkill APIが使える:
```
// openclab gateway call chat.abort
POST /gateway/chat.abort
{ "sessionKey": "agent:main:subagent:<UUID>", "runId": "<UUID>" }
```

v3ではこのAPIをツールキャンセルの統一インターフェースとして活用する。

---

## 5. DB競合対策（P2）

### 5.1 Event-Drivenモデルでのの自然解消

v3のイベントループが**直列**で動くため、P2の問題は構造的に解消される。

```
現在（並列・競合あり）:
  subtask_1完了 → tokio::spawn → run_agent_response（同session）
  subtask_2完了 → tokio::spawn → run_agent_response（同session）
  ↑ 同時実行 → DBロック競合・応答順序不定

v3（直列・競合なし）:
  subtask_1完了イベント → イベントキューに積む
  subtask_2完了イベント → イベントキューに積む
  
  イベントループ:
    subtask_1完了を処理 → run_agent_response → 完了
    subtask_2完了を処理 → run_agent_response → 完了
  ↑ 自然に直列化 → 競合なし
```

### 5.2 移行期間中の最小限の対策（P0修正と同時期）

Event-Driven化が完成するまでの暫定対策:

```rust
// session-scoped tokio Mutex による排他ロック
use dashmap::DashMap;

static SESSION_LOCKS: Lazy<Arc<DashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    Lazy::new(|| Arc::new(DashMap::new()));

fn get_session_lock(session_id: &str) -> Arc<tokio::sync::Mutex<()>> {
    SESSION_LOCKS
        .entry(session_id.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

// completion callback 内で使用
tokio::spawn(async move {
    let lock = get_session_lock(&session_id);
    let _guard = lock.lock().await;  // 同一sessionで排他実行
    
    state.run_agent_response(&session_id, ...).await
});
```

---

## 6. 実装フェーズ

### Phase 0: P0修正（即時対応、1-2時間）

**目的:** 二重送信バグを最小変更で修正

**変更ファイル:**
- `crates/core/src/engine.rs`: `on_first_response`の発火条件に `response.tool_calls.is_empty()` 追加
- `crates/discord/src/message_loop.rs`: `should_send` を `!first_sent` に簡略化

**リスク:** 低（2行の変更、既存テストで検証可能）

**確認テスト:**
```
1. ツールなし: ユーザー「こんにちは」→ 応答1回
2. ツールあり: 検索が必要な質問→ 応答1回（重複なし）
3. NO_REPLY: 応答なし
4. noreact（NO_REPLY）+ 別ツール: ツール実行、Discord送信なし
```

### Phase 1: Event-Driven基盤の構築（3-5日）

**目的:** メインループをEvent-Drivenモデルに移行

**変更ファイル:**
- `crates/discord/src/message_loop.rs`: ループをイベントキュー方式に書き換え
  - `LoopEvent` enum の定義
  - 受信タスクとイベント処理ループの分離
  - completion callbackをイベント送信に変更

**新規ファイル（候補）:**
- `crates/discord/src/event_loop.rs`: Event-Drivenループの実装

**移行戦略（後方互換性）:**
```rust
// feature flagで切り替え可能に
#[cfg(feature = "event-driven")]
pub use event_loop::run_discord_loop_v3 as run_discord_loop;

#[cfg(not(feature = "event-driven"))]
pub use message_loop::run_discord_loop;
```

**確認テスト:**
```
1. 基本動作: 全Phase 0のテストがpass
2. 割り込み: 重い処理中に別メッセージ→前の処理の結果が来ない
3. サブタスク完了: サブタスク委譲後に完了通知が届く
4. 複数サブタスク: 2つのサブタスクが完了→順番通りに応答
```

### Phase 2: ツール実行の非同期化（1-2週間）

**目的:** 全ツール実行をEvent-Drivenモデルに統合

**変更ファイル:**
- `crates/core/src/engine.rs`: ツール実行を非同期化
- `crates/actions/`: 各ツール実装のキャンセル対応

**新しいツール実行インターフェース:**
```rust
/// ツール実行の非同期インターフェース
pub trait AsyncToolExecutor {
    /// ツールを非同期で実行し、完了時にcallbackを呼ぶ
    fn spawn_tool(
        &self,
        tool_call: &ToolCall,
        on_complete: impl FnOnce(ToolResult) + Send + 'static,
    ) -> CancellationHandle;
}

/// キャンセルハンドル
pub struct CancellationHandle {
    sender: tokio::sync::oneshot::Sender<()>,
}

impl CancellationHandle {
    pub fn cancel(self) {
        let _ = self.sender.send(());
    }
}
```

**軽量ツールの扱い:**
```rust
// 実行時間が短いツールは同期的に実行してイベントを即発火
// 長いツール（ファイル操作、外部API等）は真の非同期実行
impl AsyncToolExecutor for DefaultExecutor {
    fn spawn_tool(&self, tool_call, on_complete) -> CancellationHandle {
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
        
        tokio::spawn(async move {
            tokio::select! {
                result = self.execute_tool(tool_call) => {
                    on_complete(result);
                }
                _ = cancel_rx => {
                    // キャンセル: on_completeは呼ばれない
                    tracing::info!("Tool execution cancelled: {}", tool_call.name);
                }
            }
        });
        
        CancellationHandle { sender: cancel_tx }
    }
}
```

### Phase 3: 中間テキスト送信の復活（1-3日）

**目的:** 「確認します → [処理中] → 結果」という体験を再実装

Phase 0のP0修正でツールありの場合は中間テキストを送らなくなる。Phase 3でこれを正しく実装する。

**設計:**
```
iteration 1: content="確認します", tool_calls=[search()]
  → 中間テキスト送信: "確認します"（明示的に送信）
  → ツールを非同期起動
  → イテレーション終了

[search完了イベント]
  → iteration 2: content="検索結果は..."
  → Discord送信（最終）
```

```rust
/// 中間テキスト送信（ツールあり時）
async fn send_intermediate_text(
    gateway: &DiscordGateway,
    channel_id: u64,
    text: &str,
) {
    // 空テキスト・NO_REPLYはスキップ
    if text.is_empty() || text.trim() == "NO_REPLY" {
        return;
    }
    if let Err(e) = gateway.send_to_channel(channel_id, text).await {
        warn!("Failed to send intermediate text: {e}");
    }
}
```

---

## 7. ユースケース検証

### UC1: 長時間タスク中の無音問題

**シナリオ:** 複数の外部API呼び出しが必要なタスク（30秒かかる）

```
ユーザー: 「全部のログを集計して」
Agent: [LLM応答] content="集計を開始します", tool_calls=[aggregate_logs()]
  → 中間テキスト: "集計を開始します" → Discord送信
  → aggregate_logs()を非同期起動
  → イテレーション終了（メインループに戻る）

[30秒後: aggregate_logs()完了]
  → ToolCompletionイベント発火
  → [LLM応答] content="集計完了。結果: ..." → Discord送信
```

**v3での解決:** 中間テキスト送信（Phase 3）により「開始します」という即時フィードバックが出る。  
メインループはブロックしないので、この間も他のメッセージを処理できる。

### UC2: シンプルなリアクション+返答（二重送信しない）

**シナリオ:** 「こんにちは」という挨拶

```
ユーザー: 「こんにちは」
Agent: [LLM応答] content="こんにちは！", tool_calls=[]
  → on_first_response発火（ツールなしのため）→ Discord送信（1回）
  → EngineResult{response: "こんにちは!", tool_calls_made: 0}
  → should_send = !first_sent = false → 送信しない
  ✓ 1回だけ送信
```

### UC3: 複数ステップ調査（進捗可視性）

**シナリオ:** 情報収集→整理→回答という複数ステップのタスク

```
ユーザー: 「Rustの最新動向を調べて」
Agent:
  iteration 1: "調べています..." + [web_search("Rust 2026")]
    → 中間テキスト: "調べています..."
    → web_search 非同期起動

[web_search完了]
  iteration 2: "整理中..." + [web_search("Rust blog")]
    → 中間テキスト: "整理中..."
    → web_search 非同期起動

[web_search完了]
  iteration 3: "2026年のRustは..." (ツールなし)
    → Discord送信（最終）

ユーザーには:
1. "調べています..."（即時）
2. "整理中..."（1回目のsearch完了後）
3. "2026年のRustは..."（最終結果）
→ 進捗が見える！
```

### UC-A: 処理中の「やめて！」（割り込みキャンセル）

**シナリオ:** 重いタスク処理中に停止を要求

```
ユーザー: 「全ファイルをバックアップして」
Agent: 中間テキスト"バックアップ開始..." → backup_files() 非同期起動
  → メインループは次のメッセージを待つ状態に戻る

ユーザー: 「やめて！」（3秒後）
  → イベントループがすぐ受信（ブロックしてない！）
  → channel_states[ch].active_canceller.send(()) → バックアップキャンセル
  → 「やめて！」を新規メッセージとして処理
Agent: 「バックアップをキャンセルしました」→ Discord送信
```

**v2との比較:** v2はCancellationToken伝播が必要（複雑）。v3はメインがブロックしないため「やめて！」がすぐイベントとして届く。

### UC-B: 検索中に別キーワードで検索（前のキャンセル）

**シナリオ:** 検索コマンドを誤送信→すぐ訂正

```
ユーザー: 「Pythonについて調べて」
Agent: web_search("Python") 非同期起動

ユーザー: 「やっぱりRustで」（0.5秒後）
  → イベントループ受信
  → web_search("Python")キャンセル
  → web_search("Rust") 新規起動

[web_search("Rust")完了]
Agent: 「Rustについて: ...」→ Discord送信
```

### UC-C: 軽い質問の高速連打（全部処理 vs 最後だけ）

**シナリオ:** 短い質問を連続して送る

```
ユーザー: 「1+1は？」（t=0）
ユーザー: 「2+2は？」（t=0.1秒後）
ユーザー: 「3+3は？」（t=0.2秒後）

イベントループ:
  t=0: 「1+1は？」処理開始（LLM呼び出し）
  t=0.1: 「2+2は？」イベント受信
    → 「1+1は？」の処理中？ツールなしならLLM中
    → キャンセルポリシーに依存:
      - ポリシーA（全部処理）: 「1+1は？」完了後に「2+2は？」処理
      - ポリシーB（最後だけ）: 「1+1は？」をキャンセル、「2+2は？」に進む
      - ポリシーC（並列処理、非推奨）: 両方同時処理
```

**v3での推奨:** **ポリシーA（全部処理）** をデフォルトとし、  
「重いタスク」（ツールあり）の場合のみポリシーB（割り込み優先）を適用する。

```rust
/// キャンセルポリシー
enum CancellationPolicy {
    /// 全部順番に処理（デフォルト）
    Sequential,
    /// 現在の処理をキャンセルして新規優先
    PreemptCurrent,
    /// 処理タイプで判断（ツールありならPreempt、なしはSequential）
    Adaptive,
}
```

---

## 8. アーキテクチャ移行戦略

### 8.1 段階的移行（後方互換性を維持）

```
Phase 0（現在）: message_loop.rs そのまま、P0修正のみ
    ↓
Phase 1: event_loop.rs を新規作成、feature flagで切り替え
    ↓
Phase 2: ツール実行を AsyncToolExecutor に移行
    ↓
Phase 3: message_loop.rs を廃止、event_loop.rs に統一
```

### 8.2 テスト戦略

**Unit Tests（engine.rs）:**
```rust
#[test]
async fn test_on_first_response_not_fired_with_tools() {
    // ツールありの場合はon_first_responseが発火しないことを確認
    let mut engine = setup_engine_with_mock_llm();
    let fired = Arc::new(AtomicBool::new(false));
    let fired_clone = fired.clone();
    engine.set_on_first_response(move |_| { fired_clone.store(true, SeqCst); });
    
    // LLMがtool_callsを返すようにモック
    // ...
    let result = engine.run_with_model_override(...).await;
    assert!(!fired.load(SeqCst), "on_first_response should not fire when tools are used");
}

#[test]
async fn test_no_double_send_with_tools() {
    // ツールありのケースで送信が1回だけであることを確認
    // ...
}
```

**Integration Tests:**
```
test_basic_response: ツールなし→1回送信
test_tool_response: ツールあり→1回送信
test_no_reply: NO_REPLY→送信なし
test_interrupt: 処理中に別メッセージ→前の処理がキャンセルされる
test_subtask_completion: サブタスク完了→再LLM→送信
```

### 8.3 モニタリング・デバッグ

```rust
// トレーシングの強化
tracing::info!(
    event = "tool_started",
    tool_name = %tool_call.name,
    session_id = %session_id,
    channel_id = channel_id,
    "Tool execution started (non-blocking)"
);

tracing::info!(
    event = "tool_completed",
    tool_name = %tool_name,
    session_id = %session_id,
    elapsed_ms = elapsed.as_millis(),
    "Tool completion event received"
);

tracing::info!(
    event = "processing_interrupted",
    channel_id = channel_id,
    reason = "new_message",
    "Previous processing cancelled due to interrupt"
);
```

---

## 9. 懸念事項と未解決問題

### 9.1 LLMコンテキストの管理

Event-Drivenモデルでは、「iteration 1でツールを起動し、iteration 2でLLMを呼ぶ」間に会話コンテキストが変わる可能性がある。

**具体的な問題:**
```
t=0: ユーザー「Aについて調べて」
  → LLM呼び出し → ツール起動 → イテレーション終了

t=5: ユーザー「あと別の話だけど、Bも知りたい」
  → イベントループ受信 → 別セッション扱いで処理

t=10: ツール完了イベント
  → どのLLMコンテキストで再LLMするか？
    - Bの会話の前のコンテキスト？
    - Bの会話を含むコンテキスト？
```

**推奨:** ツール起動時のコンテキストを「スナップショット」として保持し、  
completion callbackではそのスナップショットを使って再LLMする。

```rust
struct ToolCompletionContext {
    snapshot_conversation: String,
    snapshot_system_prompt: String,
    messages_before_tool: Vec<ChatMessage>,
    // ...
}
```

### 9.2 タイムアウト処理

ツールが永遠に完了しない場合の対処:

```rust
// ツール実行にタイムアウトを設定
tokio::time::timeout(
    Duration::from_secs(300),  // 5分
    execute_tool(tool_call),
).await
.unwrap_or_else(|_| ToolResult::error("Timeout"))
```

### 9.3 複数エージェントの同一チャンネル

現在のコードは `for agent_id in &agent_ids` でループしており、  
同一チャンネルに複数エージェントが設定されている場合がある。

Event-Drivenモデルでのチャンネル状態管理は `(channel_id, agent_id)` のペアで管理する必要がある。

```rust
// キーは (channel_id, agent_id) のペア
let mut channel_states: HashMap<(u64, String), ChannelState> = HashMap::new();
```

### 9.4 completion callbackとの後方互換性

既存の `CompletionRegistry` は `SubtaskCompletionFn` を使っている。  
Phase 1移行時に既存コードが壊れないよう、インターフェースの後方互換性を保つ。

```rust
// 既存の CompletionRegistry を内部的に event_tx への送信に変換
let completion_cb: SubtaskCompletionFn = Arc::new(move |subtask_id, result, exit_reason| {
    let _ = event_tx.send(LoopEvent::SubtaskCompleted {
        session_id: session_id.clone(),
        agent_id: agent_id.clone(),
        subtask_id,
        result,
        exit_reason,
        channel_id,
    });
});
```

---

## 10. 設計決定サマリー

| 項目 | v2の決定 | v3の決定 | 変更理由 |
|------|----------|----------|----------|
| メインの動作 | ブロック + CancellationToken | 即終了 + Event-Driven | 根本的に構造を変える |
| P0修正方針 | engine.rsのon_first_response条件変更 | **同じ**（Phase 0として先行） | 短期修正として互換 |
| P1実現方法 | CancellationToken伝播（複雑） | メインがブロックしない（自然） | シンプルで確実 |
| P2対策 | SessionLockRegistry | Event-Drivenで自然解消 | アーキテクチャで解決 |
| TODO #19統合 | 別途検討 | **同じモデルに統合** | 設計の統一 |
| on_first_response | 維持（条件明確化） | **Phase 0では維持、Phase 3で再設計** | 段階的移行 |
| キャンセルポリシー | 全キャンセル | Adaptive（ツールありは割り込み優先） | UXに応じた柔軟性 |
| LLMコンテキスト | 考慮なし | スナップショット方式 | 整合性確保 |

---

## 11. コード変更箇所一覧

### Phase 0（P0修正、即時対応）

**`crates/core/src/engine.rs`**
```diff
-        if iterations == 1 {
+        // Fire on_first_response ONLY when there are no tool calls
+        // (meaning this IS the final response, not an intermediate step).
+        if iterations == 1 && response.tool_calls.is_empty() {
             if let Some(ref text) = response.content {
                 if !text.is_empty() {
                     if let Some(ref cb_lock) = self.on_first_response {
```

**`crates/discord/src/message_loop.rs`**
```diff
-        let should_send = !first_sent.load(std::sync::atomic::Ordering::SeqCst)
-            || engine_result.tool_calls_made > 0;
+        // on_first_response fires only for no-tool responses (direct answers).
+        // If it fired, the final response is already sent; don't send again.
+        // If it didn't fire (tool was used), we must send the final response.
+        let should_send = !first_sent.load(std::sync::atomic::Ordering::SeqCst);
```

### Phase 1（Event-Driven基盤）

**新規: `crates/discord/src/event_loop.rs`**
- `LoopEvent` enum
- `run_discord_loop_v3()` 関数
- `handle_incoming_message()` 関数
- `handle_tool_completion()` 関数
- `handle_subtask_completed()` 関数
- `ChannelState` 構造体

**更新: `crates/discord/src/message_loop.rs`**
- completion callbackをevent_txへの送信に変更（後方互換）

**更新: `crates/discord/src/lib.rs`**
- feature flagによるループ切り替え

### Phase 2（ツール非同期化）

**更新: `crates/core/src/engine.rs`**
- `AsyncToolExecutor` trait の導入
- `spawn_tool()` の実装
- `CancellationHandle` 構造体

**更新: `crates/actions/`（各ツール）**
- キャンセル対応の追加（select!パターン）

### Phase 3（中間テキスト復活）

**更新: `crates/discord/src/event_loop.rs`**
- `send_intermediate_text()` 関数
- ツールあり時の中間テキスト送信フロー

---

## 12. 参考資料

- **TODO #19**: 長時間処理詰まり防止アーキテクチャ（commit `e04a3d3`）
- **v2設計書**: `docs/design-message-loop-v2.md`（CancellationToken方式の詳細）
- **engine.rs**: `crates/core/src/engine.rs` L410-422（on_first_response実装）
- **message_loop.rs**: `crates/discord/src/message_loop.rs` L418-420（should_sendロジック）

---

*設計確定日: 2026-03-23*  
*設計者: owner + エージェントC*  
*実装開始条件: Phase 0はすぐ開始可能。Phase 1以降は別途実装チケットを作成。*
