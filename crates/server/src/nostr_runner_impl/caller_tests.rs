use super::resolve_nostr_caller_identity;
use opencrab_actions::CallerIdentity;
use opencrab_db::queries::{
    AgentNostrConfigRow, TrustedUserPermission, TRUSTED_PLATFORM_DISCORD, TRUSTED_PLATFORM_NOSTR,
};
use rusqlite::Connection;

/// ダミー鍵（実在の pubkey は書かない）。
const OWNER_HEX: &str = "0000000000000000000000000000000000000000000000000000000000000001";
const STRANGER_HEX: &str = "0000000000000000000000000000000000000000000000000000000000000002";
const FRIEND_HEX: &str = "0000000000000000000000000000000000000000000000000000000000000003";
const AGENT: &str = "agent-1";

/// Nostr 設定行だけがある DB（オーナーは未設定）。
fn db_with_nostr_row() -> Connection {
    let conn = opencrab_db::init_memory().unwrap();
    opencrab_db::queries::upsert_agent_nostr_config(
        &conn,
        &AgentNostrConfigRow {
            agent_id: AGENT.to_string(),
            secret_key: "nsec1dummy".to_string(),
            relays_json: "[]".to_string(),
            filter_json: "{}".to_string(),
            enabled: false,
        },
    )
    .unwrap();
    conn
}

fn set_owner(conn: &Connection, pubkey: &str) {
    assert!(
        opencrab_db::queries::set_agent_nostr_owner_pubkey(conn, AGENT, pubkey).unwrap(),
        "オーナーの保存に失敗（設定行が無い）"
    );
}

fn register(conn: &Connection, platform: &str, user_id: &str, perm: TrustedUserPermission) {
    opencrab_db::queries::add_trusted_user(
        conn,
        platform,
        &format!("row-{platform}-{user_id}"),
        AGENT,
        user_id,
        perm,
        "owner",
        "2026-01-01",
        "",
    )
    .unwrap();
}

fn npub_of(hex: &str) -> String {
    opencrab_nostr::to_npub(hex).expect("npub へ変換できること")
}

/// **本丸**: オーナーの pubkey から来たターンは `Owner`。
#[test]
fn owner_pubkey_resolves_to_owner() {
    let conn = db_with_nostr_row();
    set_owner(&conn, OWNER_HEX);
    assert_eq!(
        resolve_nostr_caller_identity(&conn, AGENT, OWNER_HEX),
        CallerIdentity::Owner
    );
}

/// **本丸**: 他人の pubkey は `Agent` のまま（昇格しない）。
#[test]
fn other_pubkey_stays_agent() {
    let conn = db_with_nostr_row();
    set_owner(&conn, OWNER_HEX);
    assert_eq!(
        resolve_nostr_caller_identity(&conn, AGENT, STRANGER_HEX),
        CallerIdentity::Agent
    );
}

/// **fail-closed**: オーナー未設定なら誰も Owner にならない。
#[test]
fn unset_owner_grants_owner_to_nobody() {
    let conn = db_with_nostr_row();
    // 設定行はあるが owner_pubkey は既定の空。
    for pk in [OWNER_HEX, STRANGER_HEX] {
        assert_eq!(
            resolve_nostr_caller_identity(&conn, AGENT, pk),
            CallerIdentity::Agent,
            "オーナー未設定なのに Owner になった: {pk}"
        );
    }
    // 空白のみも未設定として扱う。
    set_owner(&conn, "   ");
    assert_eq!(
        resolve_nostr_caller_identity(&conn, AGENT, OWNER_HEX),
        CallerIdentity::Agent
    );
    // Nostr 設定行そのものが無いエージェントも同じ（オーナー未設定）。
    assert_eq!(
        resolve_nostr_caller_identity(&conn, "agent-without-nostr", OWNER_HEX),
        CallerIdentity::Agent
    );
}

