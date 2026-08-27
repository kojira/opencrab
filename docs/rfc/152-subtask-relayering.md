# RFC #152: subtask/バックグラウンド実行機構の再層化（LoopEvent 結合の解消・全ゲートウェイ対応）

- Status: Draft v2（レビュー用・実装前）
- Issue: #152
- 関連能力: #144（全ツール非ブロック）/ #150（エージェント駆動の停止）/ #151（個別/クロスセッション cancel/list）/ #142（走行中サブの steer）/ #143（親コンテキスト継承）
- 前提: 本 RFC は「計画を先に PR → 第三者レビュー → 合意後に実装」の**計画フェーズ**。コードは変更していない。

> **読み方（後続 issue で方針が変わった箇所がある）**
> 本 RFC は**執筆時点（main HEAD `8d64bef`）の現状分析と当時の意思決定の記録**であり、現在のコードの説明ではない。後続の実装で判断が変わった箇所には `> **更新（#…）**` の注記を**その場に追記**してある（当時の判断は消していない）。現状のコードを知りたい場合は注記側を、なぜそう決めたかの経緯を知りたい場合は本文側を読むこと。
>
> 現時点で方針が変わっているもの: §2-3 / §4 S4 の**転記型の所在**（#158 S3 で `opencrab_actions::transcript` へ移設）。

> **基準（オーナー確定）**
> 1. **main（HEAD `8d64bef`）から作り直す**。#144/#150/#151 の未コミット fix 版（`BackgroundDispatch` / `dispatch_pending_tool_calls` / `subtask_dispatch` / `cancel_job` / `cancel_running_tools` / `RunningJob` 等）は**プロトタイプとして破棄済み**であり、本設計は引きずらない。本 RFC の行番号はすべて main HEAD に対して張っている（上記 fix 版のシンボルは main に存在しない — §0.1 で実証）。
> 2. 軸は「**最小で層違反を解消する**」。理想アーキテクチャを一気に作らない。
>
> **本 RFC の看板の正しい読み方（オーナー確定）**
> **「再注入（＝完了後にエージェントが新ターンで継続）は全ゲートウェイで普遍・常に行う。ゲートウェイで違うのは生成メッセージの“配送口”だけ。」**
> 続行するか止めるかは、ターン資源を握った**エージェント自身の判断**。「1ターンで打ち切り」は設計不良。配送は Discord=`send_to_channel` / Nostr=`reply` / REST=保存して取得（将来 SSE/webhook push）/ heartbeat=次 tick 拾い or 保存、と**別関心**に分ける。

---

## 0. TL;DR

- subtask/バックグラウンド実行（spawn → registry → **完了後の再注入**）は**全ゲートウェイ共通のエージェント能力**だが、main では `crates/discord` にあり Discord イベントループ専用 enum `LoopEvent`（`crates/discord/src/message_loop.rs:38-77`）に結合している。
- この結合が **(a) 非 Discord 親セッションで再注入が起きない**、**(b) server ツール（`nostr_generate_key`）が subtask の sub-engine から構造的に到達不能**、**(c) server が discord のサブタスク型を import する逆依存**を生む。
- **推奨（案A）**: 再注入の**トリガと registry ランタイム**を **`crates/actions`**（discord/nostr/mcp すべての共通下位層）へ移し、完了通知を `LoopEvent` 直依存から**最小の `SubtaskCompletionSink` trait**（gateway 非依存）へ置換する。**再注入ロジックは全 GW 普遍**、**配送は GW 別アダプタ**として分離する。
- **設計の梃子（main で実証済み）**: 完了結果の**本文は既に DB（session_logs）を経由**しており、`process_subtask_completed` は完了イベントの `result` を**使っていない**（`message_loop.rs:710` の引数が `_result`）。再注入は `build_conversation_string` で DB から会話を再構築する（`message_loop.rs:746`）。→ 完了通知に必要なのは「**親セッションのエージェントを resume せよ**」というトリガのみで、本文運搬は不要。これが抽象を軽くできる根拠。
- **非ブロック実行（自動 dispatch / ツール継続）は本再層化の“上に新規設計”する**（§5）。破棄した fix 版を移設するのではない。

### 0.1 baseline 実証（main HEAD `8d64bef`）

```
$ git grep -n 'BackgroundDispatch|dispatch_pending_tool_calls|fn cancel_job|struct RunningJob|CancelJobOutcome|subtask_dispatch|cancel_running_tools' -- crates/
（一致なし＝これらは main に存在しない）

$ wc -l crates/discord/src/gateway_actions/subtask_engine.rs
2008  crates/discord/src/gateway_actions/subtask_engine.rs
```

