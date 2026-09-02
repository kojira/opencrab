//! セッション inbound の集約口（載せ替え §3）。
//!
//! ゲートは正規化した受信（本文・送信者の生識別子・対象 session_id）の束を
//! [`accept_inbound`] へ 1 回投げる。誰か・権限・standing・権限デバウンス・
//! trust 分割・record_only はここが決める。権限デバウンスのバッファと時限は
//! [`PrivilegeFire`] が持つ。配送（送信・描画・typing・webhook）はゲートに残す。
//! core が返すのは [`DeliveryEffect`]（ターンした件）。
//!
//! SkillEngine / conversation の実装は触らない。ゲートが直呼びしていた
//! [`crate::AgentRuntime`] の入口を、このモジュール 1 箇所へ集める。

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::agent_runtime::AgentRuntime;
use crate::session_watch_policy::{
    watch_author_standing, watch_hold_interval_secs, SessionPolicyError, WatchAllowSets,
};
use crate::transcript::{InboundMessageRecord, TranscriptSource};
use crate::CallerIdentity;
use crate::RunRequest;
use opencrab_core::EngineResult;

/// ゲートが core に渡す正規化済み受信 1 件。
///
/// 機械的な配送ハンドル（HTTP・描画）は含めない。`session_id` の書式はゲート側の
/// 現行規約のまま（Discord なら `discord-{agent}-{guild}-{channel}`）。
#[derive(Debug, Clone)]
pub struct NormalizedInbound<'a> {
    pub session_id: &'a str,
    pub agent_id: &'a str,
    pub sender_id: &'a str,
    pub sender_name: &'a str,
    pub avatar_url: Option<&'a str>,
    pub channel_id: Option<&'a str>,
    pub pubkey: Option<&'a str>,
    pub text: &'a str,
    pub image_urls: &'a [String],
    pub external_id: &'a str,
}

impl<'a> NormalizedInbound<'a> {
    fn as_record(&self) -> InboundMessageRecord<'a> {
        InboundMessageRecord {
            session_id: self.session_id,
            recipient_agent_id: self.agent_id,
            sender_id: self.sender_id,
            sender_name: self.sender_name,
            avatar_url: self.avatar_url,
            channel_id: self.channel_id,
            pubkey: self.pubkey,
            text: self.text,
            image_urls: self.image_urls,
        }
    }
}

/// メッセージ全体を落とす理由（DM 事前ゲート）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboundMessageDrop {
    /// どのエージェントも送信者を信頼していない。
    DmNotTrusted,
}

/// エージェント 1 体分を落とす理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboundAgentDrop {
    DmNotTrustedForAgent,
    ChannelNotWhitelisted,
}

/// ゲートが inbound 1 口へ渡す正規化イベント（生識別子のみ。権限の真偽は載せない）。
#[derive(Debug, Clone, Copy)]
pub struct NormalizedInboundEvent<'a> {
    pub sender_id: &'a str,
    pub channel_id: &'a str,
    /// 空なら DM。
    pub guild_id: &'a str,
}

/// 誰か・権限の照会。3 アダプタ型は置かない。計算本体は runner の関数を渡す。
pub struct InboundLookups<'a> {
    pub resolve_caller: &'a dyn Fn(&str, &[String], &str) -> CallerIdentity,
    pub dm_allowed_any: &'a dyn Fn(&str, &[String], &str) -> bool,
    pub dm_allowed: &'a dyn Fn(&str, &str, &str) -> bool,
    pub channel_whitelisted: &'a dyn Fn(&str, &str) -> bool,
}

/// [`accept_inbound`] が 1 件分として受ける正規化イベント。
#[derive(Debug, Clone, Copy)]
pub struct InboundWork<'a> {
    pub event: NormalizedInboundEvent<'a>,
    pub has_content: bool,
    pub kind_label: &'a str,
    pub author_key: &'a str,
}

/// watch 束の追加材料。無ければ対話系（discord / web）。
pub struct WatchAccept<'a, T> {
    pub policy_json: &'a str,
    pub interval_secs: u64,
    pub allow: WatchAllowSets<'a>,
    pub owner: &'a std::collections::HashSet<String>,
    pub followees: &'a std::collections::HashSet<String>,
    /// `Some` = 対話系（core が権限デバウンスし時限発火）。`None` = 機械束ね flush（抱えない）。
    pub privilege: Option<&'a PrivilegeFire<T>>,
}

/// 通した 1 件。ゲートはこれを見て配送（ログ）する。ターン対象は `on_run` だけ。
/// ターンの文脈に含めた件は `on_run` の第 3 引数（読んだ事実。判定中間値ではない）。
#[derive(Debug, Clone)]
pub struct AdmittedInbound {
    pub caller: CallerIdentity,
    pub admitted_agent_ids: Vec<String>,
    pub agent_drops: Vec<(String, InboundAgentDrop)>,
}

impl AdmittedInbound {
    pub fn agent_drop(&self, agent_id: &str) -> Option<InboundAgentDrop> {
        self.agent_drops
            .iter()
            .find(|(id, _)| id == agent_id)
            .map(|(_, reason)| *reason)
    }
}

/// 束全体を落とす理由（1 件の対話系で DM 不信頼のときだけ）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InboundDrop {
    Message(InboundMessageDrop),
    Policy(SessionPolicyError),
}

