use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::{info, warn};

use opencrab_llm::providers::*;
use opencrab_llm::router::LlmRouter;
use opencrab_llm::traits::LlmProvider;

// ---------- Config structs (match config/default.toml) ----------

#[derive(Debug, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub agent: AgentConfig,
    #[serde(default)]
    pub llm: LlmConfig,
    #[serde(default)]
    pub gateway: GatewayConfig,
    #[serde(default)]
    pub database: DatabaseConfig,
    #[serde(default)]
    pub tools: opencrab_actions::tools::ToolsConfig,
    #[serde(default)]
    pub evaluator: EvaluatorConfig,
    /// スリープ時スキル棚卸し（自己 curation ループ）。
    #[serde(default)]
    pub skill_consolidation: SkillConsolidationConfig,
    /// 記憶カテゴリ層（#313/#344）の sleep 中自動割当。既定オフ（#345）。
    #[serde(default)]
    pub category_maintenance: CategoryMaintenanceConfig,
    /// スリープ整理ラン（#313 段階3 / #361）。既定オフ（opt-in / #346）。
    #[serde(default)]
    pub memory_organize: MemoryOrganizeConfig,
    /// スリープ宣言ラン（#384 / #376 段階2）。既定オフ（opt-in / #346）。
    #[serde(default)]
    pub memory_declare: MemoryDeclareConfig,
    /// スリープ凝縮ラン（#411 / 記憶の 3 段目）。既定オフ（opt-in）。
    #[serde(default)]
    pub memory_condense: MemoryCondenseConfig,
    /// VC 対話（STT/TTS）。既定は無効。
    #[serde(default)]
    pub voice: opencrab_voice::VoiceConfig,
    /// 非ブロックツール実行（dispatch / RFC #152 S3a）。
    #[serde(default)]
    pub subtask: SubtaskConfig,
    /// 古い LLM ログの zip アーカイブ（#337）。
    #[serde(default)]
    pub llm_log_archive: LlmLogArchiveConfig,
    /// 外部イベント受信（webhook intake / issue #454）。既定は実質無効。
    #[serde(default)]
    pub intake: IntakeConfig,
}

/// 古い `llm_logs` を zip へ書き出して DB から外す設定（#337）。
///
/// `llm_logs` は 1 行に「プロンプト全文 + 応答」を丸ごと保存するため肥大しやすく
/// （実測で DB の 97%）、バックアップを重くする。デバッグに使う直近は残し、保持期間
/// より古い**月**を丸ごと `data/archive/llm_logs-YYYY-MM.jsonl.zip` へ書き出して DB
/// から外す。**書き出して検証してから削除する**（`llm_log_archive` モジュール参照）。
///
/// `memory_sessions`（会話ログ = 記憶の本体）には手を出さない。対象は `llm_logs` のみ。
#[derive(Debug, Deserialize, Clone)]
pub struct LlmLogArchiveConfig {
    /// アーカイブループの on/off。既定 true（#337 の目的そのもの）。
    /// 自動削除を止めたい運用者はここを false にすれば再ビルド無しで無効化できる。
    #[serde(default = "default_archive_enabled")]
    pub enabled: bool,
    /// 保持日数。これより**古い月**（月末がカットオフより前）だけをアーカイブする。
    /// 既定 30 日。境界の月は丸ごと残す（実際の保持は最大 1 か月ぶん長くなりうる）。
    #[serde(default = "default_archive_retention_days")]
    pub retention_days: i64,
    /// アーカイブ tick の間隔（秒）。既定 86400（日次）。最低 3600 秒に丸める。
    #[serde(default = "default_archive_interval_secs")]
    pub interval_secs: u64,
    /// 出力ディレクトリ。空なら DB ファイルの親 + `archive`（例: `data/archive`）に
    /// 導出する。**内蔵ディスクに置かない**方針のため、既定は DB と同じボリューム。
    #[serde(default)]
    pub dir: String,
}

impl Default for LlmLogArchiveConfig {
    fn default() -> Self {
        Self {
            enabled: default_archive_enabled(),
            retention_days: default_archive_retention_days(),
            interval_secs: default_archive_interval_secs(),
            dir: String::new(),
        }
    }
}

fn default_archive_enabled() -> bool {
    true
}
fn default_archive_retention_days() -> i64 {
    30
}
fn default_archive_interval_secs() -> u64 {
    86400
}

/// 外部イベント受信（webhook intake / issue #454）の設定。
///
/// `POST /api/hooks/{source}` で受け取った出来事を `agent_inbox` に積み、専用の消化ループ
/// （`intake_process`）が heartbeat とは独立に処理する。真実は source 側の一覧 API とし、
/// webhook で落ちた分は catch-up ポーリングが補充する。
///
/// **未設定（`[intake]` セクションが無い / secret 未設定 / route 無し）なら実質無効。**
/// テーブルは空のまま、消化ループは未処理 0 件で LLM を呼ばない。塞がず、事実を書いて選ばせる。
#[derive(Debug, Deserialize, Clone)]
pub struct IntakeConfig {
    /// source ごとの共有 secret（HMAC-SHA256 検証用）。key = source 名 / value = secret。
    /// `${ENV}` 展開済み。**このマップに無い（または空文字の）source への POST は 404**。
    /// secret はログ・エラーメッセージに出さない。
    #[serde(default)]
    pub secrets: HashMap<String, String>,
    /// source×event_type → agent_id のルーティング（完全一致）。該当が無いイベントは
    /// 受理（202）はするが受信箱に積まない（配送先が無いため）。
    #[serde(default)]
    pub routes: Vec<IntakeRoute>,
    /// 受信箱消化ループの間隔（秒）。既定 60。ループ側で最低 10 秒に丸める。
    /// **未処理が空の tick は LLM を呼ばない**（DB 1 クエリのみ / コスト制御・受け入れ基準）。
    #[serde(default = "default_intake_process_interval_secs")]
    pub process_interval_secs: u64,
    /// catch-up ポーリングの間隔（秒）。既定 600（10分）。ループ側で最低 60 秒に丸める。
    #[serde(default = "default_intake_catch_up_interval_secs")]
    pub catch_up_interval_secs: u64,
    /// omoikane source アダプタ（第一号）。未設定なら omoikane の catch-up はしない
    /// （webhook 受信は secret さえ設定すれば動く）。
    #[serde(default)]
    pub omoikane: Option<OmoikaneConfig>,
}

impl Default for IntakeConfig {
    fn default() -> Self {
        Self {
            secrets: HashMap::new(),
            routes: Vec::new(),
            process_interval_secs: default_intake_process_interval_secs(),
            catch_up_interval_secs: default_intake_catch_up_interval_secs(),
            omoikane: None,
        }
    }
}

impl IntakeConfig {
    /// source の共有 secret を返す。未設定 / 空文字は `None`（＝webhook は 404）。
    pub fn secret_for(&self, source: &str) -> Option<&str> {
        self.secrets
            .get(source)
            .map(|s| s.as_str())
            .filter(|s| !s.is_empty())
    }

    /// (source, event_type) の配送先 agent_id を返す（完全一致・先勝ち）。
    pub fn route_agent(&self, source: &str, event_type: &str) -> Option<&str> {
        self.routes
            .iter()
            .find(|r| r.source == source && r.event_type == event_type)
            .map(|r| r.agent_id.as_str())
    }
}

/// source×event_type → agent_id の 1 ルート。
#[derive(Debug, Deserialize, Clone)]
pub struct IntakeRoute {
    pub source: String,
    pub event_type: String,
    pub agent_id: String,
}

/// omoikane（ナレッジベース）source アダプタの設定。catch-up 一覧 API の接続先。
#[derive(Debug, Deserialize, Clone)]
pub struct OmoikaneConfig {
    /// catch-up ポーリングの有効/無効。既定 true（セクションを書いた時点で使う想定）。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 一覧 API のベース URL（例: `https://omoikane.example/`）。空なら catch-up しない。
    #[serde(default)]
    pub base_url: String,
    /// Bearer トークン（`${ENV}` 展開済み）。秘密。ログに出さない。
    #[serde(default)]
    pub bearer_token: String,
    /// `entry_created_by` フィルタ（対象エージェントの uid）。
    #[serde(default)]
    pub entry_created_by: String,
    /// 1 回のポーリングで取得する上限件数。既定 50。
    #[serde(default = "default_omoikane_poll_limit")]
    pub poll_limit: u32,
}

fn default_true() -> bool {
    true
}
fn default_intake_process_interval_secs() -> u64 {
    60
}
fn default_intake_catch_up_interval_secs() -> u64 {
    600
}
fn default_omoikane_poll_limit() -> u32 {
    50
}

