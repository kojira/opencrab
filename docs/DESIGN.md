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
│   ├── llm-types/  LLM の型定義のみ（葉クレート。兄弟クレートが循環せず依存できるように分離）
│   ├── gateway/    Gateway ポート（メッセージ型 / GatewayActions トレイト）。ポート専用で具象 transport・SDK を含まない
│   ├── actions/    アクション定義・実行、バックグラウンド実行のランタイム、分類ポリシー表
│   ├── db/         SQLiteスキーマとクエリ関数
│   ├── mcp/        外部ツール連携（MCP。子プロセスとして起動し張り替え・死活検出を持つ）
│   ├── discord/    Discord統合。メッセージループ、管理アクション、per-agent Bot管理
│   ├── nostr/      Nostr統合。セッション単位のキューと同時実行上限
│   ├── voice/      STT/TTS プロバイダ層（voice session の管理は discord 側）
│   ├── server/     Axum HTTPサーバー。REST API + 応答生成パイプライン + web ゲートウェイ
│   └── cli/        対話型REPLクライアント
├── web/            React フロントエンド（Vite + Tailwind CSS + i18n EN/JA）
├── config/         設定ファイル (TOML)
├── docs/           設計ドキュメント
└── skills/         スキル定義ファイル (Markdown)
```

### 2.2 依存関係の方向

```
server ──→ core ──→ db
  │          ↑
  ├──→ llm ──┘ (トレイト経由、直接依存なし)  llm ──→ llm-types
  ├──→ gateway
  ├──→ actions ──→ core, db, gateway
  ├──→ discord (optional) ──→ gateway, db, core, actions
  ├──→ nostr ──→ db, core, actions
  ├──→ mcp
  ├──→ voice
  └──→ db

cli ──→ core, db

