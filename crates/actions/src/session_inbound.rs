//! セッション inbound の集約口（載せ替え §3）。
//!
//! ゲートは正規化した受信（本文・送信者の生識別子・対象 session_id）だけを渡す。
//! 誰か・権限は [`plan_inbound`] / [`plan_inbound_flush`] の 1 口で決める。
//! 配送（送信・描画・typing・webhook）はゲートに残す。core が返すのは
//! [`DeliveryEffect`]。
//!
//! SkillEngine / conversation の実装は触らない。ゲートが直呼びしていた
//! [`crate::AgentRuntime`] の入口を、このモジュール 1 箇所へ集める。

use crate::agent_runtime::AgentRuntime;
use crate::transcript::{InboundMessageRecord, TranscriptSource};
use crate::CallerIdentity;
use crate::RunRequest;
use opencrab_core::EngineResult;

/// 誰か・権限の照会口。計算本体（DB）は runner 実装。落とす/通すは
/// [`plan_inbound`] / [`plan_inbound_flush`] が決める。
pub trait InboundIdentity: Send + Sync {
    fn resolve_caller(
        &self,
        sender_id: &str,
        agent_ids: &[String],
        owner_id: &str,
    ) -> CallerIdentity;

    fn dm_allowed_any(&self, sender_id: &str, agent_ids: &[String], owner_id: &str) -> bool;

    fn dm_allowed(&self, sender_id: &str, agent_id: &str, owner_id: &str) -> bool;

    fn is_channel_whitelisted_for_agent(&self, channel_id: &str, agent_id: &str) -> bool;
}

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

/// [`plan_inbound`] が通したときの結果。caller と、各 agent の落とす/通す。
#[derive(Debug, Clone)]
pub struct InboundPlan {
    pub caller: CallerIdentity,
    pub admitted_agent_ids: Vec<String>,
    pub agent_drops: Vec<(String, InboundAgentDrop)>,
}

impl InboundPlan {
    pub fn agent_drop(&self, agent_id: &str) -> Option<InboundAgentDrop> {
        self.agent_drops
            .iter()
            .find(|(id, _)| id == agent_id)
            .map(|(_, reason)| *reason)
    }
}

/// デバウンスフラッシュ時の分割（Q13）。flags とグループ数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlushPlan {
    pub record_only_flags: Vec<bool>,
    pub group_count: usize,
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

