# 設計: エージェント単位のツール実行 Webhook（全ツール活動ストリーミング）

## 目的

サブタスク実行中の**すべてのツール呼び出し**（`execute_shell` を含む全ツール）を、エージェント単位で設定した Webhook へストリーミング配信する。
現状のライフサイクル Webhook（started / completed / failed / timed_out / aborted）と粗い `subtask_progress` だけでは、エージェントが「いま何を実行しているか」を外部から観測できない。本設計は `tool_call_started` / `tool_call_completed` / `tool_call_failed` を個別ツール粒度で配信し、特に `execute_shell` については exit code / stdout / stderr のサマリを安全に届ける。

対象: `crates/discord`, `crates/actions`, `crates/core`, `crates/db`。
非対象: メインエンジン（depth 0）からの Discord 直接送信フロー（既存のまま）。

---

## 1. 現状アーキテクチャの調査結果

### 1.1 ライフサイクル Webhook の設定箇所・送信箇所

- **設定の入口**: `crates/discord/src/gateway_actions/webhook.rs`
  - `WebhookConfig { url: String, events: Option<Vec<String>> }`
  - `WebhookConfig::from_args(args)` が `spawn_subtask` の引数 `args["webhook"]`（`{ "url": ..., "events": [...] }`）からパースする。`events` 省略時は全イベント送信。`url` 空/欠落で `None`。
  - `wants(event)` でフィルタ。`subtask.` プレフィックスを正規化し、`started` 指定時は後方互換で `progress` も許可。
- **配送ワーカー**: `webhook.rs::spawn_run_worker(client)` が **run ごとに** `mpsc::UnboundedSender<DeliveryBatch>` と 1 ワーカーを起動。`DeliveryBatch { url, messages }` を受け取り、`send_with_retry` で**直列**送信する。
  - 同一 run 内は 1 チャネル + 1 ワーカーで**順序保証**。別 run のワーカーとは並行 → run をまたぐと interleave しうる。
  - メッセージ整形: `build_started_messages`（メタ + raw task の chunk）、`build_terminal_message`（概要 1 通）、`build_progress_message`（短い 1 通）。`DISCORD_CHUNK_LIMIT = 1900`、ハード上限 2000。
  - 429 は `Retry-After` を尊重し attempt を進めず再送（順序維持）。それ以外は `[0,2,10,30,120]` 秒の best-effort backoff。
  - ログでは `redact_webhook_url`（最終パスセグメントを `[redacted]` 化）で URL を秘匿。
- **emit 箇所**: `crates/discord/src/gateway_actions/subtask_engine.rs`
  - `execute_spawn_subtask` が `from_args` で取得 → `spawn_run_worker` 起動 → `started` 送信。終端は spawn した closure 内で `exit_reason_to_status` により completed/failed/timed_out を送信。`execute_cancel_subtask` が aborted を送信。
  - `sub_session_id = "subtask-{UUID}"`。`subtask_spawned` / `subtask_completed` / `subtask_progress` を親セッションログに記録。
  - `SkillEngine::set_on_tool_call` / `set_on_tool_result` を**粗い** `subtask_progress` 用にのみ使用（`summarize_tool_calls` でツール名の羅列、結果は 500 文字 preview）。

### 1.2 ツール呼び出しのディスパッチ・実行箇所

- **エンジンループ**: `crates/core/src/engine/skill_engine.rs`
  - assistant がツール呼び出しを返すと `on_tool_call(assistant_content, tool_calls_json)` を**ターンにつき 1 回**呼ぶ（個別 call ではなく全 call をまとめた JSON）。
  - 各 `tool_call` を `self.executor.execute(&name, &arguments)` で実行 → `ActionResult` を JSON 化 → `on_tool_result(tool_call_id, tool_name, result_json, is_error)` を**個別 call ごと**に呼ぶ。
  - 権限拒否時も `on_tool_result(..., is_error=true)` を呼ぶ。
  - **重要な制約**: 現コールバックは「開始＝ターン単位（個別 args/開始時刻なし）」「完了＝個別 call 単位（id/name/result/is_error のみ、duration なし）」。個別ツールの**開始時刻・所要時間・呼び出し引数**を厳密に取りたければ executor 層が必要。