/// **本丸**: npub で設定して hex で受信しても一致する（表現差で取りこぼさない）。
#[test]
fn owner_set_as_npub_matches_hex_speaker() {
    let conn = db_with_nostr_row();
    // 入口の正規化を通さず、npub のまま入っている行（手書き / 旧データ）でも拾う。
    set_owner(&conn, &npub_of(OWNER_HEX));
    assert_eq!(
        resolve_nostr_caller_identity(&conn, AGENT, OWNER_HEX),
        CallerIdentity::Owner,
        "npub で設定したオーナーが hex の受信で一致しない"
    );
}

/// 逆向き: hex で設定して npub で来ても一致する。
#[test]
fn owner_set_as_hex_matches_npub_speaker() {
    let conn = db_with_nostr_row();
    set_owner(&conn, OWNER_HEX);
    assert_eq!(
        resolve_nostr_caller_identity(&conn, AGENT, &npub_of(OWNER_HEX)),
        CallerIdentity::Owner,
        "hex で設定したオーナーが npub の発言者と一致しない"
    );
    // 別人の npub は依然として Agent。
    assert_eq!(
        resolve_nostr_caller_identity(&conn, AGENT, &npub_of(STRANGER_HEX)),
        CallerIdentity::Agent
    );
}

/// **識別子空間の分離**: `platform='discord'` の行は Nostr の照合に混ざらない。
///
/// 一意制約が今も `(user_id, agent_id)`（#159 の残作業）なので、同じ識別子を
/// 2 経路に同時登録できない。DB を分けて「同じ識別子でも経路が違えば効かない」
/// ことを見る。
#[test]
fn discord_trusted_rows_do_not_leak_into_nostr() {
    let discord_only = db_with_nostr_row();
    register(
        &discord_only,
        TRUSTED_PLATFORM_DISCORD,
        FRIEND_HEX,
        TrustedUserPermission::User,
    );
    assert_eq!(
        resolve_nostr_caller_identity(&discord_only, AGENT, FRIEND_HEX),
        CallerIdentity::Agent,
        "Discord 経路の行が Nostr の照合に混ざった"
    );
    // co-agent 権限でも同じ（Discord の行から Nostr で CoAgent にならない）。
    let discord_coagent = db_with_nostr_row();
    register(
        &discord_coagent,
        TRUSTED_PLATFORM_DISCORD,
        FRIEND_HEX,
        TrustedUserPermission::CoAgent,
    );
    assert_eq!(
        resolve_nostr_caller_identity(&discord_coagent, AGENT, FRIEND_HEX),
        CallerIdentity::Agent
    );

    // 同じ識別子を Nostr 経路の行として登録すると、初めて信頼される。
    let nostr_row = db_with_nostr_row();
    register(
        &nostr_row,
        TRUSTED_PLATFORM_NOSTR,
        FRIEND_HEX,
        TrustedUserPermission::User,
    );
    assert_eq!(
        resolve_nostr_caller_identity(&nostr_row, AGENT, FRIEND_HEX),
        CallerIdentity::TrustedUser
    );
}

/// Nostr 経路の行が npub で登録されていても、hex の発言者で引き当たる。
#[test]
fn trusted_row_registered_as_npub_matches_hex_speaker() {
    let conn = db_with_nostr_row();
    register(
        &conn,
        TRUSTED_PLATFORM_NOSTR,
        &npub_of(FRIEND_HEX),
        TrustedUserPermission::User,
    );
    assert_eq!(
        resolve_nostr_caller_identity(&conn, AGENT, FRIEND_HEX),
        CallerIdentity::TrustedUser
    );
}

/// **昇格経路を新設しない**: 表の `owner` 権限では `Owner` にならない
/// （Nostr で Owner になれるのは「オーナー pubkey と一致した」ときだけ）。
#[test]
fn trusted_row_with_owner_permission_does_not_become_owner() {
    let conn = db_with_nostr_row();
    register(
        &conn,
        TRUSTED_PLATFORM_NOSTR,
        STRANGER_HEX,
        TrustedUserPermission::Owner,
    );
    assert_eq!(
        resolve_nostr_caller_identity(&conn, AGENT, STRANGER_HEX),
        CallerIdentity::TrustedUser,
        "表の owner 権限から Owner へ上がる道ができている"
    );
}

