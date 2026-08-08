use std::sync::Arc;
use std::sync::Mutex;
use tracing_subscriber::EnvFilter;

use opencrab_core::heartbeat::{
    heartbeat_loop, HeartbeatCallback, HeartbeatConfig, HeartbeatDecision,
    HeartbeatIntervalResolver,
};
use opencrab_server::{config, create_router, AppState};
use tokio::sync::watch;

mod heartbeat_delivery;
mod heartbeat_turn;
mod intake_process;

/// Discord へ 1 通送るためのハンドル。**ボットのトークンを保持するので、どのハンドルで
/// 送るかが Discord 上の送信者名を決める**（#400 の核心）。
#[cfg(feature = "discord")]
type DiscordHttp = Arc<serenity::http::Http>;
#[cfg(not(feature = "discord"))]
type DiscordHttp = ();

/// 共有（TOML）ゲートウェイのハンドル置き場。ゲートウェイ起動時に埋まる。
type DiscordHttpArc = Arc<Mutex<Option<DiscordHttp>>>;

/// ハートビート用セッションを取得または作成する。
fn get_or_create_heartbeat_session(
    db: &opencrab_db::Db,
    agent_id: &str,
    channel_id: &str,
) -> String {
    // 接頭辞は継続ターンの受け口（`heartbeat_turn::resume_origin`）が親セッションの判定に
    // 使う。書式が割れると「HB の決着なのに継続しない」が無言で起きるので定数を共有する。
    let session_id = format!(
        "{}{}-{}",
        heartbeat_turn::HEARTBEAT_SESSION_PREFIX,
        agent_id,
        channel_id
    );
    let conn = db.lock().unwrap();
    if let Ok(Some(_)) = opencrab_db::queries::get_session(&conn, &session_id) {
        return session_id;
    }
    let session = opencrab_db::queries::SessionRow {
        id: session_id.clone(),
        mode: "heartbeat".to_string(),
        theme: "ハートビート自律行動".to_string(),
        phase: "active".to_string(),
        turn_number: 0,
        status: "active".to_string(),
        participant_ids_json: serde_json::json!([agent_id]).to_string(),
        facilitator_id: None,
        done_count: 0,
        max_turns: None,
        metadata_json: None,
    };
    if let Err(e) = opencrab_db::queries::insert_session(&conn, &session) {
        tracing::warn!("Failed to create heartbeat session: {e}");
    }
    session_id
}

/// ハートビート発話を、本人の実会話（`discord-…`）セッションへ `speech` として二重記録する
/// （#425 案 A）。
///
/// 背景: HB 発話は heartbeat 専用セッションにしか記録されず、自分の投稿が Discord から
/// `message` として戻ってきても `is_own_message` で捨てられるため、本人の会話セッションには
/// 1 行も残らない。結果、後続の通常返信ターン（`discord-…` セッションだけを読む）で本人だけ
/// 自分の HB 投稿を思い出せない。ここで配信成功時に会話セッションへも書くことでその欠落を塞ぐ。
///
/// - `delivered=false`（非 Discord transport が担当・配信失敗・配信先無し）のターンは記録
///   しない。**言っていないことを記憶に残さない**方向を守る。
/// - guild を解決できないエージェント単位 tick（`channel_id`/`guild_id` が空）は記録先の
///   会話セッションが無いので何もしない（[`heartbeat_channel_session_id`] が `None`）。
/// - `opencrab_db::queries::HEARTBEAT_CHANNEL_ECHO_METADATA` を印として付ける。この印の
///   付いた行は**表示専用**で、記憶系（FTS 検索・記憶索引・宣言材料）には一切載らない
///   （`is_heartbeat_channel_echo` で db/core が除外）。記憶材料は heartbeat 専用セッション
///   側が担っており、この PR の前後で記憶系の挙動は不変。HB 経路の文脈組み立ては専用
///   セッションと会話セッションの両方を読むため、この印で実会話セクション側の二重表示も
///   除外する（読み取り側は `process::build_channel_conversation_section`）。通常返信が
///   読む `process::build_conversation_string` は印を見ずに素通しするので、狙いの経路には
///   そのまま載る。
fn record_heartbeat_channel_echo(
    db: &opencrab_db::Db,
    delivered: bool,
    agent_id: &str,
    guild_id: &str,
    channel_id: &str,
    content: &str,
) {
    if !delivered {
        return;
    }
    let Some(session_id) =
        opencrab_server::process::heartbeat_channel_session_id(agent_id, guild_id, channel_id)
    else {
        return;
    };
    let Ok(conn) = db.lock() else {
        tracing::error!(agent_id = %agent_id, channel_id = %channel_id, "#425: db lock 取得に失敗し、HB 発話を会話セッションへ記録できなかった");
        return;
    };
    let log = opencrab_db::queries::SessionLogRow {
        id: None,
        agent_id: agent_id.to_string(),
        session_id,
        log_type: "speech".to_string(),
        content: content.to_string(),
        speaker_id: Some(agent_id.to_string()),
        turn_number: None,
        metadata_json: Some(opencrab_db::queries::HEARTBEAT_CHANNEL_ECHO_METADATA.to_string()),
        created_at: None,
    };
    if let Err(e) = opencrab_db::queries::insert_session_log(&conn, &log) {
        tracing::error!(agent_id = %agent_id, channel_id = %channel_id, "#425: HB 発話を会話セッションへ記録できなかった: {e}");
    }
}

/// ハートビート応答テキストから決定（SPEAK / LEARN / IDLE）を解く。
///
/// **入力は応答テキストだけ**で、プロンプトに何を積んだかには依存しない（#404 で
/// ハートビート文脈へ実会話を入れたが、この関数のシグネチャがその独立性を担保する）。
/// 判定は既存挙動のまま: `SPEAK:` を含む最初の行の右側を取り、空なら Idle。
/// `SPEAK:` が無く LEARN（大小無視）を含めば Learn、それ以外は Idle。
fn parse_heartbeat_decision(response: &str) -> HeartbeatDecision {
    let response_text = response.trim();
    if response_text.contains("SPEAK:") {
        let content = response_text
            .lines()
            .find(|l| l.contains("SPEAK:"))
            .and_then(|l| l.splitn(2, "SPEAK:").nth(1))
            .unwrap_or("")
            .trim()
            .to_string();
        if !content.is_empty() {
            HeartbeatDecision::Speak(content)
        } else {
            HeartbeatDecision::Idle
        }
    } else if response_text.to_uppercase().contains("LEARN") {
        HeartbeatDecision::Learn
    } else {
        HeartbeatDecision::Idle
    }
}

/// エージェント単位 tick（channel を持たない発話）のプロンプト内呼称。
/// 場所の呼称は transport 中立にする（#158 S2 と同方針）。
const HEARTBEAT_AGENT_SCOPED_LABEL: &str = "（自律ハートビート）";

/// tick の発火計画（#238 の precedence）。純粋関数 [`heartbeat_firing_plan`] が返す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeartbeatFiringPlan {
    /// opt-in 済み: エージェント単位で 1 回発火し、既存の channel 発火はスキップする。
    /// `interval_secs` は resolve が返した実効間隔（#251 段階3）。
    AgentScoped { interval_secs: u64 },
    /// 未 opt-in かつグローバル有効: 従来どおり channel 単位で発火する（互換・不変）。
    ChannelScoped,
    /// 未 opt-in かつグローバル無効: 発火しない（他エージェントの opt-in のためだけに
    /// 立っているループ。既定無効エージェントは 1 バイトも挙動が変わらない）。
    None,
}

/// tick の発火計画を決める（#238 の precedence 判定・純粋関数）。
///
/// opt-in（`resolved_enabled`）が最優先。opt-in 済みなら **channel 発火とは排他**に
/// エージェント単位で 1 回だけ発火する（同一 tick で二度喋らせない）。未 opt-in は
/// グローバル有効時のみ従来の channel 発火。
fn heartbeat_firing_plan(
    resolved_enabled: bool,
    resolved_interval_secs: u64,
    global_enabled: bool,
) -> HeartbeatFiringPlan {
    if resolved_enabled {
        HeartbeatFiringPlan::AgentScoped {
            interval_secs: resolved_interval_secs,
        }
    } else if global_enabled {
        HeartbeatFiringPlan::ChannelScoped
    } else {
        HeartbeatFiringPlan::None
    }
}

/// 発火ループの sleep 周期（秒）を丸める（純粋関数）。0 は `heartbeat_loop` の
/// `sleep(0)` = 0 秒周期のビジーループになるため下限 1 秒に丸める（運用者が
/// `heartbeat_interval_secs = 0` を書いても最低 1 秒 sleep させる）。初期起動・reload の
/// 両経路で同じ丸めを使う。
fn heartbeat_loop_interval_secs(configured: u64) -> u64 {
    configured.max(1)
}

/// 発火ループの sleep 周期を「そのエージェントの実効的な最小間隔」へ**短くする方向に
/// だけ**追従させる（純粋関数・#439 の部分先行）。
///
/// 従来はグローバル `[agent] heartbeat_interval_secs`（実値 1800）で固定して眠っていた
/// ため、ゲート（`heartbeat_interval_elapsed`）が設定間隔を見ていても評価の機会自体が
/// 1800 秒グリッドに丸められ、1200 秒（20 分）設定が実質 30 分になっていた。設定の床は
/// `heartbeat_min_interval_secs = 300` なので、床と評価グリッドが自己矛盾していた。
///
/// - `candidates`: この周期で実際にゲートされる間隔群（agent-scope なら 1 つ、
///   channel-scope なら各チャンネルの実効間隔、発火しない周期なら空）。
/// - グローバルより**長い**候補で周期を伸ばさない。長い設定はゲートが弾けば済むので、
///   周期を伸ばすと発火を遅らせるだけになる。
/// - 下限は運用者の床（`min_interval_secs`）。床未満へは短くしない。床がグローバルより
///   長い設定でも周期をグローバルより長くはしない（従来どおりグローバル周期で回る）。
fn heartbeat_effective_loop_interval_secs(
    candidates: &[u64],
    global_interval_secs: u64,
    min_interval_secs: u64,
) -> u64 {
    let global = heartbeat_loop_interval_secs(global_interval_secs);
    let floor = min_interval_secs.max(1);
    let shortest = candidates
        .iter()
        .copied()
        .filter(|v| *v > 0)
        .min()
        .unwrap_or(global);
    // 短くする方向にだけ追従 → 床へ引き上げ → それでもグローバルは超えない。
    shortest.min(global).max(floor).min(global)
}

