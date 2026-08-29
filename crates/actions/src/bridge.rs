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
///
/// #620: 旧来の「`nsec` キー名でマスクする」sink ゲート（SECRET_KEYS / #519・#526）は撤去した。
/// キー名一致は実際の混入（別の文字列値の中に鍵が含まれる形）を検出できず、`nsec` を JSON の
/// キーに持つ引数/結果を出す producer も皆無だった（列挙で確認）。鍵は at-rest 暗号化と実行時
/// env 注入で「エージェントの読める範囲の外」に置く方式へ移し、事後マスクに依存しない。
/// 整形（要約・サイズ分割）は従来どおり sink 側。
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
/// この安定コードを付ける（この定数を直接前置するか、`gateway_reject` 経由）。分類器は
/// この構造的な接頭辞を第一の根拠にする。`"permission"` / `"denied"` / `"forbidden"`
/// のような広い自然言語の部分一致は、実行されたが失敗した通常のエラー（例: OS の
/// "Permission denied"、shell の "Operation not permitted"）を rejected に誤分類
/// するため使わない。
pub const REJECTION_CODE_PREFIX: &str = "rejected: ";

/// [`BridgedExecutor`] の実効ツールがどの production slot から来たか。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolSlot {
    Dispatcher,
    Gateway,
    Mcp,
}

/// `list_tools` と同じ gate を通った定義と、dispatch に使う class 索引の組。
pub struct EffectiveToolDefinition {
    pub definition: FunctionDefinition,
    pub class: Option<opencrab_gateway::ToolClass>,
    pub slot: ToolSlot,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutorRuntimeState {
    pub model_override: Option<String>,
    pub current_purpose: String,
}

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

/// sub-engine 専用の最小権限 gateway。`sub_engine == Allowed` のアクションだけを
/// inner 実装へ委譲する（#63 / RFC #152 S2）。
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
            .filter(|d| d.class.sub_engine == opencrab_gateway::SubEngineAccess::Allowed)
            .collect()
    }

    async fn execute(
        &self,
        name: &str,
        args: &serde_json::Value,
        ctx: &opencrab_gateway::GatewayCallContext,
    ) -> opencrab_gateway::GatewayActionResult {
        // definitions() を 1 回だけ取って使い回す（許可判定と存在判定の両方に使う）。
        let defs = self.inner.definitions();
        let def = defs.iter().find(|d| d.name == name);
        match def {
            // `sub_engine == Allowed` のツールだけ inner へ委譲する。
            Some(d) if d.class.sub_engine == opencrab_gateway::SubEngineAccess::Allowed => {
                self.inner.execute(name, args, ctx).await
            }
            // 実在するが許可外 → 権限拒否（rejected: マーカー）。
            Some(_) => gateway_reject(format!("action '{name}' is not available in sub-engines")),
            // 未知の名前 → 通常の失敗（幻覚ツール名を Rejected に誤分類させない）。
            None => opencrab_gateway::GatewayActionResult {
                success: false,
                data: None,
                error: Some(format!("Unknown gateway action: {name}")),
            },
        }
    }
}

/// `ActionDispatcher::new()` が登録する **core アクション**のうち inline 実行のまま
/// にするもの（`default_non_dispatch_tools` の種）。
///
/// **なぜ core だけ名前リストが残るか**: 分類の権威は各ツール定義の属性
/// （`GatewayActionDef.class`）へ移した（PR-2B）。ただし core アクションは
/// `actions` クレート自身の一次ツールで `GatewayActionDef` を持たない（属性を名乗る
/// 構築サイトが無い）。基準は「**ゲート固有の名前かどうか**」で、Discord / Nostr /
/// server の各 gateway 固有の名前は属性へ吸収して定数を消したが、core は「ゲート固有の
/// 名前」ではないのでここへ残す。`BridgedExecutor` はこの 2 定数から `dispatch` を合成し、
/// gateway / MCP の属性と 1 つの索引にまとめる。
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
    // 記憶の単位（宣言）の読み取り 2 つ（#379）。地図/範囲読みは即答すべき純読み取りで、
    // 結果を見て次の範囲や宣言を同ターンで決める。dispatch すると 2 ターンに割れるだけ。
    "survey_my_history",
    "read_my_history",
    "get_task",
    "analyze_llm_usage",
    "recall_model_experiences",
    // (6) 情報価値の無い短時間の書き込み。dispatch には必ず resume ターン
    //     （= ユーザーへの追加メッセージ）が 1 本付くので、報告する価値が無い
    //     書き込みを background 化すると雑音が増えるだけ。
    "update_impression",
    "save_model_insight",
    // (6) タグ操作（#359 / #313 段階2）。整理ラン（段階3）の中で「topic を読む → タグを
    //     決める → 付ける/外す/統合する」という短い書き込みループを回す。結果（新設できたか /
    //     何件付け替えたか）を同ターンで見て次の操作を決めるので background 化しない。短時間の
    //     書き込みで、dispatch すると resume ターンの雑音が増えるだけ。呼び出し元は
    //     `TRUSTED_ONLY_ACTIONS` にも入れて Nostr（caller=Agent）から触らせない。
    "tag_topic",
    "untag_topic",
    "merge_tags",
    // (6) 記憶の単位（宣言）の記録 2 つ（#379）。宣言/取り消しは短時間の書き込みで、
    //     結果（宣言できたか / 取り消せたか）を同ターンで見て次の操作を決める。呼び出し元は
    //     `TRUSTED_ONLY_ACTIONS` にも入れて Nostr（caller=Agent）から触らせない。
    "record_memory_unit",
    "retract_memory_unit",
    // (6) 宣言ランの窓の希望（#394）。1 行を UPSERT するだけの短時間の書き込みで、
    //     返り値（丸めた後の実際の設定）を同ターンで見て決め直す。dispatch する意味が無い。
    "plan_next_memory_window",
    // (6) 記憶の凝縮（#411）。ユニットを俯瞰した原則を core として刻む/更新する/取り消す短時間の
    //     書き込み。結果（刻めたか / 根拠が解決できたか）を同ターンで見て次の原則を決めるので
    //     background 化しない。呼び出し元は `TRUSTED_ONLY_ACTIONS` にも入れて Nostr（caller=Agent）
    //     から触らせない（宣言道具と同じ論拠）。
    "record_memory_core",
    "update_memory_core",
    "retract_memory_core",
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
    // スキル生成（server 側の gateway ツール `create_skill` と同分類。あちらは
    // 定義で `class.dispatch == Dispatchable` を名乗る）。
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
    // --- ローカルのシェル実行 / ファイル操作 / 実行許可リストの自己拡張（#330） ---
    // これらは「Nostr 上での活動」や「未信頼ユーザーとの会話」とは無関係の、
    // ホスト機の制御そのものであり、最上位の権限面。caller=Agent（Nostr 受信ターン /
    // 非オーナー相手の会話ターン）へ出す理由が無い。オーナー指示は「オーナー以外の指示で
    // ローカルのファイルを見る/変えるのも駄目」なので trusted_only ではなく **owner_only**
    // に揃える（CoAgent / TrustedUser にも開けない）。
    //
    // heartbeat tick / ダッシュボード / オーナー会話は全て caller=Owner なので、自律活動と
    // オーナー操作は従来どおり通る。sub-engine（depth>=1）は spawn 元の caller を継承する
    // （`subtask.rs` の `with_caller`）ので、Owner ターンから起動した実装用サブタスクは
    // caller=Owner のまま execute_shell を使える。
    //
    // シェルの許可リスト管理は `manage_allowed_commands`（上）が既に owner_only。同じことを
    // する `add_allowed_command` / `remove_allowed_command` が分類上素通しだった是正でもある
    // （bridge policy 層に owner ゲートを設ける。server ハンドラ側の owner 検査は多層防御と
    // して残る）。
    "execute_shell",
    "ws_read",
    "ws_list",
    "ws_write",
    "ws_delete",
    "ws_edit",
    "ws_mkdir",
    "add_allowed_command",
    "remove_allowed_command",
    // 時間を待たずにハートビートを手動発火（#599）。テスト用だが「今すぐ自律ターンを起こす」
    // 操作なので、オーナー / co_agent 以外（外部ユーザー由来の caller=Agent）には出さない。
    // 発火の実行内容は時間発火と同一（別経路を作らない）。
    "run_my_heartbeat",
];

