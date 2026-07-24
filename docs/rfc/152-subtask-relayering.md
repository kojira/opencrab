# RFC #152: subtask/バックグラウンド実行機構の再層化（LoopEvent 結合の解消・全ゲートウェイ対応）

- Status: Draft（レビュー用・実装前）
- Issue: #152
- 関連: #144（非ブロック化）/ #150（停止）/ #151（個別/クロスセッション cancel/list）/ #142（steer）/ #143（親コンテキスト継承）
- 前提: 本ドキュメントは「計画を先に書いて PR → 第三者レビュー → 合意後に実装」の**計画フェーズ**。コードは変更していない。

> **本 RFC の軸: 「最小で層違反を解消する」。** 大きな理想アーキテクチャを一気に作るのではなく、
> 現状の結合点だけを外し、ビルド緑を保ちながら全ゲートウェイへ広げることを目的とする。

---

## 0. TL;DR

- subtask/バックグラウンド実行（spawn / registry / 完了再注入）は**全ゲートウェイ共通のエージェント能力**だが、現状 `crates/discord` にあり Discord イベントループ専用 enum `LoopEvent`（`crates/discord/src/message_loop.rs:75-101`）に結合している。
- この結合により **(a) 非 Discord 親セッションで完了再注入が動かない**、**(b) server ツール（`nostr_generate_key`）が subtask の sub-engine から到達不能**、**(c) server が discord のサブタスク型を import する逆依存**が発生している。
- **推奨案（案A）**: subtask ランタイム（registry / spawn / abort / DB ログ / 再注入トリガ）を **`crates/actions`** へ移し、完了通知を `LoopEvent` 直依存から**最小の `SubtaskCompletionSink` trait**（gateway 非依存）へ置き換える。各ゲートウェイがこの sink を実装して購読する。
- **重要な発見**: 完了結果の**本文は既に DB（session_logs）を経由**しており、`LoopEvent::SubtaskCompleted` の `result` フィールドは再注入で**使われていない**（`message_loop.rs:829` の `_result`）。したがって完了通知に必要なのは「**セッション S のエージェントを再起動せよ**」という resume トリガ + 返信ルーティング情報だけ。これが抽象化を軽くできる根拠。

---

## 1. 現状アーキテクチャの精読結果（エビデンス付き）

### 1.1 crate 依存グラフ（内部 `opencrab_*` 依存）

`Cargo.toml` の `[dependencies]` より:

```
db, gateway, voice, llm-types      （葉）
   → core (db, llm-types)          crates/core/Cargo.toml:19-20
   → llm  (llm-types, db)          crates/llm/Cargo.toml:19,23
      → actions (core, gateway, db, llm-types)     crates/actions/Cargo.toml:18-26
         → discord (voice, core, actions, gateway[feat=discord], db)  crates/discord/Cargo.toml:17-21
         → nostr   (core, gateway, actions, db)                       crates/nostr/Cargo.toml:18-21
         → mcp     (core, gateway, actions, db)                       crates/mcp/Cargo.toml:19-22
            → server (core, llm, gateway, actions, db, nostr, mcp, discord[optional feature "discord"], voice)  crates/server/Cargo.toml:26-37
               → cli
```

- **server → discord** は optional・feature ゲート（`discord = ["dep:opencrab-discord", "dep:serenity"]`, `crates/server/Cargo.toml:8,36`）。
- **discord → {core, actions, gateway}** は直接依存（`crates/discord/Cargo.toml:17-21`）。
- **nostr は discord に依存しない**。discord と nostr は server が束ねる兄弟で相互依存なし（`crates/nostr/Cargo.toml:18-21`）。
- → **`actions` は discord / nostr / mcp すべてが依存する共通の下位層**。ここに置いたものは全ゲートウェイから使える。

### 1.2 subtask 機構の構成要素（現状すべて `crates/discord`）

