//! 時刻起因のターン発火（#588 TimedFire）。
//!
//! scheduler が「時刻が来たら、このセッションで・このプロンプトで 1 ターン回して」と各ゲートウェイの
//! **既存のターンループ**へ渡すための、transport 中立な口。狙いは「入り口だけ特別、その先は通常ルート」:
//! ハートビートも #455 定時実行もアラームも、違うのは**いつ送るか**だけで、受ける側は同じ。したがって
//! このイベントは**種別を知らない**（`prompt` と宛先だけを運ぶ）。
//!
//! 受け口（各ゲートウェイの [`TimedFireSink`] 実装）は**薄く保つ**: 受けて自分の既存 turn を回すだけで、
//! 配送・ロック・記録・リアクション・継続ターンは全部ゲートウェイの既存実装が担う。これで
//! ハートビート専用の配送（旧 `heartbeat_delivery.rs`）や専用ターン（旧 `run_one_heartbeat`）が不要になる。

use crate::CallerIdentity;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

/// ログ用のプロンプト先頭プレビュー。全文は長いので先頭だけ・改行は空白に潰して 1 行にする
/// （時刻発火の送信側 scheduler と受信側の各ループが**同じ形**で出し、grep で突き合わせられるように）。
pub fn prompt_preview(prompt: &str) -> String {
    const MAX_CHARS: usize = 80;
    let one_line = prompt.replace(['\n', '\r'], " ");
    let head: String = one_line.chars().take(MAX_CHARS).collect();
    if one_line.chars().count() > MAX_CHARS {
        format!("{head}…")
    } else {
        head
    }
}

/// 1 ターンを追う相関 ID（診断ログ用・#665）。
///
/// ターンのライフサイクル（文脈構築 → LLM 呼び出し → ツール往復 → 応答）を横断する debug ログに
/// 載せ、llm_logs の行や複数の LLM イテレーションを 1 本のターンへ束ねて読むための短い識別子。
/// **制御には一切使わない**（純粋に可視化のためのラベル）。uuid の先頭 8 桁で十分に一意で、
/// grep しやすい短さに保つ。session_id が実質の相関キー（ターンはセッション単位で直列化される）で、
/// これはその上に載る「run 内で LLM/ツール往復を束ねる」補助キー。
pub fn new_turn_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
}

/// 時刻起因でセッションのターンを 1 回回す要求（transport 中立）。
pub struct TimedFireRequest {
    /// 発火先＝実会話セッション。**登録済み transport のセッション**で、書式は各 transport の
    /// [`TransportFire`] descriptor が名乗る（例: `nostr-{agent}` / `discord-{agent}-{guild}-{channel}`
    /// / `web-{agent}-{conversation}`）。列挙を固定しない（transport を足しても腐らない）。
    pub session_id: String,
    pub agent_id: String,
    /// transport 固有のチャンネル token（Discord は数値文字列、Nostr broadcast・web は空）。
    pub channel_id: String,
    /// Discord のギルド ID（DM とギルドの判別に使う。Nostr・web は空）。transport 固有だが、
    /// 発火先セッションの一部（`discord-{agent}-{guild}-{channel}`）なので scheduler が持っている。
    pub guild_id: String,
    /// その回に渡すプロンプト。受け口は**system プロンプトへ足す**（会話ログに「発言」として残さない）。
    pub prompt: String,
    /// 実行権限。時刻発火は本人の自己実行なので通常 `Owner`。
    pub caller: CallerIdentity,
}

/// ゲートウェイのループが実装する受け口。**受けて既存の turn を回すだけ**（薄く保つ）。
///
/// 実装は自分のループへイベント/ジョブを 1 本流すだけ（Discord は `LoopEvent::TimedFire`、Nostr は
/// 対応する `ResponseJob`）。非ブロック（発火側＝scheduler を塞がない）。
pub trait TimedFireSink: Send + Sync {
    fn fire_timed_turn(&self, req: TimedFireRequest);
}