main に実在する subtask 機構は **spawn_subtask（LLM サブエンジン版）と、その cancel/report_progress、完了イベント再注入だけ**。自動 dispatch 系は無い。

---

## 1. 現状アーキテクチャの精読結果（エビデンス付き・all main HEAD）

### 1.1 crate 依存グラフ（内部 `opencrab_*` 依存）

```
db, gateway, voice, llm-types      （葉）
   → core (db, llm-types)
   → actions (core, gateway, db)                 crates/actions/Cargo.toml:12-14
        （opencrab-llm-types は dev-dependency のみ）  crates/actions/Cargo.toml dev-deps
      → discord (voice, core, actions, gateway[feat=discord], db)  crates/discord/Cargo.toml
      → nostr   (core, gateway, actions, db)                       crates/nostr/Cargo.toml
      → mcp     (core, gateway, actions, db)
         → server (core, llm, gateway, actions, db, nostr, mcp, discord[optional "discord" feature], voice)
```

- **`actions` は discord / nostr / mcp すべての下位依存**（`crates/actions/Cargo.toml:12-14`）。ここに置いたものは全ゲートウェイから使える。
- **nostr は discord に依存しない**（兄弟）。両者を server が束ねる。
- **server → discord** は optional・feature ゲート。

### 1.2 subtask 機構の構成要素（main ではすべて `crates/discord`）

| 要素 | 型/関数 | 場所 | gateway 依存性 |
|---|---|---|---|
| 実行中 subtask のエントリ | `struct SpawnedSubtask`（abort_handle / session_id / parent_session_id / agent_id / label / **webhook** / **webhook_tx** / started_instant） | `crates/discord/src/gateway_actions/mod.rs:37-49` | **一部依存**（webhook/webhook_tx が Discord 型 = §1.5） |
| registry | `type SubtaskRegistry = Arc<DashMap<String, SpawnedSubtask>>` | `mod.rs:52` | **非依存** |
| spawn 本体（LLM sub-engine 版） | `DiscordGatewayActions::execute_spawn_subtask` | `crates/discord/src/gateway_actions/subtask_engine.rs:125-709` | **一部依存**（sub-engine 構築は非依存、event_tx/webhook 解決が Discord 側） |
| 完了/進捗の通知送信 | `fn send_subtask_completed_event` | `subtask_engine.rs:86-122` | **LoopEvent に直依存**（`subtask_engine.rs:15` で `message_loop::{parse_discord_session, LoopEvent}` を import） |
| 完了イベント enum | `enum LoopEvent`（`SubtaskCompleted{...}`） | `message_loop.rs:38-77`（**Discord イベントループ専用 enum**） | **Discord 固有** |
| 完了再注入 | `fn process_subtask_completed` | `message_loop.rs:706-802` | **一部依存**（会話再構築・再推論は AgentRunner 経由で非依存、返信 `gateway.send_to_channel` が Discord 固有 = `message_loop.rs:784`） |
| sub-engine 用最小権限 gateway | `struct SubEngineGatewayActions`（許可リスト `["report_progress"]`） | `subtask_engine.rs:35-76`、許可リスト定数 `subtask_engine.rs:28` | Discord 実装をラップ |
| cancel / report_progress | `execute_cancel_subtask` / `execute_report_progress` | `subtask_engine.rs:710` / `subtask_engine.rs:829` | registry のみ・**ほぼ非依存**（progress の debounce → LoopEvent 送信は依存） |
| ツール宣言 | `"spawn_subtask"` / `"cancel_subtask"` / `"report_progress"` の `definitions()` | `mod.rs:503 / 545 / 559`、dispatch `mod.rs:1017-1021` | **Discord 固有**（`DiscordGatewayActions` の定義。§6 のツール露出面の課題） |

### 1.3 完了再注入のデータフロー（設計の核）

1. subtask 本体（`tokio::spawn`）が完了 → **親セッションログに `type:"subtask_completed"`（`result` 本文を含む）を DB へ書く**（`execute_spawn_subtask` 内の session_logs 記録）。
2. `send_subtask_completed_event` が `LoopEvent::SubtaskCompleted{ result, channel_id, guild_id, ... }` を event_tx へ送る（`subtask_engine.rs:111`）。
3. Discord ループが受信 → `spawn_serialized_on_session` で**同一セッション直列**に `process_subtask_completed` を実行（`message_loop.rs:221-242`、直列化関数 `message_loop.rs:656`）。
4. `process_subtask_completed` は **`build_conversation_string` で DB から会話を再構築**（`message_loop.rs:746`）し、`run_agent_response` で**エージェントを再起動**、応答を `gateway.send_to_channel` で Discord 送信（`message_loop.rs:784`）。

