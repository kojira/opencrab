
// ---------- Config structs (match config/default.toml) ----------

#[derive(Debug, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub agent: AgentConfig,
    #[serde(default)]
    pub llm: LlmConfig,
    /// #884 PR2: 構造化会話（typed history 送信）。既定 off。QC 環境で on。
    #[serde(default)]
    pub conversation: ConversationConfig,
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
    /// スリープ凝縮ラン（#411 / 記憶の 3 段目）。既定 ON（#457。`enabled=false` で opt-out 可）。
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
    /// 退避ファイル（workspace/tmp）の掃除（#711）。
    #[serde(default)]
    pub offload_cleanup: OffloadCleanupConfig,
    /// 外部イベント受信（webhook intake / issue #454）。既定は実質無効。
    #[serde(default)]
    pub intake: IntakeConfig,
    /// External gate V3 UDS listen path。空・欠落は listen しない。
    #[serde(default)]
    pub gate: GateConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ConversationConfig {
    /// typed history 送信経路を有効化（既定 false）。
    #[serde(default)]
    pub typed_history: bool,
    /// typed 経路で RESPONSE_ONLY_DIRECTIVE を外す（既定 false=付けたまま）。§9.2-6。
    #[serde(default)]
    pub drop_response_directive: bool,
}

