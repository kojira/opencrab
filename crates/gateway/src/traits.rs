use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::message::{IncomingMessage, OutgoingMessage};

// ============================================
// GatewayActions: ゲートウェイ固有アクション
// ============================================

/// ゲートウェイアクション呼び出し元の型付きアイデンティティ（#36）。
///
/// 以前は bridge が `__caller` 文字列をツール引数 JSON に混ぜ込み、gateway 側が
/// `unwrap_or("agent")` で再解釈していた。LLM 由来の引数と実行コンテキストを
/// 同じ JSON に混ぜない・文字列比較で権限判定しないために型で渡す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayCaller {
    Owner,
    Agent,
    CoAgent { agent_id: String },
    TrustedUser,
}

impl GatewayCaller {
    /// 監査ログ・表示用の正準ラベル（権限判定には enum match を使うこと）。
    pub fn label(&self) -> &'static str {
        match self {
            GatewayCaller::Owner => "owner",
            GatewayCaller::Agent => "agent",
            GatewayCaller::CoAgent { .. } => "co_agent",
            GatewayCaller::TrustedUser => "trusted_user",
        }
    }
}

/// ゲートウェイアクション実行時の呼び出しコンテキスト（#36）。
///
/// bridge（実行境界）が構築し、gateway 実装へ型付きで渡す。ツール引数 JSON には
/// 実行コンテキストを一切混ぜない。
///
/// `Debug` は手実装する（`root_gateway` の trait object が `Debug` を実装しないため
/// derive できない）。
#[derive(Clone)]
pub struct GatewayCallContext {
    pub caller: GatewayCaller,
    /// 呼び出し元エンジンのセッションID。セッション文脈の無い実行（直接呼び出し等）
    /// では None。セッション必須のアクションは None を明示エラーにする（fail-closed）。
    pub session_id: Option<String>,
    /// sub-engine のネスト深さ（メインエンジン = 0）。
    pub depth: u32,
    pub agent_id: String,
    /// この呼び出しを実行している合成 gateway 自身へのハンドル（RFC #152 S2）。
    ///
    /// `spawn_subtask` が sub-engine を構築する際、子（callee）が「自分を包む合成
    /// gateway」（例: `SystemGatewayActions`）を辿れるようにするための注入口。
    /// `BridgedExecutor` が `execute` 呼び出し時に自身の `gateway_actions` を
    /// clone して載せる。自己参照 Arc ではない（Arc は `BridgedExecutor` が保持し、
    /// ctx は短命な借用として運ぶだけ。サイクルは生じない）。
    /// 既定 `None`（後方互換 — 未注入なら従来通り transport gateway 単体で動く）。
    pub root_gateway: Option<Arc<dyn GatewayActions>>,
}

impl std::fmt::Debug for GatewayCallContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayCallContext")
            .field("caller", &self.caller)
            .field("session_id", &self.session_id)
            .field("depth", &self.depth)
            .field("agent_id", &self.agent_id)
            .field(
                "root_gateway",
                &self.root_gateway.as_ref().map(|_| "<gateway>"),
            )
            .finish()
    }
}

impl GatewayCallContext {
    pub fn new(caller: GatewayCaller, agent_id: impl Into<String>) -> Self {
        Self {
            caller,
            session_id: None,
            depth: 0,
            agent_id: agent_id.into(),
            root_gateway: None,
        }
    }

    /// 素の agent 権限のコンテキスト（テスト・最小権限のデフォルト用）。
    pub fn for_agent(agent_id: impl Into<String>) -> Self {
        Self::new(GatewayCaller::Agent, agent_id)
    }

    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    pub fn with_depth(mut self, depth: u32) -> Self {
        self.depth = depth;
        self
    }

    /// 合成 gateway 自身のハンドルを注入する（RFC #152 S2）。
    pub fn with_root_gateway(mut self, root: Arc<dyn GatewayActions>) -> Self {
        self.root_gateway = Some(root);
        self
    }
}

/// ゲートウェイが提供する固有アクションのトレイト。
///
/// ゲートウェイ（例: Discord）が自身の管理コマンドをエージェントに
/// ツールとして提供するための仕組み。実装はserver crate側で行う。
#[async_trait]
pub trait GatewayActions: Send + Sync {
    /// このゲートウェイが提供するツール定義一覧
    fn definitions(&self) -> Vec<GatewayActionDef>;
    /// ツールを実行する。`args` は LLM 由来のツール引数のみ（実行コンテキストは
    /// `ctx` で渡され、JSON には混ざらない）。
    async fn execute(
        &self,
        name: &str,
        args: &serde_json::Value,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult;
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
