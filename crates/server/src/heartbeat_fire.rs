//! 時刻発火（#588 TimedFire）の**発火本体**。1 発火分を「発火先ゲートウェイのループへ
//! `TimedFire` イベントを 1 本流す」だけへ縮小した [`run_one_heartbeat`] と、その 2 つの固有部品
//! （プロンプト整形・`heartbeat_log` 記録）を置く。
//!
//! # なぜ lib（`opencrab_server`）に置くか
//!
//! 発火は 2 経路から起こる。ひとつは中央スケジューラ（時刻が来たら・`bin` の `scheduler` mod）、
//! もうひとつは `run_my_heartbeat` ツール（時間を待たずに手動発火・#599・lib の `agent_heartbeat`）。
//! **両者が「まったく同じ経路」を通る**ことが要件（テスト用の別経路を作ると本番と挙動が割れる）。
//! `bin` の mod は lib から参照できないので、共有できる lib へ置き、両方がこの 1 つの関数を呼ぶ。

use opencrab_actions::{gateway_kinds, CallerIdentity, FireTarget};

use crate::AppState;

/// Nostr 宛ハートビートターンの**表示用 channel_name**（プロンプト内の会話呼称）。
/// Nostr broadcast は特定チャンネルを持たないため、会話名の代わりにこのラベルを充てる。
///
/// **スコープではなく表示ラベル**である点に注意。旧名は「agent スコープ」の語を含んでおり、
/// agent スコープ発火（#456 で全廃済み・現在は session 単位の `nostr-` セッションから発火）が
/// まだ残っているかのように読み手を誤らせたため改名した（#472）。
/// 場所の呼称は transport 中立にする（#158 S2 と同方針）。
pub const HEARTBEAT_NOSTR_CHANNEL_LABEL: &str = "（自律ハートビート）";

/// Discord / Nostr 以外の発火先宛ハートビートの**表示用 channel_name**（真に中立の
/// ラベル・#628 条件 D）。
///
/// [`channel_label`] の `_` 分岐が使う。web 固有ラベルを流用すると 5 つ目の transport
/// （例 Matrix）でも「ダッシュボードの会話」と出てしまうので、fallback は transport を名指し
/// しない中立語にする（transport 中立・#158 S2 と同方針）。
pub const HEARTBEAT_NEUTRAL_CHANNEL_LABEL: &str = "（この会話）";

/// 発火先 → プロンプト内の会話呼称（表示用 channel_name）を解く小関数（#628 条件 D）。
///
/// **`channel_label` は `TransportFire` trait に置かない**: Discord は実行時に db から
/// チャンネル名を引き、他は固定ラベルで、「静的 descriptor」の建前と矛盾するため。発火経路
/// （このモジュール）側に `target → 表示名` の小関数として残す。
/// - **Discord**: db のチャンネル設定名（無ければ channel_id）。
/// - **Nostr**: 固定ラベル [`HEARTBEAT_NOSTR_CHANNEL_LABEL`]。
/// - **その他**: 真に中立の [`HEARTBEAT_NEUTRAL_CHANNEL_LABEL`]。
fn channel_label(db: &opencrab_db::Db, target: &FireTarget, agent_id: &str) -> String {
    match target.kind {
        gateway_kinds::DISCORD => {
            let Ok(conn) = db.lock() else {
                return target.channel_id.clone();
            };
            opencrab_db::queries::get_channel_config_for_agent(&conn, &target.channel_id, agent_id)
                .ok()
                .flatten()
                .map(|r| r.channel_name)
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| target.channel_id.clone())
        }
        gateway_kinds::NOSTR => HEARTBEAT_NOSTR_CHANNEL_LABEL.to_string(),
        _ => HEARTBEAT_NEUTRAL_CHANNEL_LABEL.to_string(),
    }
}

/// ハートビート指示文を system プロンプト用の 1 文へ整形する（#501）。
///
/// `channel_name` は発火経路で決まる（`FireTarget::NostrBroadcast` は
/// [`HEARTBEAT_NOSTR_CHANNEL_LABEL`]、`DiscordChannel` はチャンネル設定名）。
/// `instructions_text` は `resolve_heartbeat_instructions` の合成結果。整形はここ 1 箇所で、
/// 呼び出し側はこの文字列を system プロンプトへそのまま載せる。
///
/// #588 Stage 3: **ハートビートは専用の語彙を持たず、通常のターンとして走る。** いま動く必要が
/// 無ければ通常のターンと同じく `NO_REPLY` とだけ返す（沈黙＝無配送・無記録）。旧
/// `SPEAK`/`LEARN`/`IDLE` の出力規約と、見送り理由を毎回記録させる規約（#515）は撤去した。
///
/// **誘導は transport 非依存の 1 種類**（#925 §1.7・裁定 1）。V3 では配送は uniform（応答本文＝
/// そのセッションの gateway への say・Discord=チャンネル投稿 / Nostr=タイムライン投稿）なので、旧
/// `posts_response_body` の Discord / Nostr 2 分岐は撤去した。旧 Nostr の「投稿はツール（nostr_post）で」
/// は旧レーン固有の制約由来で、V3 には持ち込まない（DIRECTION-LOG 481・V3 に `nostr_post` は無い）。
///
/// 「宣言 → サブタスク起動」の進め方は**プロンプトで誘導するだけ**（機構では強制しない）。
/// **定型の宣言文は埋め込まない**（#588: 毎回同じ文字列が出ると、撤去した `IDLE:` の定型文と
/// 同じく「エージェントの発話」ではなく「システムの通知」になり、会話ログを汚染する）。伝えるのは
/// transport 非依存の事実だけで、言い回しはエージェントが毎ターン自分の言葉で決める。
fn format_heartbeat_prompt(channel_name: &str, instructions_text: &str) -> String {
    // transport 非依存の 1 文（§1.7）: 応答本文はそのままセッションの gateway へ投稿される。
    let action = "取り組むことがあれば、この応答はそのままセッションの gateway へ投稿されるので、これから何をするかを自分の言葉で短く添えたうえで、実作業は spawn_subtask で起動してください。";
    format!(
        "[ハートビート] 現在の会話「{channel_name}」。{instructions_text}\nいまはハートビートの時間です。{action}いま何もすることが無ければ、通常のターンと同じく NO_REPLY とだけ答えてください。"
    )
}

