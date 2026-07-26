use std::sync::Arc;

use async_trait::async_trait;
use opencrab_core::{ActionExecutor, ActionResult as CoreActionResult, FunctionDefinition};
use opencrab_gateway::GatewayActions;

use crate::dispatcher::ActionDispatcher;
use crate::traits::{ActionContext, ActionResult as ActionsActionResult};

/// ツール 1 件の実行イベント種別。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolEventStatus {
    Started,
    Completed,
    Failed,
    Rejected,
}

/// 1 ツール実行イベントの観測データ（webhook 等の sink へ渡す）。
/// raw な args/result を保持し、redaction/整形は sink 側が配送直前に行う。
pub struct ToolEvent<'a> {
    pub tool_name: &'a str,
    pub tool_call_id: &'a str,
    pub agent_id: &'a str,
    pub session_id: Option<&'a str>,
    pub depth: u32,
    pub status: ToolEventStatus,
    pub started_at: &'a str,
    pub duration_ms: Option<u64>,
    pub args: &'a serde_json::Value,
    pub result: Option<&'a serde_json::Value>,
    pub error: Option<&'a str>,
}

/// ツール実行イベントの sink。executor が start/terminal で呼ぶ。
pub trait ToolEventSink: Send + Sync {
    fn on_event(&self, event: &ToolEvent<'_>);
}

/// 権限ポリシーによる拒否（実行に到達しなかった）を表す構造的マーカー。
///
/// gateway action 等が permission-check で拒否したときは、エラー文言の先頭へ
/// この安定コードを付ける（`crate::reject_marker` 経由）。分類器はこの構造的な
/// 接頭辞を第一の根拠にする。`"permission"` / `"denied"` / `"forbidden"` のような
/// 広い自然言語の部分一致は、実行されたが失敗した通常のエラー（例: OS の
/// "Permission denied"、shell の "Operation not permitted"）を rejected に誤分類
/// するため使わない。
pub const REJECTION_CODE_PREFIX: &str = "rejected: ";

/// 権限ポリシーによる拒否を `GatewayActionResult` として組み立てる（`rejected:` マーカー付き）。
fn gateway_reject(msg: impl Into<String>) -> opencrab_gateway::GatewayActionResult {
    let msg = msg.into();
    tracing::debug!(
        target: "webhook_audit",
        reason = %msg,
        "gateway action rejected by permission policy"
    );
    opencrab_gateway::GatewayActionResult {
        success: false,
        data: None,
        error: Some(format!("{REJECTION_CODE_PREFIX}{msg}")),
    }
}

/// sub-engine に許可する gateway アクションの許可リスト（#63 / RFC #152 S2）。
///
/// bridge の DISCORD_ACTIONS depth ゲートは 28 アクション中 5 つしかブロックしないため、
/// 素の DiscordGatewayActions を接続すると、ハンドラ側ゲートの無いアクション
/// （send_ui / discord_channel_config / discord_create_channel / update_memory_index_config
/// 等）が depth>=1 に開放されてしまう。deny-list に頼らず、ここで明示的に許可した
/// アクションだけを sub-engine から到達可能にする（deny-by-default 最外周フィルタ）。
///
/// S2 で inner が合成 gateway（`SystemGatewayActions` = server ツール + transport の union）
/// になったため、このフィルタは**合成後のアクション和集合**に対して最外周で強制される。
/// server ツールは 1 つずつ triage して足す:
/// - `nostr_generate_key`: 生成鍵の nsec は LLM に返さず（`crates/nostr/src/actions.rs:271`）
///   サーバ内に 0600 保存。npub/pubkey のみ返る。bridge policy でも owner/trusted 限定でない
///   （TRUSTED_ONLY_ACTIONS に無い）。中リスク → 許可。
/// - それ以外の server ツール（configure_* / manage_allowed_commands 等）は既定で不許可。
///   （加えて bridge の OWNER_ONLY_ACTIONS が二重に遮断する。）
///
/// spawn_subtask は意図的に含めない: ネスト spawn は従来も（gateway 未接続のため）
/// 不可能だった現状維持。ネストを有効化する場合は bridge の MAX_DEPTH ゲートではなく
/// この許可リストが実効ゲートである点に注意。
///
/// **#175 S4 以降はこの点がより重要**: `spawn_subtask` は Discord gateway ではなく
/// 合成 gateway（`SystemGatewayActions`）の own ツールになったため、許可リストに
/// 足すと sub-engine から**必ず**到達できてしまう（transport の有無に依存しない）。
/// ガードは `sub_engine_cannot_see_spawn_subtask`（`crates/server/src/system_actions.rs`）。
pub const SUB_ENGINE_ALLOWED_ACTIONS: &[&str] = &["report_progress", "nostr_generate_key"];

/// sub-engine 専用の最小権限 gateway。許可リストのアクションだけを inner 実装へ委譲する。
///
/// `inner` は合成 gateway（`SystemGatewayActions`）へのハンドル（RFC #152 S2）。
/// これにより sub-engine から server ツール（`nostr_generate_key` 等）へ到達できる。
/// 合成 gateway は自分が扱わないツール（`report_progress` 等）を transport gateway
/// （DiscordGatewayActions）へ委譲するため、registry 照合・デバウンス・完了イベント
/// 送信は親経由の呼び出しと同一に動く（transport は親と同一インスタンスを共有）。
///
/// root_gateway が未注入の経路（後方互換）では、呼び出し側が transport gateway 単体を
/// `Arc<dyn GatewayActions>` として渡す。
pub struct SubEngineGatewayActions {
    inner: Arc<dyn GatewayActions>,
}

impl SubEngineGatewayActions {
    pub fn new(inner: Arc<dyn GatewayActions>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl GatewayActions for SubEngineGatewayActions {
    fn definitions(&self) -> Vec<opencrab_gateway::GatewayActionDef> {
        self.inner
            .definitions()
            .into_iter()
            .filter(|d| SUB_ENGINE_ALLOWED_ACTIONS.contains(&d.name.as_str()))
            .collect()
    }

    async fn execute(
        &self,
        name: &str,
        args: &serde_json::Value,
        ctx: &opencrab_gateway::GatewayCallContext,
    ) -> opencrab_gateway::GatewayActionResult {
        if SUB_ENGINE_ALLOWED_ACTIONS.contains(&name) {
            return self.inner.execute(name, args, ctx).await;
        }
        // 実在するが許可外 → 権限拒否（rejected: マーカー）。
        // 未知の名前 → 通常の失敗（幻覚ツール名を Rejected に誤分類させない）。
        if self.inner.definitions().iter().any(|d| d.name == name) {
            gateway_reject(format!("action '{name}' is not available in sub-engines"))
        } else {
            opencrab_gateway::GatewayActionResult {
                success: false,
                data: None,
                error: Some(format!("Unknown gateway action: {name}")),
            }
        }
    }
}

/// Discord 送信系アクション: depth >= 1 の sub-engine からは**定義の非表示と実行の拒否の両方**で
/// ブロックする（定義から隠すだけでは、モデルが親コンテキストの記憶で名前を呼んだ場合に素通しになる）。
///
/// **この一覧は `DiscordGatewayActions::definitions()` に実在する名前だけを持つ。**
/// 以前は 20 名のうち 13 名（`discord_send` / `discord_react` / `discord_edit_message` …）が
/// 現行 gateway に存在しない死名で、depth ゲートも dispatch 除外も実質空振りしていた。
/// ドリフト検出は `crates/discord` の `test_bridge_policy_names_are_live_gateway_actions`
/// と `discord_tools_are_classified_for_dispatch` が担う。
///
/// なお sub-engine の実効ゲートはこの deny-list ではなく [`SUB_ENGINE_ALLOWED_ACTIONS`]
/// （allow-list / 同じモジュール）。ここは多層防御。
pub const DISCORD_ACTIONS: &[&str] = &[
    "discord_send_file",
    "discord_add_reaction",
    "discord_list_channels",
    "discord_list_guilds",
    // A2UI 送信（ユーザーの応答待ちを伴う対話的配送）。sub-engine からは不可。
    "send_ui",
    "request_peer_review",
    // VC 参加/退出はサーバの他メンバーに聞こえる行為。sub-engine からは不可。
    "join_voice_channel",
    "leave_voice_channel",
];

/// Discord gateway のツールのうち **inline 実行のまま**にするもの
/// （`default_non_dispatch_tools` の種）。
///
/// 分類基準（5 項目）の権威は [`crate::subtask::default_non_dispatch_tools`] の doc、
/// 運用者向けの**分類基準**は `docs/DESIGN.md`「非ブロックツール実行」節。
/// ツール名の一覧はこの定数群が唯一の権威で、doc 側には置かない（二重管理を避ける）。
///
/// 全要素が `DiscordGatewayActions::definitions()` に実在し、かつ
/// [`DISCORD_DISPATCHABLE_ACTIONS`] と互いに素であることをテストで保証する。
pub const DISCORD_INLINE_ACTIONS: &[&str] = &[
    // (1) 制御系（default_non_dispatch_tools の制御集合と重複するが、Discord の
    //     definitions() 全名を分類し尽くすためここにも並べる）。
    //     `spawn_subtask` / `report_progress` は #175 S4 で server 側（
    //     `SERVER_INLINE_ACTIONS`）へ移設済み。Discord は定義しないのでここには無い。
    "cancel_subtask",
    // (2) 配送系。
    "discord_send_file",
    "discord_add_reaction",
    "request_peer_review",
    "join_voice_channel",
    "leave_voice_channel",
    // (2) 配送系 + ユーザーの応答待ち（pending interaction）。
    "send_ui",
    // (3) 同ターン結果依存: webhook URL / 作成物の ID をそのターンで使う。
    "ensure_webhook",
    "ensure_subtask_webhook",
    "discord_create_webhook",
    "discord_create_channel",
    "set_default_webhook",
    "set_default_subtask_webhook",
    // (4) run 内共有状態: readable/writable は走行中ターンの配送可否を左右する。
    "discord_channel_config",
    // (5) 純粋な読み取り。
    "discord_list_channels",
    "discord_list_guilds",
    "list_webhooks",
    "list_subtask_webhooks",
    "get_default_webhook",
    "get_default_subtask_webhook",
    "read_heartbeat_instructions",
    // 許可コマンドの list/add/remove は #157 S1 で server 側（`SERVER_INLINE_ACTIONS`）へ
    // 移設済み。Discord は定義しないのでここには無い（分類の所属は inline のまま）。
];

/// Discord gateway のツールのうち、**意図的に dispatch を許す**もの。
///
/// 「長時間かかる」か「同ターンで結果を使わない書き込み」だけを置く。ここに無く
/// [`DISCORD_INLINE_ACTIONS`] にも無い名前が `definitions()` に現れたらテストが落ちる。
pub const DISCORD_DISPATCHABLE_ACTIONS: &[&str] = &[
    // `rebuild_memory_index`（#175 S4）と `update_memory_index_config`（#157 S1）は
    // server 側（`SERVER_DISPATCHABLE_ACTIONS`）へ移設済み。Discord は定義しないので
    // ここには無い（どちらも分類の所属は dispatchable のまま）。
    // スキルファイルの生成。結果は確認のみで同ターンでは使わない。
    "create_skill",
    // 設定/指示文の書き込み。同ターンで読み戻さない。
    "update_heartbeat_instructions",
];

/// Nostr の**配送系**アクション（#168）。「送る」こと自体が応答なので、非ブロック
/// dispatch（RFC #152 S3a）の対象から外して inline 実行のままにする集合。
///
/// background 化すると、親ターンが「明示送信済み」フラグ（`NostrGatewayActions::sent_flag`）
/// を観測する前に run が終わり、暗黙返信（ループ側のフォールバック reply）と、後から
/// 完了した明示送信の**二重投稿**になる。
///
/// 送信系以外の2つ:
/// - `nostr_upload`: 送信ではない（`sent` を立てない）が、戻り値の URL を同じターンで
///   投稿本文に使うのが通常の用法。background 化すると URL の代わりに `spawned` が返り、
///   モデルが URL を載せられなくなる。
/// - `nostr_switch_identity`: 送信ではないが「以後の全送信のアイデンティティを差し替える」。
///   親ターンの送信と順序が入れ替わると別 identity で投稿しかねないため inline に留める。
///
/// 一方 `nostr_generate_key`（vanity 探索 = 長時間）は dispatch 対象に**残す**
/// （これを background 化するのが S3a の主目的）。
///
/// この一覧は `crates/nostr` の `NostrGatewayActions::definitions()` と対応する
/// （ドリフト検出テストが nostr crate 側にある）。`DISCORD_ACTIONS` と同様、
/// gateway 名を下位層に置くのは既存の前例に倣う。
pub const NOSTR_DELIVERY_ACTIONS: &[&str] = &[
    "nostr_post",
    "nostr_reply",
    "nostr_dm",
    "nostr_zap",
    "nostr_upload",
    "nostr_switch_identity",
];

/// Nostr gateway のツールのうち、**意図的に dispatch を許す**もの
/// （[`NOSTR_DELIVERY_ACTIONS`] の補集合）。
///
/// `nostr_generate_key` は vanity 探索で分単位かかりうる長時間処理。これを background
/// 化するのが RFC #152 S3a の主目的。ここにも [`NOSTR_DELIVERY_ACTIONS`] にも無い名前が
/// `NostrGatewayActions::definitions()` に現れたら
/// `nostr_tools_are_classified_for_dispatch`（`crates/nostr/src/actions.rs`）が落ちる。
pub const NOSTR_DISPATCHABLE_ACTIONS: &[&str] = &["nostr_generate_key"];

/// server 内蔵の設定ツール源（`crates/server/src/system_actions.rs` の
/// `SystemGatewayActions`）のうち **inline 実行のまま**にするもの。
///
/// この gateway は Discord/Nostr と違い **transport 非依存**で、web / REST / heartbeat の
/// 全ターンに載る（`crates/server/src/process.rs` の合成 executor）。分類ガードの外に
/// 置いていた頃は 6 個中 5 個が background 化され、
/// - `manage_allowed_commands(action="list")` / `configure_mcp_server(action="list")` は
///   純粋な読み取り（基準5）なのに「一覧を教えて」が 2 ターン 2 メッセージに割れ、
/// - `configure_llm_provider` は run 内共有状態（LLM ルーター）のホットスワップ（基準4）
///   なのに走行中の run と非同期に差し替わり、doc が約束する「health_check → 失敗なら
///   自動ロールバックして結果で通知」が同ターンで得られない、
/// という壊れ方をしていた。
///
/// **fail-closed**: `SystemGatewayActions::own_definitions()` の全名がこの集合か
/// [`SERVER_DISPATCHABLE_ACTIONS`] のどちらか一方に属することを
/// `server_tools_are_classified_for_dispatch`（`crates/server/src/system_actions.rs`）が
/// 検査する。新しい設定ツールを足したら分類を強制される。
pub const SERVER_INLINE_ACTIONS: &[&str] = &[
    // (4) run 内共有状態: LLM ルーターのホットスワップ。走行中の run が参照している
    //     プロバイダを差し替えるうえ、適用後の health_check / 自動ロールバックの結果を
    //     同ターンで返す契約になっている。
    "configure_llm_provider",
    // (5) 純粋な読み取り（action="list"）。add/remove も成否を同ターンで返す契約なので
    //     inline。ここでも「許可した直後に同ターンで execute_shell」は**できない**
    //     （ツール登録が run 冒頭でスナップショット / #202）ことに注意。
    //     `add_allowed_command` / `remove_allowed_command` と同分類。
    "manage_allowed_commands",
    // (4) 設定の書き込み: 以後の Nostr 送信（relay / identity）に効く共有状態。
    //     成否を同ターンで確認して次の操作へ進む。
    "configure_nostr",
    // (4) 設定の書き込み: 名前・system prompt 等、以後の run の前提を書き換える。
    "configure_self",
    // (5) 純粋な読み取り（action="list"）+ (3) 追加した直後に当該 MCP ツールを使う用法。
    "configure_mcp_server",
    // (1) 制御系: 走行中 subtask の停止。background 化しては意味を成さない。
    "cancel_subtask",
    // (1) 制御系: サブタスクの進捗報告（#175 S1）。それ自体が subtask ライフサイクルの
    //     通知（デバウンス後にメインエンジンを呼び直す）なので background 化しない。
    "report_progress",
    // (1) 制御系: サブタスクの起動（#175 S4）。それ自体が「background 化する」ツール
    //     （戻り値の subtask_id を同ターンで cancel / 追跡に使う）なので、さらに
    //     dispatch で包むと二重の背景化になり意味を成さない。
    "spawn_subtask",
    // (5) 純粋な読み取り（#157 S1 で Discord から移設）。「許可コマンドを教えて」が
    //     2 ターン 2 メッセージに割れないよう inline。
    "list_allowed_commands",
    // **移設前の分類を維持する**（#157 S1 で Discord から移設）。移設前は
    //     `DISCORD_INLINE_ACTIONS` に属していたので、所属を変えずにここへ移した。
    //     分類の妥当性そのものは移設の範囲外（変えるなら別 issue）。
    //     なお「許可した直後に同ターンで execute_shell を使う」ことは**元から
    //     できない**（ツール登録が run 冒頭で設定をスナップショットするため、
    //     許可は次の run から効く / #202）。同ターン反映を根拠にはしない。
    "add_allowed_command",
    "remove_allowed_command",
];

/// server 内蔵の設定ツール源のうち、**意図的に dispatch を許す**もの。
///
/// `nostr_generate_key` は vanity 探索で分単位かかりうる長時間処理（RFC #152 S3a の
/// 主目的）。`SystemGatewayActions` は鍵未設定でもこれを露出する bootstrap ツールとして
/// 自前で定義するため、Nostr gateway の [`NOSTR_DISPATCHABLE_ACTIONS`] とは別に
/// この gateway でも分類する必要がある。
pub const SERVER_DISPATCHABLE_ACTIONS: &[&str] = &[
    "nostr_generate_key",
    // 全メモリの再インデックス（長時間・同ターンで結果を使わない / #175 S4 で Discord
    // から移設）。dispatch の主目的そのもの。
    "rebuild_memory_index",
    // 設定の書き込み（#157 S1 で Discord から移設）。同ターンで読み戻さない。
    // Discord 側でも dispatchable だったので分類の所属は変えていない。
    "update_memory_index_config",
];

/// `ActionDispatcher::new()` が登録する **core アクション**のうち inline 実行のまま
/// にするもの（`default_non_dispatch_tools` の種）。
///
/// gateway 由来のツール（Discord / Nostr）だけを分類していた頃は、core アクション
/// 32 個が**分類ガードの外**にあり全部 dispatch されていた。その結果
/// - system prompt が指示する記憶想起フロー（`search_memory_index` →
///   `retrieve_memory_nodes`）が 2 回の背景往復 = ユーザーへ 4 通、
/// - `open_task` は戻り値の task_id を同ターンで使うのに `spawned` しか返らない、
/// という壊れ方をしていた。分類基準（[`crate::subtask::default_non_dispatch_tools`]
/// の doc）に沿って全名を明示する。
///
/// **fail-closed**: `ActionDispatcher::new()` の全アクション名がこの集合か
/// [`CORE_DISPATCHABLE_ACTIONS`] のどちらか一方に属することを
/// `core_actions_are_classified_for_dispatch`（`crates/actions/src/subtask.rs`）が
/// 検査する。新しい core アクションを登録したら、どちらかへ入れない限りテストが落ちる。
pub const CORE_INLINE_ACTIONS: &[&str] = &[
    // (1) 制御系: そのターンを終える宣言。background 化すると同ターンに効かない。
    "declare_done",
    // (3) 同ターン結果依存: 生成した内声をそのターンの応答づくりに使う。
    "generate_inner_voice",
    // (3) 同ターン結果依存: 自己評価の結果を見てそのターンの応答を直す。
    "evaluate_response",
    // (3) 同ターン結果依存: 戻り値の task_id を update/record/close で使う。
    "open_task",
    // (3) 同ターン結果依存: 編集/削除/作成の成否を確認して次の操作へ進む用法が通常
    //     （mkdir → write、edit → 失敗なら別の編集、のような同ターンの連鎖）。
    "ws_edit",
    "ws_delete",
    "ws_mkdir",
    // (4) run 内共有状態: model_override / current_purpose を書き換える。
    "select_llm",
    // (4) run 内共有状態: 以後のスキル可視性（棚）を書き換える。
    "retire_my_skill",
    "restore_my_skill",
    // (4) run 内共有状態: 以後の system prompt に効く指示文の書き込み（owner 専用）。
    "update_instructions",
    // (4) 台帳の状態: contract / progress / close が同ターンに効かないと、以後の
    //     `get_task` と食い違う（「更新したのに古い契約が見える」）。
    "update_task_contract",
    "record_task_progress",
    "close_task",
    // (5) 純粋な読み取り（即答すべきもの）。dispatch すると質問 1 つが 2 ターン
    //     2 メッセージに割れるだけ。記憶想起フローは 2 段連鎖なので特に致命的。
    "get_system_info",
    "ws_read",
    "ws_list",
    "read_skill",
    "browse_memory_index",
    "search_memory_index",
    "retrieve_memory_nodes",
    "search_my_history",
    "get_task",
    "analyze_llm_usage",
    "recall_model_experiences",
    // (6) 情報価値の無い短時間の書き込み。dispatch には必ず resume ターン
    //     （= ユーザーへの追加メッセージ）が 1 本付くので、報告する価値が無い
    //     書き込みを background 化すると雑音が増えるだけ。
    "update_impression",
    "save_model_insight",
];

/// core アクションのうち、**意図的に dispatch を許す**もの。
///
/// 「長時間かかる」か「同ターンで結果を使わない書き込み」だけを置く（dispatch には
/// resume ターンが 1 本付くので、その 1 通に見合う仕事に限る）。
pub const CORE_DISPATCHABLE_ACTIONS: &[&str] = &[
    // 長文の書き出しは payload が大きくなりうる。書けたかどうかは完了報告で足りる。
    "ws_write",
    // 学習の書き込み: 戻り値（skill_id）を同ターンで使わない。「覚えておいて」は
    // 非ブロックで処理して完了時に報告するのが自然な依頼。
    "learn_from_experience",
    "learn_from_peer",
    "reflect_and_learn",
    // 要約の保存: 同ターンで読み戻さない。
    "summarize_and_save",
    // スキル生成（Discord の `create_skill` と同分類）。
    "create_my_skill",
];

/// spawn_subtask のネスト上限。
const MAX_DEPTH: u32 = 2;

/// owner のみが可視・実行できるアクション（#45）。
pub const OWNER_ONLY_ACTIONS: &[&str] = &[
    "update_instructions",
    "update_heartbeat_instructions",
    // LLM プロバイダ設定の即時変更（ルーターのホットスワップ）。外部ユーザー由来の
    // ターン（caller=Agent）からは一覧にも出さず実行もしない。owner のみ。
    "configure_llm_provider",
    // 許可コマンド（execute_shell のホワイトリスト）の管理。実行範囲を広げるため owner のみ。
    "manage_allowed_commands",
    // Nostr 連携設定（購読リレー/フィルタ/有効化）。外部発信・アイデンティティに関わるため owner のみ。
    "configure_nostr",
    // 自分の人格/モデル/推論強度/web 検索の変更。挙動を左右するため owner のみ。
    "configure_self",
    // MCP サーバ設定の管理（外部プロセス起動・env に秘密を含みうる）。owner のみ。
    "configure_mcp_server",
];

/// owner / co_agent / trusted_user のみ（素の Agent は不可）のアクション（#45）。
/// `execute_skill` は現行の gateway に実装が無い防御的エントリ（将来追加時に
/// 最初からゲートされるように残している）。
pub const TRUSTED_ONLY_ACTIONS: &[&str] = &[
    "create_skill",
    "execute_skill",
    "read_heartbeat_instructions",
    // VC 参加/退出。可視性 == 強制の対称化（#45）: 非 trusted の Agent には
    // 一覧にも出さない。ハンドラ側はさらに厳しく owner/trusted_user のみ許可
    // （co_agent は一覧に見えても実行は拒否される）。
    "join_voice_channel",
    "leave_voice_channel",
    // Nostr の送金（zap）と任意宛先 DM。Nostr 受信イベントは外部ユーザー由来で
    // caller=Agent（最小権限）のため、これらは inbound では見えず実行もされない
    // （プロンプトインジェクションで資金流出/なりすまし DM されるのを防ぐ）。
    // owner/trusted_user が起点のターン（ダッシュボード等）でのみ使える。
    "nostr_zap",
    "nostr_dm",
    // 本鍵（アイデンティティ）の切替。外部ユーザーが勝手に乗っ取れないよう owner/
    // trusted のみ（inbound=Agent には一覧にも出さず実行もしない）。
    "nostr_switch_identity",
];

/// アクション名 → 権限/深度ポリシー（#45 の単一の表）。
///
/// 以前は可視性（`list_tools`）だけがこれらのリストを参照し、実行
/// （`dispatch_inner`）は depth 系しか強制していなかったため、「一覧から
/// 隠したツールをモデルが名前指定で実行できる」食い違いがあった。
/// 可視性と実行時強制は必ずこの関数を参照すること（discord 側ハンドラの
/// typed gate は多層防御としてそのまま残る）。
pub struct ToolPolicy {
    pub owner_only: bool,
    pub trusted_only: bool,
    /// depth >= 1 の sub-engine からブロック（Discord 送信系）。
    pub blocked_in_subengine: bool,
    /// depth >= MAX_DEPTH でブロック（ネスト上限）。
    pub depth_capped: bool,
}

pub fn tool_policy(name: &str) -> ToolPolicy {
    ToolPolicy {
        owner_only: OWNER_ONLY_ACTIONS.contains(&name),
        trusted_only: TRUSTED_ONLY_ACTIONS.contains(&name),
        blocked_in_subengine: DISCORD_ACTIONS.contains(&name),
        depth_capped: name == "spawn_subtask",
    }
}

/// tool 結果 Value から秘密鍵フィールド（`nsec`）をマスクした複製を返す。
/// 観測系（activity webhook 等の sink）へ秘密鍵を生で流さないために使う。
/// 呼び出し側は `nsec` を含むときだけ呼ぶ（clone コストを避けるため）。
fn redact_secret_fields(data: &serde_json::Value) -> serde_json::Value {
    let mut cloned = data.clone();
    if let Some(obj) = cloned.as_object_mut() {
        if obj.contains_key("nsec") {
            obj.insert(
                "nsec".to_string(),
                serde_json::Value::String("[redacted]".to_string()),
            );
        }
    }
    cloned
}

/// エラー文言から「権限拒否（実行されなかった）」を判定する。
///
/// 優先: 構造的マーカー（`REJECTION_CODE_PREFIX`）。
/// 後方互換: まだマーカー化されていない経路向けに、曖昧さの少ない明示ドメイン
/// マーカーのみを許可する（広い NL 部分一致は誤検知になるため不可）。
fn is_rejection(error: Option<&str>) -> bool {
    let Some(e) = error else {
        return false;
    };
    // 構造的シグナル（権威）。
    if e.starts_with(REJECTION_CODE_PREFIX) {
        return true;
    }
    // 後方互換の明示ドメインマーカー（未マーカー化の owner-only gateway action 等）。
    // いずれも通常の OS/ツール失敗には現れない十分に固有なトークンに限定する。
    let lower = e.to_ascii_lowercase();
    [
        "owner-only",
        "requires owner",
        "forbidden_scope",
        "redacted read requires",
    ]
    .iter()
    .any(|p| lower.contains(p))
}

/// Bridges `ActionDispatcher` to the `ActionExecutor` trait so that
/// `SkillEngine` can drive real actions.
///
/// Holds both the dispatcher and a pre-configured `ActionContext`.
/// Optionally holds `GatewayActions` to merge gateway-specific tools.
/// MCP ツール名の名前空間プレフィックス（`opencrab_mcp::MCP_TOOL_PREFIX` と一致させる。
/// actions は mcp に依存できない＝依存循環になるため定数で持つ）。
///
/// dispatch 分類でも使う: MCP ツールは運用者が繋いだ任意の外部ツールで、性質
/// （配送系か / 同ターンで結果を使うか）を静的に分類できないため、**既定 inline**
/// （安全側）にする（[`crate::subtask::SubtaskToolDispatcher::should_dispatch`]）。
pub const MCP_TOOL_PREFIX: &str = "mcp__";

pub struct BridgedExecutor {
    dispatcher: ActionDispatcher,
    context: ActionContext,
    gateway_actions: Option<Arc<dyn GatewayActions>>,
    /// MCP ツール源（`GatewayActions` 実装）。gateway_actions とは別スロット
    /// （MCP は全ターンで利用可、gateway は transport 毎で単数のため）。
    mcp_actions: Option<Arc<dyn GatewayActions>>,
    depth: u32,
    tool_event_sink: Option<Arc<dyn ToolEventSink>>,
}

impl BridgedExecutor {
    pub fn new(dispatcher: ActionDispatcher, context: ActionContext) -> Self {
        Self {
            dispatcher,
            context,
            gateway_actions: None,
            mcp_actions: None,
            depth: 0,
            tool_event_sink: None,
        }
    }

    pub fn with_depth(mut self, depth: u32) -> Self {
        self.depth = depth;
        self
    }

    pub fn with_gateway_actions(mut self, actions: Arc<dyn GatewayActions>) -> Self {
        self.gateway_actions = Some(actions);
        self
    }

    /// MCP ツール源を注入する（`mcp__<server>__<tool>` を提供する `GatewayActions`）。
    pub fn with_mcp_actions(mut self, actions: Arc<dyn GatewayActions>) -> Self {
        self.mcp_actions = Some(actions);
        self
    }

    pub fn with_tool_event_sink(mut self, sink: Arc<dyn ToolEventSink>) -> Self {
        self.tool_event_sink = Some(sink);
        self
    }

    /// dispatcher の CallerIdentity を gateway 境界の型付き caller に写像する。
    /// CoAgent の agent_id は保存する（旧 `__caller` 文字列注入では落ちていた）。
    fn gateway_call_context(&self) -> opencrab_gateway::GatewayCallContext {
        let caller = match &self.context.caller {
            crate::traits::CallerIdentity::Owner => opencrab_gateway::GatewayCaller::Owner,
            crate::traits::CallerIdentity::Agent => opencrab_gateway::GatewayCaller::Agent,
            crate::traits::CallerIdentity::CoAgent { agent_id } => {
                opencrab_gateway::GatewayCaller::CoAgent {
                    agent_id: agent_id.clone(),
                }
            }
            crate::traits::CallerIdentity::TrustedUser => {
                opencrab_gateway::GatewayCaller::TrustedUser
            }
        };
        opencrab_gateway::GatewayCallContext {
            caller,
            session_id: self.context.session_id.clone(),
            depth: self.depth,
            agent_id: self.context.agent_id.clone(),
            // 合成 gateway 自身のハンドルを子へ渡す（RFC #152 S2）。sub-engine を
            // 構築する `spawn_subtask` が「自分を包む合成 gateway」を辿れるように
            // する注入口。Arc は本 executor が所有し、ここでは clone して短命な
            // ctx に載せるだけ（自己参照 Arc ではない＝サイクルなし）。
            root_gateway: self.gateway_actions.clone(),
        }
    }

    fn caller_is_owner(&self) -> bool {
        matches!(self.context.caller, crate::traits::CallerIdentity::Owner)
    }

    fn caller_is_trusted(&self) -> bool {
        matches!(
            self.context.caller,
            crate::traits::CallerIdentity::Owner
                | crate::traits::CallerIdentity::CoAgent { .. }
                | crate::traits::CallerIdentity::TrustedUser
        )
    }

    /// このコンテキスト（caller/depth）で name が可視・実行可能か（#45）。
    /// list_tools と dispatch_inner が同一のポリシー判定を共有するための述語。
    fn policy_allows(&self, name: &str) -> bool {
        let policy = tool_policy(name);
        if policy.owner_only && !self.caller_is_owner() {
            return false;
        }
        if policy.trusted_only && !self.caller_is_trusted() {
            return false;
        }
        if self.depth >= 1 && policy.blocked_in_subengine {
            return false;
        }
        if self.depth >= MAX_DEPTH && policy.depth_capped {
            return false;
        }
        true
    }

    /// 実際のディスパッチ本体（dispatcher → gateway fallback）。
    /// instrumentation は `ActionExecutor::execute` 側で wrap する。
    async fn dispatch_inner(&self, name: &str, args: &serde_json::Value) -> CoreActionResult {
        // 可視性（list_tools）と同じポリシー表を実行時にも強制する（#45）。
        // 一覧から隠しただけでは、モデルが名前を記憶で呼んだ場合に素通しになる。
        let reject = |msg: String| CoreActionResult {
            success: false,
            data: serde_json::Value::Null,
            error: Some(format!("{REJECTION_CODE_PREFIX}{msg}")),
        };
        let policy = tool_policy(name);
        if policy.owner_only && !self.caller_is_owner() {
            return reject(format!("action '{name}' requires owner"));
        }
        if policy.trusted_only && !self.caller_is_trusted() {
            return reject(format!(
                "action '{name}' requires a trusted caller (owner/co_agent/trusted_user)"
            ));
        }
        if self.depth >= 1 && policy.blocked_in_subengine {
            return reject(format!(
                "action '{name}' is not available in sub-engines (depth {})",
                self.depth
            ));
        }
        if self.depth >= MAX_DEPTH && policy.depth_capped {
            return reject(format!(
                "{name} is not available at depth {} (max nesting: {MAX_DEPTH})",
                self.depth
            ));
        }

        // MCP ツール（mcp__ プレフィックス）は MCP プロバイダへ振り分ける。gateway が
        // unknown を返す前に処理する（名前空間は dispatcher/gateway と重ならない）。
        if name.starts_with(MCP_TOOL_PREFIX) {
            if let Some(ref mcp) = self.mcp_actions {
                let ctx = self.gateway_call_context();
                let r = mcp.execute(name, args, &ctx).await;
                return CoreActionResult {
                    success: r.success,
                    data: r.data.unwrap_or(serde_json::Value::Null),
                    error: r.error,
                };
            }
        }

        // Try dispatcher first. フォールバック判定は登録有無で行う
        // （"Unknown action" エラー文言の文字列比較は、実アクションが同文を
        // 返した場合に gateway へ誤ルートするため廃止 — #36）。
        if self.dispatcher.has_action(name) {
            return self
                .dispatcher
                .execute(name, args, &self.context)
                .await
                .into();
        }

        // Fallback to gateway actions.
        if let Some(ref gw) = self.gateway_actions {
            // 実行コンテキストは型付きで渡す。LLM 由来の args には混ぜない（#36）。
            let ctx = self.gateway_call_context();
            let gw_result = gw.execute(name, args, &ctx).await;
            return CoreActionResult {
                success: gw_result.success,
                data: gw_result.data.unwrap_or(serde_json::Value::Null),
                error: gw_result.error,
            };
        }

        // dispatcher にも gateway にも無い。
        CoreActionResult {
            success: false,
            data: serde_json::Value::Null,
            error: Some(format!("Unknown action: {name}")),
        }
    }
}

impl BridgedExecutor {
    /// instrumentation 付き実行本体。
    ///
    /// `tool_call_id` は LLM 由来の元 ID を伝播するための相関キー。`Some(id)` なら
    /// その ID を webhook/トレースの相関に使う（skill engine の tool_call.id と一致）。
    /// `None`（id を持たない直接呼び出し）のときのみ合成 UUID を生成し、ペイロード上は
    /// `correlation = "synthetic"` として区別できるようにする。
    async fn execute_instrumented(
        &self,
        name: &str,
        args: &serde_json::Value,
        tool_call_id: Option<&str>,
    ) -> CoreActionResult {
        let Some(sink) = self.tool_event_sink.clone() else {
            return self.dispatch_inner(name, args).await;
        };
        // 相関 ID: LLM 由来 ID があれば伝播、無ければ合成（同 start/terminal で一致）。
        let synthetic;
        let call_id: &str = match tool_call_id {
            Some(id) if !id.is_empty() => id,
            _ => {
                synthetic = uuid::Uuid::new_v4().to_string();
                &synthetic
            }
        };
        let started_at = chrono::Utc::now().to_rfc3339();
        let session_id = self.context.session_id.as_deref();
        sink.on_event(&ToolEvent {
            tool_name: name,
            tool_call_id: call_id,
            agent_id: &self.context.agent_id,
            session_id,
            depth: self.depth,
            status: ToolEventStatus::Started,
            started_at: &started_at,
            duration_ms: None,
            args,
            result: None,
            error: None,
        });
        let start = std::time::Instant::now();
        let result = self.dispatch_inner(name, args).await;
        let duration_ms = start.elapsed().as_millis() as u64;
        let status = if result.success {
            ToolEventStatus::Completed
        } else if is_rejection(result.error.as_deref()) {
            // permission-policy 拒否を観測可能にする（raw URL/token は載せない）。
            tracing::debug!(
                tool = %name,
                tool_call_id = %call_id,
                depth = self.depth,
                "tool call classified as rejected (policy)"
            );
            ToolEventStatus::Rejected
        } else {
            ToolEventStatus::Failed
        };
        // 秘密鍵（nsec 等）を含む結果は sink（activity webhook 等の観測系）へ生で流さない。
        // 含む場合だけ redact したコピーを作って渡す（通常は clone を避ける）。
        let redacted;
        let sink_result: &serde_json::Value = if result.data.get("nsec").is_some() {
            redacted = redact_secret_fields(&result.data);
            &redacted
        } else {
            &result.data
        };
        sink.on_event(&ToolEvent {
            tool_name: name,
            tool_call_id: call_id,
            agent_id: &self.context.agent_id,
            session_id,
            depth: self.depth,
            status,
            started_at: &started_at,
            duration_ms: Some(duration_ms),
            args,
            result: Some(sink_result),
            error: result.error.as_deref(),
        });
        result
    }
}

#[async_trait]
impl ActionExecutor for BridgedExecutor {
    async fn execute(&self, name: &str, args: &serde_json::Value) -> CoreActionResult {
        self.execute_instrumented(name, args, None).await
    }

    async fn execute_with_id(
        &self,
        name: &str,
        args: &serde_json::Value,
        tool_call_id: &str,
    ) -> CoreActionResult {
        self.execute_instrumented(name, args, Some(tool_call_id))
            .await
    }

    fn list_tools(&self) -> Vec<FunctionDefinition> {
        // 空 description は None にする（旧 to_function_def の挙動を踏襲）。
        let opt_desc = |d: String| if d.is_empty() { None } else { Some(d) };

        let mut tools: Vec<FunctionDefinition> = self
            .dispatcher
            .get_definitions(&[])
            .into_iter()
            .filter(|d| self.policy_allows(&d.name))
            .map(|d| FunctionDefinition {
                name: d.name,
                description: opt_desc(d.description),
                parameters: d.parameters,
            })
            .collect();

        // Merge gateway action definitions（同じポリシー述語でフィルタ）。
        if let Some(ref gw) = self.gateway_actions {
            for def in gw.definitions() {
                if !self.policy_allows(&def.name) {
                    continue;
                }
                tools.push(FunctionDefinition {
                    name: def.name,
                    description: opt_desc(def.description),
                    parameters: def.parameters,
                });
            }
        }

        // Merge MCP tool definitions。MCP 側の trusted_only ゲートはプロバイダが
        // caller で既にフィルタ済み（本ターンの caller で構築される）。静的 policy も一応適用。
        if let Some(ref mcp) = self.mcp_actions {
            for def in mcp.definitions() {
                if !self.policy_allows(&def.name) {
                    continue;
                }
                tools.push(FunctionDefinition {
                    name: def.name,
                    description: opt_desc(def.description),
                    parameters: def.parameters,
                });
            }
        }

        tools
    }
}

impl From<ActionsActionResult> for CoreActionResult {
    fn from(ar: ActionsActionResult) -> Self {
        CoreActionResult {
            success: ar.success,
            data: ar.data.unwrap_or(serde_json::Value::Null),
            error: ar.error,
        }
    }
}

// Static assertion: BridgedExecutor must be Send + Sync (required by ActionExecutor).
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<BridgedExecutor>();
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::CallerIdentity;
    use opencrab_gateway::{GatewayActionDef, GatewayActionResult, GatewayCallContext};
    use serde_json::json;
    use std::sync::Mutex;

    // ---- RFC #152 S2: 合成 gateway 注入 + deny-by-default 最外周フィルタ ----

    /// server ツール（nostr_generate_key）と transport ツール（report_progress）と、
    /// 開放してはならないツール（send_ui）を同時に提供する、合成 gateway のフェイク。
    /// `SystemGatewayActions`（server ツール + inner の union）の到達性だけを模す。
    struct FakeCompositeGateway;

    #[async_trait]
    impl GatewayActions for FakeCompositeGateway {
        fn definitions(&self) -> Vec<GatewayActionDef> {
            ["nostr_generate_key", "report_progress", "send_ui"]
                .iter()
                .map(|n| GatewayActionDef {
                    name: n.to_string(),
                    description: format!("{n} desc"),
                    parameters: json!({"type": "object", "properties": {}}),
                })
                .collect()
        }
        async fn execute(
            &self,
            name: &str,
            _args: &serde_json::Value,
            _ctx: &GatewayCallContext,
        ) -> GatewayActionResult {
            // 合成 gateway に到達したことを data で可視化する。
            GatewayActionResult {
                success: true,
                data: Some(json!({ "reached": name })),
                error: None,
            }
        }
    }

    /// deny-by-default: 合成 gateway の definitions 和集合から、許可リストの
    /// ツールだけが sub-engine に見える（send_ui 等は消える）。
    #[test]
    fn subengine_definitions_only_expose_allowlisted_tools() {
        let sub = SubEngineGatewayActions::new(Arc::new(FakeCompositeGateway));
        let names: Vec<String> = sub.definitions().into_iter().map(|d| d.name).collect();
        assert!(
            names.contains(&"report_progress".to_string()),
            "report_progress must remain reachable"
        );
        assert!(
            names.contains(&"nostr_generate_key".to_string()),
            "nostr_generate_key must be reachable after S2 triage"
        );
        assert!(
            !names.contains(&"send_ui".to_string()),
            "send_ui must NOT be exposed to the sub-engine"
        );
    }

    /// 許可された server ツール（nostr_generate_key）は合成 gateway へ到達・実行できる。
    #[tokio::test]
    async fn subengine_reaches_allowed_server_tool() {
        let sub = SubEngineGatewayActions::new(Arc::new(FakeCompositeGateway));
        let ctx = GatewayCallContext::new(opencrab_gateway::GatewayCaller::Agent, "a1")
            .with_session_id("subtask-x")
            .with_depth(1);
        let r = sub.execute("nostr_generate_key", &json!({}), &ctx).await;
        assert!(
            r.success,
            "nostr_generate_key must reach the composite gateway"
        );
        assert_eq!(r.data.unwrap()["reached"], "nostr_generate_key");
    }

    /// 許可されていない server/transport ツール（send_ui）は depth>=1 で到達不能
    /// （rejected: マーカー付きで拒否される。合成 gateway には届かない）。
    #[tokio::test]
    async fn subengine_blocks_disallowed_tool() {
        let sub = SubEngineGatewayActions::new(Arc::new(FakeCompositeGateway));
        let ctx = GatewayCallContext::new(opencrab_gateway::GatewayCaller::Agent, "a1")
            .with_session_id("subtask-x")
            .with_depth(1);
        let r = sub.execute("send_ui", &json!({}), &ctx).await;
        assert!(!r.success, "send_ui must be blocked in the sub-engine");
        let err = r.error.unwrap();
        assert!(
            err.starts_with(REJECTION_CODE_PREFIX),
            "block must be a structural rejection, got: {err}"
        );
        // 合成 gateway の実行痕跡（reached）が data に無い＝届いていない。
        assert!(r.data.is_none());

        // 未知名（実在しないツール）は **拒否マーカーを付けない**。分類器が幻覚の
        // ツール名を「権限で弾かれた」と誤分類すると、リトライや権限系の扱いが壊れる。
        let unknown = sub
            .execute("no_such_tool_at_all", &serde_json::json!({}), &ctx)
            .await;
        assert!(!unknown.success);
        let unknown_err = unknown.error.unwrap();
        assert!(
            !unknown_err.starts_with(REJECTION_CODE_PREFIX),
            "未知名は権限拒否として扱わない: {unknown_err}"
        );
        assert!(
            unknown_err.contains("Unknown gateway action"),
            "未知名は通常の失敗として返す: {unknown_err}"
        );
    }

    /// **許可リストは MCP スロットを覆わない**（危険の所在を固定する）。
    ///
    /// `BridgedExecutor` は gateway と MCP を別スロットで持つ。sub-engine の許可リストは
    /// gateway スロットに被せるものなので、MCP ツールは**素通りする**。したがって
    /// 「sub-engine は最小権限」を保つには、MCP を注入する側（`crates/server` の応答生成）で
    /// **深さを見て注入しない**必要がある。ここではその前提（＝許可リストに頼れないこと）を
    /// 固定する。将来 MCP を許可リスト経由に通す設計へ変えたら、このテストが落ちるので
    /// そのとき深さゲートを緩められる。
    #[tokio::test]
    async fn allowlist_does_not_cover_the_mcp_slot() {
        // gateway 側は許可リストで絞る。
        let gateway: Arc<dyn GatewayActions> =
            Arc::new(SubEngineGatewayActions::new(Arc::new(FakeCompositeGateway)));
        // MCP 側は絞られていない別スロット（同じフェイクを流用して「素通り」を見る）。
        let mcp: Arc<dyn GatewayActions> = Arc::new(FakeCompositeGateway);

        let gateway_names: Vec<String> =
            gateway.definitions().into_iter().map(|d| d.name).collect();
        assert!(
            !gateway_names.contains(&"send_ui".to_string()),
            "gateway スロットは許可リストで絞られる: {gateway_names:?}"
        );

        let mcp_names: Vec<String> = mcp.definitions().into_iter().map(|d| d.name).collect();
        assert!(
            mcp_names.contains(&"send_ui".to_string()),
            "MCP スロットは許可リストを通らない（この前提が崩れたら深さゲートを見直す）: {mcp_names:?}"
        );
    }

    /// report_progress は引き続き transport gateway へ委譲され動く（S1 挙動不変）。
    #[tokio::test]
    async fn subengine_report_progress_still_reaches_inner() {
        let sub = SubEngineGatewayActions::new(Arc::new(FakeCompositeGateway));
        let ctx = GatewayCallContext::new(opencrab_gateway::GatewayCaller::Agent, "a1")
            .with_session_id("subtask-x")
            .with_depth(1);
        let r = sub.execute("report_progress", &json!({}), &ctx).await;
        assert!(r.success);
        assert_eq!(r.data.unwrap()["reached"], "report_progress");
    }

    #[test]
    fn test_redact_secret_fields_masks_nsec() {
        let data = json!({"nsec": "nsec1supersecret", "npub": "npub1abc", "pubkey": "hex"});
        let red = redact_secret_fields(&data);
        assert_eq!(red["nsec"], "[redacted]");
        // 非秘密フィールドは保持。
        assert_eq!(red["npub"], "npub1abc");
        assert_eq!(red["pubkey"], "hex");
        // 秘密が残らない。
        assert!(!red.to_string().contains("supersecret"));
        // nsec が無ければそのまま。
        let plain = json!({"url": "https://x"});
        assert_eq!(redact_secret_fields(&plain), plain);
    }

    /// テスト用GatewayActionsモック
    struct MockGatewayActions;

    #[async_trait]
    impl GatewayActions for MockGatewayActions {
        fn definitions(&self) -> Vec<GatewayActionDef> {
            vec![
                GatewayActionDef {
                    name: "gw_action_a".to_string(),
                    description: "Gateway action A".to_string(),
                    parameters: json!({"type": "object", "properties": {}}),
                },
                GatewayActionDef {
                    name: "gw_action_b".to_string(),
                    description: "Gateway action B".to_string(),
                    parameters: json!({"type": "object", "properties": {}}),
                },
            ]
        }

        async fn execute(
            &self,
            name: &str,
            _args: &serde_json::Value,
            _ctx: &opencrab_gateway::GatewayCallContext,
        ) -> GatewayActionResult {
            match name {
                "gw_action_a" => GatewayActionResult {
                    success: true,
                    data: Some(json!({"result": "from_gateway"})),
                    error: None,
                },
                _ => GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!("Unknown gateway action: {name}")),
                },
            }
        }
    }