/// 非ブロックツール実行（dispatch）の設定（RFC #152 S3a）。
///
/// dispatch は「LLM のツール呼び出しを background subtask として実行し、そのターンには
/// `{"status":"spawned"}` だけを返して完了後に別ターンで再注入する」挙動。除外集合
/// （`opencrab_actions::default_non_dispatch_tools`）のツールは従来どおり inline 実行。
#[derive(Debug, Deserialize, Clone)]
pub struct SubtaskConfig {
    /// 自動 dispatch の有効/無効（**kill switch**）。
    ///
    /// `false` にすると全ツールが inline 実行に戻る（この機能導入前の挙動）。
    /// 回帰を踏んだ運用者が再ビルドせずに戻せる唯一の手段なので消さないこと。
    /// 環境変数 `OPENCRAB_SUBTASK_AUTO_DISPATCH`（`0`/`false`/`off`/`no` で無効）が
    /// TOML より優先する（`.env` だけで切り戻せるように）。
    #[serde(default = "default_subtask_auto_dispatch")]
    pub auto_dispatch: bool,

    /// 設定ファイル由来の**通知先フォールバック**（#157 S5）。
    ///
    /// 通知先の解決順は「明示指定 → DB の scope 別既定（tool>agent>global）→ ここ」。
    /// DB 行が 1 つも無いときだけ効く最後の砦（`WebhookSource::EnvConfig`）。
    ///
    /// 元は `[gateway.discord] default_subtask_webhook` にあり、**Discord 機能が有効な
    /// ビルドの Discord 起動ブロックでしか読まれていなかった**ため、web / REST / Nostr /
    /// heartbeat の経路からは到達できなかった。transport 非依存の `[subtask]` 名前空間へ
    /// 持ち上げる。旧キーは後方互換のフォールバックとして読み続ける
    /// （[`AppConfig::default_subtask_webhook`]）。
    #[serde(default)]
    pub default_webhook: Option<SubtaskWebhookConfig>,
}

impl Default for SubtaskConfig {
    fn default() -> Self {
        Self {
            auto_dispatch: default_subtask_auto_dispatch(),
            default_webhook: None,
        }
    }
}

impl AppConfig {
    /// 設定ファイル由来の通知先フォールバックを解決する（#157 S5）。
    ///
    /// 優先順位は **新キー `[subtask] default_webhook` → 旧キー
    /// `[gateway.discord] default_subtask_webhook`**。旧キーだけを書いた既存の設定
    /// ファイルはそのまま動き続ける（後方互換）。url が空/空白のみなら「未設定」。
    ///
    /// transport の機能フラグから独立している点が要点で、`#[cfg(feature = "discord")]`
    /// の外から呼べる。`AppState::default_subtask_webhook` へはこの結果を入れる。
    pub fn default_subtask_webhook(
        &self,
    ) -> Option<opencrab_actions::webhook_target::WebhookConfig> {
        self.subtask
            .default_webhook
            .as_ref()
            .or(self.gateway.discord.default_subtask_webhook.as_ref())
            .and_then(|c| {
                opencrab_actions::webhook_target::WebhookConfig::from_parts(
                    c.url.clone(),
                    c.events.clone(),
                )
            })
    }

    /// 新キーが**空の url で書かれていて**、旧キーの有効な値を隠しているか（#207）。
    ///
    /// [`Self::default_subtask_webhook`] の解決は「新キーの**節があるか**」で分岐し、
    /// url が空かどうかを見るのはその後。よって新キーを `url = ""` で書くと、旧キーに
    /// 有効な値があっても通知先は未設定になる。これは設定例の記述（「両方書いた場合は
    /// 新しい方が優先」「url が空なら無効」）どおりの**意図した挙動**（通知を明示的に
    /// 止める手段）なので解決順序は変えない。ただし黙って起きると原因が分からないので
    /// 起動時に警告する（[`Self::warn_if_legacy_webhook_masked`]）。
    ///
    /// 踏む経路: 設定例の `default_webhook = { url = "${SUBTASK_WEBHOOK_URL}" }` の
    /// コメントを外したが `.env` に変数を入れていない場合。`${VAR}` の展開は未定義の
    /// 変数を空文字にするため url が空になる。
    pub fn legacy_webhook_masked_by_empty_new_key(&self) -> bool {
        legacy_webhook_masked_by_empty_new_key(
            self.subtask.default_webhook.as_ref(),
            self.gateway.discord.default_subtask_webhook.as_ref(),
        )
    }

    /// 新キーが旧キーの有効な値を隠しているとき警告する。警告したら `true`。
    ///
    /// 挙動は変えない（通知先の解決結果には触らない）。何が起きているかと、どう直せば
    /// よいか（新キーに値を入れる / 新キーの行を消す）が本文から分かるようにする。
    pub fn warn_if_legacy_webhook_masked(&self) -> bool {
        if !self.legacy_webhook_masked_by_empty_new_key() {
            return false;
        }
        warn!(
            "[subtask] default_webhook has an empty url, so the value in \
             [gateway.discord] default_subtask_webhook is NOT used and subtask lifecycle \
             notifications have no destination (an empty url means \"disabled\"; the newer key \
             wins when both are set). If you meant to migrate, put the real URL in \
             [subtask] default_webhook (check that the ${{VAR}} it references is set in .env \
             -- undefined variables expand to an empty string). If you meant to keep using the \
             legacy key, delete the [subtask] default_webhook line. If you meant to turn \
             notifications off, this warning is expected."
        );
        true
    }
}

/// [`AppConfig::legacy_webhook_masked_by_empty_new_key`] の判定本体（#207）。
///
/// 「新キーの節はあるが url が空」かつ「旧キーに有効な url がある」ときだけ真。
/// 新キーの節が無ければ旧キーがそのまま使われるので隠していない。新キーに有効な url が
/// あれば新キーが使われる（これは意図どおりの優先）ので警告しない。
fn legacy_webhook_masked_by_empty_new_key(
    new_key: Option<&SubtaskWebhookConfig>,
    legacy_key: Option<&SubtaskWebhookConfig>,
) -> bool {
    let new_key_is_empty = match new_key {
        Some(c) => c.url.trim().is_empty(),
        None => return false,
    };
    let legacy_has_value = legacy_key.is_some_and(|c| !c.url.trim().is_empty());
    new_key_is_empty && legacy_has_value
}

fn default_subtask_auto_dispatch() -> bool {
    true
}

/// dispatch の kill switch を上書きする環境変数名。
pub const SUBTASK_AUTO_DISPATCH_ENV: &str = "OPENCRAB_SUBTASK_AUTO_DISPATCH";

/// `OPENCRAB_SUBTASK_AUTO_DISPATCH` を bool として解釈する。
///
/// 未設定 / 空 / 解釈不能なら `None`（TOML 値を使う）。真偽の綴りは緩く受ける
/// （`1/true/on/yes` と `0/false/off/no`、大小文字無視）。
fn auto_dispatch_from_env() -> Option<bool> {
    let raw = std::env::var(SUBTASK_AUTO_DISPATCH_ENV).ok()?;
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "on" | "yes" => Some(true),
        "0" | "false" | "off" | "no" => Some(false),
        _ => None,
    }
}

/// スリープ時スキル棚卸しの設定（design-sleep-skill-consolidation.md §10）。
#[derive(Debug, Deserialize, Clone)]
pub struct SkillConsolidationConfig {
    /// ループ全体の on/off。
    #[serde(default = "default_sc_enabled")]
    pub enabled: bool,
    /// 発火する新規活動（未処理セッション）数 N。
    #[serde(default = "default_sc_trigger")]
    pub trigger_new_sessions: i64,
    /// 保険トリガの時間キャップ（時間）。
    #[serde(default = "default_sc_time_cap")]
    pub time_cap_hours: i64,
    /// 最短間隔フロア（秒）。
    #[serde(default = "default_sc_min_interval")]
    pub min_interval_secs: i64,
    /// 棚卸しパケットに含める archived スキル数（再検討用）。
    #[serde(default = "default_sc_include_archived")]
    pub include_archived_in_review: i64,
}

impl Default for SkillConsolidationConfig {
    fn default() -> Self {
        Self {
            enabled: default_sc_enabled(),
            trigger_new_sessions: default_sc_trigger(),
            time_cap_hours: default_sc_time_cap(),
            min_interval_secs: default_sc_min_interval(),
            include_archived_in_review: default_sc_include_archived(),
        }
    }
}

fn default_sc_enabled() -> bool {
    // 設計 doc の既定は true だが、LLM を消費する自律ループのため安全側に倒して
    // opt-in（既定 false）とする。運営者が config で明示的に有効化する。
    false
}
fn default_sc_trigger() -> i64 {
    10
}
fn default_sc_time_cap() -> i64 {
    24
}
fn default_sc_min_interval() -> i64 {
    3600
}
fn default_sc_include_archived() -> i64 {
    3
}