web（フロントエンド） ──→ (HTTP経由でserverと通信)
```

`core`は`llm`や`actions`に直接依存しない。代わりに`LlmClient`トレイトと`ActionExecutor`トレイトを定義し、サーバー層で実装を結合する（依存性逆転）。

`gateway`は**ポート専用**で、SDK（`serenity` / `songbird`）に依存しない（feature も持たない）。Discord の具象実装（`DiscordGateway` 等）は`discord`クレートが所有する。これにより`gateway`／`actions`（`actions → gateway`）の依存ツリーに SDK が漏れず、共有層は transport SDK を知らないまま保たれる（#1-A。CI の R5 で回帰を止める → §2.5）。

`discord`クレートは`AgentRunner`トレイトを定義し、`server`が`AppState`に対してこれを実装することで循環依存を回避している。ゲートウェイ非依存な実行境界（応答生成・会話履歴・トークン予算・セッション管理・**ターン転記**）は`actions`クレートの`AgentRuntime`トレイトが持ち、`AgentRunner` / `NostrAgentRunner` / `WebAgentRunner`はいずれもそのスーパートレイトとして継承する（#156 / #158）。

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

### 2.4 クレート分離の方針（重要）

**目指す姿は「コアは生きたまま、外側（transport・拡張）を落とさずに差し替えられる」構成**であり、最終的な目的は**エージェントが自分自身で opencrab を開発できるようにすること**。単一バイナリのままだと「自分が動いているコードを差し替える」＝「自分を落とす」ことになるため、この分離が前提になる。

手段は**別プロセス + プロトコル**（外部ツール連携で既に実証済みの形）。動的ライブラリによるプラグイン機構は Rust に安定 ABI が無いため採らない。

したがって以下を守る:

- **汎用の機能を transport のクレートに置かない**（置くと transport が実質ランタイム化する。実際に起きている）
- **状態はコアが持つ**（プラグインは再起動されうるので、外側に状態を置くと差し替え時に分裂する）
- **上位が個々のゲートウェイを名指しで知らない**形へ寄せる（ゲートウェイを足しても上位に手が入らないこと）

分離の順序・判断基準・非目標は **[design-plugin-architecture.md](design-plugin-architecture.md)** を参照。新機能の実装やレビューの際は、まずこの基準に照らすこと。

### 2.5 境界を機械で守る検査（CI）

§2.2 の依存の向きと §2.4 の分離方針は、レビューだけに頼ると少しずつ崩れる。名指しの依存や識別子が 1 つ入っても、ビルドは通ってしまうからだ。そこで CI に検査を置き、「現状の性質」を固定する。どれも**何かを直すためではなく、既に成立している境界を回帰させないため**のもの。

- **R4: `opencrab-core` は gate/SDK クレートに依存しない**（`scripts/check-deps.sh`）
  `cargo tree -p opencrab-core --edges no-dev` の**依存ツリーの内容**を検査し、`opencrab-gateway` / `opencrab-discord` / `opencrab-nostr` / `opencrab-web-gateway` / `serenity` / `serenity-voice-model` / `songbird` が現れたら失敗させる。依存の**向きの逆転はコンパイル可否には現れない**（core が transport を巻き込んでもビルドは通る）ので、ビルドの成否ではなくツリーそのものを見る。`--edges no-dev` は normal に加え **build 依存も検査**し（build-dependency 経由の逆流を見落とさない）、dev-only 依存（テスト用の `syn` 等）は除外する。
- **R5: SDK（`serenity` / `songbird`）は共有層（`opencrab-gateway` / `opencrab-actions`）に現れない**（`scripts/check-deps.sh`）
  R4 と同じ `--edges no-dev` の依存ツリーを見て、`serenity` / `serenity-voice-model` / `songbird` が共有層の 2 クレートに漏れたら失敗させる。`gateway` がポート専用になり SDK を持たない（#1-A）性質を回帰させないための検査。
- **R6: feature の組み合わせでビルドできる**（`scripts/check-deps.sh`）
  `opencrab-gateway`（feature なし）と `opencrab-server` の discord 軸（`--no-default-features` / `--features discord` / 既定）をそれぞれ `cargo build` し、どの組み合わせでも壊れないことを確かめる。ビルドを含むので CI では build/test の後ろ（`check-deps.sh` の呼び出し位置）で走る。
- **R7: 共有層（core の production コード）に gate 名が出ない**（`crates/core/tests/no_gate_identifiers.rs`）
  `syn` で `crates/core/src` を AST 走査し、**識別子と文字列リテラル**に `discord` / `serenity` / `songbird` / `nostr` が無いことを確かめる。テスト専用の項目（`#[cfg(test)]` / `#[cfg(all(test, ...))]` 等。ただし `any(test, ...)` はテスト以外でもコンパイルされるので対象に残す）は対象外。属性は **doc コメント（`#[doc = "..."]`）だけ**を対象外にし、それ以外の属性（`#[serde(rename = "...")]` / `#[error("...")]` 等）の文字列・識別子は検査する（本番の挙動・ワイヤ表現にゲート名が焼き込まれるため。serde を多用する `db` へ広げるとき効く）。名指しが 1 つ core に入ると、上位がそのゲートウェイを特別扱いし始める入口になる（design-plugin-architecture.md §4 が実際の事故として記録している）。`cargo expand` を使わないのは、nightly を要し、展開結果に doc コメントが残って偽陽性になるため。
  **限界（意図的）**: 検査が届くのは AST に現れるトークンに限る。`tracing::info!("...")` や `format!(...)` など**マクロ本体の文字列・識別子は `syn` が生トークンのまま保持するため検出できない**（core は tracing を多用するのでログ文言にゲート名を書いても落ちない）。マクロ内まで見るのは過剰なので、検査の形は変えず限界として明示する。

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
3. **run 内の共有状態を書くもの** — 例: 実行中のモデル切り替え。背景化するとそのターンに反映されず、競合もする。
4. **純粋な読み取りで即答すべきもの** — 背景化すると「1 つの質問が 2 ターン 2 メッセージ」になり体験が悪化する。
5. **制御系** — バックグラウンド実行そのものを制御するもの（生成・停止・進捗報告）。

