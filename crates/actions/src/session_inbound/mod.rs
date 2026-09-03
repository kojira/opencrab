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

mod admit;
mod debounce;
mod delivery_effect;
mod normalize;
mod turn;

pub use admit::{
    accept_inbound, consecutive_trust_groups, plan_record_only_flags, AdmittedInbound,
    InboundAgentDrop, InboundDrop, InboundLookups, InboundMessageDrop, InboundWork, WatchAccept,
};
pub use debounce::PrivilegeFire;
pub use delivery_effect::{delivery_effect, DeliveryEffect};
pub use normalize::{NormalizedInbound, NormalizedInboundEvent};
pub use turn::{
    prepare_session_inbound, prepare_session_inbound_write, run_session_turn, start_session_turn,
    PrepareSessionInboundError,
};