/// 記憶カテゴリ層の sleep 中自動割当の設定（#345）。
///
/// #313/#344 で入ったカテゴリ層は、sleep 中に「種まき + 未分類 topic の LLM 割当」を
/// 毎 tick 行う。#313 の方針が「エージェント自身に整理させる（一期一会）」へ変わり、
/// いまの単一ラベル・sticky・12件ずつの割当は作り直しになるため、作り直す前提の処理へ
/// LLM 費用を払い続けないよう、`enabled` で丸ごと止められるようにする。**既定オフ**。
/// `skill_consolidation` と同じく LLM を消費する自律処理なので同じ流儀（opt-in）に揃える。
#[derive(Debug, Deserialize, Clone)]
pub struct CategoryMaintenanceConfig {
    /// 種まき + 割当ブロック全体の on/off。既定 false（#345）。
    #[serde(default = "default_category_maintenance_enabled")]
    pub enabled: bool,
}

impl Default for CategoryMaintenanceConfig {
    fn default() -> Self {
        Self {
            enabled: default_category_maintenance_enabled(),
        }
    }
}

fn default_category_maintenance_enabled() -> bool {
    false
}

/// スリープ整理ラン（#313 段階3 / #361）の設定。
///
/// エージェント本人が**別セッションの新規 context**で、自分の記憶（topic）に自分の人格で
/// タグを付けて整理する（`browse/search/retrieve_memory` + `tag_topic`/`untag_topic`/
/// `merge_tags`）。呼び出し元は sleep の memory maintenance ループのみ（対話ターンでは
/// 走らせない = #291）。LLM を消費する自律ランなので `skill_consolidation` /
/// `category_maintenance` と同じく **既定オフ（opt-in / #346）**。
///
/// `max_topics`（1 回の worklist 上限 N）・`min_new_topics`（発火下限）・`min_interval_minutes`
/// （日次ゲート）は**実測してから既定を確定する**ため config 可変にする（#313 の設計）。
#[derive(Debug, Deserialize, Clone)]
pub struct MemoryOrganizeConfig {
    /// 整理ラン全体の on/off。既定 false。
    #[serde(default = "default_mo_enabled")]
    pub enabled: bool,
    /// 1 回の worklist に載せる新規 topic の上限 N（bounded worklist）。初期 50。
    #[serde(default = "default_mo_max_topics")]
    pub max_topics: i64,
    /// 発火の下限。スナップショット以下の新規 topic がこの件数以上溜まったら発火。初期 20。
    #[serde(default = "default_mo_min_new_topics")]
    pub min_new_topics: i64,
    /// 日次ゲート。前回マーカーからこの**分数**以上経っていないと発火しない。既定 1440（= 24 時間）。
    /// 分単位なのはバックログ消化（#390）で一時的に間隔を詰めるため。定常運用では既定のまま使う。
    #[serde(default = "default_mo_min_interval_minutes")]
    pub min_interval_minutes: i64,
    /// 整理ラン 1 回のタイムアウト（秒）。超えたら partial 扱いでマーカーを前進させない。
    #[serde(default = "default_mo_timeout_secs")]
    pub timeout_secs: u64,
}

impl Default for MemoryOrganizeConfig {
    fn default() -> Self {
        Self {
            enabled: default_mo_enabled(),
            max_topics: default_mo_max_topics(),
            min_new_topics: default_mo_min_new_topics(),
            min_interval_minutes: default_mo_min_interval_minutes(),
            timeout_secs: default_mo_timeout_secs(),
        }
    }
}

fn default_mo_enabled() -> bool {
    false
}
fn default_mo_max_topics() -> i64 {
    50
}
fn default_mo_min_new_topics() -> i64 {
    20
}
fn default_mo_min_interval_minutes() -> i64 {
    1440
}
fn default_mo_timeout_secs() -> u64 {
    600
}

/// スリープ宣言ラン（#384 / #376 段階2）の設定。
///
/// エージェント本人が**別セッションの新規 context**で、自分の生ログ（memory_sessions）を
/// 俯瞰し、「どこからどこまでが 1 つの記憶か」を宣言する（`survey_my_history` /
/// `read_my_history` / `record_memory_unit` / `retract_memory_unit`）。タグ整理ラン
/// （[`MemoryOrganizeConfig`]）とは**入力も進捗マーカーも別**の独立したランで、足回り
/// （`OrganizeTurnRunner` / 排他スロット / caller=Owner / ツール許可リスト）だけを共有する。
/// 呼び出し元は sleep の memory maintenance ループのみ（対話ターンでは走らせない = #291）。
/// LLM を消費する自律ランなので `memory_organize` と同じく **既定オフ（opt-in / #346）**。
///
/// `max_logs`（1 回で提示する未宣言ログの枠）・`min_new_logs`（発火下限）・`min_interval_minutes`
/// （日次ゲート）は**実測してから既定を確定する**ため config 可変にする。既定は #313 の実測に
/// 倣う: 20 件では材料が薄く抽象タグしか出ず、100 件で情緒の軸が出た → 枠 100・下限 100。
///
/// `max_logs` は**枠の既定**であって固定値ではない（#394）。エージェント本人が
/// `plan_next_memory_window` で広さを表明していれば、そちらが（上下限へ丸めた上で）優先される。
/// 未表明のエージェントはここの値でそのまま走る。
#[derive(Debug, Deserialize, Clone)]
pub struct MemoryDeclareConfig {
    /// 宣言ラン全体の on/off。既定 false。
    #[serde(default = "default_md_enabled")]
    pub enabled: bool,
    /// 1 回で提示する未宣言の生ログ件数の枠（有界）。初期 100（#313 の実測）。
    #[serde(default = "default_md_max_logs")]
    pub max_logs: i64,
    /// 発火の下限。マーカーより新しい未宣言ログがこの件数以上溜まったら発火。初期 100。
    #[serde(default = "default_md_min_new_logs")]
    pub min_new_logs: i64,
    /// 日次ゲート。前回実行からこの**分数**以上経っていないと発火しない。既定 1440（= 24 時間）。
    /// 分単位なのはバックログ消化（#390）で一時的に間隔を詰めるため。定常運用では既定のまま使う。
    #[serde(default = "default_md_min_interval_minutes")]
    pub min_interval_minutes: i64,
    /// 宣言ラン 1 回のタイムアウト（秒）。超えたら partial 扱いでマーカーを前進させない。
    #[serde(default = "default_md_timeout_secs")]
    pub timeout_secs: u64,
}

impl Default for MemoryDeclareConfig {
    fn default() -> Self {
        Self {
            enabled: default_md_enabled(),
            max_logs: default_md_max_logs(),
            min_new_logs: default_md_min_new_logs(),
            min_interval_minutes: default_md_min_interval_minutes(),
            timeout_secs: default_md_timeout_secs(),
        }
    }
}

fn default_md_enabled() -> bool {
    false
}
fn default_md_max_logs() -> i64 {
    100
}
fn default_md_min_new_logs() -> i64 {
    100
}
fn default_md_min_interval_minutes() -> i64 {
    1440
}
fn default_md_timeout_secs() -> u64 {
    600
}

/// スリープ凝縮ラン（#411 / 記憶の 3 段目）の設定。
///
/// 凝縮ランは、本人が別セッションの新規 context で**自分のユニット（宣言した記憶）を時系列で
/// 少しずつ俯瞰**し、「その出来事たちが何を意味するか」という原則を `node_type='meta'` として
/// 刻む。宣言ランの双子で、入力が生ログではなくユニットである点だけが本質的に違う。
///
/// **逐次凝縮**（オーナー指摘 2026-08-08「いきなりまとまった期間を与えると平均に寄る」）:
/// 全ユニットを一括で渡さず、カーソルより新しいユニットを**時系列順に [`min_new_units`] 件ずつ**
/// の窓で読む。毎回「既存 core 全件＋今回の窓」を渡し、更新優先で core を育てる。新規エージェントが
/// 1 回で見る量と、既存エージェントの積み残し消化の 1 窓が同じ幅になる（＝新規と同じ形で消化する）。
///
/// **既定オフ**。発火の仕方（[`decide_condense`] 参照）:
/// - 残ユニット（カーソルより新しい未凝縮）が窓幅 [`min_new_units`] 以上 → **積み残し消化**として
///   throttle を待たず 1 tick 1 窓で発火（新規と同じく淡々と消化する / オーナー指摘の趣旨）。ただし
///   partial が続いたときは指数バックオフで間引く（上限は [`min_interval_minutes`]）。
/// - 0 < 残 < 窓幅 → 末尾の端数。**[`min_interval_minutes`] を待って**から流す（新しいユニットの
///   増加を待つのはここだけ）。残 0 ならゼロコールで return。
///
/// [`decide_condense`]: crate::memory_condense
#[derive(Debug, Deserialize, Clone)]
pub struct MemoryCondenseConfig {
    /// 凝縮ラン全体の on/off。既定 false。
    #[serde(default = "default_mc_enabled")]
    pub enabled: bool,
    /// **窓幅 N かつ積み残し発火の下限**（逐次凝縮）。1 回の凝縮ランが時系列順に読むユニット件数。
    /// 残ユニットがこの件数以上あれば throttle を待たず消化し、これ未満の端数は min_interval を
    /// 待って流す。初期 20（仮）。ユニット粒度が実測で概ね 3 日 = 1 ユニットなので約 2 か月ぶん。
    /// **PR-3 で実験の実測後に確定する。**
    #[serde(default = "default_mc_min_new_units")]
    pub min_new_units: i64,
    /// 端数（残 < 窓幅）を流すときだけ効く throttle。前回実行からこの**分数**以上経っていないと
    /// 端数は流さない（新しいユニットの増加を待つ）。**積み残し消化中（残 >= 窓幅）はこの値を
    /// 待たず 1 tick 1 窓で淡々と進む。** ただし partial（timeout / ターン上限 / エラー）が続いた
    /// ときの指数バックオフの**上限**としてもこの値を使う（バックオフが端数待ちより長くならない）。
    /// 初期 10080（= 7 日）。**PR-3 で確定する（仮）。**
    #[serde(default = "default_mc_min_interval_minutes")]
    pub min_interval_minutes: i64,
    /// 凝縮ラン 1 回のタイムアウト（秒）。超えたら partial 扱いで位置マーカーを進めない。
    #[serde(default = "default_mc_timeout_secs")]
    pub timeout_secs: u64,
}

