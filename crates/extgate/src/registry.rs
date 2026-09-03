//! process-local live registry。startup は空。DB から復元しない。

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
#[cfg(any(test, feature = "extgate-probe"))]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use opencrab_actions::{PrivilegeFire, WatchAllowSets};
use opencrab_db::Db;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::oneshot;

use crate::bearer::OperatorToken;
use crate::bundle::NostrBundleAdmit;
use crate::delivery_mode::DeliveryMode;
use crate::error::{ErrorCode, GateError};
use crate::operations::GatewayOperationDeclaration;
use crate::turn_queue::SessionTurnQueues;

/// hello 済みで未 close の接続。
pub struct LiveEntry {
    pub identity: u64,
    pub revision: u64,
    pub writer: Arc<tokio::sync::Mutex<OwnedWriteHalf>>,
    pub acknowledged: HashSet<String>,
    pub pending: HashMap<String, Pending>,
    /// hello で宣言された immutable な能力 snapshot（DI 拡張 §4.1）。欠落=能力ゼロ。
    pub declarations: Arc<Vec<GatewayOperationDeclaration>>,
    /// 宣言配列の canonical digest（DI-04）。宣言なしは空 string。
    pub declaration_digest: String,
}

impl LiveEntry {
    /// 宣言済みかつ callback 有無を含めて operation を引く。core は operation 名で分岐しない
    /// が、visibility/invoke 判定で live declaration に存在するかだけを generic に確かめる。
    pub fn declaration(&self, operation: &str) -> Option<&GatewayOperationDeclaration> {
        self.declarations.iter().find(|d| d.name == operation)
    }
}

pub enum Pending {
    Bind {
        binding_id: String,
        started: Instant,
    },
    Say {
        delivery_id: String,
    },
    /// DI 拡張 §5.1・option B。invoke 応答待ち。`reply` は背景 subtask で await している
    /// `invoke_and_wait` へ terminal outcome を届ける oneshot（handle_response / close が送る）。
    Invoke {
        call_id: String,
        binding_id: String,
        operation: String,
        reply: oneshot::Sender<OperationOutcome>,
    },
    /// 発話クラス（撃ちっぱなし・DESIGN-RESUME-SETTLE §3.3・第三柱）の invoke 応答待ち。
    /// `Say` と同型で operation_call を作らず deliveries 行だけを terminal 化する（settle/resume
    /// を起こさない・oneshot を持たない）。失敗（external_rejected）は say と同じく
    /// `turn_failed`/❌ で表面化し（C9）、`reply_target` はその発端 origin。
    Utterance {
        delivery_id: String,
        binding_id: String,
        /// ❌ を付ける発端 origin（単一メンションのみ Some・bundle/曖昧は None）。
        reply_target: Option<String>,
    },
}

impl Pending {
    pub fn is_bind(&self) -> bool {
        matches!(self, Self::Bind { .. })
    }

    pub fn is_say(&self) -> bool {
        matches!(self, Self::Say { .. })
    }

    pub fn is_invoke(&self) -> bool {
        matches!(self, Self::Invoke { .. })
    }

    pub fn is_utterance(&self) -> bool {
        matches!(self, Self::Utterance { .. })
    }

    pub fn binding_id(&self) -> Option<&str> {
        match self {
            Self::Bind { binding_id, .. } => Some(binding_id),
            Self::Invoke { binding_id, .. } => Some(binding_id),
            Self::Utterance { binding_id, .. } => Some(binding_id),
            Self::Say { .. } => None,
        }
    }

    pub fn delivery_id(&self) -> Option<&str> {
        match self {
            Self::Say { delivery_id } => Some(delivery_id),
            Self::Utterance { delivery_id, .. } => Some(delivery_id),
            Self::Bind { .. } | Self::Invoke { .. } => None,
        }
    }
}

#[derive(Default)]
pub struct Registry {
    live: HashMap<String, LiveEntry>,
}

impl Registry {
    pub fn get(&self, instance_id: &str) -> Option<&LiveEntry> {
        self.live.get(instance_id)
    }

    pub fn get_mut(&mut self, instance_id: &str) -> Option<&mut LiveEntry> {
        self.live.get_mut(instance_id)
    }

    pub fn is_live(&self, instance_id: &str) -> bool {
        self.live.contains_key(instance_id)
    }