/// この周期でゲート対象になる間隔群を DB から解決する（#439 の部分先行）。
///
/// 発火計画（`heartbeat_firing_plan`）と同じ経路をたどるので、発火しない周期
/// （未 opt-in × グローバル無効）では空になり、周期はグローバルのままになる。
/// DB を読めなければ空を返す = グローバル周期に落ちる（従来の挙動）。
fn resolve_heartbeat_gate_intervals(
    db: &opencrab_db::Db,
    agent_id: &str,
    default_interval_secs: u64,
    min_interval_secs: u64,
    global_enabled: bool,
) -> Vec<u64> {
    let resolved = {
        let Ok(conn) = db.lock() else {
            return vec![];
        };
        opencrab_db::queries::resolve_agent_heartbeat(
            &conn,
            agent_id,
            default_interval_secs,
            min_interval_secs,
        )
    };
    match heartbeat_firing_plan(resolved.enabled, resolved.interval_secs, global_enabled) {
        HeartbeatFiringPlan::AgentScoped { interval_secs } => vec![interval_secs],
        HeartbeatFiringPlan::ChannelScoped => {
            let channels = list_whitelisted_heartbeat_channels(db, agent_id);
            let Ok(conn) = db.lock() else {
                return vec![];
            };
            channels
                .into_iter()
                .map(|(_cid, _name, _guild, ch_interval)| {
                    opencrab_db::queries::resolve_channel_heartbeat_interval(
                        &conn,
                        agent_id,
                        ch_interval,
                        default_interval_secs,
                        min_interval_secs,
                    )
                    .interval_secs
                })
                .collect()
        }
        HeartbeatFiringPlan::None => vec![],
    }
}

/// ループの sleep 周期を毎周期解決するクロージャを作る（#439 の部分先行）。
///
/// ループ生成時に固定しないので、設定変更は**次の周期から**効く。発火するかどうかの
/// 判定・位相は従来のゲートのまま（アンカー永続化・即時反映は #439 本体）。
fn make_heartbeat_interval_resolver(
    db: opencrab_db::Db,
    agent_id: String,
    default_interval_secs: u64,
    min_interval_secs: u64,
    global_interval_secs: u64,
    global_enabled: bool,
) -> HeartbeatIntervalResolver {
    Arc::new(move || {
        let candidates = resolve_heartbeat_gate_intervals(
            &db,
            &agent_id,
            default_interval_secs,
            min_interval_secs,
            global_enabled,
        );
        heartbeat_effective_loop_interval_secs(&candidates, global_interval_secs, min_interval_secs)
    })
}

/// 前回発火からの経過が `interval_secs` 以上かを判定する（純粋関数）。
/// 未発火（`last` が `None`）なら常に発火。エージェント単位 tick も channel tick も同じ
/// ゲートを使い、`interval_secs` に resolve の値を与えることで「保存済み間隔を実際に
/// 効かせる」（#251 段階3）。
fn heartbeat_interval_elapsed(
    last: Option<std::time::Instant>,
    now: std::time::Instant,
    interval_secs: u64,
) -> bool {
    match last {
        None => true,
        Some(last_time) => now.duration_since(last_time).as_secs() >= interval_secs,
    }
}

/// heartbeat_enabled = true のチャンネルを、当該エージェント向けに解決して返す
/// （channel_id, channel_name, guild_id, interval_secs）。グローバル設定（agent_id="")と
/// エージェント固有設定の両方が同一 channel_id に存在しうるため、(1) 当該エージェント
/// に無関係な行を除外し、(2) 同一 channel_id ではエージェント固有行をグローバル行より
/// 優先して重複処理を防ぐ。
///
/// 前提（既存 precedence / #238。本 PR で変えない）: 同一 channel に global 行
/// （agent_id="", heartbeat_enabled=1）と agent 固有行が併存する場合、agent 固有行を
/// `enabled=false` にすると (2) の dedup で固有行がリストから外れ、**代わりに global 行が
/// 採用されて発火が続く**。つまり「このチャンネルだけ自律を止める」は agent 固有行の
/// 無効化だけでは達成できない（global 行も無効化するか、global 行が無い前提が要る）。
/// 塞ぐと global 行で運用しているチャンネルの挙動が変わるため、本 PR では塞がない。
/// なお `channel_state_payload`（get_my_heartbeat scope=channel）は (channel_id, agent_id)
/// 固有行のみを読むので、global 行しか無いチャンネルでは get が `enabled=false` を返しても
/// 実発火は global 行で起こりうる（get の表示と実発火が乖離する edge）。
fn list_whitelisted_heartbeat_channels(
    db: &opencrab_db::Db,
    agent_id: &str,
) -> Vec<(String, String, String, Option<u64>)> {
    let conn = db.lock().unwrap();
    match opencrab_db::queries::list_heartbeat_channels(&conn) {
        Ok(channels) => {
            // channel_id -> 選択された行。エージェント固有行を優先。
            let mut selected: std::collections::HashMap<
                String,
                opencrab_db::queries::ChannelConfigRow,
            > = std::collections::HashMap::new();
            for c in channels {
                // 当該エージェント向けでもグローバルでもない行は無視。
                if !c.agent_id.is_empty() && c.agent_id != agent_id {
                    continue;
                }
                match selected.get(&c.channel_id) {
                    // 既にエージェント固有行を選択済みならグローバル行で上書きしない。
                    Some(existing) if !existing.agent_id.is_empty() && c.agent_id.is_empty() => {
                        continue;
                    }
                    _ => {
                        selected.insert(c.channel_id.clone(), c);
                    }
                }
            }
            selected
                .into_values()
                .map(|c| {
                    (
                        c.channel_id,
                        c.channel_name,
                        // #404: 実会話セッション ID（discord-{agent}-{guild}-{channel}）の
                        // 解決に使う。dedup 後の行から取るので、interval と同じ precedence。
                        c.guild_id,
                        c.heartbeat_interval_secs,
                    )
                })
                .collect()
        }
        Err(e) => {
            tracing::warn!("Failed to list whitelisted channels: {e}");
            vec![]
        }
    }
}