> **分離線（gateway 非依存にできる部分 / Discord 固有の部分）**
> - **gateway 非依存（＝再注入の普遍部分）**: registry、spawn/abort/kill_on_drop、DB への subtask ログ記録、sub-engine 構築（core `SkillEngine` + actions executor）、「親セッションを resume せよ」というトリガ発火、会話の DB 再構築、`run_agent_response`。
> - **Discord 固有（＝配送＋トランスポート）**: `LoopEvent` enum とその mpsc ループ、serenity 受信、**返信配送 `send_to_channel`**、`parse_discord_session`（Discord セッションID 形式のパーサ）。
>
> **決定的な観察**: 手順4は完了イベントの `result` を**使っていない**（`message_loop.rs:710` の引数は `_result: String`）。本文は手順1で DB に永続化済みで、手順4は DB から読み直す。**完了通知は「resume トリガ」だけあればよい。**

### 1.4 各ゲートウェイの受信ループと subtask 対応状況（main）

| gateway | 受信ループ / 推論起動 | subtask 完了再注入 | 配送口 |
|---|---|---|---|
| **Discord** | `run_discord_loop<T: AgentRunner>`（`message_loop.rs` 本体）。`LoopEvent` を mpsc で処理。推論は `AgentRunner`（`crates/discord/src/lib.rs`、server の AppState が実装）。 | **あり**（唯一）。§1.3。 | `gateway.send_to_channel`（`message_loop.rs:784`） |
| **Nostr** | `NostrGatewayManager`：子プロセス `nostaro watch` の stdout を行読み → `handle_event`（`crates/nostr/src/manager.rs:~276`）→ `NostrAgentRunner::run_agent_response`（`manager.rs:324`）。**1イベント=1推論の同期完結**。 | **無し**。`crates/nostr/` に subtask 参照ゼロ。**per-session 直列化も無い**（§6）。 | `cli.reply(agent_id, event.reply_target(), ...)`（`manager.rs:341`、`reply_target` は `event.rs:35`） |
| **REST** | `agents_messages.rs`：`process::run_agent_response(&state, req).await` の**同期一発**（`agents_messages.rs:178`）。完了後 `status='completed'`（`agents_messages.rs:200`）。 | **無し**。使い捨ての空 registry を新規生成（`agents_messages.rs:118`）、`DiscordGatewayActions::new` を **`with_event_tx` 無し**で構築（`agents_messages.rs:120`）→ event_tx=None → 通知は破棄（`subtask_engine.rs:94-99`）。 | 現状 HTTP 応答のみ（非同期返信経路なし） |
| **heartbeat** | core の `heartbeat_loop`（`crates/core/src/heartbeat.rs:86`）のタイマ駆動。`make_heartbeat_callback`（`crates/server/src/main.rs:48`）→ `run_agent_response`（`main.rs:227`）。session_id は `heartbeat-{agent}-{channel}`（`main.rs:22`）。 | **無し**。`parse_discord_session` が非 Discord 形式で失敗し送信スキップ（`subtask_engine.rs:101-108`）。 | 次 tick 拾い or 保存 |

### 1.5 SpawnedSubtask は Discord 型（webhook）を抱えている

- `SpawnedSubtask.webhook: Option<WebhookConfig>` / `webhook_tx: Option<UnboundedSender<DeliveryBatch>>`（`mod.rs:44-46`）。`WebhookConfig` / `DeliveryBatch` は `crates/discord` の webhook モジュール由来（`mod.rs:32-33`）。
- → registry を下位層へ移すには、**この webhook フィールドの分離が不可分**（`WebhookConfig`/`DeliveryBatch` を下位へ持ち上げるか、随伴構造へ切り出す）。§4 S1 の一部として計画に含める（後述の「§7 直交」撤回）。

### 1.6 runner 抽象は既に一部存在する

