//! 汎用エージェント管理ツールの gateway 非依存実装（#157 S1）。
//!
//! 旧実装は Discord ゲートウェイ（`crates/discord` の `gateway_actions/agent_management.rs`）
//! にあり、**serenity を一切参照していない**のに Discord 経由のターンでしか露出しなかった
//! （#157 / #155）。依存は DB と実行許可設定（`ToolsConfig`）だけなので、そのまま
//! `SystemGatewayActions` の own ツールへ移し、web / Nostr / REST / heartbeat の
//! 全ターンで使えるようにする。
//!
//! 移設で維持している不変条件（壊すと重大。順に対応するテストがある）:
//! - **レスポンス JSON のキーと文言**（エラー文言も含む）を Discord 実装と 1 文字も変えない。
//! - **オーナー限定のハンドラ内検査**（add / remove）。bridge の `OWNER_ONLY_ACTIONS` は
//!   これらの名前を**持っていない**ため、このハンドラ内検査が唯一のゲートである。
//! - **コマンド名の文字種検査**（英数字・`-`・`_` のみ）。同系統の
//!   `manage_allowed_commands` は trim だけなので、移設で緩めてはいけない。
//!
//! # グローバル設定へは書かない（#202）
//!
//! 旧 Discord 実装は DB に加えて**グローバルな実行許可設定**（`AppState.tools_config`）
//! にも許可コマンドを書き込んでいた。応答生成は**全エージェント**についてこの設定を
//! 実行許可の土台として複製するため、これは「A が許可したコマンドが全エージェントで
//! 実行可能になる」漏れそのものだった（削除側は設定ファイル由来のコマンドを
//! グローバルに消せた）。移設と同時に**この書き込みを撤去した**。
//!
//! 撤去して呼び出し元が困らない理由:
//! - `crate::process::resolve_run_tools_config` が**毎 run 無条件に**、そのエージェントの
//!   DB 上の許可コマンドを走行時のローカル複製へマージする。よって呼び出したエージェント
//!   自身には**次の run で**効く（DB が信頼できる情報源）。
//! - **同ターン反映は元からどちらの経路でも効かない**。ツールの登録
//!   （`opencrab_actions::register_tools_from_config`）が run の冒頭で
//!   `ShellToolConfig` を clone してスナップショットするため、走行中に設定を
//!   書き換えても本ターンのツールには届かない。つまり撤去で失われる機能は無い。
//! - グローバル書き込みはそもそも永続しない。`crate::hot_reload` が設定ファイル変更時に
//!   グローバル値を丸ごと上書きする。
//!
//! 同じ方針は `crate::main` の起動時設定構築と `crate::api::allowed_commands`
//! （REST）にも明文化されている。

use serde_json::json;

use opencrab_gateway::{GatewayActionResult, GatewayCallContext, GatewayCaller};

use crate::AppState;

/// メモリインデックス設定（batch_size / threshold）の更新。
///
/// 旧 `DiscordGatewayActions::execute_update_memory_index_config` の移設。DB のみに依存する。
pub(crate) fn update_memory_index_config(
    state: &AppState,
    args: &serde_json::Value,
    ctx: &GatewayCallContext,
) -> GatewayActionResult {
    let batch_size = args.get("batch_size").and_then(|v| v.as_i64());
    let threshold = args.get("threshold").and_then(|v| v.as_i64());

    if batch_size.is_none() && threshold.is_none() {
        return GatewayActionResult {
            success: false,
            data: None,
            error: Some("batch_sizeまたはthresholdの少なくとも1つが必要です".to_string()),
        };
    }

    let conn = state.db.lock().unwrap();

    let current = opencrab_db::queries::get_memory_index_config(&conn, &ctx.agent_id);
    let (current_batch_size, current_threshold) = match &current {
        Ok(cfg) => (cfg.batch_size, cfg.threshold),
        Err(_) => (
            opencrab_db::queries::BATCH_SIZE_DEFAULT,
            opencrab_db::queries::THRESHOLD_DEFAULT,
        ),
    };

    let new_batch_size = batch_size.unwrap_or(current_batch_size);
    let new_threshold = threshold.unwrap_or(current_threshold);

    match opencrab_db::queries::upsert_memory_index_config(
        &conn,
        &ctx.agent_id,
        new_batch_size,
        new_threshold,
    ) {
        Ok(updated) => GatewayActionResult {
            success: true,
            data: Some(json!({
                "agent_id": ctx.agent_id,
                "previous": {
                    "batch_size": current_batch_size,
                    "threshold": current_threshold,
                },
                "current": {
                    "batch_size": updated.batch_size,
                    "threshold": updated.threshold,
                },
            })),
            error: None,
        },
        Err(e) => {
            tracing::error!("upsert_memory_index_config failed: {e}");
            GatewayActionResult {
                success: false,
                data: None,
                error: Some(format!("メモリインデックス設定の更新に失敗: {e}")),
            }
        }
    }
}

