//! 経路横断で共有する subtask registry の保持（#169 / 実体は #168 で下位層へ移動）。
//!
//! 実装は gateway 非依存層 [`opencrab_actions::subtask_registries`] にある。
//! server → nostr の依存方向のため、Nostr ゲートウェイからも同じ型を使えるように
//! するには registry 本体（`SubtaskRegistry`）と同じ crate に置く必要があった。
//! ここは既存の呼び出し元（`AppState` / web gateway / REST / heartbeat / E2E）が
//! 使っているパスを保つための再エクスポート。

pub use opencrab_actions::subtask_registries::SubtaskRegistries;