- **executor ブリッジ**: `crates/actions/src/bridge.rs`
  - `BridgedExecutor::execute(name, args)` は `ActionDispatcher` を先に試し、未知なら `GatewayActions` にフォールバック。フォールバック時に `__caller` / `__session_id` / `__depth` / `__agent_id` を args に注入。
  - **ここが per-tool 計測の最適点**: `execute` の前後で開始/完了/失敗（id・name・args・duration・exit code）を一元的に instrument できる。depth も `self.depth` で保持済み。

### 1.3 ツール定義

- `crates/actions/src/dispatcher.rs`: コアアクション登録。
- `crates/actions/src/tools/mod.rs`: config 駆動の shell 登録（`register_tools_from_config`）。
- `crates/actions/src/tools/shell.rs`: `execute_shell`。返り値は `stdout` / `stderr` / `exit_code` / `truncated`。`max_output_bytes` 既定 65536、env は `ShellToolConfig` 依存。

### 1.4 エージェント設定の保存・読込箇所

- `crates/db/src/schema.rs`
  - `agents(agent_id, name, ..., instructions, heartbeat_instructions, model, metadata_json, ...)`
  - `agent_discord_config(agent_id PK, bot_token, owner_discord_id, enabled, updated_at)`
  - `discord_channel_config(channel_id, agent_id, ... heartbeat_enabled, heartbeat_interval_secs, heartbeat_instructions)`（PK は `(channel_id, agent_id)`）
- `crates/db/src/queries.rs`: `AgentRow` / `AgentPatch` と `get_agent` / `upsert_agent` / `apply_agent_patch`、`AgentDiscordConfigRow`、channel config クエリ群。
- スキーマ migration は `pragma_table_info` で列存在チェック → `ALTER TABLE ADD COLUMN` の冪等パターン。

---

## 2. 提案データモデル

### 2.1 エージェント単位 Webhook 設定

エージェントに紐づく Webhook 設定（本設計のツール活動配信、および subtask ライフサイクル既定）の URL とイベントフィルタを永続化する。`spawn_subtask` 引数の都度指定とは独立に、**エージェント既定**として常時有効化できることが狙い。スコープ（agent / tool / global）と種別（`kind`）は同一テーブルで表す（2.3 参照）。

**採用案: 専用テーブル `agent_webhook_config` を新設**（`agents.metadata_json` への埋め込みではなく）。理由:
- 複数 URL（ライフサイクル用・ツール用で別チャンネル）を将来許容しやすい。
- `enabled` / イベントフィルタ / 出力ポリシーを構造化列で持て、`metadata_json` の肥大化と読み書き競合を避ける。
- migration が `ALTER` ではなく `CREATE TABLE IF NOT EXISTS` で完結（既存テーブルに触れない＝後方互換）。

スコープ・種別・出力ポリシーをすべて含む最終的な概念スキーマを 1 か所にまとめる（2.3 で別途列を追加しない）:

```sql
CREATE TABLE IF NOT EXISTS agent_webhook_config (
    scope        TEXT NOT NULL DEFAULT 'agent',   -- 'agent' | 'tool' | 'global'
    agent_id     TEXT NOT NULL,                   -- global は '*'
    tool_name    TEXT NOT NULL DEFAULT '',        -- scope='tool' のとき使用。未使用は ''（NULL を避けキーを一意化）
    kind         TEXT NOT NULL DEFAULT 'subtask', -- 'subtask'（ライフサイクル既定）| 'tool'（ツール活動配信）| 'lifecycle'（'subtask' の互換別名）
    url          TEXT NOT NULL,
    events_json  TEXT,                            -- ["tool_call_started", ...] / NULL=全件
    enabled      INTEGER NOT NULL DEFAULT 1,
    name         TEXT,                            -- list/ensure 用の表示名。例: 'default-subtask-progress'
    created_by   TEXT,
    -- 出力ポリシー（4章参照）
    output_mode  TEXT NOT NULL DEFAULT 'summary', -- 'summary' | 'off' | 'full'(opt-in)
    max_chars    INTEGER NOT NULL DEFAULT 1500,
    updated_at   TEXT NOT NULL,
    PRIMARY KEY (scope, agent_id, tool_name, kind)
);
CREATE INDEX IF NOT EXISTS idx_agent_webhook_agent ON agent_webhook_config(agent_id);
```

> キー設計メモ: 一意キーは `(scope, agent_id, tool_name, kind)`。`tool_name` と `agent_id` は NULL を許さずセンチネル（未使用は `''`、global は `agent_id='*'`）で表し、NULL 同士が一意制約上は別物と扱われる SQLite の挙動を避ける。これにより上記スキーマはそのまま migration 可能（新テーブルのみ、既存テーブルは不変）。