// ============================================================================
// TransportFire descriptor（#628）: transport が「自分の性質と ID 書式」を名乗る。
// ============================================================================
//
// **descriptor は静的**（ゲートウェイ停止中でも受理判定に要る）。起動直後に生存非依存で
// 登録し、[`TimedFireRouter`] が sink（生存で register/unregister）と並べて持つ。旧
// `opencrab_db::queries::SessionFireTarget`（db 層が transport 名と ID 書式を列挙していた
// enum）を撤去し、各 transport の crate が自分の descriptor を実装する形へ移した。これで
// 「transport を 1 つ足す = その crate に descriptor を 1 つ書いて登録する」で完結し、
// db / heartbeat_fire / scheduler を触らない（#627 で web を足したときの実測が根拠）。

/// [`TransportFire::parse`] が返す**発火先の解決結果**（transport 中立）。
///
/// 発火本体（`run_one_heartbeat`）が [`TimedFireRequest`] を組むのに要る素材だけを持つ。
/// `channel_id` / `guild_id` は request の token（Discord は数値文字列、Nostr / web は空）。
/// `route` は **session_id を組み直すためだけ**の transport 固有トークンで request には載らない
/// （web の conversation_id がここに入る。Discord は channel/guild、Nostr は空で足りるので未使用）。
///
/// **`kind` を持つので値だけで自己記述できる**が、性質（G ゲート対象か・応答本文を出すか）や
/// session_id の組み直しは descriptor 側の関数で、[`TimedFireRouter::descriptor`] から引く
/// （性質の源を値へ二重化しない）。round-trip / 排他テスト（#628 条件 B・C）のため
/// [`PartialEq`] を導出する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FireTarget {
    /// この発火先を所有する transport（[`TransportFire::kind`]）。
    pub kind: &'static str,
    /// request の channel token（Discord=数値文字列 / Nostr・web=空）。
    pub channel_id: String,
    /// request の guild token（Discord=数値文字列 / Nostr・web=空）。
    pub guild_id: String,
    /// session_id を組み直すためだけの transport 固有トークン（web=conversation_id /
    /// Discord・Nostr=空）。request には載らない。
    pub route: String,
}

/// [`TransportFire::should_be_running`] が問い合わせる**実行時環境**（#628 条件 D）。
///
/// **これは「静的 descriptor」の例外**である。「この transport の受信ゲートウェイが設定上
/// 立ち上がるべきか」は db / config を引かないと答えられない。descriptor が自分で db を引ける
/// よう接続と、TOML 由来の共有ゲートウェイ設定（db に無い設定・Discord の `agent_ids` 等）を
/// 運ぶ。各 descriptor は**自分の kind 分だけ**を読む（server 側に kind → 期待の手書き対応表を
/// 作らないため。新 transport の期待判定はその descriptor の中で完結する）。
pub struct TransportFireEnv<'a> {
    /// db 接続（per-agent の有効設定を引く）。
    pub conn: &'a rusqlite::Connection,
    /// TOML の共有ゲートウェイが設定されている kind（db に無い設定を server が畳んで渡す）。
    pub configured_shared_kinds: &'a HashSet<&'static str>,
}

/// transport が「自分の発火先としての性質と ID 書式」を名乗る descriptor（#628）。
///
/// **静的**（`is_g_gated` / `posts_response_body` / `human_hint` / `kind` / parse / build）＝
/// ゲートウェイの生存に依存せず常に同じ答えを返す。唯一 [`should_be_running`] だけが
/// 実行時環境を引く述語（[`TransportFireEnv`] の doc・条件 D）。
///
/// object-safe（`Arc<dyn TransportFire>` で登録簿に入れる）。
///
/// [`should_be_running`]: TransportFire::should_be_running
pub trait TransportFire: Send + Sync {
    /// transport の種別名（sink 登録簿・[`FireTarget::kind`] と同じ文字列 / `gateway_kinds`）。
    fn kind(&self) -> &'static str;

    /// **この session_id が自分の発火先か**を判定して名乗る（session_id → 発火先）。
    ///
    /// 自分のものなら [`FireTarget`] を返し、そうでなければ `None`。`agent_id`（UUID・
    /// ハイフン入り）で接頭辞を剥がすので naive な `split('-')` にしない（fail-closed）。
    fn parse(&self, session_id: &str, agent_id: &str) -> Option<FireTarget>;

