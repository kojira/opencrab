use super::*;

fn manifest(token: [u8; 32], id: &str, expires_at: i64) -> CredentialManifest {
    CredentialManifest {
        principal_id: id.to_owned(),
        token: Zeroizing::new(token),
        operations: [Operation::InstanceRead].into_iter().collect(),
        subject_ids: [1].into_iter().collect(),
        instance_ids: [Uuid::nil()].into_iter().collect(),
        creation_namespace: None,
        expires_at,
        rotation: None,
    }
}

#[tokio::test]
async fn protected_router_serves_all_six_scoped_operations_with_database_credential() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let db = opencrab_db::Db::memory().unwrap();
    let token = [13_u8; 32];
    {
        let mut conn = db.lock().unwrap();
        conn.execute("INSERT INTO agents(agent_id,name,persona_name,subject_id) VALUES ('agent-a','a','a',1)", []).unwrap();
        let mut credential = manifest(token, "protected", 4_000_000_000_000_000_000);
        credential.operations = Operation::ALL.into_iter().collect();
        bootstrap(&mut conn, &credential, 100).unwrap();
    }
    let state = std::sync::Arc::new(crate::registry::ExtgateState::new_protected(db));
    let app = crate::admin::admin_router(state.clone());
    let bearer = format!("Bearer {}", URL_SAFE_NO_PAD.encode(token));
    let instance = "/api/gate-instances/00000000-0000-0000-0000-000000000000";
    let binding = "/api/gate-bindings/00000000-0000-0000-0000-000000000002";
    let cases = [
        (
            "PUT",
            instance.to_owned(),
            r#"{"kind_id":"opaque","subject_id":1,"enabled":true,"config_b64":""}"#.to_owned(),
            201,
        ),
        ("GET", instance.to_owned(), String::new(), 200),
        (
            "POST",
            format!("{instance}/revisions"),
            r#"{"expected_revision":1,"enabled":true,"config_b64":""}"#.to_owned(),
            201,
        ),
        (
            "PUT",
            binding.to_owned(),
            r#"{"instance_id":"00000000-0000-0000-0000-000000000000","address":"room"}"#.to_owned(),
            201,
        ),
        ("DELETE", binding.to_owned(), String::new(), 200),
        ("DELETE", instance.to_owned(), String::new(), 200),
    ];
    for (method, uri, body, expected) in cases {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("authorization", &bearer)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), expected);
    }
    let conn = state.db.lock().unwrap();
    assert_eq!(
        conn.query_row("SELECT count(*) FROM gate_admin_request_audit", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap(),
        6
    );
}