| 要素 | 型/関数 | 場所 | gateway 依存性 |
|---|---|---|---|
| 実行中 subtask のエントリ | `struct SpawnedSubtask`（abort_handle / session_id / parent_session_id / agent_id / label / webhook / webhook_tx / started_instant） | `crates/discord/src/gateway_actions/mod.rs:39-54` | **非依存**（純データ + AbortHandle） |
| registry | `type SubtaskRegistry = Arc<DashMap<String, SpawnedSubtask>>` | `mod.rs:57` | **非依存** |
| spawn 本体（LLM sub-engine 版） | `DiscordGatewayActions::execute_spawn_subtask` | `crates/discord/src/gateway_actions/subtask_engine.rs:401-1061` | **一部依存**（sub-engine 構築は非依存だが webhook 解決・event_tx が Discord 側） |
| 背景 dispatch（#144 の yield→実行版） | `struct BackgroundDispatch` / `dispatch` | `subtask_engine.rs:136-295` | **event_tx（LoopEvent）に依存** |
| 完了通知の送信 | `fn send_subtask_completed_event` | `subtask_engine.rs:86-122` | **LoopEvent に直依存**（`subtask_engine.rs:15` で `message_loop::{parse_discord_session, LoopEvent}` を import） |
| 完了イベント enum | `enum LoopEvent::SubtaskCompleted{...}` | `message_loop.rs:75-101`（**Discord イベントループ専用 enum**） | **Discord 固有** |
| 完了再注入 | `fn process_subtask_completed` | `message_loop.rs:825-943` | **一部依存**（会話再構築・再推論は runner 経由で非依存、返信 `gateway.send_to_channel` は Discord 固有） |
| sub-engine 用最小権限 gateway | `struct SubEngineGatewayActions`（許可リスト `["report_progress"]`） | `subtask_engine.rs:28-76` | Discord 実装をラップ |
| 個別/クロス cancel・list（#151） | `cancel_job` / `cancel_session` / `list_session_jobs` / `list_agent_jobs` / `RunningJob` / `CancelJobOutcome` | `subtask_engine.rs:301-399` | registry のみ・**非依存** |

### 1.3 完了再注入のデータフロー（重要）

1. subtask 本体（`tokio::spawn`）が完了 → **親セッションログに `type:"subtask_completed"`（`result` 本文を含む）を DB へ書く**（`subtask_engine.rs:241-262` = BackgroundDispatch、`subtask_engine.rs:962-985` = spawn_subtask）。
2. `send_subtask_completed_event` が `LoopEvent::SubtaskCompleted{ result, channel_id, guild_id, ... }` を event_tx へ送る（`subtask_engine.rs:111-121`）。
3. Discord ループが受信 → `spawn_serialized_on_session` でセッション直列に `process_subtask_completed` を実行（`message_loop.rs:289-329`）。
4. `process_subtask_completed` は **`build_conversation_string` で DB から会話を再構築**（`message_loop.rs:882`）し、`run_agent_response` で**エージェントを再起動**（`message_loop.rs:899-915`）、応答を `gateway.send_to_channel` で Discord 送信（`message_loop.rs:925-928`）。

> **分離線（gateway 非依存にできる部分 / Discord 固有の部分）**
>
> - **gateway 非依存**: registry（`SpawnedSubtask`/`SubtaskRegistry`）、spawn/abort/kill_on_drop、DB への subtask ログ記録、sub-engine 構築（core `SkillEngine` + actions executor）、cancel/list（#151）、「セッション S を再起動せよ」という resume トリガの発火。
> - **Discord 固有**: `LoopEvent` enum とその mpsc ループ（`message_loop.rs:200-369`）、serenity 受信タスク（`message_loop.rs:209-221`）、返信ルーティング `channel_id`/`guild_id`/`send_to_channel`、`parse_discord_session`（Discord セッションID 形式のパーサ）。
>
> **決定的な観察**: 手順4の `process_subtask_completed` は完了イベントの `result` を**使っていない**（`message_loop.rs:829` の引数は `_result: String`）。本文は手順1で DB に永続化済みで、手順4は `build_conversation_string` で読み直す。**つまり完了通知は「resume トリガ + 返信ルーティング」だけあればよく、本文の運搬は不要**。この事実が抽象を軽くできる根拠。

### 1.4 各ゲートウェイの受信ループと subtask 対応状況