impl std::fmt::Display for InboundDrop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Message(d) => write!(f, "{d:?}"),
            Self::Policy(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for InboundDrop {}

/// 権限毎デバウンスの内部バッファ。タイマーは [`PrivilegeFire`] が持つ。
#[derive(Debug)]
struct PrivilegeDebounce<T> {
    buckets: BTreeMap<u64, PrivilegeBucket<T>>,
}

#[derive(Debug)]
struct PrivilegeBucket<T> {
    items: Vec<T>,
    due: tokio::time::Instant,
}

impl<T> Default for PrivilegeDebounce<T> {
    fn default() -> Self {
        Self {
            buckets: BTreeMap::new(),
        }
    }
}

impl<T> PrivilegeDebounce<T> {
    fn push(&mut self, item: T, interval_secs: u64, now: tokio::time::Instant) {
        self.buckets
            .entry(interval_secs)
            .or_insert_with(|| PrivilegeBucket {
                items: Vec::new(),
                due: now + Duration::from_secs(interval_secs),
            })
            .items
            .push(item);
    }

    fn next_due(&self) -> Option<tokio::time::Instant> {
        self.buckets.values().map(|b| b.due).min()
    }

    #[cfg(test)]
    fn intervals(&self) -> Vec<u64> {
        self.buckets.keys().copied().collect()
    }

    fn take_ready(&mut self, now: tokio::time::Instant) -> Vec<(u64, Vec<T>)> {
        let keys: Vec<u64> = self
            .buckets
            .iter()
            .filter(|(_, b)| b.due <= now)
            .map(|(&k, _)| k)
            .collect();
        keys.into_iter()
            .filter_map(|k| self.buckets.remove(&k).map(|b| (k, b.items)))
            .collect()
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.buckets.values().map(|b| b.items.len()).sum()
    }
}

struct PrivilegeHeld<T> {
    item: T,
    caller: CallerIdentity,
}

struct PrivilegeFireInner<T> {
    buf: Mutex<PrivilegeDebounce<PrivilegeHeld<T>>>,
    notify: tokio::sync::Notify,
    abort: Mutex<Option<tokio::task::AbortHandle>>,
}

impl<T> Drop for PrivilegeFireInner<T> {
    fn drop(&mut self) {
        if let Some(handle) = self.abort.lock().unwrap().take() {
            handle.abort();
        }
    }
}

/// 権限デバウンスの core 側ランタイム。バッファと時限タスクを内包する。
///
/// ゲートは寿命を合わせ、時限到達で渡すクロージャ（ターン起動）だけを渡す。
/// `next_due` / 保留 / 間隔はゲートに出さない。
pub struct PrivilegeFire<T> {
    inner: Arc<PrivilegeFireInner<T>>,
}

impl<T> Clone for PrivilegeFire<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T: Send + 'static> PrivilegeFire<T> {
    /// `on_due` は間隔到達時に core が呼ぶ。再 `accept_inbound` ではない。
    /// 渡す件がそのターンの文脈（読んだ事実）。ゲートはここで 👀 を付ける。
    pub fn new<F, Fut>(on_due: F) -> Self
    where
        F: Fn(Vec<(T, CallerIdentity)>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let inner = Arc::new(PrivilegeFireInner {
            buf: Mutex::new(PrivilegeDebounce::default()),
            notify: tokio::sync::Notify::new(),
            abort: Mutex::new(None),
        });
        let on_due = Arc::new(on_due);
        let worker = Arc::clone(&inner);
        let handle = tokio::spawn(async move {
            loop {
                let due = worker.buf.lock().unwrap().next_due();
                match due {
                    None => worker.notify.notified().await,
                    Some(at) => {
                        tokio::select! {
                            _ = tokio::time::sleep_until(at) => {
                                let groups = worker
                                    .buf
                                    .lock()
                                    .unwrap()
                                    .take_ready(tokio::time::Instant::now());
                                for (_interval, held) in groups {
                                    let items: Vec<(T, CallerIdentity)> = held
                                        .into_iter()
                                        .map(|h| (h.item, h.caller))
                                        .collect();
                                    if !items.is_empty() {
                                        on_due(items).await;
                                    }
                                }
                            }
                            _ = worker.notify.notified() => {}
                        }
                    }
                }
            }
        });
        *inner.abort.lock().unwrap() = Some(handle.abort_handle());
        Self { inner }
    }

    fn hold(&self, item: T, caller: CallerIdentity, interval_secs: u64) {
        self.inner.buf.lock().unwrap().push(
            PrivilegeHeld { item, caller },
            interval_secs,
            tokio::time::Instant::now(),
        );
        self.inner.notify.notify_one();
    }
}

impl<T> PrivilegeFire<T> {
    #[cfg(test)]
    fn held_intervals(&self) -> Vec<u64> {
        self.inner.buf.lock().unwrap().intervals()
    }

    #[cfg(test)]
    fn held_len(&self) -> usize {
        self.inner.buf.lock().unwrap().len()
    }
}

/// 配送 effect（§3.4）。ゲートはこれを既存の送信・リアクションで出す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryEffect {
    Text {
        body: String,
        stopped_by_limit: bool,
        tool_calls_made: usize,
        iterations: usize,
    },
    NoReply,
    Empty,
    Failed {
        error: String,
    },
}

/// `EngineResult` を §3.4 の配送 effect に写す。NO_REPLY 終端解釈（第一柱）はここに集約。
///
/// R4: `NO_REPLY` は**出現＝終端**。最初の `NO_REPLY` で発言を打ち切り、前段が空なら
/// [`DeliveryEffect::NoReply`]、非空ならその前段のみを [`DeliveryEffect::Text`] にする。
/// `NO_REPLY` の後に非空テキストが続いていた場合は `ctx` を相関キーに破棄ログ（§3.1.1）を残す。
pub fn delivery_effect(
    result: anyhow::Result<EngineResult>,
    ctx: crate::no_reply::DeliveryContext<'_>,
) -> DeliveryEffect {
    match result {
        Ok(er) if !er.response.is_empty() => {
            // NO_REPLY 終端（第一柱）→ CONTINUE 末尾剥がし（#890 §11）を 1 経路で確定。
            match crate::continue_marker::visible_speech_after_markers(&er.response, ctx) {
                None => DeliveryEffect::NoReply,
                Some(body) => DeliveryEffect::Text {
                    body,
                    stopped_by_limit: er.stopped_by_limit,
                    tool_calls_made: er.tool_calls_made,
                    iterations: er.iterations,
                },
            }
        }
        Ok(_) => DeliveryEffect::Empty,
        Err(e) => DeliveryEffect::Failed {
            error: format!("{e:#}"),
        },
    }
}