- Discord: `trait AgentRunner`（`crates/discord/src/lib.rs`、巨大・Discord 固有）。
- Nostr: `trait NostrAgentRunner`（`crates/nostr/src/runner.rs:14-62`、AgentRunner に依存しないよう**必要メソッドだけ切り出した最小版**）。
- 両者とも `crates/server` の `AppState` が実装（`crates/server/src/agent_runner_impl.rs`）。
- **共通の核**は `run_agent_response(RunRequest) -> EngineResult` / `build_conversation_string` / `build_agent_context` / `context_budget_tokens`。「セッションを resume する」に必要な下位能力はこの4つ。
- （将来）これらを actions 側の gateway 非依存 `SessionResumer` trait に統合する余地があるが、**今回スコープ外**（§7）。

### 1.7 sub-engine の gateway 到達性と server ツール問題（P0）

- メイン executor は `SystemGatewayActions{ inner: req.gateway_actions }` の**合成**で構築される（`crates/server/src/process.rs:1358-1362`）。`SystemGatewayActions`（`crates/server/src/system_actions.rs`）は `nostr_generate_key` 等の server ツールを提供し、inner に Discord/Nostr の gateway を委譲する。
- 一方 `execute_spawn_subtask` の sub-engine は `SubEngineGatewayActions::new(self.clone())` で構築され、**`self` = `DiscordGatewayActions` 単体**。合成された `SystemGatewayActions` ではない。
- **なぜ構造的に到達不能か**: 子（callee）に渡る `GatewayCallContext`（`crates/gateway/src/traits.rs:40-47`：`caller` / `session_id` / `depth` / `agent_id` のみ）には**自分を包む root gateway への参照が無い**。子は「自分を実行している合成 gateway」を辿れない。
- → **subtask の sub-engine から server ツール（`nostr_generate_key`）に到達できない**。これは実装前に**注入 API を設計・合意すべき論点**（S2 = 設計スパイク。§3.1(3)・§6）。

---

## 2. 問題の定式化（層違反が生む具体的制約）

1. **非 Discord で完了再注入が不可**: 完了通知が `LoopEvent`（Discord 専用 enum）に固定され、event_tx を持たない/Discord セッション形式でない親では破棄される（`subtask_engine.rs:94-108`）。→ Nostr / REST / heartbeat で subtask が実質機能しない（§1.4）。
2. **server ツールが subtask から不達**: 子に root gateway が渡らず、sub-engine が合成 `SystemGatewayActions` を見られない（§1.7）。→ 長時間 server ツールを subtask 化できない。
3. **逆依存**: server が discord のサブタスク型を import する（`opencrab_discord::SubtaskRegistry`：`crates/server/src/main.rs:565`、`crates/server/src/api/agents_messages.rs:118`／`DiscordReplyContext`：`crates/server/src/transcript.rs:93`）。REST は使い捨て registry を新規構築（`agents_messages.rs:118`）。
   - > **更新（#158 S3、2026-07）**: 転記型の逆依存は解消済み。`DiscordReplyContext` / `InteractionRecord` は `opencrab_actions::transcript` へ移設され（`AgentReplyContext` / `InteractionRecord`）、`crates/server/src/transcript.rs` は discord crate を参照しない。§4 S4 の当時の判断（下記）から方針が変わっている。
4. **ゲートウェイ非対称**: Discord だけが再注入を持ち、他は同期完結。共通能力のはずが1ゲートウェイの実装詳細に埋まっている。

---

## 3. 目標設計（複数案の比較）

### 3.1 導入する抽象（両案共通）

**(1) subtask ランタイム**（gateway 非依存）: `SpawnedSubtask`（webhook 抜きの中核）/ registry / spawn / abort(kill_on_drop) / DB への subtask ログ記録 / cancel。

**(2) 完了通知の抽象 `SubtaskCompletionSink`**（`LoopEvent` 直依存を置換）。**本文は運搬しない**（§1.3）:

```rust
pub trait SubtaskCompletionSink: Send + Sync {
    /// 親セッションのエージェントを resume して subtask 結果を会話へ再注入する。
    /// 本文は DB 永続化済み。sink には resume 判断に要る最小情報だけ渡す。
    fn on_subtask_settled(&self, ev: SubtaskSettled);
}

pub struct SubtaskSettled {
    pub session_id: String,   // 親セッション
    pub agent_id: String,
    pub subtask_id: String,
    pub exit_reason: String,  // completed / error / timeout / stopped_by_limit / progress
    // 返信ルーティングは載せない（§3.1(4) — runtime が registry から引いて sink へ渡す）。
}
```

