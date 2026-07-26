//! A2UI の Discord 側の受け口（#156 S3）。
//!
//! `send_ui` の実体（定義・引数検査・DB 永続化・保留登録簿・タイムアウト監視）は
//! gateway 非依存層（`opencrab_actions::a2ui`）へ移設済み。ここに残るのは
//! **応答の受け口**（`UiResponseSink` の Discord 実装）と、その受け口・描画・登録簿を
//! 束ねた「描画面」（`A2uiSurface`）の構築だけ。
//!
//! 描画の実装は `crate::renderer::DiscordRenderer`（`UiRenderer`）。Form モーダルの
//! 入力欄は保留状態に持たせず、ボタン押下時に部品ツリーから組み直す
//! （`crate::form_modal::resolve_form_modal_for_button`）。

use std::sync::Arc;

use opencrab_core::a2ui::{
    A2uiSurface, PendingUiSurface, UiRenderer, UiResponseEvent, UiResponseSink,
};

use super::DiscordGatewayActions;
use crate::message_loop::LoopEvent;
use crate::renderer::DiscordRenderer;

/// `UiResponseSink` の Discord 実装（`DiscordCompletionSink` と同型）。
///
/// 旧実装は保留状態（`PendingInteraction`）に `UnboundedSender<LoopEvent>` を**直に
/// 埋めて**いたため、「応答をどこへ戻すか」が transport の型そのものになり、保留状態を
/// 汎用層に置けなかった。ここへ移すことで汎用層は `Arc<dyn UiResponseSink>` だけを持つ。
///
/// `guild_id` / `is_dm` を運ばないのは**移設前と同じ挙動**を保つため:
/// 旧 `PendingInteraction` は send_ui 時点で `guild_id = ""` / `is_dm = false` を入れ、
/// タイムアウト経路はその値をそのまま `LoopEvent::InteractionResponse` に載せていた
/// （send_ui 時点では serenity 由来の guild_id を確実に取得できない）。
/// ユーザー操作（クリック・選択・モーダル送信）の経路は serenity のインタラクション
/// 由来の `guild_id` を使うため、この sink ではなく `message_loop` 内で直接
/// `LoopEvent` を送る（Discord 層から Discord 層への配送で、抽象を通す必要がない）。
pub(crate) struct DiscordUiResponseSink {
    pub event_tx: tokio::sync::mpsc::UnboundedSender<LoopEvent>,
}

impl UiResponseSink for DiscordUiResponseSink {
    fn on_ui_response(&self, ev: UiResponseEvent) {
        // Discord のチャンネル識別子は数値。旧実装（`channel_id.parse().unwrap_or(0)`）
        // と同じフォールバックを保つ。
        let channel_id: u64 = ev.target.channel_id.parse().unwrap_or(0);
        let _ = self.event_tx.send(LoopEvent::InteractionResponse {
            interaction_id: ev.interaction_id,
            session_id: ev.session_id,
            agent_id: ev.agent_id,
            channel_id,
            channel_id_str: ev.target.channel_id,
            // send_ui 時点では guild_id を確実に取得できないため空（旧 PendingInteraction
            // の既定と同じ）。
            guild_id: String::new(),
            response: ev.response,
            is_dm: false,
        });
    }
}