impl Default for MemoryCondenseConfig {
    fn default() -> Self {
        Self {
            enabled: default_mc_enabled(),
            min_new_units: default_mc_min_new_units(),
            min_interval_minutes: default_mc_min_interval_minutes(),
            timeout_secs: default_mc_timeout_secs(),
        }
    }
}

fn default_mc_enabled() -> bool {
    false
}
fn default_mc_min_new_units() -> i64 {
    20
}
fn default_mc_min_interval_minutes() -> i64 {
    10080
}
fn default_mc_timeout_secs() -> u64 {
    600
}

/// evaluator（契約に対する独立 rubric 評価）の設定。
///
/// **#291 で対話ターンからの呼び出しは撤去した**。毎ターンの採点結果が
/// `session_logs` へ `evaluation` として積まれ、指示文つきで会話に割り込み、
/// 直前のユーザー発言より採点の圧が勝つ事故が起きたため。評価そのものの設計
/// （自己採点させない・別 context で rubric 評価する）は正しいので、呼ぶ場所を
/// スリープ中（非対話時）へ移す — その配線は別 issue で行う。
///
/// そのため現在この設定はどこからも読まれない。既存の TOML を壊さないようキーは
/// 残してあり、スリープ側の配線でそのまま使う想定。
#[derive(Debug, Deserialize, Clone)]
pub struct EvaluatorConfig {
    /// 評価を有効にするか。
    #[serde(default = "default_evaluator_enabled")]
    pub enabled: bool,
    /// 合格スコア閾値 (0.0-1.0)。
    #[serde(default = "default_evaluator_threshold")]
    pub threshold: f64,
    /// 評価に使うモデル（省略時はそのエージェントの実効モデル）。
    #[serde(default)]
    pub model: Option<String>,
}

impl Default for EvaluatorConfig {
    fn default() -> Self {
        Self {
            enabled: default_evaluator_enabled(),
            threshold: default_evaluator_threshold(),
            model: None,
        }
    }
}

fn default_evaluator_enabled() -> bool {
    true
}
fn default_evaluator_threshold() -> f64 {
    0.7
}

#[derive(Debug, Deserialize)]
pub struct AgentConfig {
    #[serde(default = "default_workspace_path")]
    pub workspace_path: String,
    #[serde(default = "default_heartbeat_interval")]
    pub heartbeat_interval_secs: u64,
    #[serde(default = "default_heartbeat_enabled")]
    pub heartbeat_enabled: bool,
    /// エージェントが自分で設定できるハートビート間隔の**下限**（秒 / #247）。
    ///
    /// エージェント自身が間隔を変えられるということは、極端に短い値を要求できると
    /// いうこと。1 秒にされると費用も負荷も跳ねるので、運用者がここで床を決める。
    /// 書き込み口（`set_my_heartbeat`）は下限より短い要求を**拒否**する。
    ///
    /// 既定 300 秒（5 分）。tick は「LLM の 1 ターン + ツール実行」で、体感で数十秒〜
    /// 分単位かかる。5 分を下回ると前の tick が終わる前に次が来る領域に入り、間隔を
    /// 縮めた分だけ費用が増えるだけで自律性は上がらない。もっと速く / 遅くしたい
    /// 運用者はこの値を動かせばよい（0 を書いても最低 1 秒は残る）。
    #[serde(default = "default_heartbeat_min_interval")]
    pub heartbeat_min_interval_secs: u64,
    #[serde(default = "default_max_workspace_size")]
    pub max_workspace_size_mb: u64,
    /// ループ再起動 v1（#52）: depth 0 の run が反復上限で停止し、セッションに
    /// active タスクが残っている場合に、1回だけクリーンな context で自動再実行する。
    /// セッションロックを run1+verify+run2 の間保持し続けるため、既定は無効。
    #[serde(default)]
    pub loop_restart_enabled: bool,
    /// メモリインデックスのアイドル時メンテナンス（増分ビルドの取りこぼし回収 /
    /// キーワードバックフィル / 月次ロールアップ）。既定 true — 増分ビルドの費用は
    /// post-run トリガーで既に受容済みで、純増は一時的なバックフィルと月1回程度の
    /// ロールアップのみ。
    #[serde(default = "default_memory_maintenance_enabled")]
    pub memory_maintenance_enabled: bool,
    /// メンテナンス tick の間隔（秒）。無処理 tick は SQL 数本で終わる。
    #[serde(default = "default_memory_maintenance_interval")]
    pub memory_maintenance_interval_secs: u64,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            workspace_path: default_workspace_path(),
            heartbeat_interval_secs: default_heartbeat_interval(),
            heartbeat_enabled: default_heartbeat_enabled(),
            heartbeat_min_interval_secs: default_heartbeat_min_interval(),
            max_workspace_size_mb: default_max_workspace_size(),
            loop_restart_enabled: false,
            memory_maintenance_enabled: default_memory_maintenance_enabled(),
            memory_maintenance_interval_secs: default_memory_maintenance_interval(),
        }
    }
}

fn default_memory_maintenance_enabled() -> bool {
    true
}
fn default_memory_maintenance_interval() -> u64 {
    600
}

fn default_workspace_path() -> String {
    "data/agents/{agent_id}/workspace".to_string()
}
fn default_heartbeat_interval() -> u64 {
    29
}
fn default_max_workspace_size() -> u64 {
    100
}
fn default_heartbeat_enabled() -> bool {
    false
}
fn default_heartbeat_min_interval() -> u64 {
    300
}

/// エージェント単位ハートビート設定の境界値（#247）。
///
/// `AppState` に 1 つ持たせて、ツール（`get_my_heartbeat` / `set_my_heartbeat`）と
/// 解決（`opencrab_db::queries::resolve_agent_heartbeat`）が同じ値を見るようにする。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeartbeatLimits {
    /// エージェントが間隔を指定しなかったときに使う既定（`[agent] heartbeat_interval_secs`）。
    pub default_interval_secs: u64,
    /// 下限（`[agent] heartbeat_min_interval_secs`）。
    pub min_interval_secs: u64,
}

impl HeartbeatLimits {
    /// 間隔の**上限**（秒）。24 時間。
    ///
    /// 上限は費用のためではない（長いほど安い）。「有効なのに実質発火しない」状態を
    /// 作らせないためのもので、下限の理由と同じ**思い込みの防止**。u64 の上限値を
    /// 受け付けると、エージェントは「ハートビートを有効にした」と思ったまま二度と
    /// 発火しない。1 日に 1 回より疎な自律実行が要るなら、それはハートビートでは
    /// なく運用者側のスケジューリングの仕事。
    pub const MAX_INTERVAL_SECS: u64 = 86_400;

    /// 実効下限。運用者が 0 を書いてもビジーループにはしない（最低 1 秒）。
    pub fn effective_min(&self) -> u64 {
        self.min_interval_secs.max(1)
    }
}

impl Default for HeartbeatLimits {
    fn default() -> Self {
        Self {
            default_interval_secs: default_heartbeat_interval(),
            min_interval_secs: default_heartbeat_min_interval(),
        }
    }
}

