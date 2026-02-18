# OpenCrab 設計ドキュメント

## 1. プロジェクト概要

### 1.1 目的

OpenCrabは、自律的に思考・学習・行動するAIエージェントを構築・管理・運用するためのフレームワークである。単なるチャットボットではなく、個性を持ち、経験から学び、自分で使うLLMを選び、スキルを獲得していく「育てるAI」を実現する。

### 1.2 前提と制約

- **言語**: Rust (edition 2021)。型安全性・並行処理・パフォーマンスを重視
- **非同期ランタイム**: Tokio。全I/O操作は非同期
- **永続化**: SQLite (rusqlite, bundled)。外部DBサーバー不要で即座に動作
- **全文検索**: SQLite FTS5。記憶検索にBM25スコアリングを使用
- **LLMプロバイダー**: OpenAI, Anthropic, Google, OpenRouter, Ollama, llama.cpp の6種をサポート。クラウドとローカルの両方に対応
- **ゲートウェイ**: REST API, CLI, Discord (feature flag), WebSocket (未実装) の4チャネル設計

### 1.3 設計哲学

- **トレイトベースの抽象化**: LLMクライアント、アクション実行、ゲートウェイはすべてトレイトで定義。実装を差し替え可能
- **クレート分離**: 機能ごとに独立したクレートに分割。循環依存なし
- **Feature flagによるプラグイン**: Discord等の外部依存は`#[cfg(feature = "...")]`で条件付きコンパイル。不要な依存を排除
- **エージェント中心設計**: すべてのデータ（記憶、スキル、Soul、ワークスペース）はエージェントIDに紐づく

---

## 2. アーキテクチャ

### 2.1 クレート構成

```
opencrab/
├── crates/
│   ├── core/       エージェントの「脳」。Soul, Identity, Memory, Skill, Workspace, SkillEngine
│   ├── llm/        LLM抽象化層。マルチプロバイダー、ルーティング、メトリクス、コスト計算
│   ├── gateway/    I/Oアダプタ層。REST, CLI, Discord, WebSocket
│   ├── actions/    エージェントが実行できる行動の定義と実装
│   ├── db/         SQLiteスキーマとクエリ関数
│   ├── server/     Axum HTTPサーバー。REST API + ゲートウェイ統合
│   └── cli/        対話型REPLクライアント
├── dashboard/      Dioxus製Web UI（エージェント管理、セッション監視、分析）
├── config/         設定ファイル (TOML)
└── skills/         スキル定義ファイル (Markdown)
```

### 2.2 依存関係の方向

```
server ──→ core ──→ db
  │          ↑
  ├──→ llm ──┘ (トレイト経由、直接依存なし)
  ├──→ gateway
  ├──→ actions ──→ core, db
  └──→ db

cli ──→ core, db

dashboard ──→ (HTTP経由でserverと通信)
```

`core`は`llm`や`actions`に直接依存しない。代わりに`LlmClient`トレイトと`ActionExecutor`トレイトを定義し、サーバー層で実装を結合する（依存性逆転）。

### 2.3 データの流れ

```
外部入力 → Gateway → Server → SkillEngine → LLM
                                   ↓
                              ActionExecutor ←→ DB
                                   ↓
                              SkillEngine → LLM（ツール結果を反映）
                                   ↓
                              最終応答 → Gateway → 外部出力
```

---

## 3. エージェントモデル

### 3.1 エージェントの構成要素

エージェントは以下の要素で構成される：

| 要素 | 説明 | 保存先 |
|------|------|--------|
| **Soul** | 性格特性。Big Five性格モデル、社交スタイル、思考スタイル | `soul`テーブル |
| **Identity** | 名前、役割、所属、アバター | `identity`テーブル |
| **Memory** | キュレーション記憶（永続的な知識）とセッションログ（会話履歴） | `memory_curated`, `memory_sessions`テーブル |
| **Skill** | エージェントが持つ能力。標準スキル（ファイル定義）と獲得スキル（実行時学習） | `skills`テーブル |
| **Workspace** | エージェント専用のファイル空間。パストラバーサル防止付き | ファイルシステム |
| **LLM設定** | デフォルトモデル、用途別モデル割り当て、自己選択の許可 | 設定ファイル |

### 3.2 個性システム (Soul)

Soulは3つの軸でエージェントの個性を定義する：

1. **Personality (Big Five)**: 開放性・誠実性・外向性・協調性・神経症傾向の5次元。各0.0〜1.0
2. **Social Style**: 主張性(assertiveness)と反応性(responsiveness)の2次元。Analytical, Driver, Expressive, Amiableの4スタイル
3. **Thinking Style**: 主思考モード(analytical, creative, practical等)と副思考モード