impl DiscordGatewayActions {
    /// この gateway が提供する A2UI 描画面を組む。
    ///
    /// `pending` は登録簿と受け口が**両方**揃ったときだけ `Some`。共有（TOML）
    /// ゲートウェイは A2UI 登録簿を配線しない（`run_discord_loop` に
    /// `pending_registry = None` を渡す）ため、そこでは描画のみ行われる
    /// ＝移設前に `pending_interaction_registry` が `None` だったときと同じ挙動。
    pub(super) fn build_a2ui_surface(&self) -> A2uiSurface {
        let renderer: Arc<dyn UiRenderer> = Arc::new(DiscordRenderer::new(self.http.clone()));
        let pending = match (&self.pending_interaction_registry, &self.event_tx) {
            (Some(registry), Some(event_tx)) => Some(PendingUiSurface {
                registry: registry.clone(),
                sink: Arc::new(DiscordUiResponseSink {
                    event_tx: event_tx.clone(),
                }),
            }),
            _ => None,
        };
        A2uiSurface {
            renderer,
            platform: "discord".to_string(),
            // owner の Discord ユーザーID は gateway action が保持する値を使う。
            // （args 経由では注入されないため、以前は常に空文字で owner 判定が無効化されていた）
            owner_id: self.owner_discord_id.clone(),
            pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencrab_core::a2ui::{A2uiUserAction, RenderTarget};
    use tokio::sync::mpsc;

    fn event(channel_id: &str) -> UiResponseEvent {
        UiResponseEvent {
            interaction_id: "i1".into(),
            session_id: "discord-a1-111-222".into(),
            agent_id: "a1".into(),
            target: RenderTarget {
                channel_id: channel_id.to_string(),
                platform: "discord".into(),
            },
            response: A2uiUserAction {
                surface_id: "interaction:i1".into(),
                component_id: "_timeout".into(),
                action_name: "timeout".into(),
                context: None,
                responder_id: "system".into(),
            },
        }
    }

    #[test]
    fn sink_sends_interaction_response_loop_event() {
        let (tx, mut rx) = mpsc::unbounded_channel::<LoopEvent>();
        let sink = DiscordUiResponseSink { event_tx: tx };
        sink.on_ui_response(event("222"));
        match rx.try_recv().expect("event") {
            LoopEvent::InteractionResponse {
                interaction_id,
                session_id,
                agent_id,
                channel_id,
                channel_id_str,
                guild_id,
                response,
                is_dm,
            } => {
                assert_eq!(interaction_id, "i1");
                assert_eq!(session_id, "discord-a1-111-222");
                assert_eq!(agent_id, "a1");
                assert_eq!(channel_id, 222);
                assert_eq!(channel_id_str, "222");
                // 移設前の既定（send_ui 時点では不明）。
                assert_eq!(guild_id, "");
                assert!(!is_dm);
                assert_eq!(response.action_name, "timeout");
            }
            _ => panic!("unexpected loop event"),
        }
    }

    /// 数値でないチャンネル識別子は 0 へ落とす（旧 `parse().unwrap_or(0)` と同じ）。
    #[test]
    fn sink_falls_back_to_zero_for_non_numeric_channel() {
        let (tx, mut rx) = mpsc::unbounded_channel::<LoopEvent>();
        let sink = DiscordUiResponseSink { event_tx: tx };
        sink.on_ui_response(event("not-a-number"));
        match rx.try_recv().expect("event") {
            LoopEvent::InteractionResponse { channel_id, .. } => assert_eq!(channel_id, 0),
            _ => panic!("unexpected loop event"),
        }
    }

    #[test]
    fn surface_has_pending_only_when_registry_and_event_tx_are_wired() {
        let db = opencrab_db::Db::memory().unwrap();
        let http = Arc::new(serenity::http::Http::new("dummy-token"));
        let base = DiscordGatewayActions::new(http, db, "/tmp".to_string(), None)
            .with_owner_discord_id("o");

        // 登録簿も event_tx も無い（描画のみ）。
        let s = base.build_a2ui_surface();
        assert!(s.pending.is_none());
        assert_eq!(s.platform, "discord");
        assert_eq!(s.owner_id, "o");

        // event_tx だけ（共有 TOML ゲートウェイ）→ 描画のみ。
        let (tx, _rx) = mpsc::unbounded_channel::<LoopEvent>();
        let only_tx = base.clone().with_event_tx(tx.clone());
        assert!(only_tx.build_a2ui_surface().pending.is_none());

        // 両方（per-agent ゲートウェイ）→ 保留登録できる。
        let registry: opencrab_core::a2ui::PendingInteractionRegistry =
            Arc::new(dashmap::DashMap::new());
        let both = base.with_a2ui(registry, tx);
        assert!(both.build_a2ui_surface().pending.is_some());
    }
}
