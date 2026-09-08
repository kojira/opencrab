use super::*;

/// #698: 元栓の許可集合を構築する。フォロイー（relay 由来 / `fetch_following`）＋ owner /
/// co_agent / trusted_users（DB 由来 / `nostr_gate_allow_keys`）を [`follow_key`] で正規化して
/// 1 つの [`AllowSources`] に合成する。
///
/// **取得の失敗はすべて `Err`**（呼び出し側が起動中止 or 前回値保持で fail-loud に扱う）:
/// フォローリスト（relay）取得の失敗も、DB 由来キーの取得失敗（lock poison / query Err）も、
/// どちらもここで `?` により `Err` になる。DB 側を `Ok(空)` に握り潰さないことで、owner/trusted が
/// DB エラーで無音でキャッシュから消えるのを防ぐ（「未登録＝空」と「DB 故障＝読めない」を区別）。
/// DB 由来のキーは**単一源**（判定は resolve_nostr_caller と同じ DB 表を材料にする）。正規化は
/// ここ 1 箇所に閉じる（author_key と同じ `follow_key`。書き手と読み手のキーずれ事故を防ぐ）。
pub(super) async fn build_allow_sources<R: NostrAgentRunner>(
    runner: &R,
    cli: &NostaroCli,
    agent_id: &str,
) -> anyhow::Result<AllowSources> {
    // relay 由来（fail-loud）。
    let followees: HashSet<String> = cli
        .fetch_following(agent_id)
        .await?
        .iter()
        .map(|k| crate::pubkey::follow_key(k))
        .collect();
    // DB 由来（owner / co_agent / trusted_users）。DB 故障は `?` で伝播（Ok(空) に化けさせない）。
    let db = runner.nostr_gate_allow_keys(agent_id)?;
    let to_set = |v: &[String]| -> HashSet<String> {
        v.iter().map(|s| crate::pubkey::follow_key(s)).collect()
    };
    Ok(AllowSources {
        followees,
        owner: to_set(&db.owner),
        co_agents: to_set(&db.co_agents),
        trusted_users: to_set(&db.trusted_users),
    })
}

/// V3 gateway が ingress/delivery を担う間、core の権威 allow-set を定期更新する。
pub(super) async fn run_v3_core_keep_alive<R: NostrAgentRunner + Clone>(
    runner: R,
    cli: NostaroCli,
    agent_id: String,
    allow: AllowGate,
    store: AllowSetStore,
) {
    let mut ticker = tokio::time::interval_at(
        tokio::time::Instant::now() + std::time::Duration::from_secs(300),
        std::time::Duration::from_secs(300),
    );
    loop {
        ticker.tick().await;
        match build_allow_sources(&runner, &cli, &agent_id).await {
            Ok(next) => {
                *allow.write().unwrap() = next.clone();
                store.replace_allow(&agent_id, next);
            }
            Err(error) => warn!(
                agent_id = %agent_id,
                error = %format!("{error:#}"),
                "Nostr V3 allow-set refresh failed; retaining previous authoritative set"
            ),
        }
    }
}