#[test]
fn bootstrap_is_atomic_exact_idempotent_and_full_scan_duplicate_safe() {
    let mut conn = opencrab_db::init_memory().unwrap();
    let first = manifest([7; 32], "first", 10_000);
    assert_eq!(bootstrap(&mut conn, &first, 100).unwrap().created, true);
    let restart = bootstrap(&mut conn, &first, 101).unwrap();
    assert_eq!(
        restart,
        BootstrapOutcome {
            created: false,
            scanned_principals: 1
        }
    );
    let duplicate = manifest([7; 32], "duplicate", 10_000);
    assert!(matches!(
        bootstrap(&mut conn, &duplicate, 102),
        Err(SecurityError::Conflict)
    ));
    assert_eq!(
        conn.query_row("SELECT count(*) FROM gate_admin_principals", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn authentication_requires_exactly_one_current_match_and_never_unions_scope() {
    let mut conn = opencrab_db::init_memory().unwrap();
    let token = [9; 32];
    bootstrap(&mut conn, &manifest(token, "one", 10_000), 100).unwrap();
    let header = format!("Bearer {}", URL_SAFE_NO_PAD.encode(token));
    assert!(authorize(
        &mut conn,
        Some(&header),
        Operation::InstanceRead,
        1,
        Uuid::nil(),
        200
    )
    .is_ok());
    assert!(matches!(
        authorize(
            &mut conn,
            None,
            Operation::InstanceRead,
            1,
            Uuid::nil(),
            200
        ),
        Err(SecurityError::Unauthorized)
    ));
    assert!(matches!(
        authorize(
            &mut conn,
            Some(&header),
            Operation::InstancePut,
            1,
            Uuid::nil(),
            200
        ),
        Err(SecurityError::Unauthorized)
    ));
    assert!(matches!(
        authorize(
            &mut conn,
            Some(&header),
            Operation::InstanceRead,
            2,
            Uuid::nil(),
            200
        ),
        Err(SecurityError::Unauthorized)
    ));
    assert!(matches!(
        authorize(
            &mut conn,
            Some(&header),
            Operation::InstanceRead,
            1,
            Uuid::max(),
            200
        ),
        Err(SecurityError::Unauthorized)
    ));
    revoke(&conn, "one", 201).unwrap();
    assert!(matches!(
        authorize(
            &mut conn,
            Some(&header),
            Operation::InstanceRead,
            1,
            Uuid::nil(),
            202
        ),
        Err(SecurityError::Unauthorized)
    ));
}

#[test]
fn creation_namespace_is_bounded_to_a_scoped_existing_subject() {
    let mut conn = opencrab_db::init_memory().unwrap();
    conn.execute(
        "INSERT INTO agents(agent_id,name,persona_name,subject_id) VALUES ('agent-a','a','a',1)",
        [],
    )
    .unwrap();
    let namespace = Uuid::parse_str("12345678-1234-5678-9234-567812345678").unwrap();
    let expected = Uuid::new_v5(&namespace, b"instance\0agent-a");
    let token = [12_u8; 32];
    let mut scoped = manifest(token, "namespace", 10_000);
    scoped.instance_ids.clear();
    scoped.creation_namespace = Some(namespace);
    bootstrap(&mut conn, &scoped, 100).unwrap();
    let header = format!("Bearer {}", URL_SAFE_NO_PAD.encode(token));
    assert!(authorize(
        &mut conn,
        Some(&header),
        Operation::InstanceRead,
        1,
        expected,
        200
    )
    .is_ok());
    assert!(matches!(
        authorize(
            &mut conn,
            Some(&header),
            Operation::InstanceRead,
            1,
            Uuid::nil(),
            200
        ),
        Err(SecurityError::Unauthorized)
    ));
    assert!(matches!(
        authorize(
            &mut conn,
            Some(&header),
            Operation::InstanceRead,
            2,
            expected,
            200
        ),
        Err(SecurityError::Unauthorized)
    ));
}

#[test]
fn synthetic_multiple_credential_matches_are_unauthorized_without_scope_union() {
    let mut conn = opencrab_db::init_memory().unwrap();
    let token = [8_u8; 32];
    bootstrap(&mut conn, &manifest(token, "one", 10_000), 100).unwrap();
    let salt = [2_u8; 32];
    let hash = credential_hash(&salt, &token);
    conn.execute(
        "INSERT INTO gate_admin_principals VALUES
         ('two', ?1, ?2, 'exact', 101, 10000, NULL, NULL, NULL, NULL)",
        params![salt.as_slice(), &hash[..]],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO gate_admin_principal_operations VALUES ('two','instance.put')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO gate_admin_principal_subjects VALUES ('two',1)",
        [],
    )
    .unwrap();
    conn.execute("INSERT INTO gate_admin_principal_instances VALUES ('two','00000000-0000-0000-0000-000000000000')", []).unwrap();
    conn.execute(
        "UPDATE gate_admin_principals SET sealed_at=102 WHERE principal_id='two'",
        [],
    )
    .unwrap();
    let header = format!("Bearer {}", URL_SAFE_NO_PAD.encode(token));
    assert!(matches!(
        authorize(
            &mut conn,
            Some(&header),
            Operation::InstanceRead,
            1,
            Uuid::nil(),
            200
        ),
        Err(SecurityError::Unauthorized)
    ));
    assert!(matches!(
        authorize(
            &mut conn,
            Some(&header),
            Operation::InstancePut,
            1,
            Uuid::nil(),
            200
        ),
        Err(SecurityError::Unauthorized)
    ));
}

#[test]
fn strict_manifest_rejects_unknown_duplicate_and_weak_token_fields() {
    let token = URL_SAFE_NO_PAD.encode([3_u8; 32]);
    let base = format!(
        r#"{{"version":1,"principal_id":"operator","bearer_token":"{token}","operations":["instance.read"],"scope":{{"subject_ids":[1],"instance_ids":["00000000-0000-0000-0000-000000000000"],"creation_namespace":null}},"expires_at":"2030-01-01T00:00:00Z","rotation":null}}"#
    );
    assert!(parse_manifest(base.as_bytes()).is_ok());
    assert!(parse_manifest(
        base.replace("\"rotation\":null", "\"rotation\":null,\"unknown\":1")
            .as_bytes()
    )
    .is_err());
    assert!(parse_manifest(
        base.replace("\"version\":1", "\"version\":1,\"version\":1")
            .as_bytes()
    )
    .is_err());
    assert!(parse_manifest(base.replace(&token, "d2Vhaw").as_bytes()).is_err());
}

#[test]
fn manifest_path_requires_regular_private_core_euid_owned_inode_without_symlinks() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let path = root.join("credential.json");
    let token = URL_SAFE_NO_PAD.encode([4_u8; 32]);
    let json = format!(
        r#"{{"version":1,"principal_id":"operator","bearer_token":"{token}","operations":["instance.read"],"scope":{{"subject_ids":[1],"instance_ids":["00000000-0000-0000-0000-000000000000"],"creation_namespace":null}},"expires_at":"2030-01-01T00:00:00Z","rotation":null}}"#
    );
    std::fs::write(&path, json).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let euid = unsafe { libc::geteuid() };
    assert!(read_manifest(&path, euid).is_ok());
    assert!(matches!(
        read_manifest(&path, euid.wrapping_add(1)),
        Err(SecurityError::InvalidManifest)
    ));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    assert!(matches!(
        read_manifest(&path, euid),
        Err(SecurityError::InvalidManifest)
    ));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let link = root.join("credential-link.json");
    symlink(&path, &link).unwrap();
    assert!(matches!(
        read_manifest(&link, euid),
        Err(SecurityError::InvalidManifest)
    ));
    assert!(matches!(
        read_manifest(&root, euid),
        Err(SecurityError::InvalidManifest)
    ));
}

#[test]
fn rotation_overlap_and_immediate_revocation_are_current_state_checks() {
    let mut conn = opencrab_db::init_memory().unwrap();
    let first_token = [5; 32];
    let first = manifest(first_token, "first", 1_000);
    bootstrap(&mut conn, &first, 100).unwrap();
    let mut successor = manifest([6; 32], "successor", 900);
    successor.rotation = Some(Rotation {
        predecessor_principal_id: "first".to_owned(),
        overlap_deadline: 500,
    });
    bootstrap(&mut conn, &successor, 200).unwrap();
    let first_header = format!("Bearer {}", URL_SAFE_NO_PAD.encode(first_token));
    assert!(authorize(
        &mut conn,
        Some(&first_header),
        Operation::InstanceRead,
        1,
        Uuid::nil(),
        499
    )
    .is_ok());
    assert!(matches!(
        authorize(
            &mut conn,
            Some(&first_header),
            Operation::InstanceRead,
            1,
            Uuid::nil(),
            500
        ),
        Err(SecurityError::Unauthorized)
    ));
    revoke(&conn, "successor", 300).unwrap();
    let successor_header = format!("Bearer {}", URL_SAFE_NO_PAD.encode([6_u8; 32]));
    assert!(matches!(
        authorize(
            &mut conn,
            Some(&successor_header),
            Operation::InstanceRead,
            1,
            Uuid::nil(),
            301
        ),
        Err(SecurityError::Unauthorized)
    ));
}

#[test]
fn mutation_and_audit_are_atomic_and_rejected_savepoint_commits_only_audit() {
    let mut conn = opencrab_db::init_memory().unwrap();
    conn.execute("CREATE TABLE mutation_fixture(value TEXT)", [])
        .unwrap();
    let authorized = Authorized {
        principal_id: "principal".to_owned(),
        subject_id: 1,
        instance_id: Uuid::nil(),
    };
    // The audit FK is intentionally satisfied by a sealed fixture principal.
    insert_fixture_principal(&conn, "principal");
    let success_id = Uuid::new_v4();
    audited_mutation(
        &mut conn,
        success_id,
        100,
        Operation::InstancePut,
        &authorized,
        |tx| {
            tx.execute("INSERT INTO mutation_fixture VALUES ('committed')", [])
                .map_err(|_| SecurityError::Store)?;
            Ok(())
        },
    )
    .unwrap();
    let rejected_id = Uuid::new_v4();
    assert!(matches!(
        audited_mutation(
            &mut conn,
            rejected_id,
            101,
            Operation::InstancePut,
            &authorized,
            |tx| {
                tx.execute("INSERT INTO mutation_fixture VALUES ('rolled-back')", [])
                    .map_err(|_| SecurityError::Store)?;
                Err(SecurityError::Conflict)
            }
        ),
        Err(SecurityError::Conflict)
    ));
    assert_eq!(
        conn.query_row("SELECT count(*) FROM mutation_fixture", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        conn.query_row("SELECT count(*) FROM gate_admin_request_audit", [], |r| r
            .get::<_, i64>(
            0
        ))
        .unwrap(),
        2
    );

    // Reusing request_id forces the audit append to fail; the mutation cannot commit.
    assert!(matches!(
        audited_mutation(
            &mut conn,
            success_id,
            102,
            Operation::InstancePut,
            &authorized,
            |tx| {
                tx.execute("INSERT INTO mutation_fixture VALUES ('audit-failed')", [])
                    .map_err(|_| SecurityError::Store)?;
                Ok(())
            }
        ),
        Err(SecurityError::Store)
    ));
    assert_eq!(
        conn.query_row("SELECT count(*) FROM mutation_fixture", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

fn insert_fixture_principal(conn: &Connection, id: &str) {
    let salt = [1_u8; 32];
    let hash = [2_u8; 32];
    conn.execute(
        "INSERT INTO gate_admin_principals VALUES (?1,?2,?3,'exact',1,1000,NULL,NULL,NULL,NULL)",
        params![id, salt.as_slice(), hash.as_slice()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO gate_admin_principal_operations VALUES (?1,'instance.read')",
        [id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO gate_admin_principal_subjects VALUES (?1,1)",
        [id],
    )
    .unwrap();
    conn.execute("INSERT INTO gate_admin_principal_instances VALUES (?1,'00000000-0000-0000-0000-000000000000')", [id]).unwrap();
    conn.execute(
        "UPDATE gate_admin_principals SET sealed_at=2 WHERE principal_id=?1",
        [id],
    )
    .unwrap();
}

#[test]
fn audit_is_sanitized_and_append_only() {
    let conn = opencrab_db::init_memory().unwrap();
    append_audit(
        &conn,
        Uuid::new_v4(),
        100,
        Operation::InstanceRead,
        None,
        "unauthorized",
    )
    .unwrap();
    let values: (Option<String>, Option<i64>, Option<String>) = conn.query_row(
        "SELECT principal_id, authorized_subject_id, authorized_instance_id FROM gate_admin_request_audit",
        [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
    assert_eq!(values, (None, None, None));
    assert!(conn
        .execute("DELETE FROM gate_admin_request_audit", [])
        .is_err());
}