`queries.rs` に追加: `AgentWebhookConfigRow` / `get_agent_webhook_config(scope, agent_id, tool_name, kind)` / `upsert_agent_webhook_config(...)` / `list_agent_webhook_config(...)`。

> 代替案（不採用）: `agents.metadata_json` に `{ "tool_webhook": {...} }` を埋める。スキーマ変更不要だが、構造化フィルタ・index・将来の複数 URL に弱く、ダッシュボード編集時の JSON 全置換リスクがある。最小実装を最優先するなら fallback として許容。

### 2.2 デフォルト Subtask Webhook と解決順

`spawn_subtask.webhook` を省略した場合でも、エージェントが既存の通知先を理解・再利用できるように、subtask 用の既定 Webhook を明示的な解決対象にする。`02940b7 feat: add default subtask webhook config` の config/env 既定値は後方互換の fallback として残すが、それだけを正にしない。エージェントが gateway action/tool で確認・設定・再利用できる永続設定を正とする。

解決順は以下で固定する。**明示的な作業指示なしのフォールバックは禁止**とし、設定エラーを別の設定で隠蔽しない。

1. `spawn_subtask` 引数 `args["webhook"]` が指定されていれば、それをその run の**ライフサイクル + subtask 由来のツール観測イベント**の宛先として最優先（現行挙動を壊さない）。
2. 引数指定が無い場合、tool-specific の subtask Webhook 設定を参照する。例: `scope='tool'` + `agent_id` + `tool_name='spawn_subtask'` + `kind='subtask'`。
3. tool-specific が無い場合、agent-specific の subtask Webhook 設定を参照する。例: `scope='agent'` + `agent_id` + `kind='subtask'`。
4. agent-specific が無い場合、global/default subtask Webhook を参照する。
5. DB に該当行がまったく無い場合のみ、既存の env/config fallback（`02940b7` の設定）を読む。
6. どれも無い場合は Webhook なしで実行する。

エラー時の扱い:

- 明示 `spawn_subtask.webhook` が空/不正 URL の場合は fallback せず、`spawn_subtask` 自体を `invalid_webhook_url` で失敗させる。エージェントが指定した webhook のミスは、エージェントが気付ける形で返す。
- DB 設定行が存在するが URL が空/不正な場合は fallback せず、`invalid_default_webhook` として失敗させる。設定バグを env/config で隠さない。
- `enabled=false` は「この scope で明示的に無効化した」という意味にする。下位 fallback へ進まず、その scope の解決結果は Webhook なしとする。停止意図を env/config fallback で復活させない。
- env/config fallback は移行互換のための最後尾読み取り専用経路であり、DB に該当行が無い時だけ使う。DB 行の不正値・無効化・配送失敗の救済には使わない。

明示指定と subtask default（`kind='subtask'` / `kind='lifecycle'`）を同時に送る挙動にはしない。`spawn_subtask.webhook` は「この run だけ subtask 通知先を上書きする」意味であり、重複通知を避ける。一方、全ツール活動 Webhook（`kind='tool'`）を別チャンネルへ流す既存設計は独立した観測ストリームとして維持し、同じ URL へ解決された場合だけ URL で de-dupe する。つまり上記の単一勝者ルールは subtask default 解決に適用し、恒久的な `kind='tool'` 配信を暗黙に無効化しない。

実装上は `WebhookConfig` を拡張し「複数宛先 + 出力ポリシー」を表せる新型 `ResolvedWebhooks { targets: Vec<WebhookTarget>, diagnostics: Vec<WebhookDiagnostic> }` を導入。`WebhookTarget { url, events, output_mode, max_chars, source }`。`source` は `explicit` / `tool_default` / `agent_default` / `global_default` / `env_config` のいずれかで、ログや通常レスポンスには URL ではなく source と redacted URL のみを出す。`from_args` は引数由来の 1 ターゲットを返し、別関数 `resolve_for_agent(db, agent_id, tool_name, args)` が上記順序で解決する。既存 `WebhookConfig` は後方互換のため残置（内部で `WebhookTarget` に変換）。

### 2.3 既定 Webhook の永続化と再利用ポリシー

subtask 既定値は新規テーブルを増やさず、2.1 の `agent_webhook_config` でそのまま表現する。URL・イベントフィルタ・出力ポリシー・enabled・更新時刻を同じ API で扱えるためで、2.1 のスキーマは scope/agent/tool/global を区別する列（`scope` / `tool_name` / `name` / `created_by`）を最初から含む。