| gateway | 受信ループ / 推論起動 | subtask 完了再注入 |
|---|---|---|
| **Discord** | `run_discord_loop<T: AgentRunner>`（`message_loop.rs:180`）。`LoopEvent` を mpsc で処理。受信は serenity → `LoopEvent::IncomingMessage`（`message_loop.rs:209-221`）。推論は `AgentRunner`（`crates/discord/src/lib.rs:107-165`、server の AppState が実装）。 | **あり**（唯一の実装）。§1.3 の経路。 |
| **Nostr** | `NostrGatewayManager::run_nostr_loop` → 子プロセス `nostaro watch` の stdout を `BufReader::lines()` で行読み（`crates/nostr/src/manager.rs:243-253`）。1件 = `handle_event`（`manager.rs:276`）→ `NostrAgentRunner::run_agent_response`（`crates/nostr/src/runner.rs:14-17`、`manager.rs:324`）。**1イベント=1推論の同期完結**。 | **無し**。`crates/nostr/` に subtask 参照ゼロ。`NostrGatewayActions` は spawn_subtask を持たない。 |
| **REST** | `agents_messages.rs`。`process::run_agent_response(&state, req).await` の**同期一発**（`crates/server/src/api/agents_messages.rs:178`）。完了後 `status='completed'`（`:199-201`）。 | **無し**。使い捨ての空 registry を新規生成して渡すのみ（`agents_messages.rs:118-129`）、event_tx なし → 完了通知は `send_subtask_completed_event` の `event_tx=None` 分岐で破棄（`subtask_engine.rs:94-99`）。 |
| **heartbeat** | core の `heartbeat_loop`（`crates/core/src/heartbeat.rs:86-111`）のタイマ駆動。`make_heartbeat_callback`（`crates/server/src/main.rs:48`）→ `process::run_agent_response`（`main.rs:227-239`）。session_id は `heartbeat-{agent}-{channel}`（`main.rs:22`）。 | **無し**。`parse_discord_session` が非 Discord 形式で失敗し送信スキップ（`subtask_engine.rs:101-108`）。 |

### 1.5 runner 抽象は既に一部存在する

- Discord: `trait AgentRunner`（`crates/discord/src/lib.rs:107-165`、巨大・Discord 固有）。
- Nostr: `trait NostrAgentRunner`（`crates/nostr/src/runner.rs:14-62`、AgentRunner に依存しないよう**必要メソッドだけ切り出した最小版**）。
- 両者とも `crates/server` の `AppState` が実装（`crates/server/src/agent_runner_impl.rs:14`）。
- **共通の核**は `run_agent_response(RunRequest) -> EngineResult` / `build_conversation_string` / `build_agent_context` / `context_budget_tokens`。この4つが「セッションを再起動する」に必要な下位能力。

### 1.6 sub-engine の gateway 到達性と server ツール問題

- メイン executor は `SystemGatewayActions{ inner: req.gateway_actions }` の**合成**で構築される（`crates/server/src/process.rs:1502-1506`）。`SystemGatewayActions`（`crates/server/src/system_actions.rs:37-44`）は `nostr_generate_key` 等の server ツールを提供し、inner に Discord/Nostr の gateway を委譲する。
- 一方 `execute_spawn_subtask` の sub-engine は `SubEngineGatewayActions::new(self.clone())`（`subtask_engine.rs:727-728`）で構築され、**`self` = `DiscordGatewayActions` 単体**。合成された `SystemGatewayActions` ではない。
- → **subtask の sub-engine から server ツール（`nostr_generate_key`）に到達できない**。これが #144 で非ブロック化を「sub-engine 経由」ではなく「メイン executor で yield→背景 dispatch」する回避策（`dispatch_pending_tool_calls`, `crates/server/src/process.rs:1166-1224`）を要した直接原因。回避策は `state.subtask_dispatch.get(agent_id)`（Discord 起動時のみ populate、`crates/discord/src/manager.rs:71-78` / `crates/server/src/main.rs:585-595`）に依存するため、**Discord 以外では非ブロック dispatch も効かない**。

---

## 2. 問題の定式化（層違反が生む具体的制約）

1. **非 Discord で完了再注入が不可**: 完了通知が `LoopEvent`（Discord 専用 enum）に固定され、`event_tx` を持たない/Discord セッション形式でない親では破棄される（`subtask_engine.rs:94-108`）。→ Nostr / REST / heartbeat で subtask が実質機能しない（§1.4）。
2. **server ツールが subtask から不達**: sub-engine が Discord gateway 単体しか持たず、合成 `SystemGatewayActions` を見られない（§1.6）。→ 長時間 server ツールの非ブロック化に迂回実装（`dispatch_pending_tool_calls`）が必要になった。
3. **逆依存と回避策の重複**: server が discord のサブタスク型を import する（`opencrab_discord::{SubtaskRegistry, BackgroundDispatch, CancelJobOutcome, RunningJob, DiscordReplyContext}`：`crates/server/src/lib.rs:100`、`crates/server/src/system_actions.rs:354,443-464`、`crates/server/src/api/agents_messages.rs:118`）。REST は使い捨て registry を新規構築（`agents_messages.rs:118-129`）。同種のロジックが複数箇所に散る。
4. **ゲートウェイ非対称**: Discord だけが完全な非ブロック + 再注入を持ち、他は同期完結。共通能力のはずが1ゲートウェイの実装詳細に埋まっている。