    /// [`parse`](Self::parse) の逆写像（発火先 → session_id）。両者が独立実装なので
    /// round-trip テスト（#628 条件 C）で恒真にならないことを担保する。
    fn build_session_id(&self, target: &FireTarget, agent_id: &str) -> String;

    /// 発火時に live G マスタゲートの対象か（Discord=true / Nostr・web=false）。
    fn is_g_gated(&self) -> bool;

    /// 発火ターンの応答本文がその場に自動配送されるか（Discord・web=true / Nostr=false）。
    fn posts_response_body(&self) -> bool;

    /// 「発火できる場所」をユーザに示す短い名詞句（remedy 文言の部品・条件 D で trait 側へ）。
    /// 例: 「Discord のチャンネル」。[`TimedFireRouter::fire_target_hint`] が全 descriptor 分を畳む。
    fn human_hint(&self) -> &'static str;

    /// **この transport の受信ゲートウェイが設定上「立ち上がるべき」か**（実行時述語・条件 D）。
    ///
    /// 起動時セルフチェックが「立ち上がるべきなのに sink が 0」を検出するのに使う。隔離環境
    /// （Discord / Nostr 無効）では false を返し、sink 不在を正常とみなせる。web は外部接続を
    /// 持たず常に立ち上がるので常に true。**静的ではない**（[`TransportFireEnv`] を引く）。
    fn should_be_running(&self, env: &TransportFireEnv) -> bool;

    /// round-trip / prefix 排他テスト（#628 条件 B・C）用のサンプル発火先。
    ///
    /// `build_session_id(sample) → parse → sample` が戻り、他 descriptor がその session_id を
    /// parse しないことを、登録簿を反復する generic テストで検査する。実装者がテスト行を
    /// 足し忘れても登録さえすれば検査対象になる。
    fn sample_target(&self) -> FireTarget;
}