キーの使い分けは 2.1 の `PRIMARY KEY (scope, agent_id, tool_name, kind)` に従う:

- global: `scope='global'` / `agent_id='*'` / `tool_name=''`
- agent-specific: `scope='agent'` / `agent_id=<agent>` / `tool_name=''`
- tool-specific: `scope='tool'` / `agent_id=<agent>` / `tool_name='spawn_subtask'`

`kind` は配信対象を表し、`subtask`（started/completed/failed/timed_out/aborted のライフサイクル既定）、`tool`（本設計のツール活動配信）、`lifecycle`（`subtask` の既存互換別名）のいずれか。

> 用語注意: `scope='tool'`（gateway action の `tool_name` で引く tool-specific 設定の粒度）と `kind='tool'`（ツール活動ストリーミングという配信種別）は別概念。`scope` は「どの粒度の設定か」、`kind` は「何を配信するか」を表す。

再利用方針:

- `ensure_subtask_webhook` は、解決順で使える既存 default があれば `discord_create_webhook` を呼ばず、その設定を返す。
- 新規作成は「有効な既定が無い」「呼び出し元が `CallerIdentity::Owner`」「対象チャンネルが明示されている」「呼び出しが `ensure_subtask_webhook` である」場合だけに限定する。通常の `spawn_subtask` 解決中に暗黙作成しない。
- 同じ用途で毎回 Discord Webhook を作ると、Webhook token の流通量が増え、チャンネルの Webhook 一覧が散らかり、監査・削除・漏洩時のローテーションが難しくなる。既定再利用を標準にして、token 数・通知ノイズ・チャンネル clutter を抑える。
- 空文字、URL として parse できない値、Discord Webhook 形式として不正な値は disabled 相当にしない。設定ミスとして扱い、呼び出し元にエラーを返す。

`02940b7` の env/config fallback は「DB 設定がまだ無い初期導入」「移行前の既存運用」を支える読み取り専用の最後尾 fallback とする。agent が `set_default_subtask_webhook` を実行した後は DB の値が優先され、env/config は上書きしない。env/config fallback が使われた場合も silent にせず、`webhook_source='env_config'` を返す。

### 2.4 Agent-facing Gateway Actions / Tool API

エージェントが default を理解・管理できるよう、gateway action として以下を公開する。通常レスポンスは必ず redacted URL を返し、raw URL/token は明示的に認可された場合だけ返す。

- `get_default_subtask_webhook(args)`:
  - 入力: `agent_id?`, `tool_name?`, `scope?`, `include_secret?=false`
  - 解決順に従い、現在使われる default の `scope` / `source` / `enabled` / `events` / `redacted_url` / `updated_at` / `diagnostics` を返す。
  - 通常は raw URL を返さない。`include_secret=true` は原則拒否し、どうしても必要な場合は owner 専用の別 action と監査ログ付きで設計する（本フェーズでは非対応）。
- `set_default_subtask_webhook(args)`:
  - 入力: `scope`, `agent_id?`, `tool_name?`, `url?`, `enabled?`, `events?`, `output_mode?`, `max_chars?`
  - 権限:
    - `CallerIdentity::Owner` は全 scope（agent / tool / global、および任意の `agent_id`）を set/disable できる（従来どおり）。
    - `CallerIdentity::Agent` は**自分自身の agent scope の default subtask webhook のみ** set/disable できる。すなわち `scope='agent'` かつ `agent_id` が呼び出し元 agent 自身の場合に限り許可する。`scope='tool'` / `scope='global'`、および自分以外の `agent_id` への set/disable は拒否する（`forbidden_scope` 等のエラーを返す）。
    - `trusted_user` / `co_agent` は set/disable できない（読み取り redacted のみ。既存方針を維持）。
  - 自身の agent scope を対象にする場合、`agent_id` 省略時は呼び出し元 agent を既定とし、明示指定時は呼び出し元 agent と一致することを検証する。
  - URL が空なら対象 scope の DB default を disabled にする（削除ではなく監査可能な無効化）。Agent は自分の agent scope の default を、URL を渡して自分で設定でき、空 URL で自分で無効化もできる。
  - URL は保存前に parse/形式検証し、ログ・レスポンスには redacted URL だけを出す。raw URL/token は通常レスポンスに含めない。