/// `EngineResult` を §3.4 の配送 effect に写す。判定はここだけ。
pub fn delivery_effect(result: anyhow::Result<EngineResult>) -> DeliveryEffect {
    match result {
        Ok(er) if !er.response.is_empty() => {
            if er.response.trim() == "NO_REPLY" {
                DeliveryEffect::NoReply
            } else {
                DeliveryEffect::Text {
                    body: er.response,
                    stopped_by_limit: er.stopped_by_limit,
                    tool_calls_made: er.tool_calls_made,
                    iterations: er.iterations,
                }
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
/// [`plan_inbound`] が [`InboundIdentity::dm_allowed_any`] の結果を渡す。非 DM は常に通す。
///
/// ゲートは呼ばない。[`plan_inbound`] が内部で使う。
pub fn admit_inbound_message(is_dm: bool, dm_allowed_any: bool) -> Result<(), InboundMessageDrop> {
    if is_dm && !dm_allowed_any {
        Err(InboundMessageDrop::DmNotTrusted)
    } else {
        Ok(())
    }
}

/// エージェント個別の権限ゲート（DM 個別信頼 / チャンネル whitelist）。
///
/// ゲートは呼ばない。[`plan_inbound`] が内部で使う。
pub fn admit_inbound_agent(
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

/// inbound 1 口。ゲートは生識別子の正規化イベントだけを渡す。
///
/// caller 解決・DM 許可・ホワイトリスト判定はここで行う。
pub fn plan_inbound<I: InboundIdentity>(
    identity: &I,
    event: &NormalizedInboundEvent<'_>,
    owner_id: &str,
    agent_ids: &[String],
) -> Result<InboundPlan, InboundMessageDrop> {
    let is_dm = event.guild_id.is_empty();
    admit_inbound_message(
        is_dm,
        identity.dm_allowed_any(event.sender_id, agent_ids, owner_id),
    )?;
    let caller = identity.resolve_caller(event.sender_id, agent_ids, owner_id);
    let mut admitted_agent_ids = Vec::new();
    let mut agent_drops = Vec::new();
    for agent_id in agent_ids {
        match admit_inbound_agent(
            is_dm,
            identity.dm_allowed(event.sender_id, agent_id, owner_id),
            identity.is_channel_whitelisted_for_agent(event.channel_id, agent_id),
        ) {
            Ok(()) => admitted_agent_ids.push(agent_id.clone()),
            Err(reason) => agent_drops.push((agent_id.clone(), reason)),
        }
    }
    Ok(InboundPlan {
        caller,
        admitted_agent_ids,
        agent_drops,
    })
}

/// デバウンスフラッシュの分割 1 口。ゲートは送信者の生識別子列だけを渡す。
pub fn plan_inbound_flush<I: InboundIdentity>(
    identity: &I,
    sender_ids: &[&str],
    has_content: &[bool],
    agent_ids: &[String],
    owner_id: &str,
) -> FlushPlan {
    let levels: Vec<u8> = sender_ids
        .iter()
        .map(|id| {
            identity
                .resolve_caller(id, agent_ids, owner_id)
                .trust_level()
        })
        .collect();
    FlushPlan {
        record_only_flags: plan_record_only_flags(&levels, has_content),
        group_count: consecutive_trust_groups(&levels).len(),
    }
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

/// web の確保・記録口。行の形は実装側が持つ（`TranscriptSource` は使わない）。
///
/// [`prepare_session_inbound`] と同じ順序（ensure → record、ロック前・#284）。
pub trait SessionInboundWrite {
    fn ensure_web_session(&self, session_id: &str, agent_id: &str) -> anyhow::Result<()>;
    fn record_user_message(
        &self,
        agent_id: &str,
        session_id: &str,
        user_id: &str,
        content: &str,
    ) -> anyhow::Result<()>;
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
/// 行の形は [`SessionInboundWrite`]（`session_logs` 現行形。`TranscriptSource` は使わない）。
pub fn prepare_session_inbound_write<W: SessionInboundWrite>(
    writer: &W,
    inbound: &NormalizedInbound<'_>,
) -> Result<(), PrepareSessionInboundError> {
    writer
        .ensure_web_session(inbound.session_id, inbound.agent_id)
        .map_err(PrepareSessionInboundError::Ensure)?;
    writer
        .record_user_message(
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
    wrap_conversation: Wrap,
    build_run: Build,
) -> Option<anyhow::Result<EngineResult>>
where
    R: AgentRuntime,
    Wrap: FnOnce(&str) -> String,
    Build: FnOnce(String) -> RunRequest,
{
    let budget = runtime.context_budget_tokens(agent_id);
    let raw = match runtime.build_conversation_string(session_id, agent_id, budget) {
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

    struct MockIdentity {
        dm_any: bool,
        dm: bool,
        wl: bool,
        caller: CallerIdentity,
    }

    impl InboundIdentity for MockIdentity {
        fn resolve_caller(
            &self,
            _sender_id: &str,
            _agent_ids: &[String],
            _owner_id: &str,
        ) -> CallerIdentity {
            self.caller.clone()
        }
        fn dm_allowed_any(&self, _sender_id: &str, _agent_ids: &[String], _owner_id: &str) -> bool {
            self.dm_any
        }
        fn dm_allowed(&self, _sender_id: &str, _agent_id: &str, _owner_id: &str) -> bool {
            self.dm
        }
        fn is_channel_whitelisted_for_agent(&self, _channel_id: &str, _agent_id: &str) -> bool {
            self.wl
        }
    }

    fn event<'a>(sender: &'a str, channel: &'a str, guild: &'a str) -> NormalizedInboundEvent<'a> {
        NormalizedInboundEvent {
            sender_id: sender,
            channel_id: channel,
            guild_id: guild,
        }
    }

    #[test]
    fn plan_inbound_drops_untrusted_dm_at_message_gate() {
        let id = MockIdentity {
            dm_any: false,
            dm: false,
            wl: true,
            caller: CallerIdentity::Agent,
        };
        assert_eq!(
            plan_inbound(&id, &event("u1", "ch", ""), "owner", &["a".into()]).unwrap_err(),
            InboundMessageDrop::DmNotTrusted
        );
    }

    #[test]
    fn plan_inbound_admits_and_resolves_caller() {
        let id = MockIdentity {
            dm_any: true,
            dm: true,
            wl: true,
            caller: CallerIdentity::Owner,
        };
        let plan = plan_inbound(&id, &event("u1", "ch", "g1"), "owner", &["a".into()]).unwrap();
        assert_eq!(plan.caller, CallerIdentity::Owner);
        assert_eq!(plan.admitted_agent_ids, vec!["a".to_string()]);
        assert!(plan.agent_drops.is_empty());
    }

    #[test]
    fn plan_inbound_drops_untrusted_dm_per_agent() {
        let id = MockIdentity {
            dm_any: true,
            dm: false,
            wl: true,
            caller: CallerIdentity::TrustedUser,
        };
        let plan = plan_inbound(&id, &event("u1", "ch", ""), "owner", &["a".into()]).unwrap();
        assert_eq!(
            plan.agent_drop("a"),
            Some(InboundAgentDrop::DmNotTrustedForAgent)
        );
        assert!(plan.admitted_agent_ids.is_empty());
    }

    #[test]
    fn plan_inbound_drops_non_whitelisted_channel() {
        let id = MockIdentity {
            dm_any: false,
            dm: false,
            wl: false,
            caller: CallerIdentity::Agent,
        };
        let plan = plan_inbound(&id, &event("u1", "ch", "g1"), "owner", &["a".into()]).unwrap();
        assert_eq!(
            plan.agent_drop("a"),
            Some(InboundAgentDrop::ChannelNotWhitelisted)
        );
        assert!(plan.admitted_agent_ids.is_empty());
    }

    #[test]
    fn plan_inbound_flush_matches_record_only_flags() {
        let id = MockIdentity {
            dm_any: true,
            dm: true,
            wl: true,
            caller: CallerIdentity::TrustedUser,
        };
        let flush = plan_inbound_flush(
            &id,
            &["u1", "u2", "u3"],
            &[true, true, false],
            &["a".into()],
            "owner",
        );
        assert_eq!(
            flush.record_only_flags,
            plan_record_only_flags(&[1, 1, 1], &[true, true, false])
        );
        assert_eq!(flush.group_count, 1);
    }

    #[test]
    fn delivery_effect_maps_engine_result() {
        let text = EngineResult {
            response: "hello".into(),
            iterations: 1,
            tool_calls_made: 2,
            stopped_by_limit: false,
            xml_fallback_parses: 0,
        };
        assert_eq!(
            delivery_effect(Ok(text)),
            DeliveryEffect::Text {
                body: "hello".into(),
                stopped_by_limit: false,
                tool_calls_made: 2,
                iterations: 1,
            }
        );
        let no_reply = EngineResult {
            response: "NO_REPLY".into(),
            iterations: 1,
            tool_calls_made: 0,
            stopped_by_limit: false,
            xml_fallback_parses: 0,
        };
        assert_eq!(delivery_effect(Ok(no_reply)), DeliveryEffect::NoReply);
        let empty = EngineResult {
            response: String::new(),
            iterations: 0,
            tool_calls_made: 0,
            stopped_by_limit: false,
            xml_fallback_parses: 0,
        };
        assert_eq!(delivery_effect(Ok(empty)), DeliveryEffect::Empty);
        match delivery_effect(Err(anyhow::anyhow!("boom"))) {
            DeliveryEffect::Failed { error } => assert!(error.contains("boom"), "{error}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    struct WriteSpy {
        ensure_err: Option<String>,
        record_err: Option<String>,
        calls: std::sync::Mutex<Vec<String>>,
    }

    impl SessionInboundWrite for WriteSpy {
        fn ensure_web_session(&self, session_id: &str, agent_id: &str) -> anyhow::Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("ensure:{session_id}:{agent_id}"));
            match &self.ensure_err {
                Some(msg) => Err(anyhow::anyhow!(msg.clone())),
                None => Ok(()),
            }
        }
        fn record_user_message(
            &self,
            agent_id: &str,
            session_id: &str,
            user_id: &str,
            content: &str,
        ) -> anyhow::Result<()> {
            self.calls.lock().unwrap().push(format!(
                "record:{agent_id}:{session_id}:{user_id}:{content}"
            ));
            match &self.record_err {
                Some(msg) => Err(anyhow::anyhow!(msg.clone())),
                None => Ok(()),
            }
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
        let spy = WriteSpy {
            ensure_err: None,
            record_err: None,
            calls: std::sync::Mutex::new(Vec::new()),
        };
        let inbound = web_inbound("web-a-c1", "a", "alice", "hi");
        prepare_session_inbound_write(&spy, &inbound).expect("ensure+record は成功する");
        assert_eq!(
            *spy.calls.lock().unwrap(),
            vec![
                "ensure:web-a-c1:a".to_string(),
                "record:a:web-a-c1:alice:hi".to_string(),
            ]
        );
    }

    #[test]
    fn prepare_session_inbound_write_ensure_failure_skips_record() {
        let spy = WriteSpy {
            ensure_err: Some("disk full".into()),
            record_err: None,
            calls: std::sync::Mutex::new(Vec::new()),
        };
        match prepare_session_inbound_write(&spy, &web_inbound("s", "a", "u", "hi")) {
            Err(PrepareSessionInboundError::Ensure(e)) => {
                assert!(e.to_string().contains("disk full"), "{e:#}");
            }
            other => panic!("expected Ensure, got {other:?}"),
        }
        assert_eq!(*spy.calls.lock().unwrap(), vec!["ensure:s:a".to_string()]);
    }

    #[test]
    fn prepare_session_inbound_write_record_failure_is_distinct() {
        let spy = WriteSpy {
            ensure_err: None,
            record_err: Some("locked".into()),
            calls: std::sync::Mutex::new(Vec::new()),
        };
        match prepare_session_inbound_write(&spy, &web_inbound("s", "a", "u", "hi")) {
            Err(PrepareSessionInboundError::Record(e)) => {
                assert!(e.to_string().contains("locked"), "{e:#}");
            }
            other => panic!("expected Record, got {other:?}"),
        }
        assert_eq!(
            *spy.calls.lock().unwrap(),
            vec!["ensure:s:a".to_string(), "record:a:s:u:hi".to_string()]
        );
    }
}