---

## 3. 目標設計（複数案の比較）

### 3.1 導入する抽象（両案共通）

**(1) subtask ランタイム**（gateway 非依存）: `SpawnedSubtask` / registry / spawn / abort(kill_on_drop) / DB への subtask ログ記録 / cancel・list（#151）/ sub-engine 構築。

**(2) 完了通知の抽象 `SubtaskCompletionSink`**（`LoopEvent` 直依存を置換）:

```rust
// 最小案: 完了通知は「セッション S を resume せよ」+ 種別 のみ。
// 本文は既に DB へ永続化済み（§1.3 の発見）なので運搬しない。
pub trait SubtaskCompletionSink: Send + Sync {
    /// 親セッションのエージェントを再起動して subtask 結果を会話へ再注入する。
    /// kind = Completed / Progress を区別（progress は debounce 済みの1回）。
    fn on_subtask_settled(&self, ev: SubtaskSettled);
}

pub struct SubtaskSettled {
    pub session_id: String,   // 親セッション（ルーティングはここから導出）
    pub agent_id: String,
    pub subtask_id: String,
    pub exit_reason: String,  // completed / error / timeout / stopped_by_limit / progress
    pub kind: SettleKind,
}
```

- ランタイムは `Arc<dyn SubtaskCompletionSink>` を保持し、完了時に `on_subtask_settled` を呼ぶだけ。**`LoopEvent` を知らない**。
- 各ゲートウェイが sink を実装:
  - **Discord**: `event_tx.send(LoopEvent::SubtaskCompleted{...})` に変換（`channel_id`/`guild_id` は `parse_discord_session(session_id)` で復元 = 現状ロジックそのまま）。既存の mpsc ループ・直列化は温存。
  - **Nostr**: `run_agent_response` を回して結果を `cli.reply(...)` で返す（返信先の解決は §6 の未解決点）。
  - **REST / heartbeat**: §6 の設計判断（同期応答済み REST に非同期返信経路が無い問題）。最小では no-op（DB に結果は残る）から開始し、必要に応じ実装。

**(3) sub-engine の gateway を合成にする**: sub-engine 構築時に**呼び出し元と同じ合成 gateway**（`SystemGatewayActions` を含む）を渡せるようにし、server ツールを到達可能にする（許可リスト `SUB_ENGINE_ALLOWED_ACTIONS` の権限制御は維持 = §6 リスク）。

### 3.2 案A（推奨）: `crates/actions` に subtask ランタイム + sink trait

- **配置**: ランタイム（registry / `SpawnedSubtask` / `BackgroundDispatch` 相当 / cancel・list）と `SubtaskCompletionSink` trait を `crates/actions` に新モジュール（例 `actions::subtask`）として置く。
- **根拠**: `actions` は discord / nostr / mcp すべての下位依存（§1.1）で、既に `BridgedExecutor` / `RunRequest` / `ActionContext` を所有し sub-engine を構築できる。`core`（SkillEngine）と `gateway`（GatewayActions）にも依存済み。→ **新 crate を足さずに層違反を解消**できる（「最小」の軸に合致）。
- **依存方向**: `actions`（ランタイム + trait）← discord/nostr が sink を実装 ← server が配線。**逆依存（server→discord のサブタスク型 import）が消える**。
- **影響範囲**: subtask_engine.rs の非依存部分を actions へ移動。discord は「薄い Discord sink + 既存 LoopEvent ループ」だけ残す。server は discord ではなく actions のランタイム型を使う。
- **移行容易性**: 中〜大（テスト移設含む）。ただし段階移行しやすい（§4）。

### 3.3 案B: 専用 crate `opencrab-subtask`

- **配置**: `core/actions/gateway/db` に依存する新 crate にランタイム + trait を置き、discord/nostr/server がこれに依存。
- **利点**: 関心の分離が明確。subtask 機構の肥大化（webhook lifecycle・#142 steer・#143 継承）を1 crate に閉じられる。
- **欠点**: 新 crate のボイラープレート（Cargo/feature/ビルド時間）。依存グラフに層を1つ挿入する必要。`actions` と役割が重複しがち（両方 executor を組む）。現時点の規模では過剰の懸念（「過剰に大きな設計にしない」ガードに抵触しうる）。