- `ensure_subtask_webhook(args)`:
  - 入力: `scope?`, `agent_id?`, `tool_name?`, `channel_id?`, `name?`, `events?`
  - **読み取り/再利用パス**: 既存の有効な default があればそれを返す。これは redacted-read 扱いで、当該 agent の owner / trusted_user / co_agent に許可する既存の読み取り権限のまま（raw URL/token は返さない）。`CallerIdentity::Agent` も自分の agent scope の既存 default を解決・再利用できる。
  - **新規作成パス（Discord Webhook 作成）**: 有効な default が無く新たに `discord_create_webhook` を呼んで Discord 上に Webhook を作る場合は、`CallerIdentity::Owner` のみに限定する（owner-only）。`CallerIdentity::Agent` による Discord Webhook 自動作成は、別途チャンネル束縛つきの安全な作成ルールを設計するまで許可しない。Agent が自分の default を持ちたい場合は、URL を渡す `set_default_subtask_webhook`（agent scope の自己設定）を使う。
  - `channel_id` 無しで新規作成しない。既存再利用だけなら `channel_id` は不要。
- `list_subtask_webhooks(args)`:
  - 入力: `agent_id?`, `scope?`, `include_disabled?=false`, `include_secret?=false`
  - DB に登録された subtask/tool/lifecycle Webhook 設定を一覧する。`redacted_url` のみ返し、raw token は返さない。

`spawn_subtask` の戻り値・親セッションログには、エージェントが通知状態を検知できるよう `webhook_source` / `webhook_redacted_url` / `webhook_status` / `webhook_error` を含める。明示 webhook の検証・started 配送で検出できた失敗は `spawn_subtask` のエラーとして返す。progress/completed など非同期配送で後から失敗した場合は、親セッションの `subtask_progress` または `subtask_completed` に warning/error として残す。

権限モデル:

- 読み取り（redacted）は当該 agent の owner、trusted_user、co_agent に許可する。共有文脈では raw URL を返さない。`trusted_user` / `co_agent` は読み取り（redacted）のみで、set/disable/create はできない。
- `CallerIdentity::Agent` は**自分自身の agent scope の default subtask webhook**（`scope='agent'` かつ `agent_id` が自分）を `set_default_subtask_webhook` で set/disable できる。URL を渡しての自己設定、空 URL での自己無効化を許可する。一方、`scope='tool'` の default、`scope='global'` の default、自分以外の agent の default は set/disable できない。
- Discord Webhook の新規作成（`ensure_subtask_webhook` の `discord_create_webhook` 作成部分）は `CallerIdentity::Owner` に限定する。通常 agent が勝手に別チャンネルへ Webhook を作らない。Agent の自己作成は、安全なチャンネル束縛ルールを別途設計するまで許可しない。
- `CallerIdentity::Owner` は全 scope（agent / tool / global、任意の agent）の set/disable/create を管理できる（従来どおり）。
- 上記いずれの権限でも、通常の read/list/get/ensure レスポンスで raw webhook token を返さない。
- すべての action は監査ログに `caller` / `scope` / `agent_id` / `tool_name` / `source` / `redacted_url` / `result` を記録し、raw URL/token は記録しない。

---

## 3. イベントモデル

### 3.1 イベント種別

| event | 発火点 | 説明 |
|---|---|---|
| `tool_call_started` | `BridgedExecutor::execute` 入口（推奨）/ `on_tool_call` | 各ツール実行の開始 |
| `tool_call_completed` | `BridgedExecutor::execute` 出口（success） | 正常終了 |
| `tool_call_failed` | `BridgedExecutor::execute` 出口（error / panic / 拒否） | 失敗・権限拒否 |
| `tool_output_chunk` | （任意・opt-in のみ） | 長い出力の分割中継。下記の正当化を満たす場合のみ |

`tool_output_chunk` の採否: **既定 off**。常時ストリーミングは Discord のレート制限を即座に飽和させ、秘匿漏れリスクも増す。正当化されるのは「`output_mode='full'` を明示 opt-in し、かつ単一ツール出力が `max_chars` を超える場合に限り part X/N で分割」のケースのみ。それ以外は completed の result summary に丸める。

### 3.2 共通フィールド（payload 概念スキーマ）

各イベントは以下を含む（Discord へは整形文字列、将来 JSON モード時はそのまま）:

```jsonc
{
  "event": "tool_call_completed",
  "agent_id": "crab",
  "subtask_id": "<UUID>",          // = run_id
  "session_key": "subtask-<UUID>", // sub_session_id
  "parent_session_id": "<親 session_id>",   // 取得可能なら
  "depth": 1,
  "tool_name": "execute_shell",
  "tool_call_id": "<call id>",     // started/completed の相関キー
  "started_at": "RFC3339",
  "ended_at": "RFC3339",           // completed/failed のみ
  "duration_ms": 1234,             // completed/failed のみ
  "status": "completed",           // started | completed | failed
  "args_summary": "cmd: `git status`",       // redact 後・短縮
  "result_summary": "exit=0, stdout 1.2KB",  // redact 後・短縮
  // execute_shell 専用拡張
  "exit_code": 0,
  "stdout_summary": "...先頭/末尾 N 文字...",
  "stderr_summary": "...",
  "truncated": false
}
```

- **相関**: `(subtask_id, tool_call_id)` で started ↔ completed/failed を対応付け。`tool_call_id` は `skill_engine` の `tool_call.id` を使用。
- **親参照**: `parent_session_id` は `subtask_registry` / セッション `metadata_json.parent_session_id` から解決可能な範囲で付与（不明なら省略）。
- `args_summary` / `result_summary` は**必ず redact 済み**の短縮表現（4 章）。`execute_shell` はコマンド・exit code・出力サマリを優先的に載せる。

### 3.3 発火点の選択

**推奨: `BridgedExecutor::execute` での instrument**。理由:
- 個別ツールの**開始時刻・所要時間・args・exit code** を 1 箇所で正確に取得できる（`on_tool_call` はターン単位で個別計測不可）。
- depth / `__agent_id` / `__session_id` が既に揃っている。
- shell の `exit_code` / `truncated` は `ActionResult.data` から構造的に読める。

`BridgedExecutor` に「ツールイベント sink」を注入する（`Option<Arc<dyn ToolEventSink>>`）。sink は `subtask_engine` 側で webhook ワーカーへ送る実装を渡す。`SkillEngine` のコールバックは現行の粗い progress 用に温存し、二重配信を避けるため細粒度配信は sink 経由に一本化する。

---

## 4. セキュリティ / Redaction

漏洩面が最も大きい機能なので保守的に倒す。

1. **Webhook URL のログ秘匿**: 既存 `redact_webhook_url` を全ログ経路で踏襲。新規ログにも適用。URL を payload 本文へ載せない。
2. **シークレット redaction（args / env / stdout / stderr / result）**:
   - 既知パターンを `[REDACTED]` 置換: `sk-...` / `ghp_` / `xoxb-` / Bearer トークン / `AKIA...`、`TOKEN` `SECRET` `PASSWORD` `KEY` `API` を含む KV、`https://discord.com/api/webhooks/...`、長い base64/hex 連。
   - env は**全体を生で出さない**。`execute_shell` の env は名前のみ（値は伏せる）か、そもそも payload に含めない（既定: 含めない）。
   - redaction は配信直前に一括適用する共通関数 `redact_secrets(&str) -> String` を `webhook.rs`（または新 `redact.rs`）に置き、started/completed/failed の全フィールドへ通す。
3. **出力長の既定上限**: Discord 1 通 2000 / `DISCORD_CHUNK_LIMIT=1900` を踏襲。`stdout_summary` / `stderr_summary` は既定で**先頭 N + 末尾 N 文字**（例 600+600）に丸め、`max_chars`（既定 1500）で全体をクランプ。`truncated=true` を明示。
4. **full 出力ストリーミングは opt-in のみ**: `output_mode='full'` を `agent_webhook_config` で明示した場合に限り `tool_output_chunk` を許可。既定 `summary`。`full` でも redaction は必ず通す（opt-in は「長さ」の解放であって「秘匿解除」ではない）。
5. **深さ/対象の最小化**: depth 0（メインエンジン）はツール Webhook 対象外（既存挙動維持）。sub-engine のみストリーミング。

---

## 5. 配送挙動