/// 壊れた発言者識別子は最小権限（偶然一致させない）。
#[test]
fn malformed_speaker_is_least_privileged() {
    let conn = db_with_nostr_row();
    set_owner(&conn, OWNER_HEX);
    for bad in ["", "   ", "not-a-key", "npub1broken"] {
        assert_eq!(
            resolve_nostr_caller_identity(&conn, AGENT, bad),
            CallerIdentity::Agent,
            "壊れた識別子が最小権限に落ちない: {bad:?}"
        );
    }
}

/// オーナーは**そのエージェントの設定**で決まる（他エージェントへ波及しない）。
#[test]
fn owner_is_scoped_to_the_agent() {
    let conn = db_with_nostr_row();
    set_owner(&conn, OWNER_HEX);
    opencrab_db::queries::upsert_agent_nostr_config(
        &conn,
        &AgentNostrConfigRow {
            agent_id: "agent-2".to_string(),
            secret_key: "nsec1dummy2".to_string(),
            relays_json: "[]".to_string(),
            filter_json: "{}".to_string(),
            enabled: false,
        },
    )
    .unwrap();
    assert_eq!(
        resolve_nostr_caller_identity(&conn, "agent-2", OWNER_HEX),
        CallerIdentity::Agent,
        "別エージェントのオーナー設定が波及した"
    );
}

// ---- #489: co_agent の識別子逆引き ----

const FRIEND_AGENT_UUID: &str = "friend-agent-uuid";

/// 送信側 agent（`agent_id`）が `self_pubkey` を接続で書いた状態にする。
fn set_self_pubkey(conn: &Connection, agent_id: &str, pubkey: &str) {
    opencrab_db::queries::upsert_agent_nostr_config(
        conn,
        &AgentNostrConfigRow {
            agent_id: agent_id.to_string(),
            secret_key: "nsec1sender".to_string(),
            relays_json: "[]".to_string(),
            filter_json: "{}".to_string(),
            enabled: false,
        },
    )
    .unwrap();
    assert!(
        opencrab_db::queries::set_agent_nostr_self_pubkey(conn, agent_id, pubkey).unwrap(),
        "self_pubkey の保存に失敗（設定行が無い）"
    );
}

/// AGENT が co_agent として FRIEND_AGENT_UUID を信頼登録する（owner 登録の模擬）。
fn trust_co_agent(conn: &Connection, agent_id: &str, co_agent_uuid: &str) {
    opencrab_db::queries::insert_trusted_co_agent(
        conn,
        &opencrab_db::queries::TrustedCoAgentRow {
            id: format!("row-{agent_id}-{co_agent_uuid}"),
            agent_id: agent_id.to_string(),
            co_agent_id: co_agent_uuid.to_string(),
            allowed_actions: None,
            created_by: "owner".to_string(),
            created_at: "2026-01-01".to_string(),
        },
    )
    .unwrap();
}

/// **本丸（#489）**: UUID 対で登録した co_agent が、発言者 pubkey → UUID の逆引きで発火する。
#[test]
fn co_agent_resolves_via_reverse_lookup() {
    let conn = db_with_nostr_row();
    // 送信側 agent が自 pubkey（FRIEND_HEX）を接続で登録済み。
    set_self_pubkey(&conn, FRIEND_AGENT_UUID, FRIEND_HEX);
    // AGENT は FRIEND_AGENT_UUID を co_agent（owner 等価）として登録。
    trust_co_agent(&conn, AGENT, FRIEND_AGENT_UUID);
    // FRIEND_HEX から届いたターンは、逆引きで FRIEND_AGENT_UUID に解決され CoAgent。
    assert_eq!(
        resolve_nostr_caller_identity(&conn, AGENT, FRIEND_HEX),
        CallerIdentity::CoAgent {
            agent_id: FRIEND_AGENT_UUID.to_string()
        },
        "UUID 登録の co_agent が逆引きで発火しない（#489 の本体）"
    );
    // npub 表記で来ても同じ（正規化して逆引き）。
    assert_eq!(
        resolve_nostr_caller_identity(&conn, AGENT, &npub_of(FRIEND_HEX)),
        CallerIdentity::CoAgent {
            agent_id: FRIEND_AGENT_UUID.to_string()
        }
    );
}