/// ハートビートコールバックを生成する。
/// 初期起動とhot-reload再起動の両方で使用。
fn make_heartbeat_callback(
    db: opencrab_db::Db,
    agent_id_owned: String,
    // #400: 全体で共有する 1 本のハンドルではなく、**発話する体ごとに**解決する口を持つ。
    discord_http: Arc<heartbeat_delivery::HeartbeatDiscordHttp>,
    state: AppState,
    global_interval_secs: u64,
    // グローバル `[agent] heartbeat_enabled` の実値（ループ起動時に強制 true へ倒す前の
    // 素の値）。未 opt-in エージェントの channel 単位発火は**これが true のときだけ**
    // 許す。グローバル無効下でループが立つのは他エージェントの opt-in のためだけであり、
    // その巻き添えで既定無効エージェントが channel 発火してはならない（#238 の挙動不変）。
    global_enabled: bool,
    last_channel_ticks: Arc<Mutex<std::collections::HashMap<String, std::time::Instant>>>,
) -> HeartbeatCallback {
    // ターンの実体（#440）。tick と、サブタスク決着からの継続ターンが**同じ 1 つ**を通る。
    // 直列化ロックと dispatch registry をここで共有することが、両者が同一 HB セッションで
    // 二重に応答しないことの担保になっている（`heartbeat_turn` モジュール doc）。
    let runner = heartbeat_turn::HeartbeatTurnRunner::from_state(&state, discord_http);
    Box::new(move |_agent_id: &str, tick: u64| {
        let _agent_id = _agent_id.to_string();
        let db = db.clone();
        let agent_id_owned = agent_id_owned.clone();
        let runner = runner.clone();
        let state = state.clone();
        let global_interval_secs = global_interval_secs;
        let global_enabled = global_enabled;
        let last_channel_ticks = last_channel_ticks.clone();
        Box::pin(async move {
            // #238 / #251 段階3: エージェント単位ハートビートの解決（fail-closed）。
            // opt-in 済み（resolved.enabled）なら「エージェント単位 tick」で 1 回だけ
            // 発火し、既存の channel 単位発火はこのエージェントについてスキップする
            // （二重発火防止の precedence）。未 opt-in なら従来の channel 単位発火のまま
            // （既定無効エージェントは 1 バイトも挙動が変わらない）。
            let resolved = {
                let conn = db.lock().unwrap();
                opencrab_db::queries::resolve_agent_heartbeat(
                    &conn,
                    &agent_id_owned,
                    state.heartbeat_limits.default_interval_secs,
                    state.heartbeat_limits.min_interval_secs,
                )
            };

            // 発火対象（channel_id, channel_name, guild_id, interval_secs）。channel_id が
            // 空文字なら「エージェント単位 tick」（channel を持たない発話）を表す。
            // guild_id は実会話セッションの解決（#404）にのみ使う。
            let targets: Vec<(String, String, String, Option<u64>)> = match heartbeat_firing_plan(
                resolved.enabled,
                resolved.interval_secs,
                global_enabled,
            ) {
                HeartbeatFiringPlan::AgentScoped { interval_secs } => {
                    // opt-in 済み: エージェント単位で 1 回発火。channel 発火はしない。
                    // 間隔は resolve の値（#251 段階3 の「保存済み間隔を実際に効かせる」）。
                    // 宛先 channel_id は空。deliver_heartbeat_speech が Nostr 稼働なら
                    // registry 経由で Nostr へ、Discord 共有 http フォールバックは空 channel
                    // で「ログのみ」に縮退する（Discord エージェントの代表 channel 選択は
                    // 別 PR / スコープ外）。
                    vec![(
                        String::new(),
                        HEARTBEAT_AGENT_SCOPED_LABEL.to_string(),
                        // channel を持たないので実会話セッションも解決しない（#404）。
                        String::new(),
                        Some(interval_secs),
                    )]
                }
                HeartbeatFiringPlan::ChannelScoped => {
                    // 未 opt-in かつグローバル有効: channel 単位発火。#336: 各チャンネルの
                    // 実効間隔を **channel → agent → 運用者既定** で解決し、下限へクランプ
                    // した値を Some で載せる（下段の `unwrap_or` はもう当たらない）。
                    // チャンネル未設定でエージェント設定も無ければ既定に落ちるので、
                    // 従来（channel か既定）の挙動を包含しつつ、床とエージェント設定
                    // フォールバックだけを足す。
                    let channels = list_whitelisted_heartbeat_channels(&db, &agent_id_owned);
                    let conn = db.lock().unwrap();
                    channels
                        .into_iter()
                        .map(|(cid, name, guild_id, ch_interval)| {
                            let resolved = opencrab_db::queries::resolve_channel_heartbeat_interval(
                                &conn,
                                &agent_id_owned,
                                ch_interval,
                                state.heartbeat_limits.default_interval_secs,
                                state.heartbeat_limits.min_interval_secs,
                            );
                            // #336: 下限へ引き上げた（source="clamped"）ことは実発火経路の
                            // ログには出ておらず、get_my_heartbeat scope=channel の payload
                            // でしか観測できなかった。実発火でも 1 行残す（新しい制約は
                            // 足さず、ログのみ）。元値=ch_interval（channel 未設定なら None、
                            // その場合は agent/既定側の値が床未満だった）、床=min_interval_secs。
                            if resolved.source == "clamped" {
                                tracing::debug!(
                                    agent_id = %agent_id_owned,
                                    channel_id = %cid,
                                    channel_interval_secs = ?ch_interval,
                                    floor_interval_secs = resolved.interval_secs,
                                    "channel heartbeat 間隔を下限へ引き上げた (source=clamped)"
                                );
                            }
                            (cid, name, guild_id, Some(resolved.interval_secs))
                        })
                        .collect()
                }
                HeartbeatFiringPlan::None => {
                    // 未 opt-in かつグローバル無効: 何もしない。このループは他エージェント
                    // の opt-in のために立っているだけで、既定無効エージェントは発火しない。
                    vec![]
                }
            };

            if targets.is_empty() {
                tracing::debug!(agent_id = %agent_id_owned, tick, "No heartbeat targets, skipping tick");
                return HeartbeatDecision::Idle;
            }

            // 最後の決定を返す（全ターゲットを処理した後）
            let mut last_decision = HeartbeatDecision::Idle;

            for (channel_id_str, channel_name, guild_id, channel_interval_secs) in &targets {
                // per-target interval チェック。エージェント単位 tick（空 channel_id）は
                // last_channel_ticks を channel_id と衝突しない合成キーで引く。
                let tick_key = if channel_id_str.is_empty() {
                    format!("agent:{agent_id_owned}")
                } else {
                    channel_id_str.clone()
                };
                let effective_interval = channel_interval_secs.unwrap_or(global_interval_secs);
                let should_fire = {
                    let ticks = last_channel_ticks.lock().unwrap();
                    heartbeat_interval_elapsed(
                        ticks.get(tick_key.as_str()).copied(),
                        std::time::Instant::now(),
                        effective_interval,
                    )
                };
                if !should_fire {
                    tracing::debug!(
                        agent_id = %agent_id_owned,
                        channel_id = %channel_id_str,
                        effective_interval,
                        "Heartbeat: interval not elapsed, skipping"
                    );
                    continue;
                }
                // last_tickを更新
                {
                    let mut ticks = last_channel_ticks.lock().unwrap();
                    ticks.insert(tick_key.clone(), std::time::Instant::now());
                }

                tracing::debug!(
                    agent_id = %agent_id_owned,
                    channel_id = %channel_id_str,
                    channel_name = %channel_name,
                    tick,
                    "Heartbeat tick for channel"
                );

                // 1. チャンネルごとのセッション取得/作成
                let session_id =
                    get_or_create_heartbeat_session(&db, &agent_id_owned, channel_id_str);

                // 2. ハートビートプロンプトをsession_logsに挿入
                //    指示部分（方針・頻度・トーン）は設定可能、出力形式の規約はランタイム固定。
                let hb_source;
                {
                    let conn = db.lock().unwrap();
                    let resolved = opencrab_db::queries::resolve_heartbeat_instructions(
                        &conn,
                        &agent_id_owned,
                        channel_id_str,
                    );
                    hb_source = resolved.source;
                    // 場所の呼称は transport 中立にする（#158 S2）。名前自体は設定由来。
                    let prompt = format!(
                        "[ハートビート] 現在の会話「{}」。{}\n出力形式: SPEAK/LEARN/IDLE のいずれか。SPEAKの場合のみ 'SPEAK: <メッセージ>' の形式で一言。",
                        channel_name, resolved.text
                    );
                    let log = opencrab_db::queries::SessionLogRow {
                        id: None,
                        agent_id: agent_id_owned.clone(),
                        session_id: session_id.clone(),
                        log_type: "system".to_string(),
                        content: prompt,
                        speaker_id: Some("heartbeat".to_string()),
                        turn_number: None,
                        metadata_json: None,
                        created_at: None,
                    };
                    if let Err(e) = opencrab_db::queries::insert_session_log(&conn, &log) {
                        tracing::error!(agent_id = %agent_id_owned, "Failed to insert heartbeat prompt log: {e}");
                        continue;
                    }
                }

                // 3-7. ターン本体（文脈構築 → 推論 → 応答記録 → 決定の解釈 →
                // heartbeat_log → 発話配送）は tick と継続ターンで共有する 1 実装を通る
                // （#440 / `heartbeat_turn`）。同一 HB セッションの直列化もその中。
                let turn_target = heartbeat_turn::HeartbeatTarget {
                    agent_id: agent_id_owned.clone(),
                    session_id: session_id.clone(),
                    channel_id: channel_id_str.clone(),
                    guild_id: guild_id.clone(),
                    instructions_source: hb_source,
                };
                let Some(decision) = runner
                    .run_turn(&turn_target, heartbeat_turn::TurnOrigin::Tick { tick })
                    .await
                else {
                    // 文脈を組めなかった（移設前もここは `continue` で、直前の決定を保つ）。
                    continue;
                };

                last_decision = decision;
            }

            last_decision
        })
    })
}