/// DM の事前ゲート（「いずれかのエージェントが信頼していれば通す」）。
///
/// [`accept_inbound`] が [`InboundLookups::dm_allowed_any`] の結果を渡す。非 DM は常に通す。
///
/// ゲートは呼ばない。[`accept_inbound`] が内部で使う。
fn admit_inbound_message(is_dm: bool, dm_allowed_any: bool) -> Result<(), InboundMessageDrop> {
    if is_dm && !dm_allowed_any {
        Err(InboundMessageDrop::DmNotTrusted)
    } else {
        Ok(())
    }
}

/// エージェント個別の権限ゲート（DM 個別信頼 / チャンネル whitelist）。
///
/// ゲートは呼ばない。[`accept_inbound`] が内部で使う。
fn admit_inbound_agent(
    is_dm: bool,
    dm_allowed: bool,
    channel_whitelisted: bool,
) -> Result<(), InboundAgentDrop> {
    if is_dm {
        if !dm_allowed {
            return Err(InboundAgentDrop::DmNotTrustedForAgent);
        }
        return Ok(());
    }
    if !channel_whitelisted {
        return Err(InboundAgentDrop::ChannelNotWhitelisted);
    }
    Ok(())
}

fn admit_one(
    lookups: &InboundLookups<'_>,
    event: &NormalizedInboundEvent<'_>,
    owner_id: &str,
    agent_ids: &[String],
) -> Result<AdmittedInbound, InboundMessageDrop> {
    let is_dm = event.guild_id.is_empty();
    admit_inbound_message(
        is_dm,
        (lookups.dm_allowed_any)(event.sender_id, agent_ids, owner_id),
    )?;
    let caller = (lookups.resolve_caller)(event.sender_id, agent_ids, owner_id);
    let mut admitted_agent_ids = Vec::new();
    let mut agent_drops = Vec::new();
    for agent_id in agent_ids {
        match admit_inbound_agent(
            is_dm,
            (lookups.dm_allowed)(event.sender_id, agent_id, owner_id),
            (lookups.channel_whitelisted)(event.channel_id, agent_id),
        ) {
            Ok(()) => admitted_agent_ids.push(agent_id.clone()),
            Err(reason) => agent_drops.push((agent_id.clone(), reason)),
        }
    }
    Ok(AdmittedInbound {
        caller,
        admitted_agent_ids,
        agent_drops,
    })
}

/// 唯一の inbound 入り口。ゲートは正規化イベントの束を 1 回投げる。
///
/// - 対話系（`watch` 無し）: trust 分割（Q13）。`on_admitted` は通した件、`on_run` はトリガー。
///   `on_run` の第 3 引数は、そのターンの文脈に含めた受信の元 index（record-only を含む）。
/// - watch 即時（`privilege` あり）: 許可集合・standing・権限デバウンス。抱えた件は
///   [`PrivilegeFire`] が時限発火する（callback しない）。発火時の件が文脈。
/// - watch 束 flush（`privilege` 無し）: 許可集合。通した最後だけ `on_run`（文脈は通した全件）。
///
/// `take_hold` は権限デバウンスに抱えるときだけ呼ばれる。
#[allow(clippy::too_many_arguments)]
pub fn accept_inbound<T: Send + 'static>(
    items: &[InboundWork<'_>],
    owner_id: &str,
    agent_ids: &[String],
    lookups: &InboundLookups<'_>,
    watch: Option<WatchAccept<'_, T>>,
    mut take_hold: impl FnMut(usize) -> T,
    mut on_admitted: impl FnMut(usize, &AdmittedInbound),
    mut on_run: impl FnMut(usize, &AdmittedInbound, &[usize]),
) -> Result<(), InboundDrop> {
    let mut admitted: Vec<(usize, AdmittedInbound)> = Vec::new();
    let mut trust_at: Vec<Option<u8>> = vec![None; items.len()];
    for (i, item) in items.iter().enumerate() {
        if let Some(w) = watch.as_ref() {
            if !w.allow.is_allowed(item.author_key) {
                continue;
            }
        }
        let plan = match admit_one(lookups, &item.event, owner_id, agent_ids) {
            Ok(p) => p,
            Err(drop) => {
                if items.len() == 1 && watch.is_none() {
                    return Err(InboundDrop::Message(drop));
                }
                if watch.is_none() {
                    trust_at[i] = Some(
                        (lookups.resolve_caller)(item.event.sender_id, agent_ids, owner_id)
                            .trust_level(),
                    );
                }
                continue;
            }
        };
        trust_at[i] = Some(plan.caller.trust_level());
        if let Some(w) = watch.as_ref() {
            if let Some(fire) = w.privilege {
                let standing = watch_author_standing(item.author_key, w.owner, w.followees);
                let hold = watch_hold_interval_secs(
                    w.policy_json,
                    standing,
                    &plan.caller,
                    item.kind_label,
                    w.interval_secs,
                )
                .map_err(InboundDrop::Policy)?;
                if let Some(secs) = hold {
                    fire.hold(take_hold(i), plan.caller.clone(), secs);
                    continue;
                }
            }
        }
        admitted.push((i, plan));
    }

    let run_flags = if watch.as_ref().is_some_and(|w| w.privilege.is_none()) {
        let mut flags = vec![false; admitted.len()];
        if let Some(last) = flags.last_mut() {
            *last = true;
        }
        flags
    } else if watch.is_none() {
        let levels: Vec<u8> = trust_at
            .iter()
            .map(|t| t.expect("対話系の全件は admit または DM drop で trust を書いている"))
            .collect();
        let has_content: Vec<bool> = items.iter().map(|item| item.has_content).collect();
        let record_only = plan_record_only_flags(&levels, &has_content);
        admitted.iter().map(|(i, _)| !record_only[*i]).collect()
    } else {
        vec![true; admitted.len()]
    };

    for (j, (i, adm)) in admitted.iter().enumerate() {
        on_admitted(*i, adm);
        if run_flags[j] {
            let read = turn_read_indices(*i, &admitted, watch.as_ref(), &trust_at);
            on_run(*i, adm, &read);
        }
    }
    Ok(())
}

