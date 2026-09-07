//! 本番配線の同一性テスト（#203 要ビルド検証リスト 3）。
//!
//! 「非ブロック dispatch を有効にしたか」（`completion_sink.is_some()` 等の bool）だけを
//! 見るテストでは、**別インスタンス**の登録簿を run へ渡す壊れ方を検出できない。
//! `cancel_subtask` は「そのセッションの登録簿から subtask を引く」実装なので、run に
//! 別の登録簿が載ると auto-dispatch した subtask が永久に停止できなくなる（Discord の
//! `cancel_subtask` が常に "not found" を返す）。ここでは `Arc::ptr_eq` で
//! **同一実体**であることを固定する。
//!
//! 形は `crates/web-gateway/src/respond.rs` の
//! `run_uses_the_gateways_registry_so_cancel_can_reach_it` に倣う。
//!
//! ファイルを分けている理由: `message_loop.rs` へ変異（mutation）を入れて
//! `git checkout -- crates/discord/src/message_loop.rs` で戻すとき、テストごと
//! 巻き戻さないようにするため。
//!
//! **未固定の範囲**: `handle_agent_response` を直接呼ぶテスト（NO_REPLY の 🤐）は、
//! 呼び出し側が渡す引数（`&discord_message_id_spawn` を `""` に差し替える／`channel_id`
//! を取り違える等）の変異を検出できない。**配線側は未固定。実機確認で補う。**

use std::sync::{Arc, Mutex};

use crate::gateway::DiscordGateway;
use opencrab_actions::subtask::SubtaskRegistry;
use opencrab_actions::{delivery_effect, CallerIdentity, RunRequest, SessionLocks};
use opencrab_core::EngineResult;
use opencrab_gateway::{IncomingMessage, MessageContent, MessageSource, Sender};

use super::{
    create_event_channel, debounce_window_key, incoming_has_content, process_incoming_message,
    process_interaction_response, process_subtask_completed,
};

include!("message_loop_wiring_tests/support.rs");
include!("message_loop_wiring_tests/registry_inbound.rs");
include!("message_loop_wiring_tests/caller_reactions.rs");
include!("message_loop_wiring_tests/record_debounce.rs");
