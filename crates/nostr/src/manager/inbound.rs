use super::*;

/// 受信イベント1件を処理する（セッション記録 → 応答生成を session キューへ投入）。
///
/// **この関数は await しない**（#178）。以前は `respond_serialized(...).await` を受信
/// ループ内で直接 await していたため、per-session ロックを resume が握っている間
/// **ループ全体（全セッション・全相手）が停止**し、`nostaro watch` の stdout も読まれず
/// 滞留した。S3a で resume が日常化したため常態化していた。
///
/// その後 `tokio::spawn` へ出したが、それだけでは **同一セッションの連投の処理順が
/// 「どの spawn タスクが先にロックを取るか」で決まる**（順序保証が壊れる）うえ、
/// permit をループ内で取っていたため上限が埋まると再び全相手の受信が止まった。
/// 現在は [`SessionQueues`] へ投入するだけで、FIFO 処理・直列化・流量制限はすべて
/// consumer タスク側（ループ外）が担う。
///
/// 直列化（#168）はそのまま成立する: ロック取得は [`NostrResponder::respond_serialized`]
/// に閉じているので、consumer が回す job でも同一セッションの inbound / resume が直列化
/// される。セッションの用意と受信の転記は**ループ内で同期的に**済ませる: DB 書き込み
/// のみで await しないうえ、job 側へ回すと連投で転記順が入れ替わる。
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_event<R: NostrAgentRunner>(
    runner: &R,
    cli: &NostaroCli,
    agent_id: &str,
    self_pubkey: &str,
    // #698 元栓: 許可集合セルと、捨てた件数の揮発カウンタ。
    allow: &AllowGate,
    dropped: &Arc<AtomicU64>,
    admin: &Arc<dyn NostrIdentityAdmin>,
    runtime: &Arc<NostrSessionRuntime>,
    permits: &Arc<Semaphore>,
    queues: &Arc<SessionQueues>,
    event: NostrEvent,
) {
    let sources = allow.read().unwrap();
    match pre_record_drop(&event, self_pubkey, &sources) {
        Some(DropReason::Dm) => {
            info!(
                agent_id,
                sender = %event.pubkey,
                kind = event.kind,
                "nostr: dropping DM (kind 4/1059 are not handled — receive discarded, no reply; #514)"
            );
            return;
        }
        Some(DropReason::SelfPost) => return,
        Some(DropReason::AllowSet) => {
            dropped.fetch_add(1, AtomicOrdering::Relaxed);
            return;
        }
        None => {}
    }
    let session_id = nostr_session_id(agent_id);
    let owner_id = sources.owner.iter().next().cloned().unwrap_or_default();
    drop(sources);

    let mut admitted = false;
    let mut run_caller = None;
    let resolve =
        |sender: &str, _: &[String], _: &str| runner.resolve_nostr_caller(agent_id, sender);
    let dm_any = |sender: &str, _: &[String], owner: &str| sender == owner;
    let dm_one = |sender: &str, _: &str, owner: &str| sender == owner;
    let sid = session_id.clone();
    let wl = move |channel: &str, aid: &str| channel == sid || channel == nostr_session_id(aid);
    let lookups = opencrab_actions::InboundLookups {
        resolve_caller: &resolve,
        dm_allowed_any: &dm_any,
        dm_allowed: &dm_one,
        channel_whitelisted: &wl,
    };
    let accept = accept_nostr_inbound::<()>(
        &event,
        agent_id,
        &session_id,
        &owner_id,
        event.inbound_kind_label(),
        &lookups,
        None,
        |_| (),
        |_, _| admitted = true,
        |_, adm, _| run_caller = Some(adm.caller.clone()),
    );
    if accept.is_err() || !admitted {
        dropped.fetch_add(1, AtomicOrdering::Relaxed);
        return;
    }
    let caller = match run_caller {
        Some(c) => c,
        None => return,
    };

    // agent 単位のセッション（**1 エージェント = 1 会話** / #323）。誰から来た受信も
    // ここへ落ちるので、エージェントは自分の発言も含めて 1 本の履歴として読める。
    // 誰の発言かは下の `sender_id`（= 相手の pubkey）が担う。

    // 会話履歴・転記に載せる本文（#282）。本文だけを記録していたため、次ターン以降の
    // エージェントは author の npub も note id も kind も参照できなかった（nostaro 本体
    // より劣化）。#272/#274 の画像アンカーと同じく、受信メタ情報を本文側に焼き込む。
    // 転記とエージェント向けで**同じ文字列**を使い、従来の非対称（転記にだけ kind が
    // 載る）を解消する。
    let inbound_text = event.inbound_text();

    runner.ensure_session(&session_id, &[agent_id.to_string()], "Nostr", "{}", "nostr");

    // #570: 会話履歴へ残す本文だけ、tool_result と同じ退避
    // （[`opencrab_actions::sanitize_tool_result_for_log`]）に乗せる。Nostr の受信本文は
    // relay が受け付けたサイズがそのまま入り、コード上の上限が無かった。超大受信が
    // 「直近ユーザー発言」枠で退避も予算も素通りし、単独で context 予算を食い潰す経路を塞ぐ。
    //
    // - 閾値・保存先・案内書式は tool_result と**同一**（新しい流儀を足さない）。退避先は
    //   エージェントのワークスペース `<root>/tmp/`（`ws_read` で読み返せる）。ファイル名の
    //   一意キーには tool_call_id の代わりに Nostr の event.id を使う。
    // - **閾値以下は完全な no-op**（本文を 1 バイトも変えない）: 本番最大の受信
    //   （6,761 字 ≒ 1,700 トークン < 2,500）はここを素通りする。
    // - 秘密（nsec）混入時のマスクも同じ経路で掛かる（防御的多層）。
    //
    // 転記（`relay_inbound_notification`）は**人間向けの生本文**のまま送る（下でそのまま
    // `inbound_text` を使う）。転記先はプラットフォーム側でサイズ頭打ちになり、退避案内を
    // 人が読むチャンネルへ流すのは不適切なため、退避は会話履歴の側だけに掛ける。
    let recorded_text = opencrab_actions::sanitize_tool_result_for_log(
        "nostr_inbound",
        &inbound_text,
        &session_id,
        &event.id,
        runner.agent_workspace_root(agent_id).as_deref(),
    );

    // #284 P0-3: 受信発言の記録失敗は握り潰さない。落ちた発言は会話履歴に現れず、
    // エージェントはその投稿を見ないまま応答することになる。
    let recorded = runner.record_inbound_message(
        opencrab_actions::TranscriptSource::Nostr,
        &opencrab_actions::InboundMessageRecord {
            session_id: &session_id,
            recipient_agent_id: agent_id,
            sender_id: &event.pubkey,
            sender_name: &event.author_label(),
            avatar_url: None,
            channel_id: None,
            pubkey: Some(&event.pubkey),
            text: &recorded_text,
            image_urls: &[],
        },
    );
    if !recorded {
        tracing::error!(
            session_id = %session_id,
            agent_id = %agent_id,
            "failed to persist an inbound Nostr message after retries; the agent will answer \
             WITHOUT ever seeing it. Check database health."
        );
    }

    // クロスゲートウェイ転記（issue #252 段階 A）: 自分宛の受信を、エージェント単位で
    // 設定した Discord チャンネル（webhook）へ転記する。設定が有効なときだけ配送する
    // （未設定 / 無効 → `resolve_nostr_relay_target` が None を返し、1 件も飛ばない = fail-closed）。
    //
    // ここは受信ループ内（#178: await しない）。宛先の解決は同期 DB 読み 1 回、実際の送信は
    // 実装側で非ブロック（fire-and-forget）。送信失敗は実装側でログのみに留め、応答生成や
    // 他セッションの受信を巻き込まない。Nostr 側は Discord を型で名指しせず、actions 層の
    // 共通口（`WebhookConfig`）を通す。
    if let Some(target) = runner.resolve_nostr_relay_target(agent_id) {
        let relay_text = format!(
            "[Nostr / {kind}] {author}\n{body}",
            kind = event.inbound_kind_label(),
            author = event.author_label(),
            // #282: エージェントの会話履歴に残るのと同一の本文（メタアンカー込み）。
            body = inbound_text,
        );
        runner.relay_inbound_notification(&target, relay_text);
    }

    // #282: 「返信先はこれ」という指示だけでなく、受信イベントの**事実**（誰の / どのノート /
    // どの kind）を明示する。すべて公開情報なので隠す理由はない（nsec は当然出さない）。
    let prompt_suffix = format!(
        "[Nostr] {author} さんの投稿への応答です。\n\
         - 送信者: {author_key}（pubkey={pubkey}）\n\
         - 対象ノート: {target}\n\
         - 種別: kind:{kind}（{label}）\n\
         返信するなら nostr_reply(target=\"{target}\") を使ってください（target は返信先ノート）。\
         種別的に本文返信が不自然なもの（リアクション等）や、返信不要なら \
         NO_REPLY とだけ答えてください。",
        author = event.author_label(),
        author_key = event.author_key(),
        pubkey = event.pubkey,
        target = event.reply_target(),
        kind = event.kind,
        label = event.inbound_kind_label(),
    );

    let responder = NostrResponder::new(
        runner.clone(),
        cli.clone(),
        runtime.clone(),
        admin.clone(),
        agent_id,
    );
    let reply_target = event.reply_target().to_string();
    let event_id = event.id.clone();
    // #323 / B2: このターンの返信相手（= 転記の speaker_id）。走行中注入をこの相手の
    // 連投だけに絞り、別相手の新着が reply_target と食い違う本文を公開リレーへ誤爆
    // させない（旧 per-相手 セッションの性質の復元）。
    let speaker_pubkey = event.pubkey.clone();
    let job_session_id = session_id.clone();
    // `caller` は元栓ゲートを通過した後に `event.pubkey` から解決済み（#319 の解決点はここ）。
    // 応答生成側で session_id から逆算すると、セッション規約を変えた瞬間に権限判定が壊れる。
    // オーナー未設定・未登録なら `Agent`（fail-closed）。ここで job へ move する。
    // 流量制限（permit）は **consumer タスクの内側**で取る（`run_consumer` 参照）。
    // ここ（受信ループ内）で取ると、session ロック待ちで何もしていないタスクが permit を
    // 占有し、受信ループ全体＝そのエージェントの全相手の受信が止まる。
    let job: ResponseJob = Box::pin(async move {
        responder
            .respond_serialized(
                &job_session_id,
                &reply_target,
                &prompt_suffix,
                Some(&event_id),
                caller,
                opencrab_actions::LiveInboundScope::OnlySpeaker(speaker_pubkey),
            )
            .await;
    });
    queues.enqueue(agent_id, &session_id, permits, job);
}