- ランタイムは `Arc<dyn SubtaskCompletionSink>` を保持し、**DB 永続化の後に** `on_subtask_settled` を呼ぶだけ。**`LoopEvent` を知らない**。
- **再注入は全 GW 普遍・配送は GW 別**（オーナー確定）: sink 実装が「resume＋その GW の配送口」を担う。
  - **Discord**: `event_tx.send(LoopEvent::SubtaskCompleted{...})` に変換（`channel_id`/`guild_id` は §3.1(4) の `reply_target` から。既存 mpsc ループ・同一セッション直列化は温存）。
  - **Nostr**: 同一セッション直列を担保しつつ `run_agent_response` を回し、結果を `cli.reply(..., reply_target, ...)` で配送（§3.1(4)・§6）。
  - **REST / heartbeat**: resume は同様に行い、**配送は「保存して取得（将来 SSE/webhook push）」**（REST）/「次 tick 拾い or 保存」（heartbeat）。**再注入を省略しない**（1ターン打ち切りにしない）。

**(3) sub-engine を合成 gateway 化する（P0 = 設計スパイク S2）**:
- 子が root gateway を辿れるよう、`GatewayCallContext` に **`root_gateway: Arc<dyn GatewayActions>`** を足す等の**注入 API を実装前に設計・合意**する。自己参照 Arc（gateway が自分自身を ctx に載せる）になるため、**構築順序 / `Weak` の要否 / Arc サイクルの回避**を含めて設計する。
- **セキュリティは deny-by-default を維持**（P0）: 「許可リスト維持」と「server ツール到達」は素直には両立しない。解は **deny-by-default ラッパを合成 gateway の“外側”に置き、開放する server ツールを個別列挙**（例: `["report_progress", "nostr_generate_key"]`）＋**ツール別リスク triage**。
  - 二層強制の現実を踏まえる: bridge の `DISCORD_ACTIONS`（`crates/actions/src/bridge.rs:52`）は 28 中一部しかブロックせず、`send_ui` / `discord_channel_config` 等は**許可リスト（`subtask_engine.rs:28`、コメント `subtask_engine.rs:17-27`）だけで守られている**。加えて `OWNER_ONLY_ACTIONS`（`bridge.rs:80`）/ `TRUSTED_ONLY_ACTIONS`（`bridge.rs:99`）/ `MAX_DEPTH=2`（`bridge.rs:77`）がある。
  - 合成後の**アクション和集合**に対し、この deny-by-default フィルタ（明示許可リスト）を最外周で強制する。開放は1ツールずつ triage して足す。

**(4) 返信ルーティングは spawn 時に捕捉する（P0）**:
- settle 時に session_id から導出する方式は **Nostr で破綻**（session_id に返信先イベントIDが無い）。webhook（`webhook_tx`）同様、**`SpawnedSubtask` に gateway 不透明な `reply_target`（enum or opaque bytes）を持たせ、spawn 時に確定**する。
- runtime は settle 時に registry から `reply_target` を引いて sink へ渡す（`SubtaskSettled` 自体は最小のまま）。
- **Nostr はブロッカーではない**: 返信先は `llm_logs.trigger_message_id`（`RunRequest::with_trigger_message_id`＝`crates/actions/src/run_request.rs:78`、Nostr は `crates/nostr/src/manager.rs:322` で設定、`crates/server/src/process.rs:912` で llm_logs へ永続化）から復元でき、`reply` の宛先（`event.rs:35` の `reply_target`）に使える。

### 3.2 案A（推奨）: `crates/actions` に subtask ランタイム + sink trait

- **配置**: ランタイム（registry / 中核 `SpawnedSubtask` / spawn / abort / cancel）と `SubtaskCompletionSink` / `SubtaskSettled` を `crates/actions` の新モジュール（例 `actions::subtask`）へ。
- **根拠**: `actions` は discord/nostr/mcp すべての下位依存（`crates/actions/Cargo.toml:12-14`）で、既に `BridgedExecutor` / `RunRequest` / `ActionContext` を所有し sub-engine を構築できる。core（SkillEngine）と gateway（GatewayActions）にも依存済み。→ **新 crate を足さずに層違反を解消**（「最小」の軸）。
- **依存方向**: `actions`（ランタイム + trait）← discord/nostr が sink を実装 ← server が配線。**server→discord のサブタスク型 import が消える**。
- **移行容易性**: 中〜大（テスト新規作成含む＝§6）。段階移行しやすい（§4）。

### 3.3 案B: 専用 crate `opencrab-subtask`