/// owner / co_agent / trusted_user のみ（素の Agent は不可）のアクション（#45）。
/// `execute_skill` は現行の gateway に実装が無い防御的エントリ（将来追加時に
/// 最初からゲートされるように残している）。
pub const TRUSTED_ONLY_ACTIONS: &[&str] = &[
    "create_skill",
    "execute_skill",
    // スキル生成（core 版）と自律学習（#351）。Nostr は誰でも話しかけられるので、会話の
    // 流れでスキルを作らせ続ければスキル棚をスパムで汚染できる。オーナー明示の要望
    // （2026-08-03「スキルを作るのもなし。スパム的に作らされる可能性あるからだめ」）で
    // caller=Agent（Nostr 受信ターン / 非オーナー相手の会話ターン）からは一覧にも出さず
    // 実行もしない。gateway 版の `create_skill`（上）と同じ棚に揃える。owner/co_agent/
    // trusted_user が自分の意思で触るターン（heartbeat tick / ダッシュボード / オーナー
    // 会話）は全て caller=Owner なので従来どおり通る。`learn_from_experience` /
    // `learn_from_peer` / `reflect_and_learn` はいずれも新スキル（または記憶）を生成する
    // 学習系で、`create_my_skill` と同じく棚へ書き込むため同じゲートに揃える。
    "create_my_skill",
    "learn_from_experience",
    "learn_from_peer",
    "reflect_and_learn",
    "read_heartbeat_instructions",
    // エージェント自身の Nostr 受信 → Discord 転記先設定（#252 段階 C）。**owner 限定に
    // はしない** — 自分の転記先を自分で決めるのがこの機能の目的で、エージェントが自分の
    // 意思で触るターン（heartbeat tick / ダッシュボード / オーナー会話）は全て
    // caller=Owner なので妨げられない。一方 caller=Agent は「未信頼の外部ユーザーと
    // 会話しているターン」なので、そこへ開けると Nostr の会話ターンで自分宛受信を
    // 任意の Discord チャンネルへ流させられる。`set_my_heartbeat`（#247/#251）と同じ扱い。
    "get_my_nostr_relay",
    "set_my_nostr_relay",
    // 自分のハートビート（自律実行）の有効化と間隔（#247）。**owner 限定にはしない** —
    // 自分の設定を自分で触れることがこの機能の目的で、エージェントが自分の意思で
    // 触るターン（heartbeat tick / ダッシュボード / オーナーとの会話）は全て
    // caller=Owner なので妨げられない。一方 caller=Agent は「未信頼の外部ユーザーと
    // 会話しているターン」を意味するので、そこへ開けると会話で自律実行を起動させられる
    // （費用と挙動に効く / #240 の「意図せず自律実行が始まる」の再来）。
    "get_my_heartbeat",
    "set_my_heartbeat",
    // 定時実行（#455）。`set_my_heartbeat` と同じ理由: **owner 限定にはしない**（自分の
    // 定時実行を自分で決めるのが目的で、本人が触るターン〔heartbeat tick / ダッシュボード /
    // オーナー会話〕は caller=Owner）。一方 caller=Agent（未信頼の外部ユーザー会話ターン）へ
    // 開けると、会話で「毎朝○時に外部出力する」を仕込ませられる（#240 の再来）ので塞ぐ。
    // 更新・削除（#477）も同じ棚: **owner 限定にはしない**（自分の巡回をやめる/間隔を変えるのが
    // 目的で、本人が触るターンは caller=Owner）。一方 caller=Agent（未信頼の外部ユーザー会話）
    // からは一覧にも出さない。ハンドラ内でも所属チェック（agent_id＋session）で多層防御する。
    "get_my_schedules",
    "set_my_schedule",
    "update_my_schedule",
    "delete_my_schedule",
    // VC 参加/退出。可視性 == 強制の対称化（#45）: 非 trusted の Agent には
    // 一覧にも出さない。ハンドラ側はさらに厳しく owner/trusted_user のみ許可
    // （co_agent は一覧に見えても実行は拒否される）。
    "join_voice_channel",
    "leave_voice_channel",
    // 本鍵（アイデンティティ）の切替。外部ユーザーが勝手に乗っ取れないよう owner/
    // trusted のみ（inbound=Agent には一覧にも出さず実行もしない）。
    "nostr_switch_identity",
    // 生成鍵の npub 一覧。nsec は返さないが、自分の鍵一覧は運用者/自分（caller=Owner の
    // ターン: heartbeat / ダッシュボード / オーナー会話）だけが見ればよい情報で、外部
    // ユーザー由来の会話ターン（caller=Agent）へ出す必要は無い。`nostr_switch_identity`
    // と対で使う管理系ツールなので同じ trusted ゲートに揃える。
    "nostr_list_keys",
    // caller=Agent（Nostr 受信ターン / 非オーナー相手の会話ターン）に素通しだった 9 個
    // （#356）。棚卸しで OWNER_ONLY にも TRUSTED_ONLY にも入っておらず外部ユーザー由来の
    // 会話ターンから使えていたもの。オーナー要望（2026-08-03「記憶検索はいいと思う。他の
    // ツールさえ使えなければ」）に従い 9 個すべて **trusted_only**（owner_only ではない）。
    // owner / co_agent / trusted_user が自分の意思で触るターン（heartbeat tick /
    // ダッシュボード / オーナー会話 / 信頼済みユーザー会話）は全て caller!=Agent なので
    // 従来どおり通る。#351/#353 と同じ手口＝既存の caller ゲートへの追加のみで、新しい
    // 概念・列・設定は足していない。
    //
    // 通知転送先（webhook）の設定・読み取り。一番危険なのは `set_default_*` — Nostr で
    // 話しかけた第三者にエージェントの通知先 URL を自分のサーバへ向け替えられると、以後の
    // 通知内容がそこへ流れる。読み取り側（`get_*` / `list_*`）も設定済み URL を露出する。
    // これら 6 個は `SystemGatewayActions`（server 側 own ツール / #157 S5）の実装。
    "set_default_webhook",
    "set_default_subtask_webhook",
    "get_default_webhook",
    "get_default_subtask_webhook",
    "list_webhooks",
    "list_subtask_webhooks",
    // 記憶インデックス設定の書き込み。他の `configure_*` は全て OWNER_ONLY なのにこれだけ
    // 素通しだった漏れの是正。owner_only ではなく trusted_only に揃える（#356 のオーナー
    // 決定）。`SystemGatewayActions`（server 側 own ツール / #157 S1）の実装。
    "update_memory_index_config",
    // ホスト・システム情報の露出。core inline アクション（`CORE_INLINE_ACTIONS`）。
    "get_system_info",
    // `execute_shell` の許可コマンド一覧＝ローカル構成の露出。`execute_shell` 本体は
    // OWNER_ONLY（#330）だが、その許可リストの読み取りは素通しだった。
    // `SystemGatewayActions`（server 側 own ツール / #157 S1）の実装。
    "list_allowed_commands",
    // 記憶へのタグ付け（#359 / #313 段階2）。Nostr は誰でも話しかけられるので、会話の
    // 流れで記憶にタグを付けさせ続ければタグ語彙をスパムで汚染できる（#351/#353 と同じ
    // 論拠）。整理ラン（段階3）は caller=Owner で走る（heartbeat と同じ前例）ので支障は
    // 無い。`OWNER_ONLY` ではなく **trusted_only** — owner だけでなく CoAgent /
    // TrustedUser も従来どおり使える。owner / co_agent / trusted_user が自分の意思で触る
    // ターン（heartbeat tick / ダッシュボード / オーナー会話 / 信頼済みユーザー会話）は
    // 全て caller!=Agent なので通る。いずれも core dispatcher のアクション
    // （`CORE_INLINE_ACTIONS` / `crates/actions/src/memory_access.rs`）で、既存の caller
    // ゲートへの追加のみ＝新しい概念・列・設定は足していない。
    "tag_topic",
    "untag_topic",
    "merge_tags",
    // 記憶の単位（宣言）道具 4 つ（#379 #376 段階1）。タグ道具（上）と同じ論拠で
    // **trusted_only**: Nostr（caller=Agent）は誰でも話しかけられるので、会話の流れで
    // 生ログを俯瞰させ・宣言させ続けると、記憶レイヤをスパムで汚染できる。宣言ラン
    // （段階2）は caller=Owner で走る（heartbeat と同じ前例）ので支障は無い。owner /
    // co_agent / trusted_user が自分の意思で触るターン（heartbeat tick / ダッシュボード /
    // オーナー会話 / 信頼済みユーザー会話）は全て caller!=Agent なので従来どおり通る。
    // いずれも core dispatcher のアクション（`crates/actions/src/memory_units.rs`）で、
    // 既存の caller ゲートへの追加のみ＝新しい概念・列・設定は足していない。読み取り 2 つ
    // （survey / read）は整理ラン用の `ORGANIZE_ALLOWED_TOOLS` にも入る（記録 2 つは段階2）。
    "survey_my_history",
    "read_my_history",
    "record_memory_unit",
    "retract_memory_unit",
    // 宣言ランの窓の希望（#394）。同じ論拠で trusted_only: caller=Agent（Nostr の受信ターン）
    // から触れると、話しかけるだけで他人の宣言ランの窓を動かせてしまう。宣言ラン本体は
    // caller=Owner で走るので支障は無い。
    "plan_next_memory_window",
    // 記憶の凝縮 道具 3 つ（#411）。宣言道具と同じ論拠で trusted_only: caller=Agent（Nostr の
    // 受信ターン）から触れると、会話の流れで人格の核（core）をスパムで汚染できる。凝縮ラン本体は
    // caller=Owner で走る（宣言ラン・heartbeat と同じ前例）ので支障は無い。core dispatcher の
    // アクション（`crates/actions/src/memory_units.rs`）で、既存の caller ゲートへの追加のみ。
    "record_memory_core",
    "update_memory_core",
    "retract_memory_core",
];

// `nostr_run`（薄い nostaro passthrough / #268）は**ここに入れない**（#303）。
// opencrab が Nostr 連携で担保するのは ①鍵のエージェント間混同防止 ②nsec の隠蔽 の
// 2 点だけで、①は常に当該エージェント自身の `--config` を渡す passthrough の構造が、
// ②は出力マスクが担保している。caller による露出制限はどちらにも要らない。
// caller=Agent が指すのは **Nostr 受信ターン**（`crates/nostr/src/sink.rs`）と、非オーナー
// 相手の会話ターン。ここへ入れると Nostr 受信ターンから `nostr_run` が丸ごと消えるため、
// 「Nostr 上で自律的に活動する」という目的そのものを塞ぐ。
// （heartbeat tick は caller=Owner なので元から塞がれていない。上の各コメントも同じ。）
//
// `nostr_zap` は同じ理由で**ここに入れない**（#306）。以前は `nostr_dm` と共に入っていたが、
// `nostr_run` を開けた時点で `nostr_run zap` / `nostr_run dm` が同じターンから通るように
// なり（当時の passthrough deny は `init`/`watch`/`relay` の 3 つだけ）、inner ツール名だけを
// 隠しても能力は塞げていなかった。一貫性は**制約を増やす方向ではなく減らす方向**で取る、
// というのがオーナーの決定（#306）。使うかどうかはエージェントが自分で判断する。
//
// **`nostr_dm` は #514 で別扱いになった**: DM は秘密鍵漏洩で過去に遡って全部読めるため
// 送信禁止（オーナー決定）。定義から削除し、送信のもう一方の経路 `nostr_run dm` も
// passthrough deny（`crates/nostr/src/cli.rs` の `PASSTHROUGH_DENIED_SUBCOMMANDS` に `dm`）で
// 塞いだ。#306 の「減らす方向」とは逆の追加だが、#306 は「DM か zap か」の caller ゲートの
// 話で、#514 は「DM という機能そのものを持たない」というより上位の決定なので矛盾しない。
// 上の nostr_switch_identity / nostr_list_keys は残る — こちらは①鍵の混同防止に
// 直接効き、`nostr_run` 側でも `init` が deny されていて迂回路が無い。
// nostr_zap のゲートを外した状態は `nostr_messaging_passes_the_gate_for_agent_caller` が固定する。

/// アクション名 → 権限/深度ポリシー（#45 の単一の表）。
///
/// 以前は可視性（`list_tools`）だけがこれらのリストを参照し、実行
/// （`dispatch_inner`）は depth 系しか強制していなかったため、「一覧から
/// 隠したツールをモデルが名前指定で実行できる」食い違いがあった。
/// 可視性と実行時強制は必ずこの関数を参照すること（discord 側ハンドラの
/// typed gate は多層防御としてそのまま残る）。
///
/// **名前リストで決まる 3 つだけ**を持つ（`owner_only` / `trusted_only` /
/// `depth_capped`）。sub-engine 遮断（旧 `blocked_in_subengine`）はツール定義の属性
/// （`class.sub_engine == Blocked`）が権威になったため、`BridgedExecutor` が
/// 名前 → `ToolClass` の索引から引く（この構造体には持たせない）。
pub struct ToolPolicy {
    pub owner_only: bool,
    pub trusted_only: bool,
    /// depth >= MAX_DEPTH でブロック（ネスト上限）。
    pub depth_capped: bool,
}

pub fn tool_policy(name: &str) -> ToolPolicy {
    ToolPolicy {
        owner_only: OWNER_ONLY_ACTIONS.contains(&name),
        trusted_only: TRUSTED_ONLY_ACTIONS.contains(&name),
        depth_capped: name == "spawn_subtask",
    }
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
    /// この run を起こした inbound メッセージの返信先（gateway 不透明 token / #158 S1）。
    /// `gateway_call_context` が `GatewayCallContext.reply_target` に載せ、宛先引数を
    /// 省略したツール呼び出しのフォールバックにする。既定 `None`。
    reply_target: Option<String>,
    /// この run で使えるツール名の許可リスト（#368）。`Some` のとき、可視性
    /// （`list_tools`）と実行（`dispatch_inner`）の**両方**を、ここに載る名前だけに絞る。
    /// caller/depth ゲート（`tool_policy` / `policy_allows`）は弱めず、その**上に重ねる**
    /// 追加の deny-by-default。
    ///
    /// **全スロット（dispatcher / gateway own / MCP）に効く**のが要点。3 スロットとも
    /// 可視は `list_tools`、実行は `dispatch_inner` を通るので、この 1 箇所で覆える
    /// （スロット個別のフィルタを別々に足す必要がない）。
    ///
    /// 既定 `None`（無制限 = 従来挙動）。対話ターン・heartbeat・subtask は `None` の
    /// ままで一切変わらない。sleep 整理ラン（`memory_organize`）だけが `Some` を渡す。
    tool_allowlist: Option<std::collections::HashSet<String>>,
    /// 名前 → `ToolClass` の索引（分類の権威）。
    ///
    /// gateway / MCP の `definitions()` を舐めて `(name, class)` を入れ、core のツール
    /// （`GatewayActionDef` を持たない）は [`CORE_INLINE_ACTIONS`] / [`CORE_DISPATCHABLE_ACTIONS`]
    /// から `dispatch` を合成する（`sub_engine = NotExposed`、`sharing = AgentBound`。core は
    /// 許可リストにも拒否リストにも属さないため一律 `NotExposed` で現行と等価）。gateway /
    /// MCP を差し替えたら [`Self::rebuild_tool_class_index`] で作り直す。sub-engine 遮断
    /// （`sub_engine == Blocked`）と非同期化除外（`dispatch == Inline`）をここから引く。
    /// 索引に無い名前は「遮断しない」（属性を名乗る定義が無いツールは既定で通す）。
    tool_class_index: std::collections::HashMap<String, opencrab_gateway::ToolClass>,
}

