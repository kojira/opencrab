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
/// **既定 ON（#457）**。発火の仕方（[`decide_condense`] 参照）:
/// - 残ユニット（カーソルより新しい未凝縮）が窓幅 [`min_new_units`] 以上 → **積み残し消化**として
///   throttle を待たず 1 tick 1 窓で発火（新規と同じく淡々と消化する / オーナー指摘の趣旨）。ただし
///   partial が続いたときは指数バックオフで間引く（上限は [`min_interval_minutes`]）。
/// - 0 < 残 < 窓幅 → 末尾の端数。**[`min_interval_minutes`] を待って**から流す（新しいユニットの
///   増加を待つのはここだけ）。残 0 ならゼロコールで return。
///
/// [`decide_condense`]: crate::memory_condense
#[derive(Debug, Deserialize, Clone)]
pub struct MemoryCondenseConfig {
    /// 凝縮ラン全体の on/off。**既定 true（#457: 出荷時既定を ON）**。
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
    // #457: 出荷時既定を ON にする（オーナー判断）。#411 PR-1 は既定オフで入れたが、凝縮ランを
    // 標準機能として全エージェントに効かせる。他パラメータ（窓幅・間隔・timeout・バックオフ base）
    // は #411 PR-3 で実測確定する仮値なので触らない。
    true
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

