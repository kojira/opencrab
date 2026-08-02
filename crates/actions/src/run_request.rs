//! エージェント実行要求（#33）。
//!
//! `run_agent_response` は13個の位置引数を取り、呼び出し側が `None, 0, None, None`
//! を並べる形になっていた（`depth` と `trigger_message_id` の取り違え等が型で
//! 検出できない）。必須項目をコンストラクタ、省略可能項目を builder で受ける
//! 構造体に置き換える。

use std::sync::Arc;

use opencrab_gateway::GatewayActions;

use crate::subtask::{SubtaskCompletionSink, SubtaskRegistry};
use crate::subtask_notify::SubtaskRunNotifier;
use crate::traits::CallerIdentity;

/// 走行中注入（#289）の対象範囲（#323 / B2）。
///
/// #289 の走行中注入はターン開始後に届いたユーザー発言を、走っているターンの入力へ
/// 差分で足す。session スコープ（`speaker_id != agent_id`）で引くため、ゲートウェイ既定は
/// 「自分以外の全発言」（[`Self::AllOthers`]）。
///
/// Nostr は #323 で **1 エージェント = 1 セッション**になり、全ての相手が 1 本の履歴に
/// 同居する。ここで走行中注入を session スコープのまま使うと、`reply_target`（返信先）を
/// 相手 A のノートに固定したターンの最中に相手 B の新着が注入され、**B に答えた本文が
/// A のノートへの返信として公開リレーへ飛ぶ**（誤爆）。旧規約（`nostr-{agent}-{pubkey}`）
/// では session が相手ごとに分かれていたため「同じ相手の連投」しか注入されず、注入内容と
/// 返信先が常に一致していた。その性質を範囲で復元する:
/// - [`Self::OnlySpeaker`]: inbound は返信中の相手（`event.pubkey`）の連投だけ注入する
///   （旧 per-相手 挙動の復元）。
/// - [`Self::Silent`]: resume は生きた相手の識別子を持たない（`SubtaskSettled` に相手
///   pubkey は載っていない）ので、何も注入しない。相手の連投は転記済みで、次の inbound
///   ターンで自然に拾われるため取りこぼしにはならない。
#[derive(Debug, Clone, Default)]
pub enum LiveInboundScope {
    /// 自分（agent_id）以外の全発言を注入する（Discord / heartbeat / REST の既定）。
    #[default]
    AllOthers,
    /// この `speaker_id` の発言だけ注入する（Nostr inbound = 返信中の相手）。
    OnlySpeaker(String),
    /// 何も注入しない（Nostr resume = 生きた相手が不定）。
    Silent,
}

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
    /// サブタスク走行の通知口（#175 S4）。`depth >= 1` の再入実行（sub-engine）で
    /// 使い、走行中のツール呼び出し/結果を進捗として実況し、ツールイベント sink を
    /// executor へ挿す。`None`（既定）なら通知しない。
    ///
    /// depth0 の通常ターンでは使わない（サブタスクの lifecycle 通知は spawn 側が持つ）。
    pub run_notifier: Option<Arc<dyn SubtaskRunNotifier>>,
    /// 走行中注入（#289）の対象範囲（#323 / B2）。既定は [`LiveInboundScope::AllOthers`]
    /// （Discord / heartbeat / REST の従来挙動）。Nostr だけが相手を絞る。
    pub live_inbound_scope: LiveInboundScope,
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
            run_notifier: None,
            live_inbound_scope: LiveInboundScope::AllOthers,
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

    /// サブタスク走行の通知口を渡す（#175 S4）。`with_depth(depth + 1)` と組で使い、
    /// 再入した `run_agent_response` が進捗フックとツールイベント sink を配線する。
    pub fn with_run_notifier(mut self, notifier: Arc<dyn SubtaskRunNotifier>) -> Self {
        self.run_notifier = Some(notifier);
        self
    }

    /// 走行中注入（#289）の対象範囲を指定する（#323 / B2）。既定は `AllOthers`。
    /// Nostr の inbound は返信中の相手（`OnlySpeaker`）に、resume は `Silent` に絞る。
    pub fn with_live_inbound_scope(mut self, scope: LiveInboundScope) -> Self {
        self.live_inbound_scope = scope;
        self
    }
}
