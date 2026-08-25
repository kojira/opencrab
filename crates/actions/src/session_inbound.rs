//! セッション inbound の集約口（載せ替え §3）。
//!
//! ゲートは正規化した受信（本文・送信者の生識別子・対象 session_id）だけを渡す。
//! ここで「誰か・権限・セッション確保・記録・ターン起動」を決める。
//! 配送（送信・描画・typing・webhook）はゲートに残す。
//!
//! SkillEngine / conversation の実装は触らない。ゲートが直呼びしていた
//! [`crate::AgentRuntime`] の入口を、このモジュール 1 箇所へ集める。

use crate::agent_runtime::AgentRuntime;
use crate::transcript::{InboundMessageRecord, TranscriptSource};
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

/// DM の事前ゲート（「いずれかのエージェントが信頼していれば通す」）。
///
/// `dm_allowed_any` の**結果**を受け、落とす/通すをここで決める。非 DM は常に通す。
/// 誰を信頼するかの計算（DB）は runner 実装（server）側。
pub fn admit_inbound_message(is_dm: bool, dm_allowed_any: bool) -> Result<(), InboundMessageDrop> {
    if is_dm && !dm_allowed_any {
        Err(InboundMessageDrop::DmNotTrusted)
    } else {
        Ok(())
    }
}

/// エージェント個別の権限ゲート（DM 個別信頼 / チャンネル whitelist）。
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
/// `levels` と `has_content` の長さが違うときは空を返す（呼ばれ方を変えないための
/// 防御ではなく、テストで長さ不一致を落とす）。
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
}