1. **既存ワーカーを再利用/拡張**: `spawn_run_worker` の `DeliveryBatch { url, messages }` をそのまま使う。複数宛先は宛先ごとに `DeliveryBatch` を分けて送る（URL 単位の直列性を維持）。run ごとに 1 sender を共有し、ライフサイクルとツールイベントを**同一チャネルへ**流すことで run 内順序を一貫させる。
2. **順序保証**: 同一 run（=同一 subtask）内は単一 mpsc + 単一ワーカーで FIFO。`started` → `tool_call_started` → `tool_call_completed` → `terminal` の相対順序が保たれる。run をまたぐ interleave は許容（既存仕様どおり）。複数 URL 宛先がある場合、各 URL への相対順序は保つが URL 間の同時性は保証しない。
3. **レート制限**: 既存 `send_with_retry` の 429 + `Retry-After` 尊重を流用。直列送信ゆえバースト時は自然に背圧がかかる。
4. **背圧 / 失敗ポリシー**: チャネルは `unbounded` のため送信側は決してブロックしないが、暴走防止に **per-run の簡易レート/件数ガード**を入れる（例: ツールイベントは 1 run あたり最大 N 件、超過分はカウンタ集約 `(+M more tool calls suppressed)` を 1 通だけ出す）。配信失敗は best-effort（既存 backoff、最終的に give up しエージェント実行はブロックしない）。**Webhook 配信失敗がサブタスク本体を失敗させてはならない**。
5. **Discord 整形（漏洩なし）**: `tool_name` / `exit_code` は inline code、コマンド・出力は redact 後にコードブロック相当へ。URL・トークンは載せない。1 通に収まらない場合のみ `output_mode` に従い chunk（part X/N）。整形関数は `webhook.rs` に `build_tool_event_message(event_meta) -> Vec<String>` として追加し、ユニットテストで上限・redaction を検証。

---

## 6. 実装計画（小さなフェーズ分割）

各フェーズは独立にビルド/テスト可能な小コミットにする。

**Phase 0 — redaction 基盤（純関数）**
- `crates/discord/src/gateway_actions/webhook.rs`（or 新 `redact.rs`）に `redact_secrets` / `summarize_shell_result` / `build_tool_event_message` を追加。
- テスト: 各シークレットパターン、長文クランプ、Discord 2000 上限、UTF-8 境界。
- 既存挙動への影響なし。

**Phase 1 — ツールイベント sink を executor に注入**
- `crates/actions/src/bridge.rs`: `ToolEventSink` トレイト（`started(meta)` / `completed(meta)` / `failed(meta)`）と `BridgedExecutor` への `with_tool_event_sink(...)` を追加。`execute` 前後で時刻計測し sink を呼ぶ。sink 未設定時は完全 no-op（既定）。
- `execute_shell` の結果から `exit_code` / `truncated` / 出力サマリを抽出するアダプタ。
- テスト: モック sink で started→completed/failed の発火順・フィールド・duration を検証。

**Phase 2 — subtask 側で sink を webhook ワーカーへ接続**
- `crates/discord/src/gateway_actions/subtask_engine.rs`: `sub_executor` 構築時に sink を注入し、run の `webhook_tx` へ `DeliveryBatch` を送る実装を渡す。`wants("tool_call_started"...)` でフィルタ。
- 既存の粗い `subtask_progress` は維持（重複を避けたい場合はフラグで抑制可）。

**Phase 3 — エージェント単位設定の永続化**
- `crates/db/src/schema.rs`: `agent_webhook_config` を `CREATE TABLE IF NOT EXISTS` で追加（既存テーブル不変）。
- `crates/db/src/queries.rs`: `AgentWebhookConfigRow` / get / upsert / list。subtask default 用に `scope` / `tool_name` / `name` / `created_by` を扱う。
- `subtask_engine` で `resolve_for_agent(db, agent_id, tool_name, args)` を実装し、引数指定、tool/agent/global default、env/config fallback を 2.2 の順で解決する。明示なし fallback は禁止し、不正値はエラーとして返す。
- テスト: 解決優先順位、enabled=false で下位 fallback しないこと、URL de-dupe、空/不正 URL が fallback されずエラーになること。

**Phase 4 — 出力ポリシーと full opt-in（任意）**
- `output_mode` / `max_chars` を尊重。`full` 時のみ `tool_output_chunk` を chunk 配信。
- per-run 件数ガードと suppression サマリ。

**Phase 5 — Gateway actions / API 露出**
- `get_default_subtask_webhook` / `set_default_subtask_webhook` / `ensure_subtask_webhook` / `list_subtask_webhooks` を追加。
- `set_default_subtask_webhook` は権限分岐を実装する: `CallerIdentity::Owner` は全 scope を set/disable、`CallerIdentity::Agent` は自分自身の agent scope（`scope='agent'` かつ `agent_id` が自分）のみ set/disable、それ以外（tool/global/他 agent）は拒否。`trusted_user`/`co_agent` は read のみ。
- `ensure_subtask_webhook` は既存 default 再利用を先に行い、新規 `discord_create_webhook` 作成は Owner が明示的に呼んだ場合だけに限定する。Agent の既存 default 再利用（読み取り）は許可するが、Agent による Discord Webhook 作成は許可しない。`spawn_subtask` 解決中の暗黙作成は禁止。
- エージェント編集 UI から `agent_webhook_config` を CRUD する場合も、同じ権限・redaction・監査ログのルールを通す。

