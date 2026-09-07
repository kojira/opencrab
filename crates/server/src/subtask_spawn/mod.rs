//! `spawn_subtask` の gateway 非依存実装（#175 S4）。
//!
//! 旧実装は Discord ゲートウェイ（`crates/discord` の `execute_spawn_subtask`）にあり、
//! LLM クライアント・既定モデル・ワークスペース・ツール設定を Discord 側に持たせて
//! **sub-engine を自前で組み立てていた**。その構築コードは `process::run_agent_response`
//! の engine 構築とツール登録・許可コマンドのマージ・executor 構築・ログ記録・
//! workspace 解決・モデル解決までほぼ同一で、コメントまで同文だった。
//!
//! ここでは sub-engine を組み立てず、**`process::run_agent_response` を depth+1 で
//! 再入呼び出し**する。差分として明示的に渡すのは次の 4 つだけ:
//!
//! 1. 許可リスト（`SubEngineGatewayActions`）— `run_agent_response` が depth>=1 で自動的に
//!    合成 gateway の最外周へ被せる。
//! 2. `current_purpose = "subtask"` 相当 — 同じく depth から導出される。
//! 3. 通知（`SubtaskRunNotifier` / #175 S3）— `RunRequest::with_run_notifier`。
//! 4. タイムアウト — ここで `tokio::time::timeout` として掛ける。
//!
//! 守る不変条件（壊すと重大。順に対応するテストがある）:
//! - **順序契約**: DB 永続化 → 登録簿から除去 → 完了通知。`settle_completed` を必ず経由する。
//! - **停止の到達性**: spawn した subtask は `cancel_subtask` が引くのと同一の登録簿に入れる。
//! - **開始ゲート**: 登録簿へ insert し終えるまでタスク本体を走らせない。
//! - **ネスト禁止**: sub-engine の許可リストに `spawn_subtask` を含めない。

mod spawn;
mod sub_prompt;
mod timeout_text;

pub use spawn::spawn_subtask;

#[cfg(test)]
mod tests;