- `core/actions/gateway/db` に依存する新 crate に集約。discord/nostr/server が依存。
- **利点**: 関心分離が明確。将来 subtask が肥大（#142/#143）しても閉じられる。
- **欠点**: 新 crate のボイラープレート・ビルド時間・依存グラフに層追加。現規模では過剰（「過剰に大きくしない」ガードに抵触しうる）。

### 3.4 比較と推奨

| 観点 | 案A（actions 内） | 案B（専用 crate） |
|---|---|---|
| 層違反の解消 | ○ | ○ |
| 新規サーフェス | 小（モジュール追加） | 中（crate 追加） |
| server→discord 逆依存の除去 | ○ | ○ |
| 「最小」ガード適合 | ◎ | △ |
| 将来 subtask 肥大時 | actions が太る | 分離済みで有利 |

→ **推奨は案A**。モジュール境界を綺麗に保てば、将来 subtask が肥大したとき案B（crate 切り出し）へ低コストで再リファクタできる。

---

## 4. 移行計画（段階・ビルド緑を保つ）

各段は**単独でビルド緑 & 既存 Discord 挙動不変**を完了条件とする。**load-bearing 挙動のテストは main に無いため、移行の前に新規作成する**（§6）。

- **S0. 先行テスト作成（振る舞い変更なし）**
  - main の spawn_subtask/cancel/report_progress について、再注入順序（DB 永続化→通知）/ 許可リスト強制 / progress 世代カウンタ / insert-before-run ゲート / cancel 相当の**特性テストを新規に書く**（`subtask_engine.rs:555-558`＝開始ゲート、`subtask_engine.rs:943-967`＝progress 世代）。
  - 完了条件: 追加テストが main 挙動で緑。
- **S1. ランタイム移設（Discord を新抽象へ載せ替え・挙動不変）**
  - registry / 中核 `SpawnedSubtask`（**webhook フィールドを分離**＝`WebhookConfig`/`DeliveryBatch` の持ち上げ or 随伴構造化。§1.5）/ spawn / abort / cancel / DB ログ記録を actions へ移動。
  - discord は `DiscordCompletionSink`（event_tx→`LoopEvent::SubtaskCompleted` 変換、`parse_discord_session` はここに残す）を実装しランタイムへ注入。`send_subtask_completed_event` は sink 呼び出しへ置換。
  - 完了条件: Discord の subtask/cancel/report_progress が現状と同一挙動（S0 テスト＋実機で spawn→completion→再注入→送信を確認）。
- **S2. sub-engine を合成 gateway 化（**設計スパイク**・P0）**
  - `GatewayCallContext` への `root_gateway` 注入 API（自己参照 Arc の構築順/`Weak`）と、deny-by-default 最外周フィルタ（開放ツールの個別列挙＋triage）を**先に設計・合意**してから実装（§3.1(3)）。
  - 完了条件: subtask 内から許可した server ツール（例 `nostr_generate_key`）のみ実行可能。`send_ui`/`discord_channel_config` 等が引き続き遮断されることをテストで固定。
- **S3. 他ゲートウェイ配線（全ゲートウェイ対応）**
  - Nostr/REST/heartbeat 用の sink を実装。**再注入は全 GW で実装**、配送のみ GW 別（Discord=send_to_channel / Nostr=reply / REST=保存＋取得 / heartbeat=次tick or 保存）。Nostr sink は**同一セッション直列を担保**（§6(a)）。ツール宣言/dispatch を gateway 非依存へ移す（§6(b)）。REST の使い捨て registry 構築（`agents_messages.rs:118`）を actions ランタイム経由へ。
  - 完了条件: 少なくとも Nostr で spawn→completion→再注入→reply が動く。REST/heartbeat は resume が走り結果が会話へ入る。
- **S4. 旧結合の除去**
  - server の `opencrab_discord::SubtaskRegistry` 参照（`main.rs:565` / `agents_messages.rs:118`）を actions 由来へ置換。`DiscordReplyContext`（`transcript.rs:93`）等 Discord 固有の転記型は Discord 側に残す。
  - 完了条件: server が discord のサブタスク**ランタイム型**に依存しない。
  - > **更新（#158 S3、2026-07）**: 転記型を Discord 側に残す判断は**取り消された**。本 RFC 時点では「Discord 固有」と見なしていたが、実際には `DiscordReplyContext` の 3 variant は「このターンが何で起動されたか」（直接の発話 / サブタスク完了 / A2UI 応答）を表すだけで transport 依存の型を含まず、`InteractionRecord` も同様だった。この 2 型が discord crate に居るせいで server の転記関数が `#[cfg(feature = "discord")]` 配下に落ち、discord feature を切ると Nostr とまったく同じ形の記録まで消えていた。
    >
    > **現在の所在**: `opencrab_actions::transcript`（`AgentReplyContext` / `InteractionRecord` / `InboundMessageRecord` / `OutboundReplyRecord` / `TranscriptSource`）。記録メソッドは `AgentRunner` / `NostrAgentRunner` から `opencrab_actions::AgentRuntime`（`record_inbound_message` / `record_outbound_reply` / `record_interaction_response`）へ移り、`crates/server/src/transcript.rs` の transport 別関数は統合されて機能フラグ配下から出た。記録される `metadata_json` はバイト等価（種別文字列は列挙型が保持）。