impl BridgedExecutor {
    pub fn new(dispatcher: ActionDispatcher, context: ActionContext) -> Self {
        let mut this = Self {
            dispatcher,
            context,
            gateway_actions: None,
            mcp_actions: None,
            depth: 0,
            tool_event_sink: None,
            reply_target: None,
            tool_allowlist: None,
            tool_class_index: std::collections::HashMap::new(),
        };
        this.rebuild_tool_class_index();
        this
    }

    /// 名前 → `ToolClass` 索引を作り直す（core 合成 + gateway + MCP）。
    ///
    /// gateway / MCP を差し替えたら必ず呼ぶ（`with_gateway_actions` / `with_mcp_actions`）。
    /// 後入れ優先で挿入する: gateway / MCP の実定義が core 合成より優先されるが、名前空間は
    /// 重ならない（core と gateway own と MCP プレフィックスは互いに素）ので実際の衝突は無い。
    fn rebuild_tool_class_index(&mut self) {
        use opencrab_gateway::{DispatchMode, SubEngineAccess, ToolClass, ToolSharing};
        let mut index: std::collections::HashMap<String, ToolClass> =
            std::collections::HashMap::new();
        // core のツールは `GatewayActionDef` を持たないので合成する。core は許可リストにも
        // 拒否リストにも 1 つも属さないため `sub_engine = NotExposed` で現行と等価。
        let synth = |dispatch: DispatchMode| ToolClass {
            dispatch,
            sub_engine: SubEngineAccess::NotExposed,
            sharing: ToolSharing::AgentBound,
        };
        for name in CORE_INLINE_ACTIONS {
            index.insert((*name).to_string(), synth(DispatchMode::Inline));
        }
        for name in CORE_DISPATCHABLE_ACTIONS {
            index.insert((*name).to_string(), synth(DispatchMode::Dispatchable));
        }
        if let Some(ref gw) = self.gateway_actions {
            for def in gw.definitions() {
                index.insert(def.name, def.class);
            }
        }
        if let Some(ref mcp) = self.mcp_actions {
            for def in mcp.definitions() {
                index.insert(def.name, def.class);
            }
        }
        self.tool_class_index = index;
    }

    /// depth>=1 の sub-engine から遮断すべきか（`class.sub_engine == Blocked`）。
    /// 索引に無い名前は `false`（属性を名乗る定義が無いツールは遮断しない）。
    ///
    /// **多層防御の層が移ったことの記録（消さないこと）**:
    /// - **本番では事実上不活性**。depth>=1 では `gateway_actions` が常に
    ///   [`SubEngineGatewayActions`]（`Allowed` だけに事前フィルタする外周）なので、索引に
    ///   `Blocked` が入らず、この二層目は必ず `false` を返す。実効ゲートは外周フィルタが担う。
    ///   挙動は旧実装と完全に等価（旧 `DISCORD_ACTIONS` の名前ベース深さ拒否も、外周の許可
    ///   リストの上に乗る冗長な層だった）。
    /// - **将来 depth>=1 で生の gateway を直付けする経路を足すと、この層が復活する**
    ///   （外周フィルタを通らないツールに対して `Blocked` 属性が実効ゲートになる）。だから
    ///   「使われていないから消す」判断はしないこと。多層防御の意図は残す。
    fn is_blocked_in_subengine(&self, name: &str) -> bool {
        self.tool_class_index
            .get(name)
            .map(|c| c.sub_engine == opencrab_gateway::SubEngineAccess::Blocked)
            .unwrap_or(false)
    }

    pub fn with_depth(mut self, depth: u32) -> Self {
        self.depth = depth;
        self
    }

    pub fn with_gateway_actions(mut self, actions: Arc<dyn GatewayActions>) -> Self {
        self.gateway_actions = Some(actions);
        self.rebuild_tool_class_index();
        self
    }

    /// MCP ツール源を注入する（`mcp__<server>__<tool>` を提供する `GatewayActions`）。
    pub fn with_mcp_actions(mut self, actions: Arc<dyn GatewayActions>) -> Self {
        self.mcp_actions = Some(actions);
        self.rebuild_tool_class_index();
        self
    }

    pub fn with_tool_event_sink(mut self, sink: Arc<dyn ToolEventSink>) -> Self {
        self.tool_event_sink = Some(sink);
        self
    }

    /// inbound メッセージの返信先（gateway 不透明 token）を注入する（#158 S1）。
    ///
    /// `RunRequest.reply_target` と同じ `Option<String>` をそのまま受け、ツール実行時の
    /// `GatewayCallContext.reply_target` として gateway 実装へ運ぶ。未注入なら `None`
    /// のままで、宛先を明示するツール呼び出しの挙動は変わらない。
    pub fn with_reply_target(mut self, reply_target: Option<String>) -> Self {
        self.reply_target = reply_target;
        self
    }

    /// この run のツール許可リストを注入する（#368）。`Some` のとき、可視性と実行の
    /// 両方を、渡した名前だけに絞る（caller/depth ゲートの**上乗せ**）。`None`（既定）は
    /// 無制限で従来どおり。sleep 整理ランだけが渡す。
    pub fn with_tool_allowlist(mut self, allowlist: Option<Vec<String>>) -> Self {
        self.tool_allowlist = allowlist.map(|v| v.into_iter().collect());
        self
    }

    /// LLM に見せる実効定義を、production の slot/class 索引と一緒に列挙する。
    ///
    /// `list_tools` はこの結果から定義だけを取り出すため、採取用の分類再構築と
    /// production 可視性が別々に進む余地はない。
    pub fn effective_tool_definitions(&self) -> Vec<EffectiveToolDefinition> {
        let opt_desc = |description: String| {
            if description.is_empty() {
                None
            } else {
                Some(description)
            }
        };
        let mut tools: Vec<EffectiveToolDefinition> = self
            .dispatcher
            .get_definitions(&[])
            .into_iter()
            .filter(|definition| {
                self.policy_allows(&definition.name) && self.run_allows(&definition.name)
            })
            .map(|definition| {
                let class = self.tool_class_index.get(&definition.name).copied();
                EffectiveToolDefinition {
                    definition: FunctionDefinition {
                        name: definition.name,
                        description: opt_desc(definition.description),
                        parameters: definition.parameters,
                    },
                    class,
                    slot: ToolSlot::Dispatcher,
                }
            })
            .collect();

        if let Some(ref gateway) = self.gateway_actions {
            for definition in gateway.definitions() {
                if !self.policy_allows(&definition.name) || !self.run_allows(&definition.name) {
                    continue;
                }
                let class = self.tool_class_index.get(&definition.name).copied();
                tools.push(EffectiveToolDefinition {
                    definition: FunctionDefinition {
                        name: definition.name,
                        description: opt_desc(definition.description),
                        parameters: definition.parameters,
                    },
                    class,
                    slot: ToolSlot::Gateway,
                });
            }
        }

        if let Some(ref mcp) = self.mcp_actions {
            for definition in mcp.definitions() {
                if !self.policy_allows(&definition.name) || !self.run_allows(&definition.name) {
                    continue;
                }
                let class = self.tool_class_index.get(&definition.name).copied();
                tools.push(EffectiveToolDefinition {
                    definition: FunctionDefinition {
                        name: definition.name,
                        description: opt_desc(definition.description),
                        parameters: definition.parameters,
                    },
                    class,
                    slot: ToolSlot::Mcp,
                });
            }
        }
        tools
    }

    /// 同じ executor を使う engine が次の LLM call で読む turn-local 状態。
    pub fn runtime_state(&self) -> ExecutorRuntimeState {
        ExecutorRuntimeState {
            model_override: self
                .context
                .model_override
                .lock()
                .ok()
                .and_then(|value| value.clone()),
            current_purpose: self
                .context
                .current_purpose
                .lock()
                .map(|value| value.clone())
                .unwrap_or_default(),
        }
    }