Soulは`build_context()`メソッドで自然言語テキストに変換され、LLMへのシステムプロンプトに組み込まれる。これによりLLMの応答がエージェントの個性を反映する。

### 3.3 スキルシステム

スキルには2種類のソースがある：

- **Standard**: `skills/`ディレクトリのMarkdownファイルから読み込む定義済みスキル
- **Acquired**: エージェントが実行時に`create_my_skill`アクションで自ら作成するスキル

各スキルは使用回数(usage_count)と有効性スコア(effectiveness)を持ち、評価データが蓄積される。

### 3.4 記憶システム

2種類の記憶を管理する：

- **Curated Memory**: カテゴリ付きの永続知識。事実、観察、学習結果を分類して保存
- **Session Log**: 会話の時系列ログ。話者ID、ターン番号付き。FTS5で全文検索可能（BM25スコアリング）

---

## 4. SkillEngine（推論ループ）

### 4.1 概要

SkillEngineはエージェントの思考と行動のサイクルを駆動する中核コンポーネント。LLMのfunction calling機能を利用して、以下のループを回す：

1. システムプロンプト（Soul + Identity + Memory + Skill）とユーザーメッセージを構築
2. 利用可能なツール定義一覧をActionExecutorから取得
3. LLMにfunction calling付きでリクエスト送信
4. LLMがツール呼び出しを返した場合 → ActionExecutorで実行し、結果をメッセージ履歴に追加 → 3に戻る
5. LLMがテキスト応答を返した場合 → 最終応答として返却
6. 最大イテレーション数に達した場合 → 安全停止

### 4.2 動的モデル切り替え

SkillEngineは`model_override`（`Arc<Mutex<Option<String>>>`）を受け取る。ループの各イテレーションでこの値を確認し、`select_llm`アクションによって実行中にモデルを切り替えることができる。

例：エージェントが「この問題は複雑だからより賢いモデルに切り替えよう」と判断し、`select_llm`アクションを呼ぶと、次のLLM呼び出しから別のモデルが使われる。

### 4.3 トレイト境界

```
SkillEngine
  ├── LlmClient (トレイト)   → LlmRouterAdapter が実装
  └── ActionExecutor (トレイト) → BridgedExecutor が実装
```

`core`クレートはトレイトのみ定義し、`server`クレートで具体的な実装を結合する。

---

## 5. LLMレイヤー

### 5.1 マルチプロバイダールーター

`LlmRouter`は6つのプロバイダーを統一的に扱う：

| プロバイダー | 特徴 |
|-------------|------|
| OpenAI | GPT系モデル |
| Anthropic | Claude系モデル |
| Google | Gemini系モデル |
| OpenRouter | 多プロバイダーゲートウェイ。100以上のモデルにアクセス |
| Ollama | ローカル推論サーバー |
| llama.cpp | ローカル推論（直接実行） |

### 5.2 モデル解決フロー

```
エイリアス ("fast")
  → マッピングテーブル → "openai:gpt-4o-mini"
    → プロバイダー名 + モデル名に分解
      → 該当プロバイダーでリクエスト実行
        → 失敗時はフォールバックチェーンで別プロバイダーを試行
```

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

エージェントは`evaluate_response`アクションで直前のLLM応答を自己評価し、品質スコアと自由記述の評価をDBに記録する。`recall_model_experiences`で過去の経験を参照し、`select_llm`で最適なモデルを選択する。

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

| カテゴリ | アクション名 | 説明 |
|----------|-------------|------|
| **会話** | `send_speech` | 発言を送信 |
| | `send_noreact` | 無反応（パス） |
| | `generate_inner_voice` | 内面の独白を生成 |
| | `update_impression` | 他エージェントへの印象を更新 |
| | `declare_done` | 議論完了を宣言 |
| **ワークスペース** | `ws_read`, `ws_write`, `ws_edit` | ファイル読み書き編集 |
| | `ws_list`, `ws_delete`, `ws_mkdir` | ファイル管理 |
| **学習** | `learn_from_experience` | 経験からスキルや知識を獲得 |
| | `learn_from_peer` | 他エージェントから学ぶ |
| | `reflect_and_learn` | 自己省察して知見を導出 |
| **検索** | `search_my_history` | 過去の会話ログをFTS検索 |
| | `summarize_and_save` | 会話を要約してキュレーション記憶に保存 |
| | `create_my_skill` | 新しいスキルを自ら作成 |
| **LLM管理** | `select_llm` | 用途に応じてモデルを動的切り替え |
| | `evaluate_response` | 直前のLLM応答を自己評価 |
| | `analyze_llm_usage` | LLM使用状況を分析 |
| | `recall_model_experiences` | 過去のモデル体験を想起 |
| | `save_model_insight` | モデルに関する知見を保存 |