### テスト戦略
- 純関数（redaction / 整形 / chunk / summary）はユニットテスト中心（既存 `webhook.rs` のテスト様式に倣う）。
- `BridgedExecutor` はモック sink + モックツールで発火を検証。
- 配信はモック HTTP（既存パターンが無ければ `send_with_retry` を関数注入可能にして検証）。
- 解決規則は in-memory DB（`init_memory`）で検証: `spawn_subtask.webhook` 省略時に default が使われる、明示 `spawn_subtask.webhook` が default より勝つ、tool-specific > agent-specific > global の順に勝つ、DB 行が無い時だけ env/config fallback が使われる。
- 明示 `spawn_subtask.webhook` の空/不正 URL、DB default の空/不正 URL、`enabled=false` が下位 fallback で隠蔽されず、エージェントに識別可能なエラーまたは Webhook なし結果として返ることを検証。
- `02940b7` の env/config fallback は、DB default が無い場合だけ使われること、DB default がある場合は上書きしないこと、fallback 使用時に `webhook_source='env_config'` が返ることを検証。
- gateway actions は権限検証を必須にする:
  - `CallerIdentity::Owner` は全 scope（agent/tool/global/任意 agent）を set/disable/create 可能。
  - `CallerIdentity::Agent` は自分自身の agent scope の default subtask webhook のみ set/disable 可能（URL 指定で自己設定、空 URL で自己無効化）。`scope='tool'` / `scope='global'` / 他 agent への set/disable は拒否されることを検証する。
  - `CallerIdentity::Agent` による Discord Webhook 新規作成（`ensure_subtask_webhook` の作成部分）が拒否され、Owner のみ作成できることを検証する。Agent は既存 default の再利用（redacted-read）だけ可能。
  - `trusted_user` / `co_agent` は read（redacted）のみで set/disable/create 不可。
  - 通常 read/list/get/ensure は redacted URL のみ、raw token を返さない。
- webhook worker は解決済み default URL を受け取って配送できること、明示 webhook の started 配送失敗が `spawn_subtask` のエラーとして返ること、非同期配送失敗が親セッションログに warning/error として残ること、ログには raw URL/token が出ないことをモック HTTP とログ capture で確認する。

### Migration / 後方互換
- 新テーブルのみ追加、既存スキーマは不変 → 既存 DB はそのまま起動可能。
- `spawn_subtask` 引数 `webhook` の現行挙動は完全維持（指定があれば最優先）。
- sink 未注入・設定なしの場合は新イベントを一切出さない（オプトイン）。
- 既存ライフサイクルイベント・`subtask_progress` の文面/順序は変えない。

---

## 7. オープンな問い / トレードオフ

1. **ツールイベントの DB 永続化**: webhook 配信に加え `session_logs` に `tool_call_started/completed` を残すか。観測性・後追いには有用だが書き込み量増。案: 既定は webhook のみ、`output_mode` とは別の `persist` フラグで任意有効化。
2. **親チャンネル/サブタスクチャンネルへの通知**: ツール活動を親の Discord チャンネルに直接出すか、専用 webhook チャンネルのみか。ノイズと有用性のトレードオフ。現案は webhook 専用チャンネル前提（depth>=1 は Discord 直接送信禁止の既存制約とも整合）。
3. **出力量の既定値**: `summary` の先頭/末尾文字数（600+600?）、`max_chars`（1500?）、per-run 件数上限（N?）の妥当値は運用で要調整。過小だとデバッグに使えず、過大だとレート制限・漏洩リスク。
4. **計測点の二重化**: `on_tool_call`/`on_tool_result`（progress 用）と executor sink（細粒度用）の併存は二重配信の恐れ。どちらを正にするか／progress を将来 sink に統合するか。
5. **`tool_output_chunk` の本当の必要性**: 長時間 shell のライブ追跡が必要なユースケースが実在するか。無ければ Phase 4 は見送り、completed の summary のみで足りる。
6. **redaction の網羅性**: パターンベースは取りこぼしうる。許可リスト（出して良いフィールドのみ）方式へ倒す選択肢も検討（より安全だが情報が減る）。