/// `agent → 受け口` の登録簿（#588 TimedFire の**唯一の新規機構**）。
///
/// 各ゲートウェイのループが起動時に自分の受け口を登録し、scheduler が発火時に
/// [`TimedFireRouter::resolve`] で引く。**per-agent ゲートウェイ（自分のボット名で出る・#400）を優先**し、
/// 無ければ共有（TOML）ゲートウェイへ落ちる。この per-agent→共有の解決は、撤去した
/// `HeartbeatDiscordHttp`（http ハンドルの per-agent→共有）を「送り先ループの選択」へ置き換えたもの
/// ＝送信者の同一性（#400）はループが自分の gateway で送ることで自然に保たれる。
/// `(transport kind, agent_id)` → per-agent の受け口。
type PerAgentSinks = Mutex<HashMap<(&'static str, String), Arc<dyn TimedFireSink>>>;
/// transport kind → 共有（TOML）ゲートウェイの受け口。
type SharedSinks = Mutex<HashMap<&'static str, Arc<dyn TimedFireSink>>>;

/// transport descriptor の登録簿（#628）。**生存非依存**（起動直後に無条件登録）なので
/// sink（`per_agent` / `shared`）と違い unregister しない。挿入順を保つ Vec で、`fire_target_hint`
/// の並びと `resolve_target` の first-match 順が登録順で決まる（排他は条件 B のテストが担保）。
type Descriptors = Mutex<Vec<Arc<dyn TransportFire>>>;

#[derive(Default)]
pub struct TimedFireRouter {
    /// per-agent ゲートウェイの受け口（そのボット名で出る）。
    per_agent: PerAgentSinks,
    /// 共有（TOML）ゲートウェイの受け口（per-agent が無いエージェントの落ち先）。
    shared: SharedSinks,
    /// transport descriptor（静的・生存非依存 / #628）。
    descriptors: Descriptors,
}

impl TimedFireRouter {
    pub fn new() -> Self {
        Self::default()
    }

    /// per-agent ゲートウェイの受け口を登録する（そのボット名で出る）。
    pub fn register_per_agent(
        &self,
        kind: &'static str,
        agent_id: &str,
        sink: Arc<dyn TimedFireSink>,
    ) {
        self.per_agent
            .lock()
            .unwrap()
            .insert((kind, agent_id.to_string()), sink);
    }

    /// per-agent ゲートウェイの受け口を解除する（そのゲートウェイの停止時）。
    ///
    /// 解除すると [`TimedFireRouter::resolve`] は共有（TOML）ゲートウェイの受け口へ落ちる。これで
    /// 「専用ゲートウェイが動いている間はその体で、止まっていれば共有で」という #400 の動的
    /// フォールバックを保つ（旧 `HeartbeatDiscordHttp` が配送時に `get_http_for_agent` の None で
    /// 落としていたのと同じ挙動）。停止後も登録が残ると死んだループへ発火が消えるので必ず解除する。
    pub fn unregister_per_agent(&self, kind: &'static str, agent_id: &str) {
        self.per_agent
            .lock()
            .unwrap()
            .remove(&(kind, agent_id.to_string()));
    }

    /// 共有（TOML）ゲートウェイの受け口を登録する（per-agent が無いエージェントの落ち先）。
    pub fn register_shared(&self, kind: &'static str, sink: Arc<dyn TimedFireSink>) {
        self.shared.lock().unwrap().insert(kind, sink);
    }

    /// 発火先の受け口を引く（per-agent 優先 → 共有）。無ければ `None`（送れないので発火を諦める）。
    pub fn resolve(&self, kind: &'static str, agent_id: &str) -> Option<Arc<dyn TimedFireSink>> {
        if let Some(s) = self
            .per_agent
            .lock()
            .unwrap()
            .get(&(kind, agent_id.to_string()))
        {
            return Some(s.clone());
        }
        self.shared.lock().unwrap().get(kind).cloned()
    }

    /// その kind に per-agent か共有のいずれかの受け口が**1 つでも**登録されているか
    /// （起動時セルフチェック用・#603）。有効な受信ゲートウェイがあるのにこれが false なら、
    /// その transport の時刻発火はどこにも届かない（配線漏れ / 起動失敗）。
    pub fn has_sink_for_kind(&self, kind: &str) -> bool {
        self.per_agent
            .lock()
            .unwrap()
            .keys()
            .any(|(k, _)| *k == kind)
            || self.shared.lock().unwrap().contains_key(kind)
    }

    // ── descriptor 登録簿（#628・生存非依存） ────────────────────────────────

    /// transport descriptor を登録する（起動直後に無条件・生存非依存）。
    ///
    /// 同じ kind を二重登録しない（起動配線のミスを早期に潰す）。**sink と違い unregister しない**
    /// ——受理判定・ゲート理由表示はゲートウェイ停止中でも要るため常時在る。
    pub fn register_descriptor(&self, descriptor: Arc<dyn TransportFire>) {
        let mut descriptors = self.descriptors.lock().unwrap();
        if descriptors.iter().any(|d| d.kind() == descriptor.kind()) {
            tracing::warn!(
                kind = descriptor.kind(),
                "timed-fire: descriptor を二重登録しようとした（無視）"
            );
            return;
        }
        descriptors.push(descriptor);
    }

    /// kind から descriptor を引く（性質・build_session_id の問い合わせ用）。
    pub fn descriptor(&self, kind: &str) -> Option<Arc<dyn TransportFire>> {
        self.descriptors
            .lock()
            .unwrap()
            .iter()
            .find(|d| d.kind() == kind)
            .cloned()
    }

    /// session_id を登録済み descriptor で解決する（first-match・生存非依存）。
    ///
    /// 「どの 2 descriptor も同じ session_id を parse しない」を保てば登録順に依存しない
    /// （#628 条件 B のテストで全ペアを検査する）。どれも名乗らなければ `None`（fail-closed・
    /// 発火経路の無い種別 `heartbeat-` / `agent-msg-` 等）。
    pub fn resolve_target(&self, session_id: &str, agent_id: &str) -> Option<FireTarget> {
        self.descriptors
            .lock()
            .unwrap()
            .iter()
            .find_map(|d| d.parse(session_id, agent_id))
    }

    /// 登録済み descriptor の kind 集合（起動時セルフチェックの双方向照合用・条件 A）。
    pub fn descriptor_kinds(&self) -> HashSet<&'static str> {
        self.descriptors
            .lock()
            .unwrap()
            .iter()
            .map(|d| d.kind())
            .collect()
    }

    /// sink が登録されている kind 集合（per-agent ∪ 共有・条件 A）。
    pub fn sink_kinds(&self) -> HashSet<&'static str> {
        let mut kinds: HashSet<&'static str> =
            self.shared.lock().unwrap().keys().copied().collect();
        for (k, _) in self.per_agent.lock().unwrap().keys() {
            kinds.insert(k);
        }
        kinds
    }

    /// 発火できる場所を示す remedy 文言（全 descriptor の [`TransportFire::human_hint`] を畳む）。
    ///
    /// エラー文言 4 箇所（`agent_heartbeat` / `agent_schedule` / `api::schedules`）がこれを使う
    /// ので、transport を足すと remedy に自動で載る（手書きの列挙を撤去・#628）。
    pub fn fire_target_hint(&self) -> String {
        let descriptors = self.descriptors.lock().unwrap();
        let hints: Vec<&str> = descriptors.iter().map(|d| d.human_hint()).collect();
        hints.join("、")
    }

    /// 起動時セルフチェック（#628 条件 A・B）: **本番登録簿そのもの**で不整合を検出する。
    ///
    /// 手書きの kind リストを持たず、登録簿を反復して返すのは検出した不整合（呼び出し側が
    /// ERROR ログにする）。**手で積んだテスト用登録簿に依存しない**ので、本番の登録に足して
    /// テスト側への追記を忘れても、起動時にここで拾える。
    ///
    /// - **sink はあるが descriptor が無い** → 常に不整合（parse できない＝発火先を解決できない）。
    /// - **descriptor が `should_be_running(env)` なのに sink が無い** → 不整合（受信ゲートウェイが
    ///   立ち上がるべきなのに時刻発火が届かない＝配線 / 起動失敗）。隔離環境で `should_be_running`
    ///   が false の transport は sink 不在でも不整合にしない。
    /// - **prefix 排他違反（条件 B）** → ある descriptor A の `build_session_id(sample)` を別の
    ///   descriptor B(≠A) が parse したら、first-match（[`resolve_target`](Self::resolve_target)）で
    ///   片方が全セッションを横取りしうる。**本番登録簿そのもので**衝突を起動時に検出する
    ///   （手書き registry を反復する generic テストの取りこぼしをここで塞ぐ）。
    pub fn self_check(&self, env: &TransportFireEnv) -> Vec<TimedFireSelfCheckIssue> {
        // descriptor 側の検査は 1 度のロックで済ませる（sink 登録簿は別 Mutex なので先に取る）。
        let sink_kinds = self.sink_kinds();
        let descriptors = self.descriptors.lock().unwrap();
        let descriptor_kinds: HashSet<&'static str> =
            descriptors.iter().map(|d| d.kind()).collect();
        let mut issues = Vec::new();

        // sink → descriptor: descriptor の無い sink は parse 不能（必ず不整合）。
        for kind in &sink_kinds {
            if !descriptor_kinds.contains(kind) {
                issues.push(TimedFireSelfCheckIssue::SinkWithoutDescriptor { kind });
            }
        }
        // descriptor → sink: 立ち上がるべき transport に sink が無い（配線 / 起動失敗）。
        for descriptor in descriptors.iter() {
            let kind = descriptor.kind();
            if descriptor.should_be_running(env) && !sink_kinds.contains(kind) {
                issues.push(TimedFireSelfCheckIssue::ExpectedSinkMissing { kind });
            }
        }
        // prefix 排他（条件 B）: 本番登録簿の全ペアで、A の sample session_id を B が parse しないか。
        // probe の agent_id は parse/build に相対で効く任意の固定値でよい（発火先の解決は
        // agent_id 相対なので、同じ値で build/parse すれば書式の衝突だけを見られる）。
        for a in descriptors.iter() {
            let sid = a.build_session_id(&a.sample_target(), SELF_CHECK_PROBE_AGENT);
            for b in descriptors.iter() {
                if a.kind() != b.kind() && b.parse(&sid, SELF_CHECK_PROBE_AGENT).is_some() {
                    issues.push(TimedFireSelfCheckIssue::PrefixCollision {
                        owner: a.kind(),
                        shadowed_by: b.kind(),
                    });
                }
            }
        }
        issues
    }
}