    pub fn insert(&mut self, instance_id: String, entry: LiveEntry) {
        self.live.insert(instance_id, entry);
    }

    /// 同じ `connection_identity` の entry だけ消す。
    pub fn remove_if_identity(&mut self, instance_id: &str, identity: u64) -> Option<LiveEntry> {
        match self.live.get(instance_id) {
            Some(e) if e.identity == identity => self.live.remove(instance_id),
            _ => None,
        }
    }

    pub fn pending_say_ids(&self, instance_id: &str) -> Vec<String> {
        self.live
            .get(instance_id)
            .map(|e| {
                e.pending
                    .values()
                    .filter_map(|p| p.delivery_id().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn identities(&self) -> Vec<(String, u64)> {
        self.live
            .iter()
            .map(|(id, e)| (id.clone(), e.identity))
            .collect()
    }

    /// live かつ acknowledged binding を 1 件以上持つ instance_id 群（platform 非依存）。
    /// 「専用 V3 gateway が実際に受信できる状態」を表す。二重受信防止 lever の liveness 判定に使う。
    pub fn live_instances_with_ack(&self) -> Vec<String> {
        self.live
            .iter()
            .filter(|(_, e)| !e.acknowledged.is_empty())
            .map(|(id, _)| id.clone())
            .collect()
    }
}

/// conformance 用の計測と failure injection。本番 state には載せない。
#[cfg(any(test, feature = "extgate-probe"))]
#[derive(Default)]
pub struct GateProbe {
    pub accept_inbound_count: AtomicUsize,
    pub start_session_turn_count: AtomicUsize,
    pub lookup_resolve_count: AtomicUsize,
    pub lookup_dm_any_count: AtomicUsize,
    pub lookup_dm_count: AtomicUsize,
    pub lookup_wl_count: AtomicUsize,
    pub fail_reply_log: AtomicBool,
    pub fail_delivery_insert: AtomicBool,
    pub fail_say_write: AtomicBool,
    pub whitelist_override: Mutex<Option<bool>>,
    pub turn_queue_dropped: AtomicUsize,
}

/// kind_id=nostr の said を record 前に判定する。不正アンカーは `Err(bad_request)`。
pub type NostrSaidAdmit =
    Arc<dyn Fn(&str, &str, &str) -> Result<NostrSaidDecision, GateError> + Send + Sync>;
pub type NostrWorkspaceFn = Arc<dyn Fn(&str) -> Option<PathBuf> + Send + Sync>;
pub type NostrRelayFn = Arc<dyn Fn(&str, String) + Send + Sync>;
pub type NostrWatchSetsFn = Arc<dyn Fn(&str) -> Option<NostrWatchSets> + Send + Sync>;

#[derive(Debug, Clone, Default)]
pub struct NostrWatchSets {
    pub followees: HashSet<String>,
    pub owner: HashSet<String>,
    pub co_agents: HashSet<String>,
    pub trusted_users: HashSet<String>,
}

impl NostrWatchSets {
    pub fn as_watch_allow(&self) -> WatchAllowSets<'_> {
        WatchAllowSets {
            followees: &self.followees,
            owner: &self.owner,
            co_agents: &self.co_agents,
            trusted_users: &self.trusted_users,
        }
    }
}

#[derive(Debug, Clone)]
pub struct NostrHeldTurn {
    pub session_id: String,
    pub instance_id: String,
    pub agent_id: String,
    pub binding_id: String,
    pub origin: String,
    pub author_id: String,
    pub text: String,
    pub images: Vec<String>,
    pub address: String,
    pub owner_id: String,
    pub kind_id: String,
    pub delivery_mode: DeliveryMode,
    pub prompt_suffix: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NostrSaidDecision {
    Drop {
        bundle: Option<NostrBundleAdmit>,
    },
    Accept {
        watch_id: Option<i64>,
        immediate: bool,
        bundle: Option<NostrBundleAdmit>,
    },
}

/// invoke の三結果（DI 拡張 §5.3）。terminal 化後に oneshot で `invoke_and_wait` へ届ける。
#[derive(Debug, Clone)]
pub enum OperationOutcome {
    /// gateway が `ok(result)` を返した。result は JSON text（JSON null は `"null"`）。
    Succeeded { result_json: String },
    /// gateway が `err(operation_rejected)` を返した。
    Failed,
    /// write/EOF/protocol close/ack 不明、または startup 残 sending。
    Indeterminate,
}

/// builtin / 既存 tool 名との collision 判定（DI-03 → bad_request）。projection が core の
/// tool registry を渡す。未登録なら collision なしとして扱う（wire 単体テスト用）。
pub type ReservedToolNameFn = Arc<dyn Fn(&str) -> bool + Send + Sync>;

pub struct ExtgateState {
    pub db: Db,
    pub registry: Mutex<Registry>,
    pub token: OperatorToken,
    pub halt: AtomicBool,
    halt_notify: tokio::sync::Notify,
    next_identity: AtomicU64,
    nostr_said_admit: Mutex<Option<NostrSaidAdmit>>,
    nostr_workspace: Mutex<Option<NostrWorkspaceFn>>,
    nostr_relay: Mutex<Option<NostrRelayFn>>,
    nostr_watch_sets: Mutex<Option<NostrWatchSetsFn>>,
    nostr_privilege: Mutex<HashMap<i64, PrivilegeFire<NostrHeldTurn>>>,
    reserved_tool_name: Mutex<Option<ReservedToolNameFn>>,
    pub turn_queues: Arc<SessionTurnQueues>,
    /// #930/#933: 走行中ターンへ畳み込んで LLM に渡した said の **external_origins.seq の集合**を
    /// session ごとに記録する。record→enqueue で積まれた「その said 自身の独立ターン」が後で
    /// dequeue した際、`folded_seqs.contains(said.seq)` なら **独立ターンを起こさず skip**
    /// （二重処理を防ぐ・#930 第2欠陥）。
    /// #933 修正2: スカラ高水位は OnlySpeaker 畳み込み（Nostr resume が `OnlySpeaker(author)`）で
    /// 別話者の**未 fold** said を over-skip（`seq <= 高水位`で lost message）し得たため、**実際に
    /// fold した seq だけ**を持つ非消費の集合（BTreeSet）に置換。skip は `contains` のみ＝per-origin
    /// で正しく、非消費なので二重 take にも免疫。肥大は dequeue した seq より小さいエントリを prune
    /// （turn_queues は到着順 FIFO ＝ より小さい seq は既に dequeue 済みなので安全）。新 DB テーブルなし。
    folded_seqs: Mutex<HashMap<String, BTreeSet<i64>>>,
    /// #935 (c3): 「消費済み入力」登録簿の完了 id 側。build で初描画された subtask 完了
    /// （`[sN 完了]`）の subtask_id を session ごとに記録する。その完了に対する resume が
    /// `run_v3_said_less_turn` 頭へ到達したとき `is_consumed_completion` が真なら **resume を
    /// 起こさず skip**（消費済み入力の二重処理を防ぐ・said の [`folded_seqs`] と同型）。seq（i64・
    /// prune 対象）と 完了 id（String）は型が違うため同一 session キーの下に別コレクションで持つ
    /// （登録簿は 1 つ・#935 設計 (c3) の実装注記: prune は seq のみに意味があるため共通化しない）。
    consumed_completions: Mutex<HashMap<String, std::collections::HashSet<String>>>,
    /// #935 (a)/(b): 「プロンプトへ初投入した最終 log id」の per-session watermark（ターン跨ぎ）。
    /// poll（`crate::process` SessionLiveInbound）の watermark 概念をターン跨ぎで持つ。build で
    /// これより後の said が「初投入」＝発端以外は read+consumed・発端は started。ターン後に前進。
    injected_watermark: Mutex<HashMap<String, i64>>,
    #[cfg(any(test, feature = "extgate-probe"))]
    pub probe: GateProbe,
}

impl ExtgateState {
    pub fn new(db: Db, token: OperatorToken) -> Self {
        Self {
            db,
            registry: Mutex::new(Registry::default()),
            token,
            halt: AtomicBool::new(false),
            halt_notify: tokio::sync::Notify::new(),
            next_identity: AtomicU64::new(1),
            nostr_said_admit: Mutex::new(None),
            nostr_workspace: Mutex::new(None),
            nostr_relay: Mutex::new(None),
            nostr_watch_sets: Mutex::new(None),
            nostr_privilege: Mutex::new(HashMap::new()),
            reserved_tool_name: Mutex::new(None),
            turn_queues: Arc::new(SessionTurnQueues::new()),
            folded_seqs: Mutex::new(HashMap::new()),
            consumed_completions: Mutex::new(HashMap::new()),
            injected_watermark: Mutex::new(HashMap::new()),
            #[cfg(any(test, feature = "extgate-probe"))]
            probe: GateProbe::default(),
        }
    }

    /// #935 (c3): この subtask 完了 id を「build で初描画＝消費済み」として登録簿へ記録する。
    pub fn mark_consumed_completion(&self, session_id: &str, subtask_id: &str) {
        if let Ok(mut map) = self.consumed_completions.lock() {
            map.entry(session_id.to_string())
                .or_default()
                .insert(subtask_id.to_string());
        }
    }

    /// #935 (c3): この完了 id が「消費済み」か（非消費・読み取りのみ）。resume 側は真なら skip する。
    pub fn is_consumed_completion(&self, session_id: &str, subtask_id: &str) -> bool {
        self.consumed_completions
            .lock()
            .ok()
            .map(|map| map.get(session_id).is_some_and(|s| s.contains(subtask_id)))
            .unwrap_or(false)
    }

    /// #935 (a)/(b): 「プロンプトへ初投入した最終 log id」watermark を読む。未設定なら `init`
    /// （＝発端 said の log id）で初期化して返す。初期化により、初回ターンで履歴の古い said が
    /// 「初投入」と誤判定されて read されるのを防ぐ（restart 後も同様）。
    pub fn injected_watermark_or_init(&self, session_id: &str, init: i64) -> i64 {
        self.injected_watermark
            .lock()
            .map(|mut map| *map.entry(session_id.to_string()).or_insert(init))
            .unwrap_or(init)
    }

    /// #935 (a)/(b): watermark を単調前進させる（`id` が現在値より大きいときだけ更新）。
    pub fn advance_injected_watermark(&self, session_id: &str, id: i64) {
        if let Ok(mut map) = self.injected_watermark.lock() {
            let e = map.entry(session_id.to_string()).or_insert(id);
            if id > *e {
                *e = id;
            }
        }
    }

    /// #930/#933: この said の external_origins.seq を「走行中ターンへ畳み込んで read 済み」として
    /// 集合へ記録する（read state を emit する時点で・fold した各 said ごとに呼ぶ）。**実際に fold
    /// した seq だけ**を入れる（over-skip を防ぐ）。
    pub fn mark_folded_seq(&self, session_id: &str, seq: i64) {
        if let Ok(mut map) = self.folded_seqs.lock() {
            map.entry(session_id.to_string()).or_default().insert(seq);
        }
    }

    /// #933: この seq が「畳み込み済み」か（**非消費**・読み取りのみ）。enqueue 側は真なら独立ターン
    /// を skip する。実際に fold した seq のみ真＝別話者の未 fold said を over-skip しない。同じ seq
    /// を何度照会しても真のままで、二重 take に免疫。
    pub fn is_folded(&self, session_id: &str, seq: i64) -> bool {
        self.folded_seqs
            .lock()
            .ok()
            .map(|map| map.get(session_id).is_some_and(|s| s.contains(&seq)))
            .unwrap_or(false)
    }

    /// #933: dequeue した seq（`below`）より小さいエントリを掃除する（肥大防止）。turn_queues は
    /// 到着順 FIFO ＝ `below` より小さい seq の said ターンは既に dequeue 済みなので、その fold 記録は
    /// もう不要（安全に prune）。空になった session は除去。
    pub fn prune_folded_below(&self, session_id: &str, below: i64) {
        if let Ok(mut map) = self.folded_seqs.lock() {
            if let Some(set) = map.get_mut(session_id) {
                // below 未満を削除（below 以上を残す）。
                *set = set.split_off(&below);
                if set.is_empty() {
                    map.remove(session_id);
                }
            }
        }
    }

    pub fn set_nostr_said_admit(&self, admit: NostrSaidAdmit) {
        *self.nostr_said_admit.lock().expect("nostr admit") = Some(admit);
    }

    pub fn set_nostr_workspace(&self, workspace: NostrWorkspaceFn) {
        *self.nostr_workspace.lock().expect("nostr workspace") = Some(workspace);
    }

    pub fn set_nostr_relay(&self, relay: NostrRelayFn) {
        *self.nostr_relay.lock().expect("nostr relay") = Some(relay);
    }

    pub fn set_nostr_watch_sets(&self, sets: NostrWatchSetsFn) {
        *self.nostr_watch_sets.lock().expect("nostr watch sets") = Some(sets);
    }

    /// builtin / 既存 tool 名 collision 判定を登録する。
    pub fn set_reserved_tool_name(&self, hook: ReservedToolNameFn) {
        *self.reserved_tool_name.lock().expect("reserved tool name") = Some(hook);
    }

    /// name が builtin / 既存 tool と衝突するか。未登録なら false（wire 単体）。
    pub fn is_reserved_tool_name(&self, name: &str) -> bool {
        let hook = match self.reserved_tool_name.lock() {
            Ok(g) => g.clone(),
            Err(_) => return false,
        };
        hook.map(|h| h(name)).unwrap_or(false)
    }

    pub fn nostr_workspace_root(&self, agent_id: &str) -> Option<PathBuf> {
        let hook = self.nostr_workspace.lock().ok()?.clone();
        hook.and_then(|h| h(agent_id))
    }

    pub fn relay_nostr_inbound(&self, agent_id: &str, text: String) {
        let hook = match self.nostr_relay.lock() {
            Ok(g) => g.clone(),
            Err(_) => return,
        };
        if let Some(hook) = hook {
            hook(agent_id, text);
        }
    }

    pub fn nostr_watch_sets_for(&self, agent_id: &str) -> Option<NostrWatchSets> {
        let hook = self.nostr_watch_sets.lock().ok()?.clone();
        hook.and_then(|h| h(agent_id))
    }

    /// 専用 V3 gateway（`kind_id`）が当該 `agent_id` を実際に受信できる状態か（platform 非依存）。
    ///
    /// live かつ acknowledged binding を 1 件以上持つ instance のうち、`kind_id` が一致し subject が
    /// この agent に対応するものがあれば true。DESIGN-DISCORD-GATE §8.1 の二重受信防止 lever
    /// （共有 message_loop の `served_by_dedicated_gateway`）の liveness 側に使う汎用クエリで、
    /// core に platform 語彙を持ち込まない（`kind_id` は runtime 値の引数）。
    ///
    /// registry / DB のロック失敗は false（fail-open。true に倒すと共有側が黙って引き下がって
    /// 「どの gateway からも応答しない」を作りうるため、不明は共有側が続ける側へ倒す）。
    pub fn agent_has_live_gateway(&self, agent_id: &str, kind_id: &str) -> bool {
        let instances = match self.registry.lock() {
            Ok(reg) => reg.live_instances_with_ack(),
            Err(_) => return false,
        };
        if instances.is_empty() {
            return false;
        }
        let conn = match self.db.lock() {
            Ok(c) => c,
            Err(_) => return false,
        };
        instances.iter().any(|instance_id| {
            conn.query_row(
                "SELECT 1 FROM gate_instances i JOIN agents a ON a.subject_id = i.subject_id \
                 WHERE i.instance_id = ?1 AND i.kind_id = ?2 AND a.agent_id = ?3 \
                   AND i.deleted_at IS NULL",
                rusqlite::params![instance_id, kind_id, agent_id],
                |_| Ok(()),
            )
            .is_ok()
        })
    }

    pub fn privilege_for(
        &self,
        watch_id: i64,
        make: impl FnOnce() -> PrivilegeFire<NostrHeldTurn>,
    ) -> Result<PrivilegeFire<NostrHeldTurn>, GateError> {
        let mut g = self
            .nostr_privilege
            .lock()
            .map_err(|_| GateError::store())?;
        Ok(g.entry(watch_id).or_insert_with(make).clone())
    }

    pub fn admit_nostr_said(
        &self,
        agent_id: &str,
        author_id: &str,
        text: &str,
    ) -> Result<NostrSaidDecision, GateError> {
        let hook = self
            .nostr_said_admit
            .lock()
            .map_err(|_| GateError::store())?
            .clone();
        let Some(hook) = hook else {
            return Err(GateError::new(ErrorCode::BadRequest));
        };
        hook(agent_id, author_id, text)
    }

    pub fn lock_registry(&self) -> Result<MutexGuard<'_, Registry>, GateError> {
        match self.registry.lock() {
            Ok(g) => Ok(g),
            Err(_) => {
                self.halt();
                Err(GateError::new(ErrorCode::StoreError))
            }
        }
    }

    pub fn alloc_identity(&self) -> u64 {
        self.next_identity
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    pub fn halt(&self) {
        self.halt.store(true, std::sync::atomic::Ordering::SeqCst);
        self.halt_notify.notify_waiters();
    }

    pub fn is_halted(&self) -> bool {
        self.halt.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub async fn wait_until_halted(&self) {
        loop {
            if self.is_halted() {
                return;
            }
            self.halt_notify.notified().await;
        }
    }
}

#[cfg(test)]
mod folded_seq_tests {
    use super::*;

    fn test_state() -> ExtgateState {
        ExtgateState::new(
            opencrab_db::Db::memory().unwrap(),
            crate::OperatorToken::from_bytes("t"),
        )
    }

    // #933 不変(i): is_folded は fold した seq **だけ** 真（未 fold は偽）。
    #[test]
    fn is_folded_only_for_marked_seqs() {
        let s = test_state();
        s.mark_folded_seq("sess", 5);
        s.mark_folded_seq("sess", 9);
        assert!(s.is_folded("sess", 5));
        assert!(s.is_folded("sess", 9));
        assert!(!s.is_folded("sess", 7), "未 fold の 7 は skip されない");
        assert!(!s.is_folded("sess", 3), "未 fold の 3 は skip されない");
    }

    // #933 不変(ii): is_folded は非消費（同じ seq を何度照会しても真のまま）。
    // 旧 consume-once は 2 回目の take で漏れたが、非消費なら二重 take/複数 said に免疫。
    #[test]
    fn is_folded_is_non_consuming() {
        let s = test_state();
        s.mark_folded_seq("sess", 7);
        for _ in 0..3 {
            assert!(s.is_folded("sess", 7), "7 は何度照会しても skip 対象のまま");
        }
        assert!(!s.is_folded("sess", 8), "8 は独立ターンを起こす");
    }

    // #933 不変(iii): 複数 said 同時畳み込みで 34,35 とも skip 対象（取りこぼさない）。
    #[test]
    fn two_said_fold_both_marked() {
        let s = test_state();
        s.mark_folded_seq("sess", 34);
        s.mark_folded_seq("sess", 35);
        assert!(s.is_folded("sess", 34), "seq34 は skip 対象");
        assert!(s.is_folded("sess", 35), "seq35 は skip 対象");
    }

    // #933 修正2（R1・最重要）: OnlySpeaker 畳み込みでの over-skip 防止。
    // 別話者 B の said(seq40)が未 fold のまま、A の resume が A の said(seq41)だけ fold しても、
    // B(40)は skip されない（旧スカラ高水位なら 40<=41 で誤 skip＝lost message だった）。
    #[test]
    fn unfolded_other_speaker_seq_not_skipped_even_if_higher_folded() {
        let s = test_state();
        // A の resume が seq41 だけ畳み込み（B の seq40 は一度も fold されない）。
        s.mark_folded_seq("sess", 41);
        assert!(
            !s.is_folded("sess", 40),
            "未 fold の B(40)は独立ターンを起こす（lost 0）"
        );
        assert!(s.is_folded("sess", 41), "fold 済みの A(41)は skip");
    }

    // #933: prune は dequeue した seq 未満を掃除するが、未 fold の判定は変わらない（over-skip なし）。
    #[test]
    fn prune_below_keeps_unfolded_not_skipped() {
        let s = test_state();
        s.mark_folded_seq("sess", 41);
        // seq40 の said ターンが dequeue（40 未満を prune）。
        s.prune_folded_below("sess", 40);
        assert!(
            !s.is_folded("sess", 40),
            "prune 後も未 fold の 40 は skip されない"
        );
        assert!(s.is_folded("sess", 41), "41 は残る（40 以上）");
        // seq42 の said ターンが dequeue（42 未満を prune）→ 41 は掃除される。
        s.prune_folded_below("sess", 42);
        assert!(
            !s.is_folded("sess", 41),
            "41 の said は dequeue 済み＝prune で掃除"
        );
    }

    // #933 不変(iv): session ごとに独立。
    #[test]
    fn is_folded_is_per_session() {
        let s = test_state();
        s.mark_folded_seq("a", 10);
        assert!(s.is_folded("a", 10));
        assert!(!s.is_folded("b", 10), "別 session は未 fold");
    }
}