/// 発火の記録（`heartbeat_log`）。**これと「時間のトリガー＋渡すプロンプト」だけがハートビート
/// 固有**（single-entry の裁定）。`decision` は廃止語彙のため固定値 `fired`（列定義に経緯を明記）。
fn record_heartbeat_fire(db: &opencrab_db::Db, agent_id: &str, channel_id: &str, source: &str) {
    let Ok(conn) = db.lock() else {
        return;
    };
    let result = serde_json::json!({ "channel_id": channel_id, "source": source });
    if let Err(e) = opencrab_db::queries::insert_heartbeat_log(
        &conn,
        agent_id,
        "fired",
        Some(&result.to_string()),
    ) {
        tracing::error!(agent_id, "heartbeat: heartbeat_log の記録に失敗: {e}");
    }
}

/// 1 発火分（heartbeat・#588 TimedFire）: 時刻が来たら（または `run_my_heartbeat` で手動発火）、
/// 発火先セッションの**ゲートウェイのループへ TimedFire イベントを 1 本流すだけ**。そのループが
/// 「いつもの turn」を回す（配送・ロック・記録・継続ターンは全部ゲートウェイの既存実装）。ここに
/// 残るのは「トリガー＋渡すプロンプト（#584 指示解決）」と「発火の記録（`heartbeat_log`）」だけ。
///
/// caller は **常に `Owner`**（本人が自分の意思で動くターン）。プロンプトは受け口側で system プロンプト
/// へ足され、会話ログには「発言」として残さない（#501）。継続ターンはループ既存の subtask 完了経路
/// （Discord=`SubtaskCompleted` / Nostr=`NostrResponder` の `SubtaskCompletionSink`）が担う。
///
/// **`last_fired_at` はここでは刻まない**（呼び出し側の責務）。スケジューラは成功発火時に刻み、
/// `run_my_heartbeat`（手動発火）は**刻まない**（時間発火の位相をずらさないため・#599）。
///
/// `None` = 送れなかった（該当ゲートウェイのループが未稼働＝受け口が登録されていない）。
pub async fn run_one_heartbeat(
    state: &AppState,
    agent_id: &str,
    target: &FireTarget,
) -> Option<()> {
    let db = &state.db;
    // 発火先の descriptor を引く（session_id の組み直し・応答本文の自動配送有無・#628）。target は
    // 登録済み descriptor が parse した値なので通常必ず在る。無ければ発火経路が消えている（登録漏れ）
    // ので発火を諦める（fail-closed）。**transport の名前で分岐しない**——性質を descriptor に問う。
    let Some(descriptor) = state.timed_fire_router.descriptor(target.kind) else {
        tracing::warn!(
            agent_id,
            kind = target.kind,
            "timed-fire: descriptor が無い（登録漏れ）。発火を skip"
        );
        return None;
    };
    let kind = target.kind;
    // 発火先セッション（`build_session_id` は `parse` の逆写像・#508 の round-trip を保つ）。
    let session_id = descriptor.build_session_id(target, agent_id);
    let channel_id = target.channel_id.clone();
    let guild_id = target.guild_id.clone();
    // プロンプト内の会話呼称（transport 別・db を引く Discord は発火経路側で解く・条件 D）。
    let channel_name = channel_label(db, target, agent_id);

    // 渡すプロンプト（#584: channel → agent → default）→ 整形。誘導は transport 非依存の 1 種類
    // （#925 §1.7: V3 は応答本文＝gateway への say で uniform・`posts_response_body` の分岐は撤去）。
    let (prompt, instructions_source) = {
        let conn = db.lock().ok()?;
        let resolved =
            opencrab_db::queries::resolve_heartbeat_instructions(&conn, agent_id, &channel_id);
        (
            format_heartbeat_prompt(&channel_name, &resolved.text),
            resolved.source,
        )
    };

    // 受け口を引く（per-agent→共有・#400 と同型）。無ければ送れないので発火を諦める。
    let Some(sink) = state.timed_fire_router.resolve(kind, agent_id) else {
        tracing::warn!(agent_id, kind, session_id = %session_id, "timed-fire: 受け口が無い（ゲートウェイ未稼働）。発火を skip");
        return None;
    };
    // 時刻発火の送信ログ（#588）。受信側（各ループ）の「ターン開始」ログと突き合わせれば、
    // scheduler→ループ間で落ちたかが分かる。heartbeat 専用の文言にしない（アラーム・定時実行も
    // 同じイベントに乗る）。プロンプトは長いので先頭プレビューだけ。
    tracing::info!(
        agent_id,
        session_id = %session_id,
        transport = kind,
        prompt_preview = %opencrab_actions::prompt_preview(&prompt),
        "timed-fire: 発火（trigger → gateway loop）"
    );
    // 時刻発火のイベントを 1 本流すだけ（fire-and-forget。ロック・配送・記録・継続はループが回す）。
    sink.fire_timed_turn(opencrab_actions::TimedFireRequest {
        session_id,
        agent_id: agent_id.to_string(),
        channel_id: channel_id.clone(),
        guild_id,
        prompt,
        caller: CallerIdentity::Owner,
    });

    // 発火の記録（ハートビート固有）。
    record_heartbeat_fire(db, agent_id, &channel_id, instructions_source);
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #501: 指示文の整形は発火経路で決まる `channel_name` を差し込むだけ。Nostr（ラベル）と
    /// Discord（チャンネル名）で正しい文面になること。
    #[test]
    fn format_heartbeat_prompt_embeds_channel_name_per_fire_path() {
        // NostrBroadcast は `run_one_heartbeat` が HEARTBEAT_NOSTR_CHANNEL_LABEL を channel_name に使う。
        let nostr = format_heartbeat_prompt(HEARTBEAT_NOSTR_CHANNEL_LABEL, "巡回してね");
        assert!(
            nostr.contains("現在の会話「（自律ハートビート）」。巡回してね"),
            "Nostr 経路の文面が違う: {nostr}"
        );
        // DiscordChannel はチャンネル設定名を channel_name に使う。
        let discord = format_heartbeat_prompt("雑談", "静かにね");
        assert!(
            discord.contains("現在の会話「雑談」。静かにね"),
            "Discord 経路の文面が違う: {discord}"
        );
    }

    /// #588 Stage 3: 規約は**通常のターンへ寄せる**。撤去した SPEAK/LEARN/IDLE の語彙が文面に
    /// 残っておらず、沈黙は通常ターンと同じ `NO_REPLY` で表せる（誘導は transport 非依存の 1 種類）。
    #[test]
    fn format_heartbeat_prompt_uses_no_reply_and_drops_speak_learn_idle() {
        let p = format_heartbeat_prompt("雑談", "静かにね");
        assert!(
            p.contains("NO_REPLY"),
            "沈黙を通常ターンと同じ NO_REPLY で表す規約が無い: {p}"
        );
        for retired in ["SPEAK", "LEARN", "IDLE"] {
            assert!(
                !p.contains(retired),
                "撤去した語彙 {retired} が指示文に残っている: {p}"
            );
        }
        assert!(
            p.contains("spawn_subtask"),
            "実作業をサブタスクで起動する誘導が無い: {p}"
        );
    }

    /// #925 §1.7（裁定 1）: 誘導は transport 非依存の 1 種類。V3 は応答本文＝gateway への say で
    /// uniform なので、旧 Discord / Nostr 2 分岐（`posts_response_body`）は撤去した。旧 Nostr の
    /// 「投稿はツール（nostr_post）で・本文は投稿されない」は V3 に持ち込まない（V3 に `nostr_post`
    /// は無い・DIRECTION-LOG 481）。**定型の宣言文はハードコードしない**。
    #[test]
    fn format_heartbeat_prompt_guidance_is_transport_neutral() {
        // Discord ラベルでも Nostr ラベルでも同じ 1 種類の誘導になる。
        let discord = format_heartbeat_prompt("雑談", "静かにね");
        let nostr = format_heartbeat_prompt(HEARTBEAT_NOSTR_CHANNEL_LABEL, "巡回してね");
        for p in [&discord, &nostr] {
            assert!(
                p.contains("この応答はそのままセッションの gateway へ投稿される"),
                "transport 非依存の「gateway へ投稿される」誘導が無い: {p}"
            );
            assert!(
                p.contains("自分の言葉で"),
                "宣言をエージェント自身の言葉で書かせる誘導が無い: {p}"
            );
            // 旧 Nostr 固有の分岐（ツール投稿・本文非配送）を持ち込んでいない。
            assert!(
                !p.contains("投稿ツール")
                    && !p.contains("nostr_post")
                    && !p.contains("投稿されません"),
                "旧 Nostr 固有の transport 分岐が残っている（§1.7 撤去対象）: {p}"
            );
            // 定型の宣言文はハードコードしない。
            assert!(
                !p.contains("作業するね"),
                "定型の宣言文がハードコードされている（#588: 例示は実装すべき文字列ではない）: {p}"
            );
        }
    }
}