/// ターン 1 本の文脈に含める受信の元 index。
///
/// 対話系: トリガーと同じ連続同権限グループのうち、通した件（record-only 含む）。
/// watch 束 flush: 通した全件。watch 即時: その 1 件。
fn turn_read_indices<T>(
    trigger: usize,
    admitted: &[(usize, AdmittedInbound)],
    watch: Option<&WatchAccept<'_, T>>,
    trust_at: &[Option<u8>],
) -> Vec<usize> {
    if watch.is_some_and(|w| w.privilege.is_none()) {
        return admitted.iter().map(|(i, _)| *i).collect();
    }
    if watch.is_some() {
        return vec![trigger];
    }
    let levels: Vec<u8> = trust_at
        .iter()
        .map(|t| t.expect("対話系の全件は admit または DM drop で trust を書いている"))
        .collect();
    let groups = consecutive_trust_groups(&levels);
    let Some(&(start, len)) = groups
        .iter()
        .find(|&&(s, l)| trigger >= s && trigger < s + l)
    else {
        return vec![trigger];
    };
    let end = start + len;
    admitted
        .iter()
        .map(|(i, _)| *i)
        .filter(|&i| i >= start && i < end)
        .collect()
}

/// 連続した同一 trust_level の並びを `(開始 index, 長さ)` に切る。
///
/// 到着順を保ち、隣が変わったところで切る。移設前の `message_loop::consecutive_groups` と同一。
/// trust_level 分割後のターン本数と各 caller を現行どおり固定する（RULINGS Q13）。
pub fn consecutive_trust_groups(levels: &[u8]) -> Vec<(usize, usize)> {
    let mut groups = Vec::new();
    let mut start = 0;
    while start < levels.len() {
        let mut end = start + 1;
        while end < levels.len() && levels[end] == levels[start] {
            end += 1;
        }
        groups.push((start, end - start));
        start = end;
    }
    groups
}

/// グループごとに「内容のある最後」だけ run トリガー、他は record-only。
///
/// `true` = 記録だけ（run しない）。現行フラッシュの `record_only_flags` と同一。
/// `levels` と `has_content` の長さが違うときは panic する（呼ばれ方の契約。
/// 長さ不一致はバグなので黙って空を返さない）。
pub fn plan_record_only_flags(levels: &[u8], has_content: &[bool]) -> Vec<bool> {
    if levels.len() != has_content.len() {
        panic!(
            "plan_record_only_flags: levels ({}) and has_content ({}) length mismatch",
            levels.len(),
            has_content.len()
        );
    }
    let groups = consecutive_trust_groups(levels);
    let mut record_only = vec![true; levels.len()];
    for &(start, len) in &groups {
        if let Some(rel) = has_content[start..start + len].iter().rposition(|c| *c) {
            record_only[start + rel] = false;
        }
    }
    record_only
}

/// 確保 + inbound 記録の失敗。順序は ensure → record（[`prepare_session_inbound`] と同一）。
#[derive(Debug)]
pub enum PrepareSessionInboundError {
    Ensure(anyhow::Error),
    Record(anyhow::Error),
}

/// セッション確保 + inbound 記録（セッションロックより前・#284）。
///
/// `true` = 記録できた。ゲートは `false` を無視せずエスカレーションする。
#[must_use]
pub fn prepare_session_inbound<R: AgentRuntime>(
    runtime: &R,
    source: TranscriptSource,
    inbound: &NormalizedInbound<'_>,
    theme: &str,
    metadata_json: &str,
    mode: &str,
) -> bool {
    let agent_id = inbound.agent_id.to_string();
    runtime.ensure_session(
        inbound.session_id,
        std::slice::from_ref(&agent_id),
        theme,
        metadata_json,
        mode,
    );
    runtime.record_inbound_message(source, &inbound.as_record())
}

/// [`prepare_session_inbound`] と同じ口（ensure → record）。web はこちら。
///
/// 行の形は呼び出し側（`session_logs` 現行形。`TranscriptSource` は使わない）。
pub fn prepare_session_inbound_write(
    inbound: &NormalizedInbound<'_>,
    ensure: impl FnOnce(&str, &str) -> anyhow::Result<()>,
    record: impl FnOnce(&str, &str, &str, &str) -> anyhow::Result<()>,
) -> Result<(), PrepareSessionInboundError> {
    ensure(inbound.session_id, inbound.agent_id).map_err(PrepareSessionInboundError::Ensure)?;
    record(
        inbound.agent_id,
        inbound.session_id,
        inbound.sender_id,
        inbound.text,
    )
    .map_err(PrepareSessionInboundError::Record)?;
    Ok(())
}

/// inbound ターン起動（直列ロック内）。受信フック + 会話構築 + `run_agent_response`。
///
/// 会話構築に失敗したら `None`（現行どおり run しない）。`Some` は run の
/// `Result` そのもの（成功も失敗もゲートの配送へ渡す）。
pub async fn start_session_turn<R, Wrap, Build>(
    runtime: &R,
    source: TranscriptSource,
    inbound: &NormalizedInbound<'_>,
    system_prompt: &str,
    runtime_context_text: &str,
    wrap_conversation: Wrap,
    build_run: Build,
) -> Option<anyhow::Result<EngineResult>>
where
    R: AgentRuntime,
    Wrap: FnOnce(&str) -> String,
    Build: FnOnce(String) -> RunRequest,
{
    runtime.on_inbound_message(source, inbound.agent_id, &inbound.as_record());
    run_session_turn(
        runtime,
        inbound.session_id,
        inbound.agent_id,
        system_prompt,
        runtime_context_text,
        wrap_conversation,
        build_run,
    )
    .await
}