分類は各ゲートウェイ／アクション群ごとにコードで列挙し、**未分類のツールがあると検査が失敗する**（新しいツールを追加したら必ずどちらかに分類する）。この検査は**コードが静的に公開しているツール名**を供給する全ての源に対して存在する — 共通アクション群、Discord、Nostr、そしてサーバ内蔵の設定ツール群（web／REST／定期実行の全ターンに載るもの）の 4 つ。いずれも「実装が公開しているツール名」を起点に走査するため、定数への入れ忘れも検知できる。

**ツール名の一覧はこのドキュメントには置かない**。唯一の権威は `crates/actions/src/bridge.rs` の定数（inline 集合と dispatch 可集合の対）で、ここに書くのは上の分類基準だけとする（一覧を二重管理すると必ず実装と乖離するため）。

分類の対象外が 2 つある。どちらも**既定の振る舞い**に落ちる:

- **外部連携（MCP）由来のツール** — 運用者が繋いだ任意の外部ツールで、配送系なのか同ターンで戻り値を使うのかを静的に判定できない。安全側に倒して**既定で inline**（名前の接頭辞による規則で扱い、集合には列挙しない）。
- **設定由来のツール**（`[tools]` 設定から登録されるシェル実行ツール） — 存在するかどうかも、実行できるコマンドの範囲も、設定と DB（エージェントごとの許可コマンド）で決まる。コードが静的に知る名前の集合が無いので fail-closed 走査の対象にできない。したがって**既定どおりバックグラウンド実行**になるが、これは望ましい向きでもある（時間のかかる外部コマンドこそ非ブロック実行の主目的）。

#### 無効化（kill switch）

`config/default.toml` の `[subtask] auto_dispatch`（既定 `true`）を `false` にすると、**全ツールが従来どおり同期実行**に戻る。環境変数 `OPENCRAB_SUBTASK_AUTO_DISPATCH`（`0`/`false`/`off`/`no`）が TOML より優先されるため、`.env` だけで切り戻せる。

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
| | `browse_memory_index` | 記憶インデックスのツリー構造を閲覧（タイトル+要約のコンパクト表示） |
| | `retrieve_memory_nodes` | インデックスノードの全文テキストを取得（1-5ノード指定） |
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

### 7.1 transport の共通抽象は無い（#215）

かつて `connect` / `receive` / `send` / `disconnect` を持つ `Gateway` トレイトがあったが、**実装 4 種（REST / CLI / WebSocket / Discord）に対して利用者ゼロ**（`dyn Gateway` の使用箇所がゼロ、WebSocket は全メソッド `todo!()`）のまま腐っていたため #215 で削除した。

構造的にも段階2 の受け皿にならなかった：`receive(&mut self)` は `Arc` 共有された状態から呼べない（実運用の transport はすべて push 型）、どのメソッドにも `agent_id` が無く per-agent ゲートウェイ（#40）を表現できない。

**現状（#191 段階2）**: 受信を持つ transport（Discord / Nostr）は `opencrab-actions` の `AgentGatewayLifecycle`（起動 / 停止 / 生存確認 / DB からの復元 / 全停止 + 既定 `None` の capability accessor）を実装し、`AppState` は種別名で引く登録簿 `AgentGatewayRegistry` だけを持つ。**個々のゲートウェイの名指しフィールドは無い。** 起動時の DB からの復元も登録簿の走査（`restore_pending`）で、復元位置が transport ごとに違うという仕様は「登録済みかつ未復元の分だけを登録順に復元する」形で保っている。

残っている名指しは (1) マネージャの生成そのもの（`main`）、(2) heartbeat の発話が使う生の serenity HTTP クライアント、(3) 共有（TOML）ゲートウェイの起動ブロック（per-agent の登録簿とは別の仕組み）。MCP は**受信を持たない**ので登録簿に入れない（道具の供給者であり transport ではない）。

方針は §2.4 と [design-plugin-architecture.md](design-plugin-architecture.md) を参照。新しい transport 抽象を足すなら、削除済みの型を復活させるのではなくそこの設計から始めること。

### 7.2 メッセージ型