    /// この run のツール許可リストが `name` を許すか（#368）。`None`（未設定）なら常に
    /// 許可（無制限）。`Some` のときは集合に載る名前だけを許す。`policy_allows`（caller/depth
    /// ゲート）とは独立の**追加**述語で、`list_tools`（可視）と `dispatch_inner`（実行）の
    /// 両方が同じこの述語を通すことで「見えるが呼べない / 見えないが呼べる」の食い違いを防ぐ。
    fn run_allows(&self, name: &str) -> bool {
        match &self.tool_allowlist {
            None => true,
            Some(set) => set.contains(name),
        }
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
            // inbound の返信先を gateway 実装まで運ぶ（#158 S1）。宛先を引数で受ける
            // アクションが、引数省略時のフォールバックとして読む。
            reply_target: self.reply_target.clone(),
        }
    }

    fn caller_is_owner(&self) -> bool {
        // #485: co_agent は owner 等価（オーナー指示 2026-08-10。#330 を覆す）。owner 判定の
        // 唯一の源は `CallerIdentity::is_owner_equivalent`。OWNER_ONLY_ACTIONS（execute_shell /
        // ws_* / configure_* / (add|remove)_allowed_command 等）の可視性・実行の双方がここを通る。
        self.context.caller.is_owner_equivalent()
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
        if self.depth >= 1 && self.is_blocked_in_subengine(name) {
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
        if self.depth >= 1 && self.is_blocked_in_subengine(name) {
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
        // この run の許可リスト（#368）: caller/depth ゲートを通っても、許可リストの外なら
        // 実行を拒否する。MCP/dispatcher/gateway のどのスロットへ振り分ける**前**に効かせる
        // ことで、全スロットを 1 箇所で覆う（見えないが呼べる、を塞ぐ）。
        if !self.run_allows(name) {
            return reject(format!(
                "action '{name}' is not available in this run (tool allowlist)"
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
        // #620: 旧来の「nsec キー名でマスクする」sink ゲート（SECRET_KEYS）は撤去した。
        // キー名一致は実際の混入（値の中に鍵が含まれる形）を検出できず、そもそも `nsec` を
        // キーに持つ JSON を tool 引数/結果へ出す producer は皆無だった（列挙で確認 / #620）。
        // 鍵は at-rest 暗号化と実行時 env 注入で「読める範囲の外」に置く方式へ移した。
        let sink_args = args;
        sink.on_event(&ToolEvent {
            tool_name: name,
            tool_call_id: call_id,
            agent_id: &self.context.agent_id,
            session_id,
            depth: self.depth,
            status: ToolEventStatus::Started,
            started_at: &started_at,
            duration_ms: None,
            args: sink_args,
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
        // #620: 結果側の nsec キー名マスク（SECRET_KEYS）も撤去（上と同じ理由）。
        let sink_result = &result.data;
        sink.on_event(&ToolEvent {
            tool_name: name,
            tool_call_id: call_id,
            agent_id: &self.context.agent_id,
            session_id,
            depth: self.depth,
            status,
            started_at: &started_at,
            duration_ms: Some(duration_ms),
            args: sink_args,
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
        self.effective_tool_definitions()
            .into_iter()
            .map(|tool| tool.definition)
            .collect()
    }

    /// 非同期化しないツール名（inline 実行のまま）。
    ///
    /// 索引から `dispatch == Inline` の名前を集め、[`crate::subtask::default_non_dispatch_tools`]
    /// （制御ツール ＋ core inline）と合わせて返す。gateway / MCP を注入していない executor
    /// でも制御ツールと core は必ず inline に残る（`default_non_dispatch_tools` が保証）。
    fn inline_tool_names(&self) -> std::collections::HashSet<String> {
        let mut set = crate::subtask::default_non_dispatch_tools();
        for (name, class) in &self.tool_class_index {
            if class.dispatch == opencrab_gateway::DispatchMode::Inline {
                set.insert(name.clone());
            }
        }
        set
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
    ///
    /// sub-engine の到達可否は各定義の `class.sub_engine` 属性で決まる（PR-2B）ので、
    /// フェイクも実属性を再現する: `nostr_generate_key` / `report_progress` は
    /// `Allowed`、`send_ui` は `Blocked`。
    struct FakeCompositeGateway;

    #[async_trait]
    impl GatewayActions for FakeCompositeGateway {
        fn definitions(&self) -> Vec<GatewayActionDef> {
            [
                (
                    "nostr_generate_key",
                    opencrab_gateway::SubEngineAccess::Allowed,
                ),
                (
                    "report_progress",
                    opencrab_gateway::SubEngineAccess::Allowed,
                ),
                ("send_ui", opencrab_gateway::SubEngineAccess::Blocked),
            ]
            .iter()
            .map(|(n, sub_engine)| GatewayActionDef {
                name: n.to_string(),
                class: opencrab_gateway::ToolClass {
                    dispatch: opencrab_gateway::DispatchMode::Inline,
                    sub_engine: *sub_engine,
                    sharing: opencrab_gateway::ToolSharing::AgentBound,
                },
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

    /// テスト用GatewayActionsモック
    struct MockGatewayActions;

    #[async_trait]
    impl GatewayActions for MockGatewayActions {
        fn definitions(&self) -> Vec<GatewayActionDef> {
            vec![
                GatewayActionDef {
                    name: "gw_action_a".to_string(),
                    class: opencrab_gateway::ToolClass {
                        dispatch: opencrab_gateway::DispatchMode::Inline,
                        sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                        sharing: opencrab_gateway::ToolSharing::AgentBound,
                    },
                    description: "Gateway action A".to_string(),
                    parameters: json!({"type": "object", "properties": {}}),
                },
                GatewayActionDef {
                    name: "gw_action_b".to_string(),
                    class: opencrab_gateway::ToolClass {
                        dispatch: opencrab_gateway::DispatchMode::Inline,
                        sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                        sharing: opencrab_gateway::ToolSharing::AgentBound,
                    },
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

    /// #356: server 側 own ツール（webhook 6 個 + `update_memory_index_config` +
    /// `list_allowed_commands`）を露出するモック。これらは本番では `SystemGatewayActions`
    /// が定義するが、`BridgedExecutor::new` は `gateway_actions: None` なので、list_tools の
    /// 可視性フィルタ（`policy_allows` による gateway merge の絞り込み）を実測するには
    /// gateway 源を注入する必要がある。`get_system_info` は core dispatcher 側にあるので
    /// ここには入れない（二重登録を避ける）。
    struct MockGatewayServerSlot8;

    #[async_trait]
    impl GatewayActions for MockGatewayServerSlot8 {
        fn definitions(&self) -> Vec<GatewayActionDef> {
            PASSTHROUGH_9_SERVER_SLOT
                .iter()
                .map(|name| GatewayActionDef {
                    name: name.to_string(),
                    class: opencrab_gateway::ToolClass {
                        dispatch: opencrab_gateway::DispatchMode::Inline,
                        sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                        sharing: opencrab_gateway::ToolSharing::AgentBound,
                    },
                    description: format!("server slot {name}"),
                    parameters: json!({"type": "object", "properties": {}}),
                })
                .collect()
        }

        async fn execute(
            &self,
            name: &str,
            _args: &serde_json::Value,
            _ctx: &opencrab_gateway::GatewayCallContext,
        ) -> GatewayActionResult {
            // caller ゲートを通過した owner/trusted のときだけここへ来る（本番では実処理）。
            GatewayActionResult {
                success: true,
                data: Some(json!({"ok": name})),
                error: None,
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
                    class: opencrab_gateway::ToolClass {
                        dispatch: opencrab_gateway::DispatchMode::Inline,
                        sub_engine: opencrab_gateway::SubEngineAccess::Blocked,
                        sharing: opencrab_gateway::ToolSharing::AgentBound,
                    },
                    description: "peer review".to_string(),
                    parameters: json!({"type": "object", "properties": {}}),
                },
                GatewayActionDef {
                    name: "report_progress".to_string(),
                    class: opencrab_gateway::ToolClass {
                        dispatch: opencrab_gateway::DispatchMode::Inline,
                        sub_engine: opencrab_gateway::SubEngineAccess::Allowed,
                        sharing: opencrab_gateway::ToolSharing::AgentBound,
                    },
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
                    class: opencrab_gateway::ToolClass {
                        dispatch: opencrab_gateway::DispatchMode::Dispatchable,
                        sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                        sharing: opencrab_gateway::ToolSharing::AgentBound,
                    },
                    description: "update".to_string(),
                    parameters: json!({"type": "object", "properties": {}}),
                },
                GatewayActionDef {
                    name: "read_heartbeat_instructions".to_string(),
                    class: opencrab_gateway::ToolClass {
                        dispatch: opencrab_gateway::DispatchMode::Inline,
                        sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                        sharing: opencrab_gateway::ToolSharing::AgentBound,
                    },
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

    // ---- run 単位のツール許可リスト（#368）----

    /// MCP スロット検証用: `mcp__` 名前空間の外部ツールを 1 つ定義するモック。
    struct MockMcpSlot;

    #[async_trait]
    impl GatewayActions for MockMcpSlot {
        fn definitions(&self) -> Vec<GatewayActionDef> {
            vec![GatewayActionDef {
                name: "mcp__ext__send".to_string(),
                class: opencrab_gateway::ToolClass {
                    dispatch: opencrab_gateway::DispatchMode::Inline,
                    sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                    sharing: opencrab_gateway::ToolSharing::AgentBound,
                },
                description: "external send".to_string(),
                parameters: json!({"type": "object", "properties": {}}),
            }]
        }
        async fn execute(
            &self,
            name: &str,
            _args: &serde_json::Value,
            _ctx: &opencrab_gateway::GatewayCallContext,
        ) -> GatewayActionResult {
            GatewayActionResult {
                success: true,
                data: Some(json!({ "reached": name })),
                error: None,
            }
        }
    }

    /// 許可リストは **3 スロット全て**（dispatcher core / gateway own / MCP）を、
    /// **可視性（list_tools）と実行（dispatch）の両方**で絞る。許可リスト無し（None）なら
    /// 従来どおり全部見える（＝対話ターン・heartbeat・subtask の不変性の裏付け）。
    #[tokio::test]
    async fn tool_allowlist_gates_all_slots_visibility_and_execution() {
        // 許可リスト: 読み取り 1 + タグ 1 + 終了宣言 1（整理ランの最小形）。
        let allow = vec![
            "browse_memory_index".to_string(),
            "tag_topic".to_string(),
            "declare_done".to_string(),
        ];

        // --- 許可リスト無し（None）: 全スロットのツールが見える（不変性の対照） ---
        let (_dir0, ctx0) = test_context();
        let unrestricted = BridgedExecutor::new(ActionDispatcher::new(), ctx0)
            .with_gateway_actions(Arc::new(MockGatewayActions)) // gw_action_a/b（gateway own 相当）
            .with_mcp_actions(Arc::new(MockMcpSlot)); // mcp__ext__send（MCP スロット）
        let base: Vec<String> = unrestricted
            .list_tools()
            .into_iter()
            .map(|t| t.name)
            .collect();
        // dispatcher core / gateway own / MCP がどれも見える。
        assert!(
            base.contains(&"ws_delete".to_string()),
            "core が見える: {base:?}"
        );
        assert!(
            base.contains(&"gw_action_a".to_string()),
            "gateway own が見える"
        );
        assert!(base.contains(&"mcp__ext__send".to_string()), "MCP が見える");

        // --- 許可リスト有り（Some）: 許可外は全スロットで消える ---
        let (_dir1, ctx1) = test_context();
        let restricted = BridgedExecutor::new(ActionDispatcher::new(), ctx1)
            .with_gateway_actions(Arc::new(MockGatewayActions))
            .with_mcp_actions(Arc::new(MockMcpSlot))
            .with_tool_allowlist(Some(allow.clone()));
        let visible: Vec<String> = restricted
            .list_tools()
            .into_iter()
            .map(|t| t.name)
            .collect();
        // 経路2（list_tools 可視性）: 許可されたものだけ見える。
        assert!(visible.contains(&"browse_memory_index".to_string()));
        assert!(visible.contains(&"tag_topic".to_string()));
        assert!(visible.contains(&"declare_done".to_string()));
        // 3 スロットの許可外ツールがどれも消える。
        for forbidden in ["ws_delete", "gw_action_a", "mcp__ext__send"] {
            assert!(
                !visible.contains(&forbidden.to_string()),
                "許可外 {forbidden} が list_tools に残っている: {visible:?}"
            );
        }

        // 経路3（実行）: 許可外は dispatch で拒否（rejected: マーカー）。
        for (forbidden, slot) in [
            ("ws_delete", "dispatcher core"),
            ("gw_action_a", "gateway own"),
            ("mcp__ext__send", "MCP"),
        ] {
            let r = restricted.execute(forbidden, &json!({})).await;
            assert!(!r.success, "{slot} の {forbidden} は拒否されるべき");
            let err = r.error.unwrap_or_default();
            assert!(
                err.starts_with(REJECTION_CODE_PREFIX),
                "{slot} の {forbidden} は構造的拒否であるべき: {err}"
            );
            // gateway/MCP には届いていない（実行痕跡 reached が無い）。
            assert!(
                r.data.get("reached").is_none(),
                "{slot} の {forbidden} は実装へ届いてはならない"
            );
        }

        // 許可されたツールは実行が拒否されない（tag_topic は書き込み・DB 依存だが、
        // 少なくとも許可リストでの拒否は受けない）。
        let ok = restricted.execute("browse_memory_index", &json!({})).await;
        assert!(
            !is_rejection(ok.error.as_deref()),
            "許可ツールが許可リストで拒否されてはならない: {:?}",
            ok.error
        );
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
                    class: opencrab_gateway::ToolClass {
                        dispatch: opencrab_gateway::DispatchMode::Inline,
                        sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                        sharing: opencrab_gateway::ToolSharing::AgentBound,
                    },
                    description: "Gateway action A".to_string(),
                    parameters: json!({"type": "object", "properties": {}}),
                },
                GatewayActionDef {
                    name: "create_skill".to_string(),
                    class: opencrab_gateway::ToolClass {
                        dispatch: opencrab_gateway::DispatchMode::Dispatchable,
                        sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                        sharing: opencrab_gateway::ToolSharing::AgentBound,
                    },
                    description: "Create a skill".to_string(),
                    parameters: json!({"type": "object", "properties": {}}),
                },
                GatewayActionDef {
                    name: "execute_skill".to_string(),
                    class: opencrab_gateway::ToolClass {
                        dispatch: opencrab_gateway::DispatchMode::Inline,
                        sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                        sharing: opencrab_gateway::ToolSharing::AgentBound,
                    },
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

    /// #298: subtask 決着で resume したターンでも owner/trusted のツールが残る。
    ///
    /// `policy_allows` がこのバグの実体（`trusted_only && !caller_is_trusted()` で
    /// **list_tools からも dispatch からも**落ちる）なので、ここで固定するのは
    /// 「決着通知が運ぶ呼び出し元でツール一覧を組めば元ターンと同じ集合になる」こと。
    /// 通知の caller は `settle_completed` が registry のエントリから読む実物を使う。
    #[tokio::test]
    async fn resumed_turn_keeps_owner_and_trusted_tools() {
        use crate::subtask::{
            settle_completed, SettleContext, SpawnedSubtask, SubtaskCompletionSink,
            SubtaskLifecycle, SubtaskRegistry, SubtaskSettled,
        };

        /// 決着通知を 1 件だけ捕まえる sink。
        #[derive(Default)]
        struct CaptureSink(std::sync::Mutex<Option<SubtaskSettled>>);
        impl SubtaskCompletionSink for CaptureSink {
            fn session_prefix(&self) -> &'static str {
                ""
            }
            fn forwards_progress(&self) -> bool {
                true
            }
            fn deliver_continuation(&self, ev: SubtaskSettled) {
                *self.0.lock().unwrap() = Some(ev);
            }
        }

        // owner 発のターンが subtask を spawn した状態を作る。
        let conn = opencrab_db::init_memory().unwrap();
        let db = opencrab_db::Db::from_connection(conn);
        let registry: SubtaskRegistry = std::sync::Arc::new(dashmap::DashMap::new());
        registry.insert(
            "st-1".to_string(),
            SpawnedSubtask {
                abort_handle: tokio::spawn(std::future::pending::<()>()).abort_handle(),
                session_id: "subtask-st-1".to_string(),
                parent_session_id: "discord-agent-1-1-2".to_string(),
                agent_id: "agent-1".to_string(),
                label: "job".to_string(),
                tool_name: "spawn_subtask".to_string(),
                started_at: std::time::Instant::now(),
                reply_target: None,
                caller: CallerIdentity::Owner,
                lifecycle: SubtaskLifecycle::new(),
                steerable: false,
            },
        );

        let sink = CaptureSink::default();
        settle_completed(
            &registry,
            &db,
            &sink,
            SettleContext {
                parent_session_id: "discord-agent-1-1-2".to_string(),
                agent_id: "agent-1".to_string(),
                subtask_id: "st-1".to_string(),
                sub_session_id: "subtask-st-1".to_string(),
                exit_reason: "completed".to_string(),
                lifecycle: SubtaskLifecycle::new(),
            },
            "done",
        );
        let ev = sink.0.lock().unwrap().take().expect("sink が発火する");

        // resume 側は決着通知の caller で実行文脈を組む。
        let (_dir, ctx) = test_context_with_caller(ev.caller);
        let resumed = BridgedExecutor::new(ActionDispatcher::new(), ctx)
            .with_gateway_actions(Arc::new(MockGatewayActionsWithSkills));
        let names: Vec<String> = resumed.list_tools().into_iter().map(|t| t.name).collect();
        assert!(
            names.iter().any(|n| n == "create_skill"),
            "resume 後に trusted_only のツールが消えている: {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "update_instructions"),
            "resume 後に owner_only のツールが消えている: {names:?}"
        );

        // 対照: 最小権限へ降格すると同じツールが丸ごと消える（= このバグの実害）。
        let (_dir2, ctx2) = test_context_with_caller(CallerIdentity::Agent);
        let demoted = BridgedExecutor::new(ActionDispatcher::new(), ctx2)
            .with_gateway_actions(Arc::new(MockGatewayActionsWithSkills));
        let demoted_names: Vec<String> = demoted.list_tools().into_iter().map(|t| t.name).collect();
        assert!(!demoted_names.iter().any(|n| n == "create_skill"));
        assert!(!demoted_names.iter().any(|n| n == "update_instructions"));
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
                    class: opencrab_gateway::ToolClass {
                        dispatch: opencrab_gateway::DispatchMode::Inline,
                        sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                        sharing: opencrab_gateway::ToolSharing::AgentBound,
                    },
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

    /// #264: `nostr_list_keys` は trusted 限定（owner 限定ではない）。未信頼の会話ターン
    /// （caller=Agent）には出さず実行もしないが、owner/co_agent/trusted_user のターンでは
    /// 使える（heartbeat / ダッシュボード / オーナー会話は全て trusted 相当の caller）。
    #[test]
    fn test_nostr_list_keys_is_trusted_only() {
        let p = tool_policy("nostr_list_keys");
        assert!(p.trusted_only, "nostr_list_keys must be trusted_only");
        assert!(
            !p.owner_only,
            "nostr_list_keys should be trusted_only, not owner_only（自分の鍵一覧は自分で見る）"
        );

        // caller=Agent は可視化されない（policy 表の権威を直接見る）。
        let (_d, agent_ctx) = test_context_with_caller(CallerIdentity::Agent);
        let agent_exec = BridgedExecutor::new(ActionDispatcher::new(), agent_ctx);
        assert!(
            !agent_exec.policy_allows("nostr_list_keys"),
            "Agent（未信頼の外部会話ターン）は nostr_list_keys を使えない"
        );
        // caller=TrustedUser は使える。
        let (_d2, trusted_ctx) = test_context_with_caller(CallerIdentity::TrustedUser);
        let trusted_exec = BridgedExecutor::new(ActionDispatcher::new(), trusted_ctx);
        assert!(
            trusted_exec.policy_allows("nostr_list_keys"),
            "TrustedUser は nostr_list_keys を使える"
        );
    }

    /// #264: `nostr_switch_identity`（採用＝接続）は trusted 限定。外部ユーザー由来の
    /// 会話ターン（caller=Agent）には出さず実行もしない（乗っ取り防止）。owner/trusted の
    /// ターン（heartbeat / ダッシュボード / オーナー会話）でだけ自分の意思で採用できる。
    #[test]
    fn test_nostr_switch_identity_is_trusted_only() {
        let p = tool_policy("nostr_switch_identity");
        assert!(p.trusted_only, "nostr_switch_identity must be trusted_only");

        let (_d, agent_ctx) = test_context_with_caller(CallerIdentity::Agent);
        let agent_exec = BridgedExecutor::new(ActionDispatcher::new(), agent_ctx);
        assert!(
            !agent_exec.policy_allows("nostr_switch_identity"),
            "Agent（未信頼の外部会話ターン）は nostr_switch_identity を使えない（乗っ取り防止）"
        );
        let (_d2, owner_ctx) = test_context_with_caller(CallerIdentity::Owner);
        let owner_exec = BridgedExecutor::new(ActionDispatcher::new(), owner_ctx);
        assert!(
            owner_exec.policy_allows("nostr_switch_identity"),
            "Owner は nostr_switch_identity を使える"
        );
    }

    /// #303: `nostr_run` は caller=Agent のターンで**実際にゲートを通る**。
    ///
    /// caller=Agent が指すのは **Nostr 受信ターン**（`crates/nostr/src/sink.rs`）と
    /// 非オーナー相手の会話ターン。ここで塞がると「Nostr 上で自律的に活動する」という
    /// 目的そのものが成立しない。
    ///
    /// リスト（`TRUSTED_ONLY_ACTIONS` に無いこと）だけでは、**別の場所に新しいゲートが
    /// 足された**場合を捕まえられない。そこで `policy_allows` / `list_tools` /
    /// `dispatch_inner`（= `execute`）の 3 経路を実際に通す。
    #[tokio::test]
    async fn nostr_run_passes_the_gate_for_agent_caller() {
        /// `nostr_run` を定義するだけの fake gateway（server 側の実装は別 crate なので）。
        struct GwNostrRun;
        #[async_trait::async_trait]
        impl GatewayActions for GwNostrRun {
            fn definitions(&self) -> Vec<GatewayActionDef> {
                vec![GatewayActionDef {
                    name: "nostr_run".to_string(),
                    class: opencrab_gateway::ToolClass {
                        dispatch: opencrab_gateway::DispatchMode::Inline,
                        sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                        sharing: opencrab_gateway::ToolSharing::AgentBound,
                    },
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

        let (_d, agent_ctx) = test_context_with_caller(CallerIdentity::Agent);
        let agent_exec = BridgedExecutor::new(ActionDispatcher::new(), agent_ctx)
            .with_gateway_actions(Arc::new(GwNostrRun));

        // 1. ポリシー述語（list_tools と dispatch_inner が共有する単一の判定）。
        assert!(
            agent_exec.policy_allows("nostr_run"),
            "caller=Agent（Nostr 受信ターン）で nostr_run が policy_allows を通らない \
             — どこかに caller ゲートが足された"
        );
        // 2. 可視性: モデルに見えていること。
        assert!(
            agent_exec
                .list_tools()
                .iter()
                .any(|t| t.name == "nostr_run"),
            "caller=Agent の list_tools に nostr_run が出ない"
        );
        // 3. 実行時強制: 名前指定の実行が gateway まで到達すること。
        let r = agent_exec
            .execute("nostr_run", &json!({"subcommand": "post"}))
            .await;
        assert!(
            r.success,
            "caller=Agent の nostr_run 実行が拒否された: {:?}",
            r.error
        );
        assert_eq!(r.data["reached_gateway"], true);

        // 対照: owner ターンでも当然通る（Agent 側だけ通す非対称にしていない）。
        let (_d2, owner_ctx) = test_context_with_caller(CallerIdentity::Owner);
        let owner_exec = BridgedExecutor::new(ActionDispatcher::new(), owner_ctx);
        assert!(
            owner_exec.policy_allows("nostr_run"),
            "Owner ターンでも nostr_run は通る"
        );
    }

    /// #319: Nostr 受信ターンの呼び出し元が `Owner` なら、設定変更系（OWNER_ONLY 7 個）
    /// と自分の設定を触る TRUSTED_ONLY が**実際に通る**。
    ///
    /// 発言者からの解決（`NostrAgentRunner::resolve_nostr_caller`）で `Owner` に
    /// なったターンが、ポリシー層でどう扱われるかをここに固定する。以前は Nostr の
    /// 呼び出し元が `Agent` 固定だったため、この一覧が丸ごと消えていた（issue 本文の表）。
    #[test]
    fn test_owner_caller_unlocks_the_tools_missing_from_nostr_turns() {
        // issue #319 で「消えている」と記録されたツール。
        const OWNER_ONLY_FROM_ISSUE: [&str; 7] = [
            "configure_self",
            "configure_nostr",
            "configure_llm_provider",
            "configure_mcp_server",
            "update_instructions",
            "update_heartbeat_instructions",
            "manage_allowed_commands",
        ];
        const TRUSTED_ONLY_FROM_ISSUE: [&str; 4] = [
            "set_my_heartbeat",
            "get_my_heartbeat",
            "nostr_list_keys",
            "nostr_switch_identity",
        ];

        let (_d, owner_ctx) = test_context_with_caller(CallerIdentity::Owner);
        let owner_exec = BridgedExecutor::new(ActionDispatcher::new(), owner_ctx);
        let (_d2, agent_ctx) = test_context_with_caller(CallerIdentity::Agent);
        let agent_exec = BridgedExecutor::new(ActionDispatcher::new(), agent_ctx);

        for name in OWNER_ONLY_FROM_ISSUE.iter().chain(&TRUSTED_ONLY_FROM_ISSUE) {
            assert!(
                owner_exec.policy_allows(name),
                "オーナー発のターンで {name} が通らない"
            );
            assert!(
                !agent_exec.policy_allows(name),
                "他人発のターン（最小権限）で {name} が通ってしまう"
            );
        }
    }

    /// #306: `nostr_zap` は caller=Agent のターンで**実際にゲートを通る**。
    ///
    /// 以前は `nostr_dm` / `nostr_zap` が `TRUSTED_ONLY_ACTIONS` に入っていたが、`nostr_run`
    /// を開けた（#303）時点で `nostr_run dm` / `nostr_run zap` が同じターンから通るため、
    /// inner ツール名を隠すだけのゲートになっていた。一貫性を**制約を減らす方向**で取ると
    /// いうオーナーの決定（#306）に従い外した。ここはその決定を実測で固定する。
    ///
    /// **#514 で `nostr_dm` は撤去した**（DM 送信禁止・定義から削除＋`nostr_run dm` も deny）
    /// ので、#306 の対象から外れ、ここでの検証は `nostr_zap` に絞る。DM のブロックは bridge の
    /// caller ゲート層ではなく定義層と passthrough 層で行う（`crates/nostr`）。
    ///
    /// `nostr_run` 側（`nostr_run_passes_the_gate_for_agent_caller`）と同じく、リストに
    /// 無いことだけを見ても**別の場所に新しいゲートが足された**場合を捕まえられないので、
    /// `policy_allows` / `list_tools` / `dispatch_inner`（= `execute`）の 3 経路を通す。
    #[tokio::test]
    async fn nostr_messaging_passes_the_gate_for_agent_caller() {
        /// `nostr_zap` を定義するだけの fake gateway
        /// （本体は `crates/nostr` にあり、この crate からは参照できない）。
        struct GwNostrMessaging;
        #[async_trait::async_trait]
        impl GatewayActions for GwNostrMessaging {
            fn definitions(&self) -> Vec<GatewayActionDef> {
                ["nostr_zap"]
                    .into_iter()
                    .map(|name| GatewayActionDef {
                        name: name.to_string(),
                        class: opencrab_gateway::ToolClass {
                            dispatch: opencrab_gateway::DispatchMode::Inline,
                            sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                            sharing: opencrab_gateway::ToolSharing::AgentBound,
                        },
                        description: "x".to_string(),
                        parameters: json!({"type": "object"}),
                    })
                    .collect()
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

        let (_d, agent_ctx) = test_context_with_caller(CallerIdentity::Agent);
        let agent_exec = BridgedExecutor::new(ActionDispatcher::new(), agent_ctx)
            .with_gateway_actions(Arc::new(GwNostrMessaging));
        let listed: Vec<String> = agent_exec
            .list_tools()
            .into_iter()
            .map(|t| t.name)
            .collect();

        for name in ["nostr_zap"] {
            // 1. ポリシー述語（list_tools と dispatch_inner が共有する単一の判定）。
            assert!(
                agent_exec.policy_allows(name),
                "caller=Agent（Nostr 受信ターン）で {name} が policy_allows を通らない \
                 — TRUSTED_ONLY_ACTIONS へ戻されたか、別の場所に caller ゲートが足された"
            );
            // 2. 可視性: モデルに見えていること。
            assert!(
                listed.iter().any(|n| n == name),
                "caller=Agent の list_tools に {name} が出ない"
            );
            // 3. 実行時強制: 名前指定の実行が gateway まで到達すること。
            let r = agent_exec.execute(name, &json!({})).await;
            assert!(
                r.success,
                "caller=Agent の {name} 実行が拒否された: {:?}",
                r.error
            );
            assert_eq!(r.data["reached_gateway"], true);
        }

        // 対照: 他ツールの trusted ゲートは維持されている（一律に開けたのではない）。
        for name in ["create_skill", "nostr_switch_identity", "nostr_list_keys"] {
            assert!(
                !agent_exec.policy_allows(name),
                "{name} の trusted ゲートは維持されるべき（#306 は nostr_zap のみ・nostr_dm は #514 で撤去）"
            );
        }
    }

    /// #351 で trusted ゲートへ載せるスキル生成 / 自律学習系（core dispatcher アクション）。
    const SKILL_LEARNING_TRUSTED_ONLY: &[&str] = &[
        "create_my_skill",
        "learn_from_experience",
        "learn_from_peer",
        "reflect_and_learn",
    ];

    /// #351: スキル生成（`create_my_skill`）と自律学習（`learn_from_experience` /
    /// `learn_from_peer` / `reflect_and_learn`）は trusted_only（owner_only ではない）。
    /// gateway 版 `create_skill` と同じ棚に揃っていることをポリシー表の権威で固定する。
    #[test]
    fn skill_and_learning_actions_are_trusted_only_in_policy_table() {
        for name in SKILL_LEARNING_TRUSTED_ONLY {
            let p = tool_policy(name);
            assert!(p.trusted_only, "{name} must be trusted_only (#351)");
            assert!(
                !p.owner_only,
                "{name} は trusted_only であるべき（owner_only にすると CoAgent / \
                 TrustedUser も塞がれる / #351）"
            );
        }
    }

    /// #351: caller=Agent（Nostr 受信ターン / 非オーナー相手の会話ターン）からは、スキル
    /// 生成 / 自律学習系が **3 経路すべて**（`policy_allows` / `list_tools` /
    /// `dispatch_inner`）で落ちること。オーナー要望「スキルを作るのもなし」を実測で固定
    /// する。名前がリストから消えただけでは、別の場所に caller ゲートが足された場合を
    /// 捕まえられないので `nostr_run_passes_the_gate_for_agent_caller` と同じ 3 経路を通す。
    ///
    /// 対照: caller=Owner / CoAgent / TrustedUser（heartbeat tick / ダッシュボード /
    /// オーナー会話 / 信頼済みユーザー会話）では従来どおり 3 経路すべてで通る。
    #[tokio::test]
    async fn skill_and_learning_actions_gated_from_agent_caller() {
        // caller=Agent: 3 経路すべてで落ちる。
        let (_d, agent_ctx) = test_context_with_caller(CallerIdentity::Agent);
        let agent_exec = BridgedExecutor::new(ActionDispatcher::new(), agent_ctx);
        let agent_listed: Vec<String> = agent_exec
            .list_tools()
            .into_iter()
            .map(|t| t.name)
            .collect();

        for name in SKILL_LEARNING_TRUSTED_ONLY {
            // 1. ポリシー述語（list_tools と dispatch_inner が共有する単一の判定）。
            assert!(
                !agent_exec.policy_allows(name),
                "caller=Agent が {name} を policy_allows で通してしまう（#351）"
            );
            // 2. 可視性: モデルに見えていないこと。
            assert!(
                !agent_listed.iter().any(|n| n == name),
                "caller=Agent の list_tools に {name} が出てしまう（#351）"
            );
            // 3. 実行時強制: 名前指定の実行が拒否されること（記憶で名前を呼んでも素通し
            //    しない）。
            let r = agent_exec.execute(name, &json!({})).await;
            assert!(
                !r.success,
                "caller=Agent の {name} 実行が拒否されない（#351）"
            );
        }

        // 対照: Owner / CoAgent / TrustedUser では 3 経路すべてで通る。
        for caller in [
            CallerIdentity::Owner,
            CallerIdentity::CoAgent {
                agent_id: "peer".to_string(),
            },
            CallerIdentity::TrustedUser,
        ] {
            let (_d, ctx) = test_context_with_caller(caller.clone());
            let exec = BridgedExecutor::new(ActionDispatcher::new(), ctx);
            let listed: Vec<String> = exec.list_tools().into_iter().map(|t| t.name).collect();
            for name in SKILL_LEARNING_TRUSTED_ONLY {
                assert!(
                    exec.policy_allows(name),
                    "caller={caller:?} で {name} が policy_allows を通らない（#351 は \
                     trusted を塞がない）"
                );
                assert!(
                    listed.iter().any(|n| n == name),
                    "caller={caller:?} の list_tools に {name} が出ない（#351）"
                );
            }
        }
    }

    /// #356 で trusted ゲートへ載せる、caller=Agent に素通しだった 9 個。
    const PASSTHROUGH_9_TRUSTED_ONLY: &[&str] = &[
        "set_default_webhook",
        "set_default_subtask_webhook",
        "get_default_webhook",
        "get_default_subtask_webhook",
        "list_webhooks",
        "list_subtask_webhooks",
        "update_memory_index_config",
        "get_system_info",
        "list_allowed_commands",
    ];

    /// #356 の 9 個のうち、`SystemGatewayActions`（server 側 own ツール）で定義される 8 個。
    /// 残る 1 個 `get_system_info` は core dispatcher（`ActionDispatcher::new`）側。
    const PASSTHROUGH_9_SERVER_SLOT: &[&str] = &[
        "set_default_webhook",
        "set_default_subtask_webhook",
        "get_default_webhook",
        "get_default_subtask_webhook",
        "list_webhooks",
        "list_subtask_webhooks",
        "update_memory_index_config",
        "list_allowed_commands",
    ];

    /// #356: 素通しだった 9 個はいずれも trusted_only（owner_only ではない）。owner_only に
    /// すると CoAgent / TrustedUser も塞がれてしまう（オーナー決定は「9 個すべて
    /// trusted_only」）。ポリシー表の権威で固定する。
    #[test]
    fn passthrough_actions_are_trusted_only_in_policy_table() {
        assert_eq!(
            PASSTHROUGH_9_TRUSTED_ONLY.len(),
            9,
            "#356 の対象は 9 個（棚卸しで見つかった素通し分）"
        );
        for name in PASSTHROUGH_9_TRUSTED_ONLY {
            let p = tool_policy(name);
            assert!(p.trusted_only, "{name} must be trusted_only (#356)");
            assert!(
                !p.owner_only,
                "{name} は trusted_only であるべき（owner_only にすると CoAgent / \
                 TrustedUser も塞がれる / #356）"
            );
        }
    }

    /// #356: caller=Agent（Nostr 受信ターン / 非オーナー相手の会話ターン）からは、素通し
    /// だった 9 個が **3 経路すべて**（`policy_allows` / `list_tools` / `dispatch_inner`）で
    /// 落ちること。名前がリストから消えただけでは別の場所に caller ゲートが足された場合を
    /// 捕まえられないので `skill_and_learning_actions_gated_from_agent_caller`（#351）と同じ
    /// 3 経路を通す。
    ///
    /// 実行時強制の確認は「拒否された（`!success`）」だけでなく **policy 由来の拒否である
    /// こと**（`is_rejection` = REJECTION_CODE_PREFIX 付き）まで見る。8 個は
    /// `SystemGatewayActions` 由来で、`BridgedExecutor::new` は gateway を注入しないため、
    /// もし policy が拒否しなければ「Unknown action」で `!success` になってしまい、ゲートの
    /// 有無を区別できない。policy 拒否まで確認して初めて「trusted ゲートが効いている」と
    /// 言える（dispatch_inner の policy 判定はルーティングより前 / #45）。
    ///
    /// 対照: caller=Owner / CoAgent / TrustedUser（heartbeat tick / ダッシュボード /
    /// オーナー会話 / 信頼済みユーザー会話）では従来どおり通る（`policy_allows` true、かつ
    /// gateway を注入した list_tools に出る）。
    #[tokio::test]
    async fn passthrough_actions_gated_from_agent_caller() {
        // gateway 源（8 個の server ツール）を注入した executor で list_tools を実測する。
        // get_system_info は dispatcher 側にあるので gateway には入れない。
        let build_exec = |caller: CallerIdentity| {
            let (dir, ctx) = test_context_with_caller(caller);
            let exec = BridgedExecutor::new(ActionDispatcher::new(), ctx)
                .with_gateway_actions(Arc::new(MockGatewayServerSlot8));
            (dir, exec)
        };

        // caller=Agent: 3 経路すべてで落ちる。
        let (_d, agent_exec) = build_exec(CallerIdentity::Agent);
        let agent_listed: Vec<String> = agent_exec
            .list_tools()
            .into_iter()
            .map(|t| t.name)
            .collect();
        for name in PASSTHROUGH_9_TRUSTED_ONLY {
            // 1. ポリシー述語（list_tools と dispatch_inner が共有する単一の判定）。
            assert!(
                !agent_exec.policy_allows(name),
                "caller=Agent が {name} を policy_allows で通してしまう（#356）"
            );
            // 2. 可視性: モデルに見えていないこと（gateway を注入しても policy で除外される）。
            assert!(
                !agent_listed.iter().any(|n| n == name),
                "caller=Agent の list_tools に {name} が出てしまう（#356）"
            );
            // 3. 実行時強制: 名前指定の実行が policy で拒否されること（記憶で名前を呼んでも
            //    素通ししない）。Unknown action ではなく policy 拒否であることまで見る。
            let r = agent_exec.execute(name, &json!({})).await;
            assert!(
                !r.success,
                "caller=Agent の {name} 実行が拒否されない（#356）"
            );
            assert!(
                is_rejection(r.error.as_deref()),
                "caller=Agent の {name} 実行が policy 拒否になっていない \
                 （error={:?} / #356）",
                r.error
            );
        }

        // 対照: Owner / CoAgent / TrustedUser では policy_allows を通り list_tools に出る。
        for caller in [
            CallerIdentity::Owner,
            CallerIdentity::CoAgent {
                agent_id: "peer".to_string(),
            },
            CallerIdentity::TrustedUser,
        ] {
            let (_d, exec) = build_exec(caller.clone());
            let listed: Vec<String> = exec.list_tools().into_iter().map(|t| t.name).collect();
            for name in PASSTHROUGH_9_TRUSTED_ONLY {
                assert!(
                    exec.policy_allows(name),
                    "caller={caller:?} で {name} が policy_allows を通らない（#356 は \
                     trusted を塞がない）"
                );
                assert!(
                    listed.iter().any(|n| n == name),
                    "caller={caller:?} の list_tools に {name} が出ない（#356）"
                );
            }
        }
    }

    /// #359 で trusted ゲートへ載せるタグ操作 3 個（core inline dispatcher アクション）。
    const TAG_ACTIONS_TRUSTED_ONLY: &[&str] = &["tag_topic", "untag_topic", "merge_tags"];

    /// #359: タグ操作 3 個は trusted_only（owner_only ではない）。owner_only にすると
    /// CoAgent / TrustedUser も塞がれてしまう（オーナー決定は「TRUSTED_ONLY / OWNER_ONLY
    /// ではない」）。ポリシー表の権威で固定する。
    #[test]
    fn tag_actions_are_trusted_only_in_policy_table() {
        for name in TAG_ACTIONS_TRUSTED_ONLY {
            let p = tool_policy(name);
            assert!(p.trusted_only, "{name} must be trusted_only (#359)");
            assert!(
                !p.owner_only,
                "{name} は trusted_only であるべき（owner_only にすると CoAgent / \
                 TrustedUser も塞がれる / #359）"
            );
        }
    }

    /// #359: caller=Agent（Nostr 受信ターン / 非オーナー相手の会話ターン）からは、タグ操作
    /// 3 個が **3 経路すべて**（`policy_allows` / `list_tools` / `dispatch_inner`）で落ちる
    /// こと。名前がリストから消えただけでは別の場所に caller ゲートが足された場合を捕まえ
    /// られないので `passthrough_actions_gated_from_agent_caller`（#356）と同じ 3 経路を通す。
    ///
    /// これらは core dispatcher に**実在**するアクション（`ActionDispatcher::new` に登録済み）
    /// なので、もし policy が拒否しなければ execute が実際に走ってしまう（引数不足で
    /// `!success` にはなるが policy 拒否ではない）。よって実行時強制は「拒否された
    /// （`!success`）」だけでなく **policy 由来の拒否であること**（`is_rejection` =
    /// REJECTION_CODE_PREFIX 付き）まで見て、ゲートが効いていることを区別する（#357 に倣う）。
    ///
    /// 対照: caller=Owner / CoAgent / TrustedUser（heartbeat tick / ダッシュボード /
    /// オーナー会話 / 信頼済みユーザー会話）では従来どおり 3 経路すべてで通る。
    #[tokio::test]
    async fn tag_actions_gated_from_agent_caller() {
        // caller=Agent: 3 経路すべてで落ちる。
        let (_d, agent_ctx) = test_context_with_caller(CallerIdentity::Agent);
        let agent_exec = BridgedExecutor::new(ActionDispatcher::new(), agent_ctx);
        let agent_listed: Vec<String> = agent_exec
            .list_tools()
            .into_iter()
            .map(|t| t.name)
            .collect();

        for name in TAG_ACTIONS_TRUSTED_ONLY {
            // 1. ポリシー述語（list_tools と dispatch_inner が共有する単一の判定）。
            assert!(
                !agent_exec.policy_allows(name),
                "caller=Agent が {name} を policy_allows で通してしまう（#359）"
            );
            // 2. 可視性: モデルに見えていないこと。
            assert!(
                !agent_listed.iter().any(|n| n == name),
                "caller=Agent の list_tools に {name} が出てしまう（#359）"
            );
            // 3. 実行時強制: 名前指定の実行が policy で拒否されること（記憶で名前を呼んでも
            //    素通ししない）。実在アクションなので「引数不足エラー」ではなく policy 拒否で
            //    あることまで見る。
            let r = agent_exec.execute(name, &json!({})).await;
            assert!(
                !r.success,
                "caller=Agent の {name} 実行が拒否されない（#359）"
            );
            assert!(
                is_rejection(r.error.as_deref()),
                "caller=Agent の {name} 実行が policy 拒否になっていない（error={:?} / #359）",
                r.error
            );
        }

        // 対照: Owner / CoAgent / TrustedUser では policy_allows を通り list_tools に出る。
        for caller in [
            CallerIdentity::Owner,
            CallerIdentity::CoAgent {
                agent_id: "peer".to_string(),
            },
            CallerIdentity::TrustedUser,
        ] {
            let (_d, ctx) = test_context_with_caller(caller.clone());
            let exec = BridgedExecutor::new(ActionDispatcher::new(), ctx);
            let listed: Vec<String> = exec.list_tools().into_iter().map(|t| t.name).collect();
            for name in TAG_ACTIONS_TRUSTED_ONLY {
                assert!(
                    exec.policy_allows(name),
                    "caller={caller:?} で {name} が policy_allows を通らない（#359 は \
                     trusted を塞がない）"
                );
                assert!(
                    listed.iter().any(|n| n == name),
                    "caller={caller:?} の list_tools に {name} が出ない（#359）"
                );
            }
        }
    }

    /// #379: 記憶の単位（宣言）道具は trusted_only（owner_only ではない）。
    /// #394 で窓を決める `plan_next_memory_window` を同じ扱いで足した。
    const MEMORY_UNIT_ACTIONS_TRUSTED_ONLY: &[&str] = &[
        "survey_my_history",
        "read_my_history",
        "record_memory_unit",
        "retract_memory_unit",
        "plan_next_memory_window",
    ];

    #[test]
    fn memory_unit_actions_are_trusted_only_in_policy_table() {
        for name in MEMORY_UNIT_ACTIONS_TRUSTED_ONLY {
            let p = tool_policy(name);
            assert!(p.trusted_only, "{name} must be trusted_only (#379)");
            assert!(
                !p.owner_only,
                "{name} は trusted_only であるべき（owner_only にすると CoAgent / \
                 TrustedUser も塞がれる / #379）"
            );
        }
    }

    /// #379: caller=Agent からは記憶の単位道具 4 個が **3 経路すべて**（`policy_allows` /
    /// `list_tools` / `dispatch_inner`）で落ちる。対照で Owner / CoAgent / TrustedUser は通る。
    /// タグ道具の `tag_actions_gated_from_agent_caller`（#359）と同型。
    #[tokio::test]
    async fn memory_unit_actions_gated_from_agent_caller() {
        let (_d, agent_ctx) = test_context_with_caller(CallerIdentity::Agent);
        let agent_exec = BridgedExecutor::new(ActionDispatcher::new(), agent_ctx);
        let agent_listed: Vec<String> = agent_exec
            .list_tools()
            .into_iter()
            .map(|t| t.name)
            .collect();

        for name in MEMORY_UNIT_ACTIONS_TRUSTED_ONLY {
            // 1. ポリシー述語（list_tools と dispatch_inner が共有）。
            assert!(
                !agent_exec.policy_allows(name),
                "caller=Agent が {name} を policy_allows で通してしまう（#379）"
            );
            // 2. 可視性: モデルに見えていない。
            assert!(
                !agent_listed.iter().any(|n| n == name),
                "caller=Agent の list_tools に {name} が出てしまう（#379）"
            );
            // 3. 実行時強制: 名前指定の実行が policy 拒否になる（実在アクションなので
            //    「引数不足」ではなく policy 拒否であることまで見る）。
            let r = agent_exec.execute(name, &json!({})).await;
            assert!(
                !r.success,
                "caller=Agent の {name} 実行が拒否されない（#379）"
            );
            assert!(
                is_rejection(r.error.as_deref()),
                "caller=Agent の {name} 実行が policy 拒否になっていない（error={:?} / #379）",
                r.error
            );
        }

        for caller in [
            CallerIdentity::Owner,
            CallerIdentity::CoAgent {
                agent_id: "peer".to_string(),
            },
            CallerIdentity::TrustedUser,
        ] {
            let (_d, ctx) = test_context_with_caller(caller.clone());
            let exec = BridgedExecutor::new(ActionDispatcher::new(), ctx);
            let listed: Vec<String> = exec.list_tools().into_iter().map(|t| t.name).collect();
            for name in MEMORY_UNIT_ACTIONS_TRUSTED_ONLY {
                assert!(
                    exec.policy_allows(name),
                    "caller={caller:?} で {name} が policy_allows を通らない（#379）"
                );
                assert!(
                    listed.iter().any(|n| n == name),
                    "caller={caller:?} の list_tools に {name} が出ない（#379）"
                );
            }
        }
    }

    /// #330 で塞ぐローカル操作系ツール（policy 表の権威 = owner_only、trusted_only ではない）。
    const LOCAL_OWNER_ONLY_TOOLS: &[&str] = &[
        "execute_shell",
        "ws_read",
        "ws_list",
        "ws_write",
        "ws_delete",
        "ws_edit",
        "ws_mkdir",
        "add_allowed_command",
        "remove_allowed_command",
    ];

    /// #330: ローカルのシェル実行 / ファイル操作 / 実行許可リストの自己拡張は owner 限定。
    /// ポリシー表（`tool_policy`）の権威を直接見る。`manage_allowed_commands` と同じ
    /// owner_only（trusted_only ではない）に揃っていること。
    #[test]
    fn local_tools_are_owner_only_in_policy_table() {
        for name in LOCAL_OWNER_ONLY_TOOLS {
            let p = tool_policy(name);
            assert!(p.owner_only, "{name} must be owner_only (#330)");
            assert!(
                !p.trusted_only,
                "{name} は owner_only であるべき（CoAgent / TrustedUser にも開けない / #330）"
            );
        }
    }

    /// #330: caller=Agent（Nostr 受信ターン / 非オーナー相手の会話ターン）からは、上記
    /// ローカル操作系ツールが **3 経路すべて**（`policy_allows` / `list_tools` /
    /// `dispatch_inner`）で落ちること。`nostr_run_passes_the_gate_for_agent_caller` の逆。
    ///
    /// 対照として caller=Owner（heartbeat tick / ダッシュボード / オーナー会話）では 3 経路
    /// すべてで従来どおり使える（gateway まで到達する）ことも固定する。
    #[tokio::test]
    async fn local_tools_are_blocked_for_agent_caller() {
        /// 対象 9 ツールを定義するだけの fake gateway（実装は別 crate / config 駆動なので）。
        struct GwLocal;
        #[async_trait::async_trait]
        impl GatewayActions for GwLocal {
            fn definitions(&self) -> Vec<GatewayActionDef> {
                LOCAL_OWNER_ONLY_TOOLS
                    .iter()
                    .map(|n| GatewayActionDef {
                        name: n.to_string(),
                        class: opencrab_gateway::ToolClass {
                            dispatch: opencrab_gateway::DispatchMode::Inline,
                            sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                            sharing: opencrab_gateway::ToolSharing::AgentBound,
                        },
                        description: "x".to_string(),
                        parameters: json!({"type": "object"}),
                    })
                    .collect()
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

        // caller=Agent: 3 経路すべてで落ちる。
        let (_d, actx) = test_context_with_caller(CallerIdentity::Agent);
        let agent_exec = BridgedExecutor::new(ActionDispatcher::new(), actx)
            .with_gateway_actions(Arc::new(GwLocal));
        let agent_tools: Vec<String> = agent_exec
            .list_tools()
            .into_iter()
            .map(|t| t.name)
            .collect();
        for name in LOCAL_OWNER_ONLY_TOOLS {
            // 1. ポリシー述語。
            assert!(
                !agent_exec.policy_allows(name),
                "caller=Agent が {name} を policy_allows で通してしまう（#330）"
            );
            // 2. 可視性: モデルに見えない。
            assert!(
                !agent_tools.iter().any(|t| t == name),
                "caller=Agent の list_tools に {name} が出てはいけない（#330）"
            );
            // 3. 実行時強制: 名前指定の実行が owner ゲートで拒否される（gateway へ到達しない）。
            let r = agent_exec.execute(name, &json!({})).await;
            assert!(
                !r.success,
                "caller=Agent の {name} 実行は拒否されるべき（#330）"
            );
            assert!(
                r.error.unwrap_or_default().to_lowercase().contains("owner"),
                "{name} の拒否理由は owner ゲートであるべき（#330）"
            );
        }

        // 対照: caller=Owner（heartbeat 相当）では 3 経路すべてで従来どおり使える。
        let (_d2, octx) = test_context_with_caller(CallerIdentity::Owner);
        let owner_exec = BridgedExecutor::new(ActionDispatcher::new(), octx)
            .with_gateway_actions(Arc::new(GwLocal));
        let owner_tools: Vec<String> = owner_exec
            .list_tools()
            .into_iter()
            .map(|t| t.name)
            .collect();
        for name in LOCAL_OWNER_ONLY_TOOLS {
            assert!(
                owner_exec.policy_allows(name),
                "caller=Owner は {name} を使えるべき（heartbeat / 自律活動が死ぬ / #330）"
            );
            assert!(
                owner_tools.iter().any(|t| t == name),
                "caller=Owner の list_tools に {name} が出るべき（#330）"
            );
            // 実行時強制: owner ゲートで**止まらず**先の実装（dispatcher / gateway）へ
            // 到達する。`ws_*` は `ActionDispatcher::new()` に実在するため fake gateway では
            // なく本物の dispatcher が処理する（空引数で失敗しうるが、その失敗は owner
            // ゲート由来ではない）。よって「owner 拒否文言が出ないこと」で到達を判定する。
            let r = owner_exec.execute(name, &json!({})).await;
            assert!(
                !r.error
                    .as_deref()
                    .unwrap_or_default()
                    .contains("requires owner"),
                "caller=Owner の {name} が owner ゲートで拒否された: {:?}（#330）",
                r.error
            );
        }

        // heartbeat 相当（caller=Owner）で `execute_shell` が gateway まで到達すること
        // （dispatcher に無い config 駆動ツールなので fake gateway が処理し、往復を確認できる）。
        let r = owner_exec.execute("execute_shell", &json!({})).await;
        assert!(
            r.success,
            "caller=Owner の execute_shell 実行が拒否された: {:?}（#330）",
            r.error
        );
        assert_eq!(
            r.data["reached_gateway"], true,
            "execute_shell が gateway へ到達しない（heartbeat 経路が死ぬ / #330）"
        );

        // #485: co_agent は owner 等価。owner と同じく 3 経路すべてで LOCAL_OWNER_ONLY_TOOLS を
        // 使え、execute_shell が gateway まで到達する（オーナーの「co_agent に execute_shell /
        // ファイル操作を開放して」を満たす）。is_owner_equivalent から CoAgent を外すと落ちる。
        let (_d3, cctx) = test_context_with_caller(CallerIdentity::CoAgent {
            agent_id: "peer".to_string(),
        });
        let co_exec = BridgedExecutor::new(ActionDispatcher::new(), cctx)
            .with_gateway_actions(Arc::new(GwLocal));
        let co_tools: Vec<String> = co_exec.list_tools().into_iter().map(|t| t.name).collect();
        for name in LOCAL_OWNER_ONLY_TOOLS {
            assert!(
                co_exec.policy_allows(name),
                "caller=CoAgent は {name} を使えるべき（#485: owner 等価）"
            );
            assert!(
                co_tools.iter().any(|t| t == name),
                "caller=CoAgent の list_tools に {name} が出るべき（#485）"
            );
            let r = co_exec.execute(name, &json!({})).await;
            assert!(
                !r.error
                    .as_deref()
                    .unwrap_or_default()
                    .contains("requires owner"),
                "caller=CoAgent の {name} が owner ゲートで拒否された: {:?}（#485）",
                r.error
            );
        }
        let r = co_exec.execute("execute_shell", &json!({})).await;
        assert!(
            r.success,
            "caller=CoAgent の execute_shell 実行が拒否された: {:?}（#485）",
            r.error
        );
        assert_eq!(
            r.data["reached_gateway"], true,
            "execute_shell が gateway へ到達しない（#485: co_agent = owner 等価）"
        );
    }

    /// #330/#333: 判定軸は caller だけで、depth は増えても owner の可否を変えない。
    ///
    /// #333 で sub-engine は親ターンの caller を継承するようになった
    /// （`subtask_spawn.rs` が `spawn_subtask` の sub-run に親 caller を渡す）。したがって
    /// **Owner ターンから起動したサブ（caller=Owner・depth>=1）は実在する構成**で、そこで
    /// `execute_shell` / `ws_*` が使える必要がある（メインで直接やるのと同じ = 委譲都合の
    /// 非対称を作らない）。逆に **Agent ターンから起動したサブ（caller=Agent・depth>=1）は
    /// 塞がったまま**でなければならない（`spawn_subtask` を挟んだ迂回の封鎖）。
    ///
    /// これらは sub-engine 遮断属性（`class.sub_engine == Blocked`）を持たないので、判定は
    /// caller のみ。
    #[test]
    fn local_tools_gated_by_caller_only_regardless_of_depth() {
        // Owner: depth 0 でも depth>=1 でも使える（実在するサブ構成 = 親 Owner → サブ Owner）。
        let (_d, octx) = test_context_with_caller(CallerIdentity::Owner);
        let owner_depth0 = BridgedExecutor::new(ActionDispatcher::new(), octx);
        let (_d2, octx2) = test_context_with_caller(CallerIdentity::Owner);
        let owner_depth1 = BridgedExecutor::new(ActionDispatcher::new(), octx2).with_depth(1);
        // Agent: どの depth でも塞がる（親 Agent → サブ Agent の迂回封鎖）。
        let (_d3, actx) = test_context_with_caller(CallerIdentity::Agent);
        let agent_depth0 = BridgedExecutor::new(ActionDispatcher::new(), actx);
        let (_d4, actx2) = test_context_with_caller(CallerIdentity::Agent);
        let agent_depth1 = BridgedExecutor::new(ActionDispatcher::new(), actx2).with_depth(1);
        for name in LOCAL_OWNER_ONLY_TOOLS {
            assert!(
                !owner_depth1.is_blocked_in_subengine(name),
                "{name} に depth ゲートを足していないこと（#330）"
            );
            assert_eq!(
                owner_depth0.policy_allows(name),
                owner_depth1.policy_allows(name),
                "{name} の owner 可否が depth 0 と depth>=1 で食い違ってはいけない（#330）"
            );
            assert!(
                owner_depth1.policy_allows(name),
                "caller=Owner のサブ（depth>=1）で {name} が使えないと実装作業が死ぬ（#333）"
            );
            assert!(
                !agent_depth1.policy_allows(name),
                "caller=Agent のサブ（depth>=1）で {name} が通ると spawn_subtask 迂回が開く（#333）"
            );
            assert_eq!(
                agent_depth0.policy_allows(name),
                agent_depth1.policy_allows(name),
                "{name} の agent 可否が depth で変わってはいけない（#333）"
            );
        }
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
                class: opencrab_gateway::ToolClass {
                    dispatch: opencrab_gateway::DispatchMode::Inline,
                    sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                    sharing: opencrab_gateway::ToolSharing::AgentBound,
                },
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

    /// #158 S1: gateway の `execute` に渡る ctx が、この run を起こした inbound の
    /// 返信先（gateway 不透明 token）を運ぶこと。宛先引数を省略したツール呼び出しの
    /// フォールバック源になる。
    #[tokio::test]
    async fn test_gateway_ctx_carries_reply_target() {
        let (_dir, ctx) = test_context();
        let gw = Arc::new(CtxRecordingGateway {
            last_ctx: Mutex::new(None),
            last_args: Mutex::new(None),
        });
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
            .with_gateway_actions(gw.clone())
            .with_reply_target(Some("note1abcdef".to_string()));
        let r = executor.execute("ctx_probe", &json!({})).await;
        assert!(r.success);
        let seen = gw.last_ctx.lock().unwrap().clone().unwrap();
        assert_eq!(seen.reply_target.as_deref(), Some("note1abcdef"));
    }

    /// #158 S1 非退行: 返信先を注入しない executor は ctx.reply_target が None のまま
    /// （後方互換 = 宛先を明示する呼び出しの挙動は変わらない）。
    #[tokio::test]
    async fn test_gateway_ctx_reply_target_none_by_default() {
        let (_dir, ctx) = test_context();
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx);
        assert!(executor.gateway_call_context().reply_target.is_none());
    }

    /// #158 S1: Nostr 経路（`RunRequest.reply_target` を載せる gateway）で、`process.rs`
    /// と同じ配線を通すとツール実行の文脈に返信先が**非 None** で届くこと。Nostr は既に
    /// `RunRequest` に返信先を入れている（#167）ので、この段だけで効く。
    #[tokio::test]
    async fn test_nostr_run_request_reply_target_reaches_gateway_ctx() {
        use crate::run_request::RunRequest;

        let (_dir, action_ctx) = test_context();
        // Nostr gateway が inbound の返信先（イベント id）を RunRequest に載せる。
        let req = RunRequest::new(
            "agent-a",
            "A",
            "nostr-agent-a-npub1sender",
            "sys",
            "conv",
            "nostr",
            CallerIdentity::Agent,
        )
        .with_reply_target("note1abcdef");

        let gw = Arc::new(CtxRecordingGateway {
            last_ctx: Mutex::new(None),
            last_args: Mutex::new(None),
        });
        // process.rs の executor 構築と同じ配線（RunRequest → BridgedExecutor）。
        let executor = BridgedExecutor::new(ActionDispatcher::new(), action_ctx)
            .with_gateway_actions(gw.clone())
            .with_reply_target(req.reply_target.clone());

        assert!(executor.execute("ctx_probe", &json!({})).await.success);
        let seen = gw.last_ctx.lock().unwrap().clone().unwrap();
        assert!(
            seen.reply_target.is_some(),
            "Nostr 経路ではツール文脈の返信先が非 None になる"
        );
        assert_eq!(seen.reply_target.as_deref(), Some("note1abcdef"));
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
        // `create_skill`（#157 S6）と `update_heartbeat_instructions` /
        // `read_heartbeat_instructions`（#157 S3）は server 側の合成 gateway が実装する
        // （実在性は server crate のテストで検証）。execute_skill は防御的エントリ
        // （実装なし）であることをここで明文化する。
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
                class: opencrab_gateway::ToolClass {
                    dispatch: opencrab_gateway::DispatchMode::Inline,
                    sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                    sharing: opencrab_gateway::ToolSharing::AgentBound,
                },
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
                class: opencrab_gateway::ToolClass {
                    dispatch: opencrab_gateway::DispatchMode::Inline,
                    sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                    sharing: opencrab_gateway::ToolSharing::AgentBound,
                },
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
                class: opencrab_gateway::ToolClass {
                    dispatch: opencrab_gateway::DispatchMode::Inline,
                    sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                    sharing: opencrab_gateway::ToolSharing::AgentBound,
                },
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

    // ---- #620: sink は args/result を**そのまま**受け取る（nsec キー名マスクは撤去） ----

    /// 各イベントが sink で実際に受け取った args / result を保存する sink。
    /// bridge が渡した値そのものを観測する。
    struct IoCapturingSink {
        #[allow(clippy::type_complexity)]
        seen: Mutex<Vec<(serde_json::Value, Option<serde_json::Value>)>>,
    }
    impl ToolEventSink for IoCapturingSink {
        fn on_event(&self, ev: &ToolEvent<'_>) {
            self.seen
                .lock()
                .unwrap()
                .push((ev.args.clone(), ev.result.cloned()));
        }
    }

    /// #620: args は sink へ**そのまま**（改変せず）渡る。キー名マスク（SECRET_KEYS）は
    /// 撤去した。実際には `nsec` を JSON キーに持つ引数を出す producer は皆無なので、
    /// この撤去で外部へ出る内容は実運用では変わらない（マスク痕跡 `[redacted]` は付かない）。
    #[tokio::test]
    async fn test_sink_receives_raw_args_unchanged() {
        let (_dir, ctx) = test_context();
        let sink = Arc::new(IoCapturingSink {
            seen: Mutex::new(Vec::new()),
        });
        let executor =
            BridgedExecutor::new(ActionDispatcher::new(), ctx).with_tool_event_sink(sink.clone());
        let args = json!({"command": "echo hi", "npub": "npub1ok"});
        let _ = executor.execute("tool_no_secret", &args).await;
        let seen = sink.seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "start/terminal の 2 イベント");
        for (a, _) in seen.iter() {
            assert_eq!(*a, args, "args が改変された（sink は生で受け取るはず）");
            assert!(
                !a.to_string().contains("[redacted]"),
                "撤去したはずのマスク痕跡が付いている"
            );
        }
    }
}
