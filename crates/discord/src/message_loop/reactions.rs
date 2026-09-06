use tracing::warn;

use crate::gateway::DiscordGateway;
use opencrab_actions::DeliveryEffect;
use opencrab_gateway::IncomingMessage;

/// IncomingMessage からセッション用のリッチメタデータとテーマを構築する。
pub(super) fn build_discord_session_metadata(incoming: &IncomingMessage) -> (String, String) {
    let (guild_id, channel_id) = match &incoming.source {
        opencrab_gateway::MessageSource::Discord {
            guild_id,
            channel_id,
        } => (guild_id.clone(), channel_id.clone()),
        _ => (String::new(), String::new()),
    };

    let is_dm = guild_id.is_empty();

    if is_dm {
        let dm_user_name = incoming.sender.name.clone();
        let theme = format!("DM with {}", dm_user_name);
        let mut meta = serde_json::json!({
            "source": "discord",
            "is_dm": true,
            "channel_id": channel_id,
            "dm_user_name": dm_user_name,
            "dm_user_id": incoming.sender.id,
        });
        if let Some(ref avatar_url) = incoming.sender.avatar_url {
            meta["dm_user_avatar_url"] = serde_json::json!(avatar_url);
        }
        (theme, meta.to_string())
    } else {
        let guild_name = incoming
            .metadata
            .get("guild_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let guild_icon_url = incoming
            .metadata
            .get("guild_icon_url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let channel_name = incoming
            .metadata
            .get("channel_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let theme = if !channel_name.is_empty() && !guild_name.is_empty() {
            format!("#{} in {}", channel_name, guild_name)
        } else {
            "Discord conversation".to_string()
        };

        let meta = serde_json::json!({
            "source": "discord",
            "is_dm": false,
            "guild_id": guild_id,
            "guild_name": guild_name,
            "guild_icon_url": guild_icon_url,
            "channel_id": channel_id,
            "channel_name": channel_name,
        });
        (theme, meta.to_string())
    }
}

/// Discord用: message_idを含む変動コンテキストを前置するヘルパー。
pub(super) fn prepend_runtime_context_discord(
    user_message: &str,
    session_theme: &str,
    message_id: &str,
) -> String {
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %:z");
    let tz_name = iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".to_string());
    let now = format!("{now} ({tz_name})");
    format!(
        "[Context]\nCurrent date and time: {now}\nCurrent discussion topic: {session_theme}\nDiscord message_id: {message_id}\n\n{user_message}"
    )
}

/// リアクション付与だけを切り出した継ぎ目（#317）。
///
/// 本番の実体は [`DiscordGateway`]。テストは Discord へ実際に HTTP を出せないため、
/// 付与要求を記録する fake を差し替えて配線を固定する。gateway 非依存層には何も
/// 足さない — この trait は `crates/discord` に閉じている。
#[async_trait::async_trait]
pub(crate) trait ReactionAdder: Send + Sync {
    /// 固有メソッド `DiscordGateway::add_reaction` と**名前を分ける**。同名にすると
    /// 実装本体（`DiscordGateway::add_reaction(self, ..)`）が固有メソッドではなく
    /// この trait メソッド自身へ解決されうる。いまは固有メソッドが優先されるので
    /// 動くが、固有側が改名・削除された瞬間に**コンパイルは通ったまま無限再帰**になる。
    async fn add_unicode_reaction(
        &self,
        channel_id: u64,
        message_id: u64,
        emoji: &str,
    ) -> anyhow::Result<()>;
}

#[async_trait::async_trait]
impl ReactionAdder for DiscordGateway {
    async fn add_unicode_reaction(
        &self,
        channel_id: u64,
        message_id: u64,
        emoji: &str,
    ) -> anyhow::Result<()> {
        self.add_reaction(channel_id, message_id, emoji).await
    }
}

/// 元の投稿に Unicode 絵文字のリアクションを 1 個付ける（非致命的）。
///
/// 使い分けは**絵文字だけ**で、手続きは共通:
/// - 👀 = LLM がこの投稿を読んだ（ターンの文脈に含めた）
/// - 🤐 = エージェントが `NO_REPLY` を選んだ（読んで黙ると**決めた**）。
///   これが無いと投稿者からは「読んで黙った」のか「落ちて返せなかった」のか区別できない（#317）
///
/// 絵文字は呼び出し側のハードコード（設定項目にしない）。付与失敗（権限不足・削除済み
/// メッセージ・無効なID等）は握りつぶし、channel_id/message_id/絵文字とエラー内容だけを
/// ログに残す（秘密値は含めない）。message_id が空/非数値なら付与自体を諦める。
pub(super) async fn add_reaction_non_fatal<G: ReactionAdder>(
    gateway: &G,
    channel_id: u64,
    channel_id_str: &str,
    message_id: &str,
    emoji: &str,
) {
    let msg_id = match parse_reaction_message_id(message_id) {
        Some(id) => id,
        None => {
            if !message_id.is_empty() {
                warn!(
                    channel_id = %channel_id_str,
                    message_id = %message_id,
                    emoji = %emoji,
                    "Skip reaction: invalid message_id"
                );
            }
            return;
        }
    };
    if let Err(e) = gateway
        .add_unicode_reaction(channel_id, msg_id, emoji)
        .await
    {
        warn!(
            channel_id = %channel_id_str,
            message_id = %message_id,
            emoji = %emoji,
            error = %e,
            "Failed to add reaction (non-fatal)"
        );
    }
}

/// LLM が投稿を読んだ（ターン文脈に含めた）ときに付ける印。
pub(super) const SEEN_EMOJI: &str = "👀";

/// エージェントが `NO_REPLY` を選んだことを示す印（#317）。
/// 👀 と同じ絵文字にすると 2 つの状態が区別できなくなる。
pub(super) const NO_REPLY_EMOJI: &str = "🤐";

/// legacy message_loop が自分の最後の投稿に付ける旧「発言終わり」の印（#431）。
///
/// canonical V3 の 🏁 は activity ended で core が指定した最終生成の最後の投稿に 1 件だけ付き、
/// agent 単位で進行中の作業が無いことを表す（DESIGN-TURN-CONTINUATION §13.3）。
/// この legacy 経路は旧判定を保持しており、V3 の判断ロジックには使わない。
///
/// 旧目的: 見ている人間が「まだ続きを書いているのか、言い終わったのか」を判別できる。
/// 👀（読んだ）/🤐（黙ると決めた）と意味が衝突しない絵文字にする。付与対象も違い、
/// これは**自分の投稿**に付く（👀/🤐 は受信したユーザー投稿に付く）。
/// 既存の 2 種と同様ハードコード（設定項目は増やさない / #431 の判断）。
pub(super) const SPOKE_EMOJI: &str = "🏁";

/// ターンがエラーで失敗したことを示す印（#668）。
///
/// 上流プロバイダ障害等でターンが落ちたとき、**エラー本文をチャンネルへ出す代わりに**
/// トリガー投稿へこれを付け「失敗した」ことだけを可視化する（本文投稿は複数エージェント間で
/// エラー文に反応し合う無限ループを誘発するため出さない）。付与対象は受信したユーザー投稿
/// （👀/🤐 と同じ側）だが、意味が衝突しない絵文字にする。既存の 3 種と同様ハードコード。
pub(super) const FAILED_EMOJI: &str = "❌";

/// legacy message_loop で旧「発言終わり」リアクションの対象になるか（#431）。
///
/// V3 の正典は §13.3 の `completed_target` であり、この関数の turn 全体 `posted` 判定や
/// 上限打ち切り除外を新経路へ流用してはならない。
///
/// `true` を返すのは、ターンが**自然に**（次の行動を選ばず）終わり、かつそのターンで
/// 自分が**実際に投稿できた**（`posted`）ときだけ。以下は `false`:
/// - エラー / タイムアウト（`Err`）… 「言い終わった」ではなく落ちた
/// - 反復上限での打ち切り（`stopped_by_limit`）… 途中で切られた
/// - `posted == false` … このターンで実投稿していない（全反復 NO_REPLY/空、または
///   非 writable で送信に至らなかった）＝そもそも発話していない
/// - `started_subtask == true` … このターンが background subtask を起こした。掘削を
///   投げたターンは「次の行動を選んで」終わっている。ここで付けると『調べますね🏁』の
///   数分後に続きが届く**逆の情報**になる。印は subtask 完了で resume したターンが
///   自然終了したときにそちらへ付く（`process_subtask_completed`）。resume ターンが
///   さらに subtask を投げた場合も同じ条件で弾かれ、次の resume へ委ねられる。
///
///   `started_subtask` の実体は `RunRequest::subtask_starts` に渡したターンローカルな
///   カウンタで、**自動 dispatch と明示 `spawn_subtask` の両方**が登録簿への登録が
///   成立したところで加算する。登録簿を後から覗く形にしないのは、run が返る前に決着
///   した subtask が既に除去されていて取りこぼす（＝まさに resume が来るケースを
///   見落とす）ため。
///
/// **最終応答テキストの中身（NO_REPLY/空）では判定しない。** 反復途中で発話し最終応答が
/// `NO_REPLY` で自然終了するターン（例: 反復1で発話 → 最終 NO_REPLY）を取りこぼすため。
/// 「発話したか」は実投稿の有無（`posted`）で見る。`posted` の実体は、通常経路では
/// 実送信を試みた回数（`reply_send_seq > 0`）、subtask/interaction 経路では送信 id
/// （`sent_id.is_some()`）。実送信を試みたが失敗して id が採れなかった場合は、この関数は
/// `true` を返すが、実際の付与は呼び出し側の「最後の投稿 id が Some か」で最終的に弾かれる。
pub(super) fn end_of_speech_qualifies(
    effect: &DeliveryEffect,
    posted: bool,
    started_subtask: bool,
) -> bool {
    match effect {
        DeliveryEffect::Failed { .. } => false,
        DeliveryEffect::Text {
            stopped_by_limit, ..
        } => end_of_speech_qualifies_ok(*stopped_by_limit, posted, started_subtask),
        DeliveryEffect::NoReply | DeliveryEffect::Empty => posted && !started_subtask,
    }
}

/// [`end_of_speech_qualifies`] の Text 側だけを取り出したもの。subtask 完了 /
/// interaction 応答の経路は本文配送のあとで判定するため、この形が要る。
pub(super) fn end_of_speech_qualifies_ok(
    stopped_by_limit: bool,
    posted: bool,
    started_subtask: bool,
) -> bool {
    !stopped_by_limit && posted && !started_subtask
}

/// リアクションを付ける対象の message_id を解析する。
///
/// 空文字（message_idがメタデータに無い）や数値でない場合は `None` を返し、
/// 呼び出し側はリアクション付与をスキップする。
pub(super) fn parse_reaction_message_id(message_id: &str) -> Option<u64> {
    if message_id.is_empty() {
        return None;
    }
    message_id.parse::<u64>().ok()
}

/// デバウンス**バッファ**のキー = **channel だけ**（#543 / #556）。
///
/// オーナー指示「デバウンスはチャンネルごと。人で分けたらだめ」そのまま。バッファは channel
/// ごとに 1 本（タイマーも 1 本）。権限で並行バッファに割らない: run は DB から会話全体を読む
/// ので、権限で割ると同じ文脈に対し run が 2 回起きて増幅するだけ。
///
/// ただし**フラッシュ時に**、バッファ内を到着順のまま「連続した同一 trust_level」でグループへ
/// 切り、run はグループごとに 1 回起こす（`plan_record_only_flags`）。これで別権限は別 run に分かれ、
/// caller の降格（最後の送信者に引きずられる）も起きない。詳細は `run_discord_loop` のフラッシュ
/// 箇所のコメントを参照。
pub(super) fn debounce_window_key(msg: &IncomingMessage) -> String {
    match &msg.source {
        opencrab_gateway::MessageSource::Discord { channel_id, .. } => channel_id.clone(),
        _ => String::new(),
    }
}

/// メッセージが応答対象になる内容（テキストまたは画像）を持つか（#543）。
///
/// デバウンス窓の run トリガー選びに使う。`process_incoming_message` 冒頭の早期 return
/// （text も image も空なら何もしない）と同じ判定にそろえ、内容の無いメッセージを
/// トリガーに選んで run を消してしまうことを防ぐ。
pub(super) fn incoming_has_content(msg: &IncomingMessage) -> bool {
    let (text, images) = extract_discord_content(&msg.content);
    !text.is_empty() || !images.is_empty()
}

/// メッセージコンテンツからテキストと画像URLを抽出する。
pub(super) fn extract_discord_content(
    content: &opencrab_gateway::MessageContent,
) -> (String, Vec<String>) {
    match content {
        opencrab_gateway::MessageContent::Text(t) => (t.clone(), vec![]),
        opencrab_gateway::MessageContent::Image { url, .. } => (String::new(), vec![url.clone()]),
        opencrab_gateway::MessageContent::Multi(parts) => {
            let mut texts = Vec::new();
            let mut urls = Vec::new();
            for part in parts {
                match part {
                    opencrab_gateway::ContentPart::Text(t) => texts.push(t.clone()),
                    opencrab_gateway::ContentPart::Image { url, .. } => urls.push(url.clone()),
                }
            }
            (texts.join(" "), urls)
        }
    }
}
