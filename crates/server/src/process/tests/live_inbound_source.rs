use super::SessionLiveInbound;
use opencrab_actions::transcript::{InboundMessageRecord, TranscriptSource};
use opencrab_core::LiveInboundSource;

const AGENT: &str = "a1";
const USER: &str = "owner";
const SESSION: &str = "s1";

/// ユーザー発言を本番と同じ書き手（`record_inbound_message`）で入れる。
/// この経路の行は `agent_id` 列＝受信側エージェント / `speaker_id` 列＝送信者（#377）。
/// 述語を `speaker_id != <agent_id 引数>` に合わせてあること（#286）の検査でもある。
fn insert_user_speech(db: &opencrab_db::Db, text: &str) {
    let conn = db.lock().unwrap();
    assert!(
        crate::transcript::record_inbound_message(
            &conn,
            TranscriptSource::Discord,
            &InboundMessageRecord {
                session_id: SESSION,
                recipient_agent_id: AGENT,
                sender_id: USER,
                sender_name: "owner",
                avatar_url: None,
                channel_id: Some("222"),
                pubkey: None,
                text,
                image_urls: &[],
            },
        ),
        "テストの前提: 受信発言が記録できること"
    );
}

fn insert_agent_speech(db: &opencrab_db::Db, text: &str) {
    let conn = db.lock().unwrap();
    opencrab_db::queries::insert_session_log(
        &conn,
        &opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: AGENT.to_string(),
            session_id: SESSION.to_string(),
            log_type: "speech".to_string(),
            content: text.to_string(),
            speaker_id: Some(AGENT.to_string()),
            turn_number: None,
            metadata_json: None,
            created_at: None,
        },
    )
    .unwrap();
}

/// ターン開始後に記録された発言が届く。走行中の注入はここから始まる。
#[test]
fn speech_recorded_during_the_turn_is_delivered() {
    let db = opencrab_db::Db::memory().unwrap();
    insert_user_speech(&db, "調べておいて");
    // ここまでが会話履歴に載っている状態でターンが始まる。
    let source = SessionLiveInbound::new(db.clone(), SESSION, AGENT);

    insert_user_speech(&db, "やめて");

    let out = source.poll_new_messages();
    assert_eq!(out.len(), 1, "新着 1 件だけ: {out:?}");
    assert!(out[0].contains("やめて"), "{}", out[0]);
    assert!(
        out[0].contains("処理している間に届きました"),
        "走行中に届いた事実を添える: {}",
        out[0]
    );
    assert!(
        !out[0].contains("調べておいて"),
        "履歴に載っている発言は再送しない: {}",
        out[0]
    );
}

/// 一度返した発言は二度返さない（毎イテレーション足すとプロンプトが膨らむ）。
#[test]
fn the_same_speech_is_delivered_once() {
    let db = opencrab_db::Db::memory().unwrap();
    let source = SessionLiveInbound::new(db.clone(), SESSION, AGENT);

    insert_user_speech(&db, "やめて");
    assert_eq!(source.poll_new_messages().len(), 1);
    assert!(
        source.poll_new_messages().is_empty(),
        "2 回目の poll では同じ発言を返さない"
    );

    // その後の新着はきちんと拾える（watermark が進んだだけで塞がっていない）。
    insert_user_speech(&db, "やっぱり続けて");
    let out = source.poll_new_messages();
    assert_eq!(out.len(), 1);
    assert!(out[0].contains("やっぱり続けて"), "{}", out[0]);
}

/// 新着が無ければ何も返さない（＝プロンプトは 1 バイトも変わらない）。
#[test]
fn nothing_is_delivered_without_new_speech() {
    let db = opencrab_db::Db::memory().unwrap();
    insert_user_speech(&db, "調べておいて");
    let source = SessionLiveInbound::new(db.clone(), SESSION, AGENT);

    assert!(source.poll_new_messages().is_empty());
}

/// エージェント自身の発言は注入しない（自分の声が入力へ戻ると自己参照ループになる）。
#[test]
fn the_agents_own_speech_is_not_delivered() {
    let db = opencrab_db::Db::memory().unwrap();
    let source = SessionLiveInbound::new(db.clone(), SESSION, AGENT);

    insert_agent_speech(&db, "調べています");

    assert!(source.poll_new_messages().is_empty());
}

/// 特定の話者（pubkey）の受信発言を入れる（#323 / B2）。`speaker_id` 列に入る。
fn insert_speech_from(db: &opencrab_db::Db, speaker: &str, text: &str) {
    let conn = db.lock().unwrap();
    assert!(crate::transcript::record_inbound_message(
        &conn,
        TranscriptSource::Nostr,
        &InboundMessageRecord {
            session_id: SESSION,
            recipient_agent_id: AGENT,
            sender_id: speaker,
            sender_name: speaker,
            avatar_url: None,
            channel_id: None,
            pubkey: Some(speaker),
            text,
            image_urls: &[],
        },
    ));
}

/// [#323 / B2] `OnlySpeaker` は返信中の相手の連投だけ注入し、別相手の新着は落とす。
///
/// 1 セッションに全相手が同居する（#323）ため、無制限だと A への返信ターン中に B の
/// 新着が注入され、B に答えた本文が A への返信として公開リレーへ誤爆する。修正前
/// （`AllOthers` 相当のまま）はこのテストが落ちる（B の発言も返る）。
#[test]
fn only_speaker_scope_injects_only_the_replied_peer() {
    let db = opencrab_db::Db::memory().unwrap();
    let source = SessionLiveInbound::new(db.clone(), SESSION, AGENT).with_scope(
        opencrab_actions::LiveInboundScope::OnlySpeaker("pk-A".to_string()),
    );

    insert_speech_from(&db, "pk-A", "Aの追撃");
    insert_speech_from(&db, "pk-B", "Bの割り込み");

    let out = source.poll_new_messages();
    assert_eq!(out.len(), 1, "返信中の相手の連投だけ: {out:?}");
    assert!(out[0].contains("Aの追撃"), "{}", out[0]);
    assert!(
        !out.iter().any(|s| s.contains("Bの割り込み")),
        "別相手の新着は走行中に注入しない（公開リレーへの誤爆防止）: {out:?}"
    );
}

/// [#323 / B2] `Silent` は何も注入しない（resume = 生きた相手が不定）。
#[test]
fn silent_scope_injects_nothing() {
    let db = opencrab_db::Db::memory().unwrap();
    let source = SessionLiveInbound::new(db.clone(), SESSION, AGENT)
        .with_scope(opencrab_actions::LiveInboundScope::Silent);

    insert_speech_from(&db, "pk-A", "追撃");
    insert_speech_from(&db, "pk-B", "割り込み");

    assert!(
        source.poll_new_messages().is_empty(),
        "Silent は DB を引くまでもなく空"
    );
}

/// [#323 / B2] 既定（`AllOthers`）は従来どおり自分以外の全発言を注入する（非退行）。
#[test]
fn all_others_scope_injects_every_peer() {
    let db = opencrab_db::Db::memory().unwrap();
    let source = SessionLiveInbound::new(db.clone(), SESSION, AGENT);

    insert_speech_from(&db, "pk-A", "Aの発言");
    insert_speech_from(&db, "pk-B", "Bの発言");

    let out = source.poll_new_messages();
    assert_eq!(out.len(), 2, "全相手が注入される: {out:?}");
}