---

## 5. 既存/関連能力の扱い（fix 版は破棄・新基盤上で再設計）

**重要**: #144/#150/#151 の未コミット fix 版（`BackgroundDispatch` / `dispatch_pending_tool_calls` / `subtask_dispatch` / `cancel_job` / `cancel_running_tools` / `RunningJob`）は**プロトタイプとして破棄済み**（§0.1）。以下は**能力を新基盤（§3〜§4）の上で新規設計する**方針であり、fix 版の移設ではない。

| 能力 | 内容 | 新基盤での設計方針 |
|---|---|---|
| **#144** | 全ツール非ブロック（受信非ブロック / 回答生成のみ直列 / ツール実行はロック外 / 完了で再注入） | 「ツールを yield してロック外で実行し、完了で resume」を **actions ランタイム＋`SubtaskCompletionSink` の上に新規実装**。**全 GW で再注入**、配送は GW 別。二重回答は §6 の順序契約で担保。 |
| **#150** | エージェント駆動の停止（停止語ハードコード撤廃、エージェントがキャンセルツールを呼ぶ） | ランタイムの cancel を叩く**エージェント向けキャンセルツール**を新設。gateway 非依存（全 GW で使える）。 |
| **#151** | 個別/クロスセッション cancel/list（job_id 露出・巻き添え防止） | ランタイム registry に job_id/label を持たせ、`cancel_job(job_id)` / `list` を新設計。所有権ゲート（session/agent 一致）を最初から設計に含める。 |
| **#142** | 走行中サブへの steer（インバウンド指示） | `SpawnedSubtask` は現状アウトバウンド `webhook_tx` のみ（`mod.rs:46`）。ランタイムが1箇所に集約されるので**インバウンド tx の実装先が明確化**。本 RFC は土台のみ用意（実装は別 issue）。 |
| **#143** | 親コンテキスト継承 `inherit_context` | sub-engine 構築がランタイムへ集約されるので、継承オプションの実装先が1箇所に。土台のみ用意（実装は別 issue）。 |

---

## 6. リスク・未解決点・受け入れ基準・テスト戦略

### 二重回答の順序契約（受け入れ基準に格上げ）
- **sink 呼び出しは DB 永続化の後**（`subtask_completed` ログ書き込み後に `on_subtask_settled`）。
- **resume は DB からのみ会話再構築**（`build_conversation_string`、`message_loop.rs:746`）。完了本文はイベントで運ばない（`_result` 不使用 = `message_loop.rs:710`）。
- **温存する不変条件**: 同一セッション直列化（Discord は `spawn_serialized_on_session` = `message_loop.rs:656`）/ insert-before-run 開始ゲート（`subtask_engine.rs:555-558`, 解放 `:692`）/ progress 世代カウンタ（`subtask_engine.rs:943-967`）/ 完了時の debounce 除去。

### Nostr の真の難所（実装前に要合意）
- **(a) per-session 直列化が無い**: Nostr は `spawn_serialized_on_session` 相当を持たない（`crates/nostr/src` に session lock 無し）。1イベント=1推論の同期完結（`manager.rs:324`）。→ 非同期 resume を足すと #144 由来の二重回答不変条件が破れうる。**再注入 sink は同一セッション直列を自前で担保する要件**。
- **(b) ツール露出面が Discord 専用**: `spawn_subtask`/`cancel_subtask`/`report_progress` の宣言・dispatch は `DiscordGatewayActions`（`mod.rs:503-559, 1017-1021`）にある。全 GW で使うには**ツール定義を gateway 非依存の層へ移す作業**が要る。