impl AgentConfig {
    /// 設定ファイルの値から境界値を取り出す。
    pub fn heartbeat_limits(&self) -> HeartbeatLimits {
        HeartbeatLimits {
            default_interval_secs: self.heartbeat_interval_secs,
            min_interval_secs: self.heartbeat_min_interval_secs,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct LlmConfig {
    #[serde(default = "default_provider")]
    pub default_provider: String,
    #[serde(default = "default_model")]
    pub default_model: String,
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    #[serde(default)]
    pub fallback: FallbackConfig,
    #[serde(default)]
    pub aliases: HashMap<String, AliasConfig>,
    /// 会話コンパクション比率: context_window のうち会話履歴に使う割合 (0.0-1.0)。
    #[serde(default = "default_compaction_ratio")]
    pub compaction_ratio: f64,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            default_provider: "openai".to_string(),
            default_model: "gpt-4o".to_string(),
            providers: HashMap::new(),
            fallback: FallbackConfig::default(),
            aliases: HashMap::new(),
            compaction_ratio: default_compaction_ratio(),
        }
    }
}

fn default_compaction_ratio() -> f64 {
    0.5
}

fn default_provider() -> String {
    "openai".to_string()
}
fn default_model() -> String {
    "gpt-4o".to_string()
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct ProviderConfig {
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub organization: String,
    #[serde(default)]
    pub app_name: String,
    #[serde(default)]
    pub site_url: String,
    #[serde(default)]
    pub default_model: String,
    #[serde(default)]
    pub binary_path: String,
    /// 起動引数（ACP 等、コマンド + フラグでプロバイダを起こすもの向け）。
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub sandbox: String,
    #[serde(default)]
    pub working_dir: String,
    #[serde(default = "default_codex_timeout")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub auth_file: String,
    #[serde(default)]
    pub reasoning_effort: String,
    #[serde(default)]
    pub include_reasoning_encrypted_content: bool,
}

fn default_codex_timeout() -> u64 {
    300
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct FallbackConfig {
    #[serde(default)]
    pub chain: Vec<String>,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct AliasConfig {
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct GatewayConfig {
    #[serde(default)]
    pub rest: RestGatewayConfig,
    #[serde(default)]
    pub discord: DiscordGatewayConfig,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct DiscordGatewayConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub guild_ids: Vec<u64>,
    /// Discordメッセージに応答するエージェントのIDリスト
    #[serde(default)]
    pub agent_ids: Vec<String>,
    /// DMに応答するオーナーのDiscord User ID（設定時、このID以外からのDMは無視）
    #[serde(default)]
    pub owner_discord_id: String,
    /// spawn_subtask.webhook が省略された時に使うデフォルトの lifecycle webhook。
    ///
    /// **旧キー（後方互換）**: #157 S5 で transport 非依存の `[subtask] default_webhook`
    /// へ持ち上げた。既存の設定ファイルを壊さないためここは残し、新キーが未設定の
    /// ときのフォールバックとして読み続ける（[`AppConfig::default_subtask_webhook`]）。
    /// 新規に書くなら `[subtask] default_webhook` を使うこと。
    #[serde(default)]
    pub default_subtask_webhook: Option<SubtaskWebhookConfig>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SubtaskWebhookConfig {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub events: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct RestGatewayConfig {
    #[serde(default = "default_port")]
    pub port: u16,
}

impl Default for RestGatewayConfig {
    fn default() -> Self {
        Self { port: 8080 }
    }
}

fn default_port() -> u16 {
    8080
}

#[derive(Debug, Deserialize)]
pub struct DatabaseConfig {
    #[serde(default = "default_db_path")]
    pub path: String,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            path: default_db_path(),
        }
    }
}

fn default_db_path() -> String {
    "data/opencrab.db".to_string()
}

// ---------- Config loading ----------

/// Load config from a TOML file, expanding `${VAR}` placeholders with env vars.
pub fn load_config(path: &str) -> Result<AppConfig> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read config file: {}", path))?;

    let expanded = expand_env_vars(&raw);

    let mut config: AppConfig =
        toml::from_str(&expanded).with_context(|| "Failed to parse config TOML")?;

    // owner は入口で正規化する。`.env` からのコピペで前後に空白が混ざると、
    // `api::is_owner_id`（trim 済み比較）を通る経路では owner と認識されるのに、
    // 生比較が残っている下位 crate（form/modal、ボタン操作）だけ無言で拒否される。
    // 判定述語を下位 crate へ移す整理は #174。
    let owner = config.gateway.discord.owner_discord_id.trim();
    if owner.len() != config.gateway.discord.owner_discord_id.len() {
        config.gateway.discord.owner_discord_id = owner.to_string();
    }

    // dispatch の kill switch は環境変数で上書きできる（`.env` だけで切り戻せるように）。
    if let Some(v) = auto_dispatch_from_env() {
        config.subtask.auto_dispatch = v;
    }

    Ok(config)
}

/// Replace `${VAR_NAME}` patterns with corresponding environment variable values.
/// Unknown variables are replaced with empty strings.
pub(crate) fn expand_env_vars(input: &str) -> String {
    let mut result = input.to_string();
    // Find all ${...} patterns and replace them
    loop {
        let start = match result.find("${") {
            Some(pos) => pos,
            None => break,
        };
        let end = match result[start..].find('}') {
            Some(pos) => start + pos,
            None => break,
        };
        let var_name = &result[start + 2..end];
        let value = std::env::var(var_name).unwrap_or_default();
        result = format!("{}{}{}", &result[..start], value, &result[end + 1..]);
    }
    result
}

// ---------- Provider overrides (dashboard-managed) ----------

/// TOML の LlmConfig に DB のプロバイダーオーバーライドを適用した実効設定を返す。
///
/// マージ規則:
/// - `enabled == Some(false)`: プロバイダーを実効設定から**除外**する
///   （TOML にキーがあっても登録されない）。
/// - `enabled == Some(true)` で TOML に無いプロバイダー: 空の ProviderConfig を
///   作ってオーバーライドを適用する（ollama 等のローカル系を UI から有効化する経路）。
/// - `api_key` / `base_url` / `default_model` は Some のフィールドだけ上書き。
///   Some("") は「TOML 値の消去」として扱う。
pub fn apply_llm_overrides(
    base: &LlmConfig,
    overrides: &[opencrab_db::queries::LlmProviderOverrideRow],
) -> LlmConfig {
    let mut cfg = base.clone();
    for row in overrides {
        if row.enabled == Some(false) {
            cfg.providers.remove(&row.provider);
            continue;
        }
        let entry = cfg.providers.entry(row.provider.clone()).or_default();
        if let Some(key) = &row.api_key {
            entry.api_key = key.clone();
        }
        if let Some(url) = &row.base_url {
            entry.base_url = url.clone();
        }
        if let Some(model) = &row.default_model {
            entry.default_model = model.clone();
        }
        if let Some(effort) = &row.reasoning_effort {
            entry.reasoning_effort = effort.clone();
        }
        if let Some(bp) = &row.binary_path {
            entry.binary_path = bp.clone();
        }
        if let Some(args_json) = &row.args_json {
            // JSON 配列としてパース。壊れていれば既存を保つ。
            if let Ok(args) = serde_json::from_str::<Vec<String>>(args_json) {
                entry.args = args;
            }
        }
        if let Some(wd) = &row.working_dir {
            entry.working_dir = wd.clone();
        }
        if let Some(t) = row.timeout_secs {
            if t > 0 {
                entry.timeout_secs = t as u64;
            }
        }
    }
    cfg
}

// ---------- LLM Router builder ----------

/// Build an LlmRouter from the LLM config section.
/// Only providers with non-empty API keys (or local providers) are registered.
pub fn build_llm_router(config: &LlmConfig) -> Result<LlmRouter> {
    let mut router = LlmRouter::new();

    for (name, pconfig) in &config.providers {
        let provider: Option<Arc<dyn LlmProvider>> = match name.as_str() {
            "openai" => {
                if pconfig.api_key.is_empty() {
                    None
                } else {
                    let mut p = OpenAiProvider::new(&pconfig.api_key);
                    if !pconfig.base_url.is_empty() {
                        p = p.with_base_url(&pconfig.base_url);
                    }
                    if !pconfig.organization.is_empty() {
                        p = p.with_org_id(&pconfig.organization);
                    }
                    // GPT-5 系 / o シリーズを使うときの reasoning_effort（任意）。
                    if !pconfig.reasoning_effort.is_empty() {
                        p = p.with_reasoning_effort(&pconfig.reasoning_effort);
                    }
                    Some(Arc::new(p))
                }
            }
            "anthropic" => {
                if pconfig.api_key.is_empty() {
                    None
                } else {
                    let mut p = AnthropicProvider::new(&pconfig.api_key);
                    if !pconfig.base_url.is_empty() {
                        p = p.with_base_url(&pconfig.base_url);
                    }
                    Some(Arc::new(p))
                }
            }
            "google" => {
                if pconfig.api_key.is_empty() {
                    None
                } else {
                    let mut p = GoogleProvider::new(&pconfig.api_key);
                    if !pconfig.base_url.is_empty() {
                        p = p.with_base_url(&pconfig.base_url);
                    }
                    Some(Arc::new(p))
                }
            }
            "openrouter" => {
                if pconfig.api_key.is_empty() {
                    None
                } else {
                    let mut p = OpenRouterProvider::new(&pconfig.api_key);
                    if !pconfig.base_url.is_empty() {
                        p = p.with_base_url(&pconfig.base_url);
                    }
                    if !pconfig.app_name.is_empty() {
                        p = p.with_title(&pconfig.app_name);
                    }
                    if !pconfig.site_url.is_empty() {
                        p = p.with_referer(&pconfig.site_url);
                    }
                    Some(Arc::new(p))
                }
            }
            "ollama" => {
                let mut p = OllamaProvider::new();
                if !pconfig.base_url.is_empty() {
                    p = p.with_base_url(&pconfig.base_url);
                }
                Some(Arc::new(p))
            }
            "llamacpp" => {
                let mut p = LlamaCppProvider::new();
                if !pconfig.base_url.is_empty() {
                    p = p.with_base_url(&pconfig.base_url);
                }
                Some(Arc::new(p))
            }
            "codex" => {
                let mut p = opencrab_llm::CodexProvider::new();
                if !pconfig.default_model.is_empty() {
                    p = p.with_default_model(&pconfig.default_model);
                }
                if !pconfig.binary_path.is_empty() {
                    p = p.with_codex_path(&pconfig.binary_path);
                }
                if !pconfig.sandbox.is_empty() {
                    p = p.with_sandbox(&pconfig.sandbox);
                }
                if !pconfig.working_dir.is_empty() {
                    p = p.with_working_dir(&pconfig.working_dir);
                }
                if pconfig.timeout_secs > 0 {
                    p = p.with_timeout_secs(pconfig.timeout_secs);
                }
                // reasoning effort の上書き（gpt-5.6 系の既定 high を下げる等）。
                if !pconfig.reasoning_effort.is_empty() {
                    p = p.with_reasoning_effort(&pconfig.reasoning_effort);
                }
                if !pconfig.models.is_empty() {
                    let extra: Vec<(String, u32)> = pconfig
                        .models
                        .iter()
                        .map(|m| (m.clone(), 200_000u32))
                        .collect();
                    p = p.with_extra_models(extra);
                }
                Some(Arc::new(p))
            }
            "cursor" => {
                let mut p = opencrab_llm::CursorProvider::new();
                if !pconfig.default_model.is_empty() {
                    p = p.with_default_model(&pconfig.default_model);
                }
                if !pconfig.binary_path.is_empty() {
                    p = p.with_binary_path(&pconfig.binary_path);
                }
                if !pconfig.working_dir.is_empty() {
                    p = p.with_working_dir(&pconfig.working_dir);
                }
                if pconfig.timeout_secs > 0 {
                    p = p.with_timeout_secs(pconfig.timeout_secs);
                }
                // config に api_key があれば CURSOR_API_KEY として渡す。
                // 無ければ `cursor-agent login` 済みのアンビエント認証に任せる。
                if !pconfig.api_key.is_empty() {
                    p = p.with_api_key(&pconfig.api_key);
                }
                if !pconfig.models.is_empty() {
                    let extra: Vec<(String, u32)> = pconfig
                        .models
                        .iter()
                        .map(|m| (m.clone(), 200_000u32))
                        .collect();
                    p = p.with_extra_models(extra);
                }
                Some(Arc::new(p))
            }
            "acp" => {
                // ACP（Agent Client Protocol）エージェントを JSON-RPC/stdio で駆動する。
                // 起動コマンド/引数はエージェント毎に異なるため binary_path + args で指定。
                let mut p = opencrab_llm::AcpProvider::new();
                if !pconfig.default_model.is_empty() {
                    p = p.with_default_model(&pconfig.default_model);
                }
                if !pconfig.binary_path.is_empty() {
                    p = p.with_binary_path(&pconfig.binary_path);
                }
                if !pconfig.args.is_empty() {
                    p = p.with_args(pconfig.args.clone());
                }
                if !pconfig.working_dir.is_empty() {
                    p = p.with_working_dir(&pconfig.working_dir);
                }
                if pconfig.timeout_secs > 0 {
                    p = p.with_timeout_secs(pconfig.timeout_secs);
                }
                if !pconfig.models.is_empty() {
                    let extra: Vec<(String, u32)> = pconfig
                        .models
                        .iter()
                        .map(|m| (m.clone(), 200_000u32))
                        .collect();
                    p = p.with_extra_models(extra);
                }
                Some(Arc::new(p))
            }
            "chatgpt" => {
                let mut p = ChatGptProvider::new();
                if !pconfig.auth_file.is_empty() {
                    p = p.with_auth_file(&pconfig.auth_file);
                }
                if !pconfig.base_url.is_empty() {
                    p = p.with_base_url(&pconfig.base_url);
                }
                if !pconfig.default_model.is_empty() {
                    p = p.with_default_model(&pconfig.default_model);
                }
                if !pconfig.reasoning_effort.is_empty() {
                    p = p.with_reasoning_effort(&pconfig.reasoning_effort);
                }
                // 長考ターン（reasoning_effort の高い体）が既定 60 秒の read timeout を
                // 超えて error → リトライを繰り返さないよう、config から伸ばせる（#433）。
                if pconfig.timeout_secs > 0 {
                    p = p.with_timeout_secs(pconfig.timeout_secs);
                }
                p = p.with_include_encrypted_content(pconfig.include_reasoning_encrypted_content);
                Some(Arc::new(p))
            }
            "bonsai" => {
                let mut p = LlamaCppProvider::new().with_name("bonsai");
                if !pconfig.base_url.is_empty() {
                    p = p.with_base_url(&pconfig.base_url);
                }
                Some(Arc::new(p))
            }
            other => {
                info!(provider = %other, "Unknown provider in config, skipping");
                None
            }
        };

        if let Some(p) = provider {
            router.add_provider(p);
        }
    }

    // Set default provider
    router.set_default_provider(&config.default_provider);

    // Set fallback chain (only include registered providers)
    let registered: Vec<String> = router
        .provider_names()
        .iter()
        .map(|s| s.to_string())
        .collect();
    let chain: Vec<String> = config
        .fallback
        .chain
        .iter()
        .filter(|name| registered.contains(name))
        .cloned()
        .collect();
    if !chain.is_empty() {
        router.set_fallback_chain(chain);
    }

    // Set model aliases
    for (alias, acfg) in &config.aliases {
        let target = format!("{}:{}", acfg.provider, acfg.model);
        router.add_model_mapping(alias, target);
    }

    info!(
        providers = ?router.provider_names(),
        "LLM router configured"
    );

    Ok(router)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 環境変数を触るテストを直列化するロック。
    ///
    /// 環境変数はプロセス全体で共有されるので、`cargo test` の並列実行下では
    /// あるテストの `set_var`/`remove_var` が別テストの読み取りに割り込む。
    /// 必要なのは「同一プロセス内での直列化」だけなので、`serial_test` を
    /// 依存に追加せず標準ライブラリの `Mutex` で済ませている。
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// `ENV_LOCK` を取得する。テストが panic してロックが poison されても、
    /// 後続テストが道連れで落ちないように中身を取り出して続行する。
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 環境変数をテスト中だけ差し替え、`Drop` で元の状態（未設定なら未設定）に
    /// 戻す RAII ガード。assert 失敗で panic しても復元されるため、値が後続
    /// テストや開発者のシェル由来の設定に漏れない。
    struct EnvVarGuard {
        key: String,
        previous: Option<String>,
    }

    impl EnvVarGuard {
        fn set(key: &str, value: &str) -> Self {
            let previous = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self {
                key: key.to_string(),
                previous,
            }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(v) => std::env::set_var(&self.key, v),
                None => std::env::remove_var(&self.key),
            }
        }
    }

    #[test]
    fn test_expand_env_vars() {
        let _lock = env_lock();
        let _guard = EnvVarGuard::set("TEST_EXPAND_KEY", "hello123");
        let input = "api_key = \"${TEST_EXPAND_KEY}\"";
        let result = expand_env_vars(input);
        assert_eq!(result, "api_key = \"hello123\"");
    }

    #[test]
    fn test_expand_env_vars_missing() {
        let _lock = env_lock();
        let input = "api_key = \"${NONEXISTENT_VAR_12345}\"";
        let result = expand_env_vars(input);
        assert_eq!(result, "api_key = \"\"");
    }

    #[test]
    fn test_expand_env_vars_multiple() {
        let _lock = env_lock();
        let _a = EnvVarGuard::set("TEST_A", "aaa");
        let _b = EnvVarGuard::set("TEST_B", "bbb");
        let input = "${TEST_A} and ${TEST_B}";
        let result = expand_env_vars(input);
        assert_eq!(result, "aaa and bbb");
    }

    /// `owner_discord_id` は環境変数参照で与える（ローカル固有値を `.env` に寄せる）。
    #[test]
    fn owner_discord_id_expands_from_env() {
        let _lock = env_lock();
        let _guard = EnvVarGuard::set("TEST_OWNER_DISCORD_ID", "123456789012345678");
        let raw = "[gateway.discord]\nowner_discord_id = \"${TEST_OWNER_DISCORD_ID}\"\n";
        let cfg: AppConfig = toml::from_str(&expand_env_vars(raw)).unwrap();
        assert_eq!(cfg.gateway.discord.owner_discord_id, "123456789012345678");
        assert!(crate::api::is_owner_id(
            &cfg.gateway.discord.owner_discord_id,
            "123456789012345678"
        ));
    }

    /// 環境変数が未設定なら空文字に展開される。空のオーナー ID は誰とも一致させない
    /// （= オーナー無し扱い）ので、空の caller が owner に昇格しない。
    #[test]
    fn unset_owner_discord_id_grants_owner_to_nobody() {
        let _lock = env_lock();
        let raw = "[gateway.discord]\nowner_discord_id = \"${UNSET_OWNER_DISCORD_ID_FOR_TEST}\"\n";
        let cfg: AppConfig = toml::from_str(&expand_env_vars(raw)).unwrap();
        let owner = &cfg.gateway.discord.owner_discord_id;
        assert!(owner.is_empty());
        assert!(!crate::api::is_owner_id(owner, ""));
        assert!(!crate::api::is_owner_id(owner, "123456789012345678"));
    }

    /// `load_config`（ファイル読み込み → `${}` 展開 → TOML パース）を通しても
    /// 環境変数の値が `owner_discord_id` に入る。
    #[test]
    fn load_config_expands_owner_discord_id_from_env() {
        let _lock = env_lock();
        let _guard = EnvVarGuard::set("TEST_LOAD_OWNER_DISCORD_ID", "123456789012345678");
        // 一時ディレクトリはテストごとにユニークで、`TempDir` の Drop で削除される
        // （固定パスの残骸を作らない / 並列実行でも衝突しない）。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("owner.toml");
        std::fs::write(
            &path,
            "[gateway.discord]\nenabled = true\nowner_discord_id = \"${TEST_LOAD_OWNER_DISCORD_ID}\"\n",
        )
        .unwrap();
        let cfg = load_config(path.to_str().unwrap()).unwrap();
        assert_eq!(cfg.gateway.discord.owner_discord_id, "123456789012345678");
    }

    /// リポジトリに追跡されている設定ファイルは、owner を実 ID の直書きではなく
    /// `${OWNER_DISCORD_ID}` 参照で持つ。
    ///
    /// 期待値は実測と独立なセンチネル定数に固定する（環境変数から期待値を導出すると
    /// 参照が壊れていても両辺が同じ値になりトートロジーになる）。これにより
    /// 「変数名の typo」「実 ID の直書きへの逆戻り」「参照ごと消える」のいずれでも落ちる。
    ///
    /// 本番 (`crates/server/src/main.rs`) と CLI がロードするのは `config/default.toml`
    /// なので、配布テンプレートだけでなく両方を回す。
    ///
    /// **注意**: `config/default.toml` は追跡ファイル（＝各開発者の実稼働設定）なので、
    /// このテストは作業コピーの中身も検査する。ローカルで owner を実 ID に直書きすると
    /// 無関係な変更でも `cargo test` が落ちる。これは意図した挙動で、作業コピーでも
    /// `${OWNER_DISCORD_ID}` 参照を維持すること（ローカル固有値は `.env` に置く）。
    #[test]
    fn shipped_configs_take_owner_discord_id_from_env() {
        let _lock = env_lock();
        // `${}` を含まない値にする（expand_env_vars は展開結果も再走査するため）。
        const SENTINEL: &str = "sentinel-owner-000";
        let _guard = EnvVarGuard::set("OWNER_DISCORD_ID", SENTINEL);

        let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for name in ["config/default.toml.example", "config/default.toml"] {
            let path = repo_root.join(name);
            let cfg = load_config(path.to_str().unwrap()).unwrap_or_else(|e| {
                panic!(
                    "{name} is not valid TOML (check your working copy of this tracked file): {e:#}"
                )
            });
            assert_eq!(
                cfg.gateway.discord.owner_discord_id, SENTINEL,
                "{name}: owner_discord_id must resolve from ${{OWNER_DISCORD_ID}} \
                 (a literal ID written into this tracked file, or a typo in the variable name, \
                 breaks this). Keep the ${{OWNER_DISCORD_ID}} reference and put your own ID in .env"
            );
        }
    }

    /// 配布テンプレートは共有ゲートウェイを既定で無効にしている（個人 ID や
    /// トークンを持たない状態で配られる）。
    ///
    /// `load_config` は `${}` 展開で環境変数を読むので、`set_var` する他テストと
    /// 直列化するため `env_lock()` を取る。
    #[test]
    fn shipped_config_example_keeps_shared_gateway_disabled() {
        let _lock = env_lock();
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../config/default.toml.example");
        let cfg = load_config(path.to_str().unwrap()).expect("default.toml.example must parse");
        assert!(!cfg.gateway.discord.enabled);
    }

    /// `load_config` は owner を trim して返す（`.env` のコピペで前後に空白が
    /// 混ざっても、生比較が残る下位経路と判定がズレない）。
    #[test]
    fn load_config_trims_owner_discord_id() {
        let _lock = env_lock();
        let _guard = EnvVarGuard::set("TEST_TRIM_OWNER_DISCORD_ID", "  123456789012345678\t");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("owner-trim.toml");
        std::fs::write(
            &path,
            "[gateway.discord]\nowner_discord_id = \"${TEST_TRIM_OWNER_DISCORD_ID}\"\n",
        )
        .unwrap();
        let cfg = load_config(path.to_str().unwrap()).unwrap();
        assert_eq!(cfg.gateway.discord.owner_discord_id, "123456789012345678");
    }

    #[test]
    fn test_apply_llm_overrides() {
        use opencrab_db::queries::LlmProviderOverrideRow;
        let mut providers = HashMap::new();
        providers.insert(
            "openai".to_string(),
            ProviderConfig {
                api_key: "toml-key".to_string(),
                base_url: "https://toml.example".to_string(),
                ..Default::default()
            },
        );
        providers.insert(
            "anthropic".to_string(),
            ProviderConfig {
                api_key: "ant-key".to_string(),
                ..Default::default()
            },
        );
        let base = LlmConfig {
            providers,
            ..toml::from_str("").unwrap()
        };

        let overrides = vec![
            // openai: キーだけ DB 側で差し替え
            LlmProviderOverrideRow {
                provider: "openai".to_string(),
                api_key: Some("db-key".to_string()),
                ..Default::default()
            },
            // anthropic: 強制無効
            LlmProviderOverrideRow {
                provider: "anthropic".to_string(),
                enabled: Some(false),
                ..Default::default()
            },
            // ollama: TOML に無いが UI から有効化（base_url のみ）
            LlmProviderOverrideRow {
                provider: "ollama".to_string(),
                enabled: Some(true),
                base_url: Some("http://localhost:11434".to_string()),
                ..Default::default()
            },
        ];

        let merged = apply_llm_overrides(&base, &overrides);
        assert_eq!(merged.providers["openai"].api_key, "db-key");
        // 上書きしていないフィールドは TOML 値を維持
        assert_eq!(merged.providers["openai"].base_url, "https://toml.example");
        assert!(
            !merged.providers.contains_key("anthropic"),
            "disabled provider must be removed"
        );
        assert_eq!(
            merged.providers["ollama"].base_url,
            "http://localhost:11434"
        );
    }

    #[test]
    fn test_apply_llm_overrides_empty_is_identity() {
        let base: LlmConfig = toml::from_str("").unwrap();
        let merged = apply_llm_overrides(&base, &[]);
        assert_eq!(merged.providers.len(), base.providers.len());
        assert_eq!(merged.default_provider, base.default_provider);
    }

    #[test]
    fn test_voice_config_parses() {
        let toml_str = r#"
[voice]
enabled = true

[voice.stt]
provider = "openai"
language = "ja"

[voice.tts]
provider = "voicevox"
default_voice = "3"

[voice.tts.agent_voices]
crab = "3"
rabomi = "1"
"#;
        let cfg: AppConfig = toml::from_str(toml_str).expect("voice config must parse");
        assert!(cfg.voice.enabled);
        assert_eq!(cfg.voice.stt.provider, "openai");
        assert_eq!(cfg.voice.stt.language.as_deref(), Some("ja"));
        assert_eq!(cfg.voice.tts.voice_for_agent("crab"), "3");
        assert_eq!(cfg.voice.tts.voice_for_agent("rabomi"), "1");
        assert_eq!(cfg.voice.tts.voice_for_agent("unknown"), "3");
    }

    #[test]
    fn test_voice_disabled_by_default() {
        let cfg: AppConfig = toml::from_str("").expect("empty config must parse");
        assert!(!cfg.voice.enabled);
    }

    #[test]
    fn test_default_config() {
        let toml_str = "";
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.database.path, "data/opencrab.db");
        assert_eq!(config.gateway.rest.port, 8080);
        assert_eq!(config.llm.default_provider, "openai");
    }

    /// Regression guard for #149: shipping `config/default.toml` must keep the
    /// codex sandbox at `read-only` so the codex CLI cannot write to the
    /// workspace / run arbitrary builds. If someone flips it back to
    /// `danger-full-access` this test fails.
    #[test]
    fn test_default_toml_codex_sandbox_is_read_only() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/default.toml");
        let config = load_config(path).expect("shipped default.toml must load");
        let codex = config
            .llm
            .providers
            .get("codex")
            .expect("default.toml must define a codex provider");
        assert_eq!(
            codex.sandbox, "read-only",
            "codex sandbox in config/default.toml must stay read-only (regression #149)"
        );
    }

    // ---- #157 S5: 通知先フォールバックの持ち上げ ----

    /// **旧キーだけの設定ファイルがそのまま動く**（後方互換）。
    ///
    /// `[gateway.discord] default_subtask_webhook` は #157 S5 以前の唯一の書き方。
    /// これが読めなくなると既存の運用が黙って通知を失う。
    #[test]
    fn legacy_discord_webhook_key_is_still_honored() {
        let cfg: AppConfig = toml::from_str(
            r#"
[gateway.discord]
default_subtask_webhook = { url = "https://discord.com/api/webhooks/1/legacytok", events = ["started"] }
"#,
        )
        .unwrap();
        let resolved = cfg.default_subtask_webhook().expect("旧キーが読まれるべき");
        assert_eq!(resolved.url, "https://discord.com/api/webhooks/1/legacytok");
        assert_eq!(resolved.events, Some(vec!["started".to_string()]));
    }

    /// 新しい **transport 非依存キー** `[subtask] default_webhook` が読める。
    ///
    /// Discord の設定ブロックが 1 行も無い設定ファイルでも通知先が決まる
    /// （= Discord 機能フラグから独立した）ことがこの持ち上げの要点。
    #[test]
    fn transport_neutral_webhook_key_is_honored_without_any_discord_config() {
        let cfg: AppConfig = toml::from_str(
            r#"
[subtask]
default_webhook = { url = "https://discord.com/api/webhooks/2/neutraltok" }
"#,
        )
        .unwrap();
        let resolved = cfg.default_subtask_webhook().expect("新キーが読まれるべき");
        assert_eq!(
            resolved.url,
            "https://discord.com/api/webhooks/2/neutraltok"
        );
        assert_eq!(resolved.events, None);
    }

    /// 両方書いてあるときは新キーが勝つ（移行期の曖昧さを残さない）。
    #[test]
    fn transport_neutral_webhook_key_wins_over_the_legacy_one() {
        let cfg: AppConfig = toml::from_str(
            r#"
[subtask]
default_webhook = { url = "https://discord.com/api/webhooks/2/neutraltok" }

[gateway.discord]
default_subtask_webhook = { url = "https://discord.com/api/webhooks/1/legacytok" }
"#,
        )
        .unwrap();
        assert_eq!(
            cfg.default_subtask_webhook().unwrap().url,
            "https://discord.com/api/webhooks/2/neutraltok"
        );
    }

    /// どちらも無ければ未設定。url が空文字のときも未設定として扱う。
    #[test]
    fn absent_or_empty_webhook_url_resolves_to_none() {
        let empty: AppConfig = toml::from_str("").unwrap();
        assert!(empty.default_subtask_webhook().is_none());

        let blank: AppConfig = toml::from_str(
            r#"
[subtask]
default_webhook = { url = "" }
"#,
        )
        .unwrap();
        assert!(
            blank.default_subtask_webhook().is_none(),
            "url が空なら未設定扱い（`.env` 未設定で ${{VAR}} が空展開される運用）"
        );
    }

    // ---- #207: 新キーが空で旧キーの値を隠すときの警告 ----

    /// 判定の真理値表を網羅で固定する。
    ///
    /// 真になるのは「新キーの節がある × url が空」かつ「旧キーに有効な url がある」の
    /// 1 通りだけ。新キーの節が無ければ旧キーがそのまま使われるので隠していない。
    /// 新キーに url があればそれが使われる（意図どおりの優先）ので警告しない。
    #[test]
    fn masking_predicate_is_true_only_for_empty_new_key_over_a_valid_legacy_one() {
        let cfg = |url: &str| SubtaskWebhookConfig {
            url: url.to_string(),
            events: None,
        };
        let urls = ["", "   ", "https://example.test/hook"];
        for new_url in urls {
            for legacy_url in urls {
                let expected = new_url.trim().is_empty() && !legacy_url.trim().is_empty();
                assert_eq!(
                    legacy_webhook_masked_by_empty_new_key(
                        Some(&cfg(new_url)),
                        Some(&cfg(legacy_url))
                    ),
                    expected,
                    "new={new_url:?} legacy={legacy_url:?}"
                );
            }
            // 旧キーの節が無ければ隠すものが無い。
            assert!(!legacy_webhook_masked_by_empty_new_key(
                Some(&cfg(new_url)),
                None
            ));
            // 新キーの節が無ければ旧キーがそのまま使われる。
            assert!(!legacy_webhook_masked_by_empty_new_key(
                None,
                Some(&cfg(new_url))
            ));
        }
        assert!(!legacy_webhook_masked_by_empty_new_key(None, None));
    }

    /// 踏む経路そのままの設定ファイルで警告条件を満たし、**挙動は変わらない**。
    ///
    /// `${SUBTASK_WEBHOOK_URL}` が `.env` に無いと空文字へ展開されるので、新キーは
    /// `url = ""` と等価になる。
    #[test]
    fn empty_new_key_over_a_valid_legacy_key_warns_without_changing_resolution() {
        let cfg: AppConfig = toml::from_str(
            r#"
[subtask]
default_webhook = { url = "" }

[gateway.discord]
default_subtask_webhook = { url = "https://discord.com/api/webhooks/1/legacytok" }
"#,
        )
        .unwrap();
        assert!(
            cfg.legacy_webhook_masked_by_empty_new_key(),
            "新キーが空 + 旧キーに有効な値 → 警告条件を満たす"
        );
        assert!(cfg.warn_if_legacy_webhook_masked());
        assert!(
            cfg.default_subtask_webhook().is_none(),
            "警告を足しても解決順序は変えない（空 url は「無効」のまま）"
        );
    }

    /// 誤検知させない: 警告が「いつも出ている」ものになると誰も読まなくなる。
    #[test]
    fn no_masking_warning_for_the_ordinary_configurations() {
        // 何も書いていない（配布テンプレートの既定）。
        let empty: AppConfig = toml::from_str("").unwrap();
        assert!(!empty.warn_if_legacy_webhook_masked());

        // 旧キーだけ（移行前の既存運用）。
        let legacy_only: AppConfig = toml::from_str(
            r#"
[gateway.discord]
default_subtask_webhook = { url = "https://discord.com/api/webhooks/1/legacytok" }
"#,
        )
        .unwrap();
        assert!(!legacy_only.warn_if_legacy_webhook_masked());

        // 新キーだけ（移行後）。
        let new_only: AppConfig = toml::from_str(
            r#"
[subtask]
default_webhook = { url = "https://example.test/hook" }
"#,
        )
        .unwrap();
        assert!(!new_only.warn_if_legacy_webhook_masked());

        // 新キーを空にして意図的に無効化（旧キーも無いので隠していない）。
        let disabled: AppConfig = toml::from_str(
            r#"
[subtask]
default_webhook = { url = "" }
"#,
        )
        .unwrap();
        assert!(!disabled.warn_if_legacy_webhook_masked());
    }

    #[test]
    fn test_build_router_empty_keys() {
        let config = LlmConfig::default();
        let router = build_llm_router(&config).unwrap();
        assert!(router.provider_names().is_empty());
    }

    #[test]
    fn test_build_router_with_openrouter() {
        let mut providers = HashMap::new();
        providers.insert(
            "openrouter".to_string(),
            ProviderConfig {
                api_key: "sk-test-key".to_string(),
                app_name: "TestApp".to_string(),
                ..Default::default()
            },
        );
        let config = LlmConfig {
            providers,
            default_provider: "openrouter".to_string(),
            ..Default::default()
        };
        let router = build_llm_router(&config).unwrap();
        assert!(router.provider_names().contains(&"openrouter"));
    }
}
