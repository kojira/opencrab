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
- **ゲートウェイ**: Discord / Nostr / Web / CLI は core/server から独立配備し、generic gate-admin/runtime UDS だけで接続する。REST は core の generic 管理 API であり具象 gateway ではない（§7、Issue #1006）

### 1.3 設計哲学

- **トレイトベースの抽象化**: LLMクライアント、アクション実行、ゲートウェイはすべてトレイトで定義。実装を差し替え可能
- **クレート分離**: 機能ごとに独立したクレートに分割。循環依存なし
- **独立 process + protocol によるプラグイン**: 具象 gateway は `server` feature へ compile-in せず独立 daemon/client として配備する。core/shared/server は opaque な generic protocol だけを持ち、新 gateway 追加で変更・migration・redeploy しない（§2.4、§7）
- **エージェント中心設計**: すべてのデータ（記憶、スキル、Soul、ワークスペース）はエージェントIDに紐づく

---

## 2. アーキテクチャ

### 2.1–2.4 current target: crate/process ownership and dependency direction

The old in-process `server` feature graph was an intermediate #191 state and is not future guidance. Current production code still contains the violations inventoried in [design-gateway-process-ownership.md §3](design-gateway-process-ownership.md#3-evidence-backed-as-is-violation-inventory); implementation must move toward this common target instead of restoring compile-time gateway wiring:

```text
operator -- concrete local admin --> gateway daemon --> gateway-owned store/secrets
                                      |       |
                                      |       +-- supervises --> gateway instance child
                                      |                            |
                                      +-- generic gate-admin UDS   +-- generic runtime UDS
                                                   |                            |
                                                   v                            v
                                              core gate admin <---------- core runtime
                                                   |                            |
                                                   +------ core conversation store ---+

server ---------------- generic core APIs only --------------------------> core
       (never configures, spawns, or supervises a concrete gateway daemon)
```

**Core-owned generic conversation/execution state** means agents, positive-integer subjects and their generic allocation/tombstone/grant safeguards, sessions/membership/history, session locks, generic schedules/heartbeat, subtasks, generic gate rows, inbound deduplication, and the existing delivery ledger/state machine. **Gateway-owned concrete platform state/lifecycle** means endpoint/account IDs, credentials, external identity projections, admission/delivery policy, subscriptions, display behavior, platform schema/admin, platform send adapters, and instance children. “State belongs in core” applies only to the first class. “Transport has no generic conversation runtime” does not make the gateway stateless. Generic supervision utilities may be shared, but only an independently deployed gateway daemon supervises its instance children; `server` never does.

`core`, `db`, `gateway`, `actions`, `extgate`, `gate-client`, and `server` have no production dependency on a concrete gateway crate. Concrete daemons may depend on `gate-client` and platform-neutral process utilities, not core SQLite/server types. The separate offline migrator is the only dependency exception. Generic `AgentRuntime`, one session-lock/subtask/schedule implementation, opaque gate rows, runtime framing, dynamic declarations, the pre-separation one-frame/one-handler mapping, historical adapter calls, and terminal outcomes are retained. Issue #1006 does not add a gateway emission ledger, reconnect replay, prepared protocol, or stronger delivery guarantee. See [design-plugin-architecture.md §3](design-plugin-architecture.md#3-コアが持つもの--外に出すもの) and [design-gateway-process-ownership.md](design-gateway-process-ownership.md) for the closed boundary and transition.

### 2.5 境界を機械で守る検査（CI）

