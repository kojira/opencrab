//! エージェント実行要求（#33）。
//!
//! `run_agent_response` は13個の位置引数を取り、呼び出し側が `None, 0, None, None`
//! を並べる形になっていた（`depth` と `trigger_message_id` の取り違え等が型で
//! 検出できない）。必須項目をコンストラクタ、省略可能項目を builder で受ける
//! 構造体に置き換える。

use std::sync::Arc;

use opencrab_gateway::GatewayActions;

use crate::subtask::{SubtaskCompletionSink, SubtaskRegistry};
use crate::traits::CallerIdentity;

/// 1回のエージェント応答実行の要求。
///
/// `RunRequest::new(...)` が必須項目、`with_*` が省略可能項目。
pub struct RunRequest {
    pub agent_id: String,
    pub agent_name: String,
    pub session_id: String,
    pub system_prompt: String,
    pub conversation: String,
    /// 呼び出し元ゲートウェイ名（"discord" / "rest" / "heartbeat" 等。RuntimeInfo 用）。
    pub gateway: String,
    pub caller: CallerIdentity,
    pub gateway_actions: Option<Arc<dyn GatewayActions>>,
    pub image_urls: Vec<String>,
    /// sub-engine のネスト深さ（メインエンジン = 0）。
    pub depth: u32,
    /// この実行のトリガーになった外部メッセージID（LLM ログの相関用）。
    pub trigger_message_id: Option<String>,
    /// 応答テキスト確定時の即時コールバック（Discord への先行送信等）。
    pub on_response_text: Option<Arc<dyn Fn(String) + Send + Sync>>,
    /// 自動 dispatch（非ブロック / RFC #152 S3a）の完了再注入 sink（gateway 別）。
    /// Some のとき `run_agent_response` は depth0 でメインエンジンへ dispatcher を
    /// 注入し、dispatch 対象ツールを background subtask 化する。None なら従来どおり
    /// 全ツール inline 実行（後方互換・非破壊）。
    pub completion_sink: Option<Arc<dyn SubtaskCompletionSink>>,
    /// dispatch した単一ツール subtask を追跡する registry（cancel/list 用に共有）。
    /// None のとき `run_agent_response` が run 内でフレッシュに生成する。
    pub subtask_registry: Option<SubtaskRegistry>,
    /// この実行を起こした inbound メッセージの返信先（gateway 不透明 token / #167）。
    /// dispatch 有効時に `SubtaskToolDispatcher` → `SpawnedSubtask` へ引き継がれ、
    /// settle 時に `SubtaskSettled.reply_target` として sink へ届く。session_id から
    /// 返信先を復元できない gateway（Nostr など）用。None なら指定なし。
    pub reply_target: Option<String>,
}

impl RunRequest {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        agent_id: impl Into<String>,
        agent_name: impl Into<String>,
        session_id: impl Into<String>,
        system_prompt: impl Into<String>,
        conversation: impl Into<String>,
        gateway: impl Into<String>,
        caller: CallerIdentity,
    ) -> Self {
        Self {
            agent_id: agent_id.into(),
            agent_name: agent_name.into(),
            session_id: session_id.into(),
            system_prompt: system_prompt.into(),
            conversation: conversation.into(),
            gateway: gateway.into(),
            caller,
            gateway_actions: None,
            image_urls: Vec::new(),
            depth: 0,
            trigger_message_id: None,
            on_response_text: None,
            completion_sink: None,
            subtask_registry: None,
            reply_target: None,
        }
    }

    pub fn with_gateway_actions(mut self, ga: Arc<dyn GatewayActions>) -> Self {
        self.gateway_actions = Some(ga);
        self
    }

    pub fn with_image_urls(mut self, urls: Vec<String>) -> Self {
        self.image_urls = urls;
        self
    }

    pub fn with_depth(mut self, depth: u32) -> Self {
        self.depth = depth;
        self
    }

    pub fn with_trigger_message_id(mut self, id: impl Into<String>) -> Self {
        self.trigger_message_id = Some(id.into());
        self
    }

    pub fn with_on_response_text(mut self, cb: Arc<dyn Fn(String) + Send + Sync>) -> Self {
        self.on_response_text = Some(cb);
        self
    }

    /// 非ブロック自動 dispatch（RFC #152 S3a）を有効化する。`sink` は完了再注入の
    /// gateway 別配送口（Discord=LoopEvent / Nostr=reply ...）。`registry` は走行中
    /// subtask の共有 registry（None なら run 内でフレッシュ生成）。
    ///
    /// 「dispatch は有効にしたいが即時の再注入は不要」な経路（heartbeat 等）は
    /// `sink` に `Arc::new(NoopCompletionSink)` を渡せばよい（完了本文は
    /// `settle_completed` が DB へ永続化するので、次 tick の会話再構築で拾える）。
    pub fn with_dispatch(
        mut self,
        registry: Option<SubtaskRegistry>,
        sink: Arc<dyn SubtaskCompletionSink>,
    ) -> Self {
        self.completion_sink = Some(sink);
        self.subtask_registry = registry;
        self
    }

    /// inbound メッセージの返信先（gateway 不透明 token）を渡す（#167）。
    ///
    /// dispatch 有効時（`with_dispatch`）に `SubtaskToolDispatcher` へ引き継がれ、
    /// dispatch した subtask の `SpawnedSubtask.reply_target` に載る。settle 時に
    /// `SubtaskSettled.reply_target` として sink へ届くため、session_id から返信先を
    /// 復元できない gateway（Nostr の event id など）でも完了を返信できる。
    /// Discord のように session_id から復元する gateway は呼ばなくてよい。
    pub fn with_reply_target(mut self, reply_target: impl Into<String>) -> Self {
        self.reply_target = Some(reply_target.into());
        self
    }
}