### 3.4 比較と推奨

| 観点 | 案A（actions 内） | 案B（専用 crate） |
|---|---|---|
| 層違反の解消 | ○（全ゲートウェイが actions 依存） | ○ |
| 新規サーフェス | 小（モジュール追加のみ） | 中（crate 追加） |
| server→discord 逆依存の除去 | ○ | ○ |
| 「最小」ガード適合 | ◎ | △（将来の分離余地はあるが今は過剰） |
| 将来 subtask が肥大した時 | actions が太る懸念 | 分離済みで有利 |

→ **推奨は案A**。まず actions 内モジュールで層違反を解消し、将来 subtask 機構が十分肥大したら案B（crate 切り出し）へ再リファクタする余地を残す。crate 切り出しはモジュール境界が綺麗なら後から低コストで可能。

---

## 4. 移行計画（段階・ビルド緑を保つ）

各段は**単独でビルド緑 & 既存 Discord 挙動不変**を完了条件とする。

- **S0. 抽象の定義（振る舞い変更なし）**
  - `crates/actions` に `SubtaskCompletionSink` trait と `SubtaskSettled` 型、`SpawnedSubtask`/`SubtaskRegistry` 相当を追加（まだ誰も使わない）。
  - 完了条件: cargo build 緑、既存テスト不変。
- **S1. ランタイム移設（Discord を新抽象へ載せ替え・挙動不変）**
  - registry / spawn / abort / cancel・list（#151）/ DB ログ記録を actions へ移動。discord は `DiscordCompletionSink`（`event_tx`→`LoopEvent::SubtaskCompleted` 変換、`parse_discord_session` はここに残す）を実装してランタイムへ注入。
  - `execute_spawn_subtask` / `BackgroundDispatch` は actions ランタイムへの薄いアダプタに。
  - 完了条件: Discord の subtask/非ブロック/cancel/list が現状と同一挙動（既存テスト＋実機で spawn→completion→再注入を確認）。`_result` 不使用の事実に合わせ、完了通知から本文運搬を落とせるか検証。
- **S2. sub-engine を合成 gateway 化（server ツール到達性）**
  - sub-engine 構築に合成 gateway（`SystemGatewayActions` 含む）を渡せる口を開ける。`SUB_ENGINE_ALLOWED_ACTIONS` の許可リスト意味論は維持。
  - 完了条件: subtask 内から `nostr_generate_key` 等 server ツールが（許可された範囲で）実行可能。可能なら `dispatch_pending_tool_calls` 回避策の縮退を検討（別段でも可）。
- **S3. 他ゲートウェイ配線（全ゲートウェイ対応）**
  - Nostr / REST / heartbeat 用の sink を実装（Nostr = run_agent_response + reply、REST/heartbeat = §6 の判断に従い最小 no-op から）。REST の使い捨て registry 構築（`agents_messages.rs:118-129`）を actions ランタイム経由へ。
  - 完了条件: 少なくとも Nostr で spawn→completion→再注入が動く。REST/heartbeat は「結果が DB に残る」ことを最低保証。
- **S4. 旧結合の除去**
  - server の `opencrab_discord::{SubtaskRegistry, BackgroundDispatch, RunningJob, CancelJobOutcome}` import を actions 由来へ置換（`crates/server/src/lib.rs:100` 他）。`DiscordReplyContext` 等 Discord 固有の転記型は Discord 側に残す。
  - 完了条件: server が discord のサブタスク型に依存しない（`grep opencrab_discord::.*Subtask` が転記/UI 系のみ）。

---

## 5. 既存機能への影響

