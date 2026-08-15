use std::sync::Arc;

use async_trait::async_trait;

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

    /// **co_agent は owner 等価**（#485。オーナー指示 2026-08-10 / #330 を覆す）。
    ///
    /// server 側ハンドラ（configure_* / (add|remove)_allowed_command / ハートビート指示
    /// 更新 / webhook set/disable / voice）の owner ゲートはこの述語を通す。判定本体は
    /// [`opencrab_core::caller::CallerIdentity::is_owner_equivalent`] に委譲し、
    /// **owner 等価の唯一の源**を core 1 箇所に保つ（gateway 側で別途 match しない）。
    pub fn is_owner_equivalent(&self) -> bool {
        opencrab_core::caller::CallerIdentity::from(self).is_owner_equivalent()
    }
}

/// gateway 境界の caller から dispatcher 側の識別子へ戻す（#298）。
///
/// 逆向き（`CallerIdentity` → [`GatewayCaller`]）は
/// `BridgedExecutor::gateway_call_context` が行う。両者は同型なので写像は無損失で、
/// `GatewayCallContext` しか持たないツールハンドラ（`spawn_subtask` / `send_ui`）が
/// 「この run の呼び出し元」をそのまま記録できるようにするために必要。
/// **権限を変換しない**（昇格も降格もしない）。
///
/// 実装がここ（gateway）にあるのは孤児規則のため: `CallerIdentity` は core、
/// `GatewayCaller` は gateway にあるので、両方が外部型になる `opencrab-actions`
/// では書けない。
impl From<&GatewayCaller> for opencrab_core::caller::CallerIdentity {
    fn from(caller: &GatewayCaller) -> Self {
        use opencrab_core::caller::CallerIdentity;
        match caller {
            GatewayCaller::Owner => CallerIdentity::Owner,
            GatewayCaller::Agent => CallerIdentity::Agent,
            GatewayCaller::CoAgent { agent_id } => CallerIdentity::CoAgent {
                agent_id: agent_id.clone(),
            },
            GatewayCaller::TrustedUser => CallerIdentity::TrustedUser,
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
    /// この実行を起こした inbound メッセージの返信先（gateway 不透明 token / #158 S1）。
    ///
    /// `RunRequest.reply_target`（#167）と**同じ不透明トークン**をツール実行の文脈まで
    /// 運ぶ。宛先を引数で受けるアクション（`request_peer_review` 等）が、引数省略時の
    /// フォールバックとして使う。トークンの解釈は各 gateway の責務（Discord は
    /// channel id の数値文字列、Nostr は返信先イベント id）。
    ///
    /// 既定 `None`（後方互換 — 宛先を明示するツール呼び出しは従来どおり動く）。
    /// `None` かつ引数も無い場合は空文字で送らず明示エラーにする（fail-closed）。
    pub reply_target: Option<String>,
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
            .field("reply_target", &self.reply_target)
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
            reply_target: None,
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

    /// inbound メッセージの返信先（gateway 不透明 token）を注入する（#158 S1）。
    ///
    /// `None` を渡した場合は「宛先なし」として扱う（`RunRequest.reply_target` が
    /// `Option<String>` なので、そのまま流し込めるよう `Option` を受ける）。
    pub fn with_reply_target(mut self, reply_target: Option<String>) -> Self {
        self.reply_target = reply_target;
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

    /// この transport が A2UI（`send_ui`）の描画面を提供するなら返す（#156 S3）。
    ///
    /// `send_ui` の**実体は gateway 非依存層**（`opencrab_actions::a2ui`）にあるが、
    /// 描画とユーザー応答の受け取りは transport にしか作れない。合成 gateway
    /// （`SystemGatewayActions`）はこのメソッドで transport の描画面を引き、
    /// **提供する transport のターンでだけ** `send_ui` を露出する。
    ///
    /// 既定は `None`（A2UI を描画できない transport）。
    fn a2ui_surface(&self) -> Option<Arc<opencrab_core::a2ui::A2uiSurface>> {
        None
    }

    /// この transport が素テキストの配送口を提供するなら返す（#157 S7）。
    ///
    /// `request_peer_review` の**実体は gateway 非依存層**
    /// （`crates/server/src/peer_review.rs`）にあるが、宛先検査・メンション記法・
    /// 1 通あたりの上限・送信そのものは transport にしか作れない。合成 gateway
    /// （`SystemGatewayActions`）はこのメソッドで配送口を引き、汎用層へ渡す。
    ///
    /// `a2ui_surface()` と違い、これを提供しない transport でも
    /// `request_peer_review` は**定義に出る**（配送口が無いときだけ実行が明示エラー）。
    /// ツールの露出が transport の有無で消えないようにするのが #157 の目的そのもの。
    ///
    /// 既定は `None`（テキストを送れない transport）。
    fn text_delivery(&self) -> Option<Arc<dyn opencrab_core::text_delivery::TextDelivery>> {
        None
    }
}

/// ツール定義が自ら名乗る分類。
///
/// かつては消費側が名前リスト（gateway 固有の `*_INLINE_ACTIONS` / `*_DISPATCHABLE_ACTIONS`
/// / 拒否リスト・許可リスト）を引いて分類を外部照合していたが、それらを廃し、定義自身に
/// 持たせた（PR-2A で属性を追加、PR-2B で消費側を属性へ切り替え、gateway 固有の定数を削除）。
/// これにより「新しいツールを足したのにリストへ載せ忘れる」ドリフトを、リストのメンテナンス
/// ではなく **構築サイトでの記述の強制** で防ぐ。
///
/// すべて必須（`Default` を持たせない）: 既定値があると新ツールで書き忘れが黙って
/// 通るため。3 フィールドすべてを構築サイトで明示させることが、この型の目的そのもの。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolClass {
    /// 非ブロック dispatch（RFC #152）の対象か否か。
    pub dispatch: DispatchMode,
    /// depth>=1 の sub-engine からの可視性・実行可否。
    pub sub_engine: SubEngineAccess,
    /// このハンドルが会話固有の一時値に縛られるか、エージェント全体で共有できるか。
    pub sharing: ToolSharing,
}

/// 非ブロック dispatch（RFC #152 の「バックグラウンド実行」）の分類。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DispatchMode {
    /// そのターン内で inline 実行する（配送系 / 同ターン結果依存 / 短時間書き込み /
    /// 純粋な読み取り / run 内共有状態）。
    Inline,
    /// 非ブロックで dispatch してよい（長時間 or 同ターンで結果を使わない書き込み）。
    Dispatchable,
}

/// depth>=1 の sub-engine（`spawn_subtask` で起動した子）から見たツールの扱い。
///
/// かつて 2 つの互いに素なリスト（sub-engine 許可リストと深さ拒否リスト）が扱っていた
/// 情報を、1 つの 3 値で無損失に統合する。`SubEngineGatewayActions`（最外周フィルタ）と
/// `BridgedExecutor`（depth ゲート）がこの属性を引く。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubEngineAccess {
    /// sub-engine に見せて実行も許す。現状は `report_progress` / `nostr_generate_key` のみ。
    Allowed,
    /// depth>=1 で明示的に拒否する（多層防御）。配送系（`send_ui` / `request_peer_review` /
    /// discord 送信・VC 参加退出など）。
    Blocked,
    /// 既定。許可リストに載せない（許可・拒否のどちらでもない大多数）。
    /// sub-engine は最外周の allow-list フィルタでこれを見ない。
    NotExposed,
}