### リスク
- **大規模リファクタ**: `subtask_engine.rs` は本体＋テストで 2008 行。webhook 分離（§1.5）を含む。
- **合成 gateway 化のセキュリティ**（P0）: deny-by-default 最外周フィルタと triage を外すと `send_ui`/`discord_channel_config` 等が subtask から到達可能になる。allowlist だけで守られている事実（`subtask_engine.rs:17-27`）を必ず踏まえる。

### 未解決点（現時点で**未確認** / 要レビュー判断）
- **root gateway 注入の具体形**: `GatewayCallContext` へのフィールド追加が全 `execute` 実装へ波及する影響範囲（`crates/gateway/src/traits.rs:40-47`）。自己参照 Arc の `Weak` 要否は未確認 — S2 スパイクで確定。
- **REST の配送**: 同期 HTTP 応答後に完了した subtask の結果を、保存（DB）だけで足りるか、SSE/webhook push を今回入れるかは設計判断（最小は保存＋取得）。
- **予算圧下の再注入保証**: 完了本文が大きいとき、`build_conversation_string` のコンパクションで会話から溢れないか（確実に直近の `subtask_completed` が入る保証が要る）を S1 で実証する。

### テスト戦略（新規作成が前提・「移設」ではない）
- **先行特性テスト（S0）**: load-bearing 挙動（再注入順序 / 許可リスト強制 / progress 世代 / insert-before-run / cancel）は main にカバレッジが無いため、**移行前に新規作成**して現挙動を固定する。
- **抽象の unit テスト**: フェイク `SubtaskCompletionSink` で「completion → on_subtask_settled が1回」「DB 永続化→通知の順序」「cancel で registry 除去」「開始ゲートでリークしない」。
- **ゲートウェイ結合テスト**: Discord = spawn→completion→再注入→send_to_channel（現状維持）。Nostr = spawn→completion→resume（同一セッション直列）→reply。
- **セキュリティテスト**: 合成 gateway 化後、`nostr_generate_key` は到達可・`send_ui`/`discord_channel_config` は遮断、を固定。

---

## 7. スコープ外（今回やらないこと）

- **#142 steer / #143 inherit_context の実装本体**（土台のみ用意）。
- **runner 統一（actions 側 `SessionResumer` trait への `AgentRunner`/`NostrAgentRunner` 統合）**（§1.6。将来余地）。
- **案B（専用 crate 切り出し）**（案A 完了後に必要になれば別途）。
- **A2UI `InteractionResponse`**（`LoopEvent` の別バリアント `message_loop.rs:54`）の抽象化。subtask とは別関心。
- **voice / VC 経路**。
- **既存セッションID 形式の変更**（`discord-*` / `nostr-*` / `heartbeat-*` のパース規約はそのまま利用）。

> 注: §1.5 のとおり **webhook（`WebhookConfig`/`DeliveryBatch`）の分離は S1 の不可分な一部**であり、スコープ外ではない（v1 の「webhook 直交でスコープ外」は撤回）。

---

## 8. 決定事項（レビュー後確定）

第三者レビュー3件とオーナー判断を経て、以下を確定する（PR #153 v2 で反映）。

1. **配置 = 案A**: subtask ランタイム + `SubtaskCompletionSink` を `crates/actions` の**自己完結モジュール `actions::subtask`** に置く。将来 subtask が肥大した場合の案B（専用 crate）への切り出しを低コストに保つため、モジュール境界を crate 抽出可能な形に保つ。
2. **REST の配送 = 最小（保存 → 取得/poll）**。ダッシュボードのライブ会話は **#154 web gateway（SSE/WebSocket）** が担い、生 REST エンドポイントに live push は積まない（冗長回避）。§6 の REST 記述はこれに従う。
3. **root gateway 注入の具体形は S2 スパイクで確定**（`GatewayCallContext` への `root_gateway: Option<Arc<dyn GatewayActions>>` 追加 vs sub-engine builder 経由、自己参照 Arc の構築順/Weak 要否）。plan フェーズでは未確定のままでよい＝オーナーが今決める分岐ではない。
4. **基準 = main から新規**（fix版 #144/#150/#151 は破棄。§0.1 参照）。
5. **実装は S0→S4 を段階実装**し、各段「ビルド緑・既存挙動不変」＋レビューを完了条件とする。

### 本プログラムにおける位置づけ
本 RFC（#152）は統括 #155「discord crate の runtime 化解消」の第一歩。確立する「gateway 非依存の完了再注入抽象」「合成 gateway 注入」は、後続の #156(AgentRunner 脱Discord)/#157(汎用ツール移設)/#158(共有経路)/#159(命名) と #154(web gateway) の共通土台になる。
