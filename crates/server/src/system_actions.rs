//! エージェント（owner 限定）が OpenCrab 自体の設定を変更するためのサーバ内ツール源。
//!
//! `AppState`（db / llm_router / llm_config）を必要とするため、素の dispatcher
//! アクションでは配線できない。`GatewayActions` として実装し、`BridgedExecutor` の
//! 単一 `gateway_actions` スロットに載せる。既存の gateway（Discord/Nostr 等）を
//! `inner` として保持し、自分が扱わないツールは委譲する（composite）ことで、
//! transport 非依存に「設定ツール」を全ターンへ供給する。
//!
//! owner ゲートは bridge の `OWNER_ONLY_ACTIONS`（可視性 + 実行の双方）が担うが、
//! 多層防御として本ハンドラでも caller を確認する（fail-closed）。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use opencrab_actions::{
    cancel_subtask as neutral_cancel_subtask, CancelOutcome, SettleKind, SubtaskCompletionSink,
    SubtaskRegistry, SubtaskSettled, REJECTION_CODE_PREFIX,
};
use opencrab_gateway::{
    GatewayActionDef, GatewayActionResult, GatewayActions, GatewayCallContext, GatewayCaller,
};
use opencrab_mcp::is_valid_server_name;
use serde_json::{json, Value};

use crate::AppState;

/// `report_progress` のデバウンス待機時間。Discord 実装（`execute_report_progress`）と同一。
///
/// この時間内に後続の `report_progress` が来たら世代が進み、古い方は発火しない。
const PROGRESS_DEBOUNCE_DELAY: Duration = Duration::from_secs(3);

/// `configure_llm_provider` などのサーバ内設定ツールを提供する `GatewayActions`。
pub struct SystemGatewayActions {
    state: AppState,
    /// transport 固有の gateway（Discord/Nostr 等）。自分が扱わないツールを委譲する。
    inner: Option<Arc<dyn GatewayActions>>,
    /// auto-dispatch した走行中 subtask の共有 registry（#161）。web/Nostr/REST でも
    /// `cancel_subtask` を露出するため server-neutral 層に配線する。`run_agent_response`
    /// が dispatcher へ渡すものと同一 Arc（Discord では gateway_actions の registry とも
    /// 同一）。`None` の場合は走行中 subtask が無く cancel は not found を返す。
    subtask_registry: Option<SubtaskRegistry>,
    /// 停止（`cancel_subtask`）を通知する完了 sink（この run の `RunRequest` と同一）。
    ///
    /// 停止は `on_subtask_cancelled`（既定は no-op）で通知するため resume は起きない。
    /// REST のように「最後の subtask の決着でセッションを完了にする」経路は、この通知
    /// を受けて `sessions.status` の整合を取る（無いと永久 `active` のまま残る）。
    completion_sink: Option<Arc<dyn SubtaskCompletionSink>>,
    /// transport が提供する A2UI 描画面（#156 S3）。`inner` から 1 度だけ引く。
    ///
    /// `send_ui` の実体は gateway 非依存層（`opencrab_actions::a2ui`）にあるが、描画と
    /// ユーザー応答の受け取りは transport にしか作れない。`Some` のときだけ `send_ui`
    /// を露出する（描画できない transport のターンに「必ず失敗するツール」を出さない）。
    a2ui: Option<Arc<opencrab_core::a2ui::A2uiSurface>>,
    /// transport が提供する素テキストの配送口（#157 S7）。`inner` から 1 度だけ引く。
    ///
    /// `request_peer_review` の実体は gateway 非依存層（`crate::peer_review`）にあるが、
    /// 宛先検査・メンション記法・1 通の上限・送信そのものは transport にしか作れない。
    /// `a2ui` と違い**露出は絞らない**（配送口の無い transport でも定義に出す）: ツールが
    /// transport の有無で消えないようにするのが #157 の目的で、無いときは実行だけが
    /// 明示エラーになる。
    text_delivery: Option<Arc<dyn opencrab_core::text_delivery::TextDelivery>>,
}

impl SystemGatewayActions {
    pub fn new(
        state: AppState,
        inner: Option<Arc<dyn GatewayActions>>,
        subtask_registry: Option<SubtaskRegistry>,
        completion_sink: Option<Arc<dyn SubtaskCompletionSink>>,
    ) -> Self {
        let a2ui = inner.as_ref().and_then(|i| i.a2ui_surface());
        let text_delivery = inner.as_ref().and_then(|i| i.text_delivery());
        Self {
            state,
            inner,
            subtask_registry,
            completion_sink,
            a2ui,
            text_delivery,
        }
    }

    /// 本ツール源が直接提供するツール定義（A2UI 描画面がある構成の全量）。
    ///
    /// 分類の網羅性検査（`server_tools_are_classified_for_dispatch`）はこの**全量**を
    /// 見るので、`send_ui` も分類を強制される。
    fn own_definitions() -> Vec<GatewayActionDef> {
        let mut defs = Self::always_own_definitions();
        defs.push(opencrab_actions::send_ui_definition());
        defs
    }

    /// `with_a2ui` が false のときは `send_ui` を落とす。
    ///
    /// `send_ui` は A2UI を描画できる transport（現状 Discord）のターンだけに出す。
    /// 移設前は `DiscordGatewayActions::definitions()` にしか無かったので、これで
    /// 露出範囲が移設前と一致する。
    fn own_definitions_with_a2ui(with_a2ui: bool) -> Vec<GatewayActionDef> {
        let mut defs = Self::own_definitions();
        if !with_a2ui {
            defs.retain(|d| d.name != "send_ui");
        }
        defs
    }