/// [`TimedFireRouter::self_check`] の prefix 排他検査（条件 B）で build/parse に使う固定の probe
/// agent_id。発火先の解決は agent_id 相対なので、同じ値で build して parse すれば書式の衝突だけを
/// 検査できる（実在の agent である必要はない）。
const SELF_CHECK_PROBE_AGENT: &str = "00000000-0000-0000-0000-000000000000";

/// 起動時セルフチェック（[`TimedFireRouter::self_check`]）が見つけた不整合（#628 条件 A・B）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimedFireSelfCheckIssue {
    /// sink はあるが descriptor が無い（発火先を parse できない＝必ずバグ）。
    SinkWithoutDescriptor { kind: &'static str },
    /// descriptor が「立ち上がるべき」なのに sink が無い（受信ゲートウェイ未起動＝発火が届かない）。
    ExpectedSinkMissing { kind: &'static str },
    /// prefix 排他違反（条件 B）: `owner` の sample session_id を `shadowed_by` も parse する。
    /// first-match で登録順により片方が全セッションを横取りしうる（発火先の誤配送）。
    PrefixCollision {
        owner: &'static str,
        shadowed_by: &'static str,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingSink(Arc<AtomicUsize>);
    impl TimedFireSink for CountingSink {
        fn fire_timed_turn(&self, _req: TimedFireRequest) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// ログ用プレビュー: 短文はそのまま・改行は空白へ・80 文字超は … で切る（送受で同形）。
    #[test]
    fn prompt_preview_is_one_line_and_truncated() {
        assert_eq!(prompt_preview("短いプロンプト"), "短いプロンプト");
        // 改行/復帰は 1 行に潰す（grep で 1 行に収まる）。
        assert_eq!(
            prompt_preview("一行目\n二行目\r三行目"),
            "一行目 二行目 三行目"
        );
        // 80 文字ちょうどは切らない、超えたら … を付ける。
        let exactly_80 = "あ".repeat(80);
        assert_eq!(prompt_preview(&exactly_80), exactly_80);
        let over = "あ".repeat(81);
        let preview = prompt_preview(&over);
        assert!(preview.ends_with('…'), "超過は … で切る: {preview}");
        assert_eq!(preview.chars().count(), 81, "先頭 80 文字 + …");
    }

    /// per-agent を優先し、無ければ共有へ落ちる（#400 の per-agent→共有と同型）。
    #[test]
    fn resolves_per_agent_first_then_shared() {
        let router = TimedFireRouter::new();
        let per = Arc::new(AtomicUsize::new(0));
        let shared = Arc::new(AtomicUsize::new(0));
        router.register_shared("discord", Arc::new(CountingSink(shared.clone())));
        router.register_per_agent("discord", "crab", Arc::new(CountingSink(per.clone())));

        // crab は per-agent がある → per-agent を引く。
        router
            .resolve("discord", "crab")
            .unwrap()
            .fire_timed_turn(req("crab"));
        assert_eq!(
            (per.load(Ordering::SeqCst), shared.load(Ordering::SeqCst)),
            (1, 0)
        );

        // other は per-agent が無い → 共有へ落ちる。
        router
            .resolve("discord", "other")
            .unwrap()
            .fire_timed_turn(req("other"));
        assert_eq!(
            (per.load(Ordering::SeqCst), shared.load(Ordering::SeqCst)),
            (1, 1)
        );

        // 登録の無い transport は None（送れない）。
        assert!(router.resolve("nostr", "crab").is_none());
    }

    /// 起動時セルフチェック（#603）: per-agent でも共有でも 1 つあれば true、無ければ false。
    #[test]
    fn has_sink_for_kind_detects_missing_registration() {
        let router = TimedFireRouter::new();
        let counter = Arc::new(AtomicUsize::new(0));
        // 何も登録していないので全 kind false（これが #602 の「受け口が 0」状態）。
        assert!(!router.has_sink_for_kind("discord"));
        assert!(!router.has_sink_for_kind("nostr"));

        // per-agent を 1 つ登録 → その kind だけ true。
        router.register_per_agent("nostr", "crab", Arc::new(CountingSink(counter.clone())));
        assert!(router.has_sink_for_kind("nostr"));
        assert!(!router.has_sink_for_kind("discord"));

        // 共有だけでも true（per-agent 不在でも落ち先がある）。
        router.register_shared("discord", Arc::new(CountingSink(counter)));
        assert!(router.has_sink_for_kind("discord"));
    }

    fn req(agent: &str) -> TimedFireRequest {
        TimedFireRequest {
            session_id: format!("discord-{agent}-1-2"),
            agent_id: agent.to_string(),
            channel_id: "2".to_string(),
            guild_id: "1".to_string(),
            prompt: "p".to_string(),
            caller: CallerIdentity::Owner,
        }
    }

    // ── descriptor 登録簿（#628）のダミー実装でルータ機構を検査する ─────────────
    // （本物の Discord / Nostr / web descriptor は各 crate にあり、登録簿を反復する
    //   generic テスト——条件 B・C の排他・round-trip——は全 descriptor を見られる
    //   `crates/server` に置く。ここは actions 内の機構だけを検査する。）

    struct DummyFire {
        kind: &'static str,
        prefix: &'static str,
        g_gated: bool,
        should_run: bool,
    }
    impl TransportFire for DummyFire {
        fn kind(&self) -> &'static str {
            self.kind
        }
        fn parse(&self, session_id: &str, agent_id: &str) -> Option<FireTarget> {
            let want = format!("{}-{agent_id}", self.prefix);
            if session_id == want {
                Some(FireTarget {
                    kind: self.kind,
                    channel_id: String::new(),
                    guild_id: String::new(),
                    route: String::new(),
                })
            } else {
                None
            }
        }
        fn build_session_id(&self, _target: &FireTarget, agent_id: &str) -> String {
            format!("{}-{agent_id}", self.prefix)
        }
        fn is_g_gated(&self) -> bool {
            self.g_gated
        }
        fn posts_response_body(&self) -> bool {
            false
        }
        fn human_hint(&self) -> &'static str {
            self.kind
        }
        fn should_be_running(&self, _env: &TransportFireEnv) -> bool {
            self.should_run
        }
        fn sample_target(&self) -> FireTarget {
            FireTarget {
                kind: self.kind,
                channel_id: String::new(),
                guild_id: String::new(),
                route: String::new(),
            }
        }
    }

    fn dummy(kind: &'static str, prefix: &'static str) -> Arc<dyn TransportFire> {
        Arc::new(DummyFire {
            kind,
            prefix,
            g_gated: false,
            should_run: false,
        })
    }

    /// descriptor は生存非依存で登録され、first-match で session_id を解決する。二重登録は無視。
    #[test]
    fn resolve_target_first_match_and_no_double_register() {
        let router = TimedFireRouter::new();
        router.register_descriptor(dummy("alpha", "alpha"));
        router.register_descriptor(dummy("beta", "beta"));
        // 同じ kind の二重登録は無視（登録簿は 2 件のまま）。
        router.register_descriptor(dummy("alpha", "alpha"));
        assert_eq!(router.descriptor_kinds().len(), 2);

        let t = router.resolve_target("alpha-a1", "a1").unwrap();
        assert_eq!(t.kind, "alpha");
        assert!(router.resolve_target("beta-a1", "a1").is_some());
        // 発火経路の無い種別は None（fail-closed）。
        assert!(router.resolve_target("web-a1", "a1").is_none());
    }

    /// remedy 文言は登録した descriptor の human_hint を登録順に畳む（手書き列挙なし）。
    #[test]
    fn fire_target_hint_folds_registered_descriptors() {
        let router = TimedFireRouter::new();
        router.register_descriptor(dummy("discord", "discord"));
        router.register_descriptor(dummy("nostr", "nostr"));
        assert_eq!(router.fire_target_hint(), "discord、nostr");
    }

    /// 条件 A: self_check は「sink はあるが descriptor 無し」「立ち上がるべきなのに sink 無し」の
    /// 双方向で不整合を検出する。手書きの kind リストを持たない。
    #[test]
    fn self_check_detects_bidirectional_drift() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let configured: HashSet<&'static str> = HashSet::new();
        let env = TransportFireEnv {
            conn: &conn,
            configured_shared_kinds: &configured,
        };
        let counter = Arc::new(AtomicUsize::new(0));

        // (1) 立ち上がるべき descriptor（should_run=true）だが sink 未登録 → ExpectedSinkMissing。
        let router = TimedFireRouter::new();
        router.register_descriptor(Arc::new(DummyFire {
            kind: "discord",
            prefix: "discord",
            g_gated: true,
            should_run: true,
        }));
        assert_eq!(
            router.self_check(&env),
            vec![TimedFireSelfCheckIssue::ExpectedSinkMissing { kind: "discord" }]
        );

        // sink を足すと解消。
        router.register_shared("discord", Arc::new(CountingSink(counter.clone())));
        assert!(router.self_check(&env).is_empty());

        // (2) sink はあるが descriptor が無い kind → SinkWithoutDescriptor。
        router.register_shared("ghost", Arc::new(CountingSink(counter)));
        assert_eq!(
            router.self_check(&env),
            vec![TimedFireSelfCheckIssue::SinkWithoutDescriptor { kind: "ghost" }]
        );
    }

    /// 隔離環境（should_run=false）では sink 不在でも不整合にしない（隔離運用の担保）。
    #[test]
    fn self_check_allows_missing_sink_when_not_expected() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let configured: HashSet<&'static str> = HashSet::new();
        let env = TransportFireEnv {
            conn: &conn,
            configured_shared_kinds: &configured,
        };
        let router = TimedFireRouter::new();
        router.register_descriptor(dummy("discord", "discord")); // should_run=false
        assert!(router.self_check(&env).is_empty());
    }

    /// 条件 B（起動時防御）: prefix が衝突する descriptor を**本番と同じ登録簿に**積んだら、
    /// self_check が起動時に PrefixCollision を上げる。手書き registry を反復する generic テスト
    /// への追記を忘れても、本番登録簿そのものでここが拾う（レビュー指摘のブロッカー対応）。
    #[test]
    fn self_check_detects_prefix_collision() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let configured: HashSet<&'static str> = HashSet::new();
        let env = TransportFireEnv {
            conn: &conn,
            configured_shared_kinds: &configured,
        };
        let router = TimedFireRouter::new();
        // 別 kind だが同じ書式（prefix）を parse する 2 つ = first-match で横取りが起きる。
        router.register_descriptor(dummy("first", "shared"));
        router.register_descriptor(dummy("second", "shared"));

        let issues = router.self_check(&env);
        // 両方向で衝突が上がる（A の sample を B が、B の sample を A が parse）。
        assert!(
            issues.contains(&TimedFireSelfCheckIssue::PrefixCollision {
                owner: "first",
                shadowed_by: "second",
            }),
            "first→second の衝突が検出されない: {issues:?}"
        );
        assert!(
            issues.contains(&TimedFireSelfCheckIssue::PrefixCollision {
                owner: "second",
                shadowed_by: "first",
            }),
            "second→first の衝突が検出されない: {issues:?}"
        );
    }

    /// 衝突しない登録簿（別 prefix）では PrefixCollision を上げない（偽陽性を出さない）。
    #[test]
    fn self_check_no_collision_for_disjoint_prefixes() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let configured: HashSet<&'static str> = HashSet::new();
        let env = TransportFireEnv {
            conn: &conn,
            configured_shared_kinds: &configured,
        };
        let router = TimedFireRouter::new();
        router.register_descriptor(dummy("alpha", "alpha"));
        router.register_descriptor(dummy("beta", "beta"));
        assert!(router.self_check(&env).is_empty());
    }
}