    /// Discord 送信系アクションを含むモック（depth ゲートの検証用）。
    struct MockGatewayDiscord;

    #[async_trait]
    impl GatewayActions for MockGatewayDiscord {
        fn definitions(&self) -> Vec<GatewayActionDef> {
            vec![
                GatewayActionDef {
                    name: "request_peer_review".to_string(),
                    description: "peer review".to_string(),
                    parameters: json!({"type": "object", "properties": {}}),
                },
                GatewayActionDef {
                    name: "report_progress".to_string(),
                    description: "progress".to_string(),
                    parameters: json!({"type": "object", "properties": {}}),
                },
            ]
        }

        async fn execute(
            &self,
            _name: &str,
            _args: &serde_json::Value,
            _ctx: &opencrab_gateway::GatewayCallContext,
        ) -> GatewayActionResult {
            GatewayActionResult {
                success: true,
                data: None,
                error: None,
            }
        }
    }

    /// update_heartbeat_instructions / read_heartbeat_instructions を含むモック。
    struct MockGatewayHeartbeat;

    #[async_trait]
    impl GatewayActions for MockGatewayHeartbeat {
        fn definitions(&self) -> Vec<GatewayActionDef> {
            vec![
                GatewayActionDef {
                    name: "update_heartbeat_instructions".to_string(),
                    description: "update".to_string(),
                    parameters: json!({"type": "object", "properties": {}}),
                },
                GatewayActionDef {
                    name: "read_heartbeat_instructions".to_string(),
                    description: "read".to_string(),
                    parameters: json!({"type": "object", "properties": {}}),
                },
            ]
        }