- **IncomingMessage**: 外部→エージェント。ソース種別(REST/CLI/Discord/WebSocket)、コンテンツ、送信者、チャンネル、メタデータ。Discord の受信処理と音声セッションで実運用中
- 対になる `OutgoingMessage` / `MessageTarget` は利用者がいなかったため #215 で削除した。送信は各 transport の具象メソッド（Discord なら `send_to_channel`）で行う

### 7.3 実装済みゲートウェイ

| ゲートウェイ | 実体 | 依存 | 説明 |
|-------------|------|------|------|
| **Discord** | `opencrab-discord` の `DiscordGateway` + `opencrab-discord` のイベントループ | `serenity` / `songbird`（`server` の `discord` feature 経由） | Bot接続、メッセージ受信/送信、2000文字自動分割 |
| **REST** | `opencrab-server` の axum ハンドラ | なし | `opencrab-gateway` を経由しない |
| **Nostr** | `opencrab-nostr` | - | - |
| **Web** | `opencrab-web-gateway` | - | - |

### 7.4 Discord統合のプラグイン分離

Discord固有のロジックは専用の`opencrab-discord`クレートに分離されている：

- **`opencrab-discord`クレート**: メッセージループ、Discord管理アクション（サーバー/チャンネル一覧、チャンネル設定）、per-agent Botライフサイクル管理を提供
- **`AgentRunner`トレイト**: `discord`クレートで定義。Discord固有の判定（trust / チャンネルポリシー）・per-agentゲートウェイを抽象化し、`server`が`AppState`に対して実装する。これにより`discord → server`の循環依存を回避。エージェント処理パイプライン（LLM呼び出し等）そのものは`actions`の`AgentRuntime`（全ゲートウェイ共通、実装は`server/src/agent_runtime_impl.rs`の1箇所）が持つ
- **ターン転記（session_logs への記録）**: `AgentRunner`ではなく`actions`の`AgentRuntime`が持つ（`record_inbound_message` / `record_outbound_reply` / `record_interaction_response` — #158）。記録の種別（metadata の`source`）は列挙型`opencrab_actions::TranscriptSource`で受け、行の形は`server/src/transcript.rs`が所有する（transport の feature flag に依存しない）
- **`DiscordGateway`（serenity/songbird 実装）**: serenity Botの接続・メッセージ受信・送信を担当。`opencrab-discord`の`gateway`モジュールが所有する（#1-A で`opencrab-gateway`から移設。共有層に SDK を残さない）。`opencrab-gateway`はポート（メッセージ型 / `GatewayActions`）だけを提供し、feature も SDK も持たない
- **`server`の`discord` feature**: `opencrab-discord`をoptional依存として有効化する（`default = ["discord"]`）。SDK（serenity/songbird）は`opencrab-discord`が引き込むので、`server`が`serenity`へ直接依存する必要はない

Discord のオン/オフは`server`の`discord` feature 軸で切る（`opencrab-gateway`側の feature ではない）:

```
cargo build                                          → 既定。discord 有効（opencrab-discord + serenity/songbird をコンパイル）
cargo build -p opencrab-server --no-default-features → Discord関連コード・依存なし
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
| `session_heartbeat_config` | セッション単位ハートビート設定 (#439/#456・永続アンカー last_fired_at) |
| `agent_schedules` | per-agent 定時実行 (#455・cron/@every・last_fired_at・next は照会時算出) |
| `memory_index_nodes` | 記憶インデックスの階層ツリーノード (node_type, title, summary, log_id range) |
| `memory_index_watermark` | インデックス構築の進捗管理 (last_indexed_log_id) |

### 9.2 設計方針

- すべてのテーブルは`agent_id`でスコープ
- UPSERTパターンで冪等性を確保
- タイムスタンプはUTC RFC3339形式
- JSONフィールドでスキーマの柔軟性を確保（性格特性、メタデータ等）
- FTS5はセッションログの全文検索に使用。BM25でランキング
- Memory Indexはウォーターマーク方式の増分構築。LLMで要約を生成し、閾値超過時にバックグラウンドで自動実行

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
| ユニットテスト | ~160件 | 各クレート内のモジュール単位 |
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
