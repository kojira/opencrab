//! OpenCrab の Nostr sub-gateway。
//!
//! Discord ゲートウェイと同型の「外部メッセージ受信 → エージェント実行 → 返信」を
//! Nostr で行う。Nostr プロトコルの実処理は自作 CLI **nostaro** に subprocess で委譲し
//! （[`cli::NostaroCli`]）、OpenCrab は購読・イベント配送・送信ツールの配線を担う。
//!
//! - [`config::NostrConfig`]: リレー（既定 yabu.me / r.kojira.io・ダッシュボードで変更可）
//!   と購読フィルタ（author / keyword / kind）。
//! - [`event::NostrEvent`]: `nostaro watch --json` の1件（JSONL）。
//! - [`actions::NostrGatewayActions`]: `nostr_post`/`reply`/`zap`/`upload` ツール
//!   （`dm` は #514 で撤去 — DM は受信破棄・送信禁止）。
//! - [`key_provisioning::NostrKeyProvisioning`]: 鍵の払い出し capability（#191 段階2）。
//! - [`session::NostrSessionRuntime`]: per-session 直列化ロック + dispatch registry。
//! - [`sink::NostrResponder`]: 応答生成 + 返信配送の共通経路。subtask 完了 sink
//!   （`SubtaskCompletionSink`）も兼ね、完了時に同じ経路で resume して返信する（#168）。
//!
//! 鍵はエージェント毎に `data/agents/{id}/nostr/config.toml` に隔離する
//! （[`cli::NostaroCli::agent_config_path`]、`validate_agent_id` 経由）。
//!
//! nostaro 側の JSON watch インターフェース契約は `docs/nostaro-interface.md`。

pub mod actions;
pub mod adapter;
pub mod binding;
pub mod cli;
pub mod config;
pub mod event;
pub mod fire_descriptor;
pub mod identity;
pub mod ingress;
pub mod key_provisioning;
pub mod manager;
pub mod passthrough;
pub mod provision;
pub mod pubkey;
pub mod runner;
pub mod session;
pub mod shadow;
pub mod sink;
#[cfg(test)]
mod test_support;
pub mod text_delivery;
pub mod watch;

pub use actions::NostrGatewayActions;
pub use adapter::{
    accept_nostr_inbound, admit_nostr_said, history_body_without_anchor, parse_bundle_origins,
    parse_inbound_anchor, parse_v1_anchor, pre_record_drop, transport_route, AdmitSaidError,
    AllowSetStore, AllowSources, DropReason, IngressRoute, V1Anchor,
};
pub use binding::{
    nostr_binding_id, nostr_instance_id, plan_session_bindings, skip_default_loop,
    BindingPlanError, SessionBindingPlan,
};
pub use cli::{
    db_main_key_provider, validate_vanity_prefix, GeneratedKey, MainKeyProvider, MasterKey,
    NostaroCli, MAX_VANITY_PREFIX_LEN,
};
pub use config::{config_from_row, NostrConfig, NostrFilter, DEFAULT_RELAYS};
pub use event::{parse_watch_line, NostrEvent, DM_KINDS};
pub use fire_descriptor::NostrFire;
pub use identity::NostrIdentityAdmin;
pub use ingress::NostrIngress;
pub use key_provisioning::NostrKeyProvisioning;
pub use manager::{NostrGatewayManager, NostrInstanceFn, NostrProvisionFn, NostrReviseFn};
pub use passthrough::NostrPassthrough;
pub use provision::{instance_config_bytes, instance_config_value};
pub use pubkey::{normalize_pubkey, to_npub};
pub use runner::{NostrAgentRunner, NostrGateAllowKeys};
pub use session::{nostr_session_id, NostrSessionRuntime, NOSTR_SESSION_PREFIX};
pub use sink::NostrResponder;
