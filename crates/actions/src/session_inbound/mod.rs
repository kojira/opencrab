//! セッション inbound の集約口（載せ替え §3）。
//!
//! ゲートは正規化した受信（本文・送信者の生識別子・対象 session_id）を渡す。
//! 誰が話してよいかはゲートが決める（D-1099）。配送（送信・描画・typing・webhook）もゲート。
//! core が返すのは [`DeliveryEffect`]（ターンした件）。
//!
//! SkillEngine / conversation の実装は触らない。ゲートが直呼びしていた
//! [`crate::AgentRuntime`] の入口を、このモジュール 1 箇所へ集める。

mod delivery_effect;
mod normalize;
mod turn;

pub use delivery_effect::{delivery_effect, DeliveryEffect};
pub use normalize::NormalizedInbound;
pub use turn::{
    prepare_session_inbound, prepare_session_inbound_write, run_provisional_turn, run_session_turn,
    start_session_turn, PrepareSessionInboundError,
};

#[cfg(test)]
#[path = "tests/mod.rs"]
mod tests;
