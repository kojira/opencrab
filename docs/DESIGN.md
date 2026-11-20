# OpenCrab 設計ドキュメント

## 1. プロジェクト概要

### 1.1 目的

OpenCrabは、自律的に思考・学習・行動するAIエージェントを構築・管理・運用するためのフレームワークである。単なるチャットボットではなく、個性を持ち、経験から学び、自分で使うLLMを選び、スキルを獲得していく「育てるAI」を実現する。

### 1.2 前提と制約

- **言語**: Rust (edition 2021)。型安全性・並行処理・パフォーマンスを重視
- **非同期ランタイム**: Tokio。全I/O操作は非同期
- **永続化**: SQLite (rusqlite, bundled)。外部DBサーバー不要で即座に動作
- **全文検索**: SQLite FTS5。記憶検索にBM25スコアリングを使用
- **LLMプロバイダー**: OpenAI, Anthropic, Google, OpenRouter, Ollama, llama.cpp, Codex CLI, Cursor CLI, ACP, ChatGPT（`~/.codex/auth.json`）の 10 形式（`crates/server/src/config/router.rs` の `KNOWN_PROVIDER_TYPES`）。クラウドとローカルの両方に対応
- **ゲートウェイ**: Discord / Nostr / Web / CLI（[opencrab-gateways](https://github.com/kojira/opencrab-gateways)）と Bluesky（[opencrab-bluesky-gateway](https://github.com/kojira/opencrab-bluesky-gateway)）は core とは別のプロセスで、gate socket（Unix ソケット 1 本）だけで接続する。資格情報は無く、ソケットを開けることだけが条件（D-1098）。core はセッションだけを持つ（D-1099）。REST は core の管理 API であり gateway ではない（§7）

### 1.3 設計哲学

- **トレイトベースの抽象化**: LLMクライアント、アクション実行、ゲートウェイはすべてトレイトで定義。実装を差し替え可能
- **クレート分離**: 機能ごとに独立したクレートに分割。循環依存なし
- **独立 process + protocol によるプラグイン**: 具象 gateway は `server` feature へ compile-in せず独立 daemon/client として配備する。core/shared/server は opaque な generic protocol だけを持ち、新 gateway 追加で変更・migration・redeploy しない（§2.4、§7）
- **エージェント中心設計**: すべてのデータ（記憶、スキル、Soul、ワークスペース）はエージェントIDに紐づく

---

## 2. アーキテクチャ

### 2.1 プロセスと所有

```text
gateway process (Discord / Nostr / Web / CLI / Bluesky)
   │  gateway が持つもの: 接続先・Bot・鍵・オーナーと権限・受付・表示・設定画面・自分の DB
   │
   │  gate socket（[gate] listen_socket の Unix ソケット 1 本、JSON 1 行 = 1 frame）
   │  hello（instance・agent・宣言する道具）/ said / say / invoke / command / activity …
   ▼
core（opencrab-server）
   │  core が持つもの: エージェント・セッション（会話記録・記憶・ターン・ツール実行・
   │  定期実行・モデル設定）・配送の記録
   ▼
SQLite（data/opencrab.db）
```

- core は gateway の種類で分岐しない。gateway は `hello` で自分の instance・担当エージェント・道具（operation）を名乗り、core はそれを不透明なまま扱う（[external-gate.md](design/external-gate.md)）。
- セッションキーは gateway が決める不透明な文字列（D-1099）。core は形式を解釈しない。
- 配備は core と各 gateway が別々。gateway が再起動しても core は止まらず、core が再起動したら gateway はつながり直す（§12）。

### 2.2 境界を機械で守る検査（CI）

- `scripts/check-deps.sh`: core 側クレートが gateway 固有のクレートに依存しないことを検査する。
- `scripts/check-file-size.sh`: `crates/` の Rust ファイルは 800 行まで。
- `scripts/check-no-private-identifiers.sh`: 非公開の識別子（Discord ID、Nostr 鍵、内部パスなど）の混入を止める。

旧来の設計段階（in-process の `server` feature、gate-admin）は [design-gateway-process-ownership.md](design-gateway-process-ownership.md) に記録として残っている。

---

## 3. エージェントモデル

### 3.1 エージェントの構成要素

エージェントは以下の要素で構成される：

| 要素 | 説明 | 保存先 |
|------|------|--------|
| **Soul** | ペルソナ名、性格テキスト、思考スタイル | `agents` テーブル（`persona_name`, `personality`）、プリセットは `soul_presets` |
| **Identity** | 名前、役割、所属、アバター | `agents` テーブル（`name`, `job_title`, `organization`, `image_url`） |
| **Memory** | キュレーション記憶（永続的な知識）とセッションログ（会話履歴） | `memory_curated`, `memory_sessions`テーブル |
| **Skill** | エージェントが持つ能力。標準スキル（ファイル定義）と獲得スキル（実行時学習） | `skills`テーブル |
| **Workspace** | エージェント専用のファイル空間。パストラバーサル防止付き | ファイルシステム |
| **LLM設定** | エージェントのモデル・推論強度、会話ごとのモデル | `agents.model` / `agents.reasoning_effort`、`sessions.model` |

### 3.2 個性システム (Soul)

Soul はペルソナ名、性格テキスト（`personality`）、思考スタイル（主・副）と任意の追加特性（`custom_traits`）を持つ（`crates/core/src/soul.rs`）。これらは自然言語テキストとしてシステムプロンプトに組み込まれ、LLM の応答がエージェントの個性を反映する。

### 3.3 スキルシステム

スキルには2種類のソースがある：

- **Standard**: `skills/`ディレクトリのMarkdownファイルから読み込む定義済みスキル
- **Acquired**: エージェントが実行時に作成するスキル（`create_my_skill` は `source_type="self_created"`、gateway アクションの `create_skill` は `"acquired"`）

各スキルは使用回数(usage_count)と有効性スコア(effectiveness)を持ち、評価データが蓄積される。

### 3.4 記憶システム

3種類の記憶を管理する：

- **Curated Memory**: カテゴリ付きの永続知識。事実、観察、学習結果を分類して保存
- **Session Log**: 会話の時系列ログ。話者ID、ターン番号付き。FTS5で全文検索可能（BM25スコアリング）
- **Memory Index**: セッションログの階層ツリーインデックス。LLMで要約を生成し、root → period → session → topic の4階層に構造化。Agentic RAGにより、エージェントが`browse_memory_index`でツリーを閲覧→推論→`retrieve_memory_nodes`で全文取得の2ステップで文脈依存の記憶検索を実現

---

## 4. SkillEngine（推論ループ）

### 4.1 概要

SkillEngineはエージェントの思考と行動のサイクルを駆動する中核コンポーネント。LLMのfunction calling機能を利用して、以下のループを回す：

1. システムプロンプト（Soul + Identity + Memory + Skill）とユーザーメッセージを構築
2. 利用可能なツール定義一覧をActionExecutorから取得
3. LLMにfunction calling付きでリクエスト送信
4. LLMがツール呼び出しを返した場合 → **分類に応じて inline 実行またはバックグラウンド実行**し、結果（またはバックグラウンド実行を開始した旨）をメッセージ履歴に追加 → 3に戻る（§4.4 参照）
5. LLMがテキスト応答を返した場合 → 最後の行が `NO_REPLY` ならターンを終える。そうでなければ続ける（`crates/core/src/continue_marker.rs`、`crates/server/src/process/prompt.rs` の `TURN_COMPLETION_RULES`）
6. 最大イテレーション数に達した場合 → 安全停止

### 4.2 動的モデル切り替え

1ターンは1つのモデルで走る（ターン開始時に会話のモデルを読む）。エージェントの`select_llm`はオーナーの`/model`と同じく「この会話のモデル」（`sessions.model`、`ModelAdministration::set_session_model`）を変え、変更は次のターンから効き、また変えるまで続く（D-1135）。ターンの途中でモデルを差し替える仕組みは無い。選べるモデルは `/model` と同じ一覧（`GET /api/llm/model-choices`）で、一覧に無い名前は何も変えずに一覧付きで拒否する（D-1131）。

`get_system_info` は `current_turn_model`（このターン）、`conversation_model`（次のターン）、`selectable_models`（一覧）を返す。

LLM 呼び出しが失敗してターンが終わったときは、モデルとプロバイダのエラーを `turn_failed` のシステム行として会話に記録し、次のターンの履歴に見える（`crates/server/src/process/end_event.rs`）。

例：エージェントが「この問題は複雑だからより賢いモデルに切り替えよう」と判断し、`select_llm`アクションを呼ぶと、この会話の次のターンから別のモデルが使われる。

### 4.3 トレイト境界

```
SkillEngine
  ├── LlmClient (トレイト)   → LlmRouterAdapter が実装
  └── ActionExecutor (トレイト) → BridgedExecutor が実装
```

`core`クレートはトレイトのみ定義し、`server`クレートで具体的な実装を結合する。

### 4.4 非ブロックツール実行（バックグラウンド実行）

**方針**: 応答ループは Web サーバのように常に次の入力を受け付けられる状態を保つ。時間のかかるツールでループを止めない。

そのため、ツール呼び出しは既定で**バックグラウンド実行**に回す。エージェントには同じターン内で「開始した」ことだけが返り（`{"status":"spawned","subtask_id":...}`）、ターンはそのまま継続する。実行が終わると結果が会話に再注入され、エージェントが改めて応答する。

- 同じターンで複数のツールが呼ばれた場合、**まとめて 1 本のバックグラウンド実行**にし、**呼ばれた順に逐次実行**する（順序が意味を持つため）。まとめた分は完了通知も 1 回になる。
- 分類上 inline にすべきツールが 1 つでも混ざる場合は、**そのターンのツール群を全て inline 実行**する（inline と背景実行の相対順序は保証できないため）。
- 実行には既定のタイムアウトがあり、超過すると打ち切って「時間切れ」として決着する。打ち切りで実行されなかったツールも結果に明示する（依頼が無言で消えないように）。
- 停止（キャンセル）は全ゲートウェイから可能。停止したものは再注入されない。

#### inline にするツールの分類基準

以下に当てはまるものは**バックグラウンドに回さない**（＝従来どおり同じターン内で実行し、結果をその場で使う）:

1. **配送系** — 送信・投稿・返信・UI 提示など、外部に出ていくもの。背景化すると本文と順序が入れ替わったり、二重に送られたりする。
2. **同ターン結果依存** — 戻り値（ID や URL）を同じターンの後続処理で使うもの。
3. **run 内の共有状態を書くもの** — 例: 以後のスキル可視性、指示文、タスク台帳の状態。背景化するとそのターンに反映されず、競合もする。
4. **純粋な読み取りで即答すべきもの** — 背景化すると「1 つの質問が 2 ターン 2 メッセージ」になり体験が悪化する。
5. **制御系** — バックグラウンド実行そのものを制御するもの（生成・停止・進捗報告）。

分類の権威は**各ツール定義が自ら名乗る属性**（`GatewayActionDef.class.dispatch` = `Inline` / `Dispatchable`）である。この属性は必須で既定値を持たない（`ToolClass` に `Default` を実装しない）ため、**新しいゲートウェイツールを足すと分類の記述を強制される**（構築サイトで書かない限りコンパイルが通らない）。消費側（`BridgedExecutor`）は gateway／MCP の定義を舐めて名前→分類の索引を作り、`inline_tool_names()` で `Inline` の名前を集める。

共通アクション群（core）だけは `GatewayActionDef` を持たない一次ツールなので、例外的に `crates/actions/src/bridge/policy.rs` の定数（`CORE_INLINE_ACTIONS` / `CORE_DISPATCHABLE_ACTIONS` の対）で分類し、`ActionDispatcher` の全名がどちらかに属することを fail-closed 検査（`core_actions_are_classified_for_dispatch`）が守る。

制御ツール（`spawn_subtask` / `cancel_subtask` / `report_progress`）だけは 3 つ目の源として `default_non_dispatch_tools()`（`crates/actions/src/subtask/dispatcher.rs`）に直接ハードコードされ、常に inline に残る（それ自体が subtask ライフサイクルを操作するため）。

**ツール名の一覧はこのドキュメントには置かない**（各定義の属性・core の 2 定数・制御ツールのハードコードが権威。ここに書くのは上の分類基準だけ。一覧を二重管理すると必ず実装と乖離するため）。

上記の core built-in 分類は core tool に限る。外部 gateway operation は hello の digest-covered `authorization.allowed_callers` / `dispatch` / `sub_engine` / `sharing` / `effect` metadata が唯一の権威で、`dispatch=utterance` と `effect=utterance` は iff として両方向を検証する。完全な組合せは utterance/utterance、または `read_only|state_change` と `inline|background` である。`final_delivery=operation_driven` は有効な utterance operation を 1 つ以上要求する。Issue #1006 の declaration/invocation は新しい delivery-guarantee label を持たず、分離前と同じ一回送信・terminal outcome を使う。新しい operation 名を shared allowlist、prefix、既知 utterance 一覧へ追加してはならない。詳細は [gateway ownership §4](design-gateway-process-ownership.md#4-one-to-one-to-be-transition-and-completion-map)。

分類の対象外が 2 つある。どちらも**既定の振る舞い**に落ちる:

- **外部連携（MCP）由来のツール** — 運用者が繋いだ任意の外部ツールで、配送系なのか同ターンで戻り値を使うのかを静的に判定できない。安全側に倒して**既定で inline**（名前の接頭辞による規則で扱い、集合には列挙しない）。
- **設定由来のツール**（`[tools]` 設定から登録されるシェル実行ツール） — 存在するかどうかも、実行できるコマンドの範囲も、設定と DB（エージェントごとの許可コマンド）で決まる。コードが静的に知る名前の集合が無いので fail-closed 走査の対象にできない。したがって**既定どおりバックグラウンド実行**になるが、これは望ましい向きでもある（時間のかかる外部コマンドこそ非ブロック実行の主目的）。

#### 無効化（kill switch）

`config/default.toml` の `[subtask] auto_dispatch`（既定 `true`）を `false` にすると、**全ツールが従来どおり同期実行**に戻る。環境変数 `OPENCRAB_SUBTASK_AUTO_DISPATCH`（`0`/`false`/`off`/`no`）が TOML より優先されるため、`.env` だけで切り戻せる。

---

## 5. LLMレイヤー

### 5.1 マルチプロバイダールーター

`LlmRouter` は設定の `[llm.providers.<名前>]` ごとにプロバイダーを登録する。形式（`type`、省略時はセクション名）は次の 10 種（`KNOWN_PROVIDER_TYPES`）。未知の形式は起動を止める。

| 形式 | 特徴 |
|-------------|------|
| `openai` | OpenAI 互換 API（`base_url` で互換サーバーも可） |
| `anthropic` | Claude系モデル |
| `google` | Gemini系モデル |
| `openrouter` | 多プロバイダーゲートウェイ |
| `ollama` | ローカル推論サーバー |
| `llamacpp` | ローカル推論サーバー（llama.cpp） |
| `codex` | Codex CLI を子プロセスで起動 |
| `cursor` | Cursor CLI を子プロセスで起動 |
| `acp` | ACP エージェント（stdio JSON-RPC） |
| `chatgpt` | ChatGPT の codex/responses API（`~/.codex/auth.json` の OAuth） |

### 5.2 モデル解決フロー

```
"provider:model"（例 "chatgpt:gpt-6.2-sol"）またはエイリアス（[llm.aliases]）
  → プロバイダー名 + モデル名に分解（コロンが無ければ既定プロバイダー）
    → 該当プロバイダーでリクエスト実行
      → 失敗時は [llm.fallback] chain の別プロバイダーを試行
```

料金・コンテキスト長・出力上限はモデルごとに `model_pricing` テーブルに持つ（ダッシュボード、`PUT /api/llm/model-pricing`）。

### 5.3 コストとメトリクス

全LLM呼び出しに対して以下を記録：

- プロバイダー・モデル名
- 入力/出力トークン数
- レイテンシ（ミリ秒）
- 推定コスト（USD）
- 用途（conversation, analysis, tool_calling等）
- 品質スコア（自己評価後に記録）
- タスク成功/失敗フラグ

これにより「どのモデルが、どの用途で、どのくらいのコストで、どの品質か」を定量的に分析できる。

### 5.4 自己評価と学習

エージェントは`evaluate_response`アクションで直前のLLM応答を自己評価し、品質スコアと自由記述の評価をDBに記録する。`recall_model_experiences`で過去の経験を参照し、`select_llm`でこの会話のモデルを選ぶ（次のターンから効く）。

このサイクルにより、エージェントは使用経験に基づいてモデル選択を最適化していく。

---

## 6. アクションシステム

### 6.1 設計

アクションは`Action`トレイトを実装する。各アクションは：

- `name()`: LLMのfunction calling用の関数名
- `description()`: LLMが呼び出し判断に使う説明
- `parameters()`: JSON Schemaによるパラメータ定義
- `execute()`: 実際の処理

### 6.2 登録済みアクション一覧

一覧は README の「Action System」表が正本で、`ActionDispatcher::new()` と一致することをテスト（`readme_action_table_matches_the_dispatcher`）が守る。ここには重ねて書かない。発言は応答本文、または gateway が宣言した道具（例: `reply`）で行う。

### 6.3 ActionContext（実行コンテキスト）

アクション実行時に渡される共有状態：

- `agent_id`, `agent_name`: 実行主体の情報
- `session_id`: 現在のセッション
- `db`: データベース接続（`Arc<Mutex<Connection>>`）
- `workspace`: サンドボックスファイルシステム
- `last_metrics_id`: 直前のLLM呼び出しのメトリクスID（評価アクション用）
- `current_purpose`: 現在のLLM使用目的
- `caller`: 呼び出し元の権限（owner / co_agent / trusted_user / agent）
- `runtime_info`: 既定モデル、このターンのモデル、構成済みプロバイダー、gateway 名

### 6.4 BridgedExecutor

`ActionDispatcher`（アクション名→実装のマッピング）と`ActionContext`をまとめ、`core`クレートの`ActionExecutor`トレイトを実装するアダプタ。これによりSkillEngineから透過的にアクションを呼び出せる。

---

## 7. ゲートウェイレイヤー

### 7.1 gate socket

gateway は core の gate socket（`[gate] listen_socket`、空なら listen しない）に接続する。プロトコルの正本は [design/external-gate.md](design/external-gate.md)（現行は protocol 3・operation_protocol 1、D-1099 改訂の節）。

- `hello`: instance id、担当エージェント、`final_delivery`、宣言する道具（`operations`）を名乗る。instance が無ければ core が作る。
- `said`: 外部の発言を渡す。セッションキーは gateway が決める。
- `say` / `invoke`: core から gateway への発言と、宣言された道具の呼び出し。
- `command`: `/model` などの管理コマンド（`list_models` / `set_model` / `reset_model` / `list_session_models` / `set_session_model` / `reset_session_model` / `list_session_reasoning_efforts` / `set_session_reasoning_effort`、`crates/extgate/src/commands.rs`）。
- `activity` / `tool_activity` / `subtask_lifecycle` / `turn_failed`: 応答を求めない通知。

### 7.2 gateway が宣言する道具

道具の `input_schema` は JSON Schema の一部（`type` / `required` / `properties` / `enum` / `items` / `description` / `format`）と、core 用の印 `"x-opencrab-short-ref": true`（会話の短縮参照 `e12` などを実 ID に置き換える欄）・`"x-opencrab-workspace-file": true`（ワークスペースのファイルを outbox にコピーして渡す欄）だけを使える（`crates/extgate/src/operations.rs` の `ALLOWED_SCHEMA_KEYWORDS`）。`format` には JSON Schema 標準の値だけを書く。未知の `format` は LLM プロバイダーが道具ごと拒否するため、core の印は `format` に載せない（D-1134）。各道具は `authorization.allowed_callers` / `dispatch` / `sub_engine` / `sharing` / `effect` を名乗り、core はそれだけで扱いを決める。

---

## 8. サーバーとAPI

`server` は core の管理 API（エージェント、スキル、記憶、セッション、LLM 設定、モデル料金、定期実行、許可コマンド、MCP、ログ）とエージェントの実行パイプラインを持つ。gateway 固有の設定・資格情報・外部 ID・受付の API は持たない。公開 TCP に `/api/gate*` は無い。API の一覧は [api.md](api.md)。

---

## 9. データベーススキーマ

### 9.1 テーブル一覧

現行のスキーマは version 65（`PRAGMA user_version`）。主なテーブル:

| テーブル | 用途 |
|----------|------|
| `agents` | エージェント基本情報（名前・役割・ペルソナ・性格・指示・モデル・推論強度） |
| `soul_presets` | ペルソナのプリセット |
| `subject_id_allocator` / `subject_tombstones` | エージェントの数値 subject ID の採番と、削除済み ID の再利用防止 |
| `memory_curated` | キュレーション記憶 (category, content) |
| `memory_sessions` / `memory_sessions_fts` | 会話ログと全文検索インデックス (FTS5) |
| `memory_index_nodes` / `memory_index_fts` / `memory_index_watermark` | 記憶インデックスの階層ノード、検索、構築の進捗 |
| `memory_category_members` / `agent_memory_index_config` / `daily_log_index_watermark` | 記憶のカテゴリ、インデックス設定、日次ログ索引の進捗 |
| `skills` / `skill_usage_log` | スキル定義と使用記録 |
| `impressions` | 他エージェントへの印象 |
| `sessions` / `agent_sessions` | セッション（会話）と参加者。`sessions.model` が会話のモデル |
| `conversation_snapshots` / `agent_inbox` | 会話の要約スナップショット、エージェントの受信箱 |
| `llm_logs` / `llm_usage_metrics` / `llm_provider_overrides` | LLM 呼び出しの全文ログ、メトリクス、ダッシュボードからのプロバイダー設定 |
| `model_pricing` / `model_experience_notes` | モデル料金・コンテキスト長・出力上限、モデル体験メモ |
| `tool_logs` / `agent_logs` | ツール実行ログ、エージェントのイベントログ |
| `agent_schedules` / `heartbeat_log` | 定期・定時実行（cron / `@every`）とその発火記録 |
| `task_ledger` / `task_progress` | 長い作業の台帳 |
| `agent_allowed_commands` / `agent_mcp_config` / `agent_webhook_config` | エージェントごとの許可コマンド、MCP サーバ、webhook |
| `gate_instances` / `gate_bindings` | gateway の instance と、(instance, セッションキー) → セッションの対応 |
| `deliveries` / `external_origins` / `gateway_operation_calls` | 発言の配送記録、受信の重複防止、gateway の道具の呼び出し記録 |
| `api_principals` | REST の API 主体 |
| `import_sync_state` | OpenClaw インポートの同期状態 |

### 9.2 設計方針

- 会話・記憶テーブルは`agent_id`でスコープする。gateway の instance は不透明に扱い、gateway の種類で分岐しない
- 通常状態は UPSERT パターンで冪等性を確保する
- 外部設定/APIの時刻はUTC RFC3339、DB内部の gateway instance/binding 時刻は比較可能な UTC Unix nanoseconds とする
- JSONフィールドでスキーマの柔軟性を確保（性格特性、メタデータ等）
- FTS5はセッションログの全文検索に使用。BM25でランキング
- Memory Indexはウォーターマーク方式の増分構築。LLMで要約を生成し、閾値超過時にバックグラウンドで自動実行

---

## 10. ダッシュボード

React + Vite + Tailwind CSS（`web/`）。i18n は英語と日本語。

### ページ構成（`web/src/App.tsx`）

- **Home**（`/`）: 概要とセットアップのチェックリスト
- **Setup**（`/setup`）: LLM プロバイダーとエージェント作成の 2 段階
- **Agents**（`/agents`、`/agents/new`、`/agents/:id/…`）: 概要、編集、ペルソナ、スキル、スリープ、記憶、セッション、分析、定期実行、許可コマンド、LLM ログ、ツールログ
- **Sessions**（`/sessions`、`/sessions/:id`）: セッション一覧と詳細
- **Workspace**（`/workspace/:agentId`）: ワークスペースのファイル
- **System settings**（`/settings`）: LLM プロバイダー、モデル料金など

サーバーのREST APIを通じてデータを取得・操作する。

---

## 11. テスト戦略

### 11.1 テスト構成

CI（`.github/workflows/ci.yml`）は `cargo fmt --check`、`cargo clippy --workspace --all-targets --all-features -- -D warnings`、`cargo test --workspace --all-features`、上記の検査スクリプト、`web/` の build と test を実行する。件数はここに書かない（すぐ古くなる）。

### 11.2 テスト方針

- **ユニットテスト**: 各モジュール内で`#[cfg(test)]`。インメモリSQLite (`init_memory()`) を使用
- **E2Eテスト**: MockLlmProviderでLLM呼び出しをシミュレート。HTTP層からDB操作まで一気通貫
- **実LLMテスト**: `#[ignore]`属性で通常ビルドから除外。環境変数でモデル名・APIキーを外部注入。評価プロンプトのみハードコード
- **モデル評価テスト**: 複数モデルを実APIで比較。EVAL_SOUL環境変数でエージェントの個性バイアスを注入した評価も可能
- **gateway boundary/release test（#1006 当時の記録）**: S1 security schema の fresh/populated migration・rollback、legal/illegal seal/revoke/scope transition、core-service-EUID ownership を含む strict manifest/path、exact-idempotent bootstrap/conflict、bootstrap/rotation duplicate-bearer refusal、request の full-scan zero/one/multiple match と no-first-match/no-scope-union、operation/subject/instance/namespace/expiry/revocation/rotation、audit redaction/append-only/savepoint atomicity、stale-socket refusal/created-inode cleanup/startup ordering、public TCP 全 6 path 404 と protected core UDS-only success を検証する。加えて `1c3b782` と同じ単一 core delivery row/state、generic frame/handler 数、disconnect/startup `indeterminate`、Discord の ordered one-`create_message`-per-produced-chunk と fail-fast、Nostr single command の parity、全 `trusted_users.platform/source` fingerprint の approved disposition/zero-unmapped/Web destination/all-store rollback、gateway-owned caller classification と admitted role snapshot parity、subject ID 保存/non-reuse/grant single-use、immutable projection marker、successful-completion ordinary rerun の no-op と mismatch refusal、matched-set failure restore、read-only `verify-freeze` の DB 不変、preflight/cleanup 間 mutation abort、cleanup 中の全 destination read lock、separate atomic cleanup/freeze/manifest record、post-QC digest lineage を検証する。synthetic gateway は core/shared/server source・schema・migration・redeploy を 0 変更で追加できなければならない

---

## 12. 運用

core と各 gateway は別々に配備する。

- **core**: このリポジトリから `opencrab-server`（`cargo build --release -p opencrab-server`）と `web/`（ダッシュボード）を作って起動する。起動確認は `GET /health`（`ok`）と `GET /api/health`（`{"status":"ok"}`）。
- **gateway**: [opencrab-gateways](https://github.com/kojira/opencrab-gateways)（Discord / Nostr / Web / CLI）と [opencrab-bluesky-gateway](https://github.com/kojira/opencrab-bluesky-gateway) で別々に作って起動し、core の gate socket（`[gate] listen_socket`）を指す。Rust の gateway は `gate-client` を通じて core の特定のコミット（`Cargo.lock` に固定）に合わせて作られるので、動いている core と同じコミットに合わせたものを使う。
- core を入れ替えても gateway は止めなくてよい。gateway は core につながり直す。gateway を入れ替えても core は止まらない。

旧来の in-process 構成（`opencrab-server` の feature で Discord/Nostr/Web を有効にし、`[gateway.discord]` を設定する手順）と、#1006 の移行手順は [design-gateway-process-ownership.md](design-gateway-process-ownership.md) に記録として残っている。現行の手順としては使わない。
