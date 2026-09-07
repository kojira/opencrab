//! 記憶の単位（宣言）道具 4 つ（issue #379 #376 段階1）。
//!
//! エージェントが自分の生ログ（memory_sessions）を**俯瞰**し、**範囲を読み**、まとまりを
//! **宣言**する道具。宣言は `node_type='unit'` / `source_type='declared'` で
//! `memory_index_nodes` に載る（v30 で CHECK 拡張）。既存の time-series topic
//! （`node_type='topic'`）とは別 `node_type` なので、索引ビルド・タグ整理・月次ロールアップの
//! worklist へ**構造的に混ざらない**（#379 監査で確定）。
//!
//! 全て **TRUSTED_ONLY**（`bridge::TRUSTED_ONLY_ACTIONS`）で Nostr（caller=Agent）からは
//! list_tools に出ず dispatch でも拒否される。読み取り 2 つ（survey / read）は整理ラン用の
//! `ORGANIZE_ALLOWED_TOOLS` にも入る。記録 2 つ（record / retract）は段階2 まで入れない。
//!
//! #394 で 5 つ目 `plan_next_memory_window` を足した。宣言ランが 1 回に提示する窓（範囲の
//! 始まりと広さ）を**本人が決める**ための道具で、宣言ランからのみ使う。
//!
//! 有界化（687 発話の塊を一度に吐かせない）: `read_my_history` は行数 + 総文字数の
//! ハードキャップ + カーソル。`survey_my_history` はバケット数に上限。**生ログは読むだけ**。

use async_trait::async_trait;
use serde_json::json;

use crate::traits::{Action, ActionContext, ActionResult};

include!("memory_units/history_read.rs");
include!("memory_units/unit_declaration.rs");
include!("memory_units/memory_core.rs");

#[cfg(test)]
#[path = "memory_units/tests/mod.rs"]
mod tests;