/// 許可コマンドの追加（オーナー限定）。
///
/// 旧 `DiscordGatewayActions::execute_add_allowed_command` の移設。**DB のみ**へ永続化する。
/// 旧実装が併せて行っていたグローバル設定への書き込みは撤去した（モジュール doc の
/// 「グローバル設定へは書かない」/ #202）。
pub(crate) fn add_allowed_command(
    state: &AppState,
    args: &serde_json::Value,
    ctx: &GatewayCallContext,
) -> GatewayActionResult {
    if ctx.caller != GatewayCaller::Owner {
        return GatewayActionResult {
            success: false,
            data: None,
            error: Some("このアクションはオーナーのみ実行できます".to_string()),
        };
    }

    let command = match args.get("command").and_then(|v| v.as_str()) {
        Some(c) if !c.is_empty() => c,
        _ => {
            return GatewayActionResult {
                success: false,
                data: None,
                error: Some("commandパラメータが必要です".to_string()),
            }
        }
    };

    if !command
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        return GatewayActionResult {
            success: false,
            data: None,
            error: Some(format!(
                "コマンド名に無効な文字が含まれています: {}（英数字・ハイフン・アンダースコアのみ使用可）",
                command
            )),
        };
    }

    let conn = state.db.lock().unwrap();
    match opencrab_db::queries::add_agent_allowed_command(&conn, &ctx.agent_id, command, "owner") {
        // グローバル設定（`state.tools_config`）へは書かない。次の run が
        // `resolve_run_tools_config` で DB から拾い直す（#202）。
        Ok(true) => GatewayActionResult {
            success: true,
            data: Some(json!({
                "command": command,
                "agent_id": ctx.agent_id,
                "message": format!("`{}` を許可コマンドに追加しました", command),
            })),
            error: None,
        },
        Ok(false) => GatewayActionResult {
            success: true,
            data: Some(json!({
                "command": command,
                "agent_id": ctx.agent_id,
                "message": format!("`{}` はすでに許可コマンドに登録されています", command),
                "already_exists": true,
            })),
            error: None,
        },
        Err(e) => {
            tracing::error!("add_agent_allowed_command failed: {e}");
            GatewayActionResult {
                success: false,
                data: None,
                error: Some(format!("許可コマンドの追加に失敗: {e}")),
            }
        }
    }
}

/// 許可コマンドの一覧（DB のみ・純粋な読み取り）。
///
/// 旧 `DiscordGatewayActions::execute_list_allowed_commands` の移設。
pub(crate) fn list_allowed_commands(
    state: &AppState,
    ctx: &GatewayCallContext,
) -> GatewayActionResult {
    let conn = state.db.lock().unwrap();
    match opencrab_db::queries::list_agent_allowed_commands(&conn, &ctx.agent_id) {
        Ok(commands) => {
            let count = commands.len();
            GatewayActionResult {
                success: true,
                data: Some(json!({
                    "commands": commands,
                    "count": count,
                    "agent_id": ctx.agent_id,
                })),
                error: None,
            }
        }
        Err(e) => {
            tracing::error!("list_agent_allowed_commands failed: {e}");
            GatewayActionResult {
                success: false,
                data: None,
                error: Some(format!("許可コマンドの取得に失敗: {e}")),
            }
        }
    }
}

/// 許可コマンドの削除（オーナー限定）。
///
/// 旧 `DiscordGatewayActions::execute_remove_allowed_command` の移設。追加と対称に
/// **DB のみ**から取り除く。旧実装はグローバル設定からも `retain` で消していたが、
/// それは設定ファイル由来のコマンドや他エージェントの許可を巻き込んで消す漏れだった
/// （#202）ので撤去した。
pub(crate) fn remove_allowed_command(
    state: &AppState,
    args: &serde_json::Value,
    ctx: &GatewayCallContext,
) -> GatewayActionResult {
    if ctx.caller != GatewayCaller::Owner {
        return GatewayActionResult {
            success: false,
            data: None,
            error: Some("このアクションはオーナーのみ実行できます".to_string()),
        };
    }

    let command = match args.get("command").and_then(|v| v.as_str()) {
        Some(c) if !c.is_empty() => c,
        _ => {
            return GatewayActionResult {
                success: false,
                data: None,
                error: Some("commandパラメータが必要です".to_string()),
            }
        }
    };

    let conn = state.db.lock().unwrap();
    match opencrab_db::queries::remove_agent_allowed_command(&conn, &ctx.agent_id, command) {
        // グローバル設定（`state.tools_config`）からは消さない。設定ファイル由来の
        // コマンドや他エージェントの許可を巻き込むため（#202）。
        Ok(true) => GatewayActionResult {
            success: true,
            data: Some(json!({
                "command": command,
                "agent_id": ctx.agent_id,
                "message": format!("`{}` を許可コマンドから削除しました", command),
            })),
            error: None,
        },
        Ok(false) => GatewayActionResult {
            success: true,
            data: Some(json!({
                "command": command,
                "agent_id": ctx.agent_id,
                "message": format!("`{}` は許可コマンドに登録されていませんでした", command),
                "not_found": true,
            })),
            error: None,
        },
        Err(e) => {
            tracing::error!("remove_agent_allowed_command failed: {e}");
            GatewayActionResult {
                success: false,
                data: None,
                error: Some(format!("許可コマンドの削除に失敗: {e}")),
            }
        }
    }
}