        async fn execute(
            &self,
            _name: &str,
            _args: &serde_json::Value,
            _ctx: &opencrab_gateway::GatewayCallContext,
        ) -> GatewayActionResult {
            GatewayActionResult {
                success: true,
                data: None,
                error: None,
            }
        }
    }

    fn test_context_with_caller(caller: CallerIdentity) -> (tempfile::TempDir, ActionContext) {
        let conn = opencrab_db::init_memory().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let ws = opencrab_core::workspace::Workspace::from_root(dir.path()).unwrap();
        let ctx = ActionContext {
            caller,
            agent_id: "agent-1".to_string(),
            agent_name: "Test Agent".to_string(),
            session_id: Some("session-1".to_string()),
            db: opencrab_db::Db::from_connection(conn),
            workspace: std::sync::Arc::new(ws),
            last_metrics_id: std::sync::Arc::new(std::sync::Mutex::new(None)),
            model_override: std::sync::Arc::new(std::sync::Mutex::new(None)),
            current_purpose: std::sync::Arc::new(std::sync::Mutex::new("conversation".to_string())),
            runtime_info: std::sync::Arc::new(std::sync::Mutex::new(crate::RuntimeInfo {
                default_model: "mock:test-model".to_string(),
                active_model: None,
                available_providers: vec!["mock".to_string()],
                gateway: "test".to_string(),
            })),
        };
        (dir, ctx)
    }