    /// 描画面の有無に依存しないツール定義。
    fn always_own_definitions() -> Vec<GatewayActionDef> {
        vec![
            GatewayActionDef {
                name: "configure_llm_provider".to_string(),
                description:
                    "LLM プロバイダの設定を即時適用する（owner 限定）。DB オーバーライドに\
                保存してルーターをホットスワップするため再起動は不要。codex/cursor は適用後に\
                起動確認（health_check）を行い、失敗した場合は自動的に直前の設定へロールバック\
                して結果で通知する。acp と API キー型は自動ロールバックの対象外（acp の起動確認は\
                ネットワーク依存で誤判定しうるため。ダッシュボードの接続テストで明示的に確認する）。\
                各フィールドは三値: 省略=変更しない / null=オーバーライド解除（TOML に戻す）/ 値=上書き。\
                api_key はこのツールでは変更できない（ダッシュボードから設定する）。"
                        .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "provider": {
                            "type": "string",
                            "description": "対象プロバイダ名（例: acp, codex, cursor, openai）。"
                        },
                        "enabled": {
                            "type": ["boolean", "null"],
                            "description": "有効/無効。null で解除。"
                        },
                        "default_model": {
                            "type": ["string", "null"],
                            "description": "既定モデル。null で解除。"
                        },
                        "binary_path": {
                            "type": ["string", "null"],
                            "description": "起動バイナリ（subprocess）。空文字/null で解除。"
                        },
                        "args": {
                            "type": ["array", "null"],
                            "items": { "type": "string" },
                            "description": "起動引数（subprocess）。null で解除。"
                        },
                        "working_dir": {
                            "type": ["string", "null"],
                            "description": "作業ディレクトリ。空文字/null で解除。"
                        },
                        "timeout_secs": {
                            "type": ["integer", "null"],
                            "description": "タイムアウト秒。null で解除。"
                        },
                        "reasoning_effort": {
                            "type": ["string", "null"],
                            "description": "推論強度（low/medium/high 等）。空文字/null で解除。"
                        },
                        "base_url": {
                            "type": ["string", "null"],
                            "description": "API ベース URL。null で解除。"
                        }
                    },
                    "required": ["provider"]
                }),
            },
            GatewayActionDef {
                name: "manage_allowed_commands".to_string(),
                description:
                    "自分（このエージェント）が execute_shell で実行できる許可コマンドを\
                管理する（owner 限定）。許可コマンドの追加はシェル実行範囲を広げるため owner のみ。"
                        .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["list", "add", "remove"],
                            "description": "list=一覧 / add=追加 / remove=削除。"
                        },
                        "command": {
                            "type": "string",
                            "description": "add/remove 対象のコマンド（例: git, cargo）。list では不要。"
                        }
                    },
                    "required": ["action"]
                }),
            },
            GatewayActionDef {
                name: "configure_nostr".to_string(),
                description:
                    "自分の Nostr 連携設定（購読リレー・フィルタ authors/keywords/kinds・\
                有効/無効）を変更する（owner 限定）。秘密鍵は変更も取得もできない（鍵生成は別手段）。\
                省略したフィールドは現状維持。enabled=true にするには author か keyword が必要。\
                設定は保存と同時にマネージャへ反映（enabled なら起動 / 無効なら停止）。"
                        .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "relays": {
                            "type": "array", "items": {"type": "string"},
                            "description": "購読リレー URL 一覧（例: wss://yabu.me）。"
                        },
                        "authors": {
                            "type": "array", "items": {"type": "string"},
                            "description": "購読する author（npub/hex）。"
                        },
                        "keywords": {
                            "type": "array", "items": {"type": "string"},
                            "description": "購読キーワード。"
                        },
                        "kinds": {
                            "type": "array", "items": {"type": "integer"},
                            "description": "購読する kind 番号。"
                        },
                        "enabled": {
                            "type": "boolean",
                            "description": "有効化して起動 / 無効化して停止。"
                        }
                    }
                }),
            },
            GatewayActionDef {
                name: "configure_self".to_string(),
                description:
                    "自分（このエージェント）の人格・モデル・推論強度・web 検索などの設定を変更する\
                （owner 限定）。model/reasoning_effort/web_search の変更は次ターン以降に反映される。\
                指示文の変更は update_instructions / update_heartbeat_instructions を使う。\
                省略したフィールドは変更しない。null で解除（既定に戻す。persona_name は解除不可）。"
                        .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "persona_name": {"type": "string", "description": "ペルソナ名。"},
                        "personality": {"type": ["string", "null"], "description": "性格・思考スタイル。"},
                        "job_title": {"type": ["string", "null"], "description": "肩書き。"},
                        "organization": {"type": ["string", "null"], "description": "所属。"},
                        "model": {"type": ["string", "null"], "description": "既定モデル（provider:model）。次ターン以降に反映。"},
                        "reasoning_effort": {"type": ["string", "null"], "description": "推論強度（low/medium/high 等）。"},
                        "web_search": {"type": ["boolean", "null"], "description": "本文URL読取り/web 検索の有効化。"}
                    }
                }),
            },
            GatewayActionDef {
                name: "configure_mcp_server".to_string(),
                description:
                    "自分の MCP サーバ設定を管理する（owner 限定）。追加/更新・削除・有効切替が\
                でき、変更後は接続をバックグラウンドで貼り直す。env の値は結果に出さない（キー名のみ）。\
                add で env を省略すると既存の env を保持する。"
                        .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["list", "add", "remove", "set_enabled"],
                            "description": "list=一覧 / add=追加更新 / remove=削除 / set_enabled=有効切替。"
                        },
                        "name": {"type": "string", "description": "サーバ論理名（英数字・_・-、__ 不可）。list 以外で必須。"},
                        "command": {"type": "string", "description": "起動コマンド（add で必須、例: npx）。"},
                        "args": {"type": "array", "items": {"type": "string"}, "description": "起動引数。"},
                        "env": {"type": "object", "description": "追加環境変数（キー→値）。省略で既存保持。値は結果に出さない。"},
                        "trusted_only": {"type": "boolean", "description": "true で owner/trusted のターンのみ露出。"},
                        "enabled": {"type": "boolean", "description": "有効/無効。"}
                    },
                    "required": ["action"]
                }),
            },
            // bootstrap ツール（鍵不要）。送信系（nostr_post 等・鍵前提）とは分離し、
            // transport 非依存で全ターンに露出する。これにより「鍵を作るツールが鍵の
            // ある時しか出ない」循環依存（#141）を解消する。owner 限定にはしない
            // （nsec は返さず・送信もしないので Agent 呼び出しでも安全）。
            GatewayActionDef {
                name: "nostr_generate_key".to_string(),
                description: "新しい Nostr 鍵（keypair）を生成する。任意で vanity prefix（npub の \
                              npub1 以降・bech32 文字のみ。長さ上限は無いが、長いほど探索に時間が \
                              かかる＝3文字程度で即時、それ以上は徐々に長くなる）を指定できる。返るのは公開情報の \
                              npub / pubkey のみ。**秘密鍵(nsec)はサーバ内に安全に保存され、あなた（LLM）\
                              には渡されない**（セキュリティのため）。これは新規 keypair を作るユーティリティ\
                              であり、あなた自身の送信用アイデンティティは変更しない。"
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "prefix": {"type": "string", "description": "任意。npub の npub1 以降に前置したい bech32 文字列（長さ上限なし。長いほど探索に時間がかかる, 例: cat）。"}
                    }
                }),
            },
            // bootstrap ツール（鍵不要）。`nostr_generate_key` と対で、生成した鍵の npub
            // 一覧を返す（採用候補の確認）。transport 非依存で全ターンに露出する。
            // owner 限定にはしないが、bridge の `TRUSTED_ONLY_ACTIONS` により未信頼の
            // 会話ターン（caller=Agent）には出さない（`nostr_switch_identity` と同じ扱い）。
            GatewayActionDef {
                name: "nostr_list_keys".to_string(),
                description: "自分が nostr_generate_key で生成した鍵の一覧（npub のみ）を返す。\
                              nostr_switch_identity で本鍵に採用する候補を確認するのに使う。\
                              返るのは公開情報の npub だけで、**秘密鍵(nsec)は一切返らない**。"
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {}
                }),
            },
            // bootstrap ツール（鍵不要）。生成鍵を本鍵として採用し、**未設定でも自力で
            // 接続する**（採用時は絞り込みを自動設定せず、nostaro の mention-only 既定に
            // 委ねて自分宛のみを購読する / #271）。
            // transport 非依存で全ターンに露出する。bridge の `TRUSTED_ONLY_ACTIONS` に
            // より未信頼の会話ターン（caller=Agent）には出さない（乗っ取り防止 / #264）。
            GatewayActionDef {
                name: "nostr_switch_identity".to_string(),
                description: "自分が nostr_generate_key で生成した鍵を、この Nostr ゲートウェイの\
                              **本鍵（送信・受信のアイデンティティ）として採用**する。以後の投稿は\
                              その鍵で行われる。まだ Nostr に接続していなければ、この操作で自動的に\
                              接続まで行う（自分への言及を購読する最小フィルタを設定して起動する）。\
                              npub には nostr_generate_key で作った鍵の npub を渡す。重要な操作なので\
                              owner（信頼ユーザー）からの依頼時のみ実行される。秘密鍵は扱わない\
                              （npub 参照のみ）。"
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "npub": {"type": "string", "description": "本鍵に採用する、生成済み鍵の npub。"}
                    },
                    "required": ["npub"]
                }),
            },
            // 薄い nostaro passthrough（#268）。server-own / TRUSTED_ONLY。投稿・返信・
            // kind:0 プロフィール設定・チャンネル・取得など nostaro が持つ操作を**すべて**
            // 会話/heartbeat/オーナーの trusted ターンから使えるようにする（既存の inner
            // `nostr_post`/`reply` は Nostr 受信ターン用にそのまま残る）。opencrab が守るのは
            // 「鍵のエージェント間混同防止（config は ctx.agent_id 固定）」と「nsec 隠蔽」の
            // 2 点だけで、Nostr 仕様の判断は nostaro に委ねる（非劣化）。`init`（鍵作成/上書き）・
            // `watch`（無制限受信）・`relay`（config.toml⇔DB desync で揮発）だけ拒否する。
            GatewayActionDef {
                name: "nostr_run".to_string(),
                description: "Nostr CLI（nostaro）を薄く passthrough 実行する。`subcommand` に \
                              nostaro のサブコマンド（例: event / post / reply / dm / zap / upload / \
                              react / repost / follow / unfollow / profile / channel / get / timeline / \
                              search / decode / pubkey など）を、`args` にそのサブコマンドの\
                              フラグと値を**1 要素ずつ**配列で渡す（例: subcommand=\"event\", \
                              args=[\"--kind\",\"0\",\"--content\",\"{...}\"] で kind:0 プロフィールを設定）。\
                              投稿・プロフィール(kind:0)設定・チャンネル・取得など nostaro が持つ操作を\
                              すべて使える。署名は**あなた自身の採用済み Nostr 鍵**で行われ、秘密鍵(nsec)は\
                              扱わない・見えない。鍵の作成/採用は nostr_generate_key / \
                              nostr_switch_identity を使うこと（init は不可）。受信の常時監視（watch）は\
                              ここからは起動できない。リレー設定は opencrab 側（configure_nostr / \
                              ダッシュボード）で管理するため relay サブコマンドは不可。まだ鍵を採用して\
                              いない場合は先に nostr_switch_identity で採用すること。"
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "subcommand": {
                            "type": "string",
                            "description": "nostaro のサブコマンド（init/watch は不可）。"
                        },
                        "args": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "サブコマンドの引数。フラグと値を 1 要素ずつ配列で渡す（省略可）。"
                        }
                    },
                    "required": ["subcommand"]
                }),
            },
            // 実行中の subtask を停止するツール（#161）。Discord gateway 実装だけに
            // あった cancel_subtask を server-neutral 層へ露出し、web/Nostr/REST でも
            // 自動 dispatch された subtask を停止できるようにする。認可（親セッション/
            // owner 限定）は共有 registry を引く実体（cancel_subtask）が担う。
            // サブタスクの起動（#175 S4）。Discord gateway 実装だけにあった
            // spawn_subtask を server-neutral 層へ移し、web / REST / Nostr / heartbeat
            // でもサブタスクを起動できるようにする。実体は
            // `crate::subtask_spawn::spawn_subtask`（sub-engine は自前で組まず
            // `run_agent_response` を depth+1 で再入呼び出しする）。
            GatewayActionDef {
                name: "spawn_subtask".to_string(),
                description: "バックグラウンドでサブタスクを起動します。LLMエンジンがサブエンジンとして非同期実行し、完了後にメインエンジンを自動的に再呼び出しします。複雑な長時間処理（画像生成・コード実装・調査など）に使用してください。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "task": {
                            "type": "string",
                            "description": "サブエンジンに実行させるタスクの説明"
                        },
                        "timeout_secs": {
                            "type": "integer",
                            "description": "タイムアウト秒数（省略時1800秒）"
                        },
                        "label": {
                            "type": "string",
                            "description": "サブタスクのラベル（通知の表示用。省略時はtask先頭を使用）"
                        },
                        "webhook": {
                            "type": "object",
                            "description": "subtask lifecycle の通知先（省略時はエージェント既定 / グローバル既定を使用）。",
                            "properties": {
                                "url": {
                                    "type": "string",
                                    "description": "通知先の webhook URL"
                                },
                                "events": {
                                    "type": "array",
                                    "description": "通知するイベント（省略時は全て）。started/progress/completed/failed/timed_out/aborted",
                                    "items": { "type": "string" }
                                }
                            },
                            "required": ["url"]
                        }
                    },
                    "required": ["task"]
                }),
            },
            GatewayActionDef {
                name: "cancel_subtask".to_string(),
                description: "実行中のサブタスクをキャンセルします。キャンセルできるのは自分のセッションが親のサブタスクのみ（owner は制限なし）。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "subtask_id": {
                            "type": "string",
                            "description": "キャンセルするサブタスクのID（subtask_spawnedイベントから取得）"
                        }
                    },
                    "required": ["subtask_id"]
                }),
            },
            // 記憶インデックスの全再構築（#175 S4）。Discord gateway 実装だけにあった
            // ものを server-neutral 層へ移す。LLM クライアントを必要とする唯一の
            // Discord ツールだったため、これを移すことで discord crate が LLM を
            // 知らなくなる（#155 の前進）。
            GatewayActionDef {
                name: "rebuild_memory_index".to_string(),
                description: "メモリインデックスをゼロから再構築する。既存のインデックスを削除し、全ログを再インデックスする。時間がかかることがある。結果として logs_indexed（処理したログ数）と nodes_created（作成したインデックスノード数）を返す。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
            },
            // サブタスクの進捗報告（#175 S1）。Discord gateway 実装だけにあった
            // report_progress を server-neutral 層へ露出し、web / Nostr / REST /
            // heartbeat でもサブエンジンが進捗を報告できるようにする。引数スキーマは
            // Discord 側の定義と同一（sub-engine の system prompt が「subtask_id は
            // 省略可」と案内している契約を保つ）。
            GatewayActionDef {
                name: "report_progress".to_string(),
                description: "サブエンジンからメインエンジンへ進捗を報告します。depth >= 1のサブエンジンのみ使用可能。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "message": {
                            "type": "string",
                            "description": "進捗メッセージ"
                        },
                        "subtask_id": {
                            "type": "string",
                            "description": "このサブタスクのID（オプション）"
                        }
                    },
                    "required": ["message"]
                }),
            },
            // ---- #157 S1: gateway 非依存の汎用管理ツール（Discord から移設） ----
            //
            // 以下 4 つは実装が serenity を一切参照せず DB と実行許可設定だけに依存して
            // いたのに、Discord gateway にしか無かったため web / Nostr / REST / heartbeat
            // 経由のターンでは使えなかった（#157 / #155）。定義・引数スキーマ・
            // レスポンス JSON はすべて Discord 実装から**1 文字も変えずに**移している。
            // 実体は `crate::agent_management`。
            GatewayActionDef {
                name: "update_memory_index_config".to_string(),
                description: "メモリインデックスの設定（batch_size、threshold）を更新する。少なくとも1つのパラメータを指定する必要がある。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "batch_size": {
                            "type": "integer",
                            "description": "一度に処理するメモリのバッチサイズ"
                        },
                        "threshold": {
                            "type": "integer",
                            "description": "インデックス再構築の閾値"
                        }
                    },
                    "required": []
                }),
            },
            GatewayActionDef {
                name: "add_allowed_command".to_string(),
                description: "シェルツールの許可コマンドリストに新しいコマンドを追加する。オーナーのみ実行可能。コマンド名（例: curl, wget, git）を指定する。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "追加するコマンド名（英数字・ハイフン・アンダースコアのみ。例: curl, wget, git）"
                        }
                    },
                    "required": ["command"]
                }),
            },
            GatewayActionDef {
                name: "list_allowed_commands".to_string(),
                description: "現在DBに保存されている許可コマンドの一覧を取得する。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
            },
            GatewayActionDef {
                name: "remove_allowed_command".to_string(),
                description: "シェルツールの許可コマンドリストからコマンドを削除する。オーナーのみ実行可能。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "削除するコマンド名"
                        }
                    },
                    "required": ["command"]
                }),
            },
            // ---- #157 S6: スキル生成（Discord から移設） ----
            //
            // 実装は DB のみに依存していたのに Discord gateway にしか無かった。定義・
            // 引数スキーマ・レスポンス JSON・エラー文言は Discord 実装から**1 バイトも
            // 変えずに**移している。実体は `crate::agent_management::create_skill`。
            //
            // 権限は bridge の `TRUSTED_ONLY_ACTIONS`（可視性 + 実行の双方）とハンドラ内
            // 検査の**二重構造**。許可集合は owner / co_agent / trusted_user で完全一致して
            // おり、bridge 側は名前ベースなので移設しても効き続ける。
            //
            // 似た名前の core アクション `create_my_skill`（`source_type="self_created"` /
            // `situation_pattern` 必須）とは**統合しない**。#157 の目的は「汎用の実体を
            // transport 層から出す」ことで、重複解消は別の話。ツール名を消すと過去の
            // 会話ログに残る呼び出しが通らなくなる。
            GatewayActionDef {
                name: "create_skill".to_string(),
                description: "ユーザーから「〇〇するスキルを作って」と言われたとき新しいスキルを作成する。guidanceにコマンド例・使い方を書くことで、LLMがexecute_shellで動的に実行できるようになる。同名スキルが存在する場合は更新される。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "スキル名"
                        },
                        "description": {
                            "type": "string",
                            "description": "スキルの説明"
                        },
                        "guidance": {
                            "type": "string",
                            "description": "スキルのガイダンス（省略時は空文字列）"
                        }
                    },
                    "required": ["name", "description"]
                }),
            },
            // ---- #157 S3: ハートビート指示ツール（Discord から移設） ----
            //
            // 実装は DB のみに依存していたのに Discord gateway にしか無かった。定義・
            // 引数スキーマ・レスポンス JSON は Discord 実装から**1 文字も変えずに**移して
            // いる。実体は `crate::heartbeat_instructions`。
            // 権限は bridge の `OWNER_ONLY_ACTIONS`（update）/ `TRUSTED_ONLY_ACTIONS`
            // （read）が可視性と実行の双方でゲートし、ハンドラ内検査も残す（多層防御）。
            // **チャンネル単位の設定は非対称**（`scope="channel"` が触るのは Discord の
            // チャンネル設定テーブルなので、非 Discord 経路では通常「行が無い」応答に
            // なる）。詳細は `crate::heartbeat_instructions` の doc。
            GatewayActionDef {
                name: "update_heartbeat_instructions".to_string(),
                description: "ハートビート（自律発言）時の振る舞い指示を更新する。オーナーが「これからハートビートでは○○して」と明示的に依頼した文脈でのみ呼ぶこと。出力形式（SPEAK/LEARN/IDLE）はランタイムが固定するため、ここでは頻度・トーン・話題・沈黙条件などの方針のみを書く。オーナー限定。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "scope": {
                            "type": "string",
                            "enum": ["agent", "channel"],
                            "description": "agent=エージェント全体のグローバル指示、channel=特定チャンネルの上書き。"
                        },
                        "channel_id": {
                            "type": "string",
                            "description": "scope=channelのとき必須。対象チャンネルの数値ID。"
                        },
                        "guild_id": {
                            "type": "string",
                            "description": "scope=channelで新規にチャンネル設定を作成する場合に必要なサーバーの数値ID。"
                        },
                        "instructions": {
                            "type": "string",
                            "description": "新しいハートビート指示の全文（最大4000字）。"
                        },
                        "reason": {
                            "type": "string",
                            "description": "変更理由（監査ログに記録される。省略可）。"
                        }
                    },
                    "required": ["scope", "instructions"]
                }),
            },
            GatewayActionDef {
                name: "read_heartbeat_instructions".to_string(),
                description: "現在のハートビート指示を読み出す。scope=agentでエージェント全体、scope=channelでチャンネル上書きのみ、scope=effectiveで実際にtickで使われる合成結果（解決ルール適用後）を返す。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "scope": {
                            "type": "string",
                            "enum": ["agent", "channel", "effective"],
                            "description": "agent / channel / effective。channel・effectiveのときはchannel_id必須。"
                        },
                        "channel_id": {
                            "type": "string",
                            "description": "scope=channel または effective のとき必須。対象チャンネルの数値ID。"
                        }
                    },
                    "required": ["scope"]
                }),
            },
            // ---- #252 段階 C: エージェント自身の Nostr 受信 → Discord 転記先設定 ----
            //
            // 段階 A（#253）が敷いた `agent_nostr_relay_config` を、エージェント自身が
            // own ツールで読み書きする。引数に `agent_id` は**無い**。対象は常に
            // `ctx.agent_id`（呼び出し文脈）で、他エージェントを指す経路は作らない。
            // 実体と「自分のだけ」の保証・秘匿値の扱いは `crate::agent_nostr_relay` の doc。
            GatewayActionDef {
                name: "get_my_nostr_relay".to_string(),
                description: "自分（呼び出し元エージェント）の Nostr 受信 → Discord 転記の設定を読み出す。転記が有効か・転記先が設定済みか（転記先 URL は伏字で返す）を返す。他のエージェントの設定は読めない。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {},
                }),
            },
            GatewayActionDef {
                name: "set_my_nostr_relay".to_string(),
                description: "自分（呼び出し元エージェント）が Nostr で受け取った自分宛の受信（メンション/リプライ/DM）を Discord へ転記する設定を更新する。対象は常に自分で、他のエージェントの設定は変えられない。enabled で転記の有効/無効を、webhook_url で転記先の Discord webhook URL を設定する。URL が Discord webhook として不正なら拒否される（丸められない）ので、拒否されたらエラーの理由を見て正しい URL で指定し直すこと。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "enabled": {
                            "type": "boolean",
                            "description": "転記を有効にするか。省略すると現在の値を保つ。"
                        },
                        "webhook_url": {
                            "type": "string",
                            "description": "転記先の Discord webhook URL。空文字または null を渡すと転記先を消去する。省略すると現在の値を保つ。"
                        }
                    }
                }),
            },
            // ---- #247 段階 2: エージェント自身のハートビート設定 ----
            //
            // **指示文（`update_heartbeat_instructions`）とは別物**。あちらは
            // 「動いたとき何をするか」でオーナー限定のまま。こちらは「いつ動くか」で、
            // エージェント自身が触れる（下限つき）。
            //
            // 引数に `agent_id` は**無い**。対象は常に `ctx.agent_id`（呼び出し文脈）。
            // 実体と「自分のだけ」の保証は `crate::agent_heartbeat` の doc を参照。
            GatewayActionDef {
                name: "get_my_heartbeat".to_string(),
                description: "自分（呼び出し元エージェント）のハートビート設定を読み出す。有効か・間隔（秒）・設定できる下限と上限を返す。設定したことが無ければ無効。他のエージェントの設定は読めない。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {},
                }),
            },
            GatewayActionDef {
                name: "set_my_heartbeat".to_string(),
                description: "自分（呼び出し元エージェント）のハートビート（自律実行）の有効/無効と間隔を設定する。対象は常に自分で、他のエージェントの設定は変えられない。間隔には運用者が決めた下限があり、それより短い値は拒否される（丸められない）ので、拒否されたらエラーに載っている下限以上で指定し直すこと。ハートビートで何をするかの指示文はこのツールでは変えられない（オーナー限定の別ツール）。なお現時点ではこの設定はまだ発火の判定には使われていない（保存のみ）。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "enabled": {
                            "type": "boolean",
                            "description": "自律実行を有効にするか。省略すると現在の値を保つ。"
                        },
                        "interval_secs": {
                            "type": "integer",
                            "description": "ハートビートの間隔（秒）。下限は get_my_heartbeat の min_interval_secs、上限は max_interval_secs。null を渡すと運用者の既定に戻る。省略すると現在の値を保つ。"
                        }
                    }
                }),
            },
            // ---- #157 S5: 通知先（webhook）の管理ツール（Discord から移設） ----
            //
            // 実装は DB と設定ファイル由来の既定値しか触らないのに Discord gateway に
            // しか無かった。定義・引数スキーマ・レスポンス JSON・エラー文言は Discord
            // 実装から**1 バイトも変えずに**移している。実体は `crate::webhook_targets`。
            // 権限はハンドラ内検査のみ（bridge の owner/trusted リストには無い＝単層）。
            //
            // `ensure_webhook` / `ensure_subtask_webhook` は **Discord に残る**。既存
            // デフォルトが無いとき `discord_create_webhook`（serenity 依存）で webhook を
            // 新規作成するためで、ここには定義しない。
            GatewayActionDef {
                name: "get_default_subtask_webhook".to_string(),
                description: "spawn_subtask が webhook 未指定時に実際に使うデフォルト subtask webhook を解決して返す。トークンは秘匿され redacted_url のみ返る。owner/trusted_user/co_agent のみ。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "agent_id": {
                            "type": "string",
                            "description": "対象エージェントID（省略時は自分）。"
                        },
                        "tool_name": {
                            "type": "string",
                            "description": "tool scope を解決する際のツール名（省略可）。"
                        },
                        "scope": {
                            "type": "string",
                            "description": "参考情報（解決は固定順序: tool>agent>global>env）。"
                        }
                    },
                    "required": []
                }),
            },
            GatewayActionDef {
                name: "set_default_subtask_webhook".to_string(),
                description: "scope（agent/tool/global）ごとのデフォルト subtask webhook を設定する。urlを空/省略にするとそのscopeを無効化（enabled=false）する。owner限定。応答にrawトークンは含まれない。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "scope": {
                            "type": "string",
                            "enum": ["agent", "tool", "global"],
                            "description": "agent=エージェント既定、tool=spawn_subtaskツール既定、global=全体既定。"
                        },
                        "agent_id": {
                            "type": "string",
                            "description": "対象エージェントID（省略時は自分。global では '*' に強制）。"
                        },
                        "tool_name": {
                            "type": "string",
                            "description": "scope=tool のとき省略時 'spawn_subtask'。"
                        },
                        "url": {
                            "type": "string",
                            "description": "Discord webhook URL。空/省略でそのscopeを無効化する。"
                        },
                        "enabled": {
                            "type": "boolean",
                            "description": "有効/無効（url指定時のデフォルトtrue）。"
                        },
                        "events": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "通知イベント（省略時は全て）。"
                        },
                        "output_mode": {
                            "type": "string",
                            "description": "出力モード（省略時 'summary'）。"
                        },
                        "max_chars": {
                            "type": "integer",
                            "description": "最大文字数（省略時 1500）。"
                        },
                        "kind": {
                            "type": "string",
                            "description": "種別（省略時 'subtask'）。"
                        }
                    },
                    "required": ["scope"]
                }),
            },
            GatewayActionDef {
                name: "list_subtask_webhooks".to_string(),
                description: "登録されている subtask webhook 設定を一覧する。トークンは秘匿され redacted_url のみ返る。owner/trusted_user/co_agent のみ。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "agent_id": {
                            "type": "string",
                            "description": "対象エージェントID（省略時は自分。globalも併せて返る）。"
                        },
                        "scope": {
                            "type": "string",
                            "description": "scopeで絞り込み（省略可）。"
                        },
                        "include_disabled": {
                            "type": "boolean",
                            "description": "無効化済みも含めるか（省略時 false）。"
                        }
                    },
                    "required": []
                }),
            },
            GatewayActionDef {
                name: "get_default_webhook".to_string(),
                description: "実際に使われるデフォルト webhook を解決して返す（既定 family='activity'＝一般ツール/コマンド活動）。トークンは秘匿され redacted_url のみ返る。owner/trusted_user/co_agent のみ。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "family": {
                            "type": "string",
                            "enum": ["activity", "subtask"],
                            "description": "解決するファミリ（省略時 'activity'）。"
                        },
                        "agent_id": {
                            "type": "string",
                            "description": "対象エージェントID（省略時は自分）。"
                        },
                        "tool_name": {
                            "type": "string",
                            "description": "tool scope を解決する際のツール名（省略可）。"
                        }
                    },
                    "required": []
                }),
            },
            GatewayActionDef {
                name: "set_default_webhook".to_string(),
                description: "scope（agent/tool/global）ごとのデフォルト webhook を設定する（既定 family='activity'）。urlを空/省略にするとそのscopeを無効化（enabled=false）する。owner は全 scope、agent は自分の agent-scope のみ設定/無効化できる。応答にrawトークンは含まれない。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "scope": {
                            "type": "string",
                            "enum": ["agent", "tool", "global"],
                            "description": "agent=エージェント既定、tool=ツール既定、global=全体既定。"
                        },
                        "family": {
                            "type": "string",
                            "enum": ["activity", "subtask"],
                            "description": "設定するファミリ（省略時 'activity'）。"
                        },
                        "agent_id": {
                            "type": "string",
                            "description": "対象エージェントID（省略時は自分。global では '*' に強制）。"
                        },
                        "tool_name": {
                            "type": "string",
                            "description": "scope=tool のとき省略時 'spawn_subtask'。activity の特定ツール宛先はツール名を指定する。"
                        },
                        "url": {
                            "type": "string",
                            "description": "Discord webhook URL。空/省略でそのscopeを無効化する。"
                        },
                        "enabled": {
                            "type": "boolean",
                            "description": "有効/無効（url指定時のデフォルトtrue）。"
                        },
                        "events": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "通知イベント（省略時は全て）。"
                        },
                        "output_mode": {
                            "type": "string",
                            "description": "出力モード（省略時 'summary'）。"
                        },
                        "max_chars": {
                            "type": "integer",
                            "description": "最大文字数（省略時 1500）。"
                        }
                    },
                    "required": ["scope"]
                }),
            },
            GatewayActionDef {
                name: "list_webhooks".to_string(),
                description: "登録されている webhook 設定を一覧する。`family`/`scope` で絞り込み可（省略時は全件）。トークンは秘匿され redacted_url のみ返る。owner/trusted_user/co_agent のみ。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "agent_id": {
                            "type": "string",
                            "description": "対象エージェントID（省略時は自分。globalも併せて返る）。"
                        },
                        "family": {
                            "type": "string",
                            "description": "family（kind）で絞り込み（省略可）。例: 'activity' / 'subtask'。"
                        },
                        "scope": {
                            "type": "string",
                            "description": "scopeで絞り込み（省略可）。"
                        },
                        "include_disabled": {
                            "type": "boolean",
                            "description": "無効化済みも含めるか（省略時 false）。"
                        }
                    },
                    "required": []
                }),
            },
            // ピアレビュー依頼（#157 S7）。定義は gateway 非依存層が持つ（transport の
            // 配送口の有無に関わらず露出する）。
            crate::peer_review::request_peer_review_definition(),
        ]
    }

    /// bootstrap 用の鍵生成（鍵未設定でも実行可能）。実体は `NostaroCli::vanity`
    /// （config 非依存）で、生成した nsec は**サーバ内に 0600 で保存**し LLM には返さない
    /// （npub/pubkey のみ）。process.rs の防御マスク（tool_name==nostr_generate_key）と
    /// bridge の nsec redaction が多層で秘密漏洩を防ぐ。
    async fn nostr_generate_key(
        &self,
        args: &Value,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        let prefix = args
            .get("prefix")
            .and_then(|v| v.as_str())
            .map(|s| s.trim())
            .unwrap_or("");
        // 登録済みの Nostr transport の払い出し口（binary_path 等の設定を継承）を使う。
        // **無ければ既定**（#191 段階2 PR4）。ここは「受け口が無ければ拒否」ではない:
        // 移設前も `unwrap_or_default()` で既定 CLI にフォールバックしており、鍵生成は
        // ゲートウェイの稼働を必要としない（bootstrap 用途 = 鍵が無い状態から呼ぶ）。
        // これをガードに変えると「鍵がまだ無いから起動もしていない」正しい経路が
        // 塞がる（#191 の PR3 で踏みかけた、順序・フォールバックをガードへ機械的に
        // 移し替える誤りと同じ形）。
        let provisioning = self
            .state
            .gateways
            .get(opencrab_actions::gateway_kinds::NOSTR)
            .and_then(|gw| gw.key_provisioning())
            .unwrap_or_else(|| Arc::new(opencrab_nostr::NostrKeyProvisioning::default()));
        match provisioning.generate_key(prefix).await {
            Ok(k) => match provisioning.store_generated_key(&ctx.agent_id, &k) {
                Ok(_) => GatewayActionResult {
                    success: true,
                    // nsec は返さない（サーバ内 0600 保存済み）。npub/pubkey のみ。
                    data: Some(json!({
                        "npub": k.public_id,
                        "pubkey": k.public_key_hex,
                        "note": "新しい鍵を生成しました。秘密鍵(nsec)はサーバ内に安全に保存済みで、セキュリティ上あなた（LLM）には渡していません。共有・言及してよいのは npub までです。",
                    })),
                    error: None,
                },
                Err(e) => err(format!("鍵は生成しましたが保存に失敗しました: {e}")),
            },
            Err(e) => err(format!("nostr_generate_key 失敗: {e}")),
        }
    }

    /// bootstrap 用の鍵一覧（鍵未設定でも実行可能）。生成鍵（`generated-keys/<npub>.nsec`）の
    /// **npub のみ**を返す。実体は `NostaroCli::list_generated_keys`（ファイル名だけを列挙し、
    /// nsec 本文は開かない）。鍵生成と同じく transport の稼働を必要としない。
    fn nostr_list_keys(ctx: &GatewayCallContext) -> GatewayActionResult {
        match opencrab_nostr::NostaroCli::list_generated_keys(&ctx.agent_id) {
            Ok(npubs) => GatewayActionResult {
                success: true,
                data: Some(json!({
                    "npubs": npubs,
                    "note": "あなたが生成した鍵の npub 一覧です。nostr_switch_identity で本鍵に採用できます。秘密鍵(nsec)はサーバ内に安全に保存されており、ここには含まれません。",
                })),
                error: None,
            },
            Err(e) => err(format!("nostr_list_keys 失敗: {e}")),
        }
    }

    /// bootstrap 用の identity 採用（#264）。生成鍵を本鍵として採用し、未接続なら
    /// **自己ブートストラップで接続まで行う**（絞り込みは自動設定せず、nostaro の
    /// mention-only 既定に委ねて自分宛のみを購読する / #271）。実体は Nostr transport の
    /// `identity_provisioning` capability。
    ///
    /// 稼働の有無は capability の内側で判定する（稼働中はホットスワップ、未稼働は bootstrap
    /// 起動＝接続）。**秘密鍵(nsec)は扱わない**（npub 参照のみ・応答にも出さない）。
    async fn nostr_switch_identity(
        &self,
        args: &Value,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        let Some(npub) = args
            .get("npub")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            return err("npub パラメータが必要です".to_string());
        };
        // Nostr transport の採用 capability を引く。登録が無ければ Nostr 非対応構成。
        let Some(provisioning) = self
            .state
            .gateways
            .get(opencrab_actions::gateway_kinds::NOSTR)
            .and_then(|gw| gw.identity_provisioning())
        else {
            return err(
                "この環境では Nostr identity の採用は利用できません（Nostr 未構成）".to_string(),
            );
        };
        match provisioning.adopt_identity(&ctx.agent_id, npub).await {
            Ok(adopted) => GatewayActionResult {
                success: true,
                data: Some(json!({
                    "npub": adopted,
                    "note": "この鍵を本鍵として採用しました。未接続だった場合は自分への言及を購読する最小フィルタで Nostr に接続済みです。以後の投稿・公開ノート受信はこの identity で行われます。秘密鍵は扱っていません。",
                })),
                error: None,
            },
            Err(e) => err(format!("nostr_switch_identity 失敗: {e}")),
        }
    }

    /// 薄い nostaro passthrough（#268）。server-own / TRUSTED_ONLY。
    ///
    /// 稼働中（登録済み）の Nostr transport の passthrough capability
    /// （[`opencrab_actions::GatewayNostrPassthrough`]）へ委譲する。config は常に
    /// `ctx.agent_id` のもの（鍵混同防止）。`init`/`watch` の拒否・`--config` 上書きの封じ・
    /// 未 materialize（鍵未採用）の明示エラー・nsec マスクは capability の内側
    /// （`NostaroCli::run_passthrough`）で行う。呼び出し側はここで subcommand と args を
    /// 取り出して渡すだけ。
    async fn nostr_run(&self, args: &Value, ctx: &GatewayCallContext) -> GatewayActionResult {
        let Some(subcommand) = args
            .get("subcommand")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            return err("subcommand パラメータが必要です".to_string());
        };
        // args は文字列配列（フラグと値を 1 要素ずつ）。省略可。非文字列要素は弾く
        // （引数はそのまま argv になるので、数値等は文字列で渡させる）。
        let sub_args: Vec<String> = match args.get("args") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => {
                let mut out = Vec::with_capacity(items.len());
                for it in items {
                    match it.as_str() {
                        Some(s) => out.push(s.to_string()),
                        None => {
                            return err(
                                "args の要素はすべて文字列で指定してください（数値も文字列で）"
                                    .to_string(),
                            )
                        }
                    }
                }
                out
            }
            Some(_) => return err("args は文字列の配列で指定してください".to_string()),
        };
        // Nostr transport の passthrough capability を引く。登録が無ければ Nostr 非対応構成。
        let Some(passthrough) = self
            .state
            .gateways
            .get(opencrab_actions::gateway_kinds::NOSTR)
            .and_then(|gw| gw.nostr_passthrough())
        else {
            return err(
                "この環境では Nostr passthrough は利用できません（Nostr 未構成）".to_string(),
            );
        };
        match passthrough.run(&ctx.agent_id, subcommand, &sub_args).await {
            Ok(out) => GatewayActionResult {
                success: true,
                data: Some(json!({ "result": out })),
                error: None,
            },
            Err(e) => err(format!("nostr_run 失敗: {e}")),
        }
    }

    /// 記憶インデックスの全再構築（#175 S4）。旧 Discord 実装
    /// （`DiscordGatewayActions::execute_rebuild_memory_index`・撤去済み）をそのまま
    /// 移設したもので、LLM クライアントは `AppState` のルーターから組む。
    async fn rebuild_memory_index(&self, ctx: &GatewayCallContext) -> GatewayActionResult {
        let llm_client = crate::llm_adapter::LlmRouterAdapter::new(self.state.llm_router.clone())
            .with_agent_id(&ctx.agent_id);

        let (config, persona_name, personality, effective_model) = {
            let Ok(conn) = self.state.db.lock() else {
                return err("db lock failed".to_string());
            };
            let config = opencrab_db::queries::get_memory_index_config(&conn, &ctx.agent_id)
                .unwrap_or_else(|_| opencrab_db::queries::AgentMemoryIndexConfig {
                    agent_id: ctx.agent_id.clone(),
                    batch_size: opencrab_db::queries::BATCH_SIZE_DEFAULT,
                    threshold: opencrab_db::queries::THRESHOLD_DEFAULT,
                    updated_at: String::new(),
                });
            let (persona_name, personality) = opencrab_db::queries::get_agent(&conn, &ctx.agent_id)
                .ok()
                .flatten()
                .map(|a| (a.persona_name, a.personality))
                .unwrap_or_default();
            let effective_model = opencrab_db::queries::effective_model_for_agent(
                &conn,
                &ctx.agent_id,
                &self.state.default_model,
            )
            .unwrap_or_else(|_| self.state.default_model.clone());
            (config, persona_name, personality, effective_model)
        };

        match opencrab_core::memory_index::IndexBuilder::rebuild_index(
            &self.state.db,
            &ctx.agent_id,
            &llm_client,
            &effective_model,
            config.batch_size as usize,
            &persona_name,
            personality.as_deref(),
        )
        .await
        {
            Ok(result) => GatewayActionResult {
                success: true,
                data: Some(json!({
                    "agent_id": ctx.agent_id,
                    "logs_indexed": result.logs_indexed,
                    "nodes_created": result.nodes_created,
                    "message": format!(
                        "メモリインデックスを再構築しました（{}件のログ → {}ノード作成）",
                        result.logs_indexed, result.nodes_created,
                    ),
                })),
                error: None,
            },
            Err(e) => {
                tracing::error!("rebuild_memory_index failed: {e}");
                err(format!("メモリインデックスの再構築に失敗: {e}"))
            }
        }
    }

    /// 実行中 subtask の停止（#161・#157 S2）。共有 `SubtaskRegistry` を引き、認可
    /// （親セッション/owner 限定）・abort・除去・lifecycle 通知・親ログ記録・sink 通知を
    /// server-neutral の `cancel_subtask` に委ねる。**これが唯一の実装**で、transport 固有の
    /// 停止実装は無い（#157 S2 で Discord 実装を撤去し、その固有の後始末を neutral 層へ
    /// 取り込んだ）。registry 未配線（`None`）や不在は not found を返す。権限なしは
    /// `REJECTION_CODE_PREFIX` を付けて拒否として通知する（旧 Discord 実装と同契約）。
    fn cancel_subtask(&self, args: &Value, ctx: &GatewayCallContext) -> GatewayActionResult {
        let Some(subtask_id) = args.get("subtask_id").and_then(|v| v.as_str()) else {
            return err("cancel_subtask: 'subtask_id' is required".to_string());
        };
        let Some(registry) = self.subtask_registry.as_ref() else {
            // dispatch 未配線（走行中 subtask を追跡していない）→ 不在扱い。
            return err(format!("cancel_subtask: subtask '{subtask_id}' not found"));
        };
        let is_owner = ctx.caller == GatewayCaller::Owner;
        match neutral_cancel_subtask(
            registry,
            &self.state.db,
            self.completion_sink.as_deref(),
            // 中断の lifecycle 通知（旧 Discord 実装の後始末）はこのマップ経由で行う。
            // `spawn_subtask` が insert したものと同一 Arc（`AppState` 共有）。
            Some(&self.state.subtask_notifiers),
            subtask_id,
            is_owner,
            ctx.session_id.as_deref(),
        ) {
            CancelOutcome::Cancelled => GatewayActionResult {
                success: true,
                data: Some(json!({ "cancelled": true, "subtask_id": subtask_id })),
                error: None,
            },
            CancelOutcome::NotFound => {
                err(format!("cancel_subtask: subtask '{subtask_id}' not found"))
            }
            CancelOutcome::Unauthorized => err(format!(
                "{REJECTION_CODE_PREFIX}cancel_subtask: subtask '{subtask_id}' をこのセッションからキャンセルする権限がありません（親セッションまたは owner のみ）"
            )),
        }
    }

    /// サブタスクの進捗報告（#175 S1）。Discord 実装（`execute_report_progress`）の
    /// transport 非依存な部分を移植したもの。Discord 固有の webhook 送出は Discord 側に
    /// 残る実装が担当する（この own 実装は inner が未実装の経路でのみ走る）。
    ///
    /// 手順は Discord と同一:
    /// 1. `message` 必須 / セッション必須（fail-closed）
    /// 2. 登録簿から subtask を引く（`subtask_id` 明示 → 無ければ session_id で逆引き）
    /// 3. 所有権ゲート（自分自身の subtask か、自分が親のもののみ）
    /// 4. 親セッションログへ `subtask_progress` を記録（本文の永続化はここだけ）
    /// 5. デバウンス後に完了 sink へ `SettleKind::Progress` を通知（メインエンジン再呼び出し）
    async fn report_progress(&self, args: &Value, ctx: &GatewayCallContext) -> GatewayActionResult {
        let Some(message) = args.get("message").and_then(|v| v.as_str()) else {
            return err("report_progress: 'message' is required".to_string());
        };
        let message = message.to_string();
        // セッション必須（fail-closed）: 親セッションの解決が session_id に依存する（#36）。
        let current_session_id = match ctx.session_id.as_deref() {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => {
                return err(
                    "report_progress はセッション文脈でのみ実行できます（session_id 不明）"
                        .to_string(),
                );
            }
        };
        let subtask_id_arg = args
            .get("subtask_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let agent_id = ctx.agent_id.clone();

        // 登録簿から (subtask_id, session_id, parent_session_id) を引く。registry 未配線
        // （`None`）は「登録簿に無い」と同じ扱い（= 自己申告として親ログにだけ残す）。
        let subtask_entry: Option<(String, String, String)> =
            self.subtask_registry.as_ref().and_then(|registry| {
                if !subtask_id_arg.is_empty() {
                    registry.get(&subtask_id_arg).map(|e| {
                        (
                            subtask_id_arg.clone(),
                            e.session_id.clone(),
                            e.parent_session_id.clone(),
                        )
                    })
                } else {
                    registry
                        .iter()
                        .find(|e| e.session_id == current_session_id)
                        .map(|e| {
                            (
                                e.key().clone(),
                                e.value().session_id.clone(),
                                e.value().parent_session_id.clone(),
                            )
                        })
                }
            });

        // 所有権ゲート（#64）: subtask_id は LLM 由来の引数なので、呼び出し元セッションの
        // サブタスク（自分自身 = session_id 一致、または自分の子 = parent_session_id 一致）
        // 以外は拒否する。無検証だと他セッションへの進捗ログ書き込み・メインエンジン
        // 再呼び出しを誘発できてしまう。
        if let Some((id, session_id, parent_session_id)) = &subtask_entry {
            if session_id != &current_session_id && parent_session_id != &current_session_id {
                return err(format!(
                    "{REJECTION_CODE_PREFIX}report_progress: subtask '{id}' は呼び出し元セッションのサブタスクではありません"
                ));
            }
        }

        let subtask_id = subtask_entry
            .as_ref()
            .map(|(id, _, _)| id.clone())
            .unwrap_or(subtask_id_arg);
        let parent_session_id = subtask_entry
            .as_ref()
            .map(|(_, _, parent)| parent.clone())
            .unwrap_or_else(|| current_session_id.clone());

        // 進捗本文は親セッションログ（DB）へ永続化する。sink には本文を運ばない
        // （RFC §1.3）ので、受け口が未配線でも本文自体はここで残る。
        if !parent_session_id.is_empty() {
            if let Ok(conn) = self.state.db.lock() {
                let log = opencrab_db::queries::SessionLogRow {
                    id: None,
                    agent_id: agent_id.clone(),
                    session_id: parent_session_id.clone(),
                    log_type: "system".to_string(),
                    content: json!({
                        "type": "subtask_progress",
                        "subtask_id": subtask_id,
                        "message": message,
                        "timestamp": Utc::now().to_rfc3339(),
                    })
                    .to_string(),
                    speaker_id: None,
                    turn_number: None,
                    metadata_json: None,
                    created_at: None,
                };
                opencrab_db::queries::insert_session_log_best_effort(&conn, &log);
            }
        }

        // 進捗を lifecycle 通知口へ流す（#175 S4）。通知口は登録簿と対の随伴マップ
        // （`AppState.subtask_notifiers`）から引く。旧 Discord 実装が webhook へ
        // progress を出していた経路の置き換えで、宛先の解決も整形も実装側に閉じている。
        if let Some((resolved_subtask_id, _, _)) = &subtask_entry {
            if let Some(notifier) = self.state.subtask_notifiers.get(resolved_subtask_id) {
                notifier.on_progress(&message);
            }
        }

        // 完了受け口が未配線の経路（`with_dispatch` していない run）では、デバウンス
        // タスクを**起動しない**。起動しても 3 秒後に通知先が無く黙って消えるだけで、
        // (a) 無駄な tokio タスクと (b) 世代カウンタの残骸を積むだけだからである。
        // 記録（上の親ログ）は済んでいるので、その旨を debug ログに残して成功を返す。
        let Some(sink) = self.completion_sink.clone() else {
            tracing::debug!(
                session_id = %current_session_id,
                parent_session_id = %parent_session_id,
                subtask_id = %subtask_id,
                "report_progress: completion sink not wired; progress logged to the parent session only (no main-engine notification)"
            );
            return GatewayActionResult {
                success: true,
                data: Some(json!({
                    "reported": true,
                    "message": message,
                    // 記録はしたが再注入はしていないことを呼び出し元に明示する。
                    "notified": false,
                })),
                error: None,
            };
        };

        // デバウンス: 3 秒待ってからメインエンジン再呼び出しを 1 回だけ発火する。
        // 世代カウンタは `AppState` 側（`ProgressDebounce`）にある。この構造体は run
        // ごとに作り直されるためフィールドに置くと毎回リセットされ、バースト時に同数の
        // LLM 再呼び出し（コスト増・チャンネルスパム）が起きる。
        let debounce = self.state.progress_debounce.clone();
        let my_generation = debounce.bump(&parent_session_id);
        let parent_session_clone = parent_session_id.clone();
        let subtask_id_clone = subtask_id.clone();
        let agent_id_clone = agent_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(PROGRESS_DEBOUNCE_DELAY).await;
            // 自分より後に report_progress が来ていたら（世代が進んでいたら）発火しない。
            if !debounce.claim_latest(&parent_session_clone, my_generation) {
                return;
            }
            sink.on_subtask_settled(SubtaskSettled {
                session_id: parent_session_clone,
                agent_id: agent_id_clone,
                subtask_id: subtask_id_clone,
                exit_reason: "progress".to_string(),
                kind: SettleKind::Progress,
                // 進捗の宛先は親セッション。返信先の復元は sink 側の責務（#167）。
                reply_target: None,
            });
        });

        GatewayActionResult {
            success: true,
            data: Some(json!({
                "reported": true,
                "message": message,
                "notified": true,
            })),
            error: None,
        }
    }

    async fn configure_llm_provider(
        &self,
        args: &Value,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        // 多層防御: bridge が owner を強制するが、ハンドラでも fail-closed で確認する。
        if ctx.caller != GatewayCaller::Owner {
            return err("configure_llm_provider requires owner".to_string());
        }
        let Some(provider) = args.get("provider").and_then(|v| v.as_str()) else {
            return err("provider is required".to_string());
        };
        let provider = provider.to_string();

        // LLM 由来の args から許可フィールドだけを抜き出して三値ボディを組む。
        // api_key は意図的に受け付けない（秘密情報を LLM 経路・ログに載せない）。
        let mut body = serde_json::Map::new();
        for key in [
            "enabled",
            "default_model",
            "binary_path",
            "args",
            "working_dir",
            "timeout_secs",
            "reasoning_effort",
            "base_url",
        ] {
            if let Some(v) = args.get(key) {
                body.insert(key.to_string(), v.clone());
            }
        }

        match crate::api::providers::apply_provider_override_with_rollback(
            &self.state,
            &provider,
            &body,
        )
        .await
        {
            Ok(outcome) => {
                let data = json!({
                    "provider": provider,
                    "applied": outcome.applied,
                    "test_ok": outcome.test_ok,
                    "rolled_back": outcome.rolled_back,
                });
                if outcome.rolled_back {
                    // 適用したが起動確認に失敗 → 元に戻した。エージェントに明示的に伝える。
                    GatewayActionResult {
                        success: false,
                        data: Some(data),
                        error: Some(format!(
                            "'{provider}' の設定を適用しましたが起動確認に失敗したため、\
                             直前の設定へ自動ロールバックしました。binary_path/args/working_dir を確認してください。"
                        )),
                    }
                } else {
                    GatewayActionResult {
                        success: true,
                        data: Some(data),
                        error: None,
                    }
                }
            }
            Err((_code, msg)) => err(msg),
        }
    }

    async fn manage_allowed_commands(
        &self,
        args: &Value,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        // 多層防御: bridge が owner を強制するが、ハンドラでも fail-closed で確認する。
        if ctx.caller != GatewayCaller::Owner {
            return err("manage_allowed_commands requires owner".to_string());
        }
        let agent_id = ctx.agent_id.clone();
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string());

        let conn = match self.state.db.lock() {
            Ok(c) => c,
            Err(_) => return err("db lock failed".to_string()),
        };
        match action {
            "list" => match opencrab_db::queries::list_agent_allowed_commands(&conn, &agent_id) {
                Ok(cmds) => GatewayActionResult {
                    success: true,
                    data: Some(json!({ "commands": cmds })),
                    error: None,
                },
                Err(e) => err(e.to_string()),
            },
            "add" => {
                let Some(cmd) = command.filter(|s| !s.is_empty()) else {
                    return err("command is required for add".to_string());
                };
                match opencrab_db::queries::add_agent_allowed_command(
                    &conn, &agent_id, &cmd, "owner",
                ) {
                    Ok(added) => GatewayActionResult {
                        success: true,
                        data: Some(json!({ "command": cmd, "added": added })),
                        error: None,
                    },
                    Err(e) => err(e.to_string()),
                }
            }
            "remove" => {
                let Some(cmd) = command.filter(|s| !s.is_empty()) else {
                    return err("command is required for remove".to_string());
                };
                match opencrab_db::queries::remove_agent_allowed_command(&conn, &agent_id, &cmd) {
                    Ok(removed) => GatewayActionResult {
                        success: true,
                        data: Some(json!({ "command": cmd, "removed": removed })),
                        error: None,
                    },
                    Err(e) => err(e.to_string()),
                }
            }
            other => err(format!("unknown action: {other} (list/add/remove)")),
        }
    }

    async fn configure_nostr(&self, args: &Value, ctx: &GatewayCallContext) -> GatewayActionResult {
        // 多層防御: bridge が owner を強制するが、ハンドラでも fail-closed で確認する。
        if ctx.caller != GatewayCaller::Owner {
            return err("configure_nostr requires owner".to_string());
        }
        let agent_id = ctx.agent_id.clone();
        // 既存設定を partial 更新のベースにする（省略フィールドは現状維持）。
        let existing = {
            let conn = match self.state.db.lock() {
                Ok(c) => c,
                Err(_) => return err("db lock failed".to_string()),
            };
            opencrab_db::queries::get_agent_nostr_config(&conn, &agent_id).unwrap_or(None)
        };
        let Some(existing) = existing else {
            return err(
                "Nostr 設定が未作成です。先に鍵を生成してください（operator がダッシュボードで生成）"
                    .to_string(),
            );
        };
        let ef: Value = serde_json::from_str(&existing.filter_json).unwrap_or_else(|_| json!({}));
        // args の配列（文字列）を取り出す。無ければ None（＝現状維持）。
        let arg_strs = |k: &str| -> Option<Vec<String>> {
            args.get(k).and_then(|x| x.as_array()).map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(|s| s.to_string()))
                    .collect()
            })
        };
        let cur_strs = |v: &Value, k: &str| -> Vec<String> {
            v.get(k)
                .and_then(|x| x.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default()
        };
        let arg_or_cur_kinds = || -> Vec<u32> {
            let extract = |v: &Value| -> Vec<u32> {
                v.as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|n| n.as_u64().map(|v| v as u32))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            match args.get("kinds") {
                Some(v) => extract(v),
                None => ef.get("kinds").map(extract).unwrap_or_default(),
            }
        };

        let relays = arg_strs("relays")
            .unwrap_or_else(|| serde_json::from_str(&existing.relays_json).unwrap_or_default());
        let authors = arg_strs("authors").unwrap_or_else(|| cur_strs(&ef, "authors"));
        let keywords = arg_strs("keywords").unwrap_or_else(|| cur_strs(&ef, "keywords"));
        let kinds = arg_or_cur_kinds();
        let enabled = args
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(existing.enabled);

        match crate::api::nostr::apply_nostr_settings(
            &self.state,
            &agent_id,
            &relays,
            &authors,
            &keywords,
            &kinds,
            enabled,
            None,
        )
        .await
        {
            Ok(()) => GatewayActionResult {
                success: true,
                // secret_key は返さない。
                data: Some(json!({
                    "agent_id": agent_id,
                    "relays": relays,
                    "authors": authors,
                    "keywords": keywords,
                    "kinds": kinds,
                    "enabled": enabled,
                })),
                error: None,
            },
            Err((_code, msg)) => err(msg),
        }
    }

    async fn configure_self(&self, args: &Value, ctx: &GatewayCallContext) -> GatewayActionResult {
        // 多層防御: bridge が owner を強制するが、ハンドラでも fail-closed で確認する。
        if ctx.caller != GatewayCaller::Owner {
            return err("configure_self requires owner".to_string());
        }
        let agent_id = ctx.agent_id.clone();

        // 三値: キー欠落=変更しない / null=解除（Some(None）) / 値=設定（Some(Some(v))）。
        let tri_string = |k: &str| -> Option<Option<String>> {
            match args.get(k) {
                None => None,
                Some(Value::Null) => Some(None),
                Some(Value::String(s)) => Some(Some(s.clone())),
                _ => None,
            }
        };
        let tri_bool = |k: &str| -> Option<Option<bool>> {
            match args.get(k) {
                None => None,
                Some(Value::Null) => Some(None),
                Some(Value::Bool(b)) => Some(Some(*b)),
                _ => None,
            }
        };

        let patch = opencrab_db::queries::AgentPatch {
            // persona_name は Option<String>（解除不可）。文字列指定時のみ設定。
            persona_name: args
                .get("persona_name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            personality: tri_string("personality"),
            job_title: tri_string("job_title"),
            organization: tri_string("organization"),
            model: tri_string("model"),
            reasoning_effort: tri_string("reasoning_effort"),
            web_search: tri_bool("web_search"),
            ..Default::default()
        };

        let result = {
            let conn = match self.state.db.lock() {
                Ok(c) => c,
                Err(_) => return err("db lock failed".to_string()),
            };
            opencrab_db::queries::apply_agent_patch(&conn, &agent_id, &patch)
        };
        match result {
            Ok(true) => GatewayActionResult {
                success: true,
                data: Some(json!({ "agent_id": agent_id, "updated": true })),
                error: None,
            },
            Ok(false) => err(format!("agent not found: {agent_id}")),
            Err(e) => err(e.to_string()),
        }
    }

    async fn configure_mcp_server(
        &self,
        args: &Value,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        // 多層防御: bridge が owner を強制するが、ハンドラでも fail-closed で確認する。
        if ctx.caller != GatewayCaller::Owner {
            return err("configure_mcp_server requires owner".to_string());
        }
        let agent_id = ctx.agent_id.clone();
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string());

        match action {
            "list" => {
                let servers = {
                    let conn = match self.state.db.lock() {
                        Ok(c) => c,
                        Err(_) => return err("db lock failed".to_string()),
                    };
                    match opencrab_db::queries::list_agent_mcp_servers(&conn, &agent_id) {
                        Ok(s) => s,
                        Err(e) => return err(e.to_string()),
                    }
                };
                // env の値は出さず、キー名のみ返す（秘密を LLM 経路に載せない）。
                let list: Vec<Value> = servers
                    .iter()
                    .map(|s| {
                        let env_keys: Vec<String> =
                            serde_json::from_str::<serde_json::Map<String, Value>>(&s.env_json)
                                .map(|m| m.keys().cloned().collect())
                                .unwrap_or_default();
                        let args_arr: Vec<String> =
                            serde_json::from_str(&s.args_json).unwrap_or_default();
                        json!({
                            "name": s.name,
                            "command": s.command,
                            "args": args_arr,
                            "env_keys": env_keys,
                            "trusted_only": s.trusted_only,
                            "enabled": s.enabled,
                        })
                    })
                    .collect();
                GatewayActionResult {
                    success: true,
                    data: Some(json!({ "servers": list })),
                    error: None,
                }
            }
            "add" => {
                let Some(name) = name.filter(|s| !s.is_empty()) else {
                    return err("name is required".to_string());
                };
                if !is_valid_server_name(&name) {
                    return err(
                        "サーバ名は英数字・_・-（1〜64文字、__ を含まない）にしてください"
                            .to_string(),
                    );
                }
                let command = args
                    .get("command")
                    .and_then(|v| v.as_str())
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default();
                if command.is_empty() {
                    return err("command が必要です".to_string());
                }
                let args_vec: Vec<String> = args
                    .get("args")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|s| s.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                // 既存を（env 保持・デフォルト継承のため）読む。
                let existing = {
                    let conn = match self.state.db.lock() {
                        Ok(c) => c,
                        Err(_) => return err("db lock failed".to_string()),
                    };
                    opencrab_db::queries::get_agent_mcp_server(&conn, &agent_id, &name)
                        .unwrap_or(None)
                };
                // env は空/未指定なら既存を保持（値を伏せているため無変更更新で消さない）。
                let env_json = match args.get("env") {
                    Some(Value::Object(m)) if !m.is_empty() => {
                        serde_json::to_string(m).unwrap_or_else(|_| "{}".to_string())
                    }
                    _ => existing
                        .as_ref()
                        .map(|e| e.env_json.clone())
                        .unwrap_or_else(|| "{}".to_string()),
                };
                let trusted_only = args
                    .get("trusted_only")
                    .and_then(|v| v.as_bool())
                    .unwrap_or_else(|| existing.as_ref().map(|e| e.trusted_only).unwrap_or(false));
                let enabled = args
                    .get("enabled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or_else(|| existing.as_ref().map(|e| e.enabled).unwrap_or(true));
                let row = opencrab_db::queries::AgentMcpServerRow {
                    agent_id: agent_id.clone(),
                    name: name.clone(),
                    command,
                    args_json: serde_json::to_string(&args_vec)
                        .unwrap_or_else(|_| "[]".to_string()),
                    env_json,
                    trusted_only,
                    enabled,
                };
                {
                    let conn = match self.state.db.lock() {
                        Ok(c) => c,
                        Err(_) => return err("db lock failed".to_string()),
                    };
                    if let Err(e) = opencrab_db::queries::upsert_agent_mcp_server(&conn, &row) {
                        return err(e.to_string());
                    }
                }
                crate::api::mcp::spawn_reload(&self.state, agent_id);
                GatewayActionResult {
                    success: true,
                    // env の値は返さない。
                    data: Some(json!({ "name": name, "upserted": true, "enabled": enabled })),
                    error: None,
                }
            }
            "remove" => {
                let Some(name) = name.filter(|s| !s.is_empty()) else {
                    return err("name is required".to_string());
                };
                let removed = {
                    let conn = match self.state.db.lock() {
                        Ok(c) => c,
                        Err(_) => return err("db lock failed".to_string()),
                    };
                    match opencrab_db::queries::delete_agent_mcp_server(&conn, &agent_id, &name) {
                        Ok(r) => r,
                        Err(e) => return err(e.to_string()),
                    }
                };
                crate::api::mcp::spawn_reload(&self.state, agent_id);
                GatewayActionResult {
                    success: true,
                    data: Some(json!({ "name": name, "removed": removed })),
                    error: None,
                }
            }
            "set_enabled" => {
                let Some(name) = name.filter(|s| !s.is_empty()) else {
                    return err("name is required".to_string());
                };
                let Some(enabled) = args.get("enabled").and_then(|v| v.as_bool()) else {
                    return err("enabled (bool) is required for set_enabled".to_string());
                };
                {
                    let conn = match self.state.db.lock() {
                        Ok(c) => c,
                        Err(_) => return err("db lock failed".to_string()),
                    };
                    if let Err(e) = opencrab_db::queries::set_agent_mcp_server_enabled(
                        &conn, &agent_id, &name, enabled,
                    ) {
                        return err(e.to_string());
                    }
                }
                crate::api::mcp::spawn_reload(&self.state, agent_id);
                GatewayActionResult {
                    success: true,
                    data: Some(json!({ "name": name, "enabled": enabled })),
                    error: None,
                }
            }
            other => err(format!(
                "unknown action: {other} (list/add/remove/set_enabled)"
            )),
        }
    }
}

