use std::sync::Arc;

use rusqlite::{params, OptionalExtension, TransactionBehavior};

use crate::close::close_live;
use crate::error::ErrorCode;
use crate::ids::now_nanos;
use crate::operations::{
    runtime_declaration_digest, validate_operations, validate_runtime_compatibility, FinalDelivery,
    GatewayOperationDeclaration,
};
use crate::protocol::{ok_frame, write_json};
use crate::registry::{ExtgateState, LiveEntry};

/// Stored for a generic instance created by `hello`. Core never reads these columns (D-1099).
const NEUTRAL_KIND: &str = "gateway";
const NEUTRAL_CONFIG_B64: &str = "e30=";
const NEUTRAL_CONFIG_DIGEST: &str =
    "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a";

/// D-1099: `hello` names the instance and the agent it speaks for. Core finds or creates the
/// instance and accepts the connection; nothing is checked against stored gateway state and no
/// session is registered beforehand.
pub(crate) async fn handle_hello(
    state: &Arc<ExtgateState>,
    writer: &Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
    identity: u64,
    hello: crate::protocol::Hello,
) -> Result<String, ()> {
    if hello.protocol != 3 || hello.operation_protocol != 1 {
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
            match ensure_instance(&state.db, &hello.instance_id, &hello.agent_id) {
                Err(code) => Err(code),
                Ok(()) => match validate_hello_declarations(
                    state,
                    &hello.operations,
                    &hello.final_delivery,
                ) {
                    Err(code) => Err(code),
                    Ok((declarations, declaration_digest, final_delivery)) => {
                        reg.insert(
                            hello.instance_id.clone(),
                            LiveEntry {
                                identity,
                                writer: Arc::clone(writer),
                                pending: std::collections::HashMap::new(),
                                declarations: Arc::new(declarations),
                                declaration_digest,
                                final_delivery,
                                run_env: hello.run_env.clone(),
                            },
                        );
                        Ok(())
                    }
                },
            }
        }
    };
    if let Err(code) = registered {
        // fail-loud（#894）: the reason and instance are logged so a refusal is visible.
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
    Ok(hello.instance_id)
}

/// Finds the instance, or creates it for the agent. An existing instance must speak for the same
/// agent (`instance_conflict` otherwise); an unknown agent is `subject_unknown`.
fn ensure_instance(
    db: &opencrab_db::Db,
    instance_id: &str,
    agent_id: &str,
) -> Result<(), ErrorCode> {
    let mut conn = db.lock().map_err(|_| ErrorCode::StoreError)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| ErrorCode::StoreError)?;
    let subject_id: i64 = tx
        .query_row(
            "SELECT subject_id FROM agents WHERE agent_id = ?1 AND subject_id IS NOT NULL",
            [agent_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| ErrorCode::StoreError)?
        .ok_or(ErrorCode::SubjectUnknown)?;
    let existing: Option<(i64, Option<i64>)> = tx
        .query_row(
            "SELECT subject_id, deleted_at FROM gate_instances WHERE instance_id = ?1",
            [instance_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|_| ErrorCode::StoreError)?;
    match existing {
        Some((stored, None)) if stored == subject_id => {}
        Some(_) => return Err(ErrorCode::InstanceConflict),
        None => {
            let now = now_nanos();
            tx.execute(
                "INSERT INTO gate_instances (
                    instance_id, kind_id, subject_id, revision, enabled,
                    config_b64, config_digest, created_at, updated_at, deleted_at
                 ) VALUES (?1, ?2, ?3, 1, 1, ?4, ?5, ?6, ?6, NULL)",
                params![
                    instance_id,
                    NEUTRAL_KIND,
                    subject_id,
                    NEUTRAL_CONFIG_B64,
                    NEUTRAL_CONFIG_DIGEST,
                    now
                ],
            )
            .map_err(|_| ErrorCode::StoreError)?;
        }
    }
    tx.commit().map_err(|_| ErrorCode::StoreError)
}

/// hello の宣言を検証し、live snapshot 用の宣言配列と（informational な）canonical digest を返す
/// （DI-03）。永続 digest との照合はしない（#894）。
///
/// - reserved collision → `bad_request`（DI-03）
/// - 他の宣言不正 → `operation_declaration_invalid`（DI-22）
fn validate_hello_declarations(
    state: &ExtgateState,
    operations: &serde_json::Value,
    final_delivery: &str,
) -> Result<(Vec<GatewayOperationDeclaration>, String, FinalDelivery), ErrorCode> {
    let decls = validate_operations(operations, &|name| state.is_reserved_tool_name(name))
        .map_err(|error| error.code)?;
    let final_delivery =
        FinalDelivery::parse(final_delivery).ok_or(ErrorCode::OperationDeclarationInvalid)?;
    validate_runtime_compatibility(&decls, final_delivery).map_err(|error| error.code)?;
    let digest = runtime_declaration_digest(&decls, final_delivery);
    Ok((decls, digest, final_delivery))
}

#[cfg(test)]
mod tests {
    #[test]
    fn neutral_digest_is_the_digest_of_the_neutral_config() {
        assert_eq!(
            crate::ids::config_digest_from_b64(super::NEUTRAL_CONFIG_B64).unwrap(),
            super::NEUTRAL_CONFIG_DIGEST
        );
    }
}
