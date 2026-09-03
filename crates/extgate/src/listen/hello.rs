use std::sync::Arc;
use std::time::Duration;

use rusqlite::params;

use crate::close::close_live;
use crate::error::{ErrorCode, GateError};
use crate::operations::{declaration_digest, validate_operations, GatewayOperationDeclaration};
use crate::protocol::{bind_frame, ok_frame, write_json};
use crate::registry::{ExtgateState, LiveEntry, Pending};

const BIND_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) async fn handle_hello(
    state: &Arc<ExtgateState>,
    writer: &Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
    identity: u64,
    hello: crate::protocol::Hello,
) -> Result<String, ()> {
    if hello.protocol != 2 {
        close_live(
            state,
            None,
            Some(identity),
            ErrorCode::ProtocolUnsupported,
            Some(&hello.id),
            Some(writer),
        )
        .await;
        return Err(());
    }
    let registered = {
        let mut reg = match state.lock_registry() {
            Ok(g) => g,
            Err(_) => return Err(()),
        };
        if reg.is_live(&hello.instance_id) {
            Err(ErrorCode::InstanceActive)
        } else {
            match inspect_instance(&state.db, &hello.instance_id) {
                Err(code) => Err(code),
                Ok(inspected) if !inspected.enabled => Err(ErrorCode::InstanceDisabled),
                Ok(inspected) if inspected.revision != hello.revision => {
                    Err(ErrorCode::RevisionMismatch)
                }
                Ok(inspected) if inspected.config_digest != hello.config_digest => {
                    Err(ErrorCode::ConfigDigestMismatch)
                }
                // 宣言検証を hello 検査と同じ lock 下で完了させる（§4.1）。永続 digest との
                // 照合は撤去済み（#894）。
                Ok(_) => match validate_hello_declarations(state, &hello.operations) {
                    Err(code) => Err(code),
                    Ok((declarations, declaration_digest)) => {
                        match open_bindings(&state.db, &hello.instance_id) {
                            Err(_) => Err(ErrorCode::StoreError),
                            Ok(bindings) => {
                                let mut pending = std::collections::HashMap::new();
                                let started = std::time::Instant::now();
                                for (binding_id, _) in &bindings {
                                    pending.insert(
                                        crate::ids::bind_request_id(binding_id),
                                        Pending::Bind {
                                            binding_id: binding_id.clone(),
                                            started,
                                        },
                                    );
                                }
                                reg.insert(
                                    hello.instance_id.clone(),
                                    LiveEntry {
                                        identity,
                                        revision: hello.revision,
                                        writer: Arc::clone(writer),
                                        acknowledged: std::collections::HashSet::new(),
                                        pending,
                                        declarations: Arc::new(declarations),
                                        declaration_digest,
                                    },
                                );
                                Ok(bindings)
                            }
                        }
                    }
                },
            }
        }
    };
    let bindings = match registered {
        Ok(b) => b,
        Err(code) => {
            // fail-loud（#894）: hello 拒否は従来 err_frame を送るだけでサーバ側に理由が
            // 残らなかった。理由コード＋instance_id を WARN し、config_digest 不一致等の
            // 拒否を可視化する。
            tracing::warn!(
                instance_id = %hello.instance_id,
                reason = code.as_str(),
                "gate hello rejected"
            );
            close_live(
                state,
                None,
                Some(identity),
                code,
                Some(&hello.id),
                Some(writer),
            )
            .await;
            return Err(());
        }
    };

    if write_json(writer, &ok_frame(&hello.id)).await.is_err() {
        close_live(
            state,
            Some(&hello.instance_id),
            Some(identity),
            ErrorCode::Disconnect,
            None,
            None,
        )
        .await;
        return Err(());
    }
    for (binding_id, address) in &bindings {
        if write_json(writer, &bind_frame(binding_id, address))
            .await
            .is_err()
        {
            close_live(
                state,
                Some(&hello.instance_id),
                Some(identity),
                ErrorCode::BindFailed,
                None,
                None,
            )
            .await;
            return Err(());
        }
        spawn_bind_timeout(
            Arc::clone(state),
            hello.instance_id.clone(),
            binding_id.clone(),
            identity,
        );
    }
    Ok(hello.instance_id)
}