### 6.3 ActionContext（実行コンテキスト）

アクション実行時に渡される共有状態：

- `agent_id`, `agent_name`: 実行主体の情報
- `session_id`: 現在のセッション
- `db`: データベース接続（`Arc<Mutex<Connection>>`）
- `workspace`: サンドボックスファイルシステム
- `last_metrics_id`: 直前のLLM呼び出しのメトリクスID（評価アクション用）
- `model_override`: 動的モデル切り替え用の共有状態
- `current_purpose`: 現在のLLM使用目的

### 6.4 BridgedExecutor

`ActionDispatcher`（アクション名→実装のマッピング）と`ActionContext`をまとめ、`core`クレートの`ActionExecutor`トレイトを実装するアダプタ。これによりSkillEngineから透過的にアクションを呼び出せる。

---

## 7. ゲートウェイレイヤー

### 7.1 Gatewayトレイト

すべてのI/Oチャネルは`Gateway`トレイトを実装する：

- `connect()`: 接続確立
- `receive()`: メッセージ受信（ブロッキング）
- `send()`: メッセージ送信
- `disconnect()`: 切断

### 7.2 メッセージ型

- **IncomingMessage**: 外部→エージェント。ソース種別(REST/CLI/Discord/WebSocket)、コンテンツ、送信者、チャンネル、メタデータ
- **OutgoingMessage**: エージェント→外部。コンテンツ、ターゲット(チャンネル/DM/ブロードキャスト)、返信先ID

### 7.3 実装済みゲートウェイ

| ゲートウェイ | 状態 | 依存 | 説明 |
|-------------|------|------|------|
| **REST** | 実装済 | なし | mpscチャンネル + oneshotによるリクエスト/レスポンス型 |
| **CLI** | 実装済 | なし | stdin/stdoutベースの対話型 |
| **Discord** | 実装済 | `serenity` (feature flag) | Bot接続、メッセージ受信/送信、2000文字自動分割 |
| **WebSocket** | 未実装 | - | プレースホルダー |

### 7.4 Discord統合のプラグイン分離

Discordゲートウェイは以下の仕組みで本体から分離されている：

- `serenity`クレートは`optional = true`で宣言
- `discord` Cargo featureを有効にしない限り、serenityはコンパイルされない
- `#[cfg(feature = "discord")]` で関連コード全体を条件付きコンパイル
- feature転送: `server`の`discord` feature → `gateway`の`discord` feature

```
cargo build                    → Discord関連コード・依存なし
cargo build --features discord → serenityコンパイル、Discord統合有効
```

---

## 8. サーバーとAPI

### 8.1 構成

Axumベースの REST APIサーバー。`AppState`を全ハンドラで共有：

- `db`: SQLiteコネクション（`Arc<Mutex<Connection>>`）
- `llm_router`: マルチプロバイダーLLMルーター（`Arc<LlmRouter>`）
- `workspace_base`: ワークスペースのベースパス

### 8.2 メッセージ処理フロー（REST）

```
POST /api/sessions/{id}/messages
  ↓
1. ユーザーメッセージをDBにログ
2. LLMプロバイダーの存在確認（なければログのみで返却）
3. セッション参加者一覧を取得
4. 送信者以外の各エージェントに対して：
   a. build_agent_context() → Soul/Identity/Skillからシステムプロンプト構築
   b. build_conversation_string() → セッションログから会話履歴構築
   c. LlmRouterAdapter + BridgedExecutor + SkillEngine を生成
   d. engine.run() 実行
   e. 応答をDBにログ
5. 全エージェントの応答をJSON配列で返却
```

### 8.3 メッセージ処理フロー（Discord）

```
Discordメッセージ受信
  ↓
1. DiscordGateway.recv() でIncomingMessage取得
2. チャンネルIDからセッションを自動作成（なければ新規）
3. ユーザーメッセージをDBにログ
4. 設定された各エージェントに対して：
   a〜d. REST版と同じパイプライン
   e. 応答をDiscordチャンネルに送信
   f. 応答をDBにログ
```

RESTとDiscordは共通の処理関数（`process.rs`）を使い、入出力部分だけが異なる。

