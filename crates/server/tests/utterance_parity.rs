//! §13.1 g / #904 レビュー非ブロック所見の非回帰ピン: gate-client 各ハンドラの
//! `InvokeHandler::is_utterance` と正典 `opencrab_gateway::is_known_utterance_op`
//! （say|reply|reaction|repost）のパリティ。
//!
//! 契約: (1) 各 gateway が宣言する op のうち正典に含まれるものは全て is_utterance=true・
//! 含まれないものは false（＝ハンドラ判定 == 正典判定）。(2) 各 gateway の発話 op 集合 ⊆ 正典。
//! edc2c478（#904）で is_utterance がハンドラに入って以降で緑（非回帰ピン）。
//!
//! 追加先候補: crates/server/tests/utterance_parity.rs（server は opencrab-gateway・
//! discord-gateway・nostr-gateway・gate-client を全て依存・必要 API は pub）。

use std::path::PathBuf;
use std::sync::Arc;

use opencrab_discord_gateway::ops::{operation_declarations as discord_ops, DiscordInvokeHandler};
use opencrab_discord_gateway::transport::DryRunTransport;
use opencrab_gate_client::InvokeHandler;
use opencrab_gateway::is_known_utterance_op;
use opencrab_nostr_gateway::ops::{operation_declarations as nostr_ops, NostrInvokeHandler};

fn declared_names(v: serde_json::Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|d| d["name"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn gateway_handler_is_utterance_has_parity_with_canonical() {
    // 正典集合の明示ピン（正典側の回帰を捕まえる）。
    for n in ["say", "reply", "reaction", "repost"] {
        assert!(is_known_utterance_op(n), "正典に {n} が含まれない");
    }
    for n in ["resolve", "follow", "unfollow", "kind0", "upload"] {
        assert!(
            !is_known_utterance_op(n),
            "正典に照会/操作 op {n} が誤って含まれる"
        );
    }

    // discord: 宣言 op ごとに handler.is_utterance == 正典。
    let discord = DiscordInvokeHandler::new(Arc::new(DryRunTransport));
    for op in declared_names(discord_ops()) {
        assert_eq!(
            discord.is_utterance(&op),
            is_known_utterance_op(&op),
            "discord op {op}: handler.is_utterance が正典 is_known_utterance_op と不一致"
        );
    }
    let discord_utter: Vec<String> = declared_names(discord_ops())
        .into_iter()
        .filter(|op| discord.is_utterance(op))
        .collect();
    assert_eq!(
        discord_utter,
        vec!["reaction".to_string(), "reply".to_string()],
        "discord の発話 op 集合が {{reply,reaction}} でない"
    );

    // nostr: 宣言 op ごとに handler.is_utterance == 正典。
    let nostr = NostrInvokeHandler::new(
        PathBuf::from("/nonexistent/nostaro"),
        PathBuf::from("/nonexistent/config"),
        None,
        true,
    );
    for op in declared_names(nostr_ops()) {
        assert_eq!(
            nostr.is_utterance(&op),
            is_known_utterance_op(&op),
            "nostr op {op}: handler.is_utterance が正典と不一致"
        );
    }
    let nostr_utter: Vec<String> = declared_names(nostr_ops())
        .into_iter()
        .filter(|op| nostr.is_utterance(op))
        .collect();
    assert_eq!(
        nostr_utter,
        vec![
            "reaction".to_string(),
            "reply".to_string(),
            "repost".to_string()
        ],
        "nostr の発話 op 集合が {{reply,reaction,repost}} でない"
    );

    // ⊆ 正典（発話と名乗る op は全て正典に含まれる）。
    for op in discord_utter.iter().chain(nostr_utter.iter()) {
        assert!(
            is_known_utterance_op(op),
            "発話 op {op} が正典 is_known_utterance_op に含まれない（⊆ 違反）"
        );
    }
}
