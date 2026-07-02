use anyhow::Result;
use async_trait::async_trait;

use crate::message::{IncomingMessage, OutgoingMessage};

// ============================================
// GatewayActions: ゲートウェイ固有アクション
// ============================================

/// ゲートウェイが提供する固有アクションのトレイト。
///
/// ゲートウェイ（例: Discord）が自身の管理コマンドをエージェントに
/// ツールとして提供するための仕組み。実装はserver crate側で行う。
#[async_trait]
pub trait GatewayActions: Send + Sync {
    /// このゲートウェイが提供するツール定義一覧
    fn definitions(&self) -> Vec<GatewayActionDef>;
    /// ツールを実行する
    async fn execute(&self, name: &str, args: &serde_json::Value) -> GatewayActionResult;
}

/// ゲートウェイアクションの定義
pub struct GatewayActionDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// ゲートウェイアクションの実行結果
pub struct GatewayActionResult {
    pub success: bool,
    pub data: Option<serde_json::Value>,
    pub error: Option<String>,
}

/// ゲートウェイトレイト
///
/// 各I/Oプラットフォーム（REST API、CLI、WebSocket、Discord等）への
/// 統一的なインターフェースを提供する。
///
/// # ライフサイクル
///
/// 1. `connect()` で接続を確立
/// 2. `receive()` で受信メッセージを待ち受け
/// 3. `send()` で応答メッセージを送信
/// 4. `disconnect()` で接続を切断
#[async_trait]
pub trait Gateway: Send + Sync {
    /// ゲートウェイの名前を返す
    fn name(&self) -> &str;

    /// メッセージを受信する（ブロッキング）
    ///
    /// 新しいメッセージが届くまで待機し、受信したメッセージを返す。
    async fn receive(&mut self) -> Result<IncomingMessage>;

    /// メッセージを送信する
    ///
    /// 指定されたターゲットにメッセージを送信する。
    async fn send(&self, message: OutgoingMessage) -> Result<()>;

    /// ゲートウェイに接続する
    ///
    /// 必要な初期化処理（WebSocket接続、Discord Bot起動等）を行う。
    async fn connect(&mut self) -> Result<()>;

    /// ゲートウェイから切断する
    ///
    /// リソースのクリーンアップを行う。
    async fn disconnect(&mut self) -> Result<()>;
}

/// ピアレビュー依頼メッセージのマーカー（プロトコル定数）。
///
/// discord 側のヘッダ組み立てと server 側の system prompt 規約の両方がこれを参照する。
/// 文字列がズレると Silent Reply の例外判定が発火せず、レビューが silent に死ぬため
/// 必ずこの定数を使うこと。
pub const PEER_REVIEW_REQUEST_MARKER: &str = "[Peer Review Request]";
/// ピアレビュー返信メッセージのマーカー（プロトコル定数）。
pub const PEER_REVIEW_REPLY_MARKER: &str = "[Peer Review]";