### 8.4 設定

`config/default.toml`で環境変数展開（`${VAR}`構文）をサポート：

- LLMプロバイダーごとのAPIキーとエンドポイント
- モデルエイリアス（`fast`, `smart`, `creative`等）
- フォールバックチェーン順序
- ゲートウェイ設定（ポート、トークン、対応エージェントID）

---

## 9. データベーススキーマ

### 9.1 テーブル一覧

| テーブル | 用途 |
|----------|------|
| `agents` | エージェント基本情報 |
| `soul` | 性格特性 (Big Five JSON, Social Style JSON, Thinking Style JSON) |
| `identity` | 名前・役割・所属 |
| `memory_curated` | キュレーション記憶 (category, content) |
| `memory_sessions` | セッションログ (session_id, speaker_id, log_type, content) |
| `memory_sessions_fts` | 全文検索インデックス (FTS5) |
| `skills` | スキル定義と使用統計 (source_type, usage_count, effectiveness) |
| `impressions` | 他エージェントへの印象 |
| `sessions` | セッション管理 (mode, theme, phase, participants) |
| `llm_usage_metrics` | LLM呼び出し記録 (provider, model, tokens, latency, cost, quality_score) |
| `model_experience_notes` | モデル体験メモ (situation, observation, recommendation) |
| `model_pricing` | モデル価格情報 |
| `heartbeat_log` | ハートビート記録 |

### 9.2 設計方針

- すべてのテーブルは`agent_id`でスコープ
- UPSERTパターンで冪等性を確保
- タイムスタンプはUTC RFC3339形式
- JSONフィールドでスキーマの柔軟性を確保（性格特性、メタデータ等）
- FTS5はセッションログの全文検索に使用。BM25でランキング

---

## 10. ダッシュボード

Dioxus (Rust製WebUIフレームワーク) + Tailwind CSSで構築。

### ページ構成

- **Home**: エージェント数・セッション数・メトリクスの概要
- **Agents**: エージェント一覧、作成、削除
- **Sessions**: セッション監視、メッセージ送信
- **Memory**: キュレーション記憶の閲覧、全文検索
- **Analytics**: LLM使用量、コスト、品質の可視化
- **Persona Editor**: Soul (性格) の編集UI

サーバーのREST APIを通じてデータを取得・操作する。

---

## 11. テスト戦略

### 11.1 テスト構成

| 種類 | 件数 | 対象 |
|------|------|------|
| ユニットテスト | ~130件 | 各クレート内のモジュール単位 |
| 統合テスト | ~30件 | クレート間の連携 (engine_integration, api_e2e) |
| 実LLMテスト | ~20件 (`#[ignore]`) | OpenRouter経由の実API呼び出し |

### 11.2 テスト方針

- **ユニットテスト**: 各モジュール内で`#[cfg(test)]`。インメモリSQLite (`init_memory()`) を使用
- **E2Eテスト**: MockLlmProviderでLLM呼び出しをシミュレート。HTTP層からDB操作まで一気通貫
- **実LLMテスト**: `#[ignore]`属性で通常ビルドから除外。環境変数でモデル名・APIキーを外部注入。評価プロンプトのみハードコード
- **モデル評価テスト**: 複数モデルを実APIで比較。EVAL_SOUL環境変数でエージェントの個性バイアスを注入した評価も可能

---

## 12. 運用

### 12.1 起動方法

```bash
# REST APIサーバー
cargo run -p opencrab-server

# Discord統合付きで起動
cargo run --features discord -p opencrab-server

# CLIクライアント
cargo run -p opencrab-cli

# ダッシュボード
dx serve --project dashboard
```

### 12.2 環境変数

| 変数 | 必須 | 説明 |
|------|------|------|
| `OPENAI_API_KEY` | いずれか1つ | OpenAI APIキー |
| `ANTHROPIC_API_KEY` | いずれか1つ | Anthropic APIキー |
| `GOOGLE_API_KEY` | 任意 | Google AI APIキー |
| `OPENROUTER_API_KEY` | 任意 | OpenRouter APIキー |
| `DISCORD_TOKEN` | Discord使用時 | Discord Botトークン |

### 12.3 Discord Bot設定

1. Discord Developer Portalでアプリケーション作成
2. Bot設定で **Message Content Intent** を有効化
3. `DISCORD_TOKEN` を設定
4. `config/default.toml` の `[gateway.discord]` で `enabled = true` と `agent_ids` を設定
5. `--features discord` 付きでビルド・起動