    fn test_context() -> (tempfile::TempDir, ActionContext) {
        let conn = opencrab_db::init_memory().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let ws = opencrab_core::workspace::Workspace::from_root(dir.path()).unwrap();
        let ctx = ActionContext {
            caller: CallerIdentity::Owner,
            agent_id: "agent-1".to_string(),
            agent_name: "Test Agent".to_string(),
            session_id: Some("session-1".to_string()),
            db: opencrab_db::Db::from_connection(conn),
            workspace: std::sync::Arc::new(ws),
            last_metrics_id: std::sync::Arc::new(std::sync::Mutex::new(None)),
            model_override: std::sync::Arc::new(std::sync::Mutex::new(None)),
            current_purpose: std::sync::Arc::new(std::sync::Mutex::new("conversation".to_string())),
            runtime_info: std::sync::Arc::new(std::sync::Mutex::new(crate::RuntimeInfo {
                default_model: "mock:test-model".to_string(),
                active_model: None,
                available_providers: vec!["mock".to_string()],
                gateway: "test".to_string(),
            })),
        };
        (dir, ctx)
    }

    // ---- list_tools ----

    #[test]
    fn test_list_tools_without_gateway_actions() {
        let (_dir, ctx) = test_context();
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx);

        let tools = executor.list_tools();
        // ディスパッチャーのアクションのみ
        assert!(!tools.is_empty());
        assert!(tools.iter().all(|t| t.name != "gw_action_a"));
    }

    #[test]
    fn test_list_tools_merges_gateway_actions() {
        let (_dir, ctx) = test_context();
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
            .with_gateway_actions(Arc::new(MockGatewayActions));

        let tools = executor.list_tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();

        // ゲートウェイアクションもマージされる
        assert!(names.contains(&"gw_action_a"));
        assert!(names.contains(&"gw_action_b"));
    }

    // ---- execute ----

    #[tokio::test]
    async fn test_execute_dispatcher_action() {
        let (_dir, ctx) = test_context();
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
            .with_gateway_actions(Arc::new(MockGatewayActions));

        // ディスパッチャーに存在するアクションはディスパッチャーで処理される
        let result = executor
            .execute("generate_inner_voice", &json!({"thought": "hello"}))
            .await;
        assert!(result.success);
    }

    #[tokio::test]
    async fn test_execute_falls_back_to_gateway_actions() {
        let (_dir, ctx) = test_context();
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
            .with_gateway_actions(Arc::new(MockGatewayActions));

        // ディスパッチャーに存在しないアクションはゲートウェイにフォールバック
        let result = executor.execute("gw_action_a", &json!({})).await;
        assert!(result.success);
        assert_eq!(result.data["result"], "from_gateway");
    }

    #[test]
    fn test_peer_review_visible_at_depth0_hidden_in_subengine() {
        let (_dir, ctx) = test_context();
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
            .with_gateway_actions(Arc::new(MockGatewayDiscord));
        let names: Vec<String> = executor
            .list_tools()
            .iter()
            .map(|t| t.name.clone())
            .collect();
        assert!(names.contains(&"request_peer_review".to_string()));
        assert!(names.contains(&"report_progress".to_string()));

        // depth >= 1 の sub-engine からはピアレビュー依頼が見えない
        let (_dir2, sub_ctx) = test_context();
        let sub = BridgedExecutor::new(ActionDispatcher::new(), sub_ctx)
            .with_gateway_actions(Arc::new(MockGatewayDiscord))
            .with_depth(1);
        let names: Vec<String> = sub.list_tools().iter().map(|t| t.name.clone()).collect();
        assert!(!names.contains(&"request_peer_review".to_string()));
        assert!(names.contains(&"report_progress".to_string()));
    }

    /// 定義から隠すだけでなく、名前指定の実行も depth ゲートで拒否されること
    /// （モデルは親コンテキストの記憶でツール名を呼ぶことがある）。
    #[tokio::test]
    async fn test_peer_review_execute_rejected_in_subengine() {
        let (_dir, ctx) = test_context();
        let sub = BridgedExecutor::new(ActionDispatcher::new(), ctx)
            .with_gateway_actions(Arc::new(MockGatewayDiscord))
            .with_depth(1);
        let result = sub.execute("request_peer_review", &json!({})).await;
        assert!(!result.success);
        let err = result.error.unwrap();
        assert!(err.starts_with(REJECTION_CODE_PREFIX));
        assert!(err.contains("not available in sub-engines"));

        // ブロック対象外の gateway action は depth 1 でも実行できる
        let result = sub.execute("report_progress", &json!({})).await;
        assert!(result.success);
    }

    #[tokio::test]
    async fn test_execute_unknown_action_without_gateway() {
        let (_dir, ctx) = test_context();
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx);

        // ゲートウェイなし → ディスパッチャーのエラーがそのまま返る
        let result = executor.execute("nonexistent", &json!({})).await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Unknown action"));
    }

    #[tokio::test]
    async fn test_execute_unknown_action_with_gateway() {
        let (_dir, ctx) = test_context();
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
            .with_gateway_actions(Arc::new(MockGatewayActions));

        // ディスパッチャーにもゲートウェイにも無い → ゲートウェイのエラーが返る
        let result = executor.execute("totally_unknown", &json!({})).await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Unknown gateway action"));
    }

    /// create_skill / execute_skill を含むモック
    struct MockGatewayActionsWithSkills;

    #[async_trait]
    impl GatewayActions for MockGatewayActionsWithSkills {
        fn definitions(&self) -> Vec<GatewayActionDef> {
            vec![
                GatewayActionDef {
                    name: "gw_action_a".to_string(),
                    description: "Gateway action A".to_string(),
                    parameters: json!({"type": "object", "properties": {}}),
                },
                GatewayActionDef {
                    name: "create_skill".to_string(),
                    description: "Create a skill".to_string(),
                    parameters: json!({"type": "object", "properties": {}}),
                },
                GatewayActionDef {
                    name: "execute_skill".to_string(),
                    description: "Execute a skill".to_string(),
                    parameters: json!({"type": "object", "properties": {}}),
                },
            ]
        }

        async fn execute(
            &self,
            _name: &str,
            _args: &serde_json::Value,
            _ctx: &opencrab_gateway::GatewayCallContext,
        ) -> GatewayActionResult {
            GatewayActionResult {
                success: true,
                data: None,
                error: None,
            }
        }
    }

    #[test]
    fn test_list_tools_trusted_user_sees_skill_actions() {
        let (_dir, ctx) = test_context_with_caller(CallerIdentity::TrustedUser);
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
            .with_gateway_actions(Arc::new(MockGatewayActionsWithSkills));

        let tools = executor.list_tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();

        assert!(
            names.contains(&"create_skill"),
            "TrustedUser should see create_skill"
        );
        assert!(
            names.contains(&"execute_skill"),
            "TrustedUser should see execute_skill"
        );
        assert!(
            names.contains(&"gw_action_a"),
            "TrustedUser should see regular gateway actions"
        );
    }

    #[test]
    fn test_list_tools_agent_cannot_see_skill_actions() {
        let (_dir, ctx) = test_context_with_caller(CallerIdentity::Agent);
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
            .with_gateway_actions(Arc::new(MockGatewayActionsWithSkills));

        let tools = executor.list_tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();

        assert!(
            !names.contains(&"create_skill"),
            "Agent should NOT see create_skill"
        );
        assert!(
            !names.contains(&"execute_skill"),
            "Agent should NOT see execute_skill"
        );
        assert!(
            names.contains(&"gw_action_a"),
            "Agent should still see regular gateway actions"
        );
    }

    // ---- owner_only_actions filtering ----

    #[test]
    fn test_list_tools_owner_sees_update_instructions() {
        let (_dir, ctx) = test_context_with_caller(CallerIdentity::Owner);
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx);

        let tools = executor.list_tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(
            names.contains(&"update_instructions"),
            "Owner should see update_instructions"
        );
    }

    #[test]
    fn test_list_tools_agent_cannot_see_update_instructions() {
        let (_dir, ctx) = test_context_with_caller(CallerIdentity::Agent);
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx);

        let tools = executor.list_tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(
            !names.contains(&"update_instructions"),
            "Agent should NOT see update_instructions"
        );
    }

    /// `configure_llm_provider`（#118）は owner 限定。gateway が定義を出しても
    /// 非 owner には可視化されず、名前指定の実行も dispatch で拒否されること。
    #[tokio::test]
    async fn test_configure_llm_provider_is_owner_only() {
        struct GwConfig;
        #[async_trait::async_trait]
        impl GatewayActions for GwConfig {
            fn definitions(&self) -> Vec<GatewayActionDef> {
                vec![GatewayActionDef {
                    name: "configure_llm_provider".to_string(),
                    description: "x".to_string(),
                    parameters: json!({"type": "object"}),
                }]
            }
            async fn execute(
                &self,
                _n: &str,
                _a: &serde_json::Value,
                _c: &opencrab_gateway::GatewayCallContext,
            ) -> GatewayActionResult {
                GatewayActionResult {
                    success: true,
                    data: Some(json!({"reached_gateway": true})),
                    error: None,
                }
            }
        }

        // Agent: 一覧に出ず、名前指定の実行も owner ゲートで拒否される。
        let (_d, actx) = test_context_with_caller(CallerIdentity::Agent);
        let agent_exec = BridgedExecutor::new(ActionDispatcher::new(), actx)
            .with_gateway_actions(Arc::new(GwConfig));
        assert!(
            !agent_exec
                .list_tools()
                .iter()
                .any(|t| t.name == "configure_llm_provider"),
            "Agent must NOT see configure_llm_provider"
        );
        let r = agent_exec
            .execute("configure_llm_provider", &json!({"provider": "acp"}))
            .await;
        assert!(!r.success, "Agent execution must be rejected");
        assert!(r.error.unwrap().to_lowercase().contains("owner"));

        // Owner: 可視化され、実行は gateway に到達する。
        let (_d2, octx) = test_context_with_caller(CallerIdentity::Owner);
        let owner_exec = BridgedExecutor::new(ActionDispatcher::new(), octx)
            .with_gateway_actions(Arc::new(GwConfig));
        assert!(
            owner_exec
                .list_tools()
                .iter()
                .any(|t| t.name == "configure_llm_provider"),
            "Owner should see configure_llm_provider"
        );
        let r2 = owner_exec
            .execute("configure_llm_provider", &json!({"provider": "acp"}))
            .await;
        assert!(r2.success, "Owner execution should reach the gateway");
        assert_eq!(r2.data["reached_gateway"], true);
    }

    /// 設定変更系（#116）は owner 限定であること（ポリシー表の権威）。
    #[test]
    fn test_settings_tools_are_owner_only() {
        for name in [
            "configure_llm_provider",
            "manage_allowed_commands",
            "configure_nostr",
            "configure_self",
            "configure_mcp_server",
        ] {
            let p = tool_policy(name);
            assert!(p.owner_only, "{name} must be owner_only");
            assert!(
                !p.trusted_only,
                "{name} should be gated by owner_only, not trusted_only"
            );
        }
    }

    #[test]
    fn test_list_tools_owner_sees_update_heartbeat_instructions() {
        let (_dir, ctx) = test_context_with_caller(CallerIdentity::Owner);
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
            .with_gateway_actions(Arc::new(MockGatewayHeartbeat));
        let names: Vec<String> = executor.list_tools().into_iter().map(|t| t.name).collect();
        assert!(names.iter().any(|n| n == "update_heartbeat_instructions"));
        assert!(names.iter().any(|n| n == "read_heartbeat_instructions"));
    }

    #[test]
    fn test_list_tools_agent_cannot_see_heartbeat_actions() {
        let (_dir, ctx) = test_context_with_caller(CallerIdentity::Agent);
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
            .with_gateway_actions(Arc::new(MockGatewayHeartbeat));
        let names: Vec<String> = executor.list_tools().into_iter().map(|t| t.name).collect();
        // Agent (non-owner, non-trusted) sees neither.
        assert!(!names.iter().any(|n| n == "update_heartbeat_instructions"));
        assert!(!names.iter().any(|n| n == "read_heartbeat_instructions"));
    }

    #[test]
    fn test_list_tools_trusted_user_heartbeat_read_only() {
        let (_dir, ctx) = test_context_with_caller(CallerIdentity::TrustedUser);
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
            .with_gateway_actions(Arc::new(MockGatewayHeartbeat));
        let names: Vec<String> = executor.list_tools().into_iter().map(|t| t.name).collect();
        // TrustedUser can read but not write (write is owner-only).
        assert!(names.iter().any(|n| n == "read_heartbeat_instructions"));
        assert!(!names.iter().any(|n| n == "update_heartbeat_instructions"));
    }

    #[test]
    fn test_list_tools_trusted_user_cannot_see_update_instructions() {
        let (_dir, ctx) = test_context_with_caller(CallerIdentity::TrustedUser);
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx);

        let tools = executor.list_tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(
            !names.contains(&"update_instructions"),
            "TrustedUser should NOT see update_instructions"
        );
    }

    // ---- #36: typed GatewayCallContext ----

    /// gateway に渡った ctx / args を記録するモック。
    struct CtxRecordingGateway {
        last_ctx: Mutex<Option<opencrab_gateway::GatewayCallContext>>,
        last_args: Mutex<Option<serde_json::Value>>,
    }

    #[async_trait]
    impl GatewayActions for CtxRecordingGateway {
        fn definitions(&self) -> Vec<GatewayActionDef> {
            vec![GatewayActionDef {
                name: "ctx_probe".to_string(),
                description: "probe".to_string(),
                parameters: json!({"type": "object", "properties": {}}),
            }]
        }
        async fn execute(
            &self,
            _name: &str,
            args: &serde_json::Value,
            ctx: &opencrab_gateway::GatewayCallContext,
        ) -> GatewayActionResult {
            *self.last_ctx.lock().unwrap() = Some(ctx.clone());
            *self.last_args.lock().unwrap() = Some(args.clone());
            GatewayActionResult {
                success: true,
                data: None,
                error: None,
            }
        }
    }

    /// CoAgent の agent_id が境界を越えて保存されること（旧 `__caller` 文字列注入では
    /// "co_agent" に落ちていた）と、LLM 由来 args に実行コンテキストが混ざらないこと。
    #[tokio::test]
    async fn test_gateway_receives_typed_context_preserving_coagent_id() {
        let (_dir, ctx) = test_context_with_caller(CallerIdentity::CoAgent {
            agent_id: "co-agent-42".to_string(),
        });
        let gw = Arc::new(CtxRecordingGateway {
            last_ctx: Mutex::new(None),
            last_args: Mutex::new(None),
        });
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
            .with_gateway_actions(gw.clone())
            .with_depth(1);

        let result = executor.execute("ctx_probe", &json!({"x": 1})).await;
        assert!(result.success);

        let seen = gw.last_ctx.lock().unwrap().clone().unwrap();
        assert_eq!(
            seen.caller,
            opencrab_gateway::GatewayCaller::CoAgent {
                agent_id: "co-agent-42".to_string()
            }
        );
        assert_eq!(seen.session_id.as_deref(), Some("session-1"));
        assert_eq!(seen.depth, 1);
        assert_eq!(seen.agent_id, "agent-1");

        // args は LLM 由来のものがそのまま渡り、__* キーは注入されない。
        let args = gw.last_args.lock().unwrap().clone().unwrap();
        assert_eq!(args, json!({"x": 1}));
    }

    /// RFC #152 S2: gateway の `execute` に渡る ctx に、合成 gateway 自身への
    /// ハンドル（`root_gateway`）が注入されること。sub-engine を構築する
    /// `spawn_subtask` がこれを辿って合成 gateway を wrap できる（自己参照 Arc 不要
    /// = Arc は本 executor が所有し、ctx は clone を短命に運ぶだけ）。
    #[tokio::test]
    async fn test_gateway_ctx_carries_root_gateway_handle() {
        let (_dir, ctx) = test_context();
        let gw = Arc::new(CtxRecordingGateway {
            last_ctx: Mutex::new(None),
            last_args: Mutex::new(None),
        });
        let executor =
            BridgedExecutor::new(ActionDispatcher::new(), ctx).with_gateway_actions(gw.clone());
        let r = executor.execute("ctx_probe", &json!({})).await;
        assert!(r.success);
        let seen = gw.last_ctx.lock().unwrap().clone().unwrap();
        assert!(
            seen.root_gateway.is_some(),
            "root_gateway handle must be injected so a sub-engine can wrap the composite gateway"
        );
    }

    /// root_gateway 未注入（gateway_actions 無し）の executor は、ctx.root_gateway が
    /// None のまま（後方互換 = 非破壊）。
    #[tokio::test]
    async fn test_gateway_ctx_root_gateway_none_without_gateway_actions() {
        // gateway_actions を付けない executor では、そもそも gateway.execute へ
        // 到達しないため、ここでは gateway_call_context() の生成結果を直接確認する。
        let (_dir, ctx) = test_context();
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx);
        let call_ctx = executor.gateway_call_context();
        assert!(
            call_ctx.root_gateway.is_none(),
            "no gateway_actions => root_gateway must stay None (backward compatible)"
        );
    }

    /// "Unknown action: {name}" と同文のエラーを返す実アクションが gateway に
    /// 誤ルートされないこと（旧実装はエラー文言の文字列比較で判定していた）。
    struct UnknownEchoAction;
    #[async_trait]
    impl crate::traits::Action for UnknownEchoAction {
        fn name(&self) -> &str {
            "unknown_echo"
        }
        fn description(&self) -> &str {
            "returns an error that mimics the dispatcher's unknown-action message"
        }
        fn parameters(&self) -> serde_json::Value {
            json!({"type": "object", "properties": {}})
        }
        async fn execute(
            &self,
            _args: &serde_json::Value,
            _ctx: &crate::traits::ActionContext,
        ) -> crate::traits::ActionResult {
            crate::traits::ActionResult::error("Unknown action: unknown_echo")
        }
    }

    #[tokio::test]
    async fn test_registered_action_with_unknown_action_error_not_misrouted() {
        let (_dir, ctx) = test_context();
        let mut dispatcher = ActionDispatcher::new();
        dispatcher.register(Arc::new(UnknownEchoAction));
        let gw = Arc::new(CtxRecordingGateway {
            last_ctx: Mutex::new(None),
            last_args: Mutex::new(None),
        });
        let executor = BridgedExecutor::new(dispatcher, ctx).with_gateway_actions(gw.clone());

        let result = executor.execute("unknown_echo", &json!({})).await;
        // dispatcher の結果がそのまま返り、gateway へはフォールバックしない。
        assert!(!result.success);
        assert_eq!(
            result.error.as_deref(),
            Some("Unknown action: unknown_echo")
        );
        assert!(gw.last_ctx.lock().unwrap().is_none());
    }

    // ---- #45: 実行時ポリシー強制（可視性と対称） ----

    /// owner-only の dispatcher アクションは、一覧から隠れるだけでなく
    /// 名前指定の実行も bridge で拒否されること。
    #[tokio::test]
    async fn test_owner_only_dispatcher_action_rejected_at_execute_for_agent() {
        let (_dir, ctx) = test_context_with_caller(CallerIdentity::Agent);
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx);
        let result = executor
            .execute("update_instructions", &json!({"instructions": "x"}))
            .await;
        assert!(!result.success);
        let err = result.error.unwrap();
        assert!(err.starts_with(REJECTION_CODE_PREFIX));
        assert!(err.contains("requires owner"));
    }

    #[tokio::test]
    async fn test_owner_only_action_executes_for_owner() {
        let (_dir, ctx) = test_context_with_caller(CallerIdentity::Owner);
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx);
        // owner はポリシーを通過して dispatcher 本体に到達する（結果の成否は本体次第）。
        let result = executor
            .execute("update_instructions", &json!({"instructions": "x"}))
            .await;
        if let Some(err) = &result.error {
            assert!(
                !err.starts_with(REJECTION_CODE_PREFIX),
                "owner must not be policy-rejected: {err}"
            );
        }
    }

    /// trusted-only の gateway アクションは、素の Agent からの名前指定実行が
    /// gateway に到達する前に bridge で拒否されること（旧実装はモックまで素通し）。
    #[tokio::test]
    async fn test_trusted_only_gateway_action_rejected_at_execute_for_agent() {
        let (_dir, ctx) = test_context_with_caller(CallerIdentity::Agent);
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
            .with_gateway_actions(Arc::new(MockGatewayActionsWithSkills));
        let result = executor.execute("create_skill", &json!({})).await;
        assert!(!result.success);
        let err = result.error.unwrap();
        assert!(err.starts_with(REJECTION_CODE_PREFIX));
        assert!(err.contains("trusted"));

        // trusted_user は通過してモック（success）に到達する
        let (_dir2, ctx2) = test_context_with_caller(CallerIdentity::TrustedUser);
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx2)
            .with_gateway_actions(Arc::new(MockGatewayActionsWithSkills));
        let result = executor.execute("create_skill", &json!({})).await;
        assert!(result.success);
    }

    /// ポリシー表のドリフト検出: dispatcher 側の owner-only 名は実在する
    /// アクションであること（表が死に名を指したまま実アクションが野放しになる事故の防止）。
    #[test]
    fn test_policy_owner_only_dispatcher_names_are_live() {
        let dispatcher = ActionDispatcher::new();
        let names = dispatcher.action_names();
        assert!(
            names.iter().any(|n| n == "update_instructions"),
            "update_instructions must exist in dispatcher"
        );
        // update_heartbeat_instructions / create_skill / read_heartbeat_instructions は
        // gateway 側（discord crate のテストで実在性を検証）。execute_skill は防御的
        // エントリ（実装なし）であることをここで明文化する。
        assert!(!names.iter().any(|n| n == "execute_skill"));
    }

    // ---- ToolEventSink ----

    struct RecordingSink {
        events: Mutex<Vec<(String, String)>>, // (tool_call_id, status)
    }
    impl ToolEventSink for RecordingSink {
        fn on_event(&self, ev: &ToolEvent<'_>) {
            let status = match ev.status {
                ToolEventStatus::Started => "started",
                ToolEventStatus::Completed => "completed",
                ToolEventStatus::Failed => "failed",
                ToolEventStatus::Rejected => "rejected",
            };
            self.events
                .lock()
                .unwrap()
                .push((ev.tool_call_id.to_string(), status.to_string()));
        }
    }

    /// owner-only エラーを返す gateway モック（rejected 判定の確認用）。
    struct MockGatewayRejecting;
    #[async_trait]
    impl GatewayActions for MockGatewayRejecting {
        fn definitions(&self) -> Vec<GatewayActionDef> {
            vec![GatewayActionDef {
                name: "rej_action".to_string(),
                description: "rej".to_string(),
                parameters: json!({"type": "object", "properties": {}}),
            }]
        }
        async fn execute(
            &self,
            _name: &str,
            _args: &serde_json::Value,
            _ctx: &opencrab_gateway::GatewayCallContext,
        ) -> GatewayActionResult {
            GatewayActionResult {
                success: false,
                data: None,
                error: Some("this action is owner-only".to_string()),
            }
        }
    }

    #[tokio::test]
    async fn test_tool_event_sink_started_then_completed() {
        let (_dir, ctx) = test_context();
        let sink = Arc::new(RecordingSink {
            events: Mutex::new(Vec::new()),
        });
        let executor =
            BridgedExecutor::new(ActionDispatcher::new(), ctx).with_tool_event_sink(sink.clone());
        let r = executor
            .execute("generate_inner_voice", &json!({"thought": "hi"}))
            .await;
        assert!(r.success);
        let evs = sink.events.lock().unwrap();
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].1, "started");
        assert_eq!(evs[1].1, "completed");
        // same correlation id for the pair
        assert_eq!(evs[0].0, evs[1].0);
    }

    #[tokio::test]
    async fn test_tool_event_sink_failed_on_unknown() {
        let (_dir, ctx) = test_context();
        let sink = Arc::new(RecordingSink {
            events: Mutex::new(Vec::new()),
        });
        let executor =
            BridgedExecutor::new(ActionDispatcher::new(), ctx).with_tool_event_sink(sink.clone());
        let _ = executor.execute("nonexistent_tool", &json!({})).await;
        let evs = sink.events.lock().unwrap();
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[1].1, "failed");
    }

    #[tokio::test]
    async fn test_tool_event_sink_rejected_on_permission_error() {
        let (_dir, ctx) = test_context();
        let sink = Arc::new(RecordingSink {
            events: Mutex::new(Vec::new()),
        });
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
            .with_gateway_actions(Arc::new(MockGatewayRejecting))
            .with_tool_event_sink(sink.clone());
        let _ = executor.execute("rej_action", &json!({})).await;
        let evs = sink.events.lock().unwrap();
        assert_eq!(evs[1].1, "rejected");
    }

    // ---- M1: structured rejection classification ----

    /// 構造マーカー接頭辞付きのエラーを返す gateway モック（構造的 rejected 判定用）。
    struct MockGatewayStructuredReject;
    #[async_trait]
    impl GatewayActions for MockGatewayStructuredReject {
        fn definitions(&self) -> Vec<GatewayActionDef> {
            vec![GatewayActionDef {
                name: "sr_action".to_string(),
                description: "sr".to_string(),
                parameters: json!({"type": "object", "properties": {}}),
            }]
        }
        async fn execute(
            &self,
            _name: &str,
            _args: &serde_json::Value,
            _ctx: &opencrab_gateway::GatewayCallContext,
        ) -> GatewayActionResult {
            GatewayActionResult {
                success: false,
                data: None,
                // reject() ヘルパが付ける構造マーカーを模す。
                error: Some(format!("{REJECTION_CODE_PREFIX}forbidden_scope: nope")),
            }
        }
    }

    /// "permission denied" を含む通常の実行失敗を返す gateway モック。
    /// これは実行されたが失敗したケースで、rejected に誤分類されてはならない。
    struct MockGatewayOrdinaryPermFailure;
    #[async_trait]
    impl GatewayActions for MockGatewayOrdinaryPermFailure {
        fn definitions(&self) -> Vec<GatewayActionDef> {
            vec![GatewayActionDef {
                name: "perm_fail".to_string(),
                description: "pf".to_string(),
                parameters: json!({"type": "object", "properties": {}}),
            }]
        }
        async fn execute(
            &self,
            _name: &str,
            _args: &serde_json::Value,
            _ctx: &opencrab_gateway::GatewayCallContext,
        ) -> GatewayActionResult {
            GatewayActionResult {
                success: false,
                data: None,
                // OS 由来の通常失敗。広い NL 一致なら誤って rejected になる。
                error: Some("write failed: Permission denied (os error 13)".to_string()),
            }
        }
    }

    #[test]
    fn test_is_rejection_structured_marker() {
        assert!(is_rejection(Some(&format!(
            "{REJECTION_CODE_PREFIX}anything at all"
        ))));
    }

    #[test]
    fn test_is_rejection_ignores_ordinary_permission_failures() {
        // 実行されたが失敗した通常エラーは rejected ではない。
        assert!(!is_rejection(Some("Permission denied (os error 13)")));
        assert!(!is_rejection(Some("operation not permitted")));
        assert!(!is_rejection(Some("forbidden by remote host")));
        assert!(!is_rejection(Some("access denied to file")));
    }

    #[test]
    fn test_is_rejection_legacy_domain_markers() {
        // マーカー未付与の owner-only gateway action 等は後方互換で検知する。
        assert!(is_rejection(Some("this action is owner-only")));
        assert!(is_rejection(Some("forbidden_scope: ...")));
        assert!(is_rejection(Some("redacted read requires owner")));
    }

    #[tokio::test]
    async fn test_tool_event_sink_rejected_on_structured_marker() {
        let (_dir, ctx) = test_context();
        let sink = Arc::new(RecordingSink {
            events: Mutex::new(Vec::new()),
        });
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
            .with_gateway_actions(Arc::new(MockGatewayStructuredReject))
            .with_tool_event_sink(sink.clone());
        let _ = executor.execute("sr_action", &json!({})).await;
        let evs = sink.events.lock().unwrap();
        assert_eq!(evs[1].1, "rejected");
    }

    #[tokio::test]
    async fn test_tool_event_sink_ordinary_permission_failure_is_failed_not_rejected() {
        let (_dir, ctx) = test_context();
        let sink = Arc::new(RecordingSink {
            events: Mutex::new(Vec::new()),
        });
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
            .with_gateway_actions(Arc::new(MockGatewayOrdinaryPermFailure))
            .with_tool_event_sink(sink.clone());
        let _ = executor.execute("perm_fail", &json!({})).await;
        let evs = sink.events.lock().unwrap();
        assert_eq!(evs[1].1, "failed", "ordinary failure must not be rejected");
    }

    // ---- M2: tool_call_id propagation ----

    #[tokio::test]
    async fn test_execute_with_id_propagates_tool_call_id() {
        let (_dir, ctx) = test_context();
        let sink = Arc::new(RecordingSink {
            events: Mutex::new(Vec::new()),
        });
        let executor =
            BridgedExecutor::new(ActionDispatcher::new(), ctx).with_tool_event_sink(sink.clone());
        let r = executor
            .execute_with_id(
                "generate_inner_voice",
                &json!({"thought": "hi"}),
                "llm-call-42",
            )
            .await;
        assert!(r.success);
        let evs = sink.events.lock().unwrap();
        assert_eq!(evs.len(), 2);
        // start/terminal の両方が LLM 由来 ID を伝播する。
        assert_eq!(evs[0].0, "llm-call-42");
        assert_eq!(evs[1].0, "llm-call-42");
    }

    #[tokio::test]
    async fn test_execute_without_id_synthesizes_stable_pair() {
        let (_dir, ctx) = test_context();
        let sink = Arc::new(RecordingSink {
            events: Mutex::new(Vec::new()),
        });
        let executor =
            BridgedExecutor::new(ActionDispatcher::new(), ctx).with_tool_event_sink(sink.clone());
        // id 無し: 合成 UUID だが start/terminal で一致する。
        let _ = executor
            .execute("generate_inner_voice", &json!({"thought": "hi"}))
            .await;
        let evs = sink.events.lock().unwrap();
        assert_eq!(evs.len(), 2);
        assert!(!evs[0].0.is_empty());
        assert_eq!(evs[0].0, evs[1].0);
    }

    #[tokio::test]
    async fn test_no_sink_is_noop() {
        let (_dir, ctx) = test_context();
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx);
        let r = executor
            .execute("generate_inner_voice", &json!({"thought": "hi"}))
            .await;
        assert!(r.success);
    }
}
