//! memory_index 配下のクエリ回帰テスト。
//!
//! - `short_id`: short_id 採番・backfill・short_id/full id 引き
//! - `nodes_fts`: ノード書き込み経路と memory_index_fts の整合、削除の連鎖
//! - `category`: カテゴリ層（ルート・種・割当）とタグ操作
//! - `organize`: スリープ整理ランのマーカーと worklist（新規側・遡り側）
//! - `declared_unit`: 宣言ユニットと read/survey/declare window
//! - `fixtures`: 2 つ以上のモジュールが使う topic ノード投入ヘルパ

use super::*;

mod category;
mod declared_unit;
mod fixtures;
mod nodes_fts;
mod organize;
mod short_id;

use fixtures::*;