/// ツールのハンドルが束縛される範囲。
///
/// 判定基準:
/// **`ConversationBound` = その会話に固有の一時ハンドル（特定のメッセージ ID、受信した
/// 投稿 ID）を必須引数に取る、または対話中の live セッションに束縛される（応答を待つ）
/// ツール。それ以外はすべて `AgentBound`。**
///
/// 2 つ目の条件（live セッション束縛）は必須引数だけを見ても分からない点に注意:
/// `send_ui` の必須引数は `channel_id`（永続）と `components` だけだが、投稿後に
/// ユーザーの応答（クリック等）をそのやりとりの中で待つため `ConversationBound`。
/// 必須引数だけで判断すると `send_ui` 型を `AgentBound` と誤分類する。
///
/// 現時点で `ConversationBound` は次の 3 つだけ:
/// - `discord_add_reaction` — 必須引数 `message_id`（その会話のメッセージ）。
/// - `nostr_reply` — 必須引数 `target`（受信した投稿の note id）。
/// - `send_ui` — 対話中の live セッションに束縛される（応答を待つ）。
///
/// この段階（PR-2A）ではまだ消費者が無い（挙動に影響しない）。後から共有の機構が
/// この属性を読むので、形を今のうちに確定させる。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolSharing {
    /// 会話固有の一時ハンドルを必須引数に取る、または対話中の live セッションに束縛される
    /// （応答を待つ）。実例: リアクションの `message_id` / Nostr 返信の投稿 id /
    /// UI 送信（応答待ち）。
    ConversationBound,
    /// 会話に縛られず、エージェント全体で共有できる。
    AgentBound,
}

/// ゲートウェイアクションの定義
pub struct GatewayActionDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    /// ツールが自ら名乗る分類（dispatch / sub-engine / sharing）。必須（`Default` 無し）。
    pub class: ToolClass,
}

/// ゲートウェイアクションの実行結果
pub struct GatewayActionResult {
    pub success: bool,
    pub data: Option<serde_json::Value>,
    pub error: Option<String>,
}

/// ピアレビュー依頼メッセージのマーカー（プロトコル定数）。
///
/// discord 側のヘッダ組み立てと server 側の system prompt 規約の両方がこれを参照する。
/// 文字列がズレると Silent Reply の例外判定が発火せず、レビューが silent に死ぬため
/// 必ずこの定数を使うこと。
pub const PEER_REVIEW_REQUEST_MARKER: &str = "[Peer Review Request]";
/// ピアレビュー返信メッセージのマーカー（プロトコル定数）。
pub const PEER_REVIEW_REPLY_MARKER: &str = "[Peer Review]";