| Issue | 内容 | 新設計での扱い |
|---|---|---|
| **#144** | 全ツール非ブロック（現 Discord 結合版 `BackgroundDispatch` + `dispatch_pending_tool_calls`） | `BackgroundDispatch` を actions ランタイムへ移し sink 経由で resume。**現 Discord 結合版は置き換え**（issue 明記どおり）。yield→dispatch の骨格は維持し完了通知だけ抽象化。 |
| **#150** | エージェント駆動キャンセル（`cancel_running_tools`→`cancel_session`） | registry がランタイムへ移るが API 温存。**再ホームのみ、作り直し不要**（`crates/server/src/system_actions.rs:354`）。 |
| **#151** | 個別/クロスセッション cancel/list（`cancel_job`/`list_session_jobs`/`list_agent_jobs`/`RunningJob`/`CancelJobOutcome`） | 型と関数をランタイムへ移設（`subtask_engine.rs:301-399`）。**再ホームのみ**。server 側 import を actions へ張り替え。 |
| **#142** | 走行中サブへの steer（インバウンド指示） | 現 `SpawnedSubtask` はアウトバウンド `webhook_tx` のみ。ランタイムが1箇所に集約されるので、`SpawnedSubtask` にインバウンド tx を足す実装先が明確化。**新基盤上に新規実装**（本 RFC のスコープ外だが土台を用意）。 |
| **#143** | 親コンテキスト継承 `inherit_context` | sub-engine 構築がランタイムへ移るので、継承オプションの実装先が1箇所に。**新基盤上に新規実装**（スコープ外）。 |

---

## 6. リスク・未解決点・テスト戦略

### リスク
- **大規模リファクタ**: `subtask_engine.rs` は本体 + テストで ~2450 行（`crates/discord/src/gateway_actions/subtask_engine.rs`）。テスト移設と回帰の量が多い。
- **sub-engine 合成 gateway 化のセキュリティ**: server ツールを subtask に開放すると攻撃面が広がる。`SUB_ENGINE_ALLOWED_ACTIONS`（現 `["report_progress"]`）と owner 限定ゲート（`OWNER_ONLY_ACTIONS`）の意味論を**必ず維持**する。ネスト spawn の可否も従来どおり明示制御。
- **二重回答不変条件**: 「各ステップ出力を解放前に記録し、次サイクルは history 再構築で前回答を必ず見る」（#144）を段階移行で崩さない。progress debounce（`subtask_engine.rs:1292-1329`）の世代カウンタ挙動も温存。

### 未解決点（要レビュー判断・現時点で**未確認**）
- **Nostr の非同期返信先**: Nostr は1イベント=1推論の同期完結で永続ループが無い（`manager.rs:276-343`）。完了が元イベントより後に届く場合、`reply_target` をどこから復元するか未確認。session_id（`nostr-{agent}-{pubkey}`）に返信先イベントIDが含まれないため、別途保存が要るか要調査。
- **REST の非同期返信不能**: REST は HTTP 応答を返した後に subtask が完了しても返す先が無い（`agents_messages.rs:178-201`）。REST での subtask は「store-only（DB に結果を残すだけ・ライブ返信なし）」に割り切るのが妥当か、それとも REST では subtask を無効化するか、は設計判断。
- **完了通知から本文を落とせるか**: `_result` 不使用（`message_loop.rs:829`）は Discord 経路の観察。他経路で本文が必要になるケースが無いか、S1 で実証してから `SubtaskSettled` を確定する。

### テスト戦略
- **リグレッション基準**: 既存 `subtask_engine.rs` の unit テスト（`make_dispatch` 等 `subtask_engine.rs:2390`〜）を actions へ移し、挙動同一を担保。
- **抽象の unit テスト**: `SubtaskCompletionSink` のフェイク実装で「completion → on_subtask_settled が1回呼ばれる」「cancel で registry から確実に除去」「insert-before-run ゲートでリークしない」を検証。
- **ゲートウェイ結合テスト**: Discord = spawn→completion→再注入→送信（現状維持）。Nostr = spawn→completion→run_agent_response 再実行を最小構成で。
- **実機確認**: #144/#150/#151 の受け入れ基準（ツール実行中もメイン非ブロック、個別 cancel が巻き添えなし）を各段で確認。

---

## 7. スコープ外（今回やらないこと）

- **#142 steer / #143 inherit_context の実装本体**（土台のみ用意、機能実装は別 issue）。
- **webhook lifecycle 配送系**（`webhook.rs` / `subtask_webhook.rs`）の再設計。現状のまま Discord 側に残す（完了再注入の抽象化とは直交）。
- **A2UI `InteractionResponse`**（`LoopEvent` の別バリアント）の抽象化。subtask とは別関心。
- **案B（専用 crate 切り出し）**。案A 完了後に必要になれば別途。
- **voice / VC 経路**。
- **既存セッションID 形式の変更**（`discord-*` / `nostr-*` / `heartbeat-*` のパース規約はそのまま利用）。
