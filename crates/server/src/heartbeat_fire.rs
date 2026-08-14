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

/// web セッション宛ハートビートの**表示用 channel_name**（web 固有ラベル・#628 条件 D）。
///
/// [`channel_label`] の web 分岐が使う。web の会話は Discord のような外部チャンネル名を
/// 持たないので、ダッシュボードの会話であることを示す呼称を充てる。
pub const HEARTBEAT_WEB_CHANNEL_LABEL: &str = "（ダッシュボードの会話）";

/// Discord / Nostr / web 以外の発火先宛ハートビートの**表示用 channel_name**（真に中立の
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
/// - **web**: web 固有ラベル [`HEARTBEAT_WEB_CHANNEL_LABEL`]（明示分岐）。
/// - **その他**: 真に中立の [`HEARTBEAT_NEUTRAL_CHANNEL_LABEL`]（web 固有ラベルを流用しない。
///   5 つ目の transport が来たら明示分岐を足すが、忘れても web を名乗る誤表示にはならない）。
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
        opencrab_web_gateway::WEB_TIMED_FIRE_KIND => HEARTBEAT_WEB_CHANNEL_LABEL.to_string(),
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
/// **誘導は発火元 transport で変わる**（`posts_response_body`）。応答本文の自動配送は Discord
/// チャンネルの発火だけで、ブロードキャスト（Nostr）は自動配送しない（[`run_one_heartbeat`]。
/// オーナー判断）。したがって:
/// - **Discord（`posts_response_body = true`）**: 「この応答がそのままチャンネルへ投稿される」。
/// - **ブロードキャスト（`false`）**: 「投稿するなら投稿ツールで自分から。応答本文は投稿されない」。
///
/// 「宣言 → サブタスク起動」の進め方は**プロンプトで誘導するだけ**（機構では強制しない）。
/// **定型の宣言文は埋め込まない**（#588: 毎回同じ文字列が出ると、撤去した `IDLE:` の定型文と
/// 同じく「エージェントの発話」ではなく「システムの通知」になり、会話ログを汚染する）。伝えるのは
/// transport ごとの事実だけで、言い回しはエージェントが毎ターン自分の言葉で決める。
fn format_heartbeat_prompt(
    channel_name: &str,
    instructions_text: &str,
    posts_response_body: bool,
) -> String {
    let action = if posts_response_body {
        // Discord: 応答本文がそのまま投稿される。
        "取り組むことがあれば、この応答がそのままチャンネルへ投稿されるので、これから何をするかを自分の言葉で短く添えたうえで、実作業は spawn_subtask で起動してください。"
    } else {
        // ブロードキャスト（Nostr）: 応答本文は投稿されない。投稿はツールで行う。
        "取り組むことがあれば、投稿は投稿ツール（nostr_post 等）で自分から行い、実作業は spawn_subtask で起動してください。この応答の本文はチャンネルへは投稿されません。"
    };
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
    // 応答本文がその場に自動配送されるか（Discord・web=true / Nostr=false）。誘導を変える。
    let posts_response_body = descriptor.posts_response_body();

    // 渡すプロンプト（#584: channel → agent → default）→ 整形。応答本文の自動配送有無は transport の
    // 性質（`posts_response_body`）で決まるので、誘導もそれで変える（`format_heartbeat_prompt`）。
    let (prompt, instructions_source) = {
        let conn = db.lock().ok()?;
        let resolved =
            opencrab_db::queries::resolve_heartbeat_instructions(&conn, agent_id, &channel_id);
        (
            format_heartbeat_prompt(&channel_name, &resolved.text, posts_response_body),
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
        let nostr = format_heartbeat_prompt(HEARTBEAT_NOSTR_CHANNEL_LABEL, "巡回してね", false);
        assert!(
            nostr.contains("現在の会話「（自律ハートビート）」。巡回してね"),
            "Nostr 経路の文面が違う: {nostr}"
        );
        // DiscordChannel はチャンネル設定名を channel_name に使う。
        let discord = format_heartbeat_prompt("雑談", "静かにね", true);
        assert!(
            discord.contains("現在の会話「雑談」。静かにね"),
            "Discord 経路の文面が違う: {discord}"
        );
    }

    /// #588 Stage 3: 規約は**通常のターンへ寄せる**。撤去した SPEAK/LEARN/IDLE の語彙が文面に
    /// 残っておらず、沈黙は通常ターンと同じ `NO_REPLY` で表せることを、両 transport で担保する。
    #[test]
    fn format_heartbeat_prompt_uses_no_reply_and_drops_speak_learn_idle() {
        for posts_body in [true, false] {
            let p = format_heartbeat_prompt("雑談", "静かにね", posts_body);
            assert!(
                p.contains("NO_REPLY"),
                "沈黙を通常ターンと同じ NO_REPLY で表す規約が無い（posts_body={posts_body}）: {p}"
            );
            for retired in ["SPEAK", "LEARN", "IDLE"] {
                assert!(
                    !p.contains(retired),
                    "撤去した語彙 {retired} が指示文に残っている（posts_body={posts_body}）: {p}"
                );
            }
            assert!(
                p.contains("spawn_subtask"),
                "実作業をサブタスクで起動する誘導が無い（posts_body={posts_body}）: {p}"
            );
        }
    }

    /// #588 Stage 3（オーナー判断）: 誘導は発火元 transport で変わる。
    /// - Discord: 応答本文がそのまま投稿される旨を伝える。
    /// - ブロードキャスト（Nostr）: 応答本文は投稿されず、投稿はツールで行う旨を伝える。
    ///
    /// どちらも**定型の宣言文をハードコードしない**（issue に 3 回出てくる例示を埋め込まない）。
    #[test]
    fn format_heartbeat_prompt_guidance_varies_by_transport_without_hardcoding_a_declaration() {
        // Discord: 応答本文が投稿される。
        let discord = format_heartbeat_prompt("雑談", "静かにね", true);
        assert!(
            discord.contains("この応答がそのままチャンネルへ投稿される"),
            "Discord は応答本文が投稿される旨を伝えていない: {discord}"
        );
        assert!(
            discord.contains("自分の言葉で"),
            "Discord は宣言をエージェント自身の言葉で書かせていない: {discord}"
        );

        // ブロードキャスト（Nostr）: 応答本文は投稿されない。投稿はツールで。
        let broadcast = format_heartbeat_prompt("（自律ハートビート）", "巡回してね", false);
        assert!(
            broadcast.contains("投稿されません"),
            "ブロードキャストで「応答本文は投稿されない」旨が無い（二重投稿の道を塞ぐ要）: {broadcast}"
        );
        assert!(
            broadcast.contains("投稿ツール"),
            "ブロードキャストで投稿はツールで行う誘導が無い: {broadcast}"
        );
        assert!(
            !broadcast.contains("この応答がそのままチャンネルへ投稿される"),
            "ブロードキャストで自動投稿を示唆している（本文は投稿されないはず）: {broadcast}"
        );

        // どちらも定型の宣言文はハードコードしない。
        for p in [&discord, &broadcast] {
            assert!(
                !p.contains("作業するね"),
                "定型の宣言文がハードコードされている（#588: 例示は実装すべき文字列ではない）: {p}"
            );
        }
    }
}