/// resume / 継続ターン（直列ロック内）。会話構築 + `run_agent_response`。
///
/// inbound フックは呼ばない（受信は既に記録済み。subtask / interaction の現行どおり）。
pub async fn run_session_turn<R, Wrap, Build>(
    runtime: &R,
    session_id: &str,
    agent_id: &str,
    system_prompt: &str,
    runtime_context_text: &str,
    wrap_conversation: Wrap,
    build_run: Build,
) -> Option<anyhow::Result<EngineResult>>
where
    R: AgentRuntime,
    Wrap: FnOnce(&str) -> String,
    Build: FnOnce(String) -> RunRequest,
{
    // #826: fail-loud 予算。既定へは落とさず、一意名（超過は `context_budget_exhausted`）で
    // ログしてこのターンを run しない（`None`）。`system_prompt` / `runtime_context_text` は
    // `wrap_conversation` が前置する実 request と一致させること（呼び出し側の契約）。
    let budget = match runtime.context_budget_tokens(
        agent_id,
        session_id,
        system_prompt,
        runtime_context_text,
    ) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(
                session_id = %session_id,
                agent_id = %agent_id,
                error_name = e.name(),
                "{name}: {e}",
                name = e.name()
            );
            return None;
        }
    };
    let raw = match runtime.build_conversation_string(
        session_id,
        agent_id,
        budget,
        system_prompt,
        runtime_context_text,
    ) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(
                session_id = %session_id,
                agent_id = %agent_id,
                "build_conversation_string failed: {e}"
            );
            return None;
        }
    };
    let conversation = wrap_conversation(&raw);
    Some(runtime.run_agent_response(build_run(conversation)).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CallerIdentity;

    fn levels(cs: &[CallerIdentity]) -> Vec<u8> {
        cs.iter().map(|c| c.trust_level()).collect()
    }

    fn owner() -> CallerIdentity {
        CallerIdentity::Owner
    }
    fn co_agent() -> CallerIdentity {
        CallerIdentity::CoAgent {
            agent_id: "agent-a".to_string(),
        }
    }
    fn external() -> CallerIdentity {
        CallerIdentity::Agent
    }

    /// Q13: 現行 `consecutive_groups` の 4 ケースを core 側で固定する。
    #[test]
    fn trust_groups_match_current_consecutive_privilege_split() {
        assert_eq!(
            consecutive_trust_groups(&levels(&[owner(), external(), co_agent()])),
            vec![(0, 1), (1, 1), (2, 1)],
            "owner→外部→co_agent は 2,0,2 で 3 グループ"
        );
        assert_eq!(
            consecutive_trust_groups(&levels(&[owner(), co_agent(), external()])),
            vec![(0, 2), (2, 1)],
            "owner→co_agent は同権限(2)で合流"
        );
        assert_eq!(
            consecutive_trust_groups(&levels(&[owner(), owner()])),
            vec![(0, 2)],
            "同一 owner 連続は 1 グループ"
        );
        assert_eq!(
            consecutive_trust_groups(&levels(&[external(), external(), owner()])),
            vec![(0, 2), (2, 1)],
            "外部連続は 1 グループ、owner は別"
        );
    }

    /// 内容のある最後だけがトリガー。空グループは run しない。
    #[test]
    fn record_only_flags_trigger_latest_with_content() {
        // 同権限 2 通 + 末尾空 → 2 通目がトリガー（index 1）。
        let flags = plan_record_only_flags(&[1, 1, 1], &[true, true, false]);
        assert_eq!(flags, vec![true, false, true]);

        // 権限が違う 2 通はどちらもトリガー。
        let flags = plan_record_only_flags(&[2, 1], &[true, true]);
        assert_eq!(flags, vec![false, false]);

        // 全部空 → 全部 record-only（run 0）。
        let flags = plan_record_only_flags(&[2, 2], &[false, false]);
        assert_eq!(flags, vec![true, true]);
    }

    #[test]
    fn dm_gate_drops_untrusted_sender() {
        assert_eq!(
            admit_inbound_message(true, false),
            Err(InboundMessageDrop::DmNotTrusted)
        );
        // 非 DM は事前ゲートを見ない。
        assert!(admit_inbound_message(false, false).is_ok());
    }

    #[test]
    fn agent_gate_drops_untrusted_dm_and_non_whitelist() {
        assert_eq!(
            admit_inbound_agent(true, false, true),
            Err(InboundAgentDrop::DmNotTrustedForAgent)
        );
        assert_eq!(
            admit_inbound_agent(false, true, false),
            Err(InboundAgentDrop::ChannelNotWhitelisted)
        );
        assert!(admit_inbound_agent(true, true, false).is_ok());
        assert!(admit_inbound_agent(false, false, true).is_ok());
    }

    fn event<'a>(sender: &'a str, channel: &'a str, guild: &'a str) -> NormalizedInboundEvent<'a> {
        NormalizedInboundEvent {
            sender_id: sender,
            channel_id: channel,
            guild_id: guild,
        }
    }

    fn work<'a>(sender: &'a str, channel: &'a str, guild: &'a str) -> InboundWork<'a> {
        InboundWork {
            event: event(sender, channel, guild),
            has_content: true,
            kind_label: "",
            author_key: sender,
        }
    }

    fn accept_one(
        lookups: &InboundLookups<'_>,
        item: InboundWork<'_>,
        owner: &str,
        agents: &[String],
    ) -> Result<AdmittedInbound, InboundDrop> {
        let mut out = None;
        accept_inbound::<()>(
            &[item],
            owner,
            agents,
            lookups,
            None,
            |_| (),
            |_, adm| out = Some(adm.clone()),
            |_, _, _| {},
        )?;
        Ok(out.expect("通した件は on_admitted される"))
    }

    #[test]
    fn accept_inbound_admits_and_resolves_caller() {
        let caller = CallerIdentity::Owner;
        let resolve = |_: &str, _: &[String], _: &str| caller.clone();
        let lookups = InboundLookups {
            resolve_caller: &resolve,
            dm_allowed_any: &|_, _, _| true,
            dm_allowed: &|_, _, _| true,
            channel_whitelisted: &|_, _| true,
        };
        let plan = accept_one(&lookups, work("u1", "ch", "g1"), "owner", &["a".into()]).unwrap();
        assert_eq!(plan.caller, CallerIdentity::Owner);
        assert_eq!(plan.admitted_agent_ids, vec!["a".to_string()]);
        assert!(plan.agent_drops.is_empty());
    }

    #[test]
    fn accept_inbound_drops_untrusted_dm_per_agent() {
        let caller = CallerIdentity::TrustedUser;
        let resolve = |_: &str, _: &[String], _: &str| caller.clone();
        let lookups = InboundLookups {
            resolve_caller: &resolve,
            dm_allowed_any: &|_, _, _| true,
            dm_allowed: &|_, _, _| false,
            channel_whitelisted: &|_, _| true,
        };
        let plan = accept_one(&lookups, work("u1", "ch", ""), "owner", &["a".into()]).unwrap();
        assert_eq!(
            plan.agent_drop("a"),
            Some(InboundAgentDrop::DmNotTrustedForAgent)
        );
        assert!(plan.admitted_agent_ids.is_empty());
    }

    #[test]
    fn accept_inbound_drops_non_whitelisted_channel() {
        let caller = CallerIdentity::Agent;
        let resolve = |_: &str, _: &[String], _: &str| caller.clone();
        let lookups = InboundLookups {
            resolve_caller: &resolve,
            dm_allowed_any: &|_, _, _| false,
            dm_allowed: &|_, _, _| false,
            channel_whitelisted: &|_, _| false,
        };
        let plan = accept_one(&lookups, work("u1", "ch", "g1"), "owner", &["a".into()]).unwrap();
        assert_eq!(
            plan.agent_drop("a"),
            Some(InboundAgentDrop::ChannelNotWhitelisted)
        );
        assert!(plan.admitted_agent_ids.is_empty());
    }

    fn er(response: &str) -> EngineResult {
        EngineResult {
            response: response.into(),
            iterations: 1,
            tool_calls_made: 2,
            stopped_by_limit: false,
            last_posting_utterance_id: None,
            last_generation_had_continuation_speech: false,
            xml_fallback_parses: 0,
        }
    }

    #[test]
    fn delivery_effect_maps_engine_result() {
        let ctx = crate::no_reply::DeliveryContext::default();
        assert_eq!(
            delivery_effect(Ok(er("hello")), ctx),
            DeliveryEffect::Text {
                body: "hello".into(),
                stopped_by_limit: false,
                tool_calls_made: 2,
                iterations: 1,
            }
        );
        assert_eq!(
            delivery_effect(Ok(er("NO_REPLY")), ctx),
            DeliveryEffect::NoReply
        );
        let empty = EngineResult {
            response: String::new(),
            iterations: 0,
            tool_calls_made: 0,
            stopped_by_limit: false,
            last_posting_utterance_id: None,
            last_generation_had_continuation_speech: false,
            xml_fallback_parses: 0,
        };
        assert_eq!(delivery_effect(Ok(empty), ctx), DeliveryEffect::Empty);
        match delivery_effect(Err(anyhow::anyhow!("boom")), ctx) {
            DeliveryEffect::Failed { error } => assert!(error.contains("boom"), "{error}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// A1（第一柱・終端化）: 本文 → NO_REPLY → ゴミ の応答は前段本文で確定し、
    /// 配送 body に NO_REPLY もゴミも含めない。
    #[test]
    fn delivery_effect_terminates_at_no_reply() {
        let ctx = crate::no_reply::DeliveryContext::default();
        match delivery_effect(Ok(er("本文だけ話す NO_REPLY これはゴミ")), ctx) {
            DeliveryEffect::Text { body, .. } => {
                assert_eq!(body, "本文だけ話す");
                assert!(!body.contains("NO_REPLY"), "body に NO_REPLY 混入: {body}");
                assert!(!body.contains("ゴミ"), "body に破棄テキスト混入: {body}");
            }
            other => panic!("expected Text, got {other:?}"),
        }
        // 前段なし + 後続あり → NoReply（発言はしない）。
        assert_eq!(
            delivery_effect(Ok(er("NO_REPLY 続くゴミ")), ctx),
            DeliveryEffect::NoReply
        );
    }

    /// #890 §11: 末尾 CONTINUE マーカーは配送 body に残さない（継続判定は engine が済ませ、
    /// ここは表示保護）。途中出現は本文のまま残す。
    #[test]
    fn delivery_effect_strips_tail_continue_marker() {
        let ctx = crate::no_reply::DeliveryContext::default();
        match delivery_effect(Ok(er("確認して返すね⚡\nCONTINUE")), ctx) {
            DeliveryEffect::Text { body, .. } => {
                assert_eq!(body, "確認して返すね⚡");
                assert!(!body.contains("CONTINUE"), "body に CONTINUE 混入: {body}");
            }
            other => panic!("expected Text, got {other:?}"),
        }
        // 途中出現は剥がさない。
        match delivery_effect(Ok(er("まず CONTINUE を確認します")), ctx) {
            DeliveryEffect::Text { body, .. } => {
                assert_eq!(body, "まず CONTINUE を確認します");
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    fn web_inbound<'a>(
        session_id: &'a str,
        agent_id: &'a str,
        sender_id: &'a str,
        text: &'a str,
    ) -> NormalizedInbound<'a> {
        NormalizedInbound {
            session_id,
            agent_id,
            sender_id,
            sender_name: "",
            avatar_url: None,
            channel_id: None,
            pubkey: None,
            text,
            image_urls: &[],
            external_id: "",
        }
    }

    /// ensure → record の順。本文・識別子は渡した inbound のまま。
    #[test]
    fn prepare_session_inbound_write_ensures_then_records() {
        let calls = std::sync::Mutex::new(Vec::new());
        let inbound = web_inbound("web-a-c1", "a", "alice", "hi");
        prepare_session_inbound_write(
            &inbound,
            |sid, aid| {
                calls.lock().unwrap().push(format!("ensure:{sid}:{aid}"));
                Ok(())
            },
            |aid, sid, uid, content| {
                calls
                    .lock()
                    .unwrap()
                    .push(format!("record:{aid}:{sid}:{uid}:{content}"));
                Ok(())
            },
        )
        .expect("ensure+record は成功する");
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                "ensure:web-a-c1:a".to_string(),
                "record:a:web-a-c1:alice:hi".to_string(),
            ]
        );
    }

    #[test]
    fn prepare_session_inbound_write_ensure_failure_skips_record() {
        let calls = std::sync::Mutex::new(Vec::new());
        match prepare_session_inbound_write(
            &web_inbound("s", "a", "u", "hi"),
            |sid, aid| {
                calls.lock().unwrap().push(format!("ensure:{sid}:{aid}"));
                Err(anyhow::anyhow!("disk full"))
            },
            |_, _, _, _| {
                calls.lock().unwrap().push("record".into());
                Ok(())
            },
        ) {
            Err(PrepareSessionInboundError::Ensure(e)) => {
                assert!(e.to_string().contains("disk full"), "{e:#}");
            }
            other => panic!("expected Ensure, got {other:?}"),
        }
        assert_eq!(*calls.lock().unwrap(), vec!["ensure:s:a".to_string()]);
    }

    #[test]
    fn prepare_session_inbound_write_record_failure_is_distinct() {
        let calls = std::sync::Mutex::new(Vec::new());
        match prepare_session_inbound_write(
            &web_inbound("s", "a", "u", "hi"),
            |sid, aid| {
                calls.lock().unwrap().push(format!("ensure:{sid}:{aid}"));
                Ok(())
            },
            |aid, sid, uid, content| {
                calls
                    .lock()
                    .unwrap()
                    .push(format!("record:{aid}:{sid}:{uid}:{content}"));
                Err(anyhow::anyhow!("locked"))
            },
        ) {
            Err(PrepareSessionInboundError::Record(e)) => {
                assert!(e.to_string().contains("locked"), "{e:#}");
            }
            other => panic!("expected Record, got {other:?}"),
        }
        assert_eq!(
            *calls.lock().unwrap(),
            vec!["ensure:s:a".to_string(), "record:a:s:u:hi".to_string()]
        );
    }

    #[test]
    fn accept_inbound_drops_untrusted_dm_for_single_item() {
        let caller = CallerIdentity::Agent;
        let resolve = |_: &str, _: &[String], _: &str| caller.clone();
        let lookups = InboundLookups {
            resolve_caller: &resolve,
            dm_allowed_any: &|_, _, _| false,
            dm_allowed: &|_, _, _| false,
            channel_whitelisted: &|_, _| true,
        };
        let work = InboundWork {
            event: event("u1", "ch", ""),
            has_content: true,
            kind_label: "メンション",
            author_key: "u1",
        };
        let err = accept_inbound::<()>(
            &[work],
            "owner",
            &["a".into()],
            &lookups,
            None,
            |_| (),
            |_, _| {},
            |_, _, _| {},
        )
        .unwrap_err();
        assert_eq!(err, InboundDrop::Message(InboundMessageDrop::DmNotTrusted));
    }

    #[test]
    fn accept_inbound_session_split_runs_latest_with_content() {
        let caller = CallerIdentity::TrustedUser;
        let resolve = |_: &str, _: &[String], _: &str| caller.clone();
        let lookups = InboundLookups {
            resolve_caller: &resolve,
            dm_allowed_any: &|_, _, _| true,
            dm_allowed: &|_, _, _| true,
            channel_whitelisted: &|_, _| true,
        };
        let items = [
            InboundWork {
                event: event("u1", "ch", "g1"),
                has_content: true,
                kind_label: "",
                author_key: "u1",
            },
            InboundWork {
                event: event("u2", "ch", "g1"),
                has_content: true,
                kind_label: "",
                author_key: "u2",
            },
            InboundWork {
                event: event("u3", "ch", "g1"),
                has_content: false,
                kind_label: "",
                author_key: "u3",
            },
        ];
        let mut admitted = Vec::new();
        let mut runs = Vec::new();
        let mut reads = Vec::new();
        accept_inbound::<()>(
            &items,
            "owner",
            &["a".into()],
            &lookups,
            None,
            |_| (),
            |i, _| admitted.push(i),
            |i, _, read| {
                runs.push(i);
                reads.push(read.to_vec());
            },
        )
        .unwrap();
        assert_eq!(admitted, vec![0, 1, 2]);
        assert_eq!(runs, vec![1], "同権限 3 通の内容あり最後だけ run");
        assert_eq!(
            reads,
            vec![vec![0, 1, 2]],
            "トリガーのターン文脈は同グループの通した全件（record-only 含む）"
        );
    }

    #[tokio::test]
    async fn accept_inbound_watch_holds_non_immediate() {
        let caller = CallerIdentity::Owner;
        let resolve = |_: &str, _: &[String], _: &str| caller.clone();
        let lookups = InboundLookups {
            resolve_caller: &resolve,
            dm_allowed_any: &|_, _, _| true,
            dm_allowed: &|_, _, _| true,
            channel_whitelisted: &|_, _| true,
        };
        let owner = std::collections::HashSet::from(["aa".to_string()]);
        let followees = std::collections::HashSet::new();
        let empty = std::collections::HashSet::new();
        let allow = WatchAllowSets {
            followees: &followees,
            owner: &owner,
            co_agents: &empty,
            trusted_users: &empty,
        };
        let fire = PrivilegeFire::new(|_items: Vec<(usize, CallerIdentity)>| async {});
        let work = InboundWork {
            event: NormalizedInboundEvent {
                sender_id: "aa",
                channel_id: "nostr-a",
                guild_id: "nostr",
            },
            has_content: true,
            kind_label: "リポスト",
            author_key: "aa",
        };
        let mut held = Vec::new();
        let mut admitted = Vec::new();
        let mut runs = Vec::new();
        accept_inbound(
            &[work],
            "",
            &["a".into()],
            &lookups,
            Some(WatchAccept {
                policy_json: "{}",
                interval_secs: 60,
                allow,
                owner: &owner,
                followees: &followees,
                privilege: Some(&fire),
            }),
            |i| {
                held.push(i);
                i
            },
            |i, _| admitted.push(i),
            |i, _, _| runs.push(i),
        )
        .unwrap();
        assert_eq!(held, vec![0]);
        assert!(admitted.is_empty());
        assert!(runs.is_empty());
        assert_eq!(fire.held_intervals(), vec![60]);
        assert_eq!(fire.held_len(), 1);
    }

    #[test]
    fn privilege_debounce_flushes_each_interval() {
        let mut hold = PrivilegeDebounce::default();
        let now = tokio::time::Instant::now();
        hold.push("fast", 30, now);
        hold.push("slow", 300, now);
        assert_eq!(hold.intervals(), vec![30, 300]);
        let at_watch = hold.take_ready(now + Duration::from_secs(60));
        assert_eq!(at_watch.len(), 1);
        assert_eq!(at_watch[0].0, 30);
        assert_eq!(at_watch[0].1, vec!["fast"]);
        assert_eq!(hold.intervals(), vec![300]);
        let later = hold.take_ready(now + Duration::from_secs(300));
        assert_eq!(later.len(), 1);
        assert_eq!(later[0].0, 300);
        assert_eq!(later[0].1, vec!["slow"]);
        assert!(hold.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn privilege_fire_emits_each_interval() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let fire = PrivilegeFire::new(move |items: Vec<(String, CallerIdentity)>| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(items);
            }
        });
        fire.hold("fast".into(), CallerIdentity::Owner, 30);
        fire.hold("slow".into(), CallerIdentity::Agent, 300);
        tokio::time::advance(Duration::from_secs(30)).await;
        let first = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("30s で fast が発火する")
            .expect("channel open");
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].0, "fast");
        assert_eq!(fire.held_intervals(), vec![300]);
        tokio::time::advance(Duration::from_secs(270)).await;
        let later = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("300s で slow が発火する")
            .expect("channel open");
        assert_eq!(later.len(), 1);
        assert_eq!(later[0].0, "slow");
        assert_eq!(fire.held_len(), 0);
    }

    /// row 116-117: record-only は on_run されない。同グループのトリガーの read にだけ入る。
    #[test]
    fn accept_inbound_record_only_is_read_with_the_trigger_not_alone() {
        let caller = CallerIdentity::TrustedUser;
        let resolve = |_: &str, _: &[String], _: &str| caller.clone();
        let lookups = InboundLookups {
            resolve_caller: &resolve,
            dm_allowed_any: &|_, _, _| true,
            dm_allowed: &|_, _, _| true,
            channel_whitelisted: &|_, _| true,
        };
        let items = [
            InboundWork {
                event: event("u1", "ch", "g1"),
                has_content: true,
                kind_label: "",
                author_key: "u1",
            },
            InboundWork {
                event: event("u2", "ch", "g1"),
                has_content: true,
                kind_label: "",
                author_key: "u2",
            },
        ];
        let mut runs = Vec::new();
        let mut reads = Vec::new();
        accept_inbound::<()>(
            &items,
            "owner",
            &["a".into()],
            &lookups,
            None,
            |_| (),
            |_, _| {},
            |i, _, read| {
                runs.push(i);
                reads.push(read.to_vec());
            },
        )
        .unwrap();
        assert_eq!(runs, vec![1], "内容あり最後だけがトリガー");
        assert_eq!(
            reads,
            vec![vec![0, 1]],
            "record-only の 0 もこのターンで読む"
        );
    }

    /// row 116-117: チャンネル whitelist 落ちは agent_drop。メッセージは通るので
    /// `on_run` の read には入る。👀 を付けないのはゲートが `agent_drop` を見たとき。
    #[test]
    fn accept_inbound_whitelist_drop_is_agent_drop_not_message_drop() {
        let caller = CallerIdentity::Agent;
        let resolve = |_: &str, _: &[String], _: &str| caller.clone();
        let lookups = InboundLookups {
            resolve_caller: &resolve,
            dm_allowed_any: &|_, _, _| true,
            dm_allowed: &|_, _, _| true,
            channel_whitelisted: &|_, _| false,
        };
        let items = [InboundWork {
            event: event("u1", "ch", "g1"),
            has_content: true,
            kind_label: "",
            author_key: "u1",
        }];
        let mut plan = None;
        accept_inbound::<()>(
            &items,
            "owner",
            &["a".into()],
            &lookups,
            None,
            |_| (),
            |_, adm| plan = Some(adm.clone()),
            |_, _, _| {},
        )
        .unwrap();
        let plan = plan.expect("メッセージ自体は通る");
        assert_eq!(
            plan.agent_drop("a"),
            Some(InboundAgentDrop::ChannelNotWhitelisted)
        );
        assert!(plan.admitted_agent_ids.is_empty());
    }
}