fn err(msg: String) -> GatewayActionResult {
    GatewayActionResult {
        success: false,
        data: None,
        error: Some(msg),
    }
}

impl SystemGatewayActions {
    /// own 定義と inner 定義を name で dedup してマージする（own 優先）。
    ///
    /// nostr watch ループ稼働時は inner=NostrGatewayActions も nostr_generate_key を
    /// 定義するため、ここで dedup しないと
    /// ツール一覧に同名が2つ並ぶ（provider が拒否しうる）。`definitions()` の実体を
    /// 静的関数に切り出し、AppState 無しで dedup 契約を単体テストできるようにする（#161）。
    fn merge_definitions(
        mut own: Vec<GatewayActionDef>,
        inner: Option<&Arc<dyn GatewayActions>>,
    ) -> Vec<GatewayActionDef> {
        if let Some(inner) = inner {
            let own_names: std::collections::HashSet<String> =
                own.iter().map(|d| d.name.clone()).collect();
            for d in inner.definitions() {
                if !own_names.contains(&d.name) {
                    own.push(d);
                }
            }
        }
        own
    }
}

#[async_trait]
impl GatewayActions for SystemGatewayActions {
    fn definitions(&self) -> Vec<GatewayActionDef> {
        Self::merge_definitions(
            Self::own_definitions_with_a2ui(self.a2ui.is_some()),
            self.inner.as_ref(),
        )
    }