pub(crate) fn spawn_bind_timeout(
    state: Arc<ExtgateState>,
    instance_id: String,
    binding_id: String,
    identity: u64,
) {
    tokio::spawn(async move {
        tokio::time::sleep(BIND_TIMEOUT).await;
        let still = {
            let Ok(reg) = state.lock_registry() else {
                return;
            };
            match reg.get(&instance_id) {
                Some(e) if e.identity == identity => e
                    .pending
                    .get(&crate::ids::bind_request_id(&binding_id))
                    .is_some_and(Pending::is_bind),
                _ => false,
            }
        };
        if still {
            close_live(
                &state,
                Some(&instance_id),
                Some(identity),
                ErrorCode::BindFailed,
                None,
                None,
            )
            .await;
        }
    });
}

struct InstanceSnap {
    enabled: bool,
    revision: u64,
    config_digest: String,
}

fn inspect_instance(db: &opencrab_db::Db, instance_id: &str) -> Result<InstanceSnap, ErrorCode> {
    let conn = db.lock().map_err(|_| ErrorCode::StoreError)?;
    // operation_declaration_digest 列は残すが読まない（照合撤去・#894）。
    match conn.query_row(
        "SELECT enabled, revision, config_digest, deleted_at
         FROM gate_instances WHERE instance_id = ?1",
        params![instance_id],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<i64>>(3)?,
            ))
        },
    ) {
        Err(rusqlite::Error::QueryReturnedNoRows) => Err(ErrorCode::InstanceUnknown),
        Err(_) => Err(ErrorCode::StoreError),
        Ok((_, _, _, Some(_))) => Err(ErrorCode::InstanceUnknown),
        Ok((enabled, revision, digest, None)) => Ok(InstanceSnap {
            enabled: enabled == 1,
            revision: u64::try_from(revision).map_err(|_| ErrorCode::StoreError)?,
            config_digest: digest,
        }),
    }
}

/// hello の宣言を検証し、live snapshot 用の宣言配列と（informational な）canonical digest を返す
/// （DI-03）。**永続 digest との照合・永続化は撤去した**（#894）。core は hello ごとに宣言を
/// fresh parse して projection に使うので、永続照合は drift を防がず、宣言の説明文を編集する
/// たびに既存 instance の hello を `operation_declaration_mismatch` で殺すだけだった。digest は
/// live entry の情報用にだけ計算し、DB へは書かない（列は残すが未使用・revision/config_digest は
/// 従来どおり照合する）。
///
/// - reserved collision → `bad_request`（DI-03）
/// - 他の宣言不正 → `operation_declaration_invalid`（DI-22）
fn validate_hello_declarations(
    state: &ExtgateState,
    operations: &Option<serde_json::Value>,
) -> Result<(Vec<GatewayOperationDeclaration>, String), ErrorCode> {
    let decls = match operations {
        Some(ops) => {
            validate_operations(ops, &|n| state.is_reserved_tool_name(n)).map_err(|e| e.code)?
        }
        None => Vec::new(),
    };
    // 宣言 present（[] を含む）なら informational digest を計算。absent は「DI 宣言なし」で空。
    // 照合・永続化はしない（#894）。
    let digest = operations
        .as_ref()
        .map(|_| declaration_digest(&decls))
        .unwrap_or_default();
    Ok((decls, digest))
}

fn open_bindings(
    db: &opencrab_db::Db,
    instance_id: &str,
) -> Result<Vec<(String, String)>, GateError> {
    let conn = db.lock().map_err(|_| GateError::store())?;
    let mut stmt = conn
        .prepare(
            "SELECT binding_id, address FROM gate_bindings
             WHERE instance_id = ?1 AND closed_at IS NULL
             ORDER BY binding_id ASC",
        )
        .map_err(|_| GateError::store())?;
    let rows = stmt
        .query_map(params![instance_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .map_err(|_| GateError::store())?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|_| GateError::store())?);
    }
    Ok(out)
}