/// config名またはUUIDのagent_idを、DBのUUIDに解決する。
/// "crab"のような名前が渡された場合、find_agentsで検索してUUIDを返す。
fn resolve_agent_id(conn: &rusqlite::Connection, agent_id: &str) -> String {
    // まず直接lookupを試みる
    if let Ok(Some(_)) = opencrab_db::queries::get_agent(conn, agent_id) {
        return agent_id.to_string();
    }
    // 名前で検索（完全一致またはUUID前方一致のみ。部分一致は複数エージェント時に誤マッチするため使わない）
    if let Ok(agents) = opencrab_db::queries::find_agents(conn, agent_id) {
        if let Some((uuid, _name)) = agents.iter().find(|(id, name)| {
            id.starts_with(agent_id) || name.to_lowercase() == agent_id.to_lowercase()
        }) {
            tracing::info!(config_id = %agent_id, uuid = %uuid, "Resolved agent_id config name to UUID (exact match)");
            return uuid.clone();
        }
    }
    // シングルエージェントフォールバック: DBに登録済みのエージェントが1つだけの場合はそれを使う
    // (config名"crab"などがDBの名前と一致しない場合の対応)
    if let Ok(all_agents) = opencrab_db::queries::find_agents(conn, "") {
        if all_agents.len() == 1 {
            let (uuid, name) = &all_agents[0];
            tracing::info!(
                config_id = %agent_id,
                uuid = %uuid,
                name = %name,
                "Resolved agent_id to only registered agent (single-agent fallback)"
            );
            return uuid.clone();
        }
    }
    tracing::warn!(agent_id = %agent_id, "Could not resolve agent_id to UUID, using as-is");
    agent_id.to_string()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load .env file if present
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("opencrab=info".parse()?))
        .init();

    tracing::info!("Starting OpenCrab server...");

    // Load config from TOML (with env var expansion)
    let cfg = config::load_config("config/default.toml")?;

    // 新しい通知先キーを空 url で書くと、旧キーの有効な値が黙って無効化される（#207）。
    // 挙動（新キー優先 / 空 url は無効）は意図したものなので変えず、気づけるようにだけする。
    cfg.warn_if_legacy_webhook_masked();

    // DB初期化（本番はコネクションプール）
    let db = opencrab_db::Db::open(&cfg.database.path)?;

    // Build LLM router from config + DB のダッシュボード設定オーバーライド
    let llm_overrides = {
        let conn = db.lock().unwrap();
        opencrab_db::queries::list_llm_provider_overrides(&conn).unwrap_or_default()
    };
    let effective_llm = config::apply_llm_overrides(&cfg.llm, &llm_overrides);
    let llm_router = config::build_llm_router(&effective_llm)?;

    // 実効 voice 設定: DB オーバーライド（完全置換）> TOML。
    // 起動時の VC ランタイム構築にのみ使う（discord feature 無効時は未使用）。
    #[cfg_attr(not(feature = "discord"), allow(unused_variables))]
    let effective_voice: opencrab_voice::VoiceConfig = {
        let conn = db.lock().unwrap();
        match opencrab_db::queries::get_voice_config_override(&conn) {
            Ok(Some(json)) => serde_json::from_str(&json).unwrap_or_else(|e| {
                tracing::warn!("voice_config_override JSON is broken; using TOML: {e}");
                cfg.voice.clone()
            }),
            _ => cfg.voice.clone(),
        }
    };

    let default_model = format!("{}:{}", cfg.llm.default_provider, cfg.llm.default_model);

    // NOTE: エージェント個別の許可コマンド（DB管理）はグローバル tools_config に
    // マージしない。全エージェントの許可が混ざり、あるエージェントの許可が他へ漏れるため。
    // 個別コマンドは実行時に run_agent_response 内でそのエージェント分だけ適用する。
    let tools_cfg = cfg.tools.clone();

    #[allow(unused_mut)]
    let mut state = AppState {
        db,
        llm_router: opencrab_server::SharedLlmRouter::new(llm_router),
        llm_config: Arc::new(cfg.llm.clone()),
        // 非ブロック dispatch の kill switch（`[subtask] auto_dispatch` / env 上書き）。
        subtask_auto_dispatch: cfg.subtask.auto_dispatch,
        // 純 TOML を保持する（DB オーバーライド適用前の土台）。API の GET は
        // DB 行が無いときこれを "toml" として返すため、リセット後に古い実効値を
        // TOML と誤表示しないよう effective ではなく cfg.voice を入れる（レビュー指摘）。
        voice_config: Arc::new(cfg.voice.clone()),
        voice_runtime: Arc::new(std::sync::Mutex::new(None)),
        workspace_base: cfg.agent.workspace_path.clone(),
        tools_config: Arc::new(std::sync::RwLock::new(tools_cfg)),
        default_model,
        compaction_ratio: cfg.llm.compaction_ratio,
        evaluator: cfg.evaluator.clone(),
        skill_consolidation: cfg.skill_consolidation.clone(),
        category_maintenance: cfg.category_maintenance.clone(),
        memory_organize: cfg.memory_organize.clone(),
        memory_declare: cfg.memory_declare.clone(),
        memory_condense: cfg.memory_condense.clone(),
        loop_restart_enabled: cfg.agent.loop_restart_enabled,
        index_build_inflight: Arc::new(dashmap::DashMap::new()),
        intake: Arc::new(cfg.intake.clone()),
        mcp_manager: None,
        // 受信を持つ transport の登録簿（#191 段階2 PR2）。空で作り、各マネージャの
        // 生成箇所から後で `register` する（内部可変なので生成順を変えずに済む）。
        gateways: Arc::new(opencrab_actions::AgentGatewayRegistry::new()),
        web_gateway: Arc::new(opencrab_web_gateway::WebGateway::new()),
        subtask_registries: Arc::new(opencrab_server::subtask_registries::SubtaskRegistries::new()),
        progress_debounce: Arc::new(opencrab_server::subtask_registries::ProgressDebounce::new()),
        subtask_notifiers: Arc::new(dashmap::DashMap::new()),
        subtask_lifecycle_notifier: Arc::new(Mutex::new(None)),
        // 設定ファイル由来の通知先フォールバック（#157 S5）。**Discord 機能フラグの
        // 外**で 1 度だけ解決し、以降の利用者（gateway 非依存の管理ツール / lifecycle
        // 通知 / Discord gateway_actions）は全てこの 1 つの値を参照する。
        default_subtask_webhook: cfg.default_subtask_webhook(),
        // エージェントが自分で触るハートビート設定の境界（#247）。下限は運用者が
        // `[agent] heartbeat_min_interval_secs` で決める。
        heartbeat_limits: cfg.agent.heartbeat_limits(),
    };

    // サブタスク lifecycle 通知の実装を配線する（#175 S4）。`spawn_subtask` は gateway
    // 非依存層にあるため、通知先の解決（DB の webhook 設定 + TOML の既定）だけを持つ
    // この実装を `AppState` へ差し込む。Discord ゲートウェイの稼働有無とは独立に効く
    // （web / REST から起動したサブタスクにも lifecycle 通知が出る）。
    #[cfg(feature = "discord")]
    {
        let default_subtask_webhook = state.default_subtask_webhook.clone();
        *state.subtask_lifecycle_notifier.lock().unwrap() = Some(Arc::new(
            opencrab_discord::DiscordWebhookNotifier::new(
                state.db.clone(),
                default_subtask_webhook,
            ),
        )
            as Arc<dyn opencrab_actions::subtask_notify::SubtaskLifecycleNotifier>);
    }

    // 前プロセスから残った保留対話を**期限切れとして明示的に閉じる**（#196）。
    // 保留状態のメモリ上の登録簿はプロセスと寿命を共にするので、ここに残っている
    // `pending` 行は誰も応答を受け取れない。無言で放置すると「ボタンを押しても何も
    // 起きない」行が DB に溜まり続けるため、起動時に 1 度だけ閉じてログに残す。
    // transport に依存しない処理なので、Discord 機能フラグやゲートウェイの稼働有無の
    // **外**で行う（nostr / web / REST だけの構成でも効く）。
    {
        use opencrab_actions::AgentRuntime as _;
        state.cleanup_stale_interactions();
    }

    // 共有（TOML）ゲートウェイのハンドル。登録簿（`state.gateways`）には載らないので
    // ここで直接持つ（理由は `heartbeat_delivery` モジュール doc）。
    let shared_discord_http: DiscordHttpArc = Arc::new(Mutex::new(None));
    // ハートビート発話の Discord ハンドル解決口（#400）。**発話する体ごと**に
    // per-agent ゲートウェイ → 共有ゲートウェイの順で配送時に引く。
    let heartbeat_discord_http = Arc::new(heartbeat_delivery::HeartbeatDiscordHttp::new(
        shared_discord_http.clone(),
    ));

    // Start Discord gateway if configured and feature is enabled.
    #[cfg(feature = "discord")]
    {
        // Per-agent Discord gateway manager（#40: 共有ループが「専用ゲートウェイが
        // 稼働中か」を参照できるよう、共有ゲートウェイへ渡す AppState clone より
        // **前に**生成して配線する。実際の復元は共有ゲートウェイ起動後に行う）。
        let manager = Arc::new(opencrab_discord::DiscordGatewayManager::new(state.clone()));
        // 上位から見える唯一の入口はこの登録簿（#191 段階2 PR3・PR4）。共通操作
        // （起動 / 停止 / 生存確認）も transport 固有の操作（ツール実行の実体 =
        // `gateway_actions_for`）もここから引く。`AppState` の名指しフィールドは無い。
        // 登録簿は `state` の clone 同士で同じ Arc を共有するので、ここで入れた分は
        // 既に clone 済みの state からも見える（#40 の二重処理防止がこれに依存する）。
        state.gateways.register(manager.clone());

        let discord_cfg = &cfg.gateway.discord;

        // owner 未設定は「無音で権限モデルが変わる」ので起動時に必ず知らせる。
        // `.env` の OWNER_DISCORD_ID を入れ忘れると `${OWNER_DISCORD_ID}` が空文字に
        // 展開され、設定ファイルを見ても気づけない。
        // 共有ゲートウェイが実際に起動する条件（enabled かつトークンあり）でだけ出す。
        // per-agent ゲートウェイ側の警告は DiscordGatewayManager::start_agent_gateway が出す。
        opencrab_discord::warn_if_shared_gateway_owner_unset(
            discord_cfg.enabled,
            &discord_cfg.token,
            &discord_cfg.owner_discord_id,
        );

        // Fallback: config-based shared gateway (existing behavior).
        // 起動条件は警告条件と同じ述語を使う（条件の二重管理を避ける。理由は
        // `gateway_will_start` の doc コメント参照）。
        if opencrab_discord::gateway_will_start(discord_cfg.enabled, &discord_cfg.token) {
            tracing::info!("Starting Discord gateway (config-based fallback)...");

            // Validate agent IDs against the database
            let valid_agent_ids: Vec<String> = {
                let conn = state.db.lock().unwrap();
                let ids: Vec<String> = discord_cfg
                    .agent_ids
                    .iter()
                    .map(|agent_id| resolve_agent_id(&conn, agent_id))
                    .filter(
                        |agent_id| match opencrab_db::queries::get_agent(&conn, agent_id) {
                            Ok(Some(_)) => true,
                            _ => {
                                tracing::warn!(
                                    "Agent '{}' not found in database, skipping",
                                    agent_id
                                );
                                false
                            }
                        },
                    )
                    .collect();
                // #40: enabled な per-agent Discord 設定を持つエージェントは、専用
                // ゲートウェイの**稼働中**は共有ループが per-message でスキップする
                // （liveness ベース）。ここでリストから除外はしない: 専用側が起動失敗
                // した場合に共有側がフォールバックとして応答を続けるため。
                for agent_id in &ids {
                    if matches!(
                        opencrab_db::queries::get_agent_discord_config(&conn, agent_id),
                        Ok(Some(cfg)) if cfg.enabled
                    ) {
                        tracing::info!(
                            agent_id = %agent_id,
                            "Agent has an enabled per-agent Discord config; shared gateway \
                             will defer to it while its dedicated gateway is running"
                        );
                    }
                }
                ids
            };

            if valid_agent_ids.is_empty() {
                tracing::error!("No valid agents found for Discord gateway, not starting");
            } else {
                let gateway = Arc::new(opencrab_gateway::DiscordGateway::new(&discord_cfg.token));
                gateway.start().await?;

                // auto-dispatch の登録簿。停止（`cancel_subtask`）は gateway 非依存層の実装が
                // 同じ Arc を run 経由（`RunRequest::with_dispatch`）で受け取るため、この
                // registry はループへ渡すだけでよい（#157 S2 で gateway_actions からは外した）。
                let subtask_registry: opencrab_actions::SubtaskRegistry =
                    Arc::new(dashmap::DashMap::new());
                let subtask_registry_for_loop = subtask_registry.clone();
                // subtask 完了/進捗の通知はイベントループへの直接送信になった（#39）ため、
                // gateway_actions とループで同じチャンネルを共有する必要がある。
                let (event_tx, event_rx) = opencrab_discord::message_loop::create_event_channel();
                // 設定ファイル由来の通知先フォールバック（#157 S5 で `AppState` へ
                // 持ち上げ済み）。Discord にはもう `ensure_*` しか残っていないが、
                // 解決経路が全 transport で同じ値を見ることをここで担保する。
                let default_subtask_webhook = state.default_subtask_webhook.clone();
                // VC 対話（STT/TTS）: 実効設定（DB オーバーライド適用済み）で構築する。
                // プロバイダ構築失敗（未知の provider 等）は起動を止めず警告して無効化。
                let voice_cfg = &effective_voice;
                let voice_manager: Option<
                    Arc<opencrab_discord::voice_session::VoiceSessionManager>,
                > = if voice_cfg.enabled {
                    match (
                        opencrab_voice::build_stt(&voice_cfg.stt),
                        opencrab_voice::build_tts(&voice_cfg.tts),
                    ) {
                        (Ok(stt), Ok(tts)) => {
                            tracing::info!(
                                stt = %voice_cfg.stt.provider,
                                tts = %voice_cfg.tts.provider,
                                "voice (VC) conversation enabled"
                            );
                            let mgr = opencrab_discord::voice_session::VoiceSessionManager::new(
                                gateway.voice(),
                                stt,
                                tts,
                                voice_cfg.tts.clone(),
                                voice_cfg.stt.language.clone(),
                                event_tx.clone(),
                                gateway.http().clone(),
                            );
                            // ダッシュボードからの設定変更をホットスワップで受ける
                            *state.voice_runtime.lock().unwrap() =
                                Some(mgr.clone() as Arc<dyn opencrab_voice::VoiceRuntime>);
                            Some(mgr)
                        }
                        (stt, tts) => {
                            if let Err(e) = stt {
                                tracing::warn!("voice STT provider init failed: {e}");
                            }
                            if let Err(e) = tts {
                                tracing::warn!("voice TTS provider init failed: {e}");
                            }
                            None
                        }
                    }
                } else {
                    None
                };

                let gateway_actions_base = opencrab_discord::DiscordGatewayActions::new(
                    gateway.http().clone(),
                    state.db.clone(),
                    state.workspace_base.clone(),
                    default_subtask_webhook,
                )
                .with_event_tx(event_tx.clone())
                .with_owner_discord_id(discord_cfg.owner_discord_id.clone());
                let gateway_actions: Arc<dyn opencrab_gateway::GatewayActions> =
                    Arc::new(match &voice_manager {
                        Some(v) => gateway_actions_base.with_voice(v.clone()),
                        None => gateway_actions_base,
                    });

                *shared_discord_http.lock().unwrap() = Some(gateway.http().clone());

                let discord_state = state.clone();
                let owner_discord_id = discord_cfg.owner_discord_id.clone();
                tokio::spawn(async move {
                    opencrab_discord::run_discord_loop(
                        gateway,
                        discord_state,
                        valid_agent_ids,
                        gateway_actions,
                        owner_discord_id,
                        None, // pending_registry
                        Some((event_tx, event_rx)),
                        // 共有（TOML）ゲートウェイ: ランタイムに per-agent 設定が
                        // enable されたエージェントはメッセージ処理時にスキップ（#40）。
                        true,
                        voice_manager,
                        subtask_registry_for_loop,
                    )
                    .await;
                });

                tracing::info!(
                    agents = ?discord_cfg.agent_ids,
                    owner = %discord_cfg.owner_discord_id,
                    "Discord gateway started (config-based)"
                );
            }
        }

        // **1 つ目の復元位置。** ここまでに登録簿へ入っていて、まだ復元していない
        // ゲートウェイを登録順に復元する（#191 段階2 PR5）。この時点で登録済みなのは
        // 上で登録した 1 つだけなので、実際に走る内容は移設前の
        // `manager.restore_from_db()` と 1 対 1。
        //
        // **この位置は動かせない**（走査を最後の 1 回に畳めない理由でもある）:
        // 1. 復元は共有（TOML）ゲートウェイの**起動後**。起動直後の短い窓では共有側が
        //    メッセージを処理し、専用ゲートウェイが上がり次第 per-message スキップが効く。
        // 2. 下の起動時診断（どの体で Discord ハンドルが解決できるか）が、この復元の
        //    **完了**を前提にしている。#400 以降、実際の解決は配送のたびに行うので
        //    「復元が後ろへずれると発話が共有ゲートウェイの HTTP のまま固定される」
        //    という取り返しのつかない依存は無くなったが、診断の意味は復元後にしかない。
        state.gateways.restore_pending().await;

        // heartbeat の Discord ハンドルを **per-agent ゲートウェイから体ごとに**引ける
        // ようにする（#400）。
        //
        // 以前はここで `agent_ids.first()` の 1 体だけを解決して 1 本のハンドルを全体で
        // 共有していた。`Http` はボットのトークンを保持する＝送信者名を決めるので、
        // (1) Discord へ発話できるのは先頭の体だけ、(2) 先頭以外の体が Discord チャンネル
        // へ向いていればその発話は先頭の体の名前で出る、という並び順依存になっていた。
        // 引き口だけ渡し、**どの体のハンドルを使うかは配送時に発話者で決める**。
        //
        // **ここは登録簿の走査に畳んでいない**（#191 段階2 PR5）。必要なのは
        // `Arc<serenity::http::Http>` そのもので、heartbeat の発話経路（`SPEAK:`）が
        // serenity の API を直に叩く Discord 専用コードだから。PR4 の capability
        // （`gateway_actions_for`）が返すのは `GatewayActions` であって生の HTTP
        // クライアントではなく、ここに当てると発話経路ごと書き換えになる（挙動不変で
        // なくなる）。transport 中立化は heartbeat 側の課題として残す。
        heartbeat_discord_http.set_per_agent_source(manager.clone());

        tracing::info!("Per-agent Discord gateway manager initialized");
    }

    // メモリインデックスのアイドル時メンテナンス（増分ビルドの取りこぼし回収 /
    // キーワードバックフィル / 月次ロールアップ）。全エージェントを毎 tick 巡回。
    if cfg.agent.memory_maintenance_enabled {
        opencrab_server::memory_maintenance::spawn_memory_maintenance_loop(
            state.clone(),
            cfg.agent.memory_maintenance_interval_secs,
        );
    }

    // 古い llm_logs の zip アーカイブ（#337）。メンテナンスループが per-agent かつ
    // 高頻度（既定 600 秒）なのに対し、こちらは全 llm_logs を対象にした日次の重い I/O
    // なので別ループにする。出力先は未指定なら DB ファイルの親 + `archive`（DB と同じ
    // ボリューム = 内蔵ディスクに置かない方針）へ導出する。
    if cfg.llm_log_archive.enabled {
        let archive_dir = if cfg.llm_log_archive.dir.trim().is_empty() {
            std::path::Path::new(&cfg.database.path)
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join("archive")
        } else {
            std::path::PathBuf::from(&cfg.llm_log_archive.dir)
        };
        opencrab_server::llm_log_archive::spawn_llm_log_archive_loop(
            state.db.clone(),
            archive_dir,
            cfg.llm_log_archive.retention_days,
            cfg.llm_log_archive.interval_secs,
        );
    }

    // 外部イベント受信（webhook intake / #454）。
    //
    // 消化ループは heartbeat の起動条件（グローバル有効 or opt-in）に**依存させない**。
    // heartbeat ループは有効なエージェントが居ないと張られず、そこに inbox 消化を相乗り
    // させると webhook 対象エージェントの heartbeat が無効なとき黙って消化されない
    // （silent no-op）。専用ループにして常時起動し、未処理が空なら LLM を呼ばない
    // （per-agent の非空ゲート / 受け入れ基準）。消化ターンは heartbeat の SPEAK 配送を
    // 通さない（webhook 起点の外部 broadcast を避ける）— 詳細は intake_process モジュール doc。
    intake_process::spawn_intake_process_loop(state.clone());
    // catch-up ポーリング（起動時 + 定期）。source アダプタ未設定なら中で即 return する。
    opencrab_server::intake::spawn_intake_catchup_loop(state.clone());

    // ハートビートの初期設定
    let initial_hb_config = HeartbeatConfig {
        interval_secs: cfg.agent.heartbeat_interval_secs,
        enabled: cfg.agent.heartbeat_enabled,
    };

    let (heartbeat_config_tx, mut heartbeat_config_rx) = watch::channel(initial_hb_config.clone());

    let agent_ids: Vec<String> = {
        let base: Vec<String> = {
            #[cfg(feature = "discord")]
            {
                cfg.gateway.discord.agent_ids.clone()
            }
            #[cfg(not(feature = "discord"))]
            {
                vec!["default".to_string()]
            }
        };
        // #238: エージェント単位ハートビートに opt-in 済み（agent_heartbeat_config で
        // enabled）のエージェントにも発火ループを立てる。これで Discord チャンネルに
        // 紐づかない Nostr 専用エージェントにもループが立つ。config 由来の id は
        // 名前かもしれないので resolve して UUID で重複除去する（同一エージェントに
        // 二重にループを立てて二重発火させない）。DB 有効化の**動的**反映はスコープ外
        // （起動時列挙のみ。反映は再起動時 / #251 の set_my_heartbeat 応答と整合）。
        if let Ok(conn) = state.db.lock() {
            let mut covered: std::collections::HashSet<String> =
                base.iter().map(|id| resolve_agent_id(&conn, id)).collect();
            let mut out = base;
            match opencrab_db::queries::list_agents_with_heartbeat_enabled(&conn) {
                Ok(enabled_ids) => {
                    for id in enabled_ids {
                        let resolved = resolve_agent_id(&conn, &id);
                        if covered.insert(resolved) {
                            out.push(id);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to list heartbeat-enabled agents: {e}");
                }
            }
            out
        } else {
            base
        }
    };

    // #400: ハートビートを回す体ごとに、Discord ハンドルが解決できるかを起動時に 1 行残す。
    // 以前は先頭 1 体の解決に失敗しても `if let Some` が外れるだけで**何も出ず**、
    // 「Discord へは一切出ない構成のまま動いている」ことに気づけなかった（配送時の WARN は
    // 発火してから出るもので、起動時のハンドル未解決そのものは可視化されていなかった）。
    // ハンドルの実際の解決は配送時に行うので、ここはあくまで起動時点のスナップショット。
    //
    // Discord feature 無効ビルドでは per-agent も共有もハンドルが存在しようがなく、
    // 「解決できない」WARN が毎起動エージェントの数だけ出るだけの雑音になるので診断ごと落とす。
    #[cfg(feature = "discord")]
    {
        let resolved: Vec<String> = match state.db.lock() {
            Ok(conn) => agent_ids
                .iter()
                .map(|id| resolve_agent_id(&conn, id))
                .collect(),
            // DB を引けなければ config の表記のままで診断する（診断のために起動を止めない）。
            Err(_) => agent_ids.clone(),
        };
        for agent_id in &resolved {
            heartbeat_delivery::log_startup_http_resolution(&heartbeat_discord_http, agent_id);
        }
    }

    // #238: agent_heartbeat_config に enabled 行が 1 つでもあるか。グローバル無効でも
    // opt-in 済みエージェントが居ればループ群を起動する二段ゲートの上段（下段＝個々の
    // 発火可否は callback 内 resolve_agent_heartbeat が握る）。起動時 1 回だけ判定する
    // （動的反映はスコープ外）。
    let heartbeat_has_optin = match state.db.lock() {
        Ok(conn) => opencrab_db::queries::list_agents_with_heartbeat_enabled(&conn)
            .map(|v| !v.is_empty())
            .unwrap_or(false),
        Err(_) => false,
    };

    // heartbeat設定変更を監視してループを再起動するタスク
    let heartbeat_agent_ids = agent_ids.clone();
    let heartbeat_db = state.db.clone();
    let heartbeat_state = state.clone();
    tokio::spawn(async move {
        let mut prev_config = initial_hb_config.clone();
        let mut current_shutdown_tx: Option<watch::Sender<bool>> = None;
        let mut _handles: Vec<tokio::task::JoinHandle<()>> = vec![];

        // ハートビートループを起動するヘルパークロージャ的ブロック
        // 初期起動。グローバル有効 OR opt-in 済みエージェントが居ればループ群を起動する。
        if prev_config.enabled || heartbeat_has_optin {
            tracing::info!(
                agent_ids = ?heartbeat_agent_ids,
                interval_secs = prev_config.interval_secs,
                global_enabled = prev_config.enabled,
                has_optin = heartbeat_has_optin,
                "Starting heartbeat loops"
            );
            let global_enabled = prev_config.enabled;
            let (tx, rx_tmpl) = watch::channel(false);
            for agent_id in &heartbeat_agent_ids {
                // core heartbeat_loop は config.enabled=false で即 return するため、起動する
                // ループの enabled は true に倒す。個々の発火可否（グローバル無効下の未
                // opt-in エージェントを黙らせる等）は callback 内で fail-closed に判定する。
                // interval_secs は下限 1 秒に丸める（0 だと heartbeat_loop の sleep が 0 秒
                // 周期のビジーループになる。運用者が 0 を書いても最低 1 秒 sleep させる）。
                let config_clone = HeartbeatConfig {
                    interval_secs: heartbeat_loop_interval_secs(prev_config.interval_secs),
                    enabled: true,
                };
                let shutdown_rx = rx_tmpl.clone();
                let db = heartbeat_db.clone();
                let db_for_resolve = heartbeat_db.clone();
                let agent_id = agent_id.clone();
                let hb_discord_http = heartbeat_discord_http.clone();
                let state_for_hb = heartbeat_state.clone();
                let last_channel_ticks = Arc::new(Mutex::new(std::collections::HashMap::<
                    String,
                    std::time::Instant,
                >::new()));
                _handles.push(tokio::spawn(async move {
                    let resolved_agent_id = if let Ok(conn) = db_for_resolve.lock() {
                        resolve_agent_id(&conn, &agent_id)
                    } else {
                        agent_id.clone()
                    };
                    // #439 部分先行: 眠る長さを設定間隔へ追従させる（グローバル 1800 秒
                    // グリッドへの丸めをやめる）。毎周期 DB から解決し直す。
                    let limits = state_for_hb.heartbeat_limits;
                    let interval_resolver = make_heartbeat_interval_resolver(
                        db.clone(),
                        resolved_agent_id.clone(),
                        limits.default_interval_secs,
                        limits.min_interval_secs,
                        config_clone.interval_secs,
                        global_enabled,
                    );
                    let callback = make_heartbeat_callback(
                        db,
                        resolved_agent_id,
                        hb_discord_http,
                        state_for_hb,
                        config_clone.interval_secs,
                        global_enabled,
                        last_channel_ticks,
                    );
                    heartbeat_loop(
                        agent_id,
                        config_clone,
                        callback,
                        Some(interval_resolver),
                        shutdown_rx,
                    )
                    .await;
                }));
            }
            current_shutdown_tx = Some(tx);
        } else {
            tracing::info!("Heartbeat disabled (heartbeat_enabled = false in config)");
        }

        loop {
            if heartbeat_config_rx.changed().await.is_err() {
                break; // sender dropped
            }
            let new_config = heartbeat_config_rx.borrow().clone();
            if new_config.enabled != prev_config.enabled
                || new_config.interval_secs != prev_config.interval_secs
            {
                tracing::info!(
                    enabled = new_config.enabled,
                    interval_secs = new_config.interval_secs,
                    "Heartbeat config changed, restarting loops"
                );
                // 既存ループを停止
                if let Some(tx) = current_shutdown_tx.take() {
                    let _ = tx.send(true);
                }
                _handles.clear();

                // 新設定で起動。初期起動と同じ二段ゲート（グローバル有効 OR opt-in）。
                if new_config.enabled || heartbeat_has_optin {
                    let global_enabled = new_config.enabled;
                    let (tx, rx_tmpl) = watch::channel(false);
                    for agent_id in &heartbeat_agent_ids {
                        // core の early-return 回避のため enabled は true に倒す（初期起動と同じ）。
                        // interval_secs は下限 1 秒に丸める（0 でビジーループ化を防ぐ・初期起動と同じ）。
                        let config_clone = HeartbeatConfig {
                            interval_secs: heartbeat_loop_interval_secs(new_config.interval_secs),
                            enabled: true,
                        };
                        let shutdown_rx = rx_tmpl.clone();
                        let db = heartbeat_db.clone();
                        let db_for_resolve = heartbeat_db.clone();
                        let agent_id = agent_id.clone();
                        let hb_discord_http = heartbeat_discord_http.clone();
                        let state_for_hb = heartbeat_state.clone();
                        let last_channel_ticks = Arc::new(Mutex::new(std::collections::HashMap::<
                            String,
                            std::time::Instant,
                        >::new(
                        )));
                        _handles.push(tokio::spawn(async move {
                            let resolved_agent_id = if let Ok(conn) = db_for_resolve.lock() {
                                resolve_agent_id(&conn, &agent_id)
                            } else {
                                agent_id.clone()
                            };
                            // #439 部分先行: 初期起動と同じく設定間隔へ追従させる。
                            let limits = state_for_hb.heartbeat_limits;
                            let interval_resolver = make_heartbeat_interval_resolver(
                                db.clone(),
                                resolved_agent_id.clone(),
                                limits.default_interval_secs,
                                limits.min_interval_secs,
                                config_clone.interval_secs,
                                global_enabled,
                            );
                            let callback = make_heartbeat_callback(
                                db,
                                resolved_agent_id,
                                hb_discord_http,
                                state_for_hb,
                                config_clone.interval_secs,
                                global_enabled,
                                last_channel_ticks,
                            );
                            heartbeat_loop(
                                agent_id,
                                config_clone,
                                callback,
                                Some(interval_resolver),
                                shutdown_rx,
                            )
                            .await;
                        }));
                    }
                    current_shutdown_tx = Some(tx);
                }
                prev_config = new_config;
            }
        }
    });

    let _watcher_handle = opencrab_server::hot_reload::start_config_watcher(
        "config",
        state.db.clone(),
        // #412: 「default_model が変わったか」の基準。稼働中の実効 spec そのものを渡す
        // （上の `format!("{provider}:{model}")` と同じ形でないと永久に不一致になる）。
        state.default_model.clone(),
        state.tools_config.clone(),
        heartbeat_config_tx,
    );

    // Per-agent Nostr sub-gateway マネージャ（discord と同様に、state clone より前に
    // 生成して配線する。nostr は重い依存が無いので feature ゲート無しの常時配線）。
    {
        // nostaro は**エージェントの workspace ルートを cwd にして**起動する（#299）。
        // `execute_shell` / `ws_*` と同じ `agent.workspace_path` を渡して基準を揃える
        // （`nostr_run event --file <相対>` / `--out <相対>` がそれらと噛み合う）。
        let cli =
            opencrab_nostr::NostaroCli::new().with_workspace_base(state.workspace_base.clone());
        let manager: opencrab_server::SharedNostrManager =
            Arc::new(opencrab_nostr::NostrGatewayManager::new(state.clone()).with_cli(cli));
        // 共通操作も transport 固有の操作（nostaro の鍵生成 = `key_provisioning`）も
        // この登録簿から引く（#191 段階2 PR3・PR4）。名指しフィールドは無い。
        state.gateways.register(manager);
    }

    // **2 つ目の復元位置**（ルータ構築の直前 / #191 段階2 PR5）。ここまでで未復元なのは
    // 直前に登録した Nostr だけなので（Discord は上のブロックで復元済み・MCP は登録簿に
    // 入れない）、移設前の `manager.restore_from_db()` と 1 対 1。
    //
    // Discord を落とした構成（`--no-default-features`）では 1 つ目の走査ごと消えるため、
    // ここが唯一の復元位置になる。**新しい transport を足すときも呼び出し口は増えない**:
    // 復元させたい位置より前で `register` すればよい。
    state.gateways.restore_pending().await;

    // Per-agent MCP 接続マネージャ。enabled なサーバへ起動時に接続する。
    //
    // **transport 登録簿（`state.gateways`）には入れない**（#191 段階2）。MCP は受信を
    // 持たず、エージェントへ道具を供給する側で transport ではない。道具の注入は
    // 深さ 0（親ターン）限定という遮断が効いており、「受信を持つ transport」と同じ
    // 登録簿に混ぜるとその前提が崩れる。名指しフィールドのまま残す。
    {
        let manager: opencrab_server::SharedMcpManager =
            Arc::new(opencrab_mcp::McpClientManager::new(state.db.clone()));
        state.mcp_manager = Some(manager.clone());
        manager.restore_from_db().await;
        // 自己修復: 切断された（クラッシュ/終了した）サーバを周期的に再接続する。
        let sweeper = manager.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            tick.tick().await; // 最初の即時発火を捨てる
            loop {
                tick.tick().await;
                sweeper.reconnect_dead().await;
            }
        });
    }

    let app = create_router(state);

    let addr = format!("0.0.0.0:{}", cfg.gateway.rest.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("Server listening on {}", addr);
    axum::serve(listener, app).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // #169 の dispatch 配線（`RunRequest` に registry と sink が載る / registry は agent
    // 単位で共有）と、#440 の継続ターン配線のテストは `heartbeat_turn` へ移した。
    // `RunRequest` を組むのがそちらになったため。

    // ── #238 発火のエージェント単位化: precedence と間隔ゲート ──────────────

    /// (b)(d) opt-in 済み（resolved.enabled）は **エージェント単位で発火**し、channel 発火
    /// はしない。間隔は resolve の値がそのまま実効間隔になる（#251 段階3）。
    #[test]
    fn firing_plan_optin_is_agent_scoped_and_uses_resolved_interval() {
        // グローバル有効でも無効でも、opt-in が最優先で AgentScoped。
        for global in [true, false] {
            assert_eq!(
                heartbeat_firing_plan(true, 900, global),
                HeartbeatFiringPlan::AgentScoped { interval_secs: 900 },
                "opt-in 済みは channel 発火より優先してエージェント単位で発火（二重発火なし）"
            );
        }
        // 実効間隔は resolve の値をそのまま反映する。
        assert_eq!(
            heartbeat_firing_plan(true, 1800, true),
            HeartbeatFiringPlan::AgentScoped {
                interval_secs: 1800
            }
        );
    }

    /// (c) 未 opt-in はグローバル有効時のみ従来の channel 発火（互換）。グローバル無効なら
    /// 何も発火しない（既定無効エージェントは挙動不変）。
    #[test]
    fn firing_plan_non_optin_preserves_legacy_behavior() {
        assert_eq!(
            heartbeat_firing_plan(false, 900, true),
            HeartbeatFiringPlan::ChannelScoped,
            "未 opt-in × グローバル有効 = 従来の channel 発火"
        );
        assert_eq!(
            heartbeat_firing_plan(false, 900, false),
            HeartbeatFiringPlan::None,
            "未 opt-in × グローバル無効 = 発火しない（挙動不変）"
        );
    }

    /// 指摘#4: 発火ループの sleep 周期は下限 1 秒に丸める（0 でビジーループ化しない）。
    #[test]
    fn loop_interval_floors_zero_to_one_second() {
        assert_eq!(
            heartbeat_loop_interval_secs(0),
            1,
            "0 は 0 秒 sleep のビジーループになるので 1 秒に丸める"
        );
        assert_eq!(heartbeat_loop_interval_secs(1), 1);
        assert_eq!(heartbeat_loop_interval_secs(29), 29, "1 以上はそのまま");
    }

    /// (d) 間隔ゲート: 未発火なら常に発火。経過が interval 以上で発火、未満はスキップ。
    #[test]
    fn interval_elapsed_gates_on_resolved_interval() {
        use std::time::{Duration, Instant};
        let now = Instant::now();
        // 未発火は常に発火。
        assert!(heartbeat_interval_elapsed(None, now, 300));
        // 100 秒前に発火・間隔 300 → まだ。
        let last = now - Duration::from_secs(100);
        assert!(!heartbeat_interval_elapsed(Some(last), now, 300));
        // 300 秒前に発火・間隔 300 → 発火（>=）。
        let last = now - Duration::from_secs(300);
        assert!(heartbeat_interval_elapsed(Some(last), now, 300));
        // 間隔を縮めれば（resolve の値が効く）同じ経過でも発火する。
        let last = now - Duration::from_secs(100);
        assert!(heartbeat_interval_elapsed(Some(last), now, 60));
    }

    /// #404: 文脈に実会話を入れても `SPEAK:` / `LEARN` / `IDLE` の解釈は変わらない。
    /// パースの入力は応答テキストだけ（プロンプトに何を積んだかに依存しない）。
    #[test]
    fn heartbeat_decision_parse_depends_only_on_response_text() {
        assert!(matches!(
            parse_heartbeat_decision("SPEAK: おはよう"),
            HeartbeatDecision::Speak(c) if c == "おはよう"
        ));
        // 前置きの行があっても SPEAK: を含む行から取る。
        assert!(matches!(
            parse_heartbeat_decision("考えた結果\nSPEAK: 今日は静かだ\n"),
            HeartbeatDecision::Speak(c) if c == "今日は静かだ"
        ));
        // 中身が空なら発話しない。
        assert!(matches!(
            parse_heartbeat_decision("SPEAK:   "),
            HeartbeatDecision::Idle
        ));
        assert!(matches!(
            parse_heartbeat_decision("learn"),
            HeartbeatDecision::Learn
        ));
        assert!(matches!(
            parse_heartbeat_decision("IDLE"),
            HeartbeatDecision::Idle
        ));
        // 実会話が文脈に入っても、応答が SPEAK: を含まなければ発話にはならない
        // （実会話側の引用が誤って発話へ昇格しないこと）。
        assert!(matches!(
            parse_heartbeat_decision("チャンネルでは雑談が続いている。今は黙っておく。"),
            HeartbeatDecision::Idle
        ));
    }

    /// #404: 実会話セッション ID は `discord-{agent}-{guild}-{channel}`。
    /// エージェント単位 tick（channel_id / guild_id が空）では解決しない。
    #[test]
    fn heartbeat_channel_session_id_resolves_only_for_channel_ticks() {
        assert_eq!(
            opencrab_server::process::heartbeat_channel_session_id("agent-a", "111", "222"),
            Some("discord-agent-a-111-222".to_string())
        );
        assert_eq!(
            opencrab_server::process::heartbeat_channel_session_id("agent-a", "", ""),
            None
        );
        assert_eq!(
            opencrab_server::process::heartbeat_channel_session_id("agent-a", "111", ""),
            None
        );
        assert_eq!(
            opencrab_server::process::heartbeat_channel_session_id("agent-a", "", "222"),
            None
        );
    }

    /// #425: HB 発話の会話セッションへの二重記録は **Discord へ配信できたターンだけ**行う。
    /// (a) 配信成功 → 本人の `discord-…` 会話セッションへ `speech` が印つきで載る（修正前は
    /// 1 行も無く、本人が自分の投稿を思い出せない再現ケース）。(b) 配信失敗 → 載せない
    /// （言っていないことを記憶に残さない）。guild 無しのエージェント単位 tick → 記録先が
    /// 無いので何もしない。
    #[test]
    fn heartbeat_channel_echo_recorded_only_on_delivery() {
        let db = opencrab_db::Db::memory().unwrap();

        // (a) 配信成功
        record_heartbeat_channel_echo(&db, true, "agent-a", "111", "222", "自律発話A");
        // (b) 配信失敗 → 記録しない
        record_heartbeat_channel_echo(&db, false, "agent-a", "111", "222", "配信失敗の発話");
        // guild 無し（エージェント単位 tick）→ 記録先が無いので何もしない
        record_heartbeat_channel_echo(&db, true, "agent-a", "", "", "guild無しの発話");

        let conn = db.lock().unwrap();
        let logs =
            opencrab_db::queries::list_session_logs_by_session(&conn, "discord-agent-a-111-222")
                .unwrap();

        assert_eq!(
            logs.len(),
            1,
            "配信成功ターンだけが会話セッションへ載る（失敗・guild無しは載らない）: {logs:?}"
        );
        let row = &logs[0];
        assert_eq!(row.log_type, "speech");
        assert_eq!(row.content, "自律発話A");
        assert_eq!(row.speaker_id.as_deref(), Some("agent-a"));
        assert_eq!(
            row.metadata_json.as_deref(),
            Some(opencrab_db::queries::HEARTBEAT_CHANNEL_ECHO_METADATA),
            "HB 二重記録の印が付く（HB 経路の実会話セクションで二重表示を除外するため）"
        );
        // 印は is_heartbeat_channel_echo で表示専用（FTS・索引・宣言材料から除外）と判定される。
        assert!(
            opencrab_db::queries::is_heartbeat_channel_echo(row.metadata_json.as_deref()),
            "記録した印は is_heartbeat_channel_echo で表示専用と判定される"
        );
    }

    // ── #439 部分先行: 評価グリッドを設定間隔へ追従させる ────────────────────

    /// 運用実値に合わせた境界値（グローバル 1800 秒 / 床 300 秒）。
    const TEST_GLOBAL_INTERVAL: u64 = 1800;
    const TEST_MIN_INTERVAL: u64 = 300;

    /// テスト用に「本番と同じ経路」の resolver を作る。ループ起動時に 1 回だけ作られる
    /// ものと同じで、以降は毎回 DB を読み直す。
    fn test_interval_resolver(
        db: &opencrab_db::Db,
        agent_id: &str,
        global_enabled: bool,
    ) -> HeartbeatIntervalResolver {
        make_heartbeat_interval_resolver(
            db.clone(),
            agent_id.to_string(),
            TEST_GLOBAL_INTERVAL,
            TEST_MIN_INTERVAL,
            TEST_GLOBAL_INTERVAL,
            global_enabled,
        )
    }

    fn set_agent_heartbeat(db: &opencrab_db::Db, agent_id: &str, interval_secs: Option<i64>) {
        let conn = db.lock().unwrap();
        opencrab_db::queries::upsert_agent_heartbeat_config(
            &conn,
            &opencrab_db::queries::AgentHeartbeatConfigRow {
                agent_id: agent_id.to_string(),
                enabled: true,
                interval_secs,
            },
        )
        .unwrap();
    }

    fn set_heartbeat_channel(
        db: &opencrab_db::Db,
        channel_id: &str,
        agent_id: &str,
        interval_secs: Option<u64>,
    ) {
        let conn = db.lock().unwrap();
        opencrab_db::queries::upsert_channel_config(
            &conn,
            &opencrab_db::queries::ChannelConfigRow {
                channel_id: channel_id.to_string(),
                agent_id: agent_id.to_string(),
                guild_id: "111".to_string(),
                channel_name: "ch".to_string(),
                readable: true,
                writable: true,
                whitelisted: true,
                heartbeat_enabled: true,
                heartbeat_interval_secs: interval_secs,
                heartbeat_instructions: String::new(),
            },
        )
        .unwrap();
    }

    /// (a) agent-scope 1200 秒（20 分）の設定があれば、ループの次回 sleep は 1200 になる。
    /// 修正前はグローバル 1800 で固定されていたので、この assert は 1800 で落ちる
    /// （＝ 20 分設定が実質 30 分に丸められていた再現ケース）。
    #[test]
    fn loop_interval_follows_agent_scope_setting() {
        let db = opencrab_db::Db::memory().unwrap();
        set_agent_heartbeat(&db, "agent-a", Some(1200));

        let resolve = test_interval_resolver(&db, "agent-a", true);
        assert_eq!(
            resolve(),
            1200,
            "20 分設定なら 20 分ごとに評価する（1800 グリッドへ丸めない）"
        );
    }

    /// (b) 床（300 秒）未満の設定は床へクランプする。240 秒設定でも 240 秒ごとには
    /// 回さない。
    #[test]
    fn loop_interval_clamps_to_floor() {
        let db = opencrab_db::Db::memory().unwrap();
        set_agent_heartbeat(&db, "agent-a", Some(240));

        let resolve = test_interval_resolver(&db, "agent-a", true);
        assert_eq!(resolve(), TEST_MIN_INTERVAL, "床未満へは短くしない");
    }

    /// (c) 設定が無ければグローバル 1800 のまま（挙動不変）。グローバル有効・無効の
    /// どちらでも変わらない。
    #[test]
    fn loop_interval_stays_global_without_settings() {
        let db = opencrab_db::Db::memory().unwrap();
        for global_enabled in [true, false] {
            let resolve = test_interval_resolver(&db, "agent-a", global_enabled);
            assert_eq!(
                resolve(),
                TEST_GLOBAL_INTERVAL,
                "設定が無ければ従来どおりグローバル周期（global_enabled={global_enabled}）"
            );
        }
    }

    /// (d) 同じ resolver（ループ生成時に 1 回だけ作られるもの）が、周期中の設定変更を
    /// 次の周期の sleep に反映する。ループ生成時に間隔を固定しないことの担保。
    #[test]
    fn loop_interval_reresolves_each_cycle() {
        let db = opencrab_db::Db::memory().unwrap();
        set_agent_heartbeat(&db, "agent-a", Some(1200));

        let resolve = test_interval_resolver(&db, "agent-a", true);
        assert_eq!(resolve(), 1200);

        // 周期の途中で設定を変える。
        set_agent_heartbeat(&db, "agent-a", Some(600));
        assert_eq!(resolve(), 600, "次周期の sleep に反映される");

        // 伸ばす方向の変更も、グローバルを超えない範囲では反映される。
        set_agent_heartbeat(&db, "agent-a", Some(900));
        assert_eq!(resolve(), 900);
    }

    /// 未 opt-in（channel-scope 発火）でも、チャンネルの設定間隔に追従する。
    /// 複数チャンネルなら最短に合わせる（どのチャンネルの発火も遅らせないため）。
    #[test]
    fn loop_interval_follows_shortest_channel_scope_setting() {
        let db = opencrab_db::Db::memory().unwrap();
        set_heartbeat_channel(&db, "222", "agent-a", Some(1200));
        set_heartbeat_channel(&db, "333", "agent-a", Some(600));

        let resolve = test_interval_resolver(&db, "agent-a", true);
        assert_eq!(resolve(), 600, "最短のチャンネル設定に合わせる");
    }

    /// opt-in 済みなら channel 発火はしない（`heartbeat_firing_plan` の precedence）ので、
    /// 周期も agent-scope の値だけで決まる。より短い channel 設定に引きずられない。
    #[test]
    fn loop_interval_ignores_channels_when_agent_scoped() {
        let db = opencrab_db::Db::memory().unwrap();
        set_agent_heartbeat(&db, "agent-a", Some(1200));
        set_heartbeat_channel(&db, "222", "agent-a", Some(300));

        let resolve = test_interval_resolver(&db, "agent-a", true);
        assert_eq!(
            resolve(),
            1200,
            "opt-in 済みは agent-scope 発火のみ。発火しない channel 設定で周期を縮めない"
        );
    }

    /// グローバル無効 × 未 opt-in は発火しない周期なので、周期はグローバルのまま。
    /// 他エージェントの opt-in のために立っているループを、無関係な channel 設定で
    /// 細かく回さない。
    #[test]
    fn loop_interval_stays_global_when_nothing_fires() {
        let db = opencrab_db::Db::memory().unwrap();
        set_heartbeat_channel(&db, "222", "agent-a", Some(600));

        let resolve = test_interval_resolver(&db, "agent-a", false);
        assert_eq!(resolve(), TEST_GLOBAL_INTERVAL);
    }

    /// 方向: **短くする方向にだけ**追従する。グローバルより長い設定でループを伸ばすと
    /// 発火が遅れるだけなので、周期はグローバルのまま（長い設定はゲートが弾く）。
    #[test]
    fn loop_interval_never_stretches_beyond_global() {
        let db = opencrab_db::Db::memory().unwrap();
        set_agent_heartbeat(&db, "agent-a", Some(3600));

        let resolve = test_interval_resolver(&db, "agent-a", true);
        assert_eq!(
            resolve(),
            TEST_GLOBAL_INTERVAL,
            "1 時間設定でもループはグローバル周期で回る（発火はゲートが 1 時間で弾く）"
        );
    }

    /// 純粋関数の境界: 候補なし・0 混入・床がグローバルより長い設定でも破綻しない。
    #[test]
    fn effective_loop_interval_edges() {
        // 候補なし → グローバル。
        assert_eq!(heartbeat_effective_loop_interval_secs(&[], 1800, 300), 1800);
        // 0 は候補として無視する（ビジーループにしない）。
        assert_eq!(
            heartbeat_effective_loop_interval_secs(&[0, 900], 1800, 300),
            900
        );
        assert_eq!(
            heartbeat_effective_loop_interval_secs(&[0], 1800, 300),
            1800,
            "0 しか無ければ候補なしと同じ"
        );
        // 床 > グローバル でも周期をグローバルより長くしない（clamp の順序で破綻しない）。
        assert_eq!(
            heartbeat_effective_loop_interval_secs(&[100], 60, 300),
            60,
            "床がグローバルより長くても、周期はグローバルを超えない"
        );
        // グローバル 0 は 1 秒へ丸める（既存の heartbeat_loop_interval_secs と同じ）。
        assert_eq!(heartbeat_effective_loop_interval_secs(&[], 0, 300), 1);
    }
}