    async fn execute(
        &self,
        name: &str,
        args: &Value,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        match name {
            "configure_llm_provider" => self.configure_llm_provider(args, ctx).await,
            "manage_allowed_commands" => self.manage_allowed_commands(args, ctx).await,
            "configure_nostr" => self.configure_nostr(args, ctx).await,
            "configure_self" => self.configure_self(args, ctx).await,
            "configure_mcp_server" => self.configure_mcp_server(args, ctx).await,
            // bootstrap 鍵生成（鍵未設定でも露出）。inner より先に own が処理する。
            "nostr_generate_key" => self.nostr_generate_key(args, ctx).await,
            // bootstrap 鍵一覧（鍵未設定でも露出）。生成鍵の npub のみ返す（nsec 非返却）。
            "nostr_list_keys" => Self::nostr_list_keys(ctx),
            // bootstrap identity 採用（鍵未設定でも露出）。未接続なら自分宛のみを購読する
            // 設定で自動接続する。inner より先に own が処理する（#264）。
            "nostr_switch_identity" => self.nostr_switch_identity(args, ctx).await,
            // 薄い nostaro passthrough（#268）。稼働中の Nostr transport の passthrough
            // capability へ委譲する。inner へは委譲しない（own が処理する）。
            "nostr_run" => self.nostr_run(args, ctx).await,
            // 記憶インデックスの全再構築（#175 S4）。inner へは委譲しない。
            "rebuild_memory_index" => self.rebuild_memory_index(ctx).await,
            // 汎用エージェント管理ツール（#157 S1）。Discord 側の実装は撤去済みなので
            // inner へは委譲しない（委譲パターンにすると二重定義を招く）。許可コマンドは
            // **DB のみ**を更新する。グローバルな実行許可設定へは書かない（他エージェントへ
            // 漏れるため / #202）。次の run が `process::resolve_run_tools_config` で
            // DB から拾い直す。
            "update_memory_index_config" => {
                crate::agent_management::update_memory_index_config(&self.state, args, ctx)
            }
            "add_allowed_command" => {
                crate::agent_management::add_allowed_command(&self.state, args, ctx)
            }
            "list_allowed_commands" => {
                crate::agent_management::list_allowed_commands(&self.state, ctx)
            }
            "remove_allowed_command" => {
                crate::agent_management::remove_allowed_command(&self.state, args, ctx)
            }
            // スキル生成（#157 S6）。Discord 側の実装は撤去済みなので inner へは委譲しない
            // （委譲パターンにすると二重定義を招く）。core の `create_my_skill` とは別ツール
            // として**両方**残す（統廃合は #157 の範囲外）。
            "create_skill" => crate::agent_management::create_skill(&self.state, args, ctx),
            // ハートビート指示ツール（#157 S3）。Discord 側の実装は撤去済みなので
            // inner へは委譲しない（委譲パターンにすると二重定義を招く）。
            "update_heartbeat_instructions" => {
                crate::heartbeat_instructions::update_heartbeat_instructions(&self.state, args, ctx)
            }
            "read_heartbeat_instructions" => {
                crate::heartbeat_instructions::read_heartbeat_instructions(&self.state, args, ctx)
            }
            // エージェント自身の Nostr 転記設定（#252 段階 C）。対象は常に
            // `ctx.agent_id` で、引数から他エージェントを指す経路は無い。
            "get_my_nostr_relay" => {
                crate::agent_nostr_relay::get_my_nostr_relay(&self.state, args, ctx)
            }
            "set_my_nostr_relay" => {
                crate::agent_nostr_relay::set_my_nostr_relay(&self.state, args, ctx)
            }
            // エージェント自身のハートビート設定（#247 段階 2）。対象は常に
            // `ctx.agent_id` で、引数から他エージェントを指す経路は無い。
            "get_my_heartbeat" => crate::agent_heartbeat::get_my_heartbeat(&self.state, args, ctx),
            "set_my_heartbeat" => crate::agent_heartbeat::set_my_heartbeat(&self.state, args, ctx),
            // 通知先（webhook）の管理ツール（#157 S5）。Discord 側の実装は撤去済みなので
            // inner へは委譲しない（委譲パターンにすると二重定義を招く）。設定ファイル
            // 由来のフォールバックは `AppState::default_subtask_webhook` から読むので、
            // Discord 機能の有無に関わらず同じ既定へ到達する。
            //
            // `ensure_webhook` / `ensure_subtask_webhook` はここに**無い**（Discord に
            // 残した webhook 新規作成つきのツール）。既定の `_ =>` で inner へ委譲される。
            "get_default_subtask_webhook" => {
                crate::webhook_targets::get_default_subtask_webhook(&self.state, args, ctx)
            }
            "set_default_subtask_webhook" => {
                crate::webhook_targets::set_default_subtask_webhook(&self.state, args, ctx)
            }
            "list_subtask_webhooks" => {
                crate::webhook_targets::list_subtask_webhooks(&self.state, args, ctx)
            }
            "get_default_webhook" => {
                crate::webhook_targets::get_default_webhook(&self.state, args, ctx)
            }
            "set_default_webhook" => {
                crate::webhook_targets::set_default_webhook(&self.state, args, ctx)
            }
            "list_webhooks" => crate::webhook_targets::list_webhooks(&self.state, args, ctx),
            // subtask 起動（#175 S4）。transport 非依存の唯一の実装（Discord 側の実装は
            // 撤去済み）。inner へは委譲しない。
            "spawn_subtask" => {
                crate::subtask_spawn::spawn_subtask(
                    &self.state,
                    self.subtask_registry.as_ref(),
                    self.completion_sink.clone(),
                    // sub-engine の inner は「自分を包む合成 gateway」。`BridgedExecutor`
                    // が注入したハンドルを辿ることで、許可リスト内の server ツール
                    // （`report_progress` / `nostr_generate_key`）へ到達できる。
                    ctx.root_gateway.clone(),
                    args,
                    ctx,
                )
                .await
            }
            // A2UI 送信（#156 S3）。Discord 側の実装は撤去済みなので inner へは委譲しない
            // （委譲パターンにすると二重定義を招く）。描画面が無い transport では
            // `definitions()` に出ないが、モデルが名前で呼んだ場合に備えて明示エラーを返す
            // （fail-closed。黙って inner へ落とさない）。
            "send_ui" => match &self.a2ui {
                Some(surface) => {
                    opencrab_actions::send_ui(&self.state.db, surface, args, ctx).await
                }
                None => GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(
                        "send_ui はこのゲートウェイでは利用できません（UI を描画できません）"
                            .to_string(),
                    ),
                },
            },
            // ピアレビュー依頼（#157 S7）。Discord 側の実装は撤去済みなので inner へは
            // 委譲しない（委譲パターンにすると二重定義を招く）。配送口を持たない
            // transport でも**定義には出す**（#157 の目的）ので、無いときは黙って inner へ
            // 落とさず明示エラーを返す（fail-closed）。
            "request_peer_review" => match &self.text_delivery {
                Some(delivery) => {
                    crate::peer_review::request_peer_review(
                        &self.state.db,
                        delivery.as_ref(),
                        args,
                        ctx,
                    )
                    .await
                }
                None => GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(
                        "request_peer_review はこのゲートウェイでは利用できません（メッセージを送信できません）。\
                         このターンの transport はテキストを送れないため、ピアレビュー依頼は省略して先へ進んでよい。"
                            .to_string(),
                    ),
                },
            },
            // subtask 停止（#161 / #157 S2）。transport 非依存の唯一の実装（Discord 側の
            // 実装は撤去済み）。**inner へは委譲しない**: 委譲パターンのままにすると、
            // Discord が誤って `cancel_subtask` を再定義したときに own の実装（lifecycle
            // 通知 + 部分結果ログ + sink 通知）が黙ってバイパスされる。
            "cancel_subtask" => self.cancel_subtask(args, ctx),
            // subtask 進捗報告（#175 S1）。**唯一残る委譲パターン**（cancel_subtask は
            // #157 S2 で委譲を撤去した）。
            // transport 固有 gateway（Discord）が report_progress を実装しているなら、
            // その固有の後処理（lifecycle webhook への progress 送出）を保つため inner
            // へ委譲する＝ Discord 経路は挙動不変。実装していない transport
            // （web/Nostr/REST/heartbeat）では own が処理する。
            "report_progress" => {
                let inner_handles = self.inner.as_ref().is_some_and(|inner| {
                    inner
                        .definitions()
                        .iter()
                        .any(|d| d.name == "report_progress")
                });
                if inner_handles {
                    self.inner.as_ref().unwrap().execute(name, args, ctx).await
                } else {
                    self.report_progress(args, ctx).await
                }
            }
            // 自分が扱わないツールは inner gateway へ委譲する。
            _ => match &self.inner {
                Some(inner) => inner.execute(name, args, ctx).await,
                None => GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!("Unknown action: {name}")),
                },
            },
        }
    }

    /// transport の A2UI 描画面を**そのまま外へ通す**（#156 S3）。
    ///
    /// 本番の sub-engine 配線は入れ子（`spawn_subtask` が `ctx.root_gateway` = この合成
    /// gateway を子へ渡し、子の `run_agent_response` がそれを `inner` にして**もう 1 段**
    /// 合成 gateway を作り、`SubEngineGatewayActions` で包む）。ここで転送しないと、
    /// 内側の合成 gateway は描画面を得られず `send_ui` を定義しないため、sub-engine から
    /// 名前指定で呼ばれたときの拒否が「権限拒否（`rejected:`）」ではなく
    /// 「Unknown gateway action」に変わる（遮断自体は保たれるが分類が変わる）。
    /// 移設前は Discord gateway が最内まで `inner` として届いていたので `send_ui` は
    /// 常に「実在するが許可外」だった。その分類を保つための転送。
    fn a2ui_surface(&self) -> Option<Arc<opencrab_core::a2ui::A2uiSurface>> {
        self.a2ui.clone()
    }

    /// transport の素テキスト配送口を**そのまま外へ通す**（#157 S7）。
    ///
    /// `a2ui_surface()` の転送と同じ理由: 本番の sub-engine 配線は合成 gateway の入れ子
    /// なので、ここで転送しないと内側の合成 gateway が配送口を失い、`request_peer_review`
    /// が「定義には出るが必ず失敗する」状態になる（sub-engine では深さ拒否が先に効くため
    /// 実害は無いが、能力を黙って落とさない）。
    fn text_delivery(&self) -> Option<Arc<dyn opencrab_core::text_delivery::TextDelivery>> {
        self.text_delivery.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_definition_shape() {
        let defs = SystemGatewayActions::own_definitions();
        let d = defs
            .iter()
            .find(|d| d.name == "configure_llm_provider")
            .expect("configure_llm_provider must be defined");
        // provider は必須。
        let required = d.parameters["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "provider"));
        // 秘密情報 api_key は LLM ツールでは露出しない（ダッシュボード専用）。
        let props = d.parameters["properties"].as_object().unwrap();
        assert!(
            !props.contains_key("api_key"),
            "api_key must not be settable via the agent tool"
        );
        // 起動系フィールドは受け付ける。
        for key in ["binary_path", "args", "working_dir", "timeout_secs"] {
            assert!(props.contains_key(key), "missing property: {key}");
        }
    }

    /// Regression guard for #146: nostr_generate_key must be a *own* definition
    /// (bootstrap tool) so it is exposed on every turn regardless of whether the
    /// nostr watch loop / keys are configured. If someone moves it back into the
    /// key-gated inner NostrGatewayActions bundle, own_definitions loses it and
    /// this test fails — that is the "露出が二度と消えない" guard.
    #[test]
    fn nostr_generate_key_is_always_exposed() {
        let defs = SystemGatewayActions::own_definitions();
        let d = defs
            .iter()
            .find(|d| d.name == "nostr_generate_key")
            .expect("nostr_generate_key must be an own (always-exposed) definition (#146)");
        // vanity 用の任意 prefix パラメータを受ける。
        let props = d.parameters["properties"].as_object().unwrap();
        assert!(
            props.contains_key("prefix"),
            "nostr_generate_key must accept an optional vanity `prefix`"
        );
        // bootstrap ツールは required なし（引数なしでも鍵を作れる）。
        assert!(
            d.parameters.get("required").is_none(),
            "nostr_generate_key must not require any argument"
        );
    }

    /// #264: nostr_list_keys must also be a *own* (bootstrap) definition so the
    /// agent can inspect its generated keys before adopting one, even when no
    /// nostr gateway is running / no key is configured. It must not require args
    /// and must not leak nsec (it only returns npubs).
    #[test]
    fn nostr_list_keys_is_always_exposed() {
        let defs = SystemGatewayActions::own_definitions();
        let d = defs
            .iter()
            .find(|d| d.name == "nostr_list_keys")
            .expect("nostr_list_keys must be an own (always-exposed) definition (#264)");
        assert!(
            d.parameters.get("required").is_none(),
            "nostr_list_keys must not require any argument"
        );
    }

    /// #264: nostr_switch_identity must be a *own* (bootstrap) definition so an
    /// unconfigured agent can adopt a generated key and self-connect on any turn
    /// (not only when a nostr watch loop is already running). It requires `npub`.
    #[test]
    fn nostr_switch_identity_is_always_exposed() {
        let defs = SystemGatewayActions::own_definitions();
        let d = defs
            .iter()
            .find(|d| d.name == "nostr_switch_identity")
            .expect("nostr_switch_identity must be an own (always-exposed) definition (#264)");
        let required = d.parameters["required"].as_array().unwrap();
        assert!(
            required.iter().any(|v| v == "npub"),
            "nostr_switch_identity must require npub"
        );
    }

    /// definitions() dedups own vs inner by name: when the inner gateway also
    /// defines nostr_generate_key (nostr watch loop running), the merged tool
    /// list must still contain exactly one entry (providers reject duplicates).
    #[test]
    fn definitions_dedup_keeps_single_nostr_generate_key() {
        // own_definitions is the source that definitions() starts from; assert it
        // is unique there so the dedup contract holds.
        let defs = SystemGatewayActions::own_definitions();
        let count = defs
            .iter()
            .filter(|d| d.name == "nostr_generate_key")
            .count();
        assert_eq!(
            count, 1,
            "nostr_generate_key must be defined exactly once in own_definitions"
        );
    }

    /// Regression guard for #161: cancel_subtask must be an *own* (server-neutral)
    /// definition so web / Nostr / REST — not just Discord — expose the tool to
    /// stop auto-dispatched subtasks. If it is removed from own_definitions the
    /// tool disappears on every non-Discord transport again — that is the bug this
    /// guards against.
    #[test]
    fn cancel_subtask_is_exposed_in_own_definitions() {
        let defs = SystemGatewayActions::own_definitions();
        let d = defs
            .iter()
            .find(|d| d.name == "cancel_subtask")
            .expect("cancel_subtask must be an own (server-neutral) definition (#161)");
        // subtask_id は必須。
        let required = d.parameters["required"].as_array().unwrap();
        assert!(
            required.iter().any(|v| v == "subtask_id"),
            "cancel_subtask must require subtask_id"
        );
        // own に丁度1件（dedup の source が一意）。
        let count = defs.iter().filter(|d| d.name == "cancel_subtask").count();
        assert_eq!(
            count, 1,
            "cancel_subtask must be defined exactly once in own_definitions"
        );
    }

    /// **fail-closed な dispatch 分類ガード（#152）**。
    ///
    /// `own_definitions()` の全名が「非ブロック dispatch の除外集合（inline）」か
    /// 「意図的な dispatch 可リスト」のどちらか**ちょうど一方**に属することを要求する。
    ///
    /// この gateway は transport 非依存で web / REST / heartbeat の全ターンに載る
    /// （`crates/server/src/process.rs` の合成 executor）のに、Discord / Nostr / core と
    /// 違って分類ガードが無く、6 個中 5 個（`configure_llm_provider` /
    /// `manage_allowed_commands` / `configure_nostr` / `configure_self` /
    /// `configure_mcp_server`）が黙って background 化されていた。実装
    /// （`own_definitions()`）を起点に走査するので、新しい設定ツールを足すと分類を
    /// 明示するまでテストが落ちる。判定基準は
    /// `opencrab_actions::default_non_dispatch_tools` の doc。
    #[test]
    fn server_tools_are_classified_for_dispatch() {
        let names: Vec<String> = SystemGatewayActions::own_definitions()
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert!(!names.is_empty(), "own_definitions が空");
        let non_dispatch = opencrab_actions::default_non_dispatch_tools();

        for name in &names {
            let inline = non_dispatch.contains(name);
            let dispatchable =
                opencrab_actions::SERVER_DISPATCHABLE_ACTIONS.contains(&name.as_str());
            assert!(
                inline ^ dispatchable,
                "{name} の dispatch 分類が未定義（inline={inline}, dispatchable={dispatchable}）。\
                 新しいツールを追加したら opencrab_actions::SERVER_INLINE_ACTIONS か \
                 SERVER_DISPATCHABLE_ACTIONS のどちらかへ入れること（判定基準は \
                 default_non_dispatch_tools の doc / docs/DESIGN.md §4.4）"
            );
        }

        // 逆方向: 定数側に死名が無いこと。
        for name in opencrab_actions::SERVER_INLINE_ACTIONS {
            assert!(
                names.contains(&(*name).to_string()),
                "SERVER_INLINE_ACTIONS の {name} が own_definitions() に無い（死名）"
            );
        }
        for name in opencrab_actions::SERVER_DISPATCHABLE_ACTIONS {
            assert!(
                names.contains(&(*name).to_string()),
                "SERVER_DISPATCHABLE_ACTIONS の {name} が own_definitions() に無い（死名）"
            );
        }
        // 分類は own_definitions() を覆い尽くす。
        assert_eq!(
            opencrab_actions::SERVER_INLINE_ACTIONS.len()
                + opencrab_actions::SERVER_DISPATCHABLE_ACTIONS.len(),
            names.len(),
            "分類集合の合計が own_definitions() の数と一致しない（分類漏れ）"
        );
    }

    /// [P1 回帰] 設定変更ツールは inline（同ターンで結果を返す）。長時間の鍵探索だけが
    /// background。分類定数を経由せず `default_non_dispatch_tools()` の実効値を見る。
    #[test]
    fn config_tools_are_inline_and_key_generation_is_dispatched() {
        let non_dispatch = opencrab_actions::default_non_dispatch_tools();
        for name in [
            "configure_llm_provider",
            "manage_allowed_commands",
            "configure_nostr",
            "configure_self",
            "configure_mcp_server",
            "cancel_subtask",
            // #157 S1 で Discord から移設。分類の所属（inline）は移設前と同じ。
            // 純粋な読み取り（一覧の即答）+ 同ターン結果依存（許可した直後に
            // execute_shell を使う）。Discord 側にあった同趣旨の固定の引き継ぎ。
            "list_allowed_commands",
            "add_allowed_command",
            "remove_allowed_command",
        ] {
            assert!(
                non_dispatch.contains(name),
                "{name} は background 化してはならない（設定の共有状態書き込み / 一覧の即答）"
            );
        }
        assert!(
            !non_dispatch.contains("nostr_generate_key"),
            "nostr_generate_key は長時間の vanity 探索なので dispatch 対象に残す"
        );
        // #157 S1 で Discord から移設。dispatchable の所属も移設前と同じ
        // （設定の書き込みで同ターンに読み戻さない）。
        assert!(
            !non_dispatch.contains("update_memory_index_config"),
            "update_memory_index_config は移設前と同じく dispatch 対象に残す"
        );
        // #157 S3 で Discord から移設。分類の所属も移設前と同じ
        // （読み出し = inline / 書き込み = dispatchable）。
        assert!(
            non_dispatch.contains("read_heartbeat_instructions"),
            "read_heartbeat_instructions は移設前と同じく inline（一覧の即答）"
        );
        assert!(
            !non_dispatch.contains("update_heartbeat_instructions"),
            "update_heartbeat_instructions は移設前と同じく dispatch 対象に残す"
        );
    }

    /// #161: Discord のような inner が cancel_subtask を定義しても、merge 後は
    /// own の1件だけが残る（providers は同名重複を拒否しうる）。merge_definitions を
    /// 直接叩くことで AppState 無しに実コードの dedup 契約を検証する。
    #[test]
    fn merge_definitions_dedups_cancel_subtask_from_inner() {
        use opencrab_gateway::{GatewayActionResult, GatewayCallContext};

        /// cancel_subtask と固有ツールを定義する Discord 風 inner モック。
        struct InnerWithCancel;
        #[async_trait]
        impl GatewayActions for InnerWithCancel {
            fn definitions(&self) -> Vec<GatewayActionDef> {
                vec![
                    GatewayActionDef {
                        name: "cancel_subtask".to_string(),
                        description: "discord cancel".to_string(),
                        parameters: json!({"type": "object"}),
                    },
                    GatewayActionDef {
                        name: "discord_only_tool".to_string(),
                        description: "x".to_string(),
                        parameters: json!({"type": "object"}),
                    },
                ]
            }
            async fn execute(
                &self,
                _name: &str,
                _args: &Value,
                _ctx: &GatewayCallContext,
            ) -> GatewayActionResult {
                GatewayActionResult {
                    success: true,
                    data: None,
                    error: None,
                }
            }
        }

        let inner: Arc<dyn GatewayActions> = Arc::new(InnerWithCancel);
        let merged = SystemGatewayActions::merge_definitions(
            SystemGatewayActions::own_definitions(),
            Some(&inner),
        );
        let cancel_count = merged.iter().filter(|d| d.name == "cancel_subtask").count();
        assert_eq!(
            cancel_count, 1,
            "merge 後も cancel_subtask は1件（own 優先で dedup）"
        );
        // inner 固有ツールは通す（dedup は同名のみ）。
        assert!(merged.iter().any(|d| d.name == "discord_only_tool"));
    }

    // ---- #175 S1: report_progress の gateway 非依存化 ----

    /// Regression guard for #175 S1: report_progress must be an *own* (server-neutral)
    /// definition so web / Nostr / REST / heartbeat — not just Discord — can let a
    /// sub-engine report progress.
    #[test]
    fn report_progress_is_exposed_in_own_definitions() {
        let defs = SystemGatewayActions::own_definitions();
        let d = defs
            .iter()
            .find(|d| d.name == "report_progress")
            .expect("report_progress must be an own (server-neutral) definition (#175 S1)");
        // message は必須 / subtask_id は任意（sub-engine の system prompt の契約）。
        let required = d.parameters["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "message"));
        assert!(!required.iter().any(|v| v == "subtask_id"));
        let props = d.parameters["properties"].as_object().unwrap();
        assert!(props.contains_key("subtask_id"));
        // own に丁度 1 件（dedup の source が一意）。
        let count = defs.iter().filter(|d| d.name == "report_progress").count();
        assert_eq!(count, 1);
    }

    // ---- #175 S4: spawn_subtask の gateway 非依存化 ----

    /// Regression guard for #175 S4: `spawn_subtask` は *own*（server-neutral）定義。
    /// これが own から消えると、web / REST / Nostr / heartbeat でサブタスクを起動できなく
    /// なり、Discord だけの機能に逆戻りする。
    #[test]
    fn spawn_subtask_is_exposed_in_own_definitions() {
        let defs = SystemGatewayActions::own_definitions();
        let d = defs
            .iter()
            .find(|d| d.name == "spawn_subtask")
            .expect("spawn_subtask must be an own (server-neutral) definition (#175 S4)");
        let required = d.parameters["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "task"));
        let props = d.parameters["properties"].as_object().unwrap();
        for key in ["task", "timeout_secs", "label", "webhook"] {
            assert!(props.contains_key(key), "missing property: {key}");
        }
        assert_eq!(defs.iter().filter(|d| d.name == "spawn_subtask").count(), 1);
    }

    /// Regression guard for #175 S4 / #155: `rebuild_memory_index` も own 定義
    /// （LLM クライアントを要する唯一の Discord ツールだった）。
    #[test]
    fn rebuild_memory_index_is_exposed_in_own_definitions() {
        let defs = SystemGatewayActions::own_definitions();
        assert!(
            defs.iter().any(|d| d.name == "rebuild_memory_index"),
            "rebuild_memory_index must be an own definition (#175 S4)"
        );
    }

    /// **サブタスクのネスト禁止**（壊すと重大）。
    ///
    /// sub-engine の実効ゲートは bridge の MAX_DEPTH ではなく
    /// `SUB_ENGINE_ALLOWED_ACTIONS` の許可リスト。`spawn_subtask` が server-neutral 層へ
    /// 移った今、許可リストへうっかり足すとサブタスクが無限にネストできてしまう。
    /// 合成 gateway（own + inner）を許可リストで包んだ結果を直接固定する。
    #[test]
    fn sub_engine_cannot_see_spawn_subtask() {
        let state = crate::test_app_state();
        let composite: Arc<dyn GatewayActions> =
            Arc::new(SystemGatewayActions::new(state, None, None, None));
        let sub = opencrab_actions::SubEngineGatewayActions::new(composite);
        let names: Vec<String> = sub.definitions().into_iter().map(|d| d.name).collect();
        assert!(
            !names.contains(&"spawn_subtask".to_string()),
            "sub-engine から spawn_subtask が見えてはならない（ネスト禁止）: {names:?}"
        );
        // 許可された制御ツールは見える（許可リストが空振りしていないことの対）。
        assert!(names.contains(&"report_progress".to_string()));
        assert!(names.contains(&"nostr_generate_key".to_string()));
    }

    /// sub-engine から `spawn_subtask` を名前指定で呼んでも拒否される
    /// （定義から隠すだけでは、親コンテキストの記憶で名前を呼ばれると素通しになる）。
    #[tokio::test]
    async fn sub_engine_execution_of_spawn_subtask_is_rejected() {
        let state = crate::test_app_state();
        let composite: Arc<dyn GatewayActions> =
            Arc::new(SystemGatewayActions::new(state, None, None, None));
        let sub = opencrab_actions::SubEngineGatewayActions::new(composite);
        let r = sub
            .execute(
                "spawn_subtask",
                &json!({ "task": "nested" }),
                &sub_ctx("subtask-st-1"),
            )
            .await;
        assert!(!r.success);
        assert!(
            r.error.unwrap().starts_with(REJECTION_CODE_PREFIX),
            "許可外の実在ツールは権限拒否として返す"
        );
    }

    /// `report_progress` は随伴マップの通知口へ進捗を渡す（#175 S4 で Discord 実装から
    /// 引き継いだ配線）。落とすと lifecycle webhook から進捗が黙って消える。
    #[tokio::test]
    async fn report_progress_notifies_the_run_notifier() {
        #[derive(Default)]
        struct Recorder(std::sync::Mutex<Vec<String>>);
        impl opencrab_actions::subtask_notify::SubtaskRunNotifier for Recorder {
            fn on_progress(&self, detail: &str) {
                self.0.lock().unwrap().push(detail.to_string());
            }
        }

        let state = crate::test_app_state();
        let recorder = Arc::new(Recorder::default());
        state
            .subtask_notifiers
            .insert("st-1".to_string(), recorder.clone());
        let registry = registry_with("st-1", "subtask-st-1", "web-parent-1");
        let actions = SystemGatewayActions::new(state.clone(), None, Some(registry), None);

        let r = actions
            .execute(
                "report_progress",
                &json!({ "message": "halfway there" }),
                &sub_ctx("subtask-st-1"),
            )
            .await;
        assert!(r.success, "{:?}", r.error);
        assert_eq!(recorder.0.lock().unwrap().clone(), vec!["halfway there"]);
    }

    /// Discord のような inner が report_progress を定義しても、merge 後は own の
    /// 1 件だけが残る（provider は同名重複を拒否しうる）。
    #[test]
    fn merge_definitions_dedups_report_progress_from_inner() {
        let inner: Arc<dyn GatewayActions> = Arc::new(RecordingInner::new(&["report_progress"]));
        let merged = SystemGatewayActions::merge_definitions(
            SystemGatewayActions::own_definitions(),
            Some(&inner),
        );
        let count = merged
            .iter()
            .filter(|d| d.name == "report_progress")
            .count();
        assert_eq!(
            count, 1,
            "merge 後も report_progress は1件（own 優先で dedup）"
        );
    }

    /// 指定した名前のツールを定義し、`execute` の到達を記録する inner のフェイク。
    struct RecordingInner {
        names: Vec<String>,
        calls: std::sync::Mutex<Vec<String>>,
    }

    impl RecordingInner {
        fn new(names: &[&str]) -> Self {
            Self {
                names: names.iter().map(|s| s.to_string()).collect(),
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl GatewayActions for RecordingInner {
        fn definitions(&self) -> Vec<GatewayActionDef> {
            self.names
                .iter()
                .map(|n| GatewayActionDef {
                    name: n.clone(),
                    description: format!("{n} (inner)"),
                    parameters: json!({"type": "object"}),
                })
                .collect()
        }
        async fn execute(
            &self,
            name: &str,
            _args: &Value,
            _ctx: &GatewayCallContext,
        ) -> GatewayActionResult {
            self.calls.lock().unwrap().push(name.to_string());
            GatewayActionResult {
                success: true,
                data: Some(json!({ "reached_inner": name })),
                error: None,
            }
        }
    }

    /// 受け取った settle を記録する `SubtaskCompletionSink`。
    #[derive(Default)]
    struct RecordingSink {
        settled: std::sync::Mutex<Vec<SubtaskSettled>>,
    }

    impl RecordingSink {
        fn settled(&self) -> Vec<SubtaskSettled> {
            self.settled.lock().unwrap().clone()
        }
    }

    impl SubtaskCompletionSink for RecordingSink {
        fn on_subtask_settled(&self, ev: SubtaskSettled) {
            self.settled.lock().unwrap().push(ev);
        }
    }

    /// 走行中扱いの subtask を 1 件だけ持つ registry。
    fn registry_with(
        subtask_id: &str,
        session_id: &str,
        parent_session_id: &str,
    ) -> SubtaskRegistry {
        let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());
        registry.insert(
            subtask_id.to_string(),
            opencrab_actions::SpawnedSubtask {
                abort_handle: tokio::spawn(std::future::pending::<()>()).abort_handle(),
                session_id: session_id.to_string(),
                parent_session_id: parent_session_id.to_string(),
                agent_id: "agent-x".to_string(),
                label: "job".to_string(),
                tool_name: "spawn_subtask".to_string(),
                started_at: std::time::Instant::now(),
                reply_target: None,
                lifecycle: opencrab_actions::SubtaskLifecycle::new(),
            },
        );
        registry
    }

    fn sub_ctx(session_id: &str) -> GatewayCallContext {
        GatewayCallContext::new(GatewayCaller::Agent, "agent-x")
            .with_session_id(session_id)
            .with_depth(1)
    }

    /// 親セッションログに記録された subtask_progress のメッセージ一覧。
    fn progress_messages(state: &AppState, parent_session_id: &str) -> Vec<String> {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::list_session_logs_by_session(&conn, parent_session_id)
            .unwrap()
            .into_iter()
            .filter_map(|row| {
                let v: Value = serde_json::from_str(&row.content).ok()?;
                if v.get("type").and_then(|t| t.as_str()) != Some("subtask_progress") {
                    return None;
                }
                Some(v.get("message")?.as_str()?.to_string())
            })
            .collect()
    }

    /// **非 Discord（inner なし）で report_progress が動く**（#175 S1 の主目的）。
    /// 親ログに本文が残り、デバウンス後に完了受け口へ `Progress` が届く。
    #[tokio::test(start_paused = true)]
    async fn report_progress_works_without_inner_gateway() {
        let state = crate::test_app_state();
        let registry = registry_with("st-1", "subtask-st-1", "web-parent-1");
        let sink = Arc::new(RecordingSink::default());
        let actions = SystemGatewayActions::new(
            state.clone(),
            None,
            Some(registry),
            Some(sink.clone() as Arc<dyn SubtaskCompletionSink>),
        );

        let r = actions
            .execute(
                "report_progress",
                &json!({ "message": "halfway there" }),
                &sub_ctx("subtask-st-1"),
            )
            .await;
        assert!(r.success, "error: {:?}", r.error);
        assert_eq!(r.data.as_ref().unwrap()["notified"], json!(true));

        // 本文は親セッションログへ永続化される（sink には運ばない / RFC §1.3）。
        assert_eq!(
            progress_messages(&state, "web-parent-1"),
            vec!["halfway there".to_string()]
        );

        // デバウンス満了後に Progress が 1 本届く。
        tokio::time::sleep(PROGRESS_DEBOUNCE_DELAY + Duration::from_secs(1)).await;
        let settled = sink.settled();
        assert_eq!(settled.len(), 1, "デバウンス後に Progress が 1 本届く");
        assert_eq!(settled[0].kind, SettleKind::Progress);
        assert_eq!(settled[0].session_id, "web-parent-1");
        assert_eq!(settled[0].subtask_id, "st-1");
        assert_eq!(settled[0].exit_reason, "progress");
    }

    /// **Discord（inner あり）では inner へ委譲される**（S1 で Discord 経路は挙動不変）。
    /// own 実装は走らない＝親ログを書かない。
    #[tokio::test]
    async fn report_progress_delegates_to_inner_when_inner_defines_it() {
        let state = crate::test_app_state();
        let inner = Arc::new(RecordingInner::new(&["report_progress", "spawn_subtask"]));
        let registry = registry_with("st-1", "subtask-st-1", "discord-parent-1");
        let sink = Arc::new(RecordingSink::default());
        let actions = SystemGatewayActions::new(
            state.clone(),
            Some(inner.clone() as Arc<dyn GatewayActions>),
            Some(registry),
            Some(sink.clone() as Arc<dyn SubtaskCompletionSink>),
        );

        let r = actions
            .execute(
                "report_progress",
                &json!({ "message": "from discord" }),
                &sub_ctx("subtask-st-1"),
            )
            .await;
        assert!(r.success);
        assert_eq!(
            r.data.unwrap()["reached_inner"],
            json!("report_progress"),
            "inner（Discord 実装）へ委譲されなければならない"
        );
        assert_eq!(inner.calls(), vec!["report_progress".to_string()]);
        // own 実装は走っていない（親ログも sink も触っていない）。
        assert!(progress_messages(&state, "discord-parent-1").is_empty());
        assert!(sink.settled().is_empty());
    }

    /// 所有権ゲート: 他人の subtask（自分の session でも親でもない）は拒否する。
    #[tokio::test]
    async fn report_progress_rejects_foreign_subtask() {
        let state = crate::test_app_state();
        let registry = registry_with("st-1", "subtask-st-1", "parent-of-someone-else");
        let sink = Arc::new(RecordingSink::default());
        let actions = SystemGatewayActions::new(
            state.clone(),
            None,
            Some(registry),
            Some(sink as Arc<dyn SubtaskCompletionSink>),
        );

        let r = actions
            .execute(
                "report_progress",
                &json!({ "message": "sneaky", "subtask_id": "st-1" }),
                &sub_ctx("some-other-session"),
            )
            .await;
        assert!(!r.success);
        let e = r.error.unwrap();
        assert!(
            e.starts_with(REJECTION_CODE_PREFIX),
            "権限拒否は構造的マーカー付き: {e}"
        );
        // 他セッションの親ログを汚さない。
        assert!(progress_messages(&state, "parent-of-someone-else").is_empty());
    }

    /// 親セッションからの代理報告は許す（所有権ゲートの片方の分岐）。
    ///
    /// 所有権ゲートは「自分の subtask」か「自分が親である subtask」のどちらかなら通す。
    /// 親側の分岐を落としても他のテストは全て通ってしまう（変異実験で確認済み）ため、
    /// ここで固定する。Discord 側にも同趣旨のテストがある。
    #[tokio::test]
    async fn report_progress_allows_parent_reporting_child() {
        let state = crate::test_app_state();
        let registry = registry_with("st-1", "subtask-st-1", "parent-session");
        let sink = Arc::new(RecordingSink::default());
        let actions = SystemGatewayActions::new(
            state.clone(),
            None,
            Some(registry),
            Some(sink.clone() as Arc<dyn SubtaskCompletionSink>),
        );

        // 呼び出し元は subtask 本人ではなく「親セッション」。
        let r = actions
            .execute(
                "report_progress",
                &json!({ "message": "親からの代理報告", "subtask_id": "st-1" }),
                &sub_ctx("parent-session"),
            )
            .await;
        assert!(
            r.success,
            "親セッションからの代理報告は許される: {:?}",
            r.error
        );
        assert!(
            progress_messages(&state, "parent-session")
                .iter()
                .any(|m| m.contains("親からの代理報告")),
            "親セッションのログへ記録される"
        );
    }

    /// セッション必須ガード（fail-closed）: session_id が無い文脈では実行できない。
    #[tokio::test]
    async fn report_progress_requires_session_context() {
        let state = crate::test_app_state();
        let actions = SystemGatewayActions::new(state, None, None, None);
        let ctx = GatewayCallContext::new(GatewayCaller::Agent, "agent-x");
        let r = actions
            .execute("report_progress", &json!({ "message": "x" }), &ctx)
            .await;
        assert!(!r.success);
        assert!(r.error.unwrap().contains("session_id"));
    }

    /// message は必須。
    #[tokio::test]
    async fn report_progress_requires_message() {
        let state = crate::test_app_state();
        let actions = SystemGatewayActions::new(state, None, None, None);
        let r = actions
            .execute("report_progress", &json!({}), &sub_ctx("subtask-st-1"))
            .await;
        assert!(!r.success);
        assert!(r.error.unwrap().contains("'message' is required"));
    }

    /// 完了受け口が未配線なら、記録だけして通知はしない（デバウンスタスクも起動しない）。
    /// 「黙って消える」のを避けるため、結果に `notified: false` を載せる。
    #[tokio::test(start_paused = true)]
    async fn report_progress_records_but_does_not_notify_without_sink() {
        let state = crate::test_app_state();
        let registry = registry_with("st-1", "subtask-st-1", "rest-parent-1");
        let actions = SystemGatewayActions::new(state.clone(), None, Some(registry), None);

        let r = actions
            .execute(
                "report_progress",
                &json!({ "message": "no sink here" }),
                &sub_ctx("subtask-st-1"),
            )
            .await;
        assert!(r.success);
        assert_eq!(r.data.unwrap()["notified"], json!(false));
        // 記録は残る。
        assert_eq!(
            progress_messages(&state, "rest-parent-1"),
            vec!["no sink here".to_string()]
        );
        // デバウンスタスクを起動していない＝世代カウンタも進んでいない。
        tokio::time::sleep(PROGRESS_DEBOUNCE_DELAY + Duration::from_secs(1)).await;
        assert!(
            !state.progress_debounce.claim_latest("rest-parent-1", 1),
            "受け口未配線ではデバウンス世代を消費しない"
        );
    }

    /// **デバウンス状態が `AppState` 側にあることを固定する回帰テスト（#175 S1 の最重要点）**。
    ///
    /// `SystemGatewayActions` は run ごとに作り直される。デバウンス世代カウンタを
    /// この構造体のフィールドに置くと、2 回目の呼び出しで世代が 0 から張り直され、
    /// **両方の呼び出しが発火する**（＝バーストで LLM を無駄に呼ぶ）。ここでは
    /// 別インスタンスから 2 回報告し、届く `Progress` が 1 本だけであることを固定する。
    #[tokio::test(start_paused = true)]
    async fn progress_debounce_survives_gateway_recreation() {
        let state = crate::test_app_state();
        let registry = registry_with("st-1", "subtask-st-1", "web-parent-1");
        let sink = Arc::new(RecordingSink::default());

        // 1 回目: この run 用の gateway インスタンス。
        let first = SystemGatewayActions::new(
            state.clone(),
            None,
            Some(registry.clone()),
            Some(sink.clone() as Arc<dyn SubtaskCompletionSink>),
        );
        assert!(
            first
                .execute(
                    "report_progress",
                    &json!({ "message": "step 1" }),
                    &sub_ctx("subtask-st-1")
                )
                .await
                .success
        );
        drop(first);

        // 2 回目: 別の run（＝別インスタンス）。同じ AppState を共有する。
        let second = SystemGatewayActions::new(
            state.clone(),
            None,
            Some(registry),
            Some(sink.clone() as Arc<dyn SubtaskCompletionSink>),
        );
        assert!(
            second
                .execute(
                    "report_progress",
                    &json!({ "message": "step 2" }),
                    &sub_ctx("subtask-st-1")
                )
                .await
                .success
        );

        tokio::time::sleep(PROGRESS_DEBOUNCE_DELAY + Duration::from_secs(1)).await;

        // 本文は 2 件とも親ログへ残る（間引くのは通知だけ）。
        assert_eq!(
            progress_messages(&state, "web-parent-1"),
            vec!["step 1".to_string(), "step 2".to_string()]
        );
        // 通知は最後の 1 本だけ。デバウンス状態をインスタンスのフィールドに移すと 2 本届く。
        let settled = sink.settled();
        assert_eq!(
            settled.len(),
            1,
            "デバウンスは gateway の作り直しを跨いで効かなければならない（AppState 側に置く）。届いた: {settled:?}"
        );
        assert_eq!(settled[0].kind, SettleKind::Progress);
    }

    // ---- #157 S1: 汎用管理ツール 4 個の gateway 非依存化 ----
    //
    // 移設前（origin/main）にはこの 4 ツールの挙動テストが**1 件も無かった**ため、
    // ここは「移植」ではなく新規に契約を覆うテスト群である。守っている不変条件は
    // `crate::agent_management` のモジュール doc に列挙してある。

    /// 実行許可設定に shell セクションを持たせた `AppState`。
    ///
    /// `initial` は**設定ファイル由来**の許可コマンド（グローバル設定）を模す。
    /// per-agent の許可（DB）と混ざらないこと（#202）を検証するには、この 2 つが
    /// 区別できる構成が必要。
    fn state_with_shell(initial: &[&str]) -> AppState {
        let state = crate::test_app_state();
        {
            let mut cfg = state.tools_config.write().unwrap();
            cfg.enabled = true;
            cfg.shell = Some(opencrab_actions::tools::ShellToolConfig {
                enabled: true,
                allowed_commands: initial.iter().map(|s| s.to_string()).collect(),
                timeout_secs: 30,
                max_timeout_secs: 300,
                working_dir: None,
                inherit_env: false,
                allowed_env_vars: Vec::new(),
                max_output_bytes: 1024,
                commands: Vec::new(),
            });
        }
        state
    }

    /// 走行中の実行許可設定（`AppState.tools_config`）に載っているコマンド一覧。
    fn live_allowed_commands(state: &AppState) -> Vec<String> {
        state
            .tools_config
            .read()
            .unwrap()
            .shell
            .as_ref()
            .map(|s| s.allowed_commands.clone())
            .unwrap_or_default()
    }

    /// DB に永続化されている許可コマンド一覧。
    fn db_allowed_commands(state: &AppState, agent_id: &str) -> Vec<String> {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::list_agent_allowed_commands(&conn, agent_id).unwrap()
    }

    /// **次の run** がそのエージェントに許可するコマンド一覧。
    ///
    /// 応答生成（`crate::process`）が毎 run 呼ぶ解決点をそのまま使う。グローバル設定と
    /// 混同しないよう、per-agent の実効値はこのヘルパー越しにだけ見る。
    fn run_allowed_commands(state: &AppState, agent_id: &str) -> Vec<String> {
        crate::process::resolve_run_tools_config(state, agent_id)
            .shell
            .map(|s| s.allowed_commands)
            .unwrap_or_default()
    }

    /// シェルツールを実際に dispatch するための `ActionContext`（作業ディレクトリ付き）。
    fn shell_ctx() -> (tempfile::TempDir, opencrab_actions::ActionContext) {
        let dir = tempfile::TempDir::new().unwrap();
        let ws = opencrab_core::workspace::Workspace::from_root(dir.path()).unwrap();
        let conn = opencrab_db::init_memory().unwrap();
        let ctx = opencrab_actions::ActionContext {
            caller: opencrab_actions::CallerIdentity::Owner,
            agent_id: "agent-x".to_string(),
            agent_name: "Agent X".to_string(),
            session_id: None,
            db: opencrab_db::Db::from_connection(conn),
            workspace: Arc::new(ws),
            last_metrics_id: Arc::new(std::sync::Mutex::new(None)),
            model_override: Arc::new(std::sync::Mutex::new(None)),
            current_purpose: Arc::new(std::sync::Mutex::new("test".to_string())),
            runtime_info: Arc::new(std::sync::Mutex::new(opencrab_actions::RuntimeInfo {
                default_model: "mock:test".to_string(),
                active_model: None,
                available_providers: vec![],
                gateway: "test".to_string(),
            })),
        };
        (dir, ctx)
    }

    fn owner_ctx() -> GatewayCallContext {
        GatewayCallContext::new(GatewayCaller::Owner, "agent-x")
    }

    fn agent_ctx() -> GatewayCallContext {
        GatewayCallContext::new(GatewayCaller::Agent, "agent-x")
    }

    // ---- #157 S5: 通知先（webhook）の管理ツール ----

    /// 移設した 6 ツールの名前（#157 S5）。`ensure_*` は含まない（Discord 側に残る）。
    const MOVED_WEBHOOK_TOOLS: &[&str] = &[
        "get_default_subtask_webhook",
        "set_default_subtask_webhook",
        "list_subtask_webhooks",
        "get_default_webhook",
        "set_default_webhook",
        "list_webhooks",
    ];

    /// **#157 S5 の本題**: 6 ツールが own 定義にちょうど 1 件ずつある。
    #[test]
    fn webhook_target_tools_are_exposed_in_own_definitions() {
        let defs = SystemGatewayActions::own_definitions();
        for name in MOVED_WEBHOOK_TOOLS {
            assert_eq!(
                defs.iter().filter(|d| &d.name == name).count(),
                1,
                "{name} は own 定義にちょうど 1 件必要（#157 S5）"
            );
        }
    }

    /// **Discord 無効の構成でも 6 ツールが露出する**（#157 S5 の証明）。
    ///
    /// `inner = None` は「transport 固有 gateway が居ない」経路（web / REST / Nostr /
    /// heartbeat、および Discord feature 無効ビルド）そのもの。移設前はこの構成で
    /// 6 ツールが一切出なかった＝ #157 が報告している不具合そのもの。
    #[test]
    fn webhook_target_tools_are_exposed_without_any_transport_gateway() {
        let state = crate::test_app_state();
        let actions = SystemGatewayActions::new(state, None, None, None);
        let names: Vec<String> = actions.definitions().into_iter().map(|d| d.name).collect();
        for name in MOVED_WEBHOOK_TOOLS {
            assert!(
                names.contains(&name.to_string()),
                "transport gateway 無しの構成で {name} が露出しない（#157 の不具合そのもの）: {names:?}"
            );
        }
        // 逆に、Discord に残した `ensure_*` はここには出ない（inner が居ないため）。
        for name in ["ensure_webhook", "ensure_subtask_webhook"] {
            assert!(
                !names.contains(&name.to_string()),
                "{name} は Discord gateway 由来のはず（own に増やしてはいけない）"
            );
        }
    }

    /// 引数スキーマを移設前（Discord 定義）と同一に保つ。
    ///
    /// 名前・`required`・プロパティ名の集合をリテラルで固定する。ここが変わると
    /// 既存の会話ログにあるツール呼び出しが通らなくなる。
    #[test]
    fn webhook_target_tool_schemas_match_the_discord_originals() {
        let defs = SystemGatewayActions::own_definitions();
        let find = |n: &str| defs.iter().find(|d| d.name == n).unwrap();
        let props = |n: &str| {
            let mut keys: Vec<String> = find(n).parameters["properties"]
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect();
            keys.sort();
            keys
        };

        assert_eq!(
            find("get_default_subtask_webhook").parameters["required"],
            json!([])
        );
        assert_eq!(
            props("get_default_subtask_webhook"),
            vec!["agent_id", "scope", "tool_name"]
        );

        assert_eq!(
            find("set_default_subtask_webhook").parameters["required"],
            json!(["scope"])
        );
        assert_eq!(
            props("set_default_subtask_webhook"),
            vec![
                "agent_id",
                "enabled",
                "events",
                "kind",
                "max_chars",
                "output_mode",
                "scope",
                "tool_name",
                "url",
            ]
        );

        assert_eq!(
            find("list_subtask_webhooks").parameters["required"],
            json!([])
        );
        assert_eq!(
            props("list_subtask_webhooks"),
            vec!["agent_id", "include_disabled", "scope"]
        );

        assert_eq!(
            find("get_default_webhook").parameters["required"],
            json!([])
        );
        assert_eq!(
            props("get_default_webhook"),
            vec!["agent_id", "family", "tool_name"]
        );

        assert_eq!(
            find("set_default_webhook").parameters["required"],
            json!(["scope"])
        );
        assert_eq!(
            props("set_default_webhook"),
            vec![
                "agent_id",
                "enabled",
                "events",
                "family",
                "max_chars",
                "output_mode",
                "scope",
                "tool_name",
                "url",
            ]
        );

        assert_eq!(find("list_webhooks").parameters["required"], json!([]));
        assert_eq!(
            props("list_webhooks"),
            vec!["agent_id", "family", "include_disabled", "scope"]
        );
    }

    /// **6 ツールは inner へ委譲されない**（own が唯一の実装）。
    ///
    /// 委譲パターンのまま残すと、Discord が誤って再定義したときに own の実装が黙って
    /// バイパスされる（#155 の後退）。`ensure_*` は逆に inner へ渡る必要がある。
    #[tokio::test]
    async fn webhook_target_tools_are_not_delegated_to_inner() {
        let state = crate::test_app_state();
        let inner = Arc::new(RecordingInner::new(&[
            "get_default_subtask_webhook",
            "set_default_subtask_webhook",
            "list_subtask_webhooks",
            "get_default_webhook",
            "set_default_webhook",
            "list_webhooks",
            "ensure_webhook",
        ]));
        let actions = SystemGatewayActions::new(state, Some(inner.clone()), None, None);

        for name in MOVED_WEBHOOK_TOOLS {
            let _ = actions
                .execute(name, &json!({"scope": "agent"}), &owner_ctx())
                .await;
        }
        assert!(
            inner.calls().is_empty(),
            "移設した 6 ツールが inner へ委譲された: {:?}",
            inner.calls()
        );

        // Discord に残した `ensure_webhook` は既定アームで inner へ委譲される。
        let _ = actions
            .execute("ensure_webhook", &json!({}), &owner_ctx())
            .await;
        assert_eq!(inner.calls(), vec!["ensure_webhook".to_string()]);
    }

    /// **#157 S1 の本題**: 4 ツールが `SystemGatewayActions` の own 定義になっている。
    ///
    /// own 定義は transport の有無に依存しないため、これが `definitions()` に出ることは
    /// 「web / Nostr / REST / heartbeat でも使える」ことと同義である。own から消えると
    /// Discord 専用に逆戻りする（それが #157 が報告している不具合そのもの）。
    #[test]
    fn generic_management_tools_are_exposed_in_own_definitions() {
        let defs = SystemGatewayActions::own_definitions();
        for name in [
            "update_memory_index_config",
            "add_allowed_command",
            "list_allowed_commands",
            "remove_allowed_command",
        ] {
            assert_eq!(
                defs.iter().filter(|d| d.name == name).count(),
                1,
                "{name} は own 定義にちょうど 1 件必要（#157 S1）"
            );
        }
    }

    /// **Discord 無効の構成でも 4 ツールが露出する**（#157 S1 の証明）。
    ///
    /// `inner = None` は「transport 固有 gateway が居ない」経路（web / REST /
    /// heartbeat、および Discord feature 無効ビルド）そのもの。移設前はこの構成で
    /// 4 ツールが一切出なかった。
    #[test]
    fn generic_management_tools_are_exposed_without_any_transport_gateway() {
        let state = crate::test_app_state();
        let actions = SystemGatewayActions::new(state, None, None, None);
        let names: Vec<String> = actions.definitions().into_iter().map(|d| d.name).collect();
        for name in [
            "update_memory_index_config",
            "add_allowed_command",
            "list_allowed_commands",
            "remove_allowed_command",
        ] {
            assert!(
                names.contains(&name.to_string()),
                "transport gateway 無しの構成で {name} が露出しない（#157 の不具合そのもの）: {names:?}"
            );
        }
    }

    /// 引数スキーマを移設前（Discord 定義）と同一に保つ。
    #[test]
    fn generic_management_tool_schemas_match_the_discord_originals() {
        let defs = SystemGatewayActions::own_definitions();
        let find = |n: &str| defs.iter().find(|d| d.name == n).unwrap();

        let d = find("update_memory_index_config");
        assert!(d.parameters["required"].as_array().unwrap().is_empty());
        let props = d.parameters["properties"].as_object().unwrap();
        assert_eq!(props["batch_size"]["type"], json!("integer"));
        assert_eq!(props["threshold"]["type"], json!("integer"));

        for n in ["add_allowed_command", "remove_allowed_command"] {
            let d = find(n);
            assert_eq!(d.parameters["required"], json!(["command"]), "{n}");
            assert_eq!(
                d.parameters["properties"]["command"]["type"],
                json!("string"),
                "{n}"
            );
        }

        let d = find("list_allowed_commands");
        assert!(d.parameters["required"].as_array().unwrap().is_empty());
        assert!(d.parameters["properties"].as_object().unwrap().is_empty());
    }

    /// **オーナー限定検査が移設後も効く**（add）。
    ///
    /// bridge の `OWNER_ONLY_ACTIONS` は `add_allowed_command` /
    /// `remove_allowed_command` を**持っていない**（持っているのは新系統の
    /// `manage_allowed_commands` だけ）。つまりこのハンドラ内検査が唯一のゲートで、
    /// 落とすと非オーナーがシェル実行範囲を広げられる。
    ///
    /// エラー文言はバイト単位で移設前と同一（移設で文言が変わっていないことの防波堤）。
    #[tokio::test]
    async fn add_allowed_command_rejects_non_owner_without_side_effects() {
        // このゲートが bridge 側に無いことを固定する（多層防御ではなく単層である事実）。
        assert!(
            !opencrab_actions::OWNER_ONLY_ACTIONS.contains(&"add_allowed_command"),
            "bridge 側に owner ゲートが増えたなら、この単層前提のコメントを見直すこと"
        );

        let state = state_with_shell(&[]);
        let actions = SystemGatewayActions::new(state.clone(), None, None, None);

        let r = actions
            .execute(
                "add_allowed_command",
                &json!({"command": "curl"}),
                &agent_ctx(),
            )
            .await;

        assert!(!r.success);
        assert_eq!(
            r.error.as_deref(),
            Some("このアクションはオーナーのみ実行できます"),
            "拒否文言は移設前と 1 文字も変えない"
        );
        assert!(r.data.is_none());
        // 副作用ゼロ: DB も走行中の実行許可設定も変わらない。
        assert!(db_allowed_commands(&state, "agent-x").is_empty());
        assert!(live_allowed_commands(&state).is_empty());
    }

    /// **オーナー限定検査が移設後も効く**（remove）。既に許可済みのコマンドが
    /// 非オーナーの呼び出しで消えないこと。
    #[tokio::test]
    async fn remove_allowed_command_rejects_non_owner_without_side_effects() {
        assert!(
            !opencrab_actions::OWNER_ONLY_ACTIONS.contains(&"remove_allowed_command"),
            "bridge 側に owner ゲートが増えたなら、この単層前提のコメントを見直すこと"
        );

        let state = state_with_shell(&["git"]);
        {
            let conn = state.db.lock().unwrap();
            opencrab_db::queries::add_agent_allowed_command(&conn, "agent-x", "git", "owner")
                .unwrap();
        }
        let actions = SystemGatewayActions::new(state.clone(), None, None, None);

        let r = actions
            .execute(
                "remove_allowed_command",
                &json!({"command": "git"}),
                &agent_ctx(),
            )
            .await;

        assert!(!r.success);
        assert_eq!(
            r.error.as_deref(),
            Some("このアクションはオーナーのみ実行できます"),
            "拒否文言は移設前と 1 文字も変えない"
        );
        // 許可は残っている（DB も走行中の設定も）。
        assert_eq!(db_allowed_commands(&state, "agent-x"), vec!["git"]);
        assert_eq!(live_allowed_commands(&state), vec!["git"]);
    }

    /// **グローバルな実行許可設定へは書かない**（#202）。DB だけが更新される。
    ///
    /// 移設前の Discord 実装は DB と併せてグローバル設定にも書き込んでいた。応答生成は
    /// **全エージェント**についてこの設定を実行許可の土台として複製する
    /// （`crate::process::resolve_run_tools_config`）ので、その書き込みは
    /// 「A が許可したコマンドが全エージェントで実行可能になる」漏れそのものだった。
    ///
    /// このテストは**旧 `add_allowed_command_updates_the_live_shared_tools_config` の
    /// 反転**である。旧テストは漏れを不変条件として固定していた。
    #[tokio::test]
    async fn add_allowed_command_does_not_write_to_the_global_tools_config() {
        let state = state_with_shell(&["ls"]);
        let actions = SystemGatewayActions::new(state.clone(), None, None, None);

        let r = actions
            .execute(
                "add_allowed_command",
                &json!({"command": "curl"}),
                &owner_ctx(),
            )
            .await;
        assert!(r.success, "{:?}", r.error);

        // DB へ永続化されている（信頼できる情報源）。
        assert_eq!(db_allowed_commands(&state, "agent-x"), vec!["curl"]);
        // グローバル設定は 1 文字も変わらない。
        assert_eq!(
            live_allowed_commands(&state),
            vec!["ls"],
            "グローバル設定へ書き込むと全エージェントへ漏れる（#202）"
        );
    }

    /// 削除もグローバル設定を触らない（追加と対称 / #202）。
    ///
    /// 旧実装は `retain` でグローバル設定からも消していたため、**設定ファイル由来の
    /// コマンドをエージェントの操作でグローバルに削除できた**。
    /// 旧 `remove_allowed_command_updates_the_live_shared_tools_config` の反転。
    #[tokio::test]
    async fn remove_allowed_command_does_not_write_to_the_global_tools_config() {
        // "curl" は**設定ファイル由来**でもあり、かつ agent-x の DB 許可でもある状態。
        let state = state_with_shell(&["ls", "curl"]);
        {
            let conn = state.db.lock().unwrap();
            opencrab_db::queries::add_agent_allowed_command(&conn, "agent-x", "curl", "owner")
                .unwrap();
        }
        let actions = SystemGatewayActions::new(state.clone(), None, None, None);

        let r = actions
            .execute(
                "remove_allowed_command",
                &json!({"command": "curl"}),
                &owner_ctx(),
            )
            .await;
        assert!(r.success, "{:?}", r.error);

        assert!(db_allowed_commands(&state, "agent-x").is_empty());
        assert_eq!(
            live_allowed_commands(&state),
            vec!["ls", "curl"],
            "設定ファイル由来のコマンドをエージェントの操作で消してはならない（#202）"
        );
    }

    /// **エージェント A の追加が、エージェント B の実行許可を変えない**（#202 の本体）。
    ///
    /// 「次の run が何を許可するか」は `crate::process::resolve_run_tools_config` が
    /// 決める（応答生成が毎 run 呼ぶ唯一の解決点）。A の追加後にそれを両エージェントで
    /// 解決し、A にだけ効いていることを固定する。
    ///
    /// これが `add_allowed_command_takes_effect_on_the_next_run_but_not_the_same_turn` と対になって
    /// 「撤去しても呼び出し元は困らない / 他エージェントへは漏れない」の両方を証明する。
    #[tokio::test]
    async fn add_allowed_command_does_not_change_another_agents_permissions() {
        let state = state_with_shell(&["ls"]);
        let actions = SystemGatewayActions::new(state.clone(), None, None, None);

        let r = actions
            .execute(
                "add_allowed_command",
                &json!({"command": "curl"}),
                &GatewayCallContext::new(GatewayCaller::Owner, "agent-a"),
            )
            .await;
        assert!(r.success, "{:?}", r.error);

        assert_eq!(
            run_allowed_commands(&state, "agent-a"),
            vec!["ls", "curl"],
            "追加したエージェント自身には次の run で効かなければならない"
        );
        assert_eq!(
            run_allowed_commands(&state, "agent-b"),
            vec!["ls"],
            "agent-a の追加が agent-b の実行許可を広げてはならない（#202）"
        );
        // グローバル設定そのものも汚れていない。
        assert_eq!(live_allowed_commands(&state), vec!["ls"]);
    }

    /// **エージェント A の削除が、設定ファイル由来のコマンドや B の許可を消さない**（#202）。
    #[tokio::test]
    async fn remove_allowed_command_does_not_change_another_agents_permissions() {
        // 設定ファイル由来: "ls"。A と B の両方が DB で "curl" を許可されている。
        let state = state_with_shell(&["ls"]);
        {
            let conn = state.db.lock().unwrap();
            for agent in ["agent-a", "agent-b"] {
                opencrab_db::queries::add_agent_allowed_command(&conn, agent, "curl", "owner")
                    .unwrap();
            }
        }
        let actions = SystemGatewayActions::new(state.clone(), None, None, None);

        let r = actions
            .execute(
                "remove_allowed_command",
                &json!({"command": "curl"}),
                &GatewayCallContext::new(GatewayCaller::Owner, "agent-a"),
            )
            .await;
        assert!(r.success, "{:?}", r.error);

        assert_eq!(
            run_allowed_commands(&state, "agent-a"),
            vec!["ls"],
            "削除は呼び出したエージェントには次の run で効く"
        );
        assert_eq!(
            run_allowed_commands(&state, "agent-b"),
            vec!["ls", "curl"],
            "agent-a の削除が agent-b の許可を消してはならない（#202）"
        );
        assert_eq!(
            live_allowed_commands(&state),
            vec!["ls"],
            "設定ファイル由来の許可は残る（#202）"
        );
    }

    /// **追加した許可は「次の run」で呼び出したエージェントに効く**（撤去の前提の実証）。
    ///
    /// グローバル設定への書き込みを撤去してよい根拠は 2 つあり、両方をここで実際に
    /// 走らせて確かめる。
    ///
    /// 1. **次の run で効く**: run の冒頭で `resolve_run_tools_config` が DB の許可を
    ///    ローカル複製へマージし、`register_tools_from_config` がそれを `ShellToolAction`
    ///    へ渡す。したがって次の run のシェルツールは許可リスト検査を通す。
    /// 2. **同ターンでは元から効かない**: ツール登録は run 冒頭のスナップショットなので、
    ///    許可を追加しても**その run で登録済みのツール**には届かない。つまりグローバル
    ///    書き込みを撤去しても失われる機能は無い（撤去前も同ターン反映は無かった）。
    ///
    /// 許可リスト検査だけを見るため、実際には存在しないコマンド名を使う。
    /// 拒否は「allowed list に無い」/ 通過は「spawn 失敗」で区別でき、プロセスは
    /// 一切起動しない（PATH や OS 差に依存しない）。
    #[tokio::test]
    async fn add_allowed_command_takes_effect_on_the_next_run_but_not_the_same_turn() {
        const CMD: &str = "opencrab_absent_probe";

        let state = state_with_shell(&[]);
        let actions = SystemGatewayActions::new(state.clone(), None, None, None);

        // --- この run のツールを登録する（run 冒頭のスナップショット） ---
        let mut this_run = opencrab_actions::ActionDispatcher::new();
        opencrab_actions::register_tools_from_config(
            &crate::process::resolve_run_tools_config(&state, "agent-x"),
            &mut this_run,
        );

        // --- 走行中に許可を追加する ---
        let r = actions
            .execute(
                "add_allowed_command",
                &json!({"command": CMD}),
                &owner_ctx(),
            )
            .await;
        assert!(r.success, "{:?}", r.error);

        let (_dir, ctx) = shell_ctx();

        // 根拠 2: **同ターンでは効かない**（登録済みツールはスナップショットを持つ）。
        let same_turn = this_run
            .execute("execute_shell", &json!({"command": CMD}), &ctx)
            .await;
        assert!(!same_turn.success);
        assert!(
            same_turn
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("is not in the allowed list"),
            "同ターン反映は元から効かない前提が崩れている: {:?}",
            same_turn.error
        );

        // 根拠 1: **次の run では効く**（DB からマージされる）。
        let mut next_run = opencrab_actions::ActionDispatcher::new();
        opencrab_actions::register_tools_from_config(
            &crate::process::resolve_run_tools_config(&state, "agent-x"),
            &mut next_run,
        );
        let next = next_run
            .execute("execute_shell", &json!({"command": CMD}), &ctx)
            .await;
        assert!(!next.success, "存在しないコマンドなので spawn は失敗する");
        let e = next.error.as_deref().unwrap_or_default();
        assert!(
            !e.contains("is not in the allowed list"),
            "次の run では許可リスト検査を通らなければならない（撤去の前提）: {e}"
        );
        assert!(
            e.contains("Failed to spawn command"),
            "許可リストを通過して spawn まで到達したはず: {e}"
        );

        // グローバル設定は最後まで汚れていない。
        assert!(live_allowed_commands(&state).is_empty());
    }

    /// **コマンド名の文字種検査が効く**（英数字・`-`・`_` のみ）。
    ///
    /// 同系統の `manage_allowed_commands` は trim だけなので、移設でこちらを緩めると
    /// `rm -rf /` のようなシェル片やパス区切りを許可リストへ入れられてしまう。
    /// 検査は DB へ触る**前**に行う（副作用ゼロ）。
    #[tokio::test]
    async fn add_allowed_command_rejects_invalid_command_characters() {
        let state = state_with_shell(&[]);
        let actions = SystemGatewayActions::new(state.clone(), None, None, None);

        for bad in ["rm -rf /", "/bin/sh", "git;whoami", "cat|less", "a$b"] {
            let r = actions
                .execute(
                    "add_allowed_command",
                    &json!({"command": bad}),
                    &owner_ctx(),
                )
                .await;
            assert!(!r.success, "{bad} は拒否されなければならない");
            let e = r.error.unwrap();
            assert_eq!(
                e,
                format!(
                    "コマンド名に無効な文字が含まれています: {}（英数字・ハイフン・アンダースコアのみ使用可）",
                    bad
                ),
                "文字種エラーの文言は移設前と同一"
            );
        }
        // 1 件も通っていない。
        assert!(db_allowed_commands(&state, "agent-x").is_empty());
        assert!(live_allowed_commands(&state).is_empty());

        // 対: 妥当な文字（英数字・ハイフン・アンダースコア）は通る。
        for good in ["curl", "docker-compose", "my_tool", "python3"] {
            let r = actions
                .execute(
                    "add_allowed_command",
                    &json!({"command": good}),
                    &owner_ctx(),
                )
                .await;
            assert!(r.success, "{good} は許可されるべき: {:?}", r.error);
        }
    }

    /// `command` 未指定 / 空文字は移設前と同じ文言で失敗する（add / remove の両方）。
    #[tokio::test]
    async fn allowed_command_tools_require_a_non_empty_command() {
        let state = state_with_shell(&[]);
        let actions = SystemGatewayActions::new(state, None, None, None);
        for name in ["add_allowed_command", "remove_allowed_command"] {
            for args in [json!({}), json!({"command": ""}), json!({"command": 42})] {
                let r = actions.execute(name, &args, &owner_ctx()).await;
                assert!(!r.success, "{name} {args}");
                assert_eq!(
                    r.error.as_deref(),
                    Some("commandパラメータが必要です"),
                    "{name} {args}"
                );
            }
        }
    }

    /// **レスポンス JSON が移設前と同一**（許可コマンド 3 種）。期待値をリテラルで固定する。
    #[tokio::test]
    async fn allowed_command_response_json_is_unchanged() {
        let state = state_with_shell(&[]);
        let actions = SystemGatewayActions::new(state.clone(), None, None, None);

        // 追加（新規）
        let r = actions
            .execute(
                "add_allowed_command",
                &json!({"command": "curl"}),
                &owner_ctx(),
            )
            .await;
        assert!(r.success);
        assert_eq!(
            r.data.unwrap(),
            json!({
                "command": "curl",
                "agent_id": "agent-x",
                "message": "`curl` を許可コマンドに追加しました",
            })
        );

        // 追加（既存）: `already_exists` が付く。
        let r = actions
            .execute(
                "add_allowed_command",
                &json!({"command": "curl"}),
                &owner_ctx(),
            )
            .await;
        assert!(r.success);
        assert_eq!(
            r.data.unwrap(),
            json!({
                "command": "curl",
                "agent_id": "agent-x",
                "message": "`curl` はすでに許可コマンドに登録されています",
                "already_exists": true,
            })
        );

        // 一覧: commands / count / agent_id の 3 キー。
        let r = actions
            .execute("list_allowed_commands", &json!({}), &agent_ctx())
            .await;
        assert!(r.success);
        assert_eq!(
            r.data.unwrap(),
            json!({
                "commands": ["curl"],
                "count": 1,
                "agent_id": "agent-x",
            })
        );

        // 削除（存在した）
        let r = actions
            .execute(
                "remove_allowed_command",
                &json!({"command": "curl"}),
                &owner_ctx(),
            )
            .await;
        assert!(r.success);
        assert_eq!(
            r.data.unwrap(),
            json!({
                "command": "curl",
                "agent_id": "agent-x",
                "message": "`curl` を許可コマンドから削除しました",
            })
        );

        // 削除（存在しない）: `not_found` が付き、success は true のまま。
        let r = actions
            .execute(
                "remove_allowed_command",
                &json!({"command": "curl"}),
                &owner_ctx(),
            )
            .await;
        assert!(r.success);
        assert_eq!(
            r.data.unwrap(),
            json!({
                "command": "curl",
                "agent_id": "agent-x",
                "message": "`curl` は許可コマンドに登録されていませんでした",
                "not_found": true,
            })
        );
    }

    /// 一覧は**呼び出し元のエージェント**の許可コマンドだけを返す（agent_id スコープ）。
    #[tokio::test]
    async fn list_allowed_commands_is_scoped_to_the_calling_agent() {
        let state = crate::test_app_state();
        {
            let conn = state.db.lock().unwrap();
            opencrab_db::queries::add_agent_allowed_command(&conn, "agent-x", "curl", "owner")
                .unwrap();
            opencrab_db::queries::add_agent_allowed_command(&conn, "other-agent", "wget", "owner")
                .unwrap();
        }
        let actions = SystemGatewayActions::new(state, None, None, None);
        let r = actions
            .execute("list_allowed_commands", &json!({}), &agent_ctx())
            .await;
        assert!(r.success);
        assert_eq!(r.data.unwrap()["commands"], json!(["curl"]));
    }

    /// **レスポンス JSON が移設前と同一**（記憶インデックス設定）。
    /// `previous` / `current` の入れ子形をリテラルで固定する。
    #[tokio::test]
    async fn update_memory_index_config_response_json_is_unchanged() {
        let state = crate::test_app_state();
        let actions = SystemGatewayActions::new(state.clone(), None, None, None);

        // 未設定からの更新: previous は既定値。
        let r = actions
            .execute(
                "update_memory_index_config",
                &json!({"batch_size": 10}),
                &agent_ctx(),
            )
            .await;
        assert!(r.success, "{:?}", r.error);
        assert_eq!(
            r.data.unwrap(),
            json!({
                "agent_id": "agent-x",
                "previous": {
                    "batch_size": opencrab_db::queries::BATCH_SIZE_DEFAULT,
                    "threshold": opencrab_db::queries::THRESHOLD_DEFAULT,
                },
                "current": { "batch_size": 10, "threshold": opencrab_db::queries::THRESHOLD_DEFAULT },
            })
        );

        // 片方だけ指定すると、もう片方は現状維持。
        let r = actions
            .execute(
                "update_memory_index_config",
                &json!({"threshold": 5}),
                &agent_ctx(),
            )
            .await;
        assert!(r.success);
        assert_eq!(
            r.data.unwrap(),
            json!({
                "agent_id": "agent-x",
                "previous": { "batch_size": 10, "threshold": opencrab_db::queries::THRESHOLD_DEFAULT },
                "current": { "batch_size": 10, "threshold": 5 },
            })
        );

        // DB へ永続化されている。
        let conn = state.db.lock().unwrap();
        let cfg = opencrab_db::queries::get_memory_index_config(&conn, "agent-x").unwrap();
        assert_eq!((cfg.batch_size, cfg.threshold), (10, 5));
    }

    /// 引数が両方欠けているときは移設前と同じ文言で失敗する。
    #[tokio::test]
    async fn update_memory_index_config_requires_at_least_one_field() {
        let state = crate::test_app_state();
        let actions = SystemGatewayActions::new(state, None, None, None);
        let r = actions
            .execute("update_memory_index_config", &json!({}), &agent_ctx())
            .await;
        assert!(!r.success);
        assert_eq!(
            r.error.as_deref(),
            Some("batch_sizeまたはthresholdの少なくとも1つが必要です")
        );
    }

    /// 移設した 4 ツールは **inner（Discord）へ委譲しない**。
    ///
    /// `cancel_subtask` / `report_progress` は Discord 固有の後処理を保つため委譲する
    /// が、この 4 つは Discord 側の実装を撤去したので own が処理しなければならない。
    /// 委譲パターンで書くと、Discord が誤って再定義したときに own の実装が黙って
    /// バイパスされる。
    #[tokio::test]
    async fn generic_management_tools_are_not_delegated_to_inner() {
        let state = state_with_shell(&[]);
        let inner = Arc::new(RecordingInner::new(&[
            "update_memory_index_config",
            "add_allowed_command",
            "list_allowed_commands",
            "remove_allowed_command",
        ]));
        let actions = SystemGatewayActions::new(
            state.clone(),
            Some(inner.clone() as Arc<dyn GatewayActions>),
            None,
            None,
        );

        for (name, args) in [
            ("update_memory_index_config", json!({"batch_size": 7})),
            ("add_allowed_command", json!({"command": "curl"})),
            ("list_allowed_commands", json!({})),
            ("remove_allowed_command", json!({"command": "curl"})),
        ] {
            let r = actions.execute(name, &args, &owner_ctx()).await;
            assert!(r.success, "{name}: {:?}", r.error);
            assert!(
                r.data.as_ref().unwrap().get("reached_inner").is_none(),
                "{name} が inner へ委譲されている（own が処理すべき）"
            );
        }
        assert!(
            inner.calls().is_empty(),
            "inner へ到達してはならない: {:?}",
            inner.calls()
        );
    }

    /// **transport gateway が inner に居ても（REST + Discord 構成）漏れないことの固定**。
    ///
    /// このテストは**旧 `hot_reload_reaches_the_shared_config_even_with_a_transport_inner`
    /// の反転**である。旧テストは「inner が居てもグローバル設定に反映される」ことを
    /// 不変条件として固定していたが、それは #202 の漏れそのものだった。
    ///
    /// 経緯（#197 との関係）: REST（`crate::api::agents_messages`）は Discord が有効な
    /// とき `SystemGatewayActions { inner: Some(DiscordGatewayActions) }` を組む。移設前は
    /// その Discord gateway へ `Arc::new(RwLock::new(state.tools_config.read().clone()))`
    /// ＝**使い捨てのコピー**を渡していた。そのおかげで REST 経路は**偶然この漏れが
    /// 無かった**。素朴に移設すると共有実体へ届いて漏れる側に揃ってしまうため、同じ
    /// 変更でグローバル書き込みを撤去した。
    ///
    /// #197 について構造面で言えることは、`DiscordGatewayActions::new` がもう実行許可
    /// 設定を受け取らない（引数自体が消えた）＝**別インスタンスを作る余地がコンパイル時に
    /// 無い**という点だけである。
    #[tokio::test]
    async fn add_allowed_command_does_not_leak_to_the_global_config_with_a_transport_inner() {
        let state = state_with_shell(&[]);
        // REST + Discord 相当: transport gateway が inner に居る構成。
        let inner = Arc::new(RecordingInner::new(&["discord_send_file"]));
        let actions = SystemGatewayActions::new(
            state.clone(),
            Some(inner as Arc<dyn GatewayActions>),
            None,
            None,
        );

        let r = actions
            .execute(
                "add_allowed_command",
                &json!({"command": "curl"}),
                &owner_ctx(),
            )
            .await;
        assert!(r.success, "{:?}", r.error);

        // DB にだけ入る。
        assert_eq!(db_allowed_commands(&state, "agent-x"), vec!["curl"]);
        assert!(
            live_allowed_commands(&state).is_empty(),
            "inner の有無に関わらずグローバル設定へ書いてはならない（#202）: {:?}",
            live_allowed_commands(&state)
        );
    }

    // ================================================================================
    // #157 S6: スキル生成（create_skill）の移植テスト
    //
    // 旧 Discord 実装（`crates/discord` の `gateway_actions/agent_management.rs`）にあった
    // 3 テスト（基本 / 同名 dedup / 非 trusted 拒否）をそのまま持ってきたもの（1 件も
    // 落としていない）＋ 移設の本題（非 Discord 構成でも定義に現れる）・inner へ委譲
    // しないこと・レスポンス JSON / エラー文言 / `source_type` のリテラル固定。
    // ================================================================================

    fn co_agent_ctx() -> GatewayCallContext {
        GatewayCallContext::new(
            GatewayCaller::CoAgent {
                agent_id: "agent-peer".to_string(),
            },
            "agent-x",
        )
    }

    /// DB 上のスキル（アーカイブ済みも含む）を取得する。
    fn db_skill(state: &AppState, name: &str) -> Option<opencrab_db::queries::SkillRow> {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::find_skill_by_name_any(&conn, "agent-x", name).unwrap()
    }

    /// **#157 S6 の本題**: `create_skill` が own 定義にちょうど 1 件ある。
    ///
    /// own 定義は transport の有無に依存しないため、これが `definitions()` に出ることは
    /// 「web / Nostr / REST / heartbeat でも使える」ことと同義。own から消えると Discord
    /// 専用に逆戻りする（それが #157 が報告している不具合そのもの）。
    #[test]
    fn create_skill_is_exposed_in_own_definitions() {
        let defs = SystemGatewayActions::own_definitions();
        assert_eq!(
            defs.iter().filter(|d| d.name == "create_skill").count(),
            1,
            "create_skill は own 定義にちょうど 1 件必要（#157 S6）"
        );
    }

    /// **Discord 無効の構成でも露出する**（#157 S6 の証明）。
    ///
    /// `inner = None` は「transport 固有 gateway が居ない」経路（web / REST / Nostr /
    /// heartbeat、および Discord feature 無効ビルド）そのもの。移設前はこの構成で
    /// `create_skill` が一切出なかった。
    #[test]
    fn create_skill_is_exposed_without_any_transport_gateway() {
        let state = crate::test_app_state();
        let actions = SystemGatewayActions::new(state, None, None, None);
        let names: Vec<String> = actions.definitions().into_iter().map(|d| d.name).collect();
        assert!(
            names.contains(&"create_skill".to_string()),
            "transport gateway 無しの構成で create_skill が露出しない（#157 の不具合そのもの）: {names:?}"
        );
    }

    /// 定義（description / 引数スキーマ）を移設前（Discord 定義）と 1 バイトも変えない。
    ///
    /// description は LLM がツールを選ぶ唯一の手がかりなので、文言が変わると挙動が変わる。
    #[test]
    fn create_skill_definition_matches_the_discord_original() {
        let defs = SystemGatewayActions::own_definitions();
        let d = defs.iter().find(|d| d.name == "create_skill").unwrap();
        assert_eq!(
            d.description,
            "ユーザーから「〇〇するスキルを作って」と言われたとき新しいスキルを作成する。guidanceにコマンド例・使い方を書くことで、LLMがexecute_shellで動的に実行できるようになる。同名スキルが存在する場合は更新される。"
        );
        assert_eq!(d.parameters["type"], json!("object"));
        assert_eq!(d.parameters["required"], json!(["name", "description"]));
        let props = d.parameters["properties"].as_object().unwrap();
        let mut keys: Vec<&str> = props.keys().map(|s| s.as_str()).collect();
        keys.sort();
        assert_eq!(keys, vec!["description", "guidance", "name"]);
        for k in ["name", "description", "guidance"] {
            assert_eq!(props[k]["type"], json!("string"), "{k}");
        }
        assert_eq!(props["name"]["description"], json!("スキル名"));
        assert_eq!(props["description"]["description"], json!("スキルの説明"));
        assert_eq!(
            props["guidance"]["description"],
            json!("スキルのガイダンス（省略時は空文字列）")
        );
    }

    /// **inner へ委譲されない**（own が唯一の実装）。
    ///
    /// 委譲パターンのまま残すと、Discord が誤って再定義したときに own の実装が黙って
    /// バイパスされる（#155 の後退）。
    #[tokio::test]
    async fn create_skill_is_not_delegated_to_inner() {
        let state = crate::test_app_state();
        let inner = Arc::new(RecordingInner::new(&["create_skill"]));
        let actions = SystemGatewayActions::new(state.clone(), Some(inner.clone()), None, None);

        let r = actions
            .execute(
                "create_skill",
                &json!({"name": "天気確認", "description": "curl wttr.in で天気を確認する"}),
                &owner_ctx(),
            )
            .await;
        assert!(r.success, "{:?}", r.error);
        assert!(
            inner.calls().is_empty(),
            "create_skill が inner へ委譲された: {:?}",
            inner.calls()
        );
        // own の実装が実際に走った証拠（inner のフェイクは DB を触らない）。
        assert!(db_skill(&state, "天気確認").is_some());
    }

    /// 移植: 基本の作成。レスポンス JSON のキーと `action` の値、DB に書く
    /// `source_type` / `permission` / `situation_pattern` をリテラルで固定する。
    #[tokio::test]
    async fn create_skill_basic() {
        let state = crate::test_app_state();
        let actions = SystemGatewayActions::new(state.clone(), None, None, None);
        let result = actions
            .execute(
                "create_skill",
                &json!({
                    "name": "天気確認",
                    "description": "curl wttr.inで天気を確認する"
                }),
                &owner_ctx(),
            )
            .await;
        assert!(result.success, "create_skill should succeed");
        let data = result.data.unwrap();
        assert!(data["id"].is_string(), "should return id");
        assert_eq!(data["name"], json!("天気確認"));
        assert_eq!(data["action"], json!("created"));
        let mut keys: Vec<&str> = data
            .as_object()
            .unwrap()
            .keys()
            .map(|s| s.as_str())
            .collect();
        keys.sort();
        assert_eq!(keys, vec!["action", "id", "name"]);

        // 記録される取得元（`source_type`）を移設で変えない。core の `create_my_skill` は
        // `"self_created"` を書く**別のツール**（#157 では統廃合しない）。
        let row = db_skill(&state, "天気確認").unwrap();
        assert_eq!(row.source_type, "acquired");
        assert_eq!(row.permission, "\"agent\"");
        assert_eq!(row.situation_pattern, "");
        assert_eq!(row.guidance, "", "guidance 省略時は空文字列");
        assert!(row.is_active);
        assert!(!row.archived);
    }

    /// 移植: 同名は upsert（`action="updated"`。行は増えない）。
    #[tokio::test]
    async fn create_skill_dedup() {
        let state = crate::test_app_state();
        let actions = SystemGatewayActions::new(state.clone(), None, None, None);
        let first = actions
            .execute(
                "create_skill",
                &json!({
                    "name": "天気確認",
                    "description": "first version"
                }),
                &owner_ctx(),
            )
            .await;
        assert!(first.success);
        let result2 = actions
            .execute(
                "create_skill",
                &json!({
                    "name": "天気確認",
                    "description": "updated version",
                    "guidance": "curl wttr.in"
                }),
                &owner_ctx(),
            )
            .await;
        assert!(result2.success, "second create should succeed (dedup)");
        let data = result2.data.unwrap();
        assert_eq!(data["action"], json!("updated"));
        // 同じ行が更新される（id 不変・description / guidance だけ差し替わる）。
        assert_eq!(data["id"], first.data.unwrap()["id"]);
        let row = db_skill(&state, "天気確認").unwrap();
        assert_eq!(row.description, "updated version");
        assert_eq!(row.guidance, "curl wttr.in");
        let conn = state.db.lock().unwrap();
        let all = opencrab_db::queries::list_skills(&conn, "agent-x", false).unwrap();
        assert_eq!(all.len(), 1, "同名で行が増えてはならない");
    }

    /// アーカイブ済みの同名スキルは復活する（`action="restored"` / archived=false）。
    #[tokio::test]
    async fn create_skill_restores_archived_skill() {
        let state = crate::test_app_state();
        let actions = SystemGatewayActions::new(state.clone(), None, None, None);
        assert!(
            actions
                .execute(
                    "create_skill",
                    &json!({"name": "天気確認", "description": "v1"}),
                    &owner_ctx(),
                )
                .await
                .success
        );
        {
            let conn = state.db.lock().unwrap();
            let mut row =
                opencrab_db::queries::find_skill_by_name_any(&conn, "agent-x", "天気確認")
                    .unwrap()
                    .unwrap();
            row.archived = true;
            row.is_active = false;
            opencrab_db::queries::update_skill(&conn, &row).unwrap();
        }

        let r = actions
            .execute(
                "create_skill",
                &json!({"name": "天気確認", "description": "v2"}),
                &owner_ctx(),
            )
            .await;
        assert!(r.success, "{:?}", r.error);
        assert_eq!(r.data.unwrap()["action"], json!("restored"));
        let row = db_skill(&state, "天気確認").unwrap();
        assert!(!row.archived);
        assert!(row.is_active);
        assert_eq!(row.description, "v2");
    }

    /// 移植: 非 trusted（素の Agent）は拒否。**エラー文言はバイト単位で移設前と同一。**
    ///
    /// このゲートは**二重構造**である: bridge の `TRUSTED_ONLY_ACTIONS` が可視性と実行の
    /// 双方を（名前ベースで）ゲートし、ハンドラ内の `matches!` が多層防御として残る。
    /// bridge 側は名前で引くので移設しても効き続ける（そのことをここで固定する）。
    /// なお**ハンドラ側の拒否はマーカー無し**（`REJECTION_CODE_PREFIX` を付けない）で、
    /// これも移設前と同じ形。
    #[tokio::test]
    async fn create_skill_rejected_for_non_owner() {
        assert!(
            opencrab_actions::TRUSTED_ONLY_ACTIONS.contains(&"create_skill"),
            "bridge 側の trusted ゲートが消えたら、ハンドラ内検査が唯一のゲートになる"
        );

        let state = crate::test_app_state();
        let actions = SystemGatewayActions::new(state.clone(), None, None, None);
        let result = actions
            .execute(
                "create_skill",
                &json!({
                    "name": "test",
                    "description": "test"
                }),
                &agent_ctx(),
            )
            .await;
        assert!(!result.success);
        let err = result.error.unwrap();
        assert!(err.contains("trusted user"));
        assert_eq!(
            err, "このアクションはtrusted userのみ実行できます",
            "拒否文言は移設前と 1 バイトも変えない"
        );
        assert!(
            !err.starts_with(REJECTION_CODE_PREFIX),
            "マーカーの有無も移設前と同じ（付けない）"
        );
        // 副作用ゼロ。
        assert!(db_skill(&state, "test").is_none());
    }

    /// trusted_user / co_agent は実行できる（許可集合を移設で狭めない）。
    #[tokio::test]
    async fn create_skill_allowed_for_trusted_user_and_co_agent() {
        let state = crate::test_app_state();
        let actions = SystemGatewayActions::new(state.clone(), None, None, None);
        for (i, ctx) in [trusted_ctx(), co_agent_ctx()].into_iter().enumerate() {
            let name = format!("skill-{i}");
            let r = actions
                .execute(
                    "create_skill",
                    &json!({"name": name, "description": "d"}),
                    &ctx,
                )
                .await;
            assert!(
                r.success,
                "{:?} は実行できるべき: {:?}",
                ctx.caller, r.error
            );
            assert!(db_skill(&state, &name).is_some());
        }
    }

    /// 必須引数エラーの文言（英語のまま・マーカー無し）を固定する。
    #[tokio::test]
    async fn create_skill_missing_arguments_keep_original_messages() {
        let state = crate::test_app_state();
        let actions = SystemGatewayActions::new(state, None, None, None);

        let r = actions
            .execute("create_skill", &json!({}), &owner_ctx())
            .await;
        assert!(!r.success);
        assert_eq!(r.error.as_deref(), Some("name is required"));

        let r = actions
            .execute("create_skill", &json!({"name": "n"}), &owner_ctx())
            .await;
        assert!(!r.success);
        assert_eq!(r.error.as_deref(), Some("description is required"));
    }

    /// 分類の所属を移設で変えない（Discord でも dispatchable だった）。
    #[test]
    fn create_skill_stays_dispatchable() {
        assert!(
            !opencrab_actions::default_non_dispatch_tools().contains("create_skill"),
            "create_skill は移設前と同じく dispatch 対象に残す（結果を同ターンで使わない）"
        );
        assert!(opencrab_actions::SERVER_DISPATCHABLE_ACTIONS.contains(&"create_skill"));
    }

    // ================================================================================
    // #157 S2 / #184: 停止（cancel_subtask）の移植テスト
    //
    // 旧 Discord 実装（`crates/discord` の `execute_cancel_subtask`）にあった 8 テストを
    // そのまま持ってきたもの（1 件も落としていない）。停止の実装は
    // `opencrab_actions::cancel_subtask` 1 箇所になったので、契約はこの合成層で固定する。
    // ================================================================================

    /// 停止対象を任意の label / tool_name で 1 件登録した registry を作る。
    fn registry_with_labeled(
        subtask_id: &str,
        session_id: &str,
        parent_session_id: &str,
        label: &str,
        tool_name: &str,
    ) -> SubtaskRegistry {
        let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());
        registry.insert(
            subtask_id.to_string(),
            opencrab_actions::SpawnedSubtask {
                abort_handle: tokio::spawn(std::future::pending::<()>()).abort_handle(),
                session_id: session_id.to_string(),
                parent_session_id: parent_session_id.to_string(),
                agent_id: "agent-x".to_string(),
                label: label.to_string(),
                tool_name: tool_name.to_string(),
                started_at: std::time::Instant::now(),
                reply_target: None,
                lifecycle: opencrab_actions::SubtaskLifecycle::new(),
            },
        );
        registry
    }

    /// sub-session の行を作る（明示的な `spawn_subtask` 相当）。
    fn insert_sub_session(state: &AppState, session_id: &str, theme: &str) {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::insert_session(
            &conn,
            &opencrab_db::queries::SessionRow {
                id: session_id.to_string(),
                mode: "subtask".to_string(),
                theme: theme.to_string(),
                phase: "active".to_string(),
                turn_number: 0,
                status: "active".to_string(),
                participant_ids_json: json!(["agent-x"]).to_string(),
                facilitator_id: None,
                done_count: 0,
                max_turns: None,
                metadata_json: None,
            },
        )
        .unwrap();
    }

    /// 停止ログ（`tool_cancelled`）を親セッションから 1 件だけ引く。
    fn cancelled_log(
        state: &AppState,
        parent_session_id: &str,
    ) -> opencrab_db::queries::SessionLogRow {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::list_recent_session_logs(&conn, parent_session_id, 20)
            .unwrap()
            .into_iter()
            .find(|l| l.log_type == "tool_cancelled")
            .expect("tool_cancelled が親ログに残る")
    }

    fn cancelled_log_metadata(state: &AppState, parent_session_id: &str) -> Value {
        serde_json::from_str(
            cancelled_log(state, parent_session_id)
                .metadata_json
                .as_deref()
                .unwrap(),
        )
        .unwrap()
    }

    fn parent_ctx(parent_session_id: &str) -> GatewayCallContext {
        GatewayCallContext::new(GatewayCaller::Agent, "agent-x").with_session_id(parent_session_id)
    }

    async fn cancel(
        actions: &SystemGatewayActions,
        subtask_id: &str,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        actions
            .execute("cancel_subtask", &json!({"subtask_id": subtask_id}), ctx)
            .await
    }

    /// 不在は**権限拒否ではない**プレーンなエラー（旧 Discord テストの移植）。
    #[tokio::test]
    async fn cancel_subtask_not_found_is_plain_error() {
        let state = crate::test_app_state();
        let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());
        let actions = SystemGatewayActions::new(state, None, Some(registry), None);
        let r = cancel(&actions, "no-such", &parent_ctx("web-agent-x-c1")).await;
        assert!(!r.success);
        let err = r.error.unwrap();
        assert_eq!(err, "cancel_subtask: subtask 'no-such' not found");
        assert!(!err.starts_with(REJECTION_CODE_PREFIX));
    }

    /// 他セッションが親の subtask は拒否し、エントリも残す（abort しない）。
    #[tokio::test]
    async fn cancel_subtask_rejects_foreign_session() {
        let state = crate::test_app_state();
        let registry = registry_with("st-x", "subtask-x1", "web-other-c9");
        let actions = SystemGatewayActions::new(state, None, Some(registry.clone()), None);
        let r = cancel(&actions, "st-x", &parent_ctx("web-agent-x-c1")).await;
        assert!(!r.success);
        assert_eq!(
            r.error.as_deref().unwrap(),
            format!("{REJECTION_CODE_PREFIX}cancel_subtask: subtask 'st-x' をこのセッションからキャンセルする権限がありません（親セッションまたは owner のみ）")
        );
        assert!(registry.contains_key("st-x"), "abort されていない");
    }

    /// 親セッションからの停止は成功し、registry から除去される。
    #[tokio::test]
    async fn cancel_subtask_allows_parent_session() {
        let state = crate::test_app_state();
        let parent = "web-agent-x-c1";
        let registry = registry_with("st-mine", "subtask-m1", parent);
        let actions = SystemGatewayActions::new(state, None, Some(registry.clone()), None);
        let r = cancel(&actions, "st-mine", &parent_ctx(parent)).await;
        assert!(r.success, "{:?}", r.error);
        // レスポンス JSON も旧実装と同一。
        assert_eq!(
            r.data.unwrap(),
            json!({"cancelled": true, "subtask_id": "st-mine"})
        );
        assert!(!registry.contains_key("st-mine"));
    }

    /// owner は無関係なセッション文脈からでも停止できる。
    #[tokio::test]
    async fn cancel_subtask_owner_bypasses_session_check() {
        let state = crate::test_app_state();
        let registry = registry_with("st-any", "subtask-a1", "web-other-c9");
        let actions = SystemGatewayActions::new(state, None, Some(registry.clone()), None);
        let r = cancel(&actions, "st-any", &owner_ctx()).await;
        assert!(r.success, "{:?}", r.error);
        assert!(!registry.contains_key("st-any"));
    }

    /// セッション文脈の無い agent は他人の subtask を停止できない。
    #[tokio::test]
    async fn cancel_subtask_rejects_agent_without_session() {
        let state = crate::test_app_state();
        let registry = registry_with("st-ns", "subtask-n1", "web-other-c9");
        let actions = SystemGatewayActions::new(state, None, Some(registry.clone()), None);
        let r = cancel(&actions, "st-ns", &agent_ctx()).await;
        assert!(!r.success);
        assert!(r
            .error
            .as_deref()
            .unwrap()
            .starts_with(REJECTION_CODE_PREFIX));
        assert!(registry.contains_key("st-ns"));
    }

    /// #176: 自動 dispatch した subtask は sub-session の行を持たないため theme を引けず、
    /// registry の label（ツール名を含む）へフォールバックする。
    #[tokio::test]
    async fn cancel_subtask_falls_back_to_label_without_sub_session() {
        let state = crate::test_app_state();
        let parent = "web-agent-x-c1";
        // sub-session は**作らない**（自動 dispatch の再現）。
        let registry = registry_with_labeled(
            "st-auto",
            "subtask-auto1",
            parent,
            "execute_shell(ls -la)",
            "execute_shell",
        );
        let actions = SystemGatewayActions::new(state.clone(), None, Some(registry), None);
        let r = cancel(&actions, "st-auto", &parent_ctx(parent)).await;
        assert!(r.success, "{:?}", r.error);

        let log = cancelled_log(&state, parent);
        assert_ne!(
            log.content, "subtask '' was cancelled",
            "sub-session が無いとラベルが空になっている（#176 の退行）"
        );
        assert_eq!(log.content, "subtask 'execute_shell(ls -la)' was cancelled");
        let meta = cancelled_log_metadata(&state, parent);
        assert_eq!(meta["task"], "execute_shell(ls -la)");
        // #184: 種別名は固定値ではなく**実際に停止したツール名**。
        assert_eq!(meta["tool_name"], "execute_shell");
        assert_eq!(meta["tool_call_id"], "st-auto");
        assert_eq!(meta["label"], "execute_shell(ls -la)");
        assert_eq!(meta["completed_calls"], json!([]));
    }

    /// 明示的な `spawn_subtask`（sub-session あり）では theme を使い、`Subtask: ` prefix を
    /// 除去する。
    #[tokio::test]
    async fn cancel_subtask_prefers_sub_session_theme() {
        let state = crate::test_app_state();
        let parent = "web-agent-x-c1";
        insert_sub_session(&state, "subtask-explicit1", "Subtask: ログを調査する");
        let registry = registry_with_labeled(
            "st-explicit",
            "subtask-explicit1",
            parent,
            "spawn_subtask(ログを調査する)",
            "spawn_subtask",
        );
        let actions = SystemGatewayActions::new(state.clone(), None, Some(registry), None);
        let r = cancel(&actions, "st-explicit", &parent_ctx(parent)).await;
        assert!(r.success, "{:?}", r.error);

        assert_eq!(
            cancelled_log(&state, parent).content,
            "subtask 'ログを調査する' was cancelled"
        );
        let meta = cancelled_log_metadata(&state, parent);
        assert_eq!(meta["task"], "ログを調査する");
        assert_eq!(meta["tool_name"], "spawn_subtask");
    }

    /// sub-session はあるが theme が空のケースでも label へフォールバックする。
    #[tokio::test]
    async fn cancel_subtask_falls_back_on_empty_theme() {
        let state = crate::test_app_state();
        let parent = "web-agent-x-c1";
        insert_sub_session(&state, "subtask-empty1", "");
        let registry = registry_with_labeled(
            "st-empty",
            "subtask-empty1",
            parent,
            "nostr_generate_key(main)",
            "nostr_generate_key",
        );
        let actions = SystemGatewayActions::new(state.clone(), None, Some(registry), None);
        let r = cancel(&actions, "st-empty", &parent_ctx(parent)).await;
        assert!(r.success, "{:?}", r.error);
        assert_eq!(
            cancelled_log(&state, parent).content,
            "subtask 'nostr_generate_key(main)' was cancelled"
        );
    }

    /// 旧 Discord 実装の固有の後始末その 1: **中断を lifecycle 通知口へ伝え、随伴マップ
    /// から外す**。落とすと lifecycle webhook の `aborted` が黙って消える。
    #[tokio::test]
    async fn cancel_subtask_notifies_the_run_notifier() {
        #[derive(Default)]
        struct Recorder(std::sync::Mutex<Vec<String>>);
        impl opencrab_actions::subtask_notify::SubtaskRunNotifier for Recorder {
            fn on_cancelled(&self, _duration_ms: u64) {
                self.0.lock().unwrap().push("cancelled".to_string());
            }
        }

        let state = crate::test_app_state();
        let recorder = Arc::new(Recorder::default());
        state
            .subtask_notifiers
            .insert("st-1".to_string(), recorder.clone());
        let parent = "web-agent-x-c1";
        let registry = registry_with("st-1", "subtask-st-1", parent);
        let actions = SystemGatewayActions::new(state.clone(), None, Some(registry), None);

        let r = cancel(&actions, "st-1", &parent_ctx(parent)).await;
        assert!(r.success, "{:?}", r.error);
        assert_eq!(recorder.0.lock().unwrap().clone(), vec!["cancelled"]);
        assert!(
            !state.subtask_notifiers.contains_key("st-1"),
            "通知口は registry と対で除去する"
        );
    }

    /// **停止も完了 sink（`on_subtask_cancelled`）へ通知する**（#184 / REST の永久 active
    /// バグ）。委譲していた頃の Discord 経路はこれを落としていた。
    #[tokio::test]
    async fn cancel_subtask_notifies_the_completion_sink() {
        #[derive(Default)]
        struct Recorder(std::sync::Mutex<Vec<String>>);
        impl SubtaskCompletionSink for Recorder {
            fn on_subtask_settled(&self, _ev: SubtaskSettled) {
                self.0.lock().unwrap().push("settled".to_string());
            }
            fn on_subtask_cancelled(&self, ev: SubtaskSettled) {
                self.0
                    .lock()
                    .unwrap()
                    .push(format!("cancelled:{}:{}", ev.subtask_id, ev.exit_reason));
            }
        }

        let state = crate::test_app_state();
        let parent = "web-agent-x-c1";
        let registry = registry_with("st-1", "subtask-st-1", parent);
        let sink = Arc::new(Recorder::default());
        let actions = SystemGatewayActions::new(
            state,
            None,
            Some(registry),
            Some(sink.clone() as Arc<dyn SubtaskCompletionSink>),
        );

        let r = cancel(&actions, "st-1", &parent_ctx(parent)).await;
        assert!(r.success, "{:?}", r.error);
        assert_eq!(
            sink.0.lock().unwrap().clone(),
            vec!["cancelled:st-1:cancelled"],
            "停止は on_subtask_cancelled だけを呼ぶ（resume する on_subtask_settled は呼ばない）"
        );
    }

    /// **negative assert（#157 S2）**: Discord が `cancel_subtask` を再定義しても own が
    /// 処理する。委譲パターンに戻すと own の後始末（通知・部分結果ログ・sink）が黙って
    /// バイパスされるので、その経路を作らせない。
    #[tokio::test]
    async fn cancel_subtask_is_not_delegated_to_inner() {
        let state = crate::test_app_state();
        let parent = "web-agent-x-c1";
        let registry = registry_with("st-1", "subtask-st-1", parent);
        let inner = Arc::new(RecordingInner::new(&["cancel_subtask"]));
        let actions = SystemGatewayActions::new(
            state,
            Some(inner.clone() as Arc<dyn GatewayActions>),
            Some(registry.clone()),
            None,
        );

        let r = cancel(&actions, "st-1", &parent_ctx(parent)).await;
        assert!(r.success, "{:?}", r.error);
        assert!(
            r.data.as_ref().unwrap().get("reached_inner").is_none(),
            "cancel_subtask が inner へ委譲されている（own が処理すべき）"
        );
        assert!(
            inner.calls().is_empty(),
            "inner へ到達してはならない: {:?}",
            inner.calls()
        );
        assert!(!registry.contains_key("st-1"), "own が実際に停止している");
    }

    /// merge 後も `cancel_subtask` は 1 件（own 優先で dedup）。
    #[test]
    fn merge_definitions_still_dedups_cancel_subtask() {
        let inner: Arc<dyn GatewayActions> = Arc::new(RecordingInner::new(&["cancel_subtask"]));
        let merged = SystemGatewayActions::merge_definitions(
            SystemGatewayActions::own_definitions(),
            Some(&inner),
        );
        assert_eq!(
            merged.iter().filter(|d| d.name == "cancel_subtask").count(),
            1
        );
    }

    // ================================================================================
    // #157 S3: ハートビート指示ツールの移植テスト
    //
    // 旧 Discord 実装（`crates/discord` の `heartbeat_instructions.rs`）にあった 4 テストを
    // そのまま持ってきたもの（1 件も落としていない）＋ 移設の本題（非 Discord 構成でも
    // 定義に現れる）とレスポンス JSON / エラー文言のリテラル固定。
    // ================================================================================

    fn trusted_ctx() -> GatewayCallContext {
        GatewayCallContext::new(GatewayCaller::TrustedUser, "agent-x")
    }

    /// エージェント行を用意する（`scope="agent"` の patch 対象）。
    fn insert_agent(state: &AppState, heartbeat_instructions: &str) {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::upsert_agent(
            &conn,
            &opencrab_db::queries::AgentRow {
                agent_id: "agent-x".to_string(),
                name: "N".to_string(),
                job_title: None,
                organization: None,
                image_url: None,
                persona_name: "P".to_string(),
                personality: None,
                instructions: String::new(),
                heartbeat_instructions: heartbeat_instructions.to_string(),
                model: None,
                reasoning_effort: None,
                web_search: None,
                metadata_json: None,
            },
        )
        .unwrap();
    }

    fn audit_rows(state: &AppState) -> Vec<opencrab_db::queries::HeartbeatInstructionsAuditRow> {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::list_heartbeat_instructions_audit(&conn, "agent-x", 10).unwrap()
    }

    /// **#157 S3 の本題**: 2 ツールが own 定義（= transport の有無に依存せず全ターンで
    /// 露出する）。own から消えると Discord 専用に逆戻りする。
    #[test]
    fn heartbeat_instruction_tools_are_exposed_in_own_definitions() {
        let defs = SystemGatewayActions::own_definitions();
        for name in [
            "update_heartbeat_instructions",
            "read_heartbeat_instructions",
        ] {
            assert_eq!(
                defs.iter().filter(|d| d.name == name).count(),
                1,
                "{name} は own 定義にちょうど 1 件必要（#157 S3）"
            );
        }
        let update = defs
            .iter()
            .find(|d| d.name == "update_heartbeat_instructions")
            .unwrap();
        let required = update.parameters["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "scope"));
        assert!(required.iter().any(|v| v == "instructions"));
        let props = update.parameters["properties"].as_object().unwrap();
        for key in ["scope", "channel_id", "guild_id", "instructions", "reason"] {
            assert!(props.contains_key(key), "missing property: {key}");
        }
        let read = defs
            .iter()
            .find(|d| d.name == "read_heartbeat_instructions")
            .unwrap();
        assert!(read.parameters["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "scope"));
    }

    /// **Discord 無効の構成でも定義に現れる**（#157 の本題）。inner=None は
    /// web / Nostr / REST / heartbeat 経路そのもの。
    #[test]
    fn heartbeat_instruction_tools_are_visible_without_discord() {
        let state = crate::test_app_state();
        let actions = SystemGatewayActions::new(state, None, None, None);
        let names: Vec<String> = actions.definitions().into_iter().map(|d| d.name).collect();
        assert!(names.contains(&"update_heartbeat_instructions".to_string()));
        assert!(names.contains(&"read_heartbeat_instructions".to_string()));
        // 停止も同様（#157 S2）。
        assert!(names.contains(&"cancel_subtask".to_string()));
    }

    /// owner 以外は拒否し、監査ログも残さない（旧 Discord テストの移植）。
    #[tokio::test]
    async fn update_heartbeat_instructions_rejected_for_non_owner() {
        let state = crate::test_app_state();
        let actions = SystemGatewayActions::new(state.clone(), None, None, None);
        let r = actions
            .execute(
                "update_heartbeat_instructions",
                &json!({"scope": "agent", "instructions": "話題があるときだけ話す"}),
                &trusted_ctx(),
            )
            .await;
        assert!(!r.success);
        assert_eq!(
            r.error.as_deref(),
            Some("このアクションはオーナーのみ実行できます")
        );
        assert!(audit_rows(&state).is_empty(), "監査ログを残してはならない");
    }

    /// owner は成功し、DB へ反映され、監査ログに old/new/reason が残る（旧テストの移植）。
    /// レスポンス JSON もリテラルで固定する。
    #[tokio::test]
    async fn update_heartbeat_instructions_owner_success_and_audit() {
        let state = crate::test_app_state();
        insert_agent(&state, "OLD");
        let actions = SystemGatewayActions::new(state.clone(), None, None, None);
        let r = actions
            .execute(
                "update_heartbeat_instructions",
                &json!({
                    "scope": "agent",
                    "instructions": "NEW指示",
                    "reason": "オーナー依頼",
                }),
                &owner_ctx(),
            )
            .await;
        assert!(r.success, "{:?}", r.error);
        assert_eq!(
            r.data.unwrap(),
            json!({
                "success": true,
                "scope": "agent",
                "channel_id": Value::Null,
                "length": 5,
                "preview": "NEW指示",
            })
        );

        {
            let conn = state.db.lock().unwrap();
            let got = opencrab_db::queries::get_agent(&conn, "agent-x")
                .unwrap()
                .unwrap();
            assert_eq!(got.heartbeat_instructions, "NEW指示");
        }
        let rows = audit_rows(&state);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].scope, "agent");
        assert_eq!(rows[0].old_value.as_deref(), Some("OLD"));
        assert_eq!(rows[0].new_value.as_deref(), Some("NEW指示"));
        assert_eq!(rows[0].reason.as_deref(), Some("オーナー依頼"));
        assert_eq!(rows[0].caller_identity, GatewayCaller::Owner.label());
    }

    /// エージェント行が無ければ移設前と同じ文言で失敗する。
    #[tokio::test]
    async fn update_heartbeat_instructions_missing_agent_and_bad_args() {
        let state = crate::test_app_state();
        let actions = SystemGatewayActions::new(state, None, None, None);

        let r = actions
            .execute(
                "update_heartbeat_instructions",
                &json!({"scope": "agent", "instructions": "x"}),
                &owner_ctx(),
            )
            .await;
        assert_eq!(r.error.as_deref(), Some("エージェントが見つかりません"));

        let r = actions
            .execute(
                "update_heartbeat_instructions",
                &json!({"scope": "agent"}),
                &owner_ctx(),
            )
            .await;
        assert_eq!(r.error.as_deref(), Some("instructionsパラメータが必要です"));

        let too_long = "あ".repeat(opencrab_db::queries::MAX_HEARTBEAT_INSTRUCTIONS_LEN + 1);
        let r = actions
            .execute(
                "update_heartbeat_instructions",
                &json!({"scope": "agent", "instructions": too_long}),
                &owner_ctx(),
            )
            .await;
        assert_eq!(
            r.error.as_deref(),
            Some(
                format!(
                    "instructionsが長すぎます（最大{}文字）",
                    opencrab_db::queries::MAX_HEARTBEAT_INSTRUCTIONS_LEN
                )
                .as_str()
            )
        );

        let r = actions
            .execute(
                "update_heartbeat_instructions",
                &json!({"scope": "channel", "instructions": "x"}),
                &owner_ctx(),
            )
            .await;
        assert_eq!(
            r.error.as_deref(),
            Some("scope=channelのときはchannel_idが必要です")
        );

        let r = actions
            .execute(
                "update_heartbeat_instructions",
                &json!({"scope": "channel", "channel_id": "ch1", "instructions": "x"}),
                &owner_ctx(),
            )
            .await;
        assert_eq!(
            r.error.as_deref(),
            Some("新規チャンネル設定の作成にはguild_idが必要です")
        );

        let r = actions
            .execute(
                "update_heartbeat_instructions",
                &json!({"scope": "nope", "instructions": "x"}),
                &owner_ctx(),
            )
            .await;
        assert_eq!(
            r.error.as_deref(),
            Some("不明なscope: nope（agent または channel）")
        );
    }

    /// `scope="effective"` が解決結果（source + instructions）を返す（旧テストの移植）。
    #[tokio::test]
    async fn read_heartbeat_instructions_effective() {
        let state = crate::test_app_state();
        {
            let conn = state.db.lock().unwrap();
            opencrab_db::queries::upsert_channel_config(
                &conn,
                &opencrab_db::queries::ChannelConfigRow {
                    channel_id: "ch1".to_string(),
                    agent_id: "agent-x".to_string(),
                    guild_id: "g1".to_string(),
                    channel_name: String::new(),
                    readable: true,
                    writable: true,
                    whitelisted: false,
                    heartbeat_enabled: true,
                    heartbeat_interval_secs: None,
                    heartbeat_instructions: "業務連絡のみ".to_string(),
                },
            )
            .unwrap();
        }
        let actions = SystemGatewayActions::new(state, None, None, None);
        let r = actions
            .execute(
                "read_heartbeat_instructions",
                &json!({"scope": "effective", "channel_id": "ch1"}),
                &trusted_ctx(),
            )
            .await;
        assert!(r.success, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["scope"], "effective");
        assert_eq!(data["channel_id"], "ch1");
        assert_eq!(data["source"], "channel");
        assert_eq!(data["instructions"], "業務連絡のみ");
    }

    /// 素の agent は拒否、co_agent は許可（旧テストの移植）。移設後も権限のゲートが効く。
    #[tokio::test]
    async fn read_heartbeat_instructions_rejected_for_plain_agent() {
        let state = crate::test_app_state();
        let actions = SystemGatewayActions::new(state, None, None, None);
        let r = actions
            .execute(
                "read_heartbeat_instructions",
                &json!({"scope": "agent"}),
                &agent_ctx(),
            )
            .await;
        assert!(!r.success);
        assert_eq!(
            r.error.as_deref(),
            Some("このアクションは信頼済みの呼び出し元のみ実行できます")
        );

        let allowed = actions
            .execute(
                "read_heartbeat_instructions",
                &json!({"scope": "agent"}),
                &GatewayCallContext::new(
                    GatewayCaller::CoAgent {
                        agent_id: "co-agent-1".to_string(),
                    },
                    "agent-x",
                ),
            )
            .await;
        assert!(allowed.success, "{:?}", allowed.error);
        assert_eq!(
            allowed.data.unwrap(),
            json!({"scope": "agent", "instructions": ""})
        );
    }

    /// **チャンネル単位設定の非対称（#157 S3）**: 非 Discord 経路には通常チャンネル設定の
    /// 行が無いので、`scope="channel"` は空文字列を返し、`scope="effective"` は
    /// エージェント/既定へフォールバックする。エラーにはならない（露出はする）。
    #[tokio::test]
    async fn read_heartbeat_instructions_channel_scope_is_empty_without_a_channel_row() {
        let state = crate::test_app_state();
        insert_agent(&state, "エージェント既定の指示");
        let actions = SystemGatewayActions::new(state, None, None, None);

        let r = actions
            .execute(
                "read_heartbeat_instructions",
                &json!({"scope": "channel", "channel_id": "no-such-channel"}),
                &trusted_ctx(),
            )
            .await;
        assert!(r.success, "{:?}", r.error);
        assert_eq!(
            r.data.unwrap(),
            json!({
                "scope": "channel",
                "channel_id": "no-such-channel",
                "instructions": "",
            })
        );

        let r = actions
            .execute(
                "read_heartbeat_instructions",
                &json!({"scope": "effective", "channel_id": "no-such-channel"}),
                &trusted_ctx(),
            )
            .await;
        assert!(r.success, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["instructions"], "エージェント既定の指示");
        assert_eq!(data["source"], "agent");
    }

    /// 読み出しの引数エラー文言も移設前と同一。
    #[tokio::test]
    async fn read_heartbeat_instructions_bad_args() {
        let state = crate::test_app_state();
        let actions = SystemGatewayActions::new(state, None, None, None);
        let r = actions
            .execute(
                "read_heartbeat_instructions",
                &json!({"scope": "channel"}),
                &trusted_ctx(),
            )
            .await;
        assert_eq!(
            r.error.as_deref(),
            Some("scope=channelのときはchannel_idが必要です")
        );

        let r = actions
            .execute(
                "read_heartbeat_instructions",
                &json!({"scope": "nope"}),
                &trusted_ctx(),
            )
            .await;
        assert_eq!(
            r.error.as_deref(),
            Some("不明なscope: nope（agent / channel / effective）")
        );
    }

    /// **negative assert（#157 S3）**: Discord がハートビート指示ツールを再定義しても own が
    /// 処理する（委譲パターンにしない）。
    #[tokio::test]
    async fn heartbeat_instruction_tools_are_not_delegated_to_inner() {
        let state = crate::test_app_state();
        insert_agent(&state, "OLD");
        let inner = Arc::new(RecordingInner::new(&[
            "update_heartbeat_instructions",
            "read_heartbeat_instructions",
        ]));
        let actions = SystemGatewayActions::new(
            state,
            Some(inner.clone() as Arc<dyn GatewayActions>),
            None,
            None,
        );

        for (name, args) in [
            (
                "update_heartbeat_instructions",
                json!({"scope": "agent", "instructions": "NEW"}),
            ),
            ("read_heartbeat_instructions", json!({"scope": "agent"})),
        ] {
            let r = actions.execute(name, &args, &owner_ctx()).await;
            assert!(r.success, "{name}: {:?}", r.error);
            assert!(
                r.data.as_ref().unwrap().get("reached_inner").is_none(),
                "{name} が inner へ委譲されている（own が処理すべき）"
            );
        }
        assert!(
            inner.calls().is_empty(),
            "inner へ到達してはならない: {:?}",
            inner.calls()
        );

        // merge 後も 1 件（own 優先で dedup）。
        let inner2: Arc<dyn GatewayActions> = Arc::new(RecordingInner::new(&[
            "update_heartbeat_instructions",
            "read_heartbeat_instructions",
        ]));
        let merged = SystemGatewayActions::merge_definitions(
            SystemGatewayActions::own_definitions(),
            Some(&inner2),
        );
        for name in [
            "update_heartbeat_instructions",
            "read_heartbeat_instructions",
        ] {
            assert_eq!(merged.iter().filter(|d| d.name == name).count(), 1);
        }
    }

    // ================================================================================
    // #247 段階 2: エージェント自身のハートビート設定ツール
    // ================================================================================

    /// 境界値を固定した state（下限 300 / 既定 1800）。
    fn heartbeat_state() -> AppState {
        let mut state = crate::test_app_state();
        state.heartbeat_limits = crate::config::HeartbeatLimits {
            default_interval_secs: 1800,
            min_interval_secs: 300,
        };
        state
    }

    /// own 定義に 1 件ずつ露出する（transport の有無に依存しない）。
    /// 引数スキーマに `agent_id` が**無い**ことも固定する — あると「他人を指す経路」ができる。
    #[test]
    fn agent_heartbeat_tools_are_exposed_in_own_definitions() {
        let defs = SystemGatewayActions::own_definitions();
        for name in ["get_my_heartbeat", "set_my_heartbeat"] {
            assert_eq!(
                defs.iter().filter(|d| d.name == name).count(),
                1,
                "{name} は own 定義にちょうど 1 件必要（#247）"
            );
            let props = defs
                .iter()
                .find(|d| d.name == name)
                .unwrap()
                .parameters
                .get("properties")
                .and_then(|p| p.as_object())
                .cloned()
                .unwrap_or_default();
            assert!(
                !props.contains_key("agent_id"),
                "{name} に agent_id を生やしてはならない（対象は常に呼び出し元自身）"
            );
        }
        let set = defs.iter().find(|d| d.name == "set_my_heartbeat").unwrap();
        let props = set.parameters["properties"].as_object().unwrap();
        for key in ["enabled", "interval_secs"] {
            assert!(props.contains_key(key), "missing property: {key}");
        }
    }

    /// **既定は無効**（#240 の反省）。設定したことが無いエージェントは無効で返る。
    #[tokio::test]
    async fn get_my_heartbeat_defaults_to_disabled() {
        let actions = SystemGatewayActions::new(heartbeat_state(), None, None, None);
        let r = actions
            .execute("get_my_heartbeat", &json!({}), &trusted_ctx())
            .await;
        assert!(r.success, "{:?}", r.error);
        assert_eq!(
            r.data.unwrap(),
            json!({
                "agent_id": "agent-x",
                "enabled": false,
                "interval_secs": 1800,
                "configured_interval_secs": null,
                "source": "unset",
                "min_interval_secs": 300,
                "max_interval_secs": 86400,
                "default_interval_secs": 1800,
            })
        );
    }

    /// 有効化 + 間隔の設定が DB に載り、読み出しと一致する。
    #[tokio::test]
    async fn set_my_heartbeat_enables_and_sets_interval() {
        let state = heartbeat_state();
        let actions = SystemGatewayActions::new(state.clone(), None, None, None);
        let r = actions
            .execute(
                "set_my_heartbeat",
                &json!({"enabled": true, "interval_secs": 600}),
                &trusted_ctx(),
            )
            .await;
        assert!(r.success, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["success"], true);
        assert_eq!(data["enabled"], true);
        assert_eq!(data["interval_secs"], 600);
        assert_eq!(data["configured_interval_secs"], 600);
        assert_eq!(data["source"], "agent");

        {
            let conn = state.db.lock().unwrap();
            let row = opencrab_db::queries::get_agent_heartbeat_config(&conn, "agent-x")
                .unwrap()
                .unwrap();
            assert!(row.enabled);
            assert_eq!(row.interval_secs, Some(600));
        }

        // 片方だけの更新は、もう片方を保つ。
        let r = actions
            .execute(
                "set_my_heartbeat",
                &json!({"enabled": false}),
                &trusted_ctx(),
            )
            .await;
        assert!(r.success, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["enabled"], false);
        assert_eq!(data["configured_interval_secs"], 600);
    }

    /// **下限より短い要求は拒否**する（丸めない）。DB も書き換わらない。
    /// エラーには下限が載るので、同じターンで有効な値に直して呼び直せる。
    #[tokio::test]
    async fn set_my_heartbeat_rejects_interval_below_floor_without_writing() {
        let state = heartbeat_state();
        let actions = SystemGatewayActions::new(state.clone(), None, None, None);
        let r = actions
            .execute(
                "set_my_heartbeat",
                &json!({"enabled": true, "interval_secs": 1}),
                &trusted_ctx(),
            )
            .await;
        assert!(!r.success);
        assert_eq!(
            r.error.as_deref(),
            Some("interval_secsが短すぎます（最小300秒。指定値: 1秒）")
        );
        {
            let conn = state.db.lock().unwrap();
            assert_eq!(
                opencrab_db::queries::get_agent_heartbeat_config(&conn, "agent-x").unwrap(),
                None,
                "拒否したのに行を作ってはならない（有効化も起きない）"
            );
        }

        // 上限超え・非正整数も同様に拒否。
        let r = actions
            .execute(
                "set_my_heartbeat",
                &json!({"interval_secs": 86_401}),
                &trusted_ctx(),
            )
            .await;
        assert_eq!(
            r.error.as_deref(),
            Some("interval_secsが長すぎます（最大86400秒。指定値: 86401秒）")
        );
        let r = actions
            .execute(
                "set_my_heartbeat",
                &json!({"interval_secs": 0}),
                &trusted_ctx(),
            )
            .await;
        assert_eq!(
            r.error.as_deref(),
            Some("interval_secsは正の整数（秒）で指定してください")
        );
    }

    /// 引数が空 / 型違いは明示エラー（黙って no-op にしない）。
    #[tokio::test]
    async fn set_my_heartbeat_bad_args() {
        let actions = SystemGatewayActions::new(heartbeat_state(), None, None, None);
        let r = actions
            .execute("set_my_heartbeat", &json!({}), &trusted_ctx())
            .await;
        assert_eq!(
            r.error.as_deref(),
            Some("enabled か interval_secs のどちらかが必要です")
        );
        let r = actions
            .execute(
                "set_my_heartbeat",
                &json!({"enabled": "yes"}),
                &trusted_ctx(),
            )
            .await;
        assert_eq!(
            r.error.as_deref(),
            Some("enabledは真偽値で指定してください")
        );
    }

    /// **「自分のだけ」の保証**: `agent_id` を渡しても他人の設定は動かず、明示エラーになる。
    /// 対象は常に `ctx.agent_id`。
    #[tokio::test]
    async fn set_my_heartbeat_cannot_target_another_agent() {
        let state = heartbeat_state();
        // 別エージェントの設定を先に作っておく（有効・900 秒）。
        {
            let conn = state.db.lock().unwrap();
            opencrab_db::queries::upsert_agent_heartbeat_config(
                &conn,
                &opencrab_db::queries::AgentHeartbeatConfigRow {
                    agent_id: "victim".to_string(),
                    enabled: true,
                    interval_secs: Some(900),
                },
            )
            .unwrap();
        }
        let actions = SystemGatewayActions::new(state.clone(), None, None, None);

        for key in ["agent_id", "target_agent_id", "agent"] {
            let r = actions
                .execute(
                    "set_my_heartbeat",
                    &json!({key: "victim", "enabled": false, "interval_secs": 600}),
                    &trusted_ctx(),
                )
                .await;
            assert!(!r.success, "{key} は拒否されるべき");
            assert_eq!(
                r.error.as_deref(),
                Some(
                    format!("{key}は指定できません（このツールは呼び出し元エージェント自身の設定だけを扱います）")
                        .as_str()
                )
            );
            // 読み出しも同じ扱い（他人の設定を覗く経路にしない）。
            let r = actions
                .execute("get_my_heartbeat", &json!({key: "victim"}), &trusted_ctx())
                .await;
            assert!(!r.success, "{key} は読み出しでも拒否されるべき");
        }

        let conn = state.db.lock().unwrap();
        let victim = opencrab_db::queries::get_agent_heartbeat_config(&conn, "victim")
            .unwrap()
            .unwrap();
        assert!(victim.enabled, "他エージェントの設定が変わってはならない");
        assert_eq!(victim.interval_secs, Some(900));
        assert_eq!(
            opencrab_db::queries::get_agent_heartbeat_config(&conn, "agent-x").unwrap(),
            None,
            "呼び出し元の設定も作られない（拒否なので）"
        );
    }

    /// 素の agent（未信頼の外部ユーザー由来のターン）は読み書きとも拒否。
    /// owner は許可（= エージェント自身が heartbeat / ダッシュボードから触れる）。
    #[tokio::test]
    async fn agent_heartbeat_tools_reject_plain_agent_but_allow_owner() {
        let actions = SystemGatewayActions::new(heartbeat_state(), None, None, None);
        for name in ["get_my_heartbeat", "set_my_heartbeat"] {
            let r = actions
                .execute(name, &json!({"enabled": true}), &agent_ctx())
                .await;
            assert!(!r.success, "{name} は素の agent から実行できてはならない");
            assert_eq!(
                r.error.as_deref(),
                Some("このアクションは信頼済みの呼び出し元のみ実行できます")
            );
        }
        let r = actions
            .execute("set_my_heartbeat", &json!({"enabled": true}), &owner_ctx())
            .await;
        assert!(r.success, "{:?}", r.error);
    }

    /// 可視性でもゲートされる（#45 の「可視性 == 強制」）。owner 限定にはしない。
    #[test]
    fn agent_heartbeat_tools_are_trusted_only_but_not_owner_only() {
        for name in ["get_my_heartbeat", "set_my_heartbeat"] {
            assert!(
                opencrab_actions::TRUSTED_ONLY_ACTIONS.contains(&name),
                "{name} は trusted 限定"
            );
            assert!(
                !opencrab_actions::OWNER_ONLY_ACTIONS.contains(&name),
                "{name} を owner 限定にしてはならない（#247 の目的が自己設定）"
            );
        }
        // 指示文はオーナー限定のまま（開放しない）。
        assert!(opencrab_actions::OWNER_ONLY_ACTIONS.contains(&"update_heartbeat_instructions"));
    }

    // ---- #156 S3: A2UI 送信（send_ui）の gateway 非依存化 ----

    /// A2UI 描画面を提供する inner のフェイク（Discord の代役）。
    struct A2uiProvidingInner {
        surface: Arc<opencrab_core::a2ui::A2uiSurface>,
        calls: std::sync::Mutex<Vec<String>>,
    }

    struct NoopRenderer;

    #[async_trait]
    impl opencrab_core::a2ui::UiRenderer for NoopRenderer {
        async fn render(
            &self,
            _surface_id: &str,
            _components: &[opencrab_core::a2ui::A2uiComponent],
            channel: &opencrab_core::a2ui::RenderTarget,
        ) -> Result<opencrab_core::a2ui::RenderedMessage, opencrab_core::a2ui::RenderError>
        {
            Ok(opencrab_core::a2ui::RenderedMessage {
                platform: channel.platform.clone(),
                message_id: Some("m1".into()),
                channel_id: channel.channel_id.clone(),
            })
        }
        async fn update_on_response(
            &self,
            _rendered: &opencrab_core::a2ui::RenderedMessage,
            _response: &opencrab_core::a2ui::UserActionResponse,
        ) -> Result<(), opencrab_core::a2ui::RenderError> {
            Ok(())
        }
        async fn update_on_timeout(
            &self,
            _rendered: &opencrab_core::a2ui::RenderedMessage,
        ) -> Result<(), opencrab_core::a2ui::RenderError> {
            Ok(())
        }
    }

    struct CountingUiSink(std::sync::Mutex<usize>);

    impl opencrab_core::a2ui::UiResponseSink for CountingUiSink {
        fn on_ui_response(&self, _ev: opencrab_core::a2ui::UiResponseEvent) {
            *self.0.lock().unwrap() += 1;
        }
    }

    impl A2uiProvidingInner {
        fn new(owner_id: &str) -> Self {
            Self {
                surface: Arc::new(opencrab_core::a2ui::A2uiSurface {
                    renderer: Arc::new(NoopRenderer),
                    platform: "fake".to_string(),
                    owner_id: owner_id.to_string(),
                    pending: Some(opencrab_core::a2ui::PendingUiSurface {
                        registry: Arc::new(dashmap::DashMap::new()),
                        sink: Arc::new(CountingUiSink(std::sync::Mutex::new(0))),
                    }),
                }),
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl GatewayActions for A2uiProvidingInner {
        fn definitions(&self) -> Vec<GatewayActionDef> {
            // transport 側は `send_ui` を**定義しない**（移設済み）。
            vec![GatewayActionDef {
                name: "fake_transport_tool".to_string(),
                description: "x".to_string(),
                parameters: json!({"type": "object"}),
            }]
        }
        async fn execute(
            &self,
            name: &str,
            _args: &Value,
            _ctx: &GatewayCallContext,
        ) -> GatewayActionResult {
            self.calls.lock().unwrap().push(name.to_string());
            GatewayActionResult {
                success: true,
                data: Some(json!({ "reached_inner": name })),
                error: None,
            }
        }
        fn a2ui_surface(&self) -> Option<Arc<opencrab_core::a2ui::A2uiSurface>> {
            Some(self.surface.clone())
        }
    }

    /// 分類の網羅性検査が見る**全量**（`own_definitions`）に `send_ui` が 1 件だけある。
    /// 消すと `SERVER_INLINE_ACTIONS` の死名検出と分類ガードが空振りする。
    #[test]
    fn send_ui_is_exposed_in_own_definitions() {
        let defs = SystemGatewayActions::own_definitions();
        assert_eq!(
            defs.iter().filter(|d| d.name == "send_ui").count(),
            1,
            "send_ui must be defined exactly once in own_definitions"
        );
    }

    /// **移設の本題**: transport 固有の gateway が Discord でなくても、A2UI 描画面を
    /// 提供すれば `send_ui` が露出し、実体（gateway 非依存層）が動く。
    #[tokio::test]
    async fn send_ui_works_for_any_transport_that_provides_a_surface() {
        let state = crate::test_app_state();
        let inner = Arc::new(A2uiProvidingInner::new("owner-1"));
        let actions = SystemGatewayActions::new(state.clone(), Some(inner.clone()), None, None);

        let names: Vec<String> = actions.definitions().into_iter().map(|d| d.name).collect();
        assert!(names.contains(&"send_ui".to_string()), "{names:?}");

        let ctx = GatewayCallContext::new(GatewayCaller::Owner, "agent-x")
            .with_session_id("fake-session-1");
        let r = actions
            .execute(
                "send_ui",
                &json!({
                    "channel_id": "42",
                    "components": [{"id": "t", "component": "Text", "text": "hi"}],
                }),
                &ctx,
            )
            .await;
        assert!(r.success, "{:?}", r.error);
        let interaction_id = r.data.unwrap()["interaction_id"]
            .as_str()
            .unwrap()
            .to_string();

        // 保留状態は transport の描画面の登録簿に載る（コアの型だけ）。
        let surface = inner.a2ui_surface().unwrap();
        let pending = surface.pending.as_ref().unwrap();
        let entry = pending.registry.get(&interaction_id).expect("registered");
        assert_eq!(entry.target.channel_id, "42");
        assert_eq!(entry.target.platform, "fake");
        // オーナー限定ゲートの識別子が空文字にならない（空だと誰でも操作できてしまう）。
        assert_eq!(entry.owner_id, "owner-1");

        // **inner へ委譲していない**（own が唯一の実装）。
        assert!(
            !inner.calls.lock().unwrap().iter().any(|c| c == "send_ui"),
            "send_ui must not be delegated to inner: {:?}",
            inner.calls.lock().unwrap()
        );
    }

    /// 描画面を持たない transport（web / Nostr / REST / heartbeat）のターンでは
    /// **露出しない**（移設前の露出範囲＝Discord 経路のみ、と一致させる）。
    /// 名前で呼ばれても inner へ落とさず明示エラー（fail-closed）。
    #[tokio::test]
    async fn send_ui_is_hidden_and_refused_without_a_surface() {
        let state = crate::test_app_state();
        // inner なし（web / REST / Nostr / heartbeat、Discord feature 無効ビルド）。
        let actions = SystemGatewayActions::new(state.clone(), None, None, None);
        let names: Vec<String> = actions.definitions().into_iter().map(|d| d.name).collect();
        assert!(!names.contains(&"send_ui".to_string()), "{names:?}");

        let ctx = GatewayCallContext::new(GatewayCaller::Owner, "agent-x")
            .with_session_id("web-session-1");
        let r = actions
            .execute(
                "send_ui",
                &json!({"channel_id": "1", "components": []}),
                &ctx,
            )
            .await;
        assert!(!r.success);
        assert_eq!(
            r.error.unwrap(),
            "send_ui はこのゲートウェイでは利用できません（UI を描画できません）"
        );

        // A2UI を提供しない inner を挟んでも同じ（inner へ委譲しない）。
        let inner = Arc::new(RecordingInner::new(&["some_transport_tool"]));
        let actions = SystemGatewayActions::new(state, Some(inner.clone()), None, None);
        let names: Vec<String> = actions.definitions().into_iter().map(|d| d.name).collect();
        assert!(!names.contains(&"send_ui".to_string()), "{names:?}");
        let r = actions
            .execute(
                "send_ui",
                &json!({"channel_id": "1", "components": []}),
                &ctx,
            )
            .await;
        assert!(!r.success);
        assert!(
            !inner.calls().iter().any(|c| c == "send_ui"),
            "must not fall through to inner: {:?}",
            inner.calls()
        );
    }

    /// **sub-engine からの遮断**（移設前は Discord 側テストが固定していた不変条件）。
    ///
    /// 許可リスト（`SUB_ENGINE_ALLOWED_ACTIONS`）に無いので、合成 gateway が
    /// `send_ui` を露出していても depth >= 1 では一覧に出ず、名前指定でも
    /// 権限拒否（`rejected:` マーカー）になる。
    #[tokio::test]
    async fn send_ui_is_blocked_in_sub_engine() {
        let state = crate::test_app_state();
        let transport = Arc::new(A2uiProvidingInner::new("owner-1"));

        // **本番と同じ入れ子の配線**を組む（`crates/server/src/process.rs`）:
        //   depth0: SystemGatewayActions(inner = transport)             ← 親ターン
        //   spawn_subtask が ctx.root_gateway = depth0 の合成 gateway を子へ渡す
        //   depth1: SystemGatewayActions(inner = depth0 の合成 gateway) ← 子ターン
        //           を SubEngineGatewayActions で包む
        // 1 段構成で組むと、内側の合成 gateway が描画面を転送できているかを検出できない。
        let depth0: Arc<dyn GatewayActions> = Arc::new(SystemGatewayActions::new(
            state.clone(),
            Some(transport),
            None,
            None,
        ));
        // 親ターンでは露出する（前提の確認）。
        assert!(depth0.definitions().iter().any(|d| d.name == "send_ui"));

        let depth1: Arc<dyn GatewayActions> = Arc::new(SystemGatewayActions::new(
            state,
            Some(depth0.clone()),
            None,
            None,
        ));
        // 描画面が入れ子の内側まで届いている（届かないと下の拒否分類が
        // 「Unknown gateway action」へ変わる）。
        assert!(
            depth1.definitions().iter().any(|d| d.name == "send_ui"),
            "A2UI 描画面が入れ子の合成 gateway へ転送されていない"
        );

        let sub = opencrab_actions::SubEngineGatewayActions::new(depth1);
        let names: Vec<String> = sub.definitions().into_iter().map(|d| d.name).collect();
        assert!(
            !names.contains(&"send_ui".to_string()),
            "send_ui must NOT be exposed to the sub-engine: {names:?}"
        );

        let r = sub
            .execute(
                "send_ui",
                &json!({"channel_id": "1", "components": []}),
                &sub_ctx("subtask-s1"),
            )
            .await;
        assert!(!r.success, "send_ui must be blocked in the sub-engine");
        // 移設前と同じ分類（実在するが許可外 = 権限拒否）。「そんなツールは無い」に
        // 落ちると幻覚ツール名と同じ扱いになり、拒否の観測が変わる。
        let err = r.error.as_deref().unwrap();
        assert!(
            err.starts_with(opencrab_actions::REJECTION_CODE_PREFIX),
            "send_ui must be a policy rejection, not an unknown tool: {err}"
        );
        assert!(
            !err.contains("Unknown gateway action"),
            "分類が「そんなツールは無い」へ退行している: {err}"
        );

        // 多層防御: 名前ベースの depth 拒否リストにも残っている。
        assert!(opencrab_actions::DISCORD_ACTIONS.contains(&"send_ui"));
        assert!(opencrab_actions::tool_policy("send_ui").blocked_in_subengine);
    }

    /// `send_ui` は inline（配送系 + ユーザー応答待ち）。分類の所属は移設前と同じ。
    #[test]
    fn send_ui_stays_inline_after_the_move() {
        assert!(opencrab_actions::default_non_dispatch_tools().contains("send_ui"));
        assert!(opencrab_actions::SERVER_INLINE_ACTIONS.contains(&"send_ui"));
        assert!(!opencrab_actions::DISCORD_INLINE_ACTIONS.contains(&"send_ui"));
        assert!(!opencrab_actions::SERVER_DISPATCHABLE_ACTIONS.contains(&"send_ui"));
        assert!(!opencrab_actions::DISCORD_DISPATCHABLE_ACTIONS.contains(&"send_ui"));
    }

    // ---- #157 S7: ピアレビュー依頼（request_peer_review）の gateway 非依存化 ----

    /// 素テキスト配送口を提供する inner のフェイク（Discord の代役）。
    struct DeliveryProvidingInner {
        delivery: Arc<FakeTextDelivery>,
        calls: std::sync::Mutex<Vec<String>>,
        /// true なら `request_peer_review` を**再定義**する（negative assert 用）。
        redefines_peer_review: bool,
    }

    /// 送信を記録するだけの [`TextDelivery`]。Discord と同じ規約
    /// （数値宛先 / `<@id>` / 1900 chars）を模す。
    #[derive(Default)]
    struct FakeTextDelivery {
        sent: std::sync::Mutex<Vec<(String, String)>>,
    }

    #[async_trait]
    impl opencrab_core::text_delivery::TextDelivery for FakeTextDelivery {
        fn validate_target(&self, target: &str) -> Result<(), String> {
            if target.parse::<u64>().is_ok() {
                Ok(())
            } else {
                Err(format!("無効なchannel_id: {target}"))
            }
        }
        fn mention(&self, user_id: &str) -> String {
            format!("<@{user_id}>")
        }
        fn chunk_limit(&self) -> usize {
            1900
        }
        async fn send_text(&self, target: &str, text: &str) -> Result<(), String> {
            self.sent
                .lock()
                .unwrap()
                .push((target.to_string(), text.to_string()));
            Ok(())
        }
    }

    impl DeliveryProvidingInner {
        fn new() -> Self {
            Self {
                delivery: Arc::new(FakeTextDelivery::default()),
                calls: std::sync::Mutex::new(Vec::new()),
                redefines_peer_review: false,
            }
        }
        /// transport が誤って移設済みツールを再定義した構成。
        fn redefining() -> Self {
            Self {
                redefines_peer_review: true,
                ..Self::new()
            }
        }
    }

    #[async_trait]
    impl GatewayActions for DeliveryProvidingInner {
        fn definitions(&self) -> Vec<GatewayActionDef> {
            // transport 側は `request_peer_review` を**定義しない**（移設済み）。
            let mut defs = vec![GatewayActionDef {
                name: "fake_transport_tool".to_string(),
                description: "x".to_string(),
                parameters: json!({"type": "object"}),
            }];
            if self.redefines_peer_review {
                defs.push(GatewayActionDef {
                    name: "request_peer_review".to_string(),
                    description: "transport の古い実装".to_string(),
                    parameters: json!({"type": "object"}),
                });
            }
            defs
        }
        async fn execute(
            &self,
            name: &str,
            _args: &Value,
            _ctx: &GatewayCallContext,
        ) -> GatewayActionResult {
            self.calls.lock().unwrap().push(name.to_string());
            GatewayActionResult {
                success: true,
                data: Some(json!({ "reached_inner": name })),
                error: None,
            }
        }
        fn text_delivery(&self) -> Option<Arc<dyn opencrab_core::text_delivery::TextDelivery>> {
            Some(self.delivery.clone())
        }
    }

    /// 分類の網羅性検査が見る**全量**（`own_definitions`）に `request_peer_review` が
    /// 1 件だけある。消すと `SERVER_INLINE_ACTIONS` の死名検出と分類ガードが空振りする。
    #[test]
    fn request_peer_review_is_exposed_in_own_definitions() {
        let defs = SystemGatewayActions::own_definitions();
        assert_eq!(
            defs.iter()
                .filter(|d| d.name == "request_peer_review")
                .count(),
            1,
            "request_peer_review must be defined exactly once in own_definitions"
        );
    }

    /// **移設の本題（#157）**: Discord 無効の構成（`inner = None` / web・REST・heartbeat・
    /// Nostr のターン）でも `request_peer_review` が**定義に現れる**。
    ///
    /// `send_ui`（描画面が無いと露出しない）とはここが違う: 配送口が無いのは
    /// 「送れない」だけで、ツールの存在自体を transport の有無に依存させない。
    #[test]
    fn request_peer_review_is_defined_even_without_discord() {
        let state = crate::test_app_state();
        let actions = SystemGatewayActions::new(state, None, None, None);
        let names: Vec<String> = actions.definitions().into_iter().map(|d| d.name).collect();
        assert!(
            names.contains(&"request_peer_review".to_string()),
            "Discord 無効の構成でも定義に出ること: {names:?}"
        );
    }

    /// **移設の本題**: transport が Discord でなくても、素テキストの配送口を提供すれば
    /// 依頼が実際に投稿される（ヘッダ + part X/N）。
    #[tokio::test]
    async fn request_peer_review_works_for_any_transport_that_provides_delivery() {
        let state = crate::test_app_state();
        let inner = Arc::new(DeliveryProvidingInner::new());
        let actions = SystemGatewayActions::new(state, Some(inner.clone()), None, None);

        let ctx = GatewayCallContext::new(GatewayCaller::Owner, "agent-x")
            .with_session_id("fake-session-1");
        let r = actions
            .execute(
                "request_peer_review",
                &json!({"content": "raw diff", "channel_id": "555"}),
                &ctx,
            )
            .await;
        assert!(r.success, "{:?}", r.error);
        let data = r.data.unwrap();
        assert_eq!(data["channel_id"], "555");
        assert_eq!(data["parts"], 1);
        assert_eq!(
            data["message"],
            "ピアレビュー依頼を投稿しました。[Peer Review] で始まる返信を待ってください。"
        );

        // ヘッダ + part 1/1 の 2 通が配送口へ出た。
        let sent = inner.delivery.sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].0, "555");
        assert!(sent[0].1.starts_with("[Peer Review Request] from agent-x"));
        assert_eq!(sent[1].1, "part 1/1\nraw diff");

        // **inner へ委譲していない**（own が唯一の実装）。
        assert!(
            !inner
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c == "request_peer_review"),
            "request_peer_review must not be delegated to inner: {:?}",
            inner.calls.lock().unwrap()
        );
    }

    /// 宛先の妥当性判定と文言は transport（配送口）の責務。移設前の
    /// `無効なchannel_id: …` がそのまま返る。
    #[tokio::test]
    async fn invalid_target_error_comes_from_the_transport() {
        let state = crate::test_app_state();
        let inner = Arc::new(DeliveryProvidingInner::new());
        let actions = SystemGatewayActions::new(state, Some(inner.clone()), None, None);
        let ctx = GatewayCallContext::new(GatewayCaller::Owner, "agent-x")
            .with_session_id("fake-session-1");
        let r = actions
            .execute(
                "request_peer_review",
                &json!({"content": "diff", "channel_id": "not-a-number"}),
                &ctx,
            )
            .await;
        assert!(!r.success);
        assert_eq!(r.error.unwrap(), "無効なchannel_id: not-a-number");
        // 1 通も出していない（fail-closed）。
        assert!(inner.delivery.sent.lock().unwrap().is_empty());
    }

    /// 配送口を持たない transport では**定義には出るが実行は明示エラー**（fail-closed）。
    /// 黙って inner へ落とさない。
    #[tokio::test]
    async fn request_peer_review_is_refused_without_a_delivery() {
        let state = crate::test_app_state();
        let ctx = GatewayCallContext::new(GatewayCaller::Owner, "agent-x")
            .with_session_id("web-session-1");

        // inner なし。
        let actions = SystemGatewayActions::new(state.clone(), None, None, None);
        let r = actions
            .execute(
                "request_peer_review",
                &json!({"content": "diff", "channel_id": "1"}),
                &ctx,
            )
            .await;
        assert!(!r.success);
        // 既存の 8 種のエラー文言は変えていない。ここは移設で新設した文言で、
        // 共有プロンプトが全ターンでレビュー依頼を促すため**次の行動**まで書く。
        assert_eq!(
            r.error.unwrap(),
            "request_peer_review はこのゲートウェイでは利用できません（メッセージを送信できません）。\
             このターンの transport はテキストを送れないため、ピアレビュー依頼は省略して先へ進んでよい。"
        );

        // 配送口を提供しない inner を挟んでも同じ（inner へ委譲しない）。
        let inner = Arc::new(RecordingInner::new(&["some_transport_tool"]));
        let actions = SystemGatewayActions::new(state, Some(inner.clone()), None, None);
        let names: Vec<String> = actions.definitions().into_iter().map(|d| d.name).collect();
        assert!(
            names.contains(&"request_peer_review".to_string()),
            "{names:?}"
        );
        let r = actions
            .execute(
                "request_peer_review",
                &json!({"content": "diff", "channel_id": "1"}),
                &ctx,
            )
            .await;
        assert!(!r.success);
        assert!(
            !inner.calls().iter().any(|c| c == "request_peer_review"),
            "must not fall through to inner: {:?}",
            inner.calls()
        );
    }

    /// **sub-engine からの遮断**（移設前は Discord 側テストが固定していた不変条件）。
    ///
    /// 許可リスト（`SUB_ENGINE_ALLOWED_ACTIONS`）に無いので、合成 gateway が
    /// `request_peer_review` を露出していても depth >= 1 では一覧に出ず、名前指定でも
    /// 権限拒否（`rejected:` マーカー）になる。
    #[tokio::test]
    async fn request_peer_review_is_blocked_in_sub_engine() {
        let state = crate::test_app_state();
        let transport = Arc::new(DeliveryProvidingInner::new());

        // 本番と同じ入れ子の配線（`crates/server/src/process.rs`）。
        let depth0: Arc<dyn GatewayActions> = Arc::new(SystemGatewayActions::new(
            state.clone(),
            Some(transport),
            None,
            None,
        ));
        assert!(depth0
            .definitions()
            .iter()
            .any(|d| d.name == "request_peer_review"));
        // 配送口が入れ子の内側まで転送されている（能力を黙って落とさない）。
        assert!(depth0.text_delivery().is_some());

        let depth1: Arc<dyn GatewayActions> = Arc::new(SystemGatewayActions::new(
            state,
            Some(depth0.clone()),
            None,
            None,
        ));
        assert!(depth1.text_delivery().is_some());

        let sub = opencrab_actions::SubEngineGatewayActions::new(depth1);
        let names: Vec<String> = sub.definitions().into_iter().map(|d| d.name).collect();
        assert!(
            !names.contains(&"request_peer_review".to_string()),
            "request_peer_review must NOT be exposed to the sub-engine: {names:?}"
        );

        let r = sub
            .execute(
                "request_peer_review",
                &json!({"content": "diff", "channel_id": "1"}),
                &sub_ctx("subtask-s1"),
            )
            .await;
        assert!(!r.success);
        // 移設前と同じ分類（実在するが許可外 = 権限拒否）。
        let err = r.error.as_deref().unwrap();
        assert!(
            err.starts_with(opencrab_actions::REJECTION_CODE_PREFIX),
            "request_peer_review must be a policy rejection: {err}"
        );
        assert!(
            !err.contains("Unknown gateway action"),
            "分類が「そんなツールは無い」へ退行している: {err}"
        );

        // 多層防御: 名前ベースの depth 拒否リストにも残っている。
        assert!(opencrab_actions::DISCORD_ACTIONS.contains(&"request_peer_review"));
        assert!(opencrab_actions::tool_policy("request_peer_review").blocked_in_subengine);
    }

    /// `request_peer_review` は inline（配送系）。分類の所属は移設前と同じ。
    #[test]
    fn request_peer_review_stays_inline_after_the_move() {
        assert!(opencrab_actions::default_non_dispatch_tools().contains("request_peer_review"));
        assert!(opencrab_actions::SERVER_INLINE_ACTIONS.contains(&"request_peer_review"));
        assert!(!opencrab_actions::DISCORD_INLINE_ACTIONS.contains(&"request_peer_review"));
        assert!(!opencrab_actions::SERVER_DISPATCHABLE_ACTIONS.contains(&"request_peer_review"));
        assert!(!opencrab_actions::DISCORD_DISPATCHABLE_ACTIONS.contains(&"request_peer_review"));
    }

    /// **negative assert（#157 S7）**: transport（Discord）が `request_peer_review` を
    /// 再定義しても own が処理する（委譲パターンにしない）。
    ///
    /// 委譲のままにすると、dedup（own 優先）で定義は own に食われるのに実行は transport の
    /// 古い実装へ流れ、レビュアー解決や台帳記録が黙ってバイパスされる。
    #[tokio::test]
    async fn own_handles_request_peer_review_even_if_the_transport_redefines_it() {
        let state = crate::test_app_state();
        let inner = Arc::new(DeliveryProvidingInner::redefining());
        let actions = SystemGatewayActions::new(state, Some(inner.clone()), None, None);

        // 定義は 1 件だけ（own 優先の dedup）。
        let defs = actions.definitions();
        assert_eq!(
            defs.iter()
                .filter(|d| d.name == "request_peer_review")
                .count(),
            1
        );

        let ctx = GatewayCallContext::new(GatewayCaller::Owner, "agent-x")
            .with_session_id("fake-session-1");
        let r = actions
            .execute(
                "request_peer_review",
                &json!({"content": "diff", "channel_id": "7"}),
                &ctx,
            )
            .await;
        assert!(r.success, "{:?}", r.error);
        // own の実装が動いた証拠: 配送口へヘッダ + part が出て、inner の execute は
        // 呼ばれていない。
        assert_eq!(inner.delivery.sent.lock().unwrap().len(), 2);
        assert!(
            !inner
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c == "request_peer_review"),
            "own must not delegate: {:?}",
            inner.calls.lock().unwrap()
        );
    }

    // ------------------------------------------------------------------
    // #268: nostr_run 薄い passthrough（server-own / TRUSTED_ONLY）
    // ------------------------------------------------------------------

    /// `nostr_run` の委譲先を検証する fake passthrough capability。
    /// 呼ばれた (agent_id, subcommand, args) を記録し、固定文字列 or エラーを返す。
    #[derive(Default)]
    struct RecordingPassthrough {
        calls: std::sync::Mutex<Vec<(String, String, Vec<String>)>>,
        fail: bool,
    }

    #[async_trait]
    impl opencrab_actions::GatewayNostrPassthrough for RecordingPassthrough {
        async fn run(
            &self,
            agent_id: &str,
            subcommand: &str,
            args: &[String],
        ) -> anyhow::Result<String> {
            self.calls.lock().unwrap().push((
                agent_id.to_string(),
                subcommand.to_string(),
                args.to_vec(),
            ));
            if self.fail {
                anyhow::bail!("passthrough boom");
            }
            Ok(format!("ran {subcommand}"))
        }
    }

    /// NOSTR 種別で `nostr_passthrough` capability だけを提供する fake gateway。
    struct FakeNostrGateway {
        passthrough: Arc<RecordingPassthrough>,
    }

    #[async_trait]
    impl opencrab_actions::AgentGatewayLifecycle for FakeNostrGateway {
        fn kind(&self) -> &'static str {
            opencrab_actions::gateway_kinds::NOSTR
        }
        async fn start(&self, _agent_id: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn stop(&self, _agent_id: &str) {}
        fn is_running(&self, _agent_id: &str) -> bool {
            false
        }
        async fn restore_all(&self) {}
        async fn shutdown_all(&self) {}
        fn nostr_passthrough(&self) -> Option<Arc<dyn opencrab_actions::GatewayNostrPassthrough>> {
            Some(self.passthrough.clone())
        }
    }

    fn register_fake_nostr(state: &AppState, fail: bool) -> Arc<RecordingPassthrough> {
        let passthrough = Arc::new(RecordingPassthrough {
            fail,
            ..Default::default()
        });
        state.gateways.register(Arc::new(FakeNostrGateway {
            passthrough: passthrough.clone(),
        }));
        passthrough
    }

    /// `nostr_run` は own（全 trusted ターンで露出）かつ TRUSTED_ONLY（caller=Agent 不可）。
    /// 分類は inline（同ターンで結果を使う / 送信は送ること自体が応答）。
    #[test]
    fn nostr_run_is_own_trusted_only_and_inline() {
        let names: Vec<String> = SystemGatewayActions::own_definitions()
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert!(
            names.contains(&"nostr_run".to_string()),
            "nostr_run は own 定義（全 trusted ターンで露出）でなければならない"
        );
        // 可視性 == 実行時強制（#45）: caller=Agent には出さない・実行させない。
        let policy = opencrab_actions::tool_policy("nostr_run");
        assert!(policy.trusted_only, "nostr_run は TRUSTED_ONLY");
        assert!(!policy.owner_only, "owner 限定ではない（trusted なら可）");
        // 分類は inline（dispatch 対象外）。
        assert!(
            opencrab_actions::default_non_dispatch_tools().contains("nostr_run"),
            "nostr_run は inline（同ターン結果依存 / 配送系）"
        );
    }

    /// caller=Agent（外部ユーザー由来の会話）では `nostr_run` を露出しない。
    #[test]
    fn nostr_run_is_hidden_from_untrusted_caller() {
        assert!(opencrab_actions::TRUSTED_ONLY_ACTIONS.contains(&"nostr_run"));
        // owner / trusted は可、素の Agent は不可、を tool_policy が表す。
        assert!(opencrab_actions::tool_policy("nostr_run").trusted_only);
    }

    /// 稼働中（登録済み）の Nostr passthrough capability へ、ctx.agent_id・subcommand・args
    /// をそのまま委譲する。
    #[tokio::test]
    async fn nostr_run_delegates_to_capability() {
        let state = crate::test_app_state();
        let rec = register_fake_nostr(&state, false);
        let actions = SystemGatewayActions::new(state, None, None, None);
        let ctx = GatewayCallContext::new(GatewayCaller::Owner, "agent-268");

        let r = actions
            .execute(
                "nostr_run",
                &json!({
                    "subcommand": "event",
                    "args": ["--kind", "0", "hello; rm -rf /"]
                }),
                &ctx,
            )
            .await;
        assert!(r.success, "error: {:?}", r.error);
        assert_eq!(r.data.unwrap()["result"], "ran event");

        let calls = rec.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let (agent, sub, args) = &calls[0];
        assert_eq!(agent, "agent-268", "config は常に ctx.agent_id のもの");
        assert_eq!(sub, "event");
        assert_eq!(
            args,
            &vec![
                "--kind".to_string(),
                "0".to_string(),
                "hello; rm -rf /".to_string()
            ],
            "args は 1 要素ずつそのまま渡る（注入されない）"
        );
    }

    /// Nostr 未構成（capability 未登録）なら明示エラー（inner へ黙って落とさない）。
    #[tokio::test]
    async fn nostr_run_errors_when_nostr_not_configured() {
        let state = crate::test_app_state();
        let actions = SystemGatewayActions::new(state, None, None, None);
        let ctx = GatewayCallContext::new(GatewayCaller::Owner, "agent-x");
        let r = actions
            .execute("nostr_run", &json!({"subcommand": "post"}), &ctx)
            .await;
        assert!(!r.success);
        assert!(r.error.unwrap().contains("Nostr"));
    }

    /// subcommand 欠落・args 非文字列は capability を呼ばず即エラー。
    #[tokio::test]
    async fn nostr_run_validates_args() {
        let state = crate::test_app_state();
        let rec = register_fake_nostr(&state, false);
        let actions = SystemGatewayActions::new(state, None, None, None);
        let ctx = GatewayCallContext::new(GatewayCaller::Owner, "agent-x");

        // subcommand 無し。
        let r = actions.execute("nostr_run", &json!({}), &ctx).await;
        assert!(!r.success);
        assert!(r.error.unwrap().contains("subcommand"));

        // args に非文字列（数値）。
        let r = actions
            .execute(
                "nostr_run",
                &json!({"subcommand": "event", "args": ["--kind", 0]}),
                &ctx,
            )
            .await;
        assert!(!r.success);
        assert!(r.error.unwrap().contains("文字列"));

        // どちらも capability を呼んでいない。
        assert!(rec.calls.lock().unwrap().is_empty());
    }

    /// capability のエラー（未 materialize / init/watch 拒否 / nostaro 失敗）はそのまま
    /// `nostr_run 失敗:` として伝播する（マスク済みメッセージ）。
    #[tokio::test]
    async fn nostr_run_propagates_capability_error() {
        let state = crate::test_app_state();
        register_fake_nostr(&state, true);
        let actions = SystemGatewayActions::new(state, None, None, None);
        let ctx = GatewayCallContext::new(GatewayCaller::Owner, "agent-x");
        let r = actions
            .execute(
                "nostr_run",
                &json!({"subcommand": "post", "args": ["hi"]}),
                &ctx,
            )
            .await;
        assert!(!r.success);
        let msg = r.error.unwrap();
        assert!(msg.contains("nostr_run 失敗"), "got: {msg}");
        assert!(msg.contains("passthrough boom"), "got: {msg}");
    }
}
