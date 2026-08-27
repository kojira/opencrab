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
    cancel_subtask as neutral_cancel_subtask, steer_subtask as neutral_steer_subtask,
    CancelOutcome, SettleKind, SteerOutcome, SubtaskCompletionSink, SubtaskRegistry,
    SubtaskSettled, REJECTION_CODE_PREFIX,
};
use opencrab_gateway::{GatewayActionDef, GatewayActionResult, GatewayActions, GatewayCallContext};
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
    /// 親ターンが「この run は subtask を起こしたか」を数えるカウンタ（#431）。
    ///
    /// 明示 `spawn_subtask` が**登録簿への登録まで到達した**（＝ `success`）ときだけ
    /// 加算する。auto-dispatch 側（`SubtaskToolDispatcher`）と同一の Arc を共有し、
    /// 親ターンは 1 つの数で両経路を見る。`None`（既定）なら数えない。
    subtask_starts: Option<Arc<std::sync::atomic::AtomicUsize>>,
}

/// `report_progress` が登録簿から引く、進捗通知に要る項目だけの写し。
///
/// 登録簿のエントリ（`SpawnedSubtask`）は shard ロック下でしか読めないので、必要な
/// フィールドをここへ写してからロックを離す。
struct ProgressSubtaskEntry {
    /// 解決済みの subtask ID（引数省略時は session_id からの逆引き結果）。
    subtask_id: String,
    /// subtask 自身のセッション ID（所有権ゲート用）。
    session_id: String,
    /// 親セッション ID（進捗ログと resume の宛先）。
    parent_session_id: String,
    /// **親ターンの呼び出し元**（#298）。進捗デバウンス発火は親会話を resume する
    /// ので、resume 先の権限は元のターンのものでなければならない。
    caller: opencrab_actions::CallerIdentity,
}