/// **fail-closed（#489）**: 送信側が未接続で self_pubkey が空なら、co_agent にならない。
#[test]
fn co_agent_fail_closed_when_self_pubkey_absent() {
    let conn = db_with_nostr_row();
    // 登録はあるが、送信側 agent の self_pubkey は未設定（未接続 = 逆引き不可）。
    trust_co_agent(&conn, AGENT, FRIEND_AGENT_UUID);
    assert_eq!(
        resolve_nostr_caller_identity(&conn, AGENT, FRIEND_HEX),
        CallerIdentity::Agent,
        "self_pubkey 空でも co_agent に化けた（fail-closed 違反）"
    );
}

/// **fail-closed（#489）**: 逆引きは成立するが、その UUID を co_agent 登録していなければ Agent。
#[test]
fn co_agent_fail_closed_when_reverse_maps_to_untrusted_uuid() {
    let conn = db_with_nostr_row();
    // 送信側は自 pubkey を登録済み（逆引きは成立）だが、AGENT は誰も co_agent 登録していない。
    set_self_pubkey(&conn, FRIEND_AGENT_UUID, FRIEND_HEX);
    assert_eq!(
        resolve_nostr_caller_identity(&conn, AGENT, FRIEND_HEX),
        CallerIdentity::Agent,
        "未登録の UUID が co_agent になった（誤許可）"
    );
}

// ---- #698: 元栓の DB 由来許可源の材料化 ----

/// **本丸（#698）**: `nostr_gate_allow_keys_from_db` が owner / co_agent /
/// trusted_users(platform=nostr) を材料化する。**discord 経路の trusted_user は混ぜない**。
#[test]
fn gate_allow_keys_materializes_owner_coagent_and_nostr_trusted_only() {
    let conn = db_with_nostr_row();
    set_owner(&conn, OWNER_HEX);
    // nostr の trusted_user（許可源に載る）。
    register(
        &conn,
        TRUSTED_PLATFORM_NOSTR,
        STRANGER_HEX,
        TrustedUserPermission::User,
    );
    // discord の trusted_user（経路が違うので**載らない**）。
    register(
        &conn,
        TRUSTED_PLATFORM_DISCORD,
        FRIEND_HEX,
        TrustedUserPermission::User,
    );
    // co_agent（UUID 対 → 相手の self_pubkey が FRIEND_HEX）。
    set_self_pubkey(&conn, FRIEND_AGENT_UUID, FRIEND_HEX);
    trust_co_agent(&conn, AGENT, FRIEND_AGENT_UUID);

    let keys = super::nostr_gate_allow_keys_from_db(&conn, AGENT).unwrap();
    assert_eq!(
        keys.owner,
        vec![OWNER_HEX.to_string()],
        "owner が材料化されない"
    );
    assert_eq!(
        keys.co_agents,
        vec![FRIEND_HEX.to_string()],
        "co_agent の self_pubkey が材料化されない"
    );
    assert_eq!(
        keys.trusted_users,
        vec![STRANGER_HEX.to_string()],
        "nostr の trusted_user だけを材料化していない（discord が混ざる / 抜ける）"
    );
}

/// fail-closed（#698）: owner 未設定・trusted/co_agent 無しなら全部空
/// （フォロイー ∪ owner はフォローリスト側で担保され、ここが空でも allow-all にならない）。
#[test]
fn gate_allow_keys_empty_when_nothing_registered() {
    let conn = db_with_nostr_row();
    let keys = super::nostr_gate_allow_keys_from_db(&conn, AGENT).unwrap();
    assert!(keys.owner.is_empty());
    assert!(keys.co_agents.is_empty());
    assert!(keys.trusted_users.is_empty());
}