#[allow(clippy::derivable_impls)]
impl Default for ConversationConfig {
    fn default() -> Self {
        Self {
            typed_history: false,
            drop_response_directive: false,
        }
    }
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct GateConfig {
    #[serde(default)]
    pub listen_socket: String,
    /// Nostr ingress is V3-only; missing or any other value fails when Nostr is enabled.
    #[serde(default)]
    pub nostr_ingress: String,
    /// Discord ingress is V3-only; missing or any other value fails startup.
    #[serde(default)]
    pub discord_ingress: String,
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

/// 退避ファイル（`workspace/tmp/`）の掃除設定（#711）。
///
/// ツール結果が inline 上限を超えると本文が `data/agents/{agent_id}/workspace/tmp/` へ
/// 退避されるが、消す経路がコードに無く無限に増える（実測 206MB / 2,636 個・+44/日）。
/// mtime がこれより古い退避ファイルを日次で消す。**対象は `tmp/` の通常ファイルのみ**で、
/// ディレクトリ・サブディレクトリ・マーカー/隠しファイル・DB には触れない
/// （詳細は `offload_cleanup` モジュール参照）。
#[derive(Debug, Deserialize, Clone)]
pub struct OffloadCleanupConfig {
    /// 掃除ループの on/off。既定 true。止めたいときは false（再ビルド不要）。
    #[serde(default = "default_offload_cleanup_enabled")]
    pub enabled: bool,
    /// 保持日数。mtime がこれより古い退避ファイルを消す。既定 7 日。
    /// オーナーが 7 日超を手動削除して破綻報告なし = 実測の安全な上界を採用（発明しない）。
    /// `0` や負値は「今書いたファイルまで消す」危険側なので `offload_cleanup` 側で下限 1 へ
    /// 丸めて warn を出す（掃除を止めたいなら `enabled = false`）。
    #[serde(default = "default_offload_cleanup_retention_days")]
    pub retention_days: i64,
    /// 掃除 tick の間隔（秒）。既定 86400（日次）。最低 3600 秒に丸める。
    #[serde(default = "default_offload_cleanup_interval_secs")]
    pub interval_secs: u64,
}

impl Default for OffloadCleanupConfig {
    fn default() -> Self {
        Self {
            enabled: default_offload_cleanup_enabled(),
            retention_days: default_offload_cleanup_retention_days(),
            interval_secs: default_offload_cleanup_interval_secs(),
        }
    }
}

fn default_offload_cleanup_enabled() -> bool {
    true
}
fn default_offload_cleanup_retention_days() -> i64 {
    7
}
fn default_offload_cleanup_interval_secs() -> u64 {
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
    /// catch-up 対象の source アダプタ群。**source 名をキーにした汎用テーブル**。`kind` で
    /// アダプタ種別を選ぶ。REST 一覧 API を叩くだけの source は **設定だけで足せる**
    /// （コード変更不要 / issue #470）。未設定なら catch-up はしない
    /// （webhook 受信は secret さえ設定すれば動く）。
    #[serde(default)]
    pub sources: Vec<IntakeSourceConfig>,
}

impl Default for IntakeConfig {
    fn default() -> Self {
        Self {
            secrets: HashMap::new(),
            routes: Vec::new(),
            process_interval_secs: default_intake_process_interval_secs(),
            catch_up_interval_secs: default_intake_catch_up_interval_secs(),
            sources: Vec::new(),
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

/// catch-up アダプタの種別。REST 一覧 API を叩くだけの source は `rest_list` で足りる。
/// 特殊な認証や非 REST（ページング等、設定で吸収しきれない形）が要るときだけ新しい種別を
/// 実装する。**「何でも書ける設定言語」にしない**——吸収しきれないものは種別を分ける。
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IntakeSourceKind {
    /// 一覧 API（配列を返す GET）を叩く汎用型。
    RestList,
}

/// catch-up の 1 source 設定。`name`/`kind` 以外のフィールドはアダプタ種別により解釈が変わる。
/// 現状唯一の種別 `rest_list` は base_url + list_path + query を組み立てて GET する。
#[derive(Debug, Deserialize, Clone)]
pub struct IntakeSourceConfig {
    /// source 名。`[intake.secrets]` / `[[intake.routes]]` の source と同じキー。
    pub name: String,
    /// アダプタ種別（`rest_list` 等）。未知の値は config パースエラー（黙って無視しない）。
    pub kind: IntakeSourceKind,
    /// この source の catch-up を有効にするか。既定 true（セクションを書けば有効）。
    /// **`false` にすると設定を残したまま catch-up を一時停止できる**（webhook 受信は無影響）。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// [rest_list] 一覧 API のベース URL（例: `https://kb.example`）。空なら catch-up しない。
    /// 末尾スラッシュは有無どちらでも可。
    #[serde(default)]
    pub base_url: String,
    /// [rest_list] 送信側認証。既定は無認証（omit）。Bearer は
    /// `auth = { kind = "bearer", token = "${ENV}" }`。**webhook 受信の HMAC secret とは別物**。
    #[serde(default)]
    pub auth: IntakeAuth,
    /// [rest_list] base_url に続けるパス（例: `/v1/comments/recent`）。先頭スラッシュは補う。
    #[serde(default)]
    pub list_path: String,
    /// [rest_list] URL クエリ（key=value をそのまま付ける・値は文字列）。**取得件数の上限
    /// （`limit` 等）もここに入れる**。
    /// **注意（どちら向きに働くか）: `limit` を省くと source 側の既定で取得する＝**
    /// **全件が返ると受信箱が膨らむ「広がる」方向。件数を絞りたいなら必ず設定しろ。**
    #[serde(default)]
    pub query: std::collections::BTreeMap<String, String>,
    /// [rest_list] dedup に使う id フィールド名（既定 `"id"`）。この値が取れない要素は捨てる
    /// （id 無しを hash に落とすと catch-up の度に別キーになり毎回積み直す。webhook↔catch-up の
    /// 相互 dedup も壊れる）。
    #[serde(default = "default_intake_id_field")]
    pub id_field: String,
    /// [rest_list] この一覧が生む event_type（例: `comment.created`）。webhook 側の `type` と
    /// 一致させること（同じ dedup_key `{event_type}:{id}` を作る）。空なら catch-up しない。
    #[serde(default)]
    pub event_type: String,
    /// [rest_list] レスポンス配列の場所（トップレベルの単一キー）。省略時は防御的に自動検出
    /// （トップレベル配列 / `comments` / `data` / `items` / `results`）。これで吸収しきれない
    /// 形（入れ子・ページング等）は設定を膨らませず**新しい `kind` を実装する**。
    #[serde(default)]
    pub array_path: Option<String>,
}

/// catch-up ポーリングの**送信側**認証（source API への Bearer 等）。webhook **受信**の HMAC
/// secret（`[intake.secrets]`）とは別物。既定は無認証。
#[derive(Debug, Deserialize, Clone, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IntakeAuth {
    /// 認証ヘッダを付けない（`auth` を書かなければこれ）。
    #[default]
    None,
    /// `Authorization: Bearer <token>`。token は秘密。`${ENV}` で注入しログに出さない。
    Bearer {
        #[serde(default)]
        token: String,
    },
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
fn default_intake_id_field() -> String {
    "id".to_string()
}

/// 非ブロックツール実行（dispatch）の設定（RFC #152 S3a）。
///
/// dispatch は「LLM のツール呼び出しを background subtask として実行し、そのターンには
/// `{"status":"spawned"}` だけを返して完了後に別ターンで再注入する」挙動。非同期化しない
/// ツール（各定義の `class.dispatch == Inline` ＋ 制御ツール ＋ core inline。
/// `executor.inline_tool_names()` が索引から集める）は従来どおり inline 実行。
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
    /// transport非依存の`[subtask]`名前空間だけを使用する。
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
    /// Resolve the transport-neutral configured notification destination.
    pub fn default_subtask_webhook(
        &self,
    ) -> Option<opencrab_actions::webhook_target::WebhookConfig> {
        self.subtask.default_webhook.as_ref().and_then(|config| {
            opencrab_actions::webhook_target::WebhookConfig::from_parts(
                config.url.clone(),
                config.events.clone(),
            )
        })
    }
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