impl ProgressSubtaskEntry {
    fn from_entry(subtask_id: String, entry: &opencrab_actions::SpawnedSubtask) -> Self {
        Self {
            subtask_id,
            session_id: entry.session_id.clone(),
            parent_session_id: entry.parent_session_id.clone(),
            caller: entry.caller.clone(),
        }
    }
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
            subtask_starts: None,
        }
    }

    /// 親ターンの subtask 起動カウンタを設定する（#431）。
    ///
    /// `SubtaskToolDispatcher::with_subtask_starts` と**同じ Arc** を渡すこと。
    /// 位置引数ではなく builder にしているのは、この配線を必要とするのが
    /// `run_agent_response` の 1 箇所だけで、他の生成箇所（テスト・単発呼び出し）を
    /// `None` で埋めさせないため。
    pub fn with_subtask_starts(
        mut self,
        counter: Option<Arc<std::sync::atomic::AtomicUsize>>,
    ) -> Self {
        self.subtask_starts = counter;
        self
    }

    /// 本ツール源が直接提供するツール定義（A2UI 描画面がある構成の全量）。
    ///
    /// 各定義は分類属性（`class.dispatch` / `class.sub_engine` / `class.sharing`）を
    /// 名乗る（`ToolClass` に `Default` が無いため構築サイトで必須）。テストや
    /// `agent_heartbeat` の分類検査がこの全量から属性を引くので `pub(crate)`。
    pub(crate) fn own_definitions() -> Vec<GatewayActionDef> {
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
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
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
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
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
                            "description": "list=一覧（設定ファイル由来 + 自分に追加した分の実効リスト）/ add=追加 / remove=削除。"
                        },
                        "command": {
                            "type": "string",
                            "description": "add/remove 対象のコマンド（例: git, cargo）。list では不要。"
                        }
                    },
                    "required": ["action"]
                }),
            },
            #[cfg(feature = "nostr")]
            GatewayActionDef {
                name: "configure_nostr".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description:
                    "自分の Nostr 連携設定（購読リレー・フィルタ authors/keywords/kinds・\
                有効/無効・Nostr でのオーナーの公開鍵）を変更する（owner 限定）。\
                秘密鍵は変更も取得もできない（鍵生成は別手段）。\
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
                            "description": "購読する kind 番号。DM の kind（4 / 1059）は指定しても\
                            無視される（#514: DM は扱わない。private な話は Discord で）。"
                        },
                        "enabled": {
                            "type": "boolean",
                            "description": "有効化して起動 / 無効化して停止。"
                        },
                        "owner_pubkey": {
                            "type": "string",
                            "description": "Nostr でのオーナーの公開鍵（npub1... または 64 桁 hex）。\
                            この鍵から届いたメッセージだけが owner 権限のターンになる。\
                            未設定のうちは Nostr からは誰も owner にならないので、\
                            最初の 1 回は Discord など owner 権限のある経路から設定する。\
                            \"\" を渡すと未設定に戻る。"
                        }
                    }
                }),
            },
            GatewayActionDef {
                name: "configure_self".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
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
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
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
            #[cfg(feature = "nostr")]
            GatewayActionDef {
                name: "nostr_generate_key".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Dispatchable, sub_engine: opencrab_gateway::SubEngineAccess::Allowed, sharing: opencrab_gateway::ToolSharing::AgentBound },
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
            #[cfg(feature = "nostr")]
            GatewayActionDef {
                name: "nostr_list_keys".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
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
            #[cfg(feature = "nostr")]
            GatewayActionDef {
                name: "nostr_switch_identity".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
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
            // 薄い nostaro passthrough（#268）。server-own で、caller による制限は持たない
            // （#303 で `TRUSTED_ONLY_ACTIONS` から外した）。投稿・返信・kind:0 プロフィール
            // 設定・チャンネル・取得など nostaro が持つ操作を**すべて**、あらゆるターン
            // （Nostr 受信ターン = caller=Agent を含む）から使えるようにする（既存の inner
            // `nostr_post`/`reply` は Nostr 受信ターン用にそのまま残る）。opencrab が守るのは
            // 「鍵のエージェント間混同防止（config は ctx.agent_id 固定）」と「nsec 隠蔽」の
            // 2 点だけで、Nostr 仕様の判断は nostaro に委ねる（非劣化）。`init`（鍵作成/上書き）・
            // `watch`（無制限受信）・`relay`（config.toml⇔DB desync で揮発）に加え、#514 で
            // `dm`（DM 送信）を拒否する。`event`（任意 kind publish）は #699 のオーナー裁定で
            // 許可（暗号化を自前で組まない限り DM としては機能しない＝実用迂回にならない）。
            #[cfg(feature = "nostr")]
            GatewayActionDef {
                name: "nostr_run".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "Nostr CLI（nostaro）を薄く passthrough 実行する。`subcommand` に \
                              nostaro のサブコマンド（例: post / reply / zap / upload / \
                              react / repost / follow / unfollow / profile / channel / get / timeline / \
                              search / decode / pubkey など）を、`args` にそのサブコマンドの\
                              フラグと値を**1 要素ずつ**配列で渡す（例: subcommand=\"profile\", \
                              args=[\"--name\",\"…\",\"--about\",\"…\"] でプロフィール(kind:0)を設定）。\
                              投稿・プロフィール(kind:0)設定・チャンネル・取得など nostaro が持つ操作を\
                              使える。署名は**あなた自身の採用済み Nostr 鍵**で行われ、秘密鍵(nsec)は\
                              扱わない・見えない。鍵の作成/採用は nostr_generate_key / \
                              nostr_switch_identity を使うこと（init は不可）。受信の常時監視（watch）は\
                              ここからは起動できない。リレー設定は opencrab 側（configure_nostr / \
                              ダッシュボード）で管理するため relay サブコマンドは不可。\
                              **dm は不可**（#514: DM は扱わない。private な話は Discord の DM か\
                              指定チャンネルで）。event は任意 kind の publish に使える（例: \
                              subcommand=\"event\", args=[\"-k\",\"40\",\"-c\",\"…\"] で\
                              パブリックチャット作成）。\
                              まだ鍵を採用して\
                              いない場合は先に nostr_switch_identity で採用すること。\
                              `timeline` は**フォロー基準**（自分とフォロー中の相手のノートが対象で、\
                              足りない分だけリレーから補われる）。フォローしていない人のノートを\
                              含むリレー全体の新着を見るには `timeline --global` を使う\
                              （件数が多くなるので `--out <相対パス> --out-format json` を併せて\
                              渡せる。`--out-format` は `--out` とセットで指定する）。\
                              `--file` / `--out` などに渡す相対パスは、ws_* / execute_shell と同じ\
                              **あなた自身の workspace** が基準（ws_write で作ったファイルをそのまま\
                              `--file <相対パス>` に渡せるし、`--out <相対パス>` の出力は ws_read で読める）。"
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "subcommand": {
                            "type": "string",
                            "description": "nostaro のサブコマンド（init/watch/relay/dm は不可）。"
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
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
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
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
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
            GatewayActionDef {
                name: "steer_subtask".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "走行中のサブタスクを止めずに追加の指示（steer）を送ります。指示はサブタスクの次の反復の合間に読まれ、以後の判断へ反映されます。送れるのは自分のセッションが親のサブタスクのみ（owner は制限なし）。明示的な spawn_subtask のサブにのみ有効で、auto-dispatch のサブや既に完了/停止したサブへ送った場合はその旨が返ります。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "subtask_id": {
                            "type": "string",
                            "description": "追加指示を送るサブタスクのID（subtask_spawnedイベントから取得）"
                        },
                        "message": {
                            "type": "string",
                            "description": "サブタスクへ送る追加指示（方向転換・条件追加・見落としの伝達など）"
                        }
                    },
                    "required": ["subtask_id", "message"]
                }),
            },
            // 記憶インデックスの全再構築（#175 S4）。Discord gateway 実装だけにあった
            // ものを server-neutral 層へ移す。LLM クライアントを必要とする唯一の
            // Discord ツールだったため、これを移すことで discord crate が LLM を
            // 知らなくなる（#155 の前進）。
            GatewayActionDef {
                name: "rebuild_memory_index".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Dispatchable, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
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
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::Allowed, sharing: opencrab_gateway::ToolSharing::AgentBound },
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
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Dispatchable, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
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
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
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
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "execute_shell で実行できる許可コマンドの一覧（実効リスト）を\
                取得する。設定ファイル由来のものと自分に追加されたものを合わせて返す（#300）。"
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
            },
            GatewayActionDef {
                name: "remove_allowed_command".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
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
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Dispatchable, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
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
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Dispatchable, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
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
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
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
            #[cfg(feature = "nostr")]
            GatewayActionDef {
                name: "get_my_nostr_relay".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "自分（呼び出し元エージェント）の Nostr 受信 → Discord 転記の設定を読み出す。転記が有効か・転記先が設定済みか（転記先 URL は伏字で返す）を返す。他のエージェントの設定は読めない。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {},
                }),
            },
            #[cfg(feature = "nostr")]
            GatewayActionDef {
                name: "set_my_nostr_relay".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
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
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "自分（呼び出し元エージェント）のハートビート設定を、いま話しているセッションについて読み出す。返り値: enabled（有効か）、interval_secs（実効間隔・秒）、next_fire_at（このセッションのハートビートがゲートされていない場合に次に発火する予定時刻。照会した時点で anchor_at と最終発火時刻から算出する値で、UTC の RFC3339 文字列。無効・発火経路なし・間隔が不正などでは null。gated=true のときはこの時刻が来ても実際には発火しない）、anchor_at / last_fired_at（起点と最終発火時刻。同じく UTC RFC3339 か null）、min/max/default_interval_secs（設定できる下限・上限・既定）。設定したことが無ければ無効。有効なのに発火しないときは gated=true と、その理由 gated_reason（例: グローバルのハートビートが無効化されている / 間隔が不正）を返すので、なぜ発火しないのかを自分で把握できる。他のエージェントの設定は読めない。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {}
                }),
            },
            GatewayActionDef {
                name: "set_my_heartbeat".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "自分（呼び出し元エージェント）のハートビート（自律実行）の有効/無効と間隔を、いま話しているセッションに対して設定する。対象は常にこのセッション（Nostr の自発投稿、またはこの Discord チャンネル）で、どこに設定するか選ぶ必要はない。他のエージェントや別のチャンネルの設定は変えられない。間隔には運用者が決めた下限があり、それより短い値は拒否される（丸められない）ので、拒否されたらエラーに載っている下限以上で指定し直すこと。有効にした直後から次回発火時刻が算出され、再起動を待たず即時に反映される。発火タイミングは非対称: 一度も発火していないセッションを初めて有効化したときは間隔をまるごと待つ（今すぐは発火しない）が、既に発火したことがあるセッションの再有効化や間隔の短縮では、前回発火（や起点）＋新しい間隔が既に過ぎていれば直ちに発火する（設定変更で発火の記録は消えない）。今すぐ試したいなら run_my_heartbeat を使う。ハートビートで何をするかの指示文はこのツールでは変えられない（オーナー限定の別ツール）。".to_string(),
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
            // #599: 時間を待たずにハートビートを手動発火する（テスト用・オーナー / co_agent 限定）。
            // 時間発火とまったく同じ経路を通り、last_fired_at は更新しない（時間発火の位相を保つ）。
            GatewayActionDef {
                name: "run_my_heartbeat".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "自分（呼び出し元エージェント）のハートビートを、次の発火時刻を待たずに今すぐ手動で発火する。テストや動作確認に使う（時間発火とまったく同じ経路——宣言→サブタスク→継続→投稿——を通るので、待たずに一連の流れを検証できる）。対象は省略すると「いま話しているセッション」、session_id を渡せばそのセッション。発火先は Discord チャンネルまたは Nostr の自発投稿で、発火経路の無いセッション種別は拒否される。実際のターンは今のターンが終わってから走る（すぐに投げて返る）。time-fire の位相をずらさないため last_fired_at は更新しない（次回の定期発火時刻は変わらない）。オーナーまたは co_agent のみ実行できる。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "session_id": {
                            "type": "string",
                            "description": "発火する対象セッションの session_id（discord-… / nostr-…）。省略すると、いま話しているセッションを発火する。"
                        }
                    }
                }),
            },
            // ---- 定時実行（#455）: ハートビート（固定短間隔の tick）とは別に、cron / @every で
            // 「時刻・周期ベース」の自律実行を自分で登録できる。対象は常に ctx.session_id。
            // 語彙はハートビートに揃える（next_fire_at / gated / gated_reason）。
            GatewayActionDef {
                name: "get_my_schedules".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "自分（呼び出し元エージェント）の定時実行スケジュールを、いま話しているセッションについて一覧で読み出す。各要素: id、cron_expr（cron 式または @every 形式）、timezone、message（発火時に自分へ渡される指示文）、enabled、next_fire_at（次に発火する予定時刻。照会時に anchor と最終発火時刻から算出する UTC の RFC3339 文字列。無効・式が不正などでは null）、gated / gated_reason（enabled なのに発火しない状態とその理由）、anchor_at / last_fired_at。他のエージェントや別セッションのスケジュールは読めない。定時実行はハートビート（固定短間隔）とは別物で、「毎朝 7 時」「3 時間ごと」のような時刻・周期ベースの自律実行。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {}
                }),
            },
            GatewayActionDef {
                name: "set_my_schedule".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "自分（呼び出し元エージェント）の定時実行スケジュールを、いま話しているセッションに対して登録する。ハートビート（固定短間隔の tick）とは別の、時刻・周期ベースの自律実行。対象は常にこのセッション（Nostr の自発投稿、またはこの Discord チャンネル）で、どこに登録するか選ぶ必要はない。cron_expr は「標準 5 フィールド cron」（例: `0 7 * * *` = 毎朝 7 時、`0 */3 * * *` = 3 時間ごとの 0 分）か「@every 形式」（例: `@every 3h`、`@every 1h30m`、`@every 45m`）で指定する。timezone は cron の評価に使う IANA 名で、省略時は Asia/Tokyo。message は発火時に自分へ渡される指示文（例: ニュースを巡回して要約を書く）。cron 式が不正なら登録は拒否され、その場でエラーが返る（実行時に黙って発火しないことはない）ので、エラーが出たら直して呼び直すこと。enabled は省略時 true（登録するとそのまま定期実行が始まる）。登録直後から次回発火時刻が算出され、再起動を待たず即時に反映される。運用者がハートビートを無効化していても、定時実行は止まらない（別概念）。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "cron_expr": {
                            "type": "string",
                            "description": "標準 5 フィールド cron（例: 0 7 * * *）または @every 形式（例: @every 3h / @every 1h30m）。"
                        },
                        "message": {
                            "type": "string",
                            "description": "発火時に自分へ渡される指示文。"
                        },
                        "enabled": {
                            "type": "boolean",
                            "description": "有効にするか。省略時 true（登録すると定期実行が始まる）。false で登録だけして止めておける。"
                        },
                        "timezone": {
                            "type": "string",
                            "description": "cron の評価に使うタイムゾーン（IANA 名・例 Asia/Tokyo）。省略時 Asia/Tokyo。@every では未使用。"
                        }
                    },
                    "required": ["cron_expr", "message"]
                }),
            },
            // 更新・削除（#477）。set_my_schedule は (session, cron, message) キーの冪等作成なので、
            // 既存スケジュールの cron/message を「変える」経路が無い（別行になる）。id 指定の
            // update/delete でそれを塞ぐ。id は get_my_schedules が返したもの。**他エージェント・
            // 他セッションの id を渡しても触れない**（所属チェック）。
            GatewayActionDef {
                name: "update_my_schedule".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "自分（呼び出し元エージェント）の定時実行スケジュールを、id 指定で部分更新する。id は get_my_schedules が返したもの（他のエージェントや別セッションのスケジュールは触れない）。変更したい項目だけ渡す（省略した項目は現在の値を保つ）: cron_expr（cron 式または @every 形式に変える＝間隔を変える）、message（発火時の指示文を変える）、timezone、enabled（false にすると止まるが行は残る＝履歴が追える。true で再開）。cron_expr / timezone を変えたときや無効→有効に変えたときは、次回発火が「今」を起点に取り直される。cron 式が不正ならその場でエラーが返る（直して呼び直すこと）。変更項目を 1 つも指定しない呼び出しは拒否される。完全に消したいなら delete_my_schedule を使う。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "id": {
                            "type": "integer",
                            "description": "更新するスケジュールの id（get_my_schedules が返した値）。"
                        },
                        "cron_expr": {
                            "type": "string",
                            "description": "新しい cron 式（例: 0 7 * * *）または @every 形式（例: @every 6h）。省略すると現在の値を保つ。"
                        },
                        "message": {
                            "type": "string",
                            "description": "発火時に自分へ渡される新しい指示文。省略すると現在の値を保つ。"
                        },
                        "timezone": {
                            "type": "string",
                            "description": "cron の評価に使うタイムゾーン（IANA 名・例 Asia/Tokyo）。省略すると現在の値を保つ。"
                        },
                        "enabled": {
                            "type": "boolean",
                            "description": "有効にするか。false で止める（行は残り履歴が追える）。true で再開。省略すると現在の値を保つ。"
                        }
                    },
                    "required": ["id"]
                }),
            },
            GatewayActionDef {
                name: "delete_my_schedule".to_string(),
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
                description: "自分（呼び出し元エージェント）の定時実行スケジュールを、id 指定で削除する。id は get_my_schedules が返したもの（他のエージェントや別セッションのスケジュールは削除できない）。行ごと消えるので履歴は残らない。止めるだけで履歴を残したいなら、代わりに update_my_schedule に enabled=false を渡すこと。".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "id": {
                            "type": "integer",
                            "description": "削除するスケジュールの id（get_my_schedules が返した値）。"
                        }
                    },
                    "required": ["id"]
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
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
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
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
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
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
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
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
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
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
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
                class: opencrab_gateway::ToolClass { dispatch: opencrab_gateway::DispatchMode::Inline, sub_engine: opencrab_gateway::SubEngineAccess::NotExposed, sharing: opencrab_gateway::ToolSharing::AgentBound },
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
    #[cfg(feature = "nostr")]
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
    #[cfg(feature = "nostr")]
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
    #[cfg(feature = "nostr")]
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

    /// 薄い nostaro passthrough（#268）。server-own で caller 制限は持たない（#303）。
    ///
    /// 稼働中（登録済み）の Nostr transport の passthrough capability
    /// （[`opencrab_actions::GatewayNostrPassthrough`]）へ委譲する。config は常に
    /// `ctx.agent_id` のもの（鍵混同防止）。`init`/`watch`/`relay` の拒否・`--config` 上書きの封じ・
    /// 未 materialize（鍵未採用）の明示エラー・nsec マスクは capability の内側
    /// （`NostaroCli::run_passthrough`）で行う。呼び出し側はここで subcommand と args を
    /// 取り出して渡すだけ。
    #[cfg(feature = "nostr")]
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
        // 停止の認可は caller で決める（#331）。Owner は常に許可、非オーナーは親セッション
        // 一致に加えて subtask を spawn したターンの呼び出し元以上の信頼度が要る。
        // `is_owner` bool ではなく caller を丸ごと渡すのは、1本化で「セッション一致」だけでは
        // 見知らぬ相手のターンから Owner 由来の subtask を止められてしまうため。
        let caller: opencrab_actions::CallerIdentity = (&ctx.caller).into();
        match neutral_cancel_subtask(
            registry,
            &self.state.db,
            self.completion_sink.as_deref(),
            // 中断の lifecycle 通知（旧 Discord 実装の後始末）はこのマップ経由で行う。
            // `spawn_subtask` が insert したものと同一 Arc（`AppState` 共有）。
            Some(&self.state.subtask_notifiers),
            subtask_id,
            caller,
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

    /// 走行中 subtask への追加指示（steer / #647）。共有 `SubtaskRegistry` を引き、認可
    /// （cancel と同じ `caller_can_manage_subtask`）・steer 記録・不達判定を server-neutral の
    /// `steer_subtask` に委ねる。**これが唯一の実装**で、transport 固有の steer 実装は無い。
    /// registry 未配線（`None`）や不在は not found を返す。既に決着/停止したサブや
    /// auto-dispatch のサブへ送った場合は、**黙って捨てず**その旨をエラーで返す（#647
    /// 受け入れ条件 3・4）。権限なしは `REJECTION_CODE_PREFIX` を付けて拒否として通知する。
    fn steer_subtask(&self, args: &Value, ctx: &GatewayCallContext) -> GatewayActionResult {
        let Some(subtask_id) = args.get("subtask_id").and_then(|v| v.as_str()) else {
            return err("steer_subtask: 'subtask_id' is required".to_string());
        };
        let Some(message) = args.get("message").and_then(|v| v.as_str()) else {
            return err("steer_subtask: 'message' is required".to_string());
        };
        if message.trim().is_empty() {
            return err("steer_subtask: 'message' は空にできません".to_string());
        }
        let Some(registry) = self.subtask_registry.as_ref() else {
            // dispatch 未配線（走行中 subtask を追跡していない）→ 不在扱い。
            return err(format!("steer_subtask: subtask '{subtask_id}' not found"));
        };
        // 認可は cancel と同じ caller ベース（#331 / #647）。
        let caller: opencrab_actions::CallerIdentity = (&ctx.caller).into();
        match neutral_steer_subtask(
            registry,
            &self.state.db,
            subtask_id,
            message,
            caller,
            ctx.session_id.as_deref(),
        ) {
            SteerOutcome::Accepted => GatewayActionResult {
                success: true,
                data: Some(json!({
                    "steered": true,
                    "subtask_id": subtask_id,
                    "note": "追加指示を記録しました。サブタスクは次の反復の合間にこれを読み、受領/反映を親へ返します。",
                })),
                error: None,
            },
            SteerOutcome::NotFound => {
                err(format!("steer_subtask: subtask '{subtask_id}' not found"))
            }
            SteerOutcome::AlreadySettled => err(format!(
                "steer_subtask: subtask '{subtask_id}' は既に完了または停止しているため追加指示を届けられません"
            )),
            SteerOutcome::NotSteerable => err(format!(
                "steer_subtask: subtask '{subtask_id}' は auto-dispatch（LLM ループを持たない）ため追加指示を読む主体がありません。止めるには cancel_subtask を使ってください"
            )),
            SteerOutcome::Unauthorized => err(format!(
                "{REJECTION_CODE_PREFIX}steer_subtask: subtask '{subtask_id}' へこのセッションから追加指示を送る権限がありません（親セッションまたは owner のみ）"
            )),
            SteerOutcome::RecordFailed => err(format!(
                "steer_subtask: subtask '{subtask_id}' への追加指示の記録に失敗しました（届いていません。時間をおいて再試行してください）"
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
    ///
    /// 5 の通知は**親会話の resume** を起こすので、親ターンの呼び出し元
    /// （`SpawnedSubtask.caller`）をそのまま載せる（#298）。`ctx.caller` は sub-engine
    /// 自身（最小権限）なので使えない。
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

        // 登録簿から進捗の宛先と、resume に必要な親ターンの呼び出し元を引く。registry
        // 未配線（`None`）は「登録簿に無い」と同じ扱い（= 自己申告として親ログにだけ残す）。
        let subtask_entry: Option<ProgressSubtaskEntry> =
            self.subtask_registry.as_ref().and_then(|registry| {
                if !subtask_id_arg.is_empty() {
                    registry
                        .get(&subtask_id_arg)
                        .map(|e| ProgressSubtaskEntry::from_entry(subtask_id_arg.clone(), &e))
                } else {
                    registry
                        .iter()
                        .find(|e| e.session_id == current_session_id)
                        .map(|e| ProgressSubtaskEntry::from_entry(e.key().clone(), e.value()))
                }
            });

        // 所有権ゲート（#64 / #331）: subtask_id は LLM 由来の引数なので、呼び出し元
        // セッションのサブタスク（自分自身 = session_id 一致、または自分の子 =
        // parent_session_id 一致）以外は拒否する。無検証だと他セッションへの進捗ログ
        // 書き込み・メインエンジン再呼び出しを誘発できてしまう。
        if let Some(entry) = &subtask_entry {
            let is_self = entry.session_id == current_session_id;
            let is_parent = entry.parent_session_id == current_session_id;
            if !is_self && !is_parent {
                let id = &entry.subtask_id;
                return err(format!(
                    "{REJECTION_CODE_PREFIX}report_progress: subtask '{id}' は呼び出し元セッションのサブタスクではありません"
                ));
            }
            // 親経由の代理報告（`is_parent`）は、subtask を spawn したターンの呼び出し元
            // （`entry.caller`）を自分の権限で管理できるときだけ許す（#331）。セッションを
            // agent 単位で 1 本にした（#323）ため、`is_parent` だけだと見知らぬ相手
            // （caller=Agent）のターンから Owner 由来の subtask へ進捗を差し込み、親会話の
            // resume（メインエンジン再呼び出し）を誘発できてしまう。
            // **`is_self`（subtask 本人 = depth>=1 の自己申告）は無条件で許す** — ここに
            // caller 判定を掛けるとサブエージェント自身の進捗報告が壊れる（自セッションは
            // 本人しか名乗れないので攻撃経路にならない）。
            if !is_self {
                let caller: opencrab_actions::CallerIdentity = (&ctx.caller).into();
                if !caller.can_manage_subtask_of(&entry.caller) {
                    let id = &entry.subtask_id;
                    return err(format!(
                        "{REJECTION_CODE_PREFIX}report_progress: subtask '{id}' は別の権限で起動されたため、このターンからは進捗報告できません"
                    ));
                }
            }
        }

        let subtask_id = subtask_entry
            .as_ref()
            .map(|e| e.subtask_id.clone())
            .unwrap_or(subtask_id_arg);
        let parent_session_id = subtask_entry
            .as_ref()
            .map(|e| e.parent_session_id.clone())
            .unwrap_or_else(|| current_session_id.clone());
        // resume 時の呼び出し元（#298）。登録簿に無い自己申告は最小権限へ倒す。
        // ここで `ctx.caller`（= sub-engine 自身 = Agent）を使ってはならない。
        let resume_caller = subtask_entry
            .as_ref()
            .map(|e| e.caller.clone())
            .unwrap_or(opencrab_actions::CallerIdentity::Agent);

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
        if let Some(entry) = &subtask_entry {
            if let Some(notifier) = self.state.subtask_notifiers.get(&entry.subtask_id) {
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
            // 継続を起こすかの判断は `dispatch_settled`（#638・唯一の実装）。進捗を配送するのは
            // `forwards_progress()` が true の transport（Discord）だけ——ここで分岐しない。
            opencrab_actions::dispatch_settled(
                &*sink,
                SubtaskSettled {
                    session_id: parent_session_clone,
                    agent_id: agent_id_clone,
                    subtask_id: subtask_id_clone,
                    exit_reason: "progress".to_string(),
                    kind: SettleKind::Progress,
                    // 進捗の宛先は親セッション。返信先の復元は sink 側の責務（#167）。
                    reply_target: None,
                    // 親ターンの呼び出し元を引き継ぐ（#298）。ここを Agent 固定にすると
                    // 「進捗を報告すると自分の権限が落ちる」自爆的な挙動になる。
                    caller: resume_caller,
                },
            );
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
        if !ctx.caller.is_owner_equivalent() {
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
        if !ctx.caller.is_owner_equivalent() {
            return err("manage_allowed_commands requires owner".to_string());
        }
        let agent_id = ctx.agent_id.clone();
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string());

        // `list` は DB ロックを取る前に片付ける。実効リストの解決
        // （`effective_allowed_commands`）が内部で同じ Mutex を取るため、ここで
        // 掴んだまま呼ぶと自己デッドロックする。
        if action == "list" {
            // `list_allowed_commands` と**同じ解決点**を通す。DB 行だけを返すと
            // 設定ファイル由来のコマンドが消えて「使えない」と誤認される（#300）。
            return GatewayActionResult {
                success: true,
                data: Some(
                    json!({ "commands": crate::process::effective_allowed_commands(&self.state, &agent_id) }),
                ),
                error: None,
            };
        }

        let conn = match self.state.db.lock() {
            Ok(c) => c,
            Err(_) => return err("db lock failed".to_string()),
        };
        match action {
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

    #[cfg(feature = "nostr")]
    async fn configure_nostr(&self, args: &Value, ctx: &GatewayCallContext) -> GatewayActionResult {
        // 多層防御: bridge が owner を強制するが、ハンドラでも fail-closed で確認する。
        if !ctx.caller.is_owner_equivalent() {
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
        // #514: DM の kind（4 / 1059）はここで落とす。保存自体は apply_nostr_settings 側でも
        // ストリップするが、この tool の応答（下の "kinds"）が保存値と一致するよう、モデルへ
        // 返す前にも落として「DM を購読設定できた」と誤解させない。DM は受信破棄・送信禁止・
        // 購読除外の 3 経路で一貫して扱わない（オーナー決定）。
        let kinds: Vec<u32> = arg_or_cur_kinds()
            .into_iter()
            .filter(|k| !opencrab_nostr::DM_KINDS.contains(k))
            .collect();
        let enabled = args
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(existing.enabled);
        // Nostr のオーナー公開鍵（#319）。未指定なら現状維持。**owner 権限のあるターン
        // （実際には Discord など）からしか触れない**ので、この口が Nostr 側の
        // 「オーナー未設定 → 誰も owner になれない → 設定できない」の鶏卵を解く。
        let owner_pubkey = args.get("owner_pubkey").and_then(|v| v.as_str());

        match crate::api::nostr::apply_nostr_settings(
            &self.state,
            &agent_id,
            &relays,
            &authors,
            &keywords,
            &kinds,
            enabled,
            None,
            owner_pubkey,
        )
        .await
        {
            Ok(()) => {
                // 保存後の値を読み直して返す（入力が npub でも保存形の hex が返る＝
                // どちらの表現で渡しても同じ鍵になったことをモデルが確認できる）。
                let stored = self
                    .state
                    .db
                    .lock()
                    .ok()
                    .and_then(|conn| {
                        opencrab_db::queries::get_agent_nostr_owner_pubkey(&conn, &agent_id).ok()
                    })
                    .unwrap_or_default();
                GatewayActionResult {
                    success: true,
                    // secret_key は返さない。
                    data: Some(json!({
                        "agent_id": agent_id,
                        "relays": relays,
                        "authors": authors,
                        "keywords": keywords,
                        "kinds": kinds,
                        "enabled": enabled,
                        "owner_pubkey": stored,
                    })),
                    error: None,
                }
            }
            Err((_code, msg)) => err(msg),
        }
    }

    async fn configure_self(&self, args: &Value, ctx: &GatewayCallContext) -> GatewayActionResult {
        // 多層防御: bridge が owner を強制するが、ハンドラでも fail-closed で確認する。
        if !ctx.caller.is_owner_equivalent() {
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
            // #412: オーナーが会話で「モデルを変えて」と言う経路がここ。ダッシュボードの
            // 口だけ塞いでも、こちらから未登録モデルを入れれば「黙って既定値」が再発する。
            // `PUT`/`PATCH /api/agents/{id}` と同じ gate を通す（owner 限定の扱いは
            // 上の fail-closed チェックのままで変えない）。
            if let Some(Some(new_model)) = patch.model.as_ref() {
                let existing = opencrab_db::queries::get_agent(&conn, &agent_id)
                    .ok()
                    .flatten();
                // #676（案Y）: 送るプロバイダの spec へ切り替えるときだけ max_output_tokens を要求。
                let sends_max = self
                    .state
                    .llm_router
                    .get()
                    .sends_max_output_tokens(new_model);
                if let Err(e) = crate::process::check_agent_model_change(
                    &conn,
                    existing.as_ref(),
                    Some(new_model),
                    sends_max,
                ) {
                    return err(e);
                }
            }
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
        if !ctx.caller.is_owner_equivalent() {
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
            // PR-1B: Nostr の会話ゲートツール群は nostr feature の内側。外した構成では
            // これらの arm も descriptor も消え、既定 `_ =>` の inner 委譲へ落ちる。
            #[cfg(feature = "nostr")]
            "configure_nostr" => self.configure_nostr(args, ctx).await,
            "configure_self" => self.configure_self(args, ctx).await,
            "configure_mcp_server" => self.configure_mcp_server(args, ctx).await,
            // bootstrap 鍵生成（鍵未設定でも露出）。inner より先に own が処理する。
            #[cfg(feature = "nostr")]
            "nostr_generate_key" => self.nostr_generate_key(args, ctx).await,
            // bootstrap 鍵一覧（鍵未設定でも露出）。生成鍵の npub のみ返す（nsec 非返却）。
            #[cfg(feature = "nostr")]
            "nostr_list_keys" => Self::nostr_list_keys(ctx),
            // bootstrap identity 採用（鍵未設定でも露出）。未接続なら自分宛のみを購読する
            // 設定で自動接続する。inner より先に own が処理する（#264）。
            #[cfg(feature = "nostr")]
            "nostr_switch_identity" => self.nostr_switch_identity(args, ctx).await,
            // 薄い nostaro passthrough（#268）。稼働中の Nostr transport の passthrough
            // capability へ委譲する。inner へは委譲しない（own が処理する）。
            #[cfg(feature = "nostr")]
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
            #[cfg(feature = "nostr")]
            "get_my_nostr_relay" => {
                crate::agent_nostr_relay::get_my_nostr_relay(&self.state, args, ctx)
            }
            #[cfg(feature = "nostr")]
            "set_my_nostr_relay" => {
                crate::agent_nostr_relay::set_my_nostr_relay(&self.state, args, ctx)
            }
            // エージェント自身のハートビート設定（#247 段階 2）。対象は常に
            // `ctx.agent_id` で、引数から他エージェントを指す経路は無い。
            "get_my_heartbeat" => crate::agent_heartbeat::get_my_heartbeat(&self.state, args, ctx),
            "set_my_heartbeat" => crate::agent_heartbeat::set_my_heartbeat(&self.state, args, ctx),
            // #599: 時間を待たずに手動発火（オーナー / co_agent 限定・OWNER_ONLY_ACTIONS）。
            "run_my_heartbeat" => crate::agent_heartbeat::run_my_heartbeat(&self.state, args, ctx),
            // エージェント自身の定時実行スケジュール（#455）。対象は常に ctx.session_id。
            "get_my_schedules" => crate::agent_schedule::get_my_schedules(&self.state, args, ctx),
            "set_my_schedule" => crate::agent_schedule::set_my_schedule(&self.state, args, ctx),
            // 更新・削除（#477）。id 指定で、ctx.agent_id＋現在セッションの所属チェックを通った行だけ。
            "update_my_schedule" => {
                crate::agent_schedule::update_my_schedule(&self.state, args, ctx)
            }
            "delete_my_schedule" => {
                crate::agent_schedule::delete_my_schedule(&self.state, args, ctx)
            }
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
                let res = crate::subtask_spawn::spawn_subtask(
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
                .await;
                // #431: 起動が成立したときだけ「このターンは次の行動を選んだ」と数える。
                // `spawn_subtask` は登録簿へ insert し終えてから `success: true` を返し、
                // 手前の失敗（task 引数なし / session 不明 / 登録簿未配線）は全て
                // `success: false` なので、success ⟺ 登録済み ⟺ 完了で resume が来る。
                // 起動に失敗したターンは resume が来ない＝そのターンが最後の発話なので、
                // ここで数えないのが正しい（🏁 は付く）。
                if res.success {
                    if let Some(c) = &self.subtask_starts {
                        c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                }
                res
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
            // 走行中 subtask への追加指示（steer / #647）。cancel と同じく neutral 実装へ委ねる。
            "steer_subtask" => self.steer_subtask(args, ctx),
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
mod tests;

/// #412: `configure_self` から未登録モデルを設定できないこと。
///
/// オーナーが会話で「モデルを変えて」と言う経路がここ。ダッシュボードの口
/// （`PUT`/`PATCH /api/agents/{id}`）だけ塞いでも、こちらが素通りなら
/// 「黙って既定値」状態は同じように再発する。
#[cfg(test)]
mod configure_self_model_gate_tests;