R4–R7 were useful historical checks: they proved SDK removal from shared trees, feature detachability, and no concrete identifiers in `core`. The former R5/R6 `opencrab-server --features discord|nostr|web` matrix described the superseded in-process intermediate and must not be restored. The target audit instead checks all shared/server production dependency trees with `--edges no-dev`, all relevant manifests, and production AST/macro tokens; it rejects concrete gateway dependencies, names, schema/query/route vocabulary, config decoding, lifecycle supervision, and name-based gateway-operation policy. Dev-only Discord/Nostr QC dependencies remain permitted and tested as dev edges. Historical migrations, tests, comments, dead/legacy code, and persisted history are classified rather than mechanically word-deleted. The exact V01–V16 audit and completion evidence are in [design-gateway-process-ownership.md §3–4](design-gateway-process-ownership.md#3-evidence-backed-as-is-violation-inventory).

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

分類の権威は**各ツール定義が自ら名乗る属性**（`GatewayActionDef.class.dispatch` = `Inline` / `Dispatchable`）である。この属性は必須で既定値を持たない（`ToolClass` に `Default` を実装しない）ため、**新しいゲートウェイツールを足すと分類の記述を強制される**（構築サイトで書かない限りコンパイルが通らない）。消費側（`BridgedExecutor`）は gateway／MCP の定義を舐めて名前→分類の索引を作り、`inline_tool_names()` で `Inline` の名前を集める。

共通アクション群（core）だけは `GatewayActionDef` を持たない一次ツールなので、例外的に `crates/actions/src/bridge.rs` の定数（`CORE_INLINE_ACTIONS` / `CORE_DISPATCHABLE_ACTIONS` の対）で分類し、`ActionDispatcher` の全名がどちらかに属することを fail-closed 検査（`core_actions_are_classified_for_dispatch`）が守る。

制御ツール（`spawn_subtask` / `cancel_subtask` / `report_progress`）だけは 3 つ目の源として `default_non_dispatch_tools()`（`crates/actions/src/subtask.rs`）に直接ハードコードされ、常に inline に残る（それ自体が subtask ライフサイクルを操作するため）。

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

### 7.1 Current target and AS-IS label

Discord and Nostr each become an independently deployed owning daemon with gateway-owned store/local admin and supervised instance children. Web and CLI remain independent generic-runtime clients and own their concrete authentication/admission/display behavior; Web also owns a durable identity/policy SQLite store and local admin UDS whenever configured or migrated Web identities exist, while CLI needs no durable store today. REST is a core API, not a gateway. All communicate with core only through generic gate-admin/runtime protocols. The common target diagram and ownership vocabulary are in §2.1–2.4.

`AgentGatewayLifecycle`, `AgentGatewayRegistry`, server feature wiring, per-platform runner traits, concrete server routes, and platform-shaped timed-fire descriptors are **AS-IS/superseded intermediate architecture**, not extension points. Remove reachable violations according to #1006, while retaining generic `AgentRuntime`, session locks, schedules/heartbeat, subtasks, opaque gate rows, runtime UDS, dynamic operation declarations, inbound deduplication, and the existing single core delivery state machine. Existing Web/CLI independent clients and server dev-only QC dependencies are valid.

### 7.2 Data and lifecycle boundary

Core receives normalized content, opaque external event/reply material, binding IDs, and generic caller roles. The gateway authenticates external identities and owns platform policy. Issue #1006 preserves the `1c3b782` admitted caller-role snapshot: core transports the gateway-classified generic role with work and does not add current co-agent relationship/revision revalidation at later model, queue, tool, continuation, schedule/heartbeat, subtask, or delivery boundaries. Immediate revocation/revision revalidation is deferred to non-gating Issue #1015. Core owns generic conversation execution and never parses platform locators/config. A gateway daemon owns storage, credentials, policy, final display/delivery behavior, child readiness/exit/backoff, and stale-child recovery. `server` neither configures nor supervises it.

External delivery preserves the `1c3b782` contract. Core retains the existing `deliveries` row and `sending` / `delivered` / `failed` / `indeterminate` transitions and writes one generic frame to a live acknowledged binding, invoking the concrete gateway delivery handler once. Platform API calls are counted separately: Discord performs its historical ordered one-`create_message` call per produced chunk until all chunks succeed or the first fails, with no automatic retry; Nostr performs one current post/reply command attempt. Disconnect and startup stale sends become terminal `indeterminate` without replay. Issue #1006 adds no gateway emission ledger, prepared request/protocol digest, capability upgrade/downgrade logic, retention handshake, durable nonce retry, persisted signed-event replay, or delivery-guarantee label. The known external send-before-receipt ambiguity is not strengthened by this separation. All of those category-B enhancements are deferred to one separately designed follow-up Issue recorded in `docs/evidence/issue-1006-strict-separation-redesign.md`; it is not a #1006 completion gate.

Every legacy external-identity row, including `rest`, `extgate`, Web, arbitrary, empty, or ambiguous `trusted_users.platform/source`, receives a canonical source-row fingerprint disposition. Derivable Discord/Nostr rows map to those stores; genuine REST/API rows map to `api_principals`; all others require explicit fingerprint-bound operator edges to one or more gateway instance stores or `api_principals`. Global `extgate` fan-out requires edge-by-edge confirmation. Web edges require `--web-db` (or generic destination adapter). Core plus every participating gateway DB is included in completeness digests, freeze, and rollback; guessing, silent duplication/drop, and unmapped rows are forbidden.

### 7.3 Concrete paths

| Path | Target owner/process | Boundary |
|---|---|---|
| Discord | `discord-gatewayd` + gateway-owned SQLite + one instance child per enabled instance | generic admin/runtime UDS; current placement executable is retained as child |
| Nostr | `nostr-gatewayd` + gateway-owned SQLite + one instance child per enabled instance | generic admin/runtime UDS; direct core DB and runtime import are removed |
| Web | independently launched Web gateway + Web-owned identity/policy SQLite when needed | runtime UDS plus distinct mode-0600 Web-local admin UDS; Web never opens core DB |
| CLI | independently launched CLI gateway | runtime UDS; CLI owns terminal selection/input/display |
| REST | core/server generic administration | not a concrete gateway; no platform settings/identity API |

The approved evidence-backed source inventory, migration mapping, and completion criteria are authoritative in [design-gateway-process-ownership.md](design-gateway-process-ownership.md); its [§13 staged TDD plan](design-gateway-process-ownership.md#13-staged-assertion-level-tdd-execution-plan) governs implementation order and gates.

---

## 8. サーバーとAPI

`server` composes generic core administration and the agent execution pipeline. It may expose agents, subjects, sessions, history, LLM/tool settings, schedules/heartbeat, and non-gateway API principals. It must not expose or proxy concrete gateway configuration, credentials, external identities, policy, or lifecycle, and must not merge a concrete gateway router. The AS-IS `server::create_router_with_gate`/production route inventory merge of `opencrab_extgate::admin_router` into the permissive-CORS public router is removed. Public TCP returns 404 for all six generic gate-admin paths.

Generic core gate-admin uses a required separate `[gate_admin]` config with `listen_socket` and `bootstrap_credential_file`; this core admin UDS is distinct from `[gate].listen_socket` (generic runtime) and every gateway-local admin UDS. S1 adds only platform-neutral security tables: sealed principals with a random salt and domain-separated SHA-256 hash of a strict 32-byte bearer, normalized six-operation scopes, immutable positive-subject and canonical-instance scopes or one subject-bounded UUIDv5 creation namespace, expiry/revocation/rotation lineage, and append-only sanitized request audit. Plaintext tokens are never persisted. The exact constraints, indexes, hash input, operation identifiers, and target rules are normative in [design-gateway-process-ownership.md §6](design-gateway-process-ownership.md#6-generic-gate-admin-protocol). S2, not S1, owns subject allocation/tombstone/grant and binding-authority changes.

Before any listener, core strictly parses one regular mode-`0600` versioned JSON credential manifest owned by the core service process's effective UID (UID 0 only when the service runs as root), without following symlinks. It rejects wrong ownership without privileged handoff/override, duplicate/unknown fields, malformed or non-32-byte base64url tokens, empty/unknown scopes, insecure path metadata, and expired/revoked/conflicting state; it zeroizes plaintext buffers. First bootstrap inserts the principal with nullable `sealed_at`, inserts scopes only while unsealed, and performs exactly one `sealed_at NULL -> timestamp` transition atomically. A separate `revoked_at NULL -> timestamp` is the only other principal update; combined updates, reseal/unseal, unrevocation, timestamp edits, and scope insertion after seal are forbidden, and bootstrap/auth/restart reject any pre-existing unsealed row.

Absent-principal bootstrap and rotation use one immediate write transaction to full-scan every other sealed principal, rehash the decoded bearer with each stored salt, compare without early return, reject any duplicate credential, and only then insert/seal. Restart is read-only and requires exactly one matching row—the requested principal—plus exact token, expiry, normalized scopes, and optional predecessor/overlap metadata; it cannot unrevoke/rescope/extend/rotate an existing principal. Every request likewise scans all candidates in one consistent read transaction and authorizes only one unique match. Zero or multiple matches, missing, wrong, expired, revoked, and out-of-scope credentials share one redacted unauthorized response; first-match selection and scope union are forbidden. Authorized mutations and sanitized audit outcomes are atomic through an outer transaction/savepoint; reads and denials are also audited without denied target, token, body, config, path, SQL, salt, or hash bytes. Issuance/revocation/replacement belongs to generic core operator tooling and never creates a seventh gate-admin route.

Startup validates config/security migration/manifest/bootstrap, securely requires an absent socket path, binds, sets and verifies socket mode `0600` and core-service-effective-UID ownership, and constructs the protected router before runtime/public listeners may start. It never unlinks a pre-existing stale path; cleanup removes only the matching device/inode/type/owner socket created by that attempt. Empty-token production construction and fail-open startup are forbidden.

The REST conversation path continues to record a generic session message, build the agent context/history, run `SkillEngine`, and record/return the response. Old Discord/Nostr/Web feature routes, `AppState` concrete gateway fields, channel/trusted-platform routes, and server-owned gateway startup descriptions are AS-IS violations scheduled by #1006, not APIs to copy. Gateway final delivery is selected by dynamic generic hello metadata, never by server decoding opaque config.

---

## 9. データベーススキーマ

### 9.1 テーブル一覧

| テーブル | 用途 |
|----------|------|
| `gate_admin_principals` | S1 の generic admin principal。`sealed_at` は同一 bootstrap transaction の scope 作成中だけ NULL、認証時は必ず sealed。opaque ID、32-byte salt/hash、expiry/revocation、predecessor/overlap lineage。plaintext token は保存しない |
| `gate_admin_principal_operations` | 6 個の closed generic operation scope（principal と operation の複合主キー） |
| `gate_admin_principal_subjects` / `gate_admin_principal_instances` | sealed 後 immutable な positive subject / canonical instance exact-target scope |
| `gate_admin_principal_creation_namespaces` | principal ごと最大 1 個、nonempty subject set に bound された deterministic UUIDv5 creation scope |
| `gate_admin_request_audit` | request UUID、matched principal、authorized target、generic outcome の append-only sanitized audit。denial の raw target/secret は持たない |
| `agents` | エージェント基本情報。既存の正の INTEGER `subject_id` は UUID 化・採番し直しをせず byte-for-byte 保存 |
| `subject_id_allocator` | gateway 非依存の単調 high-water/next-ID。既存最大値より上へ backfill し、減算・再利用禁止 |
| `subject_tombstones` | hard delete で agent/subject row を消す前に同一 transaction で記録する永続 `subject_id` tombstone |
| `subject_association_grants` | `(agent_id, subject_id)` に bind した短命・hashed・single-use grant と consume/expiry tombstone。既存 gate association は grant 不要 |
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
| `session_heartbeat_config` | `(agent_id, session_id)` 単位の汎用ハートビート設定 (#439/#456・永続アンカー last_fired_at) |
| `session_heartbeat_instructions` | `session_heartbeat_config` と同じ複合主キー/FK `(agent_id, session_id)`。nullable `override_text`（NULL は current agent instructions → generic default を継承）と `updated_at` を持ち、upgrade の単一 stopped projection transaction と fresh schema の双方で作成 |
| `agent_schedules` | per-agent 定時実行 (#455・cron/@every・last_fired_at・next は照会時算出) |
| `memory_index_nodes` | 記憶インデックスの階層ツリーノード (node_type, title, summary, log_id range) |
| `memory_index_watermark` | インデックス構築の進捗管理 (last_indexed_log_id) |

### 9.2 設計方針

- 会話・記憶テーブルは`agent_id`でスコープする。gate-admin security/audit は principal と generic target でスコープし、gateway kind/name を持たない
- 通常状態は UPSERT パターンで冪等性を確保する。sealed gate-admin principal/scope は UPSERT せず、exact-idempotent comparison または explicit replacement/revocation だけを許す
- 外部設定/APIの時刻はUTC RFC3339、DB内部の gate-admin security/audit 時刻は比較可能な UTC Unix nanoseconds とする
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
- **gateway boundary/release test**: S1 security schema の fresh/populated migration・rollback、legal/illegal seal/revoke/scope transition、core-service-EUID ownership を含む strict manifest/path、exact-idempotent bootstrap/conflict、bootstrap/rotation duplicate-bearer refusal、request の full-scan zero/one/multiple match と no-first-match/no-scope-union、operation/subject/instance/namespace/expiry/revocation/rotation、audit redaction/append-only/savepoint atomicity、stale-socket refusal/created-inode cleanup/startup ordering、public TCP 全 6 path 404 と protected core UDS-only success を検証する。加えて `1c3b782` と同じ単一 core delivery row/state、generic frame/handler 数、disconnect/startup `indeterminate`、Discord の ordered one-`create_message`-per-produced-chunk と fail-fast、Nostr single command の parity、全 `trusted_users.platform/source` fingerprint の approved disposition/zero-unmapped/Web destination/all-store rollback、gateway-owned caller classification と admitted role snapshot parity、subject ID 保存/non-reuse/grant single-use、immutable projection marker、startup/read-only exact-retry の byte/logical 不変と mismatch refusal、read-only `verify-freeze` の DB 不変、preflight/cleanup 間 mutation abort、cleanup 中の全 destination read lock、separate atomic cleanup/freeze/manifest record、post-QC digest lineage を検証する。synthetic gateway は core/shared/server source・schema・migration・redeploy を 0 変更で追加できなければならない

---

## 12. 運用

The previous commands that enabled Discord/Nostr/Web as `opencrab-server` features and configured `[gateway.discord]` were instructions for the superseded in-process architecture. Do not use them as implementation guidance. The target deployment starts core/server generic services and each selected concrete gateway daemon/client independently. Each daemon receives only its gateway-owned DB/admin paths, generic gate-admin/runtime UDS paths, and separately sourced secrets; it never receives a core/legacy DB path. Operators administer platform state through the daemon-local UDS, provision core generic rows through gate-admin, and let the daemon supervise its own children.

Cutover is offline and follows [design-gateway-process-ownership.md §9–11](design-gateway-process-ownership.md#9-offline-migration-and-completeness-proof). Exactly two stopped/offline migration programs may open core directly read-write for gateway legacy state: first, the single `project-core-state` transaction before QC is the sole generic legacy-state projection phase and alone may materialize approved `api_principals`, create/backfill generic safeguards, or project heartbeat targets; second, the guarded destructive-cleanup transaction after post-QC freeze/read-only `verify-freeze` revalidates disposition/freeze lineage, deletes/drops legacy concrete source state, preserves retained generic state, and atomically records cleanup/freeze/manifest IDs in a separate cleanup/applied record without updating the projection marker. Ordinary core-owned schema migrations, S1 principal bootstrap/request audit, and generic operator-plane issuance/revocation neither read nor project gateway legacy state and are normal core administration, not a third offline phase. `verify-freeze` itself opens core and every participating gateway DB read-only and changes no database or marker. Cleanup holds immutable gateway handles/read locks while its core write transaction repeats all checks; any intervening mutation aborts before deletion. Runtime/daemon direct writes and a third migration phase are forbidden; live gateway writes use gate-admin. The unchanged order is matched core-plus-all-participating-gateway backups; fingerprint-approved read-only-core import into every destination (including Web when targeted); `project-core-state`; protected core gate-admin UDS token/mode verification; public TCP six-path 404 verification; provision-only QC; external core-plus-all-participating-destination post-QC freeze snapshots/manifest; read-only `verify-freeze`; guarded destructive cleanup; and rollback using the exact complete freeze set. The projection marker is immutable from its first-phase commit; startup, `verify-freeze`, and cleanup never modify it. A stopped lost-response rerun reads core only and returns no-op `already_applied` solely when recorded operation/request identity, version, immutable digests, and recomputed initial fingerprint match exactly; mismatch fails closed. Runtime-mutated `last_fired_at` is excluded from long-lived equality and is validated through the post-QC freeze instead. The generic projection preserves the existing core `deliveries` table byte/logically and adds no delivery-guarantee classification, gateway receipt, request reconstruction, or replay. Existing disconnect/startup handling remains authoritative: ambiguous or stale `sending` rows become terminal `indeterminate` under the historical behavior. Production deployment remains an operator action.
